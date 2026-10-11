//! The accurate phase of Ziv's strategy (D-568): the seven functions
//! evaluated with [`Big`] at a precision that rises until the rounding is
//! decided.
//!
//! Every evaluation at `prec` limbs works at `prec + 2` limbs and loses
//! far fewer than 64 bits to its reductions (each step below says how
//! many), so the result is trusted to `64 × prec - GUARD` bits. The
//! argument reductions keep that relative precision:
//!
//! - `exp`: `x = k ln 2 + r` with `ln 2` 64 bits wider than the result,
//!   then `e^(r / 2^10)` by its Taylor series and ten squarings (10 bits).
//! - `ln`: `x = 2^e m` with `m` in `[√½, √2)`, `ln m = 2 atanh((m - 1) /
//!   (m + 1))`; `e ln 2` and `ln m` never cancel, since `|ln m| < ln 2 /
//!   2` and `e = 0` near 1.
//! - `sin`, `cos`, `tan`: `x = k π/2 + r` with `π` wide enough to keep
//!   the result's precision plus 64 bits after the cancellation (a
//!   binary64 lies at least 2^-62 relative from a multiple of π/2), then
//!   Taylor series of `sin r` and `cos r` for `|r| ≤ π/4`.
//! - `atan2`: `atan t` of the ratio `t ≤ 1` after four halvings `t / (1 +
//!   √(1 + t²))`, then `π - a` or `π/2 - a`, which never cancel.
//! - `pow`: `exp(y ln x)` with `ln x` one limb wider; `|y ln x| < 1100`
//!   costs at most 11 bits.

use super::big::Big;
use super::tables;

/// The bits of each evaluation that are not trusted.
pub(super) const GUARD: i64 = 64;

/// The precisions tried, in limbs. Every binary64 worst case of the
/// univariate functions is decided at the first (CORE-MATH's lists need
/// fewer than 160 bits).
const PRECISIONS: [usize; 4] = [4, 8, 16, 32];

/// The limbs of the generated constants π and ln 2 (`tables.rs`).
pub(super) const CONST_LIMBS: usize = 40;

/// Evaluates `f` at rising precisions until its value rounds to a
/// binary64 that its trusted bits decide (Ziv's strategy). `f(prec)` must
/// be within `2^(top - (64 × prec - GUARD))` of the exact value.
pub(super) fn ziv(f: impl Fn(usize) -> Big) -> f64 {
    let mut last = Big::zero();
    for prec in PRECISIONS {
        last = f(prec);
        let trusted = i64::try_from(prec)
            .unwrap_or(0)
            .saturating_mul(64)
            .saturating_sub(GUARD);
        if let Some(value) = last.round_f64(trusted) {
            return value;
        }
    }
    // Not reached for any binary64 input the evaluation found: a value
    // within 2^-1984 relative of a midpoint would be exact, and the callers
    // handle exact results before they get here. The value at 2048 bits
    // is rounded as it is (D-623).
    last.round_f64(i64::MAX).unwrap_or(f64::NAN)
}

/// `atan(1/m)` to `prec` limbs, by its series, for an integer `m > 1`.
fn atan_inverse(m: u64, prec: usize) -> Big {
    let m2 = m.saturating_mul(m);
    let mut power = Big::from_u64(1).div_u64(m, prec);
    let mut sum = power.clone();
    let mut k = 1_u64;
    loop {
        power = power.div_u64(m2, prec);
        let term = power.div_u64(k.saturating_mul(2).saturating_add(1), prec);
        if negligible(&term, &sum, prec) {
            return sum;
        }
        sum = if k & 1 == 1 {
            sum.sub(&term, prec)
        } else {
            sum.add(&term, prec)
        };
        k = k.saturating_add(1);
    }
}

/// `prec` limbs as a bit count.
fn bits_of(prec: usize) -> i64 {
    i64::try_from(prec).unwrap_or(0).saturating_mul(64)
}

