//! Debounce, jobs and parked requests (A §3.2, §4).
//!
//! An edit starts a package's debounce. Steady typing moves the deadline,
//! but never past five debounces after the first edit, so a long burst
//! still refreshes. An open, close or save flushes the debounce. When the
//! analysis thread is free, the due package holding the document edited
//! last gets the next job. One job runs at a time, so a package's jobs
//! never overlap, and an edit during a job runs again after it.
//!
//! A request answered from a snapshot waits for an analysis of the exact
//! text it names, and positions are never mapped (D-1367). The text is
//! named by the documents' edit stamps, not their versions (D-1445): a
//! stamp grows with every open and edit, while Zed starts a reopened
//! document at version 0 again (cp-notes, CP1), so stamps order texts
//! where versions cannot. A request waits only for the texts it names,
//! never for a job of a save, a settings change, a sweep or a watched
//! file, so it may be answered from the snapshot before such a job
//! (D-1443). A client that must know a job has finished waits for
//! `cheby/idle` (D-1415), which a later wave brings. A request whose text
//! is not analyzed yet is parked and flushes its package's debounce. Each
//! landing of a run then decides it: the run's texts are the request's,
//! so it runs on that snapshot, even if the document has moved on since;
//! one of them is newer, so it is answered `ContentModified`; or they are
//! older, so it waits on. A run that gives no snapshot answers the
//! requests it covered with their fallback (A §3.7), and a package with no
//! run to come answers the rest the same way. A wait past
//! `analysis.requestWaitMs` is `RequestFailed`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cheby_driver::analysis::{Overlay, OverlayFile, PackageKind, Snapshot};
use lsp_server::RequestId;
use serde_json::Value;

use super::Server;
use super::pool::{QueryTarget, RequestEnv};
use crate::analysis_thread::{Changed, Job, Work};
use crate::features::{Analyzed, RequestError, Route};

/// Where a package is in its runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunState {
    /// Nothing to do.
    Idle,
    /// A run is due at `deadline`.
    Debouncing {
        deadline: Instant,
        /// The first change of this burst, which caps the deadline.
        first_dirty: Instant,
    },
    /// A job is on the analysis thread.
    Running {
        /// A change came during the job, so another run follows it.
        dirty_again: bool,
    },
}

/// How a change wants its run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Urgency {
    /// After the debounce: an edit.
    Debounce,
    /// At once: open, close, save, settings.
    Flush,
}

impl Server {
    /// Marks the package at `root` as changed, with what changed on disk.
    pub(crate) fn mark_dirty(&mut self, root: &Path, changed: Changed, urgency: Urgency) {
        let debounce = self.settings.debounce();
        // A test that must see a flush keeps the debounce from ending.
        #[cfg(test)]
        let debounce = if self.test.no_debounce {
            Duration::from_hours(24)
        } else {
            debounce
        };
        let Some(package) = self.packages.get_mut(root) else {
            return;
        };
        package.changed.merge(changed);
        let now = Instant::now();
        package.run = match package.run {
            RunState::Idle => RunState::Debouncing {
                deadline: match urgency {
                    Urgency::Flush => now,
                    Urgency::Debounce => now.checked_add(debounce).unwrap_or(now),
                },
                first_dirty: now,
            },
            RunState::Debouncing { first_dirty, .. } => {
                let deadline = match urgency {
                    Urgency::Flush => now,
                    Urgency::Debounce => {
                        let next = now.checked_add(debounce).unwrap_or(now);
                        let cap = debounce
                            .checked_mul(5)
                            .and_then(|cap| first_dirty.checked_add(cap))
                            .unwrap_or(next);
                        next.min(cap)
                    }
                };
                RunState::Debouncing {
                    deadline,
                    first_dirty,
                }
            }
            RunState::Running { .. } => {
                // The job's D run is wasted work now: a newer job follows.
                package.superseded.store(true, Ordering::Relaxed);
                RunState::Running { dirty_again: true }
            }
        };
    }

    /// Marks every package as changed, to run at once.
    pub(crate) fn mark_all_dirty(&mut self) {
        let roots: Vec<PathBuf> = self.packages.keys().cloned().collect();
        for root in roots {
            self.mark_dirty(&root, Changed::default(), Urgency::Flush);
        }
    }

