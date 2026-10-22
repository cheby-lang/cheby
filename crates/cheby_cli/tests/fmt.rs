//! `cheby fmt` end to end (D-1031): the built binary on temporary packages
//! and files, and the output test, which formats a temporary copy of every
//! example and requires its goldens unchanged.
//!
//! Every test works in `CARGO_TARGET_TMPDIR`; nothing under `docs/`,
//! `std/` or `tests/` is written.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::HarnessCache;
use cheby_test_support::example::{self, Example};
use cheby_test_support::process::{self, RunOutput};
use cheby_test_support::{golden, repo_root};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
const TIMEOUT: Duration = Duration::from_secs(60);

const MANIFEST: &str = "name = \"app\"\nversion = \"0.1.0\"\n";

/// A source `cheby fmt` changes: imports out of order, a header broken
/// where it fits on one line, spaces and blank lines to remove.
const UNFORMATTED: &str = "//! A test package.\n\nimport std::list\nimport std::io\n\n\n\nfn main()   {\n  io::println(greeting())\n}\n\nfn greeting(\n) -> String {\n  \"hi\"\n}   \n\n";

/// [`UNFORMATTED`] after `cheby fmt`.
const FORMATTED: &str = "//! A test package.\n\nimport std::io\nimport std::list\n\nfn main() {\n  io::println(greeting())\n}\n\nfn greeting() -> String {\n  \"hi\"\n}\n";

/// Runs `cheby --color=never <args>` in `dir` with `stdin`, with a fresh
/// `CHEBY_CACHE` of this process, deleted afterwards, so no run reads or
/// fills the developer's cache (GAP(cache-tests-isolated)).
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to spawn `cheby` fails the test"
)]
fn cheby(dir: &Path, args: &[&str], stdin: Option<&str>) -> RunOutput {
    let cache = HarnessCache::create("fmt").unwrap();
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never")
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1");
    cache.apply(&mut cmd);
    process::run(cmd, stdin.map(str::as_bytes), TIMEOUT).unwrap()
}

/// A fresh directory `name` under the test's temporary directory, with
/// `files` written into it.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to write the package fails the test"
)]
fn dir_with(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = Path::new(TMP).join("fmt").join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    for (path, text) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    dir
}

#[expect(clippy::unwrap_used, reason = "a test helper")]
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn fmt_check_on_a_formatted_package() {
    let dir = dir_with(
        "formatted",
        &[
            ("cheby.toml", MANIFEST),
            ("src/main.cheby", FORMATTED),
            (
                "src/util/text.cheby",
                "pub fn id(s: String) -> String {\n  s\n}\n",
            ),
        ],
    );
    let out = cheby(&dir, &["fmt", "--check"], None);
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr, "");
    assert_eq!(out.status, Some(0));
    // From a subdirectory, the package is found as other commands find it.
    let out = cheby(&dir.join("src/util"), &["fmt"], None);
    assert_eq!((out.stdout.as_str(), out.status), ("", Some(0)));
    assert_eq!(read(&dir.join("src/main.cheby")), FORMATTED);
}

