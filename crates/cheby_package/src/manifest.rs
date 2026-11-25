//! Reading `cheby.toml` (§13.3, D-097, B1).
//!
//! The manifest is parsed with the `toml` crate, keeping spans, so every
//! problem is an `E0036` diagnostic with a snippet of `cheby.toml`. Keys:
//!
//! - `name`: the package name, a valid `LOWER` name other than `std`
//!   (§7.1, D-058).
//! - `version`: a semantic version (D-097).
//! - `targets`: optional, a non-empty list of `"native"` and `"js"`,
//!   both by default (D-190).
//! - `description` (a string) and `licences` (a list of strings) are
//!   accepted and not used yet.
//! - `[const-eval]`: the limits of constant evaluation (D-195, D-536),
//!   `time-limit = "10s"` (an integer followed by `ms` or `s`) and
//!   `memory-limit = "1GiB"` (an integer followed by `KiB`, `MiB` or
//!   `GiB`). Any other key or value is E0036.
//! - `[runtime]`: the default stack reservations (D-660), `fiber-stack`
//!   (8 MiB by default) and `main-stack` (64 MiB by default), in the form
//!   of `memory-limit` and from 1 MiB to 16 GiB. Any other key or value is
//!   E0036. The manifest stores bytes; the runtime rounds up to its page
//!   size (D-796).
//! - `kind`: `"program"`, the default, or `"library"` (D-1851).
//! - `[dependencies]`: a git URL with a tag or a commit (D-097, D-1879), or
//!   a path that names a directory with a manifest (D-1862).
//! - `[overrides]`: directories that replace git packages (D-1865).
//! - `[native]`: the package's native code (D-940, §12.5.4), `sources`
//!   (C files by package-relative path) and `libraries` (names the linker
//!   takes after `-l`). A source must end in `.c`, and [`Manifest::load`]
//!   also checks that it exists and stays inside the package once `..`
//!   and symbolic links are resolved; [`Manifest::parse`], which has no
//!   package directory, checks the rest. The table is E0036 in a package
//!   whose `targets` leave out `native`.
//!
//! Any other key is an error (D-444).

use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt;
use std::ops::Range;
use std::path::Path;
use std::time::Duration;

use cheby_diag::{Diagnostic, FileId, SourceMap, Span, TextRange, TextSize, codes};
use toml::Spanned;
use toml::de::{DeTable, DeValue};

use crate::PackageError;
use crate::names::{is_valid_name, suggest_name};
use crate::url::{GitUrl, is_valid_rev, is_valid_tag};

/// The file name of the manifest at the package root.
pub const MANIFEST_FILE: &str = "cheby.toml";

/// A package manifest whose required keys are valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// The package name, the first segment of its module paths (§7.1).
    pub name: String,
    pub version: Version,
    /// The declared targets (D-190), never empty.
    pub targets: Targets,
    /// What the package is for (D-1851).
    pub kind: Kind,
    pub description: Option<String>,
    pub licences: Vec<String>,
    /// The valid dependencies, in manifest order.
    pub dependencies: Vec<Dependency>,
    /// `[overrides]`, in manifest order. Only the root package's apply
    /// (D-1860).
    pub overrides: Vec<Override>,
    /// `[const-eval] time-limit`: the wall-clock time one constant's
    /// evaluation may take (D-536); [`Manifest::DEFAULT_TIME_LIMIT`] when
    /// not given.
    pub const_eval_time_limit: Duration,
    /// `[const-eval] memory-limit`: the most heap bytes that a constant's
    /// evaluation may keep live at once (D-536, D-617);
    /// [`Manifest::DEFAULT_MEMORY_LIMIT`] when not given.
    pub const_eval_memory_limit: u64,
    /// `[runtime] fiber-stack`: the stack reservation in bytes of every
    /// fiber that does not set its own (D-660);
    /// [`Manifest::DEFAULT_FIBER_STACK`] when not given. Not yet rounded to
    /// pages: the runtime does that (D-796).
    pub fiber_stack: u64,
    /// `[runtime] main-stack`: the stack reservation in bytes of the root
    /// fiber running `main` and of each test (D-660);
    /// [`Manifest::DEFAULT_MAIN_STACK`] when not given.
    pub main_stack: u64,
    /// `[native]` (D-940), validated; `None` without the table, or when
    /// the table has an error.
    pub native: Option<NativeTable>,
    /// The `cheby.toml` file in the source map, for diagnostics about the
    /// package as a whole.
    pub file: FileId,
}

/// The `[native]` table of a manifest (D-940, §12.5.4): the package's C
/// sources and the system libraries it links.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeTable {
    /// `sources`, package-relative, in manifest order.
    pub sources: Vec<String>,
    /// `libraries`, the names after `-l`, in manifest order.
    pub libraries: Vec<String>,
}

impl NativeTable {
    /// Whether the table names any source or library: a table with
    /// neither asks for no native code.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.sources.is_empty() && self.libraries.is_empty()
    }
}

/// A semantic version, `MAJOR.MINOR.PATCH[-PRE][+BUILD]` (D-097).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// The pre-release identifiers after `-`, as written.
    pub pre: Option<String>,
    /// The build metadata after `+`, as written.
    pub build: Option<String>,
}

/// A compilation target (§12, D-006).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Target {
    Native,
    Js,
}

/// The kind of a package (D-1851), `kind` in its manifest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A package with a default entry point. No package may depend on it
    /// (D-1854).
    #[default]
    Program,
    /// A package that others depend on.
    Library,
}

/// A set of targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Targets {
    native: bool,
    js: bool,
}

/// One entry of `[dependencies]` (§13.3, D-238).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dependency {
    /// The key, used as the first segment of the dependency's module
    /// paths in this package.
    pub key: String,
    pub source: DependencySource,
    /// The span of the key in `cheby.toml`.
    pub span: Span,
}

