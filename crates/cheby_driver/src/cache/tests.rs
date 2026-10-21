//! Tests of the store: keys, files, location, permissions, trimming, the
//! log and eight concurrent processes. No test sets an environment
//! variable of its own process: the environment is a value, or it is set
//! on a child's `Command`.

use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use super::location::{Os, cache_root_on};
use super::store::{BLOB_MAGIC, HEADER_LEN, RECORD_MAGIC};
use super::*;

/// A fresh directory under the system's temporary directory, deleted on
/// drop.
struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cheby-k1-{}-{n}-{name}", std::process::id()));
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

const BUILD: BuildId = BuildId([7; 16]);
const OTHER_BUILD: BuildId = BuildId([8; 16]);

fn settings() -> Settings {
    Settings {
        kind: BuildKind::Run,
        rc_naive: false,
        unwind_edges: true,
        checked_counts: false,
        stack_check: true,
        target: "aarch64-apple-darwin".to_owned(),
    }
}

fn set_mtime(path: &Path, t: SystemTime) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

/// The first two hex digits: the shard directory.
fn shard(hex: &str) -> &str {
    hex.get(..2).unwrap()
}

/// The file of entry `name` under `<root>/v1/<dir>/`.
fn entry(root: &Path, dir: &str, name: Hash128) -> PathBuf {
    let hex = name.to_hex();
    root.join("v1").join(dir).join(shard(&hex)).join(&hex)
}

fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

fn files_in(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(files_in(&p));
        } else {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// The canonical encoding, spelled out: a key equals the hash of it.
fn encode(fields: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for f in fields {
        out.extend_from_slice(&u64::try_from(f.len()).unwrap().to_le_bytes());
        out.extend_from_slice(f);
    }
    out
}

// --- Keys -----------------------------------------------------------------

#[test]
fn cache_key_encoding_is_length_prefixed_with_kind_first() {
    let key = Key::builder(KeyKind::Front, BUILD)
        .str("app::main")
        .u64(3)
        .hash(Hash128([1; 16]))
        .bytes(b"")
        .finish();
    let manual = encode(&[
        b"front",
        &[7; 16],
        b"app::main",
        &3u64.to_le_bytes(),
        &[1; 16],
        b"",
    ]);
    assert_eq!(key.hash(), Hash128::of(&manual));
}

#[test]
fn cache_key_stability_pins_hex_constants() {
    // These constants pin the encoding and the hash: if one changes, every
    // cache on every machine misses once. Change them only together with
    // the layout's version (`v1/`) or on purpose.
    assert_eq!(
        Hash128::of(b"").to_hex(),
        "0db4d9615d9334210333fb31d05e1cb9",
        "SipHash-1-3 128 of nothing, zero keys"
    );
    assert_eq!(
        Hash128::of(b"cheby").to_hex(),
        "7090b8adf6e98c882f0d3e75a6c2332a",
    );
    let front = Key::builder(KeyKind::Front, BUILD)
        .str("app::main")
        .finish();
    assert_eq!(front.hash().to_hex(), "51b5b8ba77e97b2e0965bfe5c3d6e58d");
    let code = Key::builder(KeyKind::Code, BUILD)
        .hash(Hash128([2; 16]))
        .settings(&settings())
        .finish();
    assert_eq!(code.hash().to_hex(), "015b31ead847d81e53b22343df30ac0b");
}

#[test]
fn cache_key_moving_bytes_between_fields_changes_the_key() {
    let a = Key::builder(KeyKind::Code, BUILD)
        .str("ab")
        .str("c")
        .finish();
    let b = Key::builder(KeyKind::Code, BUILD)
        .str("a")
        .str("bc")
        .finish();
    let c = Key::builder(KeyKind::Code, BUILD).str("abc").finish();
    let d = Key::builder(KeyKind::Code, BUILD)
        .str("abc")
        .str("")
        .finish();
    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_ne!(c, d);
}

#[test]
fn cache_key_kind_and_build_id_enter_the_key() {
    let kinds = [
        KeyKind::Front,
        KeyKind::Facts,
        KeyKind::Masks,
        KeyKind::Code,
        KeyKind::Tests,
        KeyKind::Const,
        KeyKind::Reads,
        KeyKind::LReads,
        KeyKind::CObj,
    ];
    let tags: Vec<_> = kinds.iter().map(|k| k.tag()).collect();
    assert_eq!(
        tags,
        [
            "front", "facts", "masks", "code", "tests", "const", "reads", "lreads", "cobj"
        ]
    );
    let mut keys: Vec<_> = kinds
        .iter()
        .map(|&k| Key::builder(k, BUILD).str("m").finish().hash())
        .collect();
    keys.push(
        Key::builder(KeyKind::Front, OTHER_BUILD)
            .str("m")
            .finish()
            .hash(),
    );
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), 10);
}

