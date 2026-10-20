//! The C entry points of `std::bytes` (D-484): one per `@external(native,
//! "cheby_bytes_…")` of `std/bytes.cheby`. Arguments are borrowed and
//! results owned (D-187, D-455); `Int` is `i64`, the sized integers their
//! C types, and every other value a uniform word (plan §6). The module
//! doc of `bytes` lists the kernels that update a unique argument in
//! place.

use crate::abi::Word;
use crate::object::{ptr_to_word, to_u64};
use crate::panic::fatal;
use crate::unwind::guard::guarded;
use crate::{alloc, list, rc, string, values};

/// The bytes of a borrowed `Bytes` argument.
///
/// # Safety
///
/// `data` is the uniform word of a live `Bytes`, borrowed for the call and
/// not changed while the result is used.
unsafe fn arg<'a>(data: Word) -> &'a [u8] {
    // SAFETY: forwarded from the caller.
    unsafe { values::bytes_of(data) }
}

/// A position argument as an index into `len` bytes, if it is one.
fn index(position: i64, len: usize) -> Option<usize> {
    usize::try_from(position).ok().filter(|&i| i < len)
}

/// A bound of `slice`, clamped to `0..=len` (D-298, D-334).
fn clamp(bound: i64, len: usize) -> usize {
    usize::try_from(bound).map_or(0, |bound| bound.min(len))
}

/// The `N` bytes of `data` at `offset`, or `None` when `offset` is
/// negative or `offset + N` passes the end (D-318, D-334).
fn window<const N: usize>(data: &[u8], offset: i64) -> Option<[u8; N]> {
    let start = usize::try_from(offset).ok()?;
    let end = start.checked_add(N)?;
    data.get(start..end)?.try_into().ok()
}

/// `data` followed by the pieces `write` gives (`extra` bytes in all), in
/// place when `data` is unique with room and `reuse` holds (D-316, D-525):
/// the in-place result is `data` with one more reference for the caller.
/// Otherwise a new object with room: twice `data`'s capacity when it is
/// unique, so a chain of pushes on a unique value grows by doubling and
/// runs in amortized O(1) per byte, and twice its length when it is shared
/// or static (D-849, refining D-577); at least what the
/// result needs.
///
/// # Safety
///
/// `data` is a live `Bytes`, borrowed; the pieces lie outside it unless
/// `reuse` is false.
unsafe fn extend(
    data: Word,
    extra: usize,
    reuse: bool,
    write: impl FnOnce(&mut dyn FnMut(&[u8])),
) -> Word {
    // SAFETY: forwarded from the caller.
    let (mut out, reused) = unsafe { string::with_room(data, to_u64(extra), reuse) };
    write(&mut |piece: &[u8]| {
        // SAFETY: `out` is unique with room for every piece (`with_room`),
        // so `append` writes in place, and the pieces lie outside it.
        out = unsafe { string::append(out, piece) };
    });
    if reused {
        // SAFETY: `out` is the live argument, changed in place; the caller
        // gets a reference of its own (D-525).
        unsafe { rc::dup(out) };
    }
    out
}

/// `data` with the fixed bytes `piece` added at the end, in place at count
/// one (D-316).
///
/// # Safety
///
/// `data` is a live `Bytes`, borrowed.
unsafe fn push_bytes(data: Word, piece: &[u8]) -> Word {
    // SAFETY: forwarded from the caller; `piece` is on the Rust stack.
    unsafe { extend(data, piece.len(), true, |write| write(piece)) }
}

// ---------------------------------------------------------------------
// Creating

/// `bytes::new`: an empty `Bytes`. O(1).
#[unsafe(no_mangle)]
pub extern "C" fn cheby_bytes_new() -> Word {
    guarded(None, || string::new_bytes(&[]))
}

