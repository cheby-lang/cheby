//! `cheby test` (G4, §13.6, D-541) and the standard library's self-test
//! (H3, D-479, D-542).
//!
//! `cheby test [FILTER] [--module PATH]` checks the package, selects the
//! tests of its own modules whose full name `<module path> "<test name>"`
//! contains `FILTER` and that belong to `PATH` or a submodule of it,
//! compiles them in one JIT batch (`Roots::Tests`, D-265: tests are their
//! own units) and runs them, several at once (D-723). The report is
//! written by module path and then in source order, whatever order the
//! tests finish in. Standard output gets:
//!
//! ```text
//! test app "adds" ... ok
//! test app::util "splits" ... FAILED
//!
//! ---- app::util "splits" ----
//! panicked at src/util.cheby:12:10:
//! assertion failed: parts == ["a", "b"]
//!
//! test result: FAILED. 1 passed; 1 failed; 0 filtered out
//! ```
//!
//! A test's own output on both streams, and that of every fiber it starts,
//! is captured. Once the test has finished and every test before it has
//! been written, its output is replayed to the streams it was written to,
//! in write order, then its line is written (D-598, D-723, ADR-0062).
//! Under `CHEBY_UNWIND_EDGES=off`, D-742's measurement switch, a raise
//! ends the process at once (D-770), so the captured output of the tests
//! not yet written is lost; the switch is for measurements only. A
//! failure block holds the panic report (D-449), the text of the `Err` the
//! test returned (D-529), or, in a toolchain with debug assertions, D-458's
//! leak message when the test left objects of its accounting domain alive.
//! The summary is preceded by an empty line when anything was printed
//! before it. Compile errors stop the command before any test runs.
//!
//! Every test runs as a root fiber on a 64 MiB stack (D-596) in a group
//! and an accounting domain of its own (D-691, D-799), on one [`Runtime`]
//! per `cheby test` run and per std module batch (D-690), which reads
//! `CHEBY_THREADS` before the first test (D-658). At most as many tests run
//! at once as the runtime has threads when each one starts (§13.6). One
//! host thread starts them and waits for any of them on one channel; it
//! tears each test's group down when the test ends, in the order they end,
//! and judges the test on its own domain. Where fibers are not supported
//! (Windows until M6, D-714), tests run one after another on the thread
//! backend instead, uncaptured.
//!
//! The std self-test checks every shipped std module with its tests
//! (`Tests::All`), then builds and runs one batch per module, one test at a
//! time and uncaptured, through the same runner (D-837).
//! When a module's batch does not build, each of its tests is built alone,
//! so the ones that build still run. A module or test that does not build
//! is counted and reported with its codes; the other modules still run.

use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use cheby_diag::{Diagnostic, SourceMap};
use cheby_iface::{DefId, Ice, ModuleId, PackageId};
use cheby_jit::{Entry, Jit, JitError};
use cheby_mir::EntryKind;
use cheby_package::{Manifest, ModulePath};
use cheby_runtime::alloc::{Domain, RUNTIME_KINDS};
use cheby_runtime::capture::{Capture, Chunk, Stream};
use cheby_runtime::entry::{
    DEFAULT_ROOT_STACK_SIZE, DEFAULT_STACK_SIZE, EntryConfig, EntryMode, EntryOutcome,
};
use cheby_runtime::fiber::StackError;
use cheby_runtime::{DetachedGroup, RootDone, Runtime, RuntimeConfig, RuntimeError};
use cheby_types::thir;

use crate::backend::{self, Roots, TestRoot};
use crate::frontend::{self, Frontend};
use crate::{CheckOutput, Error, Module, Package, parse_file, parse_module_path, std_lib};

/// Which tests `cheby test` runs (D-541).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TestOptions {
    /// Selects the tests whose full name, `<module path> "<test name>"`,
    /// contains this text.
    pub filter: Option<String>,
    /// Selects the tests of this module and its submodules.
    pub module: Option<String>,
    /// `--deny-warnings`: a warning stops the command before any test is
    /// compiled (D-549).
    pub deny_warnings: bool,
    /// `--reuse-report`: the build also gives the reuse report of the
    /// package's code and tests units ([`TestProgram::reuse_report`],
    /// D-717). A test build can infer other borrow masks than `cheby run`
    /// (D-779, D-813), so its report can differ.
    pub reuse_report: bool,
}

/// The outcome of [`prepare_test`].
#[derive(Debug)]
pub enum PreparedTests {
    /// The package has errors; nothing was compiled and no test runs.
    CompileFailed(CheckOutput),
    /// The selected tests are compiled and ready to run.
    Ready(Box<TestProgram>),
}

/// One selected test.
#[derive(Clone, Debug)]
struct Selected {
    module: String,
    name: String,
    def: DefId,
}

/// The compiled tests of a package, ready to run.
pub struct TestProgram {
    output: CheckOutput,
    /// The package name, `env::program_name()` while the tests run
    /// (D-369, D-578).
    package: String,
    /// The tests' code; `None` when no test is selected.
    jit: Option<Jit>,
    tests: Vec<Selected>,
    filtered_out: usize,
    /// The package's `[runtime]` stacks (D-660).
    stacks: Stacks,
    /// The runtime's thread count when an in-process host sets one
    /// ([`TestProgram::set_threads`]); `None` reads `CHEBY_THREADS`.
    threads: Option<usize>,
    /// The text of `--reuse-report`, empty without the flag.
    reuse_report: String,
}

/// The stacks of a test run (D-660): each test's root fiber gets `main`,
/// and the fibers it spawns get `fiber` unless they set their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stacks {
    /// The manifest's `main-stack`, the stack of each test (D-125, D-596).
    main: usize,
    /// The manifest's `fiber-stack`, the runtime's default fiber stack.
    fiber: usize,
}

impl Stacks {
    /// The defaults, for a run without a manifest: the std self-test.
    const DEFAULT: Self = Self {
        main: DEFAULT_ROOT_STACK_SIZE,
        fiber: DEFAULT_STACK_SIZE,
    };
}

impl std::fmt::Debug for TestProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestProgram")
            .field("output", &self.output)
            .field("package", &self.package)
            .field("tests", &self.tests)
            .field("filtered_out", &self.filtered_out)
            .field("stacks", &self.stacks)
            .field("threads", &self.threads)
            .field("reuse_report", &self.reuse_report)
            .finish_non_exhaustive()
    }
}

/// How one test ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestOutcome {
    Passed,
    /// The text of its failure block: the panic report, the `Err` text or
    /// the leak message, ending with a newline.
    Failed(String),
    /// The test did not build (std self-test only): its codes, such as
    /// `E0053, E0075`.
    NotBuilt(String),
}

/// One test and how it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestResult {
    /// The test's name, as written.
    pub name: String,
    pub outcome: TestOutcome,
}

/// The tests of one module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleTests {
    /// The module path, such as `app::util` or `std::list`.
    pub path: String,
    /// Set when the module's tests did not build at all (std self-test):
    /// the codes, such as `E0053, E0075`. Each test is then
    /// [`TestOutcome::NotBuilt`].
    pub not_built: Option<String>,
    /// The selected tests in source order.
    pub tests: Vec<TestResult>,
}

/// The results of a test run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TestReport {
    /// The modules with selected tests, by module path.
    pub modules: Vec<ModuleTests>,
    /// How many tests the filter and `--module` left out.
    pub filtered_out: usize,
    /// Whether a write that has no `Result` failed and so ended the run
    /// (D-306, D-709), or a write of the report or of a test's captured
    /// output failed (D-834): the run stopped at
    /// once, printed nothing more, and `cheby test` exits with 1 (D-783).
    pub terminated: bool,
    /// How long the tests ran, from the first start to the last test's
    /// line, compile excluded. Informative: it is never printed.
    pub run_time: Duration,
}

/// Counts of one module or of a whole run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TestCounts {
    pub passed: usize,
    pub failed: usize,
    pub not_built: usize,
}

impl TestCounts {
    fn of<'a>(tests: impl IntoIterator<Item = &'a TestResult>) -> Self {
        let mut counts = Self::default();
        for test in tests {
            let n = match test.outcome {
                TestOutcome::Passed => &mut counts.passed,
                TestOutcome::Failed(_) => &mut counts.failed,
                TestOutcome::NotBuilt(_) => &mut counts.not_built,
            };
            *n = n.saturating_add(1);
        }
        counts
    }
}

impl ModuleTests {
    #[must_use]
    pub fn counts(&self) -> TestCounts {
        TestCounts::of(&self.tests)
    }
}

impl TestReport {
    #[must_use]
    pub fn counts(&self) -> TestCounts {
        TestCounts::of(self.modules.iter().flat_map(|m| &m.tests))
    }

    /// Whether every selected test passed (also when none was selected)
    /// and no write failure ended the run.
    #[must_use]
    pub fn success(&self) -> bool {
        let counts = self.counts();
        !self.terminated && counts.failed == 0 && counts.not_built == 0
    }

    /// The exit status: 0 when every selected test passed, 1 otherwise
    /// (D-541, D-542), and 1 when a write failure ended the run (D-783).
    #[must_use]
    pub fn exit_status(&self) -> i32 {
        i32::from(!self.success())
    }

