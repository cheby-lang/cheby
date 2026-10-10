//! The correctly rounded functions of D-396: `float::pow`, `exp`, `ln`,
//! `sin`, `cos`, `tan` and `atan2` must return the `Float` nearest to the
//! exact value on every target, computed by code shipped with the runtime
//! (ADR-0054).
//!
//! The implementation must be verified before it is registered (D-553):
//! checked against reference values computed at 256 bits or more and
//! rounded to binary64, with a rounding-boundary check, on at least 10,000
//! random inputs per function, the special values, and CORE-MATH's
//! published hard cases.
//!
//! **Status: no implementation verifies, so none is registered.** The
//! evaluation of 2026-10-10 tested the pure-Rust CORE-MATH port `pxfm`
//! 0.1.30 (also its git head, `acf7acd`): it is not correctly rounded on
//! CORE-MATH's hard cases for any of the seven functions, gets the sign of
//! zeros and infinities wrong in `pow` and `atan2`, returns grossly wrong
//! subnormal results from `exp` on some inputs, and gives NaN for
//! `pow(1.0, NaN)` and `pow(NaN, 0.0)`, where `std::float` requires 1.0.
//! The fallback is the open question after D-553 (plan §11 Q3). Until it
//! is chosen, `std::float` is not in [`super::COMPLETE`], and the seven
//! symbols are unresolved native symbols (E0053 at code generation).
//!
//! The reference (`reference`) and the evaluation (`eval`) are test modules
//! of this one, so a future implementation is checked the same way:
//!
//! ```text
//! cargo test -p cheby_runtime --release num::math -- --ignored --nocapture
//! ```

use super::Kernel;

#[cfg(test)]
mod eval;
#[cfg(test)]
mod reference;

/// Whether an implementation of the seven functions has verified and is
/// registered by [`table`].
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "read by the registry test; `table` follows it")
)]
pub(crate) const VERIFIED: bool = false;

/// Appends the D-396 kernels to `out`: none while no implementation
/// verifies.
pub(super) fn table(out: &mut Vec<Kernel>) {
    let _ = out;
}