    /// The next deadline of the loop's timer: a debounce, while the
    /// analysis thread is free, or a parked request's wait.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let debounce = if self.running.is_some() || self.held() {
            None
        } else {
            self.packages
                .values()
                .filter_map(|package| match package.run {
                    RunState::Debouncing { deadline, .. } => Some(deadline),
                    RunState::Idle | RunState::Running { .. } => None,
                })
                .min()
        };
        let parked = self.parked.iter().map(|parked| parked.deadline).min();
        debounce.into_iter().chain(parked).min()
    }

    /// Starts a job if the analysis thread is free and a package is due.
    pub(crate) fn schedule(&mut self) {
        if self.running.is_some() || self.shutting_down || self.held() {
            return;
        }
        let now = Instant::now();
        let due = self
            .packages
            .values()
            .filter(|package| {
                matches!(package.run, RunState::Debouncing { deadline, .. } if deadline <= now)
            })
            .max_by_key(|package| package.last_edit)
            .map(|package| package.root.clone());
        let Some(root) = due else {
            return;
        };
        let kind = self
            .packages
            .get(&root)
            .map_or(PackageKind::Package, |package| package.kind.clone());
        let overlay = self.overlay(&root);
        let target = self.settings.analysis.target;
        let texts = JobTexts {
            stamps: overlay
                .files
                .iter()
                .map(|(path, file)| (path.clone(), file.stamp))
                .collect(),
            started: self.stamp,
        };
        let Some(package) = self.packages.get_mut(&root) else {
            return;
        };
        let superseded = Arc::new(AtomicBool::new(false));
        package.superseded = Arc::clone(&superseded);
        package.job_versions = overlay
            .files
            .iter()
            .map(|(path, file)| (path.clone(), file.version))
            .collect();
        package.run = RunState::Running { dirty_again: false };
        let job = Job {
            root: root.clone(),
            kind,
            target,
            overlay,
            changed: std::mem::take(&mut package.changed),
            superseded,
            #[cfg(test)]
            panic: std::mem::take(&mut self.test.panic_next),
        };
        if self.jobs.send(Work::Run(job)).is_err() {
            // The analysis thread is gone; nothing will run again.
            package.run = RunState::Idle;
            self.log_error("the analysis thread has stopped; diagnostics will not refresh");
            // No run will come, so the package's parked requests get their
            // fallback now.
            self.wake(&root);
            return;
        }
        self.in_flight = Some(texts);
        self.running = Some(root);
    }

    /// The package at `root` had the last event of its job.
    pub(crate) fn job_done(&mut self, root: &Path) {
        if self.running.as_deref() == Some(root) {
            self.running = None;
        }
        let Some(package) = self.packages.get_mut(root) else {
            return;
        };
        package.job_versions.clear();
        package.run = match package.run {
            RunState::Running { dirty_again: true } => {
                let now = Instant::now();
                RunState::Debouncing {
                    deadline: now,
                    first_dirty: now,
                }
            }
            RunState::Running { dirty_again: false } | RunState::Idle => RunState::Idle,
            debouncing @ RunState::Debouncing { .. } => debouncing,
        };
    }

    /// The open documents of the package at `root`, as the job's overlay.
    fn overlay(&mut self, root: &Path) -> Overlay {
        let files: BTreeMap<PathBuf, OverlayFile> = self
            .docs
            .iter_mut()
            .filter(|(_, doc)| doc.root.as_deref() == Some(root))
            .map(|(path, doc)| {
                let file = OverlayFile {
                    text: doc.shared(),
                    version: doc.version,
                    stamp: doc.stamp,
                };
                (path.clone(), file)
            })
            .collect();
        Overlay { files }
    }
}

/// The exact texts a request names: each open document it reads, by path,
/// with the edit stamp it had when the request arrived (A §4, D-1445). A
/// closed file is read from disk, so it names no text and any snapshot
/// holds it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Want {
    /// Stamps by normalized path.
    pub texts: BTreeMap<PathBuf, u64>,
}

/// The documents' texts one job analyzed: the stamps of its overlay, and
/// the newest stamp when it started.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct JobTexts {
    /// The stamp of each open document in the job's overlay.
    pub stamps: BTreeMap<PathBuf, u64>,
    /// The newest stamp of any document when the job started.
    pub started: u64,
}

/// How a run's texts stand to a request's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The run read every text the request names.
    Ready,
    /// The run read a newer text of a document the request names.
    Stale,
    /// The run read an older text, so a later run may be the one.
    Wait,
}

impl JobTexts {
    /// How this job's texts stand to `want`'s. One newer text decides, since
    /// the request's text of that document can no longer be analyzed.
    pub(crate) fn judge(&self, want: &Want) -> Verdict {
        let mut verdict = Verdict::Ready;
        for (path, &wanted) in &want.texts {
            let this = match self.stamps.get(path) {
                Some(&ran) if ran == wanted => Verdict::Ready,
                Some(&ran) if ran > wanted => Verdict::Stale,
                // Not in the overlay of a job that started after this
                // text: the document was closed or left the package, so
                // the run read newer text from disk.
                None if self.started >= wanted => Verdict::Stale,
                Some(_) | None => Verdict::Wait,
            };
            match this {
                Verdict::Stale => return Verdict::Stale,
                Verdict::Wait => verdict = Verdict::Wait,
                Verdict::Ready => {}
            }
        }
        verdict
    }
}

