//! Code records sealed with their hash (K8: D-960, D-1260, D-1264; the
//! review's RB-5).
//!
//! A package code record and a std code record are stored as
//! `CodeRecord::encode()`'s bytes followed by their SipHash-1-3 128
//! (`cache::seal`). A byte flipped in a record's payload or in its seal, or
//! a record cut short, is a miss in every build, logged as `code.run …
//! miss` or `code.std … miss`, and the command writes the record again; the
//! program then runs as a cold build runs it. Each test opens its own store
//! in a fresh directory (D-1128) and reads the lookups through
//! `CHEBY_CACHE_LOG`'s file (D-973), never a timing.
//!
//! The program's `main` returns `Err` when what it computes is wrong, so
//! its exit status shows that the code it ran is the code a cold build
//! runs: a damaged record that were loaded would end it with another
//! status, or with a signal that ends the test binary.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::cache::seal::{record_hash, seal_code, unseal_code};
use crate::cache::{BuildId, CacheLog, Hash128, Store};

/// The build ID of every test store.
const BUILD: BuildId = BuildId([23; 16]);

/// The length of an entry file's header (`cache/store.rs`): the magic, the
/// build ID and the payload's length.
const HEADER: usize = 32;

/// A fresh directory under the system's temporary directory, removed on
/// drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cheby_driver_seal_{}_{n}_{name}",
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

/// The root: it calls `calc`, and `main` returns `Err` unless the sums are
/// right.
const MAIN: &str = "//! Root.\n\nimport app::calc\nimport std::io\nimport std::list\n\n\
    fn main() -> Result<Nil, String> {\n  let total = calc::sum([1, 2, 3, 4])\n  \
    let words = list::length(calc::words(\"a b c\"))\n  io::println(\"{total} {words}\")\n  \
    case total == 10 && words == 3 {\n    True => Ok(Nil)\n    False => Err(\"wrong\")\n  }\n}\n";
/// The package module whose record the tests damage.
const CALC: &str = "//! Calc.\n\nimport std::list\nimport std::string\n\n\
    /// The sum of `xs`.\npub fn sum(xs: List<Int>) -> Int {\n  list::fold(xs, 0, fn(acc, x) { acc + x })\n}\n\n\
    /// The words of `text`.\npub fn words(text: String) -> List<String> {\n  string::split(text, \" \")\n}\n";

/// A package `app` with [`MAIN`] and [`CALC`], and a fresh cache.
struct Fixture {
    package: TempDir,
    cache: TempDir,
    runs: u32,
}

/// One `cheby run` through the cache.
#[derive(Debug)]
struct Ran {
    /// The program's exit status.
    exit: i32,
    /// The rendered warnings of the build.
    output: String,
    /// The log's `code.run` lines.
    code: Vec<String>,
    /// The log's `code.std` lines.
    std: Vec<String>,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let package = TempDir::new(name);
        std::fs::write(
            package.path().join("cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let src = package.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("main.cheby"), MAIN).unwrap();
        std::fs::write(src.join("calc.cheby"), CALC).unwrap();
        Self {
            package,
            cache: TempDir::new(&format!("{name}_cache")),
            runs: 0,
        }
    }

    fn dir(&self) -> &Path {
        self.package.path()
    }

    /// `cheby run` through the cache, running the program.
    fn run(&mut self) -> Ran {
        self.runs = self.runs.checked_add(1).unwrap();
        let log = self.cache.path().join(format!("log-{}", self.runs));
        let store = Store::open(&self.cache.path().join("cache"), BUILD).unwrap();
        let cache = Cache::with_store(store, CacheLog::new(Some(log.clone())));
        let prepared = prepare_run_cached(self.dir(), RunOptions::default(), &cache).unwrap();
        cache.finish(false).unwrap();
        let Prepared::Ready(program) = prepared else {
            panic!("the package does not compile");
        };
        let output = program.output().render();
        let exit = program.run().unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        let of = |prefix: &str| -> Vec<String> {
            text.lines()
                .filter(|l| l.starts_with(prefix))
                .map(str::to_owned)
                .collect()
        };
        Ran {
            exit,
            output,
            code: of("code.run "),
            std: of("code.std "),
        }
    }

    /// The cacheless `cheby run`: the warnings and the exit status.
    fn cold(&self) -> (String, i32) {
        let Prepared::Ready(program) = prepare_run(self.dir(), RunOptions::default()).unwrap()
        else {
            panic!("the package does not compile");
        };
        let output = program.output().render();
        (output, program.run().unwrap())
    }

    /// Every code record file of the store whose unit's source path is
    /// `source` (`src/calc.cheby`, `std/list.cheby`), with its bytes.
    fn records_of(&self, source: &str) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let dir = self.cache.path().join("cache/v1/rec");
        for shard in std::fs::read_dir(dir).unwrap() {
            for file in std::fs::read_dir(shard.unwrap().path()).unwrap() {
                let path = file.unwrap().path();
                let bytes = std::fs::read(&path).unwrap();
                let Some(record) = bytes.get(HEADER..).and_then(unseal_code) else {
                    continue;
                };
                if record.source.as_deref() == Some(source) {
                    out.push((path, bytes));
                }
            }
        }
        out.sort();
        out
    }
}

