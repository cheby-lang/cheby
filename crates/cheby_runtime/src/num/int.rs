//! Kernels of `std::int` and the sized integer modules (D-484).
//!
//! Each module is one family (`docs/stdlib/README.md`, D-350): the
//! conversions `from_<source>`, which return the target directly when every
//! source value fits and `Result<T, Nil>` otherwise (D-308), `parse`,
//! `parse_base` and `show` (D-312), `checked_add`, `checked_sub` and
//! `checked_mul` (D-309), and `wrapping_add`, `wrapping_sub`,
//! `wrapping_mul` and `wrapping_neg` (D-025). `std::int` adds `to_base`
//! (D-390), `checked_neg` and `checked_pow` (D-392).

use super::{Kernel, result, text, text_arg};
use crate::abi::Word;
use crate::unwind::guard::guarded;
use crate::values;

/// Defines one integer family: a module `$module` with the kernels of std
/// module `$std` for the Rust type `$t`, and its `table`.
///
/// `widen` lists the sources every value of which fits `$t` (the kernel
/// returns `$t`), `narrow` the others (the kernel returns `Result<$t,
/// Nil>`), each as `source type => kernel name`.
macro_rules! int_family {
    (
        $module:ident, $std:literal, $t:ident,
        widen: [$($wsrc:ident => $wfn:ident),* $(,)?],
        narrow: [$($nsrc:ident => $nfn:ident),* $(,)?] $(,)?
    ) => {
        mod $module {
            use super::{Kernel, Word, guarded, result, text, text_arg, values};

            $(
                #[doc = concat!("`", $std, "::", stringify!($wfn), "`: every value fits (D-308).")]
                #[unsafe(export_name = concat!("cheby_", $std, "_", stringify!($wfn)))]
                pub(crate) extern "C" fn $wfn(n: $wsrc) -> $t {
                    <$t>::from(n)
                }
            )*

            $(
                #[doc = concat!("`", $std, "::", stringify!($nfn), "`: `Err(Nil)` when `n` does not fit (D-308).")]
                #[unsafe(export_name = concat!("cheby_", $std, "_", stringify!($nfn)))]
                pub(crate) extern "C" fn $nfn(n: $nsrc) -> Word {
                    guarded(None, || result(<$t>::try_from(n).ok()))
                }
            )*

            #[doc = concat!("`", $std, "::parse`: an optional `-` and decimal digits (D-312).")]
            ///
            /// # Safety
            ///
            /// `text` is a live `String`, borrowed for the call.
            #[unsafe(export_name = concat!("cheby_", $std, "_parse"))]
            pub(crate) unsafe extern "C" fn parse(text: Word) -> Word {
                guarded(None, || {
                    // SAFETY: forwarded from the caller.
                    let text = unsafe { text_arg(text) };
                    result(text::parse_int::<$t>(text, 10))
                })
            }

            #[doc = concat!("`", $std, "::parse_base`: an optional `-` and digits of `base`, 2 to 36.")]
            ///
            /// # Safety
            ///
            /// `text` is a live `String`, borrowed for the call.
            #[unsafe(export_name = concat!("cheby_", $std, "_parse_base"))]
            pub(crate) unsafe extern "C" fn parse_base(text: Word, base: i64) -> Word {
                guarded(None, || {
                    // SAFETY: forwarded from the caller.
                    let text = unsafe { text_arg(text) };
                    result(text::parse_int::<$t>(text, base))
                })
            }

            #[doc = concat!("`", $std, "::show`: decimal digits, `-` when negative.")]
            #[unsafe(export_name = concat!("cheby_", $std, "_show"))]
            pub(crate) extern "C" fn show(n: $t) -> Word {
                guarded(None, || values::string(&n.to_string()))
            }

            #[doc = concat!("`", $std, "::checked_add` (D-309).")]
            #[unsafe(export_name = concat!("cheby_", $std, "_checked_add"))]
            pub(crate) extern "C" fn checked_add(a: $t, b: $t) -> Word {
                guarded(None, || result(a.checked_add(b)))
            }

            #[doc = concat!("`", $std, "::checked_sub` (D-309).")]
            #[unsafe(export_name = concat!("cheby_", $std, "_checked_sub"))]
            pub(crate) extern "C" fn checked_sub(a: $t, b: $t) -> Word {
                guarded(None, || result(a.checked_sub(b)))
            }

            #[doc = concat!("`", $std, "::checked_mul` (D-309).")]
            #[unsafe(export_name = concat!("cheby_", $std, "_checked_mul"))]
            pub(crate) extern "C" fn checked_mul(a: $t, b: $t) -> Word {
                guarded(None, || result(a.checked_mul(b)))
            }

            #[doc = concat!("`", $std, "::wrapping_add`: modulo 2^bits (D-025).")]
            #[unsafe(export_name = concat!("cheby_", $std, "_wrapping_add"))]
            pub(crate) extern "C" fn wrapping_add(a: $t, b: $t) -> $t {
                a.wrapping_add(b)
            }

            #[doc = concat!("`", $std, "::wrapping_sub`: modulo 2^bits (D-025).")]
            #[unsafe(export_name = concat!("cheby_", $std, "_wrapping_sub"))]
            pub(crate) extern "C" fn wrapping_sub(a: $t, b: $t) -> $t {
                a.wrapping_sub(b)
            }

            #[doc = concat!("`", $std, "::wrapping_mul`: modulo 2^bits (D-025).")]
            #[unsafe(export_name = concat!("cheby_", $std, "_wrapping_mul"))]
            pub(crate) extern "C" fn wrapping_mul(a: $t, b: $t) -> $t {
                a.wrapping_mul(b)
            }

            #[doc = concat!("`", $std, "::wrapping_neg`: modulo 2^bits (D-025).")]
            #[unsafe(export_name = concat!("cheby_", $std, "_wrapping_neg"))]
            pub(crate) extern "C" fn wrapping_neg(n: $t) -> $t {
                n.wrapping_neg()
            }

            /// Appends this family's kernels to `out`.
            #[expect(
                clippy::as_conversions,
                reason = "taking the address of a function needs an `as` cast"
            )]
            pub(super) fn table(out: &mut Vec<Kernel>) {
                let t = stringify!($t);
                let entry = |name, address, params, result| Kernel {
                    name,
                    address,
                    params,
                    result,
                };
                $(out.push(entry(
                    concat!("cheby_", $std, "_", stringify!($wfn)),
                    $wfn as *const u8,
                    &[stringify!($wsrc)],
                    t,
                ));)*
                $(out.push(entry(
                    concat!("cheby_", $std, "_", stringify!($nfn)),
                    $nfn as *const u8,
                    &[stringify!($nsrc)],
                    "Word",
                ));)*
                let pair: &'static [&'static str] = &[stringify!($t), stringify!($t)];
                let one: &'static [&'static str] = &[stringify!($t)];
                out.extend([
                    entry(concat!("cheby_", $std, "_parse"), parse as *const u8, &["Word"], "Word"),
                    entry(
                        concat!("cheby_", $std, "_parse_base"),
                        parse_base as *const u8,
                        &["Word", "i64"],
                        "Word",
                    ),
                    entry(concat!("cheby_", $std, "_show"), show as *const u8, one, "Word"),
                    entry(concat!("cheby_", $std, "_checked_add"), checked_add as *const u8, pair, "Word"),
                    entry(concat!("cheby_", $std, "_checked_sub"), checked_sub as *const u8, pair, "Word"),
                    entry(concat!("cheby_", $std, "_checked_mul"), checked_mul as *const u8, pair, "Word"),
                    entry(concat!("cheby_", $std, "_wrapping_add"), wrapping_add as *const u8, pair, t),
                    entry(concat!("cheby_", $std, "_wrapping_sub"), wrapping_sub as *const u8, pair, t),
                    entry(concat!("cheby_", $std, "_wrapping_mul"), wrapping_mul as *const u8, pair, t),
                    entry(concat!("cheby_", $std, "_wrapping_neg"), wrapping_neg as *const u8, one, t),
                ]);
            }

            /// Every kernel of the family against Rust's own conversions and
            /// `checked_`/`wrapping_` operations.
            #[cfg(test)]
            mod family_tests {
                use proptest::prelude::*;

                use crate::num::int::tests::{reference_parse, to_radix};
                use crate::num::tests::{no_leaks, take_result, take_string, with_text};

                proptest! {
                    $(
                        #[test]
                        fn $wfn(n: $wsrc) {
                            prop_assert_eq!(super::$wfn(n), <$t>::from(n));
                        }
                    )*

                    $(
                        #[test]
                        fn $nfn(n: $nsrc) {
                            let got = no_leaks(|| take_result::<$t>(super::$nfn(n)));
                            prop_assert_eq!(got, <$t>::try_from(n).ok());
                        }
                    )*

                    #[test]
                    fn checked_and_wrapping(a: $t, b: $t) {
                        no_leaks(|| -> Result<(), TestCaseError> {
                            prop_assert_eq!(take_result::<$t>(super::checked_add(a, b)), a.checked_add(b));
                            prop_assert_eq!(take_result::<$t>(super::checked_sub(a, b)), a.checked_sub(b));
                            prop_assert_eq!(take_result::<$t>(super::checked_mul(a, b)), a.checked_mul(b));
                            Ok(())
                        })?;
                        prop_assert_eq!(super::wrapping_add(a, b), a.wrapping_add(b));
                        prop_assert_eq!(super::wrapping_sub(a, b), a.wrapping_sub(b));
                        prop_assert_eq!(super::wrapping_mul(a, b), a.wrapping_mul(b));
                        prop_assert_eq!(super::wrapping_neg(a), a.wrapping_neg());
                    }

                    #[test]
                    fn show_and_parse_round_trip(n: $t, base in 2_i64..=36) {
                        no_leaks(|| -> Result<(), TestCaseError> {
                            let text = take_string(super::show(n));
                            prop_assert_eq!(&text, &n.to_string());
                            // SAFETY: `word` is the live string `with_text` made.
                            let back = with_text(&text, |word| unsafe { super::parse(word) });
                            prop_assert_eq!(take_result::<$t>(back), Some(n));
                            let digits = to_radix(i128::from(n), base);
                            // SAFETY: as above.
                            let back = with_text(&digits, |word| unsafe { super::parse_base(word, base) });
                            prop_assert_eq!(take_result::<$t>(back), Some(n));
                            let upper = digits.to_uppercase();
                            // SAFETY: as above.
                            let back = with_text(&upper, |word| unsafe { super::parse_base(word, base) });
                            prop_assert_eq!(take_result::<$t>(back), Some(n));
                            Ok(())
                        })?;
                    }

                    #[test]
                    fn parse_follows_d312(text in "[-+ _0-9a-fx]{0,24}") {
                        // SAFETY: `word` is the live string `with_text` made.
                        let got = no_leaks(|| take_result::<$t>(with_text(&text, |word| unsafe { super::parse(word) })));
                        prop_assert_eq!(got, reference_parse::<$t>(&text, 10));
                    }

                    #[test]
                    fn parse_base_follows_d312(text in "-?[0-9a-zA-Z]{1,70}", base in -1_i64..=40) {
                        // SAFETY: `word` is the live string `with_text` made.
                        let got = no_leaks(|| take_result::<$t>(with_text(&text, |word| unsafe { super::parse_base(word, base) })));
                        prop_assert_eq!(got, reference_parse::<$t>(&text, base));
                    }
                }
            }
        }
    };
}

