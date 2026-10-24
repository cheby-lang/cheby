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
//!    record lies under `F2` ([`FrontRecord`]). No hint is written for
//!    empty reads, and a missing hint reads as the empty set: a lost
//!    non-empty hint only causes a miss (D-1137).
//!
//! The record holds the interface hash, the blob names of the interface
//! artifact (D-953) and of the module's structured diagnostics
//! ([`super::replay`]), its imports, its native externals, its hidden
//! reads, and its summaries: what each function, constant, type's hooks
//! and test mentions ([`ReferenceSummary`], D-967) and the package items
//! the module uses with its own W0005 candidates ([`UseSummary`], D-516,
//! D-965). A record ends with the SipHash-1-3 128 of its bytes, so a
//! damaged one is a miss (D-960). Nothing in a key, a record or a blob
//! holds an absolute path, a
//! `ModuleId`, a `FileId` or an interning number (D-264): modules are
//! named by their paths, and spans by byte ranges in the module's own file.
//! Std modules get no record: the build ID fixes std until K6 (D-955).

use cheby_mir::{CType, ForeignSig};
use cheby_package::{Target, Targets};

use super::wire::{Reader, Writer};
use super::{BuildId, Hash128, Key, KeyKind};

/// The version of the record's and the hint's encodings: 2 since records
/// hold summaries (D-965, D-967).
const VERSION: u8 = 2;

/// An item named by its module's path and its name, as summaries name
/// items (D-967): a top-level item by its name, a constructor or an
/// interface function as `Parent::name`, a test by its string.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemRef {
    /// The module's path, such as `app::shapes`.
    pub module: String,
    /// The item's name within the module.
    pub item: String,
}

/// What kind of item a [`ReferenceSummary`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MentionerKind {
    /// A function.
    Function,
    /// A constant.
    Constant,
    /// A type's hooks (D-342): `item` is the type, and it mentions the
    /// functions its `@std_hooks` names.
    Hook,
    /// A test.
    Test,
}

impl MentionerKind {
    const fn tag(self) -> u8 {
        match self {
            Self::Function => 0,
            Self::Constant => 1,
            Self::Hook => 2,
            Self::Test => 3,
        }
    }

    const fn of_tag(tag: u8) -> Option<Self> {
        Some(match tag {
            0 => Self::Function,
            1 => Self::Constant,
            2 => Self::Hook,
            3 => Self::Test,
            _ => return None,
        })
    }
}

/// What one function, constant, hook or test mentions: functions,
/// constants, dictionary functions and constructors, sorted and without
/// repeats (D-967).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReferenceSummary {
    /// The mentioning item's name in its module.
    pub item: String,
    /// What it is.
    pub kind: MentionerKind,
    /// What it mentions.
    pub mentions: Vec<ItemRef>,
}

/// A package-visible item of the module that W0005 may report (D-516): its
/// name, the word for its kind, and its name's byte range in the module's
/// own file.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnusedCandidate {
    /// The item's name.
    pub item: String,
    /// The word for its kind, such as `function`.
    pub kind: String,
    /// Its name's span in the module's own file, as start and length.
    pub span: (u32, u32),
}

/// The package-visible items of the package that the module uses outside
/// their own declarations, tests included, and the module's own
/// package-visible items that W0005 may report (D-516, D-965): what the
/// whole-package pass reads of a module it did not check.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct UseSummary {
    /// The items used, sorted, without repeats.
    pub uses: Vec<ItemRef>,
    /// The module's candidates, in item order.
    pub candidates: Vec<UnusedCandidate>,
}

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
    /// The module's reference summary, sorted by item and kind (D-967).
    pub references: Vec<ReferenceSummary>,
    /// The module's use summary (D-516, D-965).
    pub uses: UseSummary,
}

impl FrontRecord {
    /// The record's payload.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(VERSION);
        self.encode_into(&mut w);
        sealed(w.finish())
    }

    /// The record's fields, without the version (the package index holds
    /// records inline).
    pub(crate) fn encode_into(&self, w: &mut Writer) {
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
        w.len(self.references.len());
        for summary in &self.references {
            w.str(&summary.item);
            w.u8(summary.kind.tag());
            write_items(w, &summary.mentions);
        }
        write_items(w, &self.uses.uses);
        w.len(self.uses.candidates.len());
        for c in &self.uses.candidates {
            w.str(&c.item);
            w.str(&c.kind);
            w.u32(c.span.0);
            w.u32(c.span.1);
        }
    }

    /// The record of a payload, or `None` when it is malformed.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader::new(unsealed(bytes)?);
        if r.u8()? != VERSION {
            return None;
        }
        let record = Self::decode_from(&mut r)?;
        r.is_done().then_some(record)
    }

    /// The record's fields, as [`FrontRecord::encode_into`] wrote them.
    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Option<Self> {
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
        let mut references = Vec::new();
        for _ in 0..r.len()? {
            let item = r.str()?.to_owned();
            let kind = MentionerKind::of_tag(r.u8()?)?;
            let mentions = read_items(r)?;
            references.push(ReferenceSummary {
                item,
                kind,
                mentions,
            });
        }
        let uses = read_items(r)?;
        let mut candidates = Vec::new();
        for _ in 0..r.len()? {
            candidates.push(UnusedCandidate {
                item: r.str()?.to_owned(),
                kind: r.str()?.to_owned(),
                span: (r.u32()?, r.u32()?),
            });
        }
        Some(Self {
            interface,
            artifact,
            diagnostics,
            imports,
            externals,
            reads,
            references,
            uses: UseSummary { uses, candidates },
        })
    }

    /// Whether the module holds a constant: its summary has one (D-534).
    #[must_use]
    pub fn holds_constant(&self) -> bool {
        self.references
            .iter()
            .any(|s| s.kind == MentionerKind::Constant)
    }
}

/// `payload` followed by its SipHash-1-3 128: a record or an index that a
/// damaged byte would still let decode reads as a miss (D-960), since
/// unlike a blob it is not named by its hash.
pub(crate) fn sealed(mut payload: Vec<u8>) -> Vec<u8> {
    let seal = Hash128::of(&payload);
    payload.extend_from_slice(&seal.0);
    payload
}

/// The payload [`sealed`] sealed, or `None` when the seal does not match.
pub(crate) fn unsealed(bytes: &[u8]) -> Option<&[u8]> {
    let (payload, seal) = bytes.split_at_checked(bytes.len().checked_sub(16)?)?;
    (Hash128::of(payload).0 == *seal).then_some(payload)
}

/// Writes a list of items as module and name pairs.
fn write_items(w: &mut Writer, items: &[ItemRef]) {
    w.len(items.len());
    for item in items {
        w.str(&item.module);
        w.str(&item.item);
    }
}

/// Reads what [`write_items`] wrote.
fn read_items(r: &mut Reader<'_>) -> Option<Vec<ItemRef>> {
    let mut out = Vec::new();
    for _ in 0..r.len()? {
        out.push(ItemRef {
            module: r.str()?.to_owned(),
            item: r.str()?.to_owned(),
        });
    }
    Some(out)
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
