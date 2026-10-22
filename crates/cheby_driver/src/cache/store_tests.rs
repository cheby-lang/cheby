//! Tests of the store's writes behind the command, its shards, its
//! write-once rule and its reads (K3c; D-956, D-959, D-960, D-961;
//! GAP(cache-write-behind), GAP(cache-write-once-per-process),
//! GAP(cache-write-best-effort)).
//!
//! A test that needs writes to stay queued runs its puts inside a pool of
//! its own with one worker: `rayon::spawn` from that worker queues the job
//! on the same worker, which runs it only once the test's closure returns.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use super::store::{FileKind, HEADER_LEN, touch_if_old};
use super::writer::MAX_IN_FLIGHT;
use super::{BuildId, Hash128, Key, KeyKind, Store};

const BUILD: BuildId = BuildId([7; 16]);

/// A fresh directory under the system's temporary directory, deleted on
/// drop.
struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("cheby_store_tests_{}_{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A pool of one worker, whose queued jobs run only when it is idle.
fn one_worker() -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap()
}

/// Waits until every job queued on `pool` before this call has run: the
/// marker is injected from outside, so the worker takes it only after its
/// own queue is empty.
fn drain(pool: &rayon::ThreadPool) {
    let (tx, rx) = mpsc::channel();
    pool.spawn(move || tx.send(()).unwrap());
    rx.recv().unwrap();
}

fn key(name: &str) -> Key {
    Key::builder(KeyKind::Front, BUILD).str(name).finish()
}

/// The files under `dir`, recursively.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for e in entries {
        let path = e.unwrap().path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path);
        }
    }
    out.sort();
    out
}

#[test]
fn cache_store_round_trips_through_flush() {
    let dir = TestDir::new("flush-roundtrip");
    let store = Store::open(dir.path(), BUILD).unwrap();
    let blobs: Vec<Vec<u8>> = (0..64u32)
        .map(|i| format!("blob {i}").into_bytes())
        .collect();
    let names: Vec<Hash128> = blobs.iter().map(|b| store.put_blob(b).unwrap()).collect();
    for i in 0..64 {
        store
            .put_record(&key(&format!("m{i}")), format!("record {i}").as_bytes())
            .unwrap();
        store
            .put_hint(&key(&format!("m{i}")), format!("hint {i}").as_bytes())
            .unwrap();
    }
    store.flush();
    for (name, bytes) in names.iter().zip(&blobs) {
        let file = fs::read(store.entry_path(FileKind::Blob, *name)).unwrap();
        assert_eq!(&file[HEADER_LEN..], &bytes[..]);
        assert_eq!(store.get_blob(*name).unwrap().as_deref(), Some(&bytes[..]));
    }
    for i in 0..64 {
        let k = key(&format!("m{i}"));
        assert!(store.entry_path(FileKind::Record, k.hash()).is_file());
        assert_eq!(
            store.get_record(&k).unwrap(),
            Some(format!("record {i}").into_bytes())
        );
        assert_eq!(
            store.get_hint(&k).unwrap(),
            Some(format!("hint {i}").into_bytes())
        );
    }
    assert_eq!(store.writes(), 64 * 3);
    // Nothing is left in `tmp/`.
    assert_eq!(
        files_under(&dir.path().join("v1/tmp")),
        Vec::<PathBuf>::new()
    );
}

#[test]
fn cache_store_get_before_flush_sees_the_put() {
    let dir = TestDir::new("get-before-flush");
    let store = Store::open(dir.path(), BUILD).unwrap();
    let pool = one_worker();
    let k = key("m");
    pool.install(|| {
        let name = store.put_blob(b"queued blob").unwrap();
        store.put_record(&k, b"queued record").unwrap();
        store.put_hint(&k, b"queued hint").unwrap();
        // The writes wait behind this closure on the only worker.
        assert!(!store.entry_path(FileKind::Blob, name).exists());
        assert!(!store.entry_path(FileKind::Record, k.hash()).exists());
        let opens = store.opens();
        assert_eq!(
            store.get_blob(name).unwrap().as_deref(),
            Some(&b"queued blob"[..])
        );
        assert_eq!(
            store.get_record(&k).unwrap().as_deref(),
            Some(&b"queued record"[..])
        );
        assert_eq!(
            store.get_hint(&k).unwrap().as_deref(),
            Some(&b"queued hint"[..])
        );
        // Answered from memory: no file was opened.
        assert_eq!(store.opens(), opens);
        // The last put of a hint wins, before and after the write.
        store.put_hint(&k, b"newer hint").unwrap();
        assert_eq!(
            store.get_hint(&k).unwrap().as_deref(),
            Some(&b"newer hint"[..])
        );
        // `flush` on the worker itself takes the queued writes.
        store.flush();
        assert!(store.entry_path(FileKind::Blob, name).is_file());
    });
    drain(&pool);
    assert_eq!(store.writes(), 3);
    let again = Store::open(dir.path(), BUILD).unwrap();
    assert_eq!(
        again.get_hint(&k).unwrap().as_deref(),
        Some(&b"newer hint"[..])
    );
}

