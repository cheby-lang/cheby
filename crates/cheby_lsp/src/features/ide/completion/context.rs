//! Completion contexts (X §4.1): what the cursor is completing, read from
//! the tree around the token on its left. Syntax only, so it works at L0
//! and on every broken form of the syntax map's §2.2.
//!
//! Two things are found:
//!
//! - The **partial**: the word being typed, which every item replaces. It
//!   is the name, keyword or `_` that touches the cursor on its left, or an
//!   empty range at the cursor.
//! - The **anchor**: the significant token before the partial. Most
//!   contexts follow from it and its parent, so a broken tree reads like a
//!   whole one: `list::` is `::` in an `ERROR` right after a path, `foo(` an
//!   open argument list, `let y = ` an `=` in a `LET_STMT`.
//!
//! A line break between the anchor and the cursor matters, since it ends a
//! statement (D-231): after a complete expression and a line break, the
//! cursor starts a new statement, item, arm or field, by the node that
//! holds the line break. After a token that wants more, such as `=` or
//! `(`, the line break continues it.
//!
//! The contexts of package B (X §4.2 to §4.4), [`Context::Dot`],
//! [`Context::Pipe`] and a [`Context::Qualified`] with `piped`, are found
//! here too, and `completion::{dot, pipe}` build their items.

use cheby_driver::analysis::{TextRange, TextSize};
use cheby_syntax::{SyntaxElement, SyntaxKind as K, SyntaxNode, SyntaxToken};

use crate::features::ide::cursor::{is_significant, prev_significant, token_at};

/// What the cursor completes, with what its items need from the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Context<'t> {
    /// Nothing: a comment, string text, a number, a name being declared, or
    /// the middle of a complete line.
    None,
    /// A module path after `import`: `prefix` is the segments written
    /// before the partial, `["std"]` for `import std::`.
    ImportPath {
        /// The segments before the partial.
        prefix: Vec<String>,
    },
    /// An item of an import list: `import std::list::{`.
    ImportItems {
        /// The imported module's path, by segment.
        module: Vec<String>,
        /// The names the list already holds, the partial left out.
        written: Vec<String>,
    },
    /// An attribute name after `@`, or after `@!` when `inner`.
    Attribute {
        /// `@!`: only `target` applies (§4.8).
        inner: bool,
    },
    /// A name after `m::` or `I::`, or `m::I::`.
    Qualified {
        /// The segments before the partial: a module, an interface, or a
        /// module then an interface.
        qualifier: Vec<String>,
        /// The namespace the path is read in.
        ns: PathNs,
        /// The left side of a pipe whose right side this path is:
        /// `xs |> list::` (X §4.3). Package B writes those items.
        piped: Option<SyntaxNode<'t>>,
    },
    /// A field or pipe-style function after `.` (X §4.2), for package B.
    Dot {
        /// What the `.` reads.
        receiver: Receiver<'t>,
        /// The `.` itself.
        dot: SyntaxToken<'t>,
    },
    /// A function after `|>` (X §4.3), for package B.
    Pipe {
        /// The pipe, a `BIN_EXPR`.
        pipe: SyntaxNode<'t>,
        /// Its left side.
        lhs: SyntaxNode<'t>,
    },
    /// A type.
    Type,
    /// A pattern: a `case` arm's head, after `let`, `let guard`,
    /// `let assert`, `use` or `fn(` (D-1531).
    Pattern {
        /// What the pattern matches, when the tree says.
        subject: Subject<'t>,
    },
    /// A field of a record literal or update.
    RecordField {
        /// The `RECORD_EXPR`.
        record: SyntaxNode<'t>,
        /// The field names already written, the partial left out.
        written: Vec<String>,
        /// Nothing is written yet, so `..base` may start an update.
        empty: bool,
    },
    /// The name at the start of an interpolation, `"{|"`.
    Interpolation,
    /// An expression.
    Expr {
        /// The cursor starts a statement, so statement keywords apply.
        statement: bool,
        /// Where the expected type comes from.
        site: ExprSite<'t>,
    },
    /// An item at the top level.
    Item {
        /// After `pub`, `priv` or `exposed`: only what may follow them.
        after_visibility: bool,
    },
}

