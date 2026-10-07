//! Build driver: runs the compiler's phases over a package (§13.12,
//! architecture.md §4).
//!
//! In phase A of M1 the front end is the syntax layer only:
//! [`load_package`] reads `cheby.toml` (§13.3), discovers the modules
//! (§7.2), and lexes, parses and validates every module. [`check`] runs it
//! for `cheby check` (D-282) and returns the diagnostics, sorted and ready
//! to render (D-418).
//!
//! Later phases plug in after [`load_package`]: name resolution and module
//! interfaces (B), type checking (C), and then MIR and code generation.
//! The build graph, its parallel scheduling (D-263) and the build cache
//! (D-255) arrive with them. Output is deterministic (D-264): modules are
//! processed in path order and diagnostics are sorted by
//! [`SourceMap::sort`].

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::{Diagnostic, FileId, SourceMap, Span, TextRange, TextSize, codes, render_all};
use cheby_package::{MANIFEST_FILE, Manifest, ModulePath, PackageError};
use cheby_syntax::SyntaxTree;

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

/// Checks the package at `root` without producing artifacts (D-282).
///
/// # Errors
///
/// Returns an [`Error`] for failures that are not diagnostics, such as an
/// unreadable file.
pub fn check(root: &Path) -> Result<CheckOutput, Error> {
    let mut out = CheckOutput::default();
    let package = load_package(root, &mut out.sources, &mut out.diagnostics)?;
    if let Some(package) = &package {
        check_package(package, &mut out.diagnostics);
    }
    out.sources.sort(&mut out.diagnostics);
    Ok(out)
}

/// The phases after parsing. Phase A has none; name resolution (B) and
/// type checking (C) are added here, and they see each module only
/// through its syntax tree and the interfaces of its dependencies
/// (D-246).
const fn check_package(_package: &Package, _diagnostics: &mut Vec<Diagnostic>) {}

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
    // there (D-425). They do not depend on the tree's shape, and they
    // explain words such as `if` and `return` that cause the grammar
    // errors (E0007).
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
