//! Tests of the float kernels.

#![expect(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "the tests compare kernels with Rust's own casts and exact float values"
)]

use proptest::prelude::*;

use super::{f32_kernels, float_kernels};
use crate::num::tests::{no_leaks, take_result, take_string, with_text};

fn show(x: f64) -> String {
    no_leaks(|| take_string(float_kernels::show(x)))
}

fn show_f32(x: f32) -> String {
    no_leaks(|| take_string(f32_kernels::show(x)))
}

fn parse(text: &str) -> Option<f64> {
    // SAFETY: `word` is the live string `with_text` made.
    no_leaks(|| {
        take_result(with_text(text, |word| unsafe {
            float_kernels::parse(word)
        }))
    })
}

fn parse_f32(text: &str) -> Option<f32> {
    // SAFETY: as above.
    no_leaks(|| take_result(with_text(text, |word| unsafe { f32_kernels::parse(word) })))
}

#[test]
fn float_show_follows_section_3_3_6() {
    let cases: &[(f64, &str)] = &[
        (2.25, "2.25"),
        (10.0, "10.0"),
        (0.1, "0.1"),
        (100.0, "100.0"),
        (1.0e21, "1.0e21"),
        (1.0e-7, "1.0e-7"),
        (0.000_001, "0.000001"),
        (123_456_789_012_345_680_000.0, "123456789012345680000.0"),
        (1.0e20, "100000000000000000000.0"),
        (-1.5e300, "-1.5e300"),
        (0.1 + 0.2, "0.30000000000000004"),
        (f64::NAN, "NaN"),
        (f64::INFINITY, "inf"),
        (f64::NEG_INFINITY, "-inf"),
        (-0.0, "-0.0"),
        (0.0, "0.0"),
    ];
    for &(x, text) in cases {
        assert_eq!(show(x), text, "{x:?}");
    }
}

#[test]
fn f32_show_is_judged_at_f32_precision() {
    assert_eq!(show_f32(0.1), "0.1");
    assert_eq!(show_f32(2.25), "2.25");
    assert_eq!(show_f32(f32::MAX), "3.4028235e38");
    assert_eq!(show_f32(f32::NEG_INFINITY), "-inf");
    assert_eq!(show_f32(f32::NAN), "NaN");
    assert_eq!(show_f32(-0.0), "-0.0");
    assert_eq!(show_f32(1.0e21), "1.0e21");
    assert_eq!(show_f32(1.0e-7), "1.0e-7");
    // The same value as a Float needs all its digits.
    assert_eq!(show(f64::from(0.1_f32)), "0.10000000149011612");
}

#[test]
fn float_parse_follows_d313() {
    assert_eq!(parse("42"), Some(42.0));
    assert_eq!(parse("1e3"), Some(1000.0));
    assert_eq!(parse("-1.5"), Some(-1.5));
    assert_eq!(parse("-1.5e-3"), Some(-0.0015));
    assert_eq!(parse("1E+3"), Some(1000.0));
    assert_eq!(parse("-inf"), Some(f64::NEG_INFINITY));
    assert_eq!(parse("1e400"), Some(f64::INFINITY));
    assert!(parse("NaN").is_some_and(f64::is_nan));
    let third = 1.0 / 3.0;
    assert_eq!(parse(&show(third)), Some(third));
    for text in [
        "+1", ".5", "1.", "infinity", " 1", "1_000.0", "nan", "", "-",
    ] {
        assert_eq!(parse(text), None, "{text:?}");
    }
}

#[test]
fn f32_parse_follows_d313() {
    assert_eq!(parse_f32("42"), Some(42.0));
    assert_eq!(parse_f32("1e3"), Some(1000.0));
    assert_eq!(parse_f32("-1.5"), Some(-1.5));
    assert_eq!(parse_f32("0.1"), Some(0.1));
    assert_eq!(parse_f32(&show_f32(f32::EPSILON)), Some(f32::EPSILON));
    for text in ["+1", ".5", "1.", "infinity", " 1"] {
        assert_eq!(parse_f32(text), None, "{text:?}");
    }
}

