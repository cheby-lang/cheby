//! The document algebra and its printer, driven by one line width (D-1030).
//!
//! A [`Doc`] describes text with places where a line may break. A
//! [`Doc::group`] is printed flat, every [`Doc::line`] as a space and every
//! [`Doc::softline`] as nothing, when its contents fit in what is left of
//! the line up to the next place where the line breaks anyway; otherwise its
//! lines become line breaks at the group's nesting. A [`Doc::hardline`]
//! always breaks, and so does every group around it.
//!
//! [`Doc::verbatim`] carries source text that keeps its own line structure:
//! its first line continues the current line, and each later line starts
//! at the current nesting plus the line's source indentation relative to
//! the fragment's anchor (the source indentation that maps to that
//! nesting). Lines that start inside a string literal are written exactly
//! as they are, and lines that end inside one keep their trailing
//! whitespace.
//!
//! The printer never writes trailing whitespace outside string literals,
//! never writes indentation on an empty line, and ends every line with LF.

use std::rc::Rc;

/// One line of a verbatim fragment after its first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerbatimLine {
    /// The line's indentation relative to the fragment's first source line,
    /// in columns. Ignored when `starts_in_string` is set.
    pub relative_indent: isize,
    /// The line without its indentation (all of it when `starts_in_string`).
    pub text: String,
    /// The line starts inside a string literal: it is written exactly.
    pub starts_in_string: bool,
    /// The line ends inside a string literal: its trailing whitespace stays.
    pub ends_in_string: bool,
}

/// A fragment of source text printed with its own line structure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verbatim {
    /// The first line, which continues the current output line.
    pub first: String,
    /// Whether the first line ends inside a string literal.
    pub first_ends_in_string: bool,
    /// The later lines.
    pub rest: Vec<VerbatimLine>,
}

/// A document: text and the places where it may break.
#[derive(Clone, Debug)]
pub enum Doc {
    /// Nothing.
    Nil,
    /// Text without a line break.
    Text(Rc<str>),
    /// A space when flat, a line break when broken.
    Line,
    /// Nothing when flat, a line break when broken.
    SoftLine,
    /// Always a line break.
    HardLine,
    /// The contents with their line breaks indented by more columns.
    Nest(usize, Rc<Doc>),
    /// Contents printed flat if they fit; `true` when they must break
    /// because they contain a hard line break.
    Group(Rc<Doc>, bool),
    /// The first document when the enclosing group is broken, the second
    /// when it is flat.
    IfBreak(Rc<Doc>, Rc<Doc>),
    /// Documents one after the other.
    Concat(Rc<[Doc]>),
    /// A comment at the end of a line: printed as text, but its width
    /// never makes a group break, since it cannot move.
    Comment(Rc<str>),
    /// A source fragment with its own line structure.
    Verbatim(Rc<Verbatim>),
}

impl Doc {
    /// Text without a line break.
    #[must_use]
    pub fn text(text: &str) -> Self {
        if text.is_empty() {
            Self::Nil
        } else {
            Self::Text(Rc::from(text))
        }
    }

    /// A space when flat, a line break when broken.
    #[must_use]
    pub const fn line() -> Self {
        Self::Line
    }

    /// Nothing when flat, a line break when broken.
    #[must_use]
    pub const fn softline() -> Self {
        Self::SoftLine
    }

    /// Always a line break.
    #[must_use]
    pub const fn hardline() -> Self {
        Self::HardLine
    }

    /// `doc` with its line breaks indented by `by` more columns.
    #[must_use]
    pub fn nest(by: usize, doc: Self) -> Self {
        Self::Nest(by, Rc::new(doc))
    }

    /// `doc` printed flat if it fits in the rest of the line.
    #[must_use]
    pub fn group(doc: Self) -> Self {
        let breaks = doc.has_hard_break();
        Self::Group(Rc::new(doc), breaks)
    }

    /// `broken` inside a broken group, `flat` inside a flat one.
    #[must_use]
    pub fn if_break(broken: Self, flat: Self) -> Self {
        Self::IfBreak(Rc::new(broken), Rc::new(flat))
    }

    /// The documents one after the other.
    #[must_use]
    pub fn concat(docs: Vec<Self>) -> Self {
        Self::Concat(Rc::from(docs))
    }

    /// A comment that ends its line.
    #[must_use]
    pub fn comment(text: &str) -> Self {
        Self::Comment(Rc::from(text))
    }

    /// A source fragment with its own line structure.
    #[must_use]
    pub fn verbatim(verbatim: Verbatim) -> Self {
        Self::Verbatim(Rc::new(verbatim))
    }

