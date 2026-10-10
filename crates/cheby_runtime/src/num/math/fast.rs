//! The fast phase of Ziv's strategy (D-568): double-double evaluations
//! with an error bound, and the rounding test.
//!
//! A double-double [`Dd`] is an unevaluated sum `hi + lo` of two binary64
//! values with `|lo| ≤ ulp(hi)`, about 106 bits. Products use Dekker's
//! exact splitting rather than a fused multiply-add, so the results are
//! the same on every machine and no C library function is called.
//!
//! Each function returns `None` (the accurate phase decides) unless its
//! double-double value `v` and an absolute error bound `err` with
//! `|v - f(x)| ≤ err` settle the rounding: [`round_checked`] rounds `v -
//! err` and `v + err` and accepts when both give the same binary64. The
//! bounds count every rounding of the evaluation with a margin:
//!
//! - the polynomial tails evaluated in binary64 contribute `2^-52` of
//!   their magnitude, a bound computed from the actual argument;
//! - each double-double operation contributes at most `2^-100` of the
//!   magnitudes involved (its error is below `2^-104` relative);
//! - the tables and constants are the double-doubles nearest to values
//!   computed with [`Big`] at 256 bits, so they are within `2^-106`
//!   relative.
//!
//! Results near the ends of the binary64 range (subnormal results,
//! overflow) and huge arguments of `sin`, `cos` and `tan` are left to the
//! accurate phase. A test compares the fast values with [`Big`] ones on
//! many inputs and checks that the bounds hold with room to spare.

use super::big::{Big, pow2};
use super::{Trig, tables};

/// A double-double: the unevaluated sum `hi + lo`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Dd {
    pub(super) hi: f64,
    pub(super) lo: f64,
}

/// `a + b` and its rounding error, exactly (Knuth).
fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let s = a + b;
    let bb = s - a;
    (s, (a - (s - bb)) + (b - bb))
}

/// `a + b` and its rounding error, exactly, when `|a| ≥ |b|` or `a = 0`
/// (Dekker).
fn fast_two_sum(a: f64, b: f64) -> (f64, f64) {
    let s = a + b;
    (s, b - (s - a))
}

/// 2^27 + 1, Veltkamp's splitting constant.
const SPLIT: f64 = 134_217_729.0;

/// `a` as two halves of 26 bits each, `a = hi + lo` exactly, for `|a| <
/// 2^996`.
fn split(a: f64) -> (f64, f64) {
    let t = SPLIT * a;
    let hi = t - (t - a);
    (hi, a - hi)
}

/// `a × b` and its rounding error, exactly (Dekker), when no step
/// overflows or underflows.
fn two_prod(a: f64, b: f64) -> (f64, f64) {
    let p = a * b;
    let (ah, al) = split(a);
    let (bh, bl) = split(b);
    (p, ((ah * bh - p) + ah * bl + al * bh) + al * bl)
}

impl Dd {
    const fn new(hi: f64, lo: f64) -> Self {
        Self { hi, lo }
    }

    /// A binary64.
    const fn from(hi: f64) -> Self {
        Self { hi, lo: 0.0 }
    }

    /// `-self`.
    fn neg(self) -> Self {
        Self::new(-self.hi, -self.lo)
    }

    /// `self + other`, accurate also under cancellation (relative error
    /// below 2^-104).
    fn add(self, other: Self) -> Self {
        let (s1, s2) = two_sum(self.hi, other.hi);
        let (t1, t2) = two_sum(self.lo, other.lo);
        let (s1, s2) = fast_two_sum(s1, s2 + t1);
        let (hi, lo) = fast_two_sum(s1, s2 + t2);
        Self::new(hi, lo)
    }

    /// `self + b`.
    fn add_f64(self, b: f64) -> Self {
        self.add(Self::from(b))
    }

    /// `self × other` (relative error below 2^-102).
    fn mul(self, other: Self) -> Self {
        let (p, e) = two_prod(self.hi, other.hi);
        let e = e + (self.hi * other.lo + self.lo * other.hi);
        let (hi, lo) = fast_two_sum(p, e);
        Self::new(hi, lo)
    }

