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
//! The std self-test checks every shipped std module with its tests
//! (`Tests::All`), then builds and runs one batch per module. When a
//! module's batch does not build, each of its tests is built alone, so
//! the ones that build still run. A module or test that does not build is
//! counted and reported with its codes; the other modules still run.

use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};

use cheby_diag::{Diagnostic, SourceMap};
use cheby_iface::{DefId, Ice, ModuleId, PackageId};
use cheby_jit::Jit;
use cheby_mir::EntryKind;
use cheby_package::{Manifest, ModulePath};
use cheby_runtime::entry::{DEFAULT_ROOT_STACK_SIZE, EntryConfig, EntryMode, EntryOutcome};
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
}

impl std::fmt::Debug for TestProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestProgram")
            .field("output", &self.output)
            .field("package", &self.package)
            .field("tests", &self.tests)
            .field("filtered_out", &self.filtered_out)
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
    /// `E0038, E0053`.
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
    /// the codes, such as `E0038, E0053`. Each test is then
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

    /// Whether every selected test passed (also when none was selected).
    #[must_use]
    pub fn success(&self) -> bool {
        let counts = self.counts();
        counts.failed == 0 && counts.not_built == 0
    }

    /// The exit status: 0 when every selected test passed, 1 otherwise
    /// (D-541, D-542).
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
fn on_native(front: &Frontend, test: &thir::TestDef) -> bool {
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
/// (D-541). Compile errors, including E0038 and E0053 from the back end,
/// stop here.
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
                crate::report_unsupported(&mut out, diagnostics);
                return Ok(PreparedTests::CompileFailed(out));
            }
        }
    };
    Ok(PreparedTests::Ready(Box::new(TestProgram {
        output: out,
        package,
        jit,
        tests,
        filtered_out,
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
        cheby_runtime::entry::set_program(Vec::new(), self.package.clone());
        let mut report = TestReport {
            modules: Vec::new(),
            filtered_out: self.filtered_out,
        };
        let mut printed = false;
        if let Some(jit) = &self.jit {
            for test in &self.tests {
                let outcome = run_one(jit, test.def)?;
                out.write_all(
                    format!("{}\n", test_line(&test.module, &test.name, &outcome)).as_bytes(),
                )
                .map_err(write_err)?;
                printed = true;
                push_result(&mut report, &test.module, &test.name, outcome);
            }
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

/// Runs one test entry of `jit` and reports how it ended.
fn run_one(jit: &Jit, def: DefId) -> Result<TestOutcome, Error> {
    let domain = jit.domain();
    let before = domain.live_objects();
    // A test runs on a stack as large as the root fiber that runs `main`
    // (D-125, D-596), so a test behaves like the same code run from
    // `main`.
    let config = EntryConfig {
        stack_size: DEFAULT_ROOT_STACK_SIZE,
        ..EntryConfig::new(EntryMode::Test)
    };
    let outcome = jit.entry(EntryKind::Test(def))?.run(&config)?;
    // A test thunk returns the static `Nil` when the test passed, and the
    // text of the `Err` it returned as an owned `String` otherwise
    // (D-529, D-595).
    Ok(match outcome {
        EntryOutcome::Panicked(report) => TestOutcome::Failed(report.to_string()),
        EntryOutcome::Returned(word) if word == cheby_runtime::values::nil() => {
            // D-541: a passing test must leave the build's accounting
            // domain as it found it, or fail with D-458's message.
            let leaked = domain.live_objects().saturating_sub(before);
            match cheby_runtime::entry::leak_report(leaked.max(0)) {
                Some(message) => TestOutcome::Failed(message),
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
    })
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
    let batch = match build_codes(front, sources, Roots::Tests(roots)) {
        Ok(jit) => jit,
        Err(codes) => {
            let mut tests = Vec::with_capacity(selected.len());
            for &test in selected {
                let alone = vec![TestRoot { module, test }];
                let outcome = match build_codes(front, sources, Roots::Tests(alone)) {
                    Ok(jit) => run_one(&jit, test.def)?,
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
        tests.push(TestResult {
            name: test.name.to_string(),
            outcome: run_one(&batch, test.def)?,
        });
    }
    Ok(ModuleTests {
        path,
        not_built: None,
        tests,
    })
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
/// and joined with `, `, or the internal compiler error's message.
fn build_codes(front: &Frontend, sources: &SourceMap, roots: Roots<'_>) -> Result<Jit, String> {
    match backend::build(front, sources, roots, false) {
        Ok(Ok(built)) => Ok(built.jit),
        Ok(Err(diagnostics)) => Err(join_codes(&diagnostics)),
        Err(ice) => Err(format!("internal compiler error: {}", ice.message())),
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
