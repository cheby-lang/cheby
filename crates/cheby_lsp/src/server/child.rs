//! Child processes: when they run, when they die, and what the server
//! keeps of their results (A §11.4 to §11.6 as plan §1.8 amends them,
//! D-1349, D-1410 to D-1414, D-1489, D-1490, D-1493, D-1500 to D-1502).
//!
//! The server never runs generated code (D-1349). It starts `cheby
//! __backend` from `ServerOptions::child` (`current_exe`; `None` in tests
//! in process, which then never spawn) with one job per process (D-1410):
//!
//! | Point         | `constants` (W9)                                 | `ownership` (W15)                 |
//! | ------------- | ------------------------------------------------ | --------------------------------- |
//! | Runs when     | the report is clean: on save, first clean report | any save, first open              |
//! | Dirty buffers | do not stop it (S-27)                            | do not stop it                    |
//! | New save      | kill, wait [`COALESCE`], run                     | `again`: one more run after it    |
//! | Windows       | never spawned (D-1414)                           | yes                               |
//! | Timeout       | `constants.childTimeoutSeconds`                  | [`OWNERSHIP_TIMEOUT`]             |
//! | `-j`          | half the cores, at least 1                       | [`OWNERSHIP_JOBS`]                |
//! | `--target`    | `analysis.target`, when set (D-1489)             | the same                          |
//! | Concurrency   | one per process                                  | one per package, ownership first  |
//!
//! A constants run is due at the session's first clean report, and after
//! a save or watched change that changes the package's normalized text on
//! disk (D-1412): the key of the files the child reads differs from the
//! running child's, or from the last run's when none runs (D-1494): the
//! last run that finished, failed or was cut short, since a killed one
//! gave no result (D-1502). A save of unchanged text, such as an editor's
//! echo of its own save, leaves a running child and a due run as they
//! are. A due run starts when no other constants child runs in the
//! process and no analysis job of its package is running or due, so it
//! starts on the newest report. It starts only while that report is
//! clean (plan §1.2): otherwise it is pending, and
//! the next clean report starts it. It reads the saved files, so a dirty
//! buffer stops nothing (S-27): the child's own front end says `errors`
//! when disk does not check. A run stays pending only while every file
//! that keeps the report unclean is an unsaved buffer: when a saved file
//! does not check, the clean report that fixing its buffer gives would
//! start a run over the broken disk, so the next save starts it instead.
//! A package with no constant of its own starts no child, and its child
//! results are cleared (A §11.4).
//!
//! A constants child also dies when its stdout passes [`STDOUT_CAP`], at
//! `shutdown` and at its session's end, and by its own watchdog when the
//! server dies, since its stdin then closes (A §11.5). A kill is
//! `Child::kill` then `wait`. A run cut short by its time limit or the cap
//! is logged, and gives one `window/showMessage` per session (D-1415). No
//! retry follows until the key of the text on disk that it read changes
//! (A §11.5, D-1494), so saving unchanged text does not loop, and an
//! unsaved edit neither blocks nor lifts it, since the child reads disk
//! (D-1412); or until a settings change raises the time limit above the
//! one that cut it short, which the message names; lowering it does
//! nothing (D-1493). A settings change also applies `constants.evaluate`
//! at once, and the new time limit to a running child. A change of
//! `analysis.target` makes a run due at once, as the first clean report
//! does, killing a child that checks for the old target (D-1498), and
//! lifts the block of a run cut short over unchanged text (D-1500). After
//! `shutdown` nothing new comes due.
//!
//! Results (A §11.6, D-1412, D-1413): a child diagnostic is published while
//! its file's normalized hash equals the hash the child read, so editing
//! one file hides only that file's stale results. On a failed evaluation
//! the old values stay, marked stale, and each diagnostic marks its own
//! constant failed. A constant with no value after a run that failed or
//! was cut short reads as [`ConstReading::AfterFailure`] (D-1490), and
//! after a run skipped because the saved files have front-end errors as
//! [`ConstReading::SkippedForErrors`] (D-1497). So does one after a save
//! left such errors on disk, so that its due run found an unclean report
//! through saved files and the server started none (D-1501), and while
//! that mark is set every value reads as stale, as after a skipped run
//! (D-1503). Only such a save or watched change sets the mark, so a
//! session opened over saved files with errors reads as not yet until a
//! save (D-1504). The mark names the files whose saved text did not
//! check. It holds until a due run finds a clean report, or one unclean
//! only through unsaved buffers while none of those files keeps its
//! broken text on disk, since such a buffer's saved text may not check
//! either; until a run ends; or until a save, with no run waiting, brings
//! back the last run's text. A save that fixes the files clears it only
//! when its run starts (D-1504). A package with no constants gets no
//! mark. A failed constant still reads as failed. Results are plain
//! data keyed by root, path and [`Symbol`], so they cross generations. A
//! value also carries, for each of the package's module files that its
//! initializer reaches, as the child reports them, and for the file that
//! declares it, found by the module's file of §7.2 (D-162), the
//! normalized hash of the text the run read. It is current only while
//! the editor holds that text in each of them, so hover and `cheby/status`
//! mark a value stale once a module it reaches changed, saved or not
//! (D-1510). A value the child names no modules for reaches every module
//! file, a module added since included. A run that is due or running
//! sets [`ConstValues::evaluating`], kept in step with [`ChildState`],
//! and hover then says that the constants are being evaluated (D-1511).
//!
//! The logic is on [`Children`], which reads the server only through a
//! [`ChildEnv`] and answers with an [`Outbox`], so its tests need no
//! server. The `Server` methods that apply it are in `child_loop`.
//!
//! `CHEBY_LSP_CHILD_FIXTURE` names a file of canned child documents that
//! stand in for spawning, so hover on constants is tested in process and
//! on Windows. The file holds one [`Envelope`] or a JSON array of them:
//! the process's `n`th run takes the `n`th, and the last repeats. It is
//! read again at each run, and its paths are relative to the package
//! root, as the child's are. The variable is read only under
//! `cfg(debug_assertions)`, so a release server never takes results from
//! the environment (S-24).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cheby_driver::analysis::{
    Code, FileLocation, FileReport, Hash128, InputsKey, ModuleView, PlainDiagnostic, PlainEdit,
    PlainHelp, PlainLabel, Report, Severity, Snapshot, Symbol, Target, TextRange, normalized_key,
};
use crossbeam_channel::{Receiver, Sender};
use lsp_types::MessageType;
use serde_json::{Value, json};

use super::effects::Effect;
use super::progress::{Progress, ProgressToken};
use crate::child::MAX_TEXT;
use crate::child::wire::{
    ConstantsSection, Envelope, Front, Section, Status, SymbolData, WireDiagnostic, WireSeverity,
};

/// How long a save waits before its child starts, so that format-on-save
/// and save-all give one run (A §11.4). Also the wait after a kill.
pub(crate) const COALESCE: Duration = Duration::from_millis(300);

/// A child whose stdout passes this many bytes is killed (D-1411).
pub(crate) const STDOUT_CAP: usize = 67_108_864;

/// The most of a child's stderr kept, for `window/logMessage` on failure.
pub(crate) const STDERR_CAP: usize = 65_536;

/// The ownership job's time limit (D-1411).
#[expect(dead_code, reason = "W15 starts the ownership job")]
pub(crate) const OWNERSHIP_TIMEOUT: Duration = Duration::from_secs(30);

/// The ownership job's `-j` (D-1411).
pub(crate) const OWNERSHIP_JOBS: usize = 2;

/// The environment variable of canned child documents, read only in a
/// debug build (S-24).
pub(crate) const FIXTURE_VAR: &str = "CHEBY_LSP_CHILD_FIXTURE";

/// The `$/progress` title of a constants run.
const CONSTANTS_TITLE: &str = "Evaluating constants";

/// One of the two jobs (D-1410).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ChildJob {
    /// `--want constants`.
    Constants,
    /// `--want ownership`: the W15 wave's.
    #[expect(dead_code, reason = "W15 starts the ownership job")]
    Ownership,
}

impl ChildJob {
    /// The job's `--want` word.
    pub(crate) const fn want(self) -> &'static str {
        match self {
            Self::Constants => "constants",
            Self::Ownership => "ownership",
        }
    }
}

/// Where one job of one package is (A §11.4, plan §1.8).
#[derive(Debug, Default)]
pub(crate) enum ChildState {
    /// Nothing to do.
    #[default]
    Idle,
    /// A run is due at `due`: after [`COALESCE`], or behind the one
    /// constants child of the process.
    Waiting {
        /// When it may start.
        due: Instant,
    },
    /// A child is running.
    Running(Box<Running>),
}

impl ChildState {
    /// The state's name for `cheby/status`.
    const fn name(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Waiting { .. } => "waiting",
            Self::Running(_) => "running",
        }
    }
}

