//! The golden tests under `tests/lsp/` (`tests/lsp/README.md`).
//!
//! A test is one `.cheby` file, copied into a package as `src/main.cheby`,
//! or a directory with its own `cheby.toml`, copied as it is. Its answers
//! are in `<name>.expected` beside the file or the directory.
//!
//! The header, leading `// @key: value` lines of the file or of the
//! package's `src/main.cheby`, takes these keys:
//!
//! - `@decisions: D-NNN, …`: the decisions the test pins, for `cargo xtask
//!   coverage`;
//! - `@milestone: M<n>`: the milestone in which it must pass, M1 by default
//!   (D-1352, D-288);
//! - `@client: empty|standard|full`: the client capabilities
//!   ([`ClientKind`]), `standard` by default;
//! - `@encoding: utf-8|utf-16|utf-32`: the only position encoding the
//!   client offers;
//! - `@settings: {…}`: the `cheby` settings object, sent as
//!   `initializationOptions` and as the answer to `workspace/configuration`
//!   (D-1376);
//! - `@requests: name, …`: document requests sent for each source file,
//!   such as `documentSymbol` or `formatting`;
//! - `@package: none`: the file is opened outside every package (D-1371);
//!   `@package: std-checkout`: in a copy of this repository's `std/`,
//!   checked as package `std` (D-1699, D-1700), with `@probes` and
//!   `@open` ([`Checkout`]);
//! - `@clientInfo: name`: the `clientInfo.name` of `initialize`, such as
//!   `Zed`, whose `filterText` after `xs.` differs (`features.md` §4.2);
//! - `@newErrors: E0044, …`: the error codes an applied edit may add, one
//!   occurrence per listing (D-1676).
//!
//! A marker is a comment line that points at the nearest line above it
//! that is not a marker: `//   ^ hover` puts the position at the caret's
//! column, and `^~~~` makes a range of the caret and the tildes. A marker
//! `//> L:C name args` names the position by the compiler's 1-based line
//! and code-point column instead, for a column that a caret cannot reach,
//! and `//> L:C-L:C name args` a range, which may span lines.
//! Sources stay valid Cheby either way.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::client::{ClientKind, codes};
use super::text::{Encoding, Position};
use crate::milestone::Milestone;

/// The name of the package a single-file test is copied into.
pub const PACKAGE_NAME: &str = "lsp_test";

/// The directory of the golden tests, below the repository root.
pub const DIR: &str = "tests/lsp";

/// The extension of a golden.
pub const GOLDEN_EXTENSION: &str = "expected";

/// The directives of a test's header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The decisions the test pins.
    pub decisions: BTreeSet<u32>,
    /// The milestone in which the test must pass, M1 by default (D-1352).
    pub milestone: Milestone,
    /// The client capabilities.
    pub client: ClientKind,
    /// The only position encoding the client offers, if any.
    pub encoding: Option<Encoding>,
    /// The `cheby` settings object, if any.
    pub settings: Option<Value>,
    /// Document requests sent for each source file.
    pub requests: Vec<String>,
    /// `@package: none`: the file is opened alone, in a directory with no
    /// `cheby.toml` above it, so the server serves it in single-file mode
    /// (D-1371).
    pub loose: bool,
    /// `@package: std-checkout`: the test runs in a copy of this
    /// repository's `std/`, which the server checks as package `std`
    /// (D-1699, D-1700). `None` for every other test.
    pub checkout: Option<Checkout>,
    /// `@clientInfo`: the client's name in `initialize`, if not the
    /// harness's own.
    pub client_name: Option<String>,
    /// `@steps`: what the harness does after the opens, in order, before
    /// it sends the markers' requests.
    pub steps: Vec<Step>,
    /// `@rewrite: bom`: every source file of the copy starts with a BOM.
    pub bom: bool,
    /// `@rewrite: crlf`: every source file of the copy ends its lines with
    /// `\r\n`, which no file here can hold (`.gitattributes`).
    pub crlf: bool,
    /// `@newErrors`: the codes of the errors an applied edit may add, each
    /// listing allowing one occurrence (D-1676).
    pub new_errors: Vec<String>,
}

/// One step of `@steps`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `idle`: wait for a `cheby/idle` of the package sent after the last
    /// message read, so nothing is running or due, the child included
    /// (D-1415, D-1443).
    Idle,
    /// `started`: wait for the `$/progress` begin of a child run.
    Started,
    /// `quiet`: as `idle`, and the trial fails if a child run began since
    /// the last `cheby/idle` the client read (D-1493).
    Quiet,
    /// `settings NAME`: the settings object in `NAME.json` at the
    /// package's root is pushed with `workspace/didChangeConfiguration`,
    /// and becomes the client's answer to `workspace/configuration`.
    Settings(String),
    /// `edit DIR`: each file under `DIR/` of the test becomes the text of
    /// the open document at the same path below the package, which stays
    /// unsaved, and its publish is waited for.
    Edit(String),
    /// `save DIR`: as `edit`, and the file is also written to disk and
    /// saved with `didSave`.
    Save(String),
    /// `zed-save DIR`: as `save`, in the order Zed 1.27 sends a save
    /// (CP2's R3, D-1494): the file is written, then a watched change of
    /// it, `didSave` a few milliseconds later, and `didSave` again.
    ZedSave(String),
    /// `disk DIR`: each file under `DIR/` of the test is written to the
    /// same path below the package, and the server is told nothing, so a
    /// closed file goes stale or a new module file appears unseen
    /// (D-1669).
    Disk(String),
    /// `close PATH`: the open document at `PATH` below the package is
    /// closed, and its closed-file publish is waited for.
    Close(String),
    /// `willRename OLD NEW …`: `workspace/willRenameFiles` for each pair
    /// of paths below the package, a file or a folder. Its answer is a
    /// block, its edit is applied and checked (D-1676), and then the files
    /// move on disk, as Zed moves them.
    WillRename(Vec<(String, String)>),
    /// `status`: a block of the packages that `cheby/status` lists, each
    /// by its root, kind and `stdDiffers` (D-1705), and of every
    /// `window/showMessage` the server has sent since it started.
    Status,
    /// `published`: a block of the last published list of every file with
    /// a non-empty one at this point, as the `diagnostics` block shows them
    /// at the end.
    Published,
}

/// How a std checkout test places its probes (`tests/lsp/README.md`,
/// "Std checkout tests"): modules of the test's own, at
/// `std/probe_<name>.cheby`, which hold every marker and edit, since the
/// copied real modules have none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probes {
    /// `open`: never written to disk, each an unsaved buffer that routes
    /// (D-1707), so the disk matches the binary's std and D-1705's message
    /// is not sent. Every probe must then be opened.
    Open,
    /// `disk`: written before the server starts, so a probe can be a
    /// closed module. The disk then differs from the binary's module set,
    /// so D-1705's message is sent.
    Disk,
    /// `none`: the test's one file only carries the header and is not
    /// placed, for a test that opens real modules only.
    None,
}

impl Probes {
    /// Parses `@probes`'s value.
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "open" => Ok(Self::Open),
            "disk" => Ok(Self::Disk),
            "none" => Ok(Self::None),
            _ => Err(format!("@probes: `{value}` is not open, disk or none")),
        }
    }
}

/// `@package: std-checkout`'s directives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    /// `@probes`: how the probes are placed.
    pub probes: Probes,
    /// `@open`: the documents opened, in order, as paths below the
    /// checkout such as `std/list.cheby`: real modules and probes alike.
    /// Nothing else is opened, so a probe not named is a closed module.
    pub open: Vec<String>,
}

impl Checkout {
    /// Parses `@open`'s value: `.cheby` paths below `std/`, each once.
    fn parse_open(value: &str) -> Result<Vec<String>, String> {
        let mut paths: Vec<String> = Vec::new();
        for path in value.split(',').map(str::trim) {
            if !is_std_path(path) {
                return Err(format!(
                    "@open: `{path}` is not a `.cheby` file below `std/`, written with `/`"
                ));
            }
            if paths.iter().any(|p| p == path) {
                return Err(format!("@open: `{path}` is named twice"));
            }
            paths.push(path.to_owned());
        }
        Ok(paths)
    }

    /// The checkout of a header with `@package: std-checkout` when
    /// `std_checkout`, and its `@probes` and `@open`, both needed then
    /// and refused otherwise.
    fn of(
        std_checkout: bool,
        probes: Option<Probes>,
        open: Option<Vec<String>>,
    ) -> Result<Option<Self>, String> {
        match (std_checkout, probes, open) {
            (false, None, None) => Ok(None),
            (false, _, _) => Err("@probes and @open need `@package: std-checkout`".to_owned()),
            (true, None, _) => {
                Err("`@package: std-checkout` needs @probes: open, disk or none".to_owned())
            }
            // The session starts at the first open document (D-1704).
            (true, Some(_), None) => Err("`@package: std-checkout` needs @open, since a \
                                          checkout session starts at its first open document \
                                          (D-1704)"
                .to_owned()),
            (true, Some(probes), Some(open)) => Ok(Some(Self { probes, open })),
        }
    }
}

impl Step {
    /// Whether the step waits for `cheby/idle` or a child run, which the
    /// constants child, never run on Windows until M6 (D-1414), can hold
    /// back.
    #[must_use]
    pub const fn waits_for_child(&self) -> bool {
        matches!(self, Self::Idle | Self::Started | Self::Quiet)
    }

