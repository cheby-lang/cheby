//! The build cache's store (D-255, D-956 to D-961, D-973).
//!
//! The cache is a content-addressed store of immutable blobs and of small
//! records that map a key to blobs (D-956). This module holds the store
//! itself; which artifacts go into it, and under which keys, is the
//! driver's business from M3's wave 2 on.
//!
//! - [`Key`] is SipHash-1-3 128 with zero keys over a canonical,
//!   length-prefixed encoding of its fields, with the kind tag first
//!   ([`KeyKind`]) and the [`BuildId`] second. [`Settings`] holds exactly
//!   the settings of D-958.
//! - [`Store`] lays the cache out as `<root>/README` and
//!   `v1/{blob,rec,hint,tmp}/`. Blobs are named by the hash of their bytes
//!   under `blob/<first byte in hex>/<hex>`, records and hints by their
//!   key under `rec/` and `hint/`. Every file starts with a magic per kind,
//!   the build ID and the payload's length. Writers write into `tmp/` and
//!   rename into place; nothing locks or calls `fsync` (D-960). A reader
//!   checks the header, and in debug builds a blob's hash against its
//!   name, and reads a bad, short or foreign file as a miss.
//! - [`Store::open_or_temporary`] finds the root from [`CacheEnv`]
//!   (`CHEBY_CACHE` or the OS default, D-959), checks that only its owner
//!   can write it, and falls back to a fresh temporary store, with a
//!   [`Fallback`] naming why, when the root cannot be created or written.
//! - A read touches an entry older than an hour, and [`Store::trim`]
//!   deletes entries unused for five days, at most once a day (D-961).
//! - [`CacheLog`] is `CHEBY_CACHE_LOG` (D-973): every lookup as a sorted
//!   `<artifact> <module or constant path> hit|miss` line.
//! - [`front`] holds the front records with their reference and use
//!   summaries (D-965, D-967), their keys `F1` and `F2` and the read-set
//!   hints (D-955), [`replay`] the structured diagnostics they name and
//!   their replay (D-965), and [`index`] the package index of each package
//!   root and command family (D-962). The front end reads and writes them
//!   (`frontend.rs`).
//! - `code_keys` holds the code identities `C2` and `T2`, the keys of
//!   borrowing's facts and masks records and of code (D-955, D-969), and
//!   the masks record; `borrowing.rs` writes the records and logs the
//!   keys (K4b), and K4c stores code under them.
//!
//! Nothing here depends on an absolute path, a modification time or
//! hash-map order except the files' own locations (D-264): keys, blob
//! names and file contents are functions of the bytes callers pass.

pub(crate) mod code_keys;
pub mod front;
pub mod index;
mod key;
mod location;
mod log;
pub mod replay;
mod store;
mod trim;
mod wire;
mod writer;

#[cfg(test)]
mod store_tests;
#[cfg(test)]
mod tests;

use std::fmt;
use std::path::PathBuf;

use siphasher::sip128::{Hasher128 as _, SipHasher13};

pub use key::{BuildKind, Key, KeyBuilder, KeyKind, Settings};
pub use location::{CacheEnv, Fallback, cache_root};
pub use log::CacheLog;
pub use store::Store;
pub(crate) use store::StoreShare;
pub use trim::TrimReport;

/// The compiler's build ID (D-957), from `cheby_cli`'s build script: in
/// every key and every file header of the cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BuildId(pub [u8; 16]);

impl BuildId {
    /// The ID as 32 lowercase hex digits.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }
}

/// SipHash-1-3 128 with zero keys: a key or a blob's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash128(pub [u8; 16]);

impl Hash128 {
    /// The hash of `bytes`, which names a blob holding them.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        let mut h = SipHasher13::new_with_keys(0, 0);
        std::hash::Hasher::write(&mut h, bytes);
        Self(h.finish128().as_bytes())
    }

    /// The hash as 32 lowercase hex digits, as file names spell it.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    /// Parses 32 hex digits (either case), as [`Hash128::to_hex`] writes
    /// them; anything else is `None`.
    #[must_use]
    pub fn from_hex(s: &str) -> Option<Self> {
        let bytes = s.as_bytes();
        if bytes.len() != 32 {
            return None;
        }
        let mut out = [0u8; 16];
        for (slot, [hi, lo]) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
            *slot = nibble(*hi)?.wrapping_shl(4) | nibble(*lo)?;
        }
        Some(Self(out))
    }
}

/// The value of one hex digit.
fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => c.checked_sub(b'0'),
        b'a'..=b'f' => c.checked_sub(b'a').and_then(|v| v.checked_add(10)),
        b'A'..=b'F' => c.checked_sub(b'A').and_then(|v| v.checked_add(10)),
        _ => None,
    }
}

/// `bytes` as lowercase hex digits.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len().saturating_mul(2));
    for &b in bytes {
        for d in [b.wrapping_shr(4), b & 0xf] {
            if let Some(&c) = DIGITS.get(usize::from(d)) {
                s.push(char::from(c));
            }
        }
    }
    s
}

/// An error of the store that the command reports as a tool error. A bad
/// file is never one: it reads as a miss (D-960).
#[derive(Debug)]
pub enum StoreError {
    /// `CHEBY_CACHE` names a relative path (D-959).
    RelativeCacheVar(PathBuf),
    /// The root is writable by others or owned by another user (D-959).
    UnsafeRoot {
        /// The cache root.
        root: PathBuf,
        /// Why it is unsafe, as a phrase.
        why: String,
    },
    /// An operation on `path` failed.
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The error.
        error: std::io::Error,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RelativeCacheVar(path) => write!(
                f,
                "CHEBY_CACHE must be an absolute path, but it is `{}`",
                path.display()
            ),
            Self::UnsafeRoot { root, why } => {
                write!(f, "the build cache `{}` is unsafe: {why}", root.display())
            }
            Self::Io { path, error } => write!(f, "{}: {error}", path.display()),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { error, .. } => Some(error),
            Self::RelativeCacheVar(_) | Self::UnsafeRoot { .. } => None,
        }
    }
}

impl StoreError {
    /// An I/O error on `path`.
    pub(crate) fn io(path: impl Into<PathBuf>, error: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            error,
        }
    }
}
