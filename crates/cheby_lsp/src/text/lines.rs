//! The line index and LSP position encodings (A §5, D-1368).
//!
//! An LSP position is a zero-based line and a zero-based column counted in
//! the negotiated encoding: UTF-8 bytes, UTF-16 code units or code points.
//! The index counts `\n`, `\r\n` and a lone `\r` as line ends, as the
//! protocol does. Nothing here goes through `SourceFile::line_col` or
//! `cheby_diag::LineIndex`, whose rules differ (D-421: code points from 1,
//! no break at a lone `\r`, the BOM hidden).
//!
//! Conversions never fail. An offset past the text, inside a line end or
//! inside a character, and a position past a line's end or the last line,
//! all clamp to the nearest place that exists.

use cheby_driver::analysis::{TextRange, TextSize};
use lsp_types::{Position, PositionEncodingKind, Range};

/// The unit an LSP column counts (D-1368).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionEncoding {
    /// UTF-8 bytes.
    Utf8,
    /// UTF-16 code units, the protocol's default.
    Utf16,
    /// Code points.
    Utf32,
}

impl PositionEncoding {
    /// The encoding the server answers with, given the client's
    /// `general.positionEncodings`: `utf-8` if listed, else `utf-32` if
    /// listed, else `utf-16` (D-1368). A client that sends no list gets
    /// UTF-16, the protocol's default.
    #[must_use]
    pub fn negotiate(offered: Option<&[PositionEncodingKind]>) -> Self {
        let offered = offered.unwrap_or_default();
        let has = |kind: &PositionEncodingKind| offered.iter().any(|o| o == kind);
        if has(&PositionEncodingKind::UTF8) {
            Self::Utf8
        } else if has(&PositionEncodingKind::UTF32) {
            Self::Utf32
        } else {
            Self::Utf16
        }
    }

    /// The protocol's name for this encoding, for
    /// `capabilities.positionEncoding`.
    #[must_use]
    pub fn kind(self) -> PositionEncodingKind {
        match self {
            Self::Utf8 => PositionEncodingKind::UTF8,
            Self::Utf16 => PositionEncodingKind::UTF16,
            Self::Utf32 => PositionEncodingKind::UTF32,
        }
    }

    /// How many units `c` takes in this encoding.
    fn units(self, c: char) -> u32 {
        let units = match self {
            Self::Utf8 => c.len_utf8(),
            Self::Utf16 => c.len_utf16(),
            Self::Utf32 => 1,
        };
        to_u32(units)
    }

    /// How many units `text` takes in this encoding.
    fn count(self, text: &str) -> u32 {
        let units = match self {
            Self::Utf8 => text.len(),
            Self::Utf16 => text.encode_utf16().count(),
            Self::Utf32 => text.chars().count(),
        };
        to_u32(units)
    }
}

/// Whether a leading U+FEFF is a character the client counts (D-1368).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bom {
    /// The client sent this text, so a leading U+FEFF is a character to it.
    Counted,
    /// Disk text: clients strip a BOM when they load a file, so line 0
    /// starts after it.
    Hidden,
}

/// The length of U+FEFF in UTF-8.
const BOM_LEN: TextSize = 3;

/// The lines of one text, for converting byte offsets to LSP positions and
/// back. Every method takes the text the index was built from.
#[derive(Clone, Debug)]
pub struct LspLines {
    /// Where each line starts. Never empty.
    starts: Vec<TextSize>,
    /// Where each line's content ends, before its terminator. As long as
    /// `starts`.
    ends: Vec<TextSize>,
    /// The text is ASCII, so a column is a byte delta in every encoding.
    ascii: bool,
    /// The text's length.
    len: TextSize,
}

