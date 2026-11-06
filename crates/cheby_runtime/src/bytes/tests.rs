//! Tests of the `std::bytes` kernels: one unit test per kernel through its
//! C entry point, following `docs/stdlib/bytes.md` (positions D-318,
//! clamping D-298 and D-334, byte orders D-334), property tests of every
//! integer width and byte order and of `slice`, `append` and `concat`
//! against `Vec<u8>`, the in-place paths at count one (D-316, D-525), and
//! the multi-version state machine of plan §6.
//!
//! Every test runs with poisoning on (every `cfg(test)` build) and checks
//! that its thread's accounting domain returns to where it started.

#![expect(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "test arithmetic and indexing on small, known values"
)]

use proptest::prelude::*;

use super::kernels::*;
use crate::abi::{self, Word};
use crate::alloc::live;
use crate::object::{load_rc, word_to_ptr};
use crate::string::{self, new_bytes, new_string};
use crate::{list, rc, values};

// ---------------------------------------------------------------------
// Helpers

/// Runs `f` and checks that it leaves the domain as it found it.
/// A property's cases: `normal`, or 4 under Miri, as the Miri job's
/// `PROPTEST_CASES`, which an explicit count would override (D-721,
/// D-884).
const fn miri_cases(normal: u32) -> u32 {
    if cfg!(miri) { 4 } else { normal }
}

fn balanced(f: impl FnOnce()) {
    let before = live();
    f();
    assert_eq!(live(), before, "objects leaked or freed twice");
}

/// The bytes of a borrowed `Bytes` word.
fn data(word: Word) -> Vec<u8> {
    // SAFETY: tests pass live `Bytes`.
    let bytes = unsafe { string::text_bytes(word) }.unwrap().to_vec();
    // SAFETY: as above.
    assert_eq!(
        unsafe { string::bytes(word) },
        None,
        "a `Bytes` is no `String`"
    );
    bytes
}

/// Releases an owned word.
fn release(word: Word) {
    // SAFETY: tests own the words they release.
    unsafe { rc::drop(word) };
}

/// The bytes of an owned `Bytes` word, which is released.
fn take(word: Word) -> Vec<u8> {
    let out = data(word);
    release(word);
    out
}

/// The payload of an owned `Option` word, read by `read`, which is
/// released.
fn take_option<T>(word: Word, read: impl FnOnce(Word) -> T) -> Option<T> {
    // SAFETY: a live `Option`.
    let out = unsafe { values::read_option(word) }.map(read);
    release(word);
    out
}

/// A unique `Bytes` holding `bytes`, with room for `extra` more.
fn roomy(bytes: &[u8], extra: usize) -> Word {
    let word = crate::object::ptr_to_word(string::alloc_bytes((bytes.len() + extra) as u64));
    // SAFETY: the unique `Bytes` built here.
    unsafe { string::append(word, bytes) }
}

/// Calls `kernel` on `data` as its wrapper does (borrowed, then dropped)
/// and returns the result.
fn as_wrapper(data: Word, kernel: impl FnOnce(Word) -> Word) -> Word {
    let result = kernel(data);
    release(data);
    result
}

/// A list of `U8` words.
fn u8_list(bytes: &[u8]) -> Word {
    list::from_iter(bytes.iter().map(|&b| values::u8_word(b)))
}

/// `kernel(bytes, offset)` for a read kernel, with its payload as an
/// `i128` (every width fits).
fn read(
    kernel: unsafe extern "C" fn(Word, i64) -> Word,
    bytes: &[u8],
    offset: i64,
    payload: fn(Word) -> i128,
) -> Option<i128> {
    let word = new_bytes(bytes);
    // SAFETY: a live `Bytes`, borrowed.
    let out = take_option(unsafe { kernel(word, offset) }, payload);
    release(word);
    out
}

fn unsigned(word: Word) -> i128 {
    // SAFETY: a live `U64` or smaller unsigned word.
    i128::from(unsafe { values::word_to_u64(word) })
}

fn signed(word: Word) -> i128 {
    // SAFETY: a live `I64` or smaller signed word.
    i128::from(unsafe { values::word_to_i64(word) })
}

// ---------------------------------------------------------------------
// Creating

#[test]
fn new_is_empty() {
    balanced(|| {
        let empty = cheby_bytes_new();
        // SAFETY: a live `Bytes`.
        assert_eq!(unsafe { cheby_bytes_length(empty) }, 0);
        assert_eq!(take(empty), b"");
    });
}

#[test]
fn from_list_keeps_the_order() {
    balanced(|| {
        let values = u8_list(&[0xFF, 0x00, 0x7F]);
        // SAFETY: a live list, borrowed.
        let word = unsafe { cheby_bytes_from_list(values) };
        // SAFETY: a live `Bytes`.
        assert_eq!(unsafe { cheby_bytes_length(word) }, 3);
        assert_eq!(take(word), [0xFF, 0x00, 0x7F]);
        release(values);
        let empty = list::from_iter([]);
        // SAFETY: as above.
        assert_eq!(take(unsafe { cheby_bytes_from_list(empty) }), b"");
        release(empty);
    });
}