int_family! {
    int_kernels, "int", i64,
    widen: [i8 => from_i8, i16 => from_i16, i32 => from_i32, u8 => from_u8, u16 => from_u16, u32 => from_u32],
    // `from_i64` is narrowing on every target because it can fail on JS
    // (ADR-0030); on native it is always `Ok` (D-357).
    narrow: [i64 => from_i64, u64 => from_u64],
}

int_family! {
    i8_kernels, "i8", i8,
    widen: [],
    narrow: [
        i64 => from_int, i16 => from_i16, i32 => from_i32, i64 => from_i64,
        u8 => from_u8, u16 => from_u16, u32 => from_u32, u64 => from_u64,
    ],
}

int_family! {
    i16_kernels, "i16", i16,
    widen: [i8 => from_i8, u8 => from_u8],
    narrow: [
        i64 => from_int, i32 => from_i32, i64 => from_i64,
        u16 => from_u16, u32 => from_u32, u64 => from_u64,
    ],
}

int_family! {
    i32_kernels, "i32", i32,
    widen: [i8 => from_i8, i16 => from_i16, u8 => from_u8, u16 => from_u16],
    narrow: [i64 => from_int, i64 => from_i64, u32 => from_u32, u64 => from_u64],
}

int_family! {
    i64_kernels, "i64", i64,
    widen: [
        i64 => from_int, i8 => from_i8, i16 => from_i16, i32 => from_i32,
        u8 => from_u8, u16 => from_u16, u32 => from_u32,
    ],
    narrow: [u64 => from_u64],
}