    /// `self × b`.
    fn mul_f64(self, b: f64) -> Self {
        let (p, e) = two_prod(self.hi, b);
        let (hi, lo) = fast_two_sum(p, e + self.lo * b);
        Self::new(hi, lo)
    }

    /// `self / other` (relative error below 2^-100).
    fn div(self, other: Self) -> Self {
        let q1 = self.hi / other.hi;
        let r = self.add(other.mul_f64(q1).neg());
        let q2 = r.hi / other.hi;
        let r = r.add(other.mul_f64(q2).neg());
        let q3 = r.hi / other.hi;
        let (hi, lo) = fast_two_sum(q1, q2);
        Self::new(hi, lo).add_f64(q3)
    }

    /// `self × 2^k`, exact when the result stays normal.
    fn scale(self, k: i64) -> Self {
        let half = k.div_euclid(2);
        let (a, b) = (pow2(half), pow2(k.saturating_sub(half)));
        Self::new(self.hi * a * b, self.lo * a * b)
    }
}

/// 2^-n.
fn tiny(n: i64) -> f64 {
    pow2(n.saturating_neg())
}

/// The rounding test: the binary64 nearest to every value within `err` of
/// `v`, if they all have the same one. The two roundings of the test are
/// covered by widening `err` by their own error.
fn round_checked(v: Dd, err: f64) -> Option<f64> {
    if !(v.hi.is_finite() && v.lo.is_finite() && err.is_finite()) {
        return None;
    }
    let err = err + (v.lo.abs() + err) * tiny(50) + v.hi.abs() * tiny(100);
    let up = v.hi + (v.lo + err);
    let down = v.hi + (v.lo - err);
    (up.to_bits() == down.to_bits()).then_some(up)
}

/// The nearest integer to `x`, `|x| < 2^62`.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "the callers pass values far inside the range of i64"
)]
fn nearest_i64(x: f64) -> i64 {
    x.round() as i64
}

/// An index into a table, if `i` is one.
fn index(i: i64) -> Option<usize> {
    usize::try_from(i).ok()
}

// ---------------------------------------------------------------------
// The tables

/// The constants and tables of the fast phase, from `tables.rs`, which a
/// test generates with `Big` at 256 bits and checks bit for bit.
pub(super) struct Tables<'a> {
    /// ln 2.
    pub(super) ln2: Dd,
    /// π/2 and π.
    pub(super) half_pi: Dd,
    pub(super) pi: Dd,
    /// π/2 in three parts: 33 bits, 33 bits, the rest, so `k × P1` and
    /// `k × P2` are exact for `k < 2^20` (Cody and Waite).
    pub(super) half_pi_parts: [f64; 3],
    /// 2^(i/16) and 2^(j/256), i and j from 0 to 15.
    pub(super) exp2_coarse: &'a [Dd],
    pub(super) exp2_fine: &'a [Dd],
    /// For i from `LN_FIRST` to `LN_LAST`: `c = 1/(1 + i/256)` rounded,
    /// and `-ln c`.
    pub(super) ln_c: &'a [f64],
    pub(super) ln: &'a [Dd],
    /// sin(j/64) and cos(j/64), j from 0 to 51.
    pub(super) sin: &'a [Dd],
    pub(super) cos: &'a [Dd],
    /// atan(j/64), j from 0 to 64.
    pub(super) atan: &'a [Dd],
}

/// The tables of `tables.rs`.
static TABLES: Tables<'static> = Tables {
    ln2: tables::LN2,
    half_pi: tables::HALF_PI,
    pi: tables::PI,
    half_pi_parts: tables::HALF_PI_PARTS,
    exp2_coarse: &tables::EXP2_COARSE,
    exp2_fine: &tables::EXP2_FINE,
    ln_c: &tables::LN_C,
    ln: &tables::LN,
    sin: &tables::SIN,
    cos: &tables::COS,
    atan: &tables::ATAN,
};

