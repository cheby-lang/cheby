//! Std in the build cache (K6: D-968, D-980, D-971, D-978, D-1135): std's
//! one code in the store, shared by every command and settings' kind,
//! loaded on a warm command without compiling std; std's front records,
//! read instead of parsing std; std's masks over every std module; and the
//! self-test's kind of its own.
//!
//! Every test opens its own store in a fresh directory (D-1128) and reads
//! the lookups through `CHEBY_CACHE_LOG`'s file (D-973).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::cache::{BuildId, BuildKind, CacheLog, Settings, Store};
use crate::std_cache::{std_code_key, std_code_key_at, std_front_key, std_settings};

/// The build ID of every test store.
const BUILD: BuildId = BuildId([12; 16]);

/// A fresh directory under the system's temporary directory, removed on
/// drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cheby_driver_std_cache_{}_{n}_{name}",
            std::process::id()
        ));
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

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Packages over one cache.
struct Fixture {
    cache: TempDir,
    packages: Vec<TempDir>,
    runs: u32,
}

impl Fixture {
    fn new(name: &str) -> Self {
        Self {
            cache: TempDir::new(&format!("{name}_cache")),
            packages: Vec::new(),
            runs: 0,
        }
    }

    /// A package `app` with `files` (paths under `src/`), returning its
    /// root.
    fn package(&mut self, files: &[(&str, &str)]) -> PathBuf {
        let dir = TempDir::new("pkg");
        std::fs::write(
            dir.path().join("cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        for (path, text) in files {
            let path = dir.path().join("src").join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let root = dir.path().to_path_buf();
        self.packages.push(dir);
        root
    }

    fn root(&self) -> PathBuf {
        self.cache.path().join("cache")
    }

    fn open(&mut self) -> (Cache, PathBuf) {
        self.runs = self.runs.checked_add(1).unwrap();
        let log = self.cache.path().join(format!("log-{}", self.runs));
        let store = Store::open(&self.root(), BUILD).unwrap();
        (
            Cache::with_store(store, CacheLog::new(Some(log.clone()))),
            log,
        )
    }

    /// `cheby run`'s compile of `root` through the cache: the log's lines.
    fn run(&mut self, root: &Path) -> Vec<String> {
        let (cache, log) = self.open();
        let prepared = prepare_run_cached(root, RunOptions::default(), &cache).unwrap();
        cache.finish(false).unwrap();
        assert!(matches!(prepared, Prepared::Ready(_)));
        read_log(&log)
    }

    /// `cheby check` of `root` through the cache: the log's lines.
    fn check(&mut self, root: &Path) -> Vec<String> {
        let (cache, log) = self.open();
        let out = check_cached(root, &CheckOptions::default(), &cache).unwrap();
        cache.finish(false).unwrap();
        assert!(
            !out.diagnostics.iter().any(Diagnostic::is_error),
            "{:?}",
            out.diagnostics
        );
        read_log(&log)
    }

    /// `cheby test`'s compile of `root` through the cache: the log's lines.
    fn test(&mut self, root: &Path) -> Vec<String> {
        let (cache, log) = self.open();
        let prepared = prepare_test_cached(root, &TestOptions::default(), &cache).unwrap();
        cache.finish(false).unwrap();
        assert!(matches!(prepared, PreparedTests::Ready(_)));
        read_log(&log)
    }

    /// Every record file of the cache with its bytes, sorted by path.
    fn records(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let dir = self.root().join("v1").join("rec");
        let Ok(shards) = std::fs::read_dir(&dir) else {
            return out;
        };
        for shard in shards {
            for file in std::fs::read_dir(shard.unwrap().path()).unwrap() {
                let path = file.unwrap().path();
                let bytes = std::fs::read(&path).unwrap();
                out.push((path, bytes));
            }
        }
        out.sort();
        out
    }
}

fn read_log(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The lines that start with `prefix` and a space.
fn lines(log: &[String], prefix: &str) -> Vec<String> {
    let prefix = format!("{prefix} ");
    log.iter()
        .filter(|l| l.starts_with(&prefix))
        .cloned()
        .collect()
}

/// The lines that start with `prefix` and end with ` miss`.
fn misses(log: &[String], prefix: &str) -> Vec<String> {
    lines(log, prefix)
        .into_iter()
        .filter(|l| l.ends_with(" miss"))
        .collect()
}

const IO_MAIN: &str = "//! Root.\n\nimport std::io\n\nfn main() {\n  io::println(\"hello\")\n}\n";
const JSON_MAIN: &str = "//! Root.\n\nimport std::io\nimport std::json\n\nfn main() {\n  \
    io::println(json::to_string(json::Int(7)))\n}\n";
const CONST_MAIN: &str = "//! Root.\n\nimport std::io\nimport std::list\n\n\
    const total: Int = list::length(list::range(0, 10))\n\nfn main() {\n  \
    io::println(\"{total}\")\n}\n\ntest \"total\" {\n  assert total == 10\n}\n";

/// D-980: the first `cheby run` on a fresh cache is std's first use: it
/// logs a `parse.std` line per std module and a `code.std` miss per std
/// module of its load set, and stores every std module's code record; the
/// second run compiles no std unit (every `code.std` lookup hits) and
/// parses no std module.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_cache_warm_run_compiles_no_std_unit() {
    let mut f = Fixture::new("warm_run");
    let app = f.package(&[("main.cheby", IO_MAIN)]);
    let cold = f.run(&app);
    // The first lookup that misses is std's first use, which stores every
    // std module's front and code records; the command's later lookups,
    // its `code.std` ones among them, find them.
    assert!(
        lines(&cold, "front.std").contains(&"front.std std::io miss".to_owned()),
        "{cold:?}"
    );
    assert!(
        lines(&cold, "code.std").contains(&"code.std std::io hit".to_owned()),
        "{cold:?}"
    );
    assert_eq!(
        lines(&cold, "parse.std").len(),
        std_lib::STD_MODULES.len(),
        "std's first use parses every std module once: {cold:?}"
    );
    let before = f.records();
    let warm = f.run(&app);
    assert!(misses(&warm, "code.std").is_empty(), "{warm:?}");
    assert!(
        lines(&warm, "code.std").contains(&"code.std std::io hit".to_owned()),
        "{warm:?}"
    );
    if crate::std_cache::FRONT_HITS {
        assert!(lines(&warm, "parse.std").is_empty(), "{warm:?}");
        assert!(misses(&warm, "front.std").is_empty(), "{warm:?}");
    }
    assert_eq!(f.records(), before, "a warm run writes no record");
    // D-1135: `parse` and `front` stay package-only.
    for line in lines(&warm, "parse").iter().chain(&lines(&warm, "front")) {
        assert!(line.contains(" app"), "{line}");
    }
}

/// D-980: a first use compiles every std module's code, so a command of
/// another package whose load set holds other std modules compiles no std
/// unit either.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_cache_first_use_serves_every_package() {
    let mut f = Fixture::new("every_package");
    let small = f.package(&[("main.cheby", IO_MAIN)]);
    let json = f.package(&[("main.cheby", JSON_MAIN)]);
    let _ = f.run(&small);
    let other = f.run(&json);
    assert!(misses(&other, "code.std").is_empty(), "{other:?}");
    assert!(
        lines(&other, "code.std").contains(&"code.std std::json hit".to_owned()),
        "{other:?}"
    );
}

/// D-978, D-980, D-1180's end: `cheby check`'s evaluation module, `cheby
/// run`'s and `cheby test`'s builds load the same std records: one code per
/// module and settings, `BuildKind::Std`, whatever the command's kind.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_cache_one_code_for_every_command() {
    let mut f = Fixture::new("every_command");
    let app = f.package(&[("main.cheby", CONST_MAIN)]);
    let run = f.run(&app);
    assert!(!lines(&run, "parse.std").is_empty(), "{run:?}");
    for log in [f.check(&app), f.test(&app), f.run(&app)] {
        assert!(misses(&log, "code.std").is_empty(), "{log:?}");
        assert!(!lines(&log, "code.std").is_empty(), "{log:?}");
        assert!(lines(&log, "parse.std").is_empty(), "{log:?}");
        assert!(misses(&log, "front.std").is_empty(), "{log:?}");
    }
}

