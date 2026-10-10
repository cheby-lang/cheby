//! Reference values for D-396: the seven functions computed with
//! `astro-float` at 256 bits or more and rounded to binary64, to nearest,
//! ties to even.
//!
//! Each value is computed at two precisions, p and p + 128, which must
//! round to the same binary64 value, and the value at p must lie farther
//! than 2^-(p-24) relative from a midpoint between two binary64 values
//! (the rounding-boundary check). Otherwise the precision is raised, up to
//! [`PRECISIONS`]'s last entry, and then the input is dropped with a note.
//! `pow` also recognizes results that are exactly a midpoint (such as
//! `pow(x, 1.5)` when x^1.5 has 54 significant bits) by exact arithmetic.
//!
//! The special values follow C99 Annex F and `docs/stdlib/float.md`
//! (`pow(x, ±0)` and `pow(1, y)` are 1 for every x and y, NaN included).
//! Every reduction is done here at explicit precision, never trusted to the
//! library: sin, cos and tan reduce by π/2 with π at p + e + 128 bits for
//! an argument of exponent e.

#![expect(
    clippy::arithmetic_side_effects,
    clippy::many_single_char_names,
    clippy::float_cmp,
    reason = "test-only big-float arithmetic written as in the literature, and exact float tests"
)]

use astro_float::{BigFloat, Consts, RoundingMode};

const RM: RoundingMode = RoundingMode::ToEven;

/// The working precisions tried in turn.
const PRECISIONS: [usize; 5] = [256, 512, 1024, 4096, 8192];

/// The bits after the rounding position that must not all be the
/// "midpoint" pattern for a value at precision p: the last `GUARD` bits of
/// p are never trusted.
const GUARD: usize = 24;

/// A reference result.
#[derive(Debug, Clone, Copy)]
pub(super) enum Ref {
    /// The correctly rounded binary64 value.
    Value(f64),
    /// No precision decided the rounding; the input is left out.
    Dropped,
}

/// A binary64 rounding of a big value.
#[derive(Debug, Clone, Copy)]
enum Rounded {
    Value(f64),
    /// Too close to the midpoint `(2q + 1) * 2^(ulp_exp - 1)` to decide.
    Close {
        q: u64,
        ulp_exp: i64,
    },
}

/// Bit `i` of a mantissa, counted from its most significant bit.
fn bit(words: &[u64], i: usize) -> bool {
    let len = words.len() * 64;
    if i >= len {
        return false;
    }
    let position = len - 1 - i;
    (words[position / 64] >> (position % 64)) & 1 == 1
}

/// `±q * 2^ulp_exp` as a binary64, where q has at most 53 bits, or 2^53
/// after a carry.
fn compose(mut q: u64, mut ulp_exp: i64, negative: bool) -> f64 {
    let signed = |x: f64| if negative { -x } else { x };
    if q == 1 << 53 {
        q = 1 << 52;
        ulp_exp += 1;
    }
    if q == 0 {
        return signed(0.0);
    }
    let bits = if q < 1 << 52 {
        assert_eq!(ulp_exp, -1074, "only subnormals have fewer than 53 bits");
        q
    } else {
        let biased = ulp_exp + 52 + 1023;
        if biased > 2046 {
            return signed(f64::INFINITY);
        }
        (u64::try_from(biased).unwrap() << 52) | (q - (1 << 52))
    };
    signed(f64::from_bits(bits))
}

