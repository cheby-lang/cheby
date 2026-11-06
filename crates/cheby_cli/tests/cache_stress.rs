//! D-960's stress test over `cheby check` (K8): eight `cheby check`
//! processes start together on one cold cache, five times on fresh caches.
//! Every process exits 0 and prints what a `cheby check` that reads no
//! cache prints; a ninth `cheby check` then misses nothing and parses no
//! module but the one that holds the constant, which D-1134 checks from
//! source on every warm check, and a `cheby run` on that cache prints the
//! program's output.
//! The store-level `cache_stress_eight_processes` (`cheby_driver`'s
//! `cache/tests.rs`) stays beside it.
//!
//! The eight start together without a sleep: each is a shell that creates
//! its ready file and then waits, spinning, for the start file, which the
//! test creates once all eight ready files exist (R26's rule: a test waits
//! on a condition, never on a timer).
//! D-1284: "what a cacheless `cheby check` prints" is the
//! output of a `cheby check` with `CHEBY_DUMP_DIR` set, which reads no
//! cache entry (D-965), on a cache of its own.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use cheby_test_support::cache::CACHE_ENV;
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
const TIMEOUT: Duration = Duration::from_secs(300);

/// The processes that start together.
const PROCESSES: usize = 8;

/// The rounds, each on a fresh cache.
const ROUNDS: usize = 5;

/// What the program prints.
const PRINTED: &str = "total 60, limit 3, words 3\n";

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
            .join("cache_stress")
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
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A package of three modules with a constant, and a warning (an unused
/// binding), in `scratch`.
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
        "//! Root.\n\nimport app::numbers\nimport app::words\nimport std::io\n\n\
         fn main() {\n  let unused = 1\n  let total = numbers::total()\n  \
         let count = words::count(\"a b c\")\n  let limit = numbers::limit\n  \
         io::println(\"total {total}, limit {limit}, words {count}\")\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/numbers.cheby"),
        "//! Numbers, with a constant.\n\nimport std::list\n\n\
         /// How many numbers [`total`] adds.\npub const limit: Int = list::length([10, 20, 30])\n\n\
         /// The sum of the numbers.\npub fn total() -> Int {\n  \
         list::fold([10, 20, 30], 0, fn(acc, x) { acc + x })\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/words.cheby"),
        "//! Words.\n\nimport std::list\nimport std::string\n\n\
         /// How many words `text` has.\npub fn count(text: String) -> Int {\n  \
         list::length(string::split(text, \" \"))\n}\n",
    )
    .unwrap();
    dir
}

/// A `cheby` command in `dir` with `CHEBY_CACHE` set to `cache`.
fn cheby(dir: &Path, cache: &Path) -> Command {
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never")
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, cache)
        .env_remove("CHEBY_CACHE_LOG")
        .env_remove("CHEBY_DUMP_DIR");
    cmd
}

/// Runs `cmd` to its end.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to spawn `cheby` fails the test"
)]
fn output(cmd: Command) -> RunOutput {
    process::run(cmd, None, process::scaled_timeout(TIMEOUT).unwrap()).unwrap()
}

/// `cheby check` that reads no cache entry: `CHEBY_DUMP_DIR` bypasses
/// every read (D-965), on a cache of its own.
fn cacheless_check(dir: &Path, scratch: &Scratch) -> RunOutput {
    let mut cmd = cheby(dir, &scratch.dir("cacheless-cache"));
    cmd.arg("check")
        .env("CHEBY_DUMP_DIR", scratch.dir("cacheless-dump"));
    output(cmd)
}

