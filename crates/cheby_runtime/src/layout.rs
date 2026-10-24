//! The global layout table (D-279, D-252): for every layout id, the type,
//! the constructor and the field layout.
//!
//! - Ids 1-25 are fixed builtins ([`abi::layout`]): strings, boxed
//!   numbers, closures, tuples, dictionaries, `dyn` values, descriptors
//!   and the builtin constructors.
//! - Ids 26-63 are runtime-defined kinds ([`LayoutKind::Custom`]), such as
//!   list and map nodes, registered by their modules with
//!   [`register_builtin`] before first use (plan §4.3.2).
//! - User constructors are registered from id 64 on, in call order, before
//!   any code that uses them is defined (D-454). The constructors of one
//!   type are registered together with [`register_consecutive`], so their
//!   ids are consecutive in tag order (D-522). The values of each external
//!   type get a layout of their own there too ([`register_external`]).
//!
//! The table is append-only: 4096 chunks of 4096 slots behind `AtomicPtr`s,
//! 2^24 ids in all. Registering takes a lock; [`get`] takes none, because
//! freeing reads the table to find an object's size.
//!
//! Next to it, the free-shape table ([`fast_word`], D-907) gives the first
//! 16,384 ids one self-contained word each: where an object of the layout
//! keeps its references and how large it is. Freeing reads that word
//! instead of following [`get`]'s pointers and matching on the kind. It
//! takes [`get`]'s path for a 0 word, which a higher id, a kind with a
//! finalizer and one that declares no [`CustomKind::shape`] have, and for
//! a tuple, closure, dictionary or descriptor in the escaped form. A
//! constructor with more than 254 value fields is still freed from its
//! word.
//!
//! Hashing never uses a layout id (D-538): ids depend on registration
//! order. It uses [`LayoutInfo::type_key`] and [`LayoutInfo::tag`].

use std::fmt;
use std::hash::Hasher as _;
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use siphasher::sip::SipHasher13;

use crate::abi::{self, LayoutId, Word, layout};
use crate::generic;
use crate::object::{to_u64, to_usize};

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
    /// a uniform value and the scan count is the field count, or
    /// [`abi::SCAN_ESCAPE`] with the escaped form described there.
    Closure,
    /// A tuple of uniform values; the arity is the scan count, or the
    /// tagged count at offset 8 when the scan count is
    /// [`abi::SCAN_ESCAPE`].
    Tuple,
    /// A dictionary (D-524): every field is a uniform word, the scan count
    /// is the field count, or [`abi::SCAN_ESCAPE`] with the escaped form
    /// described there.
    Dict,
    /// A `dyn` value (D-524): the dictionary, then the value.
    Dyn,
    /// A type descriptor (D-524): the nominal descriptor, then the
    /// argument descriptors; the scan count is the field count, or
    /// [`abi::SCAN_ESCAPE`] with the escaped form described there.
    Desc,
    /// A nominal descriptor (D-524): a static object holding the type's
    /// printed name and module path as static strings.
    DescNominal,
    /// A constructor of an ADT: its tag (declaration order within the type)
    /// and whether it is a unit constructor (a static object).
    Ctor {
        /// The constructor's index in its type's declaration order.
        tag: u32,
        /// A constructor with no fields, represented as a static object.
        unit: bool,
    },
    /// A runtime-defined object (ids 26-63), described by its functions.
    Custom(&'static CustomKind),
}

