//! The build cache from the command line (D-959, D-973): where it lives,
//! the temporary cache when it cannot be written, the tool errors that
//! never fall back, and `CHEBY_CACHE_LOG`.
//!
//! An unwritable cache is a path under a regular file, which no user can
//! create, root included, so the Linux containers run these tests too.
//! Every `cheby` gets its own `CHEBY_CACHE` and `TMPDIR` on its
//! `Command`; nothing here sets a variable of the test process.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use cheby_test_support::cache::CACHE_ENV;
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
const TIMEOUT: Duration = Duration::from_secs(120);

/// The note's start (D-1126).
const NOTE: &str = "note: the build cache cannot be used (";

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
            .join("cache")
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

    /// A directory under it, created.
    fn dir(&self, name: &str) -> PathBuf {
        let dir = self.0.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A path under a regular file: nothing can be created there.
    fn unwritable(&self, name: &str) -> PathBuf {
        let file = self.0.join(name);
        std::fs::write(&file, b"a regular file").unwrap();
        file.join("cache")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A package of two modules with a warning, in `scratch`.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to write the package fails the test"
)]
fn package(scratch: &Scratch) -> PathBuf {
    let dir = scratch.dir("package");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("cheby.toml"),
        "name = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/main.cheby"),
        "//! Root.\n\nimport app::greet\nimport std::io\n\n\
         fn main() {\n  let unused = 1\n  io::println(greet::hello(\"cache\"))\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/greet.cheby"),
        "//! Greets.\n\npub fn hello(who: String) -> String {\n  \"hello {who}\"\n}\n",
    )
    .unwrap();
    dir
}

/// `cheby <args>` in `dir` with `CHEBY_CACHE` set to `cache`, `TMPDIR` to
/// `tmp`, and `env` added.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to spawn `cheby` fails the test"
)]
fn cheby(dir: &Path, args: &[&str], cache: &Path, tmp: &Path, env: &[(&str, &Path)]) -> RunOutput {
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never")
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, cache)
        .env("TMPDIR", tmp)
        .env_remove("CHEBY_CACHE_LOG")
        .env_remove("CHEBY_DUMP_DIR");
    for (name, value) in env {
        cmd.env(name, value);
    }
    process::run(cmd, None, TIMEOUT).unwrap()
}

/// The entries of `dir`.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: an unreadable directory fails the test"
)]
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// D-959: a cache that cannot be created makes the command use a
/// temporary one: the note once, first, on standard error, then exactly
/// what a writable cache gives, and no temporary cache left behind.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: output without the note's line fails the test"
)]
fn falls_back(command: &str) {
    let scratch = Scratch::new(command);
    let dir = package(&scratch);
    let tmp = scratch.dir("tmp");
    let writable = scratch.path().join("writable");
    let good = cheby(&dir, &[command], &writable, &tmp, &[]);
    assert!(
        !good.stderr.contains("note: the build cache"),
        "{}",
        good.stderr
    );
    let unwritable = scratch.unwritable("file");
    let out = cheby(&dir, &[command], &unwritable, &tmp, &[]);
    assert!(out.stderr.starts_with(NOTE), "{}", out.stderr);
    assert_eq!(out.stderr.matches("note: the build cache").count(), 1);
    let (note, rest) = out.stderr.split_once('\n').unwrap();
    assert!(note.contains("CHEBY_CACHE"), "{note}");
    assert!(note.contains("temporary"), "{note}");
    assert!(note.contains(&unwritable.display().to_string()), "{note}");
    assert_eq!(rest, good.stderr);
    assert_eq!(out.stdout, good.stdout);
    assert_eq!(out.status, good.status);
    assert_eq!(
        entries(&tmp),
        Vec::<String>::new(),
        "a temporary cache is left"
    );
}