/// `bytes::from_list`: the bytes of a `List<U8>`, in order. O(n).
///
/// # Safety
///
/// `values` is a live `List<U8>`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_bytes_from_list(values: Word) -> Word {
    guarded(None, || {
        // SAFETY: a live list, borrowed for the call; its elements are `U8`
        // words, always tagged.
        let elements = unsafe { list::iter(values) };
        // The result is written straight into its object, in chunks of a
        // fixed size, so no Rust-side copy of it is ever held (D-617).
        let mut out = ptr_to_word(string::alloc_bytes(to_u64(elements.len())));
        let mut chunk = [0_u8; 1024];
        let mut used = 0_usize;
        for byte in elements.map(values::word_to_u8) {
            if used == chunk.len() {
                // SAFETY: `out` is the unique `Bytes` built here; the chunk
                // lies outside it.
                out = unsafe { string::append(out, &chunk) };
                used = 0;
            }
            if let Some(slot) = chunk.get_mut(used) {
                *slot = byte;
            }
            used = used.saturating_add(1);
        }
        // SAFETY: as above.
        unsafe { string::append(out, chunk.get(..used).unwrap_or_default()) }
    })
}

/// `bytes::from_string`: the UTF-8 encoding of `text`, with no byte-order
/// mark. O(n).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_bytes_from_string(text: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let text = unsafe { string::bytes(text) }
            .unwrap_or_else(|| fatal(format_args!("{text:#x} is not a String")));
        string::new_bytes(text)
    })
}

// ---------------------------------------------------------------------
// Reading

/// `bytes::length` (D-301). O(1).
///
/// # Safety
///
/// `data` is a live `Bytes`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_bytes_length(data: Word) -> i64 {
    // SAFETY: forwarded from the caller.
    i64::try_from(unsafe { arg(data) }.len()).unwrap_or(i64::MAX)
}

/// `bytes::get`: `Some(byte)`, or `None` outside `0..length` (D-318).
/// O(1). Both results are static objects, so it allocates nothing and
/// never raises (D-902).
///
/// # Safety
///
/// `data` is a live `Bytes`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_bytes_get(data: Word, position: i64) -> Word {
    // SAFETY: forwarded from the caller.
    let data = unsafe { arg(data) };
    index(position, data.len())
        .and_then(|i| data.get(i))
        .map_or_else(values::none, |&byte| values::some_u8(byte))
}

/// `bytes::json_plain_end`, package-visible for `std::json`'s parser
/// (D-900, D-904): the offset of the first byte at or after `from` that is
/// `"`, `\` or below 0x20, or the length when there is none. `from` is
/// clamped to `0..=length` as `slice` clamps its bounds (D-298, D-334).
/// Every byte of a multi-byte UTF-8 sequence is 0x80 or more, so none of
/// them stops the scan. O(k) in the bytes scanned; it allocates nothing
/// and never raises.
///
/// # Safety
///
/// `data` is a live `Bytes`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_bytes_json_plain_end(data: Word, from: i64) -> i64 {
    // SAFETY: forwarded from the caller.
    let data = unsafe { arg(data) };
    let start = clamp(from, data.len());
    let rest = data.get(start..).unwrap_or_default();
    let found = rest
        .iter()
        .position(|&byte| byte < 0x20 || byte == b'"' || byte == b'\\')
        .unwrap_or(rest.len());
    i64::try_from(start.saturating_add(found)).unwrap_or(i64::MAX)
}

/// `bytes::slice`: positions `start..end`, clamped (D-298, D-319, D-334),
/// in place at count one (D-525). O(end - start); the whole of `data` is
/// `data` itself, shared.
///
/// # Safety
///
/// `data` is a live `Bytes`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_bytes_slice(data: Word, start: i64, end: i64) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let len = unsafe { arg(data) }.len();
        let (start, end) = (clamp(start, len), clamp(end, len));
        let range = if end > start { start..end } else { 0..0 };
        // SAFETY: a live `Bytes`; the range lies inside it.
        unsafe { string::keep_range(data, range, true) }
    })
}

