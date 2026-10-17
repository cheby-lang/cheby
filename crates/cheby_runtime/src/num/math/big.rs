//! Binary floating point of arbitrary precision: the accurate phase of
//! Ziv's strategy (D-568). A [`Big`] is `±m × 2^exp` with an integer
//! significand `m` of 64-bit limbs, normalized so that its most
//! significant limb has its top bit set. Every operation takes the
//! precision of its result in limbs and truncates toward zero, so the
//! relative error of one operation is below `2^(1 - 64 × prec)`; `add`
//! and `sub` are exact when the operands' bits fit in `prec + 2` limbs
//! above the lower one's last bit.
//!
//! [`Big::round_f64`] rounds to binary64, to nearest with ties to even,
//! subnormals and overflow included, and refuses (Ziv's rounding test)
//! when the bits it can trust do not decide the rounding.
//!
//! Integer arithmetic on limbs uses explicit wrapping, carrying and
//! checked operations; nothing here can panic.

use std::cmp::Ordering;

/// Bits per limb.
const LIMB_BITS: i64 = 64;

/// A binary floating-point number of arbitrary precision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Big {
    negative: bool,
    /// The weight of the least significant bit of `mant`: the value is
    /// `±mant × 2^exp`.
    exp: i64,
    /// The significand's limbs, least significant first. The last limb
    /// has its top bit set. Empty for zero.
    mant: Vec<u64>,
}

/// `n` limbs as a bit count.
fn bits(limbs: usize) -> i64 {
    i64::try_from(limbs)
        .unwrap_or(i64::MAX / 128)
        .saturating_mul(LIMB_BITS)
}

/// 2^k as a binary64, 0 or infinity outside the range.
pub(super) fn pow2(k: i64) -> f64 {
    if k > 1023 {
        f64::INFINITY
    } else if k >= -1022 {
        f64::from_bits(u64::try_from(k.saturating_add(1023)).unwrap_or(0) << 52)
    } else if k >= -1074 {
        f64::from_bits(1_u64 << u32::try_from(k.saturating_add(1074)).unwrap_or(0))
    } else {
        0.0
    }
}

/// Bits `start .. start + 64` of the integer whose limbs are `mant`, zero
/// outside it.
fn window(mant: &[u64], start: i64) -> u64 {
    let limb = |index: i64| -> u64 {
        usize::try_from(index)
            .ok()
            .and_then(|i| mant.get(i))
            .copied()
            .unwrap_or(0)
    };
    let quotient = start.div_euclid(LIMB_BITS);
    let shift = u32::try_from(start.rem_euclid(LIMB_BITS)).unwrap_or(0);
    let low = limb(quotient);
    if shift == 0 {
        return low;
    }
    let high = limb(quotient.saturating_add(1));
    (low >> shift) | (high << (64_u32.wrapping_sub(shift)))
}

/// Compares two integers of equal limb counts.
fn cmp_limbs(a: &[u64], b: &[u64]) -> Ordering {
    a.iter().rev().cmp(b.iter().rev())
}

/// `a += b` on equal limb counts; the carry out of the top is dropped (the
/// callers leave a spare top limb).
fn add_limbs(a: &mut [u64], b: &[u64]) {
    let mut carry = false;
    for (x, &y) in a.iter_mut().zip(b) {
        let (sum, c1) = x.overflowing_add(y);
        let (sum, c2) = sum.overflowing_add(u64::from(carry));
        *x = sum;
        carry = c1 || c2;
    }
}

/// `a -= b` on equal limb counts, with `a >= b`.
fn sub_limbs(a: &mut [u64], b: &[u64]) {
    let mut borrow = false;
    for (x, &y) in a.iter_mut().zip(b) {
        let (diff, b1) = x.overflowing_sub(y);
        let (diff, b2) = diff.overflowing_sub(u64::from(borrow));
        *x = diff;
        borrow = b1 || b2;
    }
}

impl Big {
    /// Zero.
    pub(super) const fn zero() -> Self {
        Self {
            negative: false,
            exp: 0,
            mant: Vec::new(),
        }
    }

