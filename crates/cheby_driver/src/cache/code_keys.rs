//! The keys of borrowing's records and of code (D-955, D-958, D-969,
//! D-1109; `cache.md` §4.2 and §5.3), and the masks record.
//!
//! - A package module's code identity `C2` (its code unit) or `T2` (its
//!   tests unit), [`code_identity`]: its front identity `F2` extended with
//!   the module path and interface hash of each hidden read of that unit's
//!   lowering and borrow inference (D-955). The read-set hint under `F2`
//!   names those reads ([`lreads_key`], `KeyKind::LReads`), as the front
//!   end's hint does for `F2`.
//! - The facts record of a unit lies under [`facts_key`] (its identity):
//!   the unit's [`cheby_mir::borrow::UnitFacts`], sealed.
//! - The masks record of a module and a build kind lies under
//!   [`masks_key`]: `C2`, the kind, and the digest of the build's facts
//!   about the module's own functions
//!   ([`cheby_mir::borrow::BuildFacts::about_module`]). It holds the masks
//!   of the module's functions that borrow something, by item
//!   ([`MasksRecord`]).
//! - A unit's code key ([`code_key`], GAP(code-key-contents)): the build
//!   ID, the settings of D-958 with the kind, whether positions are on
//!   (`CHEBY_POSITIONS`, D-1173), the unit's identity, the module's own
//!   masks by item, and each callee of the package in another module by
//!   its slot symbol, which carries its mask (D-1109). A tests unit's key
//!   (`KeyKind::Tests`) names every callee of the package by its slot
//!   symbol, its own module's included, since a test may call the
//!   module's private functions.
//!
//! Every field is length-prefixed in a fixed order, lists sorted by name:
//! no path, address, `DefId` or hash-map order enters a key or a record
//! (D-264, D-972).

use cheby_mir::borrow::BorrowMask;

use super::front::{sealed, unsealed};
use super::wire::{Reader, Writer};
use super::{BuildId, BuildKind, Hash128, Key, KeyKind, Settings};

/// Which unit of a module a key is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum UnitPart {
    /// The code unit: `C2`, `KeyKind::Code`.
    Code,
    /// The tests unit: `T2`, `KeyKind::Tests`.
    Tests,
}

impl UnitPart {
    /// The part's name in keys and in the lookup log.
    pub(crate) const fn tag(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Tests => "tests",
        }
    }
}

/// The key of the lowering read-set hint of `part` under the front
/// identity `f2` (D-955): the hint names the unit's hidden reads, sorted
/// module paths, in the front end's hint format. GAP(lowering-read-hints):
/// it is written only when the reads are not empty, as D-1137 does for the
/// front end's hint; a hint left non-empty after the reads became empty
/// is not rewritten here, since nothing reads it until K4c, whose lookup
/// reads it and rewrites it then.
pub(crate) fn lreads_key(f2: Hash128, part: UnitPart, build: BuildId) -> Key {
    Key::builder(KeyKind::LReads, build)
        .hash(f2)
        .str(part.tag())
        .finish()
}

/// The code identity of `part` (`C2` or `T2`): the front identity `f2`
/// extended with each hidden read's module path and interface hash, in
/// the order given (sorted by path).
pub(crate) fn code_identity(
    f2: Hash128,
    part: UnitPart,
    build: BuildId,
    reads: &[(String, Hash128)],
) -> Key {
    let tag = match part {
        UnitPart::Code => "c2",
        UnitPart::Tests => "t2",
    };
    let mut b = Key::builder(KeyKind::Code, build)
        .str(tag)
        .hash(f2)
        .u64(u64::try_from(reads.len()).unwrap_or(u64::MAX));
    for (path, hash) in reads {
        b = b.str(path).hash(*hash);
    }
    b.finish()
}

/// The key of a unit's facts record: its code identity (D-969). Lowered
/// MIR before borrowing depends on no kind or setting.
pub(crate) fn facts_key(identity: Key, build: BuildId) -> Key {
    Key::builder(KeyKind::Facts, build)
        .hash(identity.hash())
        .finish()
}

/// The key of a module's masks record for `kind` (D-969): its code
/// identity, the kind, and `digest`, the hash of the build's facts about
/// the module's own functions.
pub(crate) fn masks_key(identity: Key, kind: BuildKind, digest: Hash128, build: BuildId) -> Key {
    Key::builder(KeyKind::Masks, build)
        .hash(identity.hash())
        .str(kind.tag())
        .hash(digest)
        .finish()
}

/// What a unit's code key covers (D-969, D-958, D-1109, D-1173).
#[derive(Clone, Debug)]
pub(crate) struct CodeKeyInputs<'a> {
    /// The compiler's build ID.
    pub build: BuildId,
    /// The code unit or the tests unit.
    pub part: UnitPart,
    /// The settings that change code, the build kind among them (D-958).
    pub settings: &'a Settings,
    /// Whether source positions are on (`CHEBY_POSITIONS`, D-1173): code
    /// records with and without positions differ.
    pub positions: bool,
    /// The unit's code identity ([`code_identity`]).
    pub identity: Key,
    /// The masks of the module's own functions that borrow something, by
    /// item, sorted (empty for a tests unit, whose functions borrow
    /// nothing).
    pub own: &'a [(String, BorrowMask)],
    /// The slot symbol of each callee whose mask the unit's code depends
    /// on, sorted: for a code unit the package's functions in other
    /// modules, for a tests unit every function of the package it calls.
    pub callees: &'a [String],
}

/// A unit's code key (D-969, D-958; GAP(code-key-contents)): under
/// `KeyKind::Code` for a code unit and `KeyKind::Tests` for a tests unit,
/// the build ID, the identity, the settings, the positions switch, the
/// own masks and the callees' slot symbols, in that order.
pub(crate) fn code_key(inputs: &CodeKeyInputs<'_>) -> Key {
    let kind = match inputs.part {
        UnitPart::Code => KeyKind::Code,
        UnitPart::Tests => KeyKind::Tests,
    };
    let mut b = Key::builder(kind, inputs.build)
        .str("k1")
        .hash(inputs.identity.hash())
        .settings(inputs.settings)
        .bytes(&[u8::from(inputs.positions)])
        .u64(u64::try_from(inputs.own.len()).unwrap_or(u64::MAX));
    for (item, mask) in inputs.own {
        b = b.str(item).u64(mask.0);
    }
    b = b.u64(u64::try_from(inputs.callees.len()).unwrap_or(u64::MAX));
    for symbol in inputs.callees {
        b = b.str(symbol);
    }
    b.finish()
}

/// The version of [`MasksRecord`]'s payload.
const MASKS_VERSION: u8 = 1;

/// A module's masks record (D-969): the mask of each of its functions
/// that borrows something, by item, sorted; a function missing from it
/// is all-owned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MasksRecord {
    /// The masks, by item.
    pub masks: Vec<(String, BorrowMask)>,
}

impl MasksRecord {
    /// The record's payload, sealed (D-960).
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(MASKS_VERSION);
        w.len(self.masks.len());
        for (item, mask) in &self.masks {
            w.str(item);
            w.u64(mask.0);
        }
        sealed(w.finish())
    }

    /// The record of a payload, or `None` when it is damaged or malformed.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader::new(unsealed(bytes)?);
        if r.u8()? != MASKS_VERSION {
            return None;
        }
        let mut masks = Vec::new();
        for _ in 0..r.len()? {
            masks.push((r.str()?.to_owned(), BorrowMask(r.u64()?)));
        }
        r.is_done().then_some(Self { masks })
    }
}
