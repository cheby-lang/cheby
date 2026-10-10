//! UI tests in `tests/ui/`: one `.cheby` file per rule, or one directory
//! holding a whole package when a rule needs several modules or its own
//! manifest. The test starts with a header of `// @key: value`
//! directives before any other line.
//!
//! ```cheby
//! // @mode: check-fail
//! // @decisions: D-244, D-245
//! // @milestone: M1
//! //! A pipe into a call needs an explicit hole.
//! ```
//!
//! Directives:
//!
//! - `@mode` (required): `check-pass`, `check-fail`, `parse-fail`,
//!   `run-pass` or `run-fail`. `parse-fail` is a `check-fail` that both
//!   the compiler parser and tree-sitter must reject.
//! - `@decisions`: the decision-log entries the test pins, for coverage.
//! - `@milestone`: the milestone in which the test must pass, M1 by default.
//! - `@exit`: the expected exit code of `run-fail`, 101 by default.
//! - `@command`: `test` makes a `run-pass` or `run-fail` test run
//!   `cheby test` instead of `cheby run`, and its goldens hold that
//!   command's output (D-543). `run` is the default.
//! - `@flags`: command-line words added after the subcommand, split at
//!   whitespace, such as `--module app::util` (D-543).
//!
//! A directory `tests/ui/<chapter>/<name>/` that holds a `cheby.toml` is
//! one package test ([`Layout::Package`]). Its `cheby.toml` and `src/**`
//! are test input, copied as they are, and its header is in the root
//! module, `src/main.cheby`. No other file of the package may start with
//! a directive. Every file of a package test must parse, so a package
//! test cannot be `parse-fail`.
//!
//! Goldens sit next to the file or the directory: `.stderr` for
//! diagnostics, `.stdout` for the output of `run-*` tests, such as
//! `tests/ui/07-modules-and-packages/private_in_sibling.stderr` for the
//! directory `tests/ui/07-modules-and-packages/private_in_sibling/`.
//!
//! A run test may also have input beside it (D-543, [`RunInput`]):
//! `<name>.stdin` is fed to `cheby run` as standard input, byte for byte
//! (without it standard input is null), and `<name>.args` holds the
//! program's arguments, one per line, passed after `--`. Other modes may
//! not have them.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::milestone::Milestone;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    CheckPass,
    CheckFail,
    ParseFail,
    RunPass,
    RunFail,
}

impl Mode {
    fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "check-pass" => Self::CheckPass,
            "check-fail" => Self::CheckFail,
            "parse-fail" => Self::ParseFail,
            "run-pass" => Self::RunPass,
            "run-fail" => Self::RunFail,
            other => {
                return Err(format!(
                    "unknown mode `{other}`, expected check-pass, check-fail, parse-fail, \
                     run-pass or run-fail"
                ));
            }
        })
    }

    /// Whether the compiler must reject the file.
    #[must_use]
    pub const fn fails_to_compile(self) -> bool {
        matches!(self, Self::CheckFail | Self::ParseFail)
    }

    /// Whether the file is meant to be a syntax error.
    #[must_use]
    pub const fn is_syntax_error(self) -> bool {
        matches!(self, Self::ParseFail)
    }

    /// Whether the test runs the program, not just checks it.
    #[must_use]
    pub const fn runs(self) -> bool {
        matches!(self, Self::RunPass | Self::RunFail)
    }
}

/// The command a run test runs (D-543).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunCommand {
    /// `cheby run`.
    #[default]
    Run,
    /// `cheby test`.
    Test,
}

impl RunCommand {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "run" => Ok(Self::Run),
            "test" => Ok(Self::Test),
            other => Err(format!("unknown command `{other}`, expected run or test")),
        }
    }

    /// The `cheby` subcommand.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Test => "test",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Header {
    pub mode: Mode,
    pub decisions: BTreeSet<u32>,
    pub milestone: Milestone,
    pub exit: Option<i32>,
    /// What a run test runs; always [`RunCommand::Run`] for other modes.
    pub command: RunCommand,
    /// Words added after the subcommand (`@flags`).
    pub flags: Vec<String>,
}