    /// The whole report of the std self-test (D-542): a line per test and
    /// per module or test that did not build, a block per failure, the
    /// counts per module and in total, and the summary line.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = String::new();
        for module in &self.modules {
            if let Some(codes) = &module.not_built {
                let _ = writeln!(text, "module {} did not build: {codes}", module.path);
                continue;
            }
            for test in &module.tests {
                let _ = match &test.outcome {
                    TestOutcome::NotBuilt(codes) => writeln!(
                        text,
                        "test {} \"{}\" did not build: {codes}",
                        module.path, test.name
                    ),
                    outcome => writeln!(text, "{}", test_line(&module.path, &test.name, outcome)),
                };
            }
        }
        for module in &self.modules {
            for test in &module.tests {
                if let TestOutcome::Failed(block) = &test.outcome {
                    let _ = write!(text, "\n{}", failure_block(&module.path, &test.name, block));
                }
            }
        }
        text.push_str("\ncounts per module:\n");
        for module in &self.modules {
            let c = module.counts();
            let _ = writeln!(
                text,
                "  {}: {} passed; {} failed; {} did not build",
                module.path, c.passed, c.failed, c.not_built
            );
        }
        let c = self.counts();
        let _ = writeln!(
            text,
            "  total: {} passed; {} failed; {} did not build; {} filtered out",
            c.passed, c.failed, c.not_built, self.filtered_out
        );
        let _ = write!(text, "\n{}", self.summary());
        text
    }

    /// `test result: ok. 3 passed; 0 failed; 1 filtered out` (D-541), with
    /// `FAILED` when a test failed or did not build, and a newline.
    fn summary(&self) -> String {
        let c = self.counts();
        let verdict = if self.success() { "ok" } else { "FAILED" };
        let not_built = if c.not_built > 0 {
            format!("{} did not build; ", c.not_built)
        } else {
            String::new()
        };
        format!(
            "test result: {verdict}. {} passed; {} failed; {not_built}{} filtered out\n",
            c.passed, c.failed, self.filtered_out
        )
    }
}

/// `test <module> "<name>" ... ok` or `... FAILED`, without a newline.
fn test_line(module: &str, name: &str, outcome: &TestOutcome) -> String {
    let verdict = match outcome {
        TestOutcome::Passed => "ok",
        TestOutcome::Failed(_) | TestOutcome::NotBuilt(_) => "FAILED",
    };
    format!("test {module} \"{name}\" ... {verdict}")
}

/// `---- <module> "<name>" ----` and the failure's text.
fn failure_block(module: &str, name: &str, text: &str) -> String {
    format!("---- {module} \"{name}\" ----\n{text}")
}

/// Whether `test` exists on the native target: its own targets, which
/// include its module's `@!target`, narrowed by the package's `targets`
/// for a local test (D-190), contain `native`. A test that exists only on
/// `js` is not part of a native build (§12.6), so it is neither run nor
/// counted as filtered out (D-597).
pub(crate) fn on_native(front: &Frontend, test: &thir::TestDef) -> bool {
    let native = cheby_package::Target::Native;
    let local = front
        .modules
        .get(test.def.module)
        .is_some_and(|info| info.package == PackageId::LOCAL);
    if local && !front.package_targets.contains(native) {
        return false;
    }
    front
        .set
        .get(test.def.module)
        .and_then(|iface| iface.item(test.def.index))
        .is_none_or(|item| item.targets.contains(native))
}

/// Checks the package at `root` and compiles the tests `options` selects
/// (D-541). Compile errors, including E0053 and constant-evaluation
/// errors from the back end, stop here.
///
/// # Errors
///
/// A tool error if `--module` is not a module path or names no module of
/// the package (D-597), and an [`Error`] for internal
/// failures.
pub fn prepare_test(root: &Path, options: &TestOptions) -> Result<PreparedTests, Error> {
    prepare_test_inner(root, options, None)
}

/// [`prepare_test`] through the build cache, as `cheby test` runs it
/// (D-955, D-965); what it prints and runs is the cacheless build's
/// (D-268).
///
/// # Errors
///
/// As [`prepare_test`].
pub fn prepare_test_cached(
    root: &Path,
    options: &TestOptions,
    cache: &crate::Cache,
) -> Result<PreparedTests, Error> {
    prepare_test_inner(root, options, Some(cache))
}

fn prepare_test_inner(
    root: &Path,
    options: &TestOptions,
    cache: Option<&crate::Cache>,
) -> Result<PreparedTests, Error> {
    // D-1250: a warm `cheby test` checks only what its build needs; a build
    // that then wants a hit's body starts again with every module checked.
    match prepare_test_once(root, options, cache, Some(options.reuse_report)) {
        Err(crate::Restart::Again(mark)) => {
            crate::rollback_log(cache, mark);
            prepare_test_once(root, options, cache, None).map_err(crate::Restart::into_error)
        }
        other => other.map_err(crate::Restart::into_error),
    }
}

/// One attempt of [`prepare_test_inner`], `warm` as
/// `crate::front_end_family`'s.
fn prepare_test_once(
    root: &Path,
    options: &TestOptions,
    cache: Option<&crate::Cache>,
    warm: Option<bool>,
) -> Result<PreparedTests, crate::Restart> {
    let mark = crate::log_mark(cache);
    let mut out = CheckOutput {
        deny_warnings: options.deny_warnings,
        ..CheckOutput::default()
    };
    let front = crate::front_end_family(
        root,
        frontend::Mode::Build,
        crate::cache::index::CommandFamily::Test,
        None,
        &mut out,
        cache,
        warm.map(|reuse| (reuse, None)),
    )?;
    out.sources.sort(&mut out.diagnostics);
    let Some((front, package)) = front.filter(|_| !out.has_errors()) else {
        return Ok(PreparedTests::CompileFailed(out));
    };
    // `--module` is checked like `cheby run`'s module path (D-464,
    // D-597).
    let scope = match &options.module {
        Some(text) => {
            let path = parse_module_path(text)
                .ok_or_else(|| Error::new(format!("`{text}` is not a module path")))?;
            let exists = local_modules(&front)
                .iter()
                .any(|(module_path, _)| within(module_path, &path));
            if !exists {
                return Err(
                    Error::new(format!("there is no module `{path}` in this package")).into(),
                );
            }
            Some(path)
        }
        None => None,
    };
    let mut roots = Vec::new();
    let mut tests = Vec::new();
    let mut filtered_out: usize = 0;
    for (path, module) in local_modules(&front) {
        // A warm hit's tests come from its warm facts (D-1250).
        let Some(module_tests) = front.tests_of(module) else {
            continue;
        };
        let shown = path.to_string();
        for test in module_tests.iter().filter(|t| on_native(&front, t)) {
            let in_scope = scope.as_ref().is_none_or(|scope| within(&path, scope));
            if in_scope && matches_filter(&shown, &test.name, options.filter.as_deref()) {
                roots.push(TestRoot { module, test });
                tests.push(Selected {
                    module: shown.clone(),
                    name: test.name.to_string(),
                    def: test.def,
                });
            } else {
                filtered_out = filtered_out.saturating_add(1);
            }
        }
    }
    let mut reuse_report = String::new();
    let jit = if roots.is_empty() {
        None
    } else {
        let built = match backend::build_reporting(
            &front,
            &out.sources,
            Roots::Tests(roots),
            false,
            options.reuse_report,
        ) {
            Err(_) if warm.is_some() && front.wanted_body() => {
                return Err(crate::Restart::Again(mark));
            }
            built => built?,
        };
        match built {
            Ok(built) => {
                reuse_report = built.reuse_report.unwrap_or_default();
                Some(built.jit)
            }
            Err(diagnostics) => {
                crate::report_back_end(&mut out, diagnostics);
                return Ok(PreparedTests::CompileFailed(out));
            }
        }
    };
    // The root package's `[runtime]` stacks (D-660).
    let stacks = Stacks {
        main: crate::stack_bytes(front.main_stack),
        fiber: crate::stack_bytes(front.fiber_stack),
    };
    Ok(PreparedTests::Ready(Box::new(TestProgram {
        output: out,
        package,
        jit,
        tests,
        filtered_out,
        stacks,
        threads: None,
        reuse_report,
    })))
}

impl TestProgram {
    /// The warnings of the build, to print before the tests run.
    #[must_use]
    pub const fn output(&self) -> &CheckOutput {
        &self.output
    }

    /// The reuse report of `--reuse-report` (D-717), to print to standard
    /// error after the warnings and before any test runs; empty without the
    /// flag or when no test is selected.
    #[must_use]
    pub fn reuse_report(&self) -> &str {
        &self.reuse_report
    }

    /// Runs the tests on at most `threads` worker threads, and so at most
    /// that many at once, instead of the count `CHEBY_THREADS` sets
    /// (D-658): for in-process hosts, such as tests, which must not set the
    /// variable (`RuntimeConfig::threads`).
    pub const fn set_threads(&mut self, threads: usize) {
        self.threads = Some(threads);
    }

    /// Runs the tests, several at once (D-723), and writes the report of
    /// D-541 in source order as they finish: each test's captured output,
    /// standard output's chunks to `out` and standard error's to `err`, in
    /// the order the test wrote them, then its line to `out`; then the
    /// failure blocks and the summary to `out`. `out` and `err` should be
    /// the process's standard output and error, so that the replay has
    /// the bytes and the buffering of the tests' own writes (ADR-0062).
    ///
    /// A failed write to `out` or `err` ends the run at once: the runtime
    /// is halted, nothing more is written, and the report says
    /// `terminated` (D-306, D-783, D-834), as it
    /// does after a termination of a test on the thread backend, whose
    /// writes are not captured.
    ///
    /// # Errors
    ///
    /// An [`Error`] if the runtime cannot start (D-658) or cannot run a
    /// test.
    pub fn run(self, out: &mut dyn Write, err: &mut dyn Write) -> Result<TestReport, Error> {
        self.run_with(out, err, &mut Hooks::default())
    }

