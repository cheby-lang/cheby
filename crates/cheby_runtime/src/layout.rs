//! The global layout table (D-279, D-252): for every layout id, the type,
//! the constructor tag and the field layout.
//!
//! Ids 1-63 are fixed builtins ([`abi::layout`]); user constructors are
//! registered from id 64 on, in call order, before any code that uses
//! them is defined (D-454).
//!
//! The table is append-only: 4096 chunks of 4096 slots behind `AtomicPtr`s,
//! 2^24 ids in all. Registering takes a lock; [`get`] takes none, because
//! freeing reads the table to find an object's size.

use std::fmt;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use crate::abi::{self, LayoutId, layout};

/// What kind of object a layout describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutKind {
    /// `String`: header, `len`, `cap`, bytes.
    String,
    /// `Bytes`: the same shape as `String` (F2).
    Bytes,
    /// A boxed `Int` or `I64` outside 63 bits.
    BoxedInt,
    /// A boxed `U64` outside 63 bits.
    BoxedU64,
    /// A boxed `Float`.
    BoxedFloat,
    /// A boxed `F32`.
    BoxedF32,
    /// A closure: the code word at offset 8, then captures; every field is
    /// a uniform value and the scan count is the field count.
    Closure,
    /// A tuple of uniform values; the arity is the scan count, or the
    /// tagged count at offset 8 when the scan count is
    /// [`abi::SCAN_ESCAPE`].
    Tuple,
    /// A constructor of an ADT: its tag (declaration order within the type)
    /// and whether it is a unit constructor (a static object).
    Ctor {
        /// The constructor's index in its type's declaration order.
        tag: u32,
        /// A constructor with no fields, represented as a static object.
        unit: bool,
    },
}

/// How a field is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// A uniform word: a tagged integer or an object pointer. Freeing
    /// visits these.
    Value,
    /// A raw `Int` (`i64`).
    Int,
    /// A raw `Float` (`f64`).
    Float,
    /// A raw `Bool` (0 or 1).
    Bool,
    /// A raw `I8`.
    I8,
    /// A raw `I16`.
    I16,
    /// A raw `I32`.
    I32,
    /// A raw `I64`.
    I64,
    /// A raw `U8`.
    U8,
    /// A raw `U16`.
    U16,
    /// A raw `U32`.
    U32,
    /// A raw `U64`.
    U64,
    /// A raw `F32`.
    F32,
}

/// One field of a layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldInfo {
    /// Byte offset from the start of the object.
    pub offset: u32,
    /// How the field is stored.
    pub kind: FieldKind,
    /// The field's name, for named-field variants.
    pub name: Option<Box<str>>,
}

/// One entry of the layout table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutInfo {
    /// The name shown by debug printing, such as `Option.Some` or `String`.
    pub name: Box<str>,
    /// What the layout describes.
    pub kind: LayoutKind,
    /// The number of uniform fields, which come first in memory. Equal to
    /// the header's scan count unless that exceeds [`abi::SCAN_MAX`].
    pub value_fields: u32,
    /// The number of raw fields, after the uniform ones.
    pub raw_fields: u32,
    /// Every field in declaration order. Equality, hashing and debug
    /// printing walk this list, so they visit fields in declaration order
    /// whatever the memory order is (D-233, D-252). Empty for the kinds
    /// whose shape is not a fixed field list (strings, closures, tuples).
    pub fields: Box<[FieldInfo]>,
}

impl LayoutInfo {
    /// A constructor layout whose fields are given in declaration order;
    /// value fields are laid out first, then raw fields, each in
    /// declaration order.
    #[must_use]
    pub fn ctor(name: &str, tag: u32, fields: &[(FieldKind, Option<&str>)]) -> Self {
        let mut value_offset = abi::HEADER_SIZE;
        let value_count = fields
            .iter()
            .filter(|(kind, _)| *kind == FieldKind::Value)
            .count();
        let mut raw_offset = abi::HEADER_SIZE
            .saturating_add(abi::FIELD_SIZE.saturating_mul(crate::object::to_u64(value_count)));
        let infos = fields
            .iter()
            .map(|(kind, field_name)| {
                let slot = if *kind == FieldKind::Value {
                    &mut value_offset
                } else {
                    &mut raw_offset
                };
                let offset = u32::try_from(*slot).unwrap_or(u32::MAX);
                *slot = slot.saturating_add(abi::FIELD_SIZE);
                FieldInfo {
                    offset,
                    kind: *kind,
                    name: field_name.map(Box::from),
                }
            })
            .collect::<Box<[_]>>();
        let value_fields = u32::try_from(value_count).unwrap_or(u32::MAX);
        let raw_fields =
            u32::try_from(fields.len().saturating_sub(value_count)).unwrap_or(u32::MAX);
        Self {
            name: name.into(),
            kind: LayoutKind::Ctor {
                tag,
                unit: fields.is_empty(),
            },
            value_fields,
            raw_fields,
            fields: infos,
        }
    }