#[test]
fn from_string_encodes_utf8() {
    balanced(|| {
        for text in ["hi", "café", "", "\u{feff}x"] {
            let word = new_string(text);
            // SAFETY: a live `String`, borrowed.
            assert_eq!(
                take(unsafe { cheby_bytes_from_string(word) }),
                text.as_bytes()
            );
            release(word);
        }
    });
}

// ---------------------------------------------------------------------
// Reading

#[test]
fn get_reads_one_byte_or_none() {
    let byte = |word| i128::from(values::word_to_u8(word));
    balanced(|| {
        let bytes = [10, 20, 30];
        assert_eq!(read(cheby_bytes_get, &bytes, 1, byte), Some(20));
        assert_eq!(read(cheby_bytes_get, &bytes, 0, byte), Some(10));
        assert_eq!(read(cheby_bytes_get, &bytes, 2, byte), Some(30));
        for outside in [3, -1, i64::MIN, i64::MAX] {
            assert_eq!(read(cheby_bytes_get, &bytes, outside, byte), None);
        }
        assert_eq!(read(cheby_bytes_get, &[], 0, byte), None);
    });
}

/// `get` and `read_i8` return the static `Some` of their byte (D-902):
/// nothing is allocated, a call gives the same word every time, counting
/// never writes it, and `U8` and `I8` of one value share the object.
#[test]
fn get_and_read_i8_return_static_somes() {
    let bytes: Vec<u8> = (0..=255).collect();
    let data = new_bytes(&bytes);
    let before = live();
    for (i, &byte) in bytes.iter().enumerate() {
        let position = i as i64;
        // SAFETY: a live `Bytes`, borrowed, twice.
        let (got, again) = unsafe {
            (
                cheby_bytes_get(data, position),
                cheby_bytes_get(data, position),
            )
        };
        assert_eq!(got, again, "one static object per byte");
        // SAFETY: the word of a live object.
        assert_eq!(unsafe { values::layout_of(got) }, Some(abi::layout::SOME));
        // SAFETY: as above.
        assert_eq!(unsafe { load_rc(word_to_ptr(got)) }, abi::RC_STATIC);
        // SAFETY: a live `Option<U8>`; counting a static object writes
        // nothing.
        unsafe {
            assert_eq!(values::read_option(got).map(values::word_to_u8), Some(byte));
            rc::dup(got);
            rc::drop(got);
            rc::drop(got);
            assert_eq!(load_rc(word_to_ptr(got)), abi::RC_STATIC);
        }
        // SAFETY: a live `Bytes`, borrowed.
        let signed = unsafe { cheby_bytes_read_i8(data, position) };
        // SAFETY: a live `Option<I8>`.
        let value = unsafe { values::read_option(signed) }.map(|word| {
            // SAFETY: a live `I8` word.
            unsafe { values::word_to_i64(word) }
        });
        assert_eq!(value, Some(i64::from(byte.cast_signed())));
        if byte < 128 {
            assert_eq!(signed, got, "a U8 and an I8 of one value share it");
        }
    }
    // SAFETY: a live `Bytes`, borrowed.
    let outside = unsafe { cheby_bytes_get(data, 256) };
    assert_eq!(outside, values::none());
    assert_eq!(live(), before, "get and read_i8 allocate nothing");
    release(data);
}

/// `json_plain_end` stops at `"`, `\` and every byte below 0x20, never at
/// a byte of 0x80 or more, clamps `from` as `slice` clamps its bounds, and
/// allocates nothing (D-904).
#[test]
fn json_plain_end_stops_at_json_specials_and_clamps() {
    let end = |bytes: &[u8], from: i64| {
        let word = new_bytes(bytes);
        let before = live();
        // SAFETY: a live `Bytes`, borrowed.
        let got = unsafe { cheby_bytes_json_plain_end(word, from) };
        assert_eq!(live(), before, "json_plain_end allocates nothing");
        release(word);
        got
    };
    balanced(|| {
        let text = "ab\"c\\d\u{1}é".as_bytes();
        for (from, want) in [
            (0, 2),
            (3, 4),
            (5, 6),
            (7, 9),
            (9, 9),
            (99, 9),
            (-4, 2),
            (i64::MIN, 2),
            (i64::MAX, 9),
        ] {
            assert_eq!(end(text, from), want, "from {from}");
        }
        assert_eq!(end(&[], 0), 0);
        assert_eq!(end(&[], -1), 0);
        assert_eq!(end(&[], 5), 0);
        for byte in 0..=255_u8 {
            let stops = byte < 0x20 || byte == b'"' || byte == b'\\';
            let passed = i64::from(!stops);
            assert_eq!(end(&[byte], 0), passed, "{byte:#x}");
            assert_eq!(end(&[b'a', byte, b'"'], 0), 1 + passed, "{byte:#x}");
        }
    });
}

/// `index_of(data, part, from)` through its entry point: the payload of
/// its `Option<Int>`.
fn index_of(data: &[u8], part: &[u8], from: i64) -> Option<i64> {
    let (data, part) = (new_bytes(data), new_bytes(part));
    // SAFETY: live `Bytes`, borrowed.
    let found = take_option(unsafe { cheby_bytes_index_of(data, part, from) }, |w| {
        // SAFETY: a live `Int` word.
        unsafe { values::word_to_i64(w) }
    });
    release(data);
    release(part);
    found
}