#[test]
fn cache_key_every_setting_enters_the_key() {
    let base = settings();
    let key = |s: &Settings| Key::builder(KeyKind::Code, BUILD).settings(s).finish();
    let mut variants = vec![base.clone()];
    for kind in [
        BuildKind::Test,
        BuildKind::Plain,
        BuildKind::Std,
        BuildKind::StdSelfTest,
    ] {
        variants.push(Settings {
            kind,
            ..base.clone()
        });
    }
    variants.push(Settings {
        rc_naive: true,
        ..base.clone()
    });
    variants.push(Settings {
        unwind_edges: false,
        ..base.clone()
    });
    variants.push(Settings {
        checked_counts: true,
        ..base.clone()
    });
    variants.push(Settings {
        stack_check: false,
        ..base.clone()
    });
    variants.push(Settings {
        target: "x86_64-unknown-linux-gnu".to_owned(),
        ..base.clone()
    });
    let mut keys: Vec<_> = variants.iter().map(|s| key(s).hash()).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), variants.len());
    assert_eq!(
        [
            BuildKind::Run,
            BuildKind::Test,
            BuildKind::Plain,
            BuildKind::Std,
            BuildKind::StdSelfTest
        ]
        .map(BuildKind::tag),
        ["run", "test", "plain", "std", "std-self-test"]
    );
}

#[test]
fn cache_key_hex_round_trips() {
    let h = Hash128::of(b"round trip");
    assert_eq!(Hash128::from_hex(&h.to_hex()), Some(h));
    assert_eq!(Hash128::from_hex(&h.to_hex().to_uppercase()), Some(h));
    assert_eq!(Hash128::from_hex("00"), None);
    assert_eq!(Hash128::from_hex(&"g".repeat(32)), None);
    assert_eq!(BUILD.to_hex(), "07".repeat(16));
}

// --- The store ------------------------------------------------------------

#[test]
fn cache_store_open_creates_the_layout() {
    let dir = TestDir::new("layout");
    let root = dir.path().join("cache");
    let store = Store::open(&root, BUILD).unwrap();
    assert!(!store.is_temporary());
    assert_eq!(store.build_id(), BUILD);
    assert!(
        fs::read_to_string(root.join("README"))
            .unwrap()
            .contains("Cheby")
    );
    for d in ["blob", "rec", "hint", "tmp"] {
        assert!(root.join("v1").join(d).is_dir(), "{d}");
    }
    drop(store);
    // A second open of an existing cache works and keeps it.
    let _again = Store::open(&root, BUILD).unwrap();
    assert!(root.join("README").is_file());
}

#[test]
fn cache_store_round_trips() {
    let dir = TestDir::new("roundtrip");
    let store = Store::open(dir.path(), BUILD).unwrap();
    let big: Vec<u8> = (0..1_000_000u32).map(|i| i.to_le_bytes()[0]).collect();
    for payload in [&b""[..], b"x", b"hello, cache", &big] {
        let name = store.put_blob(payload).unwrap();
        assert_eq!(name, Hash128::of(payload));
        assert_eq!(store.get_blob(name).unwrap().as_deref(), Some(payload));
        let bytes = fs::read(entry(dir.path(), "blob", name)).unwrap();
        assert_eq!(bytes.len(), HEADER_LEN + payload.len());
        assert_eq!(&bytes[..8], &BLOB_MAGIC);
        assert_eq!(&bytes[8..24], &BUILD.0);
        assert_eq!(
            &bytes[24..32],
            &u64::try_from(payload.len()).unwrap().to_le_bytes()
        );
    }
    let key = Key::builder(KeyKind::Front, BUILD).str("app::a").finish();
    assert_eq!(store.get_record(&key).unwrap(), None);
    store.put_record(&key, b"record one").unwrap();
    assert_eq!(
        store.get_record(&key).unwrap().as_deref(),
        Some(&b"record one"[..])
    );
    store.put_record(&key, b"record two").unwrap();
    assert_eq!(
        store.get_record(&key).unwrap().as_deref(),
        Some(&b"record two"[..])
    );
    assert!(entry(dir.path(), "rec", key.hash()).is_file());
    // Hints live apart from records: the same key names both.
    assert_eq!(store.get_hint(&key).unwrap(), None);
    store.put_hint(&key, b"hint").unwrap();
    assert_eq!(store.get_hint(&key).unwrap().as_deref(), Some(&b"hint"[..]));
    assert_eq!(
        store.get_record(&key).unwrap().as_deref(),
        Some(&b"record two"[..])
    );
    assert_eq!(files_in(&dir.path().join("v1/tmp")), Vec::<PathBuf>::new());
}

