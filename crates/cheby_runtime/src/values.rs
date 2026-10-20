//! Building and reading uniform words (plan §4.3.4, §5.1, D-521).
//!
//! Every kernel builds its results through these functions (and the list
//! and map builders), so the encodings live in one place:
//!
//! | Type                                   | Uniform word                                           |
//! | -------------------------------------- | ------------------------------------------------------ |
//! | `Int`, `I64`                           | tagged when it fits 63 bits, else a `BoxedInt` (id 3)  |
//! | `I8`, `I16`, `I32`, `U8`, `U16`, `U32` | always tagged, sign-extended for the signed types      |
//! | `U64`                                  | tagged when at most 2^62 - 1, else a `BoxedU64` (id 8) |
//! | `Float`, `F32`                         | a `BoxedFloat` (id 4), a `BoxedF32` (id 5)             |
//! | `Bool`                                 | the static `True` or `False` object                    |
//!
//! Builders return owned words and consume the words they are given.
//! Readers borrow their argument; a word of the wrong shape breaks the ABI
//! and ends the process. In poisoning mode the readers check the header
//! first ([`alloc::check_live`]).

use std::cmp::Ordering;

use crate::abi::{self, LayoutId, Word, layout};
use crate::alloc;
use crate::object::{self, read_field, scan_and_layout, to_u64, word_to_ptr, write_field};
use crate::panic::fatal;
use crate::string;

/// Byte offset of a boxed number's payload and of a one-field
/// constructor's field.
const FIELD0: usize = 8;

/// The static `Nil`.
#[must_use]
pub fn nil() -> Word {
    object::nil()
}

/// The static `True` or `False`.
#[must_use]
pub fn bool_word(value: bool) -> Word {
    if value {
        object::cheby_rt_static_true.word()
    } else {
        object::cheby_rt_static_false.word()
    }
}

/// The static `Less`, `Equal` or `Greater`.
#[must_use]
pub fn order(ordering: Ordering) -> Word {
    match ordering {
        Ordering::Less => object::cheby_rt_static_less.word(),
        Ordering::Equal => object::cheby_rt_static_equal.word(),
        Ordering::Greater => object::cheby_rt_static_greater.word(),
    }
}

/// A one-raw-field box of the given layout.
fn boxed(layout: LayoutId, bits: u64) -> Word {
    let obj = alloc::alloc_object(16, abi::header(abi::RC_UNIQUE, 0, layout));
    // SAFETY: a fresh 16-byte object: the payload is its only field.
    unsafe { write_field(obj, FIELD0, bits) };
    object::ptr_to_word(obj)
}

/// The uniform word of an `Int` or `I64`: tagged, or a `BoxedInt` outside
/// 63 bits.
#[must_use]
pub fn int_word(value: i64) -> Word {
    abi::tag_int(value).unwrap_or_else(|| boxed(layout::BOXED_INT, value.cast_unsigned()))
}

/// The uniform word of a `U64`: tagged up to 2^62 - 1, else a `BoxedU64`.
#[must_use]
pub fn u64_word(value: u64) -> Word {
    match i64::try_from(value).ok().and_then(abi::tag_int) {
        Some(word) => word,
        None => boxed(layout::BOXED_U64, value),
    }
}

/// The tagged word of a sized integer that always fits 63 bits.
fn small_word(value: i64) -> Word {
    abi::tag_int(value).unwrap_or_else(|| fatal(format_args!("{value} does not fit a tag")))
}

/// The uniform word of an `I8`.
#[must_use]
pub fn i8_word(value: i8) -> Word {
    small_word(i64::from(value))
}

/// The uniform word of an `I16`.
#[must_use]
pub fn i16_word(value: i16) -> Word {
    small_word(i64::from(value))
}

/// The uniform word of an `I32`.
#[must_use]
pub fn i32_word(value: i32) -> Word {
    small_word(i64::from(value))
}

/// The uniform word of a `U8`.
#[must_use]
pub fn u8_word(value: u8) -> Word {
    small_word(i64::from(value))
}

/// The uniform word of a `U16`.
#[must_use]
pub fn u16_word(value: u16) -> Word {
    small_word(i64::from(value))
}

/// The uniform word of a `U32`.
#[must_use]
pub fn u32_word(value: u32) -> Word {
    small_word(i64::from(value))
}

/// The uniform word of a `Float`: a new `BoxedFloat`.
#[must_use]
pub fn f64_word(value: f64) -> Word {
    boxed(layout::BOXED_FLOAT, value.to_bits())
}