    /// Parses one step.
    fn parse(text: &str) -> Result<Self, String> {
        let mut words = text.split_whitespace();
        let step = match (words.next(), words.next()) {
            (Some("idle"), None) => Self::Idle,
            (Some("started"), None) => Self::Started,
            (Some("quiet"), None) => Self::Quiet,
            (Some("status"), None) => Self::Status,
            (Some("published"), None) => Self::Published,
            (Some("edit"), Some(dir)) => Self::Edit(dir.to_owned()),
            (Some("save"), Some(dir)) => Self::Save(dir.to_owned()),
            (Some("zed-save"), Some(dir)) => Self::ZedSave(dir.to_owned()),
            (Some("settings"), Some(name)) => Self::Settings(name.to_owned()),
            (Some("disk"), Some(dir)) => Self::Disk(dir.to_owned()),
            (Some("close"), Some(path)) => Self::Close(path.to_owned()),
            (Some("willRename"), Some(first)) => {
                let paths: Vec<&str> = std::iter::once(first).chain(words.by_ref()).collect();
                let pairs: Vec<(String, String)> = paths
                    .chunks(2)
                    .filter_map(|pair| match pair {
                        [old, new] => Some(((*old).to_owned(), (*new).to_owned())),
                        _ => None,
                    })
                    .collect();
                if pairs.len().saturating_mul(2) != paths.len() {
                    return Err(format!("@steps: `{text}` needs OLD NEW pairs"));
                }
                Self::WillRename(pairs)
            }
            _ => {
                return Err(format!(
                    "@steps: `{text}` is not idle, started, quiet, status, published, edit DIR, \
                     save DIR, zed-save DIR, settings NAME, disk DIR, close PATH or willRename \
                     OLD NEW …"
                ));
            }
        };
        match words.next() {
            Some(extra) => Err(format!("@steps: `{extra}` after `{text}`")),
            None => Ok(step),
        }
    }
}

impl Default for Header {
    fn default() -> Self {
        Self {
            decisions: BTreeSet::new(),
            milestone: Milestone::M1,
            client: ClientKind::Standard,
            encoding: None,
            settings: None,
            requests: Vec::new(),
            loose: false,
            checkout: None,
            client_name: None,
            steps: Vec::new(),
            bom: false,
            crlf: false,
            new_errors: Vec::new(),
        }
    }
}

impl Header {
    /// Parses the leading `// @key: value` lines of `source`.
    ///
    /// # Errors
    ///
    /// Returns a message for a malformed, repeated or unknown directive.
    pub fn of(source: &str) -> Result<Self, String> {
        let mut header = Self::default();
        let mut seen = BTreeSet::new();
        let mut std_checkout = false;
        let mut probes = None;
        let mut open = None;
        for line in source.lines() {
            let Some(directive) = line.strip_prefix("// @") else {
                break;
            };
            let (key, value) = directive
                .split_once(':')
                .ok_or_else(|| format!("directive `{line}` has no `:`"))?;
            let (key, value) = (key.trim(), value.trim());
            if !seen.insert(key.to_owned()) {
                return Err(format!("`@{key}` is given twice"));
            }
            match key {
                "decisions" => {
                    for d in value.split(',') {
                        header.decisions.insert(crate::decisions::parse(d)?);
                    }
                }
                "milestone" => {
                    header.milestone = value.parse().map_err(|e| format!("@milestone: {e}"))?;
                }
                "client" => {
                    header.client = ClientKind::parse(value).ok_or_else(|| {
                        format!("@client: `{value}` is not empty, standard or full")
                    })?;
                }
                "encoding" => {
                    header.encoding = Some(Encoding::parse(value).ok_or_else(|| {
                        format!("@encoding: `{value}` is not utf-8, utf-16 or utf-32")
                    })?);
                }
                "settings" => {
                    let settings: Value = serde_json::from_str(value)
                        .map_err(|e| format!("@settings: not JSON: {e}"))?;
                    header.settings = Some(settings);
                }
                "requests" => {
                    for name in value.split(',').map(str::trim) {
                        document_method(name)
                            .ok_or_else(|| format!("@requests: unknown request `{name}`"))?;
                        header.requests.push(name.to_owned());
                    }
                }
                "package" => match value {
                    "none" => header.loose = true,
                    "std-checkout" => std_checkout = true,
                    _ => return Err(format!("@package: `{value}` is not none or std-checkout")),
                },
                "probes" => probes = Some(Probes::parse(value)?),
                "open" => open = Some(Checkout::parse_open(value)?),
                "clientInfo" => {
                    if value.is_empty() {
                        return Err("@clientInfo: the name is empty".to_owned());
                    }
                    header.client_name = Some(value.to_owned());
                }
                "steps" => {
                    for step in value.split(',') {
                        header.steps.push(Step::parse(step)?);
                    }
                }
                "rewrite" => {
                    for form in value.split(',').map(str::trim) {
                        match form {
                            "bom" => header.bom = true,
                            "crlf" => header.crlf = true,
                            _ => return Err(format!("@rewrite: `{form}` is not bom or crlf")),
                        }
                    }
                }
                "newErrors" => {
                    for code in value.split(',').map(str::trim) {
                        if !is_code(code) {
                            return Err(format!(
                                "@newErrors: `{code}` is not an error's code such as E0044"
                            ));
                        }
                        header.new_errors.push(code.to_owned());
                    }
                }
                other => {
                    return Err(format!(
                        "unknown directive `@{other}`; the keys are decisions, milestone, \
                         client, encoding, settings, requests, package, probes, open, \
                         clientInfo, steps, rewrite and newErrors"
                    ));
                }
            }
        }
        header.checkout = Checkout::of(std_checkout, probes, open)?;
        if header.checkout.is_some() && (header.bom || header.crlf) {
            return Err(
                "@rewrite would rewrite the copied std, so a std checkout takes none".to_owned(),
            );
        }
        Ok(header)
    }

    /// Whether the trial is ignored on a platform where the harness skips
    /// runs (`runs_skipped`, Windows until M6): a step waits for the
    /// constants child, which the server never runs there (D-545, D-1414).
    /// A std checkout's session never starts the child (D-1700), so its
    /// `cheby/idle` comes on every platform and its test is never ignored.
    #[must_use]
    pub fn ignored_on(&self, runs_skipped: bool) -> bool {
        runs_skipped && self.checkout.is_none() && self.steps.iter().any(Step::waits_for_child)
    }
}

/// Whether `path` names a `.cheby` file below `std/`, relative and
/// written with `/`, with no empty, `.` or `..` segment: a module of a
/// std checkout or a probe ([`is_probe`]).
#[must_use]
pub fn is_std_path(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    segments.len() >= 2
        && segments.first() == Some(&"std")
        && Path::new(path).extension().is_some_and(|e| e == "cheby")
        && segments
            .iter()
            .all(|s| !s.is_empty() && *s != "." && *s != ".." && !s.contains('\\'))
}

/// Whether `path`, below the checkout, names a probe: a `.cheby` file
/// below `std/` whose name starts with `probe_`. No real module of std
/// has such a name, so a probe never replaces one.
#[must_use]
pub fn is_probe(path: &str) -> bool {
    is_std_path(path)
        && path
            .rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with(PROBE_PREFIX))
}

/// The start of a probe's file name.
pub const PROBE_PREFIX: &str = "probe_";

/// The file of a checkout that holds std, which marks it as one (D-1699),
/// as path components below the checkout.
pub const CHECKOUT_MARKER: [&str; 4] = ["crates", "cheby_driver", "src", "std_lib.rs"];

/// Whether `code` reads as an error's code: `E` and four digits. The
/// applied-edit check compares errors only (D-1676), so a warning's code
/// would excuse nothing.
fn is_code(code: &str) -> bool {
    let mut chars = code.chars();
    chars.next() == Some('E') && code.len() == 5 && chars.all(|c| c.is_ascii_digit())
}

/// How a test is laid out on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// One `.cheby` file, copied into a package as `src/main.cheby`.
    File,
    /// A directory with `cheby.toml` and `src/**`, copied as it is.
    Package,
    /// A std checkout test (`@package: std-checkout`): a directory with no
    /// `cheby.toml` whose `std/` holds its probes, each placed at the same
    /// path in the copy. Its other entries, such as the files of an `edit`
    /// step's `DIR/`, are copied beside `std/`. The header is in the
    /// probe whose path sorts first.
    Checkout,
}

/// One golden test.
#[derive(Debug, Clone)]
pub struct LspTest {
    /// The path relative to `tests/lsp`, with `/` separators and no
    /// extension.
    pub name: String,
    /// The `.cheby` file or the package directory.
    pub path: PathBuf,
    /// How it is laid out.
    pub layout: Layout,
    /// Its header, or why it does not parse, which fails the trial and
    /// not the whole harness.
    pub header: Result<Header, String>,
}

impl LspTest {
    /// The golden beside the file or the directory.
    #[must_use]
    pub fn golden(&self) -> PathBuf {
        match self.layout {
            Layout::File => self.path.with_extension(GOLDEN_EXTENSION),
            Layout::Package | Layout::Checkout => {
                let mut name = self.path.file_name().unwrap_or_default().to_os_string();
                name.push(".");
                name.push(GOLDEN_EXTENSION);
                self.path.with_file_name(name)
            }
        }
    }

