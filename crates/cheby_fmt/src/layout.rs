//! From the syntax tree to a [`Doc`]: the file's lines, imports sorted
//! within their blocks, item headers and comma-separated lists laid out at
//! the width (D-1030), and the dispatch of every other node to
//! [`crate::expr`].
//!
//! The formatter moves only whitespace, line breaks and blank lines. It
//! never adds, removes or reorders a token, except that imports are sorted
//! within their block and a list that breaks over lines gets a trailing
//! comma, which it loses when it fits on one line (a one-element tuple
//! keeps its comma as written). A line break it adds or removes is always
//! one the parser ignores (D-231): inside `(`, `[`, `<` or a list's
//! `{ }`, after a `,` or a binary operator, after `=`, `=>` or `<-`, and
//! before a leading `|>`, `&&` or `||`.
//!
//! Every comment and blank line has a place (`crate::gap`). A node is
//! printed verbatim, its source text with its own line structure
//! re-indented to the new indentation, only by rule (a gap between a unary
//! `-` and a number, D-470; a top-level `ERROR` node such as an attribute
//! with no item after it), or as a safety net when a layout meets a shape
//! the grammar does not produce.

use std::collections::BTreeMap;

use cheby_syntax::{SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};

use crate::doc::Doc;
use crate::gap::{Gap, Parts};
use crate::source::Source;

/// The indentation of one nesting level (AGENTS.md: two spaces).
pub(crate) const INDENT: usize = 2;

/// How many nodes of each kind were laid out by the printer and how many
/// were printed verbatim.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Nodes laid out by the printer, by kind.
    pub laid_out: BTreeMap<SyntaxKind, u64>,
    /// Nodes printed as verbatim source text, by kind, descendants included.
    pub verbatim: BTreeMap<SyntaxKind, u64>,
    /// Items (and imports and inner attributes) whose header could not be
    /// laid out, so the whole item was printed verbatim.
    pub verbatim_items: u64,
}

impl Coverage {
    /// Adds `other`'s counts to these.
    pub fn add(&mut self, other: &Self) {
        for (kind, n) in &other.laid_out {
            let slot = self.laid_out.entry(*kind).or_insert(0);
            *slot = slot.saturating_add(*n);
        }
        for (kind, n) in &other.verbatim {
            let slot = self.verbatim.entry(*kind).or_insert(0);
            *slot = slot.saturating_add(*n);
        }
        self.verbatim_items = self.verbatim_items.saturating_add(other.verbatim_items);
    }

    fn laid_out(&mut self, kind: SyntaxKind) {
        let slot = self.laid_out.entry(kind).or_insert(0);
        *slot = slot.saturating_add(1);
    }

    fn verbatim(&mut self, node: SyntaxNode<'_>) {
        for d in node.descendants() {
            let slot = self.verbatim.entry(d.kind()).or_insert(0);
            *slot = slot.saturating_add(1);
        }
    }
}

/// The delimiters of a comma-separated list, and whether a flat list has
/// spaces inside them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Delims {
    /// `(a, b)`, `[a, b]`, `<A, B>` and `{A, B}` of an import list.
    Tight,
    /// `{ a, b }` of a type body, a record literal or a field pattern.
    Spaced,
}

/// How a laid-out list treats its trailing comma.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Comma {
    /// A comma exactly when the list is broken (GAP(fmt-trailing-comma)).
    WhenBroken,
    /// The comma of a one-element tuple, kept as written at every width:
    /// `(x,)` is a tuple and `(x)` is not.
    Kept(bool),
}

/// How a comma-separated list is laid out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ListStyle {
    pub(crate) delims: Delims,
    pub(crate) comma: Comma,
    /// Whether a last element that can break by itself (a closure, a
    /// block, a `case`, a list or a record literal) may hug the
    /// delimiters: the others stay on the line and only it breaks
    /// (GAP(fmt-expression-breaks)).
    pub(crate) hug: bool,
}

impl ListStyle {
    pub(crate) const fn new(delims: Delims) -> Self {
        Self {
            delims,
            comma: Comma::WhenBroken,
            hug: false,
        }
    }
}

/// One element of a newline-separated sequence: the file's top level, an
/// interface body, a block or a `case` body.
pub(crate) struct Entry<'t> {
    pub(crate) elem: SyntaxElement<'t>,
    /// Line breaks between the previous entry and this one.
    pub(crate) newlines: usize,
}

/// A comment on a line of its own inside a list.
struct OwnLineComment<'t> {
    text: &'t str,
    blank_before: bool,
}

