//! The query pool (A §3.4, §3.7): the `cheby-query-N` threads that run
//! request handlers off the main loop.
//!
//! One channel feeds every thread. Each result goes back to the main loop
//! as a [`QueryDone`], so only the main loop writes responses. A handler
//! that panics is caught here: its request is answered `InternalError`,
//! and the thread goes on with the next job (A §3.7). A request cancelled
//! before its handler starts is answered `RequestCancelled` without
//! running it (A §3.5).
//!
//! The main loop's side is here too: which route a request takes, what it
//! reads, and the response once its handler is done.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use lsp_server::{RequestId, Response};
use lsp_types::{MessageType, Uri};
use serde_json::Value;

use super::schedule::{Parked, Want};
use super::sessions::DocRoute;
use super::state::package_root;
use super::{ClientCaps, Effect, Server, Settings, codes};
use crate::features::stdlib::StdService;
use crate::features::{
    Analyzed, CancelToken, Class, HandlerOutput, LiveText, RequestCx, RequestError, Route,
};
use crate::text::lines::PositionEncoding;
use crate::text::uri::{normalize, uri_to_path};

/// A query thread's stack: handlers recurse over trees and THIR, as the
/// compiler does (D-601). It reserves address space only.
const STACK_SIZE: usize = 256 << 20;

/// What a request reads besides its target, as the main loop had it when
/// the request arrived.
#[derive(Debug)]
pub(crate) struct RequestEnv {
    /// The settings in force.
    pub settings: Arc<Settings>,
    /// The client's capabilities.
    pub caps: ClientCaps,
    /// The session's position encoding.
    pub encoding: PositionEncoding,
    /// Std's base and sources.
    pub std: Arc<StdService>,
}

/// What a handler reads.
#[derive(Debug)]
pub(crate) enum QueryTarget {
    /// An open document's live text, for a text handler.
    Text(Arc<LiveText>),
    /// A snapshot, for a document or package handler.
    Snapshot(Arc<Analyzed>),
    /// The std base's snapshot, for a document or package handler on an
    /// opened std document (A §6.3). The pool thread waits for the base,
    /// never the main loop, and answers the route's fallback when it
    /// failed.
    Std,
    /// The newest snapshot of each package, for a workspace handler.
    Workspace(Vec<Arc<Analyzed>>),
}

/// One request for the pool.
#[derive(Debug)]
pub(crate) struct QueryJob {
    /// The request.
    pub id: RequestId,
    /// Its route.
    pub route: &'static Route,
    /// Its parameters.
    pub params: serde_json::Value,
    /// What its handler reads.
    pub target: QueryTarget,
    /// The rest of what it reads.
    pub env: Arc<RequestEnv>,
    /// Set by `$/cancelRequest`.
    pub cancel: CancelToken,
}

/// Why a handler gave no output.
#[derive(Debug)]
pub(crate) enum QueryFailure {
    /// The handler's own error.
    Error(RequestError),
    /// The handler panicked, with the panic's message: `InternalError`.
    Panic(String),
}

/// A finished request.
#[derive(Debug)]
pub(crate) struct QueryDone {
    /// The request.
    pub id: RequestId,
    /// Its method, for the log.
    pub method: &'static str,
    /// The handler's output, or why there is none.
    pub result: Result<HandlerOutput, QueryFailure>,
}

/// The pool's side of the main loop: where jobs go.
#[derive(Debug)]
pub(crate) struct QueryPool {
    jobs: Sender<QueryJob>,
}

