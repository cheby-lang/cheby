//! Code records from the command line (K4c: D-914, D-1192, D-1193, D-958,
//! D-965, D-972, D-1128): a second `cheby run`, `cheby test` and `cheby
//! check` on one cache load every package module's code from its record
//! and print what the first printed; a body edit misses only its module;
//! `CHEBY_RC=naive` keys records of its own; `--reuse-report` prints the
//! same warm as cold; two copies of a package under one basename write
//! the same records and blobs; a type error outside the root prints what
//! `cheby check` prints, cold and warm; and a test named like a function
//! does not stop a warm run.
//!
//! Every pair of commands shares one fresh `CHEBY_CACHE` (D-1128), set on
//! each child's `Command`; nothing here sets a variable of the test process.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use cheby_test_support::cache::CACHE_ENV;
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
const TIMEOUT: Duration = Duration::from_secs(300);

/// A fresh directory of this test binary, removed on drop.
struct Scratch(PathBuf);

#[expect(
    clippy::unwrap_used,
    reason = "a test helper: a failing file operation fails the test"
)]
impl Scratch {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = Path::new(TMP)
            .join("code_cache")
            .join(format!("{name}-{}-{n}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The root: one constant, and `main` prints what the other modules
/// compute.
const MAIN: &str = "//! Root: prints what its modules compute.

import app::other
import app::util
import std::io

const greeting: String = \"warm\"

/// One boxed number: building it is a line of `--reuse-report`.
type Box { Box(Int) }

fn boxed(n: Int) -> Int {
  let Box(m) = Box(n)
  m
}

fn main() {
  let ok = util::check(\"x\")
  let n = other::twice(other::base)
  let m = boxed(n)
  io::println(\"{greeting} {ok} {n} {m}\")
}

test \"greets\" {
  assert greeting == \"warm\"
}

test \"boxes\" {
  assert boxed(2) == 2
}
";

/// `check` is package-visible and only compares its parameter: borrowed.
const UTIL: &str = "//! Util: a check that borrows its parameter.

const expected: String = \"x\"

fn check(s: String) -> Bool {
  s == expected
}

test \"checks\" {
  assert check(\"x\")
}
";

/// A body edit of `check` that changes no mask.
const UTIL_EDITED: &str = "//! Util: a check that borrows its parameter.

const expected: String = \"x\"

fn check(s: String) -> Bool {
  expected == s
}

test \"checks\" {
  assert check(\"x\")
}
";

const OTHER: &str = "//! Other: doubling.

pub const base: Int = 21

pub fn twice(n: Int) -> Int {
  n * 2
}

test \"doubles\" {
  assert twice(base) == 42
}
";

/// The package's modules, as the cache log names them.
const MODULES: [&str; 3] = ["app", "app::other", "app::util"];

/// Writes the package `app` of three modules, each with a constant (so
/// every module is in the constants' module set `cheby check` compiles,
/// D-1187, D-1180) and a test, into `dir`.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to write the package fails the test"
)]
fn package(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("cheby.toml"),
        "name = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    for (file, text) in [
        ("main.cheby", MAIN),
        ("util.cheby", UTIL),
        ("other.cheby", OTHER),
    ] {
        std::fs::write(dir.join("src").join(file), text).unwrap();
    }
    dir.to_path_buf()
}

/// One `cheby` command's output and the lines of its `CHEBY_CACHE_LOG`.
struct Ran {
    out: RunOutput,
    log: Vec<String>,
}

/// `cheby <args>` in `dir` with `CHEBY_CACHE` set to `cache`, its
/// `CHEBY_CACHE_LOG` in `log`, and `env` added.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to spawn `cheby` or read its log fails the test"
)]
fn cheby(dir: &Path, args: &[&str], cache: &Path, log: &Path, env: &[(&str, &str)]) -> Ran {
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never")
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, cache)
        .env("CHEBY_CACHE_LOG", log)
        .env_remove("CHEBY_DUMP_DIR")
        .env_remove("CHEBY_RC");
    for (name, value) in env {
        cmd.env(name, value);
    }
    let out = process::run(cmd, None, TIMEOUT).unwrap();
    let log = std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    Ran { out, log }
}

/// The code-record lookups of a log of the build kind `kind`
/// (`code.<kind> …` and `tests.<kind> …` lines, plan §2.2): `run`, `test`,
/// or `plain` for `cheby check`'s and every evaluation module's (D-1187).
fn code_lookups(log: &[String], kind: &str) -> Vec<String> {
    let (code, tests) = (format!("code.{kind} "), format!("tests.{kind} "));
    log.iter()
        .filter(|l| l.starts_with(&code) || l.starts_with(&tests))
        .cloned()
        .collect()
}

/// `<artifact> <module> <verdict>` for each package module.
fn every(artifact: &str, verdict: &str, tests: bool) -> Vec<String> {
    MODULES
        .iter()
        .map(|m| {
            if tests {
                format!("{artifact} {m}#tests {verdict}")
            } else {
                format!("{artifact} {m} {verdict}")
            }
        })
        .collect()
}