/// The first occurrence of `part` at or after the clamped `from`, by
/// trying every position: the reference for `index_of`.
fn naive_index_of(data: &[u8], part: &[u8], from: i64) -> Option<i64> {
    let start = from.clamp(0, data.len() as i64) as usize;
    (start..=data.len())
        .find(|&at| data.get(at..).is_some_and(|rest| rest.starts_with(part)))
        .map(|at| at as i64)
}

/// `index_of` finds the first occurrence at or after `from`, clamps
/// `from` as `slice` clamps its bounds, and finds an empty part at the
/// clamped `from` (D-1094).
#[test]
fn index_of_finds_a_part_at_or_after_from() {
    balanced(|| {
        let request = b"GET / HTTP/1.1\r\n\r\nbody";
        let blank = b"\r\n\r\n";
        assert_eq!(index_of(request, blank, 0), Some(14));
        assert_eq!(index_of(request, b" ", 4), Some(5));
        assert_eq!(index_of(request, blank, 15), None);
        assert_eq!(index_of(request, b"", 3), Some(3));
        assert_eq!(index_of(request, b"body", i64::MIN), Some(18));
        assert_eq!(index_of(request, b"", i64::MAX), Some(22));
        assert_eq!(index_of(request, b"", -5), Some(0));
        assert_eq!(index_of(request, b"y", 22), None);
        assert_eq!(index_of(request, b"bodyx", 0), None);
        assert_eq!(index_of(b"", b"", 0), Some(0));
        assert_eq!(index_of(b"", b"a", 0), None);
        assert_eq!(index_of(b"aaab", b"aab", 0), Some(1));
        assert_eq!(index_of(&[0xFF, 0x00, 0xFF], &[0xFF], 1), Some(2));
    });
}

proptest! {
    #![proptest_config(ProptestConfig { cases: miri_cases(256), ..ProptestConfig::default() })]

    /// `index_of` agrees with a search at every position, over a small
    /// alphabet so that parts occur often.
    #[test]
    fn index_of_matches_a_naive_search(
        data in proptest::collection::vec(0_u8..3, 0..40),
        part in proptest::collection::vec(0_u8..3, 0..4),
        from in -3_i64..45,
    ) {
        balanced(|| {
            assert_eq!(index_of(&data, &part, from), naive_index_of(&data, &part, from));
        });
    }
}

#[test]
fn slice_clamps_its_bounds() {
    balanced(|| {
        let bytes = [1, 2, 3, 4];
        for (start, end, want) in [
            (1, 3, &[2, 3][..]),
            (2, 100, &[3, 4]),
            (-2, 1, &[1]),
            (3, 1, &[]),
            (i64::MIN, i64::MAX, &[1, 2, 3, 4]),
            (4, 4, &[]),
        ] {
            let word = new_bytes(&bytes);
            // SAFETY: a live `Bytes`, borrowed.
            assert_eq!(take(unsafe { cheby_bytes_slice(word, start, end) }), want);
            release(word);
        }
    });
}

#[test]
fn slice_of_the_whole_shares_the_value() {
    balanced(|| {
        let word = new_bytes(&[1, 2]);
        // SAFETY: a live `Bytes`, borrowed; the test keeps its reference.
        unsafe { rc::dup(word) };
        let out = as_wrapper(word, |w| unsafe { cheby_bytes_slice(w, -5, 9) });
        assert_eq!(out, word);
        release(out);
        release(word);
    });
}

#[test]
fn to_list_gives_every_byte() {
    balanced(|| {
        let word = new_bytes(&[0x01, 0xFE]);
        // SAFETY: a live `Bytes`, borrowed.
        let out = unsafe { cheby_bytes_to_list(word) };
        // SAFETY: a live list of `U8` words.
        let back: Vec<u8> = unsafe { list::iter(out) }.map(values::word_to_u8).collect();
        assert_eq!(back, [0x01, 0xFE]);
        release(out);
        release(word);
        let empty = cheby_bytes_new();
        // SAFETY: as above.
        let out = unsafe { cheby_bytes_to_list(empty) };
        // SAFETY: a live list.
        assert_eq!(unsafe { list::iter(out) }.count(), 0);
        release(out);
        release(empty);
    });
}

// ---------------------------------------------------------------------
// Building

#[test]
fn push_adds_one_byte() {
    balanced(|| {
        let empty = cheby_bytes_new();
        let a = as_wrapper(empty, |w| unsafe { cheby_bytes_push(w, 0x41) });
        let ab = as_wrapper(a, |w| unsafe { cheby_bytes_push(w, 0x42) });
        assert_eq!(take(ab), b"AB");
    });
}

#[test]
fn append_joins_two_sequences() {
    balanced(|| {
        let head = new_bytes(&[1, 2]);
        let tail = new_bytes(&[3]);
        let empty = cheby_bytes_new();
        // SAFETY: live `Bytes`, borrowed.
        unsafe {
            assert_eq!(take(cheby_bytes_append(head, tail)), [1, 2, 3]);
            let same = cheby_bytes_append(head, empty);
            assert_eq!(same, head, "appending nothing shares the value");
            release(same);
            assert_eq!(take(cheby_bytes_append(empty, head)), [1, 2]);
        }
        assert_eq!(data(head), [1, 2]);
        for word in [head, tail, empty] {
            release(word);
        }
    });
}

