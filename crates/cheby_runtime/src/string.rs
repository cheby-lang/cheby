//! `String` and `Bytes` objects (D-454): header (layout 1 or 2, scan 0),
//! `len`, `cap`, then `cap` bytes of which the first `len` are the
//! contents (UTF-8 text for a `String`). Static strings (literals) have
//! count 0 and `cap == len`; code generation builds them with
//! [`abi::string::static_image`].
//!
//! Both kinds update in place when their count is one and `cap` suffices
//! (D-316, D-525): [`reserve`] and [`append`] consume a text object and
//! return one with room, growing by doubling, and copy a shared or static
//! one instead of writing it.

use crate::abi::{self, LayoutId, Word};
use crate::alloc;
use crate::object::{
    self, abi_offset, read_field, scan_and_layout, to_u64, to_usize, word_to_ptr, write_field,
};
use crate::panic::fatal;
use crate::rc;
use crate::stats::{self, Event};

const LEN: i32 = abi::string::LEN_OFFSET;
const CAP: i32 = abi::string::CAP_OFFSET;
const BYTES: i32 = abi::string::BYTES_OFFSET;

/// Allocates an empty heap text object (count 1) of layout
/// [`abi::layout::STRING`] or [`abi::layout::BYTES`] with room for `cap`
/// bytes. Ends the process if the size overflows or allocation fails.
#[must_use]
pub fn alloc_text(cap: u64, layout: LayoutId) -> *mut u8 {
    let Some(size) = abi::string::alloc_size(cap) else {
        fatal(format_args!("cannot allocate a text of {cap} bytes"));
    };
    let obj = alloc::alloc_object(size, abi::header(abi::RC_UNIQUE, 0, layout));
    // SAFETY: a fresh text object: `len` and `cap` are within it.
    unsafe {
        write_field(obj, abi_offset(LEN), 0);
        write_field(obj, abi_offset(CAP), cap);
    }
    obj
}

/// Allocates an empty heap string (count 1) with room for `cap` bytes.
/// Ends the process if the size overflows or allocation fails.
#[must_use]
pub fn alloc_string(cap: u64) -> *mut u8 {
    alloc_text(cap, abi::layout::STRING)
}

/// Allocates an empty heap `Bytes` (count 1) with room for `cap` bytes.
#[must_use]
pub fn alloc_bytes(cap: u64) -> *mut u8 {
    alloc_text(cap, abi::layout::BYTES)
}

/// A new heap text object (count 1) of the given layout holding `data`,
/// with `cap == len`.
#[must_use]
pub fn new_text(data: &[u8], layout: LayoutId) -> Word {
    let len = to_u64(data.len());
    let obj = alloc_text(len, layout);
    // SAFETY: the object has room for `len` bytes after `BYTES`, and `data`
    // does not overlap a fresh allocation.
    unsafe {
        std::ptr::copy_nonoverlapping(data.as_ptr(), obj.add(abi_offset(BYTES)), data.len());
        write_field(obj, abi_offset(LEN), len);
    }
    object::ptr_to_word(obj)
}

/// A new heap string (count 1) holding `text`, with `cap == len`.
#[must_use]
pub fn new_string(text: &str) -> Word {
    new_text(text.as_bytes(), abi::layout::STRING)
}

/// A new heap `Bytes` (count 1) holding `data`, with `cap == len`.
#[must_use]
pub fn new_bytes(data: &[u8]) -> Word {
    new_text(data, abi::layout::BYTES)
}

/// The layout of a text object (`String` or `Bytes`), or `None` for any
/// other word.
///
/// # Safety
///
/// `word` is a tagged integer, 0, or points to a live object.
unsafe fn text_layout(word: Word) -> Option<LayoutId> {
    if abi::is_tagged_int(word) || word == 0 {
        return None;
    }
    // SAFETY: forwarded from the caller.
    unsafe { alloc::check_live(word, "reading a text") };
    // SAFETY: as above.
    let (_, id) = unsafe { scan_and_layout(word_to_ptr(word)) };
    matches!(id, abi::layout::STRING | abi::layout::BYTES).then_some(id)
}