    /// `±limbs × 2^exp`, normalized and truncated to `prec` limbs.
    fn from_parts(negative: bool, exp: i64, mut mant: Vec<u64>, prec: usize) -> Self {
        while mant.last() == Some(&0) {
            mant.pop();
        }
        let Some(&top) = mant.last() else {
            return Self::zero();
        };
        let shift = top.leading_zeros();
        if shift > 0 {
            let back = 64_u32.wrapping_sub(shift);
            let mut carry = 0_u64;
            for limb in &mut mant {
                let next = *limb >> back;
                *limb = (*limb << shift) | carry;
                carry = next;
            }
        }
        let mut exp = exp.saturating_sub(i64::from(shift));
        let prec = prec.max(1);
        if mant.len() > prec {
            let dropped = mant.len().wrapping_sub(prec);
            mant.drain(..dropped);
            exp = exp.saturating_add(bits(dropped));
        }
        Self {
            negative,
            exp,
            mant,
        }
    }

    /// The integer `m`.
    pub(super) fn from_u64(m: u64) -> Self {
        Self::from_parts(false, 0, vec![m], 1)
    }

    /// The integer `n`.
    pub(super) fn from_i64(n: i64) -> Self {
        Self::from_parts(n < 0, 0, vec![n.unsigned_abs()], 1)
    }

    /// A finite binary64, exactly. Infinities and NaN give zero; callers
    /// handle them first.
    pub(super) fn from_f64(x: f64) -> Self {
        if !x.is_finite() {
            return Self::zero();
        }
        let raw = x.to_bits();
        let field = (raw >> 52) & 0x7ff;
        let fraction = raw & ((1 << 52) - 1);
        let (m, e) = if field == 0 {
            (fraction, -1074)
        } else {
            (
                fraction | (1 << 52),
                i64::try_from(field).unwrap_or(0).wrapping_sub(1075),
            )
        };
        Self::from_parts(x.is_sign_negative(), e, vec![m], 1)
    }

    /// `limbs × 2^exp` for the significand's limbs, least significant
    /// first, truncated to `prec` limbs.
    pub(super) fn from_limbs(exp: i64, limbs: &[u64], prec: usize) -> Self {
        Self::from_parts(false, exp, limbs.to_vec(), prec)
    }

    /// The significand's limbs, least significant first, and the weight
    /// of the lowest bit.
    #[cfg(test)]
    pub(super) fn limbs_and_exp(&self) -> (&[u64], i64) {
        (&self.mant, self.exp)
    }

    pub(super) const fn is_zero(&self) -> bool {
        self.mant.is_empty()
    }

    pub(super) const fn is_negative(&self) -> bool {
        self.negative && !self.mant.is_empty()
    }

    /// The precision in limbs.
    pub(super) const fn limbs(&self) -> usize {
        self.mant.len()
    }

    /// The exponent of the most significant bit: a non-zero value's
    /// magnitude lies in `[2^top, 2^(top + 1))`. `i64::MIN` for zero.
    pub(super) fn top(&self) -> i64 {
        if self.is_zero() {
            return i64::MIN;
        }
        self.exp
            .saturating_add(bits(self.mant.len()))
            .saturating_sub(1)
    }

    /// `-self`.
    #[must_use]
    pub(super) fn neg(mut self) -> Self {
        self.negative = !self.negative;
        self
    }

    /// `|self|`.
    #[must_use]
    pub(super) fn abs(mut self) -> Self {
        self.negative = false;
        self
    }

    /// `self × 2^k`, exactly.
    #[must_use]
    pub(super) fn shl(mut self, k: i64) -> Self {
        if !self.is_zero() {
            self.exp = self.exp.saturating_add(k);
        }
        self
    }

    /// `self` truncated to `prec` limbs.
    #[must_use]
    pub(super) fn truncate(&self, prec: usize) -> Self {
        Self::from_parts(self.negative, self.exp, self.mant.clone(), prec)
    }

    /// `floor(|self| / 2^low)` in `len` limbs (the higher bits dropped).
    fn aligned(&self, low: i64, len: usize) -> Vec<u64> {
        let shift = self.exp.saturating_sub(low);
        (0..len)
            .map(|i| window(&self.mant, bits(i).saturating_sub(shift)))
            .collect()
    }