/// Rounds `v`, computed at precision `p`, to binary64. Unless `exact`, a
/// value whose bits after the rounding position look like a midpoint up
/// to bit p - `GUARD` is `Close`.
fn round_to_f64(v: &BigFloat, p: usize, exact: bool) -> Rounded {
    if v.is_nan() {
        return Rounded::Value(f64::NAN);
    }
    let negative = v.is_negative();
    let signed = |x: f64| if negative { -x } else { x };
    if v.is_inf() {
        return Rounded::Value(signed(f64::INFINITY));
    }
    if v.is_zero() {
        return Rounded::Value(signed(0.0));
    }
    let (words, _, _, e, _) = v.as_raw_parts().unwrap();
    assert!(bit(words, 0), "astro-float keeps its mantissas normalized");
    // v = 0.b0 b1 b2 ... * 2^e with b0 = 1: bit i weighs 2^(e - 1 - i).
    let e = i64::from(e);
    if e - 1 > 1023 {
        return Rounded::Value(signed(f64::INFINITY));
    }
    let ulp_exp = (e - 1 - 52).max(-1074);
    let Ok(kept) = usize::try_from(e - ulp_exp) else {
        // v < 2^(ulp_exp - 2): below a quarter of the smallest subnormal.
        return Rounded::Value(signed(0.0));
    };
    let mut q: u64 = 0;
    for i in 0..kept {
        q = (q << 1) | u64::from(bit(words, i));
    }
    let round_bit = bit(words, kept);
    let len = words.len() * 64;
    if !exact {
        let decided = (kept + 1..len)
            .find(|&j| bit(words, j) == round_bit)
            .is_some_and(|j| j < p.saturating_sub(GUARD));
        if !decided {
            return Rounded::Close { q, ulp_exp };
        }
    }
    let sticky = (kept + 1..len).any(|j| bit(words, j));
    if round_bit && (sticky || q & 1 == 1) {
        q += 1;
    }
    Rounded::Value(compose(q, ulp_exp, negative))
}

/// The binary64 rounding of a value known to be exact.
fn exact(v: &BigFloat, p: usize) -> f64 {
    match round_to_f64(v, p, true) {
        Rounded::Value(value) => value,
        Rounded::Close { .. } => panic!("an exact rounding is always a value"),
    }
}

/// `m * 2^e` exactly at precision `p`.
fn scaled(m: u64, e: i64, p: usize) -> BigFloat {
    if m == 0 {
        return BigFloat::from_u64(0, p);
    }
    let mut b = BigFloat::from_u64(m, p);
    let e0 = i64::from(b.exponent().unwrap());
    b.set_exponent(i32::try_from(e0 + e).unwrap());
    b
}

/// A finite binary64 exactly at precision `p`. (`BigFloat::from_f64`
/// halves subnormals in astro-float 0.9, so the value is built from its
/// integer significand.)
pub(super) fn big(x: f64, p: usize) -> BigFloat {
    assert!(x.is_finite());
    let bits = x.to_bits();
    let field = i64::try_from((bits >> 52) & 0x7ff).unwrap();
    let fraction = bits & ((1 << 52) - 1);
    let b = if field == 0 {
        scaled(fraction, -1074, p)
    } else {
        scaled(fraction | (1 << 52), field - 1075, p)
    };
    if x.is_sign_negative() { b.neg() } else { b }
}

/// Evaluates `eval(p)` at increasing precisions until the rounding is
/// decided at p and agrees at p + 128. `eval` returns the value and whether
/// it is exact.
fn ziv(mut eval: impl FnMut(usize) -> (BigFloat, bool)) -> Result<f64, Rounded> {
    let mut last = Rounded::Value(f64::NAN);
    for p in PRECISIONS {
        let (first, exact) = eval(p);
        let rounded = round_to_f64(&first, p, exact);
        let Rounded::Value(value) = rounded else {
            last = rounded;
            continue;
        };
        if exact {
            return Ok(value);
        }
        let (second, _) = eval(p + 128);
        if let Rounded::Value(check) = round_to_f64(&second, p + 128, false)
            && check.to_bits() == value.to_bits()
        {
            return Ok(value);
        }
    }
    Err(last)
}

fn decided(result: Result<f64, Rounded>) -> Ref {
    result.map_or(Ref::Dropped, Ref::Value)
}

/// 2^-200 and 2^-30, the thresholds of the shortcuts below.
const TWO_POW_M200: f64 = 6.223_015_277_861_142e-61;
const TWO_POW_M30: f64 = 9.313_225_746_154_785e-10;

