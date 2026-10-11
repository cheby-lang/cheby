//! The correctly rounded functions of D-396: `float::pow`, `exp`, `ln`,
//! `sin`, `cos`, `tan` and `atan2` return the `Float` nearest to the
//! exact value (ties to even) on every target, computed by code shipped
//! with the runtime (ADR-0054), in Rust with double-double evaluation and
//! Ziv's rounding test (D-568).
//!
//! Each function first settles its special values (C99 Annex F and
//! `docs/stdlib/float.md`) and its exactly representable results, then
//! follows Ziv's strategy:
//!
//! - `fast`: a double-double evaluation with a stated error bound, whose
//!   result is returned when the bound cannot move it across a rounding
//!   boundary (the rounding test);
//! - `accurate`: otherwise, an evaluation with `big`'s arbitrary
//!   precision, at rising precisions until the rounding is decided.
//!
//! Exact results are found before Ziv's loop, which would not end on
//! them: `exp(0)`, `ln(1)`, `sin(±0)`, `cos(0)`, `tan(±0)`, the zero and
//! infinite cases of `atan2`, and the powers `x^y` that are dyadic
//! rationals (`y` an integer, or `x` a perfect power of `y`'s
//! denominator). By Lindemann–Weierstrass every other result is
//! transcendental or not dyadic, so it is never a midpoint, and the loop
//! ends.
//!
//! The implementation is verified before it is registered (D-553, D-568):
//! checked against reference values computed at 256 bits or more with
//! `astro-float` and rounded to binary64, with a rounding-boundary check,
//! on at least 10,000 random inputs per function, the special values, and
//! CORE-MATH's published hard cases. The reference (`reference`) and the
//! evaluation (`eval`) are test modules of this one:
//!
//! ```text
//! cargo test -p cheby_runtime --release num::math -- --ignored --nocapture
//! ```

use super::Kernel;

mod accurate;
mod big;
#[cfg(test)]
mod eval;
mod fast;
#[cfg(test)]
mod reference;
mod tables;
#[cfg(test)]
mod tables_gen;

use big::Big;

/// Whether every function has verified and is registered by [`table`],
/// so `std::float` is complete.
pub(crate) const VERIFIED: bool = true;

/// 2^-30: below it, `sin x` and `tan x` round to `x`, and `cos x` to 1.
const TWO_POW_M30: f64 = 9.313_225_746_154_785e-10;

// ---------------------------------------------------------------------
// exp and ln

/// `e^x`, correctly rounded.
pub(crate) fn exp(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    // e^710 overflows; e^-746 is below a quarter of the smallest
    // subnormal.
    if x > 710.0 {
        return f64::INFINITY;
    }
    if x < -746.0 {
        return 0.0;
    }
    if x == 0.0 {
        return 1.0;
    }
    fast::exp(x).unwrap_or_else(|| accurate::ziv(|prec| accurate::exp(x, prec)))
}

/// `ln x`, correctly rounded.
pub(crate) fn ln(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if x.is_infinite() {
        return f64::INFINITY;
    }
    if x.to_bits() == 1.0_f64.to_bits() {
        return 0.0;
    }
    fast::ln(x).unwrap_or_else(|| accurate::ziv(|prec| accurate::ln(x, prec)))
}

// ---------------------------------------------------------------------
// sin, cos and tan

/// Which of the three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trig {
    Sin,
    Cos,
    Tan,
}

/// `sin x`, `cos x` or `tan x`, correctly rounded.
pub(crate) fn trig(f: Trig, x: f64) -> f64 {
    if !x.is_finite() {
        return f64::NAN;
    }
    if x.abs() < TWO_POW_M30 {
        // sin x and tan x differ from x by less than |x|³/2 < 2^-61 |x|,
        // and cos x from 1 by less than 2^-61, far below half an ulp of x
        // (2^-54 |x| or more) and of 1, which are representable. Zeros
        // keep their sign.
        return if f == Trig::Cos { 1.0 } else { x };
    }
    fast::trig(f, x).unwrap_or_else(|| {
        accurate::ziv(|prec| {
            let (sin, cos) = accurate::sin_cos(x, prec);
            match f {
                Trig::Sin => sin,
                Trig::Cos => cos,
                Trig::Tan => sin.div(&cos, prec.saturating_add(2)),
            }
        })
    })
}

// ---------------------------------------------------------------------
// atan2