/// Where a dependency comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DependencySource {
    /// A git repository at a tag or a commit (D-097).
    Git {
        git: GitUrl,
        reference: GitReference,
    },
    /// The directory of another package, relative to the directory of this
    /// manifest, with `/` between its names (D-1856, D-1862).
    Path {
        path: String,
        /// The span of the path in `cheby.toml`.
        span: Span,
    },
}

/// What a git dependency points to.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GitReference {
    Tag(String),
    Rev(String),
}

impl fmt::Display for GitReference {
    /// As `cheby.lock` writes it: `tag:v1.2.0` or `rev:4f2a9c1`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tag(tag) => write!(f, "tag:{tag}"),
            Self::Rev(rev) => write!(f, "rev:{rev}"),
        }
    }
}

/// One entry of `[overrides]` (D-1860, D-1865): the directory that replaces
/// a git package in the whole build, when the manifest is the root's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Override {
    pub git: GitUrl,
    /// Relative to the directory of this manifest.
    pub path: String,
    /// The span of the URL in `cheby.toml`.
    pub span: Span,
}

impl Manifest {
    /// The time limit of one constant's evaluation when `[const-eval]`
    /// sets none (D-195, D-536).
    pub const DEFAULT_TIME_LIMIT: Duration = Duration::from_secs(10);

    /// The memory limit of one constant's evaluation when `[const-eval]`
    /// sets none (D-195, D-536): 1 GiB.
    pub const DEFAULT_MEMORY_LIMIT: u64 = 1 << 30;

    /// `[runtime] fiber-stack` when not given (D-660): 8 MiB.
    pub const DEFAULT_FIBER_STACK: u64 = 8 << 20;

    /// `[runtime] main-stack` when not given (D-660): 64 MiB in the
    /// reference implementation.
    pub const DEFAULT_MAIN_STACK: u64 = 64 << 20;

    /// The smallest stack `[runtime]` accepts (D-660): 1 MiB.
    pub const MIN_STACK: u64 = 1 << 20;

    /// The largest stack `[runtime]` accepts (D-660): 16 GiB.
    pub const MAX_STACK: u64 = 16 << 30;

    /// Reads `cheby.toml` in `root`, adds it to `sources` as `cheby.toml`,
    /// and checks it. Problems with the manifest are pushed to
    /// `diagnostics`. Returns `None` if the manifest cannot be used:
    /// invalid TOML, or a missing or invalid `name` or `version`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, or is too large.
    pub fn load(
        root: &Path,
        sources: &mut SourceMap,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<Option<Self>, PackageError> {
        Self::load_as(root, MANIFEST_FILE, sources, diagnostics)
    }

    /// The name and version of the package at `root`, if its manifest has
    /// both, without checking the rest.
    #[must_use]
    pub fn peek_version(root: &Path) -> Option<(String, Version)> {
        let text = std::fs::read_to_string(root.join(MANIFEST_FILE)).ok()?;
        let table = DeTable::parse(&text).ok()?;
        let name = table.get_ref().get("name")?.get_ref().as_str()?;
        let version = Version::parse(table.get_ref().get("version")?.get_ref().as_str()?)?;
        is_valid_name(name).then(|| (name.to_owned(), version))
    }

    /// [`Manifest::load`], adding the file to `sources` as `shown`, which
    /// errors name too.
    ///
    /// # Errors
    ///
    /// As [`Manifest::load`].
    pub fn load_as(
        root: &Path,
        shown: &str,
        sources: &mut SourceMap,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<Option<Self>, PackageError> {
        let path = root.join(MANIFEST_FILE);
        let bytes = std::fs::read(&path).map_err(|e| PackageError::io(shown, &e))?;
        let (text, bad_utf8) = match String::from_utf8(bytes) {
            Ok(text) => (text, None),
            Err(e) => {
                let at = e.utf8_error().valid_up_to();
                (String::from_utf8_lossy(e.as_bytes()).into_owned(), Some(at))
            }
        };
        let file = sources
            .add_file(shown, text)
            .map_err(|_| PackageError::too_large(shown))?;
        let text = sources
            .file(file)
            .map_or_default(cheby_diag::SourceFile::text_arc);
        if let Some(at) = bad_utf8 {
            diagnostics.push(
                Diagnostic::new(codes::INVALID_MANIFEST, "`cheby.toml` is not valid UTF-8")
                    .with_primary(span(file, at..at.saturating_add(3)), "")
                    .with_help("save the file as UTF-8"),
            );
            return Ok(None);
        }
        Ok(Self::parse_in(file, &text, Some(root), diagnostics))
    }

    /// Checks the manifest `text` of `file`. See [`Manifest::load`]. With
    /// no package directory, the sources of `[native]` are checked only
    /// for their form, not for their files (D-940).
    #[must_use]
    pub fn parse(file: FileId, text: &str, diagnostics: &mut Vec<Diagnostic>) -> Option<Self> {
        Self::parse_in(file, text, None, diagnostics)
    }

    /// Checks the manifest `text` of `file`, for the package at `root`,
    /// as [`Manifest::load`] does once it has read the file: the sources
    /// of `[native]` are checked on disk (D-940). The language server
    /// passes the editor's unsaved text of `cheby.toml` here.
    #[must_use]
    pub fn parse_at(
        file: FileId,
        text: &str,
        root: &Path,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Option<Self> {
        Self::parse_in(file, text, Some(root), diagnostics)
    }

    /// [`Manifest::parse`], checking the files of `[native]`'s sources
    /// against the package at `root` when it is given.
    fn parse_in(
        file: FileId,
        text: &str,
        root: Option<&Path>,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Option<Self> {
        let table = match DeTable::parse(text) {
            Ok(table) => table,
            Err(e) => {
                let mut diag =
                    Diagnostic::new(codes::INVALID_MANIFEST, "`cheby.toml` is not valid TOML");
                let message = e.message().lines().next().unwrap_or("").trim().to_owned();
                diag = match e.span() {
                    Some(range) => diag.with_primary(span(file, range), message),
                    None => diag.with_note(message),
                };
                diagnostics.push(diag);
                return None;
            }
        };
        Reader {
            file,
            root,
            diagnostics,
        }
        .manifest(table.get_ref())
    }

    /// The module path of the root module, which is the package name
    /// (D-142).
    #[must_use]
    pub fn root_module(&self) -> crate::ModulePath {
        crate::ModulePath::root(&self.name)
    }
}

impl Version {
    /// Compares by the precedence of semantic versions, which leaves the
    /// build metadata out.
    #[must_use]
    pub fn cmp_precedence(&self, other: &Self) -> Ordering {
        let core = |v: &Self| (v.major, v.minor, v.patch);
        core(self).cmp(&core(other)).then_with(|| {
            match (self.pre.as_deref(), other.pre.as_deref()) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => cmp_pre(a, b),
            }
        })
    }

    /// The major version of a package identity: the version up to its first
    /// part that is not zero (D-239), such as `1`, `0.1` or `0.0.3`.
    #[must_use]
    pub fn major_part(&self) -> String {
        match (self.major, self.minor) {
            (0, 0) => format!("0.0.{}", self.patch),
            (0, minor) => format!("0.{minor}"),
            (major, _) => major.to_string(),
        }
    }

    /// Parses a semantic version 2.0.0, such as `0.1.0` or
    /// `1.0.0-rc.1+build.5`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (rest, build) = match text.split_once('+') {
            Some((rest, build)) => (rest, Some(build)),
            None => (text, None),
        };
        let (core, pre) = match rest.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (rest, None),
        };
        let mut parts = core.split('.');
        let major = numeric(parts.next()?)?;
        let minor = numeric(parts.next()?)?;
        let patch = numeric(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        if let Some(pre) = pre {
            let ok = pre.split('.').all(|id| {
                identifier(id) && (!id.bytes().all(|b| b.is_ascii_digit()) || numeric(id).is_some())
            });
            if !ok {
                return None;
            }
        }
        if let Some(build) = build
            && !build.split('.').all(identifier)
        {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
            pre: pre.map(str::to_owned),
            build: build.map(str::to_owned),
        })
    }
}