    /// The file that holds the header.
    #[must_use]
    pub fn header_file(&self) -> PathBuf {
        match self.layout {
            Layout::File => self.path.clone(),
            Layout::Package => self.path.join("src/main.cheby"),
            Layout::Checkout => self
                .probe_files()
                .ok()
                .and_then(|probes| probes.into_iter().next())
                .map_or_else(|| self.path.join("std"), |(_, source)| source),
        }
    }

    /// A [`Layout::Checkout`] test's files under its `std/`, by path
    /// below the test with `/`, sorted, and their sources.
    fn probe_files(&self) -> Result<Vec<(String, PathBuf)>, String> {
        let found = crate::files_with_extension(&self.path.join("std"), "cheby")
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        let mut probes: Vec<(String, PathBuf)> = found
            .into_iter()
            .map(|source| (crate::display_relative(&source, &self.path), source))
            .collect();
        probes.sort();
        Ok(probes)
    }

    /// The probes of a std checkout test with `header`, by path below the
    /// checkout, and their sources: every file under a
    /// [`Layout::Checkout`] test's `std/`, or a one-file test as
    /// `std/probe_<name>.cheby` unless `@probes: none`. Each must be a
    /// probe ([`is_probe`]).
    ///
    /// # Errors
    ///
    /// Returns a message for a test that is not a std checkout, a
    /// directory that cannot be read, or a file that is not a probe.
    pub fn probes(&self, header: &Header) -> Result<Vec<(String, PathBuf)>, String> {
        let checkout = header
            .checkout
            .as_ref()
            .ok_or("a test with probes needs `@package: std-checkout`")?;
        let probes = match (self.layout, checkout.probes) {
            (Layout::Package, _) => {
                return Err("`@package: std-checkout` needs a test with no `cheby.toml`".to_owned());
            }
            (Layout::File, Probes::None) => Vec::new(),
            (Layout::File, Probes::Open | Probes::Disk) => {
                let stem = self
                    .path
                    .file_stem()
                    .ok_or_else(|| format!("{}: no file name", self.path.display()))?
                    .to_string_lossy();
                vec![(format!("std/{PROBE_PREFIX}{stem}.cheby"), self.path.clone())]
            }
            (Layout::Checkout, Probes::None) => {
                return Err(
                    "a test with a `std/` has probes, so `@probes: none` is wrong".to_owned(),
                );
            }
            (Layout::Checkout, Probes::Open | Probes::Disk) => self.probe_files()?,
        };
        if let Some((path, _)) = probes.iter().find(|(path, _)| !is_probe(path)) {
            return Err(format!(
                "`{path}` is not a probe: a probe's name starts with `{PROBE_PREFIX}`, so it \
                 never replaces a module of std"
            ));
        }
        Ok(probes)
    }
}

/// Every golden test, sorted by name, each with its header or why it
/// does not parse.
///
/// # Errors
///
/// Returns a message if a directory cannot be read.
pub fn all(root: &Path) -> Result<Vec<LspTest>, String> {
    let dir = root.join(DIR);
    let mut found = Vec::new();
    if dir.exists() {
        discover(&dir, &mut found).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let mut tests = Vec::with_capacity(found.len());
    for (path, layout) in found {
        let name = path
            .strip_prefix(&dir)
            .unwrap_or(&path)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let mut test = LspTest {
            name,
            path,
            layout,
            header: Ok(Header::default()),
        };
        let file = test.header_file();
        test.header = crate::golden::read(&file)
            .and_then(|text| text.ok_or_else(|| "the file is missing".to_owned()))
            .and_then(|text| Header::of(&text))
            .map_err(|e| format!("{}: {e}", file.display()));
        tests.push(test);
    }
    Ok(tests)
}

fn discover(dir: &Path, out: &mut Vec<(PathBuf, Layout)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if entry.file_type()?.is_dir() {
            if path.join("cheby.toml").is_file() {
                out.push((path, Layout::Package));
            } else if path.join("std").is_dir() {
                out.push((path, Layout::Checkout));
            } else {
                discover(&path, out)?;
            }
        } else if path.extension().is_some_and(|e| e == "cheby") {
            out.push((path, Layout::File));
        }
    }
    Ok(())
}

/// `path`, relative with `/`, below `base`, built from its components.
fn below(base: &Path, path: &str) -> PathBuf {
    path.split('/')
        .fold(base.to_path_buf(), |at, part| at.join(part))
}

/// Copies the directory `from` into `to`, skipping hidden entries.
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let entries = std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", from.display()))?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let (source, target) = (entry.path(), to.join(&name));
        if source.is_dir() {
            copy_tree(&source, &target)?;
        } else {
            std::fs::copy(&source, &target).map_err(|e| format!("{}: {e}", source.display()))?;
        }
    }
    Ok(())
}

/// Copies the `std/` of the repository at `repo` into the directory `to`,
/// beside an empty `crates/cheby_driver/src/std_lib.rs`, so the server
/// takes `to` for a std checkout (D-1699), as Zed's workspace folder.
/// The paths are built from components, never joined with `/`.
///
/// # Errors
///
/// Returns a message if `to` or a directory above it holds a
/// `cheby.toml`, which would make `std/` part of a package, or a file
/// cannot be copied.
pub fn copy_std(repo: &Path, to: &Path) -> Result<(), String> {
    if let Some(manifest) = to
        .ancestors()
        .map(|dir| dir.join("cheby.toml"))
        .find(|manifest| manifest.is_file())
    {
        return Err(format!(
            "{}: a std checkout has no `cheby.toml` above it (D-1699)",
            manifest.display()
        ));
    }
    copy_tree(&repo.join("std"), &to.join("std"))?;
    let marker = CHECKOUT_MARKER
        .iter()
        .fold(to.to_path_buf(), |at, part| at.join(part));
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(&marker, "").map_err(|e| format!("{}: {e}", marker.display()))
}

/// Places the std checkout of `test`, whose header is `header`, in the
/// empty directory `to`: the `std/` of the repository at `repo` through
/// [`copy_std`], a [`Layout::Checkout`] test's entries other than
/// `std/` beside it, and the probes as `@probes` says. Returns the texts
/// of the probes that are only opened, by path below the checkout, which
/// are never on disk.
///
/// # Errors
///
/// Returns a message for a test that is not a std checkout, a probe that
/// would replace a module of std, an open-only probe that `@open` does
/// not name, which the server would never see, an `@open` path that is
/// neither a module of std nor a probe, or a file that cannot be copied.
pub fn place_checkout(
    repo: &Path,
    test: &LspTest,
    header: &Header,
    to: &Path,
) -> Result<BTreeMap<String, String>, String> {
    let checkout = header
        .checkout
        .as_ref()
        .ok_or("a std checkout test needs `@package: std-checkout`")?;
    let probes = test.probes(header)?;
    copy_std(repo, to)?;
    if test.layout == Layout::Checkout {
        let entries =
            std::fs::read_dir(&test.path).map_err(|e| format!("{}: {e}", test.path.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", test.path.display()))?;
            let name = entry.file_name();
            let shown = name.to_string_lossy();
            if shown.starts_with('.') || shown == "std" {
                continue;
            }
            let (source, target) = (entry.path(), to.join(&name));
            if source.is_dir() {
                copy_tree(&source, &target)?;
            } else {
                std::fs::copy(&source, &target)
                    .map_err(|e| format!("{}: {e}", source.display()))?;
            }
        }
    }
    let mut unsaved = BTreeMap::new();
    for (path, source) in probes {
        let target = below(to, &path);
        if target.exists() {
            return Err(format!("the probe `{path}` would replace a module of std"));
        }
        let probe_text =
            std::fs::read_to_string(&source).map_err(|e| format!("{}: {e}", source.display()))?;
        match checkout.probes {
            Probes::Open => {
                if !checkout.open.contains(&path) {
                    return Err(format!(
                        "`{path}` is placed by `@probes: open` but `@open` does not name it, \
                         so the server would never see it"
                    ));
                }
                unsaved.insert(path, probe_text);
            }
            Probes::Disk => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("{}: {e}", parent.display()))?;
                }
                std::fs::write(&target, probe_text)
                    .map_err(|e| format!("{}: {e}", target.display()))?;
            }
            // [`LspTest::probes`] gives none.
            Probes::None => {}
        }
    }
    if let Some(path) = checkout
        .open
        .iter()
        .find(|path| !unsaved.contains_key(*path) && !below(to, path).is_file())
    {
        return Err(format!(
            "@open: `{path}` is neither a module of std nor a probe of the test"
        ));
    }
    Ok(unsaved)
}

/// One marker of a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// The 0-based line it points at.
    pub line: usize,
    /// The 0-based code-point column of the position.
    pub column: usize,
    /// The end column of a range marker, `^~~`, exclusive, on `line`.
    pub end: Option<usize>,
    /// The 0-based end line of a `//> L:C-L:C` range, which may differ
    /// from `line`; `None` for a range on one line or no range.
    pub end_line: Option<usize>,
    /// The request's short name, such as `hover`.
    pub name: String,
    /// The words after the name, such as a rename's new name.
    pub args: Vec<String>,
}

impl Marker {
    /// The label of the marker's block in a golden: the compiler's 1-based
    /// line and code-point column and the request.
    #[must_use]
    pub fn label(&self, file: &str) -> String {
        let mut label = format!(
            "{file}:{}:{} {}",
            self.line.saturating_add(1),
            self.column.saturating_add(1),
            self.name
        );
        for arg in &self.args {
            label.push(' ');
            label.push_str(arg);
        }
        label
    }
}

