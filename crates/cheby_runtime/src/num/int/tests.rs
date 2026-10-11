//! Tests of the integer kernels beyond the per-family property tests of
//! `int_family!`.

#![expect(
    clippy::arithmetic_side_effects,
    reason = "the tests compute expected values with plain arithmetic"
)]

use proptest::prelude::*;

use super::{checked_neg, checked_pow, int_kernels, to_base, u8_kernels, u64_kernels};
use crate::abi::Word;
use crate::num::tests::{no_leaks, take_result, take_string, with_text};

/// D-312 written independently of `num::text`: an optional `-` and one or
/// more digits of `base`, read by Rust's `from_str_radix`.
pub(crate) fn reference_parse<T: TryFrom<i128>>(text: &str, base: i64) -> Option<T> {
    let base = u32::try_from(base)
        .ok()
        .filter(|base| (2..=36).contains(base))?;
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(base)) {
        return None;
    }
    T::try_from(i128::from_str_radix(text, base).ok()?).ok()
}

/// `n` written in `base` with lowercase digits.
pub(crate) fn to_radix(n: i128, base: i64) -> String {
    let base = u32::try_from(base).unwrap();
    let mut magnitude = n.unsigned_abs();
    let mut digits = Vec::new();
    loop {
        let digit = u32::try_from(magnitude % u128::from(base)).unwrap();
        digits.push(char::from_digit(digit, base).unwrap());
        magnitude /= u128::from(base);
        if magnitude == 0 {
            break;
        }
    }
    if n < 0 {
        digits.push('-');
    }
    digits.iter().rev().collect()
}

fn parse_int(text: &str) -> Option<i64> {
    // SAFETY: `word` is the live string `with_text` made.
    no_leaks(|| take_result(with_text(text, |word| unsafe { int_kernels::parse(word) })))
}

fn parse_base(text: &str, base: i64) -> Option<i64> {
    // SAFETY: as above.
    no_leaks(|| {
        take_result(with_text(text, |word| unsafe {
            int_kernels::parse_base(word, base)
        }))
    })
}

fn show_int(n: i64) -> String {
    no_leaks(|| take_string(int_kernels::show(n)))
}

fn to_base_text(n: i64, base: i64) -> Option<String> {
    no_leaks(|| take_result(to_base(n, base)))
}

#[test]
fn int_show_writes_decimal_digits() {
    assert_eq!(show_int(-42), "-42");
    assert_eq!(show_int(0), "0");
    assert_eq!(show_int(1234), "1234");
    assert_eq!(show_int(i64::MAX), "9223372036854775807");
    assert_eq!(show_int(i64::MIN), "-9223372036854775808");
    assert_eq!(parse_int(&show_int(-1234)), Some(-1234));
    assert_eq!(
        no_leaks(|| take_string(u64_kernels::show(u64::MAX))),
        "18446744073709551615"
    );
    assert_eq!(no_leaks(|| take_string(u8_kernels::show(255))), "255");
}

#[test]
fn int_parse_follows_d312() {
    assert_eq!(parse_int("42"), Some(42));
    assert_eq!(parse_int("-007"), Some(-7));
    assert_eq!(parse_int("9223372036854775807"), Some(i64::MAX));
    assert_eq!(parse_int("-9223372036854775808"), Some(i64::MIN));
    for text in [
        "+1",
        "+42",
        " 1",
        " 42",
        "1 ",
        "1_000",
        "0x10",
        "0x2A",
        "",
        "-",
        "--1",
        "1.0",
        "9223372036854775808",
        "-9223372036854775809",
        "１",
    ] {
        assert_eq!(parse_int(text), None, "{text:?}");
    }
}

#[test]
fn sized_parse_reads_the_value_then_checks_the_range() {
    // SAFETY: `word` is the live string `with_text` made.
    let parse_u8 = |text: &str| {
        no_leaks(|| take_result::<u8>(with_text(text, |word| unsafe { u8_kernels::parse(word) })))
    };
    assert_eq!(parse_u8("255"), Some(255));
    assert_eq!(parse_u8("0255"), Some(255));
    assert_eq!(parse_u8("256"), None);
    assert_eq!(parse_u8("-1"), None);
    // `-0` is the value 0, which `U8` holds (D-312).
    assert_eq!(parse_u8("-0"), Some(0));
    // SAFETY: as above.
    let parse_u64 = |text: &str| {
        no_leaks(|| take_result::<u64>(with_text(text, |word| unsafe { u64_kernels::parse(word) })))
    };
    assert_eq!(parse_u64("18446744073709551615"), Some(u64::MAX));
    assert_eq!(parse_u64("18446744073709551616"), None);
    // SAFETY: as above.
    let parse_base_u64 = |text: &str| {
        no_leaks(|| {
            take_result::<u64>(with_text(text, |word| unsafe {
                u64_kernels::parse_base(word, 16)
            }))
        })
    };
    assert_eq!(parse_base_u64("ffffffffffffffff"), Some(u64::MAX));
}