#[test]
fn concat_joins_many_sequences() {
    balanced(|| {
        let parts = list::from_iter([new_bytes(b"ab"), cheby_bytes_new(), new_bytes(b"c")]);
        // SAFETY: a live list of live `Bytes`, borrowed.
        assert_eq!(take(unsafe { cheby_bytes_concat(parts) }), b"abc");
        release(parts);
        let none = list::from_iter([]);
        // SAFETY: as above.
        assert_eq!(take(unsafe { cheby_bytes_concat(none) }), b"");
        release(none);
        let one = new_bytes(b"x");
        // SAFETY: a live `Bytes`; the list gets a reference of its own.
        unsafe { rc::dup(one) };
        let single = list::from_iter([one]);
        // SAFETY: as above.
        let out = unsafe { cheby_bytes_concat(single) };
        assert_eq!(out, one, "one part is shared");
        release(out);
        release(single);
        release(one);
    });
}

// ---------------------------------------------------------------------
// Integers in a byte order (D-334), the examples of bytes.md

/// `kernel(new(), value)`'s bytes.
fn pushed<T>(kernel: unsafe extern "C" fn(Word, T) -> Word, value: T) -> Vec<u8> {
    let empty = cheby_bytes_new();
    take(as_wrapper(empty, |w| unsafe { kernel(w, value) }))
}