impl LspLines {
    /// The index of `text`, read in one pass. With [`Bom::Hidden`], a
    /// leading U+FEFF is outside line 0.
    #[must_use]
    pub fn new(text: &str, bom: Bom) -> Self {
        let first = match bom {
            Bom::Hidden if text.starts_with('\u{feff}') => BOM_LEN,
            Bom::Hidden | Bom::Counted => 0,
        };
        let mut starts = vec![first];
        let mut ends = Vec::new();
        let mut bytes = text.bytes().enumerate().peekable();
        while let Some((at, byte)) = bytes.next() {
            let next = match byte {
                b'\r' if bytes.next_if(|&(_, b)| b == b'\n').is_some() => at.saturating_add(2),
                b'\n' | b'\r' => at.saturating_add(1),
                _ => continue,
            };
            ends.push(to_size(at));
            starts.push(to_size(next));
        }
        ends.push(to_size(text.len()));
        Self {
            starts,
            ends,
            ascii: text.is_ascii(),
            len: to_size(text.len()),
        }
    }

    /// The number of lines, at least 1. A text that ends with a line end
    /// has an empty last line after it.
    #[cfg(test)]
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// Where the content of the first line starts: 3 after a hidden BOM,
    /// else 0.
    #[must_use]
    pub fn content_start(&self) -> TextSize {
        self.starts.first().copied().unwrap_or(0)
    }

    /// `offset` as a position. An offset past the text is the text's end.
    /// It floors to a character boundary, and an offset inside a line end
    /// gives that line's content end. An offset inside a hidden BOM is the
    /// start of line 0.
    #[must_use]
    pub fn position(&self, text: &str, offset: TextSize, enc: PositionEncoding) -> Position {
        let offset = offset.min(self.len);
        let line = self
            .starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        let (start, end) = self.bounds(line);
        let at = floor_char_boundary(text, offset.clamp(start, end)).max(start);
        let column = if self.ascii {
            at.saturating_sub(start)
        } else {
            text.get(to_usize(start)..to_usize(at))
                .map_or(0, |prefix| enc.count(prefix))
        };
        Position::new(to_u32(line), column)
    }

    /// `position` as an offset. A line past the last is the end of the
    /// text. A column past the line's end is its content end. A column
    /// inside a character, such as between the halves of a UTF-16
    /// surrogate pair, is that character's start.
    #[must_use]
    pub fn offset(&self, text: &str, position: Position, enc: PositionEncoding) -> TextSize {
        let Ok(line) = usize::try_from(position.line) else {
            return self.len;
        };
        if line >= self.starts.len() {
            return self.len;
        }
        let (start, end) = self.bounds(line);
        let column = position.character;
        if self.ascii || enc == PositionEncoding::Utf8 {
            let at = start.saturating_add(column).min(end);
            return floor_char_boundary(text, at).max(start);
        }
        let Some(content) = text.get(to_usize(start)..to_usize(end)) else {
            return start;
        };
        let mut units = 0_u32;
        for (at, c) in content.char_indices() {
            let next = units.saturating_add(enc.units(c));
            if next > column {
                return start.saturating_add(to_size(at));
            }
            units = next;
        }
        end
    }

    /// `range` as an LSP range, each end converted by [`Self::position`].
    #[must_use]
    pub fn range(&self, text: &str, range: TextRange, enc: PositionEncoding) -> Range {
        Range::new(
            self.position(text, range.start(), enc),
            self.position(text, range.end(), enc),
        )
    }

    /// `range` as a byte range, each end converted by [`Self::offset`]. A
    /// range whose end comes first is turned round.
    #[must_use]
    pub fn text_range(&self, text: &str, range: Range, enc: PositionEncoding) -> TextRange {
        TextRange::new(
            self.offset(text, range.start, enc),
            self.offset(text, range.end, enc),
        )
    }

    /// `range` widened for publishing when it is empty (A §5): to the
    /// character before it, or at the start of the text to the one after
    /// it. A text with no character leaves it empty. A non-empty range is
    /// returned as it is.
    #[must_use]
    pub fn widen_empty(&self, text: &str, range: TextRange) -> TextRange {
        if !range.is_empty() {
            return range;
        }
        let first = self.content_start();
        let at = floor_char_boundary(text, range.start()).max(first);
        let before = text
            .get(to_usize(first)..to_usize(at))
            .and_then(|head| head.chars().next_back());
        if let Some(c) = before {
            return TextRange::new(at.saturating_sub(to_size(c.len_utf8())), at);
        }
        let after = text
            .get(to_usize(at)..)
            .and_then(|tail| tail.chars().next());
        match after {
            Some(c) => TextRange::at(at, to_size(c.len_utf8())),
            None => TextRange::empty(at),
        }
    }