/// One element of a comma-separated list with its comments.
struct ListItem<'t> {
    leading: Vec<OwnLineComment<'t>>,
    elem: SyntaxElement<'t>,
    trailing: Option<&'t str>,
    /// A blank line before the item, or before its first leading comment.
    blank_before: bool,
    /// A blank line between the leading comments and the element.
    blank_after_leading: bool,
}

/// Formats a file's tree into a document.
pub(crate) struct Formatter<'t> {
    pub(crate) src: Source<'t>,
    pub(crate) coverage: Coverage,
}

/// Whether `kind` is a comment of any kind.
pub(crate) const fn is_comment(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::COMMENT | SyntaxKind::DOC_COMMENT | SyntaxKind::MODULE_DOC
    )
}

/// A comment's text without trailing whitespace.
pub(crate) fn comment_text(token: SyntaxToken<'_>) -> &str {
    token.text().trim_end_matches([' ', '\t', '\r'])
}

/// Whether `kind` is neither trivia nor a line break.
const fn is_significant(kind: SyntaxKind) -> bool {
    !kind.is_trivia() && !matches!(kind, SyntaxKind::NEWLINE)
}

/// The text of `node`'s significant tokens, joined without spaces.
fn compact_text(node: SyntaxNode<'_>) -> String {
    node.tokens()
        .filter(|t| is_significant(t.kind()))
        .map(SyntaxToken::text)
        .collect()
}

/// The text of `node`'s significant tokens, joined by single spaces.
fn spaced_text(node: SyntaxNode<'_>) -> String {
    node.tokens()
        .filter(|t| is_significant(t.kind()))
        .map(SyntaxToken::text)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether the printer puts no space before a header part of this kind.
const fn tight_before(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::COLON
            | SyntaxKind::COLON2
            | SyntaxKind::TYPE_PARAM_LIST
            | SyntaxKind::PARAM_LIST
            | SyntaxKind::TYPE_LIST
            | SyntaxKind::POS_FIELD_LIST
            | SyntaxKind::ATTR_ARG_LIST
            | SyntaxKind::IMPORT_LIST
    )
}

/// Whether the printer puts no space after a header token of this kind.
const fn tight_after(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::AT | SyntaxKind::AT_BANG | SyntaxKind::COLON2
    )
}

/// The nodes whose children the header walk lays out.
const fn is_header(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::FN_DECL
            | SyntaxKind::TYPE_DECL
            | SyntaxKind::ALIAS_DECL
            | SyntaxKind::CONST_DECL
            | SyntaxKind::INTERFACE_DECL
            | SyntaxKind::TEST_DECL
            | SyntaxKind::IMPORT_DECL
            | SyntaxKind::MODULE_PATH
            | SyntaxKind::IMPORT_ITEM
            | SyntaxKind::RENAME
            | SyntaxKind::TYPE_PARAM
            | SyntaxKind::BOUND_LIST
            | SyntaxKind::INTERFACE_REF
            | SyntaxKind::PATH
            | SyntaxKind::INNER_ATTR
            | SyntaxKind::ATTR
            | SyntaxKind::VISIBILITY
            | SyntaxKind::RET_TYPE
            | SyntaxKind::PARAM
            | SyntaxKind::RECORD_FIELD
            | SyntaxKind::VARIANT
            | SyntaxKind::INTERFACE_FN
            | SyntaxKind::LOCAL_FN
    )
}

/// The comma-separated list nodes the printer lays out, with their style.
/// A one-element tuple's comma is settled by [`Formatter::list_node`].
const fn list_style(kind: SyntaxKind) -> Option<ListStyle> {
    match kind {
        SyntaxKind::TYPE_PARAM_LIST
        | SyntaxKind::PARAM_LIST
        | SyntaxKind::TYPE_LIST
        | SyntaxKind::POS_FIELD_LIST
        | SyntaxKind::ATTR_ARG_LIST
        | SyntaxKind::IMPORT_LIST
        | SyntaxKind::LIST_EXPR
        | SyntaxKind::TYPE_ARG_LIST
        | SyntaxKind::POS_PAT_LIST
        | SyntaxKind::LIST_PAT
        | SyntaxKind::TUPLE_EXPR
        | SyntaxKind::TUPLE_PAT
        | SyntaxKind::TUPLE_TYPE => Some(ListStyle::new(Delims::Tight)),
        SyntaxKind::ARG_LIST => Some(ListStyle {
            hug: true,
            ..ListStyle::new(Delims::Tight)
        }),
        SyntaxKind::VARIANT_LIST | SyntaxKind::RECORD_FIELD_LIST | SyntaxKind::FIELD_PAT_LIST => {
            Some(ListStyle::new(Delims::Spaced))
        }
        _ => None,
    }
}

