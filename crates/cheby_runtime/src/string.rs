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
use crate::ctx::ExecCtx;
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
/// bytes. A size that overflows, or a request that cannot be served, ends
/// as [`alloc::out_of_memory`] says: E0076 when it is over constant
/// evaluation's memory limit, an `out of memory` panic otherwise.
#[must_use]
pub fn alloc_text(cap: u64, layout: LayoutId) -> *mut u8 {
    // SAFETY: null asks for the running fiber to be looked up.
    unsafe { alloc_text_for(std::ptr::null_mut(), cap, layout) }
}

/// [`alloc_text`] for `ctx`, the fiber running on this thread, or null to
/// look it up: a caller that has its fiber's control block allocates
/// without a thread-local lookup.
///
/// # Safety
///
/// As for [`alloc::alloc_for`].
#[must_use]
pub(crate) unsafe fn alloc_text_for(ctx: *mut ExecCtx, cap: u64, layout: LayoutId) -> *mut u8 {
    let Some(size) = abi::string::alloc_size(cap) else {
        alloc::out_of_memory(u64::MAX)
    };
    // SAFETY: forwarded from the caller.
    let obj = unsafe { alloc::alloc_for(ctx, size, abi::header(abi::RC_UNIQUE, 0, layout)) };
    // SAFETY: a fresh text object: `len` and `cap` are within it.
    unsafe {
        write_field(obj, abi_offset(LEN), 0);
        write_field(obj, abi_offset(CAP), cap);
    }
    obj
}

/// Allocates an empty heap string (count 1) with room for `cap` bytes.
/// Fails as [`alloc_text`] does.
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
    // SAFETY: null asks for the running fiber to be looked up.
    unsafe { new_text_for(std::ptr::null_mut(), data, layout) }
}