/// The contents of a text object (`String` or `Bytes`) of a known layout.
///
/// # Safety
///
/// `word` points to a live text object that outlives `'a` and is not
/// changed during `'a`.
unsafe fn contents<'a>(word: Word) -> &'a [u8] {
    let obj = word_to_ptr(word);
    // SAFETY: a text object has `len` at this offset, and `len` bytes after
    // `BYTES`, live for `'a`.
    unsafe {
        let len = to_usize(read_field(obj, abi_offset(LEN)));
        std::slice::from_raw_parts(obj.add(abi_offset(BYTES)), len)
    }
}

/// The text of a string object, or `None` if `word` is not a string.
///
/// # Safety
///
/// `word` is a tagged integer or points to a live object that outlives
/// `'a` and is not changed during `'a`.
#[must_use]
pub unsafe fn bytes<'a>(word: Word) -> Option<&'a [u8]> {
    // SAFETY: forwarded from the caller.
    match unsafe { text_layout(word) } {
        // SAFETY: a live string, as the caller guarantees.
        Some(abi::layout::STRING) => Some(unsafe { contents(word) }),
        _ => None,
    }
}

/// The contents of a `String` or a `Bytes` (its UTF-8 bytes, or its
/// bytes), or `None` for any other word.
///
/// # Safety
///
/// As for [`bytes`].
#[must_use]
pub unsafe fn text_bytes<'a>(word: Word) -> Option<&'a [u8]> {
    // SAFETY: forwarded from the caller.
    unsafe { text_layout(word) }.map(|_| {
        // SAFETY: a live text object, as the caller guarantees.
        unsafe { contents(word) }
    })
}

/// The text of a string object as a `&str`, or `None` if `word` is not a
/// string. A string whose bytes are not UTF-8 breaks the runtime's
/// invariant and ends the process.
///
/// # Safety
///
/// As for [`bytes`].
#[must_use]
pub unsafe fn as_str<'a>(word: Word) -> Option<&'a str> {
    // SAFETY: forwarded from the caller.
    let bytes = unsafe { bytes(word) }?;
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(_) => fatal(format_args!(
            "the string at {word:#x} does not hold UTF-8 text"
        )),
    }
}

/// The text of a string object as a `&str` without validating it, or
/// `None` if `word` is not a string. O(1), for kernels whose required cost
/// is below the length of the text (`starts_with` is O(m) in the prefix,
/// D-300).
///
/// # Safety
///
/// As for [`bytes`]. Every `String` object holds UTF-8: the runtime builds
/// strings only from `&str`, code generation's literals come from `&str`,
/// appending two strings keeps UTF-8, and `string::from_bytes` validates.
#[must_use]
pub unsafe fn as_str_unchecked<'a>(word: Word) -> Option<&'a str> {
    // SAFETY: forwarded from the caller.
    let bytes = unsafe { bytes(word) }?;
    // SAFETY: a `String` object holds UTF-8 (see above).
    Some(unsafe { std::str::from_utf8_unchecked(bytes) })
}

/// The text of a string object as a Rust string, with any invalid UTF-8
/// replaced, or `None` if `word` is not a string.
///
/// # Safety
///
/// As for [`bytes`].
#[must_use]
pub unsafe fn to_rust_string(word: Word) -> Option<String> {
    // SAFETY: forwarded from the caller.
    unsafe { bytes(word) }.map(|bytes| String::from_utf8_lossy(bytes).into_owned())
}

