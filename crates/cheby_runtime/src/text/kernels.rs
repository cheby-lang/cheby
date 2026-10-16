//! The C entry points of `std::string` (D-484): one per `@external(native,
//! "cheby_string_…")` of `std/string.cheby`. Arguments are borrowed and
//! results owned (D-187, D-455); `Int` is `i64`, `Bool` is `bool`, and
//! every other value a uniform word (plan §6). The module doc of
//! `text` lists the kernels that update a unique argument in place.

use super::ops::{self, Fill, int};
use crate::abi::Word;
use crate::list;
use crate::panic::fatal;
use crate::unwind::guard::{DropOnUnwind, guarded};
use crate::{alloc, rc, string, values};

/// The text of a borrowed `String` argument.
///
/// # Safety
///
/// `text` is the uniform word of a live `String`, borrowed for the call and
/// not changed while the result is used.
unsafe fn arg<'a>(text: Word) -> &'a str {
    // SAFETY: forwarded from the caller.
    unsafe { string::as_str_unchecked(text) }
        .unwrap_or_else(|| fatal(format_args!("{text:#x} is not a String")))
}

/// A byte count as a capacity.
fn cap(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The sum of byte counts. A sum that overflows is a request no memory can
/// serve: E0076 during constant evaluation, a panic otherwise
/// ([`alloc::out_of_memory`]).
fn total(parts: impl IntoIterator<Item = u64>) -> u64 {
    parts
        .into_iter()
        .try_fold(0_u64, u64::checked_add)
        .unwrap_or_else(|| alloc::out_of_memory(u64::MAX))
}

/// A new string of the code points `chars` gives, which it must give the
/// same twice: once to measure the result and once to write it, so no
/// Rust-side copy of the result is ever held (D-617).
fn build_chars<I: Iterator<Item = char>>(chars: impl Fn() -> I) -> Word {
    let size = total(chars().map(|c| cap(c.len_utf8())));
    build(size, |write| {
        // The code points are written in chunks of a fixed size.
        let mut chunk = [0_u8; 1024];
        let mut used = 0_usize;
        for c in chars() {
            if chunk.len().saturating_sub(used) < c.len_utf8() {
                write(chunk.get(..used).unwrap_or_default());
                used = 0;
            }
            let free = chunk.get_mut(used..).unwrap_or_default();
            used = used.saturating_add(c.encode_utf8(free).len());
        }
        write(chunk.get(..used).unwrap_or_default());
    })
}

/// Builds a new string of exactly `cap` bytes from the pieces `write`
/// gives, appended in place.
fn build(cap: u64, write: impl FnOnce(&mut dyn FnMut(&[u8]))) -> Word {
    let mut text = crate::object::ptr_to_word(string::alloc_string(cap));
    write(&mut |piece: &[u8]| {
        // SAFETY: `text` is the unique string built here, owned by this
        // function; `piece` lies outside it.
        text = unsafe { string::append(text, piece) };
    });
    text
}

/// `text` followed by the pieces `write` gives (`extra` bytes in all), in
/// place when `text` is unique with room and `reuse` holds (D-525).
///
/// # Safety
///
/// `text` is a live `String`, borrowed; the pieces lie outside it unless
/// `reuse` is false.
unsafe fn extend(
    text: Word,
    extra: u64,
    reuse: bool,
    write: impl FnOnce(&mut dyn FnMut(&[u8])),
) -> Word {
    // SAFETY: forwarded from the caller.
    let (mut out, reused) = unsafe { string::with_room(text, extra, reuse) };
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

/// The part `range` of a borrowed `String`, in place when it is unique and
/// `reuse` holds (D-525).
///
/// # Safety
///
/// `text` is a live `String`, borrowed; `range` lies on character
/// boundaries of its text.
unsafe fn keep(text: Word, range: std::ops::Range<usize>, reuse: bool) -> Word {
    // SAFETY: forwarded from the caller.
    unsafe { string::keep_range(text, range, reuse) }
}

/// A new list of new strings, one per part.
fn string_list<'a>(parts: impl IntoIterator<Item = &'a str>) -> Word {
    list::from_iter(parts.into_iter().map(values::string))
}

// ---------------------------------------------------------------------
// Measuring

/// `string::length`: the number of code points (D-302). O(n).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_length(text: Word) -> i64 {
    // SAFETY: forwarded from the caller.
    int(ops::length(unsafe { arg(text) }))
}

