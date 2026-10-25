//! `cheby test`'s temporary root (D-1066, D-1067): every
//! `file::with_temp_dir` of a run lies under one root that `cheby test`
//! creates under the host's temporary directory, and the root goes when the
//! run ends, after failed tests too, so a test that fails inside its
//! scratch directory leaves nothing behind.
//!
//! Each test runs `cheby test` as a child whose `TMPDIR` is a fresh
//! directory of its own, and reads that directory afterwards.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::HarnessCache;
use cheby_test_support::process;

const TMP: &str = env!("CARGO_TARGET_TMPDIR");

/// How long `cheby test` may take before the test kills it and fails.
const PATIENCE: Duration = Duration::from_secs(120);

/// A test that writes into its scratch directory, prints the directory's
/// path, and then fails; and a test that passes in its own.
const FAILING_IN_A_SCRATCH_DIRECTORY: &str = r#"//! A test that fails inside its scratch directory, and one that passes.

import std::file
import std::io
import std::result

fn main() {
  io::println("main")
}

test "a test that fails inside its scratch directory" {
  use dir <- file::with_temp_dir()
  use _ <- result::try(io::write_file(dir + "/left.txt", "left"))
  use _ <- result::try(file::create_dir(dir + "/nested"))
  io::println("scratch: {dir}")
  assert dir == "a path no scratch directory has"
  Ok(Nil)
}

test "a test that passes inside its scratch directory" {
  use dir <- file::with_temp_dir()
  use _ <- result::try(io::write_file(dir + "/kept.txt", "kept"))
  use names <- result::try(file::list_dir(dir))
  assert names == ["kept.txt"]
  Ok(Nil)
}
"#;

/// A package `name` in this test binary's scratch directory, with `main`
/// as its root module, and an empty directory to be its run's `TMPDIR`.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to write the package fails the test"
)]
fn package(name: &str, main: &str) -> (PathBuf, PathBuf) {
    let root = Path::new(TMP)
        .join("temp_root")
        .join(format!("{name}-{}", std::process::id()));
    if root.exists() {
        std::fs::remove_dir_all(&root).unwrap();
    }
    let dir = root.join("package");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("cheby.toml"),
        "name = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/main.cheby"), main).unwrap();
    let tmp = root.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    (dir, tmp)
}

/// The entries of `dir`, sorted.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: a directory that cannot be read fails the test"
)]
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// D-1067, GAP(test-temp-root-handoff): a test that fails inside its
/// scratch directory had it under `TMPDIR/cheby-test-<16 hex>/`, and after
/// `cheby test` nothing is left under `TMPDIR`; the test that passed in its
/// own directory passed.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn temp_root_a_failed_test_leaves_nothing_behind() {
    let (dir, tmp) = package("failing", FAILING_IN_A_SCRATCH_DIRECTORY);
    let cache = HarnessCache::create("cli-temp-root").unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cheby"));
    cmd.args(["--color=never", "test"])
        .current_dir(&dir)
        .env("NO_COLOR", "1")
        .env("TMPDIR", &tmp);
    cache.apply(&mut cmd);
    let output = process::run(cmd, None, PATIENCE).unwrap();
    let shown = format!("{}{}", output.stdout, output.stderr);
    assert_eq!(output.status, Some(1), "{shown}");
    assert!(
        shown.contains("1 passed; 1 failed"),
        "the passing test did not pass: {shown}"
    );
    let line = output
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("scratch: "))
        .unwrap_or_else(|| panic!("no scratch path: {shown}"));
    let scratch = Path::new(line);
    let run_root = scratch.parent().unwrap();
    let canonical_tmp = tmp.canonicalize().unwrap();
    assert_eq!(
        run_root
            .parent()
            .map(|parent| parent.canonicalize().unwrap()),
        Some(canonical_tmp),
        "the scratch directory {line} is not under the run's root in TMPDIR"
    );
    let root_name = run_root.file_name().unwrap().to_str().unwrap();
    let hex = root_name.strip_prefix("cheby-test-").unwrap();
    assert_eq!(hex.len(), 16, "{root_name}");
    assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()), "{root_name}");
    assert_eq!(
        entries(&tmp),
        Vec::<String>::new(),
        "`cheby test` left files in TMPDIR"
    );
    std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
}
