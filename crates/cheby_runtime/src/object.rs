//! Objects in memory: converting words to pointers, reading and writing the
//! header (D-279), and the static objects of the builtin nullary
//! constructors (D-454).
//!
//! The header is accessed in two halves. The 32-bit reference count at
//! offset 0 is the only part ever written after allocation, always with an
//! atomic access, so M2's shared counts never race a plain store. The other
//! half (scan count and layout id, offset 4) is never written after
//! allocation and is read with plain loads.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::abi::{self, LayoutId, Word, layout};

const _: () = assert!(usize::BITS == 64, "the runtime supports 64-bit hosts only");
const _: () = assert!(
    cfg!(target_endian = "little"),
    "the runtime supports little-endian hosts only"
);

/// `u64` to `usize`.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "usize is 64 bits on every supported host (asserted above)"
)]
pub(crate) const fn to_usize(n: u64) -> usize {
    n as usize
}

/// `usize` to `u64`.
#[expect(
    clippy::as_conversions,
    reason = "usize is 64 bits on every supported host (asserted above)"
)]
pub(crate) const fn to_u64(n: usize) -> u64 {
    n as u64
}

/// An ABI byte offset (non-negative by construction) as a `usize`.
pub(crate) fn abi_offset(offset: i32) -> usize {
    to_usize(u64::from(offset.unsigned_abs()))
}

/// The object a pointer word points to. The word must have come from
/// [`ptr_to_word`] or from generated code, which exposes the provenance of
/// every address it embeds.
pub(crate) fn word_to_ptr(word: Word) -> *mut u8 {
    std::ptr::with_exposed_provenance_mut(to_usize(word))
}

/// The word of an object pointer. Exposes the pointer's provenance so that
/// [`word_to_ptr`] may turn the word back into a usable pointer.
pub(crate) fn ptr_to_word(ptr: *const u8) -> Word {
    to_u64(ptr.expose_provenance())
}

/// The reference count of an object, as an atomic.
///
/// # Safety
///
/// `obj` points to a live object (heap or static) whose memory is writable
/// or is only ever read through the returned reference with loads.
#[expect(
    clippy::cast_ptr_alignment,
    reason = "objects are 8-aligned, so the count at offset 0 is 4-aligned"
)]
unsafe fn rc_cell<'a>(obj: *mut u8) -> &'a AtomicI32 {
    // SAFETY: objects are 8-aligned, so the count at offset 0 is 4-aligned,
    // and the caller guarantees the object is live for the reference's use.
    unsafe { AtomicI32::from_ptr(obj.cast::<i32>()) }
}

/// Reads an object's reference count.
///
/// # Safety
///
/// `obj` points to a live object.
pub(crate) unsafe fn load_rc(obj: *mut u8) -> i32 {
    // SAFETY: forwarded from the caller.
    unsafe { rc_cell(obj) }.load(Ordering::Relaxed)
}

/// Writes an object's local (unshared) reference count.
///
/// # Safety
///
/// `obj` points to a live heap object whose count is local (positive), so
/// no other thread accesses it.
pub(crate) unsafe fn store_rc(obj: *mut u8, rc: i32) {
    // SAFETY: forwarded from the caller.
    unsafe { rc_cell(obj) }.store(rc, Ordering::Relaxed);
}

/// The shared reference count of an object, for the atomic paths of M2.
///
/// # Safety
///
/// `obj` points to a live heap object.
pub(crate) unsafe fn shared_rc<'a>(obj: *mut u8) -> &'a AtomicI32 {
    // SAFETY: forwarded from the caller.
    unsafe { rc_cell(obj) }
}

/// The scan count and layout id of an object.
///
/// # Safety
///
/// `obj` points to a live object.
#[expect(
    clippy::cast_ptr_alignment,
    reason = "objects are 8-aligned, so offset 4 is 4-aligned"
)]
pub(crate) unsafe fn scan_and_layout(obj: *const u8) -> (u8, LayoutId) {
    // SAFETY: the upper half of the header is at offset 4, 4-aligned, and
    // is never written after allocation, so a plain read does not race.
    let upper = unsafe { obj.add(4).cast::<u32>().read() };
    let [scan, ..] = upper.to_le_bytes();
    (scan, upper >> 8)
}