/// `string::grapheme_length`: the number of extended grapheme clusters
/// (D-302, D-310). O(n).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_grapheme_length(text: Word) -> i64 {
    // SAFETY: forwarded from the caller.
    int(ops::grapheme_length(unsafe { arg(text) }))
}

// ---------------------------------------------------------------------
// Comparing and searching

/// `string::compare`: by code points, one after another (D-136). UTF-8
/// byte order is code-point order. O(n) in the shorter string.
///
/// # Safety
///
/// `a` and `b` are live `String`s, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_compare(a: Word, b: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let (a, b) = unsafe { (arg(a), arg(b)) };
        values::order(a.as_bytes().cmp(b.as_bytes()))
    })
}

/// `string::starts_with`. O(m) in the prefix.
///
/// # Safety
///
/// `text` and `prefix` are live `String`s, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_starts_with(text: Word, prefix: Word) -> bool {
    // SAFETY: forwarded from the caller.
    unsafe { arg(text).starts_with(arg(prefix)) }
}

/// `string::ends_with`. O(m) in the suffix.
///
/// # Safety
///
/// `text` and `suffix` are live `String`s, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_ends_with(text: Word, suffix: Word) -> bool {
    // SAFETY: forwarded from the caller.
    unsafe { arg(text).ends_with(arg(suffix)) }
}

/// `string::contains`. O(n + m) (two-way search).
///
/// # Safety
///
/// `text` and `part` are live `String`s, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_contains(text: Word, part: Word) -> bool {
    // SAFETY: forwarded from the caller.
    unsafe { arg(text).contains(arg(part)) }
}

// ---------------------------------------------------------------------
// Splitting

/// `string::split` (D-315). When `separator` does not occur, the one part
/// is `text` itself, shared. O(n + m).
///
/// # Safety
///
/// `text` and `separator` are live `String`s, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_split(text: Word, separator: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let (whole, sep) = unsafe { (arg(text), arg(separator)) };
        if !sep.is_empty() && !whole.contains(sep) {
            // SAFETY: a live string; the list gets a reference of its own,
            // released if the list cannot be allocated.
            unsafe { rc::dup(text) };
            let held = DropOnUnwind::new([text]);
            let parts = list::from_iter([text]);
            held.disarm();
            return parts;
        }
        string_list(ops::split(whole, sep))
    })
}

/// `string::split_once` (D-315): `Some((before, after))` or `None`.
///
/// # Safety
///
/// `text` and `separator` are live `String`s, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_split_once(text: Word, separator: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let (text, sep) = unsafe { (arg(text), arg(separator)) };
        let Some((before, after)) = ops::split_once(text, sep) else {
            return values::option(None);
        };
        // Each new part is released if a later allocation fails.
        let first = DropOnUnwind::new([values::string(before)]);
        let second = DropOnUnwind::new([values::string(after)]);
        let ([first_word], [second_word]) = (first.words(), second.words());
        let pair = values::tuple(&[first_word, second_word]);
        first.disarm();
        second.disarm();
        let held = DropOnUnwind::new([pair]);
        let result = values::option(Some(pair));
        held.disarm();
        result
    })
}

/// `string::to_code_points` (D-302). O(n).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_to_code_points(text: Word) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(None, || string_list(ops::code_points(unsafe { arg(text) })))
}

/// `string::to_graphemes` (D-302, D-310). O(n).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_to_graphemes(text: Word) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(None, || string_list(ops::graphemes(unsafe { arg(text) })))
}

// ---------------------------------------------------------------------
// Building