/// The two runs of `args` on one fresh cache: the first, then the second.
fn twice(scratch: &Scratch, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> (Ran, Ran) {
    let cache = scratch.path().join(format!("cache-{}", args.join("-")));
    let first = cheby(dir, args, &cache, &scratch.path().join("log-1"), env);
    let second = cheby(dir, args, &cache, &scratch.path().join("log-2"), env);
    (first, second)
}

/// Asserts that `second` printed and exited as `first` did.
fn same_output(first: &Ran, second: &Ran) {
    assert_eq!(second.out.status, first.out.status, "{}", second.out.stderr);
    assert_eq!(second.out.stdout, first.out.stdout);
    assert_eq!(second.out.stderr, first.out.stderr);
}

/// D-914, D-1193: a second `cheby run` loads every package module's code
/// record (a `code.run` hit each, no miss) and prints what the first
/// printed, which missed every one.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_cache_warm_run_loads_every_module() {
    let scratch = Scratch::new("run");
    let dir = package(&scratch.path().join("app"));
    let (first, second) = twice(&scratch, &dir, &["run"], &[]);
    assert_eq!(first.out.status, Some(0), "{}", first.out.stderr);
    assert_eq!(first.out.stdout, "warm True 42 42\n");
    assert_eq!(
        code_lookups(&first.log, "run"),
        every("code.run", "miss", false)
    );
    assert_eq!(
        code_lookups(&second.log, "run"),
        every("code.run", "hit", false)
    );
    // Its evaluation module (D-1187) loads its `plain` records too, unless
    // every constant's image hits and no evaluation module is built (K5b,
    // D-977).
    let plain = code_lookups(&second.log, "plain");
    if !plain.is_empty() {
        assert_eq!(plain, every("code.plain", "hit", false));
    }
    same_output(&first, &second);
}

/// D-1179, D-1193: a second `cheby test` loads every code unit's (`test`
/// kind) and tests unit's record, and prints the same report; so does a
/// filtered run after them, from the same records.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_cache_warm_test_loads_every_module() {
    let scratch = Scratch::new("test");
    let dir = package(&scratch.path().join("app"));
    let (first, second) = twice(&scratch, &dir, &["test"], &[]);
    assert_eq!(first.out.status, Some(0), "{}", first.out.stderr);
    let mut cold = every("code.test", "miss", false);
    cold.extend(every("tests.test", "miss", true));
    cold.sort();
    let mut warm = every("code.test", "hit", false);
    warm.extend(every("tests.test", "hit", true));
    warm.sort();
    assert_eq!(code_lookups(&first.log, "test"), cold);
    assert_eq!(code_lookups(&second.log, "test"), warm);
    same_output(&first, &second);
    // A filter selects one test of `app::util`; its tests unit is whole, so
    // the record serves it (D-970), and it names the test as a cold run
    // with the filter does.
    let cache = scratch.path().join("cache-test");
    let filtered = cheby(
        &dir,
        &["test", "checks"],
        &cache,
        &scratch.path().join("log-3"),
        &[],
    );
    let fresh = cheby(
        &dir,
        &["test", "checks"],
        &scratch.path().join("cache-fresh"),
        &scratch.path().join("log-4"),
        &[],
    );
    assert!(
        code_lookups(&filtered.log, "test").contains(&"tests.test app::util#tests hit".to_owned()),
        "{:?}",
        filtered.log
    );
    same_output(&fresh, &filtered);
}

/// D-1187, D-1180: a second `cheby check` loads the `plain` record of every
/// module of the constants' module set (all three hold a constant) and
/// prints what the first printed.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_cache_warm_check_loads_the_constants_modules() {
    let scratch = Scratch::new("check");
    let dir = package(&scratch.path().join("app"));
    let (first, second) = twice(&scratch, &dir, &["check"], &[]);
    assert_eq!(first.out.status, Some(0), "{}", first.out.stderr);
    assert_eq!(
        code_lookups(&first.log, "plain"),
        every("code.plain", "miss", false)
    );
    assert_eq!(
        code_lookups(&second.log, "plain"),
        every("code.plain", "hit", false)
    );
    same_output(&first, &second);
}

/// D-812's case without a mask change: a body edit misses only the edited
/// module's record, and the run prints as before.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_cache_body_edit_misses_only_its_module() {
    let scratch = Scratch::new("edit");
    let dir = package(&scratch.path().join("app"));
    let (first, _) = twice(&scratch, &dir, &["run"], &[]);
    std::fs::write(dir.join("src/util.cheby"), UTIL_EDITED).unwrap();
    let edited = cheby(
        &dir,
        &["run"],
        &scratch.path().join("cache-run"),
        &scratch.path().join("log-3"),
        &[],
    );
    assert_eq!(
        code_lookups(&edited.log, "run"),
        [
            "code.run app hit",
            "code.run app::other hit",
            "code.run app::util miss",
        ]
    );
    same_output(&first, &edited);
}

