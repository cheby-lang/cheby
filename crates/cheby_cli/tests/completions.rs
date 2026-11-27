//! Shell completion end to end (D-1958 to D-1960): `cheby completions
//! <shell>` prints a script for each shell, and the built binary answers
//! the requests such a script makes, with the subcommands, the diagnostic
//! codes, the module paths and test names of a package, `.cheby` files and
//! the paths `cheby doc` finds.
//!
//! A request is `cheby -- <words>` with `CHEBY_COMPLETE` naming the shell.
//! The tests ask as fish does, one candidate per line with a tab before its
//! description, and as bash does, with the words split at `:` and `=`. No
//! shell runs here.
#![expect(
    clippy::unwrap_used,
    reason = "test helpers: a failed setup step fails the test"
)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use cheby_diag::codes::ALL;
use cheby_test_support::cache::{CACHE_ENV, FreshDir};
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(300);

const MAIN: &str = "//! The program.

import app::util

fn main() {
  assert util::twice(2) == 4
}

test \"adds\" {
  assert 1 + 1 == 2
}

test \"adds twice\" {
  assert util::twice(1) == 2
}
";

const UTIL: &str = "//! Small helpers.

/// Twice `n`.
fn twice(n: Int) -> Int {
  n + n
}

test \"doubles (small)\" {
  assert twice(2) == 4
}
";

const PROBE: &str = "test \"probes\" {
  assert True
}
";

/// A directory with the package `app`, a lone file `probe.cheby` next to
/// it, and a build cache.
struct Fixture {
    dir: FreshDir,
    cache: FreshDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            dir: FreshDir::create("cli-completions").unwrap(),
            cache: FreshDir::create("cli-completions-cache").unwrap(),
        };
        for (file, text) in [
            ("app/cheby.toml", "name = \"app\"\nversion = \"0.1.0\"\n"),
            ("app/src/main.cheby", MAIN),
            ("app/src/util.cheby", UTIL),
            ("probe.cheby", PROBE),
            ("notes.txt", "no source\n"),
        ] {
            let path = fixture.dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        fixture
    }

    /// `cheby` in `dir`, a directory of the fixture or `""` for its top.
    fn cheby(&self, dir: &str) -> Command {
        let mut cmd = Command::new(CHEBY);
        cmd.current_dir(self.dir.path().join(dir))
            .env("NO_COLOR", "1")
            .env(CACHE_ENV, self.cache.path());
        cmd
    }

    /// The candidates for the last of `words`, the words of a command line
    /// typed in `dir`, each with its description, as fish asks for them.
    fn ask(&self, dir: &str, words: &[&str]) -> Vec<(String, String)> {
        let mut cmd = self.cheby(dir);
        cmd.env("CHEBY_COMPLETE", "fish").arg("--").args(words);
        let out = process::run(cmd, None, TIMEOUT).unwrap();
        assert_eq!(out.status, Some(0), "{words:?}: {}", out.stderr);
        assert_eq!(out.stderr, "", "{words:?}");
        out.stdout
            .lines()
            .map(|line| {
                let (value, about) = line.split_once('\t').unwrap_or((line, ""));
                (value.to_owned(), about.to_owned())
            })
            .collect()
    }

    /// The values of [`Fixture::ask`] that are no flags.
    fn values(&self, dir: &str, words: &[&str]) -> Vec<String> {
        self.ask(dir, words)
            .into_iter()
            .map(|(value, _)| value)
            .filter(|value| !value.starts_with("--") && value != "-o")
            .collect()
    }

    /// What bash inserts for the word at `index` of `words`, which are the
    /// words as bash 4 splits them.
    fn ask_bash(&self, dir: &str, words: &[&str], index: usize) -> Vec<String> {
        let mut cmd = self.cheby(dir);
        cmd.env("CHEBY_COMPLETE", "bash")
            .env("_CHEBY_INDEX", index.to_string())
            .env("_CHEBY_BREAKS", " \t\n\"'@><=;|&(:")
            .env("_CHEBY_SPACE", "false")
            .arg("--")
            .args(words);
        let out = process::run(cmd, None, TIMEOUT).unwrap();
        assert_eq!(out.status, Some(0), "{words:?}: {}", out.stderr);
        assert_eq!(out.stderr, "", "{words:?}");
        out.stdout
            .split('\u{b}')
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

fn run(cmd: Command) -> RunOutput {
    process::run(cmd, None, TIMEOUT).unwrap()
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|&value| value.to_owned()).collect()
}