/// Consumes a text object and returns one with the same contents and room
/// for at least `extra` more bytes: the same object when it is unique (count
/// one) and has room, otherwise a new unique one of capacity
/// `max(len + extra, 2 * cap)`, after which the old one is released (freed
/// when it was unique, left alone when shared or static). Ends the process
/// if `text` is not a text object or the size overflows.
///
/// # Safety
///
/// `text` points to a live `String` or `Bytes` whose reference the caller
/// gives up.
#[must_use]
pub unsafe fn reserve(text: Word, extra: u64) -> Word {
    // SAFETY: forwarded from the caller.
    let Some(layout) = (unsafe { text_layout(text) }) else {
        fatal(format_args!("{text:#x} is not a text object"));
    };
    let obj = word_to_ptr(text);
    // SAFETY: a live text object.
    let (len, cap) = unsafe {
        (
            read_field(obj, abi_offset(LEN)),
            read_field(obj, abi_offset(CAP)),
        )
    };
    let Some(need) = len.checked_add(extra) else {
        fatal(format_args!("a text of {len} + {extra} bytes is too large"));
    };
    // SAFETY: a live object.
    if need <= cap && unsafe { rc::is_unique(text) } {
        return text;
    }
    let new_cap = need.max(cap.saturating_mul(2));
    let new = alloc_text(new_cap, layout);
    // SAFETY: both are live; the new object has room for `len` bytes and
    // does not overlap the old one.
    unsafe {
        std::ptr::copy_nonoverlapping(
            obj.add(abi_offset(BYTES)),
            new.add(abi_offset(BYTES)),
            to_usize(len),
        );
        write_field(new, abi_offset(LEN), len);
        rc::drop(text);
    }
    object::ptr_to_word(new)
}

/// Consumes a text object and returns it followed by `piece`, in place when
/// it is unique and has room, otherwise in a new object grown by doubling
/// ([`reserve`]). `piece` must not point into `text`'s own contents.
///
/// # Safety
///
/// As for [`reserve`].
#[must_use]
pub unsafe fn append(text: Word, piece: &[u8]) -> Word {
    // SAFETY: forwarded from the caller.
    let text = unsafe { reserve(text, to_u64(piece.len())) };
    let obj = word_to_ptr(text);
    // SAFETY: `text` is unique with room for `piece` after its `len` bytes,
    // and `piece` lies outside it.
    unsafe {
        let len = read_field(obj, abi_offset(LEN));
        std::ptr::copy_nonoverlapping(
            piece.as_ptr(),
            obj.add(abi_offset(BYTES)).add(to_usize(len)),
            piece.len(),
        );
        write_field(
            obj,
            abi_offset(LEN),
            len.saturating_add(to_u64(piece.len())),
        );
    }
    text
}

/// A unique text object with the contents of a borrowed `text` and room
/// for at least `extra` more bytes, and whether it is `text` itself.
///
/// It is `text` itself when `reuse` holds, `text` is unique and it has
/// room, so the caller may append in place (D-525): the caller then
/// appends ([`append`] stays in place) and only afterwards adds the
/// reference it returns with `rc::dup`. Otherwise it is a new unique object
/// of capacity `max(len + extra, 2 * cap)` that the caller owns, so a chain
/// of appends to a unique result grows by doubling. `text` keeps its
/// reference either way. Ends the process if `text` is not a text object
/// or the size overflows.
///
/// The caller passes `reuse` false when another argument of its kernel is
/// the same object (D-563's alias rule).
///
/// # Safety
///
/// `text` points to a live `String` or `Bytes`, borrowed for the call.
#[must_use]
pub unsafe fn with_room(text: Word, extra: u64, reuse: bool) -> (Word, bool) {
    // SAFETY: forwarded from the caller.
    let Some(layout) = (unsafe { text_layout(text) }) else {
        fatal(format_args!("{text:#x} is not a text object"));
    };
    let obj = word_to_ptr(text);
    // SAFETY: a live text object.
    let (len, cap) = unsafe {
        (
            read_field(obj, abi_offset(LEN)),
            read_field(obj, abi_offset(CAP)),
        )
    };
    let Some(need) = len.checked_add(extra) else {
        fatal(format_args!("a text of {len} + {extra} bytes is too large"));
    };
    // SAFETY: a live object.
    let path = unsafe { growth(text, extra, reuse) };
    stats::record(match path {
        Growth::InPlace => Event::TextKernelInPlace,
        Growth::Grown => Event::TextKernelGrown,
        Growth::Copied => Event::TextKernelCopied,
    });
    if path == Growth::InPlace {
        return (text, true);
    }
    let new = alloc_text(need.max(cap.saturating_mul(2)), layout);
    // SAFETY: both are live; the new object has room for `len` bytes and
    // does not overlap the old one.
    unsafe {
        std::ptr::copy_nonoverlapping(
            obj.add(abi_offset(BYTES)),
            new.add(abi_offset(BYTES)),
            to_usize(len),
        );
        write_field(new, abi_offset(LEN), len);
    }
    (object::ptr_to_word(new), false)
}