/// D-958: `CHEBY_RC=naive` is a field of the code key, so a warm naive run
/// hits the naive records, and a default run on the same cache misses
/// them; each prints the same.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_cache_naive_records_are_their_own() {
    let scratch = Scratch::new("naive");
    let dir = package(&scratch.path().join("app"));
    let naive = [("CHEBY_RC", "naive")];
    let (cold, warm) = twice(&scratch, &dir, &["run"], &naive);
    assert_eq!(
        code_lookups(&cold.log, "run"),
        every("code.run", "miss", false)
    );
    assert_eq!(
        code_lookups(&warm.log, "run"),
        every("code.run", "hit", false)
    );
    same_output(&cold, &warm);
    let cache = scratch.path().join("cache-run");
    let default = cheby(&dir, &["run"], &cache, &scratch.path().join("log-3"), &[]);
    assert_eq!(
        code_lookups(&default.log, "run"),
        every("code.run", "miss", false)
    );
    same_output(&cold, &default);
    let again = cheby(&dir, &["run"], &cache, &scratch.path().join("log-4"), &[]);
    assert_eq!(
        code_lookups(&again.log, "run"),
        every("code.run", "hit", false)
    );
}

/// D-852, D-268, D-965 (K4d, refining D-1223): `cheby run
/// --reuse-report` and `cheby test --reuse-report` print the same warm as
/// cold. The report reads code and tests records like any command: the
/// second command loads every package unit from its record (a hit each)
/// and replays the reuse lines cached beside it.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_cache_reuse_report_warm_as_cold() {
    let scratch = Scratch::new("reuse");
    let dir = package(&scratch.path().join("app"));
    for (args, kind, tests) in [
        (["run", "--reuse-report"], "run", false),
        (["test", "--reuse-report"], "test", true),
    ] {
        let (first, second) = twice(&scratch, &dir, &args, &[]);
        assert_eq!(first.out.status, Some(0), "{}", first.out.stderr);
        assert!(
            first.out.stdout.contains("reuse:") || first.out.stderr.contains("reuse:"),
            "{}{}",
            first.out.stdout,
            first.out.stderr
        );
        same_output(&first, &second);
        let artifact = format!("code.{kind}");
        let mut hits = every(&artifact, "hit", false);
        if tests {
            hits.extend(every(&format!("tests.{kind}"), "hit", true));
            hits.sort();
        }
        assert_eq!(code_lookups(&second.log, kind), hits);
    }
}

/// K4d (D-965): a plain `cheby run` writes no reuse lines, so a warm `cheby
/// run --reuse-report` after it lowers every unit again (a miss each),
/// prints what a cold report prints, and writes the lines; a third command
/// then loads every unit from its record.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_cache_reuse_report_after_a_plain_run() {
    let scratch = Scratch::new("reuse-plain");
    let dir = package(&scratch.path().join("app"));
    let cache = scratch.path().join("cache");
    let cold = cheby(
        &dir,
        &["run", "--reuse-report"],
        &scratch.path().join("cache-cold"),
        &scratch.path().join("log-0"),
        &[],
    );
    let plain = cheby(&dir, &["run"], &cache, &scratch.path().join("log-1"), &[]);
    assert_eq!(plain.out.status, Some(0), "{}", plain.out.stderr);
    let report = ["run", "--reuse-report"];
    let second = cheby(&dir, &report, &cache, &scratch.path().join("log-2"), &[]);
    same_output(&cold, &second);
    assert_eq!(
        code_lookups(&second.log, "run"),
        every("code.run", "miss", false)
    );
    let third = cheby(&dir, &report, &cache, &scratch.path().join("log-3"), &[]);
    same_output(&cold, &third);
    assert_eq!(
        code_lookups(&third.log, "run"),
        every("code.run", "hit", false)
    );
}

/// The files under `v1/<kind>/` of `cache`, by name relative to it, with
/// their bytes, sorted.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: an unreadable cache fails the test"
)]
fn entries(cache: &Path, kind: &str) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let dir = cache.join("v1").join(kind);
    for shard in std::fs::read_dir(&dir).unwrap() {
        for file in std::fs::read_dir(shard.unwrap().path()).unwrap() {
            let path = file.unwrap().path();
            let name = path.strip_prefix(&dir).unwrap().display().to_string();
            out.push((name, std::fs::read(&path).unwrap()));
        }
    }
    out.sort();
    out
}