    /// A layout with no field list.
    fn shaped(name: &str, kind: LayoutKind, value_fields: u32, raw_fields: u32) -> Self {
        Self {
            name: name.into(),
            kind,
            value_fields,
            raw_fields,
            fields: Box::new([]),
        }
    }

    /// The scan count the header of an object of this layout carries.
    #[must_use]
    pub fn scan_count(&self) -> u8 {
        u8::try_from(self.value_fields)
            .ok()
            .filter(|scan| *scan <= abi::SCAN_MAX)
            .unwrap_or(abi::SCAN_ESCAPE)
    }

    /// The size in bytes of an object of a fixed-size layout (constructors
    /// and boxed numbers); `None` for strings, bytes, closures and tuples,
    /// whose size depends on the object.
    #[must_use]
    pub fn fixed_size(&self) -> Option<u64> {
        match self.kind {
            LayoutKind::String | LayoutKind::Bytes | LayoutKind::Closure | LayoutKind::Tuple => {
                None
            }
            LayoutKind::BoxedInt
            | LayoutKind::BoxedU64
            | LayoutKind::BoxedFloat
            | LayoutKind::BoxedF32
            | LayoutKind::Ctor { .. } => {
                let fields =
                    u64::from(self.value_fields).checked_add(u64::from(self.raw_fields))?;
                abi::FIELD_SIZE
                    .checked_mul(fields)?
                    .checked_add(abi::HEADER_SIZE)
            }
        }
    }

    fn validate(&self) -> Result<(), LayoutError> {
        let LayoutKind::Ctor { unit, .. } = self.kind else {
            return Err(LayoutError::Invalid(
                "only constructor layouts are registered".into(),
            ));
        };
        let total = u64::from(self.value_fields).saturating_add(u64::from(self.raw_fields));
        if crate::object::to_u64(self.fields.len()) != total {
            return Err(LayoutError::Invalid(format!(
                "`{}` lists {} fields but counts {total}",
                self.name,
                self.fields.len()
            )));
        }
        if unit != self.fields.is_empty() {
            return Err(LayoutError::Invalid(format!(
                "`{}` is marked unit inconsistently",
                self.name
            )));
        }
        let value_end = abi::HEADER_SIZE
            .saturating_add(abi::FIELD_SIZE.saturating_mul(u64::from(self.value_fields)));
        let end = self.fixed_size().unwrap_or(0);
        let mut seen = vec![false; self.fields.len()];
        for field in &self.fields {
            let offset = u64::from(field.offset);
            let (start, limit) = if field.kind == FieldKind::Value {
                (abi::HEADER_SIZE, value_end)
            } else {
                (value_end, end)
            };
            let in_range = offset >= start && offset < limit && offset % abi::FIELD_SIZE == 0;
            let index =
                crate::object::to_usize(offset.saturating_sub(abi::HEADER_SIZE) / abi::FIELD_SIZE);
            let fresh = seen
                .get_mut(index)
                .is_some_and(|slot| !std::mem::replace(slot, true));
            if !in_range || !fresh {
                return Err(LayoutError::Invalid(format!(
                    "`{}` has a {:?} field at a bad offset {offset}",
                    self.name, field.kind
                )));
            }
        }
        Ok(())
    }
}

/// Why a layout could not be registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    /// All 2^24 layout ids are taken.
    TableFull,
    /// The layout is malformed; the message says how.
    Invalid(String),
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TableFull => f.write_str("the layout table is full"),
            Self::Invalid(message) => write!(f, "invalid layout: {message}"),
        }
    }
}

impl std::error::Error for LayoutError {}

const CHUNK_BITS: u32 = 12;
const CHUNK_SIZE: usize = 1 << CHUNK_BITS;
const CHUNK_COUNT: usize = 1 << CHUNK_BITS;

/// One chunk of user layouts: `CHUNK_SIZE` slots, leaked once allocated.
type Slot = AtomicPtr<LayoutInfo>;

static CHUNKS: [AtomicPtr<Slot>; CHUNK_COUNT] =
    [const { AtomicPtr::new(ptr::null_mut()) }; CHUNK_COUNT];

/// The next user id to hand out. Registration is serialized by this lock.
static NEXT_ID: Mutex<LayoutId> = Mutex::new(layout::FIRST_USER);

static BUILTINS: OnceLock<Box<[Option<LayoutInfo>]>> = OnceLock::new();

