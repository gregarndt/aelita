// This file is released under the same terms as Rust itself.

mod auth;

use crossbeam;
use db::{Db, PendingEntry};
use db::sqlite::SqliteDb;
use horrorshow::prelude::*;
use hyper::buffer::BufReader;
use hyper::header::{ContentType, Headers};
use hyper::net::{HttpListener, HttpStream, NetworkListener, NetworkStream};
use hyper::server::{Request, Response};
use hyper::status::StatusCode;
use hyper::uri::RequestUri;
use pipeline::PipelineId;
use quickersort::sort_by;
use spmc;
use std::collections::HashMap;
use std::convert::AsRef;
use std::error::Error;
use std::io::{BufWriter, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use ui::Pr;
use view::auth::AuthManager;

pub use view::auth::{Auth, AuthRef};

const WORKER_COUNT: usize = 3;

pub fn run_sqlite<'a, P: Pr, A: Into<AuthRef<'a>>>(
    listen: String,
    path: PathBuf,
    pipelines: HashMap<String, PipelineId>,
    secret: String,
    auth: A,
)
    where <P::C as FromStr>::Err: Error,
          <P as FromStr>::Err: Error,
{
    let path: &Path = path.as_ref();
    let listen: &str = listen.as_ref();
    let secret: &str = secret.as_ref();
    let auth = auth.into();
    let pipelines = &pipelines;
    crossbeam::scope(|scope| {
        let mut workers = Vec::with_capacity(WORKER_COUNT);
        for _ in 0..WORKER_COUNT {
            let (send, recv) = spmc::channel();
            scope.spawn(move || {
                let mut worker = Worker {
                    db: SqliteDb::<P>::open(path)
                        .expect("opening sqlite to succeed"),
                    pipelines: pipelines,
                    auth_manager: AuthManager{
                        auth: auth,
                        secret: secret,
                    },
                    _pr: PhantomData::<P>,
                };
                worker.run(recv)
            });
            workers.push(send);
        }
        let mut listener = HttpListener::new(listen).expect("a TCP socket");
        let mut i = 0;
        while let Ok(stream) = listener.accept() {
            workers[i].send(stream).unwrap();
            i += 1;
            i %= WORKER_COUNT;
        }
    });
}

struct Worker<'a, P, D>
    where P: Pr, D: Db<P>
{
    db: D,
    pipelines: &'a HashMap<String, PipelineId>,
    auth_manager: AuthManager<'a>,
    _pr: PhantomData<P>,
}

impl<'a, P: Pr, D: Db<P>> Worker<'a, P, D> {
    fn run(&mut self, recv: spmc::Receiver<HttpStream>) {
        while let Ok(mut stream) = recv.recv() {
            let addr = stream.peer_addr()
                .expect("view client address");
            let mut stream_clone = stream.clone();
            let mut buf_read = BufReader::new(
                &mut stream_clone as &mut NetworkStream
            );
            let mut buf_write = BufWriter::new(&mut stream);
            let req = match Request::new(&mut buf_read, addr) {
                Ok(req) => req,
                Err(e) => {
                    warn!("Invalid view HTTP: {:?}", e);
                    continue;
                }
            };
            let mut head = Headers::new();
            let res = Response::new(&mut buf_write, &mut head);
            match self.handle_req(req, res) {
                Ok(()) => (),
                Err(e) => {
                    warn!("Failed to handle view request: {:?}", e);
                }
            }
        }
    }
    fn handle_req(
        &mut self,
        req: Request,
        res: Response,
    ) -> Result<(), Box<Error>> {
        let (req, mut res) = match self.auth_manager.check(req, res) {
            auth::CheckResult::Authenticated(req, res) => (req, res),
            auth::CheckResult::Err(e) => return Err(Box::new(e)),
            auth::CheckResult::NotAuthenticated => return Ok(()),
        };
        let pipeline = if let RequestUri::AbsolutePath(ref path) = req.uri {
            let mut path = &path[..];
            if path == "/" {
                None
            } else {
                if path.as_bytes()[0] == b'/' {
                    path = &path[1..];
                }
                match self.pipelines.get(path) {
                    Some(pipeline_id) => {
                        *res.status_mut() = StatusCode::Ok;
                        Some((path.to_owned(), *pipeline_id))
                    }
                    None => {
                        *res.status_mut() = StatusCode::NotFound;
                        return Ok(());
                    }
                }
            }
        } else {
            *res.status_mut() = StatusCode::BadRequest;
            return Ok(());
        };
        res.headers_mut().set(ContentType::html());
        let mut res = try!(res.start());
        try!(res.write_all(br##"<!DOCTYPE html>"##));
        if let Some((name, pipeline_id)) = pipeline {
            self.handle_pipeline_req(&name, pipeline_id, req, res)
        } else {
            self.handle_home_req(req, res)
        }
    }
    fn handle_pipeline_req(
        &mut self,
        name: &str,
        pipeline_id: PipelineId,
        _req: Request,
        mut res: Response<::hyper::net::Streaming>,
    ) -> Result<(), Box<Error>> {
        let pending_entries = self.db.list_pending(pipeline_id);
        let is_empty = pending_entries.is_empty();
        let queued_entries = self.db.list_queue(pipeline_id);
        let running_entry = self.db.peek_running(pipeline_id);
        let mut running = None;
        let mut queued = Vec::new();
        let pending: Vec<_> = pending_entries.into_iter().filter_map(|entry| {
            if Some(&entry.pr) == running_entry.as_ref().map(|x| &x.pr) {
                running = Some(entry);
            } else if queued_entries.iter()
                    .filter(|q| q.pr == entry.pr)
                    .next().is_some() {
                queued.push(entry);
            } else {
                return Some(entry);
            }
            None
        }).collect();
        let html = html!{
            html {
                head {
                    title { : name }
                    link(rel="stylesheet", href="https://cdnjs.cloudflare.com/ajax/libs/normalize/4.1.1/normalize.min.css");
                    style { : raw!(include_str!("style.css")) }
                }
                body {
                    h1 { : name }
                    table {
                        thead {
                            th { : "Status" }
                            th { : "PR#" }
                            th { : "Title" }
                        }
                        tbody {
                            |t| {
                                for entry in running {
                                    render_entry(State::Running, entry, t);
                                }
                                for entry in queued {
                                    render_entry(State::Queued, entry, t);
                                }
                                for entry in pending {
                                    render_entry(State::Pending, entry, t);
                                }
                                if is_empty {
                                    t << html!{
                                        td(colspan=3) {
                                            : "No opened pull requests"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        };
        try!(html.write_to_io(&mut res));
        try!(res.end());
        Ok(())
    }
    fn handle_home_req(
        &mut self,
        _req: Request,
        mut res: Response<::hyper::net::Streaming>,
    ) -> Result<(), Box<Error>> {
        let mut pipelines: Vec<(&str, PipelineId)> =
            self.pipelines.iter().map(|(n, p)| (&n[..], *p)).collect();
        sort_by(&mut pipelines, &|a, b| a.0.cmp(b.0));
        let html = html!{
            html {
                head {
                    title { : "Aelita" }
                    link(rel="stylesheet", href="https://cdnjs.cloudflare.com/ajax/libs/normalize/4.1.1/normalize.min.css");
                    style { : raw!(include_str!("style.css")) }
                }
                body {
                    h1 { : "Pipelines" }
                    table {
                        thead {
                            th { : "Name" }
                            th { : "Running" }
                            th { : "In queue" }
                            th { : "In review" }
                            th { : "Opened" }
                        }
                        tbody {
                            @ for &(n, pid) in &pipelines { |t| {
                                let opened = self.db.list_pending(pid).len();
                                let queue = self.db.list_queue(pid).len();
                                let running = self.db.peek_running(pid)
                                    .is_some();
                                let running = if running { 1 } else { 0 };
                                let review = opened - queue - running;
                                t << html!{
                                    tr {
                                        td(class="fill-link") {
                                            a(href=n) : { n }
                                        }
                                        td { : running }
                                        td { : queue }
                                        td { : review }
                                        td { : opened }
                                    }
                                }
                            }}
                            @ if pipelines.is_empty() {
                                td(colspan=5) {
                                    : "No configured repositories"
                                }
                            }
                        }
                    }
                    h2 { : "Github cheat sheet" }
                    p { : "To use the robot, say a command to it." }
                    dl {
                        dt { : raw!("<code>r+</code>") }
                        dd { : "Add the pull request to the merge queue." }
                        dt { : raw!("<code>r=@username</code>") }
                        dd { : "Add the pull request as \"username.\"" }
                        dt { : raw!("<code>r-</code>") }
                        dd { : "Cancel the pull request." }
                        dt { : raw!("<code>try+</code>") }
                        dd { : "Test the pull request without merging it." }
                        dt { : raw!("<code>try-</code>") }
                        dd { : "Cancel the pull request." }
                    }
                }
            }
        };
        try!(html.write_to_io(&mut res));
        try!(res.end());
        Ok(())
    }
}

enum State {
    Running,
    Queued,
    Pending,
}

fn render_entry<P: Pr>(
    state: State,
    entry: PendingEntry<P>,
    t: &mut TemplateBuffer,
) {
    t << html!{
        tr {
            td {
                : match state {
                    State::Running => "Running",
                    State::Queued => "In queue",
                    State::Pending => "In review",
                }
            }
            td(class="fill-link") {
                a(href=entry.url.to_string()) { : entry.pr.to_string() }
            }
            td { : &entry.title }
        }
    };
}
