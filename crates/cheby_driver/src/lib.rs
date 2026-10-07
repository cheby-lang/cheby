//! Build driver: runs the compiler's phases over a package (§13.12,
//! architecture.md §4).
//!
//! - [`load_package`] reads `cheby.toml` (§13.3), discovers the modules
//!   (§7.2), and lexes, parses and validates every module (phase A).
//! - [`check`] runs every implemented front-end phase for `cheby check`
//!   (D-282): phase A, then, if it reported no error, the import graph,
//!   name resolution and type checking over the package and the std
//!   modules it reaches (G1, G2). [`check_syntax`] stops after phase A.
//! - [`prepare_run`] and [`Program::run`] are `cheby run` (G3): the front
//!   end, then MIR of the items reachable from the program's `main`, the
//!   JIT and `main` (D-137). Constructs the back end cannot lower yet are
//!   E0038, reported like compile errors before any code is generated
//!   (plan §8); `cheby check` never lowers MIR.
//! - [`dump_mir`] and [`dump_clif`] print the MIR and CLIF that running
//!   the root module's `main` compiles, for goldens and the determinism
//!   checks.
//!
//! The build graph's parallel scheduling (D-263) and the build cache
//! (D-255) plug in later behind the same API. Output is deterministic
//! (D-264): modules are processed in a fixed order and diagnostics are
//! sorted by [`SourceMap::sort`].

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::{Diagnostic, FileId, SourceMap, Span, TextRange, TextSize, codes, render_all};
use cheby_iface::Ice;
use cheby_jit::{Jit, JitError};
use cheby_package::{MANIFEST_FILE, Manifest, ModulePath, PackageError};
use cheby_runtime::RunConfig;
use cheby_syntax::SyntaxTree;

mod backend;
mod frontend;
pub mod std_lib;

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
}

impl CheckOutput {
    /// Whether any diagnostic is an error. Warnings alone do not fail a
    /// check (D-080).
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }

    /// The diagnostics in the layout of D-418, each followed by an empty
    /// line, with no summary (D-443).
    #[must_use]
    pub fn render(&self) -> String {
        render_all(&self.diagnostics, &self.sources)
    }
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
    let mut out = CheckOutput::default();
    front_end(root, frontend::Mode::Check, &mut out)?;
    out.sources.sort(&mut out.diagnostics);
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
    let package = load_package(root, &mut out.sources, &mut out.diagnostics)?;
    let Some(package) = package else {
        return Ok(None);
    };
    if out.has_errors() {
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
    let mut out = CheckOutput::default();
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
    let units = match backend::lower_program(&front, &out.sources, main)? {
        Ok(units) => units,
        Err(unsupported) => {
            report_unsupported(&mut out, unsupported);
            return Ok(Prepared::CompileFailed(out));
        }
    };
    let jit = match backend::compile(&units, false) {
        Ok(jit) => jit,
        // A program error found only when loading, not a tool error
        // (D-442): E0053 at the `@external` that names the symbol.
        Err(JitError::UnresolvedSymbol(symbol)) => {
            let span = front
                .natives
                .get(symbol.as_str())
                .map(|binding| binding.span)
                .ok_or_else(|| {
                    Ice::new(format!("the native symbol `{symbol}` has no `@external`"))
                })?;
            out.diagnostics.push(
                Diagnostic::new(
                    codes::UNRESOLVED_NATIVE_SYMBOL,
                    format!("the native symbol `{symbol}` is not defined"),
                )
                .with_primary(span, "nothing defines this symbol")
                .with_note(
                    "the symbol must be defined by the Cheby runtime or the running process",
                ),
            );
            return Ok(Prepared::CompileFailed(out));
        }
        Err(e) => return Err(e.into()),
    };
    let entry = cheby_mir::EntryKind::Main(main.def);
    // Fails now, before anything runs, if the thunk was not compiled.
    jit.entry(entry)?;
    let config = RunConfig {
        args: options.args,
        program_name: path.to_string(),
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
/// order, with the items reachable from `main` (plan §8), then the entry
/// unit. Empty if the package has errors, if the back end cannot lower
/// something `main` needs (E0038 in the output), or if the root module has
/// no `main`.
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
    let units = units_for_dump(root, &mut out)?;
    if units.is_empty() {
        return Ok((out, String::new()));
    }
    let mut jit = backend::compile(&units, true)?;
    Ok((out, jit.take_clif().join("\n")))
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
    match backend::lower_program(&front, &out.sources, main)? {
        Ok(units) => Ok(units),
        Err(unsupported) => {
            report_unsupported(out, unsupported);
            Ok(Vec::new())
        }
    }
}

/// Adds the back end's E0038 diagnostics after the front end's, which are
/// already sorted (warnings only, since errors stop before lowering).
fn report_unsupported(out: &mut CheckOutput, mut unsupported: Vec<Diagnostic>) {
    out.sources.sort(&mut unsupported);
    out.diagnostics.extend(unsupported);
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
    let mut modules = Vec::with_capacity(files.len());
    for file in files {
        let bytes = std::fs::read(root.join(&file.file))
            .map_err(|e| Error::new(format!("{}: {e}", file.display)))?;
        let source = parse_file(sources, &file.display, bytes, diagnostics)?;
        modules.push(Module {
            path: file.path,
            source,
        });
    }
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