/// `atan2(y, x)`, correctly rounded, with the special values of C99
/// F.9.1.4.
pub(crate) fn atan2(y: f64, x: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    let with_sign_of_y = |v: f64| if y.is_sign_negative() { -v } else { v };
    if y == 0.0 {
        let toward_positive = x > 0.0 || (x == 0.0 && x.is_sign_positive());
        return with_sign_of_y(if toward_positive {
            0.0
        } else {
            std::f64::consts::PI
        });
    }
    if x == 0.0 {
        return with_sign_of_y(std::f64::consts::FRAC_PI_2);
    }
    if y.is_infinite() {
        let angle = if x == f64::INFINITY {
            std::f64::consts::FRAC_PI_4
        } else if x == f64::NEG_INFINITY {
            accurate::quarter_pi(3)
        } else {
            std::f64::consts::FRAC_PI_2
        };
        return with_sign_of_y(angle);
    }
    if x.is_infinite() {
        return with_sign_of_y(if x > 0.0 { 0.0 } else { std::f64::consts::PI });
    }
    fast::atan2(y, x).unwrap_or_else(|| accurate::ziv(|prec| accurate::atan2(y, x, prec)))
}

// ---------------------------------------------------------------------
// pow

/// Whether `y` is an integer.
#[expect(clippy::float_cmp, reason = "an exact test")]
fn is_integer(y: f64) -> bool {
    y.is_finite() && y.trunc() == y
}

/// Whether `y` is an odd integer (every binary64 above 2^53 is even).
#[expect(clippy::float_cmp, reason = "an exact test")]
fn is_odd_integer(y: f64) -> bool {
    is_integer(y) && y.abs() < 9_007_199_254_740_992.0 && (y % 2.0).abs() == 1.0
}

/// `x^y`, correctly rounded, with the special values of C99 F.9.4.4 and
/// `docs/stdlib/float.md`: 1 for `y = ±0` and for `x = 1`, NaN included.
pub(crate) fn pow(x: f64, y: f64) -> f64 {
    if y == 0.0 || x.to_bits() == 1.0_f64.to_bits() {
        return 1.0;
    }
    if x.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    let odd = is_odd_integer(y);
    if x == 0.0 {
        return match (y < 0.0, odd) {
            (true, true) => 1.0 / x,
            (true, false) => f64::INFINITY,
            (false, true) => x,
            (false, false) => 0.0,
        };
    }
    if y.is_infinite() {
        let magnitude = x.abs();
        return if magnitude.to_bits() == 1.0_f64.to_bits() {
            1.0
        } else if (magnitude < 1.0) == (y > 0.0) {
            0.0
        } else {
            f64::INFINITY
        };
    }
    if x.is_infinite() {
        let magnitude = if y > 0.0 { f64::INFINITY } else { 0.0 };
        return if x < 0.0 && odd {
            -magnitude
        } else {
            magnitude
        };
    }
    if x < 0.0 && !is_integer(y) {
        return f64::NAN;
    }
    let negative = x < 0.0 && odd;
    let signed = |v: f64| if negative { -v } else { v };
    let ax = x.abs();
    // Far beyond the range: |y log2 |x|| > 1100 overflows or underflows
    // whatever the rounding (log2 is only an estimate here, good to far
    // better than the margin).
    let log2 = y * ax.log2();
    if log2 > 1100.0 {
        return signed(f64::INFINITY);
    }
    if log2 < -1200.0 {
        return signed(0.0);
    }
    if let Some(value) = exact_power(ax, y) {
        return signed(value);
    }
    signed(fast::pow(ax, y).unwrap_or_else(|| accurate::ziv(|prec| accurate::pow(ax, y, prec))))
}

/// A finite non-zero binary64 as `odd × 2^e`, the sign dropped.
fn odd_parts(x: f64) -> (u64, i64) {
    let raw = x.to_bits() & 0x7fff_ffff_ffff_ffff;
    let field = raw >> 52;
    let fraction = raw & ((1 << 52) - 1);
    let (m, e) = if field == 0 {
        (fraction, -1074_i64)
    } else {
        (
            fraction | (1 << 52),
            i64::try_from(field).unwrap_or(0).wrapping_sub(1075),
        )
    };
    let zeros = m.trailing_zeros();
    (m >> zeros, e.saturating_add(i64::from(zeros)))
}

/// The bits of exact powers this module computes: `odd^n` up to 8192
/// bits.
const EXACT_BITS: u64 = 8192;