/// Every shell gets a script that names the variable and the command, and
/// another shell is a usage error.
#[test]
fn a_script_for_every_shell() {
    let fixture = Fixture::new();
    for shell in ["bash", "elvish", "fish", "powershell", "zsh"] {
        let mut cmd = fixture.cheby("");
        cmd.args(["completions", shell]);
        let out = run(cmd);
        assert_eq!(out.status, Some(0), "{shell}: {}", out.stderr);
        assert_eq!(out.stderr, "", "{shell}");
        assert!(
            out.stdout.contains("CHEBY_COMPLETE"),
            "{shell}: {}",
            out.stdout
        );
        assert!(out.stdout.contains("cheby"), "{shell}: {}", out.stdout);
        // The script calls this binary by its path, since the test ran it
        // by one.
        let name = Path::new(CHEBY).file_name().unwrap().to_string_lossy();
        assert!(out.stdout.contains(&*name), "{shell}: {}", out.stdout);
    }
    let mut cmd = fixture.cheby("");
    cmd.args(["completions", "tcsh"]);
    let out = run(cmd);
    assert_eq!(out.status, Some(2), "{}", out.stderr);
    assert_eq!(out.stdout, "");
    assert!(out.stderr.contains("possible values"), "{}", out.stderr);
}

/// A run without a shell in the variable is an ordinary run.
#[test]
fn an_empty_variable_changes_nothing() {
    let fixture = Fixture::new();
    for value in ["", "0"] {
        let mut cmd = fixture.cheby("");
        cmd.env("CHEBY_COMPLETE", value).args(["explain", "E0001"]);
        let out = run(cmd);
        assert_eq!(out.status, Some(0), "{value:?}: {}", out.stderr);
        assert!(
            out.stdout.starts_with("# E0001: "),
            "{value:?}: {}",
            out.stdout
        );
    }
    let mut cmd = fixture.cheby("");
    cmd.env("CHEBY_COMPLETE", "tcsh").args(["--", "cheby", ""]);
    let out = run(cmd);
    assert_ne!(out.status, Some(0));
    assert!(out.stderr.contains("unknown shell"), "{}", out.stderr);
}

/// The commands, with their help, the hidden one left out, and the values
/// of an enumeration.
#[test]
fn commands_flags_and_enumerations() {
    let fixture = Fixture::new();
    let commands = fixture.ask("", &["cheby", ""]);
    for command in ["check", "run", "test", "doc", "explain", "completions"] {
        assert!(
            commands
                .iter()
                .any(|(value, about)| value == command && !about.is_empty()),
            "{command}: {commands:?}"
        );
    }
    assert!(
        commands.iter().all(|(value, _)| !value.contains("backend")),
        "{commands:?}"
    );
    assert_eq!(fixture.values("", &["cheby", "comp"]), ["completions"]);
    assert_eq!(
        fixture.values("", &["cheby", "check", "--target", ""]),
        ["native", "js"]
    );
    assert_eq!(
        fixture.values("", &["cheby", "completions", ""]),
        ["bash", "elvish", "fish", "powershell", "zsh"]
    );
    let flags = fixture.ask("", &["cheby", "test", "--re"]);
    let flags: Vec<&str> = flags.iter().map(|(value, _)| value.as_str()).collect();
    assert_eq!(
        flags,
        ["--reuse-report", "--release", "--report-failures-only"]
    );
}

/// `cheby explain` completes every registered code, with its summary.
#[test]
fn diagnostic_codes() {
    let fixture = Fixture::new();
    let all = fixture.ask("", &["cheby", "explain", ""]);
    for info in ALL {
        let code = info.code.to_string();
        assert!(
            all.iter()
                .any(|(value, about)| *value == code && about == info.summary),
            "{code}"
        );
    }
    let warnings = fixture.values("", &["cheby", "explain", "W000"]);
    assert!(warnings.len() > 5, "{warnings:?}");
    assert!(
        warnings.iter().all(|code| code.starts_with("W000")),
        "{warnings:?}"
    );
}

/// `run`, `build` and `test --module` complete the package's module paths
/// from any of its directories, and nothing outside a package.
#[test]
fn module_paths() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.values("app", &["cheby", "build", ""]),
        ["app", "app::util"]
    );
    assert_eq!(
        fixture.values("app/src", &["cheby", "test", "--module", "app::"]),
        ["app::util"]
    );
    assert_eq!(
        fixture.values("app", &["cheby", "run", "app::u"]),
        ["app::util"]
    );
    assert_eq!(
        fixture.values("", &["cheby", "build", ""]),
        Vec::<String>::new()
    );
}