/// A package's newest snapshot as requests read it, with the texts its
/// run read.
#[derive(Debug)]
pub(crate) struct Served {
    /// The snapshot and its memos.
    pub analyzed: Arc<Analyzed>,
    /// The texts its run read.
    pub texts: JobTexts,
}

/// A request waiting for an analysis of its text (A §4).
#[derive(Debug)]
pub(crate) struct Parked {
    /// The request.
    pub id: RequestId,
    /// Its route.
    pub route: &'static Route,
    /// Its parameters.
    pub params: Value,
    /// The package whose runs can answer it.
    pub root: PathBuf,
    /// The texts it names.
    pub want: Want,
    /// When `analysis.requestWaitMs` runs out.
    pub deadline: Instant,
    /// What its handler reads besides the snapshot, as it was on arrival.
    pub env: Arc<RequestEnv>,
}

impl Server {
    /// A request answered from a snapshot of the package at `root`: on the
    /// pool now when the newest snapshot holds its texts, else parked, with
    /// the package's debounce flushed (A §4). It waits for those texts only,
    /// never for a job that no edit started (D-1443).
    /// A request on exactly the texts of the package's last run, which
    /// gave no snapshot, gets its fallback without a run (A §3.7).
    pub(crate) fn want_snapshot(&mut self, parked: Parked) {
        match self.verdict(&parked.root, &parked.want) {
            Verdict::Ready => self.run_parked(parked),
            Verdict::Stale => self.answer(parked.id, Err(RequestError::ContentModified)),
            Verdict::Wait => {
                let failed = self
                    .packages
                    .get(&parked.root)
                    .and_then(|package| package.failed.as_ref())
                    .map(|texts| texts.judge(&parked.want));
                match failed {
                    Some(Verdict::Ready) => {
                        let fallback = (parked.route.fallback)();
                        self.answer(parked.id, Ok(fallback));
                    }
                    Some(Verdict::Stale) => {
                        self.answer(parked.id, Err(RequestError::ContentModified));
                    }
                    Some(Verdict::Wait) | None => {
                        let root = parked.root.clone();
                        self.parked.push(parked);
                        self.nudge(&root);
                    }
                }
            }
        }
    }

    /// How the newest snapshot of `root` stands to `want`.
    fn verdict(&self, root: &Path, want: &Want) -> Verdict {
        self.served
            .get(root)
            .map_or(Verdict::Wait, |served| served.texts.judge(want))
    }

    /// Makes the package at `root` run soon, for a parked request: its
    /// debounce ends now. A package that is running needs nothing, since a
    /// change during the job runs again after it. An idle one runs once
    /// more, in case its last run failed or it never ran.
    fn nudge(&mut self, root: &Path) {
        let running = self
            .packages
            .get(root)
            .is_none_or(|package| matches!(package.run, RunState::Running { .. }));
        if !running {
            self.mark_dirty(root, Changed::default(), Urgency::Flush);
        }
    }

    /// A Q run of `root` landed. `snapshot` is its snapshot, `None` when it
    /// gave none, and `texts` what it read.
    pub(crate) fn landed(
        &mut self,
        root: &Path,
        snapshot: Option<Arc<Snapshot>>,
        texts: Option<JobTexts>,
    ) {
        let texts = texts.unwrap_or_default();
        let Some(package) = self.packages.get_mut(root) else {
            // The session ended while the job ran.
            self.wake(root);
            return;
        };
        package.failed = snapshot.is_none().then(|| texts.clone());
        if let Some(snapshot) = snapshot {
            let same = self
                .served
                .get(root)
                .is_some_and(|served| Arc::ptr_eq(&served.analyzed.snapshot, &snapshot));
            if !same {
                let served = Served {
                    analyzed: Arc::new(Analyzed::new(snapshot)),
                    texts,
                };
                self.served.insert(root.to_path_buf(), served);
            }
        } else {
            // The run of these texts failed, so their snapshot will
            // not appear: the requests it covered get their fallback
            // (A §3.7).
            let (covered, rest): (Vec<Parked>, Vec<Parked>) = std::mem::take(&mut self.parked)
                .into_iter()
                .partition(|parked| {
                    parked.root == root && texts.judge(&parked.want) != Verdict::Wait
                });
            self.parked = rest;
            for parked in covered {
                let answer = match texts.judge(&parked.want) {
                    Verdict::Stale => Err(RequestError::ContentModified),
                    Verdict::Ready | Verdict::Wait => Ok((parked.route.fallback)()),
                };
                self.answer(parked.id, answer);
            }
        }
        self.wake(root);
    }