/// Reads the 8-byte field at `offset`.
///
/// # Safety
///
/// `obj` points to a live object with an 8-byte field at `offset`.
#[expect(
    clippy::cast_ptr_alignment,
    reason = "objects are 8-aligned and fields are at multiples of 8"
)]
pub(crate) unsafe fn read_field(obj: *const u8, offset: usize) -> u64 {
    // SAFETY: forwarded from the caller; fields are 8-aligned.
    unsafe { obj.add(offset).cast::<u64>().read() }
}

/// Writes the 8-byte field at `offset`.
///
/// # Safety
///
/// `obj` points to a heap object that the caller is initializing or owns
/// uniquely, with an 8-byte field at `offset`.
#[expect(
    clippy::cast_ptr_alignment,
    reason = "objects are 8-aligned and fields are at multiples of 8"
)]
pub(crate) unsafe fn write_field(obj: *mut u8, offset: usize, value: u64) {
    // SAFETY: forwarded from the caller; fields are 8-aligned.
    unsafe { obj.add(offset).cast::<u64>().write(value) };
}

/// A static object with no fields: the header only, with count
/// [`abi::RC_STATIC`] so that `dup` and `drop` never write it.
#[derive(Debug)]
#[repr(C, align(8))]
pub struct StaticObject {
    // An `UnsafeCell` keeps the object in writable memory, which the atomic
    // count load requires of Rust-defined memory. It is never written.
    header: UnsafeCell<u64>,
}

// SAFETY: the header is never written after initialization (its count is
// `RC_STATIC`, which every count operation checks before writing), so
// sharing it between threads only ever shares reads.
unsafe impl Sync for StaticObject {}

impl StaticObject {
    /// A static object of the given layout.
    #[must_use]
    pub const fn new(layout: LayoutId) -> Self {
        Self {
            header: UnsafeCell::new(abi::header(abi::RC_STATIC, 0, layout)),
        }
    }

    /// The object's word.
    #[must_use]
    pub fn word(&'static self) -> Word {
        ptr_to_word(std::ptr::from_ref(self).cast::<u8>())
    }

    /// The object's header word.
    #[must_use]
    pub fn header(&self) -> u64 {
        // SAFETY: the header is never written after initialization.
        unsafe { self.header.get().read() }
    }
}

/// The static `Nil` object.
#[unsafe(no_mangle)]
pub static cheby_rt_static_nil: StaticObject = StaticObject::new(layout::NIL);
/// The static `False` object.
#[unsafe(no_mangle)]
pub static cheby_rt_static_false: StaticObject = StaticObject::new(layout::FALSE);
/// The static `True` object.
#[unsafe(no_mangle)]
pub static cheby_rt_static_true: StaticObject = StaticObject::new(layout::TRUE);
/// The static `Less` object.
#[unsafe(no_mangle)]
pub static cheby_rt_static_less: StaticObject = StaticObject::new(layout::LESS);
/// The static `Equal` object.
#[unsafe(no_mangle)]
pub static cheby_rt_static_equal: StaticObject = StaticObject::new(layout::EQUAL);
/// The static `Greater` object.
#[unsafe(no_mangle)]
pub static cheby_rt_static_greater: StaticObject = StaticObject::new(layout::GREATER);
/// The static `None` object.
#[unsafe(no_mangle)]
pub static cheby_rt_static_none: StaticObject = StaticObject::new(layout::NONE);

/// A static object with one value field holding another static object: the
/// header, with count [`abi::RC_STATIC`] and scan count 1, then the field.
#[derive(Debug)]
#[repr(C, align(8))]
pub struct StaticPair {
    // As in `StaticObject`: an `UnsafeCell` keeps the object in writable
    // memory, which the atomic count load requires. It is never written.
    header: UnsafeCell<u64>,
    /// The field: the address of a static object, which is its word. A
    /// pointer, since a static's initializer cannot turn an address into
    /// an integer.
    field: *const StaticObject,
}

