//! Text of numbers: `show` and `parse` of every numeric type,
//! `int::parse_base` and `to_base`, and `float::to_fixed`.
//!
//! - Integer text is an optional `-` and ASCII decimal digits (D-312).
//!   `parse_base` reads digits `0`-`9` and `a`-`z` in either case for bases
//!   2 to 36, and `to_base` writes lowercase digits (D-390).
//! - Float text follows §3.3.6, D-219 and D-395: the shortest digits that
//!   read back as the same value of the type (D-228 for `F32`), in decimal
//!   form when the decimal exponent is from −6 to 20 and in exponent form
//!   otherwise, with `NaN`, `inf`, `-inf` and `-0.0` for the special values.
//! - `float::to_fixed` writes the exact binary value rounded to a number of
//!   decimals, ties away from zero, with no exponent (D-P06).
//! - Float parsing accepts exactly D-313's grammar and rounds the decimal
//!   value to the nearest value of the type, ties to even, with infinities
//!   and signed zeros beyond the range (D-394).
//!
//! The parsers check the grammar themselves before converting, because
//! Rust's `str::parse` accepts more (`+1`, `.5`, `1.`, `infinity`).

use std::fmt::LowerExp;

/// The smallest and largest decimal exponents written in decimal form
/// (D-395).
const DECIMAL_FORM: std::ops::RangeInclusive<i32> = -6..=20;

/// `show` of a `Float` (§3.3.6, D-203, D-219, D-395).
#[must_use]
pub(crate) fn float_to_string(x: f64) -> String {
    if x.is_nan() {
        return String::from("NaN");
    }
    if x.is_infinite() {
        return String::from(if x < 0.0 { "-inf" } else { "inf" });
    }
    finite_to_string(x)
}

/// `show` of an `F32`, with the shortest digits judged at `F32` precision
/// (§3.3.6, D-228).
#[must_use]
pub(crate) fn f32_to_string(x: f32) -> String {
    if x.is_nan() {
        return String::from("NaN");
    }
    if x.is_infinite() {
        return String::from(if x < 0.0 { "-inf" } else { "inf" });
    }
    finite_to_string(x)
}

/// The largest `digits` of `float::to_fixed`, which clamps it to 0 to 100
/// (D-P06, D-298).
const MAX_FIXED_DIGITS: i64 = 100;

/// `float::to_fixed` (D-P06): `x` in decimal with exactly `digits` digits
/// after the point, none and no point for 0, and never an exponent.
///
/// The digits are those of the exact binary value of `x`, rounded at
/// `digits` with ties away from zero. Rust's `{:.N}` rounds ties to even,
/// so this rounds the exact expansion itself: with every digit exact, the
/// first digit dropped decides alone, rounding the magnitude up when it is
/// 5 or more. The text starts with `-` whenever the sign bit is set, so
/// `-0.0` gives `-0.00`, and NaN and the infinities are written as `show`
/// writes them. `digits` is clamped to 0 to 100.
#[must_use]
pub(crate) fn float_to_fixed(x: f64, digits: i64) -> String {
    if !x.is_finite() {
        return float_to_string(x);
    }
    let kept = usize::try_from(digits.clamp(0, MAX_FIXED_DIGITS)).unwrap_or(0);
    let (whole, fraction) = exact_decimal(x);
    // The kept digits, the whole part and then `kept` fraction digits,
    // with zeros past the last digit of the expansion.
    let mut number = whole;
    number.extend(
        fraction
            .iter()
            .copied()
            .chain(std::iter::repeat(b'0'))
            .take(kept),
    );
    if fraction
        .get(kept)
        .is_some_and(|&first_dropped| first_dropped >= b'5')
    {
        increment_decimal(&mut number);
    }
    let point = number.len().saturating_sub(kept);
    let (whole, fraction) = number.split_at_checked(point).unwrap_or_default();
    let mut out = String::with_capacity(number.len().saturating_add(2));
    if x.is_sign_negative() {
        out.push('-');
    }
    push_digits(&mut out, whole);
    if !fraction.is_empty() {
        out.push('.');
        push_digits(&mut out, fraction);
    }
    out
}

