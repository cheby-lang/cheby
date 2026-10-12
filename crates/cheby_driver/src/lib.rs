//! Build driver: runs the compiler's phases over a package (§13.12,
//! architecture.md §4).
//!
//! - [`load_package`] reads `cheby.toml` (§13.3), discovers the modules
//!   (§7.2), and lexes, parses and validates every module (phase A).
//! - [`check`] runs every implemented front-end phase for `cheby check`
//!   (D-282): phase A, then, if it reported no error, the import graph,
//!   name resolution and type checking over the package and the std
//!   modules it reaches (G1, G2). [`check_syntax`] stops after phase A.
//!   Without front-end errors it then evaluates every constant of the
//!   package (D-459, D-534). [`check_with`] takes `--target` (D-550) and
//!   `--deny-warnings` (D-549).
//! - [`configure_jobs`] sizes the worker pool of the build graph and code
//!   generation for `-j`/`--jobs` (D-551).
//! - [`prepare_run`] and [`Program::run`] are `cheby run` (G3): the front
//!   end, then MIR of the items reachable from the program's `main`, the
//!   JIT, the constants `main` reaches, and `main` (D-137). Unresolved
//!   native symbols (E0053) and constant-evaluation errors are reported
//!   like compile errors before `main` runs; lowering reports nothing
//!   (D-554).
//! - [`prepare_test`] and [`TestProgram::run`] are `cheby test` (G4,
//!   D-541), and [`test_std`] tests the standard library itself (D-542).
//! - [`dump_mir`] and [`dump_clif`] print the MIR and CLIF that running
//!   the root module's `main` compiles, for goldens and the determinism
//!   checks.
//! - [`bench_build`] times checking and code generation for
//!   `cargo xtask bench-compile` (I1).
//!
//! The build graph runs on rayon's global pool (D-263), which
//! [`configure_jobs`] builds; the build cache (D-255) plugs in later
//! behind the same API. Output is deterministic (D-264): results are
//! merged in a fixed order whatever the number of threads, and
//! diagnostics are sorted by [`SourceMap::sort`].
//!
//! With `--deny-warnings` (D-549) a [`CheckOutput`] counts its warnings
//! as errors: [`CheckOutput::has_errors`] is true when there is one, so
//! nothing after the front end runs, and [`CheckOutput::render_with`]
//! renders each as `error[W…]`, the first with a note naming the flag.

use std::fmt;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cheby_diag::{
    Colors, Diagnostic, FileId, Severity, SourceMap, Span, TextRange, TextSize, codes, render_with,
};
use cheby_iface::Ice;
use cheby_jit::{Jit, JitError};
use cheby_package::{MANIFEST_FILE, Manifest, ModulePath, PackageError, Targets};
use cheby_runtime::RunConfig;
use cheby_syntax::SyntaxTree;

mod backend;
mod frontend;
pub mod session;
pub mod std_lib;
mod test;

pub use cheby_package::Target;
pub use test::{
    ModuleTests, PreparedTests, TestCounts, TestOptions, TestOutcome, TestProgram, TestReport,
    TestResult, prepare_test, test_std,
};

/// A failure that is not a diagnostic about the program, such as an I/O
/// error. It is printed as a plain `error:` line without a code
/// (D-442).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    message: String,
}