    /// [`TestProgram::run`] with the runner's test hooks.
    fn run_with(
        self,
        out: &mut dyn Write,
        err: &mut dyn Write,
        hooks: &mut Hooks,
    ) -> Result<TestReport, Error> {
        // D-1067 (S3, D-1206): the run's temporary
        // root, given to the runtime before any test runs and removed with
        // what the tests left when it drops, after the runtime, on every
        // path; without it, scratch directories use the host's directory.
        let _temp_root = cheby_runtime::file::TestRoot::create().ok();
        // `env::program_name()` is the package name, as under `cheby run`
        // (D-369, D-578).
        let runtime = test_runtime(&self.package, self.stacks, self.threads)?;
        let mut report = TestReport {
            filtered_out: self.filtered_out,
            ..TestReport::default()
        };
        let mut printed = false;
        if let Some(jit) = &self.jit {
            let mut runner = Runner {
                jit,
                runtime: runtime.as_ref(),
                stacks: self.stacks,
                capture: true,
                limit: usize::MAX,
                hooks,
            };
            let mut sink = |test: &Selected, outcome: TestOutcome, chunks: Vec<Chunk>| {
                replay(chunks, out, err)?;
                let line = test_line(&test.module, &test.name, &outcome);
                out.write_all(format!("{line}\n").as_bytes())?;
                printed = true;
                push_result(&mut report, &test.module, &test.name, outcome);
                Ok(())
            };
            let started = Instant::now();
            let end = runner.run(&self.tests, |test| test.def, &mut sink);
            report.run_time = started.elapsed();
            match end {
                Ok(RunEnd::Done) => {}
                Ok(RunEnd::Exited { .. } | RunEnd::WriteFailed) => {
                    // D-783, D-834: the run stops
                    // at once and prints nothing more; `cheby test` exits
                    // with 1. The halt freezes the tests still running and
                    // is completed while `jit` is still alive.
                    complete_halt(runtime);
                    report.terminated = true;
                    return Ok(report);
                }
                Err(error) => {
                    complete_halt(runtime);
                    return Err(error);
                }
            }
        }
        if let Some(runtime) = runtime {
            runtime.shutdown();
        }
        if write_tail(&report, printed, out).is_err() {
            report.terminated = true;
        }
        Ok(report)
    }
}

/// Writes a test's captured output to the streams it was written to, in
/// write order, and flushes standard output where the test flushed it, so
/// the bytes and their buffering are those of a run without the capture
/// (D-723, ADR-0062).
fn replay(chunks: Vec<Chunk>, out: &mut dyn Write, err: &mut dyn Write) -> std::io::Result<()> {
    for chunk in chunks {
        match chunk {
            Chunk::Write(Stream::Stdout, bytes) => out.write_all(&bytes)?,
            Chunk::Write(Stream::Stderr, bytes) => err.write_all(&bytes)?,
            Chunk::Flush => out.flush()?,
        }
    }
    Ok(())
}

/// Writes the failure blocks, each after an empty line, then the summary,
/// after an empty line when a test line was `printed` (D-541, D-598), and
/// flushes `out`.
fn write_tail(report: &TestReport, printed: bool, out: &mut dyn Write) -> std::io::Result<()> {
    for module in &report.modules {
        for test in &module.tests {
            if let TestOutcome::Failed(text) = &test.outcome {
                let block = failure_block(&module.path, &test.name, text);
                out.write_all(format!("\n{block}").as_bytes())?;
            }
        }
    }
    let summary = report.summary();
    let summary = if printed {
        format!("\n{summary}")
    } else {
        summary
    };
    out.write_all(summary.as_bytes())?;
    out.flush()
}

/// Appends a result to the report, opening a new module when it changes.
fn push_result(report: &mut TestReport, module: &str, name: &str, outcome: TestOutcome) {
    let result = TestResult {
        name: name.to_owned(),
        outcome,
    };
    match report.modules.last_mut() {
        Some(last) if last.path == module => last.tests.push(result),
        _ => report.modules.push(ModuleTests {
            path: module.to_owned(),
            not_built: None,
            tests: vec![result],
        }),
    }
}

/// The package's own modules, sorted by module path.
fn local_modules(front: &Frontend) -> Vec<(ModulePath, ModuleId)> {
    let mut modules: Vec<(ModulePath, ModuleId)> = front
        .modules
        .iter()
        .filter(|(_, info)| info.package == PackageId::LOCAL)
        .map(|(id, info)| (info.path.clone(), id))
        .collect();
    modules.sort();
    modules
}

/// Whether `path` is `scope` or one of its submodules.
fn within(path: &ModulePath, scope: &ModulePath) -> bool {
    path.segments().starts_with(scope.segments())
}

/// Whether the full name `<module> "<name>"` contains `filter`.
fn matches_filter(module: &str, name: &str, filter: Option<&str>) -> bool {
    filter.is_none_or(|filter| format!("{module} \"{name}\"").contains(filter))
}

/// The runtime a test run's tests run on (D-690), with `package` as the
/// program name (D-369, D-578), `stacks.fiber` as the default stack of
/// the fibers the tests spawn (D-660) and `threads` workers (`None` reads
/// `CHEBY_THREADS`, D-658), or `None` where fibers are not supported
/// (D-714): the tests then run on the thread backend, with the program
/// name set for isolated entries.
///
/// # Errors
///
/// An [`Error`] with D-658's message for an invalid `CHEBY_THREADS`, or if
/// the runtime's threads cannot start.
fn test_runtime(
    package: &str,
    stacks: Stacks,
    threads: Option<usize>,
) -> Result<Option<Runtime>, Error> {
    let config = RuntimeConfig {
        program_name: package.to_owned(),
        fiber_stack: stacks.fiber,
        threads,
        ..RuntimeConfig::default()
    };
    match Runtime::new(&config) {
        Ok(runtime) => Ok(Some(runtime)),
        Err(RuntimeError::Unsupported) => {
            cheby_runtime::entry::set_program(Vec::new(), package.to_owned());
            Ok(None)
        }
        Err(error) => Err(Error::new(error.to_string())),
    }
}

/// D-306's status, which a termination that a teardown met carries.
const WRITE_FAILURE_STATUS: i32 = 1;

/// How a run of tests ended.
#[derive(Debug)]
enum RunEnd {
    /// Every test ran and was handed to the sink.
    Done,
    /// A write that has no `Result` failed while test `index` or one of
    /// its fibers ran (an uncaptured test: the std self-test, or the thread
    /// backend), and the program ended with `status` (D-306, D-709): the
    /// runtime's halt has started, and the run stops (D-783). The caller
    /// completes the halt while the `Jit` lives.
    Exited { index: usize, status: i32 },
    /// The sink could not write a test's output or line
    /// (D-834): the run stops. The caller halts
    /// the runtime while the `Jit` lives.
    WriteFailed,
}

/// Where the runner hands each test, in source order, with its outcome
/// and its captured output; an error stops the run
/// ([`RunEnd::WriteFailed`]).
type Sink<'s, T> = dyn FnMut(&T, TestOutcome, Vec<Chunk>) -> std::io::Result<()> + 's;

/// The runner's test hooks. The default does nothing; only this crate's
/// tests set them.
#[derive(Default)]
struct Hooks {
    /// Refuses the start of the test at this index, as if no stack could
    /// be taken for it, whenever it returns true
    /// (D-836).
    refuse: Option<Box<dyn FnMut(usize) -> bool>>,
    /// When set, a line per step of the runner, in order: `started <index>
    /// bound <k> running <n>`, `refused <index> running <n>` and `ended
    /// <index>`.
    log: Option<Vec<String>>,
    /// When set, each judged test's index and accounting domain, in the
    /// order the tests ended.
    domains: Option<Vec<(usize, Arc<Domain>)>>,
    /// When set, called with a test's index as soon as the host receives
    /// that test's end, before its group is torn down.
    ended: Option<Box<dyn FnMut(usize)>>,
}

impl Hooks {
    /// Whether the start of test `index` is refused.
    fn refuses(&mut self, index: usize) -> bool {
        self.refuse.as_mut().is_some_and(|refuse| refuse(index))
    }

    /// Logs a step, when the log is set.
    fn note(&mut self, line: impl FnOnce() -> String) {
        if let Some(log) = &mut self.log {
            log.push(line());
        }
    }

    /// Tells the hook that test `index` has ended, when it is set.
    fn end(&mut self, index: usize) {
        if let Some(ended) = &mut self.ended {
            ended(index);
        }
    }
}

