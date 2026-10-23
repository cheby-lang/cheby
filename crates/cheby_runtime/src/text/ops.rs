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
/// the start; an empty separator gives the code points (D-315). The parts
/// are found as they are taken, so splitting holds no buffer of them.
pub(crate) fn split<'a>(text: &'a str, separator: &'a str) -> impl Iterator<Item = &'a str> {
    let (points, parts) = if separator.is_empty() {
        (Some(code_points(text)), None)
    } else {
        (None, Some(text.split(separator)))
    };
    points
        .into_iter()
        .flatten()
        .chain(parts.into_iter().flatten())
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

/// Whether `c` has Unicode's `White_Space` property: the whitespace that
/// `trim`, `trim_start` and `trim_end` remove (D-310, D-373) and that
/// `words` splits at (D-P13). `char::is_whitespace` is exactly that
/// property, 25 code points in Unicode 17.0.0, so U+0085 is whitespace and
/// U+FEFF is not, unlike ECMAScript's `\s`. The JS kernels of M4 share one
/// explicit table of the same code points.
pub(crate) fn is_white_space(c: char) -> bool {
    c.is_whitespace()
}

/// The byte range of `text` without `White_Space` at either end (D-310).
pub(crate) fn trim(text: &str) -> Range<usize> {
    let start = trim_start(text).start;
    let end = trim_end(text).end;
    start..end.max(start)
}

/// The byte range of `text` without `White_Space` at the start (D-373).
pub(crate) fn trim_start(text: &str) -> Range<usize> {
    let rest = text.trim_start_matches(is_white_space);
    text.len().saturating_sub(rest.len())..text.len()
}

/// The byte range of `text` without `White_Space` at the end (D-373).
pub(crate) fn trim_end(text: &str) -> Range<usize> {
    0..text.trim_end_matches(is_white_space).len()
}

/// The parts of `text` between runs of `White_Space`, with no empty parts
/// (D-P13): the whitespace that `trim` removes separates them. O(n).
pub(crate) fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(is_white_space).filter(|part| !part.is_empty())
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
pub(crate) fn reverse(text: &str) -> impl Iterator<Item = char> {
    text.chars().rev()
}

/// The code points of `str::to_uppercase(text)`, in order: each code
/// point's full uppercase mapping, which depends on nothing around it.
pub(crate) fn uppercase(text: &str) -> impl Iterator<Item = char> {
    text.chars().flat_map(char::to_uppercase)
}

/// The code points of `str::to_lowercase(text)`, in order: each code
/// point's full lowercase mapping, except that a capital sigma becomes
/// `ς` at the end of a word and `σ` elsewhere (`Final_Sigma`, D-310), the
/// one mapping that depends on its context.
pub(crate) fn lowercase(text: &str) -> impl Iterator<Item = char> {
    text.char_indices().flat_map(move |(at, c)| {
        let sigma = (c == CAPITAL_SIGMA).then(|| {
            let before = text.get(..at).unwrap_or_default();
            let after = text
                .get(at.saturating_add(c.len_utf8())..)
                .unwrap_or_default();
            if ignorable_then_cased(before.chars().rev()) && !ignorable_then_cased(after.chars()) {
                'ς'
            } else {
                'σ'
            }
        });
        let mapped = sigma.is_none().then(|| c.to_lowercase());
        sigma.into_iter().chain(mapped.into_iter().flatten())
    })
}

/// `Σ`, whose lowercase depends on its context.
const CAPITAL_SIGMA: char = 'Σ';

/// What the `Final_Sigma` condition needs of a code point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SigmaClass {
    /// `Case_Ignorable`: skipped when looking for the cased letter.
    Ignorable,
    /// `Cased` and not `Case_Ignorable`.
    Cased,
    /// Neither.
    Other,
}

/// Whether the first code point of `chars` that is not `Case_Ignorable`
/// is `Cased`: half of the `Final_Sigma` condition, as the standard
/// library's `str::to_lowercase` checks it.
fn ignorable_then_cased(chars: impl Iterator<Item = char>) -> bool {
    chars
        .map(sigma_class)
        .find(|&class| class != SigmaClass::Ignorable)
        == Some(SigmaClass::Cased)
}

/// The class of `c` for the `Final_Sigma` condition. The standard library
/// keeps `Case_Ignorable` and `Cased` to itself, so they are read off
/// `str::to_lowercase` on two three-code-point texts that end with `Σ`,
/// which lowercases to `ς` exactly when the first code point before it
/// that is not `Case_Ignorable` is `Cased`. Before `c` stands `A`, which
/// is cased, or a space, which is neither cased nor `Case_Ignorable`:
/// `AcΣ` gives `ς` when `c` is ignorable or cased, and ` cΣ` when it is
/// cased and not ignorable. Each text is a few bytes, so this allocates
/// nothing that grows with the data.
fn sigma_class(c: char) -> SigmaClass {
    if c.is_ascii() {
        return match c {
            // ASCII's `Case_Ignorable` code points: `MidLetter`,
            // `MidNumLet` and `Single_Quote` ones, and the modifier
            // symbols `^` and `` ` ``.
            '\'' | '.' | ':' | '^' | '`' => SigmaClass::Ignorable,
            c if c.is_ascii_alphabetic() => SigmaClass::Cased,
            _ => SigmaClass::Other,
        };
    }
    let ends_final = |before: char| {
        let probe: String = [before, c, CAPITAL_SIGMA].into_iter().collect();
        probe.to_lowercase().ends_with('ς')
    };
    if ends_final(' ') {
        SigmaClass::Cased
    } else if ends_final('A') {
        SigmaClass::Ignorable
    } else {
        SigmaClass::Other
    }
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

/// The code point of the scalar value `value`, or `None` when it is not a
/// Unicode scalar value (below 0, above `0x10FFFF`, or a surrogate).
pub(crate) fn scalar(value: i64) -> Option<char> {
    u32::try_from(value).ok().and_then(char::from_u32)
}