int_family! {
    u8_kernels, "u8", u8,
    widen: [],
    narrow: [
        i64 => from_int, i8 => from_i8, i16 => from_i16, i32 => from_i32,
        i64 => from_i64, u16 => from_u16, u32 => from_u32, u64 => from_u64,
    ],
}

int_family! {
    u16_kernels, "u16", u16,
    widen: [u8 => from_u8],
    narrow: [
        i64 => from_int, i8 => from_i8, i16 => from_i16, i32 => from_i32,
        i64 => from_i64, u32 => from_u32, u64 => from_u64,
    ],
}

int_family! {
    u32_kernels, "u32", u32,
    widen: [u8 => from_u8, u16 => from_u16],
    narrow: [
        i64 => from_int, i8 => from_i8, i16 => from_i16, i32 => from_i32,
        i64 => from_i64, u64 => from_u64,
    ],
}

int_family! {
    u64_kernels, "u64", u64,
    widen: [u8 => from_u8, u16 => from_u16, u32 => from_u32],
    narrow: [i64 => from_int, i8 => from_i8, i16 => from_i16, i32 => from_i32, i64 => from_i64],
}

/// `int::to_base`: `n` in `base`, 2 to 36, lowercase, or `Err(Nil)` for
/// another base (D-390).
#[unsafe(export_name = "cheby_int_to_base")]
pub(crate) extern "C" fn to_base(n: i64, base: i64) -> Word {
    guarded(None, || result(text::int_to_base(n, base)))
}