/// `bytes::to_list`: every byte, in order, as a `List<U8>`. O(n).
///
/// # Safety
///
/// `data` is a live `Bytes`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_bytes_to_list(data: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        list::from_iter(
            unsafe { arg(data) }
                .iter()
                .map(|&byte| values::u8_word(byte)),
        )
    })
}

// ---------------------------------------------------------------------
// Building

/// `bytes::push`: `data` with `byte` at the end, in place at count one
/// (D-316). Amortized O(1) when unique, O(n) when shared.
///
/// # Safety
///
/// `data` is a live `Bytes`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_bytes_push(data: Word, byte: u8) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(None, || unsafe { push_bytes(data, &[byte]) })
}

/// `bytes::append`: `data` then `other`, in place when `data` is unique
/// with room and `other` is another object (D-316, D-563's alias rule).
/// Amortized O(m) when unique, O(n + m) when shared.
///
/// # Safety
///
/// `data` and `other` are live `Bytes`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_bytes_append(data: Word, other: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let piece = unsafe { arg(other) };
        if piece.is_empty() {
            // SAFETY: a live `Bytes`; the caller gets a reference of its own.
            unsafe { rc::dup(data) };
            return data;
        }
        // SAFETY: a live `Bytes`; `piece` is another object's bytes unless
        // `data == other`, when nothing is reused.
        unsafe { extend(data, piece.len(), data != other, |write| write(piece)) }
    })
}

/// `bytes::concat`: every part, in order. Two passes over the list: the
/// exact size, then the copy; a list of one part gives that part, shared.
/// O(result).
///
/// # Safety
///
/// `parts` is a live `List<Bytes>`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_bytes_concat(parts: Word) -> Word {
    guarded(None, || {
        let pieces = || {
            // SAFETY: a live list, borrowed for the call.
            let elements = unsafe { list::iter(parts) };
            // SAFETY: its elements are live `Bytes` while the list lives.
            elements.map(|part| (part, unsafe { arg(part) }))
        };
        let mut count = 0_usize;
        let size = pieces()
            .map(|(_, piece)| {
                count = count.saturating_add(1);
                to_u64(piece.len())
            })
            .try_fold(0_u64, u64::checked_add)
            .unwrap_or_else(|| alloc::out_of_memory(u64::MAX));
        if count == 1
            && let Some((only, _)) = pieces().next()
        {
            // SAFETY: the element is live; the caller gets a reference of
            // its own.
            unsafe { rc::dup(only) };
            return only;
        }
        let mut out = crate::object::ptr_to_word(string::alloc_bytes(size));
        for (_, piece) in pieces() {
            // SAFETY: `out` is the unique `Bytes` built here, with room for
            // every piece; the pieces lie outside it.
            out = unsafe { string::append(out, piece) };
        }
        out
    })
}

// ---------------------------------------------------------------------
// Integers in a byte order (D-334)

/// `bytes::push_i8`: one byte in two's complement, in place at count one.
///
/// # Safety
///
/// `data` is a live `Bytes`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_bytes_push_i8(data: Word, value: i8) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(None, || unsafe { push_bytes(data, &value.to_be_bytes()) })
}

/// `bytes::read_i8`: the byte at `offset` in two's complement, or `None`
/// outside `0..length` (D-318). O(1). Both results are static objects, so
/// it allocates nothing and never raises (D-902).
///
/// # Safety
///
/// `data` is a live `Bytes`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_bytes_read_i8(data: Word, offset: i64) -> Word {
    // SAFETY: forwarded from the caller.
    let data = unsafe { arg(data) };
    window::<1>(data, offset).map_or_else(values::none, |bytes| {
        values::some_i8(i8::from_be_bytes(bytes))
    })
}

