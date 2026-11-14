//! `cargo xtask test`: every Rust test of the workspace under cargo-nextest
//! (D-1553), with one build cache shared by the run (D-1554) and the
//! harnesses' pending summary printed once at the end (D-1556).
//!
//! nextest runs each test, and each harness trial, in a process of its
//! own. A harness process gives its children the cache that
//! `CHEBY_TEST_KEEP_CACHE` names, so this command creates one fresh
//! owner-only directory, runs `cargo nextest run --workspace --locked`
//! with the variable set to it, and deletes it at the end, whatever the
//! result. A `CHEBY_TEST_KEEP_CACHE` that is already set is passed through
//! and kept, as D-1144 says. `CHEBY_TEST_VERDICTS` names a second fresh
//! directory, where each harness process writes the pending verdicts of
//! its trial; after nextest, the command prints their summary per harness
//! and deletes it. The command exits with nextest's status. The variables
//! `cargo run` sets for `xtask`'s own package are removed first, so the
//! nested cargo reuses the build a plain `cargo` made.

use std::path::Path;
use std::process::{Command, ExitCode, ExitStatus};

use cheby_test_support::cache::{FreshDir, HarnessCache, KEEP_CACHE_ENV};
use cheby_test_support::milestone::{VERDICTS_ENV, merged_summary};

/// Runs `cargo nextest run --workspace --locked <args>` in `root` with
/// `envs` added, under one shared cache, then prints the pending summary.
pub(crate) fn run(root: &Path, args: &[String], envs: &[(&str, &str)]) -> ExitCode {
    match nextest(root, args, envs) {
        // Unix and Windows both give an exit code in 0..=255 for a process
        // that exited; a signal is reported as a failure.
        Ok(status) => status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .map_or(ExitCode::FAILURE, ExitCode::from),
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

/// [`run`]'s work: nextest's exit status once the summary is printed.
///
/// # Errors
///
/// When a directory cannot be created, nextest cannot be started, or the
/// verdict files cannot be read.
pub(crate) fn nextest(
    root: &Path,
    args: &[String],
    envs: &[(&str, &str)],
) -> Result<ExitStatus, String> {
    // Fresh and deleted on drop, or the developer's kept directory.
    let cache = HarnessCache::create("test")?;
    let verdicts = FreshDir::create("cheby-test-verdicts")?;
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut cmd = Command::new(cargo);
    remove_package_variables(&mut cmd);
    cmd.current_dir(root)
        .args(["nextest", "run", "--workspace", "--locked"])
        .args(args)
        .env(KEEP_CACHE_ENV, cache.path())
        .env(VERDICTS_ENV, verdicts.path());
    for (key, value) in envs {
        cmd.env(key, value);
    }
    eprintln!("xtask test: {}", cache.describe());
    let status = cmd
        .status()
        .map_err(|e| format!("cannot run `cargo nextest` (is cargo-nextest installed?): {e}"))?;
    let summary = merged_summary(verdicts.path())?;
    print!("{summary}");
    Ok(status)
}

/// Removes the variables `cargo run` sets for `xtask`'s own package
/// (`CARGO_MANIFEST_DIR`, `CARGO_PKG_NAME` and the like) from `cmd`'s
/// environment. Build scripts such as `ring`'s declare
/// `rerun-if-env-changed` on them, so the nested cargo would see `xtask`'s
/// values, call those builds dirty and rebuild the workspace, and the next
/// plain `cargo` would rebuild it back.
fn remove_package_variables(cmd: &mut Command) {
    const EXACT: [&str; 6] = [
        "CARGO_MANIFEST_DIR",
        "CARGO_MANIFEST_PATH",
        "CARGO_MANIFEST_LINKS",
        "CARGO_CRATE_NAME",
        "CARGO_BIN_NAME",
        "CARGO_PRIMARY_PACKAGE",
    ];
    for (key, _) in std::env::vars_os() {
        let Some(key) = key.to_str() else { continue };
        if key.starts_with("CARGO_PKG_") || EXACT.contains(&key) {
            cmd.env_remove(key);
        }
    }
}
