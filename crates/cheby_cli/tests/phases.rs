//! `CHEBY_PHASES` from the command line (D-974, D-1277): a cold
//! and a warm `cheby run` of a small package write every phase they pass
//! in the file's format, a warm run that starts again writes `restart`
//! (D-1256), `cheby build` writes `link`, `cheby check` and `cheby test`
//! write theirs, and a warm run with `CHEBY_PHASES` after a cold run
//! without it misses nothing: the variable is no setting and no key input
//! (D-958).
//!
//! Every package gets a fresh `CHEBY_CACHE` of its own (D-1128), set on
//! each child's `Command`, and `cheby build` writes with `-o` into a
//! temporary directory. `cheby build` links against the runtime archive
//! next to `cheby`: `cargo build -p cheby_rt_exe` comes first (D-929).
#![expect(
    clippy::unwrap_used,
    reason = "test helpers: a failed setup step fails the test"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::{CACHE_ENV, FreshDir};
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(300);

/// The root: `main` calls the other two modules.
const MAIN: &str = "//! Root.

import app::other
import app::util
import std::io

fn main() {
  let ok = util::check(\"x\")
  let n = other::twice(21)
  io::println(\"{ok} {n}\")
}

test \"twice\" {
  assert other::twice(2) == 4
}
";

/// `check` is package-visible and only compares its parameter: it borrows
/// it.
const UTIL: &str = "//! Util.

/// Whether `s` is `x`.
fn check(s: String) -> Bool {
  s == \"x\"
}
";

/// A body edit of `check` that puts its parameter in a list: it owns it
/// now, so its mask changes (D-812) and a warm run starts again (D-1256).
const UTIL_MASK: &str = "//! Util.

/// Whether `s` is `x`.
fn check(s: String) -> Bool {
  [s] == [\"x\"]
}
";

/// A body edit of `check` that changes no mask.
const UTIL_BODY: &str = "//! Util.

/// Whether `s` is `x`.
fn check(s: String) -> Bool {
  \"x\" == s
}
";

const OTHER: &str = "//! Other.

/// Twice `n`.
pub fn twice(n: Int) -> Int {
  n + n
}
";

/// Writes the package `app` into `dir`.
fn package(dir: &Path) -> PathBuf {
    let root = dir.join("app");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("cheby.toml"),
        "name = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    for (file, text) in [
        ("main.cheby", MAIN),
        ("util.cheby", UTIL),
        ("other.cheby", OTHER),
    ] {
        std::fs::write(root.join("src").join(file), text).unwrap();
    }
    root
}

/// One `cheby` command's output, its phases file (`None` without
/// `CHEBY_PHASES`) and its `CHEBY_CACHE_LOG` lines.
struct Ran {
    out: RunOutput,
    phases: Option<Phases>,
    log: Vec<String>,
}

/// A phases file: its command, each phase's nanoseconds and spans in file
/// order, and `command`'s nanoseconds.
#[derive(Debug)]
struct Phases {
    command: String,
    lines: Vec<(String, u128, u64)>,
    total: u128,
}

impl Phases {
    /// Parses the file's text, asserting its format (plan §2.1).
    fn parse(text: &str) -> Self {
        assert!(text.is_ascii(), "{text}");
        assert!(text.ends_with('\n'), "{text}");
        let mut lines = text.lines();
        let head = lines.next().unwrap();
        let command = head.strip_prefix("cheby-phases 1 ").unwrap().to_owned();
        let mut out = Vec::new();
        let mut total = None;
        for line in lines {
            assert!(total.is_none(), "a line after `command`: {text}");
            let fields: Vec<&str> = line.split(' ').collect();
            let well_formed = match fields.as_slice() {
                ["command", n] => {
                    total = Some(n.parse().unwrap());
                    true
                }
                [name, n, spans] => {
                    out.push((
                        (*name).to_owned(),
                        n.parse().unwrap(),
                        spans.parse().unwrap(),
                    ));
                    true
                }
                _ => false,
            };
            assert!(well_formed, "a malformed line `{line}`: {text}");
        }
        // In `Phase` order, each once.
        let order = [
            "hash", "index", "parse", "check", "std", "consts", "lower", "codegen", "records",
            "declare", "define", "finalize", "register", "main", "teardown", "write", "restart",
            "link",
        ];
        let at: Vec<usize> = out
            .iter()
            .map(|(name, _, _)| order.iter().position(|o| o == name).unwrap())
            .collect();
        assert!(at.is_sorted_by(|a, b| a < b), "{text}");
        let total = total.unwrap();
        // Exclusive: the phases add up to at most the whole command.
        let sum: u128 = out.iter().map(|(_, n, _)| n).sum();
        assert!(sum <= total, "{sum} > {total}: {text}");
        Self {
            command,
            lines: out,
            total,
        }
    }

    fn has(&self, phase: &str) -> bool {
        self.lines
            .iter()
            .any(|(name, _, spans)| name == phase && *spans > 0)
    }

    fn names(&self) -> Vec<&str> {
        self.lines
            .iter()
            .map(|(name, _, _)| name.as_str())
            .collect()
    }
}

/// `cheby <args>` in `dir` on `cache`, with `CHEBY_PHASES` naming
/// `phases` when given and `CHEBY_CACHE_LOG` in `log`.
fn cheby(dir: &Path, args: &[&str], cache: &Path, log: &Path, phases: Option<&Path>) -> Ran {
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never")
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, cache)
        .env("CHEBY_CACHE_LOG", log)
        .env_remove("CHEBY_DUMP_DIR")
        .env_remove("CHEBY_RC")
        .env_remove("CHEBY_POSITIONS")
        .env_remove("CHEBY_PHASES");
    if let Some(path) = phases {
        let _ = std::fs::remove_file(path);
        cmd.env("CHEBY_PHASES", path);
    }
    let out = process::run(cmd, None, TIMEOUT).unwrap();
    let phases = phases.map(|path| Phases::parse(&std::fs::read_to_string(path).unwrap()));
    let log = std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    Ran { out, phases, log }
}

