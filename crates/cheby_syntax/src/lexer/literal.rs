//! Values of literal tokens (§2.5), for the typed views and later phases.
//!
//! These functions take the token text as the lexer produced it. They never
//! fail loudly: a malformed literal, already reported as a diagnostic, gives
//! `None` or a best-effort value.

use std::borrow::Cow;

use super::number::{Base, scan_number};
use crate::SyntaxKind;

/// The value of an integer literal such as `1_000`, `0xFF`, `0o17` or
/// `0b1010` (§2.5.1). `None` if the text is not a well-formed integer
/// literal or the value exceeds `u128::MAX`. Leading zeros, which are a
/// validation error, do not change the value.
#[must_use]
pub fn int_value(text: &str) -> Option<u128> {
    let scan = scan_number(text.as_bytes());
    if scan.error.is_some() || scan.len != text.len() || scan.kind != SyntaxKind::INT_NUMBER {
        return None;
    }
    let digits = text.get(scan.base.prefix().len()..)?;
    if digits.bytes().all(|b| b == b'_') {
        return None;
    }
    let radix = u128::from(scan.base.radix());
    digits
        .chars()
        .filter(|&c| c != '_')
        .try_fold(0_u128, |acc, c| {
            let digit = u128::from(c.to_digit(scan.base.radix())?);
            acc.checked_mul(radix)?.checked_add(digit)
        })
}

/// The value of a float literal such as `1.5e3` (§2.5.2), or of a decimal
/// integer literal used as a float. `None` if the text is not well formed.
/// Out-of-range values round to infinity, as Rust's parser does; range
/// checks belong to type checking.
#[must_use]
pub fn float_value(text: &str) -> Option<f64> {
    let scan = scan_number(text.as_bytes());
    if scan.error.is_some() || scan.len != text.len() || scan.base != Base::Dec {
        return None;
    }
    let cleaned: String = text.chars().filter(|&c| c != '_').collect();
    cleaned.parse().ok()
}

/// Why an escape sequence is invalid (D-114).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EscapeError {
    /// `\` followed by a character that starts no escape.
    Unknown(char),
    /// A `\` at the end of the input.
    Incomplete,
    /// `\u` without `{…}`.
    UnicodeWithoutBraces,
    /// `\u{}` or more than six hex digits.
    UnicodeDigitCount,
    /// `\u{…}` naming a surrogate or a value above `U+10FFFF`.
    NotScalarValue(u32),
}

/// The character an escape token such as `\n` or `\u{1F600}` stands for:
/// `\n \t \r \\ \" \{` and `\u{…}` with 1 to 6 hex digits naming a Unicode
/// scalar value (§2.5.3, D-114).
///
/// # Errors
///
/// Returns why the escape is invalid.
pub fn escape_value(text: &str) -> Result<char, EscapeError> {
    let rest = text.strip_prefix('\\').ok_or(EscapeError::Incomplete)?;
    let mut chars = rest.chars();
    let first = chars.next().ok_or(EscapeError::Incomplete)?;
    let simple = match first {
        'n' => Some('\n'),
        't' => Some('\t'),
        'r' => Some('\r'),
        '\\' => Some('\\'),
        '"' => Some('"'),
        '{' => Some('{'),
        _ => None,
    };
    if let Some(c) = simple {
        return Ok(c);
    }
    if first != 'u' {
        return Err(EscapeError::Unknown(first));
    }
    let hex = rest
        .strip_prefix("u{")
        .and_then(|r| r.strip_suffix('}'))
        .ok_or(EscapeError::UnicodeWithoutBraces)?;
    if hex.is_empty() || hex.len() > 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(EscapeError::UnicodeDigitCount);
    }
    let value = u32::from_str_radix(hex, 16).map_err(|_| EscapeError::UnicodeDigitCount)?;
    char::from_u32(value).ok_or(EscapeError::NotScalarValue(value))
}

/// The value of a string literal from its parts: the `STRING_TEXT` and
/// `STRING_ESCAPE` tokens in order, as `(kind, text)`. Other kinds are
/// skipped; interpolations are handled by the caller. Line breaks become
/// LF (D-114). An invalid escape, already reported, keeps its text.
pub fn string_value<'a>(parts: impl IntoIterator<Item = (SyntaxKind, &'a str)>) -> String {
    let mut value = String::new();
    for (kind, text) in parts {
        match kind {
            SyntaxKind::STRING_TEXT => value.push_str(&normalize_line_breaks(text)),
            SyntaxKind::STRING_ESCAPE => match escape_value(text) {
                Ok(c) => value.push(c),
                Err(_) => value.push_str(text),
            },
            _ => {}
        }
    }
    value
}