/// Starts [`PROCESSES`] `cheby check` processes on `cache` together and
/// returns what each printed.
#[expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper: a process that cannot start or a missed start fails the test"
)]
fn eight_checks(dir: &Path, cache: &Path, gate: &Path) -> Vec<RunOutput> {
    let start = gate.join("start");
    let handles: Vec<_> = (0..PROCESSES)
        .map(|i| {
            let ready = gate.join(format!("ready-{i}"));
            let mut cmd = Command::new("sh");
            // `exec` keeps the process `cheby check` itself; the loop spins
            // on a shell built-in, never on a timer.
            cmd.args([
                "-c",
                ": > \"$1\"; while [ ! -e \"$2\" ]; do :; done; exec \"$3\" --color=never check",
                "sh",
            ])
            .arg(&ready)
            .arg(&start)
            .arg(CHEBY)
            .current_dir(dir)
            .env("NO_COLOR", "1")
            .env(CACHE_ENV, cache)
            .env_remove("CHEBY_CACHE_LOG")
            .env_remove("CHEBY_DUMP_DIR");
            std::thread::spawn(move || output(cmd))
        })
        .collect();
    let deadline = Instant::now()
        .checked_add(process::scaled_timeout(TIMEOUT).unwrap())
        .unwrap();
    while !(0..PROCESSES).all(|i| gate.join(format!("ready-{i}")).exists()) {
        if Instant::now() > deadline {
            std::fs::write(&start, b"").unwrap();
            panic!("the {PROCESSES} processes were not all ready in time");
        }
        std::thread::yield_now();
    }
    std::fs::write(&start, b"").unwrap();
    handles.into_iter().map(|h| h.join().unwrap()).collect()
}

/// D-960: eight `cheby check` processes on one cold cache, five times on
/// fresh caches; then a ninth `cheby check` and a `cheby run` on the last.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn cache_stress_eight_checks() {
    let scratch = Scratch::new("eight-checks");
    let dir = package(&scratch);
    let cold = cacheless_check(&dir, &scratch);
    assert_eq!(cold.status, Some(0), "{}", cold.stderr);
    assert!(cold.stderr.contains("W0002"), "{}", cold.stderr);
    let mut last = PathBuf::new();
    for round in 0..ROUNDS {
        let cache = scratch.path().join(format!("cache-{round}"));
        let gate = scratch.dir(&format!("gate-{round}"));
        for (i, out) in eight_checks(&dir, &cache, &gate).iter().enumerate() {
            assert_eq!(
                (out.status, &out.stdout, &out.stderr),
                (Some(0), &cold.stdout, &cold.stderr),
                "round {round}, process {i}: {}",
                out.describe_status()
            );
        }
        last = cache;
    }

    // A ninth `cheby check` on the last cache misses nothing and parses no
    // module.
    let log = scratch.path().join("ninth.log");
    let mut ninth = cheby(&dir, &last);
    ninth.arg("check").env("CHEBY_CACHE_LOG", &log);
    let ninth = output(ninth);
    assert_eq!(
        (ninth.status, &ninth.stdout, &ninth.stderr),
        (Some(0), &cold.stdout, &cold.stderr)
    );
    let lines = std::fs::read_to_string(&log).unwrap();
    assert!(lines.contains("front app::numbers hit"), "{lines}");
    assert!(lines.contains("const app::numbers::limit hit"), "{lines}");
    let misses: Vec<&str> = lines.lines().filter(|l| l.ends_with(" miss")).collect();
    assert!(misses.is_empty(), "{misses:?}");
    // D-1134: a warm `cheby check` checks the module that holds a constant,
    // and the package modules it imports (none here), from source, hit or
    // not, so constant evaluation has their bodies; it parses no other.
    let parses: Vec<&str> = lines.lines().filter(|l| l.starts_with("parse")).collect();
    assert_eq!(parses, ["parse app::numbers"]);

    // `cheby run` on that cache prints the program's output.
    let mut run = cheby(&dir, &last);
    run.arg("run");
    let run = output(run);
    assert_eq!(run.status, Some(0), "{}", run.describe_status());
    assert_eq!(run.stdout, PRINTED);
    assert_eq!(run.stderr, cold.stderr);
}