/// A cold `cheby run` writes the phases of a build from source, a warm one
/// those of a build from records; both print the program's output.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn phases_cold_and_warm_run_write_every_phase_they_pass() {
    let tmp = FreshDir::create("cheby-phases-run").unwrap();
    let dir = package(tmp.path());
    let cache = tmp.path().join("cache");
    let file = tmp.path().join("phases.txt");
    let cold = cheby(
        &dir,
        &["run"],
        &cache,
        &tmp.path().join("log-1"),
        Some(&file),
    );
    assert_eq!(cold.out.status, Some(0), "{}", cold.out.stderr);
    assert_eq!(cold.out.stdout, "True 42\n");
    let phases = cold.phases.unwrap();
    assert_eq!(phases.command, "run");
    for phase in [
        "hash", "index", "parse", "check", "std", "lower", "codegen", "records", "declare",
        "define", "finalize", "register", "main", "write",
    ] {
        assert!(phases.has(phase), "cold run without `{phase}`: {phases:?}");
    }
    assert!(!phases.has("restart"), "{phases:?}");
    assert!(!phases.has("link"), "{phases:?}");
    assert!(phases.total > 0);
    let warm = cheby(
        &dir,
        &["run"],
        &cache,
        &tmp.path().join("log-2"),
        Some(&file),
    );
    assert_eq!(warm.out.status, Some(0), "{}", warm.out.stderr);
    assert_eq!(warm.out.stdout, cold.out.stdout);
    let phases = warm.phases.unwrap();
    for phase in [
        "hash", "index", "std", "records", "declare", "define", "finalize", "register", "main",
        "write",
    ] {
        assert!(phases.has(phase), "warm run without `{phase}`: {phases:?}");
    }
    // Nothing missed, nothing parsed, nothing started again.
    assert!(
        !warm.log.iter().any(|l| l.starts_with("parse ")),
        "{:?}",
        warm.log
    );
    assert!(!phases.has("restart"), "{:?}", phases.names());
}