/// `string::add`: `a` then `b`. In place when `a` is unique with room
/// (D-525); otherwise a new object with room: twice `a`'s capacity when
/// `a` is unique, twice its length when it is shared or static
/// (D-849, refining D-577), and at least the result's
/// length. O(n + m).
///
/// # Safety
///
/// `a` and `b` are live `String`s, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_add(a: Word, b: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let piece = unsafe { arg(b) };
        if piece.is_empty() {
            // SAFETY: a live string; the caller gets a reference of its own.
            unsafe { rc::dup(a) };
            return a;
        }
        // SAFETY: a live string; `piece` is another object's text unless
        // `a == b`, when nothing is reused.
        unsafe { extend(a, cap(piece.len()), a != b, |write| write(piece.as_bytes())) }
    })
}

/// `string::join`: the parts with `separator` between each two. Two passes
/// over the list: the exact size, then the copy. O(result).
///
/// # Safety
///
/// `parts` is a live `List<String>` and `separator` a live `String`, both
/// borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_join(parts: Word, separator: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let sep = unsafe { arg(separator) };
        let pieces = || {
            // SAFETY: a live list, borrowed for the call.
            let elements = unsafe { list::iter(parts) };
            // SAFETY: its elements are live strings while the list lives.
            elements.map(|part| unsafe { arg(part) })
        };
        let mut count = 0_usize;
        let bytes = total(pieces().map(|part| {
            count = count.saturating_add(1);
            cap(part.len())
        }));
        if count == 1 {
            // SAFETY: a live list of one live string.
            if let Some(only) = unsafe { list::iter(parts) }.next() {
                // SAFETY: the element is live; the caller gets a reference
                // of its own.
                unsafe { rc::dup(only) };
                return only;
            }
        }
        let seps = cap(sep.len()).checked_mul(cap(count.saturating_sub(1)));
        let size = total([bytes, seps.unwrap_or(u64::MAX)]);
        build(size, |write| {
            for (index, part) in pieces().enumerate() {
                if index > 0 {
                    write(sep.as_bytes());
                }
                write(part.as_bytes());
            }
        })
    })
}

/// `string::repeat`: `count` copies, `count` below 0 being 0 (D-298).
/// O(n × count).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_repeat(text: Word, count: i64) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let piece = unsafe { arg(text) };
        if count == 1 {
            // SAFETY: a live string; the caller gets a reference of its own.
            unsafe { rc::dup(text) };
            return text;
        }
        let count = if piece.is_empty() {
            0
        } else {
            ops::clamp(count)
        };
        let size = cap(piece.len())
            .checked_mul(cap(count))
            .unwrap_or_else(|| alloc::out_of_memory(u64::MAX));
        build(size, |write| {
            for _ in 0..count {
                write(piece.as_bytes());
            }
        })
    })
}

/// The bytes of a fill. A count that overflows fails as [`total`] does.
fn fill_bytes(fill: &Fill<'_>) -> u64 {
    fill.bytes()
        .unwrap_or_else(|| alloc::out_of_memory(u64::MAX))
}

/// `string::pad_start`: copies of `pad` before `text` up to `width` code
/// points (D-302, D-298). O(n + width).
///
/// # Safety
///
/// `text` and `pad` are live `String`s, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_pad_start(text: Word, width: i64, pad: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let (body, pad) = unsafe { (arg(text), arg(pad)) };
        let Some(fill) = Fill::new(ops::length(body), width, pad) else {
            // SAFETY: a live string; the caller gets a reference of its own.
            unsafe { rc::dup(text) };
            return text;
        };
        build(total([fill_bytes(&fill), cap(body.len())]), |write| {
            fill.write(|piece| write(piece.as_bytes()));
            write(body.as_bytes());
        })
    })
}

/// `string::pad_end`: copies of `pad` after `text` up to `width` code
/// points (D-302, D-298), in place when `text` is unique with room
/// (D-525). O(n + width).
///
/// # Safety
///
/// `text` and `pad` are live `String`s, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_pad_end(text: Word, width: i64, pad: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let (body, pad_text) = unsafe { (arg(text), arg(pad)) };
        let Some(fill) = Fill::new(ops::length(body), width, pad_text) else {
            // SAFETY: a live string; the caller gets a reference of its own.
            unsafe { rc::dup(text) };
            return text;
        };
        // SAFETY: a live string; the pieces are `pad`'s text, outside `text`
        // unless they are the same object, when nothing is reused.
        unsafe {
            extend(text, fill_bytes(&fill), text != pad, |write| {
                fill.write(|piece| write(piece.as_bytes()));
            })
        }
    })
}