impl Error {
    #[must_use]
    pub const fn new(message: String) -> Self {
        Self { message }
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<PackageError> for Error {
    fn from(e: PackageError) -> Self {
        Self::new(e.message().to_owned())
    }
}

/// An internal compiler error is a tool error (D-464).
impl From<Ice> for Error {
    fn from(ice: Ice) -> Self {
        Self::new(format!("internal compiler error: {}", ice.message()))
    }
}

impl From<JitError> for Error {
    fn from(e: JitError) -> Self {
        match e {
            JitError::Codegen(cheby_codegen_clif::CodegenError::Ice(ice)) => ice.into(),
            JitError::Codegen(e) => Self::new(format!("internal compiler error: {e}")),
            other => Self::new(other.to_string()),
        }
    }
}

/// A package whose manifest is valid, with its parsed modules.
#[derive(Debug)]
pub struct Package {
    /// The package root, the directory of `cheby.toml`.
    pub root: PathBuf,
    pub manifest: Manifest,
    /// The modules, sorted by file path.
    pub modules: Vec<Module>,
}

/// One module of the package.
#[derive(Debug)]
pub struct Module {
    pub path: ModulePath,
    pub source: ParsedFile,
}

/// A source file after parsing.
#[derive(Debug)]
pub struct ParsedFile {
    /// The file in the source map; its path there is the display path,
    /// such as `src/web/router.cheby` (D-421).
    pub file: FileId,
    /// The syntax tree, or `None` if the file is not UTF-8 (E0001) and
    /// was not parsed.
    pub syntax: Option<SyntaxTree>,
    /// Whether the file has grammar errors (class G) or is not UTF-8.
    pub has_syntax_errors: bool,
}

/// The result of [`check`]: the files read and the diagnostics, sorted.
#[derive(Debug, Default)]
pub struct CheckOutput {
    pub sources: SourceMap,
    pub diagnostics: Vec<Diagnostic>,
    /// `--deny-warnings` (D-549): every warning counts and is rendered as
    /// an error.
    pub deny_warnings: bool,
}

/// The note on the first warning that `--deny-warnings` turns into an
/// error (D-549).
pub const DENIED_NOTE: &str = "warnings are denied by `--deny-warnings`";

impl CheckOutput {
    /// Whether any diagnostic is an error. Warnings alone do not fail a
    /// check (D-080), unless they are denied (D-549).
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.is_error() || self.deny_warnings)
    }

    /// The diagnostics in the layout of D-418, each followed by an empty
    /// line, with no summary (D-443).
    #[must_use]
    pub fn render(&self) -> String {
        self.render_with(Colors::Off)
    }

    /// Like [`CheckOutput::render`], with color escapes if `colors` is
    /// [`Colors::On`]. With `--deny-warnings` each warning is rendered as
    /// an error with its own code, `error[W0002]: …`, and the first one
    /// has the note [`DENIED_NOTE`] (D-549).
    #[must_use]
    pub fn render_with(&self, colors: Colors) -> String {
        render_diagnostics(&self.diagnostics, &self.sources, colors, self.deny_warnings)
    }
}

/// Renders `diagnostics` in the layout of D-418, each followed by an
/// empty line. With `deny_warnings`, each warning is rendered as an error
/// with its own code and the first one gets the note [`DENIED_NOTE`]
/// (D-549). The REPL renders its entries' diagnostics with it too.
#[must_use]
pub fn render_diagnostics(
    diagnostics: &[Diagnostic],
    sources: &SourceMap,
    colors: Colors,
    deny_warnings: bool,
) -> String {
    let mut out = String::new();
    let mut noted = false;
    for diagnostic in diagnostics {
        if deny_warnings && diagnostic.severity() == Severity::Warning {
            let mut denied = diagnostic.clone();
            if !noted {
                denied.notes.push(DENIED_NOTE.to_owned());
                noted = true;
            }
            out.push_str(&as_error(&render_with(&denied, sources, colors)));
        } else {
            out.push_str(&render_with(diagnostic, sources, colors));
        }
        out.push('\n');
    }
    out
}