/// The lowest and highest indexes of the `ln` table: `m` in `[√½, √2)`
/// gives `round((m - 1) × 256)` in `[-75, 106]`.
pub(super) const LN_FIRST: i64 = -75;
pub(super) const LN_LAST: i64 = 106;

impl Dd {
    /// A double-double from the bits of its halves, for `tables.rs`.
    pub(super) const fn bits(hi: u64, lo: u64) -> Self {
        Self::new(f64::from_bits(hi), f64::from_bits(lo))
    }
}

// ---------------------------------------------------------------------
// exp

/// 256 / ln 2, to pick the reduction's multiple.
const INV_LN2_256: f64 = 369.329_930_467_574_6;

/// `e^z` for a double-double `|z| ≤ 709` as `(value, relative error
/// bound)`, or `None` below `2^-960`, where the low half of the result
/// would not be normal.
#[expect(
    clippy::many_single_char_names,
    reason = "the notation of the numerical literature"
)]
fn exp_core(z: Dd) -> Option<(Dd, f64)> {
    let t = &TABLES;
    let k = nearest_i64(z.hi * INV_LN2_256);
    let kf = f64::from(i32::try_from(k).ok()?);
    // r = z - k ln2/256, |r| ≤ ln2/512 + |z.lo|: k × ln2.hi/256 is exact
    // with its error term, the subtraction from z.hi is exact with its
    // error term, and k × ln2.lo/256 is below 2^-44.
    let step = Dd::new(t.ln2.hi / 256.0, t.ln2.lo / 256.0);
    let (p, pe) = two_prod(kf, step.hi);
    let (s, se) = two_sum(z.hi, -p);
    let correction = (se - pe) + (z.lo - kf * step.lo);
    let (rh, rl) = two_sum(s, correction);
    let r = Dd::new(rh, rl);
    // e^r - 1 = r + r²/2 + r³/6 + … + r⁸/8!: r and r²/2 in double-double,
    // the rest in binary64 (|r³/6| < 2^-31, so its rounding is below
    // 2^-83; r⁹/9! < 2^-103 is left out).
    let r2 = r.mul(r);
    let tail = rh
        * r2.hi
        * (1.0 / 6.0
            + rh * (1.0 / 24.0
                + rh * (1.0 / 120.0 + rh * (1.0 / 720.0 + rh * (1.0 / 5040.0 + rh / 40320.0)))));
    let q = r.add(Dd::new(r2.hi / 2.0, r2.lo / 2.0)).add_f64(tail);
    let e = k.div_euclid(256);
    if e < -960 {
        // The low half would fall below the normal range and lose bits.
        return None;
    }
    let coarse = t.exp2_coarse.get(index(k.rem_euclid(256) >> 4)?)?;
    let fine = t.exp2_fine.get(index(k.rem_euclid(16))?)?;
    let table = coarse.mul(*fine);
    let value = table.add(table.mul(q)).scale(e);
    // The tail's roundings (seven operations, below 2^-50 of it), the
    // double-double operations and the tables (2^-100), and the
    // reduction: ln 2's own error times k < 2^18.1 and the roundings of
    // the correction terms, each below 2^-96; the bound is 2^4 above
    // their sum.
    let err = tail.abs() * tiny(45) + tiny(86);
    Some((value, err))
}

/// `e^x` for a finite `x`, or `None` for the accurate phase.
pub(super) fn exp(x: f64) -> Option<f64> {
    if !(-708.0..=709.0).contains(&x) {
        return None;
    }
    let (value, rel) = exp_core(Dd::from(x))?;
    round_checked(value, value.hi.abs() * rel)
}

// ---------------------------------------------------------------------
// ln

