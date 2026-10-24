//! Expressions, statements, patterns and types laid out at the width
//! (D-1030), below the item headers that [`crate::layout`] lays out.
//!
//! The rules, each settled in the log for the formatter:
//!
//! - D-1157: a line break is added only where §2.8
//!   continues the line (D-231, D-107): inside an open `(`, `[` or type
//!   `<`, after `{` and before `}`, after a binary operator or after `,`,
//!   `=`, `=>` or `<-`, and before a leading `|>`, `&&` or `||`; never
//!   around `.`, `::`, `as`, `when` or a pattern's `|`. A comma-separated
//!   list is flat when it fits, else one element per line with a trailing
//!   comma; a call's arguments hug a last closure, block, `case`, list or
//!   record literal (the others stay on the line of `(` and only it
//!   breaks) when that first line fits. A chain of two or more operators
//!   of one precedence breaks at every operator together, after it, its
//!   continuation lines one level deeper; `&&` and `||` form one chain and
//!   break before the operator. A value after `=`, `<-` or `=>`, and the
//!   right operand of a lone binary operator, goes: a block, `case`,
//!   closure or binary expression on the operator's line, breaking inside;
//!   a string, literal, path or field access on the operator's line, else
//!   on the next line one level deeper; anything else, in this order, flat
//!   on the operator's line, hugging there (a list or record literal
//!   opening on that line, a call whose last argument hugs) when that
//!   first line fits, flat on the next line, hugging on the next line, else
//!   broken on the operator's line. After `=>`, flat on the next line
//!   comes before hugging on the arm's line, unless the body is a call
//!   that ends in a closure. A call's last argument hugs when
//!   it is itself a call whose last argument hugs (`Some(fold(xs, 0,
//!   fn(acc, x) {`). A function's return type stays on the line of `)`, so
//!   a header breaks its parameters first.
//! - D-1159: a pipeline is flat when it fits; otherwise one
//!   stage per line, each line starting with `|>`, at the indentation of
//!   the pipeline's first line when the pipeline starts its line (a
//!   statement, a list element, the value of `let`, `use`, `const` or an
//!   arm on a line of its own), else one level deeper. As a value after
//!   `=`, `<-` or `=>` it goes to the next line, one level deeper, when it
//!   does not fit after the operator. A pipeline whose last stage must
//!   break (it holds a closure of several statements) stays on one line up
//!   to that stage when that line fits.
//! - D-1155: the block of an item, a test or a local function is broken
//!   over lines unless empty (`{}`); any other block with exactly one
//!   statement and no comment is `{ statement }` when it fits; blocks with
//!   more statements or a comment, and every `case`, have one statement or
//!   arm per line.
//!   Blank lines at the start and the end of a block are dropped; single
//!   blank lines between statements and arms are kept (D-1030).
//! - D-1156: `case` subjects and an arm's patterns and guard
//!   stay on the line of `case` and of the arm, breaking only inside their
//!   own brackets; an arm is `patterns [when guard] => body`, the body
//!   after `=>` laid out as a value after `=` is.
//!
//! A comment or a blank line between two parts of a node keeps its place
//! and breaks the line there ([`crate::gap`], D-1160, D-1161); a token that holds a line break
//! (a multi-line raw string in a literal pattern) is kept as written. A
//! gap between a unary `-` and a number (D-470) keeps its node verbatim:
//! the formatter does not close it (D-1158).

use cheby_syntax::{SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};

use crate::doc::Doc;
use crate::gap::{Gap, Parts};
use crate::layout::{Delims, Formatter, INDENT, ListStyle, entries, hugs, is_comment};

/// A pipeline's layouts: the usual one (flat when it fits, else one stage
/// per line), and, when the last stage must break, the one that keeps
/// every stage but the last on one line.
struct PipeForms {
    normal: Doc,
    hug: Option<Doc>,
}