/// A rendered warning as an error with the same code (D-549): its header
/// `warning[W…]` becomes `error[W…]`, and with colors the warning's style
/// becomes the error's (the header and the primary labels). Rendered text
/// never holds an escape of its own (control characters are shown as
/// pictures), so the style sequences are the renderer's.
fn as_error(rendered: &str) -> String {
    /// The renderer's SGR sequences for the two severities.
    const WARNING_STYLE: &str = "\x1b[1;93m";
    const ERROR_STYLE: &str = "\x1b[1;91m";
    let (style, rest) = match rendered.strip_prefix(WARNING_STYLE) {
        Some(rest) => (ERROR_STYLE, rest),
        None => ("", rendered),
    };
    let body = rest.strip_prefix(Severity::Warning.as_str()).map_or_else(
        || rest.to_owned(),
        |after| format!("{}{after}", Severity::Error.as_str()),
    );
    format!("{style}{body}").replace(WARNING_STYLE, ERROR_STYLE)
}

/// What `cheby check` checks (D-282).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CheckOptions {
    /// `--target`: check the code as it runs on this target only, so the
    /// cross-target check reports E0067 only for it (D-550). `None` checks
    /// every target of the manifest.
    pub target: Option<Target>,
    /// `--deny-warnings`: warnings are errors (D-549).
    pub deny_warnings: bool,
}

/// The stack of each worker thread of the build: as large as the command
/// thread's (D-601), since a worker runs the same recursive passes. Only
/// address space is reserved up front.
const WORKER_STACK_SIZE: usize = 256 << 20;

/// Builds the worker pool that the build graph and code generation run on
/// (rayon's global pool, D-263), with `jobs` threads, or the available
/// parallelism when `None` (`-j`/`--jobs`, D-551). Called once per
/// process, before any build; output never depends on the number of
/// threads (D-264). A second call with the same number of threads is a
/// no-op.
///
/// # Errors
///
/// A tool error when the pool cannot be built, or when it already exists
/// with another number of threads.
pub fn configure_jobs(jobs: Option<NonZeroUsize>) -> Result<(), Error> {
    let threads = jobs.map_or_else(available_parallelism, NonZeroUsize::get);
    let built = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .stack_size(WORKER_STACK_SIZE)
        .thread_name(|index| format!("cheby-worker-{index}"))
        .build_global();
    match built {
        Ok(()) => Ok(()),
        Err(_) if rayon::current_num_threads() == threads => Ok(()),
        Err(e) => Err(Error::new(format!(
            "cannot start {threads} worker threads: {e} (the pool has {})",
            rayon::current_num_threads()
        ))),
    }
}

/// The number of threads the system can run in parallel, 1 if unknown.
fn available_parallelism() -> usize {
    std::thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

/// The package root for a command run in `start`: `start` itself or the
/// nearest ancestor that holds a `cheby.toml`.
#[must_use]
pub fn find_package_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(MANIFEST_FILE).is_file())
        .map(Path::to_path_buf)
}

/// Checks the package at `root` without producing artifacts (D-282): every
/// implemented front-end phase. Any phase-A error (manifest, discovery,
/// lexing, parsing, validation) stops the later phases, so they never see
/// a broken tree (D-452); warnings do not.
///
/// # Errors
///
/// Returns an [`Error`] for failures that are not diagnostics, such as an
/// unreadable file or an internal compiler error.
pub fn check(root: &Path) -> Result<CheckOutput, Error> {
    check_with(root, &CheckOptions::default())
}

/// [`check`] with the options of `cheby check`: `--target` narrows the
/// package's targets to one (D-550), and `--deny-warnings` makes warnings
/// errors, so constants are not evaluated after one (D-549).
///
/// # Errors
///
/// As [`check`], and a tool error "the package does not target `js`"
/// when the manifest's `targets` lack the selected target (D-550).
pub fn check_with(root: &Path, options: &CheckOptions) -> Result<CheckOutput, Error> {
    let mut out = CheckOutput {
        deny_warnings: options.deny_warnings,
        ..CheckOutput::default()
    };
    let front = front_end_for(root, frontend::Mode::Check, options.target, &mut out)?;
    out.sources.sort(&mut out.diagnostics);
    // Every constant of the package is evaluated (D-459, D-534), but only
    // when the front end reported no error, so constant evaluation never
    // sees an ill-typed program. Unresolved native symbols are not
    // reported (D-532); a constant that calls one is E0077.
    if let Some((front, _)) = front.filter(|_| !out.has_errors())
        && let Err(diagnostics) =
            backend::build(&front, &out.sources, backend::Roots::Constants, false)?
    {
        report_back_end(&mut out, diagnostics);
    }
    Ok(out)
}