/// D-264, D-972, plan §2.3: `cheby run` and `cheby test` of one package
/// from two directories under one basename, each with a fresh cache, write
/// the same `v1/rec` and `v1/blob` entries byte for byte, code and tests
/// records among them (item 11 compares only `cheby check`'s caches).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_cache_records_identical_from_two_paths() {
    let scratch = Scratch::new("paths");
    let a = package(&scratch.path().join("one").join("pkg"));
    let b = package(&scratch.path().join("two").join("longer").join("pkg"));
    let (cache_a, cache_b) = (
        scratch.path().join("cache-a"),
        scratch.path().join("cache-b"),
    );
    for args in [["run"], ["test"]] {
        let ran_a = cheby(&a, &args, &cache_a, &scratch.path().join("log-a"), &[]);
        let ran_b = cheby(&b, &args, &cache_b, &scratch.path().join("log-b"), &[]);
        assert_eq!(ran_a.out.status, Some(0), "{}", ran_a.out.stderr);
        same_output(&ran_a, &ran_b);
    }
    let records = entries(&cache_a, "rec");
    assert_ne!(records, Vec::new());
    assert_eq!(records, entries(&cache_b, "rec"));
    assert_eq!(entries(&cache_a, "blob"), entries(&cache_b, "blob"));
}

// D-1250 (K4d): a warm `cheby run` and `cheby test` parse and check only
// what misses.

/// The root of the warm-check package: it calls `numbers::three` and
/// `words::greeting`, and holds no test.
const WARM_MAIN: &str = "//! Root: prints what its modules give.

import std::io
import warm::numbers
import warm::words

fn main() {
  let n = numbers::three()
  let g = words::greeting()
  io::println(\"{n} {g}\")
}
";

/// The module the warm tests edit, with the package's only test.
const WARM_NUMBERS: &str = "//! A number the tests edit.

/// A small number.
pub fn three() -> Int {
  3
}

test \"three is positive\" {
  assert three() > 0
}
";

const WARM_WORDS: &str = "//! A word nobody edits.

/// A greeting.
pub fn greeting() -> String {
  \"hello\"
}
";

/// Writes the warm-check package `warm` into `dir`.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to write the package fails the test"
)]
fn warm_package(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("cheby.toml"),
        "name = \"warm\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    for (file, text) in [
        ("main.cheby", WARM_MAIN),
        ("numbers.cheby", WARM_NUMBERS),
        ("words.cheby", WARM_WORDS),
    ] {
        std::fs::write(dir.join("src").join(file), text).unwrap();
    }
    dir.to_path_buf()
}

/// The `parse <module>` lines of a log.
fn parses(log: &[String]) -> Vec<String> {
    log.iter()
        .filter_map(|l| l.strip_prefix("parse "))
        .map(str::to_owned)
        .collect()
}

/// D-1250: after a one-body edit, a warm `cheby run` and `cheby test`
/// parse only the edited module and miss only its code (and tests) record;
/// what they print equals a cold run of the edited package on a fresh
/// cache (D-268).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn warm_run_one_body_edit_parses_only_its_module() {
    let scratch = Scratch::new("warm-edit");
    let dir = warm_package(&scratch.path().join("warm"));
    let cache = scratch.path().join("cache");
    for args in [["test"], ["run"]] {
        let cold = cheby(&dir, &args, &cache, &scratch.path().join("log-cold"), &[]);
        assert_eq!(cold.out.status, Some(0), "{}", cold.out.stderr);
    }
    std::fs::write(
        dir.join("src/numbers.cheby"),
        WARM_NUMBERS.replace("\n  3\n", "\n  4\n"),
    )
    .unwrap();
    for (args, kind) in [(["test"], "test"), (["run"], "run")] {
        let warm = cheby(&dir, &args, &cache, &scratch.path().join("log-warm"), &[]);
        let fresh = cheby(
            &dir,
            &args,
            &scratch.path().join(format!("fresh-{kind}")),
            &scratch.path().join("log-fresh"),
            &[],
        );
        same_output(&fresh, &warm);
        assert_eq!(
            parses(&warm.log),
            ["warm::numbers"],
            "{kind}: {:?}",
            warm.log
        );
        let misses: Vec<String> = code_lookups(&warm.log, kind)
            .into_iter()
            .filter(|l| l.ends_with(" miss"))
            .collect();
        let mut expected = vec![format!("code.{kind} warm::numbers miss")];
        if kind == "test" {
            expected.push("tests.test warm::numbers#tests miss".to_owned());
        }
        assert_eq!(misses, expected, "{kind}: {:?}", warm.log);
    }
}

/// D-1250 with D-1134: a package whose unedited module holds constants that
/// all hit parses nothing on a second `cheby run`, and prints what the first
/// printed.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn warm_run_constants_that_hit_parse_nothing() {
    let scratch = Scratch::new("warm-consts");
    let dir = package(&scratch.path().join("app"));
    let (first, second) = twice(&scratch, &dir, &["run"], &[]);
    assert_eq!(first.out.status, Some(0), "{}", first.out.stderr);
    same_output(&first, &second);
    let constant_misses: Vec<&String> = second
        .log
        .iter()
        .filter(|l| l.starts_with("const ") && l.ends_with(" miss"))
        .collect();
    if constant_misses.is_empty() {
        assert_eq!(
            parses(&second.log),
            Vec::<String>::new(),
            "{:?}",
            second.log
        );
    } else {
        // Before K5b caches constants, every constant's module is checked
        // (D-1134).
        assert!(!parses(&second.log).is_empty(), "{:?}", second.log);
    }
}