// ---------------------------------------------------------------------
// Changing

/// `string::trim` (D-310), in place at count one (D-525).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_trim(text: Word) -> Word {
    // SAFETY: forwarded from the caller; the range lies on boundaries.
    guarded(None, || unsafe { keep(text, ops::trim(arg(text)), true) })
}

/// `string::trim_start` (D-373), in place at count one (D-525).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_trim_start(text: Word) -> Word {
    // SAFETY: forwarded from the caller; the range lies on boundaries.
    guarded(None, || unsafe {
        keep(text, ops::trim_start(arg(text)), true)
    })
}

/// `string::trim_end` (D-373), in place at count one (D-525).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_trim_end(text: Word) -> Word {
    // SAFETY: forwarded from the caller; the range lies on boundaries.
    guarded(None, || unsafe {
        keep(text, ops::trim_end(arg(text)), true)
    })
}

/// `string::remove_prefix` (D-373), in place at count one (D-525). O(m),
/// plus the move of the rest when it is done in place.
///
/// # Safety
///
/// `text` and `prefix` are live `String`s, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_remove_prefix(text: Word, prefix: Word) -> Word {
    // SAFETY: forwarded from the caller; the range lies on boundaries.
    guarded(None, || unsafe {
        let range = ops::remove_prefix(arg(text), arg(prefix));
        keep(text, range, text != prefix)
    })
}

/// `string::remove_suffix` (D-373), in place at count one (D-525). O(m).
///
/// # Safety
///
/// `text` and `suffix` are live `String`s, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_remove_suffix(text: Word, suffix: Word) -> Word {
    // SAFETY: forwarded from the caller; the range lies on boundaries.
    guarded(None, || unsafe {
        let range = ops::remove_suffix(arg(text), arg(suffix));
        keep(text, range, text != suffix)
    })
}

/// `string::lowercase`: full Unicode case mapping, the same in every locale
/// (D-310). O(n).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_lowercase(text: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let text = unsafe { arg(text) };
        build_chars(|| ops::lowercase(text))
    })
}

/// `string::uppercase`: full Unicode case mapping, such as `ß` to `SS`
/// (D-310). O(n).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_uppercase(text: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let text = unsafe { arg(text) };
        build_chars(|| ops::uppercase(text))
    })
}

/// `string::reverse`: the code points in reverse order (D-302). O(n).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_reverse(text: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let text = unsafe { arg(text) };
        build_chars(|| ops::reverse(text))
    })
}

/// `string::slice`: code points `start..end`, clamped (D-298, D-319), in
/// place at count one (D-525). O(end).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_slice(text: Word, start: i64, end: i64) -> Word {
    // SAFETY: forwarded from the caller; the range lies on boundaries.
    guarded(None, || unsafe {
        keep(text, ops::slice(arg(text), start, end), true)
    })
}

// ---------------------------------------------------------------------
// Conversions

/// `string::from_bytes`: `Ok(text)` for valid UTF-8, keeping a leading
/// byte-order mark (D-366), else `Err(Nil)` (D-297). O(n).
///
/// # Safety
///
/// `data` is a live `Bytes`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_from_bytes(data: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        match std::str::from_utf8(unsafe { values::bytes_of(data) }) {
            Ok(text) => ok_owned(values::string(text)),
            Err(_) => values::err_nil(),
        }
    })
}

/// `Ok(value)` of an owned value, which is released if the `Ok` cannot be
/// allocated.
fn ok_owned(value: Word) -> Word {
    let held = DropOnUnwind::new([value]);
    let result = values::ok(value);
    held.disarm();
    result
}

/// `string::to_code_point_values`: the scalar value of each code point.
/// O(n).
///
/// # Safety
///
/// `text` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_to_code_point_values(text: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let text = unsafe { arg(text) };
        list::from_iter(
            text.chars()
                .map(|c| values::int_word(i64::from(u32::from(c)))),
        )
    })
}

