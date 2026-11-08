//! The one analysis thread, `cheby-analysis` (A §3.1, §3.2).
//!
//! It owns every [`Session`], one per package root, and runs one [`Job`]
//! at a time: phase A and the Q run on the job's overlay, then the D run
//! when the Q run plans one and no newer change superseded the job
//! (D-1347). Each result goes back to the main loop as an [`Event`]. The
//! main loop sends a package's next job only after that job's last event,
//! so jobs of one package never overlap.
//!
//! A panic in a run is caught here, so it ends that run only (A §3.7). A
//! `Session` changes its own state only at commit points after a run, so
//! it stays usable after one. A panic while it forgets what changed on
//! disk, before the run, is caught too, and that session is dropped.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cheby_driver::analysis::{Globals, Live, Overlay, Report, Session, SessionConfig, Target};
use crossbeam_channel::{Receiver, Sender};

/// The analysis thread's stack. The compiler recurses over trees, so it
/// gets the size of the command thread and the worker pool (D-601).
const STACK_SIZE: usize = 256 << 20;

/// What changed on disk since a package's last job.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Changed {
    /// Stat every known file and walk `src/` again (A §8.3): at open,
    /// close and save, since no watcher reports changes yet.
    pub sweep: bool,
    /// Forget what was read from these absolute paths.
    pub paths: Vec<PathBuf>,
}

impl Changed {
    /// Folds `other` into `self`.
    pub(crate) fn merge(&mut self, other: Self) {
        self.sweep |= other.sweep;
        for path in other.paths {
            if !self.paths.contains(&path) {
                self.paths.push(path);
            }
        }
    }
}

/// One run of one package.
#[derive(Debug)]
pub(crate) struct Job {
    /// The package root, which names its session.
    pub root: PathBuf,
    /// `analysis.target`: a session with another target is replaced.
    pub target: Option<Target>,
    /// The open documents of the package.
    pub overlay: Overlay,
    /// What to forget before the run.
    pub changed: Changed,
    /// Set by the main loop at the package's next change: the D run is
    /// then skipped, since a newer job follows.
    pub superseded: Arc<AtomicBool>,
}

/// Why a run gave no result.
#[derive(Debug)]
pub(crate) enum RunFailure {
    /// An error of the driver, such as a package that cannot be read.
    Error(String),
    /// A panic, with its message.
    Panic(String),
}

impl RunFailure {
    /// The failure as one line for `window/logMessage`.
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Error(message) | Self::Panic(message) => message,
        }
    }
}

/// What the analysis thread tells the main loop.
#[derive(Debug)]
pub(crate) enum Event {
    /// The Q run of a job, with its plan taken out.
    Live {
        /// The package root.
        root: PathBuf,
        /// The run, or why it gave nothing.
        result: Result<Box<Live>, RunFailure>,
        /// A [`Event::Strict`] of the same job follows.
        strict_follows: bool,
    },
    /// The D run of a job, its last event.
    Strict {
        /// The package root.
        root: PathBuf,
        /// The run's report, or why it gave none.
        result: Result<Report, RunFailure>,
    },
}

/// Starts the analysis thread. It ends when `jobs` is dropped, after the
/// job it is running.
///
/// # Errors
///
/// The error of a thread that cannot be started.
pub(crate) fn spawn(globals: Arc<Globals>) -> std::io::Result<(Sender<Job>, Receiver<Event>)> {
    let (job_tx, job_rx) = crossbeam_channel::unbounded::<Job>();
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<Event>();
    std::thread::Builder::new()
        .name("cheby-analysis".to_owned())
        .stack_size(STACK_SIZE)
        .spawn(move || run(&job_rx, &event_tx, &globals))?;
    Ok((job_tx, event_rx))
}

/// A session with the target it was made for.
struct Entry {
    session: Session,
    target: Option<Target>,
}

/// The job loop.
fn run(jobs: &Receiver<Job>, events: &Sender<Event>, globals: &Arc<Globals>) {
    let mut sessions: BTreeMap<PathBuf, Entry> = BTreeMap::new();
    for job in jobs {
        let fresh = || Entry {
            session: Session::new(SessionConfig {
                root: job.root.clone(),
                target: job.target,
                globals: Arc::clone(globals),
            }),
            target: job.target,
        };
        let entry = sessions.entry(job.root.clone()).or_insert_with(fresh);
        if entry.target != job.target {
            *entry = fresh();
        }
        let session = &mut entry.session;
        // Forgetting changes the session's state before any commit point,
        // so a panic there may leave it half changed: it is dropped, and
        // the package's next job starts a fresh one.
        let forgot = catch(|| {
            if !job.changed.paths.is_empty() {
                session.invalidate(&job.changed.paths);
            }
            if job.changed.sweep {
                // The answer only says whether anything changed; the run
                // below happens either way, and reads what the sweep forgot.
                let _ = session.sweep();
            }
            Ok(())
        });
        if let Err(failure) = forgot {
            sessions.remove(&job.root);
            let sent = events.send(Event::Live {
                root: job.root.clone(),
                result: Err(failure),
                strict_follows: false,
            });
            if sent.is_err() {
                return;
            }
            continue;
        }
        let live = catch(|| session.analyze_live(&job.overlay));
        let (result, plan) = match live {
            Ok(mut live) => {
                let plan = live.plan.take();
                (Ok(Box::new(live)), plan)
            }
            Err(failure) => (Err(failure), None),
        };
        let superseded = job.superseded.load(Ordering::Relaxed);
        if plan.is_some() && superseded {
            session.forget_strict_key();
        }
        let strict = plan.filter(|_| !superseded);
        let sent = events.send(Event::Live {
            root: job.root.clone(),
            result,
            strict_follows: strict.is_some(),
        });
        if sent.is_err() {
            return;
        }
        if let Some(plan) = strict {
            let result = catch(|| session.analyze_strict(plan));
            if result.is_err() {
                session.forget_strict_key();
            }
            let sent = events.send(Event::Strict {
                root: job.root.clone(),
                result,
            });
            if sent.is_err() {
                return;
            }
        }
    }
}

/// Runs `f`, turning a driver error or a panic into a [`RunFailure`].
fn catch<T>(f: impl FnOnce() -> Result<T, cheby_driver::Error>) -> Result<T, RunFailure> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(RunFailure::Error(error.message().to_owned())),
        Err(payload) => {
            let text = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("a panic with no message");
            Err(RunFailure::Panic(format!("the analysis panicked: {text}")))
        }
    }
}
