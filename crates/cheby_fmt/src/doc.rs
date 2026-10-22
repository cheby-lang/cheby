//! The document algebra and its printer, driven by one line width (D-1030).
//!
//! A [`Doc`] describes text with places where a line may break. A
//! [`Doc::group`] is printed flat, every [`Doc::line`] as a space and every
//! [`Doc::softline`] as nothing, when its contents fit in what is left of
//! the line up to the next place where the line breaks anyway; otherwise its
//! lines become line breaks at the group's nesting. A [`Doc::hardline`]
//! always breaks, and so does every group around it.
//!
//! [`Doc::flat`] prints its contents flat, every group in them included.
//! [`Doc::choice`] holds two layouts of the same text and prints the first
//! when its first line fits, measured with its groups flat unless they
//! must break and with each choice in it taking whichever of its layouts
//! fits; otherwise the second. That is how a list hugs its last element
//! and a value moves to the line after its `=`. A measure tries at most
//! [`CHOICE_TRIES`] first layouts, so nested choices stay cheap.
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

use std::cell::Cell;
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
    /// The contents printed flat, every group in them included. Built only
    /// around documents without a hard line break.
    Flat(Rc<Doc>),
    /// Two layouts of the same text: the first when its first line fits,
    /// measured with every group in it flat unless it must break, else
    /// the second. Inside a flat group, the first when it has no hard
    /// line break, else the second. `true` when both always break a line.
    Choice(Rc<Doc>, Rc<Doc>, bool),
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

    /// `doc` printed flat, every group in it included.
    #[must_use]
    pub fn flat(doc: Self) -> Self {
        Self::Flat(Rc::new(doc))
    }

    /// `first` when its first line fits, else `second` (see
    /// [`Doc::Choice`]).
    #[must_use]
    pub fn choice(first: Self, second: Self) -> Self {
        let breaks = first.has_hard_break() && second.has_hard_break();
        Self::Choice(Rc::new(first), Rc::new(second), breaks)
    }

    /// The same document with its last group made to break: the group
    /// itself, or the last part of a concatenation, recursively. A choice
    /// expands to its first layout, or, when that is the flat layout, to
    /// its second: a call's arguments expand to their hugging layout. A
    /// document that ends in anything else is returned as it is.
    #[must_use]
    pub fn expand_last(&self) -> Self {
        match self {
            Self::Group(doc, _) => Self::Group(Rc::clone(doc), true),
            Self::Choice(first, second, _) => match first.as_ref() {
                Self::Flat(_) => second.expand_last(),
                first => first.expand_last(),
            },
            Self::Concat(docs) => {
                let mut parts: Vec<Self> = docs.to_vec();
                if let Some(last) = parts.pop() {
                    parts.push(last.expand_last());
                }
                Self::concat(parts)
            }
            other => other.clone(),
        }
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
            Self::Nest(_, doc) | Self::Flat(doc) => doc.has_hard_break(),
            Self::Group(_, breaks) | Self::Choice(_, _, breaks) => *breaks,
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
    /// Inside [`Doc::Flat`]: every group stays flat.
    locked: bool,
    doc: &'d Doc,
}

/// Where a measure of [`Printer::fits`] stands.
#[derive(Clone, Copy)]
struct Measure {
    /// Columns left on the line.
    remaining: usize,
    /// Whether the measured document wrote anything yet.
    started: bool,
}

/// How many first layouts of choices one [`Printer::fits`] may try in
/// all; once none are left, a choice is measured by its second layout,
/// so nested choices cost a bounded number of measures, not two to the
/// power of their depth.
const CHOICE_TRIES: u32 = 64;

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
        tries: Cell::new(CHOICE_TRIES),
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
    /// The tries left to the current [`Printer::fits`] (see
    /// [`CHOICE_TRIES`]).
    tries: Cell<u32>,
}

