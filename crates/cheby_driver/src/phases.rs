//! The phase timer behind the hidden `CHEBY_PHASES` variable (D-974,
//! GAP(phases-format)).
//!
//! When `CHEBY_PHASES` names a file, `cheby check`, `run`, `test` and
//! `build` time each phase of the command on the command's thread and
//! write the file at the command's end ([`finish`]). The phases are
//! exclusive: a [`span`] pauses the span it opens inside, and [`add`]
//! moves a time measured elsewhere (the JIT's load times) out of the
//! innermost open span, so nothing counts twice and the phases add up to
//! at most `command`.
//!
//! The file is ASCII with `\n` line ends: the first line is
//! `cheby-phases 1 <command>`, then one line `<phase> <nanoseconds>
//! <spans>` per phase that ran, in [`Phase`] order, then `command
//! <nanoseconds>`. `<spans>` counts the spans that closed and the times
//! moved in.
//!
//! - `command` runs from the [`command`] guard's start (the CLI's command
//!   function's first statement) to [`finish`].
//! - `restart` is the elapsed time of a warm command's first attempt that
//!   started again (D-1256, [`begin_attempt`] and [`attempt_restarted`],
//!   or an [`attempt`] guard): what its spans recorded no longer counts
//!   under their phases, so the second attempt's work alone counts under
//!   `parse`, `check` and the rest.
//! - `codegen`, `declare`, `define`, `finalize` and `register` come from
//!   the JIT's own clocks (`cheby_jit::LoadTimes`, [`add_load`]), moved out
//!   of the span around the batch's loading.
//! - `teardown` is the rest of the command after a top-level `main` span
//!   closed: the JIT's code freed and the exit path.
//!
//! The variable is read once per process. Unset, every function returns
//! after one atomic load: no clock is read and nothing is written.
//! `CHEBY_PHASES` is no setting and no key input (D-958): nothing here
//! reaches a cache key.
//!
//! Only the thread that opens the first span (the command's thread)
//! records; a span or an `add` on another thread is a no-op, so the
//! parallel sections are measured around, from the command's thread.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

/// A phase of a command (D-974), timed only when `CHEBY_PHASES` names a
/// file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    /// Hashing the package's sources (the front end's keys).
    Hash,
    /// Reading and writing the package index.
    Index,
    /// Parsing modules (the package's and, cold, std's).
    Parse,
    /// Resolving and type-checking modules.
    Check,
    /// Std's records: their first use and their reads.
    Std,
    /// Evaluating constants or reading their entries.
    Consts,
    /// Lowering to MIR.
    Lower,
    /// Generating machine code for fresh units (the JIT's `codegen`).
    Codegen,
    /// Reading and writing code records.
    Records,
    /// Declaring the units' functions and data in the JIT.
    Declare,
    /// Defining the units' functions and data in the JIT.
    Define,
    /// Finalizing the JIT's definitions.
    Finalize,
    /// Registering the loaded code with the runtime.
    Register,
    /// The program's `main`.
    Main,
    /// Tearing down after `main`.
    Teardown,
    /// Writing the cache's index and records at the end.
    Write,
    /// The first attempt of a warm command that started again (D-1256).
    Restart,
    /// Linking an executable.
    Link,
}

impl Phase {
    /// Every phase, in the file's order.
    pub const ALL: [Phase; 18] = [
        Phase::Hash,
        Phase::Index,
        Phase::Parse,
        Phase::Check,
        Phase::Std,
        Phase::Consts,
        Phase::Lower,
        Phase::Codegen,
        Phase::Records,
        Phase::Declare,
        Phase::Define,
        Phase::Finalize,
        Phase::Register,
        Phase::Main,
        Phase::Teardown,
        Phase::Write,
        Phase::Restart,
        Phase::Link,
    ];

    /// The phase's name in the file.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Phase::Hash => "hash",
            Phase::Index => "index",
            Phase::Parse => "parse",
            Phase::Check => "check",
            Phase::Std => "std",
            Phase::Consts => "consts",
            Phase::Lower => "lower",
            Phase::Codegen => "codegen",
            Phase::Records => "records",
            Phase::Declare => "declare",
            Phase::Define => "define",
            Phase::Finalize => "finalize",
            Phase::Register => "register",
            Phase::Main => "main",
            Phase::Teardown => "teardown",
            Phase::Write => "write",
            Phase::Restart => "restart",
            Phase::Link => "link",
        }
    }
}