    /// Whether printing this document always breaks a line.
    #[must_use]
    pub fn has_hard_break(&self) -> bool {
        match self {
            Self::Nil | Self::Text(_) | Self::Comment(_) | Self::Line | Self::SoftLine => false,
            Self::HardLine => true,
            Self::Nest(_, doc) => doc.has_hard_break(),
            Self::Group(_, breaks) => *breaks,
            Self::IfBreak(broken, flat) => broken.has_hard_break() || flat.has_hard_break(),
            Self::Concat(docs) => docs.iter().any(Self::has_hard_break),
            Self::Verbatim(v) => !v.rest.is_empty(),
        }
    }
}

/// How a document is being printed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Flat,
    Break,
}

/// One pending piece of work: a document at a nesting and in a mode.
#[derive(Clone, Copy)]
struct Cmd<'d> {
    indent: usize,
    mode: Mode,
    doc: &'d Doc,
}

/// The number of columns `text` takes.
fn width(text: &str) -> usize {
    text.chars().count()
}

/// Prints `doc` at line width `width`. The result ends with one line break
/// when `doc` ends with one, and never has trailing whitespace outside
/// string literals.
#[must_use]
pub fn print(doc: &Doc, width: usize) -> String {
    let mut printer = Printer {
        out: String::new(),
        width,
        col: 0,
        pending_indent: None,
    };
    printer.run(doc);
    printer.out
}

struct Printer {
    out: String,
    width: usize,
    /// The current column.
    col: usize,
    /// Indentation still to be written before the next text of this line.
    pending_indent: Option<usize>,
}

impl Printer {
    fn run(&mut self, doc: &Doc) {
        let mut stack = vec![Cmd {
            indent: 0,
            mode: Mode::Break,
            doc,
        }];
        while let Some(cmd) = stack.pop() {
            match cmd.doc {
                Doc::Nil => {}
                Doc::Text(text) | Doc::Comment(text) => self.write(text),
                Doc::Line => match cmd.mode {
                    Mode::Flat => self.write(" "),
                    Mode::Break => self.newline(cmd.indent),
                },
                Doc::SoftLine => {
                    if cmd.mode == Mode::Break {
                        self.newline(cmd.indent);
                    }
                }
                Doc::HardLine => self.newline(cmd.indent),
                Doc::Nest(by, doc) => stack.push(Cmd {
                    indent: cmd.indent.saturating_add(*by),
                    mode: cmd.mode,
                    doc,
                }),
                Doc::Group(doc, breaks) => {
                    let flat = Cmd {
                        indent: cmd.indent,
                        mode: Mode::Flat,
                        doc,
                    };
                    let mode = if !*breaks && self.fits(flat, &stack) {
                        Mode::Flat
                    } else {
                        Mode::Break
                    };
                    stack.push(Cmd { mode, ..flat });
                }
                Doc::IfBreak(broken, flat) => stack.push(Cmd {
                    doc: if cmd.mode == Mode::Break {
                        broken
                    } else {
                        flat
                    },
                    ..cmd
                }),
                Doc::Concat(docs) => {
                    for doc in docs.iter().rev() {
                        stack.push(Cmd { doc, ..cmd });
                    }
                }
                Doc::Verbatim(v) => self.verbatim(v, cmd.indent),
            }
        }
    }