/// A writer killed between writing its temporary file and renaming it
/// leaves `tmp/<pid>-<n>` behind; a later process with the same id must
/// still open the cache and write (D-960, D-959; review SOUND-W1-2).
#[test]
fn cache_store_stale_temporary_files_never_fail_a_write() {
    let dir = TestDir::new("stale-tmp");
    let root = dir.path().join("cache");
    drop(Store::open(&root, BUILD).unwrap());
    let tmp = root.join("v1/tmp");
    let first = super::store::next_tmp_number();
    let planted: Vec<PathBuf> = (first..first + 64)
        .map(|n| tmp.join(format!("{}-{n}", std::process::id())))
        .collect();
    for path in &planted {
        fs::write(path, b"a dead writer's half").unwrap();
    }
    // The README probe skips the planted names.
    let (store, fallback) =
        Store::open_or_temporary(&env(root.to_str(), None, None, None), BUILD).unwrap();
    assert!(fallback.is_none(), "{fallback:?}");
    assert!(!store.is_temporary());
    // And so does every write.
    let name = store.put_blob(b"after a crash").unwrap();
    assert_eq!(
        store.get_blob(name).unwrap().as_deref(),
        Some(&b"after a crash"[..])
    );
    let key = Key::builder(KeyKind::Front, BUILD).str("app").finish();
    store.put_record(&key, b"record").unwrap();
    store.put_hint(&key, b"hint").unwrap();
    // Leftovers are skipped, never removed: trimming deletes them.
    for path in &planted {
        assert_eq!(fs::read(path).unwrap(), b"a dead writer's half");
    }
    assert_eq!(files_in(&tmp).len(), planted.len());
}

#[test]
fn cache_store_truncated_file_is_a_miss_that_a_write_replaces() {
    let dir = TestDir::new("truncated");
    let store = Store::open(dir.path(), BUILD).unwrap();
    let name = store.put_blob(b"some blob bytes").unwrap();
    let key = Key::builder(KeyKind::Code, BUILD).str("m").finish();
    store.put_record(&key, b"payload").unwrap();
    for path in files_in(&dir.path().join("v1/blob"))
        .into_iter()
        .chain(files_in(&dir.path().join("v1/rec")))
    {
        let bytes = fs::read(&path).unwrap();
        fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
    }
    assert_eq!(store.get_blob(name).unwrap(), None);
    assert_eq!(store.get_record(&key).unwrap(), None);
    // Shorter than a header, and empty.
    let blob_path = files_in(&dir.path().join("v1/blob")).remove(0);
    fs::write(&blob_path, b"chbyblb1").unwrap();
    assert_eq!(store.get_blob(name).unwrap(), None);
    fs::write(&blob_path, b"").unwrap();
    assert_eq!(store.get_blob(name).unwrap(), None);
    // A longer file than the header says is bad too.
    store.put_record(&key, b"payload").unwrap();
    let rec_path = files_in(&dir.path().join("v1/rec")).remove(0);
    let mut bytes = fs::read(&rec_path).unwrap();
    bytes.push(0);
    fs::write(&rec_path, &bytes).unwrap();
    assert_eq!(store.get_record(&key).unwrap(), None);
    // The next write replaces the bad file.
    assert_eq!(store.put_blob(b"some blob bytes").unwrap(), name);
    store.put_record(&key, b"payload").unwrap();
    assert_eq!(
        store.get_blob(name).unwrap().as_deref(),
        Some(&b"some blob bytes"[..])
    );
    assert_eq!(
        store.get_record(&key).unwrap().as_deref(),
        Some(&b"payload"[..])
    );
}

