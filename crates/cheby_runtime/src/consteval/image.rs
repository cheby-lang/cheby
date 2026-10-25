//! Constant images (D-979): an evaluated constant's value written as a
//! description of values, not of memory, and the one routine that installs
//! images before `main` in every host: `cheby run`, `cheby test`, the
//! REPL and executables.
//!
//! # The bytes (GAP(const-image-format))
//!
//! An image is `"CHBV"`, the format version [`IMAGE_FORMAT`] (one byte),
//! the constant's path `module::name` (a string), the object count (a
//! number), the root (a reference), then the objects. Numbers are unsigned
//! LEB128; a string is its byte length (a number) then its UTF-8 bytes;
//! a `u64` is 8 bytes little-endian. The objects are those of the value in
//! post-order from the root, children in field order, each stored once:
//! every reference to an object of the image comes after it.
//!
//! A reference is a tag byte, then:
//!
//! | Tag | Reference                     | Then                                     |
//! | --- | ----------------------------- | ---------------------------------------- |
//! | 0   | an immediate word             | the tagged integer as a `u64`            |
//! | 1   | an object of this image       | its index (a number)                     |
//! | 2   | a runtime static, by name     | its code (one byte), see below           |
//! | 3   | an object of an earlier image | that constant's path, the object's index |
//! | 4   | a static object, by address   | its word as a `u64` (in-process only)    |
//!
//! Tag 4 appears only in an in-process image, which
//! [`serialize_in_process`] writes and only [`install_in_process`] reads
//! (GAP(repl-constant-images)): the REPL's, whose static objects (an
//! earlier entry's installed constants, the session's code data) outlive
//! every slot of the session, so an entry's image refers to them instead
//! of copying them, and sharing survives across entries as D-539 asks.
//! Such an image names addresses: it never leaves its process (no cache,
//! no program object), and [`install`] refuses it.
//!
//! The runtime statics (D-454, D-902): 0 `Nil`, 1 `False`, 2 `True`, 3
//! `Less`, 4 `Equal`, 5 `Greater`, 6 `None`, 7 the empty list, and 8 the
//! static `Some(n)` of D-902's table, followed by `n` as an `i16`
//! little-endian (from -128 to 255).
//!
//! An object is a kind byte, then its parts:
//!
//! | Kind | Value                       | Parts                                                        |
//! | ---- | --------------------------- | ------------------------------------------------------------ |
//! | 1    | `String`                    | its bytes (a string)                                         |
//! | 2    | `Bytes`                     | its length (a number), its bytes                             |
//! | 3-6  | boxed `Int`, `Float`, `F32`, `U64` | its payload's bits as a `u64`                         |
//! | 7    | tuple                       | the arity (a number), the elements' references               |
//! | 8-10 | `Some`, `Ok`, `Err`         | the field's reference                                        |
//! | 11   | user constructor            | type key (`u64`, D-559), constructor index (a number), field count (a number), then each field in declaration order: its kind (one byte, an index into `FIELD_KINDS`), then a reference for a uniform field or the raw bits as a `u64` |
//! | 12   | `List`                      | the length (a number), the elements' references, in order     |
//! | 13   | `Map`                       | the entry count (a number), each key's and value's reference, in the evaluation's iteration order |
//! | 14   | `Set`                       | the element count (a number), each element's reference, in the evaluation's iteration order |
//!
//! An image holds no address, layout id or seed, so two evaluations of the
//! same constant give the same bytes in any process (D-264). An object that
//! an earlier constant of the same [`serialize`] call holds is stored only
//! in that constant's image and referenced from the later ones by (path,
//! index), so sharing survives across constants (D-539). Results hold no
//! closures or handles (E0068); a value that holds one is refused.
//!
//! # Installing
//!
//! [`install`] checks every image first: the bytes, the references, each
//! user constructor's type key against the layouts the batch's module
//! records registered ([`TypeLayouts`], D-920, D-1169) with its fields
//! against the registered layout's, and a slot for every path. That pass
//! keeps nothing per object. Only then does a second pass over the same
//! bytes build the objects in order (D-535), each straight from its
//! bytes, so installing costs the values it builds and a word per object
//! of the image, not a decoded copy of every element: each one becomes static,
//! count 0 and out of the leak check (D-454, D-539, D-564); a nullary user
//! constructor is its registered `cb$ctor$` static; lists are built from
//! their elements, maps and sets from their entries with the process seed
//! (D-196); and each root is written into its slot. Run it on an entry
//! with the program's trampolines: hashing a key may call a type's
//! `@std_hooks` closure.
//!
//! The program image's `constants` table ([`encode_table`]) is `"CHBT"`,
//! the image count (a number), then each image's length (a number) and
//! bytes, in D-535's order; a program without constants has an empty table.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::abi::{self, LayoutId, Word, layout as ids};
use crate::image::FIELD_KINDS;
use crate::layout::{self, FieldKind, LayoutInfo, LayoutKind};
use crate::object::{self, load_rc, read_field, scan_and_layout, word_to_ptr, write_field};
use crate::{alloc, generic, hamt, list, values};

/// The image format's version, the byte after `"CHBV"`.
pub const IMAGE_FORMAT: u8 = 1;

/// The magic bytes of an image.
const MAGIC: &[u8; 4] = b"CHBV";

/// The magic bytes of the program image's `constants` table.
const TABLE_MAGIC: &[u8; 4] = b"CHBT";

/// One constant's image (D-979): the objects of its value in post-order
/// from the root, children in field order; references to this image's
/// objects by index, to runtime statics by name, and to an earlier image's
/// objects by (that constant's path, object index); user constructors by
/// type key (D-559) and constructor index. No address, layout id or seed.
/// Versioned bytes after "CHBV" (module documentation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstImage {
    /// The constant's path, `module::name`, which names its slot.
    pub path: Box<str>,
    /// The image's bytes, its path among them.
    pub bytes: Vec<u8>,
}

/// Why an image cannot be written, read or installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConstImageError {
    /// The bytes of the image of `path` are not an image of this format.
    Malformed {
        /// The constant's path, or the table's position for a table.
        path: Box<str>,
        /// What is wrong.
        what: &'static str,
    },
    /// The value of `path` holds something no image describes (a closure
    /// or a handle, which E0068 keeps out of constants).
    Unsupported {
        /// The constant's path.
        path: Box<str>,
        /// What it holds.
        what: String,
    },
    /// A user constructor whose type key and index no registered layout
    /// has, or whose fields are not the registered layout's.
    UnknownTypeKey {
        /// The constant's path.
        path: Box<str>,
        /// The type key (D-559).
        type_key: u64,
        /// The constructor's index.
        ctor: u32,
    },
    /// No slot for the constant `path`.
    NoSlot {
        /// The constant's path.
        path: Box<str>,
    },
}

