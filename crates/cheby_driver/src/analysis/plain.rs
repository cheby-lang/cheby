//! Plain data for the server: diagnostics with no generation handles, keys
//! over text, and the map between live and normalized text.
//!
//! A [`Diagnostic`] holds `Span { file: FileId, .. }`, which names a file of
//! one run only, so it never leaves the driver (D-1364). [`plain`] turns it
//! into a [`PlainDiagnostic`] at the end of a run. Every range it keeps lies
//! in a text the caller vouches for: the primary range in the owning file,
//! a related label or an edit in a file whose run text is its live text.
//! Anything else is dropped (A §2.3).
//!
//! Two keys hash text with SipHash-1-3 128, as the build cache does:
//! [`exact_key`] over the bytes as they are (D-1393), and
//! [`normalized_key`] over the text with a leading BOM removed and `\r\n`
//! read as `\n` (D-1412), which is how child results are matched.
//! [`NormMap`] maps offsets between that normalized text and the live text.
//! [`InputsKey`] folds many texts into one key.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use cheby_diag::{Code, Diagnostic, FileId, LabelStyle, Severity, TextRange, TextSize};
use siphasher::sip128::{Hasher128 as _, SipHasher13};

use crate::cache::Hash128;

/// A diagnostic as the server publishes it: paths and byte ranges, no
/// handles. The owning file is implied by where the diagnostic is kept.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PlainDiagnostic {
    /// The diagnostic's code.
    pub code: Code,
    /// Error or warning, from the code (D-419).
    pub severity: Severity,
    /// The header message.
    pub message: String,
    /// The first primary label's range in the owning file. `None` when the
    /// diagnostic has no span (A §2.7) or its span could not be placed.
    pub range: Option<TextRange>,
    /// The first primary label's message, empty for none.
    pub primary_message: String,
    /// Every other label, in order, in a file whose text is known.
    pub related: Vec<PlainLabel>,
    /// The `= note:` lines.
    pub notes: Vec<String>,
    /// The `= help:` lines, each with its edits.
    pub helps: Vec<PlainHelp>,
}

/// A label other than the first primary one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PlainLabel {
    /// The file the range lies in.
    pub path: PathBuf,
    /// A byte range in that file's live text.
    pub range: TextRange,
    /// The label's message, empty for none.
    pub message: String,
}

/// A `help` line and the edits that apply it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PlainHelp {
    /// The help's message.
    pub message: String,
    /// The machine-applicable edits, empty for a help with none.
    pub edits: Vec<PlainEdit>,
}

/// One text replacement of a [`PlainHelp`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PlainEdit {
    /// The file the range lies in.
    pub path: PathBuf,
    /// A byte range in that file's live text.
    pub range: TextRange,
    /// The text that replaces the range.
    pub replacement: String,
}

/// A file of a run whose text is the text the editor sees: its path and
/// that text. [`plain`] keeps a range only inside such a file.
#[derive(Clone, Copy, Debug)]
pub struct Placed<'a> {
    /// The file's path.
    pub path: &'a Path,
    /// The file's text in the run, equal to its live text.
    pub text: &'a str,
}

impl Placed<'_> {
    /// Whether `range` lies inside the text, on character boundaries.
    #[must_use]
    pub fn holds(&self, range: TextRange) -> bool {
        range.slice(self.text).is_some()
    }
}