/// Defines the push and read kernels of one integer type in both byte
/// orders: `push_<be>`, `push_<le>`, `read_<be>`, `read_<le>`.
macro_rules! ordered {
    ($ty:ty, $width:literal, $word:path,
     $push_be:ident, $push_le:ident, $read_be:ident, $read_le:ident) => {
        #[doc = concat!(
                            "`bytes::", stringify!($push_be), "`: `value` as ", stringify!($width),
                            " bytes, big-endian, in place at count one (D-316, D-334).\n\n",
                            "# Safety\n\n`data` is a live `Bytes`, borrowed."
                        )]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $push_be(data: Word, value: $ty) -> Word {
            // SAFETY: forwarded from the caller.
            guarded(None, || unsafe { push_bytes(data, &value.to_be_bytes()) })
        }

        #[doc = concat!(
                            "`bytes::", stringify!($push_le), "`: `value` as ", stringify!($width),
                            " bytes, little-endian, in place at count one (D-316, D-334).\n\n",
                            "# Safety\n\n`data` is a live `Bytes`, borrowed."
                        )]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $push_le(data: Word, value: $ty) -> Word {
            // SAFETY: forwarded from the caller.
            guarded(None, || unsafe { push_bytes(data, &value.to_le_bytes()) })
        }

        #[doc = concat!(
                            "`bytes::", stringify!($read_be), "`: the ", stringify!($width),
                            " bytes at `offset`, big-endian, or `None` when `offset` is negative ",
                            "or they pass the end (D-318, D-334). O(1).\n\n",
                            "# Safety\n\n`data` is a live `Bytes`, borrowed."
                        )]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $read_be(data: Word, offset: i64) -> Word {
            guarded(None, || {
                // SAFETY: forwarded from the caller.
                let data = unsafe { arg(data) };
                values::option(window::<$width>(data, offset).map(|b| $word(<$ty>::from_be_bytes(b))))
            })
        }

        #[doc = concat!(
                            "`bytes::", stringify!($read_le), "`: the ", stringify!($width),
                            " bytes at `offset`, little-endian, or `None` when `offset` is ",
                            "negative or they pass the end (D-318, D-334). O(1).\n\n",
                            "# Safety\n\n`data` is a live `Bytes`, borrowed."
                        )]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $read_le(data: Word, offset: i64) -> Word {
            guarded(None, || {
                // SAFETY: forwarded from the caller.
                let data = unsafe { arg(data) };
                values::option(window::<$width>(data, offset).map(|b| $word(<$ty>::from_le_bytes(b))))
            })
        }
    };
}

ordered!(
    u16,
    2,
    values::u16_word,
    cheby_bytes_push_u16_be,
    cheby_bytes_push_u16_le,
    cheby_bytes_read_u16_be,
    cheby_bytes_read_u16_le
);
ordered!(
    i16,
    2,
    values::i16_word,
    cheby_bytes_push_i16_be,
    cheby_bytes_push_i16_le,
    cheby_bytes_read_i16_be,
    cheby_bytes_read_i16_le
);
ordered!(
    u32,
    4,
    values::u32_word,
    cheby_bytes_push_u32_be,
    cheby_bytes_push_u32_le,
    cheby_bytes_read_u32_be,
    cheby_bytes_read_u32_le
);
ordered!(
    i32,
    4,
    values::i32_word,
    cheby_bytes_push_i32_be,
    cheby_bytes_push_i32_le,
    cheby_bytes_read_i32_be,
    cheby_bytes_read_i32_le
);
ordered!(
    u64,
    8,
    values::u64_word,
    cheby_bytes_push_u64_be,
    cheby_bytes_push_u64_le,
    cheby_bytes_read_u64_be,
    cheby_bytes_read_u64_le
);
ordered!(
    i64,
    8,
    values::int_word,
    cheby_bytes_push_i64_be,
    cheby_bytes_push_i64_le,
    cheby_bytes_read_i64_be,
    cheby_bytes_read_i64_le
);