    /// The start and content end of `line`, which must exist.
    fn bounds(&self, line: usize) -> (TextSize, TextSize) {
        let start = self.starts.get(line).copied().unwrap_or(self.len);
        let end = self.ends.get(line).copied().unwrap_or(self.len);
        (start, end.max(start))
    }
}

/// Applies one `textDocument/didChange` content change to client text
/// (A §5): `range: None` replaces the whole text, and a range is converted
/// with a fresh [`LspLines`] over the text as it stands, the BOM counted.
/// A range past the text clamps. Returns `false`, and leaves `text` as it
/// was, only if the range does not fall on character boundaries, which a
/// range from [`LspLines::text_range`] always does.
#[must_use]
pub fn apply_change(
    text: &mut String,
    range: Option<Range>,
    new_text: &str,
    enc: PositionEncoding,
) -> bool {
    let Some(range) = range else {
        new_text.clone_into(text);
        return true;
    };
    let lines = LspLines::new(text, Bom::Counted);
    let edit = lines.text_range(text, range, enc);
    let (start, end) = (to_usize(edit.start()), to_usize(edit.end()));
    if !(text.is_char_boundary(start) && text.is_char_boundary(end)) {
        return false;
    }
    text.replace_range(start..end, new_text);
    true
}

/// Moves `offset` back to a character boundary of `text`, clamped to its
/// length.
fn floor_char_boundary(text: &str, offset: TextSize) -> TextSize {
    let mut at = to_usize(offset).min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at = at.saturating_sub(1);
    }
    to_size(at)
}

/// A byte count as a [`TextSize`]. Texts are at most `u32::MAX` bytes, so
/// it saturates only on a broken input.
fn to_size(n: usize) -> TextSize {
    TextSize::try_from(n).unwrap_or(TextSize::MAX)
}