/// `ln x` for a finite `x > 0` as `(value, absolute error bound)`.
#[expect(
    clippy::many_single_char_names,
    reason = "the notation of the numerical literature"
)]
fn ln_core(x: f64) -> Option<(Dd, f64)> {
    let t = &TABLES;
    // x = 2^e × m with m in [√½, √2).
    let (x, shift) = if x < f64::MIN_POSITIVE {
        (x * pow2(54), -54_i64)
    } else {
        (x, 0)
    };
    let raw = x.to_bits();
    let field = i64::try_from((raw >> 52) & 0x7ff).ok()?;
    let mut m = f64::from_bits((raw & ((1 << 52) - 1)) | (1023 << 52));
    let mut e = field.saturating_sub(1023).saturating_add(shift);
    if m > std::f64::consts::SQRT_2 {
        m /= 2.0;
        e = e.saturating_add(1);
    }
    let i = nearest_i64((m - 1.0) * 256.0).clamp(LN_FIRST, LN_LAST);
    let slot = index(i.saturating_sub(LN_FIRST))?;
    let (c, minus_ln_c) = (*t.ln_c.get(slot)?, *t.ln.get(slot)?);
    // r = m c - 1, exactly as a double-double: m c - 1 is exact by
    // Sterbenz's lemma. |r| < 2^-8.4.
    let (ph, pl) = two_prod(m, c);
    let (rh, rl) = two_sum(ph - 1.0, pl);
    let r = Dd::new(rh, rl);
    // ln(1 + r) = r - r²/2 + r³/3 - …: three terms in double-double, the
    // tail from r⁴ to r¹² in binary64 (|r⁴/4| < 2^-35.6, its roundings
    // below 2^-48 of it; r¹³/13 < 2^-104 |r| is left out).
    let r2 = r.mul(r);
    let r3 = r2.mul(r);
    let tail = r2.hi
        * r2.hi
        * (-0.25
            + rh * (0.2
                + rh * (-1.0 / 6.0
                    + rh * (1.0 / 7.0
                        + rh * (-0.125
                            + rh * (1.0 / 9.0 + rh * (-0.1 + rh * (1.0 / 11.0 - rh / 12.0))))))));
    let series = r
        .add(Dd::new(-r2.hi / 2.0, -r2.lo / 2.0))
        .add(r3.div(Dd::from(3.0)))
        .add_f64(tail);
    let ef = f64::from(i32::try_from(e).ok()?);
    let value = t.ln2.mul_f64(ef).add(minus_ln_c).add(series);
    let magnitude = t.ln2.hi.abs() * ef.abs() + minus_ln_c.hi.abs() + rh.abs();
    let err = tail.abs() * tiny(45) + rh.abs() * tiny(94) + magnitude * tiny(96);
    Some((value, err))
}

/// `ln x` for a finite `x > 0` other than 1, or `None`.
pub(super) fn ln(x: f64) -> Option<f64> {
    let (value, err) = ln_core(x)?;
    round_checked(value, err)
}

// ---------------------------------------------------------------------
// sin, cos and tan