impl fmt::Display for ConstImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { path, what } => {
                write!(f, "the constant image of `{path}` is malformed: {what}")
            }
            Self::Unsupported { path, what } => write!(
                f,
                "the value of the constant `{path}` holds {what}, which no constant image describes"
            ),
            Self::UnknownTypeKey {
                path,
                type_key,
                ctor,
            } => write!(
                f,
                "the constant image of `{path}` names constructor {ctor} of the type key \
                 {type_key:#018x}, which no registered layout matches"
            ),
            Self::NoSlot { path } => write!(f, "the constant `{path}` has no slot"),
        }
    }
}

impl std::error::Error for ConstImageError {}

/// A runtime static named by an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Static {
    Nil,
    False,
    True,
    Less,
    Equal,
    Greater,
    None,
    EmptyList,
    /// D-902's static `Some(n)`, `n` from -128 to 255.
    SomeSmall(i16),
}

impl Static {
    /// The static's code in an image (module documentation).
    const fn code(self) -> u8 {
        match self {
            Self::Nil => 0,
            Self::False => 1,
            Self::True => 2,
            Self::Less => 3,
            Self::Equal => 4,
            Self::Greater => 5,
            Self::None => 6,
            Self::EmptyList => 7,
            Self::SomeSmall(_) => 8,
        }
    }

    /// The runtime's word of the static.
    fn word(self) -> Option<Word> {
        Some(match self {
            Self::Nil => values::nil(),
            Self::False => values::bool_word(false),
            Self::True => values::bool_word(true),
            Self::Less => values::order(std::cmp::Ordering::Less),
            Self::Equal => values::order(std::cmp::Ordering::Equal),
            Self::Greater => values::order(std::cmp::Ordering::Greater),
            Self::None => values::none(),
            Self::EmptyList => empty_list(),
            Self::SomeSmall(n) => return object::static_some_small(i64::from(n)),
        })
    }

    /// The static an object word is, if any: the builtin nullary
    /// constructors by layout id, the empty list by length, and D-902's
    /// `Some` table by address.
    ///
    /// # Safety
    ///
    /// `word` is a live object.
    unsafe fn of(word: Word, id: LayoutId) -> Option<Self> {
        match id {
            ids::NIL => Some(Self::Nil),
            ids::FALSE => Some(Self::False),
            ids::TRUE => Some(Self::True),
            ids::LESS => Some(Self::Less),
            ids::EQUAL => Some(Self::Equal),
            ids::GREATER => Some(Self::Greater),
            ids::NONE => Some(Self::None),
            // SAFETY: a live list root.
            ids::LIST_FIRST if unsafe { list::iter(word) }.next().is_none() => {
                Some(Self::EmptyList)
            }
            ids::SOME => {
                // SAFETY: a live `Some`, whose field is at offset 8.
                let field = unsafe { read_field(word_to_ptr(word), FIELD0) };
                if !abi::is_tagged_int(field) {
                    return None;
                }
                let n = i16::try_from(abi::untag_int(field)).ok()?;
                (object::static_some_small(i64::from(n)) == Some(word))
                    .then_some(Self::SomeSmall(n))
            }
            _ => None,
        }
    }
}

/// The word of the static empty list.
fn empty_list() -> Word {
    object::ptr_to_word(std::ptr::from_ref(&list::cheby_rt_static_empty_list).cast::<u8>())
}

/// The offset of a box's payload and of a one-field constructor's field.
const FIELD0: usize = 8;

/// A reference while writing: an object by (image, index).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ref {
    Imm(Word),
    Local(u32),
    Static(Static),
    /// An object of an earlier image: that image's position, the index.
    Extern(usize, u32),
    /// A static object of this process, by address (in-process images).
    Address(Word),
}

/// The kinds of objects (module documentation).
mod kind {
    pub(super) const STRING: u8 = 1;
    pub(super) const BYTES: u8 = 2;
    pub(super) const BOXED_INT: u8 = 3;
    pub(super) const BOXED_FLOAT: u8 = 4;
    pub(super) const BOXED_F32: u8 = 5;
    pub(super) const BOXED_U64: u8 = 6;
    pub(super) const TUPLE: u8 = 7;
    pub(super) const SOME: u8 = 8;
    pub(super) const OK: u8 = 9;
    pub(super) const ERR: u8 = 10;
    pub(super) const USER: u8 = 11;
    pub(super) const LIST: u8 = 12;
    pub(super) const MAP: u8 = 13;
    pub(super) const SET: u8 = 14;
}

/// The tags of references (module documentation).
mod tag {
    pub(super) const IMM: u8 = 0;
    pub(super) const LOCAL: u8 = 1;
    pub(super) const STATIC: u8 = 2;
    pub(super) const EXTERN: u8 = 3;
    pub(super) const ADDRESS: u8 = 4;
}

/// One object of a value, read for writing: its kind and its children in
/// field order.
enum Shape {
    String,
    Bytes,
    Boxed(u8),
    Tuple,
    Builtin(u8),
    User {
        type_key: u64,
        tag: u32,
        /// Each field in declaration order: its kind and, for a raw field,
        /// its bits (a uniform field's word is among the children).
        fields: Vec<(FieldKind, u64)>,
    },
    List,
    Map,
    Set,
}

/// What [`serialize`] keeps of one object while its children are written:
/// its shape, its children in field order, and how many of them it has
/// visited.
struct Frame {
    word: Word,
    shape: Shape,
    children: Vec<Word>,
    next: usize,
}

impl Frame {
    /// The frame of the object `word`, before its children.
    ///
    /// # Safety
    ///
    /// As for [`shape_of`].
    unsafe fn of(path: &str, word: Word) -> Result<Self, ConstImageError> {
        // SAFETY: forwarded from the caller.
        let (shape, children) = unsafe { shape_of(path, word) }?;
        Ok(Self {
            word,
            shape,
            children,
            next: 0,
        })
    }
}

