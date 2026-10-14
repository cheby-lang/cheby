//! `cheby test` (G4, §13.6, D-541) and the standard library's self-test
//! (H3, D-479, D-542).
//!
//! `cheby test [FILTER] [--module PATH]` checks the package, selects the
//! tests of its own modules whose full name `<module path> "<test name>"`
//! contains `FILTER` and that belong to `PATH` or a submodule of it,
//! compiles them in one JIT batch (`Roots::Tests`, D-265: tests are their
//! own units) and runs them one after another, by module path and then in
//! source order. Standard output gets:
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
//! A test's line is written when it has finished, so the program's own
//! output, which is not captured, comes before its line. A failure block
//! holds the panic report (D-449), the text of the `Err` the test returned
//! (D-529), or, in a toolchain with debug assertions, D-458's leak message
//! when the test left objects of the build's accounting domain alive. The
//! summary is preceded by an empty line when anything was printed before
//! it. Compile errors stop the command before any test runs.
//!
//! Every test runs as a root fiber on a 64 MiB stack (D-596), one after
//! another, on one [`Runtime`] per `cheby test` run and per std module
//! batch (D-690), which reads `CHEBY_THREADS` before the first test
//! (D-658). Where fibers are not supported (Windows until M6, D-714),
//! tests run on the thread backend instead.
//!
//! The std self-test checks every shipped std module with its tests
//! (`Tests::All`), then builds and runs one batch per module. When a
//! module's batch does not build, each of its tests is built alone, so
//! the ones that build still run. A module or test that does not build is
//! counted and reported with its codes; the other modules still run.

use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::{Diagnostic, SourceMap};
use cheby_iface::{DefId, Ice, ModuleId, PackageId};
use cheby_jit::Jit;
use cheby_mir::EntryKind;
use cheby_package::{Manifest, ModulePath};
use cheby_runtime::entry::{
    DEFAULT_ROOT_STACK_SIZE, DEFAULT_STACK_SIZE, EntryConfig, EntryMode, EntryOutcome,
};
use cheby_runtime::{DetachedGroup, Runtime, RuntimeConfig, RuntimeError};
use cheby_types::thir;

use crate::backend::{self, Roots, TestRoot};
use crate::frontend::{self, Frontend};
use crate::{
    CheckOutput, Error, Module, Package, front_end, parse_file, parse_module_path, std_lib,
};

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
    /// (D-306, D-709): the run stopped at once, printed nothing more, and
    /// `cheby test` exits with 1 (D-783).
    pub terminated: bool,
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
    let mut out = CheckOutput {
        deny_warnings: options.deny_warnings,
        ..CheckOutput::default()
    };
    let front = front_end(root, frontend::Mode::Build, &mut out)?;
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
                return Err(Error::new(format!(
                    "there is no module `{path}` in this package"
                )));
            }
            Some(path)
        }
        None => None,
    };
    let mut roots = Vec::new();
    let mut tests = Vec::new();
    let mut filtered_out: usize = 0;
    for (path, module) in local_modules(&front) {
        let Some(checked) = front.checked(module) else {
            continue;
        };
        let shown = path.to_string();
        for test in checked.thir.tests.iter().filter(|t| on_native(&front, t)) {
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
    let jit = if roots.is_empty() {
        None
    } else {
        match backend::build(&front, &out.sources, Roots::Tests(roots), false)? {
            Ok(built) => Some(built.jit),
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
    })))
}

impl TestProgram {
    /// The warnings of the build, to print before the tests run.
    #[must_use]
    pub const fn output(&self) -> &CheckOutput {
        &self.output
    }