#[test]
fn fmt_check_on_an_unformatted_package_then_fmt() {
    let other = "fn  id(s: String) -> String {\n  s\n}\n";
    let dir = dir_with(
        "unformatted",
        &[
            ("cheby.toml", MANIFEST),
            ("src/main.cheby", UNFORMATTED),
            ("src/util/text.cheby", other),
            ("src/util/done.cheby", "fn f() {}\n"),
            ("src/.hidden.cheby", "fn  hidden() {}\n"),
            ("notes.cheby", "fn  outside() {}\n"),
        ],
    );
    // `--check` lists the files that would change, relative to the
    // package root, writes nothing, and exits with 1.
    let out = cheby(&dir, &["fmt", "--check"], None);
    assert_eq!(out.stdout, "src/main.cheby\nsrc/util/text.cheby\n");
    assert_eq!(out.stderr, "");
    assert_eq!(out.status, Some(1));
    assert_eq!(read(&dir.join("src/main.cheby")), UNFORMATTED);
    assert_eq!(read(&dir.join("src/util/text.cheby")), other);

    let out = cheby(&dir, &["fmt"], None);
    assert_eq!((out.stdout.as_str(), out.stderr.as_str()), ("", ""));
    assert_eq!(out.status, Some(0));
    assert_eq!(read(&dir.join("src/main.cheby")), FORMATTED);
    assert_eq!(
        read(&dir.join("src/util/text.cheby")),
        "fn id(s: String) -> String {\n  s\n}\n"
    );
    // Hidden files and files outside `src/` are left alone, and no
    // temporary file is left behind.
    assert_eq!(read(&dir.join("src/.hidden.cheby")), "fn  hidden() {}\n");
    assert_eq!(read(&dir.join("notes.cheby")), "fn  outside() {}\n");
    let mut names: Vec<_> = std::fs::read_dir(dir.join("src"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, [".hidden.cheby", "main.cheby", "util"]);

    let out = cheby(&dir, &["fmt", "--check"], None);
    assert_eq!((out.stdout.as_str(), out.status), ("", Some(0)));
}

#[test]
fn fmt_stdin_round_trip() {
    let dir = dir_with("stdin", &[]);
    let out = cheby(&dir, &["fmt", "-"], Some(UNFORMATTED));
    assert_eq!(out.stdout, FORMATTED);
    assert_eq!(out.stderr, "");
    assert_eq!(out.status, Some(0));
    let out = cheby(&dir, &["fmt", "-"], Some(FORMATTED));
    assert_eq!((out.stdout.as_str(), out.status), (FORMATTED, Some(0)));
    // `--check` on standard input names it `-` and writes no source.
    let out = cheby(&dir, &["fmt", "--check", "-"], Some(UNFORMATTED));
    assert_eq!((out.stdout.as_str(), out.status), ("-\n", Some(1)));
    let out = cheby(&dir, &["fmt", "--check", "-"], Some(FORMATTED));
    assert_eq!((out.stdout.as_str(), out.status), ("", Some(0)));
}

#[test]
fn fmt_leaves_a_file_with_a_syntax_error_alone() {
    let broken = "fn main(  {\n  x\n}\n";
    let dir = dir_with(
        "syntax_error",
        &[
            ("cheby.toml", MANIFEST),
            ("src/main.cheby", broken),
            ("src/other.cheby", "fn  f() {}\n"),
        ],
    );
    let out = cheby(&dir, &["fmt"], None);
    assert_eq!(out.status, Some(1));
    assert!(out.stderr.contains("error[E"), "stderr:\n{}", out.stderr);
    assert!(
        out.stderr.contains("src/main.cheby:1:"),
        "stderr:\n{}",
        out.stderr
    );
    assert_eq!(out.stdout, "");
    assert_eq!(
        std::fs::read(dir.join("src/main.cheby")).unwrap(),
        broken.as_bytes()
    );
    // The other files are still formatted.
    assert_eq!(read(&dir.join("src/other.cheby")), "fn f() {}\n");

    let out = cheby(&dir, &["fmt", "--check"], None);
    assert_eq!(out.status, Some(1));
    assert!(out.stderr.contains("error[E"));
    assert_eq!(
        std::fs::read(dir.join("src/main.cheby")).unwrap(),
        broken.as_bytes()
    );

    let out = cheby(&dir, &["fmt", "-"], Some(broken));
    assert_eq!((out.stdout.as_str(), out.status), ("", Some(1)));
    assert!(out.stderr.contains("<stdin>:1:"), "stderr:\n{}", out.stderr);
}

#[test]
fn fmt_explicit_paths() {
    // Paths work outside a package and name only the files given.
    let dir = dir_with(
        "paths",
        &[
            ("a.cheby", UNFORMATTED),
            ("sub/b.cheby", "fn  b() {}\n"),
            ("c.cheby", "fn  c() {}\n"),
        ],
    );
    let out = cheby(&dir, &["fmt", "--check", "a.cheby", "sub/b.cheby"], None);
    assert_eq!(out.stdout, "a.cheby\nsub/b.cheby\n");
    assert_eq!(out.status, Some(1));
    let out = cheby(&dir, &["fmt", "sub/b.cheby", "a.cheby"], None);
    assert_eq!((out.stdout.as_str(), out.status), ("", Some(0)));
    assert_eq!(read(&dir.join("a.cheby")), FORMATTED);
    assert_eq!(read(&dir.join("sub/b.cheby")), "fn b() {}\n");
    assert_eq!(read(&dir.join("c.cheby")), "fn  c() {}\n");
    // A path that cannot be read is an error; the others still run.
    let out = cheby(&dir, &["fmt", "missing.cheby", "c.cheby"], None);
    assert_eq!(out.status, Some(1));
    assert!(
        out.stderr.contains("cannot read `missing.cheby`"),
        "{}",
        out.stderr
    );
    assert_eq!(read(&dir.join("c.cheby")), "fn c() {}\n");
}

/// Symbolic links under `src/` (review R2 and R3): a cycle of directory
/// links lists each file once and ends at once, even with two cycles,
/// whose paths grow exponentially; a linked file is formatted through
/// the link, which stays a link; a read-only file is an error and is
/// left alone.
#[cfg(unix)]
#[test]
fn fmt_follows_links_once_and_keeps_them() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let dir = dir_with(
        "links",
        &[
            ("cheby.toml", MANIFEST),
            ("src/main.cheby", UNFORMATTED),
            ("shared/real.cheby", "fn  shared() {}\n"),
            ("src/ro.cheby", "fn  ro() {}\n"),
        ],
    );
    symlink(".", dir.join("src/a")).unwrap();
    symlink(".", dir.join("src/b")).unwrap();
    symlink("../shared/real.cheby", dir.join("src/linked.cheby")).unwrap();
    let ro = dir.join("src/ro.cheby");
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o444)).unwrap();

    let out = cheby(&dir, &["fmt", "--check"], None);
    assert_eq!(
        out.stdout,
        "src/linked.cheby\nsrc/main.cheby\nsrc/ro.cheby\n"
    );
    assert_eq!((out.stderr.as_str(), out.status), ("", Some(1)));

    let out = cheby(&dir, &["fmt"], None);
    assert_eq!(out.status, Some(1));
    assert!(
        out.stderr
            .contains("cannot write `src/ro.cheby`: the file is read-only"),
        "{}",
        out.stderr
    );
    assert_eq!(read(&dir.join("src/main.cheby")), FORMATTED);
    assert!(
        std::fs::symlink_metadata(dir.join("src/linked.cheby"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(read(&dir.join("shared/real.cheby")), "fn shared() {}\n");
    assert_eq!(read(&ro), "fn  ro() {}\n");
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o644)).unwrap();
}