/// A runtime-defined object kind (plan §4.3.2), such as a list or map
/// node. The runtime frees, compares, hashes and prints such objects only
/// through these functions, except that a kind which declares a
/// [`CustomKind::shape`] is freed from that shape (D-907).
///
/// Each function receives the object's word and may assume it points to a
/// live object of this kind. None of them may recurse into child values:
/// they hand children to the caller (`children`'s visitor, or the
/// [`generic::Cx`], [`generic::Hasher`] and [`generic::Printer`] worklists),
/// so walking a million-deep value never deepens the Rust stack (plan §0,
/// deep data).
pub struct CustomKind {
    /// The kind's name, for diagnostics.
    pub name: &'static str,
    /// The object's size in bytes, header included, exactly as it was
    /// allocated (the size passed to `alloc_object`).
    ///
    /// Safety: called only with a live object of this kind.
    pub size: unsafe fn(Word) -> u64,
    /// Calls the visitor once for every reference the object owns (uniform
    /// words; tagged integers and 0 may be passed and are skipped). Freeing
    /// releases each one through the allocator's worklist, never
    /// recursively.
    ///
    /// Safety: called only with a live object of this kind, whose count
    /// may have reached zero.
    pub children: unsafe fn(Word, &mut dyn FnMut(Word)),
    /// Structural equality of two objects of this kind: returns `false` if
    /// they differ at this level, and otherwise schedules every pair of
    /// children that must also be equal with [`generic::Cx::push`].
    /// `None`: the generic routine reports the kind as not comparable yet.
    ///
    /// Safety: called only with live objects of this kind.
    pub equal: Option<unsafe fn(Word, Word, &mut generic::Cx) -> bool>,
    /// Hashing: writes this level's data to the hasher and schedules the
    /// children with [`generic::Hasher::push`]. `None`: not hashable yet.
    ///
    /// Safety: called only with a live object of this kind.
    pub hash: Option<unsafe fn(Word, &mut generic::Hasher)>,
    /// Debug printing: emits text and child values, in order, with
    /// [`generic::Printer::text`] and [`generic::Printer::value`]. `None`:
    /// printed as `<name>`.
    ///
    /// Safety: called only with a live object of this kind.
    pub debug: Option<unsafe fn(Word, &mut generic::Printer)>,
    /// A handle (D-703): `==`, hashing and debug text treat an object of
    /// this kind like a value of an external type (D-189, D-233, D-590),
    /// through [`LayoutInfo::is_external`]: equality and hashing stop on
    /// it with the catchable panic of a function value, and it prints as
    /// `<name>`. `equal`, `hash` and `debug` are then never called.
    pub handle: bool,
    /// Runs exactly once when the object's count reaches zero, before its
    /// block is freed and after `children` was visited (D-703). It gives
    /// up every reference the object's payload owns by passing it to
    /// `release`, which feeds the free worklist, so finalizing a million
    /// handles that each hold the next one never deepens the Rust stack.
    /// It never blocks, suspends, raises or runs Cheby code; a Rust panic
    /// in it ends the process (D-171).
    ///
    /// Safety: called only with an object of this kind whose count has
    /// reached zero, once.
    pub finalize: Option<Finalizer>,
    /// Where an object of this kind keeps its references and how large it
    /// is, in a form the free path reads from the free-shape table without
    /// calling `size` and `children` (D-907). It must describe exactly
    /// what those two functions compute, which debug builds check on
    /// every free of an object of the kind. [`KindShape::Opaque`] keeps
    /// the calls. A kind with a finalizer or a handle kind must be
    /// opaque, and [`register_builtin`] refuses one that is not.
    pub shape: KindShape,
}

/// The shape a runtime-defined kind declares for the free path
/// ([`CustomKind::shape`], D-907).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindShape {
    /// No declared shape: freeing calls `size` and `children`.
    Opaque,
    /// Every object is `size` bytes and holds `fields` value fields from
    /// offset 8.
    Fixed {
        /// The size in bytes, header included.
        size: usize,
        /// The value fields from offset 8.
        fields: usize,
    },
    /// A raw meta word at offset 8, whose low 32 bits count the value
    /// fields from offset 16: list leaves and branches.
    Counted {
        /// The size in bytes when every object has the same, or `None`
        /// when it is 16 plus 8 times the meta word's high 32 bits, the
        /// capacity.
        size: Option<usize>,
    },
}

/// A kind's finalizer ([`CustomKind::finalize`]): the dying object, and
/// the function that releases each reference its payload owns.
pub type Finalizer = unsafe fn(Word, &mut dyn FnMut(Word));

impl fmt::Debug for CustomKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CustomKind")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Kinds are compared by identity: each kind is one `static`.
impl PartialEq for CustomKind {
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self, other)
    }
}

impl Eq for CustomKind {}

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
    /// The constructor's full name: `Type.Ctor` for a builtin one, such as
    /// `Option.Some`, and the constructor's path, `module::Ctor`, for
    /// generated code's layouts (MIR's `LayoutDesc::name`), for
    /// diagnostics.
    pub name: Box<str>,
    /// The type's name as debug printing shows it (`Option`, `Shape`).
    pub type_name: Box<str>,
    /// The constructor's name as debug printing shows it (`Some`,
    /// `Circle`); for other kinds, the kind's name.
    pub ctor_name: Box<str>,
    /// Whether the constructor has named fields, printed as
    /// `Point { x: 1, y: 2 }` rather than `Point(1, 2)`.
    pub record: bool,
    /// The type's stable key ([`type_key`] of its module path and name),
    /// which hashing uses instead of the layout id (D-538).
    pub type_key: u64,
    /// The constructor's tag (declaration order); 0 for other kinds.
    pub tag: u32,
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
    /// The type's `@std_hooks` (equal, hash, debug) closures (D-482),
    /// set once with [`set_hooks`] (W3-B).
    hooks: OnceLock<[Word; 3]>,
}

/// The stable key of a type for hashing (D-538): SipHash-1-3 with a zero
/// key of the module path's UTF-8 bytes (empty for builtin types), one
/// `0xff` byte, then the type name's bytes. The same function as
/// `cheby_mir::layout::type_key`, so builtin and user keys never collide
/// by construction.
#[must_use]
pub fn type_key(module_path: &str, type_name: &str) -> u64 {
    let mut h = SipHasher13::new_with_keys(0, 0);
    h.write(module_path.as_bytes());
    h.write(&[0xff]);
    h.write(type_name.as_bytes());
    h.finish()
}