/// Where a file may stand, the `.cheby` files and the directories are
/// offered, and `-` where standard input is read.
#[test]
fn files() {
    let fixture = Fixture::new();
    // A directory ends with the platform's separator.
    let app = format!("app{}", std::path::MAIN_SEPARATOR);
    assert_eq!(
        fixture.values("", &["cheby", "check", ""]),
        ["probe.cheby", app.as_str(), "-"]
    );
    assert_eq!(fixture.values("", &["cheby", "run", "pr"]), ["probe.cheby"]);
    assert_eq!(
        fixture.values("", &["cheby", "fmt", "probe.cheby", "p"]),
        ["probe.cheby"]
    );
    // `cheby test` reads no source from standard input.
    assert_eq!(
        fixture.values("", &["cheby", "test", ""]),
        ["probe.cheby", app.as_str()]
    );
}

/// `cheby test` completes the names of the package's tests, each once,
/// with where it is, and after a file the names of that file's tests.
#[test]
fn names_of_tests() {
    let fixture = Fixture::new();
    let tests = fixture.ask("app", &["cheby", "test", "a"]);
    assert_eq!(
        tests,
        [
            ("adds".to_owned(), "test of app".to_owned()),
            ("adds twice".to_owned(), "test of app".to_owned()),
        ]
    );
    assert_eq!(
        fixture.ask("app", &["cheby", "test", "d"]),
        [("doubles (small)".to_owned(), "test of app::util".to_owned())]
    );
    assert_eq!(
        fixture.values("", &["cheby", "test", "probe.cheby", ""]),
        ["probes"]
    );
    assert_eq!(
        fixture.values("app", &["cheby", "test", "../probe.cheby", "pr"]),
        ["probes"]
    );
}

/// `cheby doc` completes modules and items of std from any directory, and
/// those of the package inside it.
#[test]
fn doc_paths() {
    let fixture = Fixture::new();
    let std_only = fixture.values("", &["cheby", "doc", "list::max"]);
    assert!(std_only.contains(&"list::max".to_owned()), "{std_only:?}");
    assert!(
        std_only.iter().all(|path| path.starts_with("list::max")),
        "{std_only:?}"
    );
    let modules = fixture.values("", &["cheby", "doc", "std::li"]);
    assert_eq!(modules, ["std::list"]);
    let about = fixture.ask("", &["cheby", "doc", "std::list::map"]);
    assert!(
        about
            .iter()
            .any(|(path, about)| path == "std::list::map" && !about.is_empty()),
        "{about:?}"
    );
    assert_eq!(
        fixture.ask("app", &["cheby", "doc", "app::u"]),
        [("app::util".to_owned(), "Small helpers.".to_owned())]
    );
    assert_eq!(
        fixture.ask("app", &["cheby", "doc", "app::util::"]),
        [("app::util::twice".to_owned(), "Twice `n`.".to_owned())]
    );
    // Outside the package its modules are not known.
    assert_eq!(
        fixture.values("", &["cheby", "doc", "app"]),
        Vec::<String>::new()
    );
}

/// Bash gives the words split at `:` and `=` and replaces only what
/// follows the last of them, and it inserts a candidate as it is written
/// (D-1960).
#[test]
fn bash_gets_what_it_replaces() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.ask_bash("app", &["cheby", "run", "app", "::", "u"], 4),
        ["util"]
    );
    assert_eq!(
        fixture.ask_bash("app", &["cheby", "build", "app", "::"], 3),
        ["util"]
    );
    assert_eq!(
        fixture.ask_bash("", &["cheby", "doc", "std", "::", "li"], 4),
        ["list"]
    );
    assert_eq!(
        fixture.ask_bash("", &["cheby", "check", "--target", "=", "na"], 4),
        ["native"]
    );
    assert_eq!(fixture.ask_bash("", &["cheby", "comp"], 1), ["completions"]);
    assert_eq!(
        fixture.ask_bash("app", &["cheby", "test", "d"], 2),
        [r"doubles\ \(small\)"]
    );
    assert_eq!(
        fixture.ask_bash("app", &["cheby", "test", r"adds\ t"], 2),
        [r"adds\ twice"]
    );
    assert_eq!(
        fixture.ask_bash("app", &["cheby", "test", "\"d"], 2),
        ["\"doubles (small)"]
    );
    // After a space the word to complete is a new one.
    let next = fixture.ask_bash("app", &["cheby", "run", "app", "::", "util", ""], 5);
    assert!(next.contains(&"--release".to_owned()), "{next:?}");
    assert!(!next.contains(&"util".to_owned()), "{next:?}");
    assert_eq!(
        strings(&["util"]),
        fixture.ask_bash("app", &["cheby", "run", "app::u"], 2)
    );
}