#[test]
fn pushes_name_their_byte_order() {
    balanced(|| {
        assert_eq!(pushed(cheby_bytes_push_i8, -1), [0xFF]);
        assert_eq!(pushed(cheby_bytes_push_i8, 5), [0x05]);
        assert_eq!(pushed(cheby_bytes_push_u16_be, 0x0102), [0x01, 0x02]);
        assert_eq!(pushed(cheby_bytes_push_u16_le, 0x0102), [0x02, 0x01]);
        assert_eq!(pushed(cheby_bytes_push_i16_be, -2), [0xFF, 0xFE]);
        assert_eq!(pushed(cheby_bytes_push_i16_le, -2), [0xFE, 0xFF]);
        assert_eq!(
            pushed(cheby_bytes_push_u32_be, 0xCBF4_3926),
            [0xCB, 0xF4, 0x39, 0x26]
        );
        assert_eq!(
            pushed(cheby_bytes_push_u32_le, 0xCBF4_3926),
            [0x26, 0x39, 0xF4, 0xCB]
        );
        assert_eq!(pushed(cheby_bytes_push_i32_be, -1), [0xFF; 4]);
        assert_eq!(
            pushed(cheby_bytes_push_i32_le, -2),
            [0xFE, 0xFF, 0xFF, 0xFF]
        );
        assert_eq!(
            pushed(cheby_bytes_push_u64_be, 0x0102_0304_0506_0708),
            [1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert_eq!(
            pushed(cheby_bytes_push_u64_le, 0x0102_0304_0506_0708),
            [8, 7, 6, 5, 4, 3, 2, 1]
        );
        assert_eq!(pushed(cheby_bytes_push_i64_le, -1), [0xFF; 8]);
        assert_eq!(
            pushed(cheby_bytes_push_i64_be, i64::MIN),
            [0x80, 0, 0, 0, 0, 0, 0, 0]
        );
    });
}

#[test]
fn reads_name_their_byte_order() {
    balanced(|| {
        let data = [0xFF, 0x05];
        assert_eq!(read(cheby_bytes_read_i8, &data, 0, signed), Some(-1));
        assert_eq!(read(cheby_bytes_read_i8, &data, 1, signed), Some(5));
        assert_eq!(read(cheby_bytes_read_i8, &data, 2, signed), None);
        let data = [0x01, 0x02, 0xFF];
        assert_eq!(
            read(cheby_bytes_read_u16_be, &data, 0, unsigned),
            Some(0x0102)
        );
        assert_eq!(
            read(cheby_bytes_read_u16_le, &data, 0, unsigned),
            Some(0x0201)
        );
        assert_eq!(read(cheby_bytes_read_i16_le, &data, 1, signed), Some(-254));
        assert_eq!(
            read(cheby_bytes_read_i16_be, &data, 1, signed),
            Some(0x02FF)
        );
        assert_eq!(read(cheby_bytes_read_u16_be, &data, 2, unsigned), None);
        assert_eq!(read(cheby_bytes_read_u16_be, &data, -1, unsigned), None);
        let data = [0x89, 0x50, 0x4E, 0x47];
        assert_eq!(
            read(cheby_bytes_read_u32_be, &data, 0, unsigned),
            Some(0x8950_4E47)
        );
        assert_eq!(
            read(cheby_bytes_read_u32_le, &data, 0, unsigned),
            Some(0x474E_5089)
        );
        assert_eq!(
            read(cheby_bytes_read_i32_be, &data, 0, signed),
            Some(-1_991_225_785)
        );
        assert_eq!(read(cheby_bytes_read_u32_be, &data, 1, unsigned), None);
        let data = [1, 2, 3, 4, 5, 6, 7, 8];
        let ones = [0xFF; 8];
        assert_eq!(
            read(cheby_bytes_read_u64_be, &data, 0, unsigned),
            Some(0x0102_0304_0506_0708)
        );
        assert_eq!(
            read(cheby_bytes_read_u64_le, &data, 0, unsigned),
            Some(0x0807_0605_0403_0201)
        );
        assert_eq!(read(cheby_bytes_read_i64_be, &ones, 0, signed), Some(-1));
        assert_eq!(
            read(cheby_bytes_read_u64_le, &ones, 0, unsigned),
            Some(i128::from(u64::MAX))
        );
        assert_eq!(read(cheby_bytes_read_u64_be, &data, 1, unsigned), None);
        assert_eq!(read(cheby_bytes_read_i64_le, &data, i64::MAX, signed), None);
    });
}

/// A push kernel and a read kernel of one type and byte order, with the
/// reference encoding and the type's range.
struct Codec {
    name: &'static str,
    width: usize,
    /// The reference bytes of a value of the type, given as an `i128`.
    encode: fn(i128) -> Vec<u8>,
    /// `push(data, value)` for a value of the type.
    push: fn(Word, i128) -> Word,
    read: unsafe extern "C" fn(Word, i64) -> Word,
    payload: fn(Word) -> i128,
    min: i128,
    max: i128,
}

macro_rules! codec {
    ($name:literal, $ty:ty, $width:literal, $to:ident, $push:ident, $read:ident, $payload:ident) => {
        Codec {
            name: $name,
            width: $width,
            encode: |v| (v as $ty).$to().to_vec(),
            // SAFETY: tests pass live `Bytes`.
            push: |w, v| unsafe { $push(w, v as $ty) },
            read: $read,
            payload: $payload,
            min: i128::from(<$ty>::MIN),
            max: i128::from(<$ty>::MAX),
        }
    };
}

/// Every push and read pair: every width, both orders, both signs.
#[expect(clippy::too_many_lines, reason = "one entry per type and order")]
fn codecs() -> [Codec; 13] {
    [
        Codec {
            name: "i8",
            width: 1,
            encode: |v| (v as i8).to_be_bytes().to_vec(),
            // SAFETY: tests pass live `Bytes`.
            push: |w, v| unsafe { cheby_bytes_push_i8(w, v as i8) },
            read: cheby_bytes_read_i8,
            payload: signed,
            min: -128,
            max: 127,
        },
        codec!(
            "u16_be",
            u16,
            2,
            to_be_bytes,
            cheby_bytes_push_u16_be,
            cheby_bytes_read_u16_be,
            unsigned
        ),
        codec!(
            "u16_le",
            u16,
            2,
            to_le_bytes,
            cheby_bytes_push_u16_le,
            cheby_bytes_read_u16_le,
            unsigned
        ),
        codec!(
            "i16_be",
            i16,
            2,
            to_be_bytes,
            cheby_bytes_push_i16_be,
            cheby_bytes_read_i16_be,
            signed
        ),
        codec!(
            "i16_le",
            i16,
            2,
            to_le_bytes,
            cheby_bytes_push_i16_le,
            cheby_bytes_read_i16_le,
            signed
        ),
        codec!(
            "u32_be",
            u32,
            4,
            to_be_bytes,
            cheby_bytes_push_u32_be,
            cheby_bytes_read_u32_be,
            unsigned
        ),
        codec!(
            "u32_le",
            u32,
            4,
            to_le_bytes,
            cheby_bytes_push_u32_le,
            cheby_bytes_read_u32_le,
            unsigned
        ),
        codec!(
            "i32_be",
            i32,
            4,
            to_be_bytes,
            cheby_bytes_push_i32_be,
            cheby_bytes_read_i32_be,
            signed
        ),
        codec!(
            "i32_le",
            i32,
            4,
            to_le_bytes,
            cheby_bytes_push_i32_le,
            cheby_bytes_read_i32_le,
            signed
        ),
        codec!(
            "u64_be",
            u64,
            8,
            to_be_bytes,
            cheby_bytes_push_u64_be,
            cheby_bytes_read_u64_be,
            unsigned
        ),
        codec!(
            "u64_le",
            u64,
            8,
            to_le_bytes,
            cheby_bytes_push_u64_le,
            cheby_bytes_read_u64_le,
            unsigned
        ),
        codec!(
            "i64_be",
            i64,
            8,
            to_be_bytes,
            cheby_bytes_push_i64_be,
            cheby_bytes_read_i64_be,
            signed
        ),
        codec!(
            "i64_le",
            i64,
            8,
            to_le_bytes,
            cheby_bytes_push_i64_le,
            cheby_bytes_read_i64_le,
            signed
        ),
    ]
}

/// The extremes of every type read back at the last offset that fits,
/// and `None` one byte later: the `Option` boundary at the end.
#[test]
fn every_width_reads_up_to_the_end() {
    balanced(|| {
        for codec in codecs() {
            for value in [
                codec.min,
                codec.max,
                0,
                1,
                i128::midpoint(codec.min, codec.max),
            ] {
                let prefix = [0xAA_u8; 3];
                let word = new_bytes(&prefix);
                let word = as_wrapper(word, |w| (codec.push)(w, value));
                let mut want = prefix.to_vec();
                want.extend((codec.encode)(value));
                assert_eq!(data(word), want, "{} {value}", codec.name);
                let bytes = data(word);
                let last = (bytes.len() - codec.width) as i64;
                assert_eq!(
                    read(codec.read, &bytes, last, codec.payload),
                    Some(value),
                    "{} {value}",
                    codec.name
                );
                assert_eq!(read(codec.read, &bytes, last + 1, codec.payload), None);
                assert_eq!(read(codec.read, &bytes, -1, codec.payload), None);
                release(word);
            }
        }
    });
}

#[test]
fn kernels_accept_static_bytes_values() {
    balanced(|| {
        // A static `Bytes` image (count 0), as constants freeze them: a
        // static string image with the `Bytes` layout in its header.
        let mut image: Vec<u64> = crate::abi::string::static_image("ab")
            .as_chunks::<8>()
            .0
            .iter()
            .copied()
            .map(u64::from_le_bytes)
            .collect();
        image[0] = crate::abi::header(crate::abi::RC_STATIC, 0, crate::abi::layout::BYTES);
        let word = crate::object::ptr_to_word(image.as_ptr().cast::<u8>());
        // SAFETY: a static `Bytes`, borrowed.
        unsafe {
            assert_eq!(cheby_bytes_length(word), 2);
            let pushed = cheby_bytes_push(word, b'c');
            assert_ne!(pushed, word, "a static value is copied");
            assert_eq!(take(pushed), b"abc");
            assert_eq!(data(word), b"ab");
        }
    });
}

// ---------------------------------------------------------------------
// In place at count one (D-316, D-525)

#[test]
fn push_and_append_work_in_place_at_count_one() {
    balanced(|| {
        let word = roomy(&[1], 16);
        let same = as_wrapper(word, |w| unsafe { cheby_bytes_push(w, 2) });
        assert_eq!(same, word, "a unique value with room is reused");
        let same = as_wrapper(same, |w| unsafe { cheby_bytes_push_u32_be(w, 0x0304_0506) });
        assert_eq!(same, word);
        let tail = new_bytes(&[7, 8]);
        let same = as_wrapper(same, |w| unsafe { cheby_bytes_append(w, tail) });
        assert_eq!(same, word);
        // SAFETY: a live `Bytes`.
        assert!(unsafe { rc::is_unique(same) });
        assert_eq!(take(same), [1, 2, 3, 4, 5, 6, 7, 8]);
        release(tail);
    });
}

#[test]
fn pushes_on_a_unique_value_grow_by_doubling() {
    balanced(|| {
        let mut word = cheby_bytes_new();
        let mut moves = 0;
        for i in 0..1000_u32 {
            let next = as_wrapper(word, |w| unsafe { cheby_bytes_push(w, i as u8) });
            if next != word {
                moves += 1;
            }
            word = next;
        }
        // Capacities 1, 2, 4, …, 1024: eleven copies for 1000 pushes.
        assert!(moves <= 11, "{moves} copies");
        // SAFETY: a live `Bytes`.
        assert_eq!(unsafe { string::capacity(word) }, 1024);
        let want: Vec<u8> = (0..1000_u32).map(|i| i as u8).collect();
        assert_eq!(take(word), want);
    });
}

#[test]
fn shared_and_aliased_values_are_never_changed() {
    balanced(|| {
        let shared = roomy(&[1, 2], 16);
        // SAFETY: a live `Bytes`; the test keeps a second reference.
        unsafe { rc::dup(shared) };
        let out = as_wrapper(shared, |w| unsafe { cheby_bytes_push(w, 3) });
        assert_ne!(out, shared);
        assert_eq!(data(shared), [1, 2]);
        assert_eq!(take(out), [1, 2, 3]);
        // `append(x, x)` with one reference: no in-place write.
        // SAFETY: a live `Bytes` passed twice.
        let doubled = unsafe { cheby_bytes_append(shared, shared) };
        assert_ne!(doubled, shared);
        assert_eq!(take(doubled), [1, 2, 1, 2]);
        assert_eq!(data(shared), [1, 2]);
        // A shared slice copies.
        // SAFETY: as above.
        unsafe { rc::dup(shared) };
        let sliced = as_wrapper(shared, |w| unsafe { cheby_bytes_slice(w, 1, 2) });
        assert_ne!(sliced, shared);
        assert_eq!(take(sliced), [2]);
        assert_eq!(data(shared), [1, 2]);
        // A unique slice keeps its range in place.
        let kept = as_wrapper(shared, |w| unsafe { cheby_bytes_slice(w, 1, 2) });
        assert_eq!(kept, shared);
        assert_eq!(take(kept), [2]);
    });
}

// ---------------------------------------------------------------------
// Property tests against `Vec<u8>`

fn small_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..24)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: miri_cases(256), ..ProptestConfig::default() })]

    /// Every push writes `to_be_bytes`/`to_le_bytes`, and every read
    /// inverts it at any offset, with `None` exactly where the window
    /// passes either end.
    #[test]
    fn integers_round_trip_in_both_orders(
        prefix in small_bytes(),
        raw in any::<i128>(),
        offset in -3_i64..40,
    ) {
        balanced(|| {
            for codec in codecs() {
                let span = codec.max - codec.min + 1;
                let value = codec.min + raw.rem_euclid(span);
                let word = new_bytes(&prefix);
                let word = as_wrapper(word, |w| (codec.push)(w, value));
                let mut model = prefix.clone();
                model.extend((codec.encode)(value));
                assert_eq!(data(word), model);
                let at = prefix.len() as i64;
                assert_eq!(read(codec.read, &model, at, codec.payload), Some(value));
                let fits = offset >= 0 && (offset as usize) + codec.width <= model.len();
                let want = fits.then(|| {
                    let start = offset as usize;
                    decode(&codec, &model[start..start + codec.width])
                });
                assert_eq!(read(codec.read, &model, offset, codec.payload), want);
                release(word);
            }
        });
    }

    #[test]
    fn slice_matches_vec(bytes in small_bytes(), start in -4_i64..30, end in -4_i64..30) {
        balanced(|| {
            let len = bytes.len() as i64;
            let (a, b) = (start.clamp(0, len) as usize, end.clamp(0, len) as usize);
            let want = if b > a { bytes[a..b].to_vec() } else { Vec::new() };
            let word = new_bytes(&bytes);
            // SAFETY: a live `Bytes`, borrowed.
            assert_eq!(take(unsafe { cheby_bytes_slice(word, start, end) }), want);
            release(word);
        });
    }

    #[test]
    fn append_and_concat_match_vec(parts in prop::collection::vec(small_bytes(), 0..6)) {
        balanced(|| {
            let words: Vec<Word> = parts.iter().map(|p| new_bytes(p)).collect();
            let list = list::from_iter(words.iter().map(|&w| {
                // SAFETY: a live `Bytes`; the list gets its own reference.
                unsafe { rc::dup(w) };
                w
            }));
            let want: Vec<u8> = parts.concat();
            // SAFETY: a live list of live `Bytes`, borrowed.
            assert_eq!(take(unsafe { cheby_bytes_concat(list) }), want);
            let mut acc = cheby_bytes_new();
            for &w in &words {
                // SAFETY: live `Bytes`, borrowed.
                acc = as_wrapper(acc, |a| unsafe { cheby_bytes_append(a, w) });
            }
            assert_eq!(take(acc), want);
            release(list);
            for (word, part) in words.into_iter().zip(&parts) {
                assert_eq!(&take(word), part, "an argument was changed");
            }
        });
    }

    #[test]
    fn lists_round_trip(bytes in small_bytes()) {
        balanced(|| {
            let values = u8_list(&bytes);
            // SAFETY: a live list, borrowed.
            let word = unsafe { cheby_bytes_from_list(values) };
            // SAFETY: a live `Bytes`, borrowed.
            let back = unsafe { cheby_bytes_to_list(word) };
            // SAFETY: a live list of `U8` words.
            let got: Vec<u8> = unsafe { list::iter(back) }.map(values::word_to_u8).collect();
            assert_eq!(got, bytes);
            for (i, &b) in bytes.iter().enumerate() {
                let byte = |w| i128::from(values::word_to_u8(w));
                assert_eq!(read(cheby_bytes_get, &bytes, i as i64, byte), Some(i128::from(b)));
            }
            for word in [values, word, back] {
                release(word);
            }
        });
    }
}

