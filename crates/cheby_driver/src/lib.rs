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
//! - With `--reuse-report` ([`RunOptions::reuse_report`],
//!   [`TestOptions::reuse_report`]) the build also gives the reuse report
//!   of the package's own code ([`Program::reuse_report`],
//!   [`TestProgram::reuse_report`]; D-717), which the CLI prints after the
//!   warnings and before anything runs.
//! - [`dump_mir`] and [`dump_clif`] print the MIR and CLIF that running
//!   the root module's `main` compiles, for goldens and the determinism
//!   checks.
//! - [`bench_build`] times checking and code generation for
//!   `cargo xtask bench-compile` (I1).
//!
//! The build graph runs on rayon's global pool (D-263), which
//! [`configure_jobs`] builds. [`check_cached`], [`prepare_run_cached`] and
//! [`prepare_test_cached`] are the commands' entry points through the build
//! cache ([`Cache`]): each package module's front record is looked up and
//! written (D-955), and its diagnostics replayed (D-965); the cacheless
//! entry points stay for tests, the REPL and `xtask bench-build` (D-964,
//! D-975). Output is deterministic (D-264) and never depends on the
//! cache (D-268): results are
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
use cheby_runtime::entry::AtExit;
use cheby_syntax::SyntaxTree;

mod aot;
mod backend;
pub mod cache;
mod frontend;
mod reuse_report;
pub mod session;
pub mod std_lib;
mod test;

pub use aot::{BuildOptions, BuildOutcome, build_cached, build_cached_with, build_target};
pub use cheby_package::Target;
pub use test::{
    ModuleTests, PreparedTests, TestCounts, TestOptions, TestOutcome, TestProgram, TestReport,
    TestResult, prepare_test, prepare_test_cached, test_std,
};

/// What a command reads and writes in the build cache (D-956 to D-965):
/// the store, the lookup log of `CHEBY_CACHE_LOG` (D-973), and whether
/// lookups read the store at all. `CHEBY_DUMP_DIR` bypasses every read and
/// still writes (D-965), so a dump shows everything the command compiled.
/// The REPL and `xtask bench-build` use none (D-964, D-975).
#[derive(Debug)]
pub struct Cache {
    pub(crate) store: cache::Store,
    pub(crate) log: cache::CacheLog,
    pub(crate) reads: bool,
    /// The driver's counts, never printed: the package modules each front
    /// end of the command parsed and checked from source, by path, once
    /// per parse and per check (GAP(cache-log-parses)).
    counts: std::sync::Mutex<Counts>,
}

/// The package modules a command's front ends parsed and checked.
#[derive(Debug, Default)]
struct Counts {
    parsed: Vec<String>,
    checked: Vec<String>,
}

impl Cache {
    /// D-959's command path: the cache `CHEBY_CACHE` or the OS default
    /// names, or a fresh temporary cache when that one cannot be created or
    /// written, whose reason comes back for the caller's note.
    ///
    /// # Errors
    ///
    /// [`cache::StoreError::RelativeCacheVar`] and
    /// [`cache::StoreError::UnsafeRoot`], which never fall back, and the
    /// temporary cache's error when it cannot be created either: tool
    /// errors (D-959).
    pub fn open(
        env: &cache::CacheEnv,
        build: cache::BuildId,
        log: cache::CacheLog,
    ) -> Result<(Self, Option<cache::Fallback>), cache::StoreError> {
        let (store, fallback) = cache::Store::open_or_temporary(env, build)?;
        Ok((Self::with_store(store, log), fallback))
    }

    /// A cache over an opened store. Reads are bypassed when
    /// `CHEBY_DUMP_DIR` is set (D-965).
    #[must_use]
    pub fn with_store(store: cache::Store, log: cache::CacheLog) -> Self {
        Self {
            store,
            log,
            reads: std::env::var_os("CHEBY_DUMP_DIR").is_none(),
            counts: std::sync::Mutex::new(Counts::default()),
        }
    }

    /// Adds a front end's parsed and checked package modules to the
    /// command's counts.
    pub(crate) fn count(&self, parsed: Vec<String>, checked: Vec<String>) {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        counts.parsed.extend(parsed);
        counts.checked.extend(checked);
    }

    /// The package modules the command's front ends parsed, by path, once
    /// per parse, sorted: the hook the cache-hit tests read (never
    /// printed).
    #[cfg_attr(not(test), expect(dead_code, reason = "read by tests only"))]
    pub(crate) fn parsed_modules(&self) -> Vec<String> {
        let mut out = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .parsed
            .clone();
        out.sort();
        out
    }