/// A child process in flight.
#[derive(Debug)]
pub(crate) struct Running {
    /// The process; its stdout and stderr belong to reader threads. Its
    /// stdin stays open while it runs, for the child's watchdog.
    pub child: Child,
    /// The run's number, so a result that arrives after a kill is dropped.
    pub run: u64,
    /// The [`disk_key`] it started from: a save restarts it only when the
    /// key changes (D-1494), and a run cut short is not retried until it
    /// does.
    pub key: Hash128,
    /// When it started, for `cheby/status`.
    pub started: Instant,
    /// When its time limit kills it.
    pub deadline: Instant,
    /// Ownership: a save came during the run, so one more follows it
    /// (D-1411). Constants never set it: a save kills instead.
    #[expect(dead_code, reason = "W15 reads it for the ownership job")]
    pub again: bool,
    /// Its `$/progress` token, when the client takes progress (D-1415).
    pub progress: Option<ProgressToken>,
}

/// A constants run cut short: no new run for the same key (A §11.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cut {
    /// The run's [`Running::key`].
    pub key: Hash128,
    /// The time limit it passed, `None` when the stdout cap cut it: a
    /// higher limit lifts the block.
    pub limit: Option<Duration>,
}

/// What the server knows of a published report beyond [`Report`] itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReportFacts {
    /// A package, not a single file (D-1371).
    pub is_package: bool,
    /// Some local module declares a constant (A §11.4). True when unknown.
    pub has_constants: bool,
    /// The report is unclean through a file whose disk text is what the
    /// analysis read, or through an error with no location, not only
    /// through unsaved buffers, so disk does not check either.
    pub disk_unclean: bool,
    /// The files with an error, or broken, whose text the analysis read
    /// from disk or from a buffer saved as it is: their saved text does
    /// not check (D-1501).
    pub saved_errors: BTreeSet<PathBuf>,
}

impl ReportFacts {
    /// A package with constants whose unclean files, if any, are unsaved
    /// buffers.
    #[cfg(test)]
    pub(crate) const PACKAGE: Self = Self {
        is_package: true,
        has_constants: true,
        disk_unclean: false,
        saved_errors: BTreeSet::new(),
    };

    /// A single-file session (D-1371).
    #[cfg(test)]
    pub(crate) const SINGLE_FILE: Self = Self {
        is_package: false,
        has_constants: true,
        disk_unclean: false,
        saved_errors: BTreeSet::new(),
    };
}

/// One file's child diagnostics, with the hash they belong to (D-1412).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ChildFile {
    /// The normalized hash of the text the child read.
    pub hash: Hash128,
    /// The diagnostics, ranges in that normalized text, related paths
    /// absolute.
    pub diagnostics: Vec<PlainDiagnostic>,
}

/// How a package's last constants run ended, for `cheby/status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LastRun {
    /// `ok`, `failed`, `cut short`, `skipped` and the like, with detail.
    pub outcome: String,
    /// How long it took.
    pub took: Duration,
}

/// One package's children and their results.
#[derive(Debug, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts about one package"
)]
pub(crate) struct PackageChild {
    /// The constants job.
    pub constants: ChildState,
    /// The ownership job (W15).
    pub ownership: ChildState,
    /// The last constants run cut short by its time limit or its stdout
    /// cap: no new run for the same key of the text on disk (A §11.5,
    /// D-1493).
    pub last_cut: Option<Cut>,
    /// The [`disk_key`] the last constants run that ended, failed or was
    /// cut short started from: with no child running or due, a save makes
    /// a run due only when the key differs (D-1494). A killed run does not
    /// set it, since it gave no result.
    pub last_key: Option<Hash128>,
    /// The [`disk_key`] of the save that made the waiting run due: a save
    /// of the same text, such as its echo, leaves the run where it is
    /// (D-1494). Cleared when the run starts.
    pub due_key: Option<Hash128>,
    /// A due run did not start because the saved files have front-end
    /// errors, so a constant with no value reads as
    /// [`ConstReading::SkippedForErrors`] (D-1501) and a value as stale
    /// (D-1503), through [`ConstValues::disk_errors`]. Set only at the
    /// start of a run that a save or watched change made due (D-1504). It
    /// holds the files whose saved text
    /// did not check, with the [`file_key`] of that text. Cleared once a
    /// due run finds a clean report, or one unclean only through unsaved
    /// buffers while none of those files still has that text on disk
    /// unless the analysis read it without errors; when a run ends; and
    /// by a save that, with no run waiting, brings back the last run's
    /// text (D-1504).
    pub unstarted: Option<BTreeMap<PathBuf, Hash128>>,
    /// The files a constants child reads, as the latest report knows
    /// them: its files, held ones included, and the manifest, with the
    /// module files saved since it (D-1494).
    pub read: BTreeSet<PathBuf>,
    /// The session's first clean report started a run (D-1411).
    pub clean_seen: bool,
    /// The latest report is clean (plan §1.2).
    pub clean: bool,
    /// A run was due while the report was not clean only through unsaved
    /// buffers: the next clean report starts it.
    pub pending: bool,
    /// The latest report is unclean through a file whose disk text does
    /// not check either ([`ReportFacts::disk_unclean`]).
    pub disk_unclean: bool,
    /// The files of the latest report, or a Q run since it, whose saved
    /// text does not check ([`ReportFacts::saved_errors`]).
    pub saved_errors: BTreeSet<PathBuf>,
    /// A package, not a single file (D-1371).
    pub is_package: bool,
    /// Some local module of the latest report declares a constant.
    pub has_constants: bool,
    /// The package can evaluate constants: a package, not a single file
    /// (D-1371), whose child can run here (D-1414) with
    /// `constants.evaluate` on.
    pub evaluates: bool,
    /// The newest constant values, shared with requests.
    pub values: Arc<ConstValues>,
    /// The child diagnostics by absolute path.
    pub diagnostics: BTreeMap<PathBuf, ChildFile>,
    /// The latest report's files, to republish when the child's lists
    /// change and no new report comes.
    pub files: BTreeMap<PathBuf, Arc<FileReport>>,
    /// A run cut short was shown with `window/showMessage` this session
    /// (D-1415).
    pub shown_cut: bool,
    /// How the last constants run ended.
    pub last: Option<LastRun>,
}

impl PackageChild {
    /// Sets [`ConstValues::evaluating`] from [`PackageChild::constants`]:
    /// a run is due (`Waiting`) or running (D-1511).
    fn sync_evaluating(&mut self) {
        let evaluating = !matches!(self.constants, ChildState::Idle);
        if self.values.evaluating != evaluating {
            Arc::make_mut(&mut self.values).evaluating = evaluating;
        }
    }

    /// Sets [`PackageChild::unstarted`] to `files`, and
    /// [`ConstValues::disk_errors`] with it (D-1501).
    fn set_unstarted(&mut self, files: Option<BTreeMap<PathBuf, Hash128>>) {
        let marked = files.is_some();
        self.unstarted = files;
        if self.values.disk_errors != marked {
            Arc::make_mut(&mut self.values).disk_errors = marked;
        }
    }

    /// Whether one of `files`, whose saved text did not check, still has
    /// that text on disk, and the latest report did not read it without
    /// errors: it is then an unsaved buffer, or broken, and its saved
    /// text is still the broken one (D-1501).
    fn still_broken_on_disk(&self, files: &BTreeMap<PathBuf, Hash128>) -> bool {
        files.iter().any(|(path, key)| {
            file_key(path) == Some(*key)
                && self.files.get(path).is_none_or(|file| {
                    normalized_key(&file.text) != *key
                        || file
                            .diagnostics
                            .iter()
                            .any(|d| d.severity == Severity::Error)
                })
        })
    }
}

/// What a reader thread sends when a child's output ends.
#[derive(Debug)]
pub(crate) struct ChildDone {
    /// The package.
    pub root: PathBuf,
    /// The job.
    pub job: ChildJob,
    /// [`Running::run`] of the child.
    pub run: u64,
    /// What came out.
    pub outcome: ChildOutcome,
}

/// How a child's output ended.
#[derive(Debug)]
pub(crate) enum ChildOutcome {
    /// Stdout and stderr reached their end: the process is exiting.
    Exited {
        /// The exit status, `None` when a signal ended it. Reader threads
        /// send `None`: the main loop owns the process and waits for it.
        status: Option<i32>,
        /// The whole stdout, at most [`STDOUT_CAP`] bytes.
        stdout: Vec<u8>,
        /// The start of stderr.
        stderr: String,
    },
    /// Stdout passed [`STDOUT_CAP`]: the main loop kills the child.
    TooLarge,
    /// Reading or waiting failed.
    Io(String),
}

/// What [`Children`] reads of the server for one step.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChildEnv<'a> {
    /// `ServerOptions::child`: the `cheby` to start, `None` for none.
    pub exe: Option<&'a Path>,
    /// `constants.evaluate`.
    pub evaluate: bool,
    /// `constants.childTimeoutSeconds`.
    pub timeout: Duration,
    /// `analysis.target`, which the constants child checks for as `cheby
    /// check --target` does (D-1489). `None` is the manifest's targets.
    pub target: Option<Target>,
    /// The client takes `$/progress` (`window.workDoneProgress`).
    pub progress: bool,
    /// The packages whose analysis job is running or due: a due child of
    /// theirs waits for the newest report.
    pub analyzing: &'a BTreeSet<PathBuf>,
}