/// The reference decoding of a codec's window: the value whose encoding
/// it is.
fn decode(codec: &Codec, window: &[u8]) -> i128 {
    let unsigned_be = |bytes: &[u8]| {
        bytes
            .iter()
            .fold(0_i128, |acc, &b| (acc << 8) | i128::from(b))
    };
    let le = codec.name.ends_with("_le");
    let mut ordered = window.to_vec();
    if le {
        ordered.reverse();
    }
    let raw = unsigned_be(&ordered);
    let bits = 8 * codec.width as u32;
    if codec.min < 0 && raw >= 1_i128 << (bits - 1) {
        raw - (1_i128 << bits)
    } else {
        raw
    }
}

// ---------------------------------------------------------------------
// The multi-version state machine (plan §6)

/// One step over live `Bytes` versions.
#[derive(Debug, Clone)]
enum Step {
    /// Another reference to version `i` becomes a version of its own.
    Share(usize),
    /// Version `i` is dropped.
    Drop(usize),
    /// `bytes::push(v, b)`.
    Push(usize, u8),
    /// `bytes::push_u32_le(v, n)`.
    PushU32(usize, u32),
    /// `bytes::push_i64_be(v, n)`.
    PushI64(usize, i64),
    /// `bytes::append(v, piece)`.
    Append(usize, Vec<u8>),
    /// `bytes::append(v, w)`, two versions.
    AppendVersion(usize, usize),
    /// `bytes::append(v, v)`, the same object twice.
    AppendSelf(usize),
    /// `bytes::slice(v, a, b)`.
    Slice(usize, i64, i64),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        (0..8_usize).prop_map(Step::Share),
        (0..8_usize).prop_map(Step::Drop),
        (0..8_usize, any::<u8>()).prop_map(|(i, b)| Step::Push(i, b)),
        (0..8_usize, any::<u32>()).prop_map(|(i, n)| Step::PushU32(i, n)),
        (0..8_usize, any::<i64>()).prop_map(|(i, n)| Step::PushI64(i, n)),
        (0..8_usize, small_bytes()).prop_map(|(i, p)| Step::Append(i, p)),
        (0..8_usize, 0..8_usize).prop_map(|(i, j)| Step::AppendVersion(i, j)),
        (0..8_usize).prop_map(Step::AppendSelf),
        (0..8_usize, -2..30_i64, -2..30_i64).prop_map(|(i, a, b)| Step::Slice(i, a, b)),
    ]
}

