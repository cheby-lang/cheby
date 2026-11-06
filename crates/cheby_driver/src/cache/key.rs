//! Cache keys (D-956, D-958): SipHash-1-3 128 with zero keys over a
//! canonical encoding.
//!
//! The encoding is a sequence of fields, each written as its length in
//! bytes (a little-endian `u64`) followed by its bytes. The first field is
//! the kind's tag ([`KeyKind::tag`]), the second the build ID's 16 bytes;
//! the caller's fields follow in the order it appends them. Because every
//! field carries its length, moving bytes from one field to the next
//! changes the key.

use siphasher::sip128::{Hasher128 as _, SipHasher13};

use super::{BuildId, Hash128};

/// What a key names, written first into its encoding (D-956).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyKind {
    /// A module's front record.
    Front,
    /// A module's borrowing facts.
    Facts,
    /// A module's masks.
    Masks,
    /// A module's code record.
    Code,
    /// A module's tests record.
    Tests,
    /// A constant's result.
    Const,
    /// The front end's recorded reads (a hint).
    Reads,
    /// Lowering's recorded reads (a hint).
    LReads,
    /// A C object.
    CObj,
    /// A package's shared library (D-941): a plain file under `v1/lib/`,
    /// named by the key (D-1197).
    NativeLib,
    /// A package index (a hint, D-962), over the package root's absolute
    /// path and the command family.
    Index,
}

impl KeyKind {
    /// The tag that starts the key's encoding.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Front => "front",
            Self::Facts => "facts",
            Self::Masks => "masks",
            Self::Code => "code",
            Self::Tests => "tests",
            Self::Const => "const",
            Self::Reads => "reads",
            Self::LReads => "lreads",
            Self::CObj => "cobj",
            Self::NativeLib => "nlib",
            Self::Index => "index",
        }
    }
}

/// The build kind, the first of the settings that change code (D-958).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BuildKind {
    /// `cheby run` and `cheby build`.
    Run,
    /// `cheby test`.
    Test,
    /// REPL package modules and constant evaluation (D-971, D-978).
    Plain,
    /// Std's own modules in every kind above.
    Std,
    /// `cargo xtask test-std`.
    StdSelfTest,
}

impl BuildKind {
    /// The kind's name in keys.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Test => "test",
            Self::Plain => "plain",
            Self::Std => "std",
            Self::StdSelfTest => "std-self-test",
        }
    }
}

/// The settings that change code (D-958), and nothing else: `--jobs`,
/// `--deny-warnings`, `--reuse-report`, color, `CHEBY_DUMP_DIR` and
/// `[runtime]` key nothing, and `[const-eval]` limits key only constant
/// results, as their own field.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "D-958's settings are independent switches"
)]
pub struct Settings {
    /// The build kind.
    pub kind: BuildKind,
    /// `CHEBY_RC=naive` (D-746).
    pub rc_naive: bool,
    /// Unwind edges (D-742).
    pub unwind_edges: bool,
    /// Checked count operations under poisoning (D-565).
    pub checked_counts: bool,
    /// The stack check.
    pub stack_check: bool,
    /// The target triple.
    pub target: String,
}

/// A cache key: names a record or a hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key(Hash128);

impl Key {
    /// A key of `kind` under `build`; fields are appended length-prefixed,
    /// in order.
    #[must_use]
    pub fn builder(kind: KeyKind, build: BuildId) -> KeyBuilder {
        KeyBuilder {
            hasher: SipHasher13::new_with_keys(0, 0),
        }
        .str(kind.tag())
        .bytes(&build.0)
    }

    /// The key's hash, which names its file.
    #[must_use]
    pub const fn hash(&self) -> Hash128 {
        self.0
    }
}

/// Builds a [`Key`] field by field.
#[derive(Clone, Debug)]
pub struct KeyBuilder {
    hasher: SipHasher13,
}

impl KeyBuilder {
    /// Appends a field of raw bytes.
    #[must_use]
    pub fn bytes(mut self, field: &[u8]) -> Self {
        let len = u64::try_from(field.len()).unwrap_or(u64::MAX);
        std::hash::Hasher::write(&mut self.hasher, &len.to_le_bytes());
        std::hash::Hasher::write(&mut self.hasher, field);
        self
    }

    /// Appends a field holding a string's UTF-8 bytes.
    #[must_use]
    pub fn str(self, field: &str) -> Self {
        self.bytes(field.as_bytes())
    }

    /// Appends a field holding a number's eight little-endian bytes.
    #[must_use]
    pub fn u64(self, field: u64) -> Self {
        self.bytes(&field.to_le_bytes())
    }

    /// Appends a field holding a hash's 16 bytes.
    #[must_use]
    pub fn hash(self, field: Hash128) -> Self {
        self.bytes(&field.0)
    }

    /// Appends the settings as six fields: the kind's tag, the four
    /// switches as one byte each (0 or 1), and the target.
    #[must_use]
    pub fn settings(self, s: &Settings) -> Self {
        self.str(s.kind.tag())
            .bytes(&[u8::from(s.rc_naive)])
            .bytes(&[u8::from(s.unwind_edges)])
            .bytes(&[u8::from(s.checked_counts)])
            .bytes(&[u8::from(s.stack_check)])
            .str(&s.target)
    }

    /// The key.
    #[must_use]
    pub fn finish(self) -> Key {
        Key(Hash128(self.hasher.finish128().as_bytes()))
    }
}