    /// Decides the parked requests of `root` against its newest snapshot,
    /// after each event of its jobs. A request still waiting when the
    /// package has no run to come gets its fallback, since its text will
    /// not be analyzed.
    pub(crate) fn wake(&mut self, root: &Path) {
        let idle = self
            .packages
            .get(root)
            .is_none_or(|package| package.run == RunState::Idle);
        let parked = std::mem::take(&mut self.parked);
        for request in parked {
            if request.root != root {
                self.parked.push(request);
                continue;
            }
            match self.verdict(root, &request.want) {
                Verdict::Ready => self.run_parked(request),
                Verdict::Stale => self.answer(request.id, Err(RequestError::ContentModified)),
                Verdict::Wait if idle => {
                    let fallback = (request.route.fallback)();
                    self.answer(request.id, Ok(fallback));
                }
                Verdict::Wait => self.parked.push(request),
            }
        }
    }

    /// Sends a request whose texts the newest snapshot holds to the pool.
    fn run_parked(&mut self, parked: Parked) {
        let Some(served) = self.served.get(&parked.root) else {
            let fallback = (parked.route.fallback)();
            self.answer(parked.id, Ok(fallback));
            return;
        };
        let target = QueryTarget::Snapshot(Arc::clone(&served.analyzed));
        let env = self.with_constants(parked.env, &parked.root);
        self.start_query(parked.id, parked.route, parked.params, target, env);
    }

    /// Answers each parked request whose wait ran out `RequestFailed`
    /// (D-1367).
    pub(crate) fn expire_parked(&mut self) {
        if self.parked.is_empty() {
            return;
        }
        let now = Instant::now();
        let (expired, waiting): (Vec<Parked>, Vec<Parked>) = std::mem::take(&mut self.parked)
            .into_iter()
            .partition(|parked| parked.deadline <= now);
        self.parked = waiting;
        let wait = self.settings.analysis.request_wait_ms;
        for parked in expired {
            let message = format!(
                "`{}`: the document was not analyzed within analysis.requestWaitMs ({wait} ms)",
                parked.route.method
            );
            self.answer(parked.id, Err(RequestError::Failed(message)));
        }
    }

    /// The deadline of a request parked now.
    pub(crate) fn park_deadline(&self) -> Instant {
        let wait = Duration::from_millis(self.settings.analysis.request_wait_ms);
        let now = Instant::now();
        now.checked_add(wait).unwrap_or(now)
    }

    /// Removes the parked request `id`, if it is parked.
    pub(crate) fn unpark(&mut self, id: &RequestId) -> Option<Parked> {
        let index = self.parked.iter().position(|parked| parked.id == *id)?;
        Some(self.parked.remove(index))
    }

    /// Answers every parked request `RequestCancelled`, at `shutdown`
    /// (A §3.2).
    pub(crate) fn cancel_parked(&mut self) {
        for parked in std::mem::take(&mut self.parked) {
            self.answer(parked.id, Err(RequestError::Cancelled));
        }
    }

    /// Whether a test holds the analysis thread, so that it starts no job.
    #[cfg(test)]
    const fn held(&self) -> bool {
        self.test.hold
    }

    /// Nothing holds the analysis thread outside tests.
    #[cfg(not(test))]
    #[expect(clippy::unused_self, reason = "the test build reads a field")]
    const fn held(&self) -> bool {
        false
    }
}

#[cfg(test)]
impl Server {
    /// Main-loop requests that only tests send: `cheby/testHold` with
    /// `{ "hold": bool }` holds the analysis thread, so that a test decides
    /// when the next job starts, `cheby/testPanicRun` makes the next job's
    /// Q run panic (R-14), `cheby/testNoDebounce` keeps every debounce
    /// from ending, so that only a flush starts a run, and
    /// `cheby/testSessions` answers the root of every session, sorted.
    pub(crate) fn test_request(
        &mut self,
        method: &str,
        params: &Value,
    ) -> Option<Result<Value, lsp_server::ResponseError>> {
        match method {
            "cheby/testHold" => {
                self.test.hold = params["hold"].as_bool().unwrap_or(false);
                Some(Ok(Value::Null))
            }
            "cheby/testPanicRun" => {
                self.test.panic_next = true;
                Some(Ok(Value::Null))
            }
            "cheby/testNoDebounce" => {
                self.test.no_debounce = true;
                Some(Ok(Value::Null))
            }
            "cheby/testSessions" => {
                let roots: Vec<String> = self
                    .packages
                    .keys()
                    .map(|root| root.display().to_string())
                    .collect();
                Some(Ok(serde_json::json!(roots)))
            }
            _ => None,
        }
    }
}

#[cfg(not(test))]
impl Server {
    /// No test requests outside tests.
    #[expect(clippy::unused_self, reason = "the test build has some")]
    pub(crate) const fn test_request(
        &self,
        _method: &str,
        _params: &Value,
    ) -> Option<Result<Value, lsp_server::ResponseError>> {
        None
    }
}