/// What a step of [`Children`] asks the main loop to do.
#[derive(Debug, Default)]
pub(crate) struct Outbox {
    /// Messages to send, in order.
    pub effects: Vec<Effect>,
    /// By package root, the files whose child list changed: publish them
    /// again from [`PackageChild::files`].
    pub republish: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
}

/// Every package's children, with the channel their reader threads send
/// on. The one field of `Server` for child state.
#[derive(Debug)]
pub(crate) struct Children {
    /// By package root.
    pub packages: BTreeMap<PathBuf, PackageChild>,
    /// `$/progress` tokens.
    pub progress: Progress,
    /// The number of the next run.
    pub next_run: u64,
    /// Canned documents in place of spawning, in a debug build only.
    pub fixture: Option<PathBuf>,
    /// The packages that were busy, or saved, since their last
    /// `cheby/idle` (`progress`).
    pub idle_owed: BTreeSet<PathBuf>,
    /// When [`Children::schedule`] last ran: a due run it could not start
    /// waits for an event, not for the timer.
    scheduled_at: Option<Instant>,
    /// [`Children::kill_all`] ran (`shutdown`): nothing comes due again.
    closed: bool,
    sender: Sender<ChildDone>,
    receiver: Receiver<ChildDone>,
}

impl Drop for Children {
    /// No child outlives the server, even when the loop ends early.
    fn drop(&mut self) {
        self.kill_all();
    }
}

/// The fixture file that `var`, [`FIXTURE_VAR`]'s value, names, taken
/// only when `debug` (S-24).
fn fixture_from(debug: bool, var: Option<std::ffi::OsString>) -> Option<PathBuf> {
    if debug { var.map(PathBuf::from) } else { None }
}