/// e^x.
pub(super) fn exp(x: f64, cc: &mut Consts) -> Ref {
    if x.is_nan() {
        return Ref::Value(f64::NAN);
    }
    // e^710 overflows and e^-746 is below a quarter of the smallest
    // subnormal.
    if x > 710.0 {
        return Ref::Value(f64::INFINITY);
    }
    if x < -746.0 {
        return Ref::Value(0.0);
    }
    if x.abs() < TWO_POW_M200 {
        // e^x is within 2^-199 of 1, far from the midpoints 1 - 2^-54 and
        // 1 + 2^-53 around it.
        return Ref::Value(1.0);
    }
    decided(ziv(|p| (big(x, p).exp(p, RM, cc), false)))
}

/// The natural logarithm.
pub(super) fn ln(x: f64, cc: &mut Consts) -> Ref {
    if x.is_nan() || x < 0.0 {
        return Ref::Value(f64::NAN);
    }
    if x == 0.0 {
        return Ref::Value(f64::NEG_INFINITY);
    }
    if x.is_infinite() {
        return Ref::Value(f64::INFINITY);
    }
    if x.to_bits() == 1.0_f64.to_bits() {
        return Ref::Value(0.0);
    }
    decided(ziv(|p| (big(x, p).ln(p, RM, cc), false)))
}

/// sin, cos or tan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Trig {
    Sin,
    Cos,
    Tan,
}

/// floor(log2 |x|) of a finite non-zero x.
fn exponent_of(x: f64) -> i64 {
    let bits = x.to_bits() & 0x7fff_ffff_ffff_ffff;
    let field = i64::try_from(bits >> 52).unwrap();
    if field == 0 {
        63 - i64::from(bits.leading_zeros()) - 1074
    } else {
        field - 1023
    }
}

/// `(r, k mod 4)` with `r = x - k * π/2` for the integer k nearest to
/// `x / (π/2)`, at precision p + e + 128 for an x of exponent e, so r keeps
/// p + 128 - 62 bits or more of relative precision (a binary64 is never
/// closer than 2^-62 to a multiple of π/2, Muller's worst case).
fn reduce(x: f64, p: usize, cc: &mut Consts) -> (BigFloat, u32) {
    let extra = usize::try_from(exponent_of(x).max(0)).unwrap();
    let wide = p + extra + 128;
    let bx = big(x, wide);
    let pi = cc.pi(wide, RM);
    let mut half_pi = pi.clone();
    half_pi.set_exponent(pi.exponent().unwrap() - 1);
    let quotient = bx.div(&half_pi, wide, RM);
    let half = big(0.5, 64);
    let k = if quotient.is_negative() {
        quotient.sub(&half, wide, RM).ceil()
    } else {
        quotient.add(&half, wide, RM).floor()
    };
    let r = bx.sub(&k.mul(&half_pi, wide, RM), wide, RM);
    let four = big(4.0, 64);
    let quarter = k.div(&four, wide, RM).floor();
    let k_mod_4 = k.sub(&quarter.mul(&four, wide, RM), wide, RM);
    let k_mod_4 = exact(&k_mod_4, wide);
    let k_mod_4 = [0.0, 1.0, 2.0, 3.0]
        .iter()
        .position(|&m| m == k_mod_4)
        .unwrap();
    (r, u32::try_from(k_mod_4).unwrap())
}

/// sin x, cos x or tan x.
pub(super) fn trig(f: Trig, x: f64, cc: &mut Consts) -> Ref {
    if !x.is_finite() {
        return Ref::Value(f64::NAN);
    }
    if x.abs() < TWO_POW_M30 {
        // sin x and tan x differ from x by less than |x|^3 / 2 < 2^-61 |x|,
        // and cos x from 1 by less than 2^-61, far below half an ulp of x
        // (2^-54 |x| or more) and of 1 (2^-54 below, 2^-53 above); x and 1
        // are representable, so they are the rounded values. Zeros keep
        // their sign.
        return Ref::Value(if f == Trig::Cos { 1.0 } else { x });
    }
    decided(ziv(|p| {
        let (r, k) = reduce(x, p, cc);
        let s = r.sin(p, RM, cc);
        let c = r.cos(p, RM, cc);
        let value = match (f, k) {
            (Trig::Sin, 0) | (Trig::Cos, 3) => s,
            (Trig::Sin, 1) | (Trig::Cos, 0) => c,
            (Trig::Sin, 2) | (Trig::Cos, 1) => s.neg(),
            (Trig::Sin | Trig::Cos, _) => c.neg(),
            (Trig::Tan, 0 | 2) => s.div(&c, p, RM),
            (Trig::Tan, _) => c.div(&s, p, RM).neg(),
        };
        (value, false)
    }))
}