/// The uniform word of an `F32`: a new `BoxedF32`, the bits in the low
/// half of its 8-byte field.
#[must_use]
pub fn f32_word(value: f32) -> Word {
    boxed(layout::BOXED_F32, u64::from(value.to_bits()))
}

/// A one-field builtin constructor owning `value`.
fn one_field(layout: LayoutId, value: Word) -> Word {
    let obj = alloc::alloc_object(16, abi::header(abi::RC_UNIQUE, 1, layout));
    // SAFETY: a fresh 16-byte object with one value field.
    unsafe { write_field(obj, FIELD0, value) };
    object::ptr_to_word(obj)
}

/// `Some(value)`, consuming `value`.
#[must_use]
pub fn some(value: Word) -> Word {
    one_field(layout::SOME, value)
}

/// `Some(byte)` of a `U8`: a static object, so nothing is allocated
/// and nothing can raise (D-902).
///
/// The table holds every `U8`, so the fatal branch cannot run. It is fatal
/// rather than an allocation because `bytes::get` runs unguarded on the
/// never-raise table: an allocation could raise with no unwind edge.
#[must_use]
pub fn some_u8(byte: u8) -> Word {
    object::static_some_small(i64::from(byte))
        .unwrap_or_else(|| fatal(format_args!("no static Some for the U8 {byte}")))
}

/// `Some(value)` of an `I8`: a static object, so nothing is allocated
/// and nothing can raise (D-902).
///
/// The table holds every `I8`, so the fatal branch cannot run; see
/// [`some_u8`] for why it is fatal.
#[must_use]
pub fn some_i8(value: i8) -> Word {
    object::static_some_small(i64::from(value))
        .unwrap_or_else(|| fatal(format_args!("no static Some for the I8 {value}")))
}

/// The static `None`.
#[must_use]
pub fn none() -> Word {
    object::cheby_rt_static_none.word()
}

/// `Ok(value)`, consuming `value`.
#[must_use]
pub fn ok(value: Word) -> Word {
    one_field(layout::OK, value)
}

/// `Err(error)`, consuming `error`.
#[must_use]
pub fn err(error: Word) -> Word {
    one_field(layout::ERR, error)
}

/// `Ok(Nil)`.
#[must_use]
pub fn ok_nil() -> Word {
    ok(nil())
}

/// `Err(Nil)`, the error of the `Result<T, Nil>` kernels (D-309).
#[must_use]
pub fn err_nil() -> Word {
    err(nil())
}

/// `Some(value)` or `None`, consuming the value.
#[must_use]
pub fn option(value: Option<Word>) -> Word {
    value.map_or_else(none, some)
}

/// `Ok(value)` or `Err(error)`, consuming the word.
#[must_use]
pub fn result(value: Result<Word, Word>) -> Word {
    match value {
        Ok(value) => ok(value),
        Err(error) => err(error),
    }
}

/// A tuple of the given elements, consuming each. A tuple has at least two
/// elements (§3.7); wider than 254 elements, the arity is stored at offset
/// 8 (escaped scan count).
#[must_use]
pub fn tuple(elements: &[Word]) -> Word {
    let count = to_u64(elements.len());
    let (scan, first) = match u8::try_from(count) {
        Ok(scan) if scan <= abi::SCAN_MAX => (scan, 8_usize),
        _ => (abi::SCAN_ESCAPE, 16),
    };
    let size = to_u64(first).saturating_add(count.saturating_mul(abi::FIELD_SIZE));
    let obj = alloc::alloc_object(size, abi::header(abi::RC_UNIQUE, scan, layout::TUPLE));
    let mut offset = first;
    // SAFETY: a fresh tuple of `count` fields from `first` (and the arity
    // at 8 when escaped).
    unsafe {
        if scan == abi::SCAN_ESCAPE {
            write_field(obj, 8, int_word(i64::try_from(count).unwrap_or(i64::MAX)));
        }
        for element in elements {
            write_field(obj, offset, *element);
            offset = offset.saturating_add(8);
        }
    }
    object::ptr_to_word(obj)
}

/// A new `String` holding `text`.
#[must_use]
pub fn string(text: &str) -> Word {
    string::new_string(text)
}

/// A new `Bytes` holding `data`.
#[must_use]
pub fn bytes(data: &[u8]) -> Word {
    string::new_bytes(data)
}

// Readers -----------------------------------------------------------------