impl Children {
    /// No package, no child. Reads [`FIXTURE_VAR`] in a debug build.
    pub(crate) fn new() -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();
        Self {
            packages: BTreeMap::new(),
            progress: Progress::default(),
            next_run: 0,
            fixture: fixture_from(cfg!(debug_assertions), std::env::var_os(FIXTURE_VAR)),
            idle_owed: BTreeSet::new(),
            scheduled_at: None,
            closed: false,
            sender,
            receiver,
        }
    }

    /// Where reader threads report; the main loop's `select!` reads it.
    pub(crate) const fn done(&self) -> &Receiver<ChildDone> {
        &self.receiver
    }

    /// The sender a reader thread takes.
    pub(crate) fn sender(&self) -> Sender<ChildDone> {
        self.sender.clone()
    }

    /// Whether a constants child can run for a package under `env`: the
    /// fixture stands in anywhere, and a real child needs an executable
    /// and a platform other than Windows (D-1414).
    fn can_run(&self, env: &ChildEnv<'_>) -> bool {
        env.evaluate && (self.fixture.is_some() || (!cfg!(windows) && env.exe.is_some()))
    }

    /// The next instant the loop must wake for a child: a `Waiting` run
    /// that comes due, or a running child's time limit. A run already due
    /// at the last [`Children::schedule`] waits for an event instead, so
    /// the loop never spins.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        let checked = self.scheduled_at;
        self.packages
            .values()
            .flat_map(|package| [&package.constants, &package.ownership])
            .filter_map(|state| match state {
                ChildState::Idle => None,
                ChildState::Waiting { due } => {
                    checked.is_none_or(|checked| *due > checked).then_some(*due)
                }
                ChildState::Running(running) => Some(running.deadline),
            })
            .min()
    }

    /// Whether a child of `root` is running or due, so `cheby/idle` waits
    /// for it (D-1415).
    pub(crate) fn busy(&self, root: &Path) -> bool {
        self.packages.get(root).is_some_and(|package| {
            !matches!(package.constants, ChildState::Idle)
                || !matches!(package.ownership, ChildState::Idle)
        })
    }

    /// The constant values for hover. `None` when the package never
    /// evaluates: single-file mode (D-1371), Windows (D-1414), or
    /// `constants.evaluate` off; hover then shows no value line.
    pub(crate) fn constants(&self, root: &Path) -> Option<Arc<ConstValues>> {
        self.packages
            .get(root)
            .filter(|package| package.evaluates)
            .map(|package| Arc::clone(&package.values))
    }

    /// The child diagnostics of `path` while `normalized`, the normalized
    /// hash of the text being published, is the one the child read
    /// (D-1412). Their ranges are in normalized text: `NormMap` maps them
    /// onto the published text.
    pub(crate) fn diagnostics(
        &self,
        root: &Path,
        path: &Path,
        normalized: Hash128,
    ) -> &[PlainDiagnostic] {
        self.packages
            .get(root)
            .and_then(|package| package.diagnostics.get(path))
            .filter(|file| file.hash == normalized)
            .map_or(&[], |file| file.diagnostics.as_slice())
    }

    /// The latest report's files of `root` among `paths`, to publish
    /// again after the child's lists changed.
    pub(crate) fn report_files(
        &self,
        root: &Path,
        paths: &BTreeSet<PathBuf>,
    ) -> BTreeMap<PathBuf, Arc<FileReport>> {
        let Some(package) = self.packages.get(root) else {
            return BTreeMap::new();
        };
        paths
            .iter()
            .filter_map(|path| {
                let file = package.files.get(path)?;
                Some((path.clone(), Arc::clone(file)))
            })
            .collect()
    }

    /// A report of the package at `root` was published, with what the
    /// server knows of it in `facts`. Keeps its files and key; at the
    /// session's first clean report, or a clean one after a save that
    /// found the report unclean through unsaved buffers only, a run comes
    /// due at once (D-1411).
    fn on_report_unsynced(
        &mut self,
        root: &Path,
        facts: ReportFacts,
        report: &Report,
        env: &ChildEnv<'_>,
        now: Instant,
    ) {
        if self.closed {
            return;
        }
        let runs = facts.is_package && self.can_run(env);
        let package = self.packages.entry(root.to_path_buf()).or_default();
        package.is_package = facts.is_package;
        package.has_constants = facts.has_constants;
        package.evaluates = runs;
        package.clean = report.clean;
        package.disk_unclean = !report.clean && facts.disk_unclean;
        package.saved_errors = if report.clean {
            BTreeSet::new()
        } else {
            facts.saved_errors
        };
        if package.disk_unclean {
            // Fixing the buffer would give a clean report over a broken
            // disk: the next save starts the run.
            package.pending = false;
        }
        package.read = report
            .files
            .keys()
            .chain(&report.held)
            .cloned()
            .chain([root.join(super::MANIFEST)])
            .collect();
        package.files = report
            .files
            .iter()
            .filter(|(path, _)| !report.held.contains(*path))
            .map(|(path, file)| (path.clone(), Arc::clone(file)))
            .collect();
        if !runs || !report.clean {
            return;
        }
        let first = !package.clean_seen;
        package.clean_seen = true;
        if (first || package.pending) && matches!(package.constants, ChildState::Idle) {
            package.pending = false;
            package.constants = ChildState::Waiting { due: now };
        }
    }

    /// A Q run of `root` found module files with phase-A errors: the
    /// package is not one that `cheby check` accepts, whatever its last
    /// report said, until a clean report comes (plan §1.2). `saved` holds
    /// those that are not unsaved buffers.
    pub(crate) fn on_broken(&mut self, root: &Path, saved: &[PathBuf]) {
        if let Some(package) = self.packages.get_mut(root) {
            package.clean = false;
            if !saved.is_empty() {
                package.disk_unclean = true;
                package.pending = false;
                package.saved_errors.extend(saved.iter().cloned());
            }
        }
    }

    /// `didSave`, or a watched change on disk, of `saved` in the package
    /// at `root`: when the package's normalized text on disk differs from
    /// the one its running constants child started from, or from the last
    /// run's when none runs, kills the child and makes a run due after
    /// [`COALESCE`] (D-1411, D-1494). Unchanged text leaves a running
    /// child and a due run as they are. Ownership is the W15 wave's.
    fn on_save_unsynced(
        &mut self,
        root: &Path,
        saved: &[PathBuf],
        env: &ChildEnv<'_>,
        now: Instant,
        out: &mut Outbox,
    ) {
        if self.closed {
            return;
        }
        self.owe_idle(root);
        let runs = self.can_run(env);
        let Some(package) = self.packages.get_mut(root) else {
            return;
        };
        if runs && package.evaluates {
            // A file the latest report does not know yet, such as a new
            // module, is read too, by this key and the next ones until a
            // report comes, so a later save of another file compares the
            // same files (D-1494).
            package
                .read
                .extend(saved.iter().filter(|path| reads(root, path)).cloned());
            let key = disk_key(&package.read);
            let since = match &package.constants {
                ChildState::Running(running) => Some(running.key),
                ChildState::Waiting { .. } => package.due_key.or(package.last_key),
                ChildState::Idle => package.last_key,
            };
            if since == Some(key) {
                // Nothing comes due. With no run waiting, the text on disk
                // is the last run's, such as after an undo saved back to
                // it, so that run's readings stand (D-1501). A waiting
                // run's start decides instead: this may be the echo of the
                // save that made it due.
                if matches!(package.constants, ChildState::Idle) {
                    package.set_unstarted(None);
                }
                return;
            }
            package.due_key = Some(key);
        }
        if let ChildState::Running(running) = std::mem::take(&mut package.constants) {
            let took = now.saturating_duration_since(running.started);
            end_running(*running, &mut self.progress, "a new save", out);
            package.last = Some(LastRun {
                outcome: "killed by a new save".to_owned(),
                took,
            });
        }
        if runs && package.evaluates {
            package.constants = ChildState::Waiting {
                due: now.checked_add(COALESCE).unwrap_or(now),
            };
        } else {
            package.constants = ChildState::Idle;
        }
    }

    /// The settings changed: `env` holds the new ones. `constants.evaluate`
    /// takes effect at once: turned off, it kills a running constants
    /// child and clears the results; turned on, it makes a run due as at
    /// the session's first clean report. A running child gets the new time
    /// limit, and a run cut short by a lower limit than the new one comes
    /// due again, since the message about it says to raise the setting
    /// (A §11.5). Idempotent, so every settings refresh may call it.
    fn on_settings_unsynced(&mut self, env: &ChildEnv<'_>, now: Instant, out: &mut Outbox) {
        if self.closed {
            return;
        }
        let can_run = self.can_run(env);
        let roots: Vec<PathBuf> = self.packages.keys().cloned().collect();
        for root in roots {
            let Some(package) = self.packages.get_mut(&root) else {
                continue;
            };
            let runs = package.is_package && can_run;
            let was = std::mem::replace(&mut package.evaluates, runs);
            if !runs {
                if let ChildState::Running(running) = std::mem::take(&mut package.constants) {
                    let took = now.saturating_duration_since(running.started);
                    end_running(*running, &mut self.progress, "evaluation turned off", out);
                    package.last = Some(LastRun {
                        outcome: "stopped: `constants.evaluate` turned off".to_owned(),
                        took,
                    });
                }
                package.constants = ChildState::Idle;
                package.pending = false;
                if was {
                    clear_results(&root, package, out);
                }
                continue;
            }
            if let ChildState::Running(running) = &mut package.constants {
                running.deadline = running.started.checked_add(env.timeout).unwrap_or(now);
            }
            let lifted = package
                .last_cut
                .is_some_and(|cut| cut.limit.is_some_and(|limit| env.timeout > limit));
            if lifted {
                package.last_cut = None;
            }
            if !was || lifted {
                if !package.clean {
                    // The next clean report starts it.
                    package.clean_seen = false;
                    package.pending = !package.disk_unclean;
                } else if matches!(package.constants, ChildState::Idle) {
                    package.clean_seen = true;
                    package.constants = ChildState::Waiting { due: now };
                }
            }
        }
    }

    /// `analysis.target` changed, and `env` holds the new one: each
    /// package that evaluates gets a constants run due at once, as at the
    /// session's first clean report (D-1498), since E0078 depends on the
    /// target (D-1489). A running child checks for the old target, so it
    /// is killed, and a run cut short under the old target no longer
    /// blocks one: its key names the text, not the target (D-1500). A
    /// killed run does not count as the last run (D-1502). A package
    /// whose report is not clean waits for the next clean report, as
    /// [`Children::on_settings`] has it.
    fn on_target_unsynced(&mut self, env: &ChildEnv<'_>, now: Instant, out: &mut Outbox) {
        if self.closed || !self.can_run(env) {
            return;
        }
        for package in self.packages.values_mut() {
            if !package.is_package || !package.evaluates {
                continue;
            }
            if let ChildState::Running(running) = std::mem::take(&mut package.constants) {
                let took = now.saturating_duration_since(running.started);
                end_running(*running, &mut self.progress, "the target changed", out);
                package.last = Some(LastRun {
                    outcome: "killed: `analysis.target` changed".to_owned(),
                    took,
                });
            }
            package.last_cut = None;
            if package.clean {
                package.clean_seen = true;
                package.pending = false;
                package.constants = ChildState::Waiting { due: now };
            } else {
                // The next clean report starts it.
                package.clean_seen = false;
                package.pending = !package.disk_unclean;
                package.constants = ChildState::Idle;
            }
        }
    }

    /// Kills what passed its time limit, then starts the earliest due run
    /// when no constants child runs in the process (A §11.4, §11.5).
    fn schedule_unsynced(&mut self, env: &ChildEnv<'_>, now: Instant, out: &mut Outbox) {
        let expired: Vec<PathBuf> = self
            .packages
            .iter()
            .filter(|(_, package)| {
                matches!(&package.constants, ChildState::Running(r) if r.deadline <= now)
            })
            .map(|(root, _)| root.clone())
            .collect();
        for root in expired {
            let detail = format!("its time limit of {} s passed", env.timeout.as_secs());
            self.cut_short(&root, &detail, Some(env.timeout), now, out);
        }
        self.scheduled_at = Some(now);
        let running = self
            .packages
            .values()
            .any(|package| matches!(package.constants, ChildState::Running(_)));
        if running {
            return;
        }
        let due = self
            .packages
            .iter()
            .filter(|(root, _)| !env.analyzing.contains(*root))
            .filter_map(|(root, package)| match package.constants {
                ChildState::Waiting { due } if due <= now => Some((due, root.clone())),
                _ => None,
            })
            .min();
        if let Some((_, root)) = due {
            self.start(&root, env, now, out);
        }
    }

    /// Starts the constants run of `root`, if its report allows one.
    fn start(&mut self, root: &Path, env: &ChildEnv<'_>, now: Instant, out: &mut Outbox) {
        let can_run = self.can_run(env);
        let Some(package) = self.packages.get_mut(root) else {
            return;
        };
        package.constants = ChildState::Idle;
        package.due_key = None;
        if !can_run || !package.evaluates {
            return;
        }
        if !package.clean {
            // Waiting for unsaved buffers to check; a disk that does not
            // check waits for the next save instead.
            package.pending = !package.disk_unclean;
            // The saved files have errors, so no run starts: hover says
            // so, as for a run the child skips (D-1497, D-1501). When only
            // unsaved buffers keep the report unclean, the saved text of
            // such a buffer may not check either: the mark stays while a
            // file it holds keeps its broken text on disk. A package with
            // no constants has no run to miss.
            let mark = if !package.has_constants {
                None
            } else if package.disk_unclean {
                Some(
                    package
                        .saved_errors
                        .iter()
                        .filter_map(|path| Some((path.clone(), file_key(path)?)))
                        .collect(),
                )
            } else {
                package
                    .unstarted
                    .take()
                    .filter(|files| package.still_broken_on_disk(files))
            };
            package.set_unstarted(mark);
            return;
        }
        // The report is clean, so no run waits for the saved files'
        // errors: the run below, or the reason none starts, decides
        // (D-1501).
        package.set_unstarted(None);
        // The text the child reads, not the editor's (D-1412, D-1494).
        let key = disk_key(&package.read);
        if package.last_cut.is_some_and(|cut| cut.key == key) {
            return;
        }
        if !package.has_constants {
            // Nothing to evaluate: no child, and no result of an older
            // run stays (A §11.4).
            clear_results(root, package, out);
            package.last = Some(LastRun {
                outcome: "skipped: the package has no constants".to_owned(),
                took: Duration::ZERO,
            });
            return;
        }
        let run = self.next_run;
        self.next_run = self.next_run.saturating_add(1);
        if let Some(fixture) = self.fixture.clone() {
            package.last_key = Some(key);
            let outcome = fixture_outcome(&fixture, run);
            self.finish(root, outcome, Some(0), now, now, out);
            return;
        }
        let Some(exe) = env.exe.filter(|_| !cfg!(windows)) else {
            return;
        };
        let child = match spawn(
            exe,
            root,
            ChildJob::Constants,
            env.target,
            run,
            self.sender(),
        ) {
            Ok(child) => child,
            Err(e) => {
                out.effects.push(Effect::log(
                    MessageType::ERROR,
                    format!("cannot start `cheby __backend` for {}: {e}", root.display()),
                ));
                return;
            }
        };
        let (progress, effects) = self.progress.begin(env.progress, CONSTANTS_TITLE);
        out.effects.extend(effects);
        let Some(package) = self.packages.get_mut(root) else {
            return;
        };
        package.constants = ChildState::Running(Box::new(Running {
            child,
            run,
            key,
            started: now,
            deadline: now.checked_add(env.timeout).unwrap_or(now),
            again: false,
            progress,
        }));
    }

    /// A child's output ended. Output of a run that was killed is dropped.
    fn on_done_unsynced(&mut self, done: ChildDone, now: Instant, out: &mut Outbox) {
        if done.job != ChildJob::Constants {
            return;
        }
        let Some(package) = self.packages.get_mut(&done.root) else {
            return;
        };
        if !matches!(&package.constants, ChildState::Running(r) if r.run == done.run) {
            return;
        }
        match done.outcome {
            ChildOutcome::TooLarge => {
                let detail = format!("its output passed {} MiB", STDOUT_CAP >> 20);
                self.cut_short(&done.root, &detail, None, now, out);
            }
            outcome => {
                let ChildState::Running(running) = std::mem::take(&mut package.constants) else {
                    return;
                };
                let Running {
                    mut child,
                    key,
                    started,
                    progress,
                    ..
                } = *running;
                // A run that gave a result, even a failure: the same text
                // would give it again (D-1494).
                package.last_key = Some(key);
                let status = match &outcome {
                    ChildOutcome::Exited { status, .. } => {
                        let waited = child.wait().ok().and_then(|s| s.code());
                        status.or(waited)
                    }
                    ChildOutcome::TooLarge | ChildOutcome::Io(_) => {
                        stop(&mut child);
                        None
                    }
                };
                drop(child);
                if let Some(token) = progress {
                    out.effects.extend(self.progress.end(token, "done"));
                }
                self.finish(&done.root, outcome, status, started, now, out);
            }
        }
    }

    /// Kills the constants child of `root` that ran too long, past
    /// `limit`, or wrote too much, with `limit` `None`: logged, shown once
    /// per session (D-1415), and not run again for the same key until a
    /// higher limit comes (A §11.5).
    fn cut_short(
        &mut self,
        root: &Path,
        detail: &str,
        limit: Option<Duration>,
        now: Instant,
        out: &mut Outbox,
    ) {
        let Some(package) = self.packages.get_mut(root) else {
            return;
        };
        let ChildState::Running(running) = std::mem::take(&mut package.constants) else {
            return;
        };
        package.last_cut = Some(Cut {
            key: running.key,
            limit,
        });
        package.last_key = Some(running.key);
        package.set_unstarted(None);
        let took = now.saturating_duration_since(running.started);
        end_running(*running, &mut self.progress, "cut short", out);
        package.last = Some(LastRun {
            outcome: format!("cut short: {detail}"),
            took,
        });
        Arc::make_mut(&mut package.values).mark_failed();
        out.effects.push(Effect::log(
            MessageType::WARNING,
            format!(
                "evaluating the constants of {} was cut short: {detail}",
                root.display()
            ),
        ));
        if !package.shown_cut {
            package.shown_cut = true;
            // Only the time limit is a setting.
            let advice = if limit.is_some() {
                " `constants.childTimeoutSeconds` sets the time limit."
            } else {
                ""
            };
            out.effects.push(Effect::show(
                MessageType::WARNING,
                format!(
                    "cheby: evaluating the constants of {} was cut short ({detail}), so constant \
                     errors and values are missing.{advice}",
                    root.display()
                ),
            ));
        }
    }

    /// Keeps what a child that ended with `status` said, and notes the
    /// files whose child list changed.
    fn finish(
        &mut self,
        root: &Path,
        outcome: ChildOutcome,
        status: Option<i32>,
        started: Instant,
        now: Instant,
        out: &mut Outbox,
    ) {
        let Some(package) = self.packages.get_mut(root) else {
            return;
        };
        // A run ended, so its outcome is the reading (D-1501).
        package.set_unstarted(None);
        let took = now.saturating_duration_since(started);
        let failure = match outcome {
            ChildOutcome::Exited { stdout, stderr, .. } => match status {
                Some(0) => match Envelope::parse(&stdout) {
                    Ok(envelope) => {
                        let outcome = apply_envelope(root, package, envelope, out);
                        package.last = Some(LastRun { outcome, took });
                        return;
                    }
                    Err(e) => format!("its output is not a document: {e}"),
                },
                Some(code) => format!("it exited with status {code}{}", stderr_tail(&stderr)),
                None => format!("it was ended by a signal{}", stderr_tail(&stderr)),
            },
            ChildOutcome::Io(e) => format!("reading its output failed: {e}"),
            ChildOutcome::TooLarge => format!("its output passed {} MiB", STDOUT_CAP >> 20),
        };
        Arc::make_mut(&mut package.values).mark_failed();
        package.last = Some(LastRun {
            outcome: format!("failed: {failure}"),
            took,
        });
        out.effects.push(Effect::log(
            MessageType::ERROR,
            format!(
                "evaluating the constants of {} failed: {failure}",
                root.display()
            ),
        ));
    }

    /// Kills every child: `shutdown`, and the loop's end (A §11.5).
    /// Nothing comes due afterwards, so the loop has no timer to wake for.
    pub(crate) fn kill_all(&mut self) {
        self.closed = true;
        for package in self.packages.values_mut() {
            for state in [&mut package.constants, &mut package.ownership] {
                if let ChildState::Running(mut running) = std::mem::take(state) {
                    stop(&mut running.child);
                }
            }
        }
        self.sync_evaluating();
    }

    /// The session at `root` ended: kill its children, end their
    /// progress, and forget their results.
    pub(crate) fn end(&mut self, root: &Path, out: &mut Outbox) {
        if let Some(mut package) = self.packages.remove(root) {
            for state in [&mut package.constants, &mut package.ownership] {
                if let ChildState::Running(running) = std::mem::take(state) {
                    end_running(*running, &mut self.progress, "session ended", out);
                }
            }
        }
        self.idle_owed.remove(root);
    }

    /// The `child` part of `cheby/status` for `root`, a package or, when
    /// `is_package` is false, a single-file session (D-1415, A §11.7).
    /// `editor` is the package's newest snapshot, which `stale` compares
    /// the values' modules against (D-1510).
    pub(crate) fn status(
        &self,
        root: &Path,
        is_package: bool,
        editor: Option<&dyn EditorText>,
        env: &ChildEnv<'_>,
        now: Instant,
    ) -> Value {
        let state = |state: &ChildState| match state {
            ChildState::Idle => json!({ "state": "idle" }),
            ChildState::Waiting { due } => json!({
                "state": state.name(),
                "dueInMs": millis(due.saturating_duration_since(now)),
            }),
            ChildState::Running(running) => json!({
                "state": state.name(),
                "run": running.run,
                "runningMs": millis(now.saturating_duration_since(running.started)),
                "killedInMs": millis(running.deadline.saturating_duration_since(now)),
            }),
        };
        let why_not = if !is_package {
            Some("single-file mode evaluates no constants (D-1371)")
        } else if !env.evaluate {
            Some("`constants.evaluate` is off")
        } else if self
            .packages
            .get(root)
            .is_some_and(|package| !package.has_constants)
        {
            Some("the package has no constants")
        } else if self.fixture.is_some() {
            None
        } else if cfg!(windows) {
            Some("constants are not evaluated on Windows until M6 (D-1414)")
        } else if env.exe.is_none() {
            Some("the server has no `cheby` to start")
        } else {
            None
        };
        let Some(package) = self.packages.get(root) else {
            return json!({ "constants": { "state": "idle" }, "notEvaluated": why_not });
        };
        let mut constants = state(&package.constants);
        if let Value::Object(map) = &mut constants {
            map.insert("pending".to_owned(), json!(package.pending));
            map.insert("cleanReport".to_owned(), json!(package.clean));
            map.insert("evaluates".to_owned(), json!(package.evaluates));
            map.insert(
                "last".to_owned(),
                package.last.as_ref().map_or(
                    Value::Null,
                    |last| json!({ "outcome": last.outcome, "tookMs": millis(last.took) }),
                ),
            );
            map.insert("values".to_owned(), json!(package.values.values.len()));
            map.insert("failed".to_owned(), json!(package.values.failed.len()));
            // Stale as hover shows it: when any value reads stale, after a
            // run that failed, was cut short or was skipped, while a save
            // has left errors on disk so that no run started (D-1503,
            // D-1505), and while a module a value reaches has other text
            // in `editor`, the newest snapshot (D-1510).
            map.insert("stale".to_owned(), json!(package.values.any_stale(editor)));
            // Skipped by the child (D-1497) or never started by the
            // server (D-1501).
            map.insert(
                "skippedForErrors".to_owned(),
                json!(package.values.front_errors || package.values.disk_errors),
            );
            map.insert(
                "diagnostics".to_owned(),
                json!(
                    package
                        .diagnostics
                        .values()
                        .map(|file| file.diagnostics.len())
                        .sum::<usize>()
                ),
            );
            map.insert("cutShortShown".to_owned(), json!(package.shown_cut));
        }
        json!({
            "constants": constants,
            "ownership": state(&package.ownership),
            "notEvaluated": why_not,
        })
    }
}