/// The binary64 nearest to `v`, which must be decidable at 256 bits.
fn constant(v: &BigFloat) -> f64 {
    match round_to_f64(v, 256, false) {
        Rounded::Value(value) => value,
        Rounded::Close { .. } => panic!("a constant too close to a midpoint"),
    }
}

/// The angle of the point (x, y).
pub(super) fn atan2(y: f64, x: f64, cc: &mut Consts) -> Ref {
    if x.is_nan() || y.is_nan() {
        return Ref::Value(f64::NAN);
    }
    let pi = cc.pi(256, RM);
    let quarter = |n: f64| constant(&pi.mul(&big(n / 4.0, 64), 256, RM));
    let with_sign_of_y = |v: f64| if y.is_sign_negative() { -v } else { v };
    // C99 F.9.1.4.
    if y == 0.0 {
        let toward_positive = x > 0.0 || (x == 0.0 && x.is_sign_positive());
        return Ref::Value(with_sign_of_y(if toward_positive {
            0.0
        } else {
            quarter(4.0)
        }));
    }
    if x == 0.0 {
        return Ref::Value(with_sign_of_y(quarter(2.0)));
    }
    if y.is_infinite() {
        let angle = match x {
            f64::INFINITY => quarter(1.0),
            f64::NEG_INFINITY => quarter(3.0),
            _ => quarter(2.0),
        };
        return Ref::Value(with_sign_of_y(angle));
    }
    if x.is_infinite() {
        return Ref::Value(with_sign_of_y(if x > 0.0 { 0.0 } else { quarter(4.0) }));
    }
    decided(ziv(|p| {
        let wide = p + 64;
        let ratio = big(y.abs(), wide).div(&big(x.abs(), wide), wide, RM);
        let angle = ratio.atan(wide, RM, cc);
        let angle = if x > 0.0 {
            angle
        } else {
            cc.pi(wide, RM).sub(&angle, wide, RM)
        };
        (if y < 0.0 { angle.neg() } else { angle }, false)
    }))
}

/// A whole float from 0 to 64 as a count.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the callers pass whole values from 0 to 64"
)]
fn small_count(v: f64) -> usize {
    assert!(is_integer(v) && (0.0..=64.0).contains(&v));
    v as usize
}

fn is_integer(y: f64) -> bool {
    y.is_finite() && y.trunc() == y
}

/// Whether `y` is an odd integer (every binary64 above 2^53 is even).
fn is_odd_integer(y: f64) -> bool {
    is_integer(y) && y.abs() < 9_007_199_254_740_992.0 && (y % 2.0).abs() == 1.0
}