impl LayoutInfo {
    /// A constructor layout whose fields are given in declaration order;
    /// value fields are laid out first, then raw fields, each in
    /// declaration order.
    ///
    /// `name` is the constructor's full name: for generated code's layouts
    /// `LayoutDesc::name`, the constructor's path `module::Ctor`, which the
    /// JIT and an executable's start pass alike (D-920); the type and
    /// constructor names are split at its last `.` (the whole name for a
    /// path), `record` is whether any field has a name, and the type key is
    /// that of a builtin type with the type's name. Generated code's
    /// layouts set the real values with [`LayoutInfo::named`].
    #[must_use]
    pub fn ctor(name: &str, tag: u32, fields: &[(FieldKind, Option<&str>)]) -> Self {
        let mut value_offset = abi::HEADER_SIZE;
        let value_count = fields
            .iter()
            .filter(|(kind, _)| *kind == FieldKind::Value)
            .count();
        let mut raw_offset =
            abi::HEADER_SIZE.saturating_add(abi::FIELD_SIZE.saturating_mul(to_u64(value_count)));
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
        let (type_name, ctor_name) = name.rsplit_once('.').unwrap_or((name, name));
        Self {
            name: name.into(),
            type_name: type_name.into(),
            ctor_name: ctor_name.into(),
            record: fields.iter().any(|(_, field_name)| field_name.is_some()),
            type_key: type_key("", type_name),
            tag,
            kind: LayoutKind::Ctor {
                tag,
                unit: fields.is_empty(),
            },
            value_fields,
            raw_fields,
            fields: infos,
            hooks: OnceLock::new(),
        }
    }

    /// The same layout with the names and key that MIR's `LayoutDesc`
    /// carries (plan §4.1): the type's name, the constructor's name,
    /// whether it has named fields, and the type key.
    #[must_use]
    pub fn named(self, type_name: &str, ctor_name: &str, record: bool, type_key: u64) -> Self {
        Self {
            type_name: type_name.into(),
            ctor_name: ctor_name.into(),
            record,
            type_key,
            ..self
        }
    }

    /// The layout of a runtime-defined kind, for [`register_builtin`].
    #[must_use]
    pub fn custom(kind: &'static CustomKind) -> Self {
        Self {
            name: kind.name.into(),
            type_name: kind.name.into(),
            ctor_name: kind.name.into(),
            record: false,
            type_key: type_key("", kind.name),
            tag: 0,
            kind: LayoutKind::Custom(kind),
            value_fields: 0,
            raw_fields: 0,
            fields: Box::new([]),
            hooks: OnceLock::new(),
        }
    }

    /// A layout with no field list.
    fn shaped(name: &str, kind: LayoutKind, value_fields: u32, raw_fields: u32) -> Self {
        Self {
            name: name.into(),
            type_name: name.into(),
            ctor_name: name.into(),
            record: false,
            type_key: type_key("", name),
            tag: 0,
            kind,
            value_fields,
            raw_fields,
            fields: Box::new([]),
            hooks: OnceLock::new(),
        }
    }

