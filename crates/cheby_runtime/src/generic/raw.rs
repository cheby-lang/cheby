//! What a word is, for the generic routines: [`View`] classifies a uniform
//! word by its layout, and [`Raw`] decodes a raw constructor field.

use crate::abi::{self, Word};
use crate::alloc;
use crate::desc;
use crate::layout::{self, CustomKind, FieldKind, LayoutInfo, LayoutKind};
use crate::object::{read_field, scan_and_layout, word_to_ptr};
use crate::panic::fatal;
use crate::string;

/// A uniform word, classified.
pub(super) enum View<'a> {
    /// A tagged integer (`Int`, a sized integer, or a small `U64`).
    Tagged(i64),
    /// A `BoxedInt`.
    Int(i64),
    /// A `BoxedU64`.
    U64(u64),
    /// A `BoxedFloat`.
    Float(f64),
    /// A `BoxedF32`.
    F32(f32),
    /// A `String`.
    String(&'a [u8]),
    /// A `Bytes`.
    Bytes(&'a [u8]),
    /// A tuple's elements.
    Tuple(Vec<Word>),
    /// A closure (a function value).
    Closure,
    /// A constructor value and its layout.
    Ctor(&'static LayoutInfo),
    /// A `dyn` value: its type's descriptor (field 0 of its dictionary)
    /// and the value inside.
    Dyn {
        /// The descriptor of the value's type.
        desc: Word,
        /// The value.
        value: Word,
    },
    /// A handle, with its layout: a value of an external type (§12.5.3),
    /// or of one of std's handle kinds 40 to 44 (D-647, D-703), which
    /// share that path ([`LayoutInfo::is_external`], D-233).
    External(&'static LayoutInfo),
    /// A type descriptor or a nominal descriptor, read into its parts.
    Desc(desc::Parts),
    /// A runtime-defined kind.
    Custom(&'static CustomKind),
    /// A dictionary, which values never contain where equality, hashing or
    /// printing can reach them.
    Internal(&'static LayoutInfo),
}

/// Classifies a borrowed uniform word.
///
/// # Safety
///
/// `word` is a tagged integer or a live object, not changed while the view
/// is used.
pub(super) unsafe fn view<'a>(word: Word, what: &str) -> View<'a> {
    if abi::is_tagged_int(word) {
        return View::Tagged(abi::untag_int(word));
    }
    if word == 0 {
        fatal(format_args!("{what} reached a null word"));
    }
    // SAFETY: forwarded from the caller.
    unsafe { alloc::check_live(word, what) };
    let obj = word_to_ptr(word);
    // SAFETY: a live object.
    let (scan, id) = unsafe { scan_and_layout(obj) };
    let Some(info) = layout::get(id) else {
        fatal(format_args!(
            "{what} reached an object with unknown layout {id}"
        ));
    };
    // SAFETY: a live box has its payload at offset 8.
    let payload = || unsafe { read_field(obj, 8) };
    match info.kind {
        LayoutKind::BoxedInt => View::Int(payload().cast_signed()),
        LayoutKind::BoxedU64 => View::U64(payload()),
        LayoutKind::BoxedFloat => View::Float(f64::from_bits(payload())),
        LayoutKind::BoxedF32 => View::F32(f32_bits(payload())),
        // SAFETY: a live text object, not changed while the view is used.
        LayoutKind::String => View::String(unsafe { text(word) }),
        // SAFETY: as above.
        LayoutKind::Bytes => View::Bytes(unsafe { text(word) }),
        LayoutKind::Tuple => {
            // SAFETY: a live tuple: the arity is the scan count, or the
            // tagged count at offset 8 when escaped.
            let (count, first) = if scan == abi::SCAN_ESCAPE {
                (abi::untag_int(unsafe { read_field(obj, 8) }), 16_usize)
            } else {
                (i64::from(scan), 8)
            };
            let fields = (0..usize::try_from(count).unwrap_or(0))
                .map(|index| {
                    // SAFETY: within the tuple.
                    unsafe { read_field(obj, index.saturating_mul(8).saturating_add(first)) }
                })
                .collect();
            View::Tuple(fields)
        }
        LayoutKind::Closure => View::Closure,
        LayoutKind::Ctor { .. } => View::Ctor(info),
        LayoutKind::Dyn => {
            // SAFETY: a live `dyn` value holds its dictionary at offset 8
            // and its value at 16 (D-524).
            let (mut dict, mut value) = unsafe { (read_field(obj, 8), read_field(obj, 16)) };
            // D-604: a `dyn` value whose value is itself a `dyn` value is
            // that inner value. Generic code builds one when it
            // converts a value of a type parameter instantiated with a
            // `dyn` type; a direct conversion keeps the inner descriptor
            // (D-502), and so does the generic one through this rule.
            // SAFETY: `value` is borrowed from a live object.
            while let Some((inner_dict, inner)) = unsafe { dyn_parts(value, what) } {
                dict = inner_dict;
                value = inner;
            }
            // SAFETY: a live dictionary holds its type's descriptor at
            // offset 8, escaped or not (D-555).
            let desc = unsafe { read_field(word_to_ptr(dict), 8) };
            View::Dyn { desc, value }
        }
        LayoutKind::Custom(_) if info.is_external() => View::External(info),
        LayoutKind::Custom(kind) => View::Custom(kind),
        // SAFETY: a live descriptor.
        LayoutKind::Desc | LayoutKind::DescNominal => match unsafe { desc::parts(word) } {
            Some(parts) => View::Desc(parts),
            None => View::Internal(info),
        },
        LayoutKind::Dict => View::Internal(info),
    }
}

/// The dictionary and the value of `word` if it is a `dyn` value.
///
/// # Safety
///
/// `word` is a tagged integer or a live object.
unsafe fn dyn_parts(word: Word, what: &str) -> Option<(Word, Word)> {
    if abi::is_tagged_int(word) || word == 0 {
        return None;
    }
    // SAFETY: forwarded from the caller.
    unsafe { alloc::check_live(word, what) };
    let obj = word_to_ptr(word);
    // SAFETY: a live object.
    let (_, id) = unsafe { scan_and_layout(obj) };
    if id != abi::layout::DYN {
        return None;
    }
    // SAFETY: a live `dyn` value: its dictionary at 8, its value at 16.
    Some(unsafe { (read_field(obj, 8), read_field(obj, 16)) })
}

/// The contents of a live text object.
///
/// # Safety
///
/// `word` is a live `String` or `Bytes`, unchanged for `'a`.
unsafe fn text<'a>(word: Word) -> &'a [u8] {
    // SAFETY: forwarded from the caller.
    unsafe { string::text_bytes(word) }.unwrap_or_default()
}

/// The `f32` in the low half of an 8-byte field.
pub(super) fn f32_bits(bits: u64) -> f32 {
    let [b0, b1, b2, b3, ..] = bits.to_le_bytes();
    f32::from_bits(u32::from_le_bytes([b0, b1, b2, b3]))
}

/// A decoded raw constructor field.
#[derive(Debug, Clone, Copy)]
pub(super) enum Raw {
    /// `Int`, `I64` and the signed sized integers.
    Int(i64),
    /// The unsigned sized integers.
    Unsigned(u64),
    /// `Float`.
    Float(f64),
    /// `F32`.
    F32(f32),
    /// `Bool`.
    Bool(bool),
}

impl Raw {
    /// Decodes the 8-byte field of a raw field of the given kind. Narrow
    /// fields are read from the low bytes (little-endian).
    pub(super) fn decode(kind: FieldKind, bits: u64) -> Self {
        let [b0, b1, b2, b3, ..] = bits.to_le_bytes();
        match kind {
            // A value field is never decoded; treated as its bits.
            FieldKind::Value | FieldKind::U64 => Self::Unsigned(bits),
            FieldKind::Int | FieldKind::I64 => Self::Int(bits.cast_signed()),
            FieldKind::Float => Self::Float(f64::from_bits(bits)),
            FieldKind::F32 => Self::F32(f32_bits(bits)),
            FieldKind::Bool => Self::Bool(b0 != 0),
            FieldKind::I8 => Self::Int(i64::from(b0.cast_signed())),
            FieldKind::I16 => Self::Int(i64::from(i16::from_le_bytes([b0, b1]))),
            FieldKind::I32 => Self::Int(i64::from(i32::from_le_bytes([b0, b1, b2, b3]))),
            FieldKind::U8 => Self::Unsigned(u64::from(b0)),
            FieldKind::U16 => Self::Unsigned(u64::from(u16::from_le_bytes([b0, b1]))),
            FieldKind::U32 => Self::Unsigned(u64::from(u32::from_le_bytes([b0, b1, b2, b3]))),
        }
    }

    /// Equality of two raw fields of one kind (§3.3.2 for floats).
    pub(super) fn same(self, other: Self) -> bool {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Unsigned(a), Self::Unsigned(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => float_eq(a, b),
            (Self::F32(a), Self::F32(b)) => f32_eq(a, b),
            (Self::Bool(a), Self::Bool(b)) => a == b,
            _ => false,
        }
    }
}

/// `Float` equality (§3.3.2, D-069): every NaN equals every NaN, and
/// `-0.0 == 0.0`.
pub(super) fn float_eq(a: f64, b: f64) -> bool {
    a == b || (a.is_nan() && b.is_nan())
}

/// `F32` equality, as for `Float`.
pub(super) fn f32_eq(a: f32, b: f32) -> bool {
    a == b || (a.is_nan() && b.is_nan())
}

/// The bits a `Float` hashes as: one value for every NaN, one for both
/// zeros, so equal floats hash equally (§3.3.2).
pub(super) fn float_hash_bits(value: f64) -> u64 {
    if value.is_nan() {
        f64::NAN.to_bits()
    } else if value == 0.0 {
        0
    } else {
        value.to_bits()
    }
}

/// The bits an `F32` hashes as.
pub(super) fn f32_hash_bits(value: f32) -> u64 {
    if value.is_nan() {
        u64::from(f32::NAN.to_bits())
    } else if value == 0.0 {
        0
    } else {
        u64::from(value.to_bits())
    }
}