/// The layout id of an object word (after the poisoning check), or `None`
/// for a tagged integer or 0.
///
/// # Safety
///
/// `word` is a tagged integer, 0, or a live object.
#[must_use]
pub unsafe fn layout_of(word: Word) -> Option<LayoutId> {
    if abi::is_tagged_int(word) || word == 0 {
        return None;
    }
    // SAFETY: forwarded from the caller.
    unsafe { alloc::check_live(word, "reading a value") };
    // SAFETY: as above.
    Some(unsafe { scan_and_layout(word_to_ptr(word)) }.1)
}

/// The payload of a box of the given layout.
///
/// # Safety
///
/// `word` is a live object.
unsafe fn payload(word: Word, expected: LayoutId, what: &str) -> u64 {
    // SAFETY: forwarded from the caller.
    if unsafe { layout_of(word) } != Some(expected) {
        fatal(format_args!("{word:#x} is not {what}"));
    }
    // SAFETY: a live box has its payload at offset 8.
    unsafe { read_field(word_to_ptr(word), FIELD0) }
}

/// The `Int` or `I64` of a uniform word (tagged or `BoxedInt`).
///
/// # Safety
///
/// `word` is the uniform word of an `Int` or `I64`, borrowed.
#[must_use]
pub unsafe fn word_to_i64(word: Word) -> i64 {
    if abi::is_tagged_int(word) {
        return abi::untag_int(word);
    }
    // SAFETY: forwarded from the caller.
    unsafe { payload(word, layout::BOXED_INT, "an Int") }.cast_signed()
}

/// The `U64` of a uniform word (tagged or `BoxedU64`).
///
/// # Safety
///
/// `word` is the uniform word of a `U64`, borrowed.
#[must_use]
pub unsafe fn word_to_u64(word: Word) -> u64 {
    if abi::is_tagged_int(word) {
        return abi::untag_int(word).cast_unsigned();
    }
    // SAFETY: forwarded from the caller.
    unsafe { payload(word, layout::BOXED_U64, "a U64") }
}

/// The value of a tagged sized integer, converted to `T`.
fn tagged<T: TryFrom<i64>>(word: Word, what: &str) -> T {
    if !abi::is_tagged_int(word) {
        fatal(format_args!("{word:#x} is not a tagged {what}"));
    }
    T::try_from(abi::untag_int(word))
        .unwrap_or_else(|_| fatal(format_args!("{word:#x} is out of range for {what}")))
}

/// The `I8` of a uniform word.
#[must_use]
pub fn word_to_i8(word: Word) -> i8 {
    tagged(word, "I8")
}

/// The `I16` of a uniform word.
#[must_use]
pub fn word_to_i16(word: Word) -> i16 {
    tagged(word, "I16")
}

/// The `I32` of a uniform word.
#[must_use]
pub fn word_to_i32(word: Word) -> i32 {
    tagged(word, "I32")
}

/// The `U8` of a uniform word.
#[must_use]
pub fn word_to_u8(word: Word) -> u8 {
    tagged(word, "U8")
}

/// The `U16` of a uniform word.
#[must_use]
pub fn word_to_u16(word: Word) -> u16 {
    tagged(word, "U16")
}

/// The `U32` of a uniform word.
#[must_use]
pub fn word_to_u32(word: Word) -> u32 {
    tagged(word, "U32")
}

/// The `Float` of a uniform word (a `BoxedFloat`).
///
/// # Safety
///
/// `word` is the uniform word of a `Float`, borrowed.
#[must_use]
pub unsafe fn word_to_f64(word: Word) -> f64 {
    // SAFETY: forwarded from the caller.
    f64::from_bits(unsafe { payload(word, layout::BOXED_FLOAT, "a Float") })
}

/// The `F32` of a uniform word (a `BoxedF32`).
///
/// # Safety
///
/// `word` is the uniform word of an `F32`, borrowed.
#[must_use]
pub unsafe fn word_to_f32(word: Word) -> f32 {
    // SAFETY: forwarded from the caller.
    let bits = unsafe { payload(word, layout::BOXED_F32, "an F32") };
    let [b0, b1, b2, b3, ..] = bits.to_le_bytes();
    f32::from_bits(u32::from_le_bytes([b0, b1, b2, b3]))
}

/// The `Bool` of a uniform word (the static `True` or `False`).
///
/// # Safety
///
/// `word` is the uniform word of a `Bool`, borrowed.
#[must_use]
pub unsafe fn word_to_bool(word: Word) -> bool {
    // SAFETY: forwarded from the caller.
    match unsafe { layout_of(word) } {
        Some(layout::TRUE) => true,
        Some(layout::FALSE) => false,
        _ => fatal(format_args!("{word:#x} is not a Bool")),
    }
}