/// `int::checked_neg`: `Err(Nil)` for `min_value` (D-309).
#[unsafe(export_name = "cheby_int_checked_neg")]
pub(crate) extern "C" fn checked_neg(n: i64) -> Word {
    guarded(None, || result(n.checked_neg()))
}

/// `int::checked_pow`: `n` to the power `exponent`, or `Err(Nil)` when the
/// result does not fit. An exponent below 0 counts as 0, so the result is
/// `Ok(1)` (`@std_clamped`, D-298, D-392).
#[unsafe(export_name = "cheby_int_checked_pow")]
pub(crate) extern "C" fn checked_pow(n: i64, exponent: i64) -> Word {
    guarded(None, || result(int_pow(n, exponent)))
}

/// `n ^ max(exponent, 0)` when it fits an `Int`.
fn int_pow(n: i64, exponent: i64) -> Option<i64> {
    let exponent = exponent.max(0);
    if let Ok(exponent) = u32::try_from(exponent) {
        return n.checked_pow(exponent);
    }
    // An exponent beyond u32: only 0, 1 and -1 have powers that fit.
    match n {
        0 => Some(0),
        1 => Some(1),
        -1 => Some(if exponent & 1 == 0 { 1 } else { -1 }),
        _ => None,
    }
}

/// Appends the integer kernels to `out`.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
pub(super) fn table(out: &mut Vec<Kernel>) {
    int_kernels::table(out);
    out.extend([
        Kernel {
            name: "cheby_int_to_base",
            address: to_base as *const u8,
            params: &["i64", "i64"],
            result: "Word",
        },
        Kernel {
            name: "cheby_int_checked_neg",
            address: checked_neg as *const u8,
            params: &["i64"],
            result: "Word",
        },
        Kernel {
            name: "cheby_int_checked_pow",
            address: checked_pow as *const u8,
            params: &["i64", "i64"],
            result: "Word",
        },
    ]);
    i8_kernels::table(out);
    i16_kernels::table(out);
    i32_kernels::table(out);
    i64_kernels::table(out);
    u8_kernels::table(out);
    u16_kernels::table(out);
    u32_kernels::table(out);
    u64_kernels::table(out);
}

#[cfg(test)]
mod tests;
