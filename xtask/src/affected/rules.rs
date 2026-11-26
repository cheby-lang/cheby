//! From changed paths to the checks of CI that they can fail (D-1948).
//!
//! Each path adds to a [`Plan`] by the first rule it matches: a std
//! module, a UI, LSP or REPL test, an example, a page of the docs, a
//! workflow, a file every build reads, or a file of a workspace package. A
//! package brings its own tests and those of every package that depends on
//! it ([`Workspace::dependents`]), and of `cheby_cli` the test binaries
//! that [`CLI_TESTS`] says exercise it. A path no rule knows is listed, so
//! nothing is skipped in silence.

use std::collections::{BTreeMap, BTreeSet};

use super::graph::Workspace;

/// The compiler and the runtime: what a program goes through when `cheby`
/// checks, builds or runs it.
pub(crate) const TOOLCHAIN: &[&str] = &["cheby_driver", "cheby_runtime", "cheby_rt_exe"];

/// Each integration-test binary of `cheby_cli`, and the packages it
/// exercises beside `cheby_cli` itself: a change in one of them, or in a
/// package one of them depends on, runs the binary. A test of this file
/// keeps the table equal to the targets cargo lists.
pub(crate) const CLI_TESTS: &[(&str, &[&str])] = &[
    ("backend", &["cheby_lsp", "cheby_runtime", "cheby_rt_exe"]),
    ("blocking_io", TOOLCHAIN),
    ("build", TOOLCHAIN),
    ("build_dwarf", TOOLCHAIN),
    ("build_sweep", TOOLCHAIN),
    ("cache", TOOLCHAIN),
    ("cache_stress", TOOLCHAIN),
    ("closed_streams", TOOLCHAIN),
    ("code_cache", TOOLCHAIN),
    ("color", &["cheby_driver"]),
    ("conformance", TOOLCHAIN),
    ("const_cache", TOOLCHAIN),
    ("dependency_cache", TOOLCHAIN),
    ("doc", &["cheby_doc"]),
    ("explain", &["cheby_diag"]),
    ("fmt", &["cheby_fmt"]),
    ("format", TOOLCHAIN),
    ("git_dependencies", TOOLCHAIN),
    ("http_fixture", TOOLCHAIN),
    ("interrupt", TOOLCHAIN),
    ("lsp", &["cheby_lsp"]),
    ("native", TOOLCHAIN),
    ("new", &["cheby_driver"]),
    ("path_dependencies", TOOLCHAIN),
    ("phases", TOOLCHAIN),
    ("process", TOOLCHAIN),
    ("process_exit", TOOLCHAIN),
    ("profile_differential", TOOLCHAIN),
    ("rc_differential", TOOLCHAIN),
    ("repl", &["cheby_repl"]),
    ("repl_backtrace", &["cheby_repl"]),
    ("repl_runtime", &["cheby_repl"]),
    ("run", TOOLCHAIN),
    ("single", TOOLCHAIN),
    ("sql_fixture", TOOLCHAIN),
    ("std_cache", TOOLCHAIN),
    ("temp_root", TOOLCHAIN),
    ("test_parallel", TOOLCHAIN),
    ("tls", TOOLCHAIN),
    ("ui", TOOLCHAIN),
];

/// The harness directories under `tests/` and the binary each is the
/// source of.
const HARNESSES: &[(&str, &str)] = &[
    ("tests/conformance/", "conformance"),
    ("tests/lsp_harness/", "lsp"),
    ("tests/repl_harness/", "repl"),
    ("tests/ui_harness/", "ui"),
];

/// The files every build or every test run reads.
const WHOLE: &[&str] = &[
    ".cargo/config.toml",
    ".config/nextest.toml",
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
];

/// Past this many tests of one binary, the filter names the binary.
const MAX_TESTS: usize = 40;

/// Past this many std modules that import a changed one, every module's
/// test blocks run.
const MAX_IMPORTERS: usize = 8;

/// What the checkout answers for the rules.
pub(crate) trait Lookup {
    /// Whether the file at `path`, from the repository root, exists.
    fn exists(&self, path: &str) -> bool;
    /// The files under `dir` with one of `extensions` whose text has
    /// `needle`, from the repository root.
    fn naming(&self, dir: &str, extensions: &[&str], needle: &str) -> Vec<String>;
    /// Whether the change to `path` adds or removes a line with `needle`.
    fn changed_line_has(&self, path: &str, needle: &str) -> bool;
    /// The text of every std module, by its path under `std`, such as
    /// `list` or `json::decode`.
    fn std_sources(&self) -> BTreeMap<String, String>;
}