    /// Compares magnitudes.
    pub(super) fn cmp_abs(&self, other: &Self) -> Ordering {
        match (self.is_zero(), other.is_zero()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => {}
        }
        match self.top().cmp(&other.top()) {
            Ordering::Equal => {}
            unequal => return unequal,
        }
        let low = self.exp.min(other.exp);
        let len = self.mant.len().max(other.mant.len()).saturating_add(1);
        cmp_limbs(&self.aligned(low, len), &other.aligned(low, len))
    }

    /// `self + other` to `prec` limbs.
    #[must_use]
    pub(super) fn add(&self, other: &Self, prec: usize) -> Self {
        if self.is_zero() {
            return other.truncate(prec);
        }
        if other.is_zero() {
            return self.truncate(prec);
        }
        let high = self.top().max(other.top());
        let floor = high.saturating_sub(bits(prec.saturating_add(2)));
        let low = self.exp.min(other.exp).max(floor);
        let span = high.saturating_sub(low).saturating_add(1);
        let len = usize::try_from(span.div_euclid(LIMB_BITS))
            .unwrap_or(0)
            .saturating_add(2);
        let mut a = self.aligned(low, len);
        let mut b = other.aligned(low, len);
        if self.negative == other.negative {
            add_limbs(&mut a, &b);
            return Self::from_parts(self.negative, low, a, prec);
        }
        match cmp_limbs(&a, &b) {
            Ordering::Equal => Self::zero(),
            Ordering::Greater => {
                sub_limbs(&mut a, &b);
                Self::from_parts(self.negative, low, a, prec)
            }
            Ordering::Less => {
                sub_limbs(&mut b, &a);
                Self::from_parts(other.negative, low, b, prec)
            }
        }
    }

    /// `self - other` to `prec` limbs.
    #[must_use]
    pub(super) fn sub(&self, other: &Self, prec: usize) -> Self {
        self.add(&other.clone().neg(), prec)
    }

    /// `self × other` to `prec` limbs.
    #[must_use]
    pub(super) fn mul(&self, other: &Self, prec: usize) -> Self {
        if self.is_zero() || other.is_zero() {
            return Self::zero();
        }
        let a = self.truncate(prec.saturating_add(1));
        let b = other.truncate(prec.saturating_add(1));
        let mut out = vec![0_u64; a.mant.len().saturating_add(b.mant.len())];
        for (i, &x) in a.mant.iter().enumerate() {
            let mut carry = 0_u128;
            for (slot, &y) in out.iter_mut().skip(i).zip(&b.mant) {
                let t = u128::from(x)
                    .wrapping_mul(u128::from(y))
                    .wrapping_add(u128::from(*slot))
                    .wrapping_add(carry);
                *slot = low_half(t);
                carry = t >> 64;
            }
            if let Some(slot) = out.get_mut(i.saturating_add(b.mant.len())) {
                *slot = low_half(carry);
            }
        }
        Self::from_parts(
            self.negative != other.negative,
            a.exp.saturating_add(b.exp),
            out,
            prec,
        )
    }

    /// `self × k` to `prec` limbs.
    #[must_use]
    pub(super) fn mul_u64(&self, k: u64, prec: usize) -> Self {
        self.mul(&Self::from_u64(k), prec)
    }

    /// `self / k` to `prec` limbs, `k > 0`.
    #[must_use]
    pub(super) fn div_u64(&self, k: u64, prec: usize) -> Self {
        if self.is_zero() || k == 0 {
            return Self::zero();
        }
        let extra = prec.saturating_add(1);
        let mut limbs = vec![0_u64; extra];
        limbs.extend_from_slice(&self.mant);
        let divisor = u128::from(k);
        let mut rem = 0_u128;
        for limb in limbs.iter_mut().rev() {
            let current = (rem << 64) | u128::from(*limb);
            *limb = low_half(current.checked_div(divisor).unwrap_or(0));
            rem = current.checked_rem(divisor).unwrap_or(0);
        }
        Self::from_parts(
            self.negative,
            self.exp.saturating_sub(bits(extra)),
            limbs,
            prec,
        )
    }