/// Runs the tests of one JIT batch as root fibers of one runtime: at most
/// `limit` at once, and at most as many as the runtime has threads when
/// each test starts (§13.6, D-723), each in a group and an accounting
/// domain of its own (D-691, D-799). One host thread, the caller's, starts
/// the tests and waits for any of them on one channel (no polling). In the
/// order the tests end, it tears each one's group down (D-782) and judges
/// it on its own domain (D-771, D-786); then, in source order, as soon as
/// the next test has ended, it hands the test to the sink with its
/// captured output (D-598, ADR-0062). Without a runtime (D-714) the tests
/// run one after another on the thread backend, uncaptured.
struct Runner<'a, 'h> {
    jit: &'a Jit,
    runtime: Option<&'a Runtime>,
    stacks: Stacks,
    /// Whether each test's output is captured (`cheby test`), or written
    /// to the real streams as it runs (the std self-test,
    /// D-837).
    capture: bool,
    /// At most this many tests at once: 1 for the std self-test.
    limit: usize,
    hooks: &'h mut Hooks,
}

/// Where one test stands in a run.
enum Slot {
    /// Not started yet.
    Waiting,
    /// Started, not ended.
    Running(Started),
    /// Ended and judged, waiting for its turn to be written.
    Finished(TestOutcome, Vec<Chunk>),
    /// Handed to the sink.
    Written,
}

/// What the host keeps of a running test.
struct Started {
    /// The test's group: its detached fibers, and its capture (D-799).
    group: Arc<DetachedGroup>,
    /// The test's own accounting domain, which its fibers inherit (D-691).
    domain: Arc<Domain>,
    capture: Option<Arc<Capture>>,
}

/// How an ended test ended for the run.
enum Ended {
    /// It was judged; its captured output.
    Test(TestOutcome, Vec<Chunk>),
    /// A termination ended the program with this status (D-709).
    Exited(i32),
}

/// The objects of a domain that were live before a test ran: none for a
/// test's own domain.
#[derive(Clone, Copy)]
struct Base {
    objects: i64,
    kinds: [i64; RUNTIME_KINDS],
}

impl Base {
    /// A domain of the test's own, which held nothing before it.
    const NONE: Self = Self {
        objects: 0,
        kinds: [0; RUNTIME_KINDS],
    };

    /// What `domain` holds now.
    fn of(domain: &Domain) -> Self {
        Self {
            objects: domain.live_objects(),
            kinds: domain.live_kinds(),
        }
    }
}

impl Runner<'_, '_> {
    /// Runs `tests` (their definitions by `def_of`) and hands each to
    /// `sink` in order.
    ///
    /// # Errors
    ///
    /// An [`Error`] if a test cannot be looked up, started or waited for;
    /// the caller then halts the runtime while the `Jit` lives.
    fn run<T>(
        &mut self,
        tests: &[T],
        def_of: impl Fn(&T) -> DefId,
        sink: &mut Sink<'_, T>,
    ) -> Result<RunEnd, Error> {
        match self.runtime {
            Some(runtime) => self.run_on(runtime, tests, def_of, sink),
            None => self.run_isolated(tests, def_of, sink),
        }
    }

    /// The thread backend (D-714): one test after another, each on an
    /// entry thread of its own, uncaptured, judged on the JIT's domain by
    /// what it left there.
    fn run_isolated<T>(
        &self,
        tests: &[T],
        def_of: impl Fn(&T) -> DefId,
        sink: &mut Sink<'_, T>,
    ) -> Result<RunEnd, Error> {
        let domain = self.jit.domain();
        for (index, test) in tests.iter().enumerate() {
            let base = Base::of(&domain);
            let entry = self.jit.entry(EntryKind::Test(def_of(test)))?;
            let outcome = match entry.run(&self.config(&domain))? {
                // A write failed (D-306, D-709), or the test called
                // `process::exit` (D-1023): the entry thread is parked for
                // good, so the JIT's code it may run is kept (leaked) for
                // the rest of the process. An exit fails only this test.
                EntryOutcome::Exited(status) => {
                    std::mem::forget(self.jit.code_token());
                    match cheby_runtime::process::take_test_exit() {
                        Some(exited) => {
                            TestOutcome::Failed(cheby_runtime::process::test_exit_report(exited))
                        }
                        None => return Ok(RunEnd::Exited { index, status }),
                    }
                }
                outcome => judge(outcome, &domain, base, false)?,
            };
            if sink(test, outcome, Vec::new()).is_err() {
                return Ok(RunEnd::WriteFailed);
            }
        }
        Ok(RunEnd::Done)
    }

    /// The runtime: the tests as root fibers, several at once.
    fn run_on<T>(
        &mut self,
        runtime: &Runtime,
        tests: &[T],
        def_of: impl Fn(&T) -> DefId,
        sink: &mut Sink<'_, T>,
    ) -> Result<RunEnd, Error> {
        // Every entry is looked up before the first test starts, so that a
        // missing one stops the run with no test running.
        let entries = tests
            .iter()
            .map(|test| self.jit.entry(EntryKind::Test(def_of(test))))
            .collect::<Result<Vec<Entry<'_>>, JitError>>()?;
        let (done, ends) = mpsc::channel();
        let mut slots: Vec<Slot> = tests.iter().map(|_| Slot::Waiting).collect();
        let mut next_start: usize = 0;
        let mut next_write: usize = 0;
        let mut running: usize = 0;
        loop {
            // Start tests while fewer run than the bound, read at each
            // start: the runtime's threads now, which a test may change
            // (§13.6, D-835).
            while let Some(&entry) = entries.get(next_start) {
                let bound = runtime.threads().min(self.limit).max(1);
                if running >= bound {
                    break;
                }
                let index = next_start;
                match self.start(runtime, entry, index, &done) {
                    Ok(started) => {
                        self.hooks
                            .note(|| format!("started {index} bound {bound} running {running}"));
                        put(&mut slots, index, Slot::Running(started));
                        running = running.saturating_add(1);
                    }
                    // D-836: no stack for the
                    // test's root while other tests run, whose stacks come
                    // back when they end: wait for one, then start this
                    // test again. With none running, the test fails with
                    // the runtime's text.
                    Err(JitError::Runtime(RuntimeError::Stack(error))) => {
                        self.hooks
                            .note(|| format!("refused {index} running {running}"));
                        if running > 0 {
                            break;
                        }
                        let text = format!("{}\n", RuntimeError::Stack(error));
                        put(
                            &mut slots,
                            index,
                            Slot::Finished(TestOutcome::Failed(text), Vec::new()),
                        );
                    }
                    Err(error) => return Err(error.into()),
                }
                next_start = next_start.saturating_add(1);
            }
            // In source order, every test whose turn has come and that has
            // ended (D-598, ADR-0062).
            while let Some(slot) = slots.get_mut(next_write) {
                if !matches!(slot, Slot::Finished(..)) {
                    break;
                }
                let taken = std::mem::replace(slot, Slot::Written);
                let Slot::Finished(outcome, chunks) = taken else {
                    break;
                };
                let Some(test) = tests.get(next_write) else {
                    break;
                };
                if sink(test, outcome, chunks).is_err() {
                    return Ok(RunEnd::WriteFailed);
                }
                next_write = next_write.saturating_add(1);
            }
            // With none running, every test has started (the loop above
            // starts one whenever none runs) and has been written.
            if running == 0 {
                return Ok(RunEnd::Done);
            }
            // Any running test's end, each reported once (T4r's `RootDone`).
            // The host holds a sender, so the channel stays open.
            let Ok((index, result)) = ends.recv() else {
                return Err(Ice::new("the channel of the tests' ends closed").into());
            };
            running = running.saturating_sub(1);
            self.hooks.note(|| format!("ended {index}"));
            self.hooks.end(index);
            let ended = slots
                .get_mut(index)
                .map(|slot| std::mem::replace(slot, Slot::Written));
            let Some(Slot::Running(started)) = ended else {
                return Err(Ice::new("a test that was not running reported its end").into());
            };
            match self.finish(runtime, index, started, result)? {
                Ended::Test(outcome, chunks) => {
                    put(&mut slots, index, Slot::Finished(outcome, chunks));
                }
                Ended::Exited(status) => return Ok(RunEnd::Exited { index, status }),
            }
        }
    }

    /// Starts test `index`'s root fiber in a new group, with a new capture
    /// when the run captures, charging a new domain, reporting its end on
    /// `done` with its index.
    fn start(
        &mut self,
        runtime: &Runtime,
        entry: Entry<'_>,
        index: usize,
        done: &RootDone,
    ) -> Result<Started, JitError> {
        if self.hooks.refuses(index) {
            let refused = StackError::OutOfMemory {
                bytes: self.stacks.main,
            };
            return Err(JitError::Runtime(RuntimeError::Stack(refused)));
        }
        let domain = Domain::new();
        let capture = self.capture.then(Capture::new);
        let group = match &capture {
            Some(capture) => DetachedGroup::with_capture(Arc::clone(capture)),
            None => DetachedGroup::new(),
        };
        entry.start_root_notify(runtime, &self.config(&domain), &group, index, done.clone())?;
        Ok(Started {
            group,
            domain,
            capture,
        })
    }

    /// The configuration of a test's entry, charging `domain`: a stack as
    /// large as the root fiber that runs `main` (D-125, D-596), the
    /// manifest's `main-stack` (D-660), so a test behaves like the same
    /// code run from `main`.
    fn config(&self, domain: &Arc<Domain>) -> EntryConfig {
        EntryConfig {
            stack_size: self.stacks.main,
            domain: Arc::clone(domain),
            ..EntryConfig::new(EntryMode::Test)
        }
    }