/// The namespace a qualified path is read in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathNs {
    /// An expression: functions, constants, constructors.
    Value,
    /// A type.
    Type,
    /// A pattern: constructors only.
    Pattern,
}

/// What a `.` reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Receiver<'t> {
    /// An expression: the first child of the `FIELD_EXPR`.
    Expr(SyntaxNode<'t>),
    /// Names of an interpolation, `"{p.|"`: the `INTERPOLATION` and the
    /// names before the `.`, the root first.
    Interpolation {
        /// The `INTERPOLATION` node.
        node: SyntaxNode<'t>,
        /// The root and the fields read before the `.`.
        names: Vec<SyntaxToken<'t>>,
    },
}

/// What a pattern matches, from the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Subject<'t> {
    /// An arm of this `CASE_EXPR`.
    Case(SyntaxNode<'t>),
    /// The pattern of this `LET_STMT`, of `let`, `let guard` or
    /// `let assert`, whose value after `=` is the subject, also with a
    /// type annotation (D-1531).
    Let(SyntaxNode<'t>),
    /// Not known from the tree.
    Unknown,
}

/// Where an expression's expected type comes from (X §4.1's rank 00).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExprSite<'t> {
    /// The partial is this `PATH_EXPR`, whose own fact may hold it.
    Path(SyntaxNode<'t>),
    /// Argument `index` of the call whose `ARG_LIST` this is.
    Arg {
        /// The `ARG_LIST`.
        args: SyntaxNode<'t>,
        /// The argument's position: commas before the cursor (X §4.7).
        index: usize,
    },
    /// The value of this `LET_STMT`, or of this `GUARD_STMT` of a
    /// `let guard` (D-1531).
    LetValue(SyntaxNode<'t>),
    /// The body of this `CASE_ARM` or `COND_ARM`.
    ArmBody(SyntaxNode<'t>),
    /// The value of this `FIELD_INIT`.
    FieldValue(SyntaxNode<'t>),
    /// Nowhere known.
    Unknown,
}

/// A context and the range its items replace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found<'t> {
    /// What is completed.
    pub context: Context<'t>,
    /// The word being typed, which every item's edit replaces. Empty at
    /// the cursor when no word is being typed.
    pub partial: TextRange,
    /// The significant token before the partial, if any.
    pub anchor: Option<SyntaxToken<'t>>,
}

impl<'t> Found<'t> {
    /// `context` with nothing typed at `offset`.
    fn at(context: Context<'t>, offset: TextSize) -> Self {
        Self {
            context,
            partial: TextRange::empty(offset),
            anchor: None,
        }
    }
}

/// The context of a cursor at `offset` of the tree under `root`.
pub(crate) fn detect(root: SyntaxNode<'_>, offset: TextSize) -> Found<'_> {
    let Some(left) = token_at(root, offset).left() else {
        return Found::at(
            Context::Item {
                after_visibility: false,
            },
            offset,
        );
    };
    if inside_text(left, offset) {
        return Found::at(Context::None, offset);
    }
    let partial = is_word(left.kind()).then_some(left);
    let range = partial.map_or(TextRange::empty(offset), SyntaxToken::text_range);
    let start = partial.map_or(Some(left), SyntaxToken::prev_token);
    let anchor = start.and_then(|t| {
        if is_significant(t.kind()) {
            Some(t)
        } else {
            prev_significant(t)
        }
    });
    let context = partial
        .filter(|p| !p.kind().is_keyword())
        .and_then(by_partial)
        .unwrap_or_else(|| by_anchor(root, start, anchor, range.start()));
    Found {
        context,
        partial: range,
        anchor,
    }
}

/// Whether the cursor is inside text no name can be typed in: a comment,
/// a string's text, a number, a lexer error.
fn inside_text(left: SyntaxToken<'_>, offset: TextSize) -> bool {
    let range = left.text_range();
    match left.kind() {
        K::COMMENT
        | K::DOC_COMMENT
        | K::MODULE_DOC
        | K::ERROR_TOKEN
        | K::STRING_START
        | K::STRING_TEXT
        | K::STRING_ESCAPE
        | K::INT_NUMBER
        | K::FLOAT_NUMBER => true,
        // A raw string's token holds its closing quote, so the end is
        // outside it.
        K::RAW_STRING => offset < range.end(),
        _ => false,
    }
}

/// Whether a token of `kind` is a word a completion may replace.
fn is_word(kind: K) -> bool {
    matches!(kind, K::LOWER_IDENT | K::UPPER_IDENT | K::UNDERSCORE) || kind.is_keyword()
}

/// The context a partial name, not a keyword, gives by its own parent: a
/// later segment of a path, a module path, an import item, an attribute,
/// a field, an interpolation, or a name being declared. `None` when the
/// anchor decides.
fn by_partial(partial: SyntaxToken<'_>) -> Option<Context<'_>> {
    let parent = partial.parent()?;
    match parent.kind() {
        K::PATH => {
            let before = segments_before(parent, partial.text_range().start());
            if before.is_empty() {
                return None;
            }
            Some(Context::Qualified {
                qualifier: before,
                ns: path_ns(parent),
                piped: piped_lhs(parent),
            })
        }
        K::MODULE_PATH => Some(Context::ImportPath {
            prefix: segments_before(parent, partial.text_range().start()),
        }),
        K::IMPORT_ITEM => {
            let list = parent.parent().filter(|l| l.kind() == K::IMPORT_LIST)?;
            Some(import_items(list, Some(partial)))
        }
        K::ATTR => Some(Context::Attribute { inner: false }),
        K::INNER_ATTR => Some(Context::Attribute { inner: true }),
        K::FIELD_EXPR => {
            let dot = parent.first_token_of_kind(K::DOT)?;
            let receiver = parent.first_child()?;
            Some(Context::Dot {
                receiver: Receiver::Expr(receiver),
                dot,
            })
        }
        K::INTERPOLATION => Some(interpolation(parent, partial.text_range().start())),
        K::FIELD_INIT if parent.first_token() == Some(partial) => {
            let record = parent.parent().filter(|r| r.kind() == K::RECORD_EXPR)?;
            Some(record_field(record, Some(partial)))
        }
        K::ERROR if parent.parent().is_some_and(|p| p.kind() == K::SOURCE_FILE) => {
            Some(Context::Item {
                after_visibility: false,
            })
        }
        // A name being declared: nothing to offer (X §4.1's `None`).
        K::FN_DECL
        | K::TYPE_DECL
        | K::ALIAS_DECL
        | K::CONST_DECL
        | K::INTERFACE_DECL
        | K::INTERFACE_FN
        | K::VARIANT
        | K::RECORD_FIELD
        | K::TYPE_PARAM
        | K::LOCAL_FN
        | K::RENAME
        | K::TEST_DECL => Some(Context::None),
        _ => None,
    }
}

/// The context from the anchor, the token before the partial or the
/// cursor. `start` is the token right before the partial, or the cursor's
/// left token, and `at` where the partial starts.
fn by_anchor<'t>(
    root: SyntaxNode<'t>,
    start: Option<SyntaxToken<'t>>,
    anchor: Option<SyntaxToken<'t>>,
    at: TextSize,
) -> Context<'t> {
    let Some(anchor) = anchor else {
        return Context::Item {
            after_visibility: false,
        };
    };
    if let Some(context) = open_anchor(anchor, at) {
        return context;
    }
    // A complete token: a line break before the cursor starts something
    // new, else the cursor is in the middle of a line.
    let broken = start.is_some_and(|s| {
        std::iter::successors(Some(s), |t| t.prev_token())
            .take_while(|t| !is_significant(t.kind()))
            .any(|t| t.kind() == K::NEWLINE)
    });
    if !broken {
        return Context::None;
    }
    let container = start.and_then(SyntaxToken::parent).unwrap_or(root);
    line_start(container, anchor)
}

/// The context of a cursor that starts a line in `container`, the node
/// that holds the line break. `anchor` is the last token before it.
fn line_start<'t>(container: SyntaxNode<'t>, anchor: SyntaxToken<'t>) -> Context<'t> {
    match container.kind() {
        K::BLOCK => Context::Expr {
            statement: true,
            site: ExprSite::Unknown,
        },
        // A cursor after an unclosed body's last line is outside its node,
        // in the file (F §4.1).
        K::SOURCE_FILE if in_unclosed_body(anchor) => Context::Expr {
            statement: true,
            site: ExprSite::Unknown,
        },
        K::SOURCE_FILE => Context::Item {
            after_visibility: false,
        },
        K::CASE_BODY => case_arm(container),
        K::RECORD_EXPR => record_field(container, None),
        K::IMPORT_LIST => import_items(container, None),
        K::ARG_LIST => Context::Expr {
            statement: false,
            site: ExprSite::Arg {
                args: container,
                index: commas_before(container, anchor.text_range().end()),
            },
        },
        K::LIST_EXPR | K::TUPLE_EXPR | K::PAREN_EXPR => Context::Expr {
            statement: false,
            site: ExprSite::Unknown,
        },
        K::PARAM_LIST => Context::Pattern {
            subject: Subject::Unknown,
        },
        _ => Context::None,
    }
}

/// The context at the head of an arm of `body`, a `CASE_BODY`: a pattern,
/// or a condition when the `case` has no subject (§5.9.5).
fn case_arm(body: SyntaxNode<'_>) -> Context<'_> {
    match body.parent().filter(|c| c.kind() == K::CASE_EXPR) {
        Some(case) if case.first_child() == Some(body) => Context::Expr {
            statement: false,
            site: ExprSite::Unknown,
        },
        Some(case) => Context::Pattern {
            subject: Subject::Case(case),
        },
        None => Context::Pattern {
            subject: Subject::Unknown,
        },
    }
}

/// Whether `anchor`, the last token before a line in the file, ends a body
/// that was never closed: its item's last significant token is not `}`,
/// and the item has a block.
fn in_unclosed_body(anchor: SyntaxToken<'_>) -> bool {
    let Some(item) = anchor.parent().and_then(|p| {
        p.ancestors()
            .find(|n| n.parent().is_some_and(|f| f.kind() == K::SOURCE_FILE))
    }) else {
        return false;
    };
    anchor.kind() != K::R_BRACE
        && item
            .descendants()
            .any(|n| n.kind() == K::BLOCK || n.kind() == K::CASE_BODY)
}

/// The context after a token that wants more, by its kind and parent.
/// `None` for a token that ends something.
fn open_anchor<'t>(anchor: SyntaxToken<'t>, at: TextSize) -> Option<Context<'t>> {
    let parent = anchor.parent()?;
    let p = parent.kind();
    let expr = |statement: bool, site: ExprSite<'t>| Some(Context::Expr { statement, site });
    let pattern = |subject: Subject<'t>| Some(Context::Pattern { subject });
    match anchor.kind() {
        K::COLON2 => colon2(anchor, parent),
        K::DOT => match p {
            K::FIELD_EXPR => Some(Context::Dot {
                receiver: Receiver::Expr(parent.first_child()?),
                dot: anchor,
            }),
            K::INTERPOLATION => Some(interpolation(parent, at)),
            // `"{p.}"` holds the `.` in an `ERROR`.
            K::ERROR => Some(
                parent
                    .parent()
                    .filter(|n| n.kind() == K::INTERPOLATION)
                    .map_or(Context::None, |node| interpolation(node, at)),
            ),
            _ => Some(Context::None),
        },
        K::PIPE_GT if p == K::BIN_EXPR => Some(Context::Pipe {
            pipe: parent,
            lhs: parent.first_child()?,
        }),
        K::AT if p == K::ATTR => Some(Context::Attribute { inner: false }),
        K::AT_BANG if p == K::INNER_ATTR => Some(Context::Attribute { inner: true }),
        K::INTERP_START => Some(Context::Interpolation),
        K::L_BRACE => match p {
            K::IMPORT_LIST => Some(import_items(parent, None)),
            K::RECORD_EXPR => Some(record_field(parent, None)),
            K::CASE_BODY => Some(case_arm(parent)),
            K::BLOCK => expr(true, ExprSite::Unknown),
            _ => Some(Context::None),
        },
        K::COMMA | K::L_PAREN | K::L_BRACK => Some(list_item(anchor, parent, at)),
        K::COLON => match p {
            K::PARAM | K::LET_STMT | K::RECORD_FIELD | K::CONST_DECL | K::TYPE_PARAM => {
                Some(Context::Type)
            }
            // The annotation of `let guard p: T = v`, as of `let` (D-1531).
            K::GUARD_STMT if is_let_guard(parent) => Some(Context::Type),
            K::FIELD_INIT => expr(false, ExprSite::FieldValue(parent)),
            K::FIELD_PAT => pattern(Subject::Unknown),
            _ => Some(Context::None),
        },
        K::THIN_ARROW | K::LT if matches!(p, K::RET_TYPE | K::FN_TYPE | K::TYPE_ARG_LIST) => {
            Some(Context::Type)
        }
        K::EQ => match p {
            K::LET_STMT => expr(false, ExprSite::LetValue(parent)),
            // The value of `let guard p = v else { … }`, as of `let`, so a
            // `: T` ranks its items and a path's `::` keeps only what a
            // path starts with (D-1530, D-1531).
            K::GUARD_STMT if is_let_guard(parent) => expr(false, ExprSite::LetValue(parent)),
            K::CONST_DECL => expr(false, ExprSite::Unknown),
            K::ALIAS_DECL => Some(Context::Type),
            _ => Some(Context::None),
        },
        K::FAT_ARROW if matches!(p, K::CASE_ARM | K::COND_ARM) => {
            expr(false, ExprSite::ArmBody(parent))
        }
        K::LEFT_ARROW | K::DOT2
            if matches!(p, K::USE_STMT | K::RECORD_EXPR | K::LIST_EXPR | K::SPREAD) =>
        {
            expr(false, ExprSite::Unknown)
        }
        K::PIPE if p == K::OR_PAT => pattern(Subject::Unknown),
        kind if is_operator(kind) && matches!(p, K::BIN_EXPR | K::PREFIX_EXPR) => {
            expr(false, ExprSite::Unknown)
        }
        K::LET_KW => pattern(Subject::Let(parent)),
        // `let assert p = e` and `let guard p = e else { … }` bind a
        // pattern, as `let` does, with `e` as the subject; `assert e` and
        // `guard e else` take an expression (D-1531).
        K::ASSERT_KW | K::GUARD_KW
            if prev_significant(anchor).is_some_and(|t| t.kind() == K::LET_KW) =>
        {
            pattern(Subject::Let(parent))
        }
        K::USE_KW => pattern(Subject::Unknown),
        K::CASE_KW | K::ASSERT_KW | K::GUARD_KW | K::WHEN_KW | K::PANIC_KW => {
            expr(false, ExprSite::Unknown)
        }
        K::IMPORT_KW => Some(Context::ImportPath { prefix: Vec::new() }),
        K::PUB_KW | K::PRIV_KW | K::EXPOSED_KW => Some(Context::Item {
            after_visibility: true,
        }),
        K::DYN_KW => Some(Context::Type),
        // Tokens that want something no completion gives: a name being
        // declared, a string, a block.
        K::FN_KW
        | K::TYPE_KW
        | K::CONST_KW
        | K::INTERFACE_KW
        | K::TEST_KW
        | K::AS_KW
        | K::ELSE_KW => Some(Context::None),
        _ => None,
    }
}

/// Whether `stmt`, a `GUARD_STMT`, is a `let guard`, which binds a
/// pattern and has a value after `=`, not a `guard` with a condition.
fn is_let_guard(stmt: SyntaxNode<'_>) -> bool {
    stmt.first_token_of_kind(K::LET_KW).is_some()
}

/// The context after `::`: a qualified path, broken or whole, or a module
/// path of an import.
fn colon2<'t>(colon2: SyntaxToken<'t>, parent: SyntaxNode<'t>) -> Option<Context<'t>> {
    match parent.kind() {
        // `list::` alone parses as a path, then `::` in an `ERROR` (syntax
        // map §2.2): the path is the token before.
        K::ERROR => {
            let last = prev_significant(colon2)?;
            // In a pattern, `shape::` parses as the binding `shape`, then
            // `::` in an `ERROR`: the module's constructors, as after
            // `shape::S` (X §4.1's `x::`).
            if let Some(binding) = last.parent().filter(|p| p.kind() == K::IDENT_PAT) {
                if binding.last_token() != Some(last) || last.kind() != K::LOWER_IDENT {
                    return Some(Context::None);
                }
                return Some(Context::Qualified {
                    qualifier: vec![last.text().to_owned()],
                    ns: PathNs::Pattern,
                    piped: None,
                });
            }
            let path = last.parent().filter(|p| p.kind() == K::PATH)?;
            if path.last_token() != Some(last) {
                return Some(Context::None);
            }
            Some(Context::Qualified {
                qualifier: segments_before(path, colon2.text_range().end()),
                ns: path_ns(path),
                piped: piped_lhs(path),
            })
        }
        K::PATH => Some(Context::Qualified {
            qualifier: segments_before(parent, colon2.text_range().end()),
            ns: path_ns(parent),
            piped: piped_lhs(parent),
        }),
        K::IMPORT_DECL => {
            let path = parent.first_child_of_kind(K::MODULE_PATH)?;
            Some(Context::ImportPath {
                prefix: segments_before(path, colon2.text_range().end()),
            })
        }
        K::MODULE_PATH => Some(Context::ImportPath {
            prefix: segments_before(parent, colon2.text_range().end()),
        }),
        _ => Some(Context::None),
    }
}

/// The context after `(`, `[` or `,`, by the list they open or separate.
fn list_item<'t>(anchor: SyntaxToken<'t>, list: SyntaxNode<'t>, at: TextSize) -> Context<'t> {
    let expr = |site| Context::Expr {
        statement: false,
        site,
    };
    let pattern = Context::Pattern {
        subject: Subject::Unknown,
    };
    match list.kind() {
        K::ARG_LIST => expr(ExprSite::Arg {
            args: list,
            index: commas_before(list, at),
        }),
        K::LIST_EXPR | K::TUPLE_EXPR | K::PAREN_EXPR | K::CASE_EXPR => expr(ExprSite::Unknown),
        K::IMPORT_LIST if anchor.kind() == K::COMMA => import_items(list, None),
        K::RECORD_EXPR if anchor.kind() == K::COMMA => record_field(list, None),
        K::PARAM_LIST | K::POS_PAT_LIST | K::LIST_PAT | K::TUPLE_PAT | K::PAREN_PAT => pattern,
        K::TYPE_ARG_LIST
        | K::TYPE_LIST
        | K::POS_FIELD_LIST
        | K::TUPLE_TYPE
        | K::FN_TYPE
        | K::BOUND_LIST => Context::Type,
        _ => Context::None,
    }
}

/// The import-list context of `list`, an `IMPORT_LIST`. `partial` is left
/// out of what is written.
fn import_items<'t>(list: SyntaxNode<'t>, partial: Option<SyntaxToken<'t>>) -> Context<'t> {
    let module = list
        .parent()
        .and_then(|decl| decl.first_child_of_kind(K::MODULE_PATH))
        .map(|path| segments_before(path, path.text_range().end()))
        .unwrap_or_default();
    let written = list
        .children()
        .filter(|item| item.kind() == K::IMPORT_ITEM)
        .filter_map(SyntaxNode::first_token)
        .filter(|name| Some(*name) != partial && is_name(name.kind()))
        .map(|name| name.text().to_owned())
        .collect();
    Context::ImportItems { module, written }
}

/// The record-field context of `record`, a `RECORD_EXPR`. `partial` is
/// left out of what is written.
fn record_field<'t>(record: SyntaxNode<'t>, partial: Option<SyntaxToken<'t>>) -> Context<'t> {
    let inits: Vec<SyntaxNode<'t>> = record
        .children()
        .filter(|c| c.kind() == K::FIELD_INIT)
        .collect();
    let written: Vec<String> = inits
        .iter()
        .filter_map(|init| init.first_token())
        .filter(|name| Some(*name) != partial && is_name(name.kind()))
        .map(|name| name.text().to_owned())
        .collect();
    let spread = record.children().any(|c| c.kind() == K::SPREAD)
        || record.children_with_tokens().any(|e| e.kind() == K::DOT2);
    Context::RecordField {
        record,
        empty: written.is_empty() && !spread,
        written,
    }
}

/// The context of a name in `node`, an `INTERPOLATION`, that starts at
/// `at`: its root, or a field after a `.`.
fn interpolation(node: SyntaxNode<'_>, at: TextSize) -> Context<'_> {
    let before: Vec<SyntaxToken<'_>> = node
        .tokens()
        .take_while(|t| t.text_range().end() <= at)
        .collect();
    let dot = before.iter().rev().find(|t| is_significant(t.kind()));
    match dot {
        Some(&dot) if dot.kind() == K::DOT => Context::Dot {
            receiver: Receiver::Interpolation {
                node,
                names: before
                    .iter()
                    .copied()
                    .filter(|t| is_name(t.kind()))
                    .collect(),
            },
            dot,
        },
        Some(dot) if dot.kind() == K::INTERP_START => Context::Interpolation,
        _ => Context::None,
    }
}

/// The name segments of `path`, a `PATH` or `MODULE_PATH`, that end at or
/// before `before`.
fn segments_before(path: SyntaxNode<'_>, before: TextSize) -> Vec<String> {
    path.children_with_tokens()
        .filter_map(SyntaxElement::as_token)
        .filter(|t| is_name(t.kind()) && t.text_range().end() <= before)
        .map(|t| t.text().to_owned())
        .collect()
}

/// The namespace `path`, a `PATH`, is read in, by its parent.
fn path_ns(path: SyntaxNode<'_>) -> PathNs {
    match path.parent().map(SyntaxNode::kind) {
        Some(K::PATH_TYPE | K::INTERFACE_REF) => PathNs::Type,
        Some(K::CTOR_PAT) => PathNs::Pattern,
        _ => PathNs::Value,
    }
}

/// The left side of the pipe whose right side is the expression of
/// `path`, if it is one: `xs |> list::`.
fn piped_lhs(path: SyntaxNode<'_>) -> Option<SyntaxNode<'_>> {
    let expr = path.parent().filter(|p| p.kind() == K::PATH_EXPR)?;
    let pipe = expr.parent().filter(|p| p.kind() == K::BIN_EXPR)?;
    pipe.first_token_of_kind(K::PIPE_GT)?;
    let lhs = pipe.first_child()?;
    (lhs != expr).then_some(lhs)
}

/// The number of commas that are children of `list` and end at or before
/// `at`: the position of the argument at `at` (X §4.7).
fn commas_before(list: SyntaxNode<'_>, at: TextSize) -> usize {
    list.children_with_tokens()
        .filter(|e| e.kind() == K::COMMA && e.text_range().end() <= at)
        .count()
}

/// Whether a token of `kind` is a name.
fn is_name(kind: K) -> bool {
    matches!(kind, K::LOWER_IDENT | K::UPPER_IDENT)
}

/// Whether `kind` is an operator of a binary or prefix expression, other
/// than `|>`.
fn is_operator(kind: K) -> bool {
    matches!(
        kind,
        K::PLUS
            | K::MINUS
            | K::STAR
            | K::SLASH
            | K::PERCENT
            | K::BANG
            | K::AMP
            | K::PIPE
            | K::CARET
            | K::SHL
            | K::SHR
            | K::EQ2
            | K::NEQ
            | K::LT
            | K::LE
            | K::GT
            | K::GE
            | K::AMP2
            | K::PIPE2
    )
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
