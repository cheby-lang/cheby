//! The later phases for the modules that a syntax error does not reach
//! (D-1951), end to end: every command that stops on compile errors prints
//! the same diagnostics and the same note, `--format json` has the note as
//! a line, and `cheby check --fix` already fixes what the later phases
//! found in the modules that parse. The UI test
//! `13-tooling/diagnostics/clean_modules_after_syntax_error` pins the text.
#![expect(
    clippy::unwrap_used,
    reason = "test helpers: a failed setup step fails the test"
)]

use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::{CACHE_ENV, FreshDir};
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(300);

const MAIN: &str = "//! Root: imports the module that does not parse.

import app::parser
import app::util

fn main() {
  let wrong: Int = parser::parse(util::flag())
}
";

const PARSER: &str = "//! Does not parse.

fn parse(flag: Bool) -> String {
  let x = (flag
  x
}
";

const UTIL: &str = "//! Parses, with an error of name resolution.

fn flag() -> Bool {
  true
}
";

/// The package `app` in a fresh directory, with a build cache.
struct Package {
    dir: FreshDir,
    cache: FreshDir,
}

impl Package {
    fn new(files: &[(&str, &str)]) -> Self {
        let package = Self {
            dir: FreshDir::create("cli-later").unwrap(),
            cache: FreshDir::create("cli-later-cache").unwrap(),
        };
        let root = package.dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        for (file, text) in files {
            std::fs::write(root.join("src").join(file), text).unwrap();
        }
        package
    }

    fn cheby(&self, args: &[&str]) -> RunOutput {
        let mut cmd = Command::new(CHEBY);
        cmd.arg("--color=never")
            .args(args)
            .current_dir(self.dir.path())
            .env("NO_COLOR", "1")
            .env(CACHE_ENV, self.cache.path());
        process::run(cmd, None, TIMEOUT).unwrap()
    }
}

/// The codes and the note of a command's standard error, in order.
fn headlines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|line| {
            line.starts_with("error") || line.starts_with("warning") || line.starts_with("note")
        })
        .collect()
}

/// `util` parses and imports nothing, so its error is reported beside the
/// syntax errors of `parser`; the root imports `parser`, so it is named
/// in the note and its own error is not reported. `check`, `build`,
/// `run` and `test` agree, twice, the second time with a warm cache. On
/// Windows `cheby build --target native` refuses before it reads the
/// package until M6 (D-545), so `build` is left out there.
#[test]
fn every_command_reports_the_modules_that_parse() {
    let package = Package::new(&[
        ("main.cheby", MAIN),
        ("parser.cheby", PARSER),
        ("util.cheby", UTIL),
    ]);
    let expected = [
        "error[E0013]: expected `,` or `)`, found identifier",
        "error[E0015]: unclosed `(`",
        "error[E0042]: cannot find value `true` in this scope",
        "note: 1 module was not checked, because it imports a module with syntax errors: app",
    ];
    let check = package.cheby(&["check"]);
    assert_eq!(check.status, Some(1));
    assert_eq!(headlines(&check.stderr), expected, "{}", check.stderr);
    let commands: &[&str] = if cfg!(windows) {
        &["check", "run", "test"]
    } else {
        &["check", "build", "run", "test"]
    };
    for _ in 0..2 {
        for &command in commands {
            let out = package.cheby(&[command]);
            assert_eq!(out.status, Some(1), "{command}");
            assert_eq!(out.stderr, check.stderr, "{command}");
            assert_eq!(out.stdout, "", "{command}");
        }
    }
    let json = package.cheby(&["check", "--format", "json"]);
    let last = json.stderr.lines().last().unwrap();
    assert_eq!(
        last,
        "{\"type\":\"note\",\"message\":\"1 module was not checked, because it imports a module \
         with syntax errors: app\"}"
    );
    assert_eq!(json.stderr.lines().count(), 4, "{}", json.stderr);
}

/// `--fix` applies the sure fix of a module that parses in the round
/// that still has the syntax error elsewhere.
#[test]
fn fix_reaches_the_modules_that_parse() {
    let package = Package::new(&[
        ("main.cheby", MAIN),
        ("parser.cheby", PARSER),
        ("util.cheby", UTIL),
    ]);
    let out = package.cheby(&["check", "--fix"]);
    assert_eq!(out.status, Some(1));
    assert!(
        out.stderr.starts_with(
            "fixed: src/util.cheby:4:3: `Bool`'s constructors are `True` and `False`; write \
             `True`\n"
        ),
        "{}",
        out.stderr
    );
    let util = std::fs::read_to_string(package.dir.path().join("src/util.cheby")).unwrap();
    assert_eq!(util, UTIL.replace("  true\n", "  True\n"));
    assert_eq!(
        headlines(&out.stderr),
        [
            "error[E0013]: expected `,` or `)`, found identifier",
            "error[E0015]: unclosed `(`",
            "note: 1 module was not checked, because it imports a module with syntax errors: app",
        ]
    );
}

/// Nothing changes where no module is left to check or something else
/// than a module is broken: a package of one module, every module broken,
/// and a broken manifest.
#[test]
fn the_stop_stays_where_nothing_is_left_to_check() {
    let alone = Package::new(&[(
        "main.cheby",
        "//! Alone.\n\nfn main() {\n  let x = (1\n  true\n}\n",
    )]);
    let out = alone.cheby(&["check"]);
    assert_eq!(headlines(&out.stderr).len(), 2, "{}", out.stderr);
    assert!(
        !out.stderr.contains("E0042") && !out.stderr.contains("note:"),
        "{}",
        out.stderr
    );
    // Every module imports the broken one: only the note is new.
    let all = Package::new(&[("main.cheby", MAIN), ("parser.cheby", PARSER)]);
    let out = all.cheby(&["check"]);
    assert_eq!(
        headlines(&out.stderr)[2..],
        ["note: 1 module was not checked, because it imports a module with syntax errors: app"],
        "{}",
        out.stderr
    );
    let manifest = Package::new(&[
        ("main.cheby", MAIN),
        ("parser.cheby", PARSER),
        ("util.cheby", UTIL),
    ]);
    std::fs::write(manifest.dir.path().join("cheby.toml"), "name = \"app\"\n").unwrap();
    let out = manifest.cheby(&["check"]);
    assert_eq!(out.status, Some(1));
    assert!(
        !out.stderr.contains("E0042") && !out.stderr.contains("note:"),
        "{}",
        out.stderr
    );
}