/// The exact decimal expansion of a finite `|x|`: the digits of its whole
/// part, `0` when it is below 1, and every digit of its fraction, empty
/// for a whole number. A binary64 value is `m × 2^e` with `m < 2^53`, so a
/// whole value is `m × 2^e` and the fraction of `f / 2^k` is the `k`-digit
/// text of `f × 5^k`, at most 1074 digits.
fn exact_decimal(x: f64) -> (Vec<u8>, Vec<u8>) {
    const FRACTION_BITS: u64 = 0x000f_ffff_ffff_ffff;
    const IMPLICIT_BIT: u64 = 0x0010_0000_0000_0000;
    let bits = x.to_bits();
    let biased = bits.wrapping_shr(52) & 0x7ff;
    let (mantissa, exponent) = if biased == 0 {
        (bits & FRACTION_BITS, -1074_i64)
    } else {
        let biased = i64::try_from(biased).unwrap_or(0);
        (
            (bits & FRACTION_BITS) | IMPLICIT_BIT,
            biased.wrapping_sub(1075),
        )
    };
    if let Ok(shift) = u32::try_from(exponent) {
        let mut whole = Decimal::from_u64(mantissa);
        whole.mul_pow(2, 31, shift);
        return (whole.digits(), Vec::new());
    }
    let k = u32::try_from(exponent.unsigned_abs()).unwrap_or(0);
    let whole = mantissa.checked_shr(k).unwrap_or(0);
    let fraction = mantissa & 1_u64.checked_shl(k).map_or(u64::MAX, |p| p.wrapping_sub(1));
    if fraction == 0 {
        return (Decimal::from_u64(whole).digits(), Vec::new());
    }
    let mut scaled = Decimal::from_u64(fraction);
    scaled.mul_pow(5, 13, k);
    let scaled = scaled.digits();
    let width = usize::try_from(k).unwrap_or(0);
    let mut digits = vec![b'0'; width.saturating_sub(scaled.len())];
    digits.extend(scaled);
    (Decimal::from_u64(whole).digits(), digits)
}

/// Adds 1 to the number whose ASCII decimal digits are `digits`, which may
/// make it one digit longer.
fn increment_decimal(digits: &mut Vec<u8>) {
    for digit in digits.iter_mut().rev() {
        if *digit == b'9' {
            *digit = b'0';
        } else {
            *digit = digit.wrapping_add(1);
            return;
        }
    }
    digits.insert(0, b'1');
}

/// A natural number in base 10^9, least significant limb first, for the
/// exact expansions of [`float_to_fixed`]. It is never empty.
struct Decimal(Vec<u32>);

impl Decimal {
    const BASE: u64 = 1_000_000_000;

    fn from_u64(n: u64) -> Self {
        let mut limbs = Vec::with_capacity(3);
        let mut rest = n;
        loop {
            limbs.push(u32::try_from(rest.wrapping_rem(Self::BASE)).unwrap_or(0));
            rest = rest.wrapping_div(Self::BASE);
            if rest == 0 {
                return Self(limbs);
            }
        }
    }

    /// Multiplies by `factor`. A limb is below 2^30 and `factor` below
    /// 2^32, so a product and its carry stay below 2^63.
    fn mul_small(&mut self, factor: u32) {
        let mut carry = 0_u64;
        for limb in &mut self.0 {
            let product = u64::from(*limb)
                .wrapping_mul(u64::from(factor))
                .wrapping_add(carry);
            *limb = u32::try_from(product.wrapping_rem(Self::BASE)).unwrap_or(0);
            carry = product.wrapping_div(Self::BASE);
        }
        while carry != 0 {
            self.0
                .push(u32::try_from(carry.wrapping_rem(Self::BASE)).unwrap_or(0));
            carry = carry.wrapping_div(Self::BASE);
        }
    }

    /// Multiplies by `base^exponent`, `step` powers at a time, where
    /// `base^step` fits a `u32`.
    fn mul_pow(&mut self, base: u32, step: u32, exponent: u32) {
        let mut left = exponent;
        while left > 0 {
            let now = left.min(step);
            self.mul_small(base.pow(now));
            left = left.wrapping_sub(now);
        }
    }

    /// The ASCII digits, with no leading zeros.
    fn digits(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.0.len().saturating_mul(9));
        let mut limbs = self.0.iter().rev();
        if let Some(top) = limbs.next() {
            out.extend(top.to_string().bytes());
        }
        for limb in limbs {
            out.extend(format!("{limb:09}").bytes());
        }
        out
    }
}

/// Lays out the shortest round-trip digits of a finite float.
///
/// Ryu writes the shortest digits that read back as the same value of the
/// type, the closest of them to the value, and of two equally close the
/// one whose last digit is even, which is §3.3.6's rule. (Rust's own `{:e}`
/// breaks such ties upwards: it writes `-1.4334993e6` for the `F32`
/// −1433499.25, where the rule gives `-1433499.2`.) This takes the digits
/// and the exponent from Ryu's text and writes them in Cheby's form.
fn finite_to_string<F: ryu::Float + LowerExp>(x: F) -> String {
    let mut buffer = ryu::Buffer::new();
    match Shortest::read(buffer.format_finite(x)) {
        Some(shortest) => shortest.layout(),
        // Unreachable: Ryu's text of a finite float always reads.
        None => format!("{x:e}"),
    }
}