/// Whether a list element can break by itself and so hug the list's
/// delimiters when it comes last: a closure, a block, a `case`, a list or
/// a record literal, or a call whose own last argument hugs, as in
/// `Some(fold(xs, 0, fn(acc, x) {`.
pub(crate) fn hugs(elem: SyntaxElement<'_>) -> bool {
    let Some(node) = elem.as_node() else {
        return false;
    };
    match node.kind() {
        SyntaxKind::CLOSURE
        | SyntaxKind::BLOCK
        | SyntaxKind::CASE_EXPR
        | SyntaxKind::LIST_EXPR
        | SyntaxKind::RECORD_EXPR => true,
        SyntaxKind::CALL_EXPR => node
            .children()
            .find(|c| c.kind() == SyntaxKind::ARG_LIST)
            .and_then(|args| args.children().last())
            .is_some_and(|last| hugs(SyntaxElement::Node(last))),
        _ => false,
    }
}

/// Whether a list of this kind is a tuple, whose one-element form keeps
/// its comma.
const fn is_tuple(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::TUPLE_EXPR | SyntaxKind::TUPLE_PAT | SyntaxKind::TUPLE_TYPE
    )
}

impl<'t> Formatter<'t> {
    pub(crate) const fn new(src: Source<'t>) -> Self {
        Self {
            src,
            coverage: Coverage {
                laid_out: BTreeMap::new(),
                verbatim: BTreeMap::new(),
                verbatim_items: 0,
            },
        }
    }

    /// The whole file: its entries one per line, imports sorted within
    /// their blocks, single blank lines kept, and one final line break.
    /// GAP(fmt-empty-file): a file with nothing but whitespace formats to
    /// the empty file, without a line break.
    pub(crate) fn file(&mut self, root: SyntaxNode<'t>) -> Doc {
        self.coverage.laid_out(root.kind());
        let mut entries = entries(root, None);
        sort_import_blocks(&mut entries);
        if entries.is_empty() {
            return Doc::Nil;
        }
        let mut docs = Vec::new();
        self.lines(&entries, &mut docs);
        docs.push(Doc::hardline());
        Doc::concat(docs)
    }