impl QueryPool {
    /// Starts the pool's threads, `clamp(cores / 2, 2, 4)` of them
    /// (A §3.1). They end when the pool is dropped, each after its job.
    ///
    /// # Errors
    ///
    /// The error of a thread that cannot be started.
    pub(crate) fn spawn() -> std::io::Result<(Self, Receiver<QueryDone>)> {
        let (job_tx, job_rx) = crossbeam_channel::unbounded::<QueryJob>();
        let (done_tx, done_rx) = crossbeam_channel::unbounded::<QueryDone>();
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let count = (cores / 2).clamp(2, 4);
        for n in 0..count {
            let jobs = job_rx.clone();
            let done = done_tx.clone();
            std::thread::Builder::new()
                .name(format!("cheby-query-{n}"))
                .stack_size(STACK_SIZE)
                .spawn(move || serve(&jobs, &done))?;
        }
        Ok((Self { jobs: job_tx }, done_rx))
    }

    /// Queues `job`. `Err` gives it back when every thread has gone, which
    /// a contained panic never causes.
    pub(crate) fn send(&self, job: QueryJob) -> Result<(), Box<QueryJob>> {
        self.jobs.send(job).map_err(|e| Box::new(e.into_inner()))
    }
}

/// One thread's loop.
fn serve(jobs: &Receiver<QueryJob>, done: &Sender<QueryDone>) {
    for job in jobs {
        let id = job.id.clone();
        let method = job.route.method;
        let result = run(job);
        if done.send(QueryDone { id, method, result }).is_err() {
            return;
        }
    }
}

/// Runs one handler, containing a panic (A §3.7).
fn run(job: QueryJob) -> Result<HandlerOutput, QueryFailure> {
    let QueryJob {
        route,
        params,
        target,
        env,
        cancel,
        ..
    } = job;
    // Cancelled while it waited in the channel: never started (A §3.5).
    cancel.check().map_err(QueryFailure::Error)?;
    let cx = RequestCx {
        settings: &env.settings,
        caps: &env.caps,
        encoding: env.encoding,
        cancel: &cancel,
        std: &env.std,
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| match (route.class, &target) {
        (Class::Text(handler), QueryTarget::Text(live)) => match live.doc_text() {
            Some(doc) => handler(&cx, doc, params),
            None => Err(RequestError::Failed(
                "the document is too large to parse".to_owned(),
            )),
        },
        (Class::Doc(handler) | Class::Package(handler), QueryTarget::Snapshot(analyzed)) => {
            handler(&cx, analyzed, params)
        }
        (Class::Doc(handler) | Class::Package(handler), QueryTarget::Std) => {
            match env.std.ready() {
                Some(ready) => handler(&cx, &ready.analyzed, params),
                None => Ok(HandlerOutput::value((route.fallback)())),
            }
        }
        (Class::Workspace(handler), QueryTarget::Workspace(packages)) => {
            let packages: Vec<&Analyzed> = packages.iter().map(|a| &**a).collect();
            handler(&cx, &packages, params)
        }
        (
            Class::Text(_),
            QueryTarget::Snapshot(_) | QueryTarget::Std | QueryTarget::Workspace(_),
        )
        | (Class::Doc(_) | Class::Package(_), QueryTarget::Text(_) | QueryTarget::Workspace(_))
        | (
            Class::Workspace(_),
            QueryTarget::Text(_) | QueryTarget::Snapshot(_) | QueryTarget::Std,
        ) => Err(RequestError::Failed(format!(
            "`{}` got the wrong kind of input",
            route.method
        ))),
    }));
    // A failure of std is logged once, by the first answer after it.
    let outcome = outcome.map(|result| {
        result.map(|mut output| {
            if let Some(message) = env.std.unreported_failure() {
                output
                    .effects
                    .push(Effect::log(MessageType::ERROR, message));
            }
            output
        })
    });
    match outcome {
        Ok(result) => result.map_err(QueryFailure::Error),
        Err(payload) => {
            let text = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("a panic with no message");
            Err(QueryFailure::Panic(text.to_owned()))
        }
    }
}

