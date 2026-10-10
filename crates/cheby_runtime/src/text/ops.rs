//! The semantics of the `std::string` kernels on Rust strings, with no
//! runtime objects, so property tests can compare them with reference
//! implementations. Positions and counts are in code points (D-302) and
//! clamped (D-298); results that keep part of an argument are byte ranges
//! into it, so the kernels can keep them in place (D-525).

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

/// A count as an `Int`. Counts of objects in memory always fit.
pub(crate) fn int(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The number of code points of `text`. O(n).
pub(crate) fn length(text: &str) -> usize {
    text.chars().count()
}

/// The number of extended grapheme clusters of `text`. O(n).
pub(crate) fn grapheme_length(text: &str) -> usize {
    text.graphemes(true).count()
}

/// The extended grapheme clusters of `text`, in order.
pub(crate) fn graphemes(text: &str) -> impl Iterator<Item = &str> {
    text.graphemes(true)
}

/// The code points of `text`, each as a one-code-point string.
pub(crate) fn code_points(text: &str) -> impl Iterator<Item = &str> {
    text.char_indices().map(|(start, c)| {
        text.get(start..start.saturating_add(c.len_utf8()))
            .unwrap_or_default()
    })
}

/// The parts of `text` between occurrences of `separator`, searched from
/// the start; an empty separator gives the code points (D-315).
pub(crate) fn split<'a>(text: &'a str, separator: &str) -> Vec<&'a str> {
    if separator.is_empty() {
        code_points(text).collect()
    } else {
        text.split(separator).collect()
    }
}

/// The parts of `text` before and after the first `separator`, or `None`
/// when it does not occur. An empty separator splits after the first code
/// point, and gives `None` on `""` (D-315).
pub(crate) fn split_once<'a>(text: &'a str, separator: &str) -> Option<(&'a str, &'a str)> {
    if separator.is_empty() {
        let first = text.chars().next()?;
        return Some(text.split_at(first.len_utf8()));
    }
    text.split_once(separator)
}

/// The byte offset of code point `position` of `text`, or `text.len()`
/// when `text` has no more code points. O(position).
fn byte_offset(text: &str, position: usize) -> usize {
    text.char_indices()
        .nth(position)
        .map_or(text.len(), |(offset, _)| offset)
}

/// A code-point position or count clamped below at 0 (D-298).
pub(crate) fn clamp(value: i64) -> usize {
    usize::try_from(value.max(0)).unwrap_or(usize::MAX)
}

/// The byte range of the code points of `text` from `start` up to, not
/// including, `end`, after clamping both to `0..=length` (D-298, D-319).
/// Empty when `end` is not above `start`. O(end).
pub(crate) fn slice(text: &str, start: i64, end: i64) -> Range<usize> {
    let (start, end) = (clamp(start), clamp(end));
    if end <= start {
        return 0..0;
    }
    let from = byte_offset(text, start);
    let rest = text.get(from..).unwrap_or_default();
    let to = from.saturating_add(byte_offset(rest, end.saturating_sub(start)));
    from..to
}

/// The byte range of `text` without `White_Space` at either end (D-310).
pub(crate) fn trim(text: &str) -> Range<usize> {
    let start = trim_start(text).start;
    let end = text.trim_end().len();
    start..end.max(start)
}

/// The byte range of `text` without `White_Space` at the start (D-373).
pub(crate) fn trim_start(text: &str) -> Range<usize> {
    text.len().saturating_sub(text.trim_start().len())..text.len()
}

/// The byte range of `text` without `White_Space` at the end (D-373).
pub(crate) fn trim_end(text: &str) -> Range<usize> {
    0..text.trim_end().len()
}

/// The byte range of `text` without one leading `prefix`, if it is there
/// (D-373). O(m) in the prefix.
pub(crate) fn remove_prefix(text: &str, prefix: &str) -> Range<usize> {
    if text.starts_with(prefix) {
        prefix.len()..text.len()
    } else {
        0..text.len()
    }
}

/// The byte range of `text` without one trailing `suffix`, if it is there
/// (D-373). O(m) in the suffix.
pub(crate) fn remove_suffix(text: &str, suffix: &str) -> Range<usize> {
    if text.ends_with(suffix) {
        0..text.len().saturating_sub(suffix.len())
    } else {
        0..text.len()
    }
}

/// The code points of `text` in reverse order (D-302).
pub(crate) fn reverse(text: &str) -> String {
    text.chars().rev().collect()
}

/// The padding that makes a text of `length` code points `width` code
/// points long: whole copies of `pad` written from the left, the last one
/// cut to fit. `None` when nothing is added: `width` is not above `length`
/// (clamped, D-298) or `pad` is `""`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fill<'a> {
    /// The pad text.
    pub(crate) pad: &'a str,
    /// The number of whole copies.
    pub(crate) copies: u64,
    /// The bytes of `pad` that make the cut last copy.
    pub(crate) tail: &'a str,
}

impl<'a> Fill<'a> {
    /// The fill for a text of `length` code points, or `None`.
    pub(crate) fn new(length: usize, width: i64, pad: &'a str) -> Option<Self> {
        let missing = clamp(width).checked_sub(length).filter(|&n| n > 0)?;
        let pad_length = self::length(pad);
        let copies = missing.checked_div(pad_length)?;
        let rest = missing.checked_rem(pad_length)?;
        let tail = pad.get(..byte_offset(pad, rest)).unwrap_or_default();
        Some(Self {
            pad,
            copies: u64::try_from(copies).unwrap_or(u64::MAX),
            tail,
        })
    }

    /// The number of bytes of the fill, or `None` if it overflows.
    pub(crate) fn bytes(&self) -> Option<u64> {
        let pad = u64::try_from(self.pad.len()).ok()?;
        let tail = u64::try_from(self.tail.len()).ok()?;
        self.copies.checked_mul(pad)?.checked_add(tail)
    }

    /// Calls `write` with each piece of the fill, in order.
    pub(crate) fn write(&self, mut write: impl FnMut(&str)) {
        let mut left = self.copies;
        while left > 0 {
            write(self.pad);
            left = left.saturating_sub(1);
        }
        if !self.tail.is_empty() {
            write(self.tail);
        }
    }
}

/// The text of the scalar values in `values`, or `None` when one is not a
/// Unicode scalar value (below 0, above `0x10FFFF`, or a surrogate).
pub(crate) fn from_code_point_values(values: impl IntoIterator<Item = i64>) -> Option<String> {
    values
        .into_iter()
        .map(|value| u32::try_from(value).ok().and_then(char::from_u32))
        .collect()
}