    /// Entries one per line, a blank line where the source has one or
    /// more, and a comment on the same line as the entry before it kept
    /// there.
    fn lines(&mut self, entries: &[Entry<'t>], docs: &mut Vec<Doc>) {
        for (i, entry) in entries.iter().enumerate() {
            if i > 0 {
                match entry.newlines {
                    0 => docs.push(Doc::text(" ")),
                    1 => docs.push(Doc::hardline()),
                    _ => {
                        docs.push(Doc::hardline());
                        docs.push(Doc::hardline());
                    }
                }
            }
            match entry.elem {
                SyntaxElement::Token(token) if is_comment(token.kind()) => {
                    if i > 0 && entry.newlines == 0 {
                        docs.push(Doc::comment(comment_text(token)));
                    } else {
                        docs.push(Doc::text(comment_text(token)));
                    }
                }
                SyntaxElement::Token(token) => docs.push(Doc::text(token.text())),
                SyntaxElement::Node(node) => docs.push(self.item(node)),
            }
        }
    }

    /// An item, import or inner attribute: its header laid out; a
    /// top-level `ERROR` node (an attribute with no item after it) is kept
    /// verbatim.
    fn item(&mut self, node: SyntaxNode<'t>) -> Doc {
        let anchor = self.src.node_line_indent(node);
        let saved = std::mem::take(&mut self.coverage);
        let laid_out = if is_header(node.kind()) {
            self.header(node, anchor)
        } else {
            None
        };
        let attempt = std::mem::replace(&mut self.coverage, saved);
        if let Some(doc) = laid_out {
            self.coverage.add(&attempt);
            doc
        } else {
            self.coverage.verbatim_items = self.coverage.verbatim_items.saturating_add(1);
            self.verbatim(node, anchor)
        }
    }

    /// `node` as verbatim source text.
    pub(crate) fn verbatim(&mut self, node: SyntaxNode<'t>, anchor: usize) -> Doc {
        self.coverage.verbatim(node);
        self.src.node(node, anchor)
    }

    /// Lays out a header-like node: leading doc comments, comments and
    /// attributes on lines of their own (a comment on the line of one
    /// stays there), then its parts on one line with single spaces, the
    /// lists among them laid out at the width. A line break between two
    /// parts is the layout's to place; a comment or a blank line between
    /// them keeps its place and breaks the line there, and the parts after
    /// it go one level deeper (GAP(fmt-comment-positions)). `None` only
    /// when a part cannot be laid out.
    fn header(&mut self, node: SyntaxNode<'t>, anchor: usize) -> Option<Doc> {
        self.coverage.laid_out(node.kind());
        let mut parts = Parts::default();
        // Line breaks since the last leading line, and what lies between
        // two parts of the header proper.
        let mut newlines = 0usize;
        let mut gap = Gap::default();
        // Whether anything was printed yet, and whether the header proper
        // (after doc comments and attributes) started.
        let mut any = false;
        let mut started = false;
        let mut prev: Option<SyntaxKind> = None;
        for child in node.children_with_tokens() {
            let kind = child.kind();
            if started && gap.take_trivia(child) {
                continue;
            }
            match kind {
                SyntaxKind::WHITESPACE => continue,
                SyntaxKind::NEWLINE => {
                    newlines = newlines.saturating_add(1);
                    continue;
                }
                _ => {}
            }
            if !started && (is_comment(kind) || kind == SyntaxKind::ATTR) {
                // A leading line. A comment on the line of the one before
                // stays at its end.
                let line = match child {
                    SyntaxElement::Token(token) => {
                        if any && newlines == 0 {
                            parts.push(Doc::text(" "));
                            parts.push(Doc::comment(comment_text(token)));
                            continue;
                        }
                        Doc::text(comment_text(token))
                    }
                    SyntaxElement::Node(attr) => {
                        let attr_anchor = self.src.node_line_indent(attr);
                        self.header(attr, attr_anchor)?
                    }
                };
                if any {
                    parts.push(Doc::hardline());
                    if newlines >= 2 {
                        parts.push(Doc::hardline());
                    }
                }
                parts.push(line);
                any = true;
                newlines = 0;
                continue;
            }
            if !started && any {
                parts.push(Doc::hardline());
                if newlines >= 2 {
                    parts.push(Doc::hardline());
                }
            }
            // A part of the header proper. A line break alone between two
            // parts is dropped: the layout places line breaks, and the
            // value after `=` is laid out by the assignment rule.
            let after_eq = prev == Some(SyntaxKind::EQ);
            let broke = started && parts.gap(&std::mem::take(&mut gap), false);
            let space = started && !broke && !tight_before(kind) && !prev.is_some_and(tight_after);
            match child {
                SyntaxElement::Node(value) if after_eq => parts.push(if broke {
                    self.node(value)
                } else {
                    self.assign_tail(value)
                }),
                SyntaxElement::Token(token) => {
                    if space {
                        parts.push(Doc::text(" "));
                    }
                    parts.push(self.token(token));
                }
                SyntaxElement::Node(part) => {
                    let mut part = self.part(part, anchor)?;
                    // A return type stays on the line of `)`, so a header
                    // that does not fit breaks its parameters first
                    // (GAP(fmt-expression-breaks)).
                    if kind == SyntaxKind::RET_TYPE && !part.has_hard_break() {
                        part = Doc::flat(part);
                    }
                    if space {
                        parts.push(Doc::text(" "));
                    }
                    parts.push(part);
                }
            }
            started = true;
            any = true;
            newlines = 0;
            prev = Some(kind);
        }
        // A node never ends with a comment (the tree builder puts it in the
        // node of the next token); if one did, its place is unknown.
        if gap.has_comment() {
            return None;
        }
        Some(parts.finish(INDENT))
    }

    /// A node inside a header: a list, a nested header, an interface body,
    /// a body block, or an expression, pattern or type laid out by
    /// [`Self::node`].
    fn part(&mut self, node: SyntaxNode<'t>, anchor: usize) -> Option<Doc> {
        let kind = node.kind();
        if let Some(style) = list_style(kind) {
            return self.list_node(node, style);
        }
        if kind == SyntaxKind::INTERFACE_BODY {
            return self.interface_body(node);
        }
        if is_header(kind) {
            return self.header(node, anchor);
        }
        if kind == SyntaxKind::BLOCK {
            return Some(self.body_block(node));
        }
        Some(self.node(node))
    }

    /// An element of a list: a token as written, a node laid out by
    /// [`Self::node`].
    fn element(&mut self, elem: SyntaxElement<'t>) -> Doc {
        match elem {
            SyntaxElement::Token(token) => Doc::text(token.text()),
            SyntaxElement::Node(node) => self.node(node),
        }
    }

    /// Any node below an item header, laid out at the width; or, when its
    /// layout gives `None` (a gap D-470 rejects, or a shape the grammar does
    /// not produce), verbatim, with its own line structure.
    pub(crate) fn node(&mut self, node: SyntaxNode<'t>) -> Doc {
        self.attempt(node, |f, node| {
            let kind = node.kind();
            if let Some(style) = list_style(kind) {
                return f.list_node(node, style);
            }
            if is_header(kind) {
                let anchor = f.src.node_line_indent(node);
                return f.header(node, anchor);
            }
            f.syntax(node)
        })
    }

    /// The block of an item or a local function: broken over lines
    /// unless empty, or verbatim as [`Self::node`] falls back.
    pub(crate) fn body_block(&mut self, node: SyntaxNode<'t>) -> Doc {
        self.attempt(node, |f, node| f.block(node, true))
    }

    /// `layout(node)`, or `node` verbatim when it gives `None`, counting
    /// only the nodes of the result.
    fn attempt(
        &mut self,
        node: SyntaxNode<'t>,
        layout: impl FnOnce(&mut Self, SyntaxNode<'t>) -> Option<Doc>,
    ) -> Doc {
        if let Some(doc) = self.try_layout(node, layout) {
            doc
        } else {
            let anchor = self.src.node_line_indent(node);
            self.verbatim(node, anchor)
        }
    }

    /// Counts `node` as laid out by the printer.
    pub(crate) fn laid_out(&mut self, node: SyntaxNode<'_>) {
        self.coverage.laid_out(node.kind());
    }

    /// `node`'s text as a fragment of source, kept as written: a string
    /// literal, which may span lines, or a literal. Counted as laid out,
    /// since keeping them as written is the layout's rule (D-1030).
    pub(crate) fn as_written(&mut self, node: SyntaxNode<'t>) -> Doc {
        for d in node.descendants() {
            self.coverage.laid_out(d.kind());
        }
        let anchor = self.src.node_line_indent(node);
        self.src.node(node, anchor)
    }

    /// A token as written. A token that holds a line break (a multi-line
    /// raw string, as in a literal pattern) is a fragment of source whose
    /// later lines start inside the string and are written exactly.
    pub(crate) fn token(&self, token: SyntaxToken<'t>) -> Doc {
        if token.text().contains('\n') {
            let range = token.text_range();
            let start = crate::source::to_usize(range.start());
            let end = crate::source::to_usize(range.end());
            self.src.fragment(start, end, self.src.line_indent(start))
        } else {
            Doc::text(token.text())
        }
    }

    /// A list node, its first and last children its delimiters.
    pub(crate) fn list_node(&mut self, node: SyntaxNode<'t>, style: ListStyle) -> Option<Doc> {
        self.coverage.laid_out(node.kind());
        let children: Vec<SyntaxElement<'t>> = node.children_with_tokens().collect();
        let (open, rest) = children.split_first()?;
        let (close, inner) = rest.split_last()?;
        let (open, close) = (open.as_token()?, close.as_token()?);
        let mut style = style;
        // `(x,)` is a tuple and `(x)` is not; and Appendix A's `field_pats`
        // has no comma after a lone `..` (`P { .. }`). Both keep their
        // comma as written.
        let lone_rest = node.kind() == SyntaxKind::FIELD_PAT_LIST
            && node.children().count() == 1
            && node
                .children()
                .next()
                .is_some_and(|c| c.kind() == SyntaxKind::REST_PAT);
        if lone_rest || (is_tuple(node.kind()) && node.children().count() == 1) {
            let comma = inner.iter().any(|c| c.kind() == SyntaxKind::COMMA);
            style.comma = Comma::Kept(comma);
        }
        self.list(open, inner, close, style)
    }

    /// A comma-separated list between the delimiters `open` and `close`.
    /// Flat when it fits and has no comment, blank line or multi-line
    /// element; otherwise one element per line with a trailing comma, or,
    /// for a list that hugs, with only its last element broken. `None`
    /// only for a shape the grammar does not produce.
    ///
    /// GAP(fmt-trailing-comma): a laid-out list has a trailing comma exactly
    /// when it is broken over lines (AGENTS.md's style), so the formatter
    /// adds or removes that one comma; a one-element tuple, and a field
    /// pattern list that holds only `..`, keep their comma as written.
    /// GAP(fmt-list-blank-lines): a blank line kept between two
    /// elements (D-1030) breaks the list.
    pub(crate) fn list(
        &mut self,
        open: SyntaxToken<'t>,
        inner: &[SyntaxElement<'t>],
        close: SyntaxToken<'t>,
        style: ListStyle,
    ) -> Option<Doc> {
        if !matches!(
            open.kind(),
            SyntaxKind::L_PAREN | SyntaxKind::LT | SyntaxKind::L_BRACE | SyntaxKind::L_BRACK
        ) || !matches!(
            close.kind(),
            SyntaxKind::R_PAREN | SyntaxKind::GT | SyntaxKind::R_BRACE | SyntaxKind::R_BRACK
        ) {
            return None;
        }
        let parsed = parse_list(inner)?;
        let open_text = Doc::text(open.text());
        let close_text = Doc::text(close.text());
        if parsed.items.is_empty() && parsed.dangling.is_empty() && parsed.open_comment.is_none() {
            return Some(Doc::concat(vec![open_text, close_text]));
        }
        let elems: Vec<Doc> = parsed
            .items
            .iter()
            .map(|item| self.element(item.elem))
            .collect();
        let forced = parsed.open_comment.is_some()
            || !parsed.dangling.is_empty()
            || parsed.items.iter().enumerate().any(|(i, item)| {
                !item.leading.is_empty() || item.trailing.is_some() || (i > 0 && item.blank_before)
            });

        if forced {
            return Some(forced_list(open_text, close_text, &parsed, elems, style));
        }

        let edge = || match style.delims {
            Delims::Tight => Doc::softline(),
            Delims::Spaced => Doc::line(),
        };
        // The hugging layout: every element but the last flat on the line
        // of `open`, and the last one broken by itself.
        let hug = match elems.split_last() {
            Some((last, firsts))
                if style.hug
                    && parsed.items.last().is_some_and(|item| hugs(item.elem))
                    && !firsts.iter().any(Doc::has_hard_break) =>
            {
                let mut flat = Vec::new();
                for elem in firsts {
                    flat.push(elem.clone());
                    flat.push(Doc::text(", "));
                }
                Some(Doc::concat(vec![
                    open_text.clone(),
                    Doc::flat(Doc::concat(flat)),
                    last.expand_last(),
                    close_text.clone(),
                ]))
            }
            _ => None,
        };
        let mut body = vec![edge()];
        for (i, elem) in elems.into_iter().enumerate() {
            if i > 0 {
                body.push(Doc::text(","));
                body.push(Doc::line());
            }
            body.push(elem);
        }
        body.push(match style.comma {
            Comma::WhenBroken => Doc::if_break(Doc::text(","), Doc::Nil),
            Comma::Kept(true) => Doc::text(","),
            Comma::Kept(false) => Doc::Nil,
        });
        let normal = Doc::group(Doc::concat(vec![
            open_text,
            Doc::nest(INDENT, Doc::concat(body)),
            edge(),
            close_text,
        ]));
        Some(match hug {
            None => normal,
            Some(hug) if normal.has_hard_break() => Doc::choice(hug, normal),
            Some(hug) => Doc::choice(Doc::flat(normal.clone()), Doc::choice(hug, normal)),
        })
    }

    /// An interface body: `{}` when empty, else one function per line.
    fn interface_body(&mut self, node: SyntaxNode<'t>) -> Option<Doc> {
        self.coverage.laid_out(node.kind());
        let children: Vec<SyntaxElement<'t>> = node.children_with_tokens().collect();
        let (open, rest) = children.split_first()?;
        let (close, _) = rest.split_last()?;
        if open.kind() != SyntaxKind::L_BRACE || close.kind() != SyntaxKind::R_BRACE {
            return None;
        }
        let mut entries = entries(node, Some((SyntaxKind::L_BRACE, SyntaxKind::R_BRACE)));
        if entries.is_empty() {
            return Some(Doc::text("{}"));
        }
        if entries.iter().any(|e| {
            !matches!(e.elem, SyntaxElement::Node(n) if n.kind() == SyntaxKind::INTERFACE_FN)
                && !is_comment(e.elem.kind())
        }) {
            return None;
        }
        let mut docs = vec![Doc::text("{")];
        // A comment on the line of `{` stays there.
        let mut start = 0;
        if let Some(first) = entries.first()
            && first.newlines == 0
            && is_comment(first.elem.kind())
        {
            docs.push(Doc::text(" "));
            docs.push(Doc::comment(first.elem.text().trim_end()));
            start = 1;
        }
        let body_entries = entries.split_off(start);
        if !body_entries.is_empty() {
            let mut body = vec![Doc::hardline()];
            self.interface_lines(&body_entries, &mut body);
            docs.push(Doc::nest(INDENT, Doc::concat(body)));
        }
        docs.push(Doc::hardline());
        docs.push(Doc::text("}"));
        Some(Doc::concat(docs))
    }

    /// The lines of an interface body, as [`Self::lines`] but laying out
    /// each function as a header, or verbatim when it cannot be. The first
    /// entry starts a line of its own whatever the source had.
    fn interface_lines(&mut self, entries: &[Entry<'t>], docs: &mut Vec<Doc>) {
        for (i, entry) in entries.iter().enumerate() {
            if i > 0 {
                match entry.newlines {
                    0 => docs.push(Doc::text(" ")),
                    1 => docs.push(Doc::hardline()),
                    _ => {
                        docs.push(Doc::hardline());
                        docs.push(Doc::hardline());
                    }
                }
            }
            match entry.elem {
                SyntaxElement::Token(token) => {
                    if i > 0 && entry.newlines == 0 {
                        docs.push(Doc::comment(comment_text(token)));
                    } else {
                        docs.push(Doc::text(comment_text(token)));
                    }
                }
                SyntaxElement::Node(node) => docs.push(self.element(SyntaxElement::Node(node))),
            }
        }
    }
}

/// A list that holds comments or blank lines: one element per line, each
/// followed by its comma (but for a one-element tuple without one), the
/// comments and single blank lines where they were.
fn forced_list(
    open_text: Doc,
    close_text: Doc,
    parsed: &ParsedList<'_>,
    elems: Vec<Doc>,
    style: ListStyle,
) -> Doc {
    let mut body = Vec::new();
    for (i, (item, elem)) in parsed.items.iter().zip(elems).enumerate() {
        body.push(Doc::hardline());
        if i > 0 && item.blank_before {
            body.push(Doc::hardline());
        }
        for (j, comment) in item.leading.iter().enumerate() {
            if j > 0 && comment.blank_before {
                body.push(Doc::hardline());
            }
            body.push(Doc::text(comment.text));
            body.push(Doc::hardline());
        }
        if item.blank_after_leading {
            body.push(Doc::hardline());
        }
        body.push(elem);
        let last = i.saturating_add(1) == parsed.items.len();
        if !(last && style.comma == Comma::Kept(false)) {
            body.push(Doc::text(","));
        }
        if let Some(trailing) = item.trailing {
            body.push(Doc::text(" "));
            body.push(Doc::comment(trailing));
        }
    }
    for (j, comment) in parsed.dangling.iter().enumerate() {
        body.push(Doc::hardline());
        if (j > 0 || !parsed.items.is_empty()) && comment.blank_before {
            body.push(Doc::hardline());
        }
        body.push(Doc::text(comment.text));
    }
    let mut docs = vec![open_text];
    if let Some(comment) = parsed.open_comment {
        docs.push(Doc::text(" "));
        docs.push(Doc::comment(comment));
    }
    docs.push(Doc::nest(INDENT, Doc::concat(body)));
    docs.push(Doc::hardline());
    docs.push(close_text);
    Doc::concat(docs)
}

/// A list's contents split into elements and comments.
struct ParsedList<'t> {
    open_comment: Option<&'t str>,
    items: Vec<ListItem<'t>>,
    dangling: Vec<OwnLineComment<'t>>,
}

/// What the list walk read last, comments and line breaks aside.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Last {
    Open,
    Elem,
    Comma,
}

/// Splits the elements between a list's delimiters into elements with
/// their comments. `None` for two elements without a comma between them,
/// or a comma without an element before it.
fn parse_list<'t>(inner: &[SyntaxElement<'t>]) -> Option<ParsedList<'t>> {
    let mut parsed = ParsedList {
        open_comment: None,
        items: Vec::new(),
        dangling: Vec::new(),
    };
    let mut phase = Last::Open;
    let mut newlines = 0usize;
    let mut pending: Vec<OwnLineComment<'t>> = Vec::new();
    for &child in inner {
        match child.kind() {
            SyntaxKind::WHITESPACE => {}
            SyntaxKind::NEWLINE => newlines = newlines.saturating_add(1),
            kind if is_comment(kind) => {
                let text = child.as_token().map_or("", comment_text);
                if newlines == 0 {
                    match phase {
                        Last::Open => {
                            if parsed.open_comment.is_some() {
                                return None;
                            }
                            parsed.open_comment = Some(text);
                        }
                        Last::Elem | Last::Comma => {
                            let item = parsed.items.last_mut()?;
                            if item.trailing.is_some() || !pending.is_empty() {
                                // A second comment after one element (one
                                // before its comma, one after it): it goes
                                // on a line of its own, before the next
                                // element.
                                pending.push(OwnLineComment {
                                    text,
                                    blank_before: false,
                                });
                            } else {
                                item.trailing = Some(text);
                            }
                        }
                    }
                } else {
                    pending.push(OwnLineComment {
                        text,
                        blank_before: newlines >= 2,
                    });
                }
                newlines = 0;
            }
            SyntaxKind::COMMA => {
                // GAP(fmt-comment-positions): a comment between an element
                // and its comma goes after the comma, the list's own
                // separator, whose place the style decides (it adds or
                // removes the last one): a comment on the element's line
                // stays at the end of that line, one on a line of its own
                // goes before the next element.
                if phase != Last::Elem {
                    return None;
                }
                phase = Last::Comma;
                newlines = 0;
            }
            _ => {
                if phase == Last::Elem {
                    return None;
                }
                let blank_before = pending.first().map_or(newlines >= 2, |c| c.blank_before);
                parsed.items.push(ListItem {
                    blank_after_leading: !pending.is_empty() && newlines >= 2,
                    leading: std::mem::take(&mut pending),
                    elem: child,
                    trailing: None,
                    blank_before,
                });
                phase = Last::Elem;
                newlines = 0;
            }
        }
    }
    parsed.dangling = pending;
    Some(parsed)
}

