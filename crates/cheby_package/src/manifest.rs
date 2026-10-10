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
//! - `[dependencies]`: each entry is checked, and then the table is
//!   rejected, because dependencies arrive in M5 (§13.4).
//!
//! Any other key is an error (D-444).

use std::borrow::Cow;
use std::fmt;
use std::ops::Range;
use std::path::Path;
use std::time::Duration;

use cheby_diag::{Diagnostic, FileId, SourceMap, Span, TextRange, TextSize, codes};
use toml::Spanned;
use toml::de::{DeTable, DeValue};

use crate::PackageError;
use crate::names::{is_valid_name, suggest_name};

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
    pub description: Option<String>,
    pub licences: Vec<String>,
    /// The dependencies as written. A manifest that has any is reported as
    /// an error until M5, but its entries are still read.
    pub dependencies: Vec<Dependency>,
    /// `[const-eval] time-limit`: the wall-clock time one constant's
    /// evaluation may take (D-536); [`Manifest::DEFAULT_TIME_LIMIT`] when
    /// not given.
    pub const_eval_time_limit: Duration,
    /// `[const-eval] memory-limit`: the most heap bytes that a constant's
    /// evaluation may keep live at once (D-536, GAP(const-limit-baseline));
    /// [`Manifest::DEFAULT_MEMORY_LIMIT`] when not given.
    pub const_eval_memory_limit: u64,
    /// The `cheby.toml` file in the source map, for diagnostics about the
    /// package as a whole.
    pub file: FileId,
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
    pub git: String,
    pub reference: GitReference,
    /// The span of the key in `cheby.toml`.
    pub span: Span,
}

/// What a git dependency points to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitReference {
    Tag(String),
    Rev(String),
}

impl Manifest {
    /// The time limit of one constant's evaluation when `[const-eval]`
    /// sets none (D-195, D-536).
    pub const DEFAULT_TIME_LIMIT: Duration = Duration::from_secs(10);

    /// The memory limit of one constant's evaluation when `[const-eval]`
    /// sets none (D-195, D-536): 1 GiB.
    pub const DEFAULT_MEMORY_LIMIT: u64 = 1 << 30;

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
        let path = root.join(MANIFEST_FILE);
        let bytes = std::fs::read(&path).map_err(|e| PackageError::io(MANIFEST_FILE, &e))?;
        let (text, bad_utf8) = match String::from_utf8(bytes) {
            Ok(text) => (text, None),
            Err(e) => {
                let at = e.utf8_error().valid_up_to();
                (String::from_utf8_lossy(e.as_bytes()).into_owned(), Some(at))
            }
        };
        let file = sources
            .add_file(MANIFEST_FILE, text)
            .map_err(|_| PackageError::too_large(MANIFEST_FILE))?;
        let text = sources
            .file(file)
            .map(cheby_diag::SourceFile::text_arc)
            .unwrap_or_default();
        if let Some(at) = bad_utf8 {
            diagnostics.push(
                Diagnostic::new(codes::INVALID_MANIFEST, "`cheby.toml` is not valid UTF-8")
                    .with_primary(span(file, at..at.saturating_add(3)), "")
                    .with_help("save the file as UTF-8"),
            );
            return Ok(None);
        }
        Ok(Self::parse(file, &text, diagnostics))
    }

    /// Checks the manifest `text` of `file`. See [`Manifest::load`].
    #[must_use]
    pub fn parse(file: FileId, text: &str, diagnostics: &mut Vec<Diagnostic>) -> Option<Self> {
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
        Reader { file, diagnostics }.manifest(table.get_ref())
    }

    /// The module path of the root module, which is the package name
    /// (D-142).
    #[must_use]
    pub fn root_module(&self) -> crate::ModulePath {
        crate::ModulePath::root(&self.name)
    }
}

impl Version {
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

/// The span of a byte range of `cheby.toml`.
fn span(file: FileId, range: Range<usize>) -> Span {
    let size = |n: usize| TextSize::try_from(n).unwrap_or(TextSize::MAX);
    Span::new(file, TextRange::new(size(range.start), size(range.end)))
}

/// Checks a parsed manifest and reports into `diagnostics`.
struct Reader<'d> {
    file: FileId,
    diagnostics: &'d mut Vec<Diagnostic>,
}

