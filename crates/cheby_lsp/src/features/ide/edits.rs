//! Text edits: line diffs and `cheby fmt`'s form (X §1.5, "Edits").
//!
//! Formatting goes through the formatter `cheby fmt` runs,
//! [`cheby_fmt::format_source`] at [`cheby_fmt::WIDTH`], with no options
//! (D-079), and reaches the client as a line diff of the text against the
//! formatter's output, so cursors and marks outside the changed lines stay
//! put. Range formatting keeps only the hunks that touch the range
//! (D-1399).
//!
//! Line endings: the formatter writes LF, and so do the edits of whole-file
//! formatting. A CRLF text is converted by the diff, as `cheby fmt`
//! converts it (X §5.4, §13.7), so a file formatted in the editor passes
//! `cheby fmt --check`. Its lines all differ from the formatter's, so the
//! diff is one hunk over the whole text. Range formatting compares lines
//! with a CR before the LF ignored, so its hunks stay local, and writes a
//! kept hunk with CRLF where the lines it replaces have CRLF (D-1444).

use std::collections::BTreeSet;
use std::ops::Range;

use cheby_driver::analysis::{TextRange, TextSize};

/// Above this many cells, [`line_diff`] gives up on a minimal diff and
/// replaces the whole changed middle with one hunk (X §1.5).
const MAX_CELLS: usize = 4_000_000;

/// A run of whole lines of the old text replaced by whole lines of the new
/// one. A line includes its line break.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hunk {
    /// The replaced lines of the old text, by index from 0. Empty for an
    /// insertion before line `old_lines.start`.
    pub old_lines: Range<usize>,
    /// The bytes of those lines in the old text.
    pub old: TextRange,
    /// The bytes of the lines that replace them in the new text.
    pub new: TextRange,
}

/// One replacement in a text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextChange {
    /// The replaced bytes.
    pub range: TextRange,
    /// What replaces them.
    pub text: String,
}

/// `text` in `cheby fmt`'s form, with LF line breaks. `None` when the
/// text has a grammar error, which the formatter refuses (§13.7).
#[must_use]
pub(crate) fn formatted(text: &str) -> Option<String> {
    cheby_fmt::format_source(text, cheby_fmt::WIDTH).ok()
}

/// The edits that bring `text` to [`formatted`]'s form, one per hunk of
/// the line diff, ascending. Empty when the text is formatted, `None` when
/// it has a grammar error.
#[must_use]
pub(crate) fn format_edits(text: &str) -> Option<Vec<TextChange>> {
    touching(text, &[TextRange::new(0, size(text.len()))], false)
}

/// The hunks of [`format_edits`] that touch the lines of a range in
/// `touched`, as edits (D-1399). A hunk is kept or dropped whole, so a
/// kept hunk may also change a neighbouring line the formatter joined to
/// it. `None` when the text has a grammar error.
///
/// The formatter sorts the imports of a block (D-1148), and a diff shows a
/// moved import as one hunk that deletes it and another that inserts it.
/// Either alone would drop the import or write it twice, so a kept hunk
/// keeps every hunk that deletes or inserts one of its import lines.
///
/// Lines are compared with a CR before their LF ignored, and a kept hunk
/// is written with CRLF when the lines it replaces end in CRLF, so range
/// formatting a CRLF file changes only the hunks it keeps (D-1444). A kept
/// hunk is taken whole, so it can reach a neighbouring line or item, such
/// as the blank lines above a reformatted function.
#[must_use]
pub(crate) fn format_touching(text: &str, touched: &[TextRange]) -> Option<Vec<TextChange>> {
    touching(text, touched, true)
}