    /// The package modules the command's front ends checked from source,
    /// by path, once per check, sorted: a module listed twice was checked
    /// twice. A crate-visible hook for tests (`build_warm_identical`
    /// among them); never printed.
    #[cfg_attr(not(test), expect(dead_code, reason = "read by tests only"))]
    pub(crate) fn checked_modules(&self) -> Vec<String> {
        let mut out = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .checked
            .clone();
        out.sort();
        out
    }

    /// This cache with every read bypassed, as `CHEBY_DUMP_DIR` makes it:
    /// every lookup misses and every result is still written (D-965).
    #[must_use]
    pub fn without_reads(mut self) -> Self {
        self.reads = false;
        self
    }

    /// The store.
    #[must_use]
    pub const fn store(&self) -> &cache::Store {
        &self.store
    }

    /// The lookup log.
    #[must_use]
    pub const fn log(&self) -> &cache::CacheLog {
        &self.log
    }

    /// Ends the command's use of the cache: writes the lookup log, trims
    /// the store when `trim` (at most once a day, D-961; the commands that
    /// may trim are `cheby check` and `cheby test`), and drops the store,
    /// which deletes a temporary one (D-959). Call it before the process
    /// exits on every path. A trim that fails is skipped: a later command
    /// trims instead, and no output depends on it.
    ///
    /// # Errors
    ///
    /// The log's I/O error. The store is dropped either way.
    pub fn finish(self, trim: bool) -> Result<(), cache::StoreError> {
        self.finish_at(trim, std::time::SystemTime::now())
    }

    /// [`Cache::finish`] with the trim's clock at `now`. Every write the
    /// store queued is in place or skipped before the trim runs, so the
    /// trim never misses an index or an entry still queued (D-961,
    /// GAP(cache-write-behind)); the trim keeps the blobs a recently used
    /// package index names.
    pub(crate) fn finish_at(
        self,
        trim: bool,
        now: std::time::SystemTime,
    ) -> Result<(), cache::StoreError> {
        let logged = self.log.finish();
        self.store.flush();
        if trim {
            let _ = self.store.trim_keeping_indexed(now);
        }
        drop(self.store);
        logged
    }
}

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
    /// The hashes of the file's bytes, computed when it was read.
    pub hashes: SourceHashes,
    /// Whether the file was parsed when the package was loaded: a warm
    /// `cheby check` leaves the modules the package index vouches for
    /// unparsed (D-962), and [`ParsedFile::syntax`] is then `None` with
    /// no syntax error.
    pub parsed: bool,
    /// The phase-A diagnostics of the file, which loading reported.
    pub syntax_diagnostics: Vec<Diagnostic>,
}

/// The two hashes of a source file's bytes, both SipHash-1-3 with zero
/// keys, computed in one pass when the file is read (D-963): the 128-bit
/// one in the cache's front keys (D-956) and D-538's 64-bit one in
/// constant seeds. One value cannot serve both: D-538's hash is 64-bit,
/// and changing a seed's input would change constant maps' orders
/// (GAP(source-hash-width)). Std's are computed when `cheby` is built
/// ([`std_lib::source_hashes`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceHashes {
    /// SipHash-1-3 128, for cache keys.
    pub sip128: cache::Hash128,
    /// SipHash-1-3 64, `cheby_runtime::consteval::source_hash`, for
    /// constant seeds.
    pub sip64: u64,
}

impl SourceHashes {
    /// Both hashes of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            sip128: cache::Hash128::of(bytes),
            sip64: cheby_runtime::consteval::source_hash(bytes),
        }
    }
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
    check_inner(root, *options, None)
}

/// [`check_with`] through the build cache, as `cheby check` runs it: the
/// package's modules are looked up in `cache` and their front records
/// written (D-955, D-965). The output is byte for byte the cacheless one
/// (D-268).
///
/// # Errors
///
/// As [`check_with`].
pub fn check_cached(
    root: &Path,
    options: &CheckOptions,
    cache: &Cache,
) -> Result<CheckOutput, Error> {
    check_inner(root, *options, Some(cache))
}

fn check_inner(
    root: &Path,
    options: CheckOptions,
    cache: Option<&Cache>,
) -> Result<CheckOutput, Error> {
    let mut out = CheckOutput {
        deny_warnings: options.deny_warnings,
        ..CheckOutput::default()
    };
    let front = front_end_for(root, frontend::Mode::Check, options.target, &mut out, cache)?;
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
    front_end_for(root, mode, None, out, None)
}