    /// The type's `@std_hooks` closures (equal, hash, debug), if set.
    #[must_use]
    pub fn hooks(&self) -> Option<[Word; 3]> {
        self.hooks.get().copied()
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
    /// and boxed numbers); `None` for the kinds whose size depends on the
    /// object.
    #[must_use]
    pub fn fixed_size(&self) -> Option<u64> {
        match self.kind {
            LayoutKind::String
            | LayoutKind::Bytes
            | LayoutKind::Closure
            | LayoutKind::Tuple
            | LayoutKind::Dict
            | LayoutKind::Desc
            | LayoutKind::Custom(_) => None,
            LayoutKind::BoxedInt
            | LayoutKind::BoxedU64
            | LayoutKind::BoxedFloat
            | LayoutKind::BoxedF32
            | LayoutKind::Dyn
            | LayoutKind::DescNominal
            | LayoutKind::Ctor { .. } => {
                let fields =
                    u64::from(self.value_fields).checked_add(u64::from(self.raw_fields))?;
                abi::FIELD_SIZE
                    .checked_mul(fields)?
                    .checked_add(abi::HEADER_SIZE)
            }
        }
    }

    /// Checks that `self` is a well-formed constructor layout, as
    /// [`register_consecutive`] does before it registers anything; the
    /// program image's installation checks every record's layouts with it
    /// first (D-920).
    pub(crate) fn validate(&self) -> Result<(), LayoutError> {
        let LayoutKind::Ctor { unit, tag } = self.kind else {
            return Err(LayoutError::Invalid(
                "only constructor layouts are registered as user layouts".into(),
            ));
        };
        if tag != self.tag {
            return Err(LayoutError::Invalid(format!(
                "`{}` has two different tags",
                self.name
            )));
        }
        let total = u64::from(self.value_fields).saturating_add(u64::from(self.raw_fields));
        if to_u64(self.fields.len()) != total {
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
            let index = to_usize(offset.saturating_sub(abi::HEADER_SIZE) / abi::FIELD_SIZE);
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

/// The number of runtime-defined ids, 26-63.
const CUSTOM_COUNT: usize = 38;
const _: () = assert!(
    layout::BUILTIN_RESERVED_END - layout::CUSTOM_FIRST + 1 == 38,
    "CUSTOM_COUNT matches the reserved range"
);

/// The runtime-defined kinds, ids 26-63, each set once.
static CUSTOM: [Slot; CUSTOM_COUNT] = [const { AtomicPtr::new(ptr::null_mut()) }; CUSTOM_COUNT];

/// The next user id to hand out. Every registration is serialized by this
/// lock.
static NEXT_ID: Mutex<LayoutId> = Mutex::new(layout::FIRST_USER);

static BUILTINS: OnceLock<Box<[Option<LayoutInfo>]>> = OnceLock::new();

/// A builtin constructor layout: no module path, the type's key.
fn builtin_ctor(type_name: &str, ctor_name: &str, tag: u32, fields: usize) -> LayoutInfo {
    let field_list = vec![(FieldKind::Value, None); fields];
    LayoutInfo::ctor(&format!("{type_name}.{ctor_name}"), tag, &field_list).named(
        type_name,
        ctor_name,
        false,
        type_key("", type_name),
    )
}

#[inline]
fn builtins() -> &'static [Option<LayoutInfo>] {
    BUILTINS.get_or_init(|| {
        let mut table: Vec<Option<LayoutInfo>> =
            vec![None; to_usize(u64::from(layout::LIST_FIRST))];
        let mut set = |id: LayoutId, info: LayoutInfo| {
            if let Some(slot) = table.get_mut(to_usize(u64::from(id))) {
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
        set(
            layout::DICT,
            LayoutInfo::shaped("dictionary", LayoutKind::Dict, 0, 0),
        );
        set(
            layout::DYN,
            LayoutInfo::shaped("dyn", LayoutKind::Dyn, 2, 0),
        );
        set(
            layout::DESC,
            LayoutInfo::shaped("descriptor", LayoutKind::Desc, 0, 0),
        );
        set(
            layout::DESC_NOMINAL,
            LayoutInfo::shaped("nominal descriptor", LayoutKind::DescNominal, 2, 0),
        );
        set(layout::NIL, builtin_ctor("Nil", "Nil", 0, 0));
        set(layout::FALSE, builtin_ctor("Bool", "False", 0, 0));
        set(layout::TRUE, builtin_ctor("Bool", "True", 1, 0));
        set(layout::LESS, builtin_ctor("Order", "Less", 0, 0));
        set(layout::EQUAL, builtin_ctor("Order", "Equal", 1, 0));
        set(layout::GREATER, builtin_ctor("Order", "Greater", 2, 0));
        set(layout::SOME, builtin_ctor("Option", "Some", 0, 1));
        set(layout::NONE, builtin_ctor("Option", "None", 1, 0));
        set(layout::OK, builtin_ctor("Result", "Ok", 0, 1));
        set(layout::ERR, builtin_ctor("Result", "Err", 1, 1));
        // The handle kinds 40 to 44 (D-703) are registered with the
        // builtins, so the first lookup of any builtin layout makes them
        // known too; the code that builds them registers them as well
        // before it allocates one. Registering takes the registration
        // lock, never this table.
        crate::handles::register();
        for (id, info) in table.iter().enumerate() {
            if let (Some(info), Ok(id)) = (info, LayoutId::try_from(id)) {
                set_fast_word(id, info);
            }
        }
        table.into_boxed_slice()
    })
}

// The free-shape table (D-907) ------------------------------------------

/// The ids the free-shape table covers: the builtins, the runtime-defined
/// kinds and the first 16,320 user layouts. Objects of higher ids are
/// freed through [`get`].
pub(crate) const FAST_IDS: usize = 1 << 14;

/// For each id below [`FAST_IDS`], a packed word that says where an
/// object of the layout keeps its references and how large it is, read
/// without a pointer chase through the table, or 0 when the id has no
/// such word (not registered yet, or a kind that declares no shape): the
/// free path then takes [`get`]. A word is written when its layout is
/// registered, from the [`LayoutInfo`] alone, and never changes; the
/// builtins' words when the layout table is first read, at the latest by
/// the first free that takes [`get`]. Every word is
/// self-contained, so a relaxed load is enough: a reader that sees 0 for
/// an id registered meanwhile takes the slow path, which is just as
/// correct. 128 KiB of zeroed static memory, of which only the pages of
/// used ids are touched.
static FAST: [AtomicU64; FAST_IDS] = [const { AtomicU64::new(0) }; FAST_IDS];

/// The modes of a free-shape word (its low 3 bits) and its flag.
pub(crate) mod fast {
    /// A fixed size (bits 8-31) and `count` (bits 48-63) value fields
    /// from offset 8: constructors, boxed numbers, `dyn` values, nominal
    /// descriptors and kinds of [`super::KindShape::Fixed`].
    pub(crate) const FIXED: u64 = 1;
    /// A uniform object whose header's scan count is its field count from
    /// offset 8: tuples, closures, dictionaries and descriptors. The
    /// escaped form ([`crate::abi::SCAN_ESCAPE`]) takes the slow path.
    pub(crate) const SCAN: u64 = 2;
    /// A raw meta word at offset 8 whose low half counts the value fields
    /// from offset 16; the size is fixed (bits 8-31) or, when that is 0,
    /// 16 plus 8 times the meta word's high half
    /// ([`super::KindShape::Counted`]).
    pub(crate) const COUNTED: u64 = 3;
    /// A `String` or `Bytes`, sized by its capacity, with no references.
    pub(crate) const STRING: u64 = 4;
    /// The mode bits.
    pub(crate) const MODE: u64 = 7;
    /// Set for a constructor or tuple layout, whose block a reuse may
    /// keep (D-716).
    pub(crate) const REUSABLE: u64 = 8;
}

/// The free-shape word of `id`, or 0.
#[expect(
    clippy::inline_always,
    reason = "read on every free; the free paths' gain depends on it being inline (D-907)"
)]
#[inline(always)]
pub(crate) fn fast_word(id: LayoutId) -> u64 {
    match FAST.get(to_usize(u64::from(id))) {
        Some(word) => word.load(Ordering::Relaxed),
        None => 0,
    }
}

/// Records the free-shape word of `id`, a layout being registered.
fn set_fast_word(id: LayoutId, info: &LayoutInfo) {
    if let Some(slot) = FAST.get(to_usize(u64::from(id))) {
        slot.store(derive_fast_word(info), Ordering::Release);
    }
}

/// A word of mode [`fast::FIXED`], or 0 when the size or the count does
/// not fit its bits.
fn fast_fixed(size: u64, count: u64, reusable: bool) -> u64 {
    if size >= 1 << 24 || count >= 1 << 16 {
        return 0;
    }
    let flag = if reusable { fast::REUSABLE } else { 0 };
    fast::FIXED | (size << 8) | (count << 48) | flag
}

/// The free-shape word of a layout, or 0 when it has none.
fn derive_fast_word(info: &LayoutInfo) -> u64 {
    match info.kind {
        LayoutKind::String | LayoutKind::Bytes => fast::STRING,
        LayoutKind::Tuple => fast::SCAN | fast::REUSABLE,
        LayoutKind::Closure | LayoutKind::Dict | LayoutKind::Desc => fast::SCAN,
        LayoutKind::Ctor { .. } => info.fixed_size().map_or(0, |size| {
            fast_fixed(size, u64::from(info.value_fields), true)
        }),
        LayoutKind::BoxedInt
        | LayoutKind::BoxedU64
        | LayoutKind::BoxedFloat
        | LayoutKind::BoxedF32
        | LayoutKind::Dyn
        | LayoutKind::DescNominal => info.fixed_size().map_or(0, |size| {
            fast_fixed(size, u64::from(info.value_fields), false)
        }),
        // A finalizer must run, so such a kind is never freed from its
        // shape; `register_builtin` refuses the combination anyway.
        LayoutKind::Custom(kind) if kind.finalize.is_some() || kind.handle => 0,
        LayoutKind::Custom(kind) => match kind.shape {
            KindShape::Fixed { size, fields } => fast_fixed(to_u64(size), to_u64(fields), false),
            KindShape::Counted { size: None } => fast::COUNTED,
            KindShape::Counted { size: Some(size) } if size < 1 << 24 => {
                fast::COUNTED | (to_u64(size) << 8)
            }
            // No shape, or a fixed size too large for the word's bits.
            KindShape::Opaque | KindShape::Counted { size: Some(_) } => 0,
        },
    }
}

/// Splits a user id into its chunk and slot indices.
fn split(id: LayoutId) -> (usize, usize) {
    let id = to_usize(u64::from(id));
    (id >> CHUNK_BITS, id & (CHUNK_SIZE - 1))
}

/// The slot of a runtime-defined id, or `None` outside 26-63.
fn custom_slot(id: LayoutId) -> Option<&'static Slot> {
    let index = id.checked_sub(layout::CUSTOM_FIRST)?;
    CUSTOM.get(to_usize(u64::from(index)))
}

fn lock_next() -> std::sync::MutexGuard<'static, LayoutId> {
    NEXT_ID.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Stores `info` at user id `id`, which must not be taken yet.
fn store_user(id: LayoutId, info: LayoutInfo) -> Result<(), LayoutError> {
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
    set_fast_word(id, &info);
    slot.store(Box::into_raw(Box::new(info)), Ordering::Release);
    Ok(())
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
    register_consecutive(vec![info])
}

/// Registers the constructor layouts `infos` with consecutive ids, in
/// order, under one lock, and returns the id of the first (D-522: the
/// constructors of one type, in tag order). All or nothing: on an error
/// nothing is registered. An empty list registers nothing and returns the
/// next free id.
///
/// # Errors
///
/// As for [`register`].
pub fn register_consecutive(infos: Vec<LayoutInfo>) -> Result<LayoutId, LayoutError> {
    for info in &infos {
        info.validate()?;
    }
    let mut next = lock_next();
    let first = *next;
    let count = u32::try_from(infos.len()).map_err(|_| LayoutError::TableFull)?;
    let end = first.checked_add(count).ok_or(LayoutError::TableFull)?;
    if end > abi::LAYOUT_MAX.saturating_add(1) {
        return Err(LayoutError::TableFull);
    }
    let mut id = first;
    for info in infos {
        store_user(id, info)?;
        id = id.saturating_add(1);
    }
    *next = end;
    Ok(first)
}

/// Registers a runtime-defined kind at a fixed id in 26-63 (list nodes
/// 26-31, map and set nodes 32-39). Idempotent: registering the same
/// [`CustomKind`] at the same id again does nothing, so modules may call
/// it before every first use.
///
/// # Errors
///
/// [`LayoutError::Invalid`] if `id` is outside 26-63, `info` is not a
/// [`LayoutKind::Custom`] layout, or a different kind holds the id.
pub fn register_builtin(id: LayoutId, info: LayoutInfo) -> Result<(), LayoutError> {
    let LayoutKind::Custom(kind) = info.kind else {
        return Err(LayoutError::Invalid(format!(
            "`{}` is not a runtime-defined kind",
            info.name
        )));
    };
    let Some(slot) = custom_slot(id).filter(|_| id <= layout::BUILTIN_RESERVED_END) else {
        return Err(LayoutError::Invalid(format!(
            "id {id} is not reserved for runtime-defined kinds"
        )));
    };
    if kind.shape != KindShape::Opaque && (kind.finalize.is_some() || kind.handle) {
        return Err(LayoutError::Invalid(format!(
            "`{}` has a finalizer or is a handle, so it cannot declare a shape",
            info.name
        )));
    }
    let _guard = lock_next();
    let current = slot.load(Ordering::Acquire);
    if current.is_null() {
        set_fast_word(id, &info);
        slot.store(Box::into_raw(Box::new(info)), Ordering::Release);
        return Ok(());
    }
    // SAFETY: a non-null slot holds a leaked `Box<LayoutInfo>`, never freed.
    let existing = unsafe { &*current };
    match existing.kind {
        LayoutKind::Custom(other) if ptr::eq(other, kind) => Ok(()),
        _ => Err(LayoutError::Invalid(format!(
            "id {id} already holds `{}`",
            existing.name
        ))),
    }
}

/// The size of an external handle object: the header and the handle.
const EXTERNAL_SIZE: u64 = 16;

unsafe fn external_size(_: Word) -> u64 {
    EXTERNAL_SIZE
}

unsafe fn external_children(_: Word, _: &mut dyn FnMut(Word)) {}

/// The kind of every value of an external type (§12.5.3): the header, then
/// the foreign handle as a raw word at offset 8 (D-590). Each external type has
/// its own layout of this kind ([`register_external`]), whose
/// [`LayoutInfo::type_name`] is the type's name, so a value debug-prints
/// as `<Db>`, and `==` and hashing stop on it like on a function value
/// (D-189, D-233). The generic routines recognize it as a handle
/// ([`LayoutInfo::is_external`]); its own `equal`, `hash` and `debug` are
/// never called.
///
/// Calling the type's drop function when the last reference goes away
/// (D-101, §9.5) is not implemented yet: M1 builds no external values,
/// because a native host library comes with M3 (OQ-P9). The kind's
/// `finalize` slot is where it goes then (D-703).
pub static EXTERNAL_HANDLE: CustomKind = CustomKind {
    name: "external handle",
    size: external_size,
    children: external_children,
    equal: None,
    hash: None,
    debug: None,
    handle: true,
    finalize: None,
    shape: KindShape::Opaque,
};

impl LayoutInfo {
    /// Whether values of this layout are handles: values of an external
    /// type ([`register_external`]) or of one of std's handle kinds 40 to
    /// 44 (D-647, D-703). `==` and hashing stop on them and debug text
    /// prints `<`[`LayoutInfo::type_name`]`>` (D-233).
    #[must_use]
    pub fn is_external(&self) -> bool {
        matches!(self.kind, LayoutKind::Custom(kind) if kind.handle)
    }
}

/// Registers the layout of the values of the external type `type_name`
/// (§12.5.3), whose key is `type_key` ([`type_key`] of its module path and
/// name), at the next user id, and returns the id. The layout is a
/// [`LayoutKind::Custom`] of [`EXTERNAL_HANDLE`].
///
/// # Errors
///
/// [`LayoutError::TableFull`] once all 2^24 ids are taken.
pub fn register_external(type_name: &str, type_key: u64) -> Result<LayoutId, LayoutError> {
    let info = LayoutInfo {
        name: type_name.into(),
        type_name: type_name.into(),
        ctor_name: type_name.into(),
        record: false,
        type_key,
        tag: 0,
        kind: LayoutKind::Custom(&EXTERNAL_HANDLE),
        value_fields: 0,
        raw_fields: 1,
        fields: Box::new([]),
        hooks: OnceLock::new(),
    };
    let mut next = lock_next();
    let id = *next;
    if id > abi::LAYOUT_MAX {
        return Err(LayoutError::TableFull);
    }
    store_user(id, info)?;
    *next = id.saturating_add(1);
    Ok(id)
}

/// Sets the `@std_hooks` closures (equal, hash, debug) of a constructor
/// layout (D-482, W3-B). The table keeps the three words forever, so they
/// must be static closures or references the caller gives up.
///
/// # Errors
///
/// [`LayoutError::Invalid`] if `id` is not a registered layout or its
/// hooks are already set to other words.
pub fn set_hooks(id: LayoutId, hooks: [Word; 3]) -> Result<(), LayoutError> {
    let Some(info) = get(id) else {
        return Err(LayoutError::Invalid(format!("no layout has id {id}")));
    };
    let stored = info.hooks.get_or_init(|| hooks);
    if *stored == hooks {
        Ok(())
    } else {
        Err(LayoutError::Invalid(format!(
            "`{}` already has other hooks",
            info.name
        )))
    }
}

/// The layout with the given id, or `None` for an id that is not a builtin
/// and has not been registered. Takes no lock.
#[must_use]
#[inline]
pub fn get(id: LayoutId) -> Option<&'static LayoutInfo> {
    if id < layout::CUSTOM_FIRST {
        return builtins().get(to_usize(u64::from(id)))?.as_ref();
    }
    let slot = if id < layout::FIRST_USER {
        custom_slot(id)?
    } else {
        let (chunk_index, slot_index) = split(id);
        let chunk = CHUNKS.get(chunk_index)?.load(Ordering::Acquire);
        if chunk.is_null() {
            return None;
        }
        // SAFETY: as in `store_user`: a leaked chunk of `CHUNK_SIZE` slots.
        unsafe { &*chunk.add(slot_index) }
    };
    let info = slot.load(Ordering::Acquire);
    // SAFETY: a non-null slot holds a leaked `Box<LayoutInfo>`, published
    // with release ordering and never freed or replaced.
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
        assert_eq!(
            get(layout::DICT).map(|info| info.kind),
            Some(LayoutKind::Dict)
        );
        assert_eq!(get(layout::DYN).and_then(LayoutInfo::fixed_size), Some(24));
        assert_eq!(
            get(layout::DESC).map(|info| info.kind),
            Some(LayoutKind::Desc)
        );
        assert!(get(13).is_none());
        assert!(get(layout::RUNTIME_RESERVED_END).is_none());
        let some = get(layout::SOME);
        assert_eq!(
            some.map(|info| info.kind),
            Some(LayoutKind::Ctor {
                tag: 0,
                unit: false
            })
        );
        assert_eq!(some.and_then(LayoutInfo::fixed_size), Some(16));
        assert_eq!(some.map(|info| &*info.ctor_name), Some("Some"));
        assert_eq!(some.map(|info| &*info.type_name), Some("Option"));
        assert_eq!(
            get(layout::NONE).map(|info| info.kind),
            Some(LayoutKind::Ctor { tag: 1, unit: true })
        );
        assert_eq!(get(layout::TRUE).map(|info| &*info.name), Some("Bool.True"));
        assert_eq!(get(layout::TRUE).map(|info| info.tag), Some(1));
        assert_eq!(
            get(layout::BOXED_FLOAT).and_then(LayoutInfo::fixed_size),
            Some(16)
        );
        assert_eq!(
            get(layout::OK).map(|info| info.type_key),
            get(layout::ERR).map(|info| info.type_key)
        );
        assert_ne!(
            get(layout::OK).map(|info| info.type_key),
            get(layout::SOME).map(|info| info.type_key)
        );
    }