/// [`new_text`] for `ctx`, the fiber running on this thread, or null to
/// look it up.
///
/// # Safety
///
/// As for [`alloc::alloc_for`].
#[must_use]
pub(crate) unsafe fn new_text_for(ctx: *mut ExecCtx, data: &[u8], layout: LayoutId) -> Word {
    let len = to_u64(data.len());
    // SAFETY: forwarded from the caller.
    let obj = unsafe { alloc_text_for(ctx, len, layout) };
    // SAFETY: the object has room for `len` bytes after `BYTES`, and `data`
    // does not overlap a fresh allocation.
    unsafe {
        copy_bytes(data.as_ptr(), obj.add(abi_offset(BYTES)), data.len());
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
pub(crate) unsafe fn contents<'a>(word: Word) -> &'a [u8] {
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
/// one) and has room, otherwise a new unique one ([`grown_capacity`]: of
/// capacity `max(len + extra, 2 * cap)` when `text` was unique, `max(len +
/// extra, 2 * len)` when it is copied), after which the old one is released
/// (freed when it was unique, left alone when shared or static). Ends the
/// process if `text` is not a text object. A size that overflows fails as
/// [`alloc_text`] does.
///
/// # Safety
///
/// `text` points to a live `String` or `Bytes` whose reference the caller
/// gives up.
#[must_use]
pub unsafe fn reserve(text: Word, extra: u64) -> Word {
    // SAFETY: forwarded from the caller; null looks the fiber up.
    let (text, _, _) = unsafe { reserve_in(std::ptr::null_mut(), text, extra) };
    text
}

/// [`reserve`] for `ctx`, the fiber running on this thread, or null to
/// look it up, which allocates the new object and releases the old one.
/// Also returns the length of the contents and the path it took. Inline,
/// so an append in place costs no call; [`move_text`] does the rest.
///
/// # Safety
///
/// As for [`reserve`]; `ctx` as for [`alloc::alloc_for`].
#[inline]
unsafe fn reserve_in(ctx: *mut ExecCtx, text: Word, extra: u64) -> (Word, u64, Growth) {
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
        alloc::out_of_memory(u64::MAX)
    };
    // SAFETY: a live object.
    let unique = unsafe { rc::is_unique(text) };
    if need <= cap && unique {
        return (text, len, Growth::InPlace);
    }
    let path = if unique {
        Growth::Grown
    } else {
        Growth::Copied
    };
    // SAFETY: forwarded from the caller.
    let new = unsafe { move_text(ctx, text, layout, grown_capacity(path, len, cap, need)) };
    (new, len, path)
}

/// Moves the contents of the text object `text` to a new one of the same
/// layout with capacity `cap`, at least its length, then releases `text`.
///
/// # Safety
///
/// `text` points to a live text object of layout `layout` whose reference
/// the caller gives up; `ctx` as for [`alloc::alloc_for`].
#[inline(never)]
unsafe fn move_text(ctx: *mut ExecCtx, text: Word, layout: LayoutId, cap: u64) -> Word {
    let obj = word_to_ptr(text);
    // SAFETY: a live text object.
    let len = unsafe { read_field(obj, abi_offset(LEN)) };
    // SAFETY: forwarded from the caller.
    let new = unsafe { alloc_text_for(ctx, cap, layout) };
    // SAFETY: both are live; the new object has room for `len` bytes and
    // does not overlap the old one, whose reference the caller gave up.
    unsafe {
        std::ptr::copy_nonoverlapping(
            obj.add(abi_offset(BYTES)),
            new.add(abi_offset(BYTES)),
            to_usize(len),
        );
        write_field(new, abi_offset(LEN), len);
        rc::cheby_rt_drop(ctx, text);
    }
    object::ptr_to_word(new)
}

/// Consumes a text object and returns it followed by `piece`, in place when
/// it is unique and has room, otherwise in a new object of the capacity
/// [`reserve`] gives (twice the old capacity for a unique text, twice the
/// length for a copy). `piece` must not point into `text`'s own contents.
///
/// # Safety
///
/// As for [`reserve`].
#[must_use]
pub unsafe fn append(text: Word, piece: &[u8]) -> Word {
    // SAFETY: forwarded from the caller; null looks the fiber up.
    let (text, _) = unsafe { append_in(std::ptr::null_mut(), text, piece) };
    text
}

/// [`append`] for `ctx`, the fiber running on this thread, or null to
/// look it up, and the path it took.
///
/// # Safety
///
/// As for [`append`]; `ctx` as for [`alloc::alloc_for`].
#[must_use]
pub(crate) unsafe fn append_in(ctx: *mut ExecCtx, text: Word, piece: &[u8]) -> (Word, Growth) {
    // SAFETY: forwarded from the caller.
    let (text, len, path) = unsafe { reserve_in(ctx, text, to_u64(piece.len())) };
    // SAFETY: `text` is unique with room for `piece` after its `len` bytes,
    // and `piece` lies outside it.
    unsafe { write_at(word_to_ptr(text), len, piece) };
    (text, path)
}

/// Appends `piece` to a unique text object that has room for it: the
/// kernels' writes into the object [`alloc_text`] or [`with_room`] made
/// for exactly their pieces, with none of [`append`]'s checks of layout
/// and count. A text without room for `piece` is grown as [`append`]
/// grows it, so a size the caller got wrong costs a copy, never a write
/// past the end.
///
/// # Safety
///
/// `text` points to a live, unique `String` or `Bytes` whose reference the
/// caller gives up, and `piece` lies outside it.
#[must_use]
pub(crate) unsafe fn append_in_room(text: Word, piece: &[u8]) -> Word {
    let obj = word_to_ptr(text);
    // SAFETY: a live text object.
    let (len, cap) = unsafe {
        (
            read_field(obj, abi_offset(LEN)),
            read_field(obj, abi_offset(CAP)),
        )
    };
    if len
        .checked_add(to_u64(piece.len()))
        .is_some_and(|need| need <= cap)
    {
        // SAFETY: unique (the caller's contract) with room for `piece`
        // after its `len` bytes, and `piece` lies outside it.
        unsafe { write_at(obj, len, piece) };
        return text;
    }
    // SAFETY: forwarded from the caller.
    unsafe { append(text, piece) }
}

/// Copies `piece` after the first `len` bytes of the text object `obj`
/// and makes its length `len + piece.len()`.
///
/// # Safety
///
/// `obj` is a live text object that nobody else reads, with room for
/// `piece` after `len` bytes, and `piece` lies outside it.
#[inline]
unsafe fn write_at(obj: *mut u8, len: u64, piece: &[u8]) {
    // SAFETY: forwarded from the caller.
    unsafe {
        copy_bytes(
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
}

/// A unique text object with the contents of a borrowed `text` and room
/// for at least `extra` more bytes, and whether it is `text` itself.
///
/// It is `text` itself when `reuse` holds, `text` is unique and it has
/// room, so the caller may append in place (D-525): the caller then
/// appends ([`append`] stays in place) and only afterwards adds the
/// reference it returns with `rc::dup`. Otherwise it is a new unique object
/// that the caller owns, of capacity `max(len + extra, 2 * cap)` when
/// `text` is unique and full, so a chain of appends to a unique result
/// grows by doubling, and `max(len + extra, 2 * len)` when it is copied
/// ([`grown_capacity`]). `text` keeps its reference either way. Ends the
/// process if `text` is not a text object.
/// A size that overflows fails as [`alloc_text`] does.
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
        alloc::out_of_memory(u64::MAX)
    };
    // Copied unless it may be reused, then in place when it has room.
    // SAFETY: a live object.
    let path = if !reuse || !unsafe { rc::is_unique(text) } {
        Growth::Copied
    } else if need <= cap {
        Growth::InPlace
    } else {
        Growth::Grown
    };
    stats::record(match path {
        Growth::InPlace => Event::TextKernelInPlace,
        Growth::Grown => Event::TextKernelGrown,
        Growth::Copied => Event::TextKernelCopied,
    });
    if path == Growth::InPlace {
        return (text, true);
    }
    let new = alloc_text(grown_capacity(path, len, cap, need), layout);
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

/// Copies `len` bytes from `from` to `to`. Up to 32 bytes, the size of
/// most pieces of an interpolation, are copied inline with two loads and
/// two stores that may overlap, instead of a call to `memcpy`.
///
/// # Safety
///
/// As for [`std::ptr::copy_nonoverlapping`].
#[inline(always)]
unsafe fn copy_bytes(from: *const u8, to: *mut u8, len: usize) {
    use std::ptr::{read_unaligned, write_unaligned};
    /// Copies the first and the last `size_of::<T>()` bytes, which cover
    /// the `len` bytes when `len` is from one to two times that size.
    ///
    /// # Safety
    ///
    /// As for [`copy_bytes`], and `len` is at least `size_of::<T>()`.
    #[expect(
        clippy::inline_always,
        reason = "a step of `copy_bytes`, which must be inline"
    )]
    #[inline(always)]
    unsafe fn ends<T>(from: *const u8, to: *mut u8, len: usize) {
        let last = len.wrapping_sub(size_of::<T>());
        // SAFETY: both ends lie within the `len` bytes of each side.
        unsafe {
            let head = read_unaligned(from.cast::<T>());
            let tail = read_unaligned(from.add(last).cast::<T>());
            write_unaligned(to.cast::<T>(), head);
            write_unaligned(to.add(last).cast::<T>(), tail);
        }
    }
    // SAFETY: forwarded from the caller; each case reads and writes only
    // within the `len` bytes.
    unsafe {
        match len {
            0 => {}
            1..4 => {
                let middle = len.wrapping_div(2);
                let last = len.wrapping_sub(1);
                let (a, b, c) = (*from, *from.add(middle), *from.add(last));
                *to = a;
                *to.add(middle) = b;
                *to.add(last) = c;
            }
            4..8 => ends::<u32>(from, to, len),
            8..16 => ends::<u64>(from, to, len),
            16..=32 => ends::<u128>(from, to, len),
            _ => std::ptr::copy_nonoverlapping(from, to, len),
        }
    }
}

/// The capacity of the new object a text of `len` bytes and capacity `cap`
/// moves to when it needs room for `need >= len` bytes and `path` is not
/// [`Growth::InPlace`]. A unique text that is full grows to
/// `max(need, 2 * cap)` (D-577), so appends to a unique accumulator cost
/// amortized O(1) per byte. A copy, of a shared or static text or under
/// D-563's alias rule, gets `max(need, 2 * len)`: its old capacity says
/// nothing about the copy, and a text that stays shared is copied at every
/// append, so doubling the old capacity each time grew the capacity, not
/// the contents, exponentially (out of memory after about 47 one-byte
/// appends to a record's string field, gate 6's ROB6-3)
/// (D-849, refining D-577).
fn grown_capacity(path: Growth, len: u64, cap: u64, need: u64) -> u64 {
    match path {
        Growth::Grown => need.max(cap.saturating_mul(2)),
        Growth::InPlace | Growth::Copied => need.max(len.saturating_mul(2)),
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

    /// A text copied because it is shared gets a capacity from its length,
    /// not from its old capacity: 64 one-byte appends to a text that is
    /// shared at every step, through `append` and through `with_room`, end
    /// with a capacity below twice the length. Doubling the old capacity
    /// ran out of memory after about 47 steps (gate 6's ROB6-3).
    #[test]
    fn copying_a_shared_text_sizes_it_by_its_length() {
        // SAFETY: the test owns its references and passes them along.
        unsafe {
            let mut text = new_string("");
            for _ in 0..64 {
                rc::dup(text);
                let next = append(text, b"!");
                assert_ne!(next, text, "a shared text is copied");
                rc::drop(text);
                text = next;
            }
            assert_eq!(bytes(text).map(<[u8]>::len), Some(64));
            assert!(capacity(text) <= 128, "capacity {}", capacity(text));
            for _ in 0..64 {
                let (next, same) = with_room(text, 1, false);
                assert!(!same, "no reuse: a copy");
                let next = append(next, b"?");
                rc::drop(text);
                text = next;
            }
            assert_eq!(bytes(text).map(<[u8]>::len), Some(128));
            assert!(capacity(text) <= 256, "capacity {}", capacity(text));
            rc::drop(text);
        }
    }
}