/// Phase A only: loads, parses and validates the package. Kept for
/// syntax-level regression tests.
///
/// # Errors
///
/// Returns an [`Error`] for failures that are not diagnostics.
pub fn check_syntax(root: &Path) -> Result<CheckOutput, Error> {
    let mut out = CheckOutput::default();
    load_package(root, &mut out.sources, &mut out.diagnostics)?;
    out.sources.sort(&mut out.diagnostics);
    Ok(out)
}

/// Phase A, then, without errors, the front end (D-452). Returns the
/// checked package if the front end ran, with the package name.
fn front_end(
    root: &Path,
    mode: frontend::Mode,
    out: &mut CheckOutput,
) -> Result<Option<(frontend::Frontend, String)>, Error> {
    front_end_for(root, mode, None, out)
}

/// [`front_end`] for one target of the package, or for all of them
/// (D-550): the selected target replaces the manifest's `targets`, so the
/// cross-target check and the constants' target sets see only it.
fn front_end_for(
    root: &Path,
    mode: frontend::Mode,
    target: Option<Target>,
    out: &mut CheckOutput,
) -> Result<Option<(frontend::Frontend, String)>, Error> {
    let package = load_package(root, &mut out.sources, &mut out.diagnostics)?;
    let Some(mut package) = package else {
        return Ok(None);
    };
    if let Some(target) = target {
        if !package.manifest.targets.contains(target) {
            return Err(Error::new(format!(
                "the package does not target `{target}`"
            )));
        }
        package.manifest.targets = Targets::only(target);
    }
    // Only phase-A errors stop the front end (D-452); a phase-A warning
    // denied by `--deny-warnings` does not, so the later phases still
    // report theirs, and the command fails after them (D-621).
    if out.diagnostics.iter().any(Diagnostic::is_error) {
        return Ok(None);
    }
    let name = package.manifest.name.clone();
    let front = frontend::run(package, mode, &mut out.sources, &mut out.diagnostics)?;
    Ok(Some((front, name)))
}

/// What `cheby run` runs (D-137, D-286).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunOptions {
    /// The module whose `main` runs, such as `my_app::tools::migrate`; the
    /// root module if `None`.
    pub module_path: Option<String>,
    /// The program's arguments (D-347).
    pub args: Vec<String>,
    /// `--deny-warnings`: a warning stops the run before anything is
    /// compiled (D-549).
    pub deny_warnings: bool,
}

/// The outcome of [`prepare_run`].
#[derive(Debug)]
pub enum Prepared {
    /// The package has errors; nothing was compiled.
    CompileFailed(CheckOutput),
    /// The program is compiled and ready to run.
    Ready(Box<Program>),
}

/// A compiled program: the JIT that holds its code, and its entry.
pub struct Program {
    output: CheckOutput,
    /// Holds the code; dropping it frees the code.
    jit: Jit,
    /// `main`'s entry, compiled into `jit`.
    entry: cheby_mir::EntryKind,
    config: RunConfig,
}

impl fmt::Debug for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Program")
            .field("output", &self.output)
            .field("jit", &self.jit)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Program {
    /// The warnings of the build, to print before the program runs.
    #[must_use]
    pub const fn output(&self) -> &CheckOutput {
        &self.output
    }

    /// Runs `main` and returns the exit status (§10.10, D-182): 0, 1 when
    /// `main` returns `Err` or output fails, 101 after a panic, and in
    /// debug builds 102 when objects leak (D-458).
    ///
    /// # Errors
    ///
    /// An [`Error`] if the runtime cannot run the entry.
    pub fn run(self) -> Result<i32, Error> {
        Ok(self.jit.entry(self.entry)?.run_main(self.config)?)
    }
}