/// The most live versions the machine keeps.
const VERSIONS: usize = 8;

/// Applies an updating kernel to version `i` as its wrapper does, and
/// gives the version its new model.
fn update(
    versions: &mut [(Word, Vec<u8>)],
    i: usize,
    model: Vec<u8>,
    kernel: impl FnOnce(Word) -> Word,
) {
    let (word, bytes_model) = &mut versions[i];
    *word = as_wrapper(*word, kernel);
    *bytes_model = model;
}

proptest! {
    #![proptest_config(ProptestConfig { cases: miri_cases(128), ..ProptestConfig::default() })]

    /// N live versions under random sharing, dropping and in-place-capable
    /// updates, each checked against its own model after every step; the
    /// domain returns to its start.
    #[test]
    fn versions_stay_independent(
        start in small_bytes(),
        steps in prop::collection::vec(step(), 1..80),
    ) {
        balanced(|| {
            let mut versions: Vec<(Word, Vec<u8>)> = vec![(new_bytes(&start), start.clone())];
            for step in &steps {
                let n = versions.len();
                match step.clone() {
                    Step::Share(i) if n < VERSIONS => {
                        let (word, model) = versions[i % n].clone();
                        // SAFETY: a live version; the copy owns the new reference.
                        unsafe { rc::dup(word) };
                        versions.push((word, model));
                    }
                    Step::Drop(i) if n > 1 => {
                        let (word, _) = versions.swap_remove(i % n);
                        release(word);
                    }
                    Step::Push(i, b) => {
                        let i = i % n;
                        let mut model = versions[i].1.clone();
                        model.push(b);
                        // SAFETY: a live `Bytes`, borrowed.
                        update(&mut versions, i, model, |w| unsafe { cheby_bytes_push(w, b) });
                    }
                    Step::PushU32(i, v) => {
                        let i = i % n;
                        let mut model = versions[i].1.clone();
                        model.extend(v.to_le_bytes());
                        // SAFETY: as above.
                        update(&mut versions, i, model, |w| unsafe { cheby_bytes_push_u32_le(w, v) });
                    }
                    Step::PushI64(i, v) => {
                        let i = i % n;
                        let mut model = versions[i].1.clone();
                        model.extend(v.to_be_bytes());
                        // SAFETY: as above.
                        update(&mut versions, i, model, |w| unsafe { cheby_bytes_push_i64_be(w, v) });
                    }
                    Step::Append(i, piece) => {
                        let i = i % n;
                        let mut model = versions[i].1.clone();
                        model.extend(&piece);
                        let piece = new_bytes(&piece);
                        // SAFETY: live `Bytes`, borrowed.
                        update(&mut versions, i, model, |w| unsafe { cheby_bytes_append(w, piece) });
                        release(piece);
                    }
                    Step::AppendVersion(i, j) => {
                        let (i, j) = (i % n, j % n);
                        let mut model = versions[i].1.clone();
                        model.extend(&versions[j].1);
                        let other = versions[j].0;
                        // The wrapper owns one reference per argument.
                        // SAFETY: live versions, borrowed.
                        unsafe { rc::dup(other) };
                        update(&mut versions, i, model, |w| unsafe { cheby_bytes_append(w, other) });
                        release(other);
                    }
                    Step::AppendSelf(i) => {
                        let i = i % n;
                        let model = versions[i].1.repeat(2);
                        // SAFETY: a live version passed twice, borrowed; the
                        // wrapper owns one reference per argument.
                        update(&mut versions, i, model, |w| unsafe {
                            rc::dup(w);
                            let out = cheby_bytes_append(w, w);
                            rc::drop(w);
                            out
                        });
                    }
                    Step::Slice(i, a, b) => {
                        let i = i % n;
                        let current = &versions[i].1;
                        let len = current.len() as i64;
                        let (x, y) = (a.clamp(0, len) as usize, b.clamp(0, len) as usize);
                        let model = if y > x { current[x..y].to_vec() } else { Vec::new() };
                        // SAFETY: a live `Bytes`, borrowed.
                        update(&mut versions, i, model, |w| unsafe { cheby_bytes_slice(w, a, b) });
                    }
                    _ => {}
                }
                for (word, model) in &versions {
                    assert_eq!(&data(*word), model);
                }
            }
            for (word, _) in versions {
                release(word);
            }
        });
    }
}

#[test]
fn every_kernel_is_registered_once() {
    let all = all();
    assert_eq!(all.len(), 38);
    let mut names: Vec<_> = all.iter().map(|entry| entry.0).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), 38);
    assert!(all.iter().all(|entry| !entry.1.is_null()));
}