/// Serializes the evaluated constants `roots` (each with its path, in
/// D-535's order) into one image each, an object two of them share stored
/// once, in the first one's image (D-539).
///
/// # Errors
///
/// [`ConstImageError::Unsupported`] for a value that holds a closure, a
/// handle or another object no image describes.
///
/// # Safety
///
/// Every word is a live value of the evaluation's heap, which stays alive
/// during the call.
pub unsafe fn serialize(roots: &[(&str, Word)]) -> Result<Vec<ConstImage>, ConstImageError> {
    // SAFETY: forwarded from the caller.
    unsafe { serialize_with(roots, false) }
}

/// [`serialize`] for an image that stays in this process (the REPL's,
/// GAP(repl-constant-images)): a static object that is no runtime static
/// (an earlier entry's installed constant, a literal of the session's
/// code) is written by its address (tag 4) instead of being copied, so
/// an entry that names a large earlier constant costs a word, and the
/// installed value shares it (D-539). Only [`install_in_process`] reads
/// such an image; it is never stored or written into a program.
///
/// # Errors
///
/// As for [`serialize`].
///
/// # Safety
///
/// As for [`serialize`], and every static object the values reach lives
/// as long as any slot the images will be installed into.
pub unsafe fn serialize_in_process(
    roots: &[(&str, Word)],
) -> Result<Vec<ConstImage>, ConstImageError> {
    // SAFETY: forwarded from the caller.
    unsafe { serialize_with(roots, true) }
}

/// [`serialize`], with static objects by address when `in_process`.
///
/// # Safety
///
/// As for [`serialize_in_process`].
unsafe fn serialize_with(
    roots: &[(&str, Word)],
    in_process: bool,
) -> Result<Vec<ConstImage>, ConstImageError> {
    let mut seen: HashMap<Word, (usize, u32)> = HashMap::new();
    let mut out = Vec::with_capacity(roots.len());
    for (image, &(path, root)) in roots.iter().enumerate() {
        let mut objects: Vec<u8> = Vec::new();
        let mut count: u32 = 0;
        // A depth-first walk in post-order with an explicit stack: each
        // frame is an object whose children are written up to its cursor.
        // A child that is a leaf or already written takes no frame, so a
        // list of a million integers costs its children's words, not a
        // frame each (the wave-6 review's R-1).
        let mut stack: Vec<Frame> = Vec::new();
        // SAFETY: the root, live.
        if unsafe { leaf_ref(root, in_process) }.is_none() && !seen.contains_key(&root) {
            // SAFETY: as above, an object.
            stack.push(unsafe { Frame::of(path, root) }?);
        }
        while let Some(top) = stack.last_mut() {
            if let Some(&child) = top.children.get(top.next) {
                top.next = top.next.saturating_add(1);
                // SAFETY: a child of a live object, itself live; no
                // object reaches itself, so it is on no frame of the stack.
                if unsafe { leaf_ref(child, in_process) }.is_none() && !seen.contains_key(&child) {
                    // SAFETY: as above, an object.
                    let frame = unsafe { Frame::of(path, child) }?;
                    stack.push(frame);
                }
                continue;
            }
            let Some(done) = stack.pop() else {
                break;
            };
            let reference = |child: Word| -> Result<Ref, ConstImageError> {
                // SAFETY: a child of a live object.
                if let Some(r) = unsafe { leaf_ref(child, in_process) } {
                    return Ok(r);
                }
                match seen.get(&child) {
                    Some(&(owner, index)) if owner == image => Ok(Ref::Local(index)),
                    Some(&(owner, index)) => Ok(Ref::Extern(owner, index)),
                    None => Err(malformed(path, "a child written after its parent")),
                }
            };
            // SAFETY: `done.word` is the live object its shape was read
            // from.
            unsafe { write_object(&mut objects, path, &done, &reference, roots) }?;
            seen.insert(done.word, (image, count));
            count = count
                .checked_add(1)
                .ok_or_else(|| malformed(path, "more than 2^32 objects"))?;
        }
        // SAFETY: the root, live.
        let root_ref = match unsafe { leaf_ref(root, in_process) } {
            Some(r) => r,
            None => match seen.get(&root) {
                Some(&(owner, index)) if owner == image => Ref::Local(index),
                Some(&(owner, index)) => Ref::Extern(owner, index),
                None => return Err(malformed(path, "a root that was not written")),
            },
        };
        let mut bytes = Vec::with_capacity(objects.len().saturating_add(32));
        bytes.extend_from_slice(MAGIC);
        bytes.push(IMAGE_FORMAT);
        put_bytes(&mut bytes, path.as_bytes());
        put_number(&mut bytes, u64::from(count));
        put_ref(&mut bytes, root_ref, roots);
        bytes.extend_from_slice(&objects);
        out.push(ConstImage {
            path: path.into(),
            bytes,
        });
    }
    Ok(out)
}

/// The reference of a word that is no object of an image: a tagged
/// integer, a runtime static, or, `in_process`, any other static object
/// by its address.
///
/// # Safety
///
/// `word` is a tagged integer or a live object.
unsafe fn leaf_ref(word: Word, in_process: bool) -> Option<Ref> {
    if abi::is_tagged_int(word) {
        return Some(Ref::Imm(word));
    }
    if word == 0 {
        return None;
    }
    // SAFETY: forwarded from the caller.
    unsafe { alloc::check_live(word, "writing a constant image") };
    // SAFETY: as above.
    let (_, id) = unsafe { scan_and_layout(word_to_ptr(word)) };
    // SAFETY: as above.
    if let Some(s) = unsafe { Static::of(word, id) } {
        return Some(Ref::Static(s));
    }
    // SAFETY: as above.
    (in_process && unsafe { is_static(word) }).then_some(Ref::Address(word))
}