#[test]
fn cache_store_wrong_magic_is_a_miss() {
    let dir = TestDir::new("magic");
    let store = Store::open(dir.path(), BUILD).unwrap();
    let key = Key::builder(KeyKind::Front, BUILD).str("m").finish();
    store.put_record(&key, b"payload").unwrap();
    let path = files_in(&dir.path().join("v1/rec")).remove(0);
    let mut bytes = fs::read(&path).unwrap();
    assert_eq!(&bytes[..8], &RECORD_MAGIC);
    // A blob's magic in a record's place.
    bytes[..8].copy_from_slice(&BLOB_MAGIC);
    fs::write(&path, &bytes).unwrap();
    assert_eq!(store.get_record(&key).unwrap(), None);
    // A record file copied to the hint of the same key.
    store.put_record(&key, b"payload").unwrap();
    let hint = entry(dir.path(), "hint", key.hash());
    fs::create_dir_all(hint.parent().unwrap()).unwrap();
    fs::copy(&path, &hint).unwrap();
    assert_eq!(store.get_hint(&key).unwrap(), None);
    assert!(store.get_record(&key).unwrap().is_some());
}

#[test]
fn cache_store_another_build_id_is_a_miss() {
    let dir = TestDir::new("build");
    let old = Store::open(dir.path(), BUILD).unwrap();
    let name = old.put_blob(b"shared bytes").unwrap();
    let key = Key::builder(KeyKind::Code, BUILD).str("m").finish();
    old.put_record(&key, b"payload").unwrap();
    old.put_hint(&key, b"hint").unwrap();
    let new = Store::open(dir.path(), OTHER_BUILD).unwrap();
    // Blob names do not hold the build ID, record keys do; either way the
    // header's build ID makes the file foreign.
    assert_eq!(new.get_blob(name).unwrap(), None);
    assert_eq!(new.get_record(&key).unwrap(), None);
    assert_eq!(new.get_hint(&key).unwrap(), None);
    assert_eq!(new.put_blob(b"shared bytes").unwrap(), name);
    assert_eq!(
        new.get_blob(name).unwrap().as_deref(),
        Some(&b"shared bytes"[..])
    );
    assert_eq!(old.get_blob(name).unwrap(), None);
}

#[test]
fn cache_store_renamed_blob_is_a_miss() {
    let dir = TestDir::new("renamed");
    let store = Store::open(dir.path(), BUILD).unwrap();
    let real = store.put_blob(b"the real bytes").unwrap();
    let other = Hash128::of(b"other bytes");
    let from = files_in(&dir.path().join("v1/blob")).remove(0);
    let to = entry(dir.path(), "blob", other);
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    fs::rename(&from, &to).unwrap();
    assert_eq!(store.get_blob(real).unwrap(), None);
    let got = store.get_blob(other).unwrap();
    if cfg!(debug_assertions) {
        // Debug toolchains check a blob's hash against its name (D-960).
        assert_eq!(got, None);
    } else {
        assert_eq!(got.as_deref(), Some(&b"the real bytes"[..]));
    }
}

#[test]
fn cache_store_read_touches_entries_older_than_an_hour() {
    let dir = TestDir::new("touch");
    let store = Store::open(dir.path(), BUILD).unwrap();
    let old_name = store.put_blob(b"old").unwrap();
    let new_name = store.put_blob(b"recent").unwrap();
    let path_of = |n: Hash128| entry(dir.path(), "blob", n);
    let now = SystemTime::now();
    let two_hours_ago = now - Duration::from_hours(2);
    let half_hour_ago = now - Duration::from_mins(30);
    set_mtime(&path_of(old_name), two_hours_ago);
    set_mtime(&path_of(new_name), half_hour_ago);
    assert!(store.get_blob(old_name).unwrap().is_some());
    assert!(store.get_blob(new_name).unwrap().is_some());
    assert!(mtime(&path_of(old_name)) >= now - Duration::from_mins(1));
    assert_eq!(mtime(&path_of(new_name)), half_hour_ago);
    // A miss touches nothing, and a record is touched like a blob.
    let key = Key::builder(KeyKind::Front, BUILD).str("m").finish();
    store.put_record(&key, b"r").unwrap();
    let rec = files_in(&dir.path().join("v1/rec")).remove(0);
    set_mtime(&rec, two_hours_ago);
    assert!(store.get_record(&key).unwrap().is_some());
    assert!(mtime(&rec) >= now - Duration::from_mins(1));
}