/// Whether the printer writes no space between two parts of a sequence
/// of kind `node`.
const fn tight(node: SyntaxKind, prev: SyntaxKind, next: SyntaxKind) -> bool {
    matches!(
        next,
        SyntaxKind::ARG_LIST
            | SyntaxKind::POS_PAT_LIST
            | SyntaxKind::TYPE_ARG_LIST
            | SyntaxKind::PARAM_LIST
            | SyntaxKind::TYPE_LIST
            | SyntaxKind::COLON
            | SyntaxKind::COLON2
            | SyntaxKind::DOT
            | SyntaxKind::COMMA
            | SyntaxKind::R_PAREN
            | SyntaxKind::R_BRACK
    ) || matches!(
        prev,
        SyntaxKind::COLON2
            | SyntaxKind::DOT
            | SyntaxKind::DOT2
            | SyntaxKind::L_PAREN
            | SyntaxKind::L_BRACK
    ) || matches!(node, SyntaxKind::PREFIX_EXPR | SyntaxKind::LITERAL_PAT)
}

/// The precedence level of a binary operator (Appendix A.5); `&&` and
/// `||` share one, so a condition breaks at all of them together.
const fn level(op: SyntaxKind) -> u8 {
    match op {
        SyntaxKind::PIPE2 | SyntaxKind::AMP2 => 1,
        SyntaxKind::EQ2
        | SyntaxKind::NEQ
        | SyntaxKind::LT
        | SyntaxKind::LE
        | SyntaxKind::GT
        | SyntaxKind::GE => 3,
        SyntaxKind::PIPE_GT => 4,
        SyntaxKind::PIPE => 5,
        SyntaxKind::CARET => 6,
        SyntaxKind::AMP => 7,
        SyntaxKind::SHL | SyntaxKind::SHR => 8,
        SyntaxKind::PLUS | SyntaxKind::MINUS => 9,
        _ => 10,
    }
}

/// Whether a pipeline whose parent is of this kind starts its line, so
/// its stages line up with its first line.
const fn starts_line(parent: SyntaxKind) -> bool {
    matches!(
        parent,
        SyntaxKind::BLOCK
            | SyntaxKind::ARG_LIST
            | SyntaxKind::LIST_EXPR
            | SyntaxKind::TUPLE_EXPR
            | SyntaxKind::LET_STMT
            | SyntaxKind::USE_STMT
            | SyntaxKind::CASE_ARM
            | SyntaxKind::COND_ARM
            | SyntaxKind::CONST_DECL
    )
}

/// Checks that `node` starts with `{` and ends with `}`.
fn check_braces(node: SyntaxNode<'_>) -> Option<()> {
    let first = node.children_with_tokens().next()?;
    let last = node.children_with_tokens().last()?;
    (first.kind() == SyntaxKind::L_BRACE && last.kind() == SyntaxKind::R_BRACE).then_some(())
}

/// The operator token of a binary expression.
fn bin_op(node: SyntaxNode<'_>) -> Option<SyntaxToken<'_>> {
    node.children_with_tokens()
        .filter_map(SyntaxElement::as_token)
        .find(|t| !t.kind().is_trivia() && t.kind() != SyntaxKind::NEWLINE)
}

/// A binary expression's operands and operator, with the comments and
/// blank lines before and after the operator.
struct BinParts<'t> {
    lhs: SyntaxNode<'t>,
    before: Gap<'t>,
    op: SyntaxToken<'t>,
    after: Gap<'t>,
    rhs: SyntaxNode<'t>,
}