/// The entry points that change [`ChildState`]: each takes its step,
/// then keeps every package's [`ConstValues::evaluating`] in step with its
/// constants state, so hover reads whether a run is due or running
/// (D-1511).
impl Children {
    /// [`Children::on_report_unsynced`], then the evaluating marks.
    pub(crate) fn on_report(
        &mut self,
        root: &Path,
        facts: ReportFacts,
        report: &Report,
        env: &ChildEnv<'_>,
        now: Instant,
    ) {
        self.on_report_unsynced(root, facts, report, env, now);
        self.sync_evaluating();
    }

    /// [`Children::on_save_unsynced`], then the evaluating marks.
    pub(crate) fn on_save(
        &mut self,
        root: &Path,
        saved: &[PathBuf],
        env: &ChildEnv<'_>,
        now: Instant,
        out: &mut Outbox,
    ) {
        self.on_save_unsynced(root, saved, env, now, out);
        self.sync_evaluating();
    }

    /// [`Children::on_settings_unsynced`], then the evaluating marks.
    pub(crate) fn on_settings(&mut self, env: &ChildEnv<'_>, now: Instant, out: &mut Outbox) {
        self.on_settings_unsynced(env, now, out);
        self.sync_evaluating();
    }

    /// [`Children::on_target_unsynced`], then the evaluating marks.
    pub(crate) fn on_target(&mut self, env: &ChildEnv<'_>, now: Instant, out: &mut Outbox) {
        self.on_target_unsynced(env, now, out);
        self.sync_evaluating();
    }