/// One kernel: symbol, address, C parameter types and C result type.
pub(super) type Entry = (
    &'static str,
    *const u8,
    &'static [&'static str],
    &'static str,
);

/// Every `std::bytes` kernel, in the order of `std/bytes.cheby`.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
#[expect(clippy::too_many_lines, reason = "one table row per kernel")]
pub(super) fn all() -> Vec<Entry> {
    const W: &str = "Word";
    const I: &str = "i64";
    /// One integer type's four kernels, in the order of `std/bytes.cheby`:
    /// push big-endian, push little-endian, read big-endian, read
    /// little-endian.
    macro_rules! family {
        ($ty:literal, $push_be:ident, $push_le:ident, $read_be:ident, $read_le:ident) => {
            [
                (
                    stringify!($push_be),
                    $push_be as *const u8,
                    &[W, $ty][..],
                    W,
                ),
                (
                    stringify!($push_le),
                    $push_le as *const u8,
                    &[W, $ty][..],
                    W,
                ),
                (stringify!($read_be), $read_be as *const u8, &[W, I][..], W),
                (stringify!($read_le), $read_le as *const u8, &[W, I][..], W),
            ]
        };
    }
    let fixed: [Entry; 13] = [
        ("cheby_bytes_new", cheby_bytes_new as *const u8, &[], W),
        (
            "cheby_bytes_from_list",
            cheby_bytes_from_list as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_bytes_from_string",
            cheby_bytes_from_string as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_bytes_length",
            cheby_bytes_length as *const u8,
            &[W],
            I,
        ),
        ("cheby_bytes_get", cheby_bytes_get as *const u8, &[W, I], W),
        (
            "cheby_bytes_json_plain_end",
            cheby_bytes_json_plain_end as *const u8,
            &[W, I],
            I,
        ),
        (
            "cheby_bytes_slice",
            cheby_bytes_slice as *const u8,
            &[W, I, I],
            W,
        ),
        (
            "cheby_bytes_to_list",
            cheby_bytes_to_list as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_bytes_push",
            cheby_bytes_push as *const u8,
            &[W, "u8"],
            W,
        ),
        (
            "cheby_bytes_append",
            cheby_bytes_append as *const u8,
            &[W, W],
            W,
        ),
        (
            "cheby_bytes_concat",
            cheby_bytes_concat as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_bytes_push_i8",
            cheby_bytes_push_i8 as *const u8,
            &[W, "i8"],
            W,
        ),
        (
            "cheby_bytes_read_i8",
            cheby_bytes_read_i8 as *const u8,
            &[W, I],
            W,
        ),
    ];
    let families = [
        family!(
            "u16",
            cheby_bytes_push_u16_be,
            cheby_bytes_push_u16_le,
            cheby_bytes_read_u16_be,
            cheby_bytes_read_u16_le
        ),
        family!(
            "i16",
            cheby_bytes_push_i16_be,
            cheby_bytes_push_i16_le,
            cheby_bytes_read_i16_be,
            cheby_bytes_read_i16_le
        ),
        family!(
            "u32",
            cheby_bytes_push_u32_be,
            cheby_bytes_push_u32_le,
            cheby_bytes_read_u32_be,
            cheby_bytes_read_u32_le
        ),
        family!(
            "i32",
            cheby_bytes_push_i32_be,
            cheby_bytes_push_i32_le,
            cheby_bytes_read_i32_be,
            cheby_bytes_read_i32_le
        ),
        family!(
            "u64",
            cheby_bytes_push_u64_be,
            cheby_bytes_push_u64_le,
            cheby_bytes_read_u64_be,
            cheby_bytes_read_u64_le
        ),
        family!(
            "i64",
            cheby_bytes_push_i64_be,
            cheby_bytes_push_i64_le,
            cheby_bytes_read_i64_be,
            cheby_bytes_read_i64_le
        ),
    ];
    fixed
        .into_iter()
        .chain(families.into_iter().flatten())
        .collect()
}