    /// An approximation of the value as a binary64 (its first 53 bits,
    /// truncated); 0 or an infinity outside the range.
    pub(super) fn to_f64(&self) -> f64 {
        let Some(&top_limb) = self.mant.last() else {
            return 0.0;
        };
        let fraction = f64::from_bits((1023_u64 << 52) | ((top_limb >> 11) & ((1 << 52) - 1)));
        let top = self.top();
        let half = top.div_euclid(2);
        let value = fraction * pow2(half) * pow2(top.saturating_sub(half));
        if self.negative { -value } else { value }
    }

    /// `1 / self` to `prec` limbs, by Newton's iteration from a binary64
    /// estimate; zero for zero.
    #[must_use]
    pub(super) fn recip(&self, prec: usize) -> Self {
        if self.is_zero() {
            return Self::zero();
        }
        let work = prec.saturating_add(1);
        let top = self.top();
        // The significand in [1, 2).
        let m = self.clone().abs().shl(top.saturating_neg());
        let one = Self::from_u64(1);
        let mut x = Self::from_f64(1.0 / m.to_f64());
        let mut correct = 50_i64;
        while correct < bits(work).saturating_add(64) {
            let e = one.sub(&m.mul(&x, work), work);
            x = x.add(&x.mul(&e, work), work);
            correct = correct.saturating_mul(2);
        }
        let x = x.truncate(prec).shl(top.saturating_neg());
        if self.negative { x.neg() } else { x }
    }

    /// `self / other` to `prec` limbs.
    #[must_use]
    pub(super) fn div(&self, other: &Self, prec: usize) -> Self {
        let work = prec.saturating_add(1);
        self.mul(&other.recip(work), prec)
    }

    /// `√self` to `prec` limbs, for `self >= 0`, by Newton's iteration for
    /// `1/√self`; zero for zero or a negative value.
    #[must_use]
    pub(super) fn sqrt(&self, prec: usize) -> Self {
        if self.is_zero() || self.negative {
            return Self::zero();
        }
        let work = prec.saturating_add(1);
        let half = self.top().div_euclid(2);
        // The significand in [1, 4).
        let m = self.clone().shl(half.saturating_mul(-2));
        let one = Self::from_u64(1);
        let mut r = Self::from_f64(1.0 / m.to_f64().sqrt());
        let mut correct = 50_i64;
        while correct < bits(work).saturating_add(64) {
            let e = one.sub(&m.mul(&r.mul(&r, work), work), work);
            r = r.add(&r.mul(&e, work).shl(-1), work);
            correct = correct.saturating_mul(2);
        }
        m.mul(&r, prec).shl(half)
    }

    /// The integer nearest to `self` (half away from zero), exactly.
    #[must_use]
    pub(super) fn round_to_integer(&self) -> Self {
        if self.is_zero() || self.exp >= 0 {
            return self.clone();
        }
        let top = self.top();
        if top < -1 {
            return Self::zero();
        }
        let len = usize::try_from(top.div_euclid(LIMB_BITS))
            .unwrap_or(0)
            .saturating_add(2);
        let mut whole = self.aligned(0, len);
        let half = window(&self.mant, self.exp.saturating_neg().saturating_sub(1)) & 1;
        let mut one = vec![0_u64; len];
        if let Some(first) = one.first_mut() {
            *first = half;
        }
        add_limbs(&mut whole, &one);
        Self::from_parts(self.negative, 0, whole, len)
    }

    /// For an integer value, the value modulo 4.
    pub(super) fn integer_mod_4(&self) -> u32 {
        let magnitude = self.aligned(0, 1).first().copied().unwrap_or(0) & 3;
        let magnitude = u32::try_from(magnitude).unwrap_or(0);
        if self.is_negative() {
            4_u32.wrapping_sub(magnitude) & 3
        } else {
            magnitude
        }
    }

    /// Bit `i` of the significand, counted from its most significant bit
    /// (0); bits outside it are 0.
    fn bit(&self, i: i64) -> bool {
        let position = bits(self.mant.len()).saturating_sub(1).saturating_sub(i);
        if position < 0 || i < 0 {
            return false;
        }
        window(&self.mant, position) & 1 == 1
    }