impl Server {
    /// A request with a route off the main loop (A §3.3). A text request
    /// runs at once on the document's live text. A snapshot request runs
    /// when the package's newest snapshot holds the text it names, and
    /// waits for one until then (D-1367). With no open document or package
    /// to read, it gets its route's fallback.
    pub(crate) fn dispatch_query(&mut self, id: RequestId, route: &'static Route, params: Value) {
        // A workspace request names no document: it reads each package's
        // newest snapshot as it is, and waits for none (A §3.3).
        if matches!(route.class, Class::Workspace(_)) {
            let packages = self
                .served
                .values()
                .map(|served| Arc::clone(&served.analyzed))
                .collect();
            let env = self.env();
            self.start_query(id, route, params, QueryTarget::Workspace(packages), env);
            return;
        }
        let path = match document_path(&params) {
            Ok(Some(path)) => path,
            // A URI that names no file, such as `untitled:`, is neither open
            // here nor in a package: the fallback (A §3.3).
            Ok(None) => {
                self.answer(id, Ok((route.fallback)()));
                return;
            }
            Err(()) => {
                let error = RequestError::InvalidParams(format!(
                    "`{}`: no `textDocument.uri` that is a URI",
                    route.method
                ));
                self.answer(id, Err(error));
                return;
            }
        };
        // A std source is read-only, so a request that would edit it gets
        // the fallback, `null` for formatting (A §6.3, X §5.4).
        if route.edits
            && self
                .docs
                .get(&path)
                .is_some_and(|doc| matches!(doc.route, DocRoute::Std))
        {
            self.answer(id, Ok((route.fallback)()));
            return;
        }
        let env = self.env();
        match route.class {
            Class::Text(_) => match self.live_text(&path) {
                Some(live) => self.start_query(id, route, params, QueryTarget::Text(live), env),
                None => self.answer(id, Ok((route.fallback)())),
            },
            Class::Doc(_) | Class::Package(_) | Class::Workspace(_) => {
                // An opened std document reads the std base's snapshot
                // until it is edited, and then gets the fallback (A §6.3).
                // The pool thread waits for the base, so the main loop
                // never does.
                if let Some(doc) = self.docs.get(&path)
                    && matches!(doc.route, DocRoute::Std)
                {
                    if doc.diverged {
                        self.answer(id, Ok((route.fallback)()));
                    } else {
                        self.start_query(id, route, params, QueryTarget::Std, env);
                    }
                    return;
                }
                let Some(root) = self.root_of(&path) else {
                    self.answer(id, Ok((route.fallback)()));
                    return;
                };
                let want = match route.class {
                    Class::Package(_) => Want {
                        texts: self
                            .docs
                            .iter()
                            .filter(|(_, doc)| doc.root.as_deref() == Some(root.as_path()))
                            .map(|(path, doc)| (path.clone(), doc.stamp))
                            .collect(),
                    },
                    Class::Doc(_) | Class::Text(_) | Class::Workspace(_) => Want {
                        texts: self
                            .docs
                            .get(&path)
                            .map(|doc| (path.clone(), doc.stamp))
                            .into_iter()
                            .collect(),
                    },
                };
                let parked = Parked {
                    id,
                    route,
                    params,
                    root,
                    want,
                    deadline: self.park_deadline(),
                    env,
                };
                self.want_snapshot(parked);
            }
        }
    }

    /// The package whose snapshots answer for `path`: an open document's
    /// own, else the nearest package root that has a session.
    fn root_of(&self, path: &Path) -> Option<PathBuf> {
        match self.docs.get(path) {
            Some(doc) => doc.root.clone(),
            None => package_root(path),
        }
        .filter(|root| self.packages.contains_key(root))
    }

    /// The live text of the open document at `path`, shared by every text
    /// request until the document changes (A §3.4).
    fn live_text(&mut self, path: &Path) -> Option<Arc<LiveText>> {
        let docs = &self.docs;
        self.texts
            .retain(|path, (stamp, _)| docs.get(path).is_some_and(|doc| doc.stamp == *stamp));
        if let Some((_, live)) = self.texts.get(path) {
            return Some(Arc::clone(live));
        }
        let doc = self.docs.get_mut(path)?;
        let live = Arc::new(LiveText::new(
            path.to_path_buf(),
            Some(doc.version),
            doc.shared(),
        ));
        self.texts
            .insert(path.to_path_buf(), (doc.stamp, Arc::clone(&live)));
        Some(live)
    }