    /// Whether `next`, printed flat, and what follows it on `rest` up to
    /// the next line break fit in the rest of the line.
    fn fits(&self, next: Cmd<'_>, rest: &[Cmd<'_>]) -> bool {
        let mut remaining = self.width.saturating_sub(self.col);
        if let Some(indent) = self.pending_indent {
            remaining = remaining.saturating_sub(indent);
        }
        let mut stack = vec![next];
        let mut rest = rest.iter().rev();
        loop {
            let cmd = match stack.pop() {
                Some(cmd) => cmd,
                None => match rest.next() {
                    Some(cmd) => *cmd,
                    None => return true,
                },
            };
            match cmd.doc {
                // A comment at the end of a line cannot move, so it never
                // makes a group break.
                Doc::Nil | Doc::Comment(_) => {}
                Doc::Text(text) => match remaining.checked_sub(width(text)) {
                    Some(left) => remaining = left,
                    None => return false,
                },
                Doc::Line => match cmd.mode {
                    Mode::Flat => match remaining.checked_sub(1) {
                        Some(left) => remaining = left,
                        None => return false,
                    },
                    Mode::Break => return true,
                },
                Doc::SoftLine => {
                    if cmd.mode == Mode::Break {
                        return true;
                    }
                }
                Doc::HardLine => return true,
                Doc::Nest(by, doc) => stack.push(Cmd {
                    indent: cmd.indent.saturating_add(*by),
                    mode: cmd.mode,
                    doc,
                }),
                Doc::Group(doc, breaks) => stack.push(Cmd {
                    mode: if *breaks { Mode::Break } else { cmd.mode },
                    doc,
                    ..cmd
                }),
                Doc::IfBreak(broken, flat) => stack.push(Cmd {
                    doc: if cmd.mode == Mode::Break {
                        broken
                    } else {
                        flat
                    },
                    ..cmd
                }),
                Doc::Concat(docs) => {
                    for doc in docs.iter().rev() {
                        stack.push(Cmd { doc, ..cmd });
                    }
                }
                Doc::Verbatim(v) => {
                    match remaining.checked_sub(width(&v.first)) {
                        Some(left) => remaining = left,
                        None => return false,
                    }
                    if !v.rest.is_empty() {
                        return true;
                    }
                }
            }
        }
    }

    /// Writes text that has no line break.
    fn write(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(indent) = self.pending_indent.take() {
            self.out.extend(std::iter::repeat_n(' ', indent));
            self.col = indent;
        }
        self.out.push_str(text);
        self.col = self.col.saturating_add(width(text));
    }

    /// Ends the line, trimming its trailing whitespace, and starts the next
    /// one at `indent`.
    fn newline(&mut self, indent: usize) {
        self.trim_trailing();
        self.end_line();
        self.pending_indent = Some(indent);
    }

    fn end_line(&mut self) {
        self.out.push('\n');
        self.col = 0;
    }

    fn trim_trailing(&mut self) {
        let kept = self.out.trim_end_matches([' ', '\t']).len();
        self.out.truncate(kept);
    }

    /// Writes a verbatim fragment whose later lines are indented relative
    /// to `base`, the nesting the fragment is printed at.
    fn verbatim(&mut self, v: &Verbatim, base: usize) {
        self.write(&v.first);
        let mut protect = v.first_ends_in_string;
        for line in &v.rest {
            if !protect {
                self.trim_trailing();
            }
            self.end_line();
            self.pending_indent = None;
            if line.starts_in_string {
                // A line that starts inside a string is part of the
                // string's value: written exactly.
                self.out.push_str(&line.text);
                self.col = width(&line.text);
            } else if !line.text.is_empty() {
                let indent = base.saturating_add_signed(line.relative_indent);
                self.out.extend(std::iter::repeat_n(' ', indent));
                self.out.push_str(&line.text);
                self.col = indent.saturating_add(width(&line.text));
            }
            protect = line.ends_in_string;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Doc, Verbatim, VerbatimLine, print};

    fn t(s: &str) -> Doc {
        Doc::text(s)
    }

    /// `(a, b, c)` as a list that breaks one element per line with a
    /// trailing comma.
    fn list(items: &[&str]) -> Doc {
        let mut inner = vec![Doc::softline()];
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                inner.push(t(","));
                inner.push(Doc::line());
            }
            inner.push(t(item));
        }
        inner.push(Doc::if_break(t(","), Doc::Nil));
        Doc::group(Doc::concat(vec![
            t("("),
            Doc::nest(2, Doc::concat(inner)),
            Doc::softline(),
            t(")"),
        ]))
    }

    #[test]
    fn fmt_doc_text_and_hardline() {
        let doc = Doc::concat(vec![t("a"), Doc::hardline(), t("b"), Doc::hardline()]);
        assert_eq!(print(&doc, 80), "a\nb\n");
    }

    #[test]
    fn fmt_doc_group_flat_when_it_fits() {
        let doc = Doc::concat(vec![t("f"), list(&["a", "b", "c"])]);
        assert_eq!(print(&doc, 80), "f(a, b, c)");
        // Exactly the width fits.
        assert_eq!(print(&doc, 10), "f(a, b, c)");
    }

    #[test]
    fn fmt_doc_group_breaks_when_too_wide() {
        let doc = Doc::concat(vec![t("f"), list(&["a", "b", "c"])]);
        assert_eq!(print(&doc, 9), "f(\n  a,\n  b,\n  c,\n)");
    }

    #[test]
    fn fmt_doc_fits_counts_the_rest_of_the_line() {
        // The text after the group up to the next line break counts.
        let doc = Doc::concat(vec![
            t("f"),
            list(&["a", "b"]),
            t(" -> Int {"),
            Doc::hardline(),
            t("}"),
        ]);
        assert_eq!(print(&doc, 80), "f(a, b) -> Int {\n}");
        assert_eq!(print(&doc, 15), "f(\n  a,\n  b,\n) -> Int {\n}");
    }

    #[test]
    fn fmt_doc_nested_groups_break_outside_in() {
        let inner = list(&["x", "y"]);
        let outer = Doc::group(Doc::concat(vec![
            t("["),
            Doc::nest(
                2,
                Doc::concat(vec![
                    Doc::softline(),
                    t("first"),
                    t(","),
                    Doc::line(),
                    t("g"),
                    inner,
                ]),
            ),
            Doc::softline(),
            t("]"),
        ]));
        assert_eq!(print(&outer, 80), "[first, g(x, y)]");
        assert_eq!(print(&outer, 12), "[\n  first,\n  g(x, y)\n]");
        assert_eq!(
            print(&outer, 6),
            "[\n  first,\n  g(\n    x,\n    y,\n  )\n]"
        );
    }

    #[test]
    fn fmt_doc_hardline_breaks_the_enclosing_group() {
        let doc = Doc::group(Doc::concat(vec![
            t("("),
            Doc::nest(
                2,
                Doc::concat(vec![Doc::softline(), t("// c"), Doc::hardline(), t("a")]),
            ),
            Doc::softline(),
            t(")"),
        ]));
        assert_eq!(print(&doc, 80), "(\n  // c\n  a\n)");
    }

    #[test]
    fn fmt_doc_soft_line_and_line_when_flat() {
        let doc = Doc::group(Doc::concat(vec![
            t("a"),
            Doc::line(),
            t("b"),
            Doc::softline(),
            t("c"),
        ]));
        assert_eq!(print(&doc, 80), "a bc");
        assert_eq!(print(&doc, 2), "a\nb\nc");
    }

    #[test]
    fn fmt_doc_no_trailing_whitespace_or_indented_blank_lines() {
        let doc = Doc::nest(
            4,
            Doc::concat(vec![t("a "), Doc::hardline(), Doc::hardline(), t("b")]),
        );
        assert_eq!(print(&doc, 80), "a\n\n    b");
    }

    #[test]
    fn fmt_doc_verbatim_reindents_relative_lines() {
        let v = Verbatim {
            first: "{".to_owned(),
            first_ends_in_string: false,
            rest: vec![
                VerbatimLine {
                    relative_indent: 2,
                    text: "x".to_owned(),
                    starts_in_string: false,
                    ends_in_string: false,
                },
                VerbatimLine {
                    relative_indent: 0,
                    text: String::new(),
                    starts_in_string: false,
                    ends_in_string: false,
                },
                VerbatimLine {
                    relative_indent: 0,
                    text: "}".to_owned(),
                    starts_in_string: false,
                    ends_in_string: false,
                },
            ],
        };
        let doc = Doc::nest(
            4,
            Doc::concat(vec![t("top"), Doc::hardline(), t("f() "), Doc::verbatim(v)]),
        );
        assert_eq!(print(&doc, 80), "top\n    f() {\n      x\n\n    }");
    }

    #[test]
    fn fmt_doc_verbatim_keeps_string_lines() {
        let v = Verbatim {
            first: "x = \"a  ".to_owned(),
            first_ends_in_string: true,
            rest: vec![VerbatimLine {
                relative_indent: 0,
                text: "   b\"".to_owned(),
                starts_in_string: true,
                ends_in_string: false,
            }],
        };
        let doc = Doc::nest(2, Doc::concat(vec![Doc::hardline(), Doc::verbatim(v)]));
        assert_eq!(print(&doc, 80), "\n  x = \"a  \n   b\"");
    }

    #[test]
    fn fmt_doc_verbatim_breaks_its_group() {
        let v = Verbatim {
            first: "T<".to_owned(),
            first_ends_in_string: false,
            rest: vec![VerbatimLine {
                relative_indent: 0,
                text: ">".to_owned(),
                starts_in_string: false,
                ends_in_string: false,
            }],
        };
        let doc = Doc::group(Doc::concat(vec![
            t("("),
            Doc::nest(2, Doc::concat(vec![Doc::softline(), Doc::verbatim(v)])),
            Doc::softline(),
            t(")"),
        ]));
        assert_eq!(print(&doc, 80), "(\n  T<\n  >\n)");
    }

    #[test]
    fn fmt_doc_if_break_follows_the_group() {
        let doc = list(&["aaaa", "bbbb"]);
        assert_eq!(print(&doc, 80), "(aaaa, bbbb)");
        assert!(print(&doc, 5).ends_with("bbbb,\n)"));
    }
}