/// `string::from_code_point_values`: `Ok(text)`, or `Err(Nil)` when an
/// element is not a Unicode scalar value. O(n).
///
/// # Safety
///
/// `points` is a live `List<Int>`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_string_from_code_point_values(points: Word) -> Word {
    guarded(None, || {
        let scalars = || {
            // SAFETY: a live list, borrowed for the call.
            let elements = unsafe { list::iter(points) };
            // SAFETY: its elements are live `Int` words while the list lives.
            elements.map(|value| ops::scalar(unsafe { values::word_to_i64(value) }))
        };
        if scalars().any(|c| c.is_none()) {
            return values::err_nil();
        }
        ok_owned(build_chars(|| scalars().flatten()))
    })
}

/// One kernel: symbol, address, C parameter types and C result type.
pub(super) type Entry = (
    &'static str,
    *const u8,
    &'static [&'static str],
    &'static str,
);

/// Every `std::string` kernel.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
#[expect(clippy::too_many_lines, reason = "one table row per kernel")]
pub(super) fn all() -> [Entry; 27] {
    const W: &str = "Word";
    [
        (
            "cheby_string_length",
            cheby_string_length as *const u8,
            &[W],
            "i64",
        ),
        (
            "cheby_string_grapheme_length",
            cheby_string_grapheme_length as *const u8,
            &[W],
            "i64",
        ),
        (
            "cheby_string_compare",
            cheby_string_compare as *const u8,
            &[W, W],
            W,
        ),
        (
            "cheby_string_starts_with",
            cheby_string_starts_with as *const u8,
            &[W, W],
            "bool",
        ),
        (
            "cheby_string_ends_with",
            cheby_string_ends_with as *const u8,
            &[W, W],
            "bool",
        ),
        (
            "cheby_string_contains",
            cheby_string_contains as *const u8,
            &[W, W],
            "bool",
        ),
        (
            "cheby_string_split",
            cheby_string_split as *const u8,
            &[W, W],
            W,
        ),
        (
            "cheby_string_split_once",
            cheby_string_split_once as *const u8,
            &[W, W],
            W,
        ),
        (
            "cheby_string_to_code_points",
            cheby_string_to_code_points as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_string_to_graphemes",
            cheby_string_to_graphemes as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_string_add",
            cheby_string_add as *const u8,
            &[W, W],
            W,
        ),
        (
            "cheby_string_join",
            cheby_string_join as *const u8,
            &[W, W],
            W,
        ),
        (
            "cheby_string_repeat",
            cheby_string_repeat as *const u8,
            &[W, "i64"],
            W,
        ),
        (
            "cheby_string_pad_start",
            cheby_string_pad_start as *const u8,
            &[W, "i64", W],
            W,
        ),
        (
            "cheby_string_pad_end",
            cheby_string_pad_end as *const u8,
            &[W, "i64", W],
            W,
        ),
        ("cheby_string_trim", cheby_string_trim as *const u8, &[W], W),
        (
            "cheby_string_trim_start",
            cheby_string_trim_start as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_string_trim_end",
            cheby_string_trim_end as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_string_remove_prefix",
            cheby_string_remove_prefix as *const u8,
            &[W, W],
            W,
        ),
        (
            "cheby_string_remove_suffix",
            cheby_string_remove_suffix as *const u8,
            &[W, W],
            W,
        ),
        (
            "cheby_string_lowercase",
            cheby_string_lowercase as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_string_uppercase",
            cheby_string_uppercase as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_string_reverse",
            cheby_string_reverse as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_string_slice",
            cheby_string_slice as *const u8,
            &[W, "i64", "i64"],
            W,
        ),
        (
            "cheby_string_from_bytes",
            cheby_string_from_bytes as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_string_to_code_point_values",
            cheby_string_to_code_point_values as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_string_from_code_point_values",
            cheby_string_from_code_point_values as *const u8,
            &[W],
            W,
        ),
    ]
}