    /// Runs the tests one after another and writes the report of D-541 to
    /// `out` as they finish. `out` should be the process's standard output,
    /// which the tests' own output also goes to.
    ///
    /// # Errors
    ///
    /// An [`Error`] if the runtime cannot run an entry or `out` cannot be
    /// written.
    pub fn run(self, out: &mut dyn Write) -> Result<TestReport, Error> {
        let write_err =
            |e: std::io::Error| Error::new(format!("cannot write the test report: {e}"));
        // `env::program_name()` is the package name, as under `cheby run`
        // (D-369, D-578).
        let runtime = test_runtime(&self.package, self.stacks)?;
        let mut report = TestReport {
            modules: Vec::new(),
            filtered_out: self.filtered_out,
            terminated: false,
        };
        let mut printed = false;
        let mut stuck = StuckGroups::default();
        if let Some(jit) = &self.jit {
            for test in &self.tests {
                let outcome =
                    match run_one(jit, runtime.as_ref(), test.def, self.stacks, &mut stuck)? {
                        Ran::Done(outcome) => outcome,
                        Ran::Exited(_) => {
                            // D-783: the run stops at once and prints nothing
                            // more; `cheby test` exits with 1. The halt is
                            // completed while `jit` is still alive.
                            complete_halt(runtime);
                            report.terminated = true;
                            return Ok(report);
                        }
                    };
                out.write_all(
                    format!("{}\n", test_line(&test.module, &test.name, &outcome)).as_bytes(),
                )
                .map_err(write_err)?;
                printed = true;
                push_result(&mut report, &test.module, &test.name, outcome);
            }
        }
        if let Some(runtime) = runtime {
            runtime.shutdown();
        }
        for module in &report.modules {
            for test in &module.tests {
                if let TestOutcome::Failed(text) = &test.outcome {
                    out.write_all(
                        format!("\n{}", failure_block(&module.path, &test.name, text)).as_bytes(),
                    )
                    .map_err(write_err)?;
                }
            }
        }
        let summary = report.summary();
        let summary = if printed {
            format!("\n{summary}")
        } else {
            summary
        };
        out.write_all(summary.as_bytes()).map_err(write_err)?;
        out.flush().map_err(write_err)?;
        Ok(report)
    }
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
/// program name (D-369, D-578) and `stacks.fiber` as the default stack of
/// the fibers the tests spawn (D-660), or `None` where fibers are not
/// supported (D-714): the tests then run on the thread backend, with the
/// program name set for isolated entries.
///
/// # Errors
///
/// An [`Error`] with D-658's message for an invalid `CHEBY_THREADS`, or if
/// the runtime's threads cannot start.
fn test_runtime(package: &str, stacks: Stacks) -> Result<Option<Runtime>, Error> {
    let config = RuntimeConfig {
        program_name: package.to_owned(),
        fiber_stack: stacks.fiber,
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

/// How running one test ended for the run.
#[derive(Debug)]
enum Ran {
    /// The test ended with this outcome, and its detached fibers have
    /// ended too.
    Done(TestOutcome),
    /// A write that has no `Result` failed while the test or one of its
    /// fibers ran, and the program ended with this status (D-306, D-709):
    /// the runtime's halt has started. The run stops (D-783).
    Exited(i32),
}

/// D-306's status, which a termination that a teardown met carries.
const WRITE_FAILURE_STATUS: i32 = 1;

/// The groups of earlier tests on one runtime whose teardown skipped a
/// fiber blocked inline in a system call (D-786). Such a fiber goes on
/// when its call returns: it is cancelled, unwinds and releases what it
/// holds, maybe while a later test runs, which would hide a leak of that
/// test (review finding ROB2-2 of gate 2). So a test that starts while one
/// of these groups is not empty gets no leak check either; it is skipped
/// and counted (D-805).
#[derive(Debug, Default)]
struct StuckGroups(Vec<Arc<DetachedGroup>>);

impl StuckGroups {
    /// Forgets the groups that are empty now, and returns whether one is
    /// left.
    fn any_live(&mut self) -> bool {
        self.0.retain(|group| group.live() > 0);
        !self.0.is_empty()
    }
}

/// Runs one test entry of `jit` as a root fiber of `runtime` in a group
/// of its own, or on the thread backend without one, and reports how it
/// ended. With a runtime, the test's detached fibers are torn down when it
/// has ended, in every build, before its objects are counted and before
/// the next test starts (§13.6, D-723, D-782); its result is not touched
/// when a termination halts the runtime meanwhile. `stuck` holds the
/// groups of the earlier tests on `runtime` that a teardown left with a
/// fiber blocked inline (D-786); the test's own joins them when it does.
fn run_one(
    jit: &Jit,
    runtime: Option<&Runtime>,
    def: DefId,
    stacks: Stacks,
    stuck: &mut StuckGroups,
) -> Result<Ran, Error> {
    let domain = jit.domain();
    // Before the count: a fiber skipped earlier may release objects from
    // now on (D-786).
    let unsure = stuck.any_live();
    let before = domain.live_objects();
    let kinds_before = domain.live_kinds();
    // A test runs on a stack as large as the root fiber that runs `main`
    // (D-125, D-596), the manifest's `main-stack` (D-660), so a test
    // behaves like the same code run from `main`.
    let config = EntryConfig {
        stack_size: stacks.main,
        ..EntryConfig::new(EntryMode::Test)
    };
    let entry = jit.entry(EntryKind::Test(def))?;
    let mut skip_check = unsure;
    let outcome = match runtime {
        Some(runtime) => {
            let group = DetachedGroup::new();
            let outcome = entry.run_root_in(runtime, &config, &group)?;
            if let EntryOutcome::Exited(status) = outcome {
                return Ok(Ran::Exited(status));
            }
            // Until package T4, a detached fiber's panic report goes
            // straight to standard error while the group is torn down.
            let teardown = runtime.teardown(&group);
            if teardown.halted {
                // A detached fiber's write failed meanwhile: the halt has
                // started, so nothing of the runtime is touched any more,
                // the test's result included.
                return Ok(Ran::Exited(WRITE_FAILURE_STATUS));
            }
            if teardown.skipped > 0 {
                skip_check = true;
                stuck.0.push(group);
            }
            outcome
        }
        None => entry.run(&config)?,
    };
    // A test thunk returns the static `Nil` when the test passed, and the
    // text of the `Err` it returned as an owned `String` otherwise
    // (D-529, D-595). Whether it panicked comes from the entry's catch
    // point, which reads the fiber's unwind record (D-699), never from the
    // word: under the flag backend an unwound thunk returns 0.
    Ok(Ran::Done(match outcome {
        // The captured report goes into the failure block (D-701). As in
        // M1, a failing test gets no leak check.
        EntryOutcome::Panicked(report) => TestOutcome::Failed(report.to_string()),
        // A detached fiber blocked inline in input or output was left
        // running, with what it holds, by this test or an earlier one
        // (`StuckGroups`): the leak check is skipped and counted (D-786).
        EntryOutcome::Returned(word) if word == cheby_runtime::values::nil() && skip_check => {
            domain.stats().skip_leak_check();
            TestOutcome::Passed
        }
        EntryOutcome::Returned(word) if word == cheby_runtime::values::nil() => {
            // D-541: a passing test must leave the build's accounting
            // domain as it found it, or fail with D-458's message and, when
            // runtime kinds are among the leaked objects, D-711's line.
            let leaked = domain.live_objects().saturating_sub(before);
            match cheby_runtime::entry::leak_report(leaked.max(0)) {
                Some(message) => {
                    let after = domain.live_kinds();
                    let mut kinds = after;
                    for (kind, before) in kinds.iter_mut().zip(kinds_before) {
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
            // owned, live `String` object of `jit`, which is alive here.
            let text = unsafe { cheby_runtime::string::to_rust_string(word) }.ok_or_else(|| {
                Error::from(Ice::new(
                    "a test entry returned neither `Nil` nor a `String`",
                ))
            })?;
            cheby_runtime::alloc::with_domain(domain, || {
                // SAFETY: `word` is the owned reference the thunk returned;
                // it is released once, in the domain it was allocated in.
                unsafe { cheby_runtime::rc::drop(word) };
            });
            TestOutcome::Failed(format!("{text}\n"))
        }
        // A write failed (D-306, D-709) on the thread backend: its entry
        // thread is parked for good, so the JIT's code it may run is kept
        // (leaked) for the rest of the process. With a runtime, `Exited`
        // returned above.
        EntryOutcome::Exited(status) => {
            std::mem::forget(jit.code_token());
            return Ok(Ran::Exited(status));
        }
    }))
}

/// Completes the halt that a termination started (D-709): every thread of
/// `runtime` is joined and its fibers stay frozen. The caller keeps the
/// `Jit` alive until this returns and drops no value of the runtime
/// (D-073, D-708).
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
    let runtime = test_runtime(STD_TEST_PACKAGE, Stacks::DEFAULT)?;
    let mut stuck = StuckGroups::default();
    let batch = match build_codes(front, sources, Roots::Tests(roots)) {
        Ok(jit) => jit,
        Err(codes) => {
            let mut tests = Vec::with_capacity(selected.len());
            for &test in selected {
                let alone = vec![TestRoot { module, test }];
                let outcome = match build_codes(front, sources, Roots::Tests(alone)) {
                    // `run_one` tears the test's detached fibers down
                    // before it returns, so none of them can run this
                    // test's code once `jit` is dropped here (D-708,
                    // D-782); the runtime's token keeps the code until
                    // the runtime stops anyway.
                    Ok(jit) => match run_one(
                        &jit,
                        runtime.as_ref(),
                        test.def,
                        Stacks::DEFAULT,
                        &mut stuck,
                    )? {
                        Ran::Done(outcome) => outcome,
                        Ran::Exited(status) => {
                            complete_halt(runtime);
                            drop(jit);
                            return Err(terminated(&path, &test.name, status));
                        }
                    },
                    Err(codes) => TestOutcome::NotBuilt(codes),
                };
                tests.push(TestResult {
                    name: test.name.to_string(),
                    outcome,
                });
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
    let mut tests = Vec::with_capacity(selected.len());
    for &test in selected {
        let outcome = match run_one(
            &batch,
            runtime.as_ref(),
            test.def,
            Stacks::DEFAULT,
            &mut stuck,
        )? {
            Ran::Done(outcome) => outcome,
            Ran::Exited(status) => {
                complete_halt(runtime);
                drop(batch);
                return Err(terminated(&path, &test.name, status));
            }
        };
        tests.push(TestResult {
            name: test.name.to_string(),
            outcome,
        });
    }
    Ok(ModuleTests {
        path,
        not_built: None,
        tests,
    })
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
fn std_test_package(
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
    //! its system call returns, so the tests after it get no leak check
    //! while it lives (`StuckGroups`).

    use std::io::Write as _;
    use std::time::{Duration, Instant};

    use super::{PreparedTests, TestOptions, TestOutcome, prepare_test};

    /// Test "a" leaves a detached fiber, holding 21 objects, blocked
    /// opening a FIFO. Test "b" writes a marker file, on which the host
    /// writes to the FIFO, so that fiber unwinds and releases what it
    /// holds while "b" waits; "b" leaks a receiver. Test "c" leaks a
    /// receiver too, once the fiber is gone. Before the fix, "b"'s check
    /// counted the release against its own leak and passed; now "b"'s
    /// check is skipped and counted, as "a"'s is, and "c"'s runs and
    /// finds the leak (review finding ROB2-2 of gate 2).
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn lifecycle_a_later_test_gets_no_leak_check_while_a_skipped_fiber_lives() {
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
        let PreparedTests::Ready(program) = prepare_test(&dir, &TestOptions::default()).unwrap()
        else {
            panic!("the tests did not build");
        };
        let domain = program.jit.as_ref().unwrap().domain();
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
        let report = program.run(&mut out).unwrap();
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
                ("b leaks a receiver", true),
                ("c leaks a receiver too", !leak_check),
            ],
            "{}",
            String::from_utf8_lossy(&out)
        );
        // "a"'s and "b"'s checks are skipped and counted (D-786).
        assert_eq!(domain.stats().leak_checks_skipped(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