/// D-962, D-980: a warm `cheby check` reads its package index and, of std,
/// each reached module's front record and interface artifact, and the
/// record once more where the build's walk reads its summaries
/// (`std_summaries`): at most three files per `front.std` hit, no package
/// record, and writes nothing.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_cache_warm_check_reads_each_std_record_once() {
    let mut f = Fixture::new("warm_check");
    let app = f.package(&[("main.cheby", IO_MAIN)]);
    let _ = f.check(&app);
    let (cache, log) = f.open();
    let out = check_cached(&app, &CheckOptions::default(), &cache).unwrap();
    let opens = cache.store().opens();
    let writes = cache.store().writes();
    cache.finish(false).unwrap();
    assert!(!out.diagnostics.iter().any(Diagnostic::is_error));
    let warm = read_log(&log);
    let hits = lines(&warm, "front.std").len();
    assert!(hits > 0, "{warm:?}");
    assert!(misses(&warm, "front.std").is_empty(), "{warm:?}");
    assert!(
        opens <= 1 + 3 * hits,
        "{opens} opens for {hits} std hits: {warm:?}"
    );
    assert!(
        opens > 2 * hits,
        "{opens} opens for {hits} std hits: {warm:?}"
    );
    assert_eq!(writes, 0);
}

/// D-958, D-980, D-1173: std's code keys hold the module, the settings with
/// their kind and the positions switch, and nothing else; a self-test's
/// kind keys other records than a user program's std, and std's front key
/// is the build ID and the path alone.
#[test]
fn std_cache_keys() {
    let command = Settings {
        kind: BuildKind::Run,
        rc_naive: false,
        unwind_edges: true,
        checked_counts: false,
        stack_check: true,
        target: "aarch64-macos".to_owned(),
    };
    let std = std_settings(&command);
    assert_eq!(std.kind, BuildKind::Std);
    assert_eq!(
        std_settings(&Settings {
            kind: BuildKind::Plain,
            ..command.clone()
        }),
        std
    );
    let key = std_code_key(BUILD, "std::io", &std);
    assert_eq!(key, std_code_key_at(BUILD, "std::io", &std, true));
    assert_ne!(key, std_code_key_at(BUILD, "std::io", &std, false));
    assert_ne!(key, std_code_key(BUILD, "std::list", &std));
    assert_ne!(key, std_code_key(BuildId([13; 16]), "std::io", &std));
    let naive = Settings {
        rc_naive: true,
        ..std.clone()
    };
    assert_ne!(key, std_code_key(BUILD, "std::io", &naive));
    let self_test = Settings {
        kind: BuildKind::StdSelfTest,
        ..std.clone()
    };
    assert_ne!(key, std_code_key(BUILD, "std::io", &self_test));
    assert_eq!(
        std_front_key(BUILD, "std::io"),
        std_front_key(BUILD, "std::io")
    );
    assert_ne!(
        std_front_key(BUILD, "std::io"),
        std_front_key(BUILD, "std::list")
    );
}