impl Header {
    /// Parses the leading `// @key: value` lines of `source`.
    ///
    /// # Errors
    ///
    /// Returns a message for a malformed or unknown directive, a missing
    /// `@mode`, `@exit` without `@mode: run-fail`, or `@command` without a
    /// run mode.
    pub fn parse(source: &str) -> Result<Self, String> {
        let mut mode = None;
        let mut decisions = BTreeSet::new();
        let mut milestone = Milestone::M1;
        let mut exit = None;
        let mut command = None;
        let mut flags = Vec::new();
        for line in source.lines() {
            let Some(directive) = line.strip_prefix("// @") else {
                break;
            };
            let (key, value) = directive
                .split_once(':')
                .ok_or_else(|| format!("directive `{line}` has no `:`"))?;
            let value = value.trim();
            match key.trim() {
                "mode" => mode = Some(Mode::parse(value)?),
                "decisions" => {
                    for d in value.split(',') {
                        decisions.insert(crate::decisions::parse(d)?);
                    }
                }
                "milestone" => milestone = value.parse()?,
                "exit" => exit = Some(value.parse().map_err(|e| format!("@exit: {e}"))?),
                "command" => command = Some(RunCommand::parse(value)?),
                "flags" => flags.extend(value.split_whitespace().map(str::to_owned)),
                other => return Err(format!("unknown directive `@{other}`")),
            }
        }
        let mode = mode.ok_or("missing `// @mode:` directive")?;
        if exit.is_some() && mode != Mode::RunFail {
            return Err("`@exit` is only allowed with `@mode: run-fail`".to_owned());
        }
        if command.is_some() && !mode.runs() {
            return Err(
                "`@command` is only allowed with `@mode: run-pass` or `run-fail`".to_owned(),
            );
        }
        Ok(Self {
            mode,
            decisions,
            milestone,
            exit,
            command: command.unwrap_or_default(),
            flags,
        })
    }
}

/// How a UI test is laid out on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// One `.cheby` file, copied into a package as `src/main.cheby`.
    File,
    /// A directory with `cheby.toml` and `src/**`, copied as it is.
    Package,
}

#[derive(Debug, Clone)]
pub struct UiTest {
    /// Path relative to `tests/ui`, with `/` separators and no extension.
    pub name: String,
    /// The `.cheby` file, or the package directory.
    pub path: PathBuf,
    pub layout: Layout,
    pub header: Header,
}

impl UiTest {
    /// The golden file next to the test file or directory, with
    /// extension `ext`.
    #[must_use]
    pub fn golden(&self, ext: &str) -> PathBuf {
        match self.layout {
            Layout::File => self.path.with_extension(ext),
            Layout::Package => {
                let mut name = self.path.file_name().unwrap_or_default().to_os_string();
                name.push(".");
                name.push(ext);
                self.path.with_file_name(name)
            }
        }
    }

    /// The file that holds the header: the test file, or the package's
    /// root module.
    #[must_use]
    pub fn header_file(&self) -> PathBuf {
        match self.layout {
            Layout::File => self.path.clone(),
            Layout::Package => self.path.join(ROOT_MODULE),
        }
    }

    /// Every `.cheby` file of the test, sorted by path.
    ///
    /// # Errors
    ///
    /// Returns a message if the package directory cannot be read.
    pub fn sources(&self) -> Result<Vec<PathBuf>, String> {
        match self.layout {
            Layout::File => Ok(vec![self.path.clone()]),
            Layout::Package => crate::files_with_extension(&self.path, "cheby")
                .map_err(|e| format!("{}: {e}", self.path.display())),
        }
    }
}

/// The input of a run test (D-543): standard input and arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunInput {
    /// The bytes of `<name>.stdin`, fed as standard input, or `None` for
    /// a null standard input.
    pub stdin: Option<Vec<u8>>,
    /// The lines of `<name>.args`, passed after `--`.
    pub args: Vec<String>,
}

impl UiTest {
    /// The test's standard input and arguments, read from `<name>.stdin`
    /// and `<name>.args` beside it (D-543).
    ///
    /// # Errors
    ///
    /// Returns a message if a file exists but cannot be read.
    pub fn input(&self) -> Result<RunInput, String> {
        let stdin_path = self.golden("stdin");
        let stdin = match std::fs::read(&stdin_path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{}: {e}", stdin_path.display())),
        };
        let args = crate::golden::read(&self.golden("args"))?
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default();
        Ok(RunInput { stdin, args })
    }
}

/// The extensions of the input files of a run test (D-543).
pub const INPUT_EXTENSIONS: [&str; 2] = ["stdin", "args"];

/// The root module of a package test, which holds the header.
pub const ROOT_MODULE: &str = "src/main.cheby";
/// The manifest that makes a directory under `tests/ui` a package test.
pub const MANIFEST: &str = "cheby.toml";