impl Printer {
    fn run(&mut self, doc: &Doc) {
        let mut stack = vec![Cmd {
            indent: 0,
            mode: Mode::Break,
            locked: false,
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
                    doc,
                    ..cmd
                }),
                Doc::Group(doc, breaks) => {
                    let flat = Cmd {
                        mode: Mode::Flat,
                        doc,
                        ..cmd
                    };
                    let mode = if cmd.locked || (!*breaks && self.fits(flat, &stack, false)) {
                        Mode::Flat
                    } else {
                        Mode::Break
                    };
                    stack.push(Cmd { mode, ..flat });
                }
                Doc::Flat(doc) => stack.push(Cmd {
                    mode: Mode::Flat,
                    locked: true,
                    doc,
                    ..cmd
                }),
                Doc::Choice(first, second, _) => {
                    let doc = if cmd.mode == Mode::Flat {
                        if first.has_hard_break() {
                            second
                        } else {
                            first
                        }
                    } else {
                        let candidate = Cmd { doc: first, ..cmd };
                        if self.fits(candidate, &stack, true) {
                            first
                        } else {
                            second
                        }
                    };
                    stack.push(Cmd { doc, ..cmd });
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

    /// Whether `next` and what follows it on `rest` up to the next line
    /// break fit in the rest of the line. A group is measured in its
    /// enclosing mode, or, with `first_line`, flat when it is part of
    /// `next`; either way a group that must break is measured broken.
    /// With `first_line`, a choice in `next` fits when either of its
    /// layouts does, as the printer then takes the one that fits.
    fn fits<'d>(&self, next: Cmd<'d>, rest: &[Cmd<'d>], first_line: bool) -> bool {
        let mut remaining = self.width.saturating_sub(self.col);
        if let Some(indent) = self.pending_indent {
            remaining = remaining.saturating_sub(indent);
        }
        let measure = Measure {
            remaining,
            started: false,
        };
        self.tries.set(CHOICE_TRIES);
        self.measure(measure, vec![(next, true)], rest, first_line)
    }

    /// The loop of [`Self::fits`]. `stack` holds pending documents, each
    /// marked whether it is part of the measured `next`; `rest` follows
    /// them.
    #[expect(
        clippy::too_many_lines,
        reason = "one match over the document's variants, as in `run`, reads best whole"
    )]
    fn measure<'d>(
        &self,
        state: Measure,
        mut stack: Vec<(Cmd<'d>, bool)>,
        rest: &[Cmd<'d>],
        first_line: bool,
    ) -> bool {
        let Measure {
            mut remaining,
            // Whether `next` wrote anything yet: with `first_line`, line
            // breaks before its first text are skipped, so a layout that
            // starts on a new line is measured by the line it writes.
            mut started,
        } = state;
        let mut rest_len = rest.len();
        loop {
            let (cmd, own) = if let Some(entry) = stack.pop() {
                entry
            } else {
                let Some(n) = rest_len.checked_sub(1) else {
                    return true;
                };
                rest_len = n;
                let Some(cmd) = rest.get(rest_len) else {
                    return true;
                };
                (*cmd, false)
            };
            let breaks_here = match cmd.doc {
                Doc::HardLine => true,
                Doc::Line | Doc::SoftLine => cmd.mode == Mode::Break,
                _ => false,
            };
            if breaks_here {
                if first_line && own && !started {
                    remaining = self.width.saturating_sub(cmd.indent);
                    continue;
                }
                return true;
            }
            match cmd.doc {
                // A comment at the end of a line cannot move, so it never
                // makes a group break.
                Doc::Nil | Doc::SoftLine | Doc::HardLine => {}
                Doc::Comment(_) => started = true,
                Doc::Text(text) => {
                    started = true;
                    match remaining.checked_sub(width(text)) {
                        Some(left) => remaining = left,
                        None => return false,
                    }
                }
                Doc::Line => match remaining.checked_sub(1) {
                    Some(left) => remaining = left,
                    None => return false,
                },
                Doc::Nest(by, doc) => stack.push((
                    Cmd {
                        indent: cmd.indent.saturating_add(*by),
                        doc,
                        ..cmd
                    },
                    own,
                )),
                Doc::Group(doc, breaks) => stack.push((
                    Cmd {
                        mode: if cmd.locked {
                            Mode::Flat
                        } else if *breaks {
                            Mode::Break
                        } else if first_line && own {
                            Mode::Flat
                        } else {
                            cmd.mode
                        },
                        doc,
                        ..cmd
                    },
                    own,
                )),
                Doc::Flat(doc) => stack.push((
                    Cmd {
                        mode: Mode::Flat,
                        locked: true,
                        doc,
                        ..cmd
                    },
                    own,
                )),
                Doc::Choice(first, second, _) => {
                    if cmd.mode == Mode::Flat {
                        let doc = if first.has_hard_break() {
                            second
                        } else {
                            first
                        };
                        stack.push((Cmd { doc, ..cmd }, own));
                    } else if self.tries.get() > 0 {
                        // The printer takes the first layout that fits,
                        // so the line fits when either does.
                        self.tries.set(self.tries.get().saturating_sub(1));
                        let mut alternative = stack.clone();
                        alternative.push((Cmd { doc: first, ..cmd }, own));
                        let state = Measure { remaining, started };
                        let rest = rest.get(..rest_len).unwrap_or(&[]);
                        if self.measure(state, alternative, rest, first_line) {
                            return true;
                        }
                        stack.push((Cmd { doc: second, ..cmd }, own));
                    } else {
                        // Out of tries: the fallback layout.
                        stack.push((Cmd { doc: second, ..cmd }, own));
                    }
                }
                Doc::IfBreak(broken, flat) => stack.push((
                    Cmd {
                        doc: if cmd.mode == Mode::Break {
                            broken
                        } else {
                            flat
                        },
                        ..cmd
                    },
                    own,
                )),
                Doc::Concat(docs) => {
                    for doc in docs.iter().rev() {
                        stack.push((Cmd { doc, ..cmd }, own));
                    }
                }
                Doc::Verbatim(v) => {
                    started = true;
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
    fn fmt_doc_flat_keeps_its_groups_flat() {
        let doc = Doc::concat(vec![t("f"), Doc::flat(list(&["aaaa", "bbbb"]))]);
        assert_eq!(print(&doc, 4), "f(aaaa, bbbb)");
    }

    #[test]
    fn fmt_doc_choice_takes_the_first_layout_whose_first_line_fits() {
        // `f(a, [` then the list broken, else every argument broken.
        let hug = Doc::concat(vec![t("f(a, "), list(&["xx", "yy"]).expand_last(), t(")")]);
        let broken = Doc::concat(vec![t("f"), list(&["a", "(xx, yy)"])]);
        let doc = Doc::choice(hug, broken);
        assert_eq!(print(&doc, 80), "f(a, (\n  xx,\n  yy,\n))");
        assert_eq!(print(&doc, 6), "f(a, (\n  xx,\n  yy,\n))");
        assert_eq!(print(&doc, 5), "f(\n  a,\n  (xx, yy),\n)");
        // Inside a flat group, the first layout unless it must break.
        let flat = Doc::group(Doc::choice(
            t("one"),
            Doc::concat(vec![Doc::hardline(), t("two")]),
        ));
        assert_eq!(print(&flat, 80), "one");
    }

    #[test]
    fn fmt_doc_choice_measures_a_layout_that_starts_on_a_new_line() {
        // The second layout starts with a line break: it is measured by
        // the line it writes.
        let next = Doc::nest(2, Doc::concat(vec![Doc::hardline(), t("value")]));
        let doc = Doc::concat(vec![
            t("x ="),
            Doc::choice(t(" a_long_value"), Doc::choice(next, t(" fallback"))),
        ]);
        assert_eq!(print(&doc, 80), "x = a_long_value");
        assert_eq!(print(&doc, 10), "x =\n  value");
        assert_eq!(print(&doc, 6), "x = fallback");
    }

    #[test]
    fn fmt_doc_nested_choices_are_bounded() {
        // Two hundred nested choices whose first layouts never fit: the
        // measure gives up on first layouts after its tries.
        let mut doc = t("end");
        for _ in 0..200 {
            doc = Doc::choice(Doc::concat(vec![t("xxxxxxxx"), doc.clone()]), doc);
        }
        assert_eq!(print(&doc, 10), "end");
    }

    #[test]
    fn fmt_doc_if_break_follows_the_group() {
        let doc = list(&["aaaa", "bbbb"]);
        assert_eq!(print(&doc, 80), "(aaaa, bbbb)");
        assert!(print(&doc, 5).ends_with("bbbb,\n)"));
    }
}