/// A mask-changing edit makes a warm run start again (D-1256): its file
/// has `restart`, and the second attempt's parses under `parse`; a body
/// edit that changes no mask does not.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn phases_warm_restart_writes_restart() {
    let tmp = FreshDir::create("cheby-phases-restart").unwrap();
    let dir = package(tmp.path());
    let cache = tmp.path().join("cache");
    let file = tmp.path().join("phases.txt");
    let cold = cheby(&dir, &["run"], &cache, &tmp.path().join("log-1"), None);
    assert_eq!(cold.out.status, Some(0), "{}", cold.out.stderr);
    std::fs::write(dir.join("src/util.cheby"), UTIL_BODY).unwrap();
    let body = cheby(
        &dir,
        &["run"],
        &cache,
        &tmp.path().join("log-2"),
        Some(&file),
    );
    assert_eq!(body.out.stdout, cold.out.stdout, "{}", body.out.stderr);
    let phases = body.phases.unwrap();
    assert!(!phases.has("restart"), "{:?}", phases.names());
    assert!(phases.has("parse"), "{:?}", phases.names());
    std::fs::write(dir.join("src/util.cheby"), UTIL_MASK).unwrap();
    let mask = cheby(
        &dir,
        &["run"],
        &cache,
        &tmp.path().join("log-3"),
        Some(&file),
    );
    assert_eq!(mask.out.stdout, cold.out.stdout, "{}", mask.out.stderr);
    let phases = mask.phases.unwrap();
    if !mask.log.iter().any(|l| l == "parse app::other") {
        // A toolchain without borrow inference changes no mask, so nothing
        // restarts (cache-hit test 2's guard).
        return;
    }
    assert!(phases.has("restart"), "{phases:?}");
    assert!(phases.has("parse") && phases.has("check"), "{phases:?}");
}

/// `cheby build` writes `link`; `cheby check` and `cheby test` write their
/// own command.
#[test]
#[cfg_attr(
    windows,
    ignore = "builds executables and runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn phases_check_test_and_build_write_their_command() {
    let tmp = FreshDir::create("cheby-phases-build").unwrap();
    let dir = package(tmp.path());
    let cache = tmp.path().join("cache");
    let file = tmp.path().join("phases.txt");
    let log = tmp.path().join("log");
    let check = cheby(&dir, &["check"], &cache, &log, Some(&file));
    assert_eq!(check.out.status, Some(0), "{}", check.out.stderr);
    let phases = check.phases.unwrap();
    assert_eq!(phases.command, "check");
    assert!(phases.has("hash") && phases.has("write"), "{phases:?}");
    assert!(!phases.has("main") && !phases.has("link"), "{phases:?}");
    let test = cheby(&dir, &["test"], &cache, &log, Some(&file));
    assert_eq!(test.out.status, Some(0), "{}", test.out.stderr);
    let phases = test.phases.unwrap();
    assert_eq!(phases.command, "test");
    assert!(phases.has("register") && phases.has("write"), "{phases:?}");
    let exe = tmp.path().join("out").join("app");
    let build = cheby(
        &dir,
        &["build", "-o", exe.to_str().unwrap()],
        &cache,
        &log,
        Some(&file),
    );
    assert_eq!(build.out.status, Some(0), "{}", build.out.stderr);
    let phases = build.phases.unwrap();
    assert_eq!(phases.command, "build");
    assert!(phases.has("link"), "{phases:?}");
    assert!(!phases.has("main"), "{phases:?}");
    assert!(exe.exists());
}

/// D-958: `CHEBY_PHASES` is no setting and no key input: a warm run with
/// it, after a cold run without it, misses nothing and prints the same.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn phases_do_not_change_keys() {
    let tmp = FreshDir::create("cheby-phases-keys").unwrap();
    let dir = package(tmp.path());
    let cache = tmp.path().join("cache");
    let cold = cheby(&dir, &["run"], &cache, &tmp.path().join("log-1"), None);
    assert_eq!(cold.out.status, Some(0), "{}", cold.out.stderr);
    assert!(
        cold.log.iter().any(|l| l.ends_with(" miss")),
        "{:?}",
        cold.log
    );
    let file = tmp.path().join("phases.txt");
    let warm = cheby(
        &dir,
        &["run"],
        &cache,
        &tmp.path().join("log-2"),
        Some(&file),
    );
    assert_eq!(warm.out.stdout, cold.out.stdout, "{}", warm.out.stderr);
    let misses: Vec<&String> = warm.log.iter().filter(|l| l.ends_with(" miss")).collect();
    assert!(misses.is_empty(), "{misses:?}");
    assert!(warm.phases.is_some());
}