// --- Location -------------------------------------------------------------

fn env(
    cheby_cache: Option<&str>,
    xdg: Option<&str>,
    home: Option<&str>,
    local: Option<&str>,
) -> CacheEnv {
    CacheEnv {
        cheby_cache: cheby_cache.map(OsString::from),
        xdg_cache_home: xdg.map(OsString::from),
        home: home.map(OsString::from),
        local_app_data: local.map(OsString::from),
    }
}

/// An absolute path on every host: Windows needs a drive.
fn abs(unix: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!("C:{unix}"))
    } else {
        PathBuf::from(unix)
    }
}

#[test]
fn cache_location_cheby_cache_wins_and_must_be_absolute() {
    let cache = abs("/somewhere/cache");
    let e = env(cache.to_str(), Some("/xdg"), Some("/home/u"), Some("C:\\L"));
    for os in [Os::Linux, Os::Mac, Os::Windows] {
        assert_eq!(cache_root_on(&e, os).unwrap(), Some(cache.clone()));
    }
    let rel = env(Some("rel/cache"), None, Some("/home/u"), None);
    let err = cache_root_on(&rel, Os::Linux).unwrap_err();
    assert!(matches!(&err, StoreError::RelativeCacheVar(p) if p == Path::new("rel/cache")));
    assert!(err.to_string().contains("CHEBY_CACHE"), "{err}");
    // A relative CHEBY_CACHE never falls back.
    assert!(matches!(
        Store::open_or_temporary(&rel, BUILD),
        Err(StoreError::RelativeCacheVar(_))
    ));
}

#[test]
fn cache_location_os_defaults() {
    let home = abs("/home/u");
    let xdg = abs("/xdg");
    let h = home.to_str();
    let x = xdg.to_str();
    // Linux: XDG_CACHE_HOME when absolute, else ~/.cache; an empty
    // CHEBY_CACHE counts as unset.
    assert_eq!(
        cache_root_on(&env(Some(""), x, h, None), Os::Linux).unwrap(),
        Some(xdg.join("cheby"))
    );
    assert_eq!(
        cache_root_on(&env(None, Some("relative"), h, None), Os::Linux).unwrap(),
        Some(home.join(".cache").join("cheby"))
    );
    assert_eq!(
        cache_root_on(&env(None, Some(""), h, None), Os::Linux).unwrap(),
        Some(home.join(".cache").join("cheby"))
    );
    assert_eq!(
        cache_root_on(&env(None, None, None, None), Os::Linux).unwrap(),
        None
    );
    // macOS ignores XDG.
    assert_eq!(
        cache_root_on(&env(None, x, h, None), Os::Mac).unwrap(),
        Some(home.join("Library").join("Caches").join("cheby"))
    );
    assert_eq!(
        cache_root_on(&env(None, x, None, None), Os::Mac).unwrap(),
        None
    );
    // Windows: %LocalAppData%\cheby.
    let local = abs("/Users/u/AppData/Local");
    assert_eq!(
        cache_root_on(&env(None, x, h, local.to_str()), Os::Windows).unwrap(),
        Some(local.join("cheby"))
    );
    assert_eq!(
        cache_root_on(&env(None, x, h, None), Os::Windows).unwrap(),
        None
    );
}

#[test]
fn cache_location_unwritable_root_falls_back_to_a_temporary_store() {
    let dir = TestDir::new("fallback");
    // A root under a regular file cannot be created, even by root.
    let file = dir.path().join("a-file");
    fs::write(&file, b"not a directory").unwrap();
    let root = file.join("cache");
    let e = env(root.to_str(), None, None, None);
    let (store, fallback) = Store::open_or_temporary(&e, BUILD).unwrap();
    let fallback = fallback.expect("a fallback");
    assert_eq!(fallback.root.as_deref(), Some(root.as_path()));
    assert!(fallback.to_string().contains("cannot use"), "{fallback}");
    assert!(store.is_temporary());
    let temp = store.root().to_path_buf();
    assert!(temp.starts_with(std::env::temp_dir()));
    assert!(temp.join("v1/blob").is_dir());
    let name = store.put_blob(b"works").unwrap();
    assert!(store.get_blob(name).unwrap().is_some());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(&temp).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }
    drop(store);
    assert!(!temp.exists(), "a temporary store is deleted on drop");
}