    /// [`Children::schedule_unsynced`], then the evaluating marks.
    pub(crate) fn schedule(&mut self, env: &ChildEnv<'_>, now: Instant, out: &mut Outbox) {
        self.schedule_unsynced(env, now, out);
        self.sync_evaluating();
    }

    /// [`Children::on_done_unsynced`], then the evaluating marks.
    pub(crate) fn on_done(&mut self, done: ChildDone, now: Instant, out: &mut Outbox) {
        self.on_done_unsynced(done, now, out);
        self.sync_evaluating();
    }

    /// Sets each package's [`ConstValues::evaluating`] from its
    /// constants state (D-1511).
    fn sync_evaluating(&mut self) {
        for package in self.packages.values_mut() {
            package.sync_evaluating();
        }
    }
}

/// The key of the normalized text on disk of `paths` (D-1412), the files
/// a constants child reads: each path with its text's normalized hash, or
/// with a mark of its own when it cannot be read. A path not on disk,
/// such as an unsaved new buffer, adds nothing, as the child does not see
/// it either. Equal keys mean the child would read the same text (D-1494).
pub(crate) fn disk_key(paths: &BTreeSet<PathBuf>) -> Hash128 {
    let mut key = InputsKey::new();
    for path in paths {
        match std::fs::read(path) {
            Ok(bytes) => key
                .path(path)
                .str("text")
                .bytes(&normalized_key(&String::from_utf8_lossy(&bytes)).0),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => key.path(path).str("unread"),
        };
    }
    key.finish()
}

/// The normalized hash of the text on disk of `path` (D-1412), `None`
/// when it cannot be read.
fn file_key(path: &Path) -> Option<Hash128> {
    let bytes = std::fs::read(path).ok()?;
    Some(normalized_key(&String::from_utf8_lossy(&bytes)))
}

/// Whether a constants child of the package at `root` reads `path`: its
/// manifest, or a module file of §7.2 (D-162). `module_file`'s answer is
/// lexical, so the package's name does not change it.
fn reads(root: &Path, path: &Path) -> bool {
    path == root.join(super::MANIFEST) || cheby_driver::module_file(root, "app", path).is_some()
}

/// `d` in whole milliseconds, cut at `u64::MAX`.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// `stderr`'s text after a colon, or nothing when it is empty.
fn stderr_tail(stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        String::new()
    } else {
        format!("; its stderr:\n{stderr}")
    }
}

/// Forgets the child results of `root`'s `package`, noting the files
/// whose child list changed.
fn clear_results(root: &Path, package: &mut PackageChild, out: &mut Outbox) {
    if !package.values.values.is_empty()
        || !package.values.failed.is_empty()
        || package.values.stale
        || package.values.front_errors
        || package.values.disk_errors
    {
        package.values = Arc::new(ConstValues::default());
    }
    package.unstarted = None;
    let changed: BTreeSet<PathBuf> = std::mem::take(&mut package.diagnostics)
        .into_keys()
        .collect();
    if !changed.is_empty() {
        out.republish
            .entry(root.to_path_buf())
            .or_default()
            .extend(changed);
    }
}

/// Kills a running child, waits for it, and ends its progress.
fn end_running(running: Running, progress: &mut Progress, message: &str, out: &mut Outbox) {
    let Running {
        mut child,
        progress: token,
        ..
    } = running;
    stop(&mut child);
    if let Some(token) = token {
        out.effects.extend(progress.end(token, message));
    }
}

/// `Child::kill` then `wait` (A §11.5). A kill fails only for a process
/// that already ended, which is the goal, and `wait` then reaps it.
fn stop(child: &mut Child) {
    drop(child.kill());
    drop(child.wait());
}

/// Half the cores, at least 1: the constants child's `-j` (D-1411).
fn constants_jobs() -> usize {
    std::thread::available_parallelism()
        .map_or(1, |n| n.get() / 2)
        .max(1)
}

/// Starts `cheby -j N __backend` for `job` at `root`, with reader threads
/// that send a [`ChildDone`] on `sender` (A §11.4). `target` is
/// `analysis.target`, passed as `--target` when set (D-1489).
fn spawn(
    exe: &Path,
    root: &Path,
    job: ChildJob,
    target: Option<Target>,
    run: u64,
    sender: Sender<ChildDone>,
) -> std::io::Result<Child> {
    let jobs = match job {
        ChildJob::Constants => constants_jobs(),
        ChildJob::Ownership => OWNERSHIP_JOBS,
    };
    let mut command = Command::new(exe);
    command
        .arg("-j")
        .arg(jobs.to_string())
        .arg("__backend")
        .arg("--schema")
        .arg(crate::child::wire::SCHEMA.to_string())
        .arg("--root")
        .arg(root);
    if let Some(target) = target {
        command.arg("--target").arg(target.as_str());
    }
    command.arg("--want").arg(job.want());
    if job == ChildJob::Constants {
        command.arg("--max-text").arg(MAX_TEXT.to_string());
    }
    let mut child = command
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("CHEBY_DUMP_DIR")
        .env_remove("CHEBY_PHASES")
        .env_remove(FIXTURE_VAR)
        .spawn()?;
    if let Err(e) = read_output(&mut child, root.to_path_buf(), job, run, sender) {
        stop(&mut child);
        return Err(e);
    }
    Ok(child)
}

/// The reader threads of a child: stderr up to [`STDERR_CAP`], the rest
/// drained, and stdout up to [`STDOUT_CAP`], which sends the outcome.
fn read_output(
    child: &mut Child,
    root: PathBuf,
    job: ChildJob,
    run: u64,
    sender: Sender<ChildDone>,
) -> std::io::Result<()> {
    let missing = || std::io::Error::other("the child has no pipe");
    let stdout = child.stdout.take().ok_or_else(missing)?;
    let stderr = child.stderr.take().ok_or_else(missing)?;
    let errors = std::thread::Builder::new()
        .name("cheby-child-stderr".to_owned())
        .spawn(move || {
            let mut kept = Vec::new();
            let cap = u64::try_from(STDERR_CAP).unwrap_or(u64::MAX);
            let mut stderr = stderr;
            drop((&mut stderr).take(cap).read_to_end(&mut kept));
            drop(std::io::copy(&mut stderr, &mut std::io::sink()));
            String::from_utf8_lossy(&kept).into_owned()
        })?;
    std::thread::Builder::new()
        .name("cheby-child-stdout".to_owned())
        .spawn(move || {
            let mut bytes = Vec::new();
            let cap = u64::try_from(STDOUT_CAP)
                .unwrap_or(u64::MAX)
                .saturating_add(1);
            let outcome = match stdout.take(cap).read_to_end(&mut bytes) {
                Err(e) => ChildOutcome::Io(e.to_string()),
                Ok(_) if bytes.len() > STDOUT_CAP => ChildOutcome::TooLarge,
                Ok(_) => ChildOutcome::Exited {
                    status: None,
                    stdout: bytes,
                    stderr: errors.join().unwrap_or_default(),
                },
            };
            // The loop is gone when this fails, and nothing reads it.
            drop(sender.send(ChildDone {
                root,
                job,
                run,
                outcome,
            }));
        })?;
    Ok(())
}

/// The `run`th canned document of the fixture file (module docs).
fn fixture_outcome(path: &Path, run: u64) -> ChildOutcome {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => return ChildOutcome::Io(format!("{FIXTURE_VAR}: {e}")),
    };
    let stdout = match serde_json::from_str::<Value>(&text) {
        Ok(Value::Array(documents)) => {
            let n = usize::try_from(run).unwrap_or(usize::MAX);
            documents
                .get(n)
                .or_else(|| documents.last())
                .map(Value::to_string)
                .unwrap_or_default()
        }
        Ok(document) => document.to_string(),
        Err(e) => return ChildOutcome::Io(format!("{FIXTURE_VAR}: {e}")),
    };
    ChildOutcome::Exited {
        status: Some(0),
        stdout: stdout.into_bytes(),
        stderr: String::new(),
    }
}

/// `relative`, a `/` path of the child, under `root`.
fn absolute(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .fold(root.to_path_buf(), |path, segment| path.join(segment))
}

/// Why a document gives no constants section: a line for the log, if
/// any, and the outcome for `cheby/status`.
struct Unusable {
    /// The log line's level and text.
    log: Option<(MessageType, String)>,
    /// The outcome.
    outcome: String,
    /// The child skipped evaluation because the saved files have
    /// front-end errors: nothing failed (D-1497).
    front_errors: bool,
}