/// A finite float as a sign, decimal digits d₁d₂…dₙ (n ≥ 1, no leading or
/// trailing zero unless the value is zero) and the exponent p of
/// d₁.dd… × 10ᵖ.
#[derive(Debug, PartialEq, Eq)]
struct Shortest {
    negative: bool,
    digits: Vec<u8>,
    exponent: i32,
}

impl Shortest {
    /// Reads a decimal float written as `[-]digits[.digits][e[-]digits]`,
    /// such as Ryu's `-1.25e-7`, `100.0`, `0.001` or `1e21`.
    fn read(text: &str) -> Option<Self> {
        let (negative, rest) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        let (mantissa, exponent) = match rest.split_once(['e', 'E']) {
            Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().ok()?),
            None => (rest, 0),
        };
        let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let all: Vec<u8> = whole.bytes().chain(fraction.bytes()).collect();
        if all.is_empty() || !all.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let Some(first) = all.iter().position(|&d| d != b'0') else {
            return Some(Self {
                negative,
                digits: vec![b'0'],
                exponent: 0,
            });
        };
        let last = all.iter().rposition(|&d| d != b'0').unwrap_or(first);
        let digits = all.get(first..=last)?.to_vec();
        // The first significant digit stands at 10^(len(whole) - 1 - first).
        let whole_len = i32::try_from(whole.len()).ok()?;
        let first = i32::try_from(first).ok()?;
        let exponent = exponent
            .checked_add(whole_len)?
            .checked_sub(1)?
            .checked_sub(first)?;
        Some(Self {
            negative,
            digits,
            exponent,
        })
    }

    /// Writes the digits in decimal form or exponent form (D-395).
    fn layout(&self) -> String {
        let mut out = String::with_capacity(self.digits.len().saturating_add(26));
        if self.negative {
            out.push('-');
        }
        let (first, rest) = self.digits.split_first().unwrap_or((&b'0', &[]));
        if !DECIMAL_FORM.contains(&self.exponent) {
            out.push(char::from(*first));
            out.push('.');
            push_digits_or_zero(&mut out, rest);
            out.push('e');
            out.push_str(&self.exponent.to_string());
            return out;
        }
        if let Ok(p) = usize::try_from(self.exponent) {
            // d.ddd × 10^p with p >= 0: p + 1 digits before the point.
            let whole = p.saturating_add(1);
            let (int_part, frac_part) = self.digits.split_at(whole.min(self.digits.len()));
            push_digits(&mut out, int_part);
            for _ in int_part.len()..whole {
                out.push('0');
            }
            out.push('.');
            push_digits_or_zero(&mut out, frac_part);
        } else {
            // p < 0: 0.000ddd with -p - 1 zeros after the point.
            out.push_str("0.");
            let zeros = self.exponent.unsigned_abs().saturating_sub(1);
            for _ in 0..zeros {
                out.push('0');
            }
            push_digits(&mut out, &self.digits);
        }
        out
    }
}

fn push_digits(out: &mut String, digits: &[u8]) {
    out.extend(digits.iter().map(|&d| char::from(d)));
}

fn push_digits_or_zero(out: &mut String, digits: &[u8]) {
    if digits.is_empty() {
        out.push('0');
    } else {
        push_digits(out, digits);
    }
}

/// `float::parse` (D-313, D-394).
#[must_use]
pub(crate) fn parse_float(text: &[u8]) -> Option<f64> {
    match float_text(text)? {
        FloatText::NaN => Some(f64::NAN),
        FloatText::Infinity => Some(f64::INFINITY),
        FloatText::NegInfinity => Some(f64::NEG_INFINITY),
        // Rust's parser rounds the decimal value correctly to the nearest
        // binary64, ties to even, overflowing to an infinity and
        // underflowing to a zero with its sign.
        FloatText::Number(text) => text.parse().ok(),
    }
}

/// `f32::parse`: straight to the nearest `F32`, never through a `Float`, so
/// it never rounds twice (D-313).
#[must_use]
pub(crate) fn parse_f32(text: &[u8]) -> Option<f32> {
    match float_text(text)? {
        FloatText::NaN => Some(f32::NAN),
        FloatText::Infinity => Some(f32::INFINITY),
        FloatText::NegInfinity => Some(f32::NEG_INFINITY),
        FloatText::Number(text) => text.parse().ok(),
    }
}