/// D-1134's hazard (D-1250): decoded interfaces number items in encoded
/// order (D-1121), so a module edited and lowered against decoded hits must
/// still write the record a cold run writes. A warm run after a body edit
/// and a cold run of the edited package on a fresh cache write the same
/// bytes under every key both hold.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn warm_run_miss_against_decoded_hits_equals_cold() {
    let scratch = Scratch::new("warm-hazard");
    let dir = warm_package(&scratch.path().join("warm"));
    let warm_cache = scratch.path().join("cache-warm");
    let cold = cheby(
        &dir,
        &["run"],
        &warm_cache,
        &scratch.path().join("log-1"),
        &[],
    );
    assert_eq!(cold.out.status, Some(0), "{}", cold.out.stderr);
    std::fs::write(
        dir.join("src/numbers.cheby"),
        WARM_NUMBERS.replace("\n  3\n", "\n  4\n"),
    )
    .unwrap();
    let warm = cheby(
        &dir,
        &["run"],
        &warm_cache,
        &scratch.path().join("log-2"),
        &[],
    );
    assert_eq!(parses(&warm.log), ["warm::numbers"], "{:?}", warm.log);
    let fresh_cache = scratch.path().join("cache-fresh");
    let fresh = cheby(
        &dir,
        &["run"],
        &fresh_cache,
        &scratch.path().join("log-3"),
        &[],
    );
    same_output(&fresh, &warm);
    let warm_records = entries(&warm_cache, "rec");
    let mut compared = 0;
    for (name, bytes) in entries(&fresh_cache, "rec") {
        if let Some((_, warm_bytes)) = warm_records.iter().find(|(n, _)| *n == name) {
            assert!(
                *warm_bytes == bytes,
                "the record {name} differs warm and cold"
            );
            compared += 1;
        }
    }
    // The edited module's code record is among them: the warm run wrote it
    // under the key the cold run did.
    let key_line = |log: &[String]| {
        log.iter()
            .find(|l| l.starts_with("key code.run warm::numbers "))
            .cloned()
    };
    assert_eq!(key_line(&warm.log), key_line(&fresh.log));
    assert!(key_line(&warm.log).is_some(), "{:?}", warm.log);
    assert!(compared > 0);
}

/// Writes the package `name` with `files` (each under `src/`) into `dir`.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to write the package fails the test"
)]
fn package_of(dir: &Path, name: &str, files: &[(&str, &str)]) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("cheby.toml"),
        format!("name = \"{name}\"\nversion = \"0.1.0\"\n"),
    )
    .unwrap();
    for (file, text) in files {
        std::fs::write(dir.join("src").join(file), text).unwrap();
    }
    dir.to_path_buf()
}

/// The root of the package with a type error outside the root: it calls
/// `user::twice`, and only the root imports `std::io`.
const ERROR_MAIN: &str = "//! Root: prints what `user` gives.

import app::user
import std::io

fn main() {
  let m = user::twice(\"s\")
  io::println(\"{m}\")
}
";

/// A type error (E0047) and an unused parameter (W0002).
const ERROR_USER: &str = "//! User: a type error.

/// Twice.
pub fn twice(s: String) -> Int {
  2 + \"x\"
}
";

/// `ERROR_USER` without the error.
const ERROR_USER_FIXED: &str = "//! User: a type error.

/// Twice.
pub fn twice(s: String) -> Int {
  2
}
";

/// D-268, D-964 with D-1250: a type error in a module that does not import
/// `std::io` prints under `cheby run` and `cheby test`, cold and warm on one
/// cache, what `cheby check` prints (the error and its warning), never an
/// internal error; the erroring module's front record is never written, so
/// the warm command parses it again.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn warm_run_error_outside_the_root_prints_checks_diagnostics() {
    let scratch = Scratch::new("warm-error");
    let dir = package_of(
        &scratch.path().join("app"),
        "app",
        &[("main.cheby", ERROR_MAIN), ("user.cheby", ERROR_USER)],
    );
    let check = cheby(
        &dir,
        &["check"],
        &scratch.path().join("cache-check"),
        &scratch.path().join("log-check"),
        &[],
    );
    assert_eq!(check.out.status, Some(1), "{}", check.out.stderr);
    assert!(
        check.out.stderr.contains("error[E0047]") && check.out.stderr.contains("warning[W0002]"),
        "{}",
        check.out.stderr
    );
    for args in [["run"], ["test"]] {
        let (cold, warm) = twice(&scratch, &dir, &args, &[]);
        same_output(&check, &cold);
        same_output(&check, &warm);
        assert!(
            warm.log.iter().any(|l| l == "front app::user miss"),
            "{args:?}: {:?}",
            warm.log
        );
        assert!(
            parses(&warm.log).iter().any(|m| m == "app::user"),
            "{args:?}: {:?}",
            warm.log
        );
    }
}

/// D-1250 with D-268: after a cold `cheby run` and `cheby test` that pass,
/// a type error edited into a module the root imports prints, on the warm
/// cache, what `cheby check` prints of the edited package.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn warm_run_error_edited_outside_the_root_prints_checks_diagnostics() {
    let scratch = Scratch::new("warm-error-edit");
    let dir = package_of(
        &scratch.path().join("app"),
        "app",
        &[("main.cheby", ERROR_MAIN), ("user.cheby", ERROR_USER_FIXED)],
    );
    let cache = scratch.path().join("cache");
    for args in [["test"], ["run"]] {
        let cold = cheby(&dir, &args, &cache, &scratch.path().join("log-cold"), &[]);
        assert_eq!(cold.out.status, Some(0), "{}", cold.out.stderr);
    }
    std::fs::write(dir.join("src/user.cheby"), ERROR_USER).unwrap();
    let check = cheby(
        &dir,
        &["check"],
        &scratch.path().join("cache-check"),
        &scratch.path().join("log-check"),
        &[],
    );
    assert_eq!(check.out.status, Some(1), "{}", check.out.stderr);
    for args in [["run"], ["test"], ["run"]] {
        let warm = cheby(&dir, &args, &cache, &scratch.path().join("log-warm"), &[]);
        same_output(&check, &warm);
    }
}