#[test]
fn cache_location_no_home_falls_back_with_the_reason() {
    let (store, fallback) = Store::open_or_temporary(&env(None, None, None, None), BUILD).unwrap();
    let fallback = fallback.expect("a fallback");
    assert_eq!(fallback.root, None);
    assert!(
        fallback.reason.contains("CHEBY_CACHE"),
        "{}",
        fallback.reason
    );
    assert!(store.is_temporary());
}

#[test]
fn cache_location_writable_root_is_used_as_is() {
    let dir = TestDir::new("normal");
    let root = dir.path().join("deep/er/cache");
    let (store, fallback) =
        Store::open_or_temporary(&env(root.to_str(), None, None, None), BUILD).unwrap();
    assert!(fallback.is_none());
    assert!(!store.is_temporary());
    assert_eq!(store.root(), root);
    drop(store);
    assert!(
        root.join("README").is_file(),
        "a real cache survives its store"
    );
}

// --- Permissions (Unix) ---------------------------------------------------

#[cfg(unix)]
#[test]
fn cache_permissions_rules() {
    use super::location::root_problem;
    assert_eq!(root_problem(501, 0o40700, 501), None);
    assert_eq!(root_problem(501, 0o40755, 501), None);
    assert!(root_problem(501, 0o40775, 501).unwrap().contains("775"));
    assert!(root_problem(501, 0o40757, 501).is_some());
    assert!(
        root_problem(0, 0o40700, 501)
            .unwrap()
            .contains("owned by user 0")
    );
}

#[cfg(unix)]
#[test]
fn cache_permissions_new_root_is_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TestDir::new("perm-new");
    let root = dir.path().join("cache");
    let _store = Store::open(&root, BUILD).unwrap();
    for d in [root.clone(), root.join("v1"), root.join("v1/blob")] {
        let mode = fs::metadata(&d).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{}: {mode:o}", d.display());
    }
}

#[cfg(unix)]
#[test]
fn cache_permissions_writable_by_others_is_an_error_without_fallback() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TestDir::new("perm-open");
    let root = dir.path().join("cache");
    fs::create_dir(&root).unwrap();
    for mode in [0o770, 0o702] {
        fs::set_permissions(&root, fs::Permissions::from_mode(mode)).unwrap();
        let err = Store::open(&root, BUILD).unwrap_err();
        assert!(matches!(&err, StoreError::UnsafeRoot { root: r, .. } if *r == root));
        assert!(err.to_string().contains("unsafe"), "{err}");
        let e = env(root.to_str(), None, None, None);
        assert!(matches!(
            Store::open_or_temporary(&e, BUILD),
            Err(StoreError::UnsafeRoot { .. })
        ));
    }
    // Nothing was created inside the unsafe root.
    assert!(!root.join("v1").exists());
}

#[cfg(unix)]
#[test]
fn cache_permissions_root_of_another_user_is_an_error() {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        // Containers run tests as root, who owns `/`; the rule itself is
        // pinned by `cache_permissions_rules`.
        return;
    }
    let err = Store::open(Path::new("/"), BUILD).unwrap_err();
    assert!(
        matches!(&err, StoreError::UnsafeRoot { why, .. } if why.contains("owned by user")),
        "{err}"
    );
}

// --- Trimming -------------------------------------------------------------