/// x to the power y.
pub(super) fn pow(x: f64, y: f64, cc: &mut Consts) -> Ref {
    // `docs/stdlib/float.md`: 1 for y = ±0 and for x = 1, NaN included.
    if y == 0.0 || x.to_bits() == 1.0_f64.to_bits() {
        return Ref::Value(1.0);
    }
    if x.is_nan() || y.is_nan() {
        return Ref::Value(f64::NAN);
    }
    let odd = is_odd_integer(y);
    // C99 F.9.4.4.
    if x == 0.0 {
        return Ref::Value(match (y < 0.0, odd) {
            (true, true) => 1.0 / x,
            (true, false) => f64::INFINITY,
            (false, true) => x,
            (false, false) => 0.0,
        });
    }
    if y.is_infinite() {
        let magnitude = x.abs();
        return Ref::Value(if magnitude.to_bits() == 1.0_f64.to_bits() {
            1.0
        } else if (magnitude < 1.0) == (y > 0.0) {
            0.0
        } else {
            f64::INFINITY
        });
    }
    if x.is_infinite() {
        let magnitude = if y > 0.0 { f64::INFINITY } else { 0.0 };
        return Ref::Value(if x < 0.0 && odd {
            -magnitude
        } else {
            magnitude
        });
    }
    if x < 0.0 && !is_integer(y) {
        return Ref::Value(f64::NAN);
    }
    let negative = x < 0.0 && odd;
    let signed = |v: f64| if negative { -v } else { v };
    let ax = x.abs();
    // Far beyond the range: |y log2 |x|| > 1100 overflows or underflows
    // whatever the rounding (log2 is only an estimate here, good to far
    // better than the margin).
    let log2 = y * ax.log2();
    if log2 > 1100.0 {
        return Ref::Value(signed(f64::INFINITY));
    }
    if log2 < -1200.0 {
        return Ref::Value(signed(0.0));
    }
    let sign = |v: BigFloat| if negative { v.neg() } else { v };
    if is_integer(y) && y.abs() <= 64.0 {
        // |x|^n exactly: at most 64 * 53 bits.
        let n = small_count(y.abs());
        let exact_p = 64 * 64 + 128;
        let power = big(ax, exact_p).powi(n, exact_p, RM);
        if y > 0.0 {
            return Ref::Value(signed(exact(&power, exact_p)));
        }
        return decided(ziv(|p| (sign(power.reciprocal(p, RM)), false)));
    }
    match ziv(|p| {
        let wide = p + 128;
        let t = big(ax, wide).ln(wide, RM, cc).mul(&big(y, wide), wide, RM);
        (sign(t.exp(p, RM, cc)), false)
    }) {
        Ok(value) => Ref::Value(value),
        Err(Rounded::Close { q, ulp_exp }) => {
            exact_midpoint(ax, y, q, ulp_exp).map_or(Ref::Dropped, |v| Ref::Value(signed(v)))
        }
        Err(Rounded::Value(_)) => Ref::Dropped,
    }
}

/// When |x|^y is exactly the midpoint `(2q + 1) * 2^(ulp_exp - 1)`, its
/// rounding (ties to even). Checked exactly for y = n / 2^k with k <= 6 and
/// |n| <= 64: m^(2^k) = |x|^n, or m^(2^k) |x|^|n| = 1 for a negative n.
fn exact_midpoint(ax: f64, y: f64, q: u64, ulp_exp: i64) -> Option<f64> {
    let (n, k) = (1..=6_u32).find_map(|k| {
        let scaled_y = y * f64::from(1_u32 << k);
        (is_integer(scaled_y) && scaled_y.abs() <= 64.0).then_some((scaled_y, k))
    })?;
    let p = 64 * 64 * 4 + 512;
    let midpoint = scaled(2 * q + 1, ulp_exp - 1, p);
    let lhs = midpoint.powi(1 << k, p, RM);
    let magnitude = small_count(n.abs());
    let power = big(ax, p).powi(magnitude, p, RM);
    let is_midpoint = if n > 0.0 {
        lhs.cmp(&power) == Some(0)
    } else {
        lhs.mul(&power, p, RM).cmp(&big(1.0, 64)) == Some(0)
    };
    is_midpoint.then(|| compose(if q & 1 == 0 { q } else { q + 1 }, ulp_exp, false))
}