/// Every UI test, sorted by path.
///
/// # Errors
///
/// Returns a message if a file cannot be read, a test has a bad header,
/// a package test has no root module or a directive outside it, or two
/// tests share a name.
pub fn all(root: &Path) -> Result<Vec<UiTest>, String> {
    let dir = root.join("tests/ui");
    let mut found = Vec::new();
    if dir.exists() {
        discover(&dir, &mut found).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let mut tests: Vec<UiTest> = Vec::with_capacity(found.len());
    for (path, layout) in found {
        let test = read_test(root, &dir, path, layout)?;
        if let Some(other) = tests.iter().find(|t| t.name == test.name) {
            return Err(format!(
                "{} and {} are both the UI test `{}`",
                crate::display_relative(&other.path, root),
                crate::display_relative(&test.path, root),
                test.name
            ));
        }
        tests.push(test);
    }
    Ok(tests)
}

/// Collects the `.cheby` files and package directories under `dir`.
/// Hidden entries are skipped, and nothing inside a package is a test of
/// its own.
fn discover(dir: &Path, out: &mut Vec<(PathBuf, Layout)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if entry.file_type()?.is_dir() {
            if path.join(MANIFEST).is_file() {
                out.push((path, Layout::Package));
            } else {
                discover(&path, out)?;
            }
        } else if path.extension().is_some_and(|e| e == "cheby") {
            out.push((path, Layout::File));
        }
    }
    Ok(())
}