#[test]
fn cache_trim_deletes_entries_unused_for_five_days() {
    let dir = TestDir::new("trim");
    let store = Store::open(dir.path(), BUILD).unwrap();
    let now = SystemTime::now();
    let six_days_ago = now - Duration::from_hours(6 * 24);
    let four_days_ago = now - Duration::from_hours(4 * 24);
    let old = store.put_blob(b"old").unwrap();
    let kept = store.put_blob(b"old but indexed").unwrap();
    let young = store.put_blob(b"young").unwrap();
    let old_key = Key::builder(KeyKind::Code, BUILD).str("old").finish();
    let young_key = Key::builder(KeyKind::Code, BUILD).str("young").finish();
    store.put_record(&old_key, b"r").unwrap();
    store.put_record(&young_key, b"r").unwrap();
    store.put_hint(&old_key, b"h").unwrap();
    let path_of = |dir_name: &str, n: Hash128| entry(dir.path(), dir_name, n);
    set_mtime(&path_of("blob", old), six_days_ago);
    set_mtime(&path_of("blob", kept), six_days_ago);
    set_mtime(&path_of("blob", young), four_days_ago);
    set_mtime(&path_of("rec", old_key.hash()), six_days_ago);
    set_mtime(&path_of("rec", young_key.hash()), four_days_ago);
    set_mtime(&path_of("hint", old_key.hash()), six_days_ago);
    let abandoned = dir.path().join("v1/tmp/999999-0");
    fs::write(&abandoned, b"half").unwrap();
    set_mtime(&abandoned, six_days_ago);
    let fresh_tmp = dir.path().join("v1/tmp/999999-1");
    fs::write(&fresh_tmp, b"being written").unwrap();

    let report = store.trim(now, &|n| n == kept).unwrap();
    assert_eq!(
        report,
        TrimReport {
            ran: true,
            blobs: 1,
            records: 1,
            hints: 1,
            temporary: 1,
            kept: 1,
        }
    );
    assert_eq!(store.get_blob(old).unwrap(), None);
    assert!(store.get_blob(kept).unwrap().is_some());
    assert!(store.get_blob(young).unwrap().is_some());
    assert_eq!(store.get_record(&old_key).unwrap(), None);
    assert!(store.get_record(&young_key).unwrap().is_some());
    assert_eq!(store.get_hint(&old_key).unwrap(), None);
    assert!(!abandoned.exists());
    assert!(fresh_tmp.exists());
    assert_eq!(mtime(&dir.path().join("v1/trimmed")), now);
}

#[test]
fn cache_trim_runs_at_most_once_a_day() {
    let dir = TestDir::new("trim-daily");
    let store = Store::open(dir.path(), BUILD).unwrap();
    let t0 = SystemTime::now() - Duration::from_hours(100);
    assert!(store.trim(t0, &|_| false).unwrap().ran);
    let name = store.put_blob(b"b").unwrap();
    let path = entry(dir.path(), "blob", name);
    set_mtime(&path, t0 - Duration::from_hours(10 * 24));
    // 23 hours later: no trim, the old blob stays.
    let report = store
        .trim(t0 + Duration::from_hours(23), &|_| false)
        .unwrap();
    assert_eq!(report, TrimReport::default());
    assert!(path.exists());
    // 25 hours later: a trim.
    let report = store
        .trim(t0 + Duration::from_hours(25), &|_| false)
        .unwrap();
    assert!(report.ran);
    assert_eq!(report.blobs, 1);
    assert!(!path.exists());
}

#[test]
fn cache_trim_keeps_what_reads_touched() {
    let dir = TestDir::new("trim-touch");
    let store = Store::open(dir.path(), BUILD).unwrap();
    let name = store.put_blob(b"used").unwrap();
    let path = entry(dir.path(), "blob", name);
    set_mtime(&path, SystemTime::now() - Duration::from_hours(6 * 24));
    assert!(store.get_blob(name).unwrap().is_some());
    let report = store.trim(SystemTime::now(), &|_| false).unwrap();
    assert!(report.ran);
    assert_eq!(report.blobs, 0);
    assert!(store.get_blob(name).unwrap().is_some());
}

// --- The log --------------------------------------------------------------

#[test]
fn cache_log_writes_sorted_lines_at_the_end() {
    let dir = TestDir::new("log");
    let file = dir.path().join("cache.log");
    let log = CacheLog::from_var(Some(file.clone().into_os_string()));
    assert!(log.is_enabled());
    std::thread::scope(|s| {
        for t in 0..4 {
            let log = &log;
            s.spawn(move || {
                for m in (0..5).rev() {
                    log.record(KeyKind::Code.tag(), &format!("app::m{m}"), (m + t) % 2 == 0);
                }
            });
        }
    });
    log.record("front", "app::main", true);
    log.record("const", "app::LIMIT", false);
    assert!(!file.exists(), "written only when the command ends");
    log.finish().unwrap();
    let text = fs::read_to_string(&file).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 22);
    let mut sorted = lines.clone();
    sorted.sort_unstable();
    assert_eq!(lines, sorted);
    assert_eq!(lines[0], "code app::m0 hit");
    assert!(lines.contains(&"const app::LIMIT miss"));
    assert!(lines.contains(&"front app::main hit"));
    assert!(text.ends_with('\n'));
}