/// The markers of `text`, in order.
///
/// # Errors
///
/// Returns a message for a marker with no line above it, no request
/// name, an unknown request, or a bad `//>` position.
pub fn markers(text: &str) -> Result<Vec<Marker>, String> {
    let mut out = Vec::new();
    let mut target = None;
    for (index, line) in text.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let number = index.saturating_add(1);
        let trimmed = line.trim_start();
        let Some(comment) = trimmed.strip_prefix("//") else {
            target = Some(index);
            continue;
        };
        let (position, end_line, rest) = if let Some(explicit) = comment.strip_prefix("> ") {
            let (at, rest) = explicit
                .trim_start()
                .split_once(' ')
                .unwrap_or((explicit, ""));
            let (l, c) = at
                .split_once(':')
                .ok_or_else(|| format!("line {number}: `{at}` is not L:C"))?;
            let parse = |s: &str| {
                s.parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .ok_or_else(|| format!("line {number}: `{at}` is not a 1-based L:C"))
            };
            // `L:C-L:C` names a range, its end exclusive.
            let (c, end) = match c.split_once('-') {
                Some((c, end)) => {
                    let (el, ec) = end
                        .split_once(':')
                        .ok_or_else(|| format!("line {number}: `{at}` is not L:C-L:C"))?;
                    (c, Some((parse(el)?, parse(ec)?)))
                }
                None => (c, None),
            };
            let (l, c) = (parse(l)?, parse(c)?);
            if end.is_some_and(|e| e < (l, c)) {
                return Err(format!("line {number}: `{at}` ends before it starts"));
            }
            match end {
                Some((el, ec)) if el == l => ((l, c, Some(ec)), None, rest.to_owned()),
                Some((el, ec)) => ((l, c, Some(ec)), Some(el), rest.to_owned()),
                None => ((l, c, None), None, rest.to_owned()),
            }
        } else if comment.trim_start().starts_with('^') {
            let Some(line_above) = target else {
                return Err(format!("line {number}: a marker with no line above it"));
            };
            let caret = line
                .chars()
                .position(|c| c == '^')
                .ok_or_else(|| format!("line {number}: no caret"))?;
            let after: String = line.chars().skip(caret.saturating_add(1)).collect();
            let tildes = after.chars().take_while(|&c| c == '~').count();
            let end = (tildes > 0).then(|| caret.saturating_add(1).saturating_add(tildes));
            let rest: String = after.chars().skip(tildes).collect();
            ((line_above, caret, end), None, rest)
        } else {
            // An ordinary comment: a marker below it still points past it.
            continue;
        };
        let mut words = rest.split_whitespace();
        let name = words
            .next()
            .ok_or_else(|| format!("line {number}: a marker with no request"))?;
        position_method(name).ok_or_else(|| format!("line {number}: unknown request `{name}`"))?;
        let (line, column, end) = position;
        out.push(Marker {
            line,
            column,
            end,
            end_line,
            name: name.to_owned(),
            args: words.map(str::to_owned).collect(),
        });
    }
    Ok(out)
}

/// The method of a request that takes a position or a range, by short
/// name.
#[must_use]
pub fn position_method(name: &str) -> Option<&'static str> {
    Some(match name {
        "hover" => "textDocument/hover",
        "definition" => "textDocument/definition",
        "declaration" => "textDocument/declaration",
        "typeDefinition" => "textDocument/typeDefinition",
        "implementation" => "textDocument/implementation",
        "references" => "textDocument/references",
        "documentHighlight" => "textDocument/documentHighlight",
        // `apply` asks for completion and applies the item it names.
        "completion" | "apply" => "textDocument/completion",
        "signatureHelp" => "textDocument/signatureHelp",
        "prepareRename" => "textDocument/prepareRename",
        "rename" => "textDocument/rename",
        "codeAction" => "textDocument/codeAction",
        // The follow-ups start with a `prepareCallHierarchy` at the marker
        // ([`hierarchy_follow_up`]).
        "prepareCallHierarchy" | "incomingCalls" | "outgoingCalls" => {
            "textDocument/prepareCallHierarchy"
        }
        // The marker's position is not sent, only its words as the query.
        "workspaceSymbol" => "workspace/symbol",
        "selectionRange" => "textDocument/selectionRange",
        "rangeFormatting" => "textDocument/rangeFormatting",
        "inlayHint" => "textDocument/inlayHint",
        _ => return None,
    })
}

/// The method of a request about a whole document, by short name.
#[must_use]
pub fn document_method(name: &str) -> Option<&'static str> {
    Some(match name {
        "documentSymbol" => "textDocument/documentSymbol",
        "formatting" => "textDocument/formatting",
        "foldingRange" => "textDocument/foldingRange",
        "documentLink" => "textDocument/documentLink",
        "codeLens" => "textDocument/codeLens",
        "semanticTokens" => "textDocument/semanticTokens/full",
        "semanticTokensRange" => "textDocument/semanticTokens/range",
        "inlayHint" => "textDocument/inlayHint",
        _ => return None,
    })
}

/// The method that follows a `prepareCallHierarchy` for marker `name`,
/// `incomingCalls` or `outgoingCalls`, sent with each item it prepared.
#[must_use]
pub fn hierarchy_follow_up(name: &str) -> Option<&'static str> {
    match name {
        "incomingCalls" => Some("callHierarchy/incomingCalls"),
        "outgoingCalls" => Some("callHierarchy/outgoingCalls"),
        _ => None,
    }
}

/// The byte ranges of every identifier and type name token of `text`,
/// in order: the positions the examples group sends its requests at.
#[must_use]
pub fn identifiers(text: &str) -> Vec<(usize, usize)> {
    use cheby_syntax::SyntaxKind;
    let lexed = cheby_syntax::lexer::lex(cheby_diag::FileId::new(0), text);
    (0..lexed.len())
        .filter(|&i| {
            matches!(
                lexed.kind(i),
                SyntaxKind::LOWER_IDENT | SyntaxKind::UPPER_IDENT
            )
        })
        .filter_map(|i| {
            let range = lexed.range(i);
            let start = usize::try_from(range.start()).ok()?;
            let end = usize::try_from(range.end()).ok()?;
            Some((start, end))
        })
        .collect()
}

/// The requests a server advertises in its `capabilities`, by short name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Advertised {
    /// Requests at a position or a range ([`position_method`]), in a fixed
    /// order. `rename` is not among them: it follows a `prepareRename`.
    pub position: Vec<&'static str>,
    /// Requests about a whole document ([`document_method`]).
    pub document: Vec<&'static str>,
    /// Whether `rename` is advertised.
    pub rename: bool,
    /// Whether `workspace/symbol` is advertised.
    pub workspace_symbol: bool,
    /// `completionProvider.triggerCharacters`.
    pub completion_triggers: Vec<String>,
    /// Whether `completionProvider.resolveProvider` is `true`.
    pub completion_resolve: bool,
}

impl Advertised {
    /// Reads `capabilities`. A provider is advertised when its key is
    /// present and neither `false` nor `null`.
    ///
    /// # Errors
    ///
    /// Returns a message for an advertised `…Provider` key this table does
    /// not know, so the examples group never skips a request in silence:
    /// a wave that advertises one adds it here.
    pub fn of(capabilities: &Value) -> Result<Self, String> {
        let has = |key: &str| {
            capabilities
                .get(key)
                .is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
        };
        let position_table = [
            ("hoverProvider", "hover"),
            ("definitionProvider", "definition"),
            ("declarationProvider", "declaration"),
            ("typeDefinitionProvider", "typeDefinition"),
            ("implementationProvider", "implementation"),
            ("referencesProvider", "references"),
            ("documentHighlightProvider", "documentHighlight"),
            ("completionProvider", "completion"),
            ("signatureHelpProvider", "signatureHelp"),
            ("codeActionProvider", "codeAction"),
            ("callHierarchyProvider", "prepareCallHierarchy"),
            ("selectionRangeProvider", "selectionRange"),
            ("documentRangeFormattingProvider", "rangeFormatting"),
        ];
        let document_table = [
            ("documentSymbolProvider", "documentSymbol"),
            ("documentFormattingProvider", "formatting"),
            ("foldingRangeProvider", "foldingRange"),
            ("documentLinkProvider", "documentLink"),
            ("codeLensProvider", "codeLens"),
            ("inlayHintProvider", "inlayHint"),
        ];
        // `semanticTokensProvider` names its requests: `full` and `range`.
        let tokens = |key: &str| {
            capabilities
                .get("semanticTokensProvider")
                .and_then(|p| p.get(key))
                .is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
        };
        let unknown: Vec<&str> = capabilities
            .as_object()
            .into_iter()
            .flat_map(|map| map.keys())
            .map(String::as_str)
            .filter(|key| key.ends_with("Provider") && has(key))
            .filter(|key| {
                !matches!(
                    *key,
                    "renameProvider" | "workspaceSymbolProvider" | "semanticTokensProvider"
                ) && !position_table.iter().any(|(k, _)| k == key)
                    && !document_table.iter().any(|(k, _)| k == key)
            })
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "the server advertises {}, which `Advertised::of` does not know",
                unknown.join(", ")
            ));
        }
        let mut position: Vec<&'static str> = position_table
            .iter()
            .filter(|(key, _)| has(key))
            .map(|&(_, name)| name)
            .collect();
        let rename = has("renameProvider");
        let prepare = capabilities
            .get("renameProvider")
            .and_then(|r| r.get("prepareProvider"))
            .and_then(Value::as_bool)
            == Some(true);
        if prepare {
            position.push("prepareRename");
        }
        let mut document: Vec<&'static str> = document_table
            .iter()
            .filter(|(key, _)| has(key))
            .map(|&(_, name)| name)
            .collect();
        if tokens("full") {
            document.push("semanticTokens");
        }
        if tokens("range") {
            document.push("semanticTokensRange");
        }
        let completion = capabilities.get("completionProvider");
        let completion_triggers = completion
            .and_then(|c| c.get("triggerCharacters"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        let completion_resolve = completion
            .and_then(|c| c.get("resolveProvider"))
            .and_then(Value::as_bool)
            == Some(true);
        Ok(Self {
            position,
            document,
            rename,
            workspace_symbol: has("workspaceSymbolProvider"),
            completion_triggers,
            completion_resolve,
        })
    }
}