fn builtins() -> &'static [Option<LayoutInfo>] {
    BUILTINS.get_or_init(|| {
        let value = |name: Option<&'static str>| (FieldKind::Value, name);
        let mut table: Vec<Option<LayoutInfo>> =
            vec![None; crate::object::to_usize(u64::from(layout::FIRST_USER))];
        let mut set = |id: LayoutId, info: LayoutInfo| {
            if let Some(slot) = table.get_mut(crate::object::to_usize(u64::from(id))) {
                *slot = Some(info);
            }
        };
        set(
            layout::STRING,
            LayoutInfo::shaped("String", LayoutKind::String, 0, 0),
        );
        set(
            layout::BYTES,
            LayoutInfo::shaped("Bytes", LayoutKind::Bytes, 0, 0),
        );
        set(
            layout::BOXED_INT,
            LayoutInfo::shaped("Int", LayoutKind::BoxedInt, 0, 1),
        );
        set(
            layout::BOXED_FLOAT,
            LayoutInfo::shaped("Float", LayoutKind::BoxedFloat, 0, 1),
        );
        set(
            layout::BOXED_F32,
            LayoutInfo::shaped("F32", LayoutKind::BoxedF32, 0, 1),
        );
        set(
            layout::CLOSURE,
            LayoutInfo::shaped("closure", LayoutKind::Closure, 0, 0),
        );
        set(
            layout::TUPLE,
            LayoutInfo::shaped("tuple", LayoutKind::Tuple, 0, 0),
        );
        set(
            layout::BOXED_U64,
            LayoutInfo::shaped("U64", LayoutKind::BoxedU64, 0, 1),
        );
        set(layout::NIL, LayoutInfo::ctor("Nil", 0, &[]));
        set(layout::FALSE, LayoutInfo::ctor("Bool.False", 0, &[]));
        set(layout::TRUE, LayoutInfo::ctor("Bool.True", 1, &[]));
        set(layout::LESS, LayoutInfo::ctor("Order.Less", 0, &[]));
        set(layout::EQUAL, LayoutInfo::ctor("Order.Equal", 1, &[]));
        set(layout::GREATER, LayoutInfo::ctor("Order.Greater", 2, &[]));
        set(
            layout::SOME,
            LayoutInfo::ctor("Option.Some", 0, &[value(None)]),
        );
        set(layout::NONE, LayoutInfo::ctor("Option.None", 1, &[]));
        set(layout::OK, LayoutInfo::ctor("Result.Ok", 0, &[value(None)]));
        set(
            layout::ERR,
            LayoutInfo::ctor("Result.Err", 1, &[value(None)]),
        );
        table.into_boxed_slice()
    })
}

/// Splits a user id into its chunk and slot indices.
fn split(id: LayoutId) -> (usize, usize) {
    let id = crate::object::to_usize(u64::from(id));
    (id >> CHUNK_BITS, id & (CHUNK_SIZE - 1))
}

/// Registers a constructor layout and returns its id, from
/// [`layout::FIRST_USER`] on in call order.
///
/// # Errors
///
/// [`LayoutError::Invalid`] if the layout is not a well-formed constructor
/// layout (field count and offsets), and [`LayoutError::TableFull`] once
/// all 2^24 ids are taken.
pub fn register(info: LayoutInfo) -> Result<LayoutId, LayoutError> {
    info.validate()?;
    let mut next = NEXT_ID.lock().unwrap_or_else(PoisonError::into_inner);
    let id = *next;
    if id > abi::LAYOUT_MAX {
        return Err(LayoutError::TableFull);
    }
    let (chunk_index, slot_index) = split(id);
    let Some(chunk_cell) = CHUNKS.get(chunk_index) else {
        return Err(LayoutError::TableFull);
    };
    let mut chunk = chunk_cell.load(Ordering::Acquire);
    if chunk.is_null() {
        let slots: Box<[Slot]> = (0..CHUNK_SIZE)
            .map(|_| AtomicPtr::new(ptr::null_mut()))
            .collect();
        chunk = Box::leak(slots).as_mut_ptr();
        chunk_cell.store(chunk, Ordering::Release);
    }
    // SAFETY: every chunk is a leaked slice of `CHUNK_SIZE` slots, never
    // freed, and `slot_index < CHUNK_SIZE`.
    let slot = unsafe { &*chunk.add(slot_index) };
    slot.store(Box::into_raw(Box::new(info)), Ordering::Release);
    *next = id.saturating_add(1);
    Ok(id)
}