#[test]
fn cache_log_without_a_file_records_nothing() {
    for log in [
        CacheLog::from_var(None),
        CacheLog::from_var(Some(OsString::new())),
    ] {
        assert!(!log.is_enabled());
        log.record("code", "app::m", true);
        assert_eq!(log.lines(), Vec::<String>::new());
        log.finish().unwrap();
    }
}

// --- Eight processes ------------------------------------------------------

/// Set on the children of [`cache_stress_eight_processes`]: the store's
/// root.
const STRESS_ROOT: &str = "CHEBY_K1_STRESS_ROOT";
const STRESS_ENTRIES: u64 = 200;
const STRESS_ROUNDS: u64 = 3;

fn stress_blob(i: u64) -> Vec<u8> {
    format!("blob {i:03}: ")
        .repeat(usize::try_from((i % 17).wrapping_add(1).wrapping_mul(100)).unwrap())
        .into_bytes()
}

fn stress_key(i: u64) -> Key {
    Key::builder(KeyKind::Code, BUILD)
        .str("stress")
        .u64(i)
        .finish()
}

fn stress_record(i: u64) -> Vec<u8> {
    let mut r = Hash128::of(&stress_blob(i)).0.to_vec();
    r.extend_from_slice(&i.to_le_bytes());
    r
}

/// The child's role: write and read every entry, several rounds.
#[test]
fn cache_stress_child() {
    let Some(root) = std::env::var_os(STRESS_ROOT) else {
        return;
    };
    let store = Store::open(Path::new(&root), BUILD).unwrap();
    for _ in 0..STRESS_ROUNDS {
        for i in 0..STRESS_ENTRIES {
            let name = store.put_blob(&stress_blob(i)).unwrap();
            assert_eq!(name, Hash128::of(&stress_blob(i)));
            store.put_record(&stress_key(i), &stress_record(i)).unwrap();
            store.put_hint(&stress_key(i), b"last writer wins").unwrap();
        }
        for i in 0..STRESS_ENTRIES {
            let name = Hash128::of(&stress_blob(i));
            assert_eq!(
                store.get_blob(name).unwrap(),
                Some(stress_blob(i)),
                "blob {i}"
            );
            assert_eq!(
                store.get_record(&stress_key(i)).unwrap(),
                Some(stress_record(i)),
                "record {i}"
            );
            assert!(
                store.get_hint(&stress_key(i)).unwrap().is_some(),
                "hint {i}"
            );
        }
    }
}

#[test]
fn cache_stress_eight_processes() {
    if std::env::var_os(STRESS_ROOT).is_some() {
        return;
    }
    let dir = TestDir::new("stress");
    let root = dir.path().join("cache");
    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..8)
        .map(|_| {
            std::process::Command::new(&exe)
                .args([
                    "--exact",
                    "cache::tests::cache_stress_child",
                    "--test-threads",
                    "1",
                ])
                .env(STRESS_ROOT, &root)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "child failed: {}\n{stdout}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("1 passed"),
            "the child ran its role:\n{stdout}"
        );
    }
    // Every file is valid, every read hits, and `tmp/` is empty.
    let store = Store::open(&root, BUILD).unwrap();
    let blobs = files_in(&root.join("v1/blob"));
    assert_eq!(blobs.len(), usize::try_from(STRESS_ENTRIES).unwrap());
    for path in &blobs {
        let name = Hash128::from_hex(path.file_name().unwrap().to_str().unwrap()).unwrap();
        let bytes = store.get_blob(name).unwrap().expect("a valid blob");
        assert_eq!(Hash128::of(&bytes), name);
    }
    assert_eq!(
        files_in(&root.join("v1/rec")).len(),
        usize::try_from(STRESS_ENTRIES).unwrap()
    );
    assert_eq!(
        files_in(&root.join("v1/hint")).len(),
        usize::try_from(STRESS_ENTRIES).unwrap()
    );
    for i in 0..STRESS_ENTRIES {
        assert_eq!(
            store.get_blob(Hash128::of(&stress_blob(i))).unwrap(),
            Some(stress_blob(i))
        );
        assert_eq!(
            store.get_record(&stress_key(i)).unwrap(),
            Some(stress_record(i))
        );
        assert_eq!(
            store.get_hint(&stress_key(i)).unwrap().as_deref(),
            Some(&b"last writer wins"[..])
        );
    }
    assert_eq!(files_in(&root.join("v1/tmp")), Vec::<PathBuf>::new());
}
