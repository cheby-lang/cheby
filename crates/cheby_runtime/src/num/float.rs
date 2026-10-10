//! Kernels of `std::float` and `std::f32` (D-484), except the correctly
//! rounded functions of D-396 (see `num::math`).
//!
//! - Conversions into a float type round to the nearest value, ties to
//!   even, and `f32::from_float` gives an infinity beyond `F32`'s range
//!   (D-359). Rust's `as` casts between numeric types do exactly this, and
//!   the casts below that cannot be `From` conversions rely on it.
//! - `parse` and `show` follow D-313 and §3.3.6 ([`super::text`]).
//! - `truncate`, `round`, `floor` and `ceil` return `Result<Int, Nil>`, with
//!   `Err(Nil)` for NaN, the infinities and a result outside `Int`'s 64-bit
//!   range (D-132); `round` takes halfway cases away from zero (D-314).
//! - `float::sqrt` is IEEE 754's correctly rounded square root.

use super::{Kernel, result, text, text_arg};
use crate::abi::Word;
use crate::values;

/// 2^63, the first value past `Int`'s range.
const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;

/// A whole float as an `Int`, or `None` for NaN, the infinities and values
/// outside −2^63 to 2^63 − 1.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "the value is whole and inside i64's range, so the cast is exact"
)]
fn whole_to_int(whole: f64) -> Option<i64> {
    (whole.is_finite() && (-TWO_POW_63..TWO_POW_63).contains(&whole)).then_some(whole as i64)
}

/// The rounding of `truncate`, `round`, `floor` and `ceil`.
#[derive(Debug, Clone, Copy)]
enum Rounding {
    Truncate,
    /// Halfway cases away from zero (D-314), as Rust's `f64::round`.
    Round,
    Floor,
    Ceil,
}

/// `x` rounded to a whole number and returned as an `Int` (D-132).
fn round_to_int(x: f64, rounding: Rounding) -> Option<i64> {
    whole_to_int(match rounding {
        Rounding::Truncate => x.trunc(),
        Rounding::Round => x.round(),
        Rounding::Floor => x.floor(),
        Rounding::Ceil => x.ceil(),
    })
}

/// A conversion into a float type that rounds to nearest, ties to even,
/// and overflows to an infinity (D-359), which is what Rust's `as` casts
/// between these types do.
trait RoundTo<T> {
    fn round_to(self) -> T;
}

impl RoundTo<f64> for i64 {
    #[expect(
        clippy::as_conversions,
        clippy::cast_precision_loss,
        reason = "D-359: to nearest, ties to even, as `as` does"
    )]
    fn round_to(self) -> f64 {
        self as f64
    }
}

impl RoundTo<f64> for u64 {
    #[expect(
        clippy::as_conversions,
        clippy::cast_precision_loss,
        reason = "D-359: to nearest, ties to even, as `as` does"
    )]
    fn round_to(self) -> f64 {
        self as f64
    }
}

impl RoundTo<f32> for f64 {
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "D-359: to nearest, ties to even, an infinity beyond F32's range, as `as` does"
    )]
    fn round_to(self) -> f32 {
        self as f32
    }
}

macro_rules! round_to_f32 {
    ($($src:ty),*) => {
        $(impl RoundTo<f32> for $src {
            #[expect(
                clippy::as_conversions,
                clippy::cast_precision_loss,
                reason = "D-359: to nearest, ties to even, as `as` does"
            )]
            fn round_to(self) -> f32 {
                self as f32
            }
        })*
    };
}

round_to_f32!(i32, u32, i64, u64);

/// Defines the conversions into a float type: `lossless` through `From`,
/// `rounding` through an `as` cast, which rounds to nearest, ties to even,
/// and overflows to an infinity (D-359).
macro_rules! float_from {
    (
        $std:literal, $t:ident,
        lossless: [$($lsrc:ident => $lfn:ident),* $(,)?],
        rounding: [$($rsrc:ident => $rfn:ident),* $(,)?] $(,)?
    ) => {
        $(
            #[doc = concat!("`", $std, "::", stringify!($lfn), "`: exact (D-359).")]
            #[unsafe(export_name = concat!("cheby_", $std, "_", stringify!($lfn)))]
            pub(crate) extern "C" fn $lfn(n: $lsrc) -> $t {
                <$t>::from(n)
            }
        )*
        $(
            #[doc = concat!("`", $std, "::", stringify!($rfn), "`: to nearest, ties to even (D-359).")]
            #[unsafe(export_name = concat!("cheby_", $std, "_", stringify!($rfn)))]
            pub(crate) extern "C" fn $rfn(n: $rsrc) -> $t {
                RoundTo::<$t>::round_to(n)
            }
        )*

        /// Appends the conversions to `out`.
        fn conversions(out: &mut Vec<Kernel>) {
            $(out.push(Kernel {
                name: concat!("cheby_", $std, "_", stringify!($lfn)),
                address: $lfn as *const u8,
                params: &[stringify!($lsrc)],
                result: stringify!($t),
            });)*
            $(out.push(Kernel {
                name: concat!("cheby_", $std, "_", stringify!($rfn)),
                address: $rfn as *const u8,
                params: &[stringify!($rsrc)],
                result: stringify!($t),
            });)*
        }
    };
}