/// π to `prec` limbs: Machin's formula `16 atan(1/5) - 4 atan(1/239)`.
fn compute_pi(prec: usize) -> Big {
    let work = prec.saturating_add(2);
    atan_inverse(5, work)
        .shl(4)
        .sub(&atan_inverse(239, work).shl(2), work)
        .truncate(prec)
}

/// ln 2 to `prec` limbs: `2 atanh(1/3)`.
fn compute_ln2(prec: usize) -> Big {
    let work = prec.saturating_add(2);
    let third = Big::from_u64(1).div_u64(3, work);
    atanh_series(&third, work).shl(1).truncate(prec)
}

/// π to `prec` limbs, from the generated constant when it is wide
/// enough.
pub(super) fn pi(prec: usize) -> Big {
    if prec > CONST_LIMBS {
        return compute_pi(prec);
    }
    Big::from_limbs(tables::PI_EXP, &tables::PI_LIMBS, prec)
}

/// ln 2 to `prec` limbs, from the generated constant when it is wide
/// enough.
pub(super) fn ln2(prec: usize) -> Big {
    if prec > CONST_LIMBS {
        return compute_ln2(prec);
    }
    Big::from_limbs(tables::LN2_EXP, &tables::LN2_LIMBS, prec)
}

/// π and ln 2 to `prec` limbs, computed, for `tables_gen.rs`.
#[cfg(test)]
pub(super) fn computed_constants(prec: usize) -> (Big, Big) {
    (compute_pi(prec), compute_ln2(prec))
}

/// Whether `term` no longer changes `sum` at `prec` limbs.
fn negligible(term: &Big, sum: &Big, prec: usize) -> bool {
    term.is_zero() || term.top() < sum.top().saturating_sub(bits_of(prec).saturating_add(8))
}

/// `atanh z = z + z³/3 + z⁵/5 + …` for `|z| ≤ 1/3`.
fn atanh_series(z: &Big, prec: usize) -> Big {
    let z2 = z.mul(z, prec);
    let mut power = z.clone();
    let mut sum = z.clone();
    let mut k = 1_u64;
    loop {
        power = power.mul(&z2, prec);
        let term = power.div_u64(k.saturating_mul(2).saturating_add(1), prec);
        if negligible(&term, &sum, prec) {
            return sum;
        }
        sum = sum.add(&term, prec);
        k = k.saturating_add(1);
    }
}

/// The nearest integer to a binary64 of magnitude below 2^62.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "the callers pass values far inside the range of i64"
)]
fn nearest_i64(x: f64) -> i64 {
    x.round() as i64
}

/// `e^x` for a `Big` x with `|x| < 2^20`, to `prec` limbs (plus 2 for the
/// work). 10 bits are lost to the squarings.
pub(super) fn exp_big(x: &Big, prec: usize) -> Big {
    let work = prec.saturating_add(2);
    let wide = work.saturating_add(1);
    let twos = nearest_i64(x.to_f64() * std::f64::consts::LOG2_E);
    let reduced = x.sub(&ln2(wide).mul(&Big::from_i64(twos), wide), work);
    let small = reduced.shl(-10);
    let mut term = Big::from_u64(1);
    let mut sum = Big::from_u64(1);
    let mut index = 1_u64;
    loop {
        term = term.mul(&small, work).div_u64(index, work);
        if negligible(&term, &sum, work) {
            break;
        }
        sum = sum.add(&term, work);
        index = index.saturating_add(1);
    }
    for _ in 0..10 {
        sum = sum.mul(&sum, work);
    }
    sum.shl(twos)
}

/// `e^x` for a finite binary64 with `|x| < 2^20`.
pub(super) fn exp(x: f64, prec: usize) -> Big {
    exp_big(&Big::from_f64(x), prec)
}

/// `ln x` for a finite binary64 `x > 0`, to `prec` limbs (plus 2).
pub(super) fn ln(x: f64, prec: usize) -> Big {
    let work = prec.saturating_add(2);
    let big = Big::from_f64(x);
    // x = 2^e × m with m in [1, 2), then in [√½, √2).
    let mut e = big.top();
    let mut m = big.shl(e.saturating_neg());
    if m.to_f64() > std::f64::consts::SQRT_2 {
        m = m.shl(-1);
        e = e.saturating_add(1);
    }
    let one = Big::from_u64(1);
    let z = m.sub(&one, work).div(&m.add(&one, work), work);
    let ln_m = atanh_series(&z, work).shl(1);
    if e == 0 {
        return ln_m;
    }
    ln_m.add(&ln2(work).mul(&Big::from_i64(e), work), work)
}