/// Two payloads whose blobs share a shard.
fn same_shard_blobs() -> (Vec<u8>, Vec<u8>) {
    let first = b"shard 0".to_vec();
    let byte = Hash128::of(&first).0[0];
    let second = (1u32..100_000)
        .map(|i| format!("shard {i}").into_bytes())
        .find(|b| Hash128::of(b).0[0] == byte)
        .unwrap();
    (first, second)
}

#[test]
fn cache_store_write_into_a_missing_shard() {
    let dir = TestDir::new("missing-shard");
    let store = Store::open(dir.path(), BUILD).unwrap();
    // Opening creates no shard.
    assert_eq!(fs::read_dir(dir.path().join("v1/blob")).unwrap().count(), 0);
    let (a, b) = same_shard_blobs();
    let a_name = store.put_blob(&a).unwrap();
    store.flush();
    let a_path = store.entry_path(FileKind::Blob, a_name);
    assert!(a_path.is_file());
    // Another process (a trim, a user) deletes the shard: the next write
    // into it makes it again.
    let shard = a_path.parent().unwrap().to_path_buf();
    fs::remove_dir_all(&shard).unwrap();
    let b_name = store.put_blob(&b).unwrap();
    store.flush();
    assert!(store.entry_path(FileKind::Blob, b_name).is_file());
    assert_eq!(store.get_blob(b_name).unwrap(), Some(b));
    assert_eq!(store.get_blob(a_name).unwrap(), None);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(&shard).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}

#[test]
fn cache_store_failed_write_is_skipped_and_misses() {
    let dir = TestDir::new("failed-write");
    let store = Store::open(dir.path(), BUILD).unwrap();
    // A regular file where a shard directory should be: no user, root
    // included, can create a file under it.
    let name = Hash128::of(b"blocked blob");
    let shard = store
        .entry_path(FileKind::Blob, name)
        .parent()
        .unwrap()
        .to_path_buf();
    fs::write(&shard, b"not a directory").unwrap();
    assert_eq!(store.put_blob(b"blocked blob").unwrap(), name);
    store.flush();
    assert_eq!(store.get_blob(name).unwrap(), None);
    assert_eq!(fs::read(&shard).unwrap(), b"not a directory");
    // A regular file where `tmp/` should be: every write fails, and is
    // skipped.
    let tmp = dir.path().join("v1/tmp");
    fs::remove_dir(&tmp).unwrap();
    fs::write(&tmp, b"not a directory").unwrap();
    let k = key("m");
    store.put_record(&k, b"record").unwrap();
    store.put_hint(&k, b"hint").unwrap();
    store.flush();
    assert_eq!(store.get_record(&k).unwrap(), None);
    assert_eq!(store.get_hint(&k).unwrap(), None);
    assert_eq!(store.writes(), 0);
    // Once the obstacle is gone, the next put writes the entry: a failed
    // write is not remembered as written.
    fs::remove_file(&tmp).unwrap();
    fs::create_dir(&tmp).unwrap();
    store.put_record(&k, b"record").unwrap();
    store.flush();
    assert_eq!(
        store.get_record(&k).unwrap().as_deref(),
        Some(&b"record"[..])
    );
    assert_eq!(store.writes(), 1);
}

#[test]
fn cache_store_drop_flushes() {
    let dir = TestDir::new("drop-flushes");
    let pool = one_worker();
    let payloads: Vec<Vec<u8>> = (0..200u32)
        .map(|i| format!("payload {i}").into_bytes())
        .collect();
    let names = pool.install(|| {
        let store = Store::open(dir.path(), BUILD).unwrap();
        let names: Vec<Hash128> = payloads
            .iter()
            .map(|p| store.put_blob(p).unwrap())
            .collect();
        for (i, p) in payloads.iter().enumerate() {
            store.put_record(&key(&format!("r{i}")), p).unwrap();
        }
        // Still queued behind this closure; the drop writes them.
        drop(store);
        names
    });
    let second = Store::open(dir.path(), BUILD).unwrap();
    for (i, (name, p)) in names.iter().zip(&payloads).enumerate() {
        assert_eq!(second.get_blob(*name).unwrap().as_ref(), Some(p));
        assert_eq!(
            second.get_record(&key(&format!("r{i}"))).unwrap().as_ref(),
            Some(p)
        );
    }
    drain(&pool);
}