/// [`format_touching`], or with `keep_crlf` false the whole-file rule of
/// [`format_edits`], which brings every line to LF.
fn touching(text: &str, touched: &[TextRange], keep_crlf: bool) -> Option<Vec<TextChange>> {
    let new = formatted(text)?;
    let starts = line_starts(text);
    let spans: Vec<Range<usize>> = touched.iter().map(|&r| lines_of(&starts, r)).collect();
    // Dropping the CR of each CRLF keeps every line where it was, so the
    // hunks' line numbers hold for `text` too (D-1444).
    let lf;
    let old = if keep_crlf && text.contains("\r\n") {
        lf = text.replace("\r\n", "\n");
        lf.as_str()
    } else {
        text
    };
    let hunks = line_diff(old, &new);
    let mut keep: Vec<bool> = hunks
        .iter()
        .map(|hunk| spans.iter().any(|span| touches(&hunk.old_lines, span)))
        .collect();
    loop {
        let moved: BTreeSet<&str> = hunks
            .iter()
            .zip(&keep)
            .filter(|&(_, &kept)| kept)
            .flat_map(|(hunk, _)| import_lines(hunk, old, &new))
            .collect();
        let mut grew = false;
        for (hunk, kept) in hunks.iter().zip(keep.iter_mut()) {
            if !*kept && import_lines(hunk, old, &new).any(|line| moved.contains(line)) {
                *kept = true;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    let changes = hunks
        .into_iter()
        .zip(keep)
        .filter(|&(_, kept)| kept)
        .map(|(hunk, _)| {
            let inserted = hunk.new.slice(&new).unwrap_or_default();
            TextChange {
                range: byte_range(&starts, &hunk.old_lines),
                text: if keep_crlf && ends_in_crlf(text, &starts, &hunk.old_lines) {
                    inserted.replace('\n', "\r\n")
                } else {
                    inserted.to_owned()
                },
            }
        })
        .collect();
    Some(changes)
}

/// Whether the first line that `lines` replaces in `text` ends in CRLF; for
/// an insertion, the line before it, else the line it goes before.
fn ends_in_crlf(text: &str, starts: &[TextSize], lines: &Range<usize>) -> bool {
    let line = if lines.is_empty() {
        lines.start.checked_sub(1).unwrap_or(lines.start)
    } else {
        lines.start
    };
    byte_range(starts, &(line..line.saturating_add(1)))
        .slice(text)
        .is_some_and(|line| line.ends_with("\r\n"))
}

/// The lines that `hunk` deletes from `old` or inserts from `new` and that
/// start an import, without their line breaks.
fn import_lines<'a>(hunk: &Hunk, old: &'a str, new: &'a str) -> impl Iterator<Item = &'a str> {
    let deleted = hunk.old.slice(old).unwrap_or_default();
    let inserted = hunk.new.slice(new).unwrap_or_default();
    deleted
        .lines()
        .chain(inserted.lines())
        .filter(|line| line.trim_start().starts_with("import "))
}

/// The hunks that turn `old` into `new`, by lines: ascending, never
/// overlapping, never touching each other, and minimal in the lines they
/// replace. The common first and last lines are trimmed, then the middle
/// is diffed by a longest common subsequence of lines. A middle of more
/// than 4,000,000 line pairs becomes one hunk (X §1.5).
#[must_use]
pub(crate) fn line_diff(old: &str, new: &str) -> Vec<Hunk> {
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let a_rest = a.get(prefix..).unwrap_or_default();
    let b_rest = b.get(prefix..).unwrap_or_default();
    let suffix = a_rest
        .iter()
        .rev()
        .zip(b_rest.iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let a_mid = a_rest
        .get(..a_rest.len().saturating_sub(suffix))
        .unwrap_or_default();
    let b_mid = b_rest
        .get(..b_rest.len().saturating_sub(suffix))
        .unwrap_or_default();
    let middle = match (a_mid.is_empty(), b_mid.is_empty()) {
        (true, true) => Vec::new(),
        (true, false) | (false, true) => vec![(0..a_mid.len(), 0..b_mid.len())],
        (false, false) => lcs_hunks(a_mid, b_mid),
    };
    let a_starts = line_starts(old);
    let b_starts = line_starts(new);
    middle
        .into_iter()
        .map(|(x, y)| {
            let old_lines = shift(x, prefix);
            let new_lines = shift(y, prefix);
            Hunk {
                old: byte_range(&a_starts, &old_lines),
                new: byte_range(&b_starts, &new_lines),
                old_lines,
            }
        })
        .collect()
}

/// The hunks between two line lists that share no first or last line,
/// by indices into each.
fn lcs_hunks(old: &[&str], new: &[&str]) -> Vec<(Range<usize>, Range<usize>)> {
    let (old_len, new_len) = (old.len(), new.len());
    if old_len
        .checked_mul(new_len)
        .is_none_or(|cells| cells > MAX_CELLS)
    {
        return vec![(0..old_len, 0..new_len)];
    }
    let table = lcs_table(old, new);
    let at = |i: usize, j: usize| -> u32 {
        table
            .get(i)
            .and_then(|row| row.get(j))
            .copied()
            .unwrap_or(0)
    };
    let mut hunks = Vec::new();
    let mut open: Option<(usize, usize)> = None;
    let (mut i, mut j) = (0, 0);
    while i < old_len || j < new_len {
        let same = matches!((old.get(i), new.get(j)), (Some(x), Some(y)) if x == y);
        if same {
            if let Some((si, sj)) = open.take() {
                hunks.push((si..i, sj..j));
            }
            i = i.saturating_add(1);
            j = j.saturating_add(1);
            continue;
        }
        open.get_or_insert((i, j));
        let delete = j >= new_len
            || (i < old_len && at(i.saturating_add(1), j) >= at(i, j.saturating_add(1)));
        if delete {
            i = i.saturating_add(1);
        } else {
            j = j.saturating_add(1);
        }
    }
    if let Some((si, sj)) = open {
        hunks.push((si..old_len, sj..new_len));
    }
    hunks
}

/// `table[i][j]`: the length of a longest common subsequence of `a[i..]`
/// and `b[j..]`, with a row and a column of zeros at the ends.
fn lcs_table(a: &[&str], b: &[&str]) -> Vec<Vec<u32>> {
    let width = b.len().saturating_add(1);
    let mut rows: Vec<Vec<u32>> = Vec::with_capacity(a.len().saturating_add(1));
    let mut below = vec![0_u32; width];
    rows.push(below.clone());
    for x in a.iter().rev() {
        let mut row = vec![0_u32; width];
        for (j, y) in b.iter().enumerate().rev() {
            let next = j.saturating_add(1);
            let value = if x == y {
                below.get(next).copied().unwrap_or(0).saturating_add(1)
            } else {
                let down = below.get(j).copied().unwrap_or(0);
                let right = row.get(next).copied().unwrap_or(0);
                down.max(right)
            };
            if let Some(cell) = row.get_mut(j) {
                *cell = value;
            }
        }
        rows.push(row.clone());
        below = row;
    }
    rows.reverse();
    rows
}

/// Where each line of `text` starts, by the lines of
/// `str::split_inclusive('\n')`, then the text's length.
fn line_starts(text: &str) -> Vec<TextSize> {
    let mut starts = vec![0];
    let mut at: TextSize = 0;
    for line in text.split_inclusive('\n') {
        at = at.saturating_add(size(line.len()));
        starts.push(at);
    }
    starts
}

/// The lines, as in [`line_starts`], that `range` covers: those it holds a
/// byte of, or, when it is empty, the line it sits on. A non-empty range
/// that ends at a line's start does not cover that line.
fn lines_of(starts: &[TextSize], range: TextRange) -> Range<usize> {
    let line = |offset: TextSize| {
        starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1)
    };
    let first = line(range.start());
    let last = if range.is_empty() {
        first
    } else {
        line(range.end().saturating_sub(1))
    };
    first..last.saturating_add(1)
}

/// Whether a hunk over `hunk` old lines touches the lines `span`. An
/// insertion touches the lines on either side of it.
fn touches(hunk: &Range<usize>, span: &Range<usize>) -> bool {
    if hunk.is_empty() {
        span.start <= hunk.start && hunk.start <= span.end
    } else {
        hunk.start < span.end && span.start < hunk.end
    }
}

/// The bytes of `lines`, by `starts` from [`line_starts`].
fn byte_range(starts: &[TextSize], lines: &Range<usize>) -> TextRange {
    let end_of_text = starts.last().copied().unwrap_or(0);
    let at = |line: usize| starts.get(line).copied().unwrap_or(end_of_text);
    TextRange::new(at(lines.start), at(lines.end))
}

/// `range` moved up by `by`.
fn shift(range: Range<usize>, by: usize) -> Range<usize> {
    range.start.saturating_add(by)..range.end.saturating_add(by)
}

/// A byte count as a [`TextSize`]. Texts are at most `u32::MAX` bytes.
fn size(n: usize) -> TextSize {
    TextSize::try_from(n).unwrap_or(TextSize::MAX)
}

/// `text` with `changes` applied, which must be ascending and must not
/// overlap.
#[cfg(test)]
pub(crate) fn apply(text: &str, changes: &[TextChange]) -> String {
    let mut out = String::new();
    let mut at = 0;
    for change in changes {
        let start = usize::try_from(change.range.start()).unwrap_or(usize::MAX);
        let end = usize::try_from(change.range.end()).unwrap_or(usize::MAX);
        out.push_str(text.get(at..start).unwrap_or_default());
        out.push_str(&change.text);
        at = end;
    }
    out.push_str(text.get(at..).unwrap_or_default());
    out
}

#[cfg(test)]
mod tests {
    use cheby_driver::analysis::TextRange;
    use proptest::prelude::*;

    use super::{Hunk, TextChange, apply, format_edits, format_touching, formatted, line_diff};

    /// `old` with `hunks` applied, taking the new lines from `new`.
    fn patch(old: &str, new: &str, hunks: &[Hunk]) -> String {
        let changes: Vec<TextChange> = hunks
            .iter()
            .map(|h| TextChange {
                range: h.old,
                text: h.new.slice(new).expect("a range of new").to_owned(),
            })
            .collect();
        apply(old, &changes)
    }

    /// The hunks as (old lines, new text) pairs.
    fn shape<'a>(new: &'a str, hunks: &[Hunk]) -> Vec<(std::ops::Range<usize>, &'a str)> {
        hunks
            .iter()
            .map(|h| (h.old_lines.clone(), h.new.slice(new).expect("a range")))
            .collect()
    }

    #[test]
    fn line_diff_of_equal_texts_is_empty() {
        assert_eq!(line_diff("", ""), []);
        assert_eq!(line_diff("a\nb\n", "a\nb\n"), []);
    }

    #[test]
    fn line_diff_is_minimal() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nB\nc\nd\nx\ne\n";
        let hunks = line_diff(old, new);
        assert_eq!(shape(new, &hunks), [(1..2, "B\n"), (4..4, "x\n")]);
        assert_eq!(patch(old, new, &hunks), new);
    }

    #[test]
    fn line_diff_handles_ends() {
        let cases = [
            ("", "a\n"),
            ("a\n", ""),
            ("a", "a\n"),
            ("a\nb", "a\nc"),
            ("x\na\n", "a\n"),
            ("a\n", "a\nb\n"),
        ];
        for (old, new) in cases {
            let hunks = line_diff(old, new);
            assert_eq!(patch(old, new, &hunks), new, "{old:?} -> {new:?}");
            assert_ne!(hunks.len(), 0);
        }
    }

    #[test]
    fn line_diff_counts_carriage_returns() {
        let hunks = line_diff("a\r\nb\r\n", "a\nb\r\n");
        assert_eq!(shape("a\nb\r\n", &hunks), [(0..1, "a\n")]);
    }

    proptest! {
        #[test]
        fn line_diff_round_trips(
            old in prop::collection::vec("[abc]{0,2}\n?", 0..12),
            new in prop::collection::vec("[abc]{0,2}\n?", 0..12),
        ) {
            let (old, new) = (old.concat(), new.concat());
            let hunks = line_diff(&old, &new);
            prop_assert_eq!(patch(&old, &new, &hunks), new.clone());
            for pair in hunks.windows(2) {
                prop_assert!(pair[0].old_lines.end < pair[1].old_lines.start);
                prop_assert!(pair[0].old.end() < pair[1].old.start());
            }
            for hunk in &hunks {
                prop_assert!(!(hunk.old.is_empty() && hunk.new.is_empty()));
            }
        }
    }

    #[test]
    fn a_large_middle_is_one_hunk() {
        let old = (0..2001)
            .map(|i| format!("a{i}\n"))
            .collect::<Vec<_>>()
            .concat();
        let new = (0..2001)
            .map(|i| format!("b{i}\n"))
            .collect::<Vec<_>>()
            .concat();
        let hunks = line_diff(&old, &new);
        assert_eq!(hunks.len(), 1);
        assert_eq!(patch(&old, &new, &hunks), new);
    }

    const MESSY: &str = "fn  main( ) {\n  let x =1\n  x\n}\n\n\n\nfn f() -> Int { 2 }\n";

    #[test]
    fn format_edits_reach_cheby_fmt_byte_for_byte() {
        let want = cheby_fmt::format_source(MESSY, cheby_fmt::WIDTH).expect("formats");
        let edits = format_edits(MESSY).expect("formats");
        assert_ne!(edits.len(), 0);
        assert_eq!(apply(MESSY, &edits), want);
        assert_eq!(format_edits(&want), Some(vec![]));
    }

    #[test]
    fn a_grammar_error_gives_none() {
        assert_eq!(format_edits("fn f() {\n  x.\n}\n"), None);
        assert_eq!(format_touching("fn f( {\n", &[TextRange::new(0, 1)]), None);
    }

    /// X §5.4: CRLF is converted, as `cheby fmt` converts it.
    #[test]
    fn crlf_text_is_converted() {
        let crlf = MESSY.replace('\n', "\r\n");
        let want = cheby_fmt::format_source(MESSY, cheby_fmt::WIDTH).expect("formats");
        let edits = format_edits(&crlf).expect("formats");
        for edit in &edits {
            assert!(!edit.text.contains('\r'), "{edit:?}");
        }
        assert_eq!(apply(&crlf, &edits), want);
        let formatted_crlf = want.replace('\n', "\r\n");
        let edits = format_edits(&formatted_crlf).expect("formats");
        assert_ne!(edits, vec![]);
        assert_eq!(apply(&formatted_crlf, &edits), want);
    }

    #[test]
    fn a_mixed_text_formats_to_lf() {
        let mixed = "fn main() {\r\n  1\n}\r\n";
        let edits = format_edits(mixed).expect("formats");
        assert_eq!(apply(mixed, &edits), "fn main() {\n  1\n}\n");
    }

    #[test]
    fn non_ascii_text_formats() {
        let text = "fn main() {\n  let s =  \"é🙂\"\n  s\n}\n";
        let edits = format_edits(text).expect("formats");
        assert_eq!(
            apply(text, &edits),
            "fn main() {\n  let s = \"é🙂\"\n  s\n}\n"
        );
    }

    #[test]
    fn format_touching_leaves_other_lines_alone() {
        let text = "fn a() {  1 }\n\nfn b() {  2 }\n\nfn c() {  3 }\n";
        let line_b = TextRange::new(15, 20);
        assert_eq!(line_b.slice(text), Some("fn b("));
        let edits = format_touching(text, &[line_b]).expect("formats");
        let got = apply(text, &edits);
        assert_eq!(got, "fn a() {  1 }\n\nfn b() {\n  2\n}\n\nfn c() {  3 }\n");
        let whole = formatted(text).expect("formats");
        assert_ne!(got, whole);
    }

    #[test]
    fn a_range_ending_at_a_line_start_leaves_that_line() {
        let text = "fn a() {  1 }\n\nfn b() {  2 }\n";
        let first_line = TextRange::new(0, 14);
        let edits = format_touching(text, &[first_line]).expect("formats");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits.first().map(|e| e.range), Some(first_line));
        let blank_line = TextRange::empty(14);
        assert_eq!(format_touching(text, &[blank_line]), Some(vec![]));
    }

    #[test]
    fn a_moved_import_keeps_both_halves() {
        let text = "import std::list\nimport std::io\n\nfn main() {\n  1\n}\n";
        let want = "import std::io\nimport std::list\n\nfn main() {\n  1\n}\n";
        assert_eq!(formatted(text).as_deref(), Some(want));
        for line in [0, 1] {
            let range = TextRange::empty(if line == 0 { 0 } else { 17 });
            let edits = format_touching(text, &[range]).expect("formats");
            assert_eq!(apply(text, &edits), want, "line {line}");
        }
        let body = TextRange::empty(40);
        assert_eq!(format_touching(text, &[body]), Some(vec![]));
    }

    #[test]
    fn crlf_range_formatting_stays_local() {
        let text = "fn a() {  1 }\r\n\r\nfn b() {  2 }\r\n";
        let line_b = TextRange::empty(17);
        let edits = format_touching(text, &[line_b]).expect("formats");
        assert_eq!(edits.len(), 1, "{edits:?}");
        assert_eq!(
            apply(text, &edits),
            "fn a() {  1 }\r\n\r\nfn b() {\r\n  2\r\n}\r\n"
        );
        // Whole-file formatting still converts to LF (X §5.4).
        let whole = format_edits(text).expect("formats");
        assert_eq!(
            apply(text, &whole),
            "fn a() {\n  1\n}\n\nfn b() {\n  2\n}\n"
        );
    }
}