    /// Ends test `index`, whose root reported `result`: tears its group
    /// down, in every build, before its objects are counted (§13.6, D-723,
    /// D-782), judges it on its own domain, and takes its captured output.
    /// A test's fibers run no user code after the teardown (ADR-0029), and
    /// a fiber the teardown skipped only unwinds, so nothing reaches the
    /// capture after this; its releases charge the test's own domain,
    /// which no other test's check reads (D-833).
    fn finish(
        &mut self,
        runtime: &Runtime,
        index: usize,
        started: Started,
        result: Result<EntryOutcome, RuntimeError>,
    ) -> Result<Ended, Error> {
        let outcome = match result {
            Ok(EntryOutcome::Exited(status)) => return Ok(Ended::Exited(status)),
            Ok(outcome) => outcome,
            Err(error) => return Err(Error::new(error.to_string())),
        };
        let teardown = runtime.teardown(&started.group);
        if teardown.halted {
            // An uncaptured detached fiber's write failed meanwhile: the
            // halt has started, so nothing of the runtime is touched any
            // more, the test's result included.
            return Ok(Ended::Exited(WRITE_FAILURE_STATUS));
        }
        // D-771: the system allocator refused a request of the test's
        // domain, so an update it stopped may have leaked what the test did
        // not leak; D-786: the teardown skipped a fiber waiting on the
        // blocking pool or blocked inline, which holds its objects. Either
        // way the test's check is skipped and counted, and only this
        // test's (D-833).
        let skip = teardown.skipped > 0 || started.domain.system_refused();
        let outcome = judge(outcome, &started.domain, Base::NONE, skip)?;
        if let Some(domains) = &mut self.hooks.domains {
            domains.push((index, Arc::clone(&started.domain)));
        }
        let chunks = started
            .capture
            .map(|capture| capture.take())
            .unwrap_or_default();
        Ok(Ended::Test(outcome, chunks))
    }
}

/// Sets `slots[index]`.
fn put(slots: &mut [Slot], index: usize, slot: Slot) {
    if let Some(place) = slots.get_mut(index) {
        *place = slot;
    }
}

/// How a test whose entry did not end the program ended: from `outcome`
/// and, for a passing test whose check is not `skip`ped, the objects of
/// `domain` still live above `base` (D-458, D-541).
///
/// # Errors
///
/// An [`Error`] for an entry that returned neither `Nil` nor a `String`,
/// or that ended the program (`Exited`, which callers handle first).
fn judge(
    outcome: EntryOutcome,
    domain: &Arc<Domain>,
    base: Base,
    skip: bool,
) -> Result<TestOutcome, Error> {
    // A test thunk returns the static `Nil` when the test passed, and the
    // text of the `Err` it returned as an owned `String` otherwise
    // (D-529, D-595). Whether it panicked comes from the entry's catch
    // point, which reads the fiber's unwind record (D-699), never from the
    // word: under the flag backend an unwound thunk returns 0.
    Ok(match outcome {
        // The captured report goes into the failure block (D-701). As in
        // M1, a failing test gets no leak check.
        EntryOutcome::Panicked(report) => TestOutcome::Failed(report.to_string()),
        // The test's domain deadlocked (D-857): the runtime cancelled its
        // root scope, so its fibers unwound, and `finish` tore its group
        // down as at every test's end. The report is the failure block,
        // and a failing test gets no leak check (D-859, D-541).
        EntryOutcome::Deadlocked(report) => TestOutcome::Failed(report),
        EntryOutcome::Returned(word) if word == cheby_runtime::values::nil() && skip => {
            domain.stats().skip_leak_check();
            TestOutcome::Passed
        }
        EntryOutcome::Returned(word) if word == cheby_runtime::values::nil() => {
            // D-541: a passing test must leave its domain as it found it,
            // or fail with D-458's message and, when runtime kinds are
            // among the leaked objects, D-711's line (D-784).
            let leaked = domain.live_objects().saturating_sub(base.objects);
            match cheby_runtime::entry::leak_report(leaked.max(0)) {
                Some(message) => {
                    let mut kinds = domain.live_kinds();
                    for (kind, before) in kinds.iter_mut().zip(base.kinds) {
                        *kind = kind.saturating_sub(before);
                    }
                    match cheby_runtime::alloc::leaked_kinds_line(kinds, leaked) {
                        Some(line) => TestOutcome::Failed(format!("{message}{line}\n")),
                        None => TestOutcome::Failed(message),
                    }
                }
                None => TestOutcome::Passed,
            }
        }
        EntryOutcome::Returned(word) => {
            // SAFETY: a test thunk that did not return `Nil` returned an
            // owned, live `String` object of the `Jit`, which is alive here.
            let text = unsafe { cheby_runtime::string::to_rust_string(word) }.ok_or_else(|| {
                Error::from(Ice::new(
                    "a test entry returned neither `Nil` nor a `String`",
                ))
            })?;
            cheby_runtime::alloc::with_domain(Arc::clone(domain), || {
                // SAFETY: `word` is the owned reference the thunk returned;
                // it is released once, in the domain it was allocated in.
                unsafe { cheby_runtime::rc::drop(word) };
            });
            TestOutcome::Failed(format!("{text}\n"))
        }
        EntryOutcome::Exited(_) => {
            return Err(Ice::new("a test that ended the program was judged").into());
        }
    })
}

/// Completes the halt that a termination started (D-709), or halts a run
/// that stops early (a failed write of the report or of captured output,
/// D-834, or an internal error): every thread of
/// `runtime` is joined and its fibers, the running tests' included, stay
/// frozen. The caller keeps the `Jit` alive until this returns and drops
/// no value of the runtime (D-073, D-708).
fn complete_halt(runtime: Option<Runtime>) {
    if let Some(runtime) = runtime {
        runtime.halt();
    }
}

/// The package name of the std self-test's synthetic package.
const STD_TEST_PACKAGE: &str = "std_self_test";

/// Tests the standard library itself (H3, D-479, D-542): checks every
/// shipped std module with its `test` blocks, then builds and runs the
/// tests whose full name `std::<module> "<name>"` contains `filter`, one
/// JIT batch per module. A module whose batch does not build has each of
/// its tests built alone. Modules and tests that do not build are counted
/// and reported with their codes, and the other modules still run.
///
/// The tests' own output goes to standard output as they run; the report
/// is returned for [`TestReport::render`].
///
/// # Errors
///
/// An [`Error`] for internal failures that stop the whole run, such as a
/// std file that does not parse.
pub fn test_std(filter: Option<&str>) -> Result<TestReport, Error> {
    let mut sources = SourceMap::new();
    let mut diagnostics = Vec::new();
    let package = std_test_package(&mut sources, &mut diagnostics)?;
    let front = frontend::run(
        package,
        frontend::Mode::StdTests,
        &mut sources,
        &mut diagnostics,
    )?;
    cheby_runtime::entry::set_program(Vec::new(), STD_TEST_PACKAGE.to_owned());
    let mut report = TestReport::default();
    let mut std_modules: Vec<(ModulePath, ModuleId)> = front
        .modules
        .iter()
        .filter(|(id, info)| info.package == PackageId::STD && *id != ModuleId::BUILTIN)
        .map(|(id, info)| (info.path.clone(), id))
        .collect();
    std_modules.sort();
    for (path, module) in std_modules {
        let shown = path.to_string();
        let errors = codes_in(&front, module, &diagnostics);
        let Some(checked) = front.checked(module) else {
            report.modules.push(ModuleTests {
                path: shown,
                not_built: Some(errors.unwrap_or_else(|| "not checked".to_owned())),
                tests: Vec::new(),
            });
            continue;
        };
        let mut selected: Vec<&thir::TestDef> = Vec::new();
        for test in checked.thir.tests.iter().filter(|t| on_native(&front, t)) {
            if matches_filter(&shown, &test.name, filter) {
                selected.push(test);
            } else {
                report.filtered_out = report.filtered_out.saturating_add(1);
            }
        }
        if selected.is_empty() {
            continue;
        }
        if let Some(codes) = errors {
            report
                .modules
                .push(not_built_module(shown, &selected, codes));
            continue;
        }
        report.modules.push(std_module_tests(
            &front, &sources, module, shown, &selected,
        )?);
    }
    Ok(report)
}

/// Builds and runs one std module's selected tests: one batch, or one
/// batch per test when the module's batch does not build.
fn std_module_tests(
    front: &Frontend,
    sources: &SourceMap,
    module: ModuleId,
    path: String,
    selected: &[&thir::TestDef],
) -> Result<ModuleTests, Error> {
    let roots = selected
        .iter()
        .map(|&test| TestRoot { module, test })
        .collect();
    // One runtime per module batch (D-690), stopped when it returns.
    // Std has no manifest: the default stacks (D-660).
    let runtime = test_runtime(STD_TEST_PACKAGE, Stacks::DEFAULT, None)?;
    let mut tests = Vec::with_capacity(selected.len());
    let batch = match build_codes(front, sources, Roots::Tests(roots)) {
        Ok(jit) => jit,
        Err(codes) => {
            for &test in selected {
                let alone = vec![TestRoot { module, test }];
                match build_codes(front, sources, Roots::Tests(alone)) {
                    // The runner tears the test's detached fibers down
                    // before it returns, so none of them can run this
                    // test's code once `jit` is dropped here (D-708,
                    // D-782); the runtime's token keeps the code until
                    // the runtime stops anyway.
                    Ok(jit) => {
                        let ran = run_std_tests(&jit, runtime.as_ref(), &path, &[test], &mut tests);
                        if let Err(error) = ran {
                            complete_halt(runtime);
                            drop(jit);
                            return Err(error);
                        }
                    }
                    Err(codes) => tests.push(TestResult {
                        name: test.name.to_string(),
                        outcome: TestOutcome::NotBuilt(codes),
                    }),
                }
            }
            if tests
                .iter()
                .all(|t| matches!(t.outcome, TestOutcome::NotBuilt(_)))
            {
                return Ok(not_built_module(path, selected, codes));
            }
            return Ok(ModuleTests {
                path,
                not_built: None,
                tests,
            });
        }
    };
    if let Err(error) = run_std_tests(&batch, runtime.as_ref(), &path, selected, &mut tests) {
        complete_halt(runtime);
        drop(batch);
        return Err(error);
    }
    Ok(ModuleTests {
        path,
        not_built: None,
        tests,
    })
}