/// The shape and the children of the object `word`.
///
/// # Safety
///
/// `word` is a live object that is no runtime static.
unsafe fn shape_of(path: &str, word: Word) -> Result<(Shape, Vec<Word>), ConstImageError> {
    let unsupported = |what: String| ConstImageError::Unsupported {
        path: path.into(),
        what,
    };
    if word == 0 {
        return Err(malformed(path, "a null word"));
    }
    let obj = word_to_ptr(word);
    // SAFETY: forwarded from the caller.
    let (_, id) = unsafe { scan_and_layout(obj) };
    Ok(match id {
        ids::STRING => (Shape::String, Vec::new()),
        ids::BYTES => (Shape::Bytes, Vec::new()),
        ids::BOXED_INT => (Shape::Boxed(kind::BOXED_INT), Vec::new()),
        ids::BOXED_FLOAT => (Shape::Boxed(kind::BOXED_FLOAT), Vec::new()),
        ids::BOXED_F32 => (Shape::Boxed(kind::BOXED_F32), Vec::new()),
        ids::BOXED_U64 => (Shape::Boxed(kind::BOXED_U64), Vec::new()),
        // SAFETY: a live tuple.
        ids::TUPLE => (Shape::Tuple, unsafe { values::read_tuple(word) }),
        ids::SOME | ids::OK | ids::ERR => {
            let k = match id {
                ids::SOME => kind::SOME,
                ids::OK => kind::OK,
                _ => kind::ERR,
            };
            // SAFETY: a live one-field constructor.
            (Shape::Builtin(k), vec![unsafe { read_field(obj, FIELD0) }])
        }
        // SAFETY: a live list root; the elements stay alive with it.
        ids::LIST_FIRST => (Shape::List, unsafe { list::iter(word) }.collect()),
        _ => {
            // SAFETY: a live object.
            match unsafe { hamt::collection_of(word) } {
                Some(hamt::Collection::Map) => {
                    let mut children = Vec::new();
                    // SAFETY: a live map, unchanged during the walk.
                    unsafe {
                        hamt::for_each_entry(word, &mut |k, v| {
                            children.push(k);
                            children.push(v.unwrap_or(0));
                        });
                    }
                    return Ok((Shape::Map, children));
                }
                Some(hamt::Collection::Set) => {
                    let mut children = Vec::new();
                    // SAFETY: a live set, unchanged during the walk.
                    unsafe { hamt::for_each_entry(word, &mut |k, _| children.push(k)) };
                    return Ok((Shape::Set, children));
                }
                None => {}
            }
            let Some(info) = layout::get(id) else {
                return Err(unsupported(format!("an object of the unknown layout {id}")));
            };
            let LayoutKind::Ctor { tag, .. } = info.kind else {
                return Err(unsupported(format!("a `{}`", info.name)));
            };
            if id < ids::FIRST_USER || info.is_external() {
                return Err(unsupported(format!("a `{}`", info.name)));
            }
            let mut fields = Vec::with_capacity(info.fields.len());
            let mut children = Vec::new();
            for field in &info.fields {
                let offset = usize::try_from(field.offset).unwrap_or(usize::MAX);
                // SAFETY: the layout's field lies within the object.
                let raw = unsafe { read_field(obj, offset) };
                if field.kind == FieldKind::Value {
                    children.push(raw);
                    fields.push((field.kind, 0));
                } else {
                    fields.push((field.kind, raw));
                }
            }
            (
                Shape::User {
                    type_key: info.type_key,
                    tag,
                    fields,
                },
                children,
            )
        }
    })
}

/// Writes the object of `frame`, each child's reference given by
/// `reference`, in field order, without collecting them.
///
/// # Safety
///
/// `frame.word` is the live object its shape was read from.
unsafe fn write_object(
    out: &mut Vec<u8>,
    path: &str,
    frame: &Frame,
    reference: &dyn Fn(Word) -> Result<Ref, ConstImageError>,
    roots: &[(&str, Word)],
) -> Result<(), ConstImageError> {
    let word = frame.word;
    let shape = &frame.shape;
    let children = &frame.children;
    let put_all = |out: &mut Vec<u8>| -> Result<(), ConstImageError> {
        for &child in children {
            put_ref(out, reference(child)?, roots);
        }
        Ok(())
    };
    let obj = word_to_ptr(word);
    match shape {
        Shape::String => {
            out.push(kind::STRING);
            // SAFETY: a live `String`.
            put_bytes(out, unsafe { values::bytes_of(word) });
        }
        Shape::Bytes => {
            out.push(kind::BYTES);
            // SAFETY: a live `Bytes`.
            put_bytes(out, unsafe { values::bytes_of(word) });
        }
        Shape::Boxed(k) => {
            out.push(*k);
            // SAFETY: a live box, whose payload is at offset 8.
            put_u64(out, unsafe { read_field(obj, FIELD0) });
        }
        Shape::Tuple => {
            out.push(kind::TUPLE);
            put_number(out, to_u64(children.len()));
            put_all(out)?;
        }
        Shape::Builtin(k) => {
            out.push(*k);
            put_all(out)?;
        }
        Shape::User {
            type_key,
            tag,
            fields,
        } => {
            out.push(kind::USER);
            put_u64(out, *type_key);
            put_number(out, u64::from(*tag));
            put_number(out, to_u64(fields.len()));
            let mut values = children.iter();
            for &(field_kind, raw) in fields {
                out.push(field_kind_index(field_kind));
                if field_kind == FieldKind::Value {
                    let Some(&child) = values.next() else {
                        return Err(malformed(path, "a constructor whose fields changed"));
                    };
                    put_ref(out, reference(child)?, roots);
                } else {
                    put_u64(out, raw);
                }
            }
        }
        Shape::List | Shape::Set => {
            out.push(if matches!(shape, Shape::List) {
                kind::LIST
            } else {
                kind::SET
            });
            put_number(out, to_u64(children.len()));
            put_all(out)?;
        }
        Shape::Map => {
            out.push(kind::MAP);
            put_number(out, to_u64(children.len().checked_div(2).unwrap_or(0)));
            put_all(out)?;
        }
    }
    Ok(())
}

