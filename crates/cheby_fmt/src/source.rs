//! Source text as the printer sees it: verbatim fragments with their line
//! structure, and the line breaks that belong to string literals.
//!
//! A string literal may span lines, and its line breaks and the whitespace
//! around them are part of its value (§2.5.3, D-114, D-431). Those line
//! breaks are recorded once per file, so a fragment never re-indents a line
//! that starts inside a string or trims one that ends inside it. A CRLF
//! line break is written as LF everywhere: inside a string the value is the
//! same (D-114, D-431).

use cheby_syntax::{SyntaxKind, SyntaxNode, SyntaxTree};

use crate::doc::{Doc, Verbatim, VerbatimLine};

/// Columns a tab counts for when measuring indentation.
const TAB_WIDTH: usize = 2;

/// The text of a file and where its string literals hold line breaks.
pub(crate) struct Source<'t> {
    text: &'t str,
    /// Byte offsets of the `\n` characters inside string literals, sorted.
    string_breaks: Vec<usize>,
}

impl<'t> Source<'t> {
    pub(crate) fn new(tree: &'t SyntaxTree) -> Self {
        let text = tree.text();
        let mut string_breaks = Vec::new();
        for token in tree.root().tokens() {
            if token.kind() == SyntaxKind::NEWLINE {
                continue;
            }
            let start = to_usize(token.text_range().start());
            for (i, c) in token.text().char_indices() {
                if c == '\n' {
                    string_breaks.push(start.saturating_add(i));
                }
            }
        }
        Self {
            text,
            string_breaks,
        }
    }

    /// Whether the `\n` at byte `at` belongs to a string literal.
    fn in_string(&self, at: usize) -> bool {
        self.string_breaks.binary_search(&at).is_ok()
    }

    /// The text of `start..end`, or nothing for a range out of bounds.
    fn slice(&self, start: usize, end: usize) -> &'t str {
        self.text.get(start..end).unwrap_or("")
    }

    /// The indentation, in columns, of the line that holds byte `at`.
    pub(crate) fn line_indent(&self, at: usize) -> usize {
        let before = self.slice(0, at);
        let line_start = before.rfind('\n').map_or(0, |i| i.saturating_add(1));
        indent_width(self.slice(line_start, at))
    }

    /// The indentation of the line where `node` starts.
    pub(crate) fn node_line_indent(&self, node: SyntaxNode<'_>) -> usize {
        self.line_indent(to_usize(node.text_range().start()))
    }

    /// `node`'s text as a verbatim fragment, its later lines indented
    /// relative to `anchor`, the source indentation that maps to the
    /// printer's current nesting.
    pub(crate) fn node(&self, node: SyntaxNode<'_>, anchor: usize) -> Doc {
        let range = node.text_range();
        self.fragment(to_usize(range.start()), to_usize(range.end()), anchor)
    }

    /// The text of `start..end` as a verbatim fragment: see [`Self::node`].
    /// Runs of blank lines outside string literals collapse to one.
    pub(crate) fn fragment(&self, start: usize, end: usize, anchor: usize) -> Doc {
        let mut lines = Vec::new();
        let mut line_start = start;
        let text = self.slice(start, end);
        for (i, c) in text.char_indices() {
            if c == '\n' {
                let at = start.saturating_add(i);
                lines.push((line_start, at, self.in_string(at)));
                line_start = at.saturating_add(1);
            }
        }
        lines.push((line_start, end, false));

        let mut rest: Vec<VerbatimLine> = Vec::new();
        let mut first = String::new();
        let mut first_ends_in_string = false;
        let mut prev_in_string = false;
        for (n, &(from, to, ends_in_string)) in lines.iter().enumerate() {
            let raw = strip_cr(self.slice(from, to));
            if n == 0 {
                first = if ends_in_string {
                    raw.to_owned()
                } else {
                    raw.trim_end_matches([' ', '\t']).to_owned()
                };
                first_ends_in_string = ends_in_string;
            } else if prev_in_string {
                rest.push(VerbatimLine {
                    relative_indent: 0,
                    text: raw.to_owned(),
                    starts_in_string: true,
                    ends_in_string,
                });
            } else {
                let body = raw.trim_start_matches([' ', '\t']);
                let indent = indent_width(raw);
                let body = if ends_in_string {
                    body
                } else {
                    body.trim_end_matches([' ', '\t'])
                };
                let blank = body.is_empty() && !ends_in_string;
                let prev_blank = rest
                    .last()
                    .is_some_and(|l| l.text.is_empty() && !l.starts_in_string);
                if !(blank && prev_blank) {
                    rest.push(VerbatimLine {
                        relative_indent: signed(indent).saturating_sub(signed(anchor)),
                        text: body.to_owned(),
                        starts_in_string: false,
                        ends_in_string,
                    });
                }
            }
            prev_in_string = ends_in_string;
        }
        if first.is_empty() && rest.is_empty() {
            return Doc::Nil;
        }
        Doc::verbatim(Verbatim {
            first,
            first_ends_in_string,
            rest,
        })
    }
}

/// `text` without a final `\r` (the first half of a CRLF).
fn strip_cr(text: &str) -> &str {
    text.strip_suffix('\r').unwrap_or(text)
}

/// The width in columns of the leading spaces and tabs of `text`.
fn indent_width(text: &str) -> usize {
    text.chars()
        .take_while(|c| matches!(c, ' ' | '\t'))
        .map(|c| if c == '\t' { TAB_WIDTH } else { 1 })
        .sum()
}

/// A text offset as an index.
pub(crate) fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// A column count as a signed number, saturating.
fn signed(n: usize) -> isize {
    isize::try_from(n).unwrap_or(isize::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cheby_diag::FileId;

    use super::Source;
    use crate::doc::print;

    fn fragment(text: &str, start: usize, end: usize, anchor: usize) -> String {
        let parse = cheby_syntax::parse(FileId::new(0), Arc::from(text));
        let source = Source::new(parse.tree());
        print(&source.fragment(start, end, anchor), 80)
    }

    #[test]
    fn fmt_trivia_fragment_keeps_relative_lines() {
        let text = "fn f() {\n    x  \n\n\n    y\n}\n";
        let start = text.find('{').unwrap();
        let end = text.rfind('}').unwrap() + 1;
        assert_eq!(fragment(text, start, end, 0), "{\n    x\n\n    y\n}");
    }

    #[test]
    fn fmt_trivia_fragment_keeps_multiline_strings() {
        let text = "fn f() {\n  \"a  \n   b\"\n}\n";
        let start = text.find('{').unwrap();
        let end = text.rfind('}').unwrap() + 1;
        assert_eq!(fragment(text, start, end, 0), "{\n  \"a  \n   b\"\n}");
    }

    #[test]
    fn fmt_trivia_fragment_writes_lf() {
        let text = "fn f() {\r\n  x\r\n}\r\n";
        let start = text.find('{').unwrap();
        let end = text.rfind('}').unwrap() + 1;
        assert_eq!(fragment(text, start, end, 0), "{\n  x\n}");
    }
}