/// The outcome of [`run`].
#[derive(Debug)]
pub enum RunResult {
    CompileFailed(CheckOutput),
    Ran { output: CheckOutput, exit: i32 },
}

/// Checks and compiles the package at `root` for `cheby run`.
///
/// # Errors
///
/// A tool error (D-464) if the module path is malformed, names no module
/// of the package, or names a module without `main`, and an [`Error`] for
/// internal failures.
pub fn prepare_run(root: &Path, options: RunOptions) -> Result<Prepared, Error> {
    let mut out = CheckOutput {
        deny_warnings: options.deny_warnings,
        ..CheckOutput::default()
    };
    let front = front_end(root, frontend::Mode::Build, &mut out)?;
    out.sources.sort(&mut out.diagnostics);
    let Some((front, package)) = front.filter(|_| !out.has_errors()) else {
        return Ok(Prepared::CompileFailed(out));
    };
    let path = match &options.module_path {
        Some(text) => parse_module_path(text)
            .ok_or_else(|| Error::new(format!("`{text}` is not a module path")))?,
        None => ModulePath::root(&package),
    };
    let module = front
        .local_module(&path)
        .ok_or_else(|| Error::new(format!("there is no module `{path}` in this package")))?;
    let main = front
        .main_of(module)
        .ok_or_else(|| Error::new(format!("module `{path}` has no `main` function")))?;
    // E0053 for a native symbol nothing defines (D-532) and the
    // constant-evaluation errors of the constants `main` reaches (D-535)
    // stop the run.
    let jit = match backend::build(&front, &out.sources, backend::Roots::Main(main), false)? {
        Ok(built) => built.jit,
        Err(diagnostics) => {
            report_back_end(&mut out, diagnostics);
            return Ok(Prepared::CompileFailed(out));
        }
    };
    let entry = cheby_mir::EntryKind::Main(main.def);
    // Fails now, before anything runs, if the thunk was not compiled.
    jit.entry(entry)?;
    // `env::program_name()` is the package's name under `cheby run`, so
    // usage messages are the same on every machine and for every module
    // path that is run (D-369).
    let config = RunConfig {
        args: options.args,
        program_name: package,
        ..RunConfig::default()
    };
    Ok(Prepared::Ready(Box::new(Program {
        output: out,
        jit,
        entry,
        config,
    })))
}

/// Checks, compiles and runs the package at `root` (G3). Warnings are in
/// the result; [`prepare_run`] gives them before the program runs.
///
/// # Errors
///
/// As [`prepare_run`] and [`Program::run`].
pub fn run(root: &Path, options: RunOptions) -> Result<RunResult, Error> {
    match prepare_run(root, options)? {
        Prepared::CompileFailed(output) => Ok(RunResult::CompileFailed(output)),
        Prepared::Ready(mut program) => {
            let output = std::mem::take(&mut program.output);
            let exit = program.run()?;
            Ok(RunResult::Ran { output, exit })
        }
    }
}

/// The times of [`bench_build`], and the size of what it built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BenchTimings {
    /// Loading, parsing and checking the package and the std modules it
    /// reaches.
    pub front_end: Duration,
    /// Lowering every function of the package to MIR and compiling it with
    /// the JIT.
    pub back_end: Duration,
    /// The bytes of machine code the JIT defined (D-718).
    pub code_bytes: u64,
    /// The reference-count operations, `dup` and `drop` statements, of
    /// every MIR unit compiled (D-718).
    pub rc_ops: u64,
}