/// A numeric version part: digits without a leading zero.
fn numeric(part: &str) -> Option<u64> {
    let digits = !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let leading_zero = part.len() > 1 && part.starts_with('0');
    if digits && !leading_zero {
        part.parse().ok()
    } else {
        None
    }
}

/// A pre-release or build identifier: `[0-9A-Za-z-]+`.
fn identifier(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The precedence of two pre-releases: identifier by identifier, numbers
/// before words, and the shorter first.
fn cmp_pre(a: &str, b: &str) -> Ordering {
    let mut b_ids = b.split('.');
    for a_id in a.split('.') {
        let Some(b_id) = b_ids.next() else {
            return Ordering::Greater;
        };
        let number = |id: &str| id.parse::<u64>().ok();
        let found = match (number(a_id), number(b_id)) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a_id.cmp(b_id),
        };
        if found != Ordering::Equal {
            return found;
        }
    }
    if b_ids.next().is_some() {
        Ordering::Less
    } else {
        Ordering::Equal
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        if let Some(build) = &self.build {
            write!(f, "+{build}")?;
        }
        Ok(())
    }
}

impl Target {
    /// The spelling in `cheby.toml` and on the command line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Js => "js",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "native" => Some(Self::Native),
            "js" => Some(Self::Js),
            _ => None,
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Targets {
    /// Both targets, the default (§13.3).
    pub const ALL: Self = Self {
        native: true,
        js: true,
    };
    /// No target.
    pub const NONE: Self = Self {
        native: false,
        js: false,
    };

    /// Only `target`.
    #[must_use]
    pub const fn only(target: Target) -> Self {
        Self::NONE.with(target)
    }

    /// This set with `target` added.
    #[must_use]
    pub const fn with(mut self, target: Target) -> Self {
        self.insert(target);
        self
    }

    /// The targets in both sets.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self {
            native: self.native && other.native,
            js: self.js && other.js,
        }
    }

    /// Whether the set has no target.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.native && !self.js
    }

    #[must_use]
    pub const fn contains(self, target: Target) -> bool {
        match target {
            Target::Native => self.native,
            Target::Js => self.js,
        }
    }

    /// The targets in the set, `native` first.
    pub fn iter(self) -> impl Iterator<Item = Target> {
        [Target::Native, Target::Js]
            .into_iter()
            .filter(move |t| self.contains(*t))
    }

    const fn insert(&mut self, target: Target) -> bool {
        let slot = match target {
            Target::Native => &mut self.native,
            Target::Js => &mut self.js,
        };
        let new = !*slot;
        *slot = true;
        new
    }
}

impl Default for Targets {
    fn default() -> Self {
        Self::ALL
    }
}

/// What is wrong with an entry of `[native]`'s `sources` (D-940).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceProblem {
    /// It does not end in `.c`.
    NotC,
    /// It is absolute or starts at a root.
    NotRelative,
    /// It leaves the package through `..` or a symbolic link.
    Outside,
    /// No file or directory has its path.
    Missing,
    /// It names a directory or another kind of file.
    NotAFile,
}

/// Whether the relative `path` stays inside the directory it is relative
/// to once its `..` components are resolved lexically.
fn stays_inside(path: &Path) -> bool {
    let mut depth = 0_usize;
    for component in path.components() {
        match component {
            std::path::Component::Normal(_) => depth = depth.saturating_add(1),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => match depth.checked_sub(1) {
                Some(up) => depth = up,
                None => return false,
            },
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return false,
        }
    }
    true
}