impl Unusable {
    /// A failure, logged as an error.
    fn failed(text: String) -> Self {
        Self {
            outcome: format!("failed: {text}"),
            log: Some((MessageType::ERROR, text)),
            front_errors: false,
        }
    }
}

/// The constants section of an `ok` document whose front end was clean,
/// with the hash of each file it read by its `/` path, `cheby.toml`'s
/// included.
fn usable(envelope: Envelope) -> Result<(ConstantsSection, BTreeMap<String, Hash128>), Unusable> {
    if envelope.schema != crate::child::wire::SCHEMA {
        return Err(Unusable::failed(format!(
            "the child wrote schema {}, not {}",
            envelope.schema,
            crate::child::wire::SCHEMA
        )));
    }
    match envelope.status {
        Status::Ok => {}
        Status::NoPackage => {
            return Err(Unusable::failed("no `cheby.toml` at the root".to_owned()));
        }
        Status::ToolError { message } | Status::InternalError { message } => {
            return Err(Unusable::failed(message));
        }
    }
    let front_errors = envelope.front == Some(Front::Errors);
    let section = match envelope.sections.constants {
        Some(Section::Ok(section)) => section,
        // The saved files do not check, or the child cannot evaluate.
        Some(Section::Skipped { reason }) => {
            return Err(Unusable {
                outcome: format!("skipped: {reason}"),
                log: Some((MessageType::INFO, format!("skipped: {reason}"))),
                front_errors,
            });
        }
        Some(Section::Failed { message }) => return Err(Unusable::failed(message)),
        None => {
            return Err(Unusable::failed(
                "the document has no constants section".to_owned(),
            ));
        }
    };
    if front_errors {
        return Err(Unusable {
            outcome: "skipped: front-end errors".to_owned(),
            log: None,
            front_errors,
        });
    }
    let mut hashes: BTreeMap<String, Hash128> = envelope
        .files
        .into_iter()
        .map(|file| (file.path, file.hash.0))
        .collect();
    if let Some(manifest) = envelope.manifest {
        hashes.insert("cheby.toml".to_owned(), manifest.0);
    }
    Ok((section, hashes))
}

/// Keeps a document's results in `package` (A §11.6, D-1412, D-1413), and
/// returns the outcome for `cheby/status`. A document with no usable
/// section changes no diagnostic, and its values are from an older run.
fn apply_envelope(
    root: &Path,
    package: &mut PackageChild,
    envelope: Envelope,
    out: &mut Outbox,
) -> String {
    let log = |out: &mut Outbox, level, text: String| {
        out.effects.push(Effect::log(
            level,
            format!("constants of {}: {text}", root.display()),
        ));
    };
    let (section, hashes) = match usable(envelope) {
        Ok(usable) => usable,
        Err(unusable) => {
            let values = Arc::make_mut(&mut package.values);
            values.stale = true;
            values.front_errors = unusable.front_errors;
            if let Some((level, text)) = unusable.log {
                log(out, level, text);
            }
            return unusable.outcome;
        }
    };
    let (diagnostics, failed, unplaced) = child_diagnostics(root, &section, &hashes);
    for d in unplaced {
        log(
            out,
            MessageType::WARNING,
            format!(
                "a {} in `{}` is not shown: its code is unknown or its file has no hash",
                d.code, d.path
            ),
        );
    }
    let evaluated = !section
        .diagnostics
        .iter()
        .any(|d| d.severity == WireSeverity::Error);
    let values = if evaluated {
        ConstValues {
            values: const_entries(root, &section, &hashes),
            failed,
            stale: false,
            front_errors: false,
            disk_errors: false,
            evaluating: package.values.evaluating,
        }
    } else {
        // `backend::build` gives no values on `Err`: the old ones stay,
        // marked stale, and each diagnostic marks its own constant
        // (D-1413).
        ConstValues {
            values: package.values.values.clone(),
            failed,
            stale: true,
            front_errors: false,
            disk_errors: false,
            evaluating: package.values.evaluating,
        }
    };
    package.values = Arc::new(values);
    let changed: BTreeSet<PathBuf> = package
        .diagnostics
        .keys()
        .chain(diagnostics.keys())
        .filter(|path| package.diagnostics.get(*path) != diagnostics.get(*path))
        .cloned()
        .collect();
    package.diagnostics = diagnostics;
    if !changed.is_empty() {
        out.republish
            .entry(root.to_path_buf())
            .or_default()
            .extend(changed);
    }
    let count = section.diagnostics.len();
    if evaluated {
        format!("ok: {} values, {count} diagnostics", section.values.len())
    } else {
        format!("evaluation failed: {count} diagnostics")
    }
}

/// The section's diagnostics by absolute path with the hash each file was
/// read with, the constants they mark failed, and those whose file has no
/// hash, which cannot be placed.
fn child_diagnostics<'s>(
    root: &Path,
    section: &'s ConstantsSection,
    hashes: &BTreeMap<String, Hash128>,
) -> (
    BTreeMap<PathBuf, ChildFile>,
    BTreeMap<Symbol, Code>,
    Vec<&'s WireDiagnostic>,
) {
    let mut files: BTreeMap<PathBuf, ChildFile> = BTreeMap::new();
    let mut failed = BTreeMap::new();
    let mut unplaced = Vec::new();
    for d in &section.diagnostics {
        let Some(plain) = plain_of(root, d) else {
            unplaced.push(d);
            continue;
        };
        if let Some(symbol) = d.symbol.as_ref().and_then(SymbolData::to_symbol)
            && plain.severity == Severity::Error
        {
            failed.entry(symbol).or_insert(plain.code);
        }
        let Some(&hash) = hashes.get(d.path.as_str()) else {
            unplaced.push(d);
            continue;
        };
        files
            .entry(absolute(root, &d.path))
            .or_insert_with(|| ChildFile {
                hash,
                diagnostics: Vec::new(),
            })
            .diagnostics
            .push(plain);
    }
    (files, failed, unplaced)
}

/// The section's values by [`Symbol`], each with the module files it
/// depends on and the hash its run read each with (D-1510): those its
/// `modules` name, or every module file of the run for a value with
/// none, and its declaring file, found by the module's file of §7.2
/// (D-162).
fn const_entries(
    root: &Path,
    section: &ConstantsSection,
    hashes: &BTreeMap<String, Hash128>,
) -> BTreeMap<Symbol, ConstEntry> {
    // The run's module files, `cheby.toml` left out.
    let read: BTreeMap<PathBuf, Hash128> = hashes
        .iter()
        .filter(|(path, _)| path.as_str() != super::MANIFEST)
        .map(|(path, &hash)| (absolute(root, path), hash))
        .collect();
    // By package name, each module's file and its hash.
    let mut by_module: BTreeMap<String, BTreeMap<Vec<String>, (PathBuf, Hash128)>> =
        BTreeMap::new();
    let mut entries = BTreeMap::new();
    for value in &section.values {
        let Some(symbol) = value.symbol.to_symbol() else {
            continue;
        };
        let package = symbol.module.package().to_owned();
        let files = by_module.entry(package.clone()).or_insert_with(|| {
            hashes
                .iter()
                .filter_map(|(path, &hash)| {
                    let file = cheby_driver::module_file(root, &package, Path::new(path))?;
                    Some((file.path.segments().to_vec(), (absolute(root, path), hash)))
                })
                .collect()
        });
        let mut reached: BTreeMap<PathBuf, Hash128> = match &value.modules {
            Some(modules) => modules
                .iter()
                .map(|path| {
                    let hash = hashes.get(path.as_str()).copied().unwrap_or(UNREAD);
                    (absolute(root, path), hash)
                })
                .collect(),
            None => read.clone(),
        };
        if let Some((path, hash)) = files.get(symbol.module.segments()) {
            reached.entry(path.clone()).or_insert(*hash);
        }
        entries.insert(
            symbol,
            ConstEntry {
                text: value.text.clone(),
                truncated: value.truncated,
                reached,
                whole: value.modules.is_none(),
            },
        );
    }
    entries
}

/// A child diagnostic as plain data, paths absolute. `None` for a code
/// this build does not know.
fn plain_of(root: &Path, d: &WireDiagnostic) -> Option<PlainDiagnostic> {
    Some(PlainDiagnostic {
        code: Code::parse(&d.code)?,
        severity: match d.severity {
            WireSeverity::Error => Severity::Error,
            WireSeverity::Warning => Severity::Warning,
        },
        message: d.message.clone(),
        range: d.range.map(TextRange::from),
        primary_message: d.primary_message.clone(),
        related: d
            .related
            .iter()
            .map(|label| PlainLabel {
                path: absolute(root, &label.path),
                range: label.range.into(),
                message: label.message.clone(),
            })
            .collect(),
        notes: d.notes.clone(),
        helps: d
            .helps
            .iter()
            .map(|help| PlainHelp {
                message: help.message.clone(),
                edits: help
                    .edits
                    .iter()
                    .map(|edit| PlainEdit {
                        path: absolute(root, &edit.path),
                        range: edit.range.into(),
                        replacement: edit.replacement.clone(),
                    })
                    .collect(),
            })
            .collect(),
    })
}