/// The index of `kind` in [`FIELD_KINDS`].
fn field_kind_index(kind: FieldKind) -> u8 {
    FIELD_KINDS
        .iter()
        .position(|&k| k == kind)
        .and_then(|i| u8::try_from(i).ok())
        .unwrap_or(u8::MAX)
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn malformed(path: &str, what: &'static str) -> ConstImageError {
    ConstImageError::Malformed {
        path: path.into(),
        what,
    }
}

fn put_number(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let low = u8::try_from(n & 0x7f).unwrap_or(0);
        n >>= 7;
        if n == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

fn put_u64(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(&n.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_number(out, to_u64(bytes.len()));
    out.extend_from_slice(bytes);
}

fn put_ref(out: &mut Vec<u8>, r: Ref, roots: &[(&str, Word)]) {
    match r {
        Ref::Imm(word) => {
            out.push(tag::IMM);
            put_u64(out, word);
        }
        Ref::Local(index) => {
            out.push(tag::LOCAL);
            put_number(out, u64::from(index));
        }
        Ref::Static(s) => {
            out.push(tag::STATIC);
            out.push(s.code());
            if let Static::SomeSmall(n) = s {
                out.extend_from_slice(&n.to_le_bytes());
            }
        }
        Ref::Extern(image, index) => {
            out.push(tag::EXTERN);
            let path = roots.get(image).map_or("", |&(path, _)| path);
            put_bytes(out, path.as_bytes());
            put_number(out, u64::from(index));
        }
        Ref::Address(word) => {
            out.push(tag::ADDRESS);
            put_u64(out, word);
        }
    }
}

/// The program image's `constants` table (`ProgramImage::constants`):
/// every image in order (module documentation); empty for no image.
#[must_use]
pub fn encode_table(images: &[ConstImage]) -> Vec<u8> {
    if images.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    out.extend_from_slice(TABLE_MAGIC);
    put_number(&mut out, to_u64(images.len()));
    for image in images {
        put_bytes(&mut out, &image.bytes);
    }
    out
}

/// The images of a `constants` table written by [`encode_table`].
///
/// # Errors
///
/// [`ConstImageError::Malformed`] for bytes that are not such a table, or
/// an image whose header is not an image's.
pub fn decode_table(bytes: &[u8]) -> Result<Vec<ConstImage>, ConstImageError> {
    Ok(table_views(bytes)?
        .into_iter()
        .map(|view| ConstImage {
            path: view.path.into(),
            bytes: view.bytes.to_vec(),
        })
        .collect())
}

/// The number of images of a `constants` table, its headers checked as
/// [`decode_table`] checks them, without copying anything.
///
/// # Errors
///
/// As for [`decode_table`].
pub(crate) fn table_len(bytes: &[u8]) -> Result<usize, ConstImageError> {
    Ok(table_views(bytes)?.len())
}

/// One image, borrowed: a [`ConstImage`]'s, or one in a `constants`
/// table, read in place.
#[derive(Clone, Copy, Debug)]
struct View<'a> {
    path: &'a str,
    bytes: &'a [u8],
}

impl<'a> View<'a> {
    fn of(image: &'a ConstImage) -> Self {
        Self {
            path: &image.path,
            bytes: &image.bytes,
        }
    }
}

/// The images of a `constants` table, in place.
fn table_views(bytes: &[u8]) -> Result<Vec<View<'_>>, ConstImageError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let table = "the constants table";
    let mut reader = Reader::new(table, bytes);
    if reader.take(4)? != TABLE_MAGIC {
        return Err(malformed(table, "not a constants table"));
    }
    let count = reader.number()?;
    let mut out = Vec::new();
    for _ in 0..count {
        let len = reader.len()?;
        let image = reader.take(len)?;
        let path = Reader::new("an image", image).header()?;
        out.push(View { path, bytes: image });
    }
    if !reader.at_end() {
        return Err(malformed(table, "bytes after the last image"));
    }
    Ok(out)
}

/// A reader over an image's bytes.
struct Reader<'a> {
    path: &'a str,
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    const fn new(path: &'a str, bytes: &'a [u8]) -> Self {
        Self { path, bytes, at: 0 }
    }

    fn err(&self, what: &'static str) -> ConstImageError {
        malformed(self.path, what)
    }

    const fn at_end(&self) -> bool {
        self.at >= self.bytes.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ConstImageError> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| self.err("a length past the end"))?;
        let part = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| self.err("bytes past the end"))?;
        self.at = end;
        Ok(part)
    }

    fn byte(&mut self) -> Result<u8, ConstImageError> {
        let [b] = *self.take(1)? else {
            return Err(self.err("a byte past the end"));
        };
        Ok(b)
    }

    fn u64(&mut self) -> Result<u64, ConstImageError> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| self.err("a word past the end"))?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn number(&mut self) -> Result<u64, ConstImageError> {
        let mut n: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            let b = self.byte()?;
            let low = u64::from(b & 0x7f);
            if shift >= 64 || (shift == 63 && low > 1) {
                return Err(self.err("a number of more than 64 bits"));
            }
            n |= low << shift;
            if b & 0x80 == 0 {
                return Ok(n);
            }
            shift = shift.saturating_add(7);
        }
    }

    fn len(&mut self) -> Result<usize, ConstImageError> {
        let n = self.number()?;
        let n = usize::try_from(n).map_err(|_| self.err("a length past the address space"))?;
        if n > self.bytes.len().saturating_sub(self.at) {
            return Err(self.err("a length past the end"));
        }
        Ok(n)
    }

    fn index(&mut self) -> Result<u32, ConstImageError> {
        u32::try_from(self.number()?).map_err(|_| self.err("an index of more than 32 bits"))
    }

    fn text(&mut self) -> Result<&'a str, ConstImageError> {
        let len = self.len()?;
        std::str::from_utf8(self.take(len)?).map_err(|_| self.err("a string that is not UTF-8"))
    }

    /// The magic, the version and the path.
    fn header(&mut self) -> Result<&'a str, ConstImageError> {
        if self.take(4)? != MAGIC {
            return Err(self.err("not a constant image"));
        }
        if self.byte()? != IMAGE_FORMAT {
            return Err(self.err("an image format this runtime does not read"));
        }
        self.text()
    }
}

/// A reference read from an image: an earlier image by position.
#[derive(Clone, Copy, Debug)]
enum ReadRef {
    Imm(Word),
    Local(usize),
    Static(Static),
    Extern(usize, usize),
    /// A static object of this process (in-process images only).
    Address(Word),
}

/// What the first pass of [`install`] keeps of one checked image: where
/// its objects start, how many there are, its root and its slot. Nothing
/// per object.
#[derive(Debug)]
struct Checked<'a> {
    view: View<'a>,
    start: usize,
    count: usize,
    root: ReadRef,
    slot: *mut u64,
}

/// What reading an image refers to: the images checked before it (by
/// path, their object counts), the registered layouts, and whether
/// addresses (tag 4) are allowed.
struct Cx<'a> {
    paths: HashMap<&'a str, usize>,
    counts: Vec<usize>,
    types: &'a TypeLayouts,
    in_process: bool,
}

/// Where the second pass of [`install`] builds: the words of this
/// image's objects built so far, those of the earlier images, and the
/// process seed.
#[derive(Clone, Copy)]
struct Building<'w> {
    words: &'w [Word],
    built: &'w [Vec<Word>],
    seed: u64,
}