/// The file the variable names, read once per process.
static TARGET: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The command's recorder, created by the first call that records.
static RECORDER: Mutex<Option<Recorder>> = Mutex::new(None);

/// The file `CHEBY_PHASES` names: `None` when it is unset or empty.
pub(crate) fn target_from(var: Option<OsString>) -> Option<PathBuf> {
    var.filter(|value| !value.is_empty()).map(PathBuf::from)
}

fn target() -> Option<&'static PathBuf> {
    TARGET
        .get_or_init(|| target_from(std::env::var_os("CHEBY_PHASES")))
        .as_ref()
}

fn recorder() -> MutexGuard<'static, Option<Recorder>> {
    RECORDER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether the process has recorded nothing (tests: the unset variable).
#[cfg(test)]
pub(crate) fn recorded_nothing() -> bool {
    target().is_none() && recorder().is_none()
}

/// Runs `f` on the recorder when timing is on and the calling thread is
/// the command's; the first such call creates the recorder.
fn with_recorder<R>(f: impl FnOnce(&mut Recorder, Instant) -> R) -> Option<R> {
    target()?;
    let now = Instant::now();
    let here = std::thread::current().id();
    let mut guard = recorder();
    let recorder = guard.get_or_insert_with(|| Recorder::new(here, now));
    (recorder.owner == here).then(|| f(recorder, now))
}

/// Starts `phase` on the calling thread's command; the guard pauses the
/// enclosing span and adds its own elapsed time to `phase` when dropped.
/// A no-op (no clock read) when `CHEBY_PHASES` is unset.
pub fn span(phase: Phase) -> Span {
    let depth = with_recorder(|recorder, now| recorder.open(phase, now));
    Span {
        depth,
        _thread: PhantomData,
    }
}

/// The guard of [`span`].
#[must_use]
#[derive(Debug)]
pub struct Span {
    /// The stack depth the span opened at; `None` when nothing records.
    depth: Option<usize>,
    /// A span closes on the thread that opened it.
    _thread: PhantomData<*const ()>,
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(depth) = self.depth {
            with_recorder(|recorder, now| recorder.close(depth, now));
        }
    }
}

/// Moves a time measured elsewhere (the JIT's `LoadTimes`) out of the
/// innermost open span into `phase`: the enclosing phase shrinks by
/// exactly `time`, so nothing counts twice.
pub fn add(phase: Phase, time: Duration) {
    with_recorder(|recorder, _| recorder.add(phase, time));
}

/// Starts a first attempt that may have to start again (D-1256): the
/// phases its spans record count as usual unless [`Attempt::restarted`]
/// moves its whole elapsed time into [`Phase::Restart`].
pub fn attempt() -> Attempt {
    let mark = with_recorder(Recorder::mark);
    Attempt {
        mark,
        _thread: PhantomData,
    }
}

/// The guard of [`attempt`].
#[must_use]
#[derive(Debug)]
pub struct Attempt {
    mark: Option<Mark>,
    _thread: PhantomData<*const ()>,
}

impl Attempt {
    /// The attempt started again: its elapsed time is `restart`, and what
    /// its spans recorded no longer counts under their phases.
    pub fn restarted(self) {
        if let Some(mark) = self.mark {
            with_recorder(|recorder, now| recorder.restarted(&mark, now));
        }
    }
}

/// Marks the start of a warm command's attempt (D-1256) that a later
/// [`attempt_restarted`] may move into [`Phase::Restart`]: the form of
/// [`attempt`] for an attempt whose end is decided elsewhere (the log's
/// mark and rollback, `log_mark` and `rollback_log`). A later mark
/// replaces it.
pub fn begin_attempt() {
    with_recorder(|recorder, now| recorder.attempt = Some(recorder.mark(now)));
}

/// The attempt [`begin_attempt`] marked started again: as
/// [`Attempt::restarted`]. Nothing without a mark.
pub fn attempt_restarted() {
    with_recorder(|recorder, now| {
        if let Some(mark) = recorder.attempt.take() {
            recorder.restarted(&mark, now);
        }
    });
}