#[test]
fn cache_store_temporary_store_with_queued_writes_leaves_nothing() {
    let dir = TestDir::new("temporary-queued");
    let pool = one_worker();
    pool.install(|| {
        let store = Store::temporary(dir.path(), BUILD).unwrap();
        assert!(store.is_temporary());
        for i in 0..100u32 {
            store.put_blob(format!("t{i}").as_bytes()).unwrap();
            store.put_record(&key(&format!("t{i}")), b"record").unwrap();
        }
        drop(store);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    });
    // The jobs queued on the worker run now and write nothing.
    drain(&pool);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn cache_store_blob_put_twice_is_written_once() {
    let dir = TestDir::new("write-once");
    let store = Store::open(dir.path(), BUILD).unwrap();
    // A clean package's empty diagnostics blob: one file, however many
    // modules name it, whether the writes are queued or done.
    for _ in 0..50 {
        store.put_blob(b"").unwrap();
    }
    store.flush();
    store.put_blob(b"").unwrap();
    let k = key("m");
    store.put_record(&k, b"record").unwrap();
    store.put_record(&k, b"record").unwrap();
    store.put_hint(&k, b"hint").unwrap();
    store.put_hint(&k, b"hint").unwrap();
    assert_eq!(store.writes(), 3);
    assert_eq!(files_under(&dir.path().join("v1")).len(), 3);
    // A hint is written again with other bytes, and so is a record (its
    // key fixes its bytes in a real command, D-264).
    store.put_hint(&k, b"other hint").unwrap();
    store.put_record(&k, b"other record").unwrap();
    assert_eq!(store.writes(), 5);
    assert_eq!(
        store.get_hint(&k).unwrap().as_deref(),
        Some(&b"other hint"[..])
    );
    drop(store);
    // An entry this process read and found valid is not written again: a
    // record, and a blob where the read checks its hash (debug builds; a
    // release build writes it again, since a damaged blob reads as valid
    // there and its next put must replace it, D-960).
    let second = Store::open(dir.path(), BUILD).unwrap();
    assert!(second.get_record(&k).unwrap().is_some());
    second.put_record(&k, b"other record").unwrap();
    assert_eq!(second.writes(), 0);
    let empty = Hash128::of(b"");
    assert_eq!(second.get_blob(empty).unwrap().as_deref(), Some(&b""[..]));
    assert_eq!(second.put_blob(b"").unwrap(), empty);
    let blob_writes = usize::from(!cfg!(debug_assertions));
    assert_eq!(second.writes(), blob_writes);
    // A file found missing is written again.
    fs::remove_file(second.entry_path(FileKind::Blob, empty)).unwrap();
    assert_eq!(second.get_blob(empty).unwrap(), None);
    second.put_blob(b"").unwrap();
    assert_eq!(second.writes(), blob_writes + 1);
    // So is a record whose payload was damaged.
    let path = second.entry_path(FileKind::Record, k.hash());
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&path, &bytes).unwrap();
    assert!(second.get_record(&k).unwrap().is_some());
    second.put_record(&k, b"other record").unwrap();
    assert_eq!(second.writes(), blob_writes + 2);
    assert_eq!(&fs::read(&path).unwrap()[HEADER_LEN..], b"other record");
}

#[test]
fn cache_store_read_opens_each_entry_once() {
    let dir = TestDir::new("opens");
    let store = Store::open(dir.path(), BUILD).unwrap();
    let k = key("m");
    store.put_record(&k, b"record").unwrap();
    store.flush();
    let before = store.opens();
    assert!(store.get_record(&k).unwrap().is_some());
    assert_eq!(store.opens(), before + 1);
    // A missing entry is one attempt too, and still a miss.
    assert_eq!(store.get_hint(&k).unwrap(), None);
    assert_eq!(store.opens(), before + 2);
    // A file shorter than its header says, or longer, is a miss.
    let path = store.entry_path(FileKind::Record, k.hash());
    let mut bytes = fs::read(&path).unwrap();
    bytes.push(0);
    fs::write(&path, &bytes).unwrap();
    assert_eq!(store.get_record(&k).unwrap(), None);
    bytes.truncate(bytes.len() - 2);
    fs::write(&path, &bytes).unwrap();
    assert_eq!(store.get_record(&k).unwrap(), None);
    fs::write(&path, &bytes[..HEADER_LEN - 1]).unwrap();
    assert_eq!(store.get_record(&k).unwrap(), None);
}

/// `touch_if_old`, which reads the time by path, touches as a read does
/// (D-961).
#[test]
fn cache_store_touch_by_path_matches_a_read() {
    let dir = TestDir::new("touch-path");
    let path = dir.path().join("entry");
    fs::write(&path, b"x").unwrap();
    let now = SystemTime::now();
    let f = fs::File::options().write(true).open(&path).unwrap();
    f.set_modified(now - Duration::from_hours(2)).unwrap();
    drop(f);
    touch_if_old(&path, now);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), now);
    let half = now - Duration::from_mins(30);
    let f = fs::File::options().write(true).open(&path).unwrap();
    f.set_modified(half).unwrap();
    drop(f);
    touch_if_old(&path, now);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), half);
}

#[test]
fn cache_store_too_many_bytes_in_flight_write_on_the_caller() {
    let dir = TestDir::new("in-flight");
    let store = Store::open(dir.path(), BUILD).unwrap();
    let pool = one_worker();
    pool.install(|| {
        let small = store.put_blob(b"small").unwrap();
        let big = vec![1u8; MAX_IN_FLIGHT];
        let big_name = store.put_blob(&big).unwrap();
        // The small one waits behind this closure; the big one would pass
        // the bound, so the put wrote it before returning.
        assert!(!store.entry_path(FileKind::Blob, small).exists());
        assert!(store.entry_path(FileKind::Blob, big_name).is_file());
        store.flush();
        assert!(store.entry_path(FileKind::Blob, small).is_file());
    });
    drain(&pool);
    assert_eq!(store.writes(), 2);
}