/// A unit's line, `code.run <module> hit|miss` or `code.std …`.
fn line(artifact: &str, module: &str, verdict: &str) -> String {
    format!("{artifact} {module} {verdict}")
}

/// Whether the environment's positions are on (`CHEBY_POSITIONS`, D-1173):
/// a record names its source path only then.
fn positions_on() -> bool {
    crate::backend::mir_options_from_env().is_ok_and(|mir| mir.positions)
}

/// Damages the one record of `source` with `damage`, then checks that the
/// next run misses `module`'s lookup of `artifact`, writes the record
/// again, and exits as the cold build does; and that the run after hits it.
fn damaged_is_a_miss(
    name: &str,
    source: &str,
    artifact: &str,
    module: &str,
    damage: fn(&mut Vec<u8>),
) {
    if !positions_on() {
        return;
    }
    let mut f = Fixture::new(name);
    let (cold_output, cold_exit) = f.cold();
    assert_eq!(cold_exit, 0);
    let first = f.run();
    assert_eq!((first.exit, &first.output), (0, &cold_output));
    let warm = f.run();
    let hit = line(artifact, module, "hit");
    let miss = line(artifact, module, "miss");
    let lookups = |ran: &Ran| {
        if artifact == "code.std" {
            ran.std.clone()
        } else {
            ran.code.clone()
        }
    };
    assert!(lookups(&warm).contains(&hit), "{warm:?}");

    let records = f.records_of(source);
    assert_eq!(records.len(), 1, "one record of {source}");
    let (path, good) = records.into_iter().next().unwrap();
    let mut bad = good.clone();
    damage(&mut bad);
    assert_ne!(bad, good);
    std::fs::write(&path, &bad).unwrap();

    let damaged = f.run();
    assert!(lookups(&damaged).contains(&miss), "{damaged:?}");
    assert_eq!((damaged.exit, &damaged.output), (0, &cold_output));
    // Written again, byte for byte the record a cold run wrote.
    assert_eq!(std::fs::read(&path).unwrap(), good);
    let again = f.run();
    assert!(lookups(&again).contains(&hit), "{again:?}");
    assert_eq!((again.exit, &again.output), (0, &cold_output));
}

/// The length of the payload of the entry file `bytes`, seal included.
fn payload_len(bytes: &[u8]) -> usize {
    bytes.len().checked_sub(HEADER).unwrap()
}

/// Flips one byte in the middle of the payload, before the seal.
#[expect(
    clippy::ptr_arg,
    reason = "every damage has one type, and the truncations need a `Vec`"
)]
fn flip_payload(bytes: &mut Vec<u8>) {
    let at = HEADER
        .checked_add(payload_len(bytes).checked_sub(16).unwrap() / 2)
        .unwrap();
    bytes[at] ^= 0x5a;
}

/// Flips one byte of the seal, the file's last 16 bytes.
#[expect(
    clippy::ptr_arg,
    reason = "every damage has one type, and the truncations need a `Vec`"
)]
fn flip_seal(bytes: &mut Vec<u8>) {
    let at = bytes.len().checked_sub(7).unwrap();
    bytes[at] ^= 0x01;
}

/// Cuts the file to a little more than half: the header's length no longer
/// matches.
fn truncate(bytes: &mut Vec<u8>) {
    bytes.truncate(HEADER.checked_add(payload_len(bytes) / 2).unwrap());
}

/// Cuts the payload short and fixes the header's length, so only the seal
/// tells: the store reads it, and the seal does not match.
fn truncate_with_header(bytes: &mut Vec<u8>) {
    let keep = payload_len(bytes) / 2;
    bytes.truncate(HEADER.checked_add(keep).unwrap());
    let len = u64::try_from(keep).unwrap().to_le_bytes();
    bytes[HEADER.checked_sub(8).unwrap()..HEADER].copy_from_slice(&len);
}

/// RB-5: a byte flipped in a package code record's payload is a miss in
/// release builds as in debug builds, written again, and the program runs
/// as a cold build runs it.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn seal_package_payload_flip_is_a_miss() {
    damaged_is_a_miss(
        "payload",
        "src/calc.cheby",
        "code.run",
        "app::calc",
        flip_payload,
    );
}