/// [`front_end`] for one target of the package, or for all of them
/// (D-550): the selected target replaces the manifest's `targets`, so the
/// cross-target check and the constants' target sets see only it. With a
/// cache, the package's modules are looked up and written (D-955); a
/// front end that skipped checking its hits again
/// ([`frontend::Frontend::complete`]) reported an error, and is returned
/// as `None`, so nothing lowers it.
fn front_end_for(
    root: &Path,
    mode: frontend::Mode,
    target: Option<Target>,
    out: &mut CheckOutput,
    cache: Option<&Cache>,
) -> Result<Option<(frontend::Frontend, String)>, Error> {
    let family = match mode {
        frontend::Mode::Check => cache::index::CommandFamily::Check,
        frontend::Mode::Build | frontend::Mode::StdTests => cache::index::CommandFamily::Build,
    };
    front_end_family(root, mode, family, target, out, cache)
}

/// [`front_end_for`] for a command `family` (D-962): with a cache, the
/// package index of the root and the family is read first, and in
/// `cheby check` the modules it vouches for are not parsed. A front end
/// that must start again ([`frontend::Stop::Restart`]) is run once more
/// from the start, every module parsed and every hit decoded at its lookup.
fn front_end_family(
    root: &Path,
    mode: frontend::Mode,
    family: cache::index::CommandFamily,
    target: Option<Target>,
    out: &mut CheckOutput,
    cache: Option<&Cache>,
) -> Result<Option<(frontend::Frontend, String)>, Error> {
    let deny_warnings = out.deny_warnings;
    let index_key = cache.map(|cache| {
        let root = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
        cache::index::PackageIndex::key(&root, family, cache.store.build_id())
    });
    // A bad, stale or missing index is ignored (D-962).
    let index: Option<(cache::index::PackageIndex, Vec<u8>)> = cache
        .filter(|cache| cache.reads)
        .zip(index_key)
        .and_then(|(cache, key)| cache.store.get_hint(&key).ok().flatten())
        .and_then(|bytes| cache::index::PackageIndex::decode(&bytes).map(|index| (index, bytes)));
    for lazy in [true, false] {
        *out = CheckOutput {
            deny_warnings,
            ..CheckOutput::default()
        };
        let use_ = cache
            .zip(index_key)
            .map(|(cache, index_key)| frontend::CacheUse {
                cache,
                index: index.as_ref(),
                index_key,
                lazy,
            });
        match front_end_once(root, mode, target, out, use_)? {
            Attempt::Done(result) => return Ok(result),
            Attempt::Again if lazy => {}
            Attempt::Again => break,
        }
    }
    Err(Ice::new("a front end started again twice").into())
}

/// How one run of a front end ended.
#[expect(
    clippy::large_enum_variant,
    reason = "one value per command, moved out at once"
)]
enum Attempt {
    /// With its result: the front end, or `None` when it did not run to
    /// the end (phase-A errors, a manifest that cannot be used).
    Done(Option<(frontend::Frontend, String)>),
    /// It must start again ([`frontend::Stop::Restart`]).
    Again,
}

/// One run of [`front_end_family`].
fn front_end_once(
    root: &Path,
    mode: frontend::Mode,
    target: Option<Target>,
    out: &mut CheckOutput,
    cache: Option<frontend::CacheUse<'_>>,
) -> Result<Attempt, Error> {
    // Only `cheby check` leaves modules unparsed: every other command
    // lowers every module (GAP(warm-recheck-readers)).
    let vouched = cache
        .filter(|use_| use_.lazy && mode == frontend::Mode::Check)
        .and_then(|use_| use_.index)
        .map(|(index, _)| index);
    let skip = |path: &ModulePath, hash: cache::Hash128| {
        vouched.is_some_and(|index| {
            index
                .get(&path.to_string())
                .is_some_and(|entry| entry.source == hash)
        })
    };
    let package = load_package_skipping(root, &mut out.sources, &mut out.diagnostics, &skip)?;
    let Some(mut package) = package else {
        return Ok(Attempt::Done(None));
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
        // The front end, which would replay the phase-A diagnostics of the
        // modules the index let go unparsed, does not run: start again,
        // parsing every module, so the output is a cold check's (D-268,
        // D-965; GAP(package-index-shape)). Only the error path pays.
        if vouched.is_some() && package.modules.iter().any(|module| !module.parsed) {
            return Ok(Attempt::Again);
        }
        return Ok(Attempt::Done(None));
    }
    let name = package.manifest.name.clone();
    let Some(front) =
        frontend::run_cached(package, mode, &mut out.sources, &mut out.diagnostics, cache)?
    else {
        return Ok(Attempt::Again);
    };
    if !front.complete {
        return Ok(Attempt::Done(None));
    }
    Ok(Attempt::Done(Some((front, name))))
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
    /// `--reuse-report`: the build also gives the reuse report
    /// ([`Program::reuse_report`], D-717).
    pub reuse_report: bool,
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
    /// The text of `--reuse-report`, empty without the flag.
    reuse_report: String,
}