/// The bytes `range` of a borrowed text object as a text object of the
/// same layout, owned by the caller: `text` itself (one more reference)
/// when the range is the whole text; `text` changed in place when `reuse`
/// holds and it is unique (D-525), with its capacity kept; otherwise a new
/// object. A range outside the contents ends the process. The caller keeps
/// `String` ranges on character boundaries.
///
/// # Safety
///
/// `text` points to a live `String` or `Bytes`, borrowed for the call.
#[must_use]
pub unsafe fn keep_range(text: Word, range: std::ops::Range<usize>, reuse: bool) -> Word {
    // SAFETY: forwarded from the caller.
    let Some(layout) = (unsafe { text_layout(text) }) else {
        fatal(format_args!("{text:#x} is not a text object"));
    };
    // SAFETY: a live text object, read before any change.
    let contents = unsafe { contents(text) };
    let Some(part) = contents.get(range.clone()) else {
        fatal(format_args!(
            "the range {range:?} is outside a text of {} bytes",
            contents.len()
        ));
    };
    let kept = part.len();
    if kept == contents.len() {
        // SAFETY: a live object; the caller gets a reference of its own.
        unsafe { rc::dup(text) };
        return text;
    }
    // SAFETY: a live object.
    if reuse && unsafe { rc::is_unique(text) } {
        let obj = word_to_ptr(text);
        // SAFETY: the range lies inside the object's contents, `ptr::copy`
        // allows overlap, and nobody else holds the object.
        unsafe {
            let start = obj.add(abi_offset(BYTES));
            std::ptr::copy(start.add(range.start), start, kept);
            write_field(obj, abi_offset(LEN), to_u64(kept));
            rc::dup(text);
        }
        return text;
    }
    new_text(part, layout)
}

/// Which path adding bytes to a text object takes ([`crate::stats`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Growth {
    /// Unique with room: written in place.
    InPlace,
    /// Unique but out of room: moved to a new object of doubled capacity.
    Grown,
    /// Shared or static: copied.
    Copied,
}

/// The path adding `extra` bytes to `text` takes when `reuse` holds (see
/// [`with_room`]); [`Growth::Copied`] when it does not.
///
/// # Safety
///
/// `text` points to a live text object.
pub(crate) unsafe fn growth(text: Word, extra: u64, reuse: bool) -> Growth {
    // SAFETY: forwarded from the caller.
    if !reuse || !unsafe { rc::is_unique(text) } {
        return Growth::Copied;
    }
    let obj = word_to_ptr(text);
    // SAFETY: a live text object.
    let (len, cap) = unsafe {
        (
            read_field(obj, abi_offset(LEN)),
            read_field(obj, abi_offset(CAP)),
        )
    };
    if len.checked_add(extra).is_some_and(|need| need <= cap) {
        Growth::InPlace
    } else {
        Growth::Grown
    }
}