#[test]
fn rounding_to_int_follows_d132_and_d314() {
    let round = |kernel: extern "C" fn(f64) -> crate::abi::Word, x: f64| {
        no_leaks(|| take_result::<i64>(kernel(x)))
    };
    assert_eq!(round(float_kernels::truncate, 2.7), Some(2));
    assert_eq!(round(float_kernels::truncate, -2.7), Some(-2));
    assert_eq!(round(float_kernels::round, 2.4), Some(2));
    assert_eq!(round(float_kernels::round, 2.5), Some(3));
    assert_eq!(round(float_kernels::round, -2.5), Some(-3));
    assert_eq!(round(float_kernels::round, -0.4), Some(0));
    assert_eq!(
        round(float_kernels::round, 0.499_999_999_999_999_94),
        Some(0)
    );
    assert_eq!(round(float_kernels::floor, 2.7), Some(2));
    assert_eq!(round(float_kernels::floor, -2.5), Some(-3));
    assert_eq!(round(float_kernels::ceil, 2.1), Some(3));
    assert_eq!(round(float_kernels::ceil, -2.5), Some(-2));
    for kernel in [
        float_kernels::truncate,
        float_kernels::round,
        float_kernels::floor,
        float_kernels::ceil,
    ] {
        assert_eq!(round(kernel, f64::NAN), None);
        assert_eq!(round(kernel, f64::INFINITY), None);
        assert_eq!(round(kernel, f64::NEG_INFINITY), None);
        assert_eq!(round(kernel, 1.0e19), None);
        assert_eq!(round(kernel, -1.0e19), None);
        // 2^63 is out of range, -2^63 is `int::min_value`.
        assert_eq!(round(kernel, 9_223_372_036_854_775_808.0), None);
        assert_eq!(round(kernel, -9_223_372_036_854_775_808.0), Some(i64::MIN));
        // The largest Float below 2^63.
        assert_eq!(
            round(kernel, 9_223_372_036_854_774_784.0),
            Some(9_223_372_036_854_774_784)
        );
    }
    // floor of a value just above -2^63 - 1 stays in range; ceil of the
    // value just below 2^63 is that value.
    assert_eq!(
        round(float_kernels::floor, -9_223_372_036_854_775_807.5),
        Some(i64::MIN)
    );
}

#[test]
fn f32_rounding_to_int() {
    let round = |kernel: extern "C" fn(f32) -> crate::abi::Word, x: f32| {
        no_leaks(|| take_result::<i64>(kernel(x)))
    };
    assert_eq!(round(f32_kernels::truncate, -2.7), Some(-2));
    assert_eq!(round(f32_kernels::round, 2.5), Some(3));
    assert_eq!(round(f32_kernels::round, -2.5), Some(-3));
    assert_eq!(round(f32_kernels::floor, -2.5), Some(-3));
    assert_eq!(round(f32_kernels::ceil, 2.1), Some(3));
    assert_eq!(round(f32_kernels::truncate, f32::NAN), None);
    assert_eq!(round(f32_kernels::truncate, f32::INFINITY), None);
    assert_eq!(round(f32_kernels::truncate, f32::MAX), None);
    assert_eq!(round(f32_kernels::truncate, -9.223_372e18), Some(i64::MIN));
}

#[test]
fn conversions_follow_d359() {
    assert_eq!(float_kernels::from_int(-3), -3.0);
    assert_eq!(
        float_kernels::from_int(9_007_199_254_740_993),
        9_007_199_254_740_992.0
    );
    assert_eq!(
        float_kernels::from_i64(9_007_199_254_740_993),
        9_007_199_254_740_992.0
    );
    assert_eq!(
        float_kernels::from_u64(u64::MAX),
        18_446_744_073_709_551_616.0
    );
    assert_eq!(float_kernels::from_i8(-128), -128.0);
    assert_eq!(float_kernels::from_u32(u32::MAX), 4_294_967_295.0);
    assert_eq!(float_kernels::from_f32(1.5), 1.5);
    assert_eq!(float_kernels::from_f32(f32::INFINITY), f64::INFINITY);
    assert!(float_kernels::from_f32(f32::NAN).is_nan());
    assert_eq!(
        float_kernels::from_f32(-0.0).to_bits(),
        (-0.0_f64).to_bits()
    );
    assert_eq!(float_kernels::from_f32(0.1), 0.100_000_001_490_116_12);
    assert_eq!(f32_kernels::from_u8(255), 255.0);
    assert_eq!(f32_kernels::from_int(16_777_217), 16_777_216.0);
    assert_eq!(f32_kernels::from_float(1.0e300), f32::INFINITY);
    assert_eq!(f32_kernels::from_float(-1.0e300), f32::NEG_INFINITY);
    assert_eq!(f32_kernels::from_float(1.0e-300).to_bits(), 0);
    // A Float between two F32 values rounds to the nearer one, ties to even.
    assert_eq!(
        f32_kernels::from_float(1.0 + f64::from(f32::EPSILON) / 2.0),
        1.0
    );
    assert_eq!(
        f32_kernels::from_float(1.0 + f64::from(f32::EPSILON) * 1.5),
        1.0 + 2.0 * f32::EPSILON
    );
}

#[test]
fn sqrt_is_correctly_rounded() {
    assert_eq!(float_kernels::sqrt(16.0), 4.0);
    assert_eq!(float_kernels::sqrt(2.0), std::f64::consts::SQRT_2);
    assert!(float_kernels::sqrt(-1.0).is_nan());
    assert_eq!(float_kernels::sqrt(f64::INFINITY), f64::INFINITY);
    assert_eq!(float_kernels::sqrt(-0.0).to_bits(), (-0.0_f64).to_bits());
    // The square root of 2^-1074 is exactly 2^-537.
    assert_eq!(
        float_kernels::sqrt(5e-324),
        f64::from_bits((1023 - 537) << 52)
    );
}