    #[test]
    fn type_keys_match_mirs_encoding() {
        let mut h = SipHasher13::new_with_keys(0, 0);
        h.write(b"geo");
        h.write(&[0xff]);
        h.write(b"Point");
        assert_eq!(type_key("geo", "Point"), h.finish());
        assert_ne!(type_key("geo", "Point"), type_key("ge", "oPoint"));
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
        assert!(info.record);
        assert_eq!((&*info.type_name, &*info.ctor_name), ("geo", "Point"));
        let named = info.named("Point", "Point", true, 7);
        assert_eq!((&*named.type_name, named.type_key), ("Point", 7));
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
    fn consecutive_registration_is_atomic_across_threads() {
        let threads: Vec<_> = (0..4)
            .map(|n| {
                std::thread::spawn(move || {
                    (0..50)
                        .map(|_| {
                            let infos = (0..3)
                                .map(|tag| LayoutInfo::ctor(&format!("t{n}.C"), tag, &[]))
                                .collect();
                            register_consecutive(infos).unwrap()
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for thread in threads {
            for first in thread.join().unwrap() {
                for tag in 0..3_u32 {
                    let info = get(first.wrapping_add(tag)).unwrap();
                    assert_eq!(info.tag, tag);
                    assert_eq!(get(first).unwrap().name, info.name);
                }
            }
        }
    }

    #[test]
    fn a_failed_consecutive_registration_registers_nothing() {
        let mut bad = LayoutInfo::ctor("t.Bad", 1, &[(FieldKind::Value, None)]);
        bad.value_fields = 2;
        let good = LayoutInfo::ctor("t.Good", 0, &[]);
        assert!(register_consecutive(vec![good, bad]).is_err());
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

    unsafe fn no_size(_: Word) -> u64 {
        8
    }

    unsafe fn no_children(_: Word, _: &mut dyn FnMut(Word)) {}

    static KIND_A: CustomKind = CustomKind {
        name: "test node a",
        size: no_size,
        children: no_children,
        equal: None,
        hash: None,
        debug: None,
        handle: false,
        finalize: None,
        shape: KindShape::Opaque,
    };

    static KIND_B: CustomKind = CustomKind {
        name: "test node b",
        size: no_size,
        children: no_children,
        equal: None,
        hash: None,
        debug: None,
        handle: false,
        finalize: None,
        shape: KindShape::Opaque,
    };

    #[test]
    fn builtin_kinds_register_once_at_their_id() {
        // 63 is reserved and unused by every runtime module.
        let id = layout::BUILTIN_RESERVED_END;
        register_builtin(id, LayoutInfo::custom(&KIND_A)).unwrap();
        register_builtin(id, LayoutInfo::custom(&KIND_A)).unwrap();
        assert!(register_builtin(id, LayoutInfo::custom(&KIND_B)).is_err());
        assert_eq!(
            get(id).map(|info| info.kind),
            Some(LayoutKind::Custom(&KIND_A))
        );
        assert!(register_builtin(layout::FIRST_USER, LayoutInfo::custom(&KIND_B)).is_err());
        assert!(register_builtin(layout::STRING, LayoutInfo::custom(&KIND_B)).is_err());
        let ctor = LayoutInfo::ctor("t.C", 0, &[]);
        assert!(register_builtin(62, ctor).is_err());
    }

    unsafe fn no_finalize(_: Word, _: &mut dyn FnMut(Word)) {}

    static SHAPED_WITH_FINALIZER: CustomKind = CustomKind {
        name: "test node with a finalizer",
        size: no_size,
        children: no_children,
        equal: None,
        hash: None,
        debug: None,
        handle: false,
        finalize: Some(no_finalize),
        shape: KindShape::Fixed { size: 8, fields: 0 },
    };

    static SHAPED_HANDLE: CustomKind = CustomKind {
        name: "test handle",
        size: no_size,
        children: no_children,
        equal: None,
        hash: None,
        debug: None,
        handle: true,
        finalize: None,
        shape: KindShape::Fixed { size: 8, fields: 0 },
    };

    /// A finalizer must run and a handle is compared as one, so neither
    /// kind may be freed from a declared shape (D-907); the refusal comes
    /// before the id is taken.
    #[test]
    fn kinds_with_a_finalizer_or_handles_declare_no_shape() {
        for kind in [&SHAPED_WITH_FINALIZER, &SHAPED_HANDLE] {
            let error = register_builtin(62, LayoutInfo::custom(kind)).unwrap_err();
            assert!(
                error.to_string().contains("cannot declare a shape"),
                "{error}"
            );
        }
        assert!(get(62).is_none());
        assert_eq!(fast_word(62), 0);
    }

    /// The words of the shapes a kind may declare, and the opaque one.
    #[test]
    fn declared_shapes_make_their_words() {
        let word = |shape| {
            let kind = CustomKind { shape, ..KIND_A };
            derive_fast_word(&LayoutInfo::custom(Box::leak(Box::new(kind))))
        };
        assert_eq!(word(KindShape::Opaque), 0);
        assert_eq!(
            word(KindShape::Fixed {
                size: 32,
                fields: 1
            }),
            fast::FIXED | (32 << 8) | (1 << 48)
        );
        assert_eq!(word(KindShape::Counted { size: None }), fast::COUNTED);
        assert_eq!(
            word(KindShape::Counted { size: Some(528) }),
            fast::COUNTED | (528 << 8)
        );
        assert_eq!(
            word(KindShape::Counted {
                size: Some(1 << 24)
            }),
            0
        );
        assert_eq!(
            word(KindShape::Fixed {
                size: 8,
                fields: 1 << 16
            }),
            0
        );
    }

    #[test]
    fn hooks_are_set_once() {
        let id = register(LayoutInfo::ctor("t.Hooked", 0, &[])).unwrap();
        assert_eq!(get(id).and_then(LayoutInfo::hooks), None);
        set_hooks(id, [8, 16, 24]).unwrap();
        set_hooks(id, [8, 16, 24]).unwrap();
        assert!(set_hooks(id, [8, 16, 32]).is_err());
        assert_eq!(get(id).and_then(LayoutInfo::hooks), Some([8, 16, 24]));
        assert!(set_hooks(abi::LAYOUT_MAX, [0; 3]).is_err());
    }
}