enum FloatText<'a> {
    NaN,
    Infinity,
    NegInfinity,
    Number(&'a str),
}

/// Checks D-313's grammar:
///
/// ```ebnf
/// float_text = [ "-" ] digits [ "." digits ] [ ( "e" | "E" ) [ "+" | "-" ] digits ]
///            | "NaN" | "inf" | "-inf" ;
/// ```
fn float_text(text: &[u8]) -> Option<FloatText<'_>> {
    match text {
        b"NaN" => return Some(FloatText::NaN),
        b"inf" => return Some(FloatText::Infinity),
        b"-inf" => return Some(FloatText::NegInfinity),
        _ => {}
    }
    let rest = text.strip_prefix(b"-").unwrap_or(text);
    let rest = skip_digits(rest)?;
    let rest = match rest.strip_prefix(b".") {
        Some(fraction) => skip_digits(fraction)?,
        None => rest,
    };
    let rest = match rest.split_first() {
        Some((b'e' | b'E', exponent)) => {
            let exponent = match exponent.split_first() {
                Some((b'+' | b'-', digits)) => digits,
                _ => exponent,
            };
            skip_digits(exponent)?
        }
        _ => rest,
    };
    if !rest.is_empty() {
        return None;
    }
    // Only ASCII bytes were accepted, so the text is UTF-8.
    std::str::from_utf8(text).ok().map(FloatText::Number)
}

/// Skips one or more ASCII digits; `None` when there is none.
fn skip_digits(text: &[u8]) -> Option<&[u8]> {
    let count = text.iter().take_while(|b| b.is_ascii_digit()).count();
    if count == 0 {
        return None;
    }
    text.get(count..)
}

/// Reads an integer written as an optional `-` and one or more digits of
/// `base`, and nothing else, as a value of `T` (D-312). `None` for any other
/// text, for a base outside 2 to 36, and for a value outside `T`'s range.
#[must_use]
pub(crate) fn parse_int<T: TryFrom<i128>>(text: &[u8], base: i64) -> Option<T> {
    let base = u32::try_from(base).ok().filter(|b| (2..=36).contains(b))?;
    let (negative, digits) = match text.strip_prefix(b"-") {
        Some(digits) => (true, digits),
        None => (false, text),
    };
    if digits.is_empty() {
        return None;
    }
    let mut magnitude: u128 = 0;
    for &byte in digits {
        let digit = char::from(byte).to_digit(base)?;
        magnitude = magnitude
            .checked_mul(u128::from(base))?
            .checked_add(u128::from(digit))?;
    }
    let magnitude = i128::try_from(magnitude).ok()?;
    // `-0` is the value 0, which every integer type holds, so it parses on
    // the unsigned types too: D-312 checks the value's range, not the
    // sign's presence (D-558).
    let value = if negative {
        magnitude.checked_neg()?
    } else {
        magnitude
    };
    T::try_from(value).ok()
}

/// `int::to_base`: `n` in `base`, from 2 to 36, with lowercase digits, `-`
/// for a negative value, no prefix and no leading zeros (D-390).
#[must_use]
pub(crate) fn int_to_base(n: i64, base: i64) -> Option<String> {
    let base = u32::try_from(base).ok().filter(|b| (2..=36).contains(b))?;
    let wide = u64::from(base);
    let mut magnitude = n.unsigned_abs();
    let mut digits = Vec::new();
    loop {
        let digit = u32::try_from(magnitude.checked_rem(wide)?).ok()?;
        digits.push(char::from_digit(digit, base)?);
        magnitude = magnitude.checked_div(wide)?;
        if magnitude == 0 {
            break;
        }
    }
    if n < 0 {
        digits.push('-');
    }
    Some(digits.iter().rev().collect())
}