/// `(sin x, cos x)` with absolute error bounds, for `2^-30 ≤ |x| ≤ 10^6`.
#[expect(
    clippy::many_single_char_names,
    reason = "the notation of the numerical literature"
)]
fn sin_cos_core(x: f64) -> Option<((Dd, f64), (Dd, f64))> {
    let t = &TABLES;
    let (r, quadrant, reduce_err) = if x.abs() < std::f64::consts::FRAC_PI_4 {
        (Dd::from(x), 0, 0.0)
    } else {
        // r = x - k π/2 with π/2 = P1 + P2 + P3 + P4: k P1 and k P2 are
        // exact, x - k P1 too (Sterbenz), the other steps carry their
        // errors; |k P4| < 2^20 × 2^-119.
        let k = nearest_i64(x * std::f64::consts::FRAC_2_PI);
        let kf = f64::from(i32::try_from(k).ok()?);
        let [p1, p2, p3] = t.half_pi_parts;
        let t1 = x - kf * p1;
        let (s, se) = two_sum(t1, -(kf * p2));
        let (p, pe) = two_prod(kf, p3);
        let (r1, r1e) = two_sum(s, -p);
        let (rh, rl) = two_sum(r1, (se + r1e) - pe);
        (Dd::new(rh, rl), k.rem_euclid(4), tiny(93))
    };
    let negative = r.hi < 0.0;
    let ar = if negative { r.neg() } else { r };
    // |r| = j/64 + δ with |δ| ≤ 1/128.
    let j = nearest_i64(ar.hi * 64.0).clamp(0, 51);
    let (sa, ca) = (*t.sin.get(index(j)?)?, *t.cos.get(index(j)?)?);
    let jf = f64::from(i32::try_from(j).ok()?) / 64.0;
    let (dh, dl) = two_sum(ar.hi, -jf);
    let d = Dd::new(dh, dl).add_f64(ar.lo);
    let d2 = d.mul(d);
    let z = d2.hi;
    // sin δ = δ - δ³/6 + … - δ¹¹/11!: δ in double-double, the rest in
    // binary64 (|δ³/6| < 2^-23.5); cos δ - 1 = -δ²/2 + δ⁴/4! - … :
    // -δ²/2 in double-double, the rest in binary64 (|δ⁴/24| < 2^-32.5).
    let sin_tail = d.hi
        * z
        * (-1.0 / 6.0
            + z * (1.0 / 120.0 + z * (-1.0 / 5040.0 + z * (1.0 / 362_880.0 - z / 39_916_800.0))));
    let cos_tail = z
        * z
        * (1.0 / 24.0
            + z * (-1.0 / 720.0
                + z * (1.0 / 40320.0 + z * (-1.0 / 3_628_800.0 + z / 479_001_600.0))));
    let sin_d = d.add_f64(sin_tail);
    let cos_d_minus_1 = Dd::new(-d2.hi / 2.0, -d2.lo / 2.0).add_f64(cos_tail);
    let sin_abs = sa.add(sa.mul(cos_d_minus_1)).add(ca.mul(sin_d));
    let cos = ca.add(ca.mul(cos_d_minus_1)).add(sa.mul(sin_d).neg());
    let sin = if negative { sin_abs.neg() } else { sin_abs };
    let err = (sin_tail.abs() + cos_tail.abs()) * tiny(45) + reduce_err + tiny(94);
    let (sin, cos) = match quadrant {
        0 => (sin, cos),
        1 => (cos, sin.neg()),
        2 => (sin.neg(), cos.neg()),
        _ => (cos.neg(), sin),
    };
    Some(((sin, err), (cos, err)))
}

/// `sin x`, `cos x` or `tan x` for a finite `|x| ≥ 2^-30`, or `None`.
pub(super) fn trig(f: Trig, x: f64) -> Option<f64> {
    if x.abs() > 1.0e6 {
        return None;
    }
    let ((sin, sin_err), (cos, cos_err)) = sin_cos_core(x)?;
    match f {
        Trig::Sin => round_checked(sin, sin_err),
        Trig::Cos => round_checked(cos, cos_err),
        Trig::Tan => {
            let tan = sin.div(cos);
            let rel = sin_err / sin.hi.abs() + cos_err / cos.hi.abs() + tiny(98);
            round_checked(tan, tan.hi.abs() * rel)
        }
    }
}

// ---------------------------------------------------------------------
// atan2