/// The layout with the given id, or `None` for an id that is not a builtin
/// and has not been registered. Takes no lock.
#[must_use]
pub fn get(id: LayoutId) -> Option<&'static LayoutInfo> {
    if id < layout::FIRST_USER {
        return builtins()
            .get(crate::object::to_usize(u64::from(id)))?
            .as_ref();
    }
    let (chunk_index, slot_index) = split(id);
    let chunk = CHUNKS.get(chunk_index)?.load(Ordering::Acquire);
    if chunk.is_null() {
        return None;
    }
    // SAFETY: as in `register`: a leaked chunk of `CHUNK_SIZE` slots.
    let slot = unsafe { &*chunk.add(slot_index) };
    let info = slot.load(Ordering::Acquire);
    // SAFETY: a non-null slot holds a leaked `Box<LayoutInfo>`, published
    // with release ordering and never freed or changed again.
    (!info.is_null()).then(|| unsafe { &*info })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_layouts_are_fixed() {
        assert!(get(layout::INVALID).is_none());
        assert_eq!(
            get(layout::STRING).map(|info| info.kind),
            Some(LayoutKind::String)
        );
        assert_eq!(
            get(layout::TUPLE).map(|info| info.kind),
            Some(LayoutKind::Tuple)
        );
        assert_eq!(
            get(layout::BOXED_U64).map(|info| info.kind),
            Some(LayoutKind::BoxedU64)
        );
        assert!(get(9).is_none());
        assert!(get(layout::RUNTIME_RESERVED_END).is_none());
        assert!(get(layout::BUILTIN_RESERVED_END).is_none());
        let some = get(layout::SOME);
        assert_eq!(
            some.map(|info| info.kind),
            Some(LayoutKind::Ctor {
                tag: 0,
                unit: false
            })
        );
        assert_eq!(some.and_then(LayoutInfo::fixed_size), Some(16));
        assert_eq!(
            get(layout::NONE).map(|info| info.kind),
            Some(LayoutKind::Ctor { tag: 1, unit: true })
        );
        assert_eq!(get(layout::TRUE).map(|info| &*info.name), Some("Bool.True"));
        assert_eq!(
            get(layout::BOXED_FLOAT).and_then(LayoutInfo::fixed_size),
            Some(16)
        );
    }

    #[test]
    fn ctor_layouts_put_value_fields_first() {
        let info = LayoutInfo::ctor(
            "geo.Point",
            0,
            &[
                (FieldKind::Float, Some("x")),
                (FieldKind::Value, Some("label")),
                (FieldKind::Int, Some("y")),
            ],
        );
        let offsets: Vec<_> = info
            .fields
            .iter()
            .map(|field| (field.offset, field.kind))
            .collect();
        assert_eq!(
            offsets,
            [
                (16, FieldKind::Float),
                (8, FieldKind::Value),
                (24, FieldKind::Int)
            ]
        );
        assert_eq!((info.value_fields, info.raw_fields), (1, 2));
        assert_eq!(info.scan_count(), 1);
        assert_eq!(info.fixed_size(), Some(32));
        assert_eq!(
            info.fields.first().and_then(|field| field.name.as_deref()),
            Some("x")
        );
    }

    #[test]
    fn register_hands_out_user_ids_and_get_finds_them() {
        let first = register(LayoutInfo::ctor("t.A", 0, &[(FieldKind::Value, None)])).unwrap();
        let second = register(LayoutInfo::ctor("t.B", 1, &[])).unwrap();
        assert!(first >= layout::FIRST_USER);
        assert!(second > first);
        assert_eq!(get(first).map(|info| &*info.name), Some("t.A"));
        assert_eq!(get(second).map(|info| &*info.name), Some("t.B"));
        assert!(get(abi::LAYOUT_MAX).is_none());
        assert!(get(u32::MAX).is_none());
    }

    #[test]
    fn register_rejects_malformed_layouts() {
        let mut info = LayoutInfo::ctor("t.Bad", 0, &[(FieldKind::Value, None)]);
        info.value_fields = 2;
        assert!(matches!(register(info), Err(LayoutError::Invalid(_))));

        let mut info = LayoutInfo::ctor(
            "t.Bad",
            0,
            &[(FieldKind::Value, None), (FieldKind::Int, None)],
        );
        if let Some(field) = info.fields.get_mut(1) {
            field.offset = 8;
        }
        assert!(matches!(register(info), Err(LayoutError::Invalid(_))));

        let tuple = LayoutInfo::shaped("tuple", LayoutKind::Tuple, 0, 0);
        assert!(matches!(register(tuple), Err(LayoutError::Invalid(_))));
    }

    #[test]
    fn wide_ctors_escape_the_scan_count() {
        let fields = vec![(FieldKind::Value, None); 300];
        let info = LayoutInfo::ctor("t.Wide", 0, &fields);
        assert_eq!(info.scan_count(), abi::SCAN_ESCAPE);
        assert_eq!(info.fixed_size(), Some(8 + 8 * 300));
    }
}