/// The problem of the source `path`, relative to the package at `root`, on
/// disk: missing, not a regular file, or outside the package once
/// symbolic links are resolved; `None` when it is a file inside.
fn source_on_disk(root: &Path, path: &Path) -> Option<SourceProblem> {
    let Ok(full) = std::fs::canonicalize(root.join(path)) else {
        return Some(SourceProblem::Missing);
    };
    // A root that cannot be resolved has no file inside it.
    let Ok(root) = std::fs::canonicalize(root) else {
        return Some(SourceProblem::Missing);
    };
    if !full.starts_with(&root) {
        return Some(SourceProblem::Outside);
    }
    if !full.is_file() {
        return Some(SourceProblem::NotAFile);
    }
    None
}

/// Whether `name` is a library name of `[native]` (D-940): non-empty, of
/// ASCII letters, digits, `_`, `-`, `.` and `+`.
fn is_library_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+'))
}

/// The span of a byte range of `cheby.toml`.
fn span(file: FileId, range: Range<usize>) -> Span {
    let size = |n: usize| TextSize::try_from(n).unwrap_or(TextSize::MAX);
    Span::new(file, TextRange::new(size(range.start), size(range.end)))
}

/// Checks a parsed manifest and reports into `diagnostics`.
struct Reader<'d> {
    file: FileId,
    /// The package directory, when the manifest was loaded from one: the
    /// sources of `[native]` are then checked on disk (D-940).
    root: Option<&'d Path>,
    diagnostics: &'d mut Vec<Diagnostic>,
}

/// The units of `[const-eval] time-limit`, with their factors to
/// milliseconds (D-536).
const TIME_UNITS: &[(&str, u64)] = &[("ms", 1), ("s", 1000)];

/// The units of `[const-eval] memory-limit`, with their factors to bytes
/// (D-536).
const MEMORY_UNITS: &[(&str, u64)] = &[("KiB", 1 << 10), ("MiB", 1 << 20), ("GiB", 1 << 30)];

/// The top-level keys, in the order §13.3 lists them, for help lines
/// (D-444, D-940).
const KNOWN_KEYS: &str = "`name`, `version`, `description`, `licences`, `kind`, `targets`, \
     `[dependencies]`, `[overrides]`, `[const-eval]`, `[runtime]` and `[native]`";

/// The help line of an unknown key in `[native]` (D-940).
const NATIVE_KEYS: &str = "the keys are `sources` and `libraries`";

/// The help line of `[runtime]`'s errors (D-660).
const RUNTIME_KEYS: &str = "the keys are `fiber-stack` and `main-stack`";