/// Reads a C99 hexadecimal float such as `-0x1.62e42fefa39efp+9`,
/// `0x8.f333a2ffb20ep-332`, `+0x.fffffffffffffp-1022`, `+0x1p-1074`, a
/// decimal such as `+0` or `1`, `inf`, `nan` or `snan` (as a NaN). `None` for other text or a
/// value that is not exactly a binary64.
pub(super) fn hex_float(text: &str) -> Option<f64> {
    let text = text.trim();
    let (negative, body) = match text.as_bytes().first() {
        Some(b'-') => (true, text.get(1..)?),
        Some(b'+') => (false, text.get(1..)?),
        _ => (false, text),
    };
    let magnitude = match body {
        "inf" => f64::INFINITY,
        "nan" | "snan" => f64::NAN,
        _ if !body.starts_with("0x") => body.parse().ok()?,
        _ => {
            let digits = body.strip_prefix("0x")?;
            let (mantissa, exponent) = digits.split_once('p')?;
            let mut exponent: i64 = exponent.parse().ok()?;
            let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
            let mut m: u128 = 0;
            for c in whole.chars() {
                m = m * 16 + u128::from(c.to_digit(16)?);
            }
            for c in fraction.chars() {
                m = m * 16 + u128::from(c.to_digit(16)?);
                exponent -= 4;
            }
            exact_binary64(m, exponent)?
        }
    };
    Some(if negative { -magnitude } else { magnitude })
}