/// Defines `parse`, `show`, `truncate`, `round`, `floor` and `ceil` of a
/// float type and the `table` of its module.
macro_rules! float_family {
    ($std:literal, $t:ident, parse: $parse:path, show: $show:path $(,)?) => {
        #[doc = concat!("`", $std, "::parse` (D-313, D-394).")]
        ///
        /// # Safety
        ///
        /// `text` is a live `String`, borrowed for the call.
        #[unsafe(export_name = concat!("cheby_", $std, "_parse"))]
        pub(crate) unsafe extern "C" fn parse(text: Word) -> Word {
            // SAFETY: forwarded from the caller.
            let text = unsafe { text_arg(text) };
            result($parse(text))
        }

        #[doc = concat!("`", $std, "::show` (§3.3.6, D-219, D-395).")]
        #[unsafe(export_name = concat!("cheby_", $std, "_show"))]
        pub(crate) extern "C" fn show(x: $t) -> Word {
            values::string(&$show(x))
        }

        #[doc = concat!("`", $std, "::truncate`: toward zero (D-132).")]
        #[unsafe(export_name = concat!("cheby_", $std, "_truncate"))]
        pub(crate) extern "C" fn truncate(x: $t) -> Word {
            result(round_to_int(f64::from(x), Rounding::Truncate))
        }

        #[doc = concat!("`", $std, "::round`: halfway cases away from zero (D-314).")]
        #[unsafe(export_name = concat!("cheby_", $std, "_round"))]
        pub(crate) extern "C" fn round(x: $t) -> Word {
            result(round_to_int(f64::from(x), Rounding::Round))
        }

        #[doc = concat!("`", $std, "::floor` (D-132).")]
        #[unsafe(export_name = concat!("cheby_", $std, "_floor"))]
        pub(crate) extern "C" fn floor(x: $t) -> Word {
            result(round_to_int(f64::from(x), Rounding::Floor))
        }

        #[doc = concat!("`", $std, "::ceil` (D-132).")]
        #[unsafe(export_name = concat!("cheby_", $std, "_ceil"))]
        pub(crate) extern "C" fn ceil(x: $t) -> Word {
            result(round_to_int(f64::from(x), Rounding::Ceil))
        }

        /// Appends this module's kernels to `out`.
        #[expect(
            clippy::as_conversions,
            reason = "taking the address of a function needs an `as` cast"
        )]
        pub(super) fn table(out: &mut Vec<Kernel>) {
            conversions(out);
            let one: &'static [&'static str] = &[stringify!($t)];
            let entry = |name, address, params, result| Kernel {
                name,
                address,
                params,
                result,
            };
            out.extend([
                entry(
                    concat!("cheby_", $std, "_parse"),
                    parse as *const u8,
                    &["Word"],
                    "Word",
                ),
                entry(
                    concat!("cheby_", $std, "_show"),
                    show as *const u8,
                    one,
                    "Word",
                ),
                entry(
                    concat!("cheby_", $std, "_truncate"),
                    truncate as *const u8,
                    one,
                    "Word",
                ),
                entry(
                    concat!("cheby_", $std, "_round"),
                    round as *const u8,
                    one,
                    "Word",
                ),
                entry(
                    concat!("cheby_", $std, "_floor"),
                    floor as *const u8,
                    one,
                    "Word",
                ),
                entry(
                    concat!("cheby_", $std, "_ceil"),
                    ceil as *const u8,
                    one,
                    "Word",
                ),
            ]);
            extra(out);
        }
    };
}

/// `std::float`.
mod float_kernels {
    use super::{Kernel, RoundTo, Rounding, Word, result, round_to_int, text, text_arg, values};

    float_from! {
        "float", f64,
        lossless: [
            i8 => from_i8, i16 => from_i16, i32 => from_i32,
            u8 => from_u8, u16 => from_u16, u32 => from_u32, f32 => from_f32,
        ],
        rounding: [i64 => from_int, i64 => from_i64, u64 => from_u64],
    }

    float_family!("float", f64, parse: text::parse_float, show: text::float_to_string);

    /// `float::sqrt`: correctly rounded (IEEE 754), NaN for a negative `x`,
    /// and `-0.0` for `-0.0`.
    #[unsafe(export_name = "cheby_float_sqrt")]
    pub(crate) extern "C" fn sqrt(x: f64) -> f64 {
        x.sqrt()
    }

    #[expect(
        clippy::as_conversions,
        reason = "taking the address of a function needs an `as` cast"
    )]
    fn extra(out: &mut Vec<Kernel>) {
        out.push(Kernel {
            name: "cheby_float_sqrt",
            address: sqrt as *const u8,
            params: &["f64"],
            result: "f64",
        });
    }
}

/// `std::f32`.
mod f32_kernels {
    use super::{Kernel, RoundTo, Rounding, Word, result, round_to_int, text, text_arg, values};

    float_from! {
        "f32", f32,
        lossless: [i8 => from_i8, i16 => from_i16, u8 => from_u8, u16 => from_u16],
        rounding: [
            f64 => from_float, i64 => from_int, i32 => from_i32, i64 => from_i64,
            u32 => from_u32, u64 => from_u64,
        ],
    }

    float_family!("f32", f32, parse: text::parse_f32, show: text::f32_to_string);

    fn extra(_: &mut Vec<Kernel>) {}
}

/// Appends the float kernels to `out`.
pub(super) fn table(out: &mut Vec<Kernel>) {
    float_kernels::table(out);
    f32_kernels::table(out);
}

#[cfg(test)]
mod tests;