/// Runs `tests` of `jit`, std module `path`'s, one at a time and
/// uncaptured, each in an accounting domain of its own, through `cheby
/// test`'s runner with a bound of one (D-542, D-837),
/// and appends their results to `results`.
///
/// # Errors
///
/// D-803's error when a write failure ended the program during a test,
/// which stops the self-test, and an [`Error`] for an internal failure.
/// The caller halts `runtime` while `jit` lives.
fn run_std_tests(
    jit: &Jit,
    runtime: Option<&Runtime>,
    path: &str,
    tests: &[&thir::TestDef],
    results: &mut Vec<TestResult>,
) -> Result<(), Error> {
    let mut hooks = Hooks::default();
    let mut runner = Runner {
        jit,
        runtime,
        stacks: Stacks::DEFAULT,
        capture: false,
        limit: 1,
        hooks: &mut hooks,
    };
    let end = runner.run(tests, |test| test.def, &mut |test, outcome, _chunks| {
        results.push(TestResult {
            name: test.name.to_string(),
            outcome,
        });
        Ok(())
    })?;
    match end {
        RunEnd::Done => Ok(()),
        RunEnd::Exited { index, status } => {
            let name = tests
                .get(index)
                .map(|test| test.name.to_string())
                .unwrap_or_default();
            Err(terminated(path, &name, status))
        }
        RunEnd::WriteFailed => Err(Ice::new("the std self-test could not keep a result").into()),
    }
}

/// The error that stops the std self-test when a write failure ended the
/// program during the test `name` of `module` (D-306, D-709). The
/// self-test stops there, as `cheby test` does (D-783), but says why: it
/// is a developer tool, whose report would otherwise end without a word
/// (D-803).
fn terminated(module: &str, name: &str, status: i32) -> Error {
    Error::new(format!(
        "the std self-test stopped: a write failed during test {module} \"{name}\", \
         which ended the program with status {status}"
    ))
}

/// A module none of whose selected tests built.
fn not_built_module(path: String, selected: &[&thir::TestDef], codes: String) -> ModuleTests {
    ModuleTests {
        path,
        tests: selected
            .iter()
            .map(|test| TestResult {
                name: test.name.to_string(),
                outcome: TestOutcome::NotBuilt(codes.clone()),
            })
            .collect(),
        not_built: Some(codes),
    }
}

/// Builds `roots`; a build that fails gives its distinct codes, sorted
/// and joined with `, `, or the message of the error that stopped it (an
/// internal compiler error's starts with `internal compiler error: `).
fn build_codes(front: &Frontend, sources: &SourceMap, roots: Roots<'_>) -> Result<Jit, String> {
    match backend::build(front, sources, roots, false) {
        Ok(Ok(built)) => Ok(built.jit),
        Ok(Err(diagnostics)) => Err(join_codes(&diagnostics)),
        Err(e) => Err(e.message().to_owned()),
    }
}

/// The distinct codes of `diagnostics`' errors, sorted, joined with `, `.
fn join_codes<'a>(diagnostics: impl IntoIterator<Item = &'a Diagnostic>) -> String {
    let mut codes: Vec<String> = diagnostics
        .into_iter()
        .filter(|d| d.is_error())
        .map(|d| d.code.to_string())
        .collect();
    codes.sort();
    codes.dedup();
    codes.join(", ")
}

/// The codes of the front-end errors located in `module`'s file, if any.
fn codes_in(front: &Frontend, module: ModuleId, diagnostics: &[Diagnostic]) -> Option<String> {
    let file = front.modules.get(module)?.file?;
    let here: Vec<&Diagnostic> = diagnostics
        .iter()
        .filter(|d| d.is_error() && d.primary_span().is_some_and(|s| s.file == file))
        .collect();
    (!here.is_empty()).then(|| join_codes(here))
}