/// A count as an LSP `u32`, saturating.
fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A [`TextSize`] as a `usize`. Lossless on every supported target.
fn to_usize(n: TextSize) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::arithmetic_side_effects,
        reason = "test models count lines and columns"
    )]

    use proptest::prelude::*;
    use proptest::sample::Index;
    use proptest::test_runner::{Config, RngSeed};

    use super::*;

    const ENCODINGS: [PositionEncoding; 3] = [
        PositionEncoding::Utf8,
        PositionEncoding::Utf16,
        PositionEncoding::Utf32,
    ];

    fn pos(line: u32, character: u32) -> Position {
        Position::new(line, character)
    }

    /// The reference model: the position of every char boundary of `text`
    /// that is not inside a `\r\n` or a hidden BOM, walking characters one
    /// by one.
    fn model(text: &str, bom: Bom, enc: PositionEncoding) -> Vec<(TextSize, Position)> {
        let skip = match bom {
            Bom::Hidden if text.starts_with('\u{feff}') => 3,
            Bom::Hidden | Bom::Counted => 0,
        };
        let mut out = Vec::new();
        let (mut line, mut column) = (0_u32, 0_u32);
        let mut chars = text.char_indices().skip(usize::from(skip > 0)).peekable();
        while let Some((at, c)) = chars.next() {
            out.push((to_size(at), pos(line, column)));
            match c {
                '\r' if chars.peek().is_some_and(|&(_, n)| n == '\n') => {
                    chars.next();
                    (line, column) = (line + 1, 0);
                }
                '\n' | '\r' => (line, column) = (line + 1, 0),
                _ => column += enc.units(c),
            }
        }
        out.push((to_size(text.len()), pos(line, column)));
        out
    }

    fn piece() -> impl Strategy<Value = &'static str> {
        prop::sample::select(vec!["a", "é", "€", "😀", "\n", "\r\n", "\r"])
    }

    fn text() -> impl Strategy<Value = String> {
        (any::<bool>(), prop::collection::vec(piece(), 0..40)).prop_map(|(bom, pieces)| {
            let mut text = String::from(if bom { "\u{feff}" } else { "" });
            text.extend(pieces);
            text
        })
    }

    fn config() -> Config {
        Config {
            cases: 512,
            rng_seed: RngSeed::Fixed(0x1368),
            failure_persistence: None,
            ..Config::default()
        }
    }

    proptest! {
        #![proptest_config(config())]

        #[test]
        fn offset_position_round_trip(text in text()) {
            for bom in [Bom::Counted, Bom::Hidden] {
                let lines = LspLines::new(&text, bom);
                for enc in ENCODINGS {
                    for (offset, expected) in model(&text, bom, enc) {
                        let position = lines.position(&text, offset, enc);
                        prop_assert_eq!(position, expected, "{:?} {:?} at {}", bom, enc, offset);
                        prop_assert_eq!(lines.offset(&text, position, enc), offset);
                    }
                }
            }
        }

        #[test]
        fn edits_through_positions_equal_byte_edits(
            start in text(),
            edits in prop::collection::vec((any::<Index>(), any::<Index>(), text()), 1..8),
            enc in prop::sample::select(ENCODINGS.to_vec()),
        ) {
            let mut by_position = start.clone();
            let mut by_offset = start;
            for (from, to, insert) in edits {
                let lines = LspLines::new(&by_offset, Bom::Counted);
                let offsets: Vec<TextSize> = model(&by_offset, Bom::Counted, enc)
                    .into_iter()
                    .map(|(offset, _)| offset)
                    .collect();
                let a = *from.get(&offsets);
                let b = *to.get(&offsets);
                let (a, b) = (a.min(b), a.max(b));
                let range = lines.range(&by_offset, TextRange::new(a, b), enc);
                prop_assert!(apply_change(&mut by_position, Some(range), &insert, enc));
                by_offset.replace_range(to_usize(a)..to_usize(b), &insert);
                prop_assert_eq!(&by_position, &by_offset);
            }
        }
    }

    #[test]
    fn negotiation_prefers_utf8_then_utf32() {
        use PositionEncodingKind as K;
        let cases: [(Option<Vec<K>>, PositionEncoding); 6] = [
            (None, PositionEncoding::Utf16),
            (Some(vec![]), PositionEncoding::Utf16),
            (Some(vec![K::UTF16]), PositionEncoding::Utf16),
            (Some(vec![K::UTF16, K::UTF32]), PositionEncoding::Utf32),
            (
                Some(vec![K::UTF32, K::UTF8, K::UTF16]),
                PositionEncoding::Utf8,
            ),
            (Some(vec![K::new("utf-7")]), PositionEncoding::Utf16),
        ];
        for (offered, expected) in cases {
            let got = PositionEncoding::negotiate(offered.as_deref());
            assert_eq!(got, expected, "{offered:?}");
            assert_eq!(PositionEncoding::negotiate(Some(&[got.kind()])), got);
        }
    }

    #[test]
    fn line_ends_count_lf_crlf_and_lone_cr() {
        let text = "a\nb\r\nc\rd\n";
        let lines = LspLines::new(text, Bom::Counted);
        assert_eq!(lines.line_count(), 5);
        let enc = PositionEncoding::Utf16;
        let starts: Vec<_> = (0..5).map(|l| lines.offset(text, pos(l, 0), enc)).collect();
        assert_eq!(starts, [0, 2, 5, 7, 9]);
    }

    #[test]
    fn clamping_table() {
        // "😀" is 4 bytes, 2 UTF-16 units, 1 code point.
        let text = "x😀y\r\nz";
        let lines = LspLines::new(text, Bom::Counted);
        let (u8, u16, u32) = (
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        );
        let offsets: [(Position, PositionEncoding, TextSize); 10] = [
            // Inside the surrogate pair: the character's start.
            (pos(0, 2), u16, 1),
            (pos(0, 3), u16, 5),
            // Inside the UTF-8 bytes: floored.
            (pos(0, 3), u8, 1),
            (pos(0, 2), u32, 5),
            // Past the line's end: its content end, before `\r\n`.
            (pos(0, 99), u16, 6),
            (pos(0, 99), u8, 6),
            // Past the last line, or the last line's end: the end of text.
            (pos(5, 0), u16, 9),
            (pos(u32::MAX, 0), u8, 9),
            (pos(1, 7), u32, 9),
            (pos(1, 0), u16, 8),
        ];
        for (position, enc, expected) in offsets {
            assert_eq!(
                lines.offset(text, position, enc),
                expected,
                "{position:?} {enc:?}"
            );
        }
        let positions: [(TextSize, PositionEncoding, Position); 6] = [
            // Inside "😀": floored to its start.
            (3, u16, pos(0, 1)),
            // Between `\r` and `\n`: the content end.
            (7, u16, pos(0, 4)),
            (7, u32, pos(0, 3)),
            (6, u8, pos(0, 6)),
            // Past the text: its end.
            (99, u16, pos(1, 1)),
            (8, u16, pos(1, 0)),
        ];
        for (offset, enc, expected) in positions {
            assert_eq!(
                lines.position(text, offset, enc),
                expected,
                "{offset} {enc:?}"
            );
        }
    }

    #[test]
    fn bom_counts_only_in_client_text() {
        let text = "\u{feff}ab\nc";
        let enc = PositionEncoding::Utf16;
        let counted = LspLines::new(text, Bom::Counted);
        assert_eq!(counted.position(text, 3, enc), pos(0, 1));
        assert_eq!(counted.offset(text, pos(0, 0), enc), 0);
        let hidden = LspLines::new(text, Bom::Hidden);
        assert_eq!(hidden.content_start(), 3);
        assert_eq!(hidden.position(text, 3, enc), pos(0, 0));
        assert_eq!(hidden.position(text, 0, enc), pos(0, 0));
        assert_eq!(hidden.offset(text, pos(0, 0), enc), 3);
        assert_eq!(hidden.offset(text, pos(1, 1), enc), 7);
        // A BOM only counts at the start.
        let inner = "a\u{feff}b";
        let lines = LspLines::new(inner, Bom::Hidden);
        assert_eq!(lines.position(inner, 4, enc), pos(0, 2));
    }

    #[test]
    fn empty_text_has_one_line() {
        let lines = LspLines::new("", Bom::Hidden);
        assert_eq!(lines.line_count(), 1);
        for enc in ENCODINGS {
            assert_eq!(lines.position("", 0, enc), pos(0, 0));
            assert_eq!(lines.offset("", pos(3, 3), enc), 0);
        }
        assert_eq!(
            lines.widen_empty("", TextRange::empty(0)),
            TextRange::empty(0)
        );
    }

    #[test]
    fn empty_ranges_widen_to_a_character() {
        let text = "é\n😀";
        let lines = LspLines::new(text, Bom::Counted);
        let widen = |at| lines.widen_empty(text, TextRange::empty(at));
        assert_eq!(widen(0), TextRange::new(0, 2));
        assert_eq!(widen(2), TextRange::new(0, 2));
        assert_eq!(widen(7), TextRange::new(3, 7));
        // Inside a character: floored first.
        assert_eq!(widen(5), TextRange::new(2, 3));
        assert_eq!(
            lines.widen_empty(text, TextRange::new(0, 2)),
            TextRange::new(0, 2)
        );
        let bom = "\u{feff}x";
        let hidden = LspLines::new(bom, Bom::Hidden);
        assert_eq!(
            hidden.widen_empty(bom, TextRange::empty(3)),
            TextRange::new(3, 4)
        );
    }

    #[test]
    fn whole_text_change_replaces() {
        let mut text = String::from("old");
        assert!(apply_change(
            &mut text,
            None,
            "new",
            PositionEncoding::Utf16
        ));
        assert_eq!(text, "new");
        let range = Range::new(pos(0, 1), pos(9, 9));
        assert!(apply_change(
            &mut text,
            Some(range),
            "!",
            PositionEncoding::Utf16
        ));
        assert_eq!(text, "n!");
        // A reversed range is turned round.
        let reversed = Range::new(pos(0, 2), pos(0, 0));
        assert!(apply_change(
            &mut text,
            Some(reversed),
            "",
            PositionEncoding::Utf16
        ));
        assert_eq!(text, "");
    }
}