impl Building<'_> {
    /// The word of a checked reference.
    fn get(self, r: ReadRef) -> Word {
        resolve(r, self.words, self.built)
    }
}

/// The user layouts a batch registered, by type key (D-559): each type's
/// constructors in tag order, with the layout id that registration
/// assigned and the `cb$ctor$` static of a nullary one (0 otherwise).
/// Built from the batch's module records after registration
/// (`crate::image::type_layouts`); a later record's type replaces an
/// earlier one's with the same key (a new REPL generation).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypeLayouts {
    types: HashMap<u64, Vec<(LayoutId, Word)>>,
}

impl TypeLayouts {
    /// No type.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the constructors of the type `type_key`, in tag order: each
    /// one's layout id and its static (0 for a constructor with fields).
    pub fn insert(&mut self, type_key: u64, ctors: Vec<(LayoutId, Word)>) {
        self.types.insert(type_key, ctors);
    }

    /// The layout id and static of constructor `tag` of `type_key`.
    #[must_use]
    pub fn get(&self, type_key: u64, tag: u32) -> Option<(LayoutId, Word)> {
        let tag = usize::try_from(tag).ok()?;
        self.types.get(&type_key)?.get(tag).copied()
    }
}

/// Installs `images` in order (D-535) before `main` (D-979): static
/// objects with count 0 outside the leak check (D-454, D-539, D-564), type
/// keys resolved to the layout ids that `types` lists (the batch's
/// registered layouts, D-920), lists built, maps and sets built with the
/// process seed (D-196), each root written to the slot `slot_of` gives
/// for its path with a release store. Checks every image before it writes
/// anything.
///
/// # Errors
///
/// [`ConstImageError::Malformed`], [`ConstImageError::UnknownTypeKey`] or
/// [`ConstImageError::NoSlot`], before anything is built or written.
///
/// # Safety
///
/// Each slot `slot_of` returns is a writable, 8-aligned constant slot that
/// no running code reads yet (a REPL's detached fibers may load it: the
/// store is atomic), and the layouts and statics `types` names are
/// registered and live as long as the installed values. Run it on an
/// entry whose trampolines can call the hooks of the keys' types.
pub unsafe fn install(
    images: &[ConstImage],
    slot_of: &mut dyn FnMut(&str) -> Option<*mut u64>,
    types: &TypeLayouts,
) -> Result<(), ConstImageError> {
    let views: Vec<View<'_>> = images.iter().map(View::of).collect();
    // SAFETY: forwarded from the caller.
    unsafe { install_views(&views, slot_of, types, false) }
}

/// [`install`] for the images [`serialize_in_process`] wrote in this
/// process (the REPL's, GAP(repl-constant-images)): a static object named
/// by its address (tag 4) is that object, shared, never a copy.
///
/// # Errors
///
/// As for [`install`].
///
/// # Safety
///
/// As for [`install`], and every address the images name is a live static
/// object that lives as long as the installed values (as
/// [`serialize_in_process`]'s caller guaranteed).
pub unsafe fn install_in_process(
    images: &[ConstImage],
    slot_of: &mut dyn FnMut(&str) -> Option<*mut u64>,
    types: &TypeLayouts,
) -> Result<(), ConstImageError> {
    let views: Vec<View<'_>> = images.iter().map(View::of).collect();
    // SAFETY: forwarded from the caller.
    unsafe { install_views(&views, slot_of, types, true) }
}

/// [`install`] of the images of a `constants` table ([`encode_table`]),
/// read in place: an executable installs from its program object without
/// a copy of the table.
///
/// # Errors
///
/// As for [`decode_table`] and [`install`].
///
/// # Safety
///
/// As for [`install`].
pub unsafe fn install_table(
    table: &[u8],
    slot_of: &mut dyn FnMut(&str) -> Option<*mut u64>,
    types: &TypeLayouts,
) -> Result<(), ConstImageError> {
    let views = table_views(table)?;
    // SAFETY: forwarded from the caller.
    unsafe { install_views(&views, slot_of, types, false) }
}

/// Installs `views` (module documentation): a first pass checks them all
/// and keeps nothing per object, then a second pass over the same bytes
/// builds each object straight from them.
///
/// # Safety
///
/// As for [`install_in_process`] when `in_process`, else [`install`].
unsafe fn install_views(
    views: &[View<'_>],
    slot_of: &mut dyn FnMut(&str) -> Option<*mut u64>,
    types: &TypeLayouts,
    in_process: bool,
) -> Result<(), ConstImageError> {
    // Every image is read and checked first.
    let mut cx = Cx {
        paths: HashMap::new(),
        counts: Vec::with_capacity(views.len()),
        types,
        in_process,
    };
    let mut checked = Vec::with_capacity(views.len());
    for &view in views {
        let c = check(view, &cx, slot_of)?;
        if cx.paths.insert(view.path, cx.counts.len()).is_some() {
            return Err(malformed(view.path, "a second image of one constant"));
        }
        cx.counts.push(c.count);
        checked.push(c);
    }
    // Then everything is built, image by image, from the checked bytes.
    let mut built: Vec<Vec<Word>> = Vec::with_capacity(checked.len());
    let seed = generic::process_seed();
    for c in &checked {
        let mut reader = Reader::new(c.view.path, c.view.bytes);
        reader.at = c.start;
        let mut words: Vec<Word> = Vec::with_capacity(c.count);
        for index in 0..c.count {
            let into = Building {
                words: &words,
                built: &built,
                seed,
            };
            // SAFETY: the first pass read these bytes and checked every
            // reference against the objects before it and the earlier
            // images, every constructor against `types`, every address as
            // the caller allows; nothing changed them since.
            let word = unsafe { read_object(&mut reader, index, &cx, Some(into)) }?;
            words.push(word);
        }
        let root = resolve(c.root, &words, &built);
        // SAFETY: `slot_of`'s slot, checked non-null and aligned, writable
        // as the caller guarantees; the store is atomic, since a REPL's
        // detached fibers may load the slot (D-547).
        unsafe { AtomicU64::from_ptr(c.slot) }.store(root, Ordering::Release);
        built.push(words);
    }
    Ok(())
}

/// Reads and checks one image, building nothing: every reference against
/// the objects before it and the images of `cx`, every user constructor
/// against `cx.types`, and its slot.
fn check<'a>(
    view: View<'a>,
    cx: &Cx<'_>,
    slot_of: &mut dyn FnMut(&str) -> Option<*mut u64>,
) -> Result<Checked<'a>, ConstImageError> {
    let path = view.path;
    let mut reader = Reader::new(path, view.bytes);
    if reader.header()? != path {
        return Err(malformed(path, "a path that is not the image's"));
    }
    let count = usize::try_from(reader.number()?)
        .map_err(|_| malformed(path, "an object count past the address space"))?;
    if count > view.bytes.len() {
        return Err(malformed(path, "more objects than bytes"));
    }
    let root = read_ref(&mut reader, count, cx)?;
    let start = reader.at;
    for index in 0..count {
        // SAFETY: without a place to build in, it only reads.
        unsafe { read_object(&mut reader, index, cx, None) }?;
    }
    if !reader.at_end() {
        return Err(malformed(path, "bytes after the last object"));
    }
    let slot = slot_of(path)
        .filter(|slot| !slot.is_null() && slot.is_aligned())
        .ok_or_else(|| ConstImageError::NoSlot { path: path.into() })?;
    Ok(Checked {
        view,
        start,
        count,
        root,
        slot,
    })
}