    /// Rounds to binary64, to nearest with ties to even, when the first
    /// `trusted` bits of the significand decide it: the value must be
    /// within `2^(top - trusted)` of the exact one. `None` when the exact
    /// value may lie on the other side of a midpoint (Ziv's rounding
    /// test). With `trusted = i64::MAX` the value is taken as exact.
    pub(super) fn round_f64(&self, trusted: i64) -> Option<f64> {
        if self.is_zero() {
            return Some(if self.negative { -0.0 } else { 0.0 });
        }
        let top = self.top();
        let signed = |x: f64| if self.negative { -x } else { x };
        if top >= 1024 {
            return Some(signed(f64::INFINITY));
        }
        // The weight of the last kept bit: 52 bits below the top, or the
        // smallest subnormal's.
        let ulp_exp = top.saturating_sub(52).max(-1074);
        let kept = top.saturating_sub(ulp_exp).saturating_add(1);
        let mut q = 0_u64;
        for i in 0..kept.max(0) {
            q = (q << 1) | u64::from(self.bit(i));
        }
        let round_bit = self.bit(kept);
        if trusted != i64::MAX {
            let end = trusted.min(bits(self.mant.len()));
            let decided = (kept.saturating_add(1)..end).any(|j| self.bit(j) == round_bit);
            if !decided {
                return None;
            }
        }
        let sticky = (kept.saturating_add(1)..bits(self.mant.len())).any(|j| self.bit(j));
        if round_bit && (sticky || q & 1 == 1) {
            q = q.wrapping_add(1);
        }
        Some(signed(compose(q, ulp_exp)))
    }
}

/// The low 64 bits of a 128-bit value.
const fn low_half(x: u128) -> u64 {
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "keeping the low half is the point"
    )]
    {
        x as u64
    }
}