/// D-980, D-479: `cargo xtask test-std` builds std's tests under
/// `BuildKind::StdSelfTest`, inferring std's masks over its tests as a test
/// build does, and reads and writes none of std's records, so a user
/// program's std records stay as they were.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_cache_self_test_kind_is_its_own() {
    let mut f = Fixture::new("self_test");
    let app = f.package(&[("main.cheby", IO_MAIN)]);
    let _ = f.run(&app);
    let before = f.records();
    assert_ne!(before.len(), 0, "the run wrote no record");
    let report = test_std(Some("std::bool")).unwrap();
    assert!(
        report.modules.iter().all(|m| m.not_built.is_none()),
        "{report:?}"
    );
    assert_eq!(
        f.records(),
        before,
        "test-std changed the user kind's std records"
    );
    let warm = f.run(&app);
    assert!(misses(&warm, "code.std").is_empty(), "{warm:?}");
}

/// D-968: std's masks are inferred over every std module, whatever the
/// program imports: the masks of the whole-std computation are those of
/// std's units in any command, and some std function borrows.
#[test]
fn std_cache_masks_whole() {
    let whole = crate::std_cache::whole_std().unwrap();
    assert!(!whole.named().is_empty(), "no std function borrows");
    assert_eq!(whole.modules().len(), std_lib::STD_MODULES.len());
    let again = crate::std_cache::whole_std().unwrap();
    assert_eq!(whole.named(), again.named());
}

/// A constant whose evaluation keeps lists live through `std::list`'s
/// code: its memory peak depends on how std's functions count (D-617,
/// D-978).
const NEAR_LIMIT: &str = "//! A constant near its memory limit.\n\nimport std::list\n\n\
    pub const big: List<List<Int>> = list::map(list::range(0, 3000), fn(i) { [i, i] })\n\n\
    fn main() {\n  let _ = list::length(big)\n}\n";

/// Whether `cheby check` of `root` under a memory limit of `kib` KiB
/// reports E0076, the package's cache `f`'s.
fn check_fails_at(f: &mut Fixture, root: &Path, kib: u64) -> bool {
    write_limit(root, kib);
    let (cache, _) = f.open();
    let out = check_cached(root, &CheckOptions::default(), &cache).unwrap();
    cache.finish(false).unwrap();
    out.diagnostics
        .iter()
        .any(|d| d.code == cheby_diag::codes::CONSTANT_LIMIT)
}

/// Whether `cheby run`'s build of `root` under `kib` KiB reports E0076.
fn run_fails_at(f: &mut Fixture, root: &Path, kib: u64) -> bool {
    write_limit(root, kib);
    let (cache, _) = f.open();
    let prepared = prepare_run_cached(root, RunOptions::default(), &cache).unwrap();
    cache.finish(false).unwrap();
    match prepared {
        Prepared::Ready(_) => false,
        Prepared::CompileFailed(out) => out
            .diagnostics
            .iter()
            .any(|d| d.code == cheby_diag::codes::CONSTANT_LIMIT),
    }
}