/// An in-memory package whose root module imports every shipped std
/// module, so that one front-end run checks them all.
pub(crate) fn std_test_package(
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Package, Error> {
    let manifest_text = format!("name = \"{STD_TEST_PACKAGE}\"\nversion = \"0.0.0\"\n");
    let manifest_file = sources
        .add_file("cheby.toml", manifest_text.clone())
        .map_err(|_| Error::new("internal compiler error: the std test manifest".to_owned()))?;
    let manifest = Manifest::parse(manifest_file, &manifest_text, diagnostics)
        .ok_or_else(|| Error::from(Ice::new("the std self-test's manifest does not parse")))?;
    let mut text = String::from("//! Imports every shipped std module (D-542).\n\n");
    for module in std_lib::STD_MODULES {
        let _ = writeln!(text, "import std::{}", module.path);
    }
    let hashes = crate::SourceHashes::of(text.as_bytes());
    let source = parse_file(sources, "src/main.cheby", text.into_bytes(), diagnostics)?;
    if source.has_syntax_errors {
        return Err(Ice::new("the std self-test's root module does not parse").into());
    }
    Ok(Package {
        root: PathBuf::new(),
        manifest,
        modules: vec![Module {
            path: ModulePath::root(STD_TEST_PACKAGE),
            source,
            hashes,
            parsed: true,
            // `test_std` uses no cache, the only reader of these.
            syntax_diagnostics: Vec::new(),
        }],
    })
}

#[cfg(test)]
mod stack_tests {
    //! The `[runtime]` stacks reach every host that runs a root (D-660):
    //! `cheby run`'s `RunConfig` and `cheby test`'s stacks, and the std
    //! self-test keeps the defaults.

    use std::path::PathBuf;

    use cheby_package::Manifest;
    use cheby_runtime::entry::{AtExit, DEFAULT_ROOT_STACK_SIZE, DEFAULT_STACK_SIZE};

    use super::{PreparedTests, Stacks, TestOptions, prepare_test};
    use crate::{Prepared, RunOptions, prepare_run};

    /// A package `app` in a fresh temporary directory with a `main`, one
    /// test and `runtime` as its `[runtime]` table's lines.
    fn package(name: &str, runtime: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cheby_stacks_{}_{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("cheby.toml"),
            format!("name = \"app\"\nversion = \"0.1.0\"\n\n[runtime]\n{runtime}"),
        )
        .unwrap();
        std::fs::write(
            dir.join("src/main.cheby"),
            "//! Root.\n\nfn main() {\n  assert 1 == 1\n}\n\ntest \"one\" {\n  assert 1 == 1\n}\n",
        )
        .unwrap();
        dir
    }

    /// The `RunConfig` of `cheby run` and the stacks of `cheby test` for
    /// the package at `dir`.
    fn stacks_of(dir: &std::path::Path) -> (usize, usize, AtExit, Stacks) {
        let Prepared::Ready(program) = prepare_run(dir, RunOptions::default()).unwrap() else {
            panic!("the package did not build");
        };
        let PreparedTests::Ready(tests) = prepare_test(dir, &TestOptions::default()).unwrap()
        else {
            panic!("the tests did not build");
        };
        (
            program.config.root_stack_size,
            program.config.fiber_stack,
            program.config.at_exit,
            tests.stacks,
        )
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn runtime_stacks_reach_run_and_test() {
        let dir = package("set", "fiber-stack = \"3MiB\"\nmain-stack = \"5MiB\"\n");
        let (main, fiber, at_exit, stacks) = stacks_of(&dir);
        assert_eq!((main, fiber), (5 << 20, 3 << 20));
        assert_eq!(at_exit, AtExit::Teardown);
        assert_eq!(
            stacks,
            Stacks {
                main: 5 << 20,
                fiber: 3 << 20
            }
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn runtime_stacks_default_without_the_table() {
        let dir = package("default", "");
        let (main, fiber, _, stacks) = stacks_of(&dir);
        assert_eq!((main, fiber), (DEFAULT_ROOT_STACK_SIZE, DEFAULT_STACK_SIZE));
        assert_eq!(stacks, Stacks::DEFAULT);
        assert_eq!(
            u64::try_from(DEFAULT_ROOT_STACK_SIZE).unwrap(),
            Manifest::DEFAULT_MAIN_STACK
        );
        assert_eq!(
            u64::try_from(DEFAULT_STACK_SIZE).unwrap(),
            Manifest::DEFAULT_FIBER_STACK
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(all(test, unix))]
mod stuck_tests {
    //! D-786 in `cheby test`: a fiber that a teardown skipped goes on when
    //! its system call returns and releases what it holds into its own
    //! test's domain, so only that test's check is skipped and the tests
    //! after it are checked (D-833, which supersedes
    //! D-805's skips of later checks).

    use std::io::Write as _;
    use std::time::{Duration, Instant};

    use super::{Hooks, PreparedTests, TestOptions, TestOutcome, prepare_test};

    /// Test "a" leaves a detached fiber, holding 21 objects, blocked
    /// opening a FIFO on the blocking pool. Test "b" writes a marker file,
    /// on which the host writes to the FIFO, so that fiber unwinds and
    /// releases what it holds while "b" waits; "b" leaks a receiver. Test
    /// "c" leaks a receiver too, once the fiber is gone. The tests run one
    /// at a time, so "b" runs while the fiber lives. Under D-805, "b"'s
    /// check was skipped, because the release charged the domain all tests
    /// shared (review finding ROB2-2 of gate 2); with a domain per test the
    /// release charges "a"'s, so "b"'s check runs and finds its leak, and
    /// only "a"'s is skipped and counted.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn lifecycle_a_later_test_is_checked_while_a_skipped_fiber_lives() {
        let dir = std::env::temp_dir().join(format!("cheby_stuck_tests_{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let fifo = dir.join("fifo");
        let marker = dir.join("marker");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success(), "mkfifo failed");
        std::fs::write(
            dir.join("cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/main.cheby"),
            format!(
                "//! A skipped fiber releases its objects during a later test.\n\n\
                 import std::channel\nimport std::duration\nimport std::fiber\nimport std::io\n\
                 import std::list\nimport std::selector\n\n\
                 type Mail {{\n  Mail(channel::Receiver<Mail>)\n}}\n\n\
                 fn pause(ms: Int) -> Nil {{\n  selector::new()\n  \
                 |> selector::after(_, duration::milliseconds(ms), fn() {{ Nil }})\n  \
                 |> selector::select\n}}\n\n\
                 fn main() {{\n  Nil\n}}\n\n\
                 test \"a leaves a blocked reader\" {{\n  fiber::spawn_detached(fn() {{\n    \
                 let held = list::map(list::range(0, 20), fn(i) {{ \"item {{i}}\" }})\n    \
                 let _ = io::read_file(\"{fifo}\")\n    let _ = held\n    Nil\n  }})\n  \
                 pause(200)\n  assert True\n}}\n\n\
                 test \"b leaks a receiver\" {{\n  \
                 let assert Ok(Nil) = io::write_file(\"{marker}\", \"b\")\n  \
                 let (tx, rx) = channel::new(1)\n  let _ = channel::send(tx, Mail(rx))\n  \
                 pause(500)\n  assert True\n}}\n\n\
                 test \"c leaks a receiver too\" {{\n  let (tx, rx) = channel::new(1)\n  \
                 let _ = channel::send(tx, Mail(rx))\n  assert True\n}}\n",
                fifo = fifo.display(),
                marker = marker.display(),
            ),
        )
        .unwrap();
        let PreparedTests::Ready(mut program) =
            prepare_test(&dir, &TestOptions::default()).unwrap()
        else {
            panic!("the tests did not build");
        };
        program.set_threads(1);
        let host = {
            let (fifo, marker) = (fifo.clone(), marker.clone());
            std::thread::spawn(move || {
                let start = Instant::now();
                while !marker.exists() {
                    assert!(start.elapsed() < Duration::from_secs(10), "no marker");
                    std::thread::sleep(Duration::from_millis(1));
                }
                let mut writer = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
                writer.write_all(b"x").unwrap();
            })
        };
        let mut out = Vec::new();
        let mut hooks = Hooks {
            domains: Some(Vec::new()),
            ..Hooks::default()
        };
        let report = program
            .run_with(&mut out, &mut std::io::sink(), &mut hooks)
            .unwrap();
        host.join().unwrap();
        let outcomes: Vec<(&str, bool)> = report
            .modules
            .iter()
            .flat_map(|module| &module.tests)
            .map(|test| (test.name.as_str(), test.outcome == TestOutcome::Passed))
            .collect();
        let leak_check = cheby_runtime::LEAK_CHECK_ENABLED;
        assert_eq!(
            outcomes,
            [
                ("a leaves a blocked reader", true),
                ("b leaks a receiver", !leak_check),
                ("c leaks a receiver too", !leak_check),
            ],
            "{}",
            String::from_utf8_lossy(&out)
        );
        // Only "a"'s check is skipped and counted (D-786), in its own
        // domain.
        let skipped: Vec<(usize, u64)> = hooks
            .domains
            .unwrap()
            .iter()
            .map(|(index, domain)| (*index, domain.stats().leak_checks_skipped()))
            .collect();
        assert_eq!(skipped, [(0, 1), (1, 0), (2, 0)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
#[cfg(unix)]
mod runner_tests {
    //! The parallel runner (D-723): retries of a start refused a stack
    //! (D-836), the bound read at each start
    //! (D-835), and checks on a domain per
    //! test (D-691, D-771, D-833), with tests that
    //! run at once.

    use std::path::PathBuf;
    use std::sync::Arc;

    use cheby_runtime::alloc::Domain;

    use super::{Hooks, PreparedTests, TestOptions, TestOutcome, TestProgram, prepare_test};

    /// `pause(ms)`, which waits on a timer. A test that must leak sends a
    /// channel's own receiver into it (`MAIL` and `LEAK`, as
    /// `tests/ui/13-tooling/test/leak.cheby` does): binding a kernel to drop
    /// its result is E0055 since D-944.
    const HELPERS: &str = "import std::duration\nimport std::selector\n\n\
         fn pause(ms: Int) -> Nil {\n  selector::new()\n  \
         |> selector::after(_, duration::milliseconds(ms), fn() { Nil })\n  \
         |> selector::select\n}\n\n";

    /// A type whose value holds a channel's receiver: sent into its own
    /// channel, it keeps the channel alive forever, a leak the spec
    /// documents (§9.6, D-029). Needs `import std::channel`.
    const MAIL: &str = "type Mail {\n  Mail(channel::Receiver<Mail>)\n}\n\n";

    /// The statements of a test that leaks a receiver ([`MAIL`]).
    const LEAK: &str = "  let (tx, rx) = channel::new(1)\n  let _ = channel::send(tx, Mail(rx))\n";

    /// A package `app` in a fresh temporary directory whose root module
    /// holds [`HELPERS`], then `body`, compiled with every test selected,
    /// to run on `threads` workers.
    fn program(name: &str, imports: &str, body: &str, threads: usize) -> (PathBuf, TestProgram) {
        let dir = std::env::temp_dir().join(format!("cheby_runner_{}_{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let source = format!(
            "//! Tests for the runner.\n\n{imports}{HELPERS}fn main() {{\n  Nil\n}}\n\n{body}"
        );
        std::fs::write(dir.join("src/main.cheby"), source).unwrap();
        let PreparedTests::Ready(mut program) =
            prepare_test(&dir, &TestOptions::default()).unwrap()
        else {
            panic!("the tests did not build");
        };
        program.set_threads(threads);
        (dir, *program)
    }

    /// Runs `program` with `hooks`; its report's text and each test's name
    /// and whether it passed.
    fn run(program: TestProgram, hooks: &mut Hooks) -> (String, Vec<(String, TestOutcome)>) {
        let mut out = Vec::new();
        let report = program
            .run_with(&mut out, &mut std::io::sink(), hooks)
            .unwrap();
        assert!(!report.terminated);
        let outcomes = report
            .modules
            .into_iter()
            .flat_map(|module| module.tests)
            .map(|test| (test.name, test.outcome))
            .collect();
        (String::from_utf8(out).unwrap(), outcomes)
    }

    /// Whether every test passed.
    fn all_passed(outcomes: &[(String, TestOutcome)]) -> bool {
        outcomes
            .iter()
            .all(|(_, outcome)| *outcome == TestOutcome::Passed)
    }

    /// The skipped checks counted in each test's domain, by test index.
    fn skipped(domains: Option<Vec<(usize, Arc<Domain>)>>) -> Vec<(usize, u64)> {
        let mut skipped: Vec<(usize, u64)> = domains
            .unwrap()
            .iter()
            .map(|(index, domain)| (*index, domain.stats().leak_checks_skipped()))
            .collect();
        skipped.sort_unstable();
        skipped
    }

    const TWO_TESTS: &str = "test \"a waits\" {\n  pause(200)\n  assert True\n}\n\n\
         test \"b runs\" {\n  assert True\n}\n";

    /// D-836: a start refused a stack while
    /// another test runs waits for that test to end and is tried again;
    /// with no test running, the test fails with the runtime's text and
    /// the next one still runs.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn runner_retries_a_start_refused_a_stack_and_fails_it_alone() {
        let (dir, retried) = program("retry", "", TWO_TESTS, 2);
        let mut first = true;
        let mut hooks = Hooks {
            refuse: Some(Box::new(move |index| {
                let refuse = index == 1 && first;
                first &= index != 1;
                refuse
            })),
            log: Some(Vec::new()),
            ..Hooks::default()
        };
        let (text, outcomes) = run(retried, &mut hooks);
        assert_eq!(
            hooks.log.unwrap(),
            [
                "started 0 bound 2 running 0",
                "refused 1 running 1",
                "ended 0",
                "started 1 bound 2 running 0",
                "ended 1",
            ]
        );
        assert!(all_passed(&outcomes), "{text}");
        let PreparedTests::Ready(mut failed) = prepare_test(&dir, &TestOptions::default()).unwrap()
        else {
            panic!("the tests did not build");
        };
        failed.set_threads(2);
        let mut hooks = Hooks {
            refuse: Some(Box::new(|index| index == 0)),
            log: Some(Vec::new()),
            ..Hooks::default()
        };
        let (text, _) = run(*failed, &mut hooks);
        assert_eq!(
            hooks.log.unwrap(),
            [
                "refused 0 running 0",
                "started 1 bound 2 running 0",
                "ended 1",
            ]
        );
        assert_eq!(
            text,
            format!(
                "test app \"a waits\" ... FAILED\ntest app \"b runs\" ... ok\n\n\
                 ---- app \"a waits\" ----\n\
                 cannot take a stack for the entry: a stack region of {} bytes could not be mapped\n\
                 \ntest result: FAILED. 1 passed; 1 failed; 0 filtered out\n",
                cheby_runtime::entry::DEFAULT_ROOT_STACK_SIZE
            )
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// §13.6, D-835: the bound on the
    /// tests running at once is the runtime's thread count when each test
    /// starts, so a test that lowers it holds back the tests that start
    /// after it, while those already running go on.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn runner_bound_is_read_when_each_test_starts() {
        // "a" waits first, so that "b" to "d" start under the first count.
        let body = "test \"a lowers the count\" {\n  pause(100)\n  runtime::set_thread_count(1)\n}\n\n\
             test \"b waits\" {\n  pause(300)\n}\n\n\
             test \"c waits\" {\n  pause(300)\n}\n\n\
             test \"d waits\" {\n  pause(300)\n}\n\n\
             test \"e runs\" {\n  assert True\n}\n\n\
             test \"f runs\" {\n  assert True\n}\n";
        let (dir, program) = program("bound", "import std::runtime\n", body, 4);
        let mut hooks = Hooks {
            log: Some(Vec::new()),
            ..Hooks::default()
        };
        let (text, outcomes) = run(program, &mut hooks);
        assert!(all_passed(&outcomes), "{text}");
        let starts: Vec<String> = hooks
            .log
            .unwrap()
            .into_iter()
            .filter(|line| line.starts_with("started"))
            .collect();
        assert_eq!(
            starts,
            [
                "started 0 bound 4 running 0",
                "started 1 bound 4 running 1",
                "started 2 bound 4 running 2",
                "started 3 bound 4 running 3",
                "started 4 bound 1 running 0",
                "started 5 bound 1 running 0",
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// D-691, D-833: each test is checked on a domain
    /// of its own, so a test that leaks one object fails with the leak
    /// check's message while a test beside it allocates and frees a million
    /// strings, and that test passes.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn runner_checks_each_test_on_its_own_domain() {
        let body = format!(
            "{MAIL}fn churn(n: Int, total: Int) -> Int {{\n  case n {{\n    0 => total\n    \
             _ => churn(n - 1, total + string::length(\"item {{n}}\"))\n  }}\n}}\n\n\
             test \"churns a million strings\" {{\n  assert churn(1000000, 0) > 0\n}}\n\n\
             test \"leaks a receiver\" {{\n{LEAK}  pause(50)\n}}\n"
        );
        let imports = "import std::channel\nimport std::string\n";
        let (dir, program) = program("domains", imports, &body, 2);
        let mut hooks = Hooks {
            domains: Some(Vec::new()),
            ..Hooks::default()
        };
        let (text, outcomes) = run(program, &mut hooks);
        let leak_check = cheby_runtime::LEAK_CHECK_ENABLED;
        let leaked = if leak_check {
            // The receiver and the `Mail` that holds it (D-784's line).
            TestOutcome::Failed(
                "error: leak check failed: 2 objects are still live at exit\n\
                 note: leaked 1 <Receiver>, and 1 object of other kinds\n"
                    .to_owned(),
            )
        } else {
            TestOutcome::Passed
        };
        assert_eq!(
            outcomes,
            [
                ("churns a million strings".to_owned(), TestOutcome::Passed),
                ("leaks a receiver".to_owned(), leaked),
            ],
            "{text}"
        );
        assert_eq!(skipped(hooks.domains), [(0, 0), (1, 0)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// D-771 per test: a fiber of one test is refused memory by the system
    /// allocator, so that test's check, and only that one, is skipped and
    /// counted; a test running beside it that leaks still fails, and a
    /// clean one is checked.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn runner_skips_only_the_check_of_the_test_refused_memory() {
        let body = format!(
            "{MAIL}@target(native)\nfn ask(done: channel::Sender<Nil>) -> Int {{\n  let _ = done\n  \
             string::length(string::repeat(\"x\", 1152921504606846976))\n}}\n\n\
             @target(native)\ntest \"a is refused memory\" {{\n  let (done_tx, done_rx) = channel::new::<Nil>(1)\n  \
             fiber::spawn_detached(fn() {{ ask(done_tx) }})\n  \
             assert channel::receive(done_rx) == Err(Nil)\n}}\n\n\
             test \"b leaks a receiver\" {{\n{LEAK}  pause(50)\n}}\n\n\
             test \"c is clean\" {{\n  assert True\n}}\n"
        );
        let imports = "import std::channel\nimport std::fiber\nimport std::string\n";
        let (dir, program) = program("refused", imports, &body, 3);
        let mut hooks = Hooks {
            domains: Some(Vec::new()),
            ..Hooks::default()
        };
        let (text, outcomes) = run(program, &mut hooks);
        let passed: Vec<(&str, bool)> = outcomes
            .iter()
            .map(|(name, outcome)| (name.as_str(), *outcome == TestOutcome::Passed))
            .collect();
        let leak_check = cheby_runtime::LEAK_CHECK_ENABLED;
        assert_eq!(
            passed,
            [
                ("a is refused memory", true),
                ("b leaks a receiver", !leak_check),
                ("c is clean", true),
            ],
            "{text}"
        );
        assert_eq!(skipped(hooks.domains), [(0, 1), (1, 0), (2, 0)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// D-857, D-859: a deadlock is judged per test, also while another test
    /// runs. Test "a" deadlocks; test "b", which runs beside it, keeps
    /// waiting on timers and the blocking pool until a marker file exists,
    /// which the host writes only when it receives "a"'s end. So "a"'s end
    /// is reported while "b" still runs, whatever the detector's latency,
    /// and "b" then passes. "a"'s failure block is the report, with the
    /// location of its receive, and no leak check runs for it (D-541).
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn deadlock_of_a_test_is_reported_while_a_sibling_runs() {
        let marker = std::env::temp_dir().join(format!(
            "cheby_runner_{}_deadlock_marker",
            std::process::id()
        ));
        if marker.exists() {
            std::fs::remove_file(&marker).unwrap();
        }
        let body = format!(
            "fn wait_for_marker(tries: Int) -> Bool {{\n  \
             case io::read_file(\"{marker}\"), tries {{\n    \
             Ok(_), _ => True\n    Err(_), 0 => False\n    Err(_), _ => {{\n      \
             pause(5)\n      wait_for_marker(tries - 1)\n    }}\n  }}\n}}\n\n\
             test \"a deadlocks\" {{\n  let (tx, rx) = channel::new::<Int>(0)\n  \
             let assert Ok(n) = channel::receive(rx)\n  \
             assert channel::send(tx, n) == Ok(Nil)\n}}\n\n\
             test \"b runs until a has ended\" {{\n  assert wait_for_marker(12000)\n}}\n",
            marker = marker.display(),
        );
        let (dir, program) = program(
            "deadlock",
            "import std::channel\nimport std::io\n",
            &body,
            2,
        );
        // The receive's line in the module the runner compiled.
        let source = std::fs::read_to_string(dir.join("src/main.cheby")).unwrap();
        let line = source
            .lines()
            .position(|line| line == "  let assert Ok(n) = channel::receive(rx)")
            .unwrap()
            + 1;
        let mut hooks = Hooks {
            log: Some(Vec::new()),
            ended: Some(Box::new({
                let marker = marker.clone();
                move |index| {
                    if index == 0 {
                        std::fs::write(&marker, "a has ended").unwrap();
                    }
                }
            })),
            ..Hooks::default()
        };
        let (text, outcomes) = run(program, &mut hooks);
        let report = format!(
            "fatal error: all fibers are blocked\n  \
             blocked in channel::receive at src/main.cheby:{line}:22\n"
        );
        assert_eq!(
            outcomes,
            [
                (
                    "a deadlocks".to_owned(),
                    TestOutcome::Failed(report.clone())
                ),
                ("b runs until a has ended".to_owned(), TestOutcome::Passed),
            ],
            "{text}"
        );
        // Both tests started before either ended, and "a" ended first.
        assert_eq!(
            hooks.log.unwrap(),
            [
                "started 0 bound 2 running 0",
                "started 1 bound 2 running 1",
                "ended 0",
                "ended 1",
            ]
        );
        assert_eq!(
            text,
            format!(
                "test app \"a deadlocks\" ... FAILED\ntest app \"b runs until a has ended\" ... ok\n\n\
                 ---- app \"a deadlocks\" ----\n{report}\n\
                 test result: FAILED. 1 passed; 1 failed; 0 filtered out\n"
            )
        );
        std::fs::remove_file(&marker).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