/// The `Ordering` of an `Order` word.
///
/// # Safety
///
/// `word` is the uniform word of an `Order`, borrowed.
#[must_use]
pub unsafe fn word_to_order(word: Word) -> Ordering {
    // SAFETY: forwarded from the caller.
    match unsafe { layout_of(word) } {
        Some(layout::LESS) => Ordering::Less,
        Some(layout::EQUAL) => Ordering::Equal,
        Some(layout::GREATER) => Ordering::Greater,
        _ => fatal(format_args!("{word:#x} is not an Order")),
    }
}

/// The field of a one-field builtin constructor.
///
/// # Safety
///
/// `word` is a live object of that layout.
unsafe fn field0(word: Word) -> Word {
    // SAFETY: forwarded from the caller.
    unsafe { read_field(word_to_ptr(word), FIELD0) }
}

/// The value inside an `Option`, borrowed: `Some(v)` gives `Some(v)`,
/// `None` gives `None`.
///
/// # Safety
///
/// `word` is the uniform word of an `Option`, borrowed; the result is
/// valid while `word` is.
#[must_use]
pub unsafe fn read_option(word: Word) -> Option<Word> {
    // SAFETY: forwarded from the caller.
    match unsafe { layout_of(word) } {
        // SAFETY: a live `Some`.
        Some(layout::SOME) => Some(unsafe { field0(word) }),
        Some(layout::NONE) => None,
        _ => fatal(format_args!("{word:#x} is not an Option")),
    }
}

/// The value inside a `Result`, borrowed.
///
/// # Errors
///
/// `Err(error)` for an `Err(error)` value: the result mirrors the value.
///
/// # Safety
///
/// `word` is the uniform word of a `Result`, borrowed; the result is valid
/// while `word` is.
pub unsafe fn read_result(word: Word) -> Result<Word, Word> {
    // SAFETY: forwarded from the caller.
    match unsafe { layout_of(word) } {
        // SAFETY: a live `Ok` or `Err`.
        Some(layout::OK) => Ok(unsafe { field0(word) }),
        // SAFETY: as above.
        Some(layout::ERR) => Err(unsafe { field0(word) }),
        _ => fatal(format_args!("{word:#x} is not a Result")),
    }
}

/// The elements of a tuple, borrowed.
///
/// # Safety
///
/// `word` is the uniform word of a tuple, borrowed; the elements are valid
/// while `word` is.
#[must_use]
pub unsafe fn read_tuple(word: Word) -> Vec<Word> {
    // SAFETY: forwarded from the caller.
    if unsafe { layout_of(word) } != Some(layout::TUPLE) {
        fatal(format_args!("{word:#x} is not a tuple"));
    }
    let obj = word_to_ptr(word);
    // SAFETY: a live tuple: its arity is the scan count, or the tagged
    // count at offset 8 when escaped, and its fields follow.
    unsafe {
        let (scan, _) = scan_and_layout(obj);
        let (count, first) = if scan == abi::SCAN_ESCAPE {
            (word_to_i64(read_field(obj, 8)), 16_usize)
        } else {
            (i64::from(scan), 8)
        };
        (0..usize::try_from(count).unwrap_or(0))
            .map(|index| read_field(obj, index.saturating_mul(8).saturating_add(first)))
            .collect()
    }
}

/// The text of a `String`, borrowed.
///
/// # Safety
///
/// `word` is the uniform word of a `String`, borrowed, not changed while
/// the result is used.
#[must_use]
pub unsafe fn str_of<'a>(word: Word) -> &'a str {
    // SAFETY: forwarded from the caller.
    unsafe { string::as_str(word) }
        .unwrap_or_else(|| fatal(format_args!("{word:#x} is not a String")))
}