/// `(sin x, cos x)` for a finite binary64, to `prec` limbs (plus 2).
pub(super) fn sin_cos(x: f64, prec: usize) -> (Big, Big) {
    let work = prec.saturating_add(2);
    let big = Big::from_f64(x);
    let (r, k) = if x.abs() < 0.78 {
        (big, 0)
    } else {
        // π/2 wide enough that r = x - k π/2 keeps `work` limbs plus 64
        // bits after cancelling up to 62 bits below x's own exponent.
        let extra = usize::try_from(big.top().max(0).saturating_add(127) / 64).unwrap_or(0);
        let wide = work.saturating_add(extra).saturating_add(1);
        let half_pi = pi(wide).shl(-1);
        let k = big.div(&half_pi, wide).round_to_integer();
        let r = big.sub(&k.mul(&half_pi, wide), wide).truncate(work);
        (r, k.integer_mod_4())
    };
    let r2 = r.mul(&r, work);
    // sin r = r - r³/3! + r⁵/5! - …
    let mut term = r.clone();
    let mut sin = r.clone();
    let mut j = 1_u64;
    loop {
        let divisor = j
            .saturating_mul(2)
            .saturating_mul(j.saturating_mul(2).saturating_add(1));
        term = term.mul(&r2, work).div_u64(divisor, work).neg();
        if negligible(&term, &sin, work) {
            break;
        }
        sin = sin.add(&term, work);
        j = j.saturating_add(1);
    }
    // cos r = 1 - r²/2! + r⁴/4! - …
    let mut term = Big::from_u64(1);
    let mut cos = Big::from_u64(1);
    let mut j = 1_u64;
    loop {
        let divisor = j
            .saturating_mul(2)
            .saturating_sub(1)
            .saturating_mul(j.saturating_mul(2));
        term = term.mul(&r2, work).div_u64(divisor, work).neg();
        if negligible(&term, &cos, work) {
            break;
        }
        cos = cos.add(&term, work);
        j = j.saturating_add(1);
    }
    match k {
        0 => (sin, cos),
        1 => (cos, sin.neg()),
        2 => (sin.neg(), cos.neg()),
        _ => (cos.neg(), sin),
    }
}

/// `atan t` for `0 ≤ t ≤ 1`, to `prec` limbs (plus 2).
fn atan_unit(t: &Big, prec: usize) -> Big {
    let work = prec.saturating_add(2);
    let one = Big::from_u64(1);
    let mut t = t.clone();
    // Four halvings of the angle: t / (1 + √(1 + t²)) ≤ tan(π/64).
    for _ in 0..4 {
        let root = one.add(&t.mul(&t, work), work).sqrt(work);
        t = t.div(&one.add(&root, work), work);
    }
    let t2 = t.mul(&t, work);
    let mut power = t.clone();
    let mut sum = t;
    let mut k = 1_u64;
    loop {
        power = power.mul(&t2, work).neg();
        let term = power.div_u64(k.saturating_mul(2).saturating_add(1), work);
        if negligible(&term, &sum, work) {
            break;
        }
        sum = sum.add(&term, work);
        k = k.saturating_add(1);
    }
    sum.shl(4)
}

/// `atan2(y, x)` for finite, non-zero binary64s, to `prec` limbs (plus
/// 2).
pub(super) fn atan2(y: f64, x: f64, prec: usize) -> Big {
    let work = prec.saturating_add(2);
    let (ay, ax) = (Big::from_f64(y.abs()), Big::from_f64(x.abs()));
    let angle = if ay.cmp_abs(&ax).is_le() {
        atan_unit(&ay.div(&ax, work), prec)
    } else {
        pi(work)
            .shl(-1)
            .sub(&atan_unit(&ax.div(&ay, work), prec), work)
    };
    let angle = if x < 0.0 {
        pi(work).sub(&angle, work)
    } else {
        angle
    };
    if y < 0.0 { angle.neg() } else { angle }
}