/// The capacity of a text object, for tests of growth.
///
/// # Safety
///
/// `text` points to a live text object.
#[cfg(test)]
pub(crate) unsafe fn capacity(text: Word) -> u64 {
    // SAFETY: forwarded from the caller.
    unsafe { read_field(word_to_ptr(text), abi_offset(CAP)) }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::alloc::live_objects;
    use crate::alloc::tests::serial;

    /// A static string object in an 8-aligned buffer, as code generation
    /// emits it.
    pub(crate) struct StaticImage(Vec<u64>);

    impl StaticImage {
        pub(crate) fn new(text: &str) -> Self {
            let words = abi::string::static_image(text)
                .as_chunks::<8>()
                .0
                .iter()
                .copied()
                .map(u64::from_le_bytes)
                .collect();
            Self(words)
        }

        pub(crate) fn word(&self) -> Word {
            object::ptr_to_word(self.0.as_ptr().cast::<u8>())
        }
    }

    #[test]
    fn heap_strings_hold_their_text() {
        let _guard = serial();
        let before = live_objects();
        for text in ["", "Hello, world!", "héllo wörld ✓", &"x".repeat(1000)] {
            let word = new_string(text);
            // SAFETY: a live string.
            unsafe {
                assert_eq!(bytes(word), Some(text.as_bytes()));
                assert_eq!(as_str(word), Some(text));
                assert_eq!(to_rust_string(word).as_deref(), Some(text));
                assert_eq!(object::load_rc(word_to_ptr(word)), 1);
                rc::drop(word);
            }
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn strings_with_spare_capacity_free_by_cap() {
        let _guard = serial();
        let before = live_objects();
        let obj = alloc_string(100);
        let word = object::ptr_to_word(obj);
        // SAFETY: a live empty string; then the only reference is dropped.
        unsafe {
            assert_eq!(bytes(word), Some(&b""[..]));
            rc::drop(word);
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn static_images_read_back_as_strings() {
        let image = StaticImage::new("Hello, world!");
        // SAFETY: a static string in a live buffer.
        unsafe {
            assert_eq!(bytes(image.word()), Some(&b"Hello, world!"[..]));
            assert_eq!(object::load_rc(word_to_ptr(image.word())), abi::RC_STATIC);
        }
    }

    #[test]
    fn non_strings_are_rejected() {
        let data = new_bytes(b"\xff\x00");
        // SAFETY: a tagged integer, static objects and a live `Bytes`.
        unsafe {
            assert_eq!(bytes(abi::tag_int(5).unwrap()), None);
            assert_eq!(bytes(object::nil()), None);
            assert_eq!(bytes(0), None);
            assert_eq!(bytes(data), None);
            assert_eq!(as_str(data), None);
            assert_eq!(text_bytes(data), Some(&b"\xff\x00"[..]));
            assert_eq!(text_bytes(object::nil()), None);
            rc::drop(data);
        }
    }

    #[test]
    fn appending_to_a_unique_text_with_room_is_in_place() {
        let before = live_objects();
        let text = object::ptr_to_word(alloc_string(8));
        // SAFETY: the test owns `text` and passes its reference along.
        unsafe {
            let same = append(text, b"abc");
            assert_eq!(same, text);
            let same = append(same, b"defgh");
            assert_eq!(same, text);
            assert_eq!(as_str(same), Some("abcdefgh"));
            // Full: grows by doubling into a new object, freeing the old.
            let grown = append(same, b"i");
            assert_eq!(capacity(grown), 16);
            assert_eq!(as_str(grown), Some("abcdefghi"));
            assert_eq!(live_objects(), before.wrapping_add(1));
            rc::drop(grown);
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn appending_to_a_shared_or_static_text_copies_it() {
        let before = live_objects();
        let shared = new_string("ab");
        let image = StaticImage::new("lit");
        // SAFETY: the test owns its references and passes them along.
        unsafe {
            rc::dup(shared);
            let copy = append(shared, b"c");
            assert_ne!(copy, shared);
            assert_eq!(as_str(copy), Some("abc"));
            assert_eq!(as_str(shared), Some("ab"));
            assert!(rc::is_unique(shared));
            let from_static = append(image.word(), b"!");
            assert_eq!(as_str(from_static), Some("lit!"));
            assert_eq!(as_str(image.word()), Some("lit"));
            let data = append(new_bytes(b"\x01"), b"\x02");
            assert_eq!(text_bytes(data), Some(&b"\x01\x02"[..]));
            assert_eq!(as_str(data), None);
            for word in [copy, shared, from_static, data] {
                rc::drop(word);
            }
        }
        assert_eq!(live_objects(), before);
    }
}
