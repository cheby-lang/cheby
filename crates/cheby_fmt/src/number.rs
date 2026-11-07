//! The digit separators of a number (D-1345).
//!
//! A decimal literal of five or more digits written with no `_` gets one
//! between groups of three, counted from its last digit: `1000000` prints
//! as `1_000_000`. A float's integer part follows the same rule, and its
//! fraction and exponent are left alone.
//!
//! Every other literal stays as written. An author who wrote a `_` chose
//! the grouping, as in the dollars and cents of `10_00`, and no rule could
//! rebuild it. A literal with a base prefix has no decimal place values,
//! and one with a leading zero is an error for the user to fix (§2.5.1).

use std::borrow::Cow;

use cheby_syntax::SyntaxKind;

/// How many digits an integer part needs before it is grouped.
const FIRST_GROUPED: usize = 5;

/// How many digits a group holds.
const GROUP: usize = 3;

/// `text`, the text of a token of kind `kind`, as the style prints it.
pub(crate) fn styled(kind: SyntaxKind, text: &str) -> Cow<'_, str> {
    if !matches!(kind, SyntaxKind::INT_NUMBER | SyntaxKind::FLOAT_NUMBER) || text.contains('_') {
        return Cow::Borrowed(text);
    }
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (whole, rest) = text.split_at(end);
    if whole.len() < FIRST_GROUPED || whole.starts_with('0') {
        return Cow::Borrowed(text);
    }
    let first = whole.len() % GROUP;
    let mut out = String::new();
    for (i, digit) in whole.char_indices() {
        if i != 0 && i % GROUP == first {
            out.push('_');
        }
        out.push(digit);
    }
    out.push_str(rest);
    Cow::Owned(out)
}