/// `atan2(y, x)` for finite non-zero `y` and `x`, or `None`.
#[expect(
    clippy::many_single_char_names,
    reason = "the notation of the numerical literature"
)]
pub(super) fn atan2(y: f64, x: f64) -> Option<f64> {
    let t = &TABLES;
    let (ay, ax) = (y.abs(), x.abs());
    let swap = ay > ax;
    let (num, den) = if swap { (ax, ay) } else { (ay, ax) };
    // Scale both so den is in [1, 2): exact unless num becomes tiny, and
    // then the result is tiny or near a multiple of π/2, which the
    // accurate phase handles.
    let shift = Big::from_f64(den).top().saturating_neg();
    let (num, den) = (Dd::from(num).scale(shift).hi, Dd::from(den).scale(shift).hi);
    if num < tiny(900) || !(1.0..2.0).contains(&den) {
        return None;
    }
    // t = num / den ≤ 1 as a double-double.
    let ratio = Dd::from(num).div(Dd::from(den));
    // atan t = atan(j/64) + atan u, u = (t - j/64) / (1 + t j/64), |u| ≤
    // 1/128.
    let j = nearest_i64(ratio.hi * 64.0).clamp(0, 64);
    let base = *t.atan.get(index(j)?)?;
    let c = f64::from(i32::try_from(j).ok()?) / 64.0;
    let u = if j == 0 {
        ratio
    } else {
        let (nh, nl) = two_sum(ratio.hi, -c);
        let numerator = Dd::new(nh, nl).add_f64(ratio.lo);
        let denominator = ratio.mul_f64(c).add_f64(1.0);
        numerator.div(denominator)
    };
    // atan u = u - u³/3 + … + u¹³/13: u in double-double, the rest in
    // binary64 (|u³/3| < 2^-22.5).
    let z = u.hi * u.hi;
    let tail = u.hi
        * z
        * (-1.0 / 3.0
            + z * (0.2 + z * (-1.0 / 7.0 + z * (1.0 / 9.0 + z * (-1.0 / 11.0 + z / 13.0)))));
    let mut angle = base.add(u.add_f64(tail));
    let mut err = tail.abs() * tiny(45) + (base.hi + u.hi.abs()) * tiny(94);
    if swap {
        angle = t.half_pi.add(angle.neg());
        err += tiny(95);
    }
    if x < 0.0 {
        angle = t.pi.add(angle.neg());
        err += tiny(94);
    }
    if y < 0.0 {
        angle = angle.neg();
    }
    round_checked(angle, err)
}

// ---------------------------------------------------------------------
// pow

/// `|x|^y` for a finite `|x| > 0` other than 1 and a finite non-zero `y`
/// whose power is not exact, or `None`.
pub(super) fn pow(ax: f64, y: f64) -> Option<f64> {
    if y.abs() > pow2(900) {
        return None;
    }
    let (ln, ln_err) = ln_core(ax)?;
    // z = y ln|x|: the product's error term is exact, y × ln.lo rounds.
    let (p, pe) = two_prod(y, ln.hi);
    let (zh, zl) = fast_two_sum(p, pe + y * ln.lo);
    if !(-708.0..=709.0).contains(&zh) {
        return None;
    }
    let z_err = y.abs() * ln_err + zh.abs() * tiny(100);
    let (value, rel) = exp_core(Dd::new(zh, zl))?;
    // e^(z + d) = e^z (1 + d + …): an error d in z is a relative error of
    // at most 1.01 d in the result.
    round_checked(value, value.hi.abs() * (rel + 1.01 * z_err))
}

#[cfg(test)]
mod tests {
    use super::super::accurate;
    use super::*;