/// `q × 2^ulp_exp` as a binary64, where `q` has at most 53 bits, or 2^53
/// after a carry; an infinity beyond the range.
fn compose(mut q: u64, mut ulp_exp: i64) -> f64 {
    if q == 1 << 53 {
        q = 1 << 52;
        ulp_exp = ulp_exp.saturating_add(1);
    }
    if q == 0 {
        return 0.0;
    }
    if q < 1 << 52 {
        // A subnormal: `ulp_exp` is -1074.
        return f64::from_bits(q);
    }
    let biased = ulp_exp.saturating_add(52 + 1023);
    if biased > 2046 {
        return f64::INFINITY;
    }
    let biased = u64::try_from(biased).unwrap_or(0);
    f64::from_bits((biased << 52) | (q & ((1 << 52) - 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact(b: &Big) -> f64 {
        b.round_f64(i64::MAX).unwrap()
    }

    #[test]
    fn binary64_values_round_trip() {
        let mut state = 0x5eed_u64;
        let mut values = vec![1.0, 5e-324, f64::MIN_POSITIVE, f64::MAX, 3.0, 0.1, -2.5];
        // Fewer under Miri, which interprets every step (D-721).
        for _ in 0..if cfg!(miri) { 100 } else { 20_000 } {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            values.push(f64::from_bits(state));
        }
        for x in values.into_iter().filter(|x| x.is_finite()) {
            let b = Big::from_f64(x);
            assert_eq!(exact(&b).to_bits(), x.to_bits(), "{x:e}");
            assert_eq!(b.round_f64(200).map(f64::to_bits), Some(x.to_bits()));
            if x != 0.0 {
                assert_eq!(b.to_f64().to_bits(), x.to_bits(), "{x:e}");
                let a = x.abs();
                let next = a.next_up();
                if next.is_finite() {
                    // The midpoint rounds to even, and is never decided
                    // when it is not known to be exact.
                    let mid = Big::from_f64(a).add(&Big::from_f64(next), 4).shl(-1);
                    let even = if a.to_bits() & 1 == 0 { a } else { next };
                    assert_eq!(exact(&mid).to_bits(), even.to_bits(), "tie above {a:e}");
                    assert_eq!(mid.round_f64(200), None);
                }
            }
        }
    }

    #[test]
    fn arithmetic_is_exact_on_small_values() {
        let p = 4;
        let a = Big::from_f64(1.5);
        let b = Big::from_f64(-0.25);
        assert_eq!(exact(&a.add(&b, p)), 1.25);
        assert_eq!(exact(&a.sub(&b, p)), 1.75);
        assert_eq!(exact(&a.mul(&b, p)), -0.375);
        assert_eq!(exact(&a.sub(&a, p)), 0.0);
        assert_eq!(exact(&Big::from_f64(3.0).div_u64(4, p)), 0.75);
        assert_eq!(
            exact(&Big::from_f64(6.0).div(&Big::from_f64(-4.0), p)),
            -1.5
        );
        assert_eq!(exact(&Big::from_f64(2.25).sqrt(p)), 1.5);
        assert_eq!(
            exact(&Big::from_f64(1e300).mul(&Big::from_f64(1e300), p)),
            f64::INFINITY
        );
        assert_eq!(exact(&Big::from_f64(5e-324).shl(-1)), 0.0);
        assert_eq!(exact(&Big::from_f64(5e-324).mul_u64(3, p).shl(-1)), 1e-323);
        assert_eq!(exact(&Big::from_f64(2.5).round_to_integer()), 3.0);
        assert_eq!(exact(&Big::from_f64(-2.4).round_to_integer()), -2.0);
        assert_eq!(exact(&Big::from_f64(0.4).round_to_integer()), 0.0);
        assert_eq!(Big::from_f64(-7.0).integer_mod_4(), 1);
        assert_eq!(Big::from_f64(1e20).integer_mod_4(), 0);
        assert_eq!(Big::from_f64(1e20 + 65536.0 * 3.0).integer_mod_4(), 0);
        assert_eq!(
            Big::from_f64(2.0f64.powi(60) + 3.0 * 256.0).integer_mod_4(),
            0
        );
        assert_eq!(Big::from_i64(-1).integer_mod_4(), 3);
    }

    #[test]
    fn division_and_roots_agree_with_binary64() {
        let mut state = 0xabc_u64;
        // Fewer under Miri, which interprets every step (D-721).
        for _ in 0..if cfg!(miri) { 20 } else { 2_000 } {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let lhs = f64::from_bits(state >> 2);
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let rhs = f64::from_bits(state >> 2);
            if !(lhs.is_finite() && rhs.is_finite() && lhs != 0.0 && rhs != 0.0) {
                continue;
            }
            let prec = 3;
            let quotient = Big::from_f64(lhs).div(&Big::from_f64(rhs), prec);
            assert_eq!(
                quotient.round_f64(150).map(f64::to_bits),
                Some((lhs / rhs).to_bits()),
                "{lhs:e} / {rhs:e}"
            );
            let root = Big::from_f64(lhs.abs()).sqrt(prec);
            assert_eq!(
                root.round_f64(150).map(f64::to_bits),
                Some(lhs.abs().sqrt().to_bits()),
                "sqrt {lhs:e}"
            );
            let product = Big::from_f64(lhs).mul(&Big::from_f64(rhs), prec);
            assert_eq!(
                exact(&product).to_bits(),
                (lhs * rhs).to_bits(),
                "{lhs:e} * {rhs:e}"
            );
            let difference = Big::from_f64(lhs).add(&Big::from_f64(-rhs), prec);
            assert_eq!(
                exact(&difference).to_bits(),
                (lhs - rhs).to_bits(),
                "{lhs:e} - {rhs:e}"
            );
        }
    }

    #[test]
    fn comparisons_order_magnitudes() {
        let a = Big::from_f64(-3.0);
        let b = Big::from_f64(2.0);
        assert_eq!(a.cmp_abs(&b), Ordering::Greater);
        assert_eq!(b.cmp_abs(&a), Ordering::Less);
        assert_eq!(b.cmp_abs(&Big::from_f64(2.0)), Ordering::Equal);
        assert_eq!(Big::zero().cmp_abs(&b), Ordering::Less);
        assert!(a.is_negative());
        assert_eq!(a.top(), 1);
    }
}