/// What the latest evaluation said about one package's constants
/// (A §11.6, D-1413). Plain data keyed by [`Symbol`], so it outlives the
/// snapshot it came with.
#[derive(Clone, Debug, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts about one package's last run and its state"
)]
pub(crate) struct ConstValues {
    /// Each evaluated constant's value.
    pub values: BTreeMap<Symbol, ConstEntry>,
    /// The constants a diagnostic marked failed, with its code.
    pub failed: BTreeMap<Symbol, Code>,
    /// The last evaluation failed, was cut short or was skipped, so every
    /// value here is from an earlier one (D-1413), and a constant with
    /// none reads as [`ConstReading::AfterFailure`] (D-1490), or as
    /// [`ConstReading::SkippedForErrors`] with `front_errors`.
    pub stale: bool,
    /// The last run was skipped because the saved files have front-end
    /// errors (D-1497).
    pub front_errors: bool,
    /// The server started no run for the text on disk because the saved
    /// files have front-end errors (D-1501): [`PackageChild::unstarted`]
    /// is set. A constant with no value reads as skipped for errors, as
    /// with `front_errors`, every value as stale, as after a skipped run
    /// (D-1503), and a failed constant as failed.
    pub disk_errors: bool,
    /// A constants run is due or running for the package: its
    /// [`PackageChild::constants`] is `Waiting` or `Running`. A constant
    /// with no value then reads as [`ConstReading::Evaluating`], and a
    /// stale value says that the new save is being evaluated (D-1511).
    pub evaluating: bool,
}

/// One constant's value and where it came from.
#[derive(Clone, Debug)]
pub(crate) struct ConstEntry {
    /// The text, at most 4,096 characters (D-1413).
    pub text: String,
    /// The text was cut.
    pub truncated: bool,
    /// The package's module files the value depends on, by absolute path,
    /// each with the normalized hash (D-1412) of the text its run read:
    /// those its initializer reaches, as the child reports them, and the
    /// declaring file (D-1510). A file the run listed with no hash has
    /// [`UNREAD`], which no text matches.
    pub reached: BTreeMap<PathBuf, Hash128>,
    /// The child named no modules for the value, so `reached` holds every
    /// module file the run read, and a module file added since counts too
    /// (D-1510).
    pub whole: bool,
}

impl ConstEntry {
    /// Whether `editor` holds, in each module file the value depends on,
    /// the text its run read, and, for a value that reaches every module,
    /// no module file the run did not read (D-1510).
    fn current(&self, editor: &dyn EditorText) -> bool {
        self.reached
            .iter()
            .all(|(path, &hash)| editor.hash(path) == Some(hash))
            && (!self.whole
                || editor
                    .modules()
                    .iter()
                    .all(|path| self.reached.contains_key(path)))
    }
}

/// The hash of a module file whose text a run did not report: no text
/// has it, so a value that depends on that file is never current.
const UNREAD: Hash128 = Hash128([0; 16]);

/// The package's text as the editor holds it, which a value's reading
/// compares with the text its run read (D-1412, D-1510). Hover and
/// `cheby/status` read it from the newest [`Snapshot`].
pub(crate) trait EditorText {
    /// The normalized hash (D-1412) of the package's module file at the
    /// absolute `path`, `None` when the package has no module there.
    fn hash(&self, path: &Path) -> Option<Hash128>;

    /// The package's module files, by absolute path.
    fn modules(&self) -> BTreeSet<PathBuf>;
}

impl EditorText for Snapshot {
    fn hash(&self, path: &Path) -> Option<Hash128> {
        let file = self.file_of(path)?;
        self.module_at(file).filter(|module| module.is_local())?;
        Some(normalized_key(self.source(file)?.text()))
    }

    fn modules(&self) -> BTreeSet<PathBuf> {
        self.local_modules()
            .filter_map(ModuleView::file)
            .filter_map(|file| match self.location(file)? {
                FileLocation::Package(path) => Some(path.clone()),
                FileLocation::Std(_) | FileLocation::Synthetic => None,
            })
            .collect()
    }
}

/// Module files by absolute path with their normalized hashes: the
/// editor's text in tests.
impl EditorText for BTreeMap<PathBuf, Hash128> {
    fn hash(&self, path: &Path) -> Option<Hash128> {
        self.get(path).copied()
    }

    fn modules(&self) -> BTreeSet<PathBuf> {
        self.keys().cloned().collect()
    }
}

/// What hover shows for one constant of a package (X §2.1's constant
/// value rule, D-1413). A std constant shows its written initializer
/// (`StdItem::initializer`) and never asks, so it is never `NotYet`.
///
/// It lives here, with the results it reads, and reaches hover through a
/// `RequestCx` field that holds [`Children::constants`]'s answer, so
/// `features` names no new crate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConstReading {
    /// A value. `stale` when the last evaluation failed or was skipped, a
    /// module file it depends on changed since (D-1510), or the server
    /// starts no run for errors on disk (D-1503): hover then says "when
    /// last saved", or, while `evaluating`, "when last evaluated" (D-1511).
    Value {
        /// The value's text.
        text: String,
        /// The text was cut at 4,096 characters.
        truncated: bool,
        /// From an older evaluation or older text.
        stale: bool,
        /// A run is due or running for the package (D-1511).
        evaluating: bool,
    },
    /// No value, while a run is due or running for the package (D-1511).
    /// It comes before [`ConstReading::AfterFailure`],
    /// [`ConstReading::SkippedForErrors`] and [`ConstReading::NotYet`].
    Evaluating,
    /// Evaluation failed at this constant, with the diagnostic's code.
    Failed {
        /// Such as E0075.
        code: Code,
    },
    /// No value yet: the package's constants have not run, and values
    /// are computed on save.
    NotYet,
    /// No value, not even a stale one, after the last run failed or was
    /// cut short (D-1490): saving cannot give one until that is fixed,
    /// since `backend::build` gives no values on a failure (D-1413).
    AfterFailure,
    /// No value, not even a stale one, after the last run was skipped
    /// because the saved files have front-end errors (D-1497), or after a
    /// save left such errors on disk, so the server started no run
    /// (D-1501): nothing failed, and saving the fixed buffers gives the
    /// value.
    SkippedForErrors,
}

impl ConstValues {
    /// The last run failed or was cut short: every value is stale, and
    /// the run was not skipped for errors (D-1413, D-1490).
    fn mark_failed(&mut self) {
        self.stale = true;
        self.front_errors = false;
        self.disk_errors = false;
    }

    /// The reading of `symbol` while `editor` holds the package's text. A
    /// constant the last run marked failed reads as failed; any other
    /// value is stale when that run failed (D-1413), when a module file it
    /// depends on no longer has the text the run read (D-1510), or while
    /// the server starts no run for errors on disk, since the value may
    /// depend on a broken file (D-1503). A constant with no value is
    /// [`ConstReading::Evaluating`] while a run is due or running
    /// (D-1511); else [`ConstReading::SkippedForErrors`] when the last run
    /// was skipped because the saved files have front-end errors
    /// (D-1497), or when the server started none for them (D-1501),
    /// [`ConstReading::AfterFailure`] when it failed or was cut short,
    /// else [`ConstReading::NotYet`] (D-1490).
    pub(crate) fn reading(&self, symbol: &Symbol, editor: &dyn EditorText) -> ConstReading {
        if let Some(&code) = self.failed.get(symbol) {
            return ConstReading::Failed { code };
        }
        match self.values.get(symbol) {
            Some(entry) => ConstReading::Value {
                text: entry.text.clone(),
                truncated: entry.truncated,
                stale: self.stale || self.disk_errors || !entry.current(editor),
                evaluating: self.evaluating,
            },
            None if self.evaluating => ConstReading::Evaluating,
            None if self.front_errors || self.disk_errors => ConstReading::SkippedForErrors,
            None if self.stale => ConstReading::AfterFailure,
            None => ConstReading::NotYet,
        }
    }

    /// Adds `symbol`'s value `text`, from a run that read each file of
    /// `reached` with its hash (D-1510): what a test of hover needs from
    /// a run.
    #[cfg(test)]
    pub(crate) fn insert_value(
        &mut self,
        symbol: Symbol,
        text: &str,
        truncated: bool,
        reached: BTreeMap<PathBuf, Hash128>,
    ) {
        self.values.insert(
            symbol,
            ConstEntry {
                text: text.to_owned(),
                truncated,
                reached,
                whole: false,
            },
        );
    }

    /// Whether any value reads stale ([`ConstValues::reading`]) while
    /// `editor`, if known, holds the package's text: `cheby/status`'s
    /// `stale` (D-1505, D-1510). With no editor text, only the run's own
    /// marks count.
    pub(crate) fn any_stale(&self, editor: Option<&dyn EditorText>) -> bool {
        self.stale
            || self.disk_errors
            || editor.is_some_and(|editor| {
                self.values.iter().any(|(symbol, entry)| {
                    !self.failed.contains_key(symbol) && !entry.current(editor)
                })
            })
    }
}

#[cfg(test)]
#[path = "child_tests.rs"]
mod tests;