// SAFETY: neither the header nor the field is ever written after
// initialization (the count is `RC_STATIC`, which every count operation
// checks before writing, and the field of a static object is never
// updated in place), and the field points to a `Sync` static, so sharing
// the object between threads only ever shares reads.
unsafe impl Sync for StaticPair {}

impl StaticPair {
    /// A static object of the given layout whose one field is `field`.
    #[must_use]
    pub const fn new(layout: LayoutId, field: &'static StaticObject) -> Self {
        Self {
            header: UnsafeCell::new(abi::header(abi::RC_STATIC, 1, layout)),
            field: std::ptr::from_ref(field),
        }
    }

    /// The object's word.
    #[must_use]
    pub fn word(&'static self) -> Word {
        ptr_to_word(std::ptr::from_ref(self).cast::<u8>())
    }

    /// The object's header word.
    #[must_use]
    pub fn header(&self) -> u64 {
        // SAFETY: the header is never written after initialization.
        unsafe { self.header.get().read() }
    }
}

/// The static `Err(Nil)` the `scope_run` intrinsic returns when its body
/// failed (D-693): the cause stays in the fiber, so the value carries
/// nothing and catching allocates nothing.
#[unsafe(no_mangle)]
pub static cheby_rt_static_err_nil: StaticPair = StaticPair::new(layout::ERR, &cheby_rt_static_nil);

/// A static `Ok(Nil)`, which `channel::send` returns when it succeeds, so
/// that a send allocates nothing for its result. Runtime code only: no
/// symbol of generated code names it.
pub(crate) static STATIC_OK_NIL: StaticPair = StaticPair::new(layout::OK, &cheby_rt_static_nil);

/// The static object of a builtin nullary constructor, by layout id.
#[must_use]
pub fn static_ctor(id: LayoutId) -> Option<&'static StaticObject> {
    match id {
        layout::NIL => Some(&cheby_rt_static_nil),
        layout::FALSE => Some(&cheby_rt_static_false),
        layout::TRUE => Some(&cheby_rt_static_true),
        layout::LESS => Some(&cheby_rt_static_less),
        layout::EQUAL => Some(&cheby_rt_static_equal),
        layout::GREATER => Some(&cheby_rt_static_greater),
        layout::NONE => Some(&cheby_rt_static_none),
        _ => None,
    }
}

/// The word of the static `Nil` object.
#[must_use]
pub fn nil() -> Word {
    cheby_rt_static_nil.word()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_objects_are_aligned_and_static() {
        for id in [
            layout::NIL,
            layout::FALSE,
            layout::TRUE,
            layout::LESS,
            layout::EQUAL,
            layout::GREATER,
            layout::NONE,
        ] {
            let object = static_ctor(id).unwrap();
            let word = object.word();
            assert_eq!(word % 8, 0);
            assert!(!abi::is_tagged_int(word));
            assert_eq!(abi::header_rc(object.header()), abi::RC_STATIC);
            assert_eq!(abi::header_layout(object.header()), id);
            // SAFETY: the word points to a live static object.
            let (scan, read_layout) = unsafe { scan_and_layout(word_to_ptr(word)) };
            assert_eq!((scan, read_layout), (0, id));
            // SAFETY: as above.
            assert_eq!(unsafe { load_rc(word_to_ptr(word)) }, abi::RC_STATIC);
        }
        assert!(static_ctor(layout::SOME).is_none());
    }

    #[test]
    fn the_static_err_nil_holds_the_static_nil() {
        let word = cheby_rt_static_err_nil.word();
        assert_eq!(word % 8, 0);
        let header = cheby_rt_static_err_nil.header();
        assert_eq!(header, abi::header(abi::RC_STATIC, 1, layout::ERR));
        // SAFETY: the word points to a live static object with one field.
        unsafe {
            assert_eq!(scan_and_layout(word_to_ptr(word)), (1, layout::ERR));
            assert_eq!(load_rc(word_to_ptr(word)), abi::RC_STATIC);
            assert_eq!(read_field(word_to_ptr(word), 8), nil());
        }
        assert_eq!(std::mem::size_of::<StaticPair>(), 16);
    }

    #[test]
    fn words_and_pointers_round_trip() {
        let word = nil();
        assert_eq!(ptr_to_word(word_to_ptr(word)), word);
    }
}