/// A reference to one of the first `before` objects of this image, a
/// static, an immediate, an object of an earlier image, or, in an
/// in-process image, a static object by address.
fn read_ref(
    reader: &mut Reader<'_>,
    before: usize,
    cx: &Cx<'_>,
) -> Result<ReadRef, ConstImageError> {
    match reader.byte()? {
        tag::IMM => {
            let word = reader.u64()?;
            if !abi::is_tagged_int(word) {
                return Err(reader.err("an immediate that is not a tagged integer"));
            }
            Ok(ReadRef::Imm(word))
        }
        tag::LOCAL => {
            let index = usize::try_from(reader.index()?).unwrap_or(usize::MAX);
            if index >= before {
                return Err(reader.err("a reference to an object not written before it"));
            }
            Ok(ReadRef::Local(index))
        }
        tag::STATIC => {
            let s = match reader.byte()? {
                0 => Static::Nil,
                1 => Static::False,
                2 => Static::True,
                3 => Static::Less,
                4 => Static::Equal,
                5 => Static::Greater,
                6 => Static::None,
                7 => Static::EmptyList,
                8 => {
                    let bytes: [u8; 2] = reader
                        .take(2)?
                        .try_into()
                        .map_err(|_| reader.err("a static past the end"))?;
                    Static::SomeSmall(i16::from_le_bytes(bytes))
                }
                _ => return Err(reader.err("an unknown runtime static")),
            };
            if s.word().is_none() {
                return Err(reader.err("an unknown runtime static"));
            }
            Ok(ReadRef::Static(s))
        }
        tag::EXTERN => {
            let path = reader.text()?;
            let index = usize::try_from(reader.index()?).unwrap_or(usize::MAX);
            let image =
                cx.paths.get(path).copied().ok_or_else(|| {
                    reader.err("a reference to a constant not installed before it")
                })?;
            if cx.counts.get(image).is_none_or(|&count| index >= count) {
                return Err(reader.err("a reference past another image's objects"));
            }
            Ok(ReadRef::Extern(image, index))
        }
        tag::ADDRESS => {
            let word = reader.u64()?;
            if !cx.in_process {
                return Err(reader.err("an address, which only an in-process image holds"));
            }
            if word == 0 || abi::is_tagged_int(word) || !word.is_multiple_of(8) {
                return Err(reader.err("an address that is no object"));
            }
            Ok(ReadRef::Address(word))
        }
        _ => Err(reader.err("an unknown reference tag")),
    }
}

/// Reads the object number `index` of an image and, `into` a place to
/// build, builds it and returns its word, static (0 when only reading).
/// One reader for both passes, so the second reads exactly what the first
/// checked.
///
/// # Safety
///
/// With `into`, the bytes were checked by a pass without it against the
/// same `cx`, so every reference resolves to a built object or a live
/// static one, and this runs on an entry whose trampolines can call the
/// hooks of the keys' types.
unsafe fn read_object(
    reader: &mut Reader<'_>,
    index: usize,
    cx: &Cx<'_>,
    into: Option<Building<'_>>,
) -> Result<Word, ConstImageError> {
    let k = reader.byte()?;
    match k {
        kind::STRING => {
            let text = reader.text()?;
            // SAFETY: a fresh object of this thread.
            Ok(into.map_or(0, |_| unsafe { made_static(values::string(text)) }))
        }
        kind::BYTES => {
            let len = reader.len()?;
            let data = reader.take(len)?;
            // SAFETY: a fresh object of this thread.
            Ok(into.map_or(0, |_| unsafe { made_static(values::bytes(data)) }))
        }
        kind::BOXED_INT | kind::BOXED_FLOAT | kind::BOXED_F32 | kind::BOXED_U64 => {
            let id = match k {
                kind::BOXED_INT => ids::BOXED_INT,
                kind::BOXED_FLOAT => ids::BOXED_FLOAT,
                kind::BOXED_F32 => ids::BOXED_F32,
                _ => ids::BOXED_U64,
            };
            let bits = reader.u64()?;
            Ok(into.map_or(0, |_| {
                let obj = alloc::alloc_object(16, abi::header(abi::RC_UNIQUE, 0, id));
                // SAFETY: a fresh 16-byte box: its payload is its only
                // field; then a fresh object of this thread.
                unsafe {
                    write_field(obj, FIELD0, bits);
                    made_static(object::ptr_to_word(obj))
                }
            }))
        }
        kind::TUPLE => {
            let n = reader.len()?;
            if n < 2 {
                return Err(reader.err("a tuple of fewer than two elements"));
            }
            let mut elements = Vec::new();
            for _ in 0..n {
                let r = read_ref(reader, index, cx)?;
                if let Some(into) = into {
                    elements.push(into.get(r));
                }
            }
            // SAFETY: a fresh tuple of this thread, its elements static.
            Ok(into.map_or(0, |_| unsafe { made_static(values::tuple(&elements)) }))
        }
        kind::SOME | kind::OK | kind::ERR => {
            let r = read_ref(reader, index, cx)?;
            Ok(into.map_or(0, |into| {
                let field = into.get(r);
                let word = match k {
                    kind::SOME => values::some(field),
                    kind::OK => values::ok(field),
                    _ => values::err(field),
                };
                // SAFETY: a fresh object of this thread, its field static.
                unsafe { made_static(word) }
            }))
        }
        kind::USER => read_user(reader, index, cx, into),
        kind::LIST => {
            let n = reader.len()?;
            let Some(into) = into else {
                for _ in 0..n {
                    read_ref(reader, index, cx)?;
                }
                return Ok(0);
            };
            let mut failed = None;
            let list = list::from_iter((0..n).map_while(|_| match read_ref(reader, index, cx) {
                Ok(r) => Some(into.get(r)),
                Err(e) => {
                    failed = Some(e);
                    None
                }
            }));
            // SAFETY: a fresh list of this thread, its elements static.
            unsafe { alloc::freeze(list) };
            failed.map_or(Ok(list), Err)
        }
        kind::MAP => read_collection(reader, index, cx, into, hamt::Collection::Map),
        kind::SET => read_collection(reader, index, cx, into, hamt::Collection::Set),
        _ => Err(reader.err("an unknown object kind")),
    }
}