/// `|x|^y` for a finite `|x| > 0` other than 1 and a finite `y` with
/// `|y log2 |x|| ≤ 1200`, to `prec` limbs (plus 2).
pub(super) fn pow(ax: f64, y: f64, prec: usize) -> Big {
    let work = prec.saturating_add(2);
    let t = ln(ax, prec.saturating_add(1)).mul(&Big::from_f64(y), work.saturating_add(1));
    exp_big(&t, prec)
}

/// `n × π / 4` rounded to binary64, for the special values of `atan2`.
pub(super) fn quarter_pi(n: u64) -> f64 {
    ziv(|prec| {
        pi(prec.saturating_add(2))
            .mul_u64(n, prec.saturating_add(2))
            .shl(-2)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_have_their_known_bits() {
        // π = 0x3.243F6A8885A308D313198A2E03707344A4093822299F31D0…
        let pi_bits = pi(4).shl(-1).round_f64(i64::MAX);
        assert_eq!(pi_bits, Some(std::f64::consts::FRAC_PI_2));
        let scaled = pi(4).shl(124).round_to_integer();
        // The first 128 bits of π: 0x3243F6A8885A308D313198A2E0370734.
        let expected = Big::from_u64(0x3243_F6A8_885A_308D)
            .shl(64)
            .add(&Big::from_u64(0x3131_98A2_E037_0734), 4);
        assert_eq!(
            scaled.sub(&expected, 4).cmp_abs(&Big::from_u64(1)),
            std::cmp::Ordering::Less
        );
        // ln 2 = 0x0.B17217F7D1CF79ABC9E3B39803F2F6AF…
        let scaled = ln2(4).shl(128).round_to_integer();
        let expected = Big::from_u64(0xB172_17F7_D1CF_79AB)
            .shl(64)
            .add(&Big::from_u64(0xC9E3_B398_03F2_F6AF), 4);
        assert_eq!(
            scaled.sub(&expected, 4).cmp_abs(&Big::from_u64(1)),
            std::cmp::Ordering::Less
        );
        // The generated constants and a direct computation agree.
        assert_eq!(pi(CONST_LIMBS + 1).truncate(CONST_LIMBS), pi(CONST_LIMBS));
        assert_eq!(ln2(CONST_LIMBS + 1).truncate(CONST_LIMBS), ln2(CONST_LIMBS));
    }

    #[test]
    fn functions_give_known_values() {
        assert_eq!(ziv(|p| exp(1.0, p)), std::f64::consts::E);
        assert_eq!(ziv(|p| ln(2.0, p)), std::f64::consts::LN_2);
        assert_eq!(ziv(|p| ln(10.0, p)), std::f64::consts::LN_10);
        assert_eq!(ziv(|p| sin_cos(1.0e22, p).0), -0.852_200_849_767_188_8);
        assert_eq!(ziv(|p| sin_cos(std::f64::consts::PI, p).1), -1.0);
        assert_eq!(ziv(|p| atan2(1.0, 1.0, p)), std::f64::consts::FRAC_PI_4);
        assert_eq!(
            ziv(|p| atan2(1.0, -1.0, p)),
            3.0 * std::f64::consts::FRAC_PI_4
        );
        assert_eq!(ziv(|p| pow(2.0, 0.5, p)), std::f64::consts::SQRT_2);
        assert_eq!(quarter_pi(4), std::f64::consts::PI);
        assert_eq!(quarter_pi(2), std::f64::consts::FRAC_PI_2);
        assert_eq!(ziv(|p| exp(-745.133_219_101_941_1, p)), 5e-324);
        assert_eq!(
            ziv(|p| exp(709.782_712_893_384, p)),
            1.797_693_134_862_273_2e308
        );
        assert_eq!(ziv(|p| exp(709.8, p)), f64::INFINITY);
    }
}