/// Whether a REPL session in `root`, with std's code from the cache `f`'s
/// (K6, D-971), reports E0076 for an entry that names the constant.
fn repl_fails_at(f: &mut Fixture, root: &Path, kib: u64) -> bool {
    write_limit(root, kib);
    let (cache, _) = f.open();
    let entries = ["import app", "import std::list", "list::length(app::big)"];
    session_output(root, cache, &entries).contains("E0076")
}

/// What a REPL session in `root` with std's code from `cache` prints for
/// `entries`, both streams in order; the session ends, which finishes the
/// cache.
fn session_output(root: &Path, cache: Cache, entries: &[&'static str]) -> String {
    use crate::session::{Output, Session, SessionOptions, Start};
    struct Printed(String);
    impl Output for Printed {
        fn stdout(&mut self, text: &str) {
            self.0.push_str(text);
        }
        fn stderr(&mut self, text: &str) {
            self.0.push_str(text);
        }
    }
    let root = root.to_path_buf();
    let entries = entries.to_vec();
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || {
            let options = SessionOptions {
                threads: Some(2),
                ..SessionOptions::default()
            };
            let Start::Ready(mut session, _) =
                Session::start_cached(&root, options, Some(cache)).unwrap()
            else {
                panic!("the session did not start");
            };
            let mut out = Printed(String::new());
            for entry in entries {
                session.eval(entry, &mut out).unwrap();
            }
            let _ = session.end();
            out.0
        })
        .unwrap()
        .join()
        .unwrap()
}

/// D-971, D-980, D-964: the REPL runs std's normal code from the store: a
/// session on a cache whose std records a run wrote loads every std
/// module's record (no `code.std` miss), evaluates a std constant through
/// a record's slot, and stores nothing of its own.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_cache_repl_runs_std_from_the_store() {
    let mut f = Fixture::new("repl_store");
    let app = f.package(&[("main.cheby", IO_MAIN)]);
    let _ = f.run(&app);
    let before = f.records();
    let (cache, log) = f.open();
    let entries = [
        "import std::duration",
        "duration::to_milliseconds(duration::seconds(2))",
        "const twice: Int = 2 * duration::to_milliseconds(duration::zero) + 4",
        "twice",
    ];
    let printed = session_output(&app, cache, &entries);
    assert_eq!(printed, "2000 : Int\n4 : Int\n");
    let log = read_log(&log);
    assert_eq!(
        lines(&log, "code.std").len(),
        std_lib::STD_MODULES.len(),
        "{log:?}"
    );
    assert!(misses(&log, "code.std").is_empty(), "{log:?}");
    assert_eq!(f.records(), before, "the session stored something");
}

/// Writes `root`'s manifest with a `[const-eval]` memory limit of `kib`
/// KiB.
fn write_limit(root: &Path, kib: u64) {
    std::fs::write(
        root.join("cheby.toml"),
        format!(
            "name = \"app\"\nversion = \"0.1.0\"\n\n[const-eval]\nmemory-limit = \"{kib}KiB\"\n"
        ),
    )
    .unwrap();
}

/// D-978, D-1190's Open item: near the memory limit, the REPL's E0076
/// verdict equals the other commands'. The REPL evaluates in its session
/// JIT on std's normal code (D-971), and since K6 every evaluation module
/// runs std's one code too (no longer std without masks, D-1180), so the
/// limit at which `cheby check` starts to pass is the one at which `cheby
/// run` and the REPL do.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_cache_repl_memory_verdict_equals_the_commands() {
    let mut f = Fixture::new("near_limit");
    let app = f.package(&[("main.cheby", NEAR_LIMIT)]);
    // The least limit, in KiB, at which `cheby check` passes.
    let (mut low, mut high) = (1_u64, 64 * 1024);
    assert!(check_fails_at(&mut f, &app, low));
    assert!(!check_fails_at(&mut f, &app, high));
    while high - low > 1 {
        let mid = low + (high - low) / 2;
        if check_fails_at(&mut f, &app, mid) {
            low = mid;
        } else {
            high = mid;
        }
    }
    assert!(
        run_fails_at(&mut f, &app, low),
        "run passes below check's limit {high} KiB"
    );
    assert!(
        !run_fails_at(&mut f, &app, high),
        "run fails at check's limit {high} KiB"
    );
    assert!(
        repl_fails_at(&mut f, &app, low),
        "the REPL passes below check's limit {high} KiB"
    );
    assert!(
        !repl_fails_at(&mut f, &app, high),
        "the REPL fails at check's limit {high} KiB"
    );
}