/// Checks the package at `dir` and generates code for every function of
/// it, running nothing (D-247 "checks and generates code"; I1, plan
/// §8.5). A hidden entry for `cargo xtask bench-compile`.
///
/// # Errors
///
/// An [`Error`] when the package has errors, the back end cannot build
/// it (with the codes), or for internal failures.
pub fn bench_build(dir: &Path) -> Result<BenchTimings, Error> {
    let start = Instant::now();
    let mut out = CheckOutput::default();
    let front = front_end(dir, frontend::Mode::Build, &mut out)?;
    let front_end_time = start.elapsed();
    out.sources.sort(&mut out.diagnostics);
    let Some((front, _)) = front.filter(|_| !out.has_errors()) else {
        return Err(Error::new(format!(
            "the benchmark project has errors:\n{}",
            out.render()
        )));
    };
    let start = Instant::now();
    let built = backend::build(&front, &out.sources, backend::Roots::AllFunctions, false)?;
    let back_end = start.elapsed();
    let built = match built {
        Ok(built) => built,
        Err(diagnostics) => {
            out.diagnostics = diagnostics;
            return Err(Error::new(format!(
                "the back end cannot build the benchmark project:\n{}",
                out.render()
            )));
        }
    };
    Ok(BenchTimings {
        front_end: front_end_time,
        back_end,
        code_bytes: built.jit.code_stats().code_bytes,
        rc_ops: built.rc_ops.unwrap_or(0),
    })
}

/// `a::b::c` as a module path, if every segment is non-empty.
fn parse_module_path(text: &str) -> Option<ModulePath> {
    let mut segments = text.split("::");
    let first = segments.next().filter(|s| !s.is_empty())?;
    let mut path = ModulePath::root(first);
    for segment in segments {
        if segment.is_empty() {
            return None;
        }
        path = path.child(segment);
    }
    Some(path)
}

/// The MIR that running the root module's `main` compiles, after
/// reference counting (D1): the units of the modules it needs, in build
/// order, with the items reachable from `main` (D-510), then the entry
/// unit. Empty if the package has errors or if the root module has no
/// `main`.
///
/// # Errors
///
/// An [`Error`] for internal failures.
pub fn dump_mir(root: &Path) -> Result<(CheckOutput, String), Error> {
    let mut out = CheckOutput::default();
    let units = units_for_dump(root, &mut out)?;
    let mut text = Vec::with_capacity(units.len());
    for unit in &units {
        text.push(cheby_mir::dump(unit)?);
    }
    Ok((out, text.join("\n")))
}

/// The CLIF of every function [`dump_mir`] shows, before Cranelift's
/// compilation, each preceded by its symbol and the symbols it references.
/// Empty whenever [`dump_mir`] is.
///
/// # Errors
///
/// An [`Error`] for internal failures.
pub fn dump_clif(root: &Path) -> Result<(CheckOutput, String), Error> {
    let mut out = CheckOutput::default();
    let front = front_end(root, frontend::Mode::Build, &mut out)?;
    out.sources.sort(&mut out.diagnostics);
    let Some((front, package)) = front.filter(|_| !out.has_errors()) else {
        return Ok((out, String::new()));
    };
    let Some(main) = front
        .local_module(&ModulePath::root(&package))
        .and_then(|m| front.main_of(m))
    else {
        return Ok((out, String::new()));
    };
    match backend::build(&front, &out.sources, backend::Roots::Main(main), true)? {
        Ok(mut built) => Ok((out, built.jit.take_clif().join("\n"))),
        Err(diagnostics) => {
            report_back_end(&mut out, diagnostics);
            Ok((out, String::new()))
        }
    }
}

fn units_for_dump(root: &Path, out: &mut CheckOutput) -> Result<Vec<cheby_mir::MirModule>, Error> {
    let front = front_end(root, frontend::Mode::Build, out)?;
    out.sources.sort(&mut out.diagnostics);
    let Some((front, package)) = front.filter(|_| !out.has_errors()) else {
        return Ok(Vec::new());
    };
    let main = front
        .local_module(&ModulePath::root(&package))
        .and_then(|m| front.main_of(m));
    let Some(main) = main else {
        return Ok(Vec::new());
    };
    Ok(backend::lower_program(&front, &out.sources, main)?)
}