/// Copies `from` into `to`, skipping hidden entries.
#[expect(clippy::unwrap_used, reason = "a test helper")]
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

/// Runs `cheby <args>` in `dir` as the conformance harness runs an
/// example: the example's `env`, freed objects poisoned (D-544), the
/// default thread count and counting, and the test's own cache, a
/// [`HarnessCache`] (GAP(cache-tests-isolated)).
fn cheby_like_harness(
    dir: &Path,
    args: &[&str],
    goldens: &example::Goldens,
    cache: &Path,
) -> Result<RunOutput, String> {
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never").args(args).current_dir(dir);
    for (key, value) in &goldens.env {
        cmd.env(key, value);
    }
    cmd.env("CHEBY_RT_POISON", "1")
        .env_remove("CHEBY_THREADS")
        .env_remove("CHEBY_RC")
        .env("CHEBY_CACHE", cache);
    let timeout = process::scaled_timeout(TIMEOUT)?;
    process::run(cmd, goldens.stdin.as_deref().map(str::as_bytes), timeout)
}

/// The first line of each diagnostic in `stderr`, such as
/// `error[E0040]: …`, without the locations that formatting may move.
fn diagnostic_heads(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|l| l.starts_with("error") || l.starts_with("warning"))
        .collect()
}

/// Formats a copy of `ex` and checks that it behaves as the original:
/// a run example gives its goldens, a `skip-run` example's `cheby check`
/// gives the same status and diagnostics as an unformatted copy's.
fn check_formatted_example(ex: &Example, cache: &Path) -> Result<(), String> {
    let base = Path::new(TMP).join("fmt").join("examples");
    let formatted = base.join(&ex.name);
    if formatted.exists() {
        std::fs::remove_dir_all(&formatted).map_err(|e| e.to_string())?;
    }
    copy_dir(&ex.dir, &formatted);
    let goldens = ex.goldens()?;
    let out = cheby_like_harness(&formatted, &["fmt"], &goldens, cache)?;
    if out.status != Some(0) {
        return Err(format!(
            "`cheby fmt` failed ({})\n{}",
            out.describe_status(),
            out.stderr
        ));
    }
    if ex.config.skip_run {
        let original = base.join(format!("{}.original", ex.name));
        if original.exists() {
            std::fs::remove_dir_all(&original).map_err(|e| e.to_string())?;
        }
        copy_dir(&ex.dir, &original);
        let before = cheby_like_harness(&original, &["check"], &goldens, cache)?;
        let after = cheby_like_harness(&formatted, &["check"], &goldens, cache)?;
        if before.status != after.status
            || diagnostic_heads(&before.stderr) != diagnostic_heads(&after.stderr)
        {
            return Err(format!(
                "`cheby check` differs after formatting\nbefore ({}):\n{}\nafter ({}):\n{}",
                before.describe_status(),
                before.stderr,
                after.describe_status(),
                after.stderr
            ));
        }
        return Ok(());
    }
    let mut args = vec!["run"];
    if !goldens.args.is_empty() {
        args.push("--");
        args.extend(goldens.args.iter().map(String::as_str));
    }
    let out = cheby_like_harness(&formatted, &args, &goldens, cache)?;
    let expected_exit = goldens.exit.unwrap_or(0);
    if out.status != Some(expected_exit) {
        return Err(format!(
            "expected exit code {expected_exit}, got {}\nstderr:\n{}",
            out.describe_status(),
            out.stderr
        ));
    }
    golden::compare(
        "stdout",
        goldens.stdout.as_deref().unwrap_or(""),
        &out.stdout,
    )?;
    golden::compare(
        "stderr",
        goldens.stderr.as_deref().unwrap_or(""),
        &out.stderr,
    )
}

