//! `--format json`, `cheby check --fix` and `cheby test
//! --report-failures-only` end to end (D-1944, D-1945, D-1946).
//!
//! With `--format json`, diagnostics and tool errors are JSON lines on
//! standard error and test results JSON lines on standard output. `--fix`
//! applies the helps that are marked as sure fixes, checks again, and leaves
//! guesses and helps that silence a warning alone.
//!
//! Every package gets a fresh `CHEBY_CACHE` of its own (D-1128).
#![expect(
    clippy::unwrap_used,
    reason = "test helpers: a failed setup step fails the test"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::{CACHE_ENV, FreshDir};
use cheby_test_support::process::{self, RunOutput};
use serde_json::Value;

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(300);

/// A package `app` in a fresh directory, with `main` as its root module.
struct Package {
    dir: FreshDir,
    cache: FreshDir,
}

impl Package {
    fn new(main: &str) -> Self {
        let package = Self {
            dir: FreshDir::create("cli-format").unwrap(),
            cache: FreshDir::create("cli-format-cache").unwrap(),
        };
        std::fs::create_dir_all(package.root().join("src")).unwrap();
        std::fs::write(
            package.root().join("cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(package.main(), main).unwrap();
        package
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn main(&self) -> PathBuf {
        self.root().join("src/main.cheby")
    }

    fn text(&self) -> String {
        std::fs::read_to_string(self.main()).unwrap()
    }

    fn cheby(&self, args: &[&str]) -> RunOutput {
        cheby_in(self.root(), self.cache.path(), args)
    }
}

fn cheby_in(dir: &Path, cache: &Path, args: &[&str]) -> RunOutput {
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never")
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, cache)
        .env_remove("CHEBY_DUMP_DIR")
        .env_remove("CHEBY_PHASES");
    process::run(cmd, None, TIMEOUT).unwrap()
}

/// Every line of `text` as a JSON object.
fn lines(text: &str) -> Vec<Value> {
    text.lines()
        .map(|line| {
            let value = serde_json::from_str::<Value>(line);
            assert!(value.as_ref().is_ok_and(Value::is_object), "{line}");
            value.unwrap()
        })
        .collect()
}

/// Habits of other languages with sure fixes: a type word, a `;`, `var`,
/// single quotes and `->` in an arm, which are grammar errors, and `true`,
/// which only name resolution finds once they are gone (D-452). The unused
/// binding has a help that would only silence it.
const HABITS: &str = "//! Habits.

import std::io

pub fn double(x: int) -> Int {
  x * 2;
}

fn main() -> Nil {
  var flag = true
  let name = 'world'
  let unused = 1
  case flag {
    True -> io::println(\"hello {name}\")
    False => io::println(\"{double(2)}\")
  }
}
";

const FIXED: &str = "//! Habits.

import std::io

pub fn double(x: Int) -> Int {
  x * 2
}

fn main() -> Nil {
  let flag = True
  let name = \"world\"
  let unused = 1
  case flag {
    True => io::println(\"hello {name}\")
    False => io::println(\"{double(2)}\")
  }
}
";

/// Each diagnostic is one JSON object on standard error with the parts of
/// the text form, and nothing is written to standard output.
#[test]
fn diagnostics_are_json_lines_on_standard_error() {
    let package = Package::new(HABITS);
    let out = package.cheby(&["check", "--format", "json"]);
    assert_eq!(out.status, Some(1), "{}", out.stderr);
    assert_eq!(out.stdout, "");
    let found = lines(&out.stderr);
    assert!(found.iter().all(|d| d["type"] == "diagnostic"));
    assert!(found.iter().all(|d| d["severity"] == "error"));
    let codes: Vec<&str> = found.iter().map(|d| d["code"].as_str().unwrap()).collect();
    assert_eq!(
        codes,
        ["E0013", "E0003", "E0013", "E0003", "E0013", "E0012"]
    );
    // `int` at 5:18, bytes 46 to 49, with the edit of its help.
    let int = &found[0];
    assert_eq!(int["message"], "expected a type, found identifier");
    assert_eq!(int["file"], "src/main.cheby");
    assert_eq!(
        (
            &int["line"],
            &int["column"],
            &int["end_line"],
            &int["end_column"]
        ),
        (
            &Value::from(5),
            &Value::from(18),
            &Value::from(5),
            &Value::from(21)
        )
    );
    assert_eq!(
        (&int["start"], &int["end"]),
        (&Value::from(46), &Value::from(49))
    );
    assert_eq!(int["labels"][0]["primary"], true);
    let help = &int["helps"][0];
    assert_eq!(
        help["message"],
        "type names start with an upper-case letter; write `Int`"
    );
    assert_eq!(help["fix"], true);
    assert_eq!(help["edits"][0]["replacement"], "Int");
    assert_eq!(
        (&help["edits"][0]["start"], &help["edits"][0]["end"]),
        (&Value::from(46), &Value::from(49))
    );
    // The quotes' fix has two edits, and E0012's help has none.
    assert_eq!(found[3]["helps"][0]["edits"].as_array().unwrap().len(), 2);
    assert_eq!(found[5]["helps"][0]["fix"], false);
    assert_eq!(found[5]["helps"][0]["edits"].as_array().unwrap().len(), 0);
    // The text form is the default, and the flag is global.
    let text = package.cheby(&["check"]);
    assert!(
        text.stderr.starts_with("error[E0013]: expected a type"),
        "{}",
        text.stderr
    );
    let before = package.cheby(&["--format=json", "check"]);
    assert_eq!(before.stderr, out.stderr);
}

/// `--fix` applies the sure fixes, checks again and applies those the
/// first round uncovered (D-452). What is left is reported as always.
#[test]
fn fix_applies_sure_fixes_round_after_round() {
    let package = Package::new(HABITS);
    let out = package.cheby(&["check", "--fix"]);
    // E0012 has no fix, so the later phases never ran: `true` stays.
    assert_eq!(out.status, Some(1), "{}", out.stderr);
    let fixed: Vec<&str> = out
        .stderr
        .lines()
        .filter(|line| line.starts_with("fixed: "))
        .collect();
    assert_eq!(
        fixed,
        [
            "fixed: src/main.cheby:5:18: type names start with an upper-case letter; write `Int`",
            "fixed: src/main.cheby:6:8: statements are separated by line breaks, not `;`",
            "fixed: src/main.cheby:10:3: Cheby has no `var`; write `let`",
            "fixed: src/main.cheby:11:14: strings use double quotes, such as \"text\"",
            "fixed: src/main.cheby:14:10: a `case` arm uses `=>`; write `=>`",
        ]
    );
    assert!(
        out.stderr
            .contains("error[E0012]: unexpected `(` in string interpolation")
    );
    assert_eq!(
        package.text(),
        FIXED.replace("let flag = True", "let flag = true")
    );
}

/// Once the grammar errors are gone, one call fixes `true`, and the help
/// that would only silence the unused binding is not applied.
#[test]
#[cfg_attr(
    windows,
    ignore = "a check that passes evaluates constants with the JIT (D-545)"
)]
fn fix_reaches_the_later_phases_and_leaves_other_helps() {
    let source = HABITS.replace("\"{double(2)}\"", "\"no\"");
    let package = Package::new(&source);
    let out = package.cheby(&["check", "--fix", "--format", "json"]);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    let found = lines(&out.stderr);
    let kinds: Vec<&str> = found.iter().map(|d| d["type"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["fix", "fix", "fix", "fix", "fix", "fix", "diagnostic"]
    );
    let last_fix = &found[5];
    assert_eq!(last_fix["code"], "E0042");
    assert_eq!(
        (&last_fix["file"], &last_fix["line"], &last_fix["column"]),
        (
            &Value::from("src/main.cheby"),
            &Value::from(10),
            &Value::from(14)
        )
    );
    let warning = &found[6];
    assert_eq!(
        (&warning["code"], &warning["severity"]),
        (&Value::from("W0002"), &Value::from("warning"))
    );
    assert_eq!(warning["helps"][0]["fix"], false);
    assert_eq!(package.text(), FIXED.replace("\"{double(2)}\"", "\"no\""));
    // Nothing is left to fix: the file stays as it is.
    let again = package.cheby(&["check", "--fix"]);
    assert_eq!(again.status, Some(0));
    assert!(!again.stderr.contains("fixed: "), "{}", again.stderr);
    assert_eq!(package.text(), FIXED.replace("\"{double(2)}\"", "\"no\""));
    // A denied warning is an error under its own code, with the note.
    let denied = package.cheby(&["check", "--deny-warnings", "--format", "json"]);
    assert_eq!(denied.status, Some(1));
    let found = lines(&denied.stderr);
    assert_eq!(
        (&found[0]["code"], &found[0]["severity"]),
        (&Value::from("W0002"), &Value::from("error"))
    );
    assert_eq!(
        found[0]["notes"][0],
        "warnings are denied by `--deny-warnings`"
    );
}

/// A guess among names and the rename of a declaration are not sure
/// fixes: their helps have edits, for an editor to offer, and no mark, and
/// `--fix` writes nothing.
#[test]
fn fix_leaves_guesses_alone() {
    let guess = "//! A guess.

import std::io

fn main() -> Nil {
  let total = 1
  io::println(\"{totl}\")
}
";
    let rename = "//! A rename.

fn main() -> Nil {
  let myTotal = 1
  let _ = myTotal
}
";
    for (source, code) in [(guess, "E0042"), (rename, "E0006")] {
        let package = Package::new(source);
        let out = package.cheby(&["check", "--fix", "--format", "json"]);
        assert_eq!(out.status, Some(1), "{}", out.stderr);
        let found = lines(&out.stderr);
        assert!(
            found.iter().all(|d| d["type"] == "diagnostic"),
            "{}",
            out.stderr
        );
        let helps: Vec<&Value> = found
            .iter()
            .filter(|d| d["code"] == code)
            .flat_map(|d| d["helps"].as_array().unwrap())
            .filter(|help| !help["edits"].as_array().unwrap().is_empty())
            .collect();
        assert!(!helps.is_empty(), "{code}: {}", out.stderr);
        assert!(helps.iter().all(|help| help["fix"] == false), "{code}");
        assert_eq!(package.text(), source, "{code}");
    }
}

/// A tool error is a JSON line too, and usage errors stay text.
#[test]
fn tool_errors_are_json_lines() {
    let dir = FreshDir::create("cli-format-empty").unwrap();
    let cache = FreshDir::create("cli-format-cache").unwrap();
    let out = cheby_in(dir.path(), cache.path(), &["check", "--format", "json"]);
    assert_eq!(out.status, Some(1));
    assert_eq!(out.stdout, "");
    assert_eq!(
        out.stderr,
        "{\"type\":\"error\",\"message\":\"could not find `cheby.toml` in the current directory or any parent\"}\n"
    );
    let usage = cheby_in(dir.path(), cache.path(), &["check", "--format", "yaml"]);
    assert_eq!(usage.status, Some(2));
    assert!(
        usage.stderr.starts_with("error: invalid value 'yaml'"),
        "{}",
        usage.stderr
    );
}

const TESTS: &str = "//! Tests.

import std::io

fn double(x: Int) -> Int {
  x * 2
}

test \"doubles\" {
  io::println(\"from the test\")
  assert double(2) == 4
}

test \"fails\" {
  io::eprintln(\"to standard error\")
  assert double(2) == 5
}
";

/// A test's line has its captured output and its failure, and a summary
/// line ends the results, all on standard output.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT code, which Windows cannot until M6 (D-545)"
)]
fn results_are_json_lines_on_standard_output() {
    let package = Package::new(TESTS);
    let out = package.cheby(&["test", "--format", "json"]);
    assert_eq!(out.status, Some(1), "{}", out.stderr);
    assert_eq!(out.stderr, "");
    assert_eq!(
        out.stdout,
        concat!(
            r#"{"type":"test","module":"app","name":"doubles","outcome":"passed","stdout":"from the test\n"}"#,
            "\n",
            r#"{"type":"test","module":"app","name":"fails","outcome":"failed","stderr":"to standard error\n","#,
            r#""failure":"panicked at src/main.cheby:16:10:\nassertion failed: double(2) == 5\n  left: 4\n right: 5\n"}"#,
            "\n",
            r#"{"type":"summary","passed":1,"failed":0,"filtered_out":0}"#,
            "\n"
        )
        .replace(r#""passed":1,"failed":0"#, r#""passed":1,"failed":1"#)
    );
    lines(&out.stdout);
}

/// `--report-failures-only` writes no line for a passing test and drops
/// its output, in both formats; the summary still counts it.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT code, which Windows cannot until M6 (D-545)"
)]
fn failures_only_leaves_passing_tests_out() {
    let package = Package::new(TESTS);
    let text = package.cheby(&["test", "--report-failures-only"]);
    assert_eq!(text.status, Some(1));
    assert_eq!(text.stderr, "to standard error\n");
    assert_eq!(
        text.stdout,
        "test app \"fails\" ... FAILED\n\n---- app \"fails\" ----\npanicked at \
         src/main.cheby:16:10:\nassertion failed: double(2) == 5\n  left: 4\n right: 5\n\ntest \
         result: FAILED. 1 passed; 1 failed; 0 filtered out\n"
    );
    let json = package.cheby(&["test", "--report-failures-only", "--format", "json"]);
    let found = lines(&json.stdout);
    let kinds: Vec<(&str, &str)> = found
        .iter()
        .map(|d| {
            (
                d["type"].as_str().unwrap(),
                d["name"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(kinds, [("test", "fails"), ("summary", "")]);
    // With only passing tests selected, the summary is the whole output.
    let passing = package.cheby(&["test", "--report-failures-only", "doubles"]);
    assert_eq!(passing.status, Some(0));
    assert_eq!(
        passing.stdout,
        "test result: ok. 1 passed; 0 failed; 1 filtered out\n"
    );
}
