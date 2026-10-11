//! The shape of numeric literals (§2.5.1, §2.5.2, D-084).
//!
//! The lexer and the token checks share [`scan_number`], so both agree on
//! where a number ends and what is wrong with it. The token rules mirror
//! `tree-sitter-cheby`, which is more permissive than the spec. What
//! tree-sitter rejects is a grammar error ([`NumberError`]); leading zeros
//! and `_` right after a base prefix are validation errors, reported by
//! [`super::checks`].

use crate::SyntaxKind;

/// The base of an integer literal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Base {
    Dec,
    Hex,
    Oct,
    Bin,
}

impl Base {
    pub(crate) const fn radix(self) -> u32 {
        match self {
            Self::Dec => 10,
            Self::Hex => 16,
            Self::Oct => 8,
            Self::Bin => 2,
        }
    }

    pub(crate) const fn prefix(self) -> &'static str {
        match self {
            Self::Dec => "",
            Self::Hex => "0x",
            Self::Oct => "0o",
            Self::Bin => "0b",
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Dec => "decimal",
            Self::Hex => "hexadecimal",
            Self::Oct => "octal",
            Self::Bin => "binary",
        }
    }

    const fn is_digit(self, b: u8) -> bool {
        match self {
            Self::Dec => b.is_ascii_digit(),
            Self::Hex => b.is_ascii_hexdigit(),
            Self::Oct => matches!(b, b'0'..=b'7'),
            Self::Bin => matches!(b, b'0' | b'1'),
        }
    }
}

/// A grammar error in a numeric literal. Offsets are relative to the start
/// of the token.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NumberError {
    /// A base prefix with nothing of its base after it, such as `0x` or
    /// `0b2` (E0008). A prefix followed only by `_` is a validation error.
    NoDigits,
    /// Digits that do not belong to the base, from `at` to the end of the
    /// token, such as the `2` of `0b102` (E0008).
    DigitOutOfBase { at: usize },
    /// Letters after the number, from `at` to the end of the token, such as
    /// the `u8` of `1u8` (E0009).
    Suffix { at: usize },
}

/// Where a number ends and what it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct NumberScan {
    /// The length of the token in bytes, including an absorbed suffix.
    pub(crate) len: usize,
    /// [`SyntaxKind::INT_NUMBER`] or [`SyntaxKind::FLOAT_NUMBER`].
    pub(crate) kind: SyntaxKind,
    pub(crate) base: Base,
    pub(crate) error: Option<NumberError>,
}

