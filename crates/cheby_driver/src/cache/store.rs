//! The store on disk (D-956, D-959, D-960, D-961).
//!
//! Layout under the root:
//!
//! ```text
//! <root>/README           what the directory is, and that deleting it is safe
//! <root>/v1/blob/ab/<hex> blobs, named by the hash of their payload
//! <root>/v1/rec/ab/<hex>  records, named by their key
//! <root>/v1/hint/ab/<hex> hints, named by their key
//! <root>/v1/tmp/          files being written
//! <root>/v1/trimmed       its modification time is the last trim
//! ```
//!
//! `ab` is the name's first byte in hex. Every file is a 32-byte header
//! and the payload: the kind's 8-byte magic ([`BLOB_MAGIC`],
//! [`RECORD_MAGIC`], [`HINT_MAGIC`]), the build ID's 16 bytes, and the
//! payload's length as a little-endian `u64`. A file whose header does
//! not match, whose length differs from the header's, or (in debug
//! builds) a blob whose payload does not hash to its name reads as a miss,
//! and the next write of the entry replaces it.
//!
//! A writer creates a new file in `tmp/` (named by the process id and a
//! counter, never by content; a name that exists, a killed writer's
//! leftover, is skipped), writes it whole and renames it into place.
//! Nothing locks and nothing calls `fsync` (D-960): concurrent writers of
//! one blob or record write the same bytes (D-264), and hints are
//! last-writer-wins.

use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use super::{BuildId, Hash128, Key, StoreError};

/// The magic of a blob file.
pub(crate) const BLOB_MAGIC: [u8; 8] = *b"chbyblb1";
/// The magic of a record file.
pub(crate) const RECORD_MAGIC: [u8; 8] = *b"chbyrec1";
/// The magic of a hint file.
pub(crate) const HINT_MAGIC: [u8; 8] = *b"chbyhnt1";
/// The header's length: magic, build ID, payload length.
pub(crate) const HEADER_LEN: usize = 32;

/// The layout's directory; a new layout uses a new one (D-956).
pub(crate) const LAYOUT_DIR: &str = "v1";
/// Blobs.
pub(crate) const BLOB_DIR: &str = "blob";
/// Records.
pub(crate) const RECORD_DIR: &str = "rec";
/// Hints.
pub(crate) const HINT_DIR: &str = "hint";
/// Files being written.
pub(crate) const TMP_DIR: &str = "tmp";

/// A read touches an entry older than this (D-961).
const TOUCH_AFTER: Duration = Duration::from_hours(1);

/// The README at the root.
const README: &str = "\
This directory is the build cache of the Cheby toolchain (`cheby`).

It holds compiled artifacts addressed by the hash of their inputs. Deleting
it is safe: commands rebuild what they need. `CHEBY_CACHE` moves it.
";

/// Numbers the temporary files of this process.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How many names in `tmp/` a writer tries before it gives up.
const TMP_ATTEMPTS: u32 = 256;

/// The three kinds of file, each with its directory and magic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileKind {
    /// `blob/`.
    Blob,
    /// `rec/`.
    Record,
    /// `hint/`.
    Hint,
}

impl FileKind {
    /// The directory under `v1/`.
    pub(crate) const fn dir(self) -> &'static str {
        match self {
            Self::Blob => BLOB_DIR,
            Self::Record => RECORD_DIR,
            Self::Hint => HINT_DIR,
        }
    }

    /// The magic that starts the file.
    pub(crate) const fn magic(self) -> [u8; 8] {
        match self {
            Self::Blob => BLOB_MAGIC,
            Self::Record => RECORD_MAGIC,
            Self::Hint => HINT_MAGIC,
        }
    }
}

/// An open cache: its root, its build ID, and whether it is a temporary
/// one that is deleted when the store is dropped (D-959).
#[derive(Debug)]
pub struct Store {
    root: PathBuf,
    v1: PathBuf,
    build: BuildId,
    temporary: bool,
}

impl Store {
    /// Opens the cache at `root`, creating it owner-only with its README
    /// and `v1/{blob,rec,hint,tmp}/` when they are missing.
    ///
    /// # Errors
    ///
    /// [`StoreError::UnsafeRoot`] on Unix when group or others can write
    /// the root or another user owns it (D-959); [`StoreError::Io`] when
    /// the root or its directories cannot be created or `tmp/` cannot be
    /// written.
    pub fn open(root: &Path, build: BuildId) -> Result<Self, StoreError> {
        Self::open_with(root, build, false)
    }