/// The children of `node` that are not whitespace or line breaks, each
/// with the number of line breaks before it. With `delims`, the first and
/// last children (the delimiters) are left out.
pub(crate) fn entries<'t>(
    node: SyntaxNode<'t>,
    delims: Option<(SyntaxKind, SyntaxKind)>,
) -> Vec<Entry<'t>> {
    let mut out = Vec::new();
    let mut newlines = 0usize;
    let children: Vec<SyntaxElement<'t>> = node.children_with_tokens().collect();
    let inner = match delims {
        Some(_) => children
            .get(1..children.len().saturating_sub(1))
            .unwrap_or(&[]),
        None => &children[..],
    };
    for &child in inner {
        match child.kind() {
            SyntaxKind::WHITESPACE => {}
            SyntaxKind::NEWLINE => newlines = newlines.saturating_add(1),
            _ => {
                out.push(Entry {
                    elem: child,
                    newlines,
                });
                newlines = 0;
            }
        }
    }
    out
}

/// Sorts each block of imports by its module path's text in byte order
/// (D-1030). GAP(fmt-import-blocks): a block is a run of imports on
/// consecutive lines with no blank line or comment between or after them;
/// equal paths are ordered by the import's whole text.
fn sort_import_blocks(entries: &mut [Entry<'_>]) {
    let is_import = |e: &Entry<'_>| {
        matches!(e.elem, SyntaxElement::Node(n)
            if n.kind() == SyntaxKind::IMPORT_DECL
                && !n.tokens().any(|t| is_comment(t.kind())))
    };
    let mut i = 0;
    while i < entries.len() {
        let Some(first) = entries.get(i) else { break };
        if !is_import(first) {
            i = i.saturating_add(1);
            continue;
        }
        let mut end = i.saturating_add(1);
        while let Some(next) = entries.get(end) {
            if next.newlines == 1 && is_import(next) {
                end = end.saturating_add(1);
            } else {
                break;
            }
        }
        // An import followed by a comment on its own line keeps its place.
        if entries
            .get(end)
            .is_some_and(|e| e.newlines == 0 && is_comment(e.elem.kind()))
        {
            end = end.saturating_sub(1);
        }
        if let Some(block) = entries.get_mut(i..end)
            && block.len() > 1
        {
            let first_newlines = block.first().map_or(0, |e| e.newlines);
            block.sort_by_cached_key(|e| match e.elem {
                SyntaxElement::Node(n) => (import_path(n), spaced_text(n)),
                SyntaxElement::Token(_) => (String::new(), String::new()),
            });
            for (k, e) in block.iter_mut().enumerate() {
                e.newlines = if k == 0 { first_newlines } else { 1 };
            }
        }
        i = end.max(i.saturating_add(1));
    }
}

/// The text of an import's module path, such as `std::map`.
fn import_path(node: SyntaxNode<'_>) -> String {
    node.children()
        .find(|c| c.kind() == SyntaxKind::MODULE_PATH)
        .map(compact_text)
        .unwrap_or_default()
}