/// Moves the JIT's load times (`Jit::load_times`) out of the innermost
/// open span: `codegen`, `declare`, `define`, `finalize` and `register`,
/// each as [`add`] does.
pub fn add_load(times: cheby_jit::LoadTimes) {
    with_recorder(|recorder, _| {
        for (phase, time) in [
            (Phase::Codegen, times.codegen),
            (Phase::Declare, times.declare),
            (Phase::Define, times.define),
            (Phase::Finalize, times.finalize),
            (Phase::Register, times.register),
        ] {
            if !time.is_zero() {
                recorder.add(phase, time);
            }
        }
    });
}

/// Writes the file once per process at the command's end: `command` is
/// `check`, `run`, `test` or `build`. A write that fails is ignored
/// (D-1129's spirit: a measurement never fails a command).
pub fn finish(command: &str) {
    let Some(path) = target() else {
        return;
    };
    let text = {
        let now = Instant::now();
        let mut guard = recorder();
        let recorder = guard.get_or_insert_with(|| Recorder::new(std::thread::current().id(), now));
        if recorder.finished {
            return;
        }
        recorder.finished = true;
        recorder.render(command, now)
    };
    // A measurement never fails a command.
    let _ = std::fs::write(path, text);
}

/// Starts the command `name` on the calling thread (its clock starts
/// here) and calls [`finish`] with `name` when the guard drops, whichever
/// way the command returns. A no-op when `CHEBY_PHASES` is unset.
pub fn command(name: &'static str) -> Command {
    let started = with_recorder(|_, _| ()).is_some();
    Command {
        name: started.then_some(name),
        _thread: PhantomData,
    }
}

/// The guard of [`command`].
#[must_use]
#[derive(Debug)]
pub struct Command {
    name: Option<&'static str>,
    _thread: PhantomData<*const ()>,
}

impl Drop for Command {
    fn drop(&mut self) {
        if let Some(name) = self.name {
            finish(name);
        }
    }
}

/// What a phase recorded: signed nanoseconds, since [`add`] may move time
/// out of a span that is still open, and the spans that closed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Total {
    pub(crate) nanos: i128,
    pub(crate) spans: u64,
}

/// An open span on the stack.
#[derive(Clone, Copy, Debug)]
struct Frame {
    phase: Phase,
    /// When the frame last became the innermost one.
    resumed: Instant,
}

/// The state an [`Attempt`] started from.
#[derive(Clone, Debug)]
pub(crate) struct Mark {
    at: Instant,
    totals: BTreeMap<Phase, Total>,
}

/// The command's timeline, driven by explicit instants so that tests can
/// replay one.
#[derive(Debug)]
pub(crate) struct Recorder {
    owner: ThreadId,
    start: Instant,
    stack: Vec<Frame>,
    /// Each phase that recorded anything.
    totals: BTreeMap<Phase, Total>,
    finished: bool,
    /// The attempt [`begin_attempt`] marked.
    attempt: Option<Mark>,
    /// When a top-level `main` span closed, if no span opened since: the
    /// rest of the command is `teardown`.
    after_main: Option<Instant>,
}

fn nanos(time: Duration) -> i128 {
    i128::try_from(time.as_nanos()).unwrap_or(i128::MAX)
}

impl Recorder {
    /// A recorder whose command started at `start` on thread `owner`.
    pub(crate) fn new(owner: ThreadId, start: Instant) -> Recorder {
        Recorder {
            owner,
            start,
            stack: Vec::new(),
            totals: BTreeMap::new(),
            finished: false,
            attempt: None,
            after_main: None,
        }
    }

    /// `phase`'s total, created empty on first use.
    fn slot(&mut self, phase: Phase) -> &mut Total {
        self.totals.entry(phase).or_default()
    }

    /// Credits the innermost frame with its time since it last resumed.
    fn pause(&mut self, now: Instant) {
        if let Some(top) = self.stack.last_mut() {
            let elapsed = now.saturating_duration_since(top.resumed);
            top.resumed = now;
            let total = self.totals.entry(top.phase).or_default();
            total.nanos = total.nanos.saturating_add(nanos(elapsed));
        }
    }

    /// Opens `phase` at `now`; returns the depth its frame sits at.
    pub(crate) fn open(&mut self, phase: Phase, now: Instant) -> usize {
        self.pause(now);
        self.end_teardown(now);
        self.stack.push(Frame {
            phase,
            resumed: now,
        });
        self.stack.len().saturating_sub(1)
    }