/// The rest of a map or a set (its kind byte read) and, `into` a place to
/// build, the collection built under the process seed from the entries in
/// the image's order (D-196): no list of pairs, no second trie.
fn read_collection(
    reader: &mut Reader<'_>,
    index: usize,
    cx: &Cx<'_>,
    into: Option<Building<'_>>,
    collection: hamt::Collection,
) -> Result<Word, ConstImageError> {
    let n = reader.len()?;
    let map = collection == hamt::Collection::Map;
    let Some(into) = into else {
        for _ in 0..n {
            read_ref(reader, index, cx)?;
            if map {
                read_ref(reader, index, cx)?;
            }
        }
        return Ok(0);
    };
    let mut failed = None;
    let mut entry = || -> Result<(Word, Option<Word>), ConstImageError> {
        let key = into.get(read_ref(reader, index, cx)?);
        let value = if map {
            Some(into.get(read_ref(reader, index, cx)?))
        } else {
            None
        };
        Ok((key, value))
    };
    let word = hamt::from_entries(
        collection,
        into.seed,
        (0..n).map_while(|_| match entry() {
            Ok(e) => Some(e),
            Err(e) => {
                failed = Some(e);
                None
            }
        }),
    );
    // SAFETY: a fresh map or set of this thread, its entries static.
    unsafe { alloc::freeze(word) };
    failed.map_or(Ok(word), Err)
}

/// The rest of a user constructor (its kind byte read), checked against
/// `cx.types` and, `into` a place to build, built: its registered
/// `cb$ctor$` static for a nullary one.
fn read_user(
    reader: &mut Reader<'_>,
    index: usize,
    cx: &Cx<'_>,
    into: Option<Building<'_>>,
) -> Result<Word, ConstImageError> {
    let type_key = reader.u64()?;
    let tag = reader.index()?;
    let n = reader.len()?;
    let path = reader.path;
    let unknown = || ConstImageError::UnknownTypeKey {
        path: path.into(),
        type_key,
        ctor: tag,
    };
    let (id, static_word) = cx.types.get(type_key, tag).ok_or_else(unknown)?;
    let registered = layout::get(id).ok_or_else(unknown)?;
    if registered.fields.len() != n || registered.type_key != type_key {
        return Err(unknown());
    }
    let mut values = Vec::with_capacity(n);
    for field in &registered.fields {
        let kind_index = usize::from(reader.byte()?);
        if FIELD_KINDS.get(kind_index) != Some(&field.kind) {
            return Err(unknown());
        }
        if field.kind == FieldKind::Value {
            let r = read_ref(reader, index, cx)?;
            values.push(into.map_or(0, |into| into.get(r)));
        } else {
            values.push(reader.u64()?);
        }
    }
    if values.is_empty() {
        if static_word == 0 {
            return Err(reader.err("a nullary constructor without its static"));
        }
        return Ok(into.map_or(0, |_| static_word));
    }
    // SAFETY: `registered` is the layout of `id`, and the fields' words
    // are static: built objects or statics.
    Ok(into.map_or(0, |_| unsafe { build_user(id, registered, &values) }))
}

/// Makes a fresh object static, its children being static already, and
/// returns it.
///
/// # Safety
///
/// `word` is a fresh object of this thread whose children are static.
unsafe fn made_static(word: Word) -> Word {
    // SAFETY: forwarded from the caller.
    unsafe { alloc::forget(word) };
    word
}

/// A static user constructor of layout `id`, its fields `values` in
/// declaration order (a word, or a raw field's bits).
///
/// # Safety
///
/// `info` is the registered layout of `id`, and every value field's word
/// is static.
unsafe fn build_user(id: LayoutId, info: &LayoutInfo, values: &[Word]) -> Word {
    let size = info.fixed_size().unwrap_or(abi::HEADER_SIZE);
    let obj = alloc::alloc_object(size, abi::header(abi::RC_UNIQUE, info.scan_count(), id));
    for (field, &value) in info.fields.iter().zip(values) {
        let offset = usize::try_from(field.offset).unwrap_or(usize::MAX);
        // SAFETY: a fresh object of the layout `info`, whose field lies at
        // `offset`.
        unsafe { write_field(obj, offset, value) };
    }
    // SAFETY: a fresh object of this thread, its fields static.
    unsafe { made_static(object::ptr_to_word(obj)) }
}

/// The word of a checked reference.
fn resolve(r: ReadRef, words: &[Word], built: &[Vec<Word>]) -> Word {
    match r {
        ReadRef::Imm(word) | ReadRef::Address(word) => word,
        ReadRef::Local(index) => words.get(index).copied().unwrap_or(0),
        ReadRef::Static(s) => s.word().unwrap_or(0),
        ReadRef::Extern(image, index) => built
            .get(image)
            .and_then(|words| words.get(index))
            .copied()
            .unwrap_or(0),
    }
}

/// Whether `word` is a static object (count 0), for tests and hosts.
///
/// # Safety
///
/// `word` is a tagged integer or a live object.
#[must_use]
pub unsafe fn is_static(word: Word) -> bool {
    if abi::is_tagged_int(word) || word == 0 {
        return false;
    }
    // SAFETY: forwarded from the caller.
    unsafe { load_rc(word_to_ptr(word)) == abi::RC_STATIC }
}

#[cfg(test)]
mod tests;