#[cfg(test)]
#[expect(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "tests do arithmetic on small exponents and show a deliberate double rounding"
)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn float_show_follows_section_3_3_6() {
        let cases: &[(f64, &str)] = &[
            (2.25, "2.25"),
            (-2.25, "-2.25"),
            (2.0, "2.0"),
            (10.0, "10.0"),
            (0.1, "0.1"),
            (0.5, "0.5"),
            (100.0, "100.0"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1.0e20, "100000000000000000000.0"),
            (1.0e21, "1.0e21"),
            (1.0e-7, "1.0e-7"),
            (0.000_001, "0.000001"),
            (0.000_001_5, "0.0000015"),
            (123_456_789_012_345_680_000.0, "123456789012345680000.0"),
            (-1.5e300, "-1.5e300"),
            (1.0 / 3.0, "0.3333333333333333"),
            (f64::MAX, "1.7976931348623157e308"),
            (f64::MIN_POSITIVE, "2.2250738585072014e-308"),
            (5e-324, "5.0e-324"),
            (f64::EPSILON, "2.220446049250313e-16"),
            (std::f64::consts::PI, "3.141592653589793"),
            (9_007_199_254_740_992.0, "9007199254740992.0"),
            (18_446_744_073_709_551_616.0, "18446744073709552000.0"),
            (1.0e16, "10000000000000000.0"),
            (1.5e-6, "0.0000015"),
            (1.25e21, "1.25e21"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (f64::NAN, "NaN"),
            (-f64::NAN, "NaN"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
        ];
        for &(x, text) in cases {
            assert_eq!(float_to_string(x), text, "{x:?}");
        }
    }

    #[test]
    fn f32_show_is_judged_at_f32_precision() {
        let cases: &[(f32, &str)] = &[
            (0.1, "0.1"),
            (2.25, "2.25"),
            (10.0, "10.0"),
            (f32::MAX, "3.4028235e38"),
            (f32::MIN, "-3.4028235e38"),
            (f32::EPSILON, "1.1920929e-7"),
            (std::f32::consts::PI, "3.1415927"),
            (std::f32::consts::E, "2.7182817"),
            (16_777_216.0, "16777216.0"),
            (1.0e20, "100000000000000000000.0"),
            (1.0e21, "1.0e21"),
            (1.0e-7, "1.0e-7"),
            (0.000_001, "0.000001"),
            (f32::from_bits(1), "1.0e-45"),
            // The literal is the F32 -1433499.25, an exact tie between
            // -1433499.2 and -1433499.3: the even digit wins.
            (-1_433_499.2, "-1433499.2"),
            (-0.0, "-0.0"),
            (f32::NAN, "NaN"),
            (f32::INFINITY, "inf"),
            (f32::NEG_INFINITY, "-inf"),
        ];
        for &(x, text) in cases {
            assert_eq!(f32_to_string(x), text, "{x:?}");
        }
    }

    #[test]
    fn float_parse_accepts_d313() {
        let cases: &[(&str, f64)] = &[
            ("42", 42.0),
            ("1e3", 1000.0),
            ("1E+3", 1000.0),
            ("-1.5", -1.5),
            ("-1.5e-3", -0.0015),
            ("0.1", 0.1),
            ("007.50", 7.5),
            ("1e400", f64::INFINITY),
            ("-1e400", f64::NEG_INFINITY),
            ("1e-400", 0.0),
            ("1e99999999999999999999999", f64::INFINITY),
            ("1e-99999999999999999999999", 0.0),
            ("inf", f64::INFINITY),
            ("-inf", f64::NEG_INFINITY),
            ("9007199254740993", 9_007_199_254_740_992.0),
            ("2.2250738585072011e-308", 2.225_073_858_507_201e-308),
            ("4.9406564584124654e-324", 5e-324),
        ];
        for &(text, value) in cases {
            let parsed = parse_float(text.as_bytes());
            assert_eq!(parsed.map(f64::to_bits), Some(value.to_bits()), "{text}");
        }
        assert!(parse_float(b"NaN").is_some_and(f64::is_nan));
        let negative_zero = parse_float(b"-0").map(f64::to_bits);
        assert_eq!(negative_zero, Some((-0.0f64).to_bits()));
        let negative_tiny = parse_float(b"-1e-400").map(f64::to_bits);
        assert_eq!(negative_tiny, Some((-0.0f64).to_bits()));
    }

    #[test]
    fn float_parse_rejects_other_spellings() {
        for text in [
            "", "-", "+1", "+1.0", ".5", "1.", "-.5", "1.e3", "e3", "1e", "1e+", "1_000.0", " 1",
            "1 ", "1.0 ", "infinity", "Infinity", "nan", "NAN", "-NaN", "+inf", "Inf", "0x10",
            "1.5f", "1,5", "١", "--1", "1e3.5", "1ee3",
        ] {
            assert_eq!(parse_float(text.as_bytes()), None, "{text:?}");
            assert_eq!(parse_f32(text.as_bytes()), None, "{text:?}");
        }
    }

    #[test]
    fn f32_parse_rounds_once() {
        assert_eq!(parse_f32(b"0.1"), Some(0.1f32));
        assert_eq!(parse_f32(b"1e3"), Some(1000.0f32));
        assert_eq!(parse_f32(b"-1.5"), Some(-1.5f32));
        assert_eq!(parse_f32(b"42"), Some(42.0f32));
        assert_eq!(parse_f32(b"3.4028235e38"), Some(f32::MAX));
        // Just above the midpoint between `F32`'s largest value and 2^128.
        assert_eq!(parse_f32(b"3.4028236e38"), Some(f32::INFINITY));
        assert_eq!(parse_f32(b"1e39"), Some(f32::INFINITY));
        // 1 + 2^-24 + 2^-60 rounds up at F32 precision; through a Float it
        // would first round to the tie 1 + 2^-24 and then to even, 1.0.
        let text = b"1.00000005960464477539930798505163";
        assert_eq!(parse_f32(text), Some(1.000_000_1_f32));
        assert_eq!(parse_float(text).map(|x| x as f32), Some(1.0f32));
    }

    #[test]
    fn int_parse_follows_d312() {
        assert_eq!(parse_int::<i64>(b"42", 10), Some(42));
        assert_eq!(parse_int::<i64>(b"-007", 10), Some(-7));
        assert_eq!(parse_int::<i64>(b"9223372036854775807", 10), Some(i64::MAX));
        assert_eq!(
            parse_int::<i64>(b"-9223372036854775808", 10),
            Some(i64::MIN)
        );
        assert_eq!(
            parse_int::<i64>(b"0000000000000000000000000000000000000000000001", 10),
            Some(1)
        );
        for text in [
            "+1",
            " 1",
            "1 ",
            "1_000",
            "0x10",
            "",
            "-",
            "--1",
            "1.0",
            "1e3",
            "٣",
            "9223372036854775808",
            "-9223372036854775809",
            "999999999999999999999999999999999999999999",
        ] {
            assert_eq!(parse_int::<i64>(text.as_bytes(), 10), None, "{text:?}");
        }
        assert_eq!(parse_int::<u8>(b"255", 10), Some(255));
        assert_eq!(parse_int::<u8>(b"256", 10), None);
        assert_eq!(parse_int::<u8>(b"-1", 10), None);
        assert_eq!(parse_int::<u8>(b"-0", 10), Some(0));
        assert_eq!(parse_int::<i8>(b"-128", 10), Some(-128));
        assert_eq!(parse_int::<i8>(b"128", 10), None);
        assert_eq!(
            parse_int::<u64>(b"18446744073709551615", 10),
            Some(u64::MAX)
        );
        assert_eq!(parse_int::<u64>(b"18446744073709551616", 10), None);
    }

    #[test]
    fn parse_base_and_to_base_follow_int_md() {
        assert_eq!(parse_int::<i64>(b"ff", 16), Some(255));
        assert_eq!(parse_int::<i64>(b"FF", 16), Some(255));
        assert_eq!(parse_int::<i64>(b"-101", 2), Some(-5));
        assert_eq!(parse_int::<i64>(b"z", 36), Some(35));
        assert_eq!(parse_int::<i64>(b"Z", 36), Some(35));
        assert_eq!(parse_int::<i64>(b"12", 2), None);
        assert_eq!(parse_int::<i64>(b"0xff", 16), None);
        assert_eq!(parse_int::<i64>(b"+ff", 16), None);
        assert_eq!(parse_int::<i64>(b"1", 37), None);
        assert_eq!(parse_int::<i64>(b"1", 1), None);
        assert_eq!(parse_int::<i64>(b"0", 0), None);
        assert_eq!(parse_int::<i64>(b"1", -2), None);
        assert_eq!(parse_int::<i64>(b"1", i64::MAX), None);
        assert_eq!(parse_int::<i64>(b"7fffffffffffffff", 16), Some(i64::MAX));
        assert_eq!(parse_int::<i64>(b"-8000000000000000", 16), Some(i64::MIN));
        assert_eq!(parse_int::<i64>(b"8000000000000000", 16), None);
        assert_eq!(parse_int::<u64>(b"ffffffffffffffff", 16), Some(u64::MAX));

        assert_eq!(int_to_base(255, 16).as_deref(), Some("ff"));
        assert_eq!(int_to_base(-5, 2).as_deref(), Some("-101"));
        assert_eq!(int_to_base(0, 8).as_deref(), Some("0"));
        assert_eq!(int_to_base(123, 16).as_deref(), Some("7b"));
        assert_eq!(int_to_base(35, 36).as_deref(), Some("z"));
        assert_eq!(
            int_to_base(i64::MIN, 2).as_deref(),
            Some(&*format!("-1{}", "0".repeat(63)))
        );
        assert_eq!(
            int_to_base(i64::MIN, 16).as_deref(),
            Some("-8000000000000000")
        );
        assert_eq!(int_to_base(10, 1), None);
        assert_eq!(int_to_base(10, 37), None);
        assert_eq!(int_to_base(10, -16), None);
    }

    #[test]
    fn increment_decimal_carries() {
        let cases: &[(&str, &str)] = &[
            ("0", "1"),
            ("8", "9"),
            ("9", "10"),
            ("0099", "0100"),
            ("999", "1000"),
            ("1299", "1300"),
        ];
        for &(before, after) in cases {
            let mut digits = before.as_bytes().to_vec();
            increment_decimal(&mut digits);
            assert_eq!(digits, after.as_bytes(), "{before}");
        }
    }

    #[test]
    fn exact_decimal_writes_every_digit() {
        // The fraction of `f / 2^k` has `k` digits; the trailing zeros are
        // dropped here.
        let digits = |x: f64| {
            let (whole, fraction) = exact_decimal(x);
            let fraction = String::from_utf8(fraction).unwrap();
            (
                String::from_utf8(whole).unwrap(),
                fraction.trim_end_matches('0').to_owned(),
            )
        };
        let own = |whole: &str, fraction: &str| (whole.to_owned(), fraction.to_owned());
        assert_eq!(digits(0.0), own("0", ""));
        assert_eq!(digits(-0.0), own("0", ""));
        assert_eq!(digits(2.0), own("2", ""));
        assert_eq!(digits(0.125), own("0", "125"));
        assert_eq!(digits(-2.5), own("2", "5"));
        assert_eq!(digits(1.0e21), own("1000000000000000000000", ""));
        assert_eq!(digits(9_007_199_254_740_993.0), own("9007199254740992", ""));
        // 2^-1074 has 1074 fraction digits, the last a 5.
        let (whole, fraction) = exact_decimal(5e-324);
        let fraction = String::from_utf8(fraction).unwrap();
        assert_eq!(whole, b"0");
        assert_eq!(fraction.len(), 1074);
        assert!(fraction.starts_with(&format!("{}49406564584124654", "0".repeat(323))));
        assert!(fraction.ends_with('5'));
        assert_eq!(digits(f64::MAX).0.len(), 309);
    }

    /// D-P06's text from an independent expansion: Rust's `{:.1100}` writes
    /// every digit of the exact value, since none has more than 1074 after
    /// the point, and the result rounds it at `digits`, ties away from zero.
    fn fixed_reference(x: f64, digits: usize) -> String {
        let full = format!("{:.1100}", x.abs());
        let (whole, fraction) = full.split_once('.').unwrap();
        let mut number: Vec<u8> = whole.bytes().chain(fraction.bytes().take(digits)).collect();
        if fraction.as_bytes()[digits] >= b'5' {
            increment_decimal(&mut number);
        }
        let (whole, fraction) = number.split_at(number.len() - digits);
        let sign = if x.is_sign_negative() { "-" } else { "" };
        let whole = std::str::from_utf8(whole).unwrap();
        let fraction = std::str::from_utf8(fraction).unwrap();
        if digits == 0 {
            format!("{sign}{whole}")
        } else {
            format!("{sign}{whole}.{fraction}")
        }
    }

    /// Whether `x` is exactly halfway between two values with `digits`
    /// decimals: its expansion goes on past `digits` with a 5 and nothing
    /// after it.
    fn is_fixed_tie(x: f64, digits: usize) -> bool {
        let full = format!("{:.1100}", x.abs());
        let (_, fraction) = full.split_once('.').unwrap();
        let rest = &fraction.as_bytes()[digits..];
        rest.first() == Some(&b'5') && rest[1..].iter().all(|&d| d == b'0')
    }

    /// Checks `float_to_fixed` against the reference, and away from ties
    /// against Rust's own `{:.N}`, which rounds the exact value too and
    /// differs only at a tie, which it rounds to even (D-P06).
    fn check_fixed(x: f64, digits: usize) -> Result<(), TestCaseError> {
        let ours = float_to_fixed(x, i64::try_from(digits).unwrap());
        prop_assert_eq!(&ours, &fixed_reference(x, digits), "{:e} {}", x, digits);
        if !is_fixed_tie(x, digits) {
            prop_assert_eq!(&ours, &format!("{x:.digits$}"), "{:e} {}", x, digits);
        }
        prop_assert!(!ours.contains('e'));
        prop_assert_eq!(ours.contains('.'), digits > 0);
        prop_assert_eq!(ours.starts_with('-'), x.is_sign_negative());
        Ok(())
    }

    /// Checks Ryu's digits (ours) against Rust's `{:e}`, which picks the
    /// same shortest, closest digits but breaks an exact tie upwards: where
    /// they differ, ours must be the even one, one below in the last digit.
    fn agrees_with_core(ours: &Shortest, core: &Shortest) -> bool {
        if ours == core {
            return true;
        }
        let (Some((&a, a_rest)), Some((&b, b_rest))) =
            (ours.digits.split_last(), core.digits.split_last())
        else {
            return false;
        };
        ours.negative == core.negative
            && ours.exponent == core.exponent
            && a_rest == b_rest
            && a % 2 == 0
            && b == a + 1
    }

    proptest! {
        // 4 cases under Miri, as the Miri job's `PROPTEST_CASES` (D-721).
        #![proptest_config(ProptestConfig::with_cases(if cfg!(miri) { 4 } else { 20_000 }))]

        #[test]
        fn float_text_round_trips(bits: u64) {
            let x = f64::from_bits(bits);
            let text = float_to_string(x);
            let back = parse_float(text.as_bytes()).unwrap();
            prop_assert!(back.to_bits() == x.to_bits() || (x.is_nan() && back.is_nan()), "{text}");
            prop_assert!(text.contains('.') || text.contains('e') || !x.is_finite());
        }

        #[test]
        fn f32_text_round_trips(bits: u32) {
            let x = f32::from_bits(bits);
            let text = f32_to_string(x);
            let back = parse_f32(text.as_bytes()).unwrap();
            prop_assert!(back.to_bits() == x.to_bits() || (x.is_nan() && back.is_nan()), "{text}");
        }

        #[test]
        fn float_digits_are_core_digits_with_even_ties(bits: u64) {
            let x = f64::from_bits(bits);
            prop_assume!(x.is_finite());
            let mut buffer = ryu::Buffer::new();
            let ours = Shortest::read(buffer.format_finite(x)).unwrap();
            let core = Shortest::read(&format!("{x:e}")).unwrap();
            prop_assert!(agrees_with_core(&ours, &core), "{ours:?} {core:?}");
        }

        #[test]
        fn f32_digits_are_core_digits_with_even_ties(bits: u32) {
            let x = f32::from_bits(bits);
            prop_assume!(x.is_finite());
            let mut buffer = ryu::Buffer::new();
            let ours = Shortest::read(buffer.format_finite(x)).unwrap();
            let core = Shortest::read(&format!("{x:e}")).unwrap();
            prop_assert!(agrees_with_core(&ours, &core), "{ours:?} {core:?}");
        }

        #[test]
        fn int_text_round_trips(n: i64) {
            prop_assert_eq!(parse_int::<i64>(n.to_string().as_bytes(), 10), Some(n));
            for base in 2..=36 {
                let text = int_to_base(n, base).unwrap();
                prop_assert_eq!(parse_int::<i64>(text.as_bytes(), base), Some(n));
                prop_assert_eq!(i64::from_str_radix(&text, u32::try_from(base).unwrap()).ok(), Some(n));
            }
        }

        #[test]
        fn to_fixed_rounds_the_exact_value(bits: u64, digits in 0_usize..=100) {
            let x = f64::from_bits(bits);
            prop_assume!(x.is_finite());
            check_fixed(x, digits)?;
        }

        #[test]
        fn to_fixed_rounds_moderate_values(x in -1.0e7_f64..1.0e7, digits in 0_usize..=25) {
            check_fixed(x, digits)?;
        }

        /// `n / 2^j` has exactly `j` fraction digits, the last a 5, so it is
        /// a tie at `j - 1` digits.
        #[test]
        fn to_fixed_rounds_ties_away_from_zero(n in -1_000_000_i32..=1_000_000, j in 0_i32..=40, extra in 0_usize..=3) {
            let x = f64::from(n) * 2.0_f64.powi(-j);
            let digits = usize::try_from(j).unwrap().saturating_sub(1) + extra;
            check_fixed(x, digits)?;
        }

        #[test]
        fn decimal_text_parses_like_rust(text in "-?[0-9]{1,25}(\\.[0-9]{1,25})?([eE][+-]?[0-9]{1,4})?") {
            let ours = parse_float(text.as_bytes()).map(f64::to_bits);
            prop_assert_eq!(ours, text.parse::<f64>().ok().map(f64::to_bits));
        }
    }
}