/// `m * 2^e` rounded to binary64, to nearest, ties to even, as C's
/// `strtod` reads hexadecimal text (CORE-MATH's lists hold a few inputs
/// with more bits than a subnormal keeps). `None` beyond the finite range.
fn exact_binary64(m: u128, e: i64) -> Option<f64> {
    if m == 0 {
        return Some(0.0);
    }
    let width = i64::from(128 - m.leading_zeros());
    // The leading bit weighs 2^(e + width - 1); the last kept bit is 52
    // places below it, or 2^-1074 for a subnormal.
    let ulp_exp = (e + width - 53).max(-1074);
    let shift = e - ulp_exp;
    let q = if shift >= 0 {
        m.checked_shl(u32::try_from(shift).ok()?)?
    } else {
        let dropped = u32::try_from(-shift).ok()?;
        if dropped >= 128 {
            0
        } else {
            let kept = m >> dropped;
            let rest = m & ((1_u128 << dropped) - 1);
            let half = 1_u128 << (dropped - 1);
            if rest > half || (rest == half && kept & 1 == 1) {
                kept + 1
            } else {
                kept
            }
        }
    };
    let q = u64::try_from(q).ok()?;
    let value = compose(q, ulp_exp, false);
    value.is_finite().then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cc() -> Consts {
        Consts::new().unwrap()
    }

    fn value(r: Ref) -> f64 {
        match r {
            Ref::Value(value) => value,
            Ref::Dropped => panic!("dropped"),
        }
    }

    #[test]
    fn binary64_values_round_trip_and_midpoints_round_to_even() {
        let mut state = 0x5eed_u64;
        let mut values = vec![1.0, 5e-324, f64::MIN_POSITIVE, f64::MAX, 3.0, 0.1];
        for _ in 0..20_000 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            values.push(f64::from_bits(state));
        }
        for x in values.into_iter().filter(|x| x.is_finite() && *x != 0.0) {
            let b = big(x, 256);
            let Rounded::Value(back) = round_to_f64(&b, 256, false) else {
                panic!("{x:e} is representable");
            };
            assert_eq!(back.to_bits(), x.to_bits());
            let next = x.abs().next_up();
            if next.is_finite() {
                let a = x.abs();
                let mid = big(a, 256)
                    .add(&big(next, 256), 256, RM)
                    .div(&big(2.0, 64), 256, RM);
                let even = if a.to_bits() & 1 == 0 { a } else { next };
                let Rounded::Value(tie) = round_to_f64(&mid, 256, true) else {
                    panic!("exact");
                };
                assert_eq!(tie.to_bits(), even.to_bits(), "tie above {a:e}");
                assert!(matches!(
                    round_to_f64(&mid, 256, false),
                    Rounded::Close { .. }
                ));
            }
        }
    }

    #[test]
    fn known_values() {
        let mut cc = cc();
        assert_eq!(value(exp(1.0, &mut cc)), std::f64::consts::E);
        assert_eq!(value(ln(2.0, &mut cc)), std::f64::consts::LN_2);
        assert_eq!(value(ln(10.0, &mut cc)), std::f64::consts::LN_10);
        assert_eq!(value(atan2(1.0, 1.0, &mut cc)), std::f64::consts::FRAC_PI_4);
        assert_eq!(value(atan2(0.0, -1.0, &mut cc)), std::f64::consts::PI);
        assert_eq!(
            value(atan2(-1.0, 0.0, &mut cc)),
            -std::f64::consts::FRAC_PI_2
        );
        assert_eq!(
            value(trig(Trig::Sin, std::f64::consts::FRAC_PI_2, &mut cc)),
            1.0
        );
        assert_eq!(value(trig(Trig::Cos, std::f64::consts::PI, &mut cc)), -1.0);
        // sin(1e22), the classic test of argument reduction.
        assert_eq!(
            value(trig(Trig::Sin, 1.0e22, &mut cc)),
            -0.852_200_849_767_188_8
        );
        assert_eq!(value(pow(2.0, 10.0, &mut cc)), 1024.0);
        assert_eq!(value(pow(2.0, -1.0, &mut cc)), 0.5);
        assert_eq!(value(pow(f64::NAN, 0.0, &mut cc)), 1.0);
        assert_eq!(value(pow(1.0, f64::NAN, &mut cc)), 1.0);
        assert!(value(pow(-8.0, 1.0 / 3.0, &mut cc)).is_nan());
        assert_eq!(value(exp(-745.133_219_101_941_1, &mut cc)), 5e-324);
        assert_eq!(
            value(exp(709.782_712_893_384, &mut cc)),
            1.797_693_134_862_273_2e308
        );
        assert_eq!(value(exp(709.8, &mut cc)), f64::INFINITY);
    }

    #[test]
    fn exact_midpoints_of_pow_round_to_even() {
        let mut cc = cc();
        // t = 2^18 - 1 is odd and t^3 lies in [2^53, 2^54), so t^3 has 54
        // significant bits: a midpoint. pow(t^2, 1.5) is exactly t^3.
        let t = f64::from((1_u32 << 18) - 1);
        let t3 = big(t, 256).powi(3, 256, RM);
        let Rounded::Close { .. } = round_to_f64(&t3, 256, false) else {
            panic!("t^3 is a midpoint");
        };
        let Rounded::Value(expected) = round_to_f64(&t3, 256, true) else {
            panic!("exact");
        };
        assert_eq!(
            value(pow(t * t, 1.5, &mut cc)).to_bits(),
            expected.to_bits()
        );
    }

    #[test]
    fn hex_floats_read_exactly() {
        assert_eq!(hex_float("0x1.62e42fefa39efp+9"), Some(709.782_712_893_384));
        assert_eq!(hex_float("-0x1p-1074"), Some(-5e-324));
        assert_eq!(
            hex_float("+0x.fffffffffffffp-1022"),
            Some(f64::from_bits(0x000f_ffff_ffff_ffff))
        );
        assert_eq!(
            hex_float("0x8.f333a2ffb20ep-332"),
            Some(1.022_974_476_853_389_6e-99)
        );
        assert_eq!(
            hex_float("0x8.f333a2ffb20ep-332"),
            hex_float("0x1.1e66745ff641cp-329")
        );
        assert_eq!(hex_float("+0x1.fffffffffffffp+1023"), Some(f64::MAX));
        assert_eq!(
            hex_float("-0").map(f64::to_bits),
            Some((-0.0_f64).to_bits())
        );
        assert!(hex_float("snan").is_some_and(f64::is_nan));
        assert_eq!(hex_float("0x1p+1024"), None);
        // Rounded like strtod: ties to even.
        assert_eq!(hex_float("0x1.00000000000008p+0"), Some(1.0));
        assert_eq!(
            hex_float("0x1.00000000000018p+0"),
            Some(1.0 + 2.0 * f64::EPSILON)
        );
        assert_eq!(hex_float("0x1p-1075"), Some(0.0));
        assert_eq!(hex_float("0x1.8p-1075"), Some(5e-324));
        // Python's float.fromhex gives 0x0.02562874b979fp-1022.
        assert_eq!(
            hex_float("0x1.2b143a5cbcf82p-1029"),
            Some(f64::from_bits(0x2562_874b_979f))
        );
    }
}