/// The std modules that import `module`, directly or through other std
/// modules, by the `import std::…` lines of `sources`. An import names a
/// module or an item of one, so its longest part that is a module counts.
pub(crate) fn std_importers(sources: &BTreeMap<String, String>, module: &str) -> BTreeSet<String> {
    let imports_of = |text: &str| -> BTreeSet<String> {
        text.lines()
            .filter_map(|line| line.strip_prefix("import std::"))
            .filter_map(|rest| {
                let path: String = rest
                    .chars()
                    .take_while(|&c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
                    .collect();
                let mut parts: Vec<&str> =
                    path.split("::").filter(|part| !part.is_empty()).collect();
                while !parts.is_empty() && !sources.contains_key(&parts.join("::")) {
                    parts.pop();
                }
                (!parts.is_empty()).then(|| parts.join("::"))
            })
            .collect()
    };
    let imports: BTreeMap<&str, BTreeSet<String>> = sources
        .iter()
        .map(|(name, text)| (name.as_str(), imports_of(text)))
        .collect();
    let mut found: BTreeSet<String> = BTreeSet::new();
    loop {
        let more: Vec<String> = imports
            .iter()
            .filter(|&(name, _)| *name != module && !found.contains(*name))
            .filter(|(_, imported)| {
                imported.contains(module) || imported.iter().any(|name| found.contains(name))
            })
            .map(|(name, _)| (*name).to_owned())
            .collect();
        if more.is_empty() {
            return found;
        }
        found.extend(more);
    }
}

/// A check of CI, in the order the plan lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Check {
    RustFmt,
    Clippy,
    Tests,
    TestStd,
    Examples,
    ChebyFmt,
    Parsers,
    Determinism,
    Coverage,
    DecisionsLint,
    Rustdoc,
    Typos,
    Taplo,
    Actionlint,
    Deny,
}

/// One command of the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) check: Check,
    pub(crate) program: &'static str,
    pub(crate) args: Vec<String>,
    /// The environment variables it needs.
    pub(crate) env: Vec<(&'static str, &'static str)>,
}

impl Step {
    fn new(check: Check, program: &'static str, args: &[&str]) -> Self {
        Self {
            check,
            program,
            args: args.iter().map(|&arg| arg.to_owned()).collect(),
            env: Vec::new(),
        }
    }

    /// The command as a shell takes it.
    pub(crate) fn shown(&self) -> String {
        let quoted = |arg: &String| {
            let plain = arg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_=./:@+,".contains(c));
            if plain && !arg.is_empty() {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        };
        self.env
            .iter()
            .map(|(name, value)| format!("{name}='{value}'"))
            .chain(std::iter::once(self.program.to_owned()))
            .chain(self.args.iter().map(quoted))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// What a set of changed paths asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// The checks to run, each with the paths that ask for it.
    pub(crate) why: BTreeMap<Check, BTreeSet<String>>,
    /// The checks that run on everything they can: clippy on the workspace,
    /// every test, every std module, every example.
    whole: BTreeSet<Check>,
    /// The packages clippy checks, unless it checks the workspace.
    clippy: BTreeSet<String>,
    /// The packages whose every test runs.
    packages: BTreeSet<String>,
    /// A test binary or a package, as nextest's filter writes it, with the
    /// patterns of the tests to run in it; `None` for all of them.
    scopes: BTreeMap<String, Option<BTreeSet<String>>>,
    /// The std modules whose test blocks run, unless all of them do.
    std_modules: BTreeSet<String>,
    /// The `.cheby` files of std and the examples that `cheby fmt` checks.
    cheby_files: BTreeSet<String>,
    /// The examples `check-determinism` builds, unless it builds all.
    determinism: BTreeSet<String>,
    /// The paths no rule knows.
    pub(crate) unmatched: Vec<String>,
}

impl Plan {
    /// Asks for `check` on everything it can run on.
    fn ask_whole(&mut self, check: Check, path: &str) {
        self.ask(check, path);
        self.whole.insert(check);
    }

    fn ask(&mut self, check: Check, path: &str) {
        self.why.entry(check).or_default().insert(path.to_owned());
    }

    /// Every test of `scope`, a `binary_id(…)` or a `package(…)`.
    fn all_of(&mut self, path: &str, scope: String) {
        self.ask(Check::Tests, path);
        self.scopes.insert(scope, None);
    }

    /// The tests of `scope` whose names match `pattern`.
    fn tests_of(&mut self, path: &str, scope: String, pattern: String) {
        self.ask(Check::Tests, path);
        if let Some(patterns) = self
            .scopes
            .entry(scope)
            .or_insert_with(|| Some(BTreeSet::new()))
        {
            patterns.insert(pattern);
        }
    }

    fn package(&mut self, path: &str, name: &str) {
        self.ask(Check::Tests, path);
        self.packages.insert(name.to_owned());
    }

    /// The filter of `cargo nextest` for the tests, `None` for every test
    /// or for none ([`Check::Tests`] says which).
    pub(crate) fn filter(&self) -> Option<String> {
        if self.whole.contains(&Check::Tests) {
            return None;
        }
        let mut terms: Vec<String> = self
            .packages
            .iter()
            .map(|package| format!("package({package})"))
            .collect();
        for (scope, patterns) in &self.scopes {
            // `binary_id(cheby_cli::ui)` and `package(cheby_cli)` are both
            // of the package `cheby_cli`.
            let package = scope
                .split_once('(')
                .map_or("", |(_, rest)| rest)
                .split([':', ')'])
                .next()
                .unwrap_or_default();
            if self.packages.contains(package) {
                continue;
            }
            match patterns {
                Some(patterns) if patterns.len() <= MAX_TESTS => {
                    let any: Vec<&str> = patterns.iter().map(String::as_str).collect();
                    terms.push(format!("({scope} & test(/({})/))", any.join("|")));
                }
                Some(_) | None => terms.push(scope.clone()),
            }
        }
        (!terms.is_empty()).then(|| terms.join(" | "))
    }

    /// The commands, in the order CI's list has them.
    pub(crate) fn steps(&self) -> Vec<Step> {
        let mut steps = Vec::new();
        for &check in self.why.keys() {
            let whole = self.whole.contains(&check);
            let mut step = Step::new(check, check.program(), check.arguments());
            match check {
                Check::Clippy if whole => step.args.push("--workspace".to_owned()),
                Check::Clippy => {
                    for package in &self.clippy {
                        step.args.extend(["-p".to_owned(), package.clone()]);
                    }
                }
                Check::Tests => {
                    if let Some(filter) = self.filter() {
                        step.args
                            .extend(["--no-tests=pass".to_owned(), "-E".to_owned(), filter]);
                    }
                }
                Check::TestStd if !whole => {
                    // A test's full name is `std::<module> "<name>"`.
                    steps.extend(self.std_modules.iter().map(|module| {
                        let mut one = step.clone();
                        one.args.push(format!("std::{module} \""));
                        one
                    }));
                    continue;
                }
                Check::Determinism if !whole => {
                    steps.extend(self.determinism.iter().map(|example| {
                        let mut one = step.clone();
                        one.args.push(example.clone());
                        one
                    }));
                    continue;
                }
                Check::ChebyFmt => step.args.extend(self.cheby_files.iter().cloned()),
                Check::Rustdoc => step.env.push(("RUSTDOCFLAGS", "-D warnings")),
                Check::RustFmt
                | Check::TestStd
                | Check::Examples
                | Check::Parsers
                | Check::Determinism
                | Check::Coverage
                | Check::DecisionsLint
                | Check::Typos
                | Check::Taplo
                | Check::Actionlint
                | Check::Deny => {}
            }
            steps.push(step);
        }
        steps
    }
}

impl Check {
    const fn program(self) -> &'static str {
        match self {
            Self::Typos => "typos",
            Self::Taplo => "taplo",
            Self::Actionlint => "actionlint",
            Self::RustFmt
            | Self::Clippy
            | Self::Tests
            | Self::TestStd
            | Self::Examples
            | Self::ChebyFmt
            | Self::Parsers
            | Self::Determinism
            | Self::Coverage
            | Self::DecisionsLint
            | Self::Rustdoc
            | Self::Deny => "cargo",
        }
    }

    /// The arguments every command of the check starts with.
    const fn arguments(self) -> &'static [&'static str] {
        match self {
            Self::RustFmt => &["fmt", "--all", "--check"],
            Self::Clippy => &["clippy", "--locked", "--all-targets"],
            Self::Tests => &["xtask", "test"],
            Self::TestStd => &["xtask", "test-std"],
            Self::Examples => &["xtask", "check-examples"],
            Self::ChebyFmt => &[
                "run",
                "--locked",
                "-q",
                "-p",
                "cheby_cli",
                "--",
                "fmt",
                "--check",
            ],
            Self::Parsers => &["xtask", "cross-check"],
            Self::Determinism => &["xtask", "check-determinism"],
            Self::Coverage => &["xtask", "coverage"],
            Self::DecisionsLint => &["xtask", "decisions", "lint"],
            Self::Rustdoc => &[
                "doc",
                "--workspace",
                "--no-deps",
                "--document-private-items",
                "--locked",
            ],
            Self::Typos | Self::Actionlint => &[],
            Self::Taplo => &["fmt", "--check"],
            Self::Deny => &["deny", "--locked", "check"],
        }
    }
}