/// Adds the back end's diagnostics (E0053, the constant-evaluation
/// errors) after the front end's, which are already sorted (warnings
/// only, since errors stop before the back end).
fn report_back_end(out: &mut CheckOutput, mut diagnostics: Vec<Diagnostic>) {
    out.sources.sort(&mut diagnostics);
    out.diagnostics.extend(diagnostics);
}

/// Reads the manifest, discovers the modules and parses each one. Every
/// file read is added to `sources`; problems with the program are pushed
/// to `diagnostics`, unsorted. Returns `None` if the manifest cannot be
/// used, in which case no module is read.
///
/// # Errors
///
/// Returns an [`Error`] for failures that are not diagnostics.
pub fn load_package(
    root: &Path,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Option<Package>, Error> {
    let Some(manifest) = Manifest::load(root, sources, diagnostics)? else {
        return Ok(None);
    };
    let files = cheby_package::discover(root, &manifest.name, diagnostics)?;
    // Read, parsed and validated in parallel (D-263); file ids and
    // diagnostics follow the order of `files` (D-264).
    let modules = frontend::read_modules(root, files, sources, diagnostics)?;
    Ok(Some(Package {
        root: root.to_path_buf(),
        manifest,
        modules,
    }))
}

/// Adds the file `path` with contents `bytes` to `sources`, then parses
/// it and validates it: fully if it has no grammar error, and otherwise
/// only with the token-level checks. A file that is not
/// UTF-8 is reported (E0001, §2.1) and not parsed; `sources` gets its
/// lossy decoding for the snippet.
///
/// # Errors
///
/// Returns an [`Error`] if the file is too large for the source map.
pub fn parse_file(
    sources: &mut SourceMap,
    path: &str,
    bytes: Vec<u8>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<ParsedFile, Error> {
    let too_large = |_| Error::from(PackageError::too_large(path));
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => {
            let at = e.utf8_error().valid_up_to();
            let lossy = String::from_utf8_lossy(e.as_bytes()).into_owned();
            let file = sources.add_file(path, lossy).map_err(too_large)?;
            // The valid prefix is the same in the lossy text, and the bad
            // bytes became one U+FFFD there.
            let start = TextSize::try_from(at).unwrap_or(TextSize::MAX);
            let range = TextRange::at(
                start,
                TextSize::try_from('\u{fffd}'.len_utf8()).unwrap_or(1),
            );
            diagnostics.push(
                Diagnostic::new(codes::INVALID_UTF8, "source file is not valid UTF-8")
                    .with_primary(Span::new(file, range), "this byte is not valid UTF-8")
                    .with_help("save the file as UTF-8"),
            );
            return Ok(ParsedFile {
                file,
                syntax: None,
                has_syntax_errors: true,
            });
        }
    };
    let text: Arc<str> = Arc::from(text);
    let file = sources
        .add_file(path, Arc::clone(&text))
        .map_err(too_large)?;
    let parse = cheby_syntax::parse(file, text);
    let (tree, errors) = parse.into_parts();
    let has_syntax_errors = errors.iter().any(Diagnostic::is_error);
    diagnostics.extend(errors);
    // Validation assumes the shapes the grammar allows. On a tree with
    // grammar errors it reports noise next to the real error, such as a
    // short tuple for an unclosed `(`, so only its token-level checks run
    // there (D-425). They do not depend on the tree's shape. Words such as
    // `if` and `return` that cause a grammar error are explained by a help
    // on that error; names are checked later, by name resolution.
    if has_syntax_errors {
        diagnostics.extend(cheby_syntax::validate_tokens(&tree));
    } else {
        diagnostics.extend(cheby_syntax::validate(&tree));
    }
    Ok(ParsedFile {
        file,
        syntax: Some(tree),
        has_syntax_errors,
    })
}

#[cfg(test)]
mod tests;