/// The root of the package whose module holds a test named like one of
/// its functions.
const NAMED_MAIN: &str = "//! Root: prints a size.

import mask::lib
import std::io

fn main() {
  let n = lib::size(\"hello\")
  io::println(\"{n}\")
}
";

/// `test \"size\"` beside `fn size`: an item and a test of one text.
const NAMED_LIB: &str = "//! Lib: a size and its test of the same name.

/// A size.
pub fn size(_s: String) -> Int {
  42
}

test \"size\" {
  assert size(\"ab\") == 42
}
";

/// D-969, D-1250 with D-268: a test whose name is the text of one of its
/// module's functions never stands for that function when stored facts and
/// masks are read back by name, so a second `cheby run` and `cheby test` on
/// one cache load every package module's code and print what the first
/// printed.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn warm_run_test_named_like_a_function() {
    let scratch = Scratch::new("warm-named");
    let dir = package_of(
        &scratch.path().join("mask"),
        "mask",
        &[("main.cheby", NAMED_MAIN), ("lib.cheby", NAMED_LIB)],
    );
    for (args, kind) in [(["run"], "run"), (["test"], "test")] {
        let (first, second) = twice(&scratch, &dir, &args, &[]);
        assert_eq!(first.out.status, Some(0), "{}", first.out.stderr);
        same_output(&first, &second);
        let misses: Vec<String> = code_lookups(&second.log, kind)
            .into_iter()
            .filter(|l| l.ends_with(" miss"))
            .collect();
        assert_eq!(misses, Vec::<String>::new(), "{kind}: {:?}", second.log);
    }
    // The other command first, then this one twice, on one cache.
    for (other, this) in [("test", "run"), ("run", "test")] {
        let cache = scratch.path().join(format!("cache-{other}-{this}"));
        let log = scratch.path().join("log-mixed");
        let ran = [other, this, this].map(|args| cheby(&dir, &[args], &cache, &log, &[]));
        for one in &ran {
            assert_eq!(
                one.out.status,
                Some(0),
                "{other} {this}: {}",
                one.out.stderr
            );
        }
        same_output(&ran[1], &ran[2]);
    }
}

/// What the converting modules of [`dyn_package`] share.
const DYN_KINDS: &str = "//! What the converting modules share.

/// Satisfied by every type.
interface Any

/// A generic box.
type Box<T> {
  value: T,
}

/// One field.
type Point {
  x: Int,
}

/// Holds a function, so `==` panics once the descriptors agree.
type Holder {
  f: fn(Int) -> Int,
}

/// Text of a point.
fn show(p: Point) -> String {
  let x = p.x
  \"point {x}\"
}

/// Boxes `x` for a type parameter: the descriptor is built at run time.
fn wrap<T: Any>(x: T) -> dyn Any {
  let b: dyn Any = Box { value: x }
  b
}
";

/// `left` and `right` of [`dyn_package`], the same text: each conversion
/// happens in the module's own unit, and `Own` and `I` are declared in
/// each, so they share a printed name and a shape and nothing else.
const DYN_SIDE: &str = "//! Converts in a unit of its own.

import dynid::kinds

/// Declared in both modules.
type Own {
  x: Int,
}

/// Declared in both modules.
interface I

/// Declared in both modules, beside `Own`.
type Other {
  x: Int,
}

/// A user type.
fn plain(x: Int) -> dyn kinds::Any {
  let b: dyn kinds::Any = kinds::Point { x }
  b
}

/// A function value inside a user type.
fn holder() -> dyn kinds::Any {
  let b: dyn kinds::Any = kinds::Holder { f: fn(x: Int) -> Int { x } }
  b
}

/// A generic type with a user argument, a static descriptor.
fn generic(x: Int) -> dyn kinds::Any {
  let b: dyn kinds::Any = kinds::Box { value: kinds::Point { x } }
  b
}

/// A descriptor built at run time.
fn built(x: Int) -> dyn kinds::Any {
  kinds::wrap(kinds::Point { x })
}

/// A tuple with a user type.
fn tuple(x: Int) -> dyn kinds::Any {
  let b: dyn kinds::Any = (x, kinds::Point { x })
  b
}

/// `dyn` of the builtin interface.
fn show(x: Int) -> dyn Show {
  let b: dyn Show = kinds::Point { x }
  b
}

/// A list of `dyn Show`, boxed.
fn shows() -> dyn kinds::Any {
  let xs: List<dyn Show> = []
  let b: dyn kinds::Any = xs
  b
}

/// A list of `dyn` of a user interface, boxed.
fn anys() -> dyn kinds::Any {
  let xs: List<dyn kinds::Any> = []
  let b: dyn kinds::Any = xs
  b
}

/// This module's own type.
fn own(x: Int) -> dyn kinds::Any {
  let b: dyn kinds::Any = Own { x }
  b
}

/// This module's own type inside a generic type of another module.
fn own_boxed(x: Int) -> dyn kinds::Any {
  let b: dyn kinds::Any = kinds::Box { value: Own { x } }
  b
}

/// A list that differs from the other module's only by which `I`.
fn own_dyns() -> dyn kinds::Any {
  let xs: List<dyn I> = []
  let b: dyn kinds::Any = xs
  b
}

/// An empty list: no value to compare, so only the descriptor differs.
fn own_list() -> dyn kinds::Any {
  let xs: List<Own> = []
  let b: dyn kinds::Any = xs
  b
}

/// An empty list of this module's other type.
fn other_list() -> dyn kinds::Any {
  let xs: List<Other> = []
  let b: dyn kinds::Any = xs
  b
}
";

/// A `priv` type converted in its module's code unit and in its tests
/// unit.
const DYN_SECRET: &str = "//! A private type, converted here.

import dynid::kinds

priv type S {
  x: Int,
}

/// A boxed private value.
fn boxed(x: Int) -> dyn kinds::Any {
  let b: dyn kinds::Any = S { x }
  b
}

test \"a tests unit names the private type\" {
  let a: dyn kinds::Any = S { x: 1 }
  assert a == boxed(1)
}
";