impl Reader<'_> {
    fn diag(&self, message: impl Into<String>, at: Range<usize>, label: &str) -> Diagnostic {
        Diagnostic::new(codes::INVALID_MANIFEST, message).with_primary(span(self.file, at), label)
    }

    fn push(&mut self, diag: Diagnostic) {
        self.diagnostics.push(diag);
    }

    fn manifest(mut self, table: &DeTable<'_>) -> Option<Manifest> {
        let mut name = None;
        let mut version = None;
        let mut targets = Targets::ALL;
        let mut kind = Kind::default();
        let mut description = None;
        let mut licences = Vec::new();
        let mut dependencies = Vec::new();
        let mut dependency_keys = Vec::new();
        let mut overrides = Vec::new();
        let mut time_limit = Manifest::DEFAULT_TIME_LIMIT;
        let mut memory_limit = Manifest::DEFAULT_MEMORY_LIMIT;
        let mut fiber_stack = Manifest::DEFAULT_FIBER_STACK;
        let mut main_stack = Manifest::DEFAULT_MAIN_STACK;
        let mut native = None;
        for (key, value) in table {
            match key.get_ref().as_ref() {
                "native" => native = Some((key.span(), self.native(value))),
                "name" => name = self.name(value),
                "version" => version = self.version(value),
                "kind" => kind = self.kind(value).unwrap_or_default(),
                "targets" => targets = self.targets(value).unwrap_or(Targets::ALL),
                "description" => description = self.string(value, "`description`"),
                "licences" => licences = self.licences(value),
                "const-eval" => self.const_eval(value, &mut time_limit, &mut memory_limit),
                "overrides" => overrides = self.overrides(value),
                "runtime" => self.runtime(value, &mut fiber_stack, &mut main_stack),
                "dependencies" => {
                    (dependencies, dependency_keys) = self.dependencies(value);
                }
                other => {
                    let diag = self
                        .diag(
                            format!("unknown key `{other}` in `cheby.toml`"),
                            key.span(),
                            "unknown key",
                        )
                        .with_help(format!("the keys are {KNOWN_KEYS}"));
                    self.push(diag);
                }
            }
        }
        for (key, example) in [
            ("name", "name = \"my_app\""),
            ("version", "version = \"0.1.0\""),
        ] {
            if !table.contains_key(key) {
                self.push(
                    Diagnostic::new(
                        codes::INVALID_MANIFEST,
                        format!("`cheby.toml` has no `{key}`"),
                    )
                    .with_help(format!("add the package {key}, such as `{example}`")),
                );
            }
        }
        if let Some(name) = &name {
            self.dependency_keys(name, &dependency_keys);
        }
        let native = match native {
            Some((at, _)) if !targets.contains(Target::Native) => {
                let diag = self
                    .diag(
                        "`[native]` is not allowed in a package that does not target `native`",
                        at,
                        "",
                    )
                    .with_help("add `\"native\"` to `targets`, or remove `[native]`");
                self.push(diag);
                None
            }
            Some((_, table)) => table,
            None => None,
        };
        Some(Manifest {
            name: name?,
            version: version?,
            targets,
            kind,
            description,
            licences,
            dependencies,
            overrides,
            const_eval_time_limit: time_limit,
            const_eval_memory_limit: memory_limit,
            fiber_stack,
            main_stack,
            native,
            file: self.file,
        })
    }

    /// Reads `[native]` (D-940): `sources` and `libraries`, each a list of
    /// strings, and no other key. Returns the table when it has no error.
    fn native(&mut self, value: &Spanned<DeValue<'_>>) -> Option<NativeTable> {
        let Some(table) = value.get_ref().as_table() else {
            self.wrong_type(value, "`native`", "a table");
            return None;
        };
        let mut ok = true;
        let mut sources = Vec::new();
        let mut libraries = Vec::new();
        for (key, value) in table {
            match key.get_ref().as_ref() {
                "sources" => {
                    let items = self.native_list(value, "`sources`", "a source", &mut ok);
                    for (text, at) in items {
                        ok &= self.native_source(&text, at);
                        sources.push(text);
                    }
                }
                "libraries" => {
                    let items = self.native_list(value, "`libraries`", "a library", &mut ok);
                    for (text, at) in items {
                        ok &= self.native_library(&text, at);
                        libraries.push(text);
                    }
                }
                other => {
                    let diag = self
                        .diag(
                            format!("unknown key `{other}` in `[native]`"),
                            key.span(),
                            "unknown key",
                        )
                        .with_help(NATIVE_KEYS);
                    self.push(diag);
                    ok = false;
                }
            }
        }
        ok.then_some(NativeTable { sources, libraries })
    }

    /// The strings of a list of `[native]`, each with its span; reports a
    /// value that is not a list, and an item that is not a string, and
    /// clears `ok` then.
    fn native_list(
        &mut self,
        value: &Spanned<DeValue<'_>>,
        what: &str,
        item_what: &str,
        ok: &mut bool,
    ) -> Vec<(String, Range<usize>)> {
        let Some(items) = value.get_ref().as_array() else {
            self.wrong_type(value, what, "a list of strings");
            *ok = false;
            return Vec::new();
        };
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            if let Some(text) = item.get_ref().as_str() {
                out.push((text.to_owned(), item.span()));
            } else {
                self.wrong_type(item, item_what, "a string");
                *ok = false;
            }
        }
        out
    }

    /// Checks one entry of `sources` (D-940): a package-relative path
    /// ending in `.c` that stays inside the package once `..` is resolved
    /// and, when the package directory is known, a file that exists and
    /// stays inside the package once symbolic links are resolved.
    /// D-1200: an absolute path and a directory are
    /// E0036 too, `..` is judged lexically before the file is looked up,
    /// an empty table is valid and asks for nothing, and a source listed
    /// twice is not checked.
    #[expect(
        clippy::case_sensitive_file_extension_comparisons,
        reason = "D-940: a source ends in `.c`, in lower case"
    )]
    fn native_source(&mut self, text: &str, at: Range<usize>) -> bool {
        let path = Path::new(text);
        let problem = if !text.ends_with(".c") {
            Some(SourceProblem::NotC)
        } else if path.has_root() || path.is_absolute() {
            Some(SourceProblem::NotRelative)
        } else if !stays_inside(path) {
            Some(SourceProblem::Outside)
        } else {
            self.root.and_then(|root| source_on_disk(root, path))
        };
        let Some(problem) = problem else {
            return true;
        };
        let diag = match problem {
            SourceProblem::NotC => self
                .diag(format!("`{text}` is not a C source"), at, "")
                .with_help(
                    "list only the `.c` files to compile; the compiler finds headers through \
                     `#include`",
                ),
            SourceProblem::NotRelative => self.diag(
                format!("the C source `{text}` must be a path relative to the package root"),
                at,
                "",
            ),
            SourceProblem::Outside => self
                .diag(
                    format!("the C source `{text}` is outside the package"),
                    at,
                    "",
                )
                .with_note(
                    "a C source must stay inside the package once `..` and symbolic links are \
                     resolved",
                ),
            SourceProblem::Missing => {
                self.diag(format!("the C source `{text}` does not exist"), at, "")
            }
            SourceProblem::NotAFile => {
                self.diag(format!("the C source `{text}` is not a file"), at, "")
            }
        };
        self.push(diag);
        false
    }

    /// Checks one entry of `libraries` (D-940): a non-empty name of ASCII
    /// letters, digits, `_`, `-`, `.` and `+`.
    fn native_library(&mut self, text: &str, at: Range<usize>) -> bool {
        if is_library_name(text) {
            return true;
        }
        let message = if text.is_empty() {
            "a library name must not be empty".to_owned()
        } else {
            format!("`{text}` is not a valid library name")
        };
        let diag = self.diag(message, at, "").with_help(
            "write the name the linker takes after `-l`, such as `\"sqlite3\"`, in letters, \
             digits, `_`, `-`, `.` and `+`; give each library its own string",
        );
        self.push(diag);
        false
    }

    /// Reads `[const-eval]` (D-536, D-616): sets the
    /// limits it gives and reports every other key and every bad value.
    fn const_eval(
        &mut self,
        value: &Spanned<DeValue<'_>>,
        time_limit: &mut Duration,
        memory_limit: &mut u64,
    ) {
        let Some(table) = value.get_ref().as_table() else {
            self.wrong_type(value, "`const-eval`", "a table");
            return;
        };
        for (key, value) in table {
            match key.get_ref().as_ref() {
                "time-limit" => {
                    if let Some(limit) = self.limit(value, "`time-limit`", TIME_UNITS, "10s") {
                        *time_limit = Duration::from_millis(limit);
                    }
                }
                "memory-limit" => {
                    if let Some(limit) = self.limit(value, "`memory-limit`", MEMORY_UNITS, "1GiB") {
                        *memory_limit = limit;
                    }
                }
                other => {
                    let diag = self
                        .diag(
                            format!("unknown key `{other}` in `[const-eval]`"),
                            key.span(),
                            "unknown key",
                        )
                        .with_help("the keys are `time-limit` and `memory-limit`");
                    self.push(diag);
                }
            }
        }
    }

    /// Reads `[runtime]` (D-660): sets the stack sizes it gives and reports
    /// every other key and every bad value. Only the root package's table
    /// takes effect; a dependency's would be checked the same way, but
    /// dependencies are rejected until M5 (§13.4).
    fn runtime(
        &mut self,
        value: &Spanned<DeValue<'_>>,
        fiber_stack: &mut u64,
        main_stack: &mut u64,
    ) {
        let Some(table) = value.get_ref().as_table() else {
            self.wrong_type(value, "`runtime`", "a table");
            return;
        };
        for (key, value) in table {
            match key.get_ref().as_ref() {
                "fiber-stack" => {
                    if let Some(size) = self.stack_size(value, "`fiber-stack`", "8MiB") {
                        *fiber_stack = size;
                    }
                }
                "main-stack" => {
                    if let Some(size) = self.stack_size(value, "`main-stack`", "64MiB") {
                        *main_stack = size;
                    }
                }
                other => {
                    let diag = self
                        .diag(
                            format!("unknown key `{other}` in `[runtime]`"),
                            key.span(),
                            "unknown key",
                        )
                        .with_help(RUNTIME_KEYS);
                    self.push(diag);
                }
            }
        }
    }

    /// A stack size of `[runtime]`: `memory-limit`'s form (D-536), from
    /// [`Manifest::MIN_STACK`] to [`Manifest::MAX_STACK`] (D-660). Reports
    /// E0036 and returns `None` for anything else. The size is kept in
    /// bytes, unrounded (D-796).
    fn stack_size(
        &mut self,
        value: &Spanned<DeValue<'_>>,
        what: &str,
        example: &str,
    ) -> Option<u64> {
        let size = self.amount(value, what, MEMORY_UNITS, example)?;
        if (Manifest::MIN_STACK..=Manifest::MAX_STACK).contains(&size) {
            return Some(size);
        }
        // The range message's wording is D-796's.
        let diag = self.diag(
            format!("{what} must be from 1MiB to 16GiB"),
            value.span(),
            "",
        );
        self.push(diag);
        None
    }

    /// A limit written as a string: a positive integer followed by one of
    /// `units`, converted with the unit's factor. Reports E0036, with
    /// `example` in the help, and returns `None` for anything else.
    fn limit(
        &mut self,
        value: &Spanned<DeValue<'_>>,
        what: &str,
        units: &[(&str, u64)],
        example: &str,
    ) -> Option<u64> {
        let limit = self.amount(value, what, units, example)?;
        if limit == 0 {
            let diag = self.diag(
                format!("{what} must be greater than zero"),
                value.span(),
                "",
            );
            self.push(diag);
            return None;
        }
        Some(limit)
    }

    /// An amount written as a string: an integer in decimal digits followed
    /// by one of `units`, converted with the unit's factor, zero included.
    /// Reports E0036, with `example` in the help, and returns `None` for a
    /// value of another form or one that does not fit in 64 bits.
    fn amount(
        &mut self,
        value: &Spanned<DeValue<'_>>,
        what: &str,
        units: &[(&str, u64)],
        example: &str,
    ) -> Option<u64> {
        let text = self.string(value, what)?;
        let spelled = units
            .iter()
            .map(|(unit, _)| format!("`{unit}`"))
            .collect::<Vec<_>>();
        let unit_list = match spelled.split_last() {
            Some((last, [])) => last.clone(),
            Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
            None => String::new(),
        };
        let digits = text.bytes().take_while(u8::is_ascii_digit).count();
        let (number, unit) = text.split_at(digits);
        let factor = units
            .iter()
            .find(|(name, _)| *name == unit)
            .map(|(_, factor)| *factor);
        let (Some(factor), false) = (factor, number.is_empty()) else {
            let diag = self
                .diag(format!("`{text}` is not a valid {what}"), value.span(), "")
                .with_help(format!(
                    "write an integer followed by {unit_list}, such as `\"{example}\"`"
                ));
            self.push(diag);
            return None;
        };
        let limit = number
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(factor));
        if limit.is_none() {
            let diag = self.diag(format!("{what} `{text}` is too large"), value.span(), "");
            self.push(diag);
        }
        limit
    }

    fn wrong_type(&mut self, value: &Spanned<DeValue<'_>>, what: &str, expected: &str) {
        let found = value.get_ref().type_str();
        let article = if found.starts_with(['a', 'e', 'i', 'o', 'u']) {
            "an"
        } else {
            "a"
        };
        let diag = self.diag(
            format!("{what} must be {expected}"),
            value.span(),
            &format!("this is {article} {found}"),
        );
        self.push(diag);
    }

    fn string(&mut self, value: &Spanned<DeValue<'_>>, what: &str) -> Option<String> {
        if let Some(s) = value.get_ref().as_str() {
            Some(s.to_owned())
        } else {
            self.wrong_type(value, what, "a string");
            None
        }
    }

    fn name(&mut self, value: &Spanned<DeValue<'_>>) -> Option<String> {
        let name = self.string(value, "`name`")?;
        if name == "std" {
            let diag = self.diag(
                "the package name `std` is reserved for the standard library",
                value.span(),
                "",
            );
            self.push(diag);
            return None;
        }
        if !is_valid_name(&name) {
            let mut diag = self
                .diag(
                    format!("package name `{name}` is not a valid name"),
                    value.span(),
                    "",
                )
                .with_note("a package name is a snake_case identifier that is not a keyword");
            if let Some(suggestion) = suggest_name(&name) {
                diag = diag.with_help(format!("write `\"{suggestion}\"`"));
            }
            self.push(diag);
            return None;
        }
        Some(name)
    }

    fn version(&mut self, value: &Spanned<DeValue<'_>>) -> Option<Version> {
        let text = self.string(value, "`version`")?;
        let version = Version::parse(&text);
        if version.is_none() {
            let diag = self
                .diag(
                    format!("`{text}` is not a semantic version"),
                    value.span(),
                    "",
                )
                .with_help("write a version such as `0.1.0`");
            self.push(diag);
        }
        version
    }

    fn targets(&mut self, value: &Spanned<DeValue<'_>>) -> Option<Targets> {
        let Some(items) = value.get_ref().as_array() else {
            self.wrong_type(value, "`targets`", "a list of targets");
            return None;
        };
        let mut targets = Targets::NONE;
        let mut ok = true;
        for item in items {
            let Some(text) = item.get_ref().as_str() else {
                self.wrong_type(item, "a target", "a string");
                ok = false;
                continue;
            };
            if let Some(target) = Target::parse(text) {
                if !targets.insert(target) {
                    let diag =
                        self.diag(format!("target `{text}` is listed twice"), item.span(), "");
                    self.push(diag);
                }
            } else {
                let diag = self
                    .diag(format!("unknown target `{text}`"), item.span(), "")
                    .with_help("the targets are `\"native\"` and `\"js\"`");
                self.push(diag);
                ok = false;
            }
        }
        if items.is_empty() {
            let diag = self.diag("`targets` is empty", value.span(), "").with_help(
                "list `\"native\"`, `\"js\"` or both, or remove `targets` to support both",
            );
            self.push(diag);
            ok = false;
        }
        ok.then_some(targets)
    }

    fn licences(&mut self, value: &Spanned<DeValue<'_>>) -> Vec<String> {
        let Some(items) = value.get_ref().as_array() else {
            self.wrong_type(value, "`licences`", "a list of strings");
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|item| self.string(item, "a licence"))
            .collect()
    }

    /// `kind` (D-1851).
    fn kind(&mut self, value: &Spanned<DeValue<'_>>) -> Option<Kind> {
        let text = self.string(value, "`kind`")?;
        match text.as_str() {
            "program" => Some(Kind::Program),
            "library" => Some(Kind::Library),
            _ => {
                let diag = self
                    .diag(format!("unknown kind `{text}`"), value.span(), "")
                    .with_help("the kinds are `\"program\"` and `\"library\"`");
                self.push(diag);
                None
            }
        }
    }

    /// Reads every dependency. Also returns every key with its span, valid
    /// entry or not, for [`Self::dependency_keys`].
    fn dependencies(
        &mut self,
        value: &Spanned<DeValue<'_>>,
    ) -> (Vec<Dependency>, Vec<(String, Span)>) {
        let Some(table) = value.get_ref().as_table() else {
            self.wrong_type(value, "`dependencies`", "a table");
            return (Vec::new(), Vec::new());
        };
        let keys = table
            .keys()
            .map(|k| (k.get_ref().as_ref().to_owned(), span(self.file, k.span())))
            .collect();
        let deps = table
            .iter()
            .filter_map(|(k, v)| self.dependency(k, v))
            .collect();
        (deps, keys)
    }

    /// A git URL, reported at `at` when a dependency may not have it
    /// (D-1879).
    fn git_url(&mut self, text: &str, at: Range<usize>) -> Option<GitUrl> {
        match GitUrl::parse(text) {
            Ok(url) => Some(url),
            Err(problem) => {
                // A password is never repeated in a message.
                let shown = if problem.contains("password") {
                    "the git URL".to_owned()
                } else {
                    format!("the git URL `{text}`")
                };
                let diag = self.diag(format!("{shown} {problem}"), at, "").with_help(
                    "write `https://host/path`, `ssh://git@host/path` or `git@host:path`",
                );
                self.push(diag);
                None
            }
        }
    }

    /// The `tag` or `rev` of a git dependency, `field`, when git could have
    /// it (D-1879).
    fn git_reference(&mut self, field: &str, value: &Spanned<DeValue<'_>>) -> Option<GitReference> {
        let text = self.string(value, &format!("`{field}`"))?;
        let (valid, label) = if field == "tag" {
            (is_valid_tag(&text), "not the name of a git tag")
        } else {
            (
                is_valid_rev(&text),
                "not a commit hash of at least seven digits",
            )
        };
        if !valid {
            let diag = self.diag(
                format!("`{text}` is not a valid `{field}`"),
                value.span(),
                label,
            );
            self.push(diag);
            return None;
        }
        Some(if field == "tag" {
            GitReference::Tag(text)
        } else {
            GitReference::Rev(text)
        })
    }

    /// `[overrides]` (D-1860, D-1865).
    fn overrides(&mut self, value: &Spanned<DeValue<'_>>) -> Vec<Override> {
        let Some(table) = value.get_ref().as_table() else {
            self.wrong_type(value, "`overrides`", "a table");
            return Vec::new();
        };
        let mut out: Vec<Override> = Vec::new();
        for (key, value) in table {
            let Some(git) = self.git_url(key.get_ref().as_ref(), key.span()) else {
                continue;
            };
            let path = value
                .get_ref()
                .as_table()
                .filter(|entry| entry.len() == 1)
                .and_then(|entry| entry.get("path"));
            let Some(path) = path else {
                let diag = self
                    .diag("an override has only `path`", value.span(), "")
                    .with_help("write `{ path = \"../…\" }`");
                self.push(diag);
                continue;
            };
            let Some(path) = self.relative_path(path) else {
                continue;
            };
            if out.iter().any(|o| o.git.canonical() == git.canonical()) {
                let diag = self.diag(
                    format!("`{}` is overridden twice", git.canonical()),
                    key.span(),
                    "",
                );
                self.push(diag);
                continue;
            }
            out.push(Override {
                git,
                path,
                span: span(self.file, key.span()),
            });
        }
        out
    }

    fn dependency(
        &mut self,
        key: &Spanned<Cow<'_, str>>,
        value: &Spanned<DeValue<'_>>,
    ) -> Option<Dependency> {
        let name = key.get_ref().as_ref();
        let mut ok = true;
        if name == "std" {
            let diag = self.diag(
                "the dependency key `std` is reserved for the standard library",
                key.span(),
                "",
            );
            self.push(diag);
            ok = false;
        } else if !is_valid_name(name) {
            let diag = self
                .diag(
                    format!("dependency key `{name}` is not a valid name"),
                    key.span(),
                    "",
                )
                .with_note("a dependency key is a snake_case identifier that is not a keyword");
            self.push(diag);
            ok = false;
        }
        let Some(table) = value.get_ref().as_table() else {
            self.wrong_type(value, &format!("dependency `{name}`"), "a table");
            return None;
        };
        if let Some((at, value)) = table.get_key_value("path") {
            return self.path_dependency(key, table, at, value).filter(|_| ok);
        }
        let mut git = None;
        let mut reference = None;
        for (k, v) in table {
            let field = k.get_ref().as_ref();
            match field {
                "git" => {
                    let text = self.string(v, "`git`");
                    git = text.and_then(|text| self.git_url(&text, v.span()));
                }
                "tag" | "rev" => {
                    let Some(found) = self.git_reference(field, v) else {
                        ok = false;
                        continue;
                    };
                    if reference.is_some() {
                        let diag = self.diag(
                            format!("dependency `{name}` has both `tag` and `rev`"),
                            k.span(),
                            "",
                        );
                        self.push(diag);
                        ok = false;
                    }
                    reference = Some(found);
                }
                other => {
                    let diag = self
                        .diag(
                            format!("unknown key `{other}` in dependency `{name}`"),
                            k.span(),
                            "",
                        )
                        .with_help(
                            "a dependency has `git` and either `tag` or `rev`, or only `path`",
                        );
                    self.push(diag);
                    ok = false;
                }
            }
        }
        // A value that is there and wrong was reported.
        let missing = if !table.contains_key("git") {
            Some("`git`")
        } else if !table.contains_key("tag") && !table.contains_key("rev") {
            Some("`tag` or `rev`")
        } else {
            None
        };
        if let Some(missing) = missing {
            let diag = self
                .diag(
                    format!("dependency `{name}` has no {missing}"),
                    key.span(),
                    "",
                )
                .with_help(
                    "write `{ path = \"../…\" }` or `{ git = \"https://…\", tag = \"v1.0.0\" }`",
                );
            self.push(diag);
        }
        let source = DependencySource::Git {
            git: git?,
            reference: reference?,
        };
        ok.then(|| Dependency {
            key: name.to_owned(),
            source,
            span: span(self.file, key.span()),
        })
    }

    /// The path of a local package (D-1862).
    fn relative_path(&mut self, value: &Spanned<DeValue<'_>>) -> Option<String> {
        let path = self.string(value, "`path`")?;
        let drive = path.as_bytes().get(1) == Some(&b':');
        let problem = if path.is_empty() {
            Some("the path of a dependency is empty".to_owned())
        } else if path.contains('\\') {
            Some(format!("the path `{path}` has a `\\`"))
        } else if path.starts_with('/') || drive {
            Some(format!("the path `{path}` is absolute"))
        } else {
            None
        };
        if let Some(message) = problem {
            let diag = self.diag(message, value.span(), "").with_help(
                "write it relative to the directory of this `cheby.toml`, with `/` between names",
            );
            self.push(diag);
            return None;
        }
        Some(path)
    }

    /// A dependency with a `path` (D-1856, D-1862), whose key
    /// [`Self::dependency`] has checked. With a package directory, the
    /// path must name a directory that holds a manifest.
    fn path_dependency(
        &mut self,
        key: &Spanned<Cow<'_, str>>,
        table: &DeTable<'_>,
        at: &Spanned<Cow<'_, str>>,
        value: &Spanned<DeValue<'_>>,
    ) -> Option<Dependency> {
        let name = key.get_ref().as_ref();
        let mut ok = true;
        for (k, _) in table.iter().filter(|(k, _)| k.span() != at.span()) {
            let field = k.get_ref().as_ref();
            let message = if matches!(field, "git" | "tag" | "rev") {
                format!("dependency `{name}` has both `path` and `{field}`")
            } else {
                format!("unknown key `{field}` in dependency `{name}`")
            };
            let diag = self
                .diag(message, k.span(), "")
                .with_help("a dependency has `git` and either `tag` or `rev`, or only `path`");
            self.push(diag);
            ok = false;
        }
        let path = self.relative_path(value)?;
        if let Some(root) = self.root
            && !root.join(&path).join(MANIFEST_FILE).is_file()
        {
            let diag = self.diag(
                format!("there is no package at `{path}`"),
                value.span(),
                "no `cheby.toml` in this directory",
            );
            self.push(diag);
            return None;
        }
        ok.then(|| Dependency {
            key: name.to_owned(),
            source: DependencySource::Path {
                path,
                span: span(self.file, value.span()),
            },
            span: span(self.file, key.span()),
        })
    }

    /// A dependency key must differ from the package name (D-238). Checked
    /// on every key, whether or not the rest of its entry is valid.
    fn dependency_keys(&mut self, package: &str, keys: &[(String, Span)]) {
        for (_, at) in keys.iter().filter(|(key, _)| key == package) {
            self.push(
                Diagnostic::new(
                    codes::INVALID_MANIFEST,
                    format!("dependency key `{package}` is the package's own name"),
                )
                .with_primary(*at, "")
                .with_help("choose another key; the key is the name this package imports it by"),
            );
        }
    }
}