/// A binary expression's parts, or `None` when it is not one operand, an
/// operator and an operand.
fn bin_parts(node: SyntaxNode<'_>) -> Option<BinParts<'_>> {
    let mut nodes = Vec::new();
    let mut op = None;
    let mut before = Gap::default();
    let mut gap = Gap::default();
    for child in node.children_with_tokens() {
        if gap.take_trivia(child) {
            continue;
        }
        match child {
            SyntaxElement::Token(t) => {
                if op.replace(t).is_some() || nodes.len() != 1 {
                    return None;
                }
                before = std::mem::take(&mut gap);
            }
            SyntaxElement::Node(n) => nodes.push(n),
        }
    }
    match (nodes.as_slice(), op) {
        (&[lhs, rhs], Some(op)) => Some(BinParts {
            lhs,
            before,
            op,
            after: gap,
            rhs,
        }),
        _ => None,
    }
}

/// Whether `node` is a call whose last argument is a closure, or a call
/// that ends in such a call, which hugs the call's parentheses.
fn ends_in_closure(node: SyntaxNode<'_>) -> bool {
    node.kind() == SyntaxKind::CALL_EXPR
        && node
            .children()
            .find(|c| c.kind() == SyntaxKind::ARG_LIST)
            .and_then(|args| args.children().last())
            .is_some_and(|last| last.kind() == SyntaxKind::CLOSURE || ends_in_closure(last))
}

/// `doc`, the laid-out `value` after an assignment-like operator (`=`,
/// `<-`, `=>`, or the one operator of a binary expression), with the
/// space or line break before it (D-1157). After
/// `=>` (`arm`), a value that fits flat on the next line goes there
/// before it breaks on the arm's line.
fn tail_doc(value: SyntaxNode<'_>, doc: Doc, arm: bool) -> Doc {
    let kind = value.kind();
    let same_line = Doc::concat(vec![Doc::text(" "), doc.clone()]);
    let next_line = |doc: Doc| Doc::nest(INDENT, Doc::concat(vec![Doc::hardline(), doc]));
    match kind {
        // These break by themselves, on the operator's line.
        SyntaxKind::BLOCK | SyntaxKind::CASE_EXPR | SyntaxKind::CLOSURE | SyntaxKind::BIN_EXPR => {
            same_line
        }
        // These cannot break: on the next line when they do not fit.
        SyntaxKind::STRING_LIT
        | SyntaxKind::LITERAL
        | SyntaxKind::PATH_EXPR
        | SyntaxKind::FIELD_EXPR
        | SyntaxKind::HOLE
        | SyntaxKind::PATH_TYPE => Doc::choice(same_line, next_line(doc)),
        // The rest (calls, lists, records, tuples), in this order: flat on
        // the operator's line; hugging on the operator's line (a list or a
        // record opening there, a call whose last argument hugs its
        // parentheses), when that first line fits; flat on the next line;
        // hugging on the next line; else broken on the operator's line.
        // After `=>`, flat on the next line comes before hugging, except
        // for a call that ends in a closure.
        _ => {
            let flat = (!doc.has_hard_break()).then(|| Doc::flat(doc.clone()));
            let hug = hugs(SyntaxElement::Node(value)).then(|| doc.expand_last());
            let a_flat = flat.clone().map(|f| Doc::concat(vec![Doc::text(" "), f]));
            let b_flat = flat.map(next_line);
            let a_hug = hug.clone().map(|h| Doc::concat(vec![Doc::text(" "), h]));
            let b_hug = hug.map(next_line);
            let order = if arm && !ends_in_closure(value) {
                [a_flat, b_flat, a_hug, b_hug]
            } else {
                [a_flat, a_hug, b_flat, b_hug]
            };
            order
                .into_iter()
                .rev()
                .flatten()
                .fold(same_line, |fallback, layout| Doc::choice(layout, fallback))
        }
    }
}

impl<'t> Formatter<'t> {
    /// Lays out an expression, statement, pattern or type node; `None` for
    /// a kind this part of the formatter does not lay out, or a node with
    /// a gap D-470 rejects or a shape the grammar does not produce.
    pub(crate) fn syntax(&mut self, node: SyntaxNode<'t>) -> Option<Doc> {
        match node.kind() {
            SyntaxKind::LITERAL | SyntaxKind::STRING_LIT => Some(self.as_written(node)),
            SyntaxKind::BIN_EXPR => self.binary(node),
            SyntaxKind::PREFIX_EXPR | SyntaxKind::LITERAL_PAT => self.prefix(node),
            SyntaxKind::RECORD_EXPR => self.record(node),
            SyntaxKind::BLOCK => self.block(node, false),
            SyntaxKind::CASE_BODY => self.braced(node, false),
            SyntaxKind::LET_STMT => self.seq(node, Some(SyntaxKind::EQ)),
            SyntaxKind::USE_STMT => self.seq(node, Some(SyntaxKind::LEFT_ARROW)),
            SyntaxKind::CASE_ARM | SyntaxKind::COND_ARM => {
                self.seq(node, Some(SyntaxKind::FAT_ARROW))
            }
            SyntaxKind::CALL_EXPR
            | SyntaxKind::FIELD_EXPR
            | SyntaxKind::PATH_EXPR
            | SyntaxKind::PAREN_EXPR
            | SyntaxKind::SPREAD
            | SyntaxKind::HOLE
            | SyntaxKind::PANIC_EXPR
            | SyntaxKind::TODO_EXPR
            | SyntaxKind::MESSAGE
            | SyntaxKind::GUARD
            | SyntaxKind::FIELD_INIT
            | SyntaxKind::CLOSURE
            | SyntaxKind::CASE_EXPR
            | SyntaxKind::ASSERT_STMT
            | SyntaxKind::USE_BINDER
            | SyntaxKind::WILDCARD_PAT
            | SyntaxKind::IDENT_PAT
            | SyntaxKind::CTOR_PAT
            | SyntaxKind::FIELD_PAT
            | SyntaxKind::REST_PAT
            | SyntaxKind::PAREN_PAT
            | SyntaxKind::OR_PAT
            | SyntaxKind::AS_PAT
            | SyntaxKind::PATH_TYPE
            | SyntaxKind::FN_TYPE
            | SyntaxKind::DYN_TYPE => self.seq(node, None),
            _ => None,
        }
    }

    /// A node whose parts sit on one line, separated by single spaces or
    /// none ([`tight`]); its child nodes laid out by [`Self::node`]. With
    /// `tail`, the node after that token (`=`, `<-` or `=>`) is the value
    /// of an assignment ([`Self::assign_tail`]). A comment or a blank line
    /// between two parts keeps its place and breaks the line there, and
    /// the parts after it go one level deeper (D-1160, D-1161); a token
    /// that holds a line
    /// break (a multi-line raw string) is kept as written.
    pub(crate) fn seq(&mut self, node: SyntaxNode<'t>, tail: Option<SyntaxKind>) -> Option<Doc> {
        self.laid_out(node);
        let kind = node.kind();
        let mut parts = Parts::default();
        let mut prev: Option<SyntaxKind> = None;
        let mut gap = Gap::default();
        for child in node.children_with_tokens() {
            let child_kind = child.kind();
            if gap.take_trivia(child) {
                continue;
            }
            let broke = parts.gap(&std::mem::take(&mut gap), prev.is_none());
            if let (Some(p), SyntaxElement::Node(value)) = (prev, child)
                && tail == Some(p)
            {
                // After a comment, the value starts its own line.
                parts.push(if broke {
                    self.node(value)
                } else {
                    self.assign_tail(value)
                });
                prev = Some(child_kind);
                continue;
            }
            if let Some(p) = prev
                && !broke
                && !tight(kind, p, child_kind)
            {
                parts.push(Doc::text(" "));
            }
            match child {
                SyntaxElement::Token(token) => parts.push(self.token(token)),
                SyntaxElement::Node(n) => parts.push(self.node(n)),
            }
            prev = Some(child_kind);
        }
        // A node never ends with a comment (the tree builder puts it in
        // the node of the next token); if one did, its place is unknown.
        if gap.has_comment() {
            return None;
        }
        Some(parts.finish(INDENT))
    }

    /// The value after `=`, `<-` or `=>`, with the space before it: on
    /// the operator's line, breaking inside where it must; a pipeline or
    /// an atom that does not fit there on the next line, one level deeper
    /// (D-1157, D-1159).
    pub(crate) fn assign_tail(&mut self, value: SyntaxNode<'t>) -> Doc {
        let kind = value.kind();
        if kind == SyntaxKind::BIN_EXPR
            && bin_op(value).is_some_and(|t| t.kind() == SyntaxKind::PIPE_GT)
        {
            let Some(forms) = self.try_layout(value, Self::pipe_forms) else {
                let anchor = self.src.node_line_indent(value);
                return Doc::concat(vec![Doc::text(" "), self.verbatim(value, anchor)]);
            };
            let same_line = if forms.normal.has_hard_break() {
                forms.hug
            } else {
                Some(Doc::flat(forms.normal.clone()))
            };
            let next_line = Doc::nest(INDENT, Doc::concat(vec![Doc::hardline(), forms.normal]));
            return match same_line {
                Some(doc) => Doc::choice(Doc::concat(vec![Doc::text(" "), doc]), next_line),
                None => next_line,
            };
        }
        let doc = self.node(value);
        let arm = value
            .parent()
            .is_some_and(|p| matches!(p.kind(), SyntaxKind::CASE_ARM | SyntaxKind::COND_ARM));
        tail_doc(value, doc, arm)
    }

    /// A unary operator glued to its operand, and a negative literal
    /// pattern. D-1158: a gap between `-` and a
    /// number is an error (D-470) the formatter does not close, so that
    /// node stays as written.
    fn prefix(&mut self, node: SyntaxNode<'t>) -> Option<Doc> {
        let minus = node
            .children_with_tokens()
            .next()
            .is_some_and(|c| c.kind() == SyntaxKind::MINUS);
        let gap = node.children_with_tokens().any(|c| {
            matches!(c.kind(), SyntaxKind::WHITESPACE | SyntaxKind::NEWLINE) || is_comment(c.kind())
        });
        let number = node.children_with_tokens().skip(1).any(|c| match c {
            SyntaxElement::Token(t) => {
                matches!(t.kind(), SyntaxKind::INT_NUMBER | SyntaxKind::FLOAT_NUMBER)
            }
            SyntaxElement::Node(n) => {
                n.kind() == SyntaxKind::LITERAL
                    && n.first_token().is_some_and(|t| {
                        matches!(t.kind(), SyntaxKind::INT_NUMBER | SyntaxKind::FLOAT_NUMBER)
                    })
            }
        });
        if minus && gap && number {
            return None;
        }
        self.seq(node, None)
    }

    /// A binary expression: a pipeline, or a chain of operators of one
    /// precedence, broken at every operator together when it does not
    /// fit (D-1157).
    fn binary(&mut self, node: SyntaxNode<'t>) -> Option<Doc> {
        let op = bin_parts(node)?.op.kind();
        if op == SyntaxKind::PIPE_GT {
            let forms = self.pipe_forms(node)?;
            return Some(match forms.hug {
                Some(hug) => Doc::choice(hug, forms.normal),
                None => forms.normal,
            });
        }
        let chain_level = level(op);
        let mut operands = Vec::new();
        // Each operator with the gaps before and after it.
        let mut ops = Vec::new();
        let mut cur = node;
        loop {
            let BinParts {
                lhs,
                before,
                op,
                after,
                rhs,
            } = bin_parts(cur)?;
            self.laid_out(cur);
            operands.push(rhs);
            ops.push((before, op, after));
            let next = (lhs.kind() == SyntaxKind::BIN_EXPR)
                .then(|| bin_parts(lhs))
                .flatten()
                .filter(|parts| level(parts.op.kind()) == chain_level && chain_level != 3);
            if next.is_some() {
                cur = lhs;
            } else {
                operands.push(lhs);
                break;
            }
        }
        operands.reverse();
        ops.reverse();
        if let ([lhs, rhs], [(before, op, after)]) = (operands.as_slice(), ops.as_slice())
            && !matches!(op.kind(), SyntaxKind::AMP2 | SyntaxKind::PIPE2)
            && before.is_empty()
            && after.is_empty()
        {
            // One operator: its right operand is laid out as a value after
            // `=` is, so `x == (` can open a tuple that breaks inside.
            let lhs = self.node(*lhs);
            let rhs_doc = self.node(*rhs);
            return Some(Doc::concat(vec![
                lhs,
                Doc::text(" "),
                Doc::text(op.text()),
                tail_doc(*rhs, rhs_doc, false),
            ]));
        }
        let mut docs = operands.into_iter().map(|n| self.node(n));
        let first = docs.next()?;
        let mut rest = Vec::new();
        // A comment or a blank line around an operator keeps its place and
        // breaks the line there (D-1160); the chain's
        // other operators then break too, as its group must.
        for ((before, op, after), doc) in ops.into_iter().zip(docs) {
            let before = before.doc(false);
            let after = after.doc(false);
            if matches!(op.kind(), SyntaxKind::AMP2 | SyntaxKind::PIPE2) {
                rest.push(before.unwrap_or_else(Doc::line));
                rest.push(Doc::text(op.text()));
                rest.push(after.unwrap_or_else(|| Doc::text(" ")));
            } else {
                let op_starts_line = before.is_some();
                rest.push(before.unwrap_or_else(|| Doc::text(" ")));
                rest.push(Doc::text(op.text()));
                rest.push(after.unwrap_or_else(|| {
                    if op_starts_line {
                        Doc::text(" ")
                    } else {
                        Doc::line()
                    }
                }));
            }
            rest.push(doc);
        }
        Some(Doc::group(Doc::concat(vec![
            first,
            Doc::nest(INDENT, Doc::concat(rest)),
        ])))
    }

    /// A pipeline's layouts (D-1159). A comment or a blank
    /// line before a `|>` takes the place of the line break before it; one
    /// after a `|>` breaks the line there, and the stage goes one level
    /// deeper (D-1160). Either way the pipeline is
    /// broken, one stage per line.
    fn pipe_forms(&mut self, node: SyntaxNode<'t>) -> Option<PipeForms> {
        let mut stages = Vec::new();
        let mut cur = node;
        let head = loop {
            let BinParts {
                lhs,
                before,
                op,
                after,
                rhs,
            } = bin_parts(cur)?;
            if op.kind() != SyntaxKind::PIPE_GT {
                return None;
            }
            self.laid_out(cur);
            stages.push((before, rhs, after));
            let pipe = lhs.kind() == SyntaxKind::BIN_EXPR
                && bin_parts(lhs).is_some_and(|parts| parts.op.kind() == SyntaxKind::PIPE_GT);
            if pipe {
                cur = lhs;
            } else {
                break lhs;
            }
        };
        stages.reverse();
        let indent = if node.parent().is_some_and(|p| starts_line(p.kind())) {
            0
        } else {
            INDENT
        };
        let last_hugs = stages
            .last()
            .is_some_and(|(_, n, _)| hugs(SyntaxElement::Node(*n)));
        let quiet = stages.iter().all(|(b, _, a)| b.is_empty() && a.is_empty());
        let head = self.node(head);
        let mut docs = Vec::new();
        let mut broken_stages = Vec::new();
        for (before, stage, after) in stages {
            let stage = self.node(stage);
            broken_stages.push(before.doc(false).unwrap_or_else(Doc::line));
            broken_stages.push(Doc::text("|>"));
            if let Some(gap) = after.doc(false) {
                broken_stages.push(Doc::nest(INDENT, Doc::concat(vec![gap, stage.clone()])));
            } else {
                broken_stages.push(Doc::text(" "));
                broken_stages.push(stage.clone());
            }
            docs.push(stage);
        }
        let stages = docs;
        let normal = Doc::group(Doc::concat(vec![
            head.clone(),
            Doc::nest(indent, Doc::concat(broken_stages)),
        ]));
        let hug = match stages.split_last() {
            Some((last, firsts))
                if last_hugs
                    && quiet
                    && normal.has_hard_break()
                    && !head.has_hard_break()
                    && !firsts.iter().any(Doc::has_hard_break) =>
            {
                let mut flat = vec![head];
                for stage in firsts {
                    flat.push(Doc::text(" |> "));
                    flat.push(stage.clone());
                }
                // The last stage in its hugging layout: the pipeline
                // stays on one line only when that stage hugs.
                Some(Doc::concat(vec![
                    Doc::flat(Doc::concat(flat)),
                    Doc::text(" |> "),
                    last.expand_last(),
                ]))
            }
            _ => None,
        };
        Some(PipeForms { normal, hug })
    }

    /// A record literal or update: its type, then its fields as a list
    /// in `{ }` with spaces inside.
    fn record(&mut self, node: SyntaxNode<'t>) -> Option<Doc> {
        self.laid_out(node);
        let children: Vec<SyntaxElement<'t>> = node.children_with_tokens().collect();
        let open_at = children
            .iter()
            .position(|c| c.kind() == SyntaxKind::L_BRACE)?;
        let (before, rest) = children.split_at(open_at);
        let (open, rest) = rest.split_first()?;
        let (close, inner) = rest.split_last()?;
        let mut path = None;
        let mut gap = Gap::default();
        for &child in before {
            if gap.take_trivia(child) {
                continue;
            }
            match child {
                SyntaxElement::Node(n) if path.is_none() => path = Some(n),
                _ => return None,
            }
        }
        let mut parts = Parts::default();
        parts.push(self.node(path?));
        // A comment between the type and `{` (D-1160).
        if !parts.gap(&gap, false) {
            parts.push(Doc::text(" "));
        }
        let style = ListStyle::new(Delims::Spaced);
        parts.push(self.list(open.as_token()?, inner, close.as_token()?, style)?);
        Some(parts.finish(INDENT))
    }

    /// A block: `{}` when empty; with `body` (an item's, a test's or a
    /// local function's), or with more than one statement or a comment,
    /// one statement per line; else `{ statement }` when it fits
    /// (D-1155).
    pub(crate) fn block(&mut self, node: SyntaxNode<'t>, body: bool) -> Option<Doc> {
        if !body {
            let entries = entries(node, Some((SyntaxKind::L_BRACE, SyntaxKind::R_BRACE)));
            if let [entry] = entries.as_slice()
                && let SyntaxElement::Node(stmt) = entry.elem
            {
                self.laid_out(node);
                check_braces(node)?;
                let stmt = self.node(stmt);
                return Some(Doc::group(Doc::concat(vec![
                    Doc::text("{"),
                    Doc::nest(INDENT, Doc::concat(vec![Doc::line(), stmt])),
                    Doc::line(),
                    Doc::text("}"),
                ])));
            }
        }
        self.braced(node, true)
    }

    /// Statements or `case` arms between `{` and `}`, one per line:
    /// `{}` when there are none (a block only). A comment on the line of
    /// `{` stays there, a comment after a statement stays after it, other
    /// comments keep their own lines, and single blank lines between
    /// entries are kept.
    fn braced(&mut self, node: SyntaxNode<'t>, empty_ok: bool) -> Option<Doc> {
        self.laid_out(node);
        check_braces(node)?;
        let entries = entries(node, Some((SyntaxKind::L_BRACE, SyntaxKind::R_BRACE)));
        if entries.is_empty() {
            return empty_ok.then(|| Doc::text("{}"));
        }
        let mut docs = vec![Doc::text("{")];
        let mut start = 0;
        if let Some(first) = entries.first()
            && first.newlines == 0
            && let SyntaxElement::Token(token) = first.elem
            && is_comment(token.kind())
        {
            docs.push(Doc::text(" "));
            docs.push(Doc::comment(crate::layout::comment_text(token)));
            start = 1;
        }
        let mut body = Vec::new();
        for (i, entry) in entries.iter().enumerate().skip(start) {
            let trailing = entry.newlines == 0 && is_comment(entry.elem.kind());
            if i > start {
                if trailing {
                    body.push(Doc::text(" "));
                } else {
                    body.push(Doc::hardline());
                    if entry.newlines >= 2 {
                        body.push(Doc::hardline());
                    }
                }
            } else {
                body.push(Doc::hardline());
            }
            match entry.elem {
                SyntaxElement::Token(token) if is_comment(token.kind()) => {
                    let text = crate::layout::comment_text(token);
                    body.push(if trailing && i > start {
                        Doc::comment(text)
                    } else {
                        Doc::text(text)
                    });
                }
                SyntaxElement::Token(_) => return None,
                SyntaxElement::Node(n) => body.push(self.node(n)),
            }
        }
        docs.push(Doc::nest(INDENT, Doc::concat(body)));
        docs.push(Doc::hardline());
        docs.push(Doc::text("}"));
        Some(Doc::concat(docs))
    }

    /// `node`'s layout, with the nodes it laid out counted, or `None`
    /// with nothing counted.
    pub(crate) fn try_layout<T>(
        &mut self,
        node: SyntaxNode<'t>,
        layout: impl FnOnce(&mut Self, SyntaxNode<'t>) -> Option<T>,
    ) -> Option<T> {
        let saved = std::mem::take(&mut self.coverage);
        let laid_out = layout(self, node);
        let attempt = std::mem::replace(&mut self.coverage, saved);
        if laid_out.is_some() {
            self.coverage.add(&attempt);
        }
        laid_out
    }
}
