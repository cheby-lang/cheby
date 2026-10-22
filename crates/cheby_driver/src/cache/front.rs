//! Front records and their keys (D-955, D-956; `cache.md` §3.3).
//!
//! A package module's front identity is found in two steps:
//!
//! 1. Its front key `F1` ([`FrontKey`]): `KeyKind::Front` over the field
//!    `f1`, the package name, the checked targets, the module path, the
//!    SipHash-1-3 128 of its source (D-963) and, per import in source
//!    order, the resolved module path and its interface hash, or
//!    `unresolved`.
//! 2. The read-set hint under `F1` ([`reads_key`], `KeyKind::Reads`)
//!    names the front end's hidden reads `R` as sorted module paths
//!    ([`encode_hint`]). The front identity `F2` ([`front_identity`])
//!    extends `F1` with each read's path and interface hash, and the front
//!    record lies under `F2` ([`FrontRecord`]).
//!
//! The record holds the interface hash, the blob names of the interface
//! artifact (D-953) and of the module's structured diagnostics
//! ([`super::replay`]), its imports, its native externals and its hidden
//! reads. Nothing in a key, a record or a blob holds an absolute path, a
//! `ModuleId`, a `FileId` or an interning number (D-264): modules are
//! named by their paths, and spans by byte ranges in the module's own file.
//! Std modules get no record: the build ID fixes std until K6 (D-955).

use cheby_mir::{CType, ForeignSig};
use cheby_package::{Target, Targets};

use super::wire::{Reader, Writer};
use super::{BuildId, Hash128, Key, KeyKind};

/// The version of the record's and the hint's encodings.
const VERSION: u8 = 1;

/// One import of a module, as its front key and record hold it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportKey {
    /// The import resolved to the module at `path`, whose interface hash
    /// was `hash`.
    Module {
        /// The imported module's path.
        path: String,
        /// Its interface hash.
        hash: Hash128,
    },
    /// The import did not resolve (E0038, E0039).
    Unresolved,
}

/// What `F1` covers (D-955, `cache.md` §4.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontKey {
    /// The package's name.
    pub package: String,
    /// The checked targets: the manifest's, or the one `--target` selects.
    pub targets: Targets,
    /// The module's path.
    pub module: String,
    /// The SipHash-1-3 128 of the module's source bytes (D-963).
    pub source: Hash128,
    /// The imports in source order.
    pub imports: Vec<ImportKey>,
}

impl FrontKey {
    /// The key `F1`.
    #[must_use]
    pub fn key(&self, build: BuildId) -> Key {
        let mut b = Key::builder(KeyKind::Front, build)
            .str("f1")
            .str(&self.package)
            .bytes(&[
                u8::from(self.targets.contains(Target::Native)),
                u8::from(self.targets.contains(Target::Js)),
            ])
            .str(&self.module)
            .hash(self.source)
            .u64(u64::try_from(self.imports.len()).unwrap_or(u64::MAX));
        for import in &self.imports {
            b = match import {
                ImportKey::Module { path, hash } => b.bytes(&[1]).str(path).hash(*hash),
                ImportKey::Unresolved => b.bytes(&[0]),
            };
        }
        b.finish()
    }
}

/// The key of the read-set hint under `f1` (`KeyKind::Reads`).
#[must_use]
pub fn reads_key(f1: Key, build: BuildId) -> Key {
    Key::builder(KeyKind::Reads, build).hash(f1.hash()).finish()
}

/// The front identity `F2`: `F1` extended with each hidden read's path
/// and interface hash, in the order given (sorted by path).
#[must_use]
pub fn front_identity(f1: Key, build: BuildId, reads: &[(String, Hash128)]) -> Key {
    let mut b = Key::builder(KeyKind::Front, build)
        .str("f2")
        .hash(f1.hash())
        .u64(u64::try_from(reads.len()).unwrap_or(u64::MAX));
    for (path, hash) in reads {
        b = b.str(path).hash(*hash);
    }
    b.finish()
}

/// A read-set hint's payload: the sorted module paths.
#[must_use]
pub fn encode_hint(paths: &[String]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(VERSION);
    w.len(paths.len());
    for path in paths {
        w.str(path);
    }
    w.finish()
}