/// A package `dynid` in `dir` whose root is `main`, with the modules
/// `kinds`, `left`, `right` and `secret`.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to write the package fails the test"
)]
fn dyn_package(dir: &Path, main: &str) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("cheby.toml"),
        "name = \"dynid\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    for (file, text) in [
        ("main.cheby", main),
        ("kinds.cheby", DYN_KINDS),
        ("left.cheby", DYN_SIDE),
        ("right.cheby", DYN_SIDE),
        ("secret.cheby", DYN_SECRET),
    ] {
        std::fs::write(dir.join("src").join(file), text).unwrap();
    }
    dir.to_path_buf()
}

/// §8.6.1, D-530, GAP(dyn-identity): `dyn` values of one type that two
/// units converted are equal, for every shape of descriptor, on a cold run
/// and on a warm one, which loads the copies from two records (D-918), and
/// in a constant's initializer, whose value the program then holds.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn dyn_values_of_two_modules_are_equal_on_a_run() {
    let scratch = Scratch::new("dyn-equal");
    let dir = dyn_package(
        &scratch.path().join("dynid"),
        "//! Compares what two modules converted.

import dynid::left
import dynid::right
import std::io

/// Evaluated when the program is built, across the same two units.
const at_build: Bool = left::plain(1) == right::plain(1)

fn main() {
  let plain = left::plain(1) == right::plain(1)
  let generic = left::generic(1) == right::generic(1)
  let built = left::built(1) == right::built(1)
  let tuple = left::tuple(1) == right::tuple(1)
  let show = left::show(1) == right::show(1)
  let shows = left::shows() == right::shows()
  let anys = left::anys() == right::anys()
  let mixed = left::built(1) == right::generic(1)
  let values = left::plain(1) == right::plain(2)
  let ints = left::tuple(1) == right::tuple(2)
  io::println(\"{plain} {generic} {built} {tuple} {show} {shows} {anys} {mixed}\")
  io::println(\"{values} {ints} {at_build}\")
}
",
    );
    let (first, second) = twice(&scratch, &dir, &["run"], &[]);
    assert_eq!(first.out.status, Some(0), "{}", first.out.stderr);
    assert_eq!(
        first.out.stdout,
        "True True True True True True True True\nFalse False True\n"
    );
    let hits: Vec<String> = ["dynid", "dynid::kinds", "dynid::left", "dynid::right"]
        .iter()
        .map(|m| format!("code.run {m} hit"))
        .collect();
    assert_eq!(code_lookups(&second.log, "run"), hits);
    same_output(&first, &second);
}

/// §3.13, D-233, GAP(dyn-identity): a function value inside `dyn` panics
/// when the two types are one, so the two units' descriptors must agree
/// before the walk reaches it: an answer of False would hide the panic.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_function_inside_dyn_values_of_two_modules_panics() {
    let scratch = Scratch::new("dyn-function");
    let dir = dyn_package(
        &scratch.path().join("dynid"),
        "//! Reaches a function value.

import dynid::left
import dynid::right
import std::io

fn main() {
  let same = left::holder() == right::holder()
  io::println(\"{same}\")
}
",
    );
    let (first, second) = twice(&scratch, &dir, &["run"], &[]);
    assert_eq!(first.out.status, Some(101), "{}", first.out.stderr);
    assert_eq!(first.out.stdout, "");
    assert!(
        first.out.stderr.contains("function"),
        "{}",
        first.out.stderr
    );
    same_output(&first, &second);
}

/// §8.6.1, D-530, GAP(dyn-identity): a `Set` holds one value that two
/// modules converted once, and a `Map` finds it through the other module's
/// key: equal values hash alike and now compare alike.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn set_and_map_of_dyn_values_of_two_modules() {
    let scratch = Scratch::new("dyn-set");
    let dir = dyn_package(
        &scratch.path().join("dynid"),
        "//! A `Set` and a `Map` of what two modules converted.

import dynid::kinds
import dynid::left
import dynid::right
import std::io
import std::map
import std::set

fn main() {
  let none: set::Set<dyn kinds::Any> = set::new()
  let one = set::insert(none, left::plain(1))
  let both = set::insert(one, right::plain(1))
  let set_size = set::size(both)
  let shows: set::Set<dyn Show> = set::new()
  let show_size = set::size(set::insert(set::insert(shows, left::show(1)), right::show(1)))
  let empty: map::Map<dyn kinds::Any, Int> = map::new()
  let first = map::insert(empty, left::generic(1), 1)
  let second = map::insert(first, right::generic(1), 2)
  let map_size = map::size(second)
  let found = map::get(second, left::generic(1))
  io::println(\"{set_size} {show_size} {map_size} {found}\")
}
",
    );
    let (first, second) = twice(&scratch, &dir, &["run"], &[]);
    assert_eq!(first.out.status, Some(0), "{}", first.out.stderr);
    assert_eq!(first.out.stdout, "1 1 1 Some(2)\n");
    same_output(&first, &second);
}

/// §8.6.1, GAP(dyn-identity): types with one printed name in two modules
/// stay different types, alone, as an argument, and as the interface of a
/// `dyn` type; and two names in one module do too, so the key reads both
/// the name and the module path. Only the empty lists read descriptors
/// alone: a value's own constructor would tell the others apart (D-559).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn types_of_one_printed_name_stay_apart_on_a_run() {
    let scratch = Scratch::new("dyn-apart");
    let dir = dyn_package(
        &scratch.path().join("dynid"),
        "//! What must not compare equal.

import dynid::kinds
import dynid::left
import dynid::right
import std::io
import std::set

fn main() {
  let own = left::own(1) == right::own(1)
  let boxed = left::own_boxed(1) == right::own_boxed(1)
  let dyns = left::own_dyns() == right::own_dyns()
  let lists = left::own_list() == right::own_list()
  let names = left::own_list() == left::other_list()
  let none: set::Set<dyn kinds::Any> = set::new()
  let size = set::size(set::insert(set::insert(none, left::own(1)), right::own(1)))
  let same = left::own(1) == left::own(1)
  io::println(\"{own} {boxed} {dyns} {lists} {names} {size} {same}\")
}
",
    );
    let (first, second) = twice(&scratch, &dir, &["run"], &[]);
    assert_eq!(first.out.status, Some(0), "{}", first.out.stderr);
    assert_eq!(first.out.stdout, "False False False False False 2 True\n");
    same_output(&first, &second);
}

/// §8.6.1, GAP(dyn-identity): a module's code unit and its tests unit
/// each hold a copy of a `priv` type's descriptor, and a value from each
/// compares equal.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_module_tests_unit_agrees_with_its_code_unit() {
    let scratch = Scratch::new("dyn-tests");
    let dir = dyn_package(
        &scratch.path().join("dynid"),
        "//! Nothing but a main.\n\nfn main() {\n}\n",
    );
    let (first, second) = twice(&scratch, &dir, &["test"], &[]);
    assert_eq!(first.out.status, Some(0), "{}", first.out.stdout);
    same_output(&first, &second);
}