    /// [`Store::open`], marking the store temporary when `temporary`.
    pub(crate) fn open_with(
        root: &Path,
        build: BuildId,
        temporary: bool,
    ) -> Result<Self, StoreError> {
        create_private_dir_all(root)?;
        super::location::check_root(root)?;
        let v1 = root.join(LAYOUT_DIR);
        for dir in [BLOB_DIR, RECORD_DIR, HINT_DIR, TMP_DIR] {
            create_private_dir_all(&v1.join(dir))?;
        }
        let store = Self {
            root: root.to_path_buf(),
            v1,
            build,
            temporary,
        };
        let readme = root.join("README");
        if readme.is_file() {
            // A write into `tmp/` shows that the cache can be written; a
            // read-only cache falls back to a temporary one (D-959).
            let (probe, file) = store.create_tmp()?;
            drop(file);
            fs::remove_file(&probe).map_err(|e| StoreError::io(&probe, e))?;
        } else {
            store.write_atomically(&readme, &[README.as_bytes()])?;
        }
        Ok(store)
    }

    /// The cache's root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The build ID every file of this store carries.
    #[must_use]
    pub const fn build_id(&self) -> BuildId {
        self.build
    }

    /// Whether this is a temporary store, deleted when it is dropped.
    #[must_use]
    pub const fn is_temporary(&self) -> bool {
        self.temporary
    }

    /// The `v1/` directory.
    pub(crate) fn layout_dir(&self) -> &Path {
        &self.v1
    }

    /// Stores `bytes` as a blob and returns its name, the hash of `bytes`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] when the file cannot be written or renamed.
    pub fn put_blob(&self, bytes: &[u8]) -> Result<Hash128, StoreError> {
        let name = Hash128::of(bytes);
        self.put(FileKind::Blob, name, bytes)?;
        Ok(name)
    }

    /// The blob named `name`, or `None` when it is missing or bad.
    ///
    /// # Errors
    ///
    /// None today: every failure to read is a miss (D-960). The `Result`
    /// leaves room for errors the user must see.
    pub fn get_blob(&self, name: Hash128) -> Result<Option<Vec<u8>>, StoreError> {
        let payload = self.get(FileKind::Blob, name);
        Ok(payload.filter(|bytes| !cfg!(debug_assertions) || Hash128::of(bytes) == name))
    }

    /// Stores `payload` as the record of `key`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] when the file cannot be written or renamed.
    pub fn put_record(&self, key: &Key, payload: &[u8]) -> Result<(), StoreError> {
        self.put(FileKind::Record, key.hash(), payload)
    }

    /// The record of `key`, or `None` when it is missing or bad.
    ///
    /// # Errors
    ///
    /// None today, as [`Store::get_blob`].
    pub fn get_record(&self, key: &Key) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.get(FileKind::Record, key.hash()))
    }

    /// Stores `payload` as the hint of `key`; the last writer wins.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] when the file cannot be written or renamed.
    pub fn put_hint(&self, key: &Key, payload: &[u8]) -> Result<(), StoreError> {
        self.put(FileKind::Hint, key.hash(), payload)
    }

    /// The hint of `key`, or `None` when it is missing or bad.
    ///
    /// # Errors
    ///
    /// None today, as [`Store::get_blob`].
    pub fn get_hint(&self, key: &Key) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.get(FileKind::Hint, key.hash()))
    }

    /// The path of the entry `name` of `kind`.
    pub(crate) fn entry_path(&self, kind: FileKind, name: Hash128) -> PathBuf {
        let hex = name.to_hex();
        let shard = hex.get(..2).unwrap_or("00");
        self.v1.join(kind.dir()).join(shard).join(&hex)
    }

    /// Writes an entry: header and payload into `tmp/`, then a rename.
    fn put(&self, kind: FileKind, name: Hash128, payload: &[u8]) -> Result<(), StoreError> {
        let path = self.entry_path(kind, name);
        if let Some(shard) = path.parent() {
            create_private_dir(shard)?;
        }
        let header = header(kind, self.build, payload.len());
        self.write_atomically(&path, &[&header, payload])
    }

    /// Reads an entry, checking its header; touches it when it is older
    /// than an hour (D-961).
    fn get(&self, kind: FileKind, name: Hash128) -> Option<Vec<u8>> {
        let path = self.entry_path(kind, name);
        let mut bytes = fs::read(&path).ok()?;
        if !header_ok(&bytes, kind, self.build) {
            return None;
        }
        bytes.drain(..HEADER_LEN);
        touch_if_old(&path, SystemTime::now());
        Some(bytes)
    }

    /// A fresh path in `tmp/`: the process id and a counter.
    fn new_tmp_path(&self) -> PathBuf {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        self.v1
            .join(TMP_DIR)
            .join(format!("{}-{n}", std::process::id()))
    }

    /// Creates a new file in `tmp/` under a fresh name. A name that exists
    /// is skipped for the next counter value, never removed: it is a
    /// killed writer's leftover from an earlier process with the same id,
    /// which trimming deletes later, or a live writer's in another pid
    /// namespace that shares the cache (D-960: a stale file never makes a
    /// write fail).
    fn create_tmp(&self) -> Result<(PathBuf, File), StoreError> {
        let mut last = None;
        for _ in 0..TMP_ATTEMPTS {
            let tmp = self.new_tmp_path();
            match File::options().write(true).create_new(true).open(&tmp) {
                Ok(file) => return Ok((tmp, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    last = Some(StoreError::io(tmp, error));
                }
                Err(error) => return Err(StoreError::io(tmp, error)),
            }
        }
        Err(last.unwrap_or_else(|| {
            let dir = self.v1.join(TMP_DIR);
            StoreError::io(dir, io::ErrorKind::AlreadyExists.into())
        }))
    }

    /// Writes `parts` into a new file in `tmp/` and renames it to `path`.
    /// When the rename fails but `path` exists, another writer won, which
    /// is as good: it wrote the same bytes, or it is a hint (D-960).
    fn write_atomically(&self, path: &Path, parts: &[&[u8]]) -> Result<(), StoreError> {
        let (tmp, file) = self.create_tmp()?;
        let written = write_all(file, parts);
        if let Err(error) = written {
            let _ = fs::remove_file(&tmp);
            return Err(StoreError::io(tmp, error));
        }
        match fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&tmp);
                if path.is_file() {
                    Ok(())
                } else {
                    Err(StoreError::io(path, error))
                }
            }
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        if self.temporary {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

/// The header of an entry of `kind` with a payload of `len` bytes.
pub(crate) fn header(kind: FileKind, build: BuildId, len: usize) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    let len = u64::try_from(len).unwrap_or(u64::MAX).to_le_bytes();
    for (slot, b) in h
        .iter_mut()
        .zip(kind.magic().iter().chain(&build.0).chain(&len))
    {
        *slot = *b;
    }
    h
}