/// The output test of D-1031: every example, formatted in a temporary
/// copy, runs with its own `stdin`, `args` and `env` and gives its goldens
/// unchanged. Examples that wait for real time (D-725) run after the
/// others, one at a time.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn fmt_examples_keep_their_output() {
    let examples = example::all(&repo_root()).unwrap();
    let harness_cache = HarnessCache::create("fmt-examples").unwrap();
    let cache = harness_cache.path().to_path_buf();
    let (timing, parallel): (Vec<&Example>, Vec<&Example>) =
        examples.iter().partition(|ex| ex.config.timing);
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .div_ceil(2)
        .max(1);
    let mut failures: Vec<String> = Vec::new();
    for chunk in parallel.chunks(workers) {
        let results: Vec<(String, Result<(), String>)> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|ex| {
                    let cache = &cache;
                    s.spawn(move || (ex.name.clone(), check_formatted_example(ex, cache)))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (name, result) in results {
            if let Err(e) = result {
                failures.push(format!("{name}: {e}"));
            }
        }
    }
    for ex in timing {
        if let Err(e) = check_formatted_example(ex, &cache) {
            failures.push(format!("{}: {e}", ex.name));
        }
    }
    println!("formatted and ran {} examples", examples.len());
    assert!(examples.len() >= 30, "only {} examples", examples.len());
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