fn read_test(root: &Path, dir: &Path, path: PathBuf, layout: Layout) -> Result<UiTest, String> {
    let shown = |p: &Path| crate::display_relative(p, root);
    let header_file = match layout {
        Layout::File => path.clone(),
        Layout::Package => path.join(ROOT_MODULE),
    };
    let source = std::fs::read_to_string(&header_file).map_err(|e| match layout {
        Layout::Package if e.kind() == std::io::ErrorKind::NotFound => format!(
            "{}: a package test needs `{ROOT_MODULE}`, which holds its header",
            shown(&path)
        ),
        _ => format!("{}: {e}", shown(&header_file)),
    })?;
    let header = Header::parse(&source).map_err(|e| format!("{}: {e}", shown(&header_file)))?;
    let name = match layout {
        Layout::File => crate::display_relative(&path.with_extension(""), dir),
        Layout::Package => crate::display_relative(&path, dir),
    };
    let test = UiTest {
        name,
        path,
        layout,
        header,
    };
    if test.header.command == RunCommand::Test && test.golden("args").exists() {
        return Err(format!(
            "{}: `cheby test` takes no program arguments",
            shown(&test.golden("args"))
        ));
    }
    if !test.header.mode.runs() {
        for ext in INPUT_EXTENSIONS {
            let input = test.golden(ext);
            if input.exists() {
                return Err(format!(
                    "{}: only `run-pass` and `run-fail` tests take `.{ext}` input",
                    shown(&input)
                ));
            }
        }
    }
    if layout == Layout::Package {
        if test.header.mode == Mode::ParseFail {
            return Err(format!(
                "{}: a package test cannot be `parse-fail`, since every file of it must \
                 parse; write the syntax error as a single-file test",
                shown(&header_file)
            ));
        }
        for file in test.sources()? {
            if file == header_file {
                continue;
            }
            let contents =
                std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", shown(&file)))?;
            if contents.starts_with("// @") {
                return Err(format!(
                    "{}: directives belong in the package's `{ROOT_MODULE}`",
                    shown(&file)
                ));
            }
        }
    }
    Ok(test)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_header() {
        let h = Header::parse(
            "// @mode: run-fail\n// @decisions: D-244, D-245\n// @exit: 1\n//! Doc.\n// @mode: x\n",
        )
        .unwrap();
        assert_eq!(h.mode, Mode::RunFail);
        assert_eq!(h.decisions.into_iter().collect::<Vec<_>>(), vec![244, 245]);
        assert_eq!(h.milestone, Milestone::M1);
        assert_eq!(h.exit, Some(1));
    }

    #[test]
    fn rejects_bad_headers() {
        assert!(Header::parse("//! no mode\n").is_err());
        assert!(Header::parse("// @mode: check-pass\n// @colour: red\n").is_err());
        assert!(Header::parse("// @mode: check-pass\n// @exit: 1\n").is_err());
        assert!(Header::parse("// @mode: check-pass\n// @command: test\n").is_err());
        assert!(Header::parse("// @mode: run-pass\n// @command: bench\n").is_err());
    }

    #[test]
    fn parses_the_command() {
        let run = Header::parse("// @mode: run-pass\n").unwrap();
        assert_eq!(run.command, RunCommand::Run);
        let test = Header::parse("// @mode: run-fail\n// @command: test\n// @exit: 1\n").unwrap();
        assert_eq!(test.command, RunCommand::Test);
        assert_eq!(test.command.name(), "test");
        assert_eq!(test.flags, Vec::<String>::new());
    }

    #[test]
    fn parses_flags() {
        let h = Header::parse("// @mode: run-pass\n// @flags: --module app::util  adds\n").unwrap();
        assert_eq!(h.flags, ["--module", "app::util", "adds"]);
    }

    /// A scratch `tests/ui` tree under the system's temporary directory.
    fn scratch(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("cheby-ui-discovery-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, text) in files {
            let path = root.join("tests/ui").join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        root
    }

    #[test]
    fn finds_files_and_packages() {
        let root = scratch(
            "ok",
            &[
                ("01-a/single.cheby", "// @mode: check-pass\n"),
                ("07-b/pkg/cheby.toml", "name = \"app\"\n"),
                (
                    "07-b/pkg/src/main.cheby",
                    "// @mode: check-fail\n// @decisions: D-162\n",
                ),
                ("07-b/pkg/src/util/inner.cheby", "//! Not a test.\n"),
            ],
        );
        let tests = all(&root).unwrap();
        let names: Vec<_> = tests.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["01-a/single", "07-b/pkg"]);
        let [single, pkg] = tests.as_slice() else {
            panic!("two tests expected");
        };
        assert_eq!(single.layout, Layout::File);
        assert_eq!(
            single.golden("stderr"),
            root.join("tests/ui/01-a/single.stderr")
        );
        assert_eq!(pkg.layout, Layout::Package);
        assert_eq!(pkg.header.mode, Mode::CheckFail);
        assert_eq!(pkg.golden("stderr"), root.join("tests/ui/07-b/pkg.stderr"));
        assert_eq!(
            pkg.header_file(),
            root.join("tests/ui/07-b/pkg/src/main.cheby")
        );
        assert_eq!(pkg.sources().unwrap().len(), 2);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn reads_run_input() {
        let root = scratch(
            "input",
            &[
                ("c/reads.cheby", "// @mode: run-pass\n"),
                ("c/reads.stdin", "line one\r\nline two"),
                ("c/reads.args", "a\n\nb c\n"),
                ("c/plain.cheby", "// @mode: run-fail\n"),
                ("c/pkg/cheby.toml", ""),
                ("c/pkg/src/main.cheby", "// @mode: run-pass\n"),
                ("c/pkg.args", "x\n"),
            ],
        );
        let tests = all(&root).unwrap();
        let input = |name: &str| {
            tests
                .iter()
                .find(|t| t.name == name)
                .unwrap()
                .input()
                .unwrap()
        };
        assert_eq!(
            input("c/reads"),
            RunInput {
                stdin: Some(b"line one\r\nline two".to_vec()),
                args: vec!["a".to_owned(), String::new(), "b c".to_owned()],
            }
        );
        assert_eq!(input("c/plain"), RunInput::default());
        assert_eq!(input("c/pkg").args, ["x"]);
        std::fs::remove_dir_all(&root).unwrap();
        let root = scratch(
            "input-on-check",
            &[("c/t.cheby", "// @mode: check-pass\n"), ("c/t.stdin", "x")],
        );
        assert!(all(&root).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rejects_bad_packages() {
        let cases: [(&str, &[(&str, &str)]); 4] = [
            ("no-root", &[("c/p/cheby.toml", "")]),
            (
                "parse-fail",
                &[
                    ("c/p/cheby.toml", ""),
                    ("c/p/src/main.cheby", "// @mode: parse-fail\n"),
                ],
            ),
            (
                "stray-directive",
                &[
                    ("c/p/cheby.toml", ""),
                    ("c/p/src/main.cheby", "// @mode: check-pass\n"),
                    ("c/p/src/other.cheby", "// @mode: check-pass\n"),
                ],
            ),
            (
                "duplicate",
                &[
                    ("c/p.cheby", "// @mode: check-pass\n"),
                    ("c/p/cheby.toml", ""),
                    ("c/p/src/main.cheby", "// @mode: check-pass\n"),
                ],
            ),
        ];
        for (name, files) in cases {
            let root = scratch(name, files);
            assert!(all(&root).is_err(), "{name}");
            std::fs::remove_dir_all(&root).unwrap();
        }
    }
}