/// Scans the number at the start of `bytes`, which must start with an ASCII
/// digit. Otherwise the result is empty, with [`NumberError::NoDigits`].
///
/// Rules: `0x`, `0o` and `0b` followed by digits of the base and `_`; or
/// decimal digits and `_`, then optionally `.` and a digit-led run (a `.`
/// not followed by a digit is not consumed, so `1..2` and `x.0` work), then
/// optionally an exponent `e`/`E`, an optional sign and a digit-led run.
/// Any directly following run of ASCII letters, digits and `_` is absorbed
/// into the token as an error.
#[expect(
    clippy::arithmetic_side_effects,
    reason = "indices are bounded by `bytes.len()`, so `+ 1` and `+ 2` cannot overflow"
)]
pub(crate) fn scan_number(bytes: &[u8]) -> NumberScan {
    let at = |i: usize| bytes.get(i).copied();
    let run = |mut i: usize, pred: &dyn Fn(u8) -> bool| {
        while at(i).is_some_and(pred) {
            i += 1;
        }
        i
    };
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    if !at(0).is_some_and(|b| b.is_ascii_digit()) {
        return NumberScan {
            len: 0,
            kind: SyntaxKind::INT_NUMBER,
            base: Base::Dec,
            error: Some(NumberError::NoDigits),
        };
    }

    let base = match (at(0), at(1)) {
        (Some(b'0'), Some(b'x')) => Some(Base::Hex),
        (Some(b'0'), Some(b'o')) => Some(Base::Oct),
        (Some(b'0'), Some(b'b')) => Some(Base::Bin),
        _ => None,
    };
    if let Some(base) = base {
        let digits_end = run(2, &|b| base.is_digit(b) || b == b'_');
        let end = run(digits_end, &word);
        // Only underscores after the prefix (`0x_`) match tree-sitter's
        // `0x[0-9a-fA-F_]+`, so that case is a validation error.
        let error = if digits_end == 2 {
            Some(NumberError::NoDigits)
        } else if end == digits_end {
            None
        } else if at(digits_end).is_some_and(|b| b.is_ascii_digit()) {
            Some(NumberError::DigitOutOfBase { at: digits_end })
        } else {
            Some(NumberError::Suffix { at: digits_end })
        };
        return NumberScan {
            len: end,
            kind: SyntaxKind::INT_NUMBER,
            base,
            error,
        };
    }

    let digit_run = |i: usize| run(i, &|b| b.is_ascii_digit() || b == b'_');
    let mut i = digit_run(1);
    let mut kind = SyntaxKind::INT_NUMBER;
    if at(i) == Some(b'.') && at(i + 1).is_some_and(|b| b.is_ascii_digit()) {
        i = digit_run(i + 2);
        kind = SyntaxKind::FLOAT_NUMBER;
    }
    if matches!(at(i), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(at(i + 1), Some(b'+' | b'-')));
        let first = i + 1 + sign;
        if at(first).is_some_and(|b| b.is_ascii_digit()) {
            i = digit_run(first + 1);
            kind = SyntaxKind::FLOAT_NUMBER;
        }
    }
    let end = run(i, &word);
    let error = (end != i).then_some(NumberError::Suffix { at: i });
    NumberScan {
        len: end,
        kind,
        base: Base::Dec,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::{Base, NumberError, NumberScan, scan_number};
    use crate::SyntaxKind::{FLOAT_NUMBER, INT_NUMBER};

    fn scan(text: &str) -> NumberScan {
        scan_number(text.as_bytes())
    }

    #[test]
    fn integers() {
        for text in [
            "0", "7", "1_000", "1__", "007", "0x1F_ff", "0o17", "0b1010_1", "0x_1",
        ] {
            let s = scan(text);
            assert_eq!(
                (s.len, s.kind, s.error),
                (text.len(), INT_NUMBER, None),
                "{text}"
            );
        }
        assert_eq!(scan("0xff").base, Base::Hex);
        assert_eq!(scan("0o7").base, Base::Oct);
        assert_eq!(scan("0b1").base, Base::Bin);
        assert_eq!(scan("10").base, Base::Dec);
    }

    #[test]
    fn floats() {
        for text in ["1.5", "0.0", "1_0.2_5", "1e5", "1E+5", "2.5e-3", "1e5_0"] {
            let s = scan(text);
            assert_eq!(
                (s.len, s.kind, s.error),
                (text.len(), FLOAT_NUMBER, None),
                "{text}"
            );
        }
    }

    #[test]
    fn stops_before_dots() {
        assert_eq!(scan("1..2").len, 1);
        assert_eq!(scan("1.").len, 1);
        assert_eq!(scan("1.e5").len, 1);
        assert_eq!(scan("0.1.2").len, 3);
        assert_eq!(scan("1.5.x").len, 3);
        assert_eq!(scan("3 + 4").len, 1);
        assert_eq!(scan("1e+").len, 2);
    }

    #[test]
    fn errors() {
        assert_eq!(scan("0x").error, Some(NumberError::NoDigits));
        assert_eq!(scan("0b_").error, None);
        let s = scan("0b__2");
        assert_eq!(
            (s.len, s.error),
            (5, Some(NumberError::DigitOutOfBase { at: 4 }))
        );
        let s = scan("0xg");
        assert_eq!((s.len, s.error), (3, Some(NumberError::NoDigits)));
        let s = scan("0b102");
        assert_eq!(
            (s.len, s.error),
            (5, Some(NumberError::DigitOutOfBase { at: 4 }))
        );
        let s = scan("0o78");
        assert_eq!(
            (s.len, s.error),
            (4, Some(NumberError::DigitOutOfBase { at: 3 }))
        );
        let s = scan("1u8");
        assert_eq!((s.len, s.error), (3, Some(NumberError::Suffix { at: 1 })));
        let s = scan("2.5f");
        assert_eq!(
            (s.len, s.kind, s.error),
            (4, FLOAT_NUMBER, Some(NumberError::Suffix { at: 3 }))
        );
        let s = scan("1e");
        assert_eq!(
            (s.len, s.kind, s.error),
            (2, INT_NUMBER, Some(NumberError::Suffix { at: 1 }))
        );
        let s = scan("0xffz");
        assert_eq!((s.len, s.error), (5, Some(NumberError::Suffix { at: 4 })));
        let s = scan("x1");
        assert_eq!((s.len, s.error), (0, Some(NumberError::NoDigits)));
        let s = scan("1e_5");
        assert_eq!((s.len, s.error), (4, Some(NumberError::Suffix { at: 1 })));
    }
}