    /// Closes the frame at `depth` (and any frame left open above it).
    pub(crate) fn close(&mut self, depth: usize, now: Instant) {
        while self.stack.len() > depth {
            self.pause(now);
            if let Some(frame) = self.stack.pop() {
                let total = self.slot(frame.phase);
                total.spans = total.spans.saturating_add(1);
                if frame.phase == Phase::Main && self.stack.is_empty() {
                    self.after_main = Some(now);
                }
            }
            if let Some(top) = self.stack.last_mut() {
                top.resumed = now;
            }
        }
    }

    /// Credits `teardown` with the time since a top-level `main` span
    /// closed, if one did and nothing opened since.
    fn end_teardown(&mut self, now: Instant) {
        if let Some(at) = self.after_main.take() {
            let total = self.slot(Phase::Teardown);
            total.nanos = total
                .nanos
                .saturating_add(nanos(now.saturating_duration_since(at)));
            total.spans = total.spans.saturating_add(1);
        }
    }

    /// Moves `time` out of the innermost open frame into `phase`.
    pub(crate) fn add(&mut self, phase: Phase, time: Duration) {
        let time = nanos(time);
        if let Some(top) = self.stack.last().map(|frame| frame.phase) {
            let enclosing = self.slot(top);
            enclosing.nanos = enclosing.nanos.saturating_sub(time);
        }
        let total = self.slot(phase);
        total.nanos = total.nanos.saturating_add(time);
        total.spans = total.spans.saturating_add(1);
    }

    /// The state an attempt starts from; the innermost open frame is
    /// credited up to `now` first, so the mark splits its time exactly.
    pub(crate) fn mark(&mut self, now: Instant) -> Mark {
        self.pause(now);
        Mark {
            at: now,
            totals: self.totals.clone(),
        }
    }

    /// The attempt from `mark` started again: every phase but `restart`
    /// goes back to the mark, and `restart` ends at the mark's value plus
    /// the attempt's whole elapsed time (an inner attempt's restart
    /// included); the part no span recorded is moved out of the innermost
    /// open frame.
    pub(crate) fn restarted(&mut self, mark: &Mark, now: Instant) {
        let elapsed = now.saturating_duration_since(mark.at);
        let mut others: i128 = 0;
        let mut recorded: i128 = 0;
        for phase in Phase::ALL {
            let before = mark.totals.get(&phase).copied().unwrap_or_default();
            let now_total = self.totals.get(&phase).copied().unwrap_or_default();
            let delta = now_total.nanos.saturating_sub(before.nanos);
            recorded = recorded.saturating_add(delta);
            if phase != Phase::Restart {
                others = others.saturating_add(delta);
                if mark.totals.contains_key(&phase) {
                    self.totals.insert(phase, before);
                } else {
                    self.totals.remove(&phase);
                }
            }
        }
        let restart = self.slot(Phase::Restart);
        restart.nanos = restart.nanos.saturating_add(others);
        let rest = nanos(elapsed).saturating_sub(recorded).max(0);
        let rest = Duration::from_nanos(u64::try_from(rest).unwrap_or(u64::MAX));
        self.add(Phase::Restart, rest);
    }

    /// What `phase` recorded so far, the open frames' running time left
    /// out.
    #[cfg(test)]
    pub(crate) fn total(&self, phase: Phase) -> Total {
        self.totals.get(&phase).copied().unwrap_or_default()
    }

    /// The file's text at `now`, every open frame credited first.
    pub(crate) fn render(&mut self, command: &str, now: Instant) -> String {
        self.pause(now);
        self.end_teardown(now);
        let mut text = format!("cheby-phases 1 {command}\n");
        // `Phase`'s order is the file's (its `Ord` is the declaration order).
        for (phase, total) in &self.totals {
            if total.spans == 0 && total.nanos == 0 {
                continue;
            }
            let nanos = total.nanos.max(0);
            let _ = writeln!(text, "{} {nanos} {}", phase.name(), total.spans);
        }
        let command_nanos = now.saturating_duration_since(self.start).as_nanos();
        let _ = writeln!(text, "command {command_nanos}");
        text
    }
}