/// The contents of a raw string token such as `r#"a "b""#`, without its
/// delimiters (§2.5.3, D-130). Line breaks become LF, as in other strings
/// (D-431). An unterminated raw string runs to the end of the token.
#[must_use]
pub fn raw_string_value(text: &str) -> Cow<'_, str> {
    let Some(rest) = text.strip_prefix('r') else {
        return Cow::Borrowed("");
    };
    let after_hashes = rest.trim_start_matches('#');
    let hashes = rest.len().saturating_sub(after_hashes.len());
    let Some(body) = after_hashes.strip_prefix('"') else {
        return Cow::Borrowed("");
    };
    let closer_len = hashes.saturating_add(1);
    let closed = body.len() >= closer_len
        && body
            .get(body.len().saturating_sub(closer_len)..)
            .is_some_and(|tail| tail.starts_with('"') && tail.bytes().skip(1).all(|b| b == b'#'));
    let contents = if closed {
        body.get(..body.len().saturating_sub(closer_len))
            .unwrap_or(body)
    } else {
        body
    };
    normalize_line_breaks(contents)
}

/// `text` with every CRLF replaced by LF.
fn normalize_line_breaks(text: &str) -> Cow<'_, str> {
    if text.contains("\r\n") {
        Cow::Owned(text.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EscapeError, escape_value, float_value, int_value, raw_string_value, string_value,
    };
    use crate::SyntaxKind::{INTERP_START, LOWER_IDENT, STRING_ESCAPE, STRING_TEXT};

    #[test]
    fn integers() {
        assert_eq!(int_value("0"), Some(0));
        assert_eq!(int_value("1_000"), Some(1000));
        assert_eq!(int_value("0xFF"), Some(255));
        assert_eq!(int_value("0x_f_f"), Some(255));
        assert_eq!(int_value("0o17"), Some(15));
        assert_eq!(int_value("0b1010"), Some(10));
        assert_eq!(int_value("007"), Some(7));
        assert_eq!(
            int_value("340282366920938463463374607431768211455"),
            Some(u128::MAX)
        );
        assert_eq!(int_value("340282366920938463463374607431768211456"), None);
        for bad in ["", "0x", "0x_", "1u8", "1.5", "0b2", "+5", "x"] {
            assert_eq!(int_value(bad), None, "{bad}");
        }
    }

    #[test]
    fn floats() {
        assert_eq!(float_value("1.5"), Some(1.5));
        assert_eq!(float_value("1_0.2_5"), Some(10.25));
        assert_eq!(float_value("1e3"), Some(1000.0));
        assert_eq!(float_value("2.5E-1"), Some(0.25));
        assert_eq!(float_value("3"), Some(3.0));
        assert_eq!(float_value("1e999"), Some(f64::INFINITY));
        for bad in ["", "0x1", "1.5f", "1.", "inf", "NaN"] {
            assert_eq!(float_value(bad), None, "{bad}");
        }
    }

    #[test]
    fn escapes() {
        assert_eq!(escape_value(r"\n"), Ok('\n'));
        assert_eq!(escape_value(r"\t"), Ok('\t'));
        assert_eq!(escape_value(r"\r"), Ok('\r'));
        assert_eq!(escape_value(r"\\"), Ok('\\'));
        assert_eq!(escape_value(r#"\""#), Ok('"'));
        assert_eq!(escape_value(r"\{"), Ok('{'));
        assert_eq!(escape_value(r"\u{e9}"), Ok('é'));
        assert_eq!(escape_value(r"\u{10FFFF}"), Ok('\u{10ffff}'));
        assert_eq!(escape_value(r"\q"), Err(EscapeError::Unknown('q')));
        assert_eq!(escape_value("\\"), Err(EscapeError::Incomplete));
        assert_eq!(escape_value(r"\u"), Err(EscapeError::UnicodeWithoutBraces));
        assert_eq!(escape_value(r"\u{}"), Err(EscapeError::UnicodeDigitCount));
        assert_eq!(
            escape_value(r"\u{0000041}"),
            Err(EscapeError::UnicodeDigitCount)
        );
        assert_eq!(
            escape_value(r"\u{D800}"),
            Err(EscapeError::NotScalarValue(0xD800))
        );
        assert_eq!(
            escape_value(r"\u{110000}"),
            Err(EscapeError::NotScalarValue(0x11_0000))
        );
    }

    #[test]
    fn strings() {
        let parts = [
            (STRING_TEXT, "a\r\nb "),
            (STRING_ESCAPE, r"\{"),
            (INTERP_START, "{"),
            (LOWER_IDENT, "x"),
            (STRING_ESCAPE, r"\q"),
            (STRING_ESCAPE, r"\u{41}"),
        ];
        assert_eq!(string_value(parts), "a\nb {\\qA");
    }

    #[test]
    fn raw_strings() {
        assert_eq!(raw_string_value(r#"r"a\b""#), r"a\b");
        assert_eq!(raw_string_value("r#\"say \"hi\"\"#"), "say \"hi\"");
        assert_eq!(raw_string_value("r\"x\r\ny\""), "x\ny");
        assert_eq!(raw_string_value("r#\"open"), "open");
        assert_eq!(raw_string_value("r#\"open\""), "open\"");
        assert_eq!(raw_string_value(r#"r"""#), "");
        assert_eq!(raw_string_value("x"), "");
    }
}