/// Whether `bytes` is a whole entry of `kind` under `build`.
pub(crate) fn header_ok(bytes: &[u8], kind: FileKind, build: BuildId) -> bool {
    let Some((head, payload)) = bytes.split_at_checked(HEADER_LEN) else {
        return false;
    };
    let Some((magic, rest)) = head.split_at_checked(8) else {
        return false;
    };
    let Some((id, len)) = rest.split_at_checked(16) else {
        return false;
    };
    let Ok(len) = <[u8; 8]>::try_from(len) else {
        return false;
    };
    magic == kind.magic()
        && id == build.0
        && u64::try_from(payload.len()).is_ok_and(|n| n == u64::from_le_bytes(len))
}

/// Writes `parts` into the new file `f`.
fn write_all(mut f: File, parts: &[&[u8]]) -> io::Result<()> {
    for part in parts {
        f.write_all(part)?;
    }
    Ok(())
}

/// The counter value the next temporary file takes (for tests that plant
/// a dead writer's leftovers).
#[cfg(test)]
pub(crate) fn next_tmp_number() -> u64 {
    TMP_COUNTER.load(Ordering::Relaxed)
}

/// Sets `path`'s modification time to `now` when it is older than an
/// hour. Failures are ignored: touching only keeps trimming away.
pub(crate) fn touch_if_old(path: &Path, now: SystemTime) {
    let Ok(modified) = fs::metadata(path).and_then(|m| m.modified()) else {
        return;
    };
    if now
        .duration_since(modified)
        .is_ok_and(|age| age > TOUCH_AFTER)
    {
        // Setting the time needs write access on Windows; the bytes are
        // left alone.
        if let Ok(f) = File::options().write(true).open(path) {
            let _ = f.set_modified(now);
        }
    }
}

/// Creates `dir` and its missing parents, owner-only on Unix.
pub(crate) fn create_private_dir_all(dir: &Path) -> Result<(), StoreError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    set_private_mode(&mut builder);
    builder.create(dir).map_err(|e| StoreError::io(dir, e))
}

/// Creates `dir` owner-only on Unix; an existing directory is fine.
fn create_private_dir(dir: &Path) -> Result<(), StoreError> {
    let mut builder = fs::DirBuilder::new();
    set_private_mode(&mut builder);
    match builder.create(dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(StoreError::io(dir, e)),
        _ => Ok(()),
    }
}

/// Makes `builder` create directories that only their owner can use.
#[cfg(unix)]
fn set_private_mode(builder: &mut fs::DirBuilder) {
    use std::os::unix::fs::DirBuilderExt as _;
    builder.mode(0o700);
}

/// On Windows a new directory inherits its parent's access list, which is
/// the user's own under `%LocalAppData%` (GAP(cache-permissions-windows)).
#[cfg(not(unix))]
fn set_private_mode(_builder: &mut fs::DirBuilder) {}
