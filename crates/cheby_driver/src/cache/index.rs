//! The package index (D-962; D-1133).
//!
//! Each command keeps, per package root and command family
//! ([`CommandFamily`]), one index under `v1/hint/`, keyed by
//! `KeyKind::Index` over the root's absolute path and the family. It holds,
//! per package module with a front record, the module's front-key inputs
//! (its source hash, its imports as written and, inside the record, as
//! resolved), its `F1` and `F2` keys, and its front record inline (with its
//! hidden reads). A warm command reads it first: a module whose source
//! hash matches is not parsed until something needs its tree (its imports
//! come from here), and a module whose `F1` and `F2` match takes its
//! record from here, so no record or hint file is opened for it.
//!
//! The index is a hint, not an artifact (D-962, D-972): last writer wins,
//! it may hold paths, it never reaches output, `check-determinism` leaves
//! it out, and one that is missing, stale or malformed is ignored. A
//! command writes its index only when the bytes differ from the index it
//! read, so a warm command with no edit writes none. Trimming keeps the
//! blobs that a recently used index names (D-961).

use std::path::Path;

use super::front::FrontRecord;
use super::wire::{Reader, Writer};
use super::{BuildId, Hash128, Key, KeyKind};

/// The first bytes of an index's payload, which tell an index from a
/// read-set hint in `v1/hint/` (both are hints, D-956).
const MAGIC: &[u8; 4] = b"CBIX";
/// The version of the index's encoding.
const VERSION: u8 = 1;

/// The command families that keep separate indexes (D-962).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommandFamily {
    /// `cheby check`.
    Check,
    /// `cheby run`.
    Run,
    /// `cheby test`.
    Test,
    /// `cheby build`.
    Build,
}

impl CommandFamily {
    /// The family's name in the index's key.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Run => "run",
            Self::Test => "test",
            Self::Build => "build",
        }
    }
}

/// One module of a [`PackageIndex`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    /// The module's path.
    pub path: String,
    /// The SipHash-1-3 128 of its source (D-963).
    pub source: Hash128,
    /// Its imports as written, in source order, each as its segments
    /// joined by `::`.
    pub written: Vec<String>,
    /// Its front key `F1` (D-955).
    pub f1: Hash128,
    /// Its front identity `F2`, the key of `record`.
    pub f2: Hash128,
    /// Its front record.
    pub record: FrontRecord,
}

/// A package's index (D-962): per module its front-key inputs, its hidden
/// reads, its `F2` and its front record inline, sorted by module path. A
/// hint: last writer wins, never in output (D-1133).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageIndex {
    /// The modules, sorted by path, without repeats.
    pub modules: Vec<IndexEntry>,
}

impl PackageIndex {
    /// The key of the index of the package at `root`, an absolute path, for
    /// `family` (D-962).
    #[must_use]
    pub fn key(root: &Path, family: CommandFamily, build: BuildId) -> Key {
        Key::builder(KeyKind::Index, build)
            .bytes(root.as_os_str().as_encoded_bytes())
            .str(family.tag())
            .finish()
    }

    /// The entry of the module at `path`.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&IndexEntry> {
        self.modules
            .binary_search_by(|e| e.path.as_str().cmp(path))
            .ok()
            .and_then(|at| self.modules.get(at))
    }

    /// The index's payload.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.raw(MAGIC);
        w.u8(VERSION);
        w.len(self.modules.len());
        for e in &self.modules {
            w.str(&e.path);
            w.raw(&e.source.0);
            w.len(e.written.len());
            for path in &e.written {
                w.str(path);
            }
            w.raw(&e.f1.0);
            w.raw(&e.f2.0);
            e.record.encode_into(&mut w);
        }
        super::front::sealed(w.finish())
    }

    /// The index of a payload, or `None` when it is malformed, of another
    /// version, or not sorted by path: a bad index is ignored, never an
    /// error. Never panics.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader::new(super::front::unsealed(bytes)?);
        if r.take(MAGIC.len())? != MAGIC || r.u8()? != VERSION {
            return None;
        }
        let mut modules: Vec<IndexEntry> = Vec::new();
        for _ in 0..r.len()? {
            let path = r.str()?.to_owned();
            let source = Hash128(r.array16()?);
            let mut written = Vec::new();
            for _ in 0..r.len()? {
                written.push(r.str()?.to_owned());
            }
            let f1 = Hash128(r.array16()?);
            let f2 = Hash128(r.array16()?);
            let record = FrontRecord::decode_from(&mut r)?;
            if modules.last().is_some_and(|last| last.path >= path) {
                return None;
            }
            modules.push(IndexEntry {
                path,
                source,
                written,
                f1,
                f2,
                record,
            });
        }
        r.is_done().then_some(Self { modules })
    }

    /// Whether `bytes` is an index's payload rather than a read-set
    /// hint's, judged by its first bytes.
    #[must_use]
    pub fn is_index(bytes: &[u8]) -> bool {
        bytes.starts_with(MAGIC)
    }

    /// The blobs the index names: each record's interface artifact and
    /// diagnostics, which trimming keeps (D-961).
    pub fn blobs(&self) -> impl Iterator<Item = Hash128> + '_ {
        self.modules
            .iter()
            .flat_map(|e| [e.record.artifact, e.record.diagnostics])
    }
}