/// Formatting options every formatting request sends.
#[must_use]
pub fn formatting_options() -> Value {
    json!({ "tabSize": 2, "insertSpaces": true })
}

/// The params of position request `name` at `start`, or at the range
/// `start..end`, of `uri`. `diagnostics` are those published for the
/// file, of which a code action gets the ones its range touches.
///
/// # Errors
///
/// Returns a message for an unknown request or missing arguments.
pub fn position_params(
    name: &str,
    uri: &str,
    start: Position,
    end: Option<Position>,
    args: &[String],
    diagnostics: &[Value],
) -> Result<(&'static str, Value), String> {
    let method = position_method(name).ok_or_else(|| format!("unknown request `{name}`"))?;
    let document = json!({ "uri": uri });
    let range = json!({ "start": start.to_json(), "end": end.unwrap_or(start).to_json() });
    let params = match name {
        "references" => json!({
            "textDocument": document,
            "position": start.to_json(),
            "context": { "includeDeclaration": !args.iter().any(|a| a == "noDeclaration") },
        }),
        "rename" => {
            let new_name = args.first().ok_or("`rename` needs the new name after it")?;
            json!({ "textDocument": document, "position": start.to_json(), "newName": new_name })
        }
        "codeAction" => {
            let touching: Vec<Value> = diagnostics
                .iter()
                .filter(|d| touches(d, start, end.unwrap_or(start)))
                .cloned()
                .collect();
            let mut context = json!({ "diagnostics": touching });
            if let (Some(only), Some(map)) = (args.first(), context.as_object_mut()) {
                map.insert("only".to_owned(), json!([only]));
            }
            json!({ "textDocument": document, "range": range, "context": context })
        }
        "selectionRange" => json!({ "textDocument": document, "positions": [start.to_json()] }),
        "workspaceSymbol" => json!({ "query": args.join(" ") }),
        "rangeFormatting" => json!({
            "textDocument": document,
            "range": range,
            "options": formatting_options(),
        }),
        "completion" | "apply" => json!({
            "textDocument": document,
            "position": start.to_json(),
            "context": { "triggerKind": 1 },
        }),
        "inlayHint" => json!({ "textDocument": document, "range": range }),
        _ => json!({ "textDocument": document, "position": start.to_json() }),
    };
    Ok((method, params))
}

/// The params of document request `name` of `uri`, whose last line is
/// `last` (for ranged requests over the whole text).
///
/// # Errors
///
/// Returns a message for an unknown request.
pub fn document_params(
    name: &str,
    uri: &str,
    last: Position,
) -> Result<(&'static str, Value), String> {
    let method = document_method(name).ok_or_else(|| format!("unknown request `{name}`"))?;
    let document = json!({ "uri": uri });
    let params = match name {
        "formatting" => json!({ "textDocument": document, "options": formatting_options() }),
        "inlayHint" | "semanticTokensRange" => json!({
            "textDocument": document,
            "range": { "start": { "line": 0, "character": 0 }, "end": last.to_json() },
        }),
        _ => json!({ "textDocument": document }),
    };
    Ok((method, params))
}

/// Whether the range of diagnostic `d` touches `start..end`.
fn touches(d: &Value, start: Position, end: Position) -> bool {
    let range = d.get("range").unwrap_or(&Value::Null);
    let (Ok(a), Ok(b)) = (
        Position::from_json(range.get("start").unwrap_or(&Value::Null)),
        Position::from_json(range.get("end").unwrap_or(&Value::Null)),
    ) else {
        return false;
    };
    a <= end && start <= b
}

