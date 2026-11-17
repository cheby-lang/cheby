//! The native comparison of `std::heap`'s kernels (D-1613): the built-in
//! `compare` of `Int`, the sized integer types, `String` and tuples of
//! those, recursively, decided from the element type's descriptor.
//!
//! The kernels receive `compare` as a closure that `std/heap.cheby` builds
//! in a function bounded by `T: Compare`, `fn(a, b) { Compare::compare(a,
//! b) }`. Its only capture is that bound's dictionary (plan §5.4: a
//! closure holds its captured locals, here none, then the dictionaries its
//! body uses), and field 0 of a dictionary is the descriptor of its type
//! (D-499, D-524, §8.6.1). So [`native_key`] reads the element type from
//! the closure, never from the shape of a value: a user type whose own
//! `compare` orders values that look like `Int`s, such as nullary
//! constructors, has its own descriptor and is called back.
//!
//! [`Key::compare`] gives the `Order` the built-in `compare` gives: `Int`,
//! `I64` and the sized types that fit 63 bits by their signed value, `U64`
//! by its unsigned value, `String` by its bytes, which is code point order
//! (`cheby_string_compare`, D-136), and a tuple by its elements from left
//! to right, the first result other than `Equal` deciding (D-167).

use std::cmp::Ordering;

use crate::abi::{self, Word, layout};
use crate::desc;
use crate::object::{read_field, scan_and_layout, word_to_ptr};
use crate::panic::fatal;
use crate::string;
use crate::values;

/// An element type whose `compare` is built in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Key {
    /// `Int`, `I64`, `I8`, `I16`, `I32`, `U8`, `U16` or `U32`: a tagged
    /// word, or a `BoxedInt` for an `Int` or `I64` outside 63 bits.
    Signed,
    /// `U64`: a tagged word, or a `BoxedU64` above 2^62 - 1.
    Unsigned,
    /// `String`, by its bytes.
    Text,
    /// A tuple of such types, in order.
    Tuple(Box<[Key]>),
}

impl Key {
    /// `compare(a, b)` of two borrowed values of this type.
    ///
    /// # Safety
    ///
    /// `a` and `b` are live values of the type this key was read from.
    pub(super) unsafe fn compare(&self, a: Word, b: Word) -> Ordering {
        match self {
            // SAFETY: forwarded from the caller: values of these types.
            Self::Signed => unsafe { values::word_to_i64(a).cmp(&values::word_to_i64(b)) },
            // SAFETY: as above.
            Self::Unsigned => unsafe { values::word_to_u64(a).cmp(&values::word_to_u64(b)) },
            // SAFETY: as above: two live strings.
            Self::Text => unsafe { text(a).cmp(text(b)) },
            Self::Tuple(keys) => {
                for (index, key) in keys.iter().enumerate() {
                    // SAFETY: two live tuples of `keys.len()` elements, each
                    // of its key's type.
                    let order = unsafe { key.compare(field(a, index), field(b, index)) };
                    if order != Ordering::Equal {
                        return order;
                    }
                }
                Ordering::Equal
            }
        }
    }
}

/// The bytes of a borrowed `String`.
///
/// # Safety
///
/// `word` is a live `String`.
unsafe fn text<'a>(word: Word) -> &'a [u8] {
    // SAFETY: forwarded from the caller.
    unsafe { string::text_bytes(word) }
        .unwrap_or_else(|| fatal(format_args!("{word:#x} is not a String")))
}

/// Element `index` of a borrowed tuple.
///
/// # Safety
///
/// `word` is a live tuple with more than `index` elements.
unsafe fn field(word: Word, index: usize) -> Word {
    let obj = word_to_ptr(word);
    // SAFETY: a live tuple: its arity is the scan count, or the tagged
    // count at offset 8 when escaped, and its fields follow.
    let (scan, _) = unsafe { scan_and_layout(obj) };
    let first = if scan == abi::SCAN_ESCAPE { 16 } else { 8 };
    // SAFETY: the element is within the tuple, as the caller guarantees.
    unsafe { read_field(obj, index.saturating_mul(8).saturating_add(first)) }
}

/// The built-in comparison of the element type of `by`, or `None` when it
/// must be called back. `by` is the closure `std/heap.cheby` passes: a
/// closure whose one capture is a `Compare` dictionary, whose descriptor
/// is the element type's (module documentation). Any other word gives
/// `None`.
///
/// # Safety
///
/// `by` is a live closure.
pub(super) unsafe fn native_key(by: Word) -> Option<Key> {
    // SAFETY: forwarded from the caller.
    let dict = unsafe { only_capture(by) }?;
    // SAFETY: `dict` is a live capture of a live closure.
    if unsafe { layout_of(dict) } != Some(layout::DICT) {
        return None;
    }
    // SAFETY: a live dictionary holds its type's descriptor at offset 8
    // (D-524).
    let descriptor = unsafe { read_field(word_to_ptr(dict), 8) };
    // SAFETY: a dictionary's descriptor is live while the dictionary is.
    unsafe { key_of(descriptor) }
}

/// The only capture of a borrowed closure, or `None` when `word` is not a
/// closure with exactly one.
///
/// # Safety
///
/// `word` is a tagged integer or a live object.
unsafe fn only_capture(word: Word) -> Option<Word> {
    // SAFETY: forwarded from the caller.
    if unsafe { layout_of(word) } != Some(layout::CLOSURE) {
        return None;
    }
    let obj = word_to_ptr(word);
    // SAFETY: a live closure: the code word at 8, then the captures, so
    // its scan count is one more than its captures.
    let (scan, _) = unsafe { scan_and_layout(obj) };
    // SAFETY: as above: the one capture is at offset 16.
    (scan == 2).then(|| unsafe { read_field(obj, 16) })
}

/// The layout id of a word's object, or `None` for a tagged integer or 0.
///
/// # Safety
///
/// `word` is 0, a tagged integer or a live object.
unsafe fn layout_of(word: Word) -> Option<abi::LayoutId> {
    if word == 0 || abi::is_tagged_int(word) {
        return None;
    }
    // SAFETY: a live object, as the caller guarantees.
    Some(unsafe { scan_and_layout(word_to_ptr(word)) }.1)
}

/// The key of the type a live descriptor describes, or `None` when its
/// `compare` is not built in. A builtin type has an empty module path
/// (D-559), so a user type of the same name is never taken for it.
///
/// # Safety
///
/// `descriptor` is a live descriptor or nominal descriptor.
pub(super) unsafe fn key_of(descriptor: Word) -> Option<Key> {
    // SAFETY: forwarded from the caller.
    let parts = unsafe { desc::parts(descriptor) }?;
    // SAFETY: a live descriptor's nominal descriptor holds two strings.
    let (name, module) = unsafe { desc::names(parts.nominal) };
    if !module.is_empty() {
        return None;
    }
    match (name, parts.args.is_empty()) {
        (b"Int" | b"I64" | b"I8" | b"I16" | b"I32" | b"U8" | b"U16" | b"U32", true) => {
            Some(Key::Signed)
        }
        (b"U64", true) => Some(Key::Unsigned),
        (b"String", true) => Some(Key::Text),
        (b"Tuple", false) => parts
            .args
            .iter()
            // SAFETY: the arguments of a live descriptor are live
            // descriptors.
            .map(|&arg| unsafe { key_of(arg) })
            .collect::<Option<Box<[Key]>>>()
            .map(Key::Tuple),
        _ => None,
    }
}
