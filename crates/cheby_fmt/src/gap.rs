//! Comments and blank lines between two parts of a node.
//!
//! The tree builder puts every comment between two significant children
//! of the node that holds the next token (D-280): a node never starts or
//! ends with one. So each layout walks a node's parts and collects what
//! lies between two of them into a [`Gap`], which the layout then prints
//! in its place.
//!
//! D-1160: a comment keeps its place relative to the
//! tokens around it. A comment on the line of the token before it stays at
//! the end of that line; a comment on a line of its own stays on a line of
//! its own before the token after it. Since a `//` comment ends its line
//! (D-078), a gap that holds one always breaks the line after it. That is
//! a legal break: the source had a line break in the same gap, between the
//! same two tokens, and whether a line continues depends only on those
//! tokens and the brackets around them (§2.8, D-107, D-231). The parts of
//! the node after such a break go one level deeper, as a value after `=`
//! does on the next line.
//!
//! D-1161: a blank line between two parts of
//! a node, where the layout has no list, statement or arm to separate, is
//! kept as one blank line, as D-1030 keeps single blank lines; the line
//! breaks there, and what follows goes one level deeper. A blank line
//! before or after a comment of the gap stays where it was.

use cheby_syntax::{SyntaxElement, SyntaxKind};

use crate::doc::Doc;
use crate::layout::{comment_text, is_comment};

/// A comment on a line of its own.
struct OwnLine<'t> {
    text: &'t str,
    /// A blank line before it.
    blank_before: bool,
}

/// What lies between two parts of a node: comments, and line breaks
/// counted to tell a comment at the end of a line from one on a line of
/// its own and to find blank lines.
#[derive(Default)]
pub(crate) struct Gap<'t> {
    /// A comment on the line of the part before the gap.
    trailing: Option<&'t str>,
    /// Comments on lines of their own.
    own: Vec<OwnLine<'t>>,
    /// Line breaks since the part before the gap or the last comment.
    newlines: usize,
}

impl<'t> Gap<'t> {
    /// Counts a line break.
    pub(crate) const fn newline(&mut self) {
        self.newlines = self.newlines.saturating_add(1);
    }

    /// Adds a comment, given without its trailing whitespace.
    pub(crate) fn comment(&mut self, text: &'t str) {
        if self.newlines == 0 && self.trailing.is_none() && self.own.is_empty() {
            self.trailing = Some(text);
        } else {
            self.own.push(OwnLine {
                text,
                blank_before: self.newlines >= 2,
            });
        }
        self.newlines = 0;
    }

    /// Takes `elem` into the gap when it is whitespace, a line break or a
    /// comment; `false` for a part of the node.
    pub(crate) fn take_trivia(&mut self, elem: SyntaxElement<'t>) -> bool {
        let SyntaxElement::Token(token) = elem else {
            return false;
        };
        match token.kind() {
            SyntaxKind::WHITESPACE => true,
            SyntaxKind::NEWLINE => {
                self.newline();
                true
            }
            kind if is_comment(kind) => {
                self.comment(comment_text(token));
                true
            }
            _ => false,
        }
    }

    /// Whether the gap holds a comment.
    pub(crate) const fn has_comment(&self) -> bool {
        self.trailing.is_some() || !self.own.is_empty()
    }

    /// Whether the gap needs a place in the layout: a comment, or a blank
    /// line.
    pub(crate) const fn is_empty(&self) -> bool {
        !self.has_comment() && self.newlines < 2
    }

    /// The gap's document, which ends with a line break, so the next part
    /// starts a line; `None` when the gap holds no comment and no blank
    /// line, and the layout decides. With `at_start`, nothing comes before
    /// the gap on its line, so a comment there is written without a line
    /// break before it.
    pub(crate) fn doc(&self, at_start: bool) -> Option<Doc> {
        if self.is_empty() {
            return None;
        }
        let mut docs = Vec::new();
        if let Some(text) = self.trailing {
            if !at_start {
                docs.push(Doc::text(" "));
            }
            docs.push(Doc::comment(text));
        }
        for (i, line) in self.own.iter().enumerate() {
            let first = i == 0 && self.trailing.is_none();
            if !(first && at_start) {
                docs.push(Doc::hardline());
                if line.blank_before {
                    docs.push(Doc::hardline());
                }
            }
            docs.push(Doc::comment(line.text));
        }
        docs.push(Doc::hardline());
        if self.newlines >= 2 {
            docs.push(Doc::hardline());
        }
        Some(Doc::concat(docs))
    }
}

/// The parts of a node and the docs of its gaps, assembled: the parts up
/// to the first gap that breaks the line as they are, and the rest one
/// level deeper (D-1160).
#[derive(Default)]
pub(crate) struct Parts {
    docs: Vec<Doc>,
    /// Where the first gap that breaks the line starts.
    split: Option<usize>,
}

impl Parts {
    /// Adds the document of a gap; returns whether it breaks the line.
    pub(crate) fn gap(&mut self, gap: &Gap<'_>, at_start: bool) -> bool {
        let Some(doc) = gap.doc(at_start) else {
            return false;
        };
        if self.split.is_none() {
            self.split = Some(self.docs.len());
        }
        self.docs.push(doc);
        true
    }

    /// Adds a part.
    pub(crate) fn push(&mut self, doc: Doc) {
        self.docs.push(doc);
    }

    /// The parts, nested from the first gap that breaks on.
    pub(crate) fn finish(mut self, indent: usize) -> Doc {
        match self.split {
            None => Doc::concat(self.docs),
            Some(at) => {
                let rest = self.docs.split_off(at.min(self.docs.len()));
                self.docs.push(Doc::nest(indent, Doc::concat(rest)));
                Doc::concat(self.docs)
            }
        }
    }
}