/// RB-5: a byte flipped in a package code record's seal is a miss.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn seal_package_seal_flip_is_a_miss() {
    damaged_is_a_miss("seal", "src/calc.cheby", "code.run", "app::calc", flip_seal);
}

/// RB-5 (the review's std half): a byte flipped in a std code record, which
/// every program shares (D-980), is a miss; std's first use runs again and
/// writes it again.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn seal_std_payload_flip_is_a_miss() {
    damaged_is_a_miss(
        "std",
        "std/list.cheby",
        "code.std",
        "std::list",
        flip_payload,
    );
}

/// A record cut short is a miss: by its header's length, and with the
/// length fixed, by its seal.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn seal_truncated_record_is_a_miss() {
    damaged_is_a_miss(
        "truncated",
        "src/calc.cheby",
        "code.run",
        "app::calc",
        truncate,
    );
    damaged_is_a_miss(
        "truncated_header",
        "std/string.cheby",
        "code.std",
        "std::string",
        truncate_with_header,
    );
}

/// The seal is `CodeRecord::encode()` followed by its SipHash-1-3 128, the
/// record's bytes unchanged (D-1253); `record_hash` is the hash of those
/// bytes, which is also the seal.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn seal_wraps_the_record_bytes() {
    if !positions_on() {
        return;
    }
    let mut f = Fixture::new("wraps");
    assert_eq!(f.run().exit, 0);
    let (_, bytes) = f.records_of("src/calc.cheby").into_iter().next().unwrap();
    let stored = &bytes[HEADER..];
    let record = unseal_code(stored).unwrap();
    let encoded = record.encode();
    assert_eq!(seal_code(&record), stored);
    assert_eq!(&stored[..encoded.len()], encoded.as_slice());
    assert_eq!(stored.len(), encoded.len() + 16);
    assert_eq!(record_hash(&record), Hash128::of(&encoded));
    assert_eq!(&stored[encoded.len()..], record_hash(&record).0.as_slice());
    // An unsealed record (an older `cheby`'s form) is not read.
    assert!(unseal_code(&encoded).is_none());
    assert!(unseal_code(&[]).is_none());
    assert!(unseal_code(&stored[..15]).is_none());
}

/// What sealing adds to a warm run, as a count, never a time (plan §0):
/// the code records a warm `cheby run` of `bench/runtime`'s
/// `rc/list_pipeline` unseals, and their bytes, each hashed once more by
/// its seal beside `Shared::get`'s digest of the whole file. Every code
/// lookup that hits unseals exactly one record. The numbers are printed
/// for the report (`--nocapture`); the program itself is not run.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn seal_counts_the_bytes_a_warm_run_unseals() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/runtime/rc/list_pipeline");
    let cache = TempDir::new("list_pipeline");
    let root = cache.path().join("cache");
    let open = |n: u32| {
        let log = cache.path().join(format!("log-{n}"));
        let store = Store::open(&root, BUILD).unwrap();
        (
            Cache::with_store(store, CacheLog::new(Some(log.clone()))),
            log,
        )
    };
    let (cold, _) = open(1);
    let prepared = prepare_run_cached(&dir, RunOptions::default(), &cold).unwrap();
    cold.finish(false).unwrap();
    assert!(matches!(prepared, Prepared::Ready(_)));
    drop(prepared);

    let (warm, log) = open(2);
    let prepared = prepare_run_cached(&dir, RunOptions::default(), &warm).unwrap();
    let reads = warm.store().record_reads();
    warm.finish(false).unwrap();
    assert!(matches!(prepared, Prepared::Ready(_)));
    let (mut records, mut bytes) = (0_usize, 0_usize);
    for (name, len) in reads {
        let hex = name.to_hex();
        let shard = hex.get(..2).unwrap();
        let file = std::fs::read(root.join("v1/rec").join(shard).join(&hex)).unwrap();
        if file.get(HEADER..).and_then(unseal_code).is_some() {
            records = records.checked_add(1).unwrap();
            bytes = bytes.checked_add(len).unwrap();
        }
    }
    let text = std::fs::read_to_string(&log).unwrap();
    let code = |prefix: &str| {
        text.lines()
            .filter(|l| l.starts_with(prefix) && l.ends_with(" hit"))
            .count()
    };
    let (package, std) = (code("code.run "), code("code.std "));
    assert!(
        text.lines()
            .all(|l| !(l.starts_with("code.") && l.ends_with(" miss"))),
        "{text}"
    );
    assert_eq!(records, package.checked_add(std).unwrap(), "{text}");
    println!(
        "seal: a warm `cheby run` of rc/list_pipeline unseals {records} code records \
         ({package} package, {std} std), {bytes} bytes"
    );
}