proptest! {
    #[test]
    fn float_text_round_trips_through_the_kernels(bits: u64) {
        let x = f64::from_bits(bits);
        let back = parse(&show(x)).unwrap();
        prop_assert!(back.to_bits() == x.to_bits() || (x.is_nan() && back.is_nan()));
    }

    #[test]
    fn f32_text_round_trips_through_the_kernels(bits: u32) {
        let x = f32::from_bits(bits);
        let back = parse_f32(&show_f32(x)).unwrap();
        prop_assert!(back.to_bits() == x.to_bits() || (x.is_nan() && back.is_nan()));
    }

    #[test]
    fn rounding_matches_rust(bits: u64) {
        let x = f64::from_bits(bits);
        let fits = |r: f64| (r.is_finite() && (-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&r)).then_some(r as i64);
        prop_assert_eq!(no_leaks(|| take_result::<i64>(float_kernels::truncate(x))), fits(x.trunc()));
        prop_assert_eq!(no_leaks(|| take_result::<i64>(float_kernels::round(x))), fits(x.round()));
        prop_assert_eq!(no_leaks(|| take_result::<i64>(float_kernels::floor(x))), fits(x.floor()));
        prop_assert_eq!(no_leaks(|| take_result::<i64>(float_kernels::ceil(x))), fits(x.ceil()));
    }

    #[test]
    fn small_conversions_are_exact_or_rounded_once(a: i8, b: i16, c: i32, d: u8, e: u16, f: u32) {
        prop_assert_eq!(float_kernels::from_i8(a).to_bits(), f64::from(a).to_bits());
        prop_assert_eq!(float_kernels::from_i16(b).to_bits(), f64::from(b).to_bits());
        prop_assert_eq!(float_kernels::from_i32(c).to_bits(), f64::from(c).to_bits());
        prop_assert_eq!(float_kernels::from_u8(d).to_bits(), f64::from(d).to_bits());
        prop_assert_eq!(float_kernels::from_u16(e).to_bits(), f64::from(e).to_bits());
        prop_assert_eq!(float_kernels::from_u32(f).to_bits(), f64::from(f).to_bits());
        prop_assert_eq!(f32_kernels::from_i8(a).to_bits(), f32::from(a).to_bits());
        prop_assert_eq!(f32_kernels::from_i16(b).to_bits(), f32::from(b).to_bits());
        prop_assert_eq!(f32_kernels::from_u8(d).to_bits(), f32::from(d).to_bits());
        prop_assert_eq!(f32_kernels::from_u16(e).to_bits(), f32::from(e).to_bits());
        // I32 and U32 to F32 round once, to nearest, ties to even: the same
        // as rounding the exact Float, since every I32 and U32 is exact
        // there.
        prop_assert_eq!(f32_kernels::from_i32(c).to_bits(), (f64::from(c) as f32).to_bits());
        prop_assert_eq!(f32_kernels::from_u32(f).to_bits(), (f64::from(f) as f32).to_bits());
    }

    #[test]
    fn float_from_f32_is_exact(bits: u32) {
        let x = f32::from_bits(bits);
        let wide = float_kernels::from_f32(x);
        prop_assert!(wide.to_bits() == f64::from(x).to_bits() || (x.is_nan() && wide.is_nan()));
    }

    #[test]
    fn conversions_match_rust_casts(n: i64, u: u64, x: f64) {
        prop_assert_eq!(float_kernels::from_i64(n).to_bits(), (n as f64).to_bits());
        prop_assert_eq!(f32_kernels::from_i64(n).to_bits(), (n as f32).to_bits());
        prop_assert_eq!(float_kernels::from_int(n).to_bits(), (n as f64).to_bits());
        prop_assert_eq!(float_kernels::from_u64(u).to_bits(), (u as f64).to_bits());
        prop_assert_eq!(f32_kernels::from_int(n).to_bits(), (n as f32).to_bits());
        prop_assert_eq!(f32_kernels::from_u64(u).to_bits(), (u as f32).to_bits());
        let narrowed = f32_kernels::from_float(x);
        prop_assert!(narrowed.to_bits() == (x as f32).to_bits() || (x.is_nan() && narrowed.is_nan()));
    }

    /// Rust's int-to-float casts round to nearest, ties to even (D-359):
    /// checked here against exact integer arithmetic.
    #[test]
    fn int_to_float_rounds_to_nearest_even(n: i64) {
        let x = float_kernels::from_int(n);
        let exact = i128::from(n);
        let got = x as i128;
        let below = f64::from_bits(x.to_bits().wrapping_sub(1));
        let above = f64::from_bits(x.to_bits().wrapping_add(1));
        let error = (got - exact).abs();
        for neighbor in [below, above] {
            if neighbor.is_finite() && neighbor.abs() < 1.0e19 {
                let other = (neighbor as i128 - exact).abs();
                prop_assert!(error < other || (error == other && x.to_bits() & 1 == 0));
            }
        }
    }
}