/// `base^n` exactly, when it has at most [`EXACT_BITS`] bits.
fn exact_integer_power(base: u64, n: u64) -> Option<Big> {
    let width = u64::from(64_u32.saturating_sub(base.leading_zeros()));
    if width.saturating_mul(n) > EXACT_BITS {
        return None;
    }
    let limbs = usize::try_from(EXACT_BITS / 64 + 2).unwrap_or(130);
    let mut result = Big::from_u64(1);
    let mut square = Big::from_u64(base);
    let mut n = n;
    while n > 0 {
        if n & 1 == 1 {
            result = result.mul(&square, limbs);
        }
        n >>= 1;
        if n > 0 {
            square = square.mul(&square, limbs);
        }
    }
    Some(result)
}

/// The integer `2^k`-th root of `n`, if `n` is a perfect `2^k`-th power.
fn perfect_root(n: u64, k: u32) -> Option<u64> {
    let mut value = n;
    for _ in 0..k {
        let root = value.isqrt();
        if root.checked_mul(root) != Some(value) {
            return None;
        }
        value = root;
    }
    Some(value)
}

/// `|x|^y` when it can be decided exactly: `y` an integer whose power
/// has at most [`EXACT_BITS`] bits (rounded exactly, or for a negative
/// `y` divided out with enough bits to decide), or a dyadic result of a
/// fractional `y` (a midpoint, such as `(t²)^1.5 = t³` with 54 bits, is
/// rounded to even). `None` when `|x|^y` is not dyadic, so Ziv's loop
/// decides it.
fn exact_power(ax: f64, y: f64) -> Option<f64> {
    let (ox, ex) = odd_parts(ax);
    let (oy, ey) = odd_parts(y);
    let ny = i64::try_from(oy).ok()?;
    let ny = if y < 0.0 { ny.saturating_neg() } else { ny };
    // y = ny × 2^ey with ny odd: x^y = ox^y × 2^(ex × y).
    let (base, n, scale) = if ey >= 0 {
        let n = ny.checked_mul(1_i64.checked_shl(u32::try_from(ey).ok()?)?)?;
        (ox, n, ex.checked_mul(n)?)
    } else {
        // y = ny / 2^k: dyadic only when ox is a perfect 2^k-th power and
        // 2^k divides ex × ny.
        let k = u32::try_from(ey.saturating_neg()).ok()?;
        if k > 6 {
            return None;
        }
        let root = perfect_root(ox, k)?;
        let scaled = ex.checked_mul(ny)?;
        if scaled.rem_euclid(1_i64 << k) != 0 {
            return None;
        }
        (root, ny, scaled >> k)
    };
    if base == 1 {
        // A power of two.
        return Big::from_u64(1).shl(scale).round_f64(i64::MAX);
    }
    if n > 0 {
        let power = exact_integer_power(base, n.unsigned_abs())?;
        return power.shl(scale).round_f64(i64::MAX);
    }
    if ey < 0 {
        // An odd root of a power below one is never dyadic.
        return None;
    }
    // 1 / base^|n| × 2^scale: the distance to any midpoint is at least
    // 2^-(bits + 55) relative, so `bits + 128` bits decide it.
    let power = exact_integer_power(base, n.unsigned_abs())?;
    let limbs = power.limbs().saturating_add(4);
    let trusted = i64::try_from(limbs)
        .ok()?
        .saturating_mul(64)
        .saturating_sub(accurate::GUARD);
    power.recip(limbs).shl(scale).round_f64(trusted)
}

// ---------------------------------------------------------------------
// The kernels

/// `float::pow` (D-396).
extern "C" fn pow_kernel(x: f64, y: f64) -> f64 {
    pow(x, y)
}

/// `float::exp` (D-396).
extern "C" fn exp_kernel(x: f64) -> f64 {
    exp(x)
}

/// `float::ln` (D-396).
extern "C" fn ln_kernel(x: f64) -> f64 {
    ln(x)
}

/// `float::sin` (D-396).
extern "C" fn sin_kernel(x: f64) -> f64 {
    trig(Trig::Sin, x)
}

/// `float::cos` (D-396).
extern "C" fn cos_kernel(x: f64) -> f64 {
    trig(Trig::Cos, x)
}

/// `float::tan` (D-396).
extern "C" fn tan_kernel(x: f64) -> f64 {
    trig(Trig::Tan, x)
}

/// `float::atan2` (D-396).
extern "C" fn atan2_kernel(y: f64, x: f64) -> f64 {
    atan2(y, x)
}