/// The plan for `paths`, from the repository root with `/`.
pub(crate) fn plan(paths: &[String], workspace: &Workspace, lookup: &dyn Lookup) -> Plan {
    let rules = Rules { workspace, lookup };
    let mut plan = Plan::default();
    for path in paths {
        // A wrong word or a decision that does not exist can be anywhere.
        plan.ask(Check::Typos, path);
        plan.ask(Check::DecisionsLint, path);
        if has_extension(path, "rs") {
            plan.ask(Check::RustFmt, path);
            plan.ask(Check::Rustdoc, path);
        }
        if has_extension(path, "toml") {
            plan.ask(Check::Taplo, path);
        }
        rules.apply(path, &mut plan);
    }
    plan
}

fn has_extension(path: &str, extension: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|found| found == extension)
}

/// `name` for a pattern of nextest's `test(/…/)`: what a regular
/// expression reads as a sign is escaped, and so is `/`, which ends the
/// pattern.
fn escaped(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if "\\.+*?()|[]{}^$/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

struct Rules<'a> {
    workspace: &'a Workspace,
    lookup: &'a dyn Lookup,
}

impl Rules<'_> {
    fn apply(&self, path: &str, plan: &mut Plan) {
        if let Some(module) = path
            .strip_prefix("std/")
            .and_then(|rest| rest.strip_suffix(".cheby"))
        {
            self.std_module(path, &module.replace('/', "::"), plan);
        } else if let Some(rest) = path.strip_prefix("tests/ui/") {
            if let Some(name) = self.test_name("tests/ui/", rest) {
                self.ui_test(path, &name, true, plan);
            }
            Self::test_source(path, plan);
        } else if let Some(rest) = path.strip_prefix("tests/lsp/") {
            if let Some(name) = self.test_name("tests/lsp/", rest) {
                Self::lsp_test(path, &name, plan);
            }
            plan.ask(Check::Coverage, path);
        } else if let Some(rest) = path.strip_prefix("tests/repl/") {
            let name = rest.split(['/', '.']).next().unwrap_or_default();
            if !name.is_empty() && name != "README" {
                Self::repl_test(path, name, plan);
            }
            Self::test_source(path, plan);
        } else if let Some(&(_, binary)) = HARNESSES.iter().find(|(dir, _)| path.starts_with(dir)) {
            plan.all_of(path, format!("binary_id(cheby_cli::{binary})"));
            plan.ask(Check::Clippy, path);
            plan.clippy.insert("cheby_cli".to_owned());
        } else if path == "tests/milestone" {
            for (_, binary) in HARNESSES {
                plan.all_of(path, format!("binary_id(cheby_cli::{binary})"));
            }
            plan.package(path, "cheby_test_support");
            plan.package(path, "xtask");
        } else if path == "tests/tree-sitter-cheby.rev" {
            plan.ask(Check::Parsers, path);
        } else if path.starts_with("tests/fixtures/") {
            plan.package(path, "cheby_cli");
        } else if let Some(rest) = path.strip_prefix("docs/examples/") {
            if let Some((example, _)) = rest.split_once('/') {
                self.example(path, example, true, plan);
            }
        } else if path.starts_with("docs/diagnostics/") {
            // The explanations are embedded in `cheby_diag` (D-420).
            plan.package(path, "cheby_diag");
            plan.all_of(path, "binary_id(cheby_cli::explain)".to_owned());
            plan.tests_of(
                path,
                "binary_id(cheby_cli::lsp)".to_owned(),
                "\\] golden\\/explain\\/".to_owned(),
            );
        } else if path.starts_with("docs/spec/") {
            plan.ask(Check::Coverage, path);
        } else if path.starts_with(".github/") {
            plan.ask(Check::Actionlint, path);
        } else if WHOLE.contains(&path) {
            Self::everything(path, plan);
        } else if path == "deny.toml" {
            plan.ask(Check::Deny, path);
        } else if let Some(package) = self.workspace.owner(path) {
            self.package(path, &package.name, plan);
        } else if !(has_extension(path, "md") || matches!(path, "_typos.toml" | ".taplo.toml")) {
            plan.unmatched.push(path.to_owned());
        }
    }

    /// A file all builds read: every check of the list, on everything.
    fn everything(path: &str, plan: &mut Plan) {
        for check in [
            Check::Clippy,
            Check::Tests,
            Check::TestStd,
            Check::Determinism,
        ] {
            plan.ask_whole(check, path);
        }
        for check in [Check::Examples, Check::Rustdoc] {
            plan.ask(check, path);
        }
        if path.starts_with("Cargo.") {
            plan.ask(Check::Deny, path);
        }
    }

    /// A file of the workspace package `name`.
    fn package(&self, path: &str, name: &str, plan: &mut Plan) {
        if has_extension(path, "rs") || path.ends_with("/Cargo.toml") {
            plan.ask(Check::Clippy, path);
            plan.clippy.insert(name.to_owned());
        }
        if path.ends_with("/Cargo.toml") {
            plan.ask(Check::Deny, path);
        }
        // One of `cheby_cli`'s test files is its own binary and nothing
        // else's source.
        let own_test = path
            .strip_prefix("crates/cheby_cli/tests/")
            .and_then(|rest| rest.strip_suffix(".rs"))
            .filter(|test| CLI_TESTS.iter().any(|(known, _)| known == test));
        if let Some(test) = own_test {
            plan.all_of(path, format!("binary_id(cheby_cli::{test})"));
            return;
        }
        for dependent in self.workspace.dependents(name) {
            if dependent != "cheby_cli" {
                plan.package(path, &dependent);
            }
        }
        if matches!(name, "cheby_cli" | "cheby_test_support") {
            plan.package(path, "cheby_cli");
        } else {
            for &(test, exercises) in CLI_TESTS {
                if self.workspace.closure(exercises).contains(name) {
                    plan.all_of(path, format!("binary_id(cheby_cli::{test})"));
                }
            }
        }
        let toolchain = self.workspace.closure(TOOLCHAIN);
        if toolchain.contains(name) {
            // Std's tests and the examples go through what changed.
            plan.ask_whole(Check::TestStd, path);
            plan.ask_whole(Check::Determinism, path);
        }
        if toolchain.contains(name) || name == "cheby_cli" {
            plan.ask(Check::Examples, path);
        }
        if name == "cheby_syntax" {
            plan.ask(Check::Parsers, path);
        }
    }

    /// The name of the UI or LSP test that the file `rest` below `dir` is
    /// part of: the directory with a `cheby.toml` for a package test, and
    /// otherwise the file without what follows its first `.`. `None` for a
    /// file outside the chapter or group directories.
    fn test_name(&self, dir: &str, rest: &str) -> Option<String> {
        let parts: Vec<&str> = rest.split('/').collect();
        let (file, above) = parts.split_last()?;
        if above.is_empty() {
            return None;
        }
        for depth in (1..=above.len()).rev() {
            let package = above.get(..depth)?.join("/");
            if self.lookup.exists(&format!("{dir}{package}/cheby.toml")) {
                return Some(package);
            }
        }
        let stem = file.split('.').next().filter(|stem| !stem.is_empty())?;
        Some(format!("{}/{stem}", above.join("/")))
    }

    /// A source that the parser cross-check reads, with a header that
    /// `coverage` reads.
    fn test_source(path: &str, plan: &mut Plan) {
        plan.ask(Check::Coverage, path);
        if has_extension(path, "cheby") || has_extension(path, "repl") {
            plan.ask(Check::Parsers, path);
        }
    }

    /// The UI test `name`: its trial, its parity trial in the language
    /// server's harness, and, when its own files changed, its colored
    /// rendering.
    fn ui_test(&self, path: &str, name: &str, own: bool, plan: &mut Plan) {
        let name = escaped(name);
        let _ = self;
        plan.tests_of(
            path,
            "binary_id(cheby_cli::ui)".to_owned(),
            format!("\\] {name}$"),
        );
        plan.tests_of(
            path,
            "binary_id(cheby_cli::lsp)".to_owned(),
            format!("\\] parity\\/ui\\/{name}$"),
        );
        if own {
            plan.tests_of(
                path,
                "binary_id(cheby_cli::color)".to_owned(),
                format!("^ui\\/{name}$"),
            );
        }
    }

    fn lsp_test(path: &str, name: &str, plan: &mut Plan) {
        plan.tests_of(
            path,
            "binary_id(cheby_cli::lsp)".to_owned(),
            format!("\\] golden\\/{}$", escaped(name)),
        );
    }

    fn repl_test(path: &str, name: &str, plan: &mut Plan) {
        plan.tests_of(
            path,
            "binary_id(cheby_cli::repl)".to_owned(),
            format!("\\] {}$", escaped(name)),
        );
    }

    /// The example `example`: its conformance trials at every variant and
    /// the language server's trials on it. With `own`, its own files
    /// changed, so the checks of its sources run too.
    fn example(&self, path: &str, example: &str, own: bool, plan: &mut Plan) {
        let name = escaped(example);
        plan.tests_of(
            path,
            "binary_id(cheby_cli::conformance)".to_owned(),
            format!("\\] {name}(::test)?(@.*)?$"),
        );
        plan.tests_of(
            path,
            "binary_id(cheby_cli::lsp)".to_owned(),
            format!(
                "\\] (examples|parity\\/examples|symbols\\/examples|rename_sweep|rename_run)\\/{name}$"
            ),
        );
        plan.ask(Check::Examples, path);
        if !own {
            return;
        }
        plan.all_of(path, "binary_id(cheby_fmt::corpus)".to_owned());
        plan.ask(Check::Determinism, path);
        plan.determinism.insert(example.to_owned());
        if has_extension(path, "cheby") {
            plan.ask(Check::Parsers, path);
            if self.lookup.exists(path) {
                plan.ask(Check::ChebyFmt, path);
                plan.cheby_files.insert(path.to_owned());
            }
        }
    }

    /// The std module `module`, such as `list` or `json::decode`: its test
    /// blocks and those of the std modules that import it, the tests and
    /// examples that name it, the goldens of its
    /// code, and the checks of its source (AGENTS.md, "Building and
    /// testing").
    fn std_module(&self, path: &str, module: &str, plan: &mut Plan) {
        plan.ask(Check::TestStd, path);
        plan.std_modules.insert(module.to_owned());
        // The test blocks of the std modules that use this one run too, as
        // a package's dependents do.
        let importers = std_importers(&self.lookup.std_sources(), module);
        if importers.len() > MAX_IMPORTERS {
            plan.ask_whole(Check::TestStd, path);
        } else {
            plan.std_modules.extend(importers);
        }
        plan.ask(Check::Parsers, path);
        if self.lookup.exists(path) {
            plan.ask(Check::ChebyFmt, path);
            plan.cheby_files.insert(path.to_owned());
        }
        // The MIR and CLIF goldens pin std's code, the std base and the
        // list of std's items its names, and the lookup tests its docs.
        for (scope, pattern) in [
            (
                "package(cheby_driver)",
                "mir_of|clif_of|aot_001|std_base|std_lib",
            ),
            ("package(cheby_resolve)", "std_"),
        ] {
            plan.tests_of(path, scope.to_owned(), pattern.to_owned());
        }
        for binary in [
            "cheby_cli::std_cache",
            "cheby_cli::doc",
            "cheby_fmt::corpus",
        ] {
            plan.all_of(path, format!("binary_id({binary})"));
        }
        if self.lookup.changed_line_has(path, "@external") {
            // The kernels are checked against their declarations.
            plan.tests_of(
                path,
                "package(cheby_runtime)".to_owned(),
                "kernels_match".to_owned(),
            );
        }
        let import = format!("std::{module}");
        for file in self.lookup.naming("tests/ui", &["cheby"], &import) {
            if let Some(name) = file
                .strip_prefix("tests/ui/")
                .and_then(|rest| self.test_name("tests/ui/", rest))
            {
                self.ui_test(path, &name, false, plan);
            }
        }
        // A golden of the server names a std item by its module or by the
        // module's file.
        let file_name = format!("std/{}.cheby", module.replace("::", "/"));
        for needle in [import.as_str(), file_name.as_str()] {
            for file in self
                .lookup
                .naming("tests/lsp", &["cheby", "expected"], needle)
            {
                if let Some(name) = file
                    .strip_prefix("tests/lsp/")
                    .and_then(|rest| self.test_name("tests/lsp/", rest))
                {
                    Self::lsp_test(path, &name, plan);
                }
            }
        }
        for file in self.lookup.naming("tests/repl", &["repl"], &import) {
            let name = file
                .strip_prefix("tests/repl/")
                .and_then(|rest| rest.split(['/', '.']).next());
            if let Some(name) = name {
                Self::repl_test(path, name, plan);
            }
        }
        for file in self.lookup.naming("docs/examples", &["cheby"], &import) {
            let example = file
                .strip_prefix("docs/examples/")
                .and_then(|rest| rest.split('/').next());
            if let Some(example) = example {
                self.example(path, example, false, plan);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::affected::graph::tests::workspace;

    /// A checkout as the rules see it: its files with their texts, and the
    /// paths whose change touches an `@external`.
    #[derive(Default)]
    struct Files {
        texts: BTreeMap<&'static str, &'static str>,
        externals: BTreeSet<&'static str>,
    }

    impl Lookup for Files {
        fn exists(&self, path: &str) -> bool {
            self.texts.contains_key(path)
        }

        fn naming(&self, dir: &str, extensions: &[&str], needle: &str) -> Vec<String> {
            self.texts
                .iter()
                .filter(|(path, text)| {
                    path.starts_with(&format!("{dir}/"))
                        && extensions.iter().any(|ext| has_extension(path, ext))
                        && text.contains(needle)
                })
                .map(|(path, _)| (*path).to_owned())
                .collect()
        }

        fn changed_line_has(&self, path: &str, _needle: &str) -> bool {
            self.externals.contains(path)
        }

        fn std_sources(&self) -> BTreeMap<String, String> {
            self.texts
                .iter()
                .filter_map(|(path, text)| {
                    let module = path.strip_prefix("std/")?.strip_suffix(".cheby")?;
                    Some((module.replace('/', "::"), (*text).to_owned()))
                })
                .collect()
        }
    }

    fn files() -> Files {
        Files {
            texts: BTreeMap::from([
                ("std/list.cheby", "import std::option\n"),
                ("std/io.cheby", ""),
                ("std/option.cheby", ""),
                (
                    "std/string.cheby",
                    "import std::list::{map}\nimport std::io as out\n",
                ),
                ("std/json/decode.cheby", "import std::string\n"),
                (
                    "tests/ui/05-expressions/pipes/hole.cheby",
                    "import std::list\n",
                ),
                ("tests/ui/07-modules/pkg/cheby.toml", ""),
                ("tests/ui/07-modules/pkg/src/main.cheby", "import std::io\n"),
                ("tests/lsp/completion/dot.cheby", "import std::list\n"),
                ("tests/lsp/hover/item.expected", "\"std/list.cheby\"\n"),
                ("tests/repl/lists.repl", "import std::list\n"),
                (
                    "docs/examples/003_fizzbuzz/src/main.cheby",
                    "import std::list\n",
                ),
            ]),
            externals: BTreeSet::from(["std/io.cheby"]),
        }
    }

    fn planned(paths: &[&str]) -> Plan {
        let paths: Vec<String> = paths.iter().map(|&path| path.to_owned()).collect();
        plan(&paths, &workspace(), &files())
    }

    fn commands(paths: &[&str]) -> Vec<String> {
        planned(paths).steps().iter().map(Step::shown).collect()
    }

    /// A UI test's file runs its trial, its parity trial and its colored
    /// rendering, and the checks that read test sources.
    #[test]
    fn a_ui_test_runs_its_own_trials() {
        assert_eq!(
            commands(&["tests/ui/05-expressions/pipes/hole.stderr"]),
            [
                "cargo xtask test --no-tests=pass -E '(binary_id(cheby_cli::color) & \
                 test(/(^ui\\/05-expressions\\/pipes\\/hole$)/)) | (binary_id(cheby_cli::lsp) & \
                 test(/(\\] parity\\/ui\\/05-expressions\\/pipes\\/hole$)/)) | \
                 (binary_id(cheby_cli::ui) & test(/(\\] 05-expressions\\/pipes\\/hole$)/))'",
                "cargo xtask coverage",
                "cargo xtask decisions lint",
                "typos",
            ]
        );
        // A file of a package test names the package's directory.
        let package = planned(&["tests/ui/07-modules/pkg/src/main.cheby"]);
        let filter = package.filter().unwrap();
        assert!(
            filter.contains("test(/(\\] 07-modules\\/pkg$)/)"),
            "{filter}"
        );
        assert!(package.why.contains_key(&Check::Parsers));
        // The chapter's README is no test.
        assert_eq!(planned(&["tests/ui/README.md"]).filter(), None);
    }

    /// A leaf package runs itself, what depends on it, and the binaries of
    /// `cheby_cli` that exercise it; a core package runs nearly all.
    #[test]
    fn a_package_runs_its_dependents() {
        assert_eq!(
            commands(&["crates/cheby_fmt/src/lib.rs"]),
            [
                "cargo fmt --all --check",
                "cargo clippy --locked --all-targets -p cheby_fmt",
                "cargo xtask test --no-tests=pass -E 'package(cheby_fmt) | package(cheby_lsp) | \
                 binary_id(cheby_cli::backend) | binary_id(cheby_cli::fmt) | \
                 binary_id(cheby_cli::lsp)'",
                "cargo xtask decisions lint",
                "RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps \
                 --document-private-items --locked",
                "typos",
            ]
        );
        let doc = planned(&["crates/cheby_doc/src/lib.rs"]);
        assert_eq!(
            doc.filter().as_deref(),
            Some("package(cheby_doc) | binary_id(cheby_cli::doc)")
        );
        let syntax = planned(&["crates/cheby_syntax/src/parser/mod.rs"]);
        let filter = syntax.filter().unwrap();
        // Every binary of `cheby_cli` goes through the parser, but its unit
        // tests do not, and the runtime's tests have nothing to do with it.
        assert!(filter.contains("binary_id(cheby_cli::ui)") && filter.contains("package(xtask)"));
        assert!(!filter.contains("package(cheby_cli)") && !filter.contains("cheby_runtime"));
        for check in [
            Check::TestStd,
            Check::Examples,
            Check::Determinism,
            Check::Parsers,
        ] {
            assert!(syntax.why.contains_key(&check), "{check:?}");
        }
        let shown: Vec<String> = syntax.steps().iter().map(Step::shown).collect();
        assert!(shown.contains(&"cargo xtask test-std".to_owned()));
        assert!(shown.contains(&"cargo xtask check-determinism".to_owned()));
    }

    /// The runtime's archive package runs the binaries that link it. The
    /// archive itself needs no step: `cargo xtask test` builds it (D-1950).
    #[test]
    fn the_runtime_archive_runs_what_links_it() {
        let shown = commands(&["crates/cheby_rt_exe/src/lib.rs"]);
        assert!(
            shown[2].contains("binary_id(cheby_cli::build)"),
            "{}",
            shown[2]
        );
        assert!(!shown[2].contains("cheby_cli::fmt"), "{}", shown[2]);
        assert!(shown.iter().all(|command| !command.contains("cargo build")));
    }

    /// One of `cheby_cli`'s test files runs its binary alone, and its
    /// sources run the whole package.
    #[test]
    fn the_command_line_and_its_tests() {
        assert_eq!(
            planned(&["crates/cheby_cli/tests/doc.rs"])
                .filter()
                .as_deref(),
            Some("binary_id(cheby_cli::doc)")
        );
        assert_eq!(
            planned(&[
                "crates/cheby_cli/src/main.rs",
                "crates/cheby_cli/tests/doc.rs"
            ])
            .filter()
            .as_deref(),
            Some("package(cheby_cli)")
        );
        assert_eq!(
            planned(&["tests/ui_harness/main.rs"]).filter().as_deref(),
            Some("binary_id(cheby_cli::ui)")
        );
        assert_eq!(
            planned(&["xtask/src/main.rs"]).filter().as_deref(),
            Some("package(xtask)")
        );
    }

    /// A std module runs its test blocks, the tests and examples that name
    /// it, the goldens of its code, and the kernel check when an
    /// `@external` changed.
    #[test]
    fn a_std_module_runs_what_names_it() {
        let list = planned(&["std/list.cheby"]);
        let filter = list.filter().unwrap();
        for part in [
            "(binary_id(cheby_cli::ui) & test(/(\\] 05-expressions\\/pipes\\/hole$)/))",
            "\\] golden\\/completion\\/dot$|\\] golden\\/hover\\/item$",
            "\\] parity\\/ui\\/05-expressions\\/pipes\\/hole$",
            "(binary_id(cheby_cli::repl) & test(/(\\] lists$)/))",
            "(binary_id(cheby_cli::conformance) & test(/(\\] 003_fizzbuzz(::test)?(@.*)?$)/))",
            "(package(cheby_driver) & test(/(mir_of|clif_of|aot_001|std_base|std_lib)/))",
            "binary_id(cheby_cli::std_cache)",
            "binary_id(cheby_fmt::corpus)",
        ] {
            assert!(filter.contains(part), "{part}\n{filter}");
        }
        assert!(!filter.contains("kernels_match") && !filter.contains("cheby_cli::color"));
        let shown: Vec<String> = list.steps().iter().map(Step::shown).collect();
        // Its own test blocks, and those of `string`, which imports it, and
        // of `json::decode`, which imports `string`; not those of `option`,
        // which it imports.
        let std_tests: Vec<&str> = shown
            .iter()
            .filter(|command| command.starts_with("cargo xtask test-std"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            std_tests,
            [
                "cargo xtask test-std 'std::json::decode \"'",
                "cargo xtask test-std 'std::list \"'",
                "cargo xtask test-std 'std::string \"'",
            ]
        );
        assert!(shown.contains(
            &"cargo run --locked -q -p cheby_cli -- fmt --check std/list.cheby".to_owned()
        ));
        assert!(shown.contains(&"cargo xtask check-examples".to_owned()));
        assert!(shown.contains(&"cargo xtask cross-check".to_owned()));
        let io = planned(&["std/io.cheby"]).filter().unwrap();
        assert!(
            io.contains("(package(cheby_runtime) & test(/(kernels_match)/))"),
            "{io}"
        );
        assert!(io.contains("\\] 07-modules\\/pkg$"), "{io}");
    }

    /// Importers are found through other std modules, by the module an
    /// import names, and many of them become all of `test-std`.
    #[test]
    fn std_modules_that_import_a_module() {
        let sources = files().std_sources();
        let of =
            |module: &str| -> Vec<String> { std_importers(&sources, module).into_iter().collect() };
        assert_eq!(of("option"), ["json::decode", "list", "string"]);
        assert_eq!(of("io"), ["json::decode", "string"]);
        assert_eq!(of("json::decode"), Vec::<String>::new());
        let mut many = Files::default();
        for n in 0..=MAX_IMPORTERS {
            let path: &'static str = Box::leak(format!("std/m{n}.cheby").into_boxed_str());
            many.texts.insert(path, "import std::base\n");
        }
        many.texts.insert("std/base.cheby", "");
        let paths = vec!["std/base.cheby".to_owned()];
        let shown: Vec<String> = plan(&paths, &workspace(), &many)
            .steps()
            .iter()
            .map(Step::shown)
            .collect();
        assert!(
            shown.contains(&"cargo xtask test-std".to_owned()),
            "{shown:#?}"
        );
    }

    #[test]
    fn an_example_runs_its_trials_and_its_checks() {
        let shown = commands(&["docs/examples/003_fizzbuzz/src/main.cheby"]);
        assert!(
            shown[0].contains("\\] 003_fizzbuzz(::test)?(@.*)?$"),
            "{}",
            shown[0]
        );
        assert!(
            shown[0].contains("rename_sweep|rename_run)\\/003_fizzbuzz$"),
            "{}",
            shown[0]
        );
        assert_eq!(
            shown[1..],
            [
                "cargo xtask check-examples",
                "cargo run --locked -q -p cheby_cli -- fmt --check \
                 docs/examples/003_fizzbuzz/src/main.cheby",
                "cargo xtask cross-check",
                "cargo xtask check-determinism 003_fizzbuzz",
                "cargo xtask decisions lint",
                "typos",
            ]
        );
    }

    /// Docs ask for the checks of text, the spec for coverage, a workflow
    /// for its linter, and a file every build reads for everything.
    #[test]
    fn docs_workflows_and_files_of_every_build() {
        assert_eq!(
            commands(&["docs/plan/README.md"]),
            ["cargo xtask decisions lint", "typos"]
        );
        assert_eq!(
            commands(&["docs/spec/13-tooling.md"]),
            [
                "cargo xtask coverage",
                "cargo xtask decisions lint",
                "typos"
            ]
        );
        assert_eq!(
            commands(&[".github/workflows/ci.yml"]),
            ["cargo xtask decisions lint", "typos", "actionlint"]
        );
        let lock = commands(&["Cargo.lock"]);
        assert!(lock.contains(&"cargo clippy --locked --all-targets --workspace".to_owned()));
        assert!(lock.contains(&"cargo xtask test".to_owned()));
        assert!(lock.contains(&"cargo deny --locked check".to_owned()));
        let manifest = commands(&["crates/cheby_fmt/Cargo.toml"]);
        assert!(manifest.contains(&"taplo fmt --check".to_owned()));
        assert!(manifest.contains(&"cargo deny --locked check".to_owned()));
        let diagnostics = planned(&["docs/diagnostics/E0042.md"]).filter().unwrap();
        assert!(diagnostics.contains("package(cheby_diag)"), "{diagnostics}");
        assert!(
            diagnostics.contains("binary_id(cheby_cli::explain)"),
            "{diagnostics}"
        );
    }

    #[test]
    fn a_path_without_a_rule_is_listed() {
        let plan = planned(&["fuzz/fuzz_targets/parse.rs", "docs/notes.md"]);
        assert_eq!(plan.unmatched, ["fuzz/fuzz_targets/parse.rs"]);
        assert_eq!(plan.filter(), None);
        assert!(!plan.why.contains_key(&Check::Tests));
    }

    /// Past [`MAX_TESTS`] tests of one binary, the filter names the binary.
    #[test]
    fn many_tests_of_a_binary_become_the_binary() {
        let paths: Vec<String> = (0..=MAX_TESTS)
            .map(|n| format!("tests/ui/05-expressions/t{n}.cheby"))
            .collect();
        let filter = plan(&paths, &workspace(), &files()).filter().unwrap();
        assert_eq!(
            filter,
            "binary_id(cheby_cli::color) | binary_id(cheby_cli::lsp) | binary_id(cheby_cli::ui)"
        );
    }

    #[test]
    fn arguments_are_quoted_for_a_shell() {
        let mut step = Step::new(Check::Tests, "cargo", &["xtask", "test", "-E", "a | b's"]);
        step.env.push(("RUSTDOCFLAGS", "-D warnings"));
        assert_eq!(
            step.shown(),
            "RUSTDOCFLAGS='-D warnings' cargo xtask test -E 'a | b'\\''s'"
        );
        assert_eq!(escaped("a/b.c(d)"), "a\\/b\\.c\\(d\\)");
    }

    /// The table names every integration test of `cheby_cli` that cargo
    /// lists, and no other.
    #[test]
    fn the_table_has_every_test_binary_of_the_command_line() {
        let workspace = Workspace::load(&cheby_test_support::repo_root()).unwrap();
        let mut listed: Vec<&str> = workspace
            .package("cheby_cli")
            .unwrap()
            .tests
            .iter()
            .map(String::as_str)
            .collect();
        listed.sort_unstable();
        let table: Vec<&str> = CLI_TESTS.iter().map(|&(name, _)| name).collect();
        assert_eq!(table, listed);
        // Every package the table and the rules name exists.
        let named = CLI_TESTS
            .iter()
            .flat_map(|&(_, exercises)| exercises)
            .chain(
                [
                    "cheby_cli",
                    "cheby_test_support",
                    "cheby_diag",
                    "cheby_resolve",
                    "xtask",
                ]
                .iter(),
            );
        for name in named {
            assert!(workspace.package(name).is_some(), "{name}");
        }
        assert!(
            workspace
                .package("cheby_fmt")
                .unwrap()
                .tests
                .contains(&"corpus".to_owned())
        );
    }
}