/// The units of `[const-eval] time-limit`, with their factors to
/// milliseconds (D-536).
const TIME_UNITS: &[(&str, u64)] = &[("ms", 1), ("s", 1000)];

/// The units of `[const-eval] memory-limit`, with their factors to bytes
/// (D-536).
const MEMORY_UNITS: &[(&str, u64)] = &[("KiB", 1 << 10), ("MiB", 1 << 20), ("GiB", 1 << 30)];

/// The top-level keys, in the order §13.3 lists them, for help lines.
const KNOWN_KEYS: &str =
    "`name`, `version`, `description`, `licences`, `targets`, `[dependencies]` and `[const-eval]`";

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
        let mut description = None;
        let mut licences = Vec::new();
        let mut dependencies = Vec::new();
        let mut dependency_keys = Vec::new();
        let mut time_limit = Manifest::DEFAULT_TIME_LIMIT;
        let mut memory_limit = Manifest::DEFAULT_MEMORY_LIMIT;
        for (key, value) in table {
            match key.get_ref().as_ref() {
                "name" => name = self.name(value),
                "version" => version = self.version(value),
                "targets" => targets = self.targets(value).unwrap_or(Targets::ALL),
                "description" => description = self.string(value, "`description`"),
                "licences" => licences = self.licences(value),
                "const-eval" => self.const_eval(value, &mut time_limit, &mut memory_limit),
                "dependencies" => {
                    (dependencies, dependency_keys) = self.dependencies(key, value);
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
        Some(Manifest {
            name: name?,
            version: version?,
            targets,
            description,
            licences,
            dependencies,
            const_eval_time_limit: time_limit,
            const_eval_memory_limit: memory_limit,
            file: self.file,
        })
    }

    /// Reads `[const-eval]` (D-536, GAP(const-eval-limit-zero)): sets the
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
        match limit {
            Some(0) => {
                let diag = self.diag(
                    format!("{what} must be greater than zero"),
                    value.span(),
                    "",
                );
                self.push(diag);
                None
            }
            Some(limit) => Some(limit),
            None => {
                let diag = self.diag(format!("{what} `{text}` is too large"), value.span(), "");
                self.push(diag);
                None
            }
        }
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

    /// Reads every dependency, then reports the table, which is not
    /// supported before M5 (§13.4). Also returns every key with its span,
    /// valid entry or not, for [`Self::dependency_keys`].
    fn dependencies(
        &mut self,
        key: &Spanned<Cow<'_, str>>,
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
        if !table.is_empty() {
            let diag = self
                .diag("dependencies are not supported before M5", key.span(), "")
                .with_note("this toolchain builds only packages without dependencies");
            self.push(diag);
        }
        (deps, keys)
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
        let mut git = None;
        let mut reference = None;
        for (k, v) in table {
            let field = k.get_ref().as_ref();
            match field {
                "git" => git = self.string(v, "`git`"),
                "tag" | "rev" => {
                    let Some(text) = self.string(v, &format!("`{field}`")) else {
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
                    reference = Some(if field == "tag" {
                        GitReference::Tag(text)
                    } else {
                        GitReference::Rev(text)
                    });
                }
                other => {
                    let diag = self
                        .diag(
                            format!("unknown key `{other}` in dependency `{name}`"),
                            k.span(),
                            "",
                        )
                        .with_help("a dependency has `git` and either `tag` or `rev`");
                    self.push(diag);
                    ok = false;
                }
            }
        }
        let missing = if git.is_none() {
            "`git`"
        } else {
            "`tag` or `rev`"
        };
        let (Some(git), Some(reference)) = (git, reference) else {
            let diag = self
                .diag(
                    format!("dependency `{name}` has no {missing}"),
                    key.span(),
                    "",
                )
                .with_help("write `{ git = \"https://…\", tag = \"v1.0.0\" }`");
            self.push(diag);
            return None;
        };
        ok.then(|| Dependency {
            key: name.to_owned(),
            git,
            reference,
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