    /// What a request reads besides its target, as it is now.
    fn env(&self) -> Arc<RequestEnv> {
        Arc::new(RequestEnv {
            settings: Arc::clone(&self.settings),
            caps: self.caps.clone(),
            encoding: self.encoding,
            std: Arc::clone(&self.std),
        })
    }

    /// Sends a request to the pool.
    pub(crate) fn start_query(
        &mut self,
        id: RequestId,
        route: &'static Route,
        params: Value,
        target: QueryTarget,
        env: Arc<RequestEnv>,
    ) {
        let cancel = CancelToken::new();
        let job = QueryJob {
            id: id.clone(),
            route,
            params,
            target,
            env,
            cancel: cancel.clone(),
        };
        if self.pool.send(job).is_ok() {
            self.in_pool.insert(id, cancel);
        } else {
            let error = RequestError::Failed("the query threads have stopped".to_owned());
            self.answer(id, Err(error));
        }
    }

    /// A handler is done: its effects first, then its response (plan §1.5).
    /// A panic is answered `InternalError` and logged (A §3.7).
    pub(crate) fn on_query_done(&mut self, done: QueryDone) {
        let QueryDone { id, method, result } = done;
        self.in_pool.remove(&id);
        match result {
            Ok(output) => {
                for effect in output.effects {
                    self.apply(effect);
                }
                self.answer(id, Ok(output.value));
            }
            Err(QueryFailure::Error(error)) => self.answer(id, Err(error)),
            Err(QueryFailure::Panic(message)) => {
                self.log(
                    MessageType::ERROR,
                    format!("`{method}` panicked: {message}"),
                );
                let response = Response::new_err(
                    id,
                    codes::INTERNAL_ERROR,
                    format!("`{method}` failed with an internal error: {message}"),
                );
                self.send(response.into());
            }
        }
    }

    /// `$/cancelRequest` (A §3.5): a parked request is answered
    /// `RequestCancelled` at once, a running one has its token set, and
    /// one already answered or unknown is ignored.
    pub(crate) fn on_cancel_request(&mut self, params: &Value) {
        let Some(id) = params
            .get("id")
            .and_then(|id| serde_json::from_value::<RequestId>(id.clone()).ok())
        else {
            return;
        };
        if let Some(parked) = self.unpark(&id) {
            self.answer(parked.id, Err(RequestError::Cancelled));
        } else if let Some(cancel) = self.in_pool.get(&id) {
            cancel.cancel();
        }
    }

    /// Sends the response to a request served off the main loop.
    pub(crate) fn answer(&mut self, id: RequestId, result: Result<Value, RequestError>) {
        let response = match result {
            Ok(value) => Response::new_ok(id, value),
            Err(error) => Response {
                id,
                response_result: Err(error.into_response()),
            },
        };
        self.send(response.into());
    }
}

/// The normalized path of `params.textDocument.uri`, or for a call
/// hierarchy's calls of the document the item was prepared in,
/// `params.item.data.origin`, else `params.item.uri`: `Ok(None)` for a URI
/// that names no file, `Err` for malformed parameters.
fn document_path(params: &Value) -> Result<Option<PathBuf>, ()> {
    let uri: Uri = params
        .pointer("/textDocument/uri")
        .or_else(|| {
            params
                .pointer("/item/data/origin")
                .filter(|v| v.is_string())
        })
        .or_else(|| params.pointer("/item/uri"))
        .and_then(Value::as_str)
        .ok_or(())?
        .parse()
        .map_err(|_| ())?;
    Ok(uri_to_path(&uri).map(|path| normalize(&path)))
}