impl fmt::Debug for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Program")
            .field("output", &self.output)
            .field("jit", &self.jit)
            .field("config", &self.config)
            .field("reuse_report", &self.reuse_report)
            .finish_non_exhaustive()
    }
}

impl Program {
    /// The warnings of the build, to print before the program runs.
    #[must_use]
    pub const fn output(&self) -> &CheckOutput {
        &self.output
    }

    /// The reuse report of `--reuse-report` (D-717), to print to standard
    /// error after the warnings and before the program runs: one line per
    /// candidate, each with its newline, sorted; empty without the flag.
    #[must_use]
    pub fn reuse_report(&self) -> &str {
        &self.reuse_report
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

    /// Chooses what happens to the fibers left when `main` has returned
    /// (D-073, D-708): [`AtExit::Teardown`] unless set. Only `cheby run`,
    /// whose process exits next, asks for [`AtExit::Freeze`], and only in
    /// a build without the leak check (D-797).
    pub const fn set_at_exit(&mut self, at_exit: AtExit) {
        self.config.at_exit = at_exit;
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
    prepare_run_inner(root, options, None)
}

/// [`prepare_run`] through the build cache, as `cheby run` runs it (D-955,
/// D-965); what it prints and runs is the cacheless build's (D-268).
///
/// # Errors
///
/// As [`prepare_run`].
pub fn prepare_run_cached(
    root: &Path,
    options: RunOptions,
    cache: &Cache,
) -> Result<Prepared, Error> {
    prepare_run_inner(root, options, Some(cache))
}

fn prepare_run_inner(
    root: &Path,
    options: RunOptions,
    cache: Option<&Cache>,
) -> Result<Prepared, Error> {
    let mut out = CheckOutput {
        deny_warnings: options.deny_warnings,
        ..CheckOutput::default()
    };
    let front = front_end_family(
        root,
        frontend::Mode::Build,
        cache::index::CommandFamily::Run,
        None,
        &mut out,
        cache,
    )?;
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
    let built = backend::build_reporting(
        &front,
        &out.sources,
        backend::Roots::Main(main),
        false,
        options.reuse_report,
    )?;
    let (jit, reuse_report) = match built {
        Ok(built) => (built.jit, built.reuse_report.unwrap_or_default()),
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
    // The root package's `[runtime]` stacks (D-660); the runtime rounds
    // them up to its page size (D-796).
    let config = RunConfig {
        args: options.args,
        program_name: package,
        root_stack_size: stack_bytes(front.main_stack),
        fiber_stack: stack_bytes(front.fiber_stack),
        ..RunConfig::default()
    };
    Ok(Prepared::Ready(Box::new(Program {
        output: out,
        jit,
        entry,
        config,
        reuse_report,
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

/// A `[runtime]` stack size (D-660) as the runtime takes it: bytes, which
/// the manifest kept within 1 MiB to 16 GiB; on a target whose addresses
/// cannot hold it, the largest size, which the runtime clamps.
pub(crate) fn stack_bytes(bytes: u64) -> usize {
    usize::try_from(bytes).unwrap_or(usize::MAX)
}

/// `a::b::c` as a module path, if every segment is non-empty.
pub(crate) fn parse_module_path(text: &str) -> Option<ModulePath> {
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
    load_package_skipping(root, sources, diagnostics, &|_, _| false)
}

/// [`load_package`], leaving unparsed every module whose path and source
/// hash `skip` accepts ([`Module::parsed`]).
fn load_package_skipping(
    root: &Path,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
    skip: &(dyn Fn(&ModulePath, cache::Hash128) -> bool + Sync),
) -> Result<Option<Package>, Error> {
    let Some(manifest) = Manifest::load(root, sources, diagnostics)? else {
        return Ok(None);
    };
    let files = cheby_package::discover(root, &manifest.name, diagnostics)?;
    // Read, parsed and validated in parallel (D-263); file ids and
    // diagnostics follow the order of `files` (D-264).
    let modules = frontend::read_modules(root, files, sources, diagnostics, skip)?;
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
mod alone_tests;
#[cfg(test)]
mod aot_tests;
#[cfg(test)]
mod cache_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod reads_tests;
#[cfg(test)]
mod tests;