/// Appends the D-396 kernels to `out`, once they have verified.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
pub(super) fn table(out: &mut Vec<Kernel>) {
    const ONE: &[&str] = &["f64"];
    const TWO: &[&str] = &["f64", "f64"];
    if !VERIFIED {
        return;
    }
    let entry = |name, address, params| Kernel {
        name,
        address,
        params,
        result: "f64",
    };
    out.extend([
        entry("cheby_float_pow", pow_kernel as *const u8, TWO),
        entry("cheby_float_exp", exp_kernel as *const u8, ONE),
        entry("cheby_float_ln", ln_kernel as *const u8, ONE),
        entry("cheby_float_sin", sin_kernel as *const u8, ONE),
        entry("cheby_float_cos", cos_kernel as *const u8, ONE),
        entry("cheby_float_tan", tan_kernel as *const u8, ONE),
        entry("cheby_float_atan2", atan2_kernel as *const u8, TWO),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_values_follow_c99_and_float_md() {
        assert_eq!(pow(2.0, 10.0), 1024.0);
        assert_eq!(pow(2.0, -1.0), 0.5);
        assert_eq!(pow(f64::NAN, 0.0), 1.0);
        assert_eq!(pow(1.0, f64::NAN), 1.0);
        assert!(pow(-8.0, 1.0 / 3.0).is_nan());
        assert_eq!(pow(-2.0, 3.0), -8.0);
        assert_eq!(pow(-0.0, 3.0).to_bits(), (-0.0_f64).to_bits());
        assert_eq!(pow(-0.0, std::f64::consts::PI).to_bits(), 0.0_f64.to_bits());
        assert_eq!(pow(-0.0, -3.0), f64::NEG_INFINITY);
        assert_eq!(pow(-1.0, f64::INFINITY), 1.0);
        assert_eq!(pow(0.5, f64::NEG_INFINITY), f64::INFINITY);
        assert_eq!(pow(f64::NEG_INFINITY, -3.0).to_bits(), (-0.0_f64).to_bits());
        assert_eq!(pow(9.0, 0.5), 3.0);
        assert_eq!(pow(2.0, -1075.0), 0.0);
        assert_eq!(pow(2.0, -1074.0), 5e-324);
        assert_eq!(pow(3.0, -2.0), 1.0 / 9.0);
        assert_eq!(exp(0.0), 1.0);
        assert_eq!(exp(1.0), std::f64::consts::E);
        assert_eq!(exp(f64::NEG_INFINITY), 0.0);
        assert_eq!(exp(1000.0), f64::INFINITY);
        assert_eq!(ln(1.0), 0.0);
        assert_eq!(ln(0.0), f64::NEG_INFINITY);
        assert!(ln(-1.0).is_nan());
        assert_eq!(trig(Trig::Sin, 0.0), 0.0);
        assert_eq!(trig(Trig::Sin, -0.0).to_bits(), (-0.0_f64).to_bits());
        assert_eq!(trig(Trig::Sin, std::f64::consts::FRAC_PI_2), 1.0);
        assert!(trig(Trig::Sin, f64::INFINITY).is_nan());
        assert_eq!(trig(Trig::Cos, 0.0), 1.0);
        assert_eq!(trig(Trig::Cos, std::f64::consts::PI), -1.0);
        assert_eq!(trig(Trig::Tan, 0.0), 0.0);
        assert!(trig(Trig::Tan, f64::NEG_INFINITY).is_nan());
        assert_eq!(atan2(1.0, 1.0), std::f64::consts::FRAC_PI_4);
        assert_eq!(atan2(0.0, -1.0), std::f64::consts::PI);
        assert_eq!(atan2(-1.0, 0.0), -std::f64::consts::FRAC_PI_2);
        assert_eq!(
            atan2(f64::INFINITY, f64::NEG_INFINITY),
            3.0 * std::f64::consts::FRAC_PI_4
        );
        assert_eq!(atan2(-0.0, -0.0), -std::f64::consts::PI);
    }

    #[test]
    fn exact_powers_round_once() {
        // t = 2^18 - 1 is odd and t³ lies in [2^53, 2^54): a midpoint,
        // rounded to even. pow(t², 1.5) is exactly t³.
        let t = f64::from((1_u32 << 18) - 1);
        let t3 = (t * t * t).to_bits();
        let exact = Big::from_f64(t)
            .mul(&Big::from_f64(t), 4)
            .mul(&Big::from_f64(t), 4);
        assert_eq!(
            pow(t * t, 1.5).to_bits(),
            exact.round_f64(i64::MAX).map_or(0, f64::to_bits)
        );
        assert_eq!(pow(t, 3.0).to_bits(), t3);
        // 0.75^2000 = 3^2000 × 2^-4000: 3170 significant bits, exact.
        let exact = exact_integer_power(3, 2000).and_then(|p| p.shl(-4000).round_f64(i64::MAX));
        assert_eq!(Some(pow(0.75, 2000.0)), exact);
        assert_eq!(pow(0.75, -3.0), 64.0 / 27.0);
    }

    /// Every D-396 kernel, called through the address the registry gives
    /// for its symbol, computes its own function.
    #[test]
    fn registered_kernels_call_their_functions() {
        type One = extern "C" fn(f64) -> f64;
        type Two = extern "C" fn(f64, f64) -> f64;
        let mut table = Vec::new();
        super::table(&mut table);
        assert_eq!(table.len(), 7);
        let address = |name: &str| {
            table
                .iter()
                .find(|kernel| kernel.name == name)
                .map(|kernel| kernel.address)
                .unwrap()
        };
        // SAFETY: the registry lists each kernel with its own C signature,
        // which `every_kernel_matches_its_std_declaration` checks.
        let one = |name: &str| unsafe { std::mem::transmute::<*const u8, One>(address(name)) };
        // SAFETY: as above.
        let two = |name: &str| unsafe { std::mem::transmute::<*const u8, Two>(address(name)) };
        for x in [0.5, 1.0, 2.5, -3.75, 1e-3, 700.0] {
            assert_eq!(one("cheby_float_exp")(x).to_bits(), exp(x).to_bits());
            assert_eq!(one("cheby_float_ln")(x).to_bits(), ln(x).to_bits());
            assert_eq!(
                one("cheby_float_sin")(x).to_bits(),
                trig(Trig::Sin, x).to_bits()
            );
            assert_eq!(
                one("cheby_float_cos")(x).to_bits(),
                trig(Trig::Cos, x).to_bits()
            );
            assert_eq!(
                one("cheby_float_tan")(x).to_bits(),
                trig(Trig::Tan, x).to_bits()
            );
            assert_eq!(
                two("cheby_float_pow")(x, 1.5).to_bits(),
                pow(x, 1.5).to_bits()
            );
            assert_eq!(
                two("cheby_float_atan2")(x, -2.0).to_bits(),
                atan2(x, -2.0).to_bits()
            );
        }
        assert_eq!(one("cheby_float_exp")(0.0), 1.0);
        assert_eq!(one("cheby_float_ln")(1.0), 0.0);
        assert_eq!(one("cheby_float_sin")(0.0), 0.0);
        assert_eq!(one("cheby_float_cos")(0.0), 1.0);
        assert_eq!(one("cheby_float_tan")(0.0), 0.0);
        assert_eq!(two("cheby_float_pow")(2.0, 10.0), 1024.0);
        assert_eq!(two("cheby_float_atan2")(0.0, 1.0), 0.0);
        // The direct calls as well, so each wrapper is exercised by name.
        assert_eq!(pow_kernel(3.0, 2.0), 9.0);
        assert_eq!(exp_kernel(0.0), 1.0);
        assert_eq!(ln_kernel(1.0), 0.0);
        assert_eq!(sin_kernel(0.0), 0.0);
        assert_eq!(cos_kernel(0.0), 1.0);
        assert_eq!(tan_kernel(0.0), 0.0);
        assert_eq!(atan2_kernel(0.0, 1.0), 0.0);
    }
}

#[cfg(test)]
mod timing {
    #![expect(
        clippy::as_conversions,
        clippy::cast_precision_loss,
        reason = "a timing loop"
    )]

    use std::hint::black_box;
    use std::time::Instant;

    use super::*;

    /// The average time per call of each function over typical inputs.
    /// `cargo test -p cheby_runtime --release num::math::timing -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement, not a check"]
    fn time_per_call() {
        type Case = (&'static str, fn(f64) -> f64);
        let n = 20_000;
        let inputs: Vec<f64> = (0..n).map(|i| 0.001 + f64::from(i) * 0.000_37).collect();
        let cases: [Case; 7] = [
            ("exp", exp),
            ("ln", ln),
            ("sin", |x| trig(Trig::Sin, x)),
            ("cos", |x| trig(Trig::Cos, x)),
            ("tan", |x| trig(Trig::Tan, x)),
            ("atan2", |x| atan2(x, 1.5)),
            ("pow", |x| pow(x, 2.7)),
        ];
        for (name, f) in cases {
            let start = Instant::now();
            let mut sum = 0.0;
            for &x in &inputs {
                sum += f(black_box(x));
            }
            black_box(sum);
            let each = start.elapsed().as_nanos() as f64 / f64::from(n);
            println!("{name}: {each:.0} ns per call");
        }
    }
}