/// The bytes of a `Bytes` (or the UTF-8 bytes of a `String`), borrowed.
///
/// # Safety
///
/// As for [`str_of`].
#[must_use]
pub unsafe fn bytes_of<'a>(word: Word) -> &'a [u8] {
    // SAFETY: forwarded from the caller.
    unsafe { string::text_bytes(word) }
        .unwrap_or_else(|| fatal(format_args!("{word:#x} is not a Bytes or a String")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc::live_objects;
    use crate::rc;

    fn release(word: Word) {
        // SAFETY: tests own the words they release.
        unsafe { rc::drop(word) };
    }

    #[test]
    fn ints_round_trip_at_the_tag_boundaries() {
        let before = live_objects();
        for (value, tagged) in [
            (0, true),
            (-1, true),
            (abi::INT63_MAX, true),
            (abi::INT63_MIN, true),
            (abi::INT63_MAX + 1, false),
            (abi::INT63_MIN - 1, false),
            (i64::MAX, false),
            (i64::MIN, false),
        ] {
            let word = int_word(value);
            assert_eq!(abi::is_tagged_int(word), tagged, "{value}");
            // SAFETY: a live word of an `Int`.
            assert_eq!(unsafe { word_to_i64(word) }, value);
            if !tagged {
                // SAFETY: as above.
                assert_eq!(unsafe { layout_of(word) }, Some(layout::BOXED_INT));
            }
            release(word);
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn u64s_round_trip_at_the_tag_boundaries() {
        let before = live_objects();
        let limit = (1_u64 << 62) - 1;
        for (value, tagged) in [
            (0, true),
            (limit, true),
            (limit + 1, false),
            (u64::MAX, false),
        ] {
            let word = u64_word(value);
            assert_eq!(abi::is_tagged_int(word), tagged, "{value}");
            // SAFETY: a live word of a `U64`.
            assert_eq!(unsafe { word_to_u64(word) }, value);
            release(word);
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn sized_ints_are_always_tagged() {
        assert_eq!(word_to_i8(i8_word(i8::MIN)), i8::MIN);
        assert_eq!(word_to_i16(i16_word(i16::MAX)), i16::MAX);
        assert_eq!(word_to_i32(i32_word(i32::MIN)), i32::MIN);
        assert_eq!(word_to_u8(u8_word(u8::MAX)), u8::MAX);
        assert_eq!(word_to_u16(u16_word(u16::MAX)), u16::MAX);
        assert_eq!(word_to_u32(u32_word(u32::MAX)), u32::MAX);
        assert_eq!(i8_word(-1), abi::tag_int(-1).unwrap());
        assert_eq!(
            u32_word(u32::MAX),
            abi::tag_int(i64::from(u32::MAX)).unwrap()
        );
    }

    #[test]
    fn floats_round_trip_bit_for_bit() {
        let before = live_objects();
        for value in [0.0, -0.0, 1.5, f64::NAN, f64::INFINITY, f64::MIN_POSITIVE] {
            let word = f64_word(value);
            // SAFETY: a live `Float`.
            assert_eq!(unsafe { word_to_f64(word) }.to_bits(), value.to_bits());
            release(word);
        }
        for value in [0.0_f32, -0.0, 0.1, f32::NAN, f32::NEG_INFINITY] {
            let word = f32_word(value);
            // SAFETY: a live `F32`.
            unsafe {
                assert_eq!(word_to_f32(word).to_bits(), value.to_bits());
                assert_eq!(read_field(word_to_ptr(word), 8) >> 32, 0);
            }
            release(word);
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn constructors_and_tuples_read_back() {
        let before = live_objects();
        // SAFETY: live words built here, read while alive.
        unsafe {
            assert!(word_to_bool(bool_word(true)));
            assert!(!word_to_bool(bool_word(false)));
            assert_eq!(word_to_order(order(Ordering::Greater)), Ordering::Greater);
            let some = some(int_word(3));
            assert_eq!(read_option(some), Some(int_word(3)));
            assert_eq!(read_option(none()), None);
            let ok = ok(string("yes"));
            assert_eq!(read_result(ok).map(|w| str_of(w)), Ok("yes"));
            let err = err_nil();
            assert_eq!(read_result(err), Err(nil()));
            let pair = tuple(&[int_word(1), bytes(b"\x00")]);
            let fields = read_tuple(pair);
            assert_eq!(fields.len(), 2);
            assert_eq!(fields.first().map(|w| word_to_i64(*w)), Some(1));
            assert_eq!(fields.get(1).map(|w| bytes_of(*w)), Some(&b"\x00"[..]));
            let wide: Vec<Word> = (0..300).map(int_word).collect();
            let wide = tuple(&wide);
            assert_eq!(read_tuple(wide).len(), 300);
            assert_eq!(read_tuple(wide).last().map(|w| word_to_i64(*w)), Some(299));
            for word in [some, ok, err, pair, wide, option(None), result(Err(nil()))] {
                release(word);
            }
        }
        assert_eq!(live_objects(), before);
    }
}