/// `diag` as plain data.
///
/// `place` answers for each file of the run: `Some` when its run text is
/// its live text, `None` otherwise (last good text, a stub, std, or a
/// synthetic file). The primary range is kept when its file is `owner` and
/// `place` places it. A label elsewhere is dropped when its file is not
/// placed, and a help is dropped whole when any of its edits is not
/// placed, since half a fix is wrong (A §2.3). A range that falls outside
/// its text is dropped the same way.
#[must_use]
pub fn plain<'a>(
    diag: &Diagnostic,
    owner: FileId,
    place: impl Fn(FileId) -> Option<Placed<'a>>,
) -> PlainDiagnostic {
    let primary = diag
        .labels
        .iter()
        .position(|label| label.style == LabelStyle::Primary);
    let primary_label = primary.and_then(|i| diag.labels.get(i));
    let range = primary_label.and_then(|label| {
        (label.span.file == owner)
            .then(|| place(owner))
            .flatten()
            .filter(|placed| placed.holds(label.span.range))
            .map(|_| label.span.range)
    });
    let related = diag
        .labels
        .iter()
        .enumerate()
        .filter(|&(i, _)| Some(i) != primary)
        .filter_map(|(_, label)| {
            let placed = place(label.span.file).filter(|p| p.holds(label.span.range))?;
            Some(PlainLabel {
                path: placed.path.to_path_buf(),
                range: label.span.range,
                message: label.message.clone(),
            })
        })
        .collect();
    let helps = diag
        .helps
        .iter()
        .filter_map(|help| {
            let edits = help
                .edits
                .iter()
                .map(|edit| {
                    let placed = place(edit.span.file).filter(|p| p.holds(edit.span.range))?;
                    Some(PlainEdit {
                        path: placed.path.to_path_buf(),
                        range: edit.span.range,
                        replacement: edit.replacement.clone(),
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            Some(PlainHelp {
                message: help.message.clone(),
                edits,
            })
        })
        .collect();
    PlainDiagnostic {
        code: diag.code,
        severity: diag.severity(),
        message: diag.message.clone(),
        range,
        primary_message: primary_label.map(|l| l.message.clone()).unwrap_or_default(),
        related,
        notes: diag.notes.clone(),
        helps,
    }
}

/// The byte-order mark that normalization removes from the start of a text.
const BOM: &str = "\u{feff}";

/// The key of `text` exactly as it is: equal keys mean equal bytes, so
/// offsets agree (D-1393).
#[must_use]
pub fn exact_key(text: &str) -> Hash128 {
    Hash128::of(text.as_bytes())
}

/// The key of `text` normalized: a leading BOM removed and `\r\n` read as
/// `\n` (D-1412). Equal to `exact_key(&normalize(text))`, without the copy.
#[must_use]
pub fn normalized_key(text: &str) -> Hash128 {
    let mut h = SipHasher13::new_with_keys(0, 0);
    for piece in pieces(text) {
        std::hash::Hasher::write(&mut h, piece.as_bytes());
    }
    Hash128(h.finish128().as_bytes())
}

/// `text` with a leading BOM removed and every `\r\n` read as `\n`
/// (D-1412). Borrowed when neither occurs, which is nearly every file.
#[must_use]
pub fn normalize(text: &str) -> Cow<'_, str> {
    let body = text.strip_prefix(BOM).unwrap_or(text);
    if body.contains("\r\n") {
        Cow::Owned(pieces(text).collect())
    } else {
        Cow::Borrowed(body)
    }
}

/// The normalized text in pieces: the text after any BOM, split before
/// each `\r` that a `\n` follows, with that `\r` left out.
fn pieces(text: &str) -> impl Iterator<Item = &str> {
    text.strip_prefix(BOM)
        .unwrap_or(text)
        .split("\r\n")
        .enumerate()
        .flat_map(|(i, piece)| [(i > 0).then_some("\n"), Some(piece)])
        .flatten()
}

/// Maps byte offsets between a live text and its normalized form
/// (D-1412): child results have ranges in normalized text, the editor in
/// live text. The identity for a text with no BOM and no `\r\n`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NormMap {
    /// Whether the live text starts with a BOM, whose bytes go.
    bom: bool,
    /// For each `\r\n`, ascending: the normalized offset of its `\n`, and
    /// the offset of its `\r` in the live text after any BOM.
    joined: Vec<(TextSize, TextSize)>,
    /// The normalized text's length.
    normalized_len: TextSize,
    /// The live text's length.
    live_len: TextSize,
}

/// The BOM's length in bytes.
const BOM_LEN: TextSize = 3;

/// `n` as a [`TextSize`], cut at `u32::MAX`. Source files are never that
/// large (`SourceMap::add_file`).
fn size(n: usize) -> TextSize {
    TextSize::try_from(n).unwrap_or(TextSize::MAX)
}

impl NormMap {
    /// The map of `live`.
    #[must_use]
    pub fn new(live: &str) -> Self {
        let (bom, body) = live
            .strip_prefix(BOM)
            .map_or((false, live), |rest| (true, rest));
        let joined: Vec<(TextSize, TextSize)> = body
            .match_indices("\r\n")
            .enumerate()
            .map(|(k, (at, _))| (size(at.saturating_sub(k)), size(at)))
            .collect();
        Self {
            bom,
            normalized_len: size(body.len().saturating_sub(joined.len())),
            joined,
            live_len: size(live.len()),
        }
    }

    /// Whether the map changes no offset.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        !self.bom && self.joined.is_empty()
    }

    /// The normalized text's length in bytes.
    #[must_use]
    pub const fn normalized_len(&self) -> TextSize {
        self.normalized_len
    }

    /// The live text's length in bytes.
    #[must_use]
    pub const fn live_len(&self) -> TextSize {
        self.live_len
    }

    /// The bytes the BOM takes in the live text.
    const fn bom_len(&self) -> TextSize {
        if self.bom { BOM_LEN } else { 0 }
    }

    /// The live offset of the normalized `offset`. The offset of a `\n`
    /// that was `\r\n` maps before the `\r`, so a range never splits the
    /// pair. `None` past the end.
    #[must_use]
    pub fn to_live(&self, offset: TextSize) -> Option<TextSize> {
        if offset > self.normalized_len {
            return None;
        }
        let before = self.joined.partition_point(|&(norm, _)| norm < offset);
        offset
            .checked_add(self.bom_len())?
            .checked_add(TextSize::try_from(before).ok()?)
    }

    /// [`NormMap::to_live`] of both ends.
    #[must_use]
    pub fn to_live_range(&self, range: TextRange) -> Option<TextRange> {
        Some(TextRange::new(
            self.to_live(range.start())?,
            self.to_live(range.end())?,
        ))
    }

    /// The normalized offset of the live `offset`. An offset inside the BOM
    /// maps to 0, and one between `\r` and `\n` to the `\n`. `None` past
    /// the end.
    #[must_use]
    pub fn to_normalized(&self, offset: TextSize) -> Option<TextSize> {
        if offset > self.live_len {
            return None;
        }
        let body = offset.saturating_sub(self.bom_len());
        let dropped = self.joined.partition_point(|&(_, cr)| cr < body);
        body.checked_sub(TextSize::try_from(dropped).ok()?)
    }

    /// [`NormMap::to_normalized`] of both ends.
    #[must_use]
    pub fn to_normalized_range(&self, range: TextRange) -> Option<TextRange> {
        Some(TextRange::new(
            self.to_normalized(range.start())?,
            self.to_normalized(range.end())?,
        ))
    }
}

/// Builds one key over many parts, each length-prefixed so no two
/// sequences of parts share a key: a run's inputs, or a D run's strict key
/// (A §2.1).
#[derive(Clone, Debug)]
pub struct InputsKey {
    /// The hasher, zero-keyed as [`Hash128::of`] is.
    hasher: SipHasher13,
}

impl Default for InputsKey {
    fn default() -> Self {
        Self::new()
    }
}

impl InputsKey {
    /// A key with no parts yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            hasher: SipHasher13::new_with_keys(0, 0),
        }
    }

    /// Appends a part of raw bytes.
    pub fn bytes(&mut self, part: &[u8]) -> &mut Self {
        let len = u64::try_from(part.len()).unwrap_or(u64::MAX);
        std::hash::Hasher::write(&mut self.hasher, &len.to_le_bytes());
        std::hash::Hasher::write(&mut self.hasher, part);
        self
    }

    /// Appends a string's UTF-8 bytes.
    pub fn str(&mut self, part: &str) -> &mut Self {
        self.bytes(part.as_bytes())
    }

    /// Appends a path's bytes, as the platform encodes them.
    pub fn path(&mut self, part: &Path) -> &mut Self {
        self.bytes(part.as_os_str().as_encoded_bytes())
    }

    /// Appends a file: its path, then its text exactly (D-1393).
    pub fn file(&mut self, path: &Path, text: &str) -> &mut Self {
        self.path(path).str(text)
    }

    /// The key.
    #[must_use]
    pub fn finish(&self) -> Hash128 {
        Hash128(self.hasher.finish128().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use cheby_diag::{Span, codes};

    use super::*;

    const OWNER: FileId = FileId::new(0);
    const SAME: FileId = FileId::new(1);
    const STALE: FileId = FileId::new(2);

    const OWNER_TEXT: &str = "fn main() {\n  x\n}\n";
    const SAME_TEXT: &str = "pub fn f() {}\n";

    fn place(file: FileId) -> Option<Placed<'static>> {
        match file {
            f if f == OWNER => Some(Placed {
                path: Path::new("/p/src/main.cheby"),
                text: OWNER_TEXT,
            }),
            f if f == SAME => Some(Placed {
                path: Path::new("/p/src/same.cheby"),
                text: SAME_TEXT,
            }),
            _ => None,
        }
    }

    fn span(file: FileId, start: TextSize, end: TextSize) -> Span {
        Span::new(file, TextRange::new(start, end))
    }

    /// Every range of `d` lies inside the text of its path.
    fn assert_no_foreign_range(d: &PlainDiagnostic) {
        let text_of = |path: &Path| {
            [OWNER, SAME]
                .into_iter()
                .filter_map(place)
                .find(|p| p.path == path)
                .map(|p| p.text)
        };
        if let Some(range) = d.range {
            assert!(range.slice(OWNER_TEXT).is_some());
        }
        for label in &d.related {
            let text = text_of(&label.path).expect("a placed path");
            assert!(label.range.slice(text).is_some());
        }
        for edit in d.helps.iter().flat_map(|h| &h.edits) {
            let text = text_of(&edit.path).expect("a placed path");
            assert!(edit.range.slice(text).is_some());
        }
    }

    #[test]
    fn plain_keeps_placed_parts_in_order() {
        let diag = Diagnostic::new(codes::SYNTAX_ERROR, "head")
            .with_secondary(span(OWNER, 0, 2), "first")
            .with_primary(span(OWNER, 14, 15), "here")
            .with_primary(span(SAME, 4, 6), "also")
            .with_note("a note")
            .with_help("words only")
            .with_suggestion("fix it", span(OWNER, 14, 15), "y");
        let d = plain(&diag, OWNER, place);
        assert_eq!(d.code, codes::SYNTAX_ERROR);
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.message, "head");
        assert_eq!(d.range, Some(TextRange::new(14, 15)));
        assert_eq!(d.primary_message, "here");
        let related: Vec<(&Path, &str)> = d
            .related
            .iter()
            .map(|l| (l.path.as_path(), l.message.as_str()))
            .collect();
        assert_eq!(
            related,
            vec![
                (Path::new("/p/src/main.cheby"), "first"),
                (Path::new("/p/src/same.cheby"), "also"),
            ]
        );
        assert_eq!(d.notes, vec!["a note".to_owned()]);
        assert_eq!(d.helps.len(), 2);
        assert_eq!(d.helps.first().map(|h| h.edits.len()), Some(0));
        assert_eq!(
            d.helps.get(1).and_then(|h| h.edits.first()),
            Some(&PlainEdit {
                path: PathBuf::from("/p/src/main.cheby"),
                range: TextRange::new(14, 15),
                replacement: "y".to_owned(),
            })
        );
        assert_no_foreign_range(&d);
    }

    #[test]
    fn plain_drops_what_lies_in_other_text() {
        let diag = Diagnostic::new(codes::SYNTAX_ERROR, "head")
            .with_primary(span(OWNER, 14, 15), "here")
            .with_secondary(span(STALE, 0, 3), "in last good text")
            .with_secondary(span(SAME, 0, 400), "past the end")
            .with_suggestion("half a fix", span(OWNER, 14, 15), "y")
            .with_suggestion("stale", span(STALE, 0, 1), "z");
        let mut diag = diag;
        if let Some(help) = diag.helps.first_mut() {
            help.edits.push(cheby_diag::Edit {
                span: span(STALE, 1, 2),
                replacement: "w".to_owned(),
            });
        }
        let d = plain(&diag, OWNER, place);
        assert_eq!(d.range, Some(TextRange::new(14, 15)));
        assert!(d.related.is_empty(), "both labels lie in unplaced text");
        assert!(
            d.helps.is_empty(),
            "a help with any foreign edit goes whole"
        );
        assert_no_foreign_range(&d);
    }

    #[test]
    fn plain_primary_needs_the_owner_and_a_boundary() {
        // Primary in another file than the owner: no range.
        let elsewhere =
            Diagnostic::new(codes::SYNTAX_ERROR, "m").with_primary(span(SAME, 0, 1), "p");
        let d = plain(&elsewhere, OWNER, place);
        assert_eq!(d.range, None);
        assert_eq!(d.primary_message, "p");
        assert!(d.related.is_empty(), "the primary is never a related label");
        // No span at all (A §2.7).
        let none = Diagnostic::new(codes::SYNTAX_ERROR, "m");
        assert_eq!(plain(&none, OWNER, place).range, None);
        // A range that splits a character.
        let text = "é";
        let split = Diagnostic::new(codes::SYNTAX_ERROR, "m").with_primary(span(OWNER, 0, 1), "");
        let d = plain(&split, OWNER, |_| {
            Some(Placed {
                path: Path::new("/x"),
                text,
            })
        });
        assert_eq!(d.range, None);
    }

    #[test]
    fn keys_normalize_bom_and_crlf_only() {
        assert_eq!(exact_key("a\nb"), Hash128::of(b"a\nb"));
        assert_eq!(normalized_key("a\nb"), exact_key("a\nb"));
        for live in ["\u{feff}a\r\nb", "a\r\nb", "\u{feff}a\nb"] {
            assert_eq!(normalized_key(live), exact_key("a\nb"), "{live:?}");
            assert_eq!(normalize(live), "a\nb");
            assert_ne!(exact_key(live), exact_key("a\nb"));
        }
        // A lone `\r`, a BOM not at the start and `\r\r\n` keep their bytes.
        for (live, normal) in [
            ("a\rb", "a\rb"),
            ("a\u{feff}b", "a\u{feff}b"),
            ("a\r\r\nb", "a\r\nb"),
            ("\r\n\r\n", "\n\n"),
            ("", ""),
        ] {
            assert_eq!(normalize(live), normal);
            assert_eq!(normalized_key(live), exact_key(normal), "{live:?}");
        }
        assert!(matches!(normalize("plain\n"), Cow::Borrowed(_)));
        assert!(matches!(normalize("\u{feff}plain\n"), Cow::Borrowed(_)));
    }

    #[test]
    fn norm_map_is_the_identity_on_plain_text() {
        let map = NormMap::new("let x = 1\n");
        assert!(map.is_identity());
        for offset in 0..=10 {
            assert_eq!(map.to_live(offset), Some(offset));
            assert_eq!(map.to_normalized(offset), Some(offset));
        }
        assert_eq!(map.to_live(11), None);
        assert_eq!(map.to_normalized(11), None);
    }

    #[test]
    fn norm_map_round_trips_every_offset() {
        for live in [
            "\u{feff}ab\r\ncd\r\n",
            "\r\n\r\nx",
            "a\r\r\nb\rc\r\n",
            "\u{feff}",
            "x\r\n",
        ] {
            let map = NormMap::new(live);
            let normal = normalize(live);
            assert_eq!(map.normalized_len(), size(normal.len()), "{live:?}");
            assert_eq!(map.live_len(), size(live.len()));
            assert!(!map.is_identity());
            for n in 0..=map.normalized_len() {
                let l = map.to_live(n).expect("in bounds");
                assert_eq!(map.to_normalized(l), Some(n), "{live:?} at {n}");
                // The bytes after each offset agree, so ranges carry over.
                let rest_live =
                    normalize(live.get(usize::try_from(l).unwrap()..).unwrap()).into_owned();
                let rest_norm = normal.get(usize::try_from(n).unwrap()..).unwrap();
                let rest_live = rest_live.strip_prefix(BOM).unwrap_or(&rest_live);
                assert_eq!(rest_live, rest_norm, "{live:?} at {n}");
            }
            assert_eq!(map.to_live(map.normalized_len().saturating_add(1)), None);
        }
    }

    #[test]
    fn norm_map_keeps_crlf_whole() {
        // "ab\r\ncd": normalized "ab\ncd".
        let map = NormMap::new("\u{feff}ab\r\ncd");
        assert_eq!(map.to_live(0), Some(3));
        assert_eq!(
            map.to_live(2),
            Some(5),
            "before the `\\n` is before the `\\r`"
        );
        assert_eq!(map.to_live(3), Some(7), "after the `\\n`");
        assert_eq!(map.to_normalized(1), Some(0), "inside the BOM");
        assert_eq!(map.to_normalized(6), Some(2), "between `\\r` and `\\n`");
        assert_eq!(
            map.to_live_range(TextRange::new(2, 3)),
            Some(TextRange::new(5, 7))
        );
        assert_eq!(
            map.to_normalized_range(TextRange::new(5, 7)),
            Some(TextRange::new(2, 3))
        );
        assert_eq!(map.to_live_range(TextRange::new(0, 9)), None);
    }

    #[test]
    fn inputs_key_separates_parts() {
        let key = |parts: &[&str]| {
            let mut k = InputsKey::new();
            for part in parts {
                k.str(part);
            }
            k.finish()
        };
        assert_eq!(key(&["ab", "c"]), key(&["ab", "c"]));
        assert_ne!(key(&["ab", "c"]), key(&["a", "bc"]));
        assert_ne!(key(&["ab"]), key(&["ab", ""]));
        assert_ne!(key(&[]), key(&[""]));
        let mut a = InputsKey::default();
        a.file(Path::new("/p/a.cheby"), "x");
        let mut b = InputsKey::new();
        b.path(Path::new("/p/a.cheby")).str("x");
        assert_eq!(a.finish(), b.finish());
    }
}