/// The paths of a hint, or `None` when it is malformed.
#[must_use]
pub fn decode_hint(bytes: &[u8]) -> Option<Vec<String>> {
    let mut r = Reader::new(bytes);
    if r.u8()? != VERSION {
        return None;
    }
    let n = r.len()?;
    let mut paths = Vec::new();
    for _ in 0..n {
        paths.push(r.str()?.to_owned());
    }
    r.is_done().then_some(paths)
}

/// An `@external(native, …)` binding of a module (E0054 runs over the
/// cached ones, D-965).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct External {
    /// The native symbol.
    pub symbol: String,
    /// The C signature.
    pub sig: ForeignSig,
    /// The symbol literal's byte range in the module's own file, as start
    /// and length.
    pub span: (u32, u32),
}

/// The record under `F2` (D-955, D-956).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontRecord {
    /// The module's interface hash.
    pub interface: Hash128,
    /// The interface artifact's blob (D-953).
    pub artifact: Hash128,
    /// The structured diagnostics' blob ([`super::replay`]).
    pub diagnostics: Hash128,
    /// The imports, as in `F1`.
    pub imports: Vec<ImportKey>,
    /// The native bindings, in item order.
    pub externals: Vec<External>,
    /// The front end's hidden reads, sorted paths, as in `F2`.
    pub reads: Vec<String>,
}

impl FrontRecord {
    /// The record's payload.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(VERSION);
        w.raw(&self.interface.0);
        w.raw(&self.artifact.0);
        w.raw(&self.diagnostics.0);
        w.len(self.imports.len());
        for import in &self.imports {
            match import {
                ImportKey::Module { path, hash } => {
                    w.u8(1);
                    w.str(path);
                    w.raw(&hash.0);
                }
                ImportKey::Unresolved => w.u8(0),
            }
        }
        w.len(self.externals.len());
        for e in &self.externals {
            w.str(&e.symbol);
            w.len(e.sig.params.len());
            for &p in &e.sig.params {
                w.u8(ctype_tag(p));
            }
            w.u8(ctype_tag(e.sig.ret));
            w.u32(e.span.0);
            w.u32(e.span.1);
        }
        w.len(self.reads.len());
        for path in &self.reads {
            w.str(path);
        }
        w.finish()
    }

    /// The record of a payload, or `None` when it is malformed.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader::new(bytes);
        if r.u8()? != VERSION {
            return None;
        }
        let interface = Hash128(r.array16()?);
        let artifact = Hash128(r.array16()?);
        let diagnostics = Hash128(r.array16()?);
        let mut imports = Vec::new();
        for _ in 0..r.len()? {
            imports.push(match r.u8()? {
                0 => ImportKey::Unresolved,
                1 => ImportKey::Module {
                    path: r.str()?.to_owned(),
                    hash: Hash128(r.array16()?),
                },
                _ => return None,
            });
        }
        let mut externals = Vec::new();
        for _ in 0..r.len()? {
            let symbol = r.str()?.to_owned();
            let mut params = Vec::new();
            for _ in 0..r.len()? {
                params.push(ctype(r.u8()?)?);
            }
            let ret = ctype(r.u8()?)?;
            let span = (r.u32()?, r.u32()?);
            externals.push(External {
                symbol,
                sig: ForeignSig { params, ret },
                span,
            });
        }
        let mut reads = Vec::new();
        for _ in 0..r.len()? {
            reads.push(r.str()?.to_owned());
        }
        r.is_done().then_some(Self {
            interface,
            artifact,
            diagnostics,
            imports,
            externals,
            reads,
        })
    }
}

/// A C type's byte in a record.
const fn ctype_tag(t: CType) -> u8 {
    match t {
        CType::I8 => 0,
        CType::I16 => 1,
        CType::I32 => 2,
        CType::I64 => 3,
        CType::U8 => 4,
        CType::U16 => 5,
        CType::U32 => 6,
        CType::U64 => 7,
        CType::F32 => 8,
        CType::F64 => 9,
        CType::Bool => 10,
        CType::Ptr => 11,
        CType::Void => 12,
    }
}

/// The C type of a record's byte.
const fn ctype(tag: u8) -> Option<CType> {
    Some(match tag {
        0 => CType::I8,
        1 => CType::I16,
        2 => CType::I32,
        3 => CType::I64,
        4 => CType::U8,
        5 => CType::U16,
        6 => CType::U32,
        7 => CType::U64,
        8 => CType::F32,
        9 => CType::F64,
        10 => CType::Bool,
        11 => CType::Ptr,
        12 => CType::Void,
        _ => return None,
    })
}
