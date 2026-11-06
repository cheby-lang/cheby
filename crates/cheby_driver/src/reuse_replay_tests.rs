//! The reuse lines cached beside code records (K4d, D-965,
//! D-1257): their bytes round-trip, malformed ones
//! miss, a stale code key finds none, and a warm `--reuse-report` replays
//! what a cold one printed, a unit without lines replaying none.
//!
//! Every test opens its own store in a fresh directory (D-1128).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::cache::{BuildId, CacheLog, Key, KeyKind, Store};
use crate::reuse_replay::{ReuseLine, ReuseLines, find_in, lines_key, store_in};
use crate::{Cache, Prepared, RunOptions, prepare_run_cached};

/// The build ID of every test store.
const BUILD: BuildId = BuildId([9; 16]);

/// A fresh directory under the system's temporary directory, removed on
/// drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cheby_driver_reuse_{}_{n}_{name}",
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

fn sample() -> ReuseLines {
    ReuseLines {
        lines: vec![
            ReuseLine {
                owner: None,
                line: 3,
                col: 7,
                text: "building `Box` allocates: no value of the same size dies before it".into(),
            },
            ReuseLine {
                owner: Some("Shape::area".into()),
                line: 12,
                col: 5,
                text: "building `Shape` reuses `current` when it is unique".into(),
            },
            ReuseLine {
                owner: Some("a test with spaces".into()),
                line: 40,
                col: 1,
                text: "`set::insert` gets `held` at its last use: in place when it is unique at run time".into(),
            },
        ],
    }
}

fn code_key(n: u64) -> Key {
    Key::builder(KeyKind::Code, BUILD).u64(n).finish()
}

#[test]
fn reuse_replay_lines_round_trip() {
    let lines = sample();
    let bytes = lines.encode();
    assert_eq!(ReuseLines::decode(&bytes), Some(lines));
    let empty = ReuseLines::default();
    assert_eq!(ReuseLines::decode(&empty.encode()), Some(empty));
    // The encoding is canonical: the same lines give the same bytes.
    assert_eq!(sample().encode(), bytes);
}

#[test]
fn reuse_replay_malformed_bytes_miss() {
    let bytes = sample().encode();
    for cut in 0..bytes.len() {
        assert_eq!(ReuseLines::decode(&bytes[..cut]), None, "cut at {cut}");
    }
    let mut flipped = bytes.clone();
    if let Some(b) = flipped.get_mut(6) {
        *b ^= 0x40;
    }
    assert_eq!(ReuseLines::decode(&flipped), None);
    let mut longer = bytes;
    longer.push(0);
    assert_eq!(ReuseLines::decode(&longer), None);
    assert_eq!(ReuseLines::decode(b"CBRL"), None);
}

#[test]
fn reuse_replay_stale_key_misses() {
    let dir = TempDir::new("stale");
    let store = Store::open(&dir.path().join("cache"), BUILD).unwrap();
    let share = store.share();
    let (old, new) = (code_key(1), code_key(2));
    assert_ne!(lines_key(&old, BUILD), lines_key(&new, BUILD));
    // The lines' key is never the code key itself.
    assert_ne!(lines_key(&old, BUILD), old);
    store_in(&share, &old, &sample());
    store.flush();
    assert_eq!(find_in(&share, true, &old), Some(sample()));
    assert_eq!(find_in(&share, true, &new), None);
    // A command that bypasses reads (`CHEBY_DUMP_DIR`) finds none.
    assert_eq!(find_in(&share, false, &old), None);
}

/// The package of the end-to-end tests: `main` builds a boxed value (a
/// line of the report), `plain` allocates nothing (a unit without lines).
fn package(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("cheby.toml"),
        "name = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/main.cheby"),
        "//! Root.\n\nimport app::plain\nimport std::io\n\n/// A box.\ntype Box { Box(Int) }\n\n\
         fn unbox(b: Box) -> Int {\n  let Box(n) = b\n  n\n}\n\n\
         fn main() {\n  let n = unbox(Box(plain::two()))\n  io::println(\"{n}\")\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/plain.cheby"),
        "//! Plain: no allocation.\n\n/// Two.\npub fn two() -> Int {\n  2\n}\n",
    )
    .unwrap();
}

/// `cheby run --reuse-report` through a cache over `cache_dir`: the report
/// and the log's `code.run` lines.
fn report_run(root: &Path, cache_dir: &Path, log: &Path) -> (String, Vec<String>) {
    let store = Store::open(cache_dir, BUILD).unwrap();
    let cache = Cache::with_store(store, CacheLog::new(Some(log.to_path_buf())));
    let options = RunOptions {
        reuse_report: true,
        ..RunOptions::default()
    };
    let report = match prepare_run_cached(root, options, &cache).unwrap() {
        Prepared::Ready(program) => program.reuse_report().to_owned(),
        Prepared::CompileFailed(out) => panic!("{}", out.render()),
    };
    cache.finish(false).unwrap();
    let lines = std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .filter(|l| l.starts_with("code.run "))
        .map(str::to_owned)
        .collect();
    (report, lines)
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn reuse_replay_warm_report_reads_records() {
    if std::env::var_os("CHEBY_RC").is_some() {
        // A naive run's report is its one line, and reads no lines.
        return;
    }
    let dir = TempDir::new("warm");
    let root = dir.path().join("app");
    package(&root);
    let cache = dir.path().join("cache");
    let (cold, cold_log) = report_run(&root, &cache, &dir.path().join("log-1"));
    assert!(cold.contains("reuse: "), "{cold}");
    assert_eq!(cold_log, ["code.run app miss", "code.run app::plain miss"]);
    let (warm, warm_log) = report_run(&root, &cache, &dir.path().join("log-2"));
    assert_eq!(warm, cold);
    // `app::plain` has no line: its record replays none.
    assert!(!warm.contains("plain.cheby"), "{warm}");
    assert_eq!(warm_log, ["code.run app hit", "code.run app::plain hit"]);
}