/// `cheby check` evaluates constants with the JIT once the front end
/// reports no error, so it is skipped on Windows until M6 (D-545).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn cache_unwritable_check_falls_back() {
    falls_back("check");
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn cache_unwritable_run_falls_back() {
    falls_back("run");
}

/// A relative `CHEBY_CACHE` is a tool error that names the variable, never
/// a fallback (D-959).
#[test]
fn cache_relative_is_a_tool_error() {
    let scratch = Scratch::new("relative");
    let dir = package(&scratch);
    let tmp = scratch.dir("tmp");
    let out = cheby(&dir, &["check"], Path::new("relative/cache"), &tmp, &[]);
    assert_eq!(out.status, Some(1));
    assert_eq!(
        out.stderr,
        "error: CHEBY_CACHE must be an absolute path, but it is `relative/cache`\n"
    );
    assert_eq!(out.stdout, "");
}

/// A root that others can write is a tool error (D-959).
#[cfg(unix)]
#[test]
fn cache_unsafe_root_is_a_tool_error() {
    use std::os::unix::fs::PermissionsExt as _;
    let scratch = Scratch::new("unsafe");
    let dir = package(&scratch);
    let tmp = scratch.dir("tmp");
    let root = scratch.dir("open");
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
    let out = cheby(&dir, &["check"], &root, &tmp, &[]);
    assert_eq!(out.status, Some(1), "{}", out.stderr);
    assert!(
        out.stderr.starts_with("error: the build cache `") && out.stderr.contains("is unsafe"),
        "{}",
        out.stderr
    );
    assert_eq!(entries(&tmp), Vec::<String>::new());
}

/// When the temporary cache cannot be created either, the command ends
/// with a tool error (D-959's last sentence), and runs nothing.
#[test]
fn cache_failed_fallback_is_a_tool_error() {
    let scratch = Scratch::new("no_fallback");
    let dir = package(&scratch);
    let cache = scratch.unwritable("file");
    let tmp = scratch.unwritable("tmpfile");
    for command in ["check", "run"] {
        let out = cheby(&dir, &[command], &cache, &tmp, &[]);
        assert_eq!(out.status, Some(1), "{command}: {}", out.stderr);
        assert!(
            out.stderr.starts_with("error: "),
            "{command}: {}",
            out.stderr
        );
        assert_eq!(out.stderr.lines().count(), 1, "{command}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{command}");
    }
}

/// The package's `front` and `parse` lines of the log at `path`, in its
/// order: std's own lines (`front.std`, `parse.std`, K6, D-980) are left
/// out, as D-1135's lines are package-only.
fn package_lines(path: &Path) -> String {
    let mut out = String::new();
    let text = std::fs::read_to_string(path).unwrap_or_default();
    for line in text
        .lines()
        .filter(|l| l.starts_with("front ") || l.starts_with("parse "))
    {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// `CHEBY_CACHE_LOG` gets one sorted `front <module> hit|miss` line per
/// package module (D-973): all misses on a fresh cache, all hits on the
/// next command. A command also lists each package module it parses
/// (`parse <module>`): both on the cold check, none on the warm one.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn cache_log_lists_front_lookups() {
    let scratch = Scratch::new("log");
    let dir = package(&scratch);
    let tmp = scratch.dir("tmp");
    let cache = scratch.path().join("cache");
    let log = scratch.path().join("log");
    let first = cheby(&dir, &["check"], &cache, &tmp, &[("CHEBY_CACHE_LOG", &log)]);
    assert_eq!(first.status, Some(0), "{}", first.stderr);
    assert_eq!(
        package_lines(&log),
        "front app miss\nfront app::greet miss\nparse app\nparse app::greet\n"
    );
    let second = cheby(&dir, &["check"], &cache, &tmp, &[("CHEBY_CACHE_LOG", &log)]);
    assert_eq!(package_lines(&log), "front app hit\nfront app::greet hit\n");
    assert_eq!(second.stderr, first.stderr);
    assert_eq!(second.stdout, first.stdout);
}