    /// splitmix64.
    fn next(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn uniform(state: &mut u64, low: f64, high: f64) -> f64 {
        let unit = f64::from(u32::try_from(next(state) >> 40).unwrap()) / f64::from(1_u32 << 24);
        low + (high - low) * unit
    }

    /// The error of a double-double against a `Big` value, in units of
    /// the bound: below 1/16 for every input.
    fn ratio(value: Dd, err: f64, exact: &Big) -> f64 {
        let diff = Big::from_f64(value.hi)
            .add(&Big::from_f64(value.lo), 6)
            .sub(exact, 6)
            .to_f64()
            .abs();
        diff / err
    }

    #[test]
    fn double_double_operations_are_accurate() {
        let a = Dd::from(1.0).div(Dd::from(3.0));
        let back = a.mul_f64(3.0).add_f64(-1.0);
        assert!(back.hi.abs() < tiny(100), "{back:?}");
        let (p, e) = two_prod(0.1, 0.3);
        let exact = Big::from_f64(0.1).mul(&Big::from_f64(0.3), 4);
        let sum = Big::from_f64(p).add(&Big::from_f64(e), 4);
        assert_eq!(sum, exact);
    }

    #[test]
    fn exp_and_ln_bounds_hold() {
        let mut state = 7;
        let mut worst: f64 = 0.0;
        for _ in 0..400 {
            let x = uniform(&mut state, -660.0, 709.0);
            let (value, rel) = exp_core(Dd::from(x)).unwrap();
            worst = worst.max(ratio(value, value.hi.abs() * rel, &accurate::exp(x, 4)));
            let y = f64::from_bits(next(&mut state) >> 2);
            if y.is_finite() && y > 0.0 {
                let (value, err) = ln_core(y).unwrap();
                worst = worst.max(ratio(value, err, &accurate::ln(y, 4)));
            }
            let near_one = 1.0 + uniform(&mut state, -0.01, 0.01);
            let (value, err) = ln_core(near_one).unwrap();
            worst = worst.max(ratio(value, err, &accurate::ln(near_one, 4)));
        }
        assert!(worst < 1.0 / 16.0, "worst error / bound: {worst}");
    }

    #[test]
    fn trig_and_atan2_bounds_hold() {
        let mut state = 11;
        let mut worst: f64 = 0.0;
        for _ in 0..300 {
            let x = if next(&mut state) & 1 == 0 {
                uniform(&mut state, -10.0, 10.0)
            } else {
                uniform(&mut state, -1.0e6, 1.0e6)
            };
            if x.abs() < 1e-9 {
                continue;
            }
            let ((sin, sin_err), (cos, cos_err)) = sin_cos_core(x).unwrap();
            let (exact_sin, exact_cos) = accurate::sin_cos(x, 4);
            worst = worst.max(ratio(sin, sin_err, &exact_sin));
            worst = worst.max(ratio(cos, cos_err, &exact_cos));
        }
        assert!(worst < 1.0 / 16.0, "worst error / bound: {worst}");
    }

    /// The bounds of `atan2` and `pow`, through their rounding test: a
    /// bound 16 times too small would let these pass only by luck, so
    /// the test widens nothing and compares with the accurate phase.
    #[test]
    fn atan2_and_pow_agree_with_the_accurate_phase_on_hard_ranges() {
        let mut state = 5;
        for _ in 0..300 {
            let y = uniform(&mut state, -4.0, 4.0)
                * pow2(nearest_i64(uniform(&mut state, -60.0, 60.0)));
            let x = uniform(&mut state, -4.0, 4.0);
            if y != 0.0
                && x != 0.0
                && let Some(v) = atan2(y, x)
            {
                let want = accurate::ziv(|p| accurate::atan2(y, x, p));
                assert_eq!(v.to_bits(), want.to_bits(), "atan2 {y:e} {x:e}");
            }
            let base = uniform(&mut state, 0.0, 3.0);
            let exponent = uniform(&mut state, -300.0, 300.0);
            if base > 0.0
                && base.to_bits() != 1.0_f64.to_bits()
                && (exponent * base.log2()).abs() < 1000.0
                && let Some(v) = pow(base, exponent)
            {
                let want = accurate::ziv(|p| accurate::pow(base, exponent, p));
                assert_eq!(v.to_bits(), want.to_bits(), "pow {base:e} {exponent:e}");
            }
        }
    }

    #[test]
    fn fast_results_are_the_accurate_ones() {
        let mut state = 3;
        for _ in 0..300 {
            let x = uniform(&mut state, -20.0, 20.0);
            let y = uniform(&mut state, -20.0, 20.0);
            if let Some(v) = exp(x) {
                assert_eq!(
                    v.to_bits(),
                    accurate::ziv(|p| accurate::exp(x, p)).to_bits(),
                    "exp {x:e}"
                );
            }
            let ax = x.abs();
            if let Some(v) = ln(ax) {
                assert_eq!(
                    v.to_bits(),
                    accurate::ziv(|p| accurate::ln(ax, p)).to_bits(),
                    "ln {ax:e}"
                );
            }
            for f in [Trig::Sin, Trig::Cos, Trig::Tan] {
                if let Some(v) = trig(f, x) {
                    let want = accurate::ziv(|p| {
                        let (s, c) = accurate::sin_cos(x, p);
                        match f {
                            Trig::Sin => s,
                            Trig::Cos => c,
                            Trig::Tan => s.div(&c, p.saturating_add(2)),
                        }
                    });
                    assert_eq!(v.to_bits(), want.to_bits(), "{f:?} {x:e}");
                }
            }
            if let Some(v) = atan2(y, x) {
                assert_eq!(
                    v.to_bits(),
                    accurate::ziv(|p| accurate::atan2(y, x, p)).to_bits(),
                    "atan2 {y:e} {x:e}"
                );
            }
            if let Some(v) = pow(ax, y) {
                assert_eq!(
                    v.to_bits(),
                    accurate::ziv(|p| accurate::pow(ax, y, p)).to_bits(),
                    "pow {ax:e} {y:e}"
                );
            }
        }
    }

    /// The fast phase against the accurate one on many random inputs per
    /// function (`CHEBY_MATH_FAST_INPUTS`, 100,000 by default), with the
    /// share that falls back. Run in release:
    /// `cargo test -p cheby_runtime --release fast_phase_agrees -- --ignored --nocapture`.
    #[test]
    #[ignore = "a long check, about a minute in release"]
    #[expect(
        clippy::many_single_char_names,
        reason = "the notation of the numerical literature"
    )]
    fn fast_phase_agrees_with_the_accurate_phase_at_scale() {
        let count: u32 = std::env::var("CHEBY_MATH_FAST_INPUTS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(100_000);
        let mut state = 0x0bad_5eed;
        let names = ["exp", "ln", "sin", "cos", "tan", "atan2", "pow"];
        for (which, name) in names.iter().enumerate() {
            let mut fallbacks = 0_u32;
            for _ in 0..count {
                let bits = next(&mut state);
                let wide = f64::from_bits(bits);
                let (x, y) = match which {
                    0 => (uniform(&mut state, -708.0, 709.0), 0.0),
                    1 => (wide.abs(), 0.0),
                    2..=4 => (
                        if bits & 1 == 0 {
                            uniform(&mut state, -8.0, 8.0)
                        } else {
                            uniform(&mut state, -1.0e6, 1.0e6)
                        },
                        0.0,
                    ),
                    5 => (wide, f64::from_bits(next(&mut state))),
                    _ => (
                        uniform(&mut state, 0.0, 4.0),
                        uniform(&mut state, -400.0, 400.0),
                    ),
                };
                if !(x.is_finite() && y.is_finite()) || x == 0.0 {
                    continue;
                }
                let (fast, accurate) = match which {
                    0 => (exp(x), accurate::ziv(|p| accurate::exp(x, p))),
                    1 if x.to_bits() != 1.0_f64.to_bits() => {
                        (ln(x), accurate::ziv(|p| accurate::ln(x, p)))
                    }
                    2..=4 if x.abs() >= 1e-9 => {
                        let f = [Trig::Sin, Trig::Cos, Trig::Tan][which - 2];
                        let want = accurate::ziv(|p| {
                            let (s, c) = accurate::sin_cos(x, p);
                            match f {
                                Trig::Sin => s,
                                Trig::Cos => c,
                                Trig::Tan => s.div(&c, p.saturating_add(2)),
                            }
                        });
                        (trig(f, x), want)
                    }
                    5 if y != 0.0 => (atan2(y, x), accurate::ziv(|p| accurate::atan2(y, x, p))),
                    6 if x.to_bits() != 1.0_f64.to_bits()
                        && y != 0.0
                        && (y * x.log2()).abs() < 1000.0 =>
                    {
                        (pow(x, y), accurate::ziv(|p| accurate::pow(x, y, p)))
                    }
                    _ => continue,
                };
                match fast {
                    Some(v) => assert_eq!(v.to_bits(), accurate.to_bits(), "{name}({x:e}, {y:e})"),
                    None => fallbacks += 1,
                }
            }
            println!("{name}: {count} inputs, {fallbacks} to the accurate phase");
        }
    }
}