#[test]
fn parse_base_and_to_base_follow_int_md() {
    assert_eq!(parse_base("ff", 16), Some(255));
    assert_eq!(parse_base("FF", 16), Some(255));
    assert_eq!(parse_base("-101", 2), Some(-5));
    assert_eq!(parse_base("z", 36), Some(35));
    assert_eq!(parse_base("12", 2), None);
    assert_eq!(parse_base("0xff", 16), None);
    assert_eq!(parse_base("1", 37), None);
    assert_eq!(parse_base("1", 1), None);
    assert_eq!(parse_base("7b", 16), Some(123));
    assert_eq!(to_base_text(255, 16).as_deref(), Some("ff"));
    assert_eq!(to_base_text(-5, 2).as_deref(), Some("-101"));
    assert_eq!(to_base_text(0, 8).as_deref(), Some("0"));
    assert_eq!(to_base_text(123, 16).as_deref(), Some("7b"));
    assert_eq!(to_base_text(10, 1), None);
    assert_eq!(to_base_text(10, 37), None);
    assert_eq!(
        to_base_text(i64::MIN, 16).as_deref(),
        Some("-8000000000000000")
    );
}

#[test]
fn checked_neg_and_checked_pow_follow_int_md() {
    let neg = |n| no_leaks(|| take_result::<i64>(checked_neg(n)));
    let pow = |n, e| no_leaks(|| take_result::<i64>(checked_pow(n, e)));
    assert_eq!(neg(5), Some(-5));
    assert_eq!(neg(i64::MIN), None);
    assert_eq!(neg(i64::MAX), Some(-i64::MAX));
    assert_eq!(pow(2, 10), Some(1024));
    assert_eq!(pow(2, 62), Some(1 << 62));
    assert_eq!(pow(2, 63), None);
    assert_eq!(pow(-2, 63), Some(i64::MIN));
    assert_eq!(pow(-3, 3), Some(-27));
    assert_eq!(pow(0, 0), Some(1));
    // A negative exponent counts as 0 (@std_clamped, D-298, D-392).
    assert_eq!(pow(10, -2), Some(1));
    assert_eq!(pow(0, i64::MIN), Some(1));
    // Exponents beyond u32.
    assert_eq!(pow(1, i64::MAX), Some(1));
    assert_eq!(pow(-1, i64::MAX), Some(-1));
    assert_eq!(pow(-1, i64::MAX - 1), Some(1));
    assert_eq!(pow(0, i64::MAX), Some(0));
    assert_eq!(pow(2, i64::MAX), None);
    assert_eq!(pow(-2, 1 << 40), None);
}

#[test]
fn results_use_the_uniform_encoding_at_the_boundaries() {
    // Int results past 63 bits are boxed and read back.
    for n in [
        i64::MAX,
        i64::MIN,
        (1 << 62) - 1,
        1 << 62,
        -(1 << 62),
        -(1 << 62) - 1,
    ] {
        assert_eq!(
            no_leaks(|| take_result::<i64>(int_kernels::from_i64(n))),
            Some(n)
        );
    }
    // U64 values at the tagged boundary 2^62 - 1 and above.
    for n in [(1_u64 << 62) - 1, 1 << 62, u64::MAX] {
        assert_eq!(
            no_leaks(|| take_result::<u64>(u64_kernels::checked_add(n, 0))),
            Some(n)
        );
    }
    assert_eq!(
        no_leaks(|| take_result::<i64>(int_kernels::from_u64(u64::MAX))),
        None
    );
    assert_eq!(
        no_leaks(|| take_result::<i64>(int_kernels::from_u64(1 << 63))),
        None
    );
    assert_eq!(
        no_leaks(|| take_result::<i64>(int_kernels::from_u64((1 << 63) - 1))),
        Some(i64::MAX)
    );
}

proptest! {
    #[test]
    fn checked_pow_matches_repeated_multiplication(n in -40_i64..=40, exponent in -3_i64..=70) {
        let expected = (0..exponent.max(0)).try_fold(1_i64, |product, _| product.checked_mul(n));
        prop_assert_eq!(no_leaks(|| take_result::<i64>(checked_pow(n, exponent))), expected);
    }

    #[test]
    fn checked_neg_matches_rust(n: i64) {
        prop_assert_eq!(no_leaks(|| take_result::<i64>(checked_neg(n))), n.checked_neg());
    }

    #[test]
    fn to_base_round_trips(n: i64, base in 2_i64..=36) {
        let text = to_base_text(n, base).unwrap();
        prop_assert_eq!(&text, &to_radix(i128::from(n), base));
        prop_assert_eq!(parse_base(&text, base), Some(n));
    }
}

/// A kernel's address is the function itself (the JIT calls it by address).
#[test]
fn kernels_are_callable_through_their_addresses() {
    let mut out = Vec::new();
    crate::num::kernels(&mut out);
    let address = out
        .iter()
        .find(|(name, _)| *name == "cheby_int_checked_add")
        .map(|(_, address)| *address)
        .unwrap();
    // SAFETY: the registered address of `extern "C" fn(i64, i64) -> Word`.
    let kernel: extern "C" fn(i64, i64) -> Word = unsafe { std::mem::transmute(address) };
    assert_eq!(no_leaks(|| take_result::<i64>(kernel(2, 3))), Some(5));
}
