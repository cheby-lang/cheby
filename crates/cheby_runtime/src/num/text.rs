//! Text of numbers: `show` and `parse` of every numeric type, and
//! `int::parse_base` and `to_base`.
//!
//! - Integer text is an optional `-` and ASCII decimal digits (D-312).
//!   `parse_base` reads digits `0`-`9` and `a`-`z` in either case for bases
//!   2 to 36, and `to_base` writes lowercase digits (D-390).
//! - Float text follows §3.3.6, D-219 and D-395: the shortest digits that
//!   read back as the same value of the type (D-228 for `F32`), in decimal
//!   form when the decimal exponent is from −6 to 20 and in exponent form
//!   otherwise, with `NaN`, `inf`, `-inf` and `-0.0` for the special values.
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
    // GAP(unsigned-minus-zero): `-0` is the value 0, which every integer
    // type holds, so it parses on the unsigned types too (D-312 checks the
    // value's range, not the sign's presence).
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
        #![proptest_config(ProptestConfig::with_cases(20_000))]

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
        fn decimal_text_parses_like_rust(text in "-?[0-9]{1,25}(\\.[0-9]{1,25})?([eE][+-]?[0-9]{1,4})?") {
            let ours = parse_float(text.as_bytes()).map(f64::to_bits);
            prop_assert_eq!(ours, text.parse::<f64>().ok().map(f64::to_bits));
        }
    }
}