/// Sorts what a request answers where the order means nothing, so goldens
/// do not depend on it: locations of `references` by path, line and
/// column; the edits of a `rename` or a `willRename` step's
/// `WorkspaceEdit`, its `changes` and each document of its
/// `documentChanges` by range, and those documents by URI; and the ranges
/// of `foldingRange` by start, then end.
#[must_use]
pub fn canonical(name: &str, answer: Value) -> Value {
    fn fold_key(v: &Value) -> [u64; 4] {
        ["startLine", "startCharacter", "endLine", "endCharacter"]
            .map(|f| v.get(f).and_then(Value::as_u64).unwrap_or(0))
    }
    fn key(v: &Value) -> (String, u64, u64) {
        let uri = v
            .get("uri")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let start = v
            .get("range")
            .and_then(|r| r.get("start"))
            .unwrap_or(&Value::Null);
        let n = |f: &str| start.get(f).and_then(Value::as_u64).unwrap_or(0);
        (uri, n("line"), n("character"))
    }
    /// A text edit's range, start then end.
    fn range_key(v: &Value) -> [u64; 4] {
        let range = v.get("range").unwrap_or(&Value::Null);
        let n = |end: &str, f: &str| {
            range
                .get(end)
                .and_then(|p| p.get(f))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        [
            n("start", "line"),
            n("start", "character"),
            n("end", "line"),
            n("end", "character"),
        ]
    }
    /// The URI a document change names: a text document edit's, or a
    /// resource operation's `uri` or `oldUri`.
    fn document_key(v: &Value) -> String {
        v.pointer("/textDocument/uri")
            .or_else(|| v.get("uri"))
            .or_else(|| v.get("oldUri"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }
    match (name, answer) {
        ("references", Value::Array(mut items)) => {
            items.sort_by_key(key);
            Value::Array(items)
        }
        ("foldingRange", Value::Array(mut items)) => {
            items.sort_by_key(fold_key);
            Value::Array(items)
        }
        ("rename" | "willRename", Value::Object(mut edit)) => {
            if let Some(Value::Object(changes)) = edit.get_mut("changes") {
                // Each URI's list is sorted on its own; the map's order is the
                // printer's.
                for edits in changes.values_mut() {
                    if let Value::Array(items) = edits {
                        items.sort_by_key(range_key);
                    }
                }
            }
            if let Some(Value::Array(documents)) = edit.get_mut("documentChanges") {
                for document in documents.iter_mut() {
                    if let Some(Value::Array(items)) = document.get_mut("edits") {
                        items.sort_by_key(range_key);
                    }
                }
                documents.sort_by_key(document_key);
            }
            Value::Object(edit)
        }
        (_, answer) => answer,
    }
}

/// Whether the examples group takes error `code` as an answer to
/// `method`: `RequestFailed` from `textDocument/prepareRename` and
/// `textDocument/rename`, which is how they refuse a name (`features.md`
/// §3.2, D-1671). Any other error fails the trial, `InternalError`
/// always.
#[must_use]
pub fn refusal_allowed(method: &str, code: i64) -> bool {
    code == codes::REQUEST_FAILED
        && matches!(method, "textDocument/prepareRename" | "textDocument/rename")
}

/// Whether a rename sweep (D-1677) takes the refusal `message` of a
/// rename to a fresh name. Such a name binds nothing, so a refusal says the
/// writer missed an occurrence, which is what the sweep exists to find,
/// except for three kinds: a function whose satisfaction the package
/// uses, which `verify` refuses for its E0063 in D-1671's text (D-1391);
/// and, only in an example whose `cheby check` fails before any rename
/// (`checks` false), D-1390's state gates, whose texts never start with
/// "Renaming would", and D-1698's refusal of an error that would go away,
/// since a fresh name can remove an error the example already has
/// (D-1668, D-1716). Any other refusal fails the trial.
#[must_use]
pub fn sweep_refusal_allowed(message: &str, checks: bool) -> bool {
    message.starts_with("Renaming would cause: error[E0063]: ")
        || (!checks
            && (message.starts_with("Renaming would hide an error")
                || (!message.starts_with("Renaming would ")
                    && !message.starts_with("the edit could not be checked"))))
}

/// The points of `points`, each a label of its `prepareRename` request
/// and the point, where `prepared`, the answers by label, holds an
/// answer that is not `null`: a refused or failed request has none. At
/// most `budget` of them are kept, by the fixed stride of
/// [`super::editing::sample`], so the renames are the same on every run.
/// Returns them and how many accepted points were dropped.
#[must_use]
pub fn accepted_renames<T: Clone>(
    points: &[(String, T)],
    prepared: &BTreeMap<String, Value>,
    budget: usize,
) -> (Vec<T>, usize) {
    let accepted: Vec<T> = points
        .iter()
        .filter(|(label, _)| prepared.get(label).is_some_and(|v| !v.is_null()))
        .map(|(_, point)| point.clone())
        .collect();
    super::editing::sample(&accepted, budget)
}

/// One block of a golden: `== <label>` and pretty JSON.
#[must_use]
pub fn block(label: &str, value: &Value) -> String {
    let body = serde_json::to_string_pretty(value).unwrap_or_else(|e| format!("<{e}>"));
    format!("== {label}\n{body}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers() {
        let header = Header::of(
            "// @decisions: D-1352\n// @client: empty\n// @encoding: utf-32\n\
             // @settings: {\"analysis\": {\"debounceMs\": 0}}\n\
             // @requests: documentSymbol, formatting\nfn main() {}\n",
        )
        .unwrap();
        assert_eq!(header.decisions, BTreeSet::from([1352]));
        assert_eq!(header.milestone, Milestone::M1);
        assert_eq!(header.client, ClientKind::Empty);
        assert_eq!(header.encoding, Some(Encoding::Utf32));
        assert_eq!(
            header.settings,
            Some(json!({"analysis": {"debounceMs": 0}}))
        );
        assert_eq!(header.requests, ["documentSymbol", "formatting"]);
        assert!(!header.loose);
        assert!(Header::of("// @package: none\n").unwrap().loose);
        assert_eq!(Header::of("fn main() {}\n").unwrap(), Header::default());
        let steps = Header::of(
            "// @steps: idle, edit dirty, save next, started, settings raise, quiet, zed-save z\n\
                 // @rewrite: crlf\n",
        )
        .unwrap();
        assert_eq!(
            steps.steps,
            [
                Step::Idle,
                Step::Edit("dirty".to_owned()),
                Step::Save("next".to_owned()),
                Step::Started,
                Step::Settings("raise".to_owned()),
                Step::Quiet,
                Step::ZedSave("z".to_owned()),
            ]
        );
        assert!(steps.crlf && !steps.bom);
        let moves = Header::of(
            "// @steps: disk late, close src/a.cheby, willRename src/a.cheby src/b.cheby src/c src/d\n\
             // @newErrors: E0044, E0044\n",
        )
        .unwrap();
        assert_eq!(
            moves.steps,
            [
                Step::Disk("late".to_owned()),
                Step::Close("src/a.cheby".to_owned()),
                Step::WillRename(vec![
                    ("src/a.cheby".to_owned(), "src/b.cheby".to_owned()),
                    ("src/c".to_owned(), "src/d".to_owned()),
                ]),
            ]
        );
        assert_eq!(moves.new_errors, ["E0044", "E0044"]);
        for bad in [
            "// @steps: disk\n",
            "// @steps: close\n",
            "// @steps: close a b\n",
            "// @steps: willRename\n",
            "// @steps: willRename src/a.cheby\n",
            "// @steps: willRename a b c\n",
            "// @newErrors: E44\n",
            "// @newErrors: X0044\n",
            "// @newErrors: E00440\n",
            "// @newErrors: W0007\n",
            "// @steps: wait\n",
            "// @steps: save\n",
            "// @steps: idle now\n",
            "// @steps: settings\n",
            "// @steps: quiet now\n",
            "// @rewrite: utf-16\n",
            "// @mode: check-pass\n",
            "// @client: odd\n",
            "// @encoding: utf-7\n",
            "// @settings: {\n",
            "// @requests: hover\n",
            "// @package: app\n",
            "// @milestone: M2\n// @milestone: M3\n",
            "// @decisions D-1\n",
        ] {
            assert!(Header::of(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn caret_markers() {
        let text = "fn main() {\n  let total = add(1, 2)\n  //          ^ hover\n  // a note\n  //    ^~~~ codeAction quickfix\n}\n";
        let found = markers(text).unwrap();
        assert_eq!(
            found,
            [
                Marker {
                    line: 1,
                    column: 14,
                    end: None,
                    end_line: None,
                    name: "hover".to_owned(),
                    args: vec![],
                },
                Marker {
                    line: 1,
                    column: 8,
                    end: Some(12),
                    end_line: None,
                    name: "codeAction".to_owned(),
                    args: vec!["quickfix".to_owned()],
                },
            ]
        );
        assert_eq!(
            found[0].label("src/main.cheby"),
            "src/main.cheby:2:15 hover"
        );
    }

    #[test]
    fn explicit_markers() {
        let found = markers("fn main() {}\n//> 1:1 rename start\n").unwrap();
        assert_eq!(found[0].line, 0);
        assert_eq!(found[0].column, 0);
        assert_eq!(found[0].args, ["start"]);
        assert!(markers("//> 0:1 hover\n").is_err());
        assert!(markers("//> 1 hover\n").is_err());
        // A range, on one line or across lines.
        let found = markers("a\nb\n//> 1:2-1:4 inlayHint\n//> 1:1-3:1 inlayHint\n").unwrap();
        assert_eq!(
            (found[0].line, found[0].column, found[0].end),
            (0, 1, Some(3))
        );
        assert_eq!(found[0].end_line, None);
        assert_eq!((found[1].end, found[1].end_line), (Some(0), Some(2)));
        assert!(markers("//> 2:1-1:1 inlayHint\n").is_err());
        assert!(markers("//> 1:1-2 inlayHint\n").is_err());
    }

    #[test]
    fn editing_markers() {
        let found = markers("x.\n//^ apply `let guard`\n// ^ inlayHint\n").unwrap();
        assert_eq!(
            position_method(&found[0].name),
            Some("textDocument/completion")
        );
        let at = Position {
            line: 0,
            character: 2,
        };
        let end = Position {
            line: 3,
            character: 0,
        };
        let (method, params) =
            position_params("inlayHint", "file:///a", at, Some(end), &[], &[]).unwrap();
        assert_eq!(method, "textDocument/inlayHint");
        assert_eq!(params["range"]["end"], json!({ "line": 3, "character": 0 }));
        let (_, params) = position_params("apply", "file:///a", at, None, &[], &[]).unwrap();
        assert_eq!(params["context"], json!({ "triggerKind": 1 }));
        let header = Header::of("// @clientInfo: Zed\n").unwrap();
        assert_eq!(header.client_name.as_deref(), Some("Zed"));
        assert!(Header::of("// @clientInfo:\n").is_err());
    }

    #[test]
    fn bad_markers() {
        assert!(markers("// ^ hover\n").is_err());
        assert!(markers("x\n// ^\n").is_err());
        assert!(markers("x\n// ^ frobnicate\n").is_err());
    }

    #[test]
    fn identifier_tokens() {
        let text = "fn main() {\n  let total = io::x(1)\n}\n";
        let words: Vec<&str> = identifiers(text)
            .into_iter()
            .map(|(a, b)| text.get(a..b).unwrap())
            .collect();
        assert_eq!(words, ["main", "total", "io", "x"]);
    }

    #[test]
    fn advertised_requests() {
        let caps = json!({
            "hoverProvider": true,
            "definitionProvider": false,
            "renameProvider": {"prepareProvider": true},
            "documentFormattingProvider": {},
            "textDocumentSync": 2,
        });
        let found = Advertised::of(&caps).unwrap();
        assert_eq!(found.position, ["hover", "prepareRename"]);
        assert_eq!(found.document, ["formatting"]);
        assert!(found.rename);
        assert_eq!(Advertised::of(&json!({})), Ok(Advertised::default()));
        let ranged = Advertised::of(&json!({ "documentRangeFormattingProvider": true })).unwrap();
        assert_eq!(ranged.position, ["rangeFormatting"]);
        // An unknown provider fails, unless it is not advertised.
        assert!(Advertised::of(&json!({ "monikerProvider": true })).is_err());
        assert!(Advertised::of(&json!({ "monikerProvider": false })).is_ok());
        let completion = Advertised::of(&json!({
            "completionProvider": { "triggerCharacters": [".", ">"], "resolveProvider": true },
        }))
        .unwrap();
        assert_eq!(completion.position, ["completion"]);
        assert_eq!(completion.completion_triggers, [".", ">"]);
        assert!(completion.completion_resolve);
        let workspace = Advertised::of(&json!({ "workspaceSymbolProvider": true })).unwrap();
        assert!(workspace.workspace_symbol);
        let tokens =
            |provider: Value| Advertised::of(&json!({ "semanticTokensProvider": provider }));
        let both = tokens(json!({ "legend": {}, "full": true, "range": true })).unwrap();
        assert_eq!(both.document, ["semanticTokens", "semanticTokensRange"]);
        let full = tokens(json!({ "legend": {}, "full": { "delta": false } })).unwrap();
        assert_eq!(full.document, ["semanticTokens"]);
        assert_eq!(
            tokens(json!({ "legend": {} })).unwrap().document,
            Vec::<&str>::new()
        );
    }

    #[test]
    fn hierarchy_and_workspace_markers() {
        let found = markers("fn f() {}\n// ^ incomingCalls\n// ^ workspaceSymbol ma in\n").unwrap();
        assert_eq!(
            hierarchy_follow_up(&found[0].name),
            Some("callHierarchy/incomingCalls")
        );
        assert_eq!(hierarchy_follow_up("prepareCallHierarchy"), None);
        let at = Position {
            line: 0,
            character: 3,
        };
        let (method, params) = position_params(
            "workspaceSymbol",
            "file:///a",
            at,
            None,
            &found[1].args,
            &[],
        )
        .unwrap();
        assert_eq!(method, "workspace/symbol");
        assert_eq!(params, json!({ "query": "ma in" }));
    }

    #[test]
    fn params_and_sorting() {
        let at = Position {
            line: 1,
            character: 2,
        };
        let (method, params) =
            position_params("rename", "file:///a", at, None, &["b".to_owned()], &[]).unwrap();
        assert_eq!(method, "textDocument/rename");
        assert_eq!(params["newName"], json!("b"));
        assert!(position_params("rename", "file:///a", at, None, &[], &[]).is_err());
        let d = json!({"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 5}}});
        let far = json!({"range": {"start": {"line": 9, "character": 0}, "end": {"line": 9, "character": 1}}});
        let (_, params) =
            position_params("codeAction", "file:///a", at, None, &[], &[d, far]).unwrap();
        assert_eq!(
            params["context"]["diagnostics"].as_array().map(Vec::len),
            Some(1)
        );
        let refs = json!([
            {"uri": "file:///b", "range": {"start": {"line": 0, "character": 0}}},
            {"uri": "file:///a", "range": {"start": {"line": 3, "character": 1}}},
            {"uri": "file:///a", "range": {"start": {"line": 1, "character": 9}}},
        ]);
        let sorted = canonical("references", refs);
        assert_eq!(sorted[0]["range"]["start"]["line"], json!(1));
        assert_eq!(sorted[2]["uri"], json!("file:///b"));
        let folds = json!([
            {"startLine": 4, "endLine": 9},
            {"startLine": 0, "startCharacter": 3, "endLine": 9},
            {"startLine": 0, "endLine": 1, "kind": "comment"},
        ]);
        let sorted = canonical("foldingRange", folds);
        assert_eq!(sorted[0]["kind"], json!("comment"));
        assert_eq!(sorted[2]["startLine"], json!(4));
    }

    /// A text edit from `(l, c)` to `(l, e)`.
    fn text_edit(l: u64, c: u64, e: u64) -> Value {
        json!({
            "range": {"start": {"line": l, "character": c}, "end": {"line": l, "character": e}},
            "newText": "x",
        })
    }

    #[test]
    fn rename_edits_are_sorted() {
        let changes = json!({"changes": {"file:///a": [text_edit(3, 0, 1), text_edit(1, 4, 5), text_edit(1, 0, 2)]}});
        let sorted = canonical("rename", changes);
        let starts: Vec<(Value, Value)> = sorted["changes"]["file:///a"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["range"]["start"]["line"].clone(),
                    e["range"]["start"]["character"].clone(),
                )
            })
            .collect();
        assert_eq!(
            starts,
            [
                (json!(1), json!(0)),
                (json!(1), json!(4)),
                (json!(3), json!(0))
            ]
        );
        // `documentChanges`: the documents by URI, each one's edits by range,
        // an end breaking a tie of starts.
        let documents = json!({"documentChanges": [
            {"textDocument": {"uri": "file:///b", "version": 2}, "edits": [text_edit(2, 0, 3), text_edit(2, 0, 1)]},
            {"textDocument": {"uri": "file:///a", "version": null}, "edits": [text_edit(5, 0, 1), text_edit(0, 7, 8)]},
        ]});
        for name in ["rename", "willRename"] {
            let sorted = canonical(name, documents.clone());
            let changes = sorted["documentChanges"].as_array().unwrap();
            assert_eq!(changes[0]["textDocument"]["uri"], json!("file:///a"));
            assert_eq!(changes[0]["edits"][0]["range"]["start"]["line"], json!(0));
            assert_eq!(
                changes[1]["edits"][0]["range"]["end"]["character"],
                json!(1)
            );
            assert_eq!(
                changes[1]["edits"][1]["range"]["end"]["character"],
                json!(3)
            );
        }
        // Another request's answer keeps its order.
        assert_eq!(canonical("hover", documents.clone()), documents);
    }

    #[test]
    fn only_rename_refusals_are_answers() {
        for method in ["textDocument/prepareRename", "textDocument/rename"] {
            assert!(refusal_allowed(method, codes::REQUEST_FAILED));
            assert!(!refusal_allowed(method, codes::INTERNAL_ERROR));
            assert!(!refusal_allowed(method, codes::INVALID_PARAMS));
        }
        assert!(!refusal_allowed(
            "textDocument/hover",
            codes::REQUEST_FAILED
        ));
        assert!(!refusal_allowed(
            "workspace/willRenameFiles",
            codes::REQUEST_FAILED
        ));
    }

    /// D-1677: the sweeps fail on a refusal that only a missed occurrence
    /// explains. An error that would go away is one the example already
    /// has, so only an example that does not check may refuse for it
    /// (D-1668, D-1716).
    #[test]
    fn a_sweep_takes_only_the_refusals_a_fresh_name_can_get() {
        let satisfier = "Renaming would cause: error[E0063]: `Point` does not satisfy `Show` \
                         at src/main.cheby:3:1";
        let gate = "cannot tell what `.code` at `src/main.cheby:4` refers to";
        let hides = "Renaming would hide an error: error[E0044]: `stored` is the name of a \
                     function in this module at src/routes.cheby:217:27";
        for checks in [true, false] {
            assert!(sweep_refusal_allowed(satisfier, checks));
            for missed in [
                "Renaming would cause: error[E0042]: cannot find value `secret` in this scope \
                 at src/main.cheby:60:41",
                "Renaming would cause: warning[W0007]: `Some` hides the prelude's `Some`",
                "the edit could not be checked: verify: the edits of `src/main.cheby` do not fit it",
            ] {
                assert!(!sweep_refusal_allowed(missed, checks), "{missed}");
            }
        }
        for allowed in [gate, hides] {
            assert!(sweep_refusal_allowed(allowed, false), "{allowed}");
            assert!(!sweep_refusal_allowed(allowed, true), "{allowed}");
        }
    }

    #[test]
    fn renames_where_prepare_accepted_within_a_budget() {
        let points: Vec<(String, usize)> = (0..250).map(|i| (format!("p{i}"), i)).collect();
        let mut prepared = BTreeMap::new();
        for i in 0..250_usize {
            // Every third point is refused (no answer), every fifth `null`.
            if i % 3 == 0 {
                continue;
            }
            let answer = if i % 5 == 0 {
                Value::Null
            } else {
                json!({"start": i})
            };
            prepared.insert(format!("p{i}"), answer);
        }
        let accepted = (0..250_usize).filter(|i| i % 3 != 0 && i % 5 != 0).count();
        let (kept, dropped) = accepted_renames(&points, &prepared, 100);
        assert!(kept.len() <= 100, "{}", kept.len());
        assert_eq!(kept.len() + dropped, accepted);
        assert!(kept.iter().all(|i| i % 3 != 0 && i % 5 != 0));
        // The same on every run, and from the first accepted point.
        assert_eq!(accepted_renames(&points, &prepared, 100).0, kept);
        assert_eq!(kept.first(), Some(&1));
        // Under the budget, every accepted point is kept.
        let (all, none) = accepted_renames(&points, &prepared, 1_000);
        assert_eq!((all.len(), none), (accepted, 0));
    }

    /// The header of a std checkout test: its three keys, and the
    /// combinations that are refused.
    #[test]
    fn checkout_headers() {
        let header = Header::of(
            "// @package: std-checkout\n// @probes: disk\n\
             // @open: std/probe_a.cheby, std/json/tree.cheby\n// @steps: idle, status, published\n",
        )
        .unwrap();
        assert_eq!(
            header.checkout,
            Some(Checkout {
                probes: Probes::Disk,
                open: vec![
                    "std/probe_a.cheby".to_owned(),
                    "std/json/tree.cheby".to_owned()
                ],
            })
        );
        assert!(!header.loose);
        assert_eq!(header.steps, [Step::Idle, Step::Status, Step::Published]);
        let none =
            Header::of("// @package: std-checkout\n// @probes: none\n// @open: std/list.cheby\n")
                .unwrap();
        assert_eq!(none.checkout.map(|c| c.probes), Some(Probes::None));
        let open = "// @open: std/list.cheby\n";
        for bad in [
            // No @probes, no @open, or either without the package.
            "// @package: std-checkout\n// @open: std/list.cheby\n".to_owned(),
            "// @package: std-checkout\n// @probes: open\n".to_owned(),
            "// @probes: open\n".to_owned(),
            open.to_owned(),
            "// @package: none\n// @probes: open\n// @open: std/list.cheby\n".to_owned(),
            format!("// @package: std-checkout\n// @probes: closed\n{open}"),
            format!("// @package: std-checkout\n// @probes: open\n{open}// @rewrite: bom\n"),
            "// @package: std-checkout\n// @probes: open\n// @open: src/main.cheby\n".to_owned(),
            "// @package: std-checkout\n// @probes: open\n// @open: std/list\n".to_owned(),
            "// @package: std-checkout\n// @probes: open\n// @open: std/../list.cheby\n".to_owned(),
            "// @package: std-checkout\n// @probes: open\n// @open: std\\list.cheby\n".to_owned(),
            "// @package: std-checkout\n// @probes: open\n// @open: std/a.cheby, std/a.cheby\n"
                .to_owned(),
            "// @package: checkout\n".to_owned(),
        ] {
            assert!(Header::of(&bad).is_err(), "{bad}");
        }
        assert!(is_probe("std/probe_a.cheby"));
        assert!(is_probe("std/json/probe_a.cheby"));
        assert!(!is_probe("std/list.cheby"));
        assert!(!is_probe("v2/std/probe_a.cheby"));
        assert!(!is_probe("std/probe_a.txt"));
    }

    /// Windows ignores a golden that waits for the child (D-1414), but
    /// never a std checkout's, whose session starts no child (D-1700).
    #[test]
    fn checkout_goldens_are_not_ignored_for_idle() {
        let checkout = Header::of(
            "// @package: std-checkout\n// @probes: none\n// @open: std/list.cheby\n\
             // @steps: idle, quiet\n",
        )
        .unwrap();
        assert!(!checkout.ignored_on(true));
        assert!(!checkout.ignored_on(false));
        let package = Header::of("// @steps: idle\n").unwrap();
        assert!(package.ignored_on(true));
        assert!(!package.ignored_on(false));
        for step in ["started", "quiet"] {
            assert!(
                Header::of(&format!("// @steps: {step}\n"))
                    .unwrap()
                    .ignored_on(true)
            );
        }
        assert!(!Header::of("// @steps: status\n").unwrap().ignored_on(true));
    }

    /// A repository with a small `std/` and a `tests/lsp/` beside it.
    fn scratch_repo(prefix: &str) -> crate::cache::FreshDir {
        let repo = crate::cache::FreshDir::create(prefix).unwrap();
        for (path, text) in [
            ("std/list.cheby", "//! list\n"),
            ("std/json/tree.cheby", "//! tree\n"),
            (
                "std/probe_old.cheby",
                "//! a module that looks like a probe\n",
            ),
        ] {
            let path = below(repo.path(), path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        repo
    }

    /// A one-file test at `path` of `repo` with `text`.
    fn one_file(repo: &Path, name: &str, body: &str) -> (LspTest, Header) {
        let path = repo.join(format!("{name}.cheby"));
        std::fs::write(&path, body).unwrap();
        let header = Header::of(body).unwrap();
        let test = LspTest {
            name: name.to_owned(),
            path,
            layout: Layout::File,
            header: Ok(header.clone()),
        };
        (test, header)
    }

    /// The relative paths and texts of every file below `dir`.
    fn tree_of(dir: &Path) -> BTreeMap<String, String> {
        crate::files_with_extension(dir, "cheby")
            .unwrap()
            .into_iter()
            .chain(crate::files_with_extension(dir, "rs").unwrap())
            .map(|path| {
                let text = std::fs::read_to_string(&path).unwrap();
                (crate::display_relative(&path, dir), text)
            })
            .collect()
    }

    /// The copy holds `std/` as it is, beside an empty `std_lib.rs` at the
    /// path D-1699 tests, and nothing else. An open-only probe is never on
    /// disk; an on-disk probe is.
    #[test]
    fn checkout_copy_places_probes_both_ways() {
        let repo = scratch_repo("cheby-lsp-checkout-copy");
        let probe = "//! a probe\nimport std::list\n";
        for (probes, on_disk) in [("open", false), ("disk", true)] {
            let body = format!(
                "// @package: std-checkout\n// @probes: {probes}\n\
                 // @open: std/probe_typed.cheby, std/list.cheby\n{probe}"
            );
            let (test, header) = one_file(repo.path(), "typed", &body);
            let to = crate::cache::FreshDir::create("cheby-lsp-checkout-to").unwrap();
            let unsaved = place_checkout(repo.path(), &test, &header, to.path()).unwrap();
            let mut expected = BTreeMap::from([
                (
                    "crates/cheby_driver/src/std_lib.rs".to_owned(),
                    String::new(),
                ),
                ("std/list.cheby".to_owned(), "//! list\n".to_owned()),
                ("std/json/tree.cheby".to_owned(), "//! tree\n".to_owned()),
                (
                    "std/probe_old.cheby".to_owned(),
                    "//! a module that looks like a probe\n".to_owned(),
                ),
            ]);
            if on_disk {
                expected.insert("std/probe_typed.cheby".to_owned(), body);
                assert!(unsaved.is_empty(), "{unsaved:?}");
            } else {
                assert_eq!(
                    unsaved,
                    BTreeMap::from([("std/probe_typed.cheby".to_owned(), body)])
                );
            }
            assert_eq!(tree_of(to.path()), expected, "@probes: {probes}");
            assert!(!to.path().join("cheby.toml").exists());
            assert!(
                to.path()
                    .join("crates")
                    .join("cheby_driver")
                    .join("src")
                    .join("std_lib.rs")
                    .is_file()
            );
        }
        // `@probes: none`: the file only carries the header.
        let (test, header) = one_file(
            repo.path(),
            "clean",
            "// @package: std-checkout\n// @probes: none\n// @open: std/list.cheby\n",
        );
        let to = crate::cache::FreshDir::create("cheby-lsp-checkout-to").unwrap();
        assert!(
            place_checkout(repo.path(), &test, &header, to.path())
                .unwrap()
                .is_empty()
        );
        assert!(!to.path().join("std").join("probe_clean.cheby").exists());
    }

    /// A directory with a `std/` and no `cheby.toml` is a std checkout
    /// test: its header is in the probe that sorts first, its probes go to
    /// the same paths, and a step's `DIR/` is copied beside `std/`.
    #[test]
    fn checkout_directory_tests() {
        let repo = scratch_repo("cheby-lsp-checkout-dir");
        let dir = below(repo.path(), "tests/lsp/std_checkout/dependent");
        let header = "// @package: std-checkout\n// @probes: disk\n\
                      // @open: std/probe_base.cheby\n// @steps: edit v2\n";
        for (path, text) in [
            (
                "std/probe_base.cheby",
                format!("{header}pub fn base() -> Int {{\n  1\n}}\n"),
            ),
            ("std/probe_user.cheby", "//! user\n".to_owned()),
            ("v2/std/probe_base.cheby", "//! v2\n".to_owned()),
        ] {
            let path = below(&dir, path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let found = all(repo.path()).unwrap();
        let [test] = found.as_slice() else {
            panic!("{found:?}");
        };
        assert_eq!(test.name, "std_checkout/dependent");
        assert_eq!(test.layout, Layout::Checkout);
        assert_eq!(test.header_file(), below(&dir, "std/probe_base.cheby"));
        assert_eq!(
            test.golden(),
            below(repo.path(), "tests/lsp/std_checkout/dependent.expected")
        );
        let header = test.header.clone().unwrap();
        assert_eq!(header.steps, [Step::Edit("v2".to_owned())]);
        let to = crate::cache::FreshDir::create("cheby-lsp-checkout-to").unwrap();
        assert!(
            place_checkout(repo.path(), test, &header, to.path())
                .unwrap()
                .is_empty()
        );
        let placed = tree_of(to.path());
        for path in [
            "std/probe_base.cheby",
            "std/probe_user.cheby",
            "v2/std/probe_base.cheby",
        ] {
            assert_eq!(
                placed.get(path),
                std::fs::read_to_string(below(&dir, path)).ok().as_ref(),
                "{path}"
            );
        }
    }

    /// A probe that would replace a module, a file that is not a probe, an
    /// open-only probe that `@open` does not name, an `@open` path that
    /// names nothing, and a `cheby.toml` above the copy are refused.
    #[test]
    fn checkout_refusals() {
        let repo = scratch_repo("cheby-lsp-checkout-refusals");
        let refused = |name: &str, text: &str| {
            let (test, header) = one_file(repo.path(), name, text);
            let to = crate::cache::FreshDir::create("cheby-lsp-checkout-to").unwrap();
            place_checkout(repo.path(), &test, &header, to.path()).unwrap_err()
        };
        let error = refused(
            "old",
            "// @package: std-checkout\n// @probes: disk\n// @open: std/list.cheby\n",
        );
        assert!(error.contains("would replace a module of std"), "{error}");
        let error = refused(
            "unnamed",
            "// @package: std-checkout\n// @probes: open\n// @open: std/list.cheby\n",
        );
        assert!(error.contains("does not name it"), "{error}");
        let error = refused(
            "missing",
            "// @package: std-checkout\n// @probes: none\n// @open: std/gone.cheby\n",
        );
        assert!(
            error.contains("neither a module of std nor a probe"),
            "{error}"
        );
        // A directory test whose `std/` holds a real module's name.
        let dir = below(repo.path(), "tests/lsp/std_checkout/edits_list");
        std::fs::create_dir_all(dir.join("std")).unwrap();
        std::fs::write(
            dir.join("std").join("list.cheby"),
            "// @package: std-checkout\n// @probes: disk\n// @open: std/list.cheby\n",
        )
        .unwrap();
        let test = all(repo.path()).unwrap().remove(0);
        let header = test.header.clone().unwrap();
        let error = test.probes(&header).unwrap_err();
        assert!(error.contains("is not a probe"), "{error}");
        // A `cheby.toml` above the copy would make `std/` a package's.
        let parent = crate::cache::FreshDir::create("cheby-lsp-checkout-manifest").unwrap();
        std::fs::write(parent.path().join("cheby.toml"), "").unwrap();
        let to = parent.path().join("copy");
        std::fs::create_dir_all(&to).unwrap();
        let error = copy_std(repo.path(), &to).unwrap_err();
        assert!(error.contains("no `cheby.toml` above it"), "{error}");
    }
}
