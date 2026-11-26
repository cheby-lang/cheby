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
//!
//! Before nextest, the command builds the runtime archive
//! `libcheby_rt_exe.a` in the run's profile and for its target (D-1950):
//! `cheby build` links the archive next to `cheby` (D-929), nextest builds
//! the workspace's tests but not this static library, and an archive of
//! older runtime sources fails every test that builds an executable.

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
    build_archive(root, args)?;
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

/// Builds the runtime archive where the tests of this run look for it
/// (D-1950): with the profile and the target that `args`, nextest's
/// arguments, select.
fn build_archive(root: &Path, args: &[String]) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut cmd = Command::new(cargo);
    remove_package_variables(&mut cmd);
    let status = cmd
        .current_dir(root)
        .args(["build", "--locked", "--package", "cheby_rt_exe"])
        .args(build_arguments(args))
        .status()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("cannot build the runtime archive (`cargo build -p cheby_rt_exe`)".to_owned())
    }
}

/// The arguments of `cargo build` that put its output where `cargo nextest
/// run` with `args` puts its own: the profile, the target and the target
/// directory. nextest names a cargo profile `--cargo-profile`, and what
/// follows `--` goes to the test binaries.
fn build_arguments(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut args = args.iter().take_while(|arg| *arg != "--");
    while let Some(arg) = args.next() {
        let (flag, value) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (arg.as_str(), None),
        };
        let build_flag = match flag {
            "--release" | "-r" => {
                out.push("--release".to_owned());
                continue;
            }
            "--cargo-profile" => "--profile",
            "--target" => "--target",
            "--target-dir" => "--target-dir",
            _ => continue,
        };
        if let Some(value) = value.or_else(|| args.next().cloned()) {
            out.extend([build_flag.to_owned(), value]);
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The archive is built with the profile and the target of the test
    /// run, and nextest's other arguments are not cargo build's.
    #[test]
    fn the_archive_follows_the_runs_profile_and_target() {
        let of = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|&arg| arg.to_owned()).collect();
            build_arguments(&args).join(" ")
        };
        assert_eq!(of(&["-E", "package(xtask)", "--no-fail-fast"]), "");
        assert_eq!(of(&["--release", "-E", "test(x)"]), "--release");
        assert_eq!(of(&["-r"]), "--release");
        assert_eq!(
            of(&["--cargo-profile", "bench", "--target=aarch64-apple-darwin"]),
            "--profile bench --target aarch64-apple-darwin"
        );
        assert_eq!(
            of(&["--target-dir", "out", "--profile", "ci"]),
            "--target-dir out"
        );
        // What follows `--` belongs to the test binaries.
        assert_eq!(of(&["--", "--release"]), "");
    }
}
