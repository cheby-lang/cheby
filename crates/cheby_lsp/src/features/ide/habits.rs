//! Rewrites of habits from other languages (X §6.5, plan §1.11), the
//! E0042 rows for `null`, `nil` and `none`, E0044's rename and E0058's
//! method call (D-1668, D-1736). `true` and `false` as values and `int`,
//! `string`, `bool` and `float` as types have no row here: their helps
//! carry the edit, which `generic` offers as the fix (D-1931).
//!
//! Each rewrite is found from its diagnostic's code and primary span, and
//! the tokens there, never from the message (D-1388). Tokens are read in
//! source order with `SyntaxToken::next_token`, not through the tree's
//! nodes: a habit is a grammar error, the parser's recovery has cut its
//! text into error nodes, and `if` lexes as a name.
//!
//! - E0013 after `if`: `if c { a } else { b }` becomes
//!   `case { c => a _ => b }`, one arm per line, and `else if` adds an arm.
//!   An `if` with no `else` gets `_ => Nil`, and an empty block `Nil`, so
//!   neither is rewritten in a module that hides the prelude's `Nil`
//!   (D-1678). One `if` whose block is one
//!   `return x` becomes `guard !(c) else { x }`; the condition is negated
//!   as `!( )`, since NaN makes inverting a comparison unsound.
//! - E0013 after `return`: `return x` becomes `x`.
//! - E0013 after `for`: `for p in xs { … }` becomes
//!   `list::each(xs, fn(p) { … })`, with `std::list` named through
//!   `imports::import_plan`, which adds the import when the file has none,
//!   checked on resolve (D-1753).
//! - E0013 after `let mut`: `mut` is removed.
//! - E0013 at `else` before `=>` in a `case`: `_`.
//! - E0042 at `null`, `nil` or `none` as a value: `None`, `Nil`, `None`. The
//!   row writes no prelude name the module hides (D-1678).
//! - E0044: the binding and its later uses are renamed to a name from
//!   `scope::fresh`, with rename's writer and no `verify`, as a fix within
//!   one file (D-1668, D-1684). The frame does not format its edits
//!   (D-1735).
//! - E0058 on a field of a known type that is called: `xs.map(f)` becomes
//!   `xs |> list::map(_, f)`, and `xs.length()` becomes `xs |> list::length`,
//!   when the type's home module has a function of that name that takes
//!   the receiver first and the call's arguments after it (X §4.2). A call
//!   that is an operand of an operator binding as tightly as `|>` or more
//!   is written in the call form, `list::length(xs) + 1`, as completion
//!   writes it (D-1749). In a std checkout no `StdIndex` item is read
//!   (D-1706).
//!
//! An arm's body drops its braces when it is one expression on one line
//! with no comment, and keeps its block otherwise. `if … return` and a
//! last `return` are rewritten only in the body block of a function, a
//! local function included, or of a closure, as D-1401 says (D-1746), and a
//! `guard` only with code after it (plan §1.11). A `return` anywhere
//! else is left to its help. The rows of E0013 are not preferred, so a
//! client applies none of them on its own; E0042's and E0058's are
//! (D-1737).
//!
//! Every rewrite is checked by parsing the file with it applied, in memory
//! (X §6.5): the rewrite of a grammar error must lower the file's count of
//! grammar errors, any other must not raise it, and no grammar error may
//! touch the text the rewrite wrote, or nothing is offered. A condition, a
//! pattern or a list moves without the comments at its ends, so such a
//! comment counts as dropped.
//! A rewrite whose file would have fewer comments than before is offered
//! `disabled`, "this would drop a comment" (plan §1.11, D-1401), which the
//! frame omits for a client that cannot show it (D-1690).
//!
//! Every edit comes with the listing but the `for` rewrite's, which comes
//! on resolve: its closure's body must give `Nil`, as `list::each` asks,
//! and the facts hold no type for a body that the parser's recovery put in
//! an error node, so the frame checks the edits through `verify` there
//! ([`check_rewrite`], D-1753), and a client that cannot resolve `edit` is
//! not offered it. A resolve computes the same rewrites again.

use std::sync::Arc;

use cheby_driver::analysis::codes;
use cheby_driver::analysis::{
    DefKind, FileId, Introduced, ItemSig, ModuleId, ModulePath, ModuleView, PlainDiagnostic,
    PlainEdit, Snapshot, StdIndex, TextRange, TextSize, Ty, TyKind, VerifyRequest, accepts,
    builtin,
};
use cheby_syntax::names::to_snake_case;
use cheby_syntax::{SyntaxKind as K, SyntaxNode, SyntaxToken, SyntaxTree};

use super::Refusal;
use super::actions::{Action, ActionId, ActionKind, DiagRef, FixCx, hides_nil};
use super::imports::{import_plan, parse_path, path_text};
use super::print::hides_prelude;
use super::rename::RenameError;
use super::{nav, rename, scope};
use crate::features::Analyzed;
use crate::features::index::Meaning;

/// The reason a rewrite that would lose a comment is disabled (plan
/// §1.11).
pub(crate) const DROPS_A_COMMENT: &str = "this would drop a comment";

/// The habit rewrites for `range`, from the report's diagnostics that
/// overlap it, each with its edits.
pub(crate) fn provide(
    cx: &FixCx<'_>,
    _range: TextRange,
    diagnostics: &[(usize, &PlainDiagnostic)],
) -> Vec<Action> {
    rewrites(cx, diagnostics)
        .into_iter()
        .enumerate()
        // A rewrite that a resolve checks is offered only to a client that
        // resolves edits (D-1753).
        .filter(|(_, rewrite)| !rewrite.verified || cx.resolve_edits)
        .filter_map(|(index, rewrite)| {
            let verified = rewrite.verified && rewrite.disabled.is_none();
            Some(Action {
                title: rewrite.title,
                kind: ActionKind::QuickFix,
                diagnostic: Some(DiagRef::Report(rewrite.diagnostic)),
                // A disabled rewrite is no one's preferred fix.
                preferred: rewrite.preferred && rewrite.disabled.is_none(),
                removes: false,
                edit: (!verified).then_some(rewrite.edits),
                data: if verified {
                    Some(ActionId::Habit {
                        index: u32::try_from(index).ok()?,
                    })
                } else {
                    None
                },
                disabled: rewrite.disabled,
                command: None,
            })
        })
        .collect()
}

/// The edits of the habit rewrite `id` at `range`: the `index`-th that
/// [`provide`] lists there, computed again from the report.
///
/// # Errors
///
/// A [`Refusal`] when no such rewrite is offered there, or it is disabled.
pub(crate) fn resolve(
    cx: &FixCx<'_>,
    range: TextRange,
    id: &ActionId,
) -> Result<Vec<PlainEdit>, Refusal> {
    let gone = || Refusal("the rewrite is no longer offered here".to_owned());
    let index = match id {
        ActionId::Habit { index } => usize::try_from(*index).map_err(|_| gone())?,
        ActionId::Fix { .. }
        | ActionId::AddImport { .. }
        | ActionId::RemoveImport
        | ActionId::OrganizeImports
        | ActionId::FillArms
        | ActionId::Stub { .. }
        | ActionId::MakeVisible { .. }
        | ActionId::Rename { .. } => return Err(gone()),
    };
    let report: &[PlainDiagnostic] = cx
        .report
        .map_or(&[], |report| report.diagnostics.as_slice());
    let overlapping: Vec<(usize, &PlainDiagnostic)> = report
        .iter()
        .enumerate()
        .filter(|(_, d)| d.range.is_some_and(|at| touches(at, range)))
        .collect();
    rewrites(cx, &overlapping)
        .into_iter()
        .nth(index)
        .filter(|rewrite| rewrite.disabled.is_none())
        .map(|rewrite| rewrite.edits)
        .ok_or_else(gone)
}

/// The check of a habit rewrite whose edits come on resolve, the `for`
/// rewrite (D-1753): `verify` on `edits`, all in one file, where only a new
/// error that touches the text the rewrite wrote refuses it, as a refused
/// fix answers (D-1731). So a closure whose body's value is not `Nil`,
/// which `list::each` refuses, is refused with "Applying the fix would
/// cause: …", while an error elsewhere in the file, which the checker did
/// not see while the `for` was a grammar error (D-494), is not. The
/// written ranges go to `verify` too, so a new error there never takes an
/// old error of the same message elsewhere, whose line the added import
/// moved.
///
/// # Errors
///
/// As [`rename::check`].
pub(crate) fn check_rewrite(analyzed: &Analyzed, edits: &[PlainEdit]) -> Result<(), RenameError> {
    let Some(path) = edits.first().map(|edit| edit.path.clone()) else {
        return Ok(());
    };
    let raw: Vec<(TextRange, String)> = edits
        .iter()
        .map(|edit| (edit.range, edit.replacement.clone()))
        .collect();
    let wrote = written(&raw).unwrap_or_default();
    let fresh: Vec<(std::path::PathBuf, TextRange)> =
        wrote.iter().map(|range| (path.clone(), *range)).collect();
    let counts = |introduced: &Introduced| {
        introduced.path == path
            && introduced
                .diagnostic
                .range
                .is_some_and(|at| wrote.iter().any(|w| touches(at, *w)))
    };
    let request = VerifyRequest {
        edits,
        moves: &[],
        renamed: &[],
        hides: false,
        written: &fresh,
    };
    rename::check_counting(analyzed, &request, rename::Action::Fix, &counts)
}

/// Whether `a` and `b` share a point, ends included.
fn touches(a: TextRange, b: TextRange) -> bool {
    a.start() <= b.end() && b.start() <= a.end()
}

/// One rewrite, checked.
struct Rewrite {
    /// The index of its diagnostic in the report.
    diagnostic: usize,
    /// Its title (D-1736).
    title: String,
    /// The row marks it preferred.
    preferred: bool,
    /// Its edits, in the request's file.
    edits: Vec<PlainEdit>,
    /// Why it cannot be applied, if it would drop a comment.
    disabled: Option<String>,
    /// Its edits come on resolve and are checked there (D-1753).
    verified: bool,
}

/// A rewrite before its checks: replacements of ranges of the file's text.
struct Draft {
    /// Its title (D-1736).
    title: String,
    /// It rewrites a grammar error, so the file's count of them must drop.
    grammar: bool,
    /// The row marks it preferred.
    preferred: bool,
    /// The ranges and their replacements.
    edits: Vec<(TextRange, String)>,
    /// What the rewritten file must hold where the first edit starts.
    shape: Shape,
    /// Its edits come on resolve, where the frame checks them through
    /// `verify`: the `for` rewrite, whose closure must check (D-1753).
    verified: bool,
}

/// A check of the rewritten file's tree at the first edit's start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// Nothing more than the counts.
    Any,
    /// A statement that is the last of the body block of a function, a
    /// local function or a closure: a `return` removed (D-1401).
    LastInBody,
    /// A `guard` statement of such a block, with a statement after it.
    GuardInBody,
}

/// The file's counts before any rewrite.
#[derive(Clone, Copy)]
struct Counts {
    /// Grammar errors (class G).
    errors: usize,
    /// `COMMENT` and `DOC_COMMENT` tokens.
    comments: usize,
}

/// Every rewrite among `diagnostics`, in their order.
fn rewrites(cx: &FixCx<'_>, diagnostics: &[(usize, &PlainDiagnostic)]) -> Vec<Rewrite> {
    let (Some(path), Some(tree)) = (cx.path(), cx.view.tree()) else {
        return Vec::new();
    };
    if tree.text() != cx.text {
        return Vec::new();
    }
    let mut before: Option<Counts> = None;
    let mut out = Vec::new();
    for &(index, diagnostic) in diagnostics {
        let Some(at) = diagnostic.range else {
            continue;
        };
        let draft = match diagnostic.code {
            codes::SYNTAX_ERROR => grammar_habit(cx, tree, at),
            codes::UNRESOLVED_NAME => literal_word(cx, tree, at),
            codes::SHADOWED_NAME => shadowed(cx, at),
            codes::UNKNOWN_FIELD => method_call(cx, tree, at),
            _ => None,
        };
        let Some(draft) = draft else {
            continue;
        };
        let counts = *before.get_or_insert_with(|| counts(tree.file(), cx.text));
        let Some(checked) = check(tree, cx.text, &draft, counts) else {
            continue;
        };
        let disabled = match checked {
            Checked::Offered => None,
            Checked::DropsAComment => Some(DROPS_A_COMMENT.to_owned()),
        };
        out.push(Rewrite {
            diagnostic: index,
            title: draft.title,
            preferred: draft.preferred,
            edits: draft
                .edits
                .into_iter()
                .map(|(range, replacement)| PlainEdit {
                    path: path.to_path_buf(),
                    range,
                    replacement,
                })
                .collect(),
            disabled,
            verified: draft.verified,
        });
    }
    out
}

/// The grammar errors and comments of `text`.
fn counts(file: FileId, text: &str) -> Counts {
    let parsed = cheby_syntax::parse(file, Arc::from(text));
    Counts {
        errors: parsed.errors().len(),
        comments: comments(parsed.tree().root()),
    }
}

/// The comment tokens under `root`.
fn comments(root: SyntaxNode<'_>) -> usize {
    root.tokens()
        .filter(|t| matches!(t.kind(), K::COMMENT | K::DOC_COMMENT))
        .count()
}

/// How a draft came through its checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Checked {
    /// It is offered.
    Offered,
    /// It is offered `disabled`: it would drop a comment.
    DropsAComment,
}

/// The checks of `draft` (module doc): `None` when it is not offered.
fn check(tree: &SyntaxTree, text: &str, draft: &Draft, before: Counts) -> Option<Checked> {
    let new = applied(text, &draft.edits)?;
    let parsed = cheby_syntax::parse(tree.file(), Arc::from(new.as_str()));
    let errors = parsed.errors().len();
    if (draft.grammar && errors >= before.errors) || errors > before.errors {
        return None;
    }
    // What the rewrite wrote must parse: a lower count alone would pass a
    // rewrite that trades two errors for a new one of its own (T-1).
    let written = written(&draft.edits)?;
    let in_written = parsed.errors().iter().any(|error| {
        error.primary_span().is_some_and(|span| {
            written
                .iter()
                .any(|w| span.range.start() <= w.end() && w.start() <= span.range.end())
        })
    });
    if in_written {
        return None;
    }
    let at = draft.edits.iter().map(|(range, _)| range.start()).min()?;
    if !shaped(parsed.tree().root(), at, draft.shape) {
        return None;
    }
    Some(if comments(parsed.tree().root()) == before.comments {
        Checked::Offered
    } else {
        Checked::DropsAComment
    })
}

/// The ranges of the rewritten text that `edits` wrote, ascending.
fn written(edits: &[(TextRange, String)]) -> Option<Vec<TextRange>> {
    let mut sorted: Vec<&(TextRange, String)> = edits.iter().collect();
    sorted.sort_by_key(|(range, _)| (range.start(), range.end()));
    let mut out = Vec::with_capacity(sorted.len());
    // How far the text after the edits so far has moved, as a signed sum.
    let mut grown = 0_i64;
    for (range, replacement) in sorted {
        let start = i64::from(range.start()).checked_add(grown)?;
        let len = i64::try_from(replacement.len()).ok()?;
        let end = start.checked_add(len)?;
        out.push(TextRange::new(
            u32::try_from(start).ok()?,
            u32::try_from(end).ok()?,
        ));
        grown = grown
            .checked_add(len)?
            .checked_sub(i64::from(range.end()).checked_sub(i64::from(range.start()))?)?;
    }
    Some(out)
}

/// `text` with `edits` applied, or `None` when they overlap or leave the
/// text.
fn applied(text: &str, edits: &[(TextRange, String)]) -> Option<String> {
    let mut sorted: Vec<&(TextRange, String)> = edits.iter().collect();
    sorted.sort_by_key(|(range, _)| (range.start(), range.end()));
    let mut out = String::with_capacity(text.len());
    let mut at = 0_usize;
    for (range, replacement) in sorted {
        let start = usize::try_from(range.start()).ok()?;
        let end = usize::try_from(range.end()).ok()?;
        out.push_str(text.get(at..start)?);
        out.push_str(replacement);
        at = end;
    }
    out.push_str(text.get(at..)?);
    Some(out)
}

/// Whether the tree `root` holds what `shape` asks for at `at`.
fn shaped(root: SyntaxNode<'_>, at: TextSize, shape: Shape) -> bool {
    let statement = || {
        let token = root.token_at_offset(at)?;
        let node = token
            .parent()?
            .ancestors()
            .find(|n| n.parent().is_some_and(|p| p.kind() == K::BLOCK))?;
        (node.text_range().start() == at && is_body_block(node.parent()?)).then_some(node)
    };
    match shape {
        Shape::Any => true,
        Shape::LastInBody => statement().is_some_and(|node| node.next_sibling().is_none()),
        Shape::GuardInBody => statement()
            .is_some_and(|node| node.kind() == K::GUARD_STMT && node.next_sibling().is_some()),
    }
}

/// Whether `block` is the body of a top-level function, a local function
/// or a closure (D-1401, D-1746).
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "a match on syntax kinds names only the kinds it handles"
)]
fn is_body_block(block: SyntaxNode<'_>) -> bool {
    block.parent().is_some_and(|owner| match owner.kind() {
        K::FN_DECL => owner.parent().is_some_and(|p| p.kind() == K::SOURCE_FILE),
        K::CLOSURE | K::LOCAL_FN => true,
        _ => false,
    })
}

/// The token that starts at `at`'s start.
fn token_at(tree: &SyntaxTree, at: TextRange) -> Option<SyntaxToken<'_>> {
    tree.root()
        .token_at_offset(at.start())
        .filter(|t| t.text_range().start() == at.start())
}

/// The token before `token` that is no trivia, on the same line.
fn before(token: SyntaxToken<'_>) -> Option<SyntaxToken<'_>> {
    let mut at = token.prev_token();
    while let Some(t) = at {
        if !t.kind().is_trivia() {
            return Some(t);
        }
        at = t.prev_token();
    }
    None
}

/// The token after `token` that is no trivia, and with `lines` no line
/// break either.
fn after(token: SyntaxToken<'_>, lines: bool) -> Option<SyntaxToken<'_>> {
    let mut at = token.next_token();
    while let Some(t) = at {
        if !(t.kind().is_trivia() || (lines && t.kind() == K::NEWLINE)) {
            return Some(t);
        }
        at = t.next_token();
    }
    None
}

/// Whether `token` is the name `word`, as `if` and `return` lex.
fn is_word(token: SyntaxToken<'_>, word: &str) -> bool {
    token.kind() == K::LOWER_IDENT && token.text() == word
}

/// The first token after `start` that `stop` accepts outside every bracket
/// opened after `start`, or `None` at a `}`, `)` or `]` that closes
/// nothing opened there.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "a match on syntax kinds names only the kinds it handles"
)]
fn find_outside<'t>(
    start: SyntaxToken<'t>,
    stop: impl Fn(SyntaxToken<'t>) -> bool,
) -> Option<SyntaxToken<'t>> {
    let mut depth = 0_u32;
    let mut at = start.next_token();
    while let Some(t) = at {
        if depth == 0 && stop(t) {
            return Some(t);
        }
        match t.kind() {
            K::L_PAREN | K::L_BRACK | K::L_BRACE => depth = depth.checked_add(1)?,
            K::R_PAREN | K::R_BRACK | K::R_BRACE => depth = depth.checked_sub(1)?,
            _ => {}
        }
        at = t.next_token();
    }
    None
}

/// The `}` that closes `open`, a `{`.
fn closing(open: SyntaxToken<'_>) -> Option<SyntaxToken<'_>> {
    find_outside(open, |t| t.kind() == K::R_BRACE)
}

/// The code between `a` and `b`: from the first token after `a` to the
/// last before `b` that is neither white space, a line break nor a
/// comment, so a comment at either end stays out of what moves. Such a
/// comment is then dropped, which disables the rewrite (plan §1.11), and
/// never lands inside a condition or a pattern, where the code written
/// after it would join the comment (T-1). Empty when only such tokens lie
/// between.
fn code_between<'a>(text: &'a str, a: SyntaxToken<'_>, b: SyntaxToken<'_>) -> Option<&'a str> {
    let code = |t: SyntaxToken<'_>| {
        !(t.kind().is_trivia() || matches!(t.kind(), K::NEWLINE | K::COMMENT | K::DOC_COMMENT))
    };
    let mut first = a.next_token();
    while let Some(t) = first {
        if t == b {
            return Some("");
        }
        if code(t) {
            break;
        }
        first = t.next_token();
    }
    let mut last = b.prev_token();
    while let Some(t) = last {
        if t == a {
            return Some("");
        }
        if code(t) {
            break;
        }
        last = t.prev_token();
    }
    let (first, last) = (first?, last?);
    if first.text_range().start() > last.text_range().end() {
        return Some("");
    }
    TextRange::new(first.text_range().start(), last.text_range().end()).slice(text)
}

/// The text between the end of `a` and the start of `b`, trimmed.
fn between<'a>(text: &'a str, a: SyntaxToken<'_>, b: SyntaxToken<'_>) -> Option<&'a str> {
    (a.text_range().end() <= b.text_range().start())
        .then(|| TextRange::new(a.text_range().end(), b.text_range().start()).slice(text))
        .flatten()
        .map(str::trim)
}

/// The whitespace that starts the line holding `offset`.
fn indentation(text: &str, offset: TextSize) -> &str {
    let at = usize::try_from(offset).unwrap_or(0);
    let start = text
        .get(..at)
        .and_then(|before| before.rfind('\n'))
        .map_or(0, |n| n.saturating_add(1));
    let line = text.get(start..).unwrap_or("");
    let end = line
        .find(|c: char| c != ' ' && c != '\t')
        .unwrap_or(line.len());
    line.get(..end).unwrap_or("")
}

/// `code`, the text from `first` to `last`, with each line after its
/// first two spaces deeper, since it moves into an arm or a block. Kept
/// as it is when a token in it other than white space holds a line break,
/// such as a string over lines, whose text would change.
fn deeper(code: &str, first: SyntaxToken<'_>, last: SyntaxToken<'_>) -> String {
    let mut at = Some(first);
    while let Some(t) = at {
        if !matches!(t.kind(), K::NEWLINE | K::WHITESPACE) && t.text().contains('\n') {
            return code.to_owned();
        }
        if t == last {
            break;
        }
        at = t.next_token();
    }
    code.split('\n')
        .enumerate()
        .map(|(n, line)| {
            if n == 0 || line.trim().is_empty() {
                line.to_owned()
            } else {
                format!("  {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a token between `open` and `close` is a comment.
fn has_comment(open: SyntaxToken<'_>, close: SyntaxToken<'_>) -> bool {
    let mut at = open.next_token();
    while let Some(t) = at {
        if t == close {
            return false;
        }
        if matches!(t.kind(), K::COMMENT | K::DOC_COMMENT) {
            return true;
        }
        at = t.next_token();
    }
    false
}

/// What an arm writes for the block from `open` to `close`: its one
/// expression, when it has one on one line and no comment, `nil` when it is
/// empty, else the block (X §6.5, step 4). `nil` is `None` in a module that
/// hides the prelude's `Nil` (D-1678), and an empty block then has no arm.
fn arm_body(
    tree: &SyntaxTree,
    text: &str,
    open: SyntaxToken<'_>,
    close: SyntaxToken<'_>,
    nil: Option<&str>,
) -> Option<String> {
    let inner = between(text, open, close)?;
    if inner.is_empty() {
        return nil.map(str::to_owned);
    }
    if !inner.contains('\n') && !has_comment(open, close) && one_expression(tree, inner) {
        return Some(inner.to_owned());
    }
    let block = TextRange::new(open.text_range().start(), close.text_range().end()).slice(text)?;
    Some(deeper(block, open, close))
}

/// Whether `code` is one expression, by parsing it as the body of a
/// function.
fn one_expression(tree: &SyntaxTree, code: &str) -> bool {
    let parsed = cheby_syntax::parse(tree.file(), Arc::from(format!("fn t() {{\n{code}\n}}\n")));
    if !parsed.errors().is_empty() {
        return false;
    }
    let block = parsed
        .tree()
        .root()
        .first_child_of_kind(K::FN_DECL)
        .and_then(|f| f.first_child_of_kind(K::BLOCK));
    let mut statements = block.into_iter().flat_map(SyntaxNode::children);
    match (statements.next(), statements.next()) {
        (Some(only), None) => !matches!(
            only.kind(),
            K::LET_STMT | K::USE_STMT | K::ASSERT_STMT | K::GUARD_STMT | K::LOCAL_FN
        ),
        _ => false,
    }
}

/// The rewrite of the E0013 at `at`, if a habit caused it.
fn grammar_habit(cx: &FixCx<'_>, tree: &SyntaxTree, at: TextRange) -> Option<Draft> {
    let token = token_at(tree, at)?;
    if token.kind() == K::ELSE_KW {
        return else_arm(token);
    }
    let word = before(token)?;
    let draft = |title: &str, edits: Vec<(TextRange, String)>, shape: Shape| Draft {
        title: title.to_owned(),
        grammar: true,
        preferred: false,
        edits,
        shape,
        verified: false,
    };
    match word.text() {
        _ if word.kind() != K::LOWER_IDENT => None,
        "if" => if_rewrite(cx, tree, cx.text, word),
        "return" => Some(draft(
            "Remove the `return`",
            vec![(
                TextRange::new(word.text_range().start(), at.start()),
                String::new(),
            )],
            Shape::LastInBody,
        )),
        "for" => for_rewrite(cx, cx.text, word),
        "mut" if before(word).is_some_and(|t| t.kind() == K::LET_KW) => Some(draft(
            "Remove `mut`",
            vec![(
                TextRange::new(word.text_range().start(), at.start()),
                String::new(),
            )],
            Shape::Any,
        )),
        _ => None,
    }
}

/// `else =>` in a `case` to `_ =>`.
fn else_arm(token: SyntaxToken<'_>) -> Option<Draft> {
    let arrow = after(token, false)?;
    let in_case = token
        .parent()?
        .ancestors()
        .any(|n| n.kind() == K::CASE_BODY);
    (arrow.kind() == K::FAT_ARROW && in_case).then(|| Draft {
        title: "Write `_`".to_owned(),
        grammar: true,
        preferred: false,
        edits: vec![(token.text_range(), "_".to_owned())],
        shape: Shape::Any,
        verified: false,
    })
}

/// `null`, `nil` and `none` written as a value, at the E0042 at `at`, to
/// the prelude's word (P-4), unless the module hides that constructor
/// (D-1678). `true` and `false` are their help's fix (D-1931).
fn literal_word(cx: &FixCx<'_>, tree: &SyntaxTree, at: TextRange) -> Option<Draft> {
    let token =
        token_at(tree, at).filter(|t| t.text_range() == at && t.kind() == K::LOWER_IDENT)?;
    let (word, def) = match token.text() {
        "null" | "none" => ("None", builtin::NONE),
        "nil" => ("Nil", builtin::NIL),
        _ => return None,
    };
    if hides_prelude(&cx.analyzed.snapshot, cx.view, def, None) {
        return None;
    }
    let path = token.parent().filter(|p| p.kind() == K::PATH)?;
    let alone = path.tokens().filter(|t| !t.kind().is_trivia()).count() == 1;
    (alone && path.parent().is_some_and(|p| p.kind() == K::PATH_EXPR)).then(|| Draft {
        title: format!("Write `{word}`"),
        grammar: false,
        preferred: true,
        edits: vec![(at, word.to_owned())],
        shape: Shape::Any,
        verified: false,
    })
}

/// One arm of an `if` chain: its condition and its block.
struct Arm<'t> {
    /// The condition, as written.
    condition: String,
    /// The block's `{`.
    open: SyntaxToken<'t>,
    /// The block's `}`.
    close: SyntaxToken<'t>,
}

/// The rewrite of the `if` chain that starts at `word` (X §6.5's
/// algorithm): `case`, or `guard` for one `if` whose block is one
/// `return`.
fn if_rewrite(
    cx: &FixCx<'_>,
    tree: &SyntaxTree,
    text: &str,
    word: SyntaxToken<'_>,
) -> Option<Draft> {
    // The prelude's `Nil`, unless the module hides it (D-1678).
    let nil = (!hides_nil(&cx.analyzed.snapshot, cx.view)).then_some("Nil");
    let mut arms: Vec<Arm<'_>> = Vec::new();
    let mut last: Option<(SyntaxToken<'_>, SyntaxToken<'_>)> = None;
    let mut keyword = word;
    loop {
        let open = find_outside(keyword, |t| t.kind() == K::L_BRACE)?;
        let condition = code_between(text, keyword, open).filter(|c| !c.is_empty())?;
        let close = closing(open)?;
        arms.push(Arm {
            condition: condition.to_owned(),
            open,
            close,
        });
        let Some(next) = after(close, true).filter(|t| t.kind() == K::ELSE_KW) else {
            break;
        };
        let then = after(next, true)?;
        if is_word(then, "if") {
            keyword = then;
            continue;
        }
        if then.kind() != K::L_BRACE {
            return None;
        }
        last = Some((then, closing(then)?));
        break;
    }
    let end = last.map_or(arms.last()?.close, |(_, close)| close);
    let range = TextRange::new(word.text_range().start(), end.text_range().end());
    let indent = indentation(text, word.text_range().start());
    if let ([arm], None) = (arms.as_slice(), last)
        && let Some(guard) = guard_text(text, arm, indent)
    {
        return Some(Draft {
            title: "Rewrite as `guard`".to_owned(),
            grammar: true,
            preferred: false,
            edits: vec![(range, guard)],
            shape: Shape::GuardInBody,
            verified: false,
        });
    }
    let mut lines = vec!["case {".to_owned()];
    for arm in &arms {
        let body = arm_body(tree, text, arm.open, arm.close, nil)?;
        lines.push(format!("{indent}  {} => {body}", arm.condition));
    }
    let otherwise = match last {
        Some((open, close)) => arm_body(tree, text, open, close, nil)?,
        None => nil?.to_owned(),
    };
    lines.push(format!("{indent}  _ => {otherwise}"));
    lines.push(format!("{indent}}}"));
    Some(Draft {
        title: "Rewrite as `case`".to_owned(),
        grammar: true,
        preferred: false,
        edits: vec![(range, lines.join("\n"))],
        shape: Shape::Any,
        verified: false,
    })
}

/// `guard !(c) else { x }` for `arm` when its block is one `return x`.
fn guard_text(text: &str, arm: &Arm<'_>, indent: &str) -> Option<String> {
    let first = after(arm.open, true)?;
    if !is_word(first, "return") {
        return None;
    }
    let mut at = first.next_token();
    while let Some(t) = at {
        if t == arm.close {
            break;
        }
        if is_word(t, "return") {
            return None;
        }
        at = t.next_token();
    }
    let value = between(text, first, arm.close).filter(|v| !v.is_empty())?;
    let value_start = after(first, true)?;
    let value = deeper(value, value_start, arm.close);
    Some(format!(
        "guard !({}) else {{\n{indent}  {value}\n{indent}}}",
        arm.condition
    ))
}

/// `for p in xs { … }` from `word` to `list::each(xs, fn(p) { … })`, with
/// the import of `std::list` when the file has none.
fn for_rewrite(cx: &FixCx<'_>, text: &str, word: SyntaxToken<'_>) -> Option<Draft> {
    let stop = |t: SyntaxToken<'_>| is_word(t, "in") || t.kind() == K::L_BRACE;
    let keyword = find_outside(word, stop).filter(|t| is_word(*t, "in"))?;
    let pattern = code_between(text, word, keyword).filter(|p| !p.is_empty())?;
    let open = find_outside(keyword, |t| t.kind() == K::L_BRACE)?;
    let list = code_between(text, keyword, open).filter(|l| !l.is_empty())?;
    let close = closing(open)?;
    let block = TextRange::new(open.text_range().start(), close.text_range().end()).slice(text)?;
    let snapshot = &*cx.analyzed.snapshot;
    let plan = import_plan(snapshot, cx.view, std_index(cx), &parse_path("std::list")?)?;
    let callee = format!("{}::each", plan.name);
    let mut edits = vec![(
        TextRange::new(word.text_range().start(), close.text_range().end()),
        format!("{callee}({list}, fn({pattern}) {block})"),
    )];
    edits.extend(plan.edit);
    Some(Draft {
        title: format!("Rewrite as `{callee}`"),
        grammar: true,
        preferred: false,
        edits,
        shape: Shape::Any,
        verified: true,
    })
}

/// The std index, outside a std checkout (D-1706).
fn std_index(cx: &FixCx<'_>) -> Option<&'static StdIndex> {
    if cx.analyzed.snapshot.is_std_checkout() {
        return None;
    }
    cx.std.index()
}

/// E0044's rename of the binding at `at` and its later uses to a free name
/// (D-1668).
fn shadowed(cx: &FixCx<'_>, at: TextRange) -> Option<Draft> {
    let snapshot = &*cx.analyzed.snapshot;
    let index = cx.analyzed.index();
    let picked = nav::pick(index, cx.view, at.start())?;
    if picked.origin != at {
        return None;
    }
    let entry = picked.entries.first()?;
    let Meaning::Local { .. } = entry.meaning else {
        return None;
    };
    let old = at.slice(cx.text)?;
    // A name E0006 refuses, `myVal`, is written in snake case, `my_val_2`,
    // since `fresh` gives only names that E0006 accepts.
    let wanted = format!("{}_2", to_snake_case(old));
    let new = scope::fresh(snapshot, cx.view, at.start(), &wanted, &[])?;
    let edits = rename::write(index, std::slice::from_ref(&entry.symbol), &new, cx.cancel).ok()?;
    let path = cx.path()?;
    if !edits.iter().any(|edit| edit.range == at) || edits.iter().any(|edit| edit.path != path) {
        return None;
    }
    Some(Draft {
        title: format!("Rename `{old}` to `{new}`"),
        grammar: false,
        preferred: false,
        edits: edits
            .into_iter()
            .map(|edit| (edit.range, edit.replacement))
            .collect(),
        shape: Shape::Any,
        verified: false,
    })
}

/// E0058's `xs.f(a)` to `xs |> m::f(_, a)`, or to `m::f(xs, a)` inside an
/// operand (D-1749), where `m` is the home module of the receiver's type
/// and has `f`, which takes the receiver first and the call's arguments
/// after it (X §6.5, §4.2).
fn method_call(cx: &FixCx<'_>, tree: &SyntaxTree, at: TextRange) -> Option<Draft> {
    let snapshot = &*cx.analyzed.snapshot;
    let name = token_at(tree, at).filter(|t| t.text_range() == at && t.kind() == K::LOWER_IDENT)?;
    let field = name.parent().filter(|p| p.kind() == K::FIELD_EXPR)?;
    let call = field
        .parent()
        .filter(|p| p.kind() == K::CALL_EXPR && p.first_child() == Some(field))?;
    let args = call.first_child_of_kind(K::ARG_LIST)?;
    let receiver = field.first_child()?;
    let item = scope::body_at(cx.view, at.start())?;
    let fact = cx.view.facts()?.body(item)?.field_at(at.start())?;
    let base = fact.base?;
    let count = args.children().count();
    let (home, loaded) = home_of(snapshot, base)?;
    let fits = match loaded {
        Some(module) => takes(snapshot, cx.view, module, name.text(), count, base),
        None => std_index(cx).is_some_and(|index| {
            let head = snapshot.head_of(base);
            let home = path_text(&home);
            index.named(name.text()).any(|item| {
                item.module == home
                    && item.kind == DefKind::Fn
                    && count.checked_add(1) == Some(item.param_names.len())
                    && item.params.first() == Some(&head)
            })
        }),
    };
    if !fits {
        return None;
    }
    let (callee, import) = if loaded == Some(cx.view.id()) {
        (name.text().to_owned(), None)
    } else {
        let plan = import_plan(snapshot, cx.view, std_index(cx), &home)?;
        (format!("{}::{}", plan.name, name.text()), plan.edit)
    };
    let receiver_text = receiver.text_range().slice(cx.text)?;
    let inner = if count == 0 {
        None
    } else {
        let open = args.first_token().filter(|t| t.kind() == K::L_PAREN)?;
        let close = args.last_token().filter(|t| t.kind() == K::R_PAREN)?;
        let inner = between(cx.text, open, close)?;
        Some(inner.strip_suffix(',').unwrap_or(inner).trim_end())
    };
    // Inside an operand the call form, as completion writes it, else the
    // pipe (X §4.2, D-1749).
    let written = match (tight(call), inner) {
        (true, None) => format!("{callee}({receiver_text})"),
        (true, Some(inner)) => format!("{callee}({receiver_text}, {inner})"),
        (false, None) => format!("{receiver_text} |> {callee}"),
        (false, Some(inner)) => format!("{receiver_text} |> {callee}(_, {inner})"),
    };
    let mut edits = vec![(call.text_range(), written.clone())];
    edits.extend(import);
    Some(Draft {
        title: format!("Write `{}`", one_line(&written)),
        grammar: false,
        preferred: true,
        edits,
        shape: Shape::Any,
        verified: false,
    })
}

/// `text` on one line, each run of white space with a line break in it
/// one space, for a title.
fn one_line(text: &str) -> String {
    if !text.contains('\n') {
        return text.to_owned();
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The home module of `ty`'s head, which holds its functions (X §4.2): its
/// declaring module, or a builtin type's home, with its id when the run
/// loaded it. `None` for a type with no such module.
fn home_of(snapshot: &Snapshot, ty: Ty) -> Option<(ModulePath, Option<ModuleId>)> {
    let def = match snapshot.globals().types.kind(ty) {
        TyKind::Named { def, .. } => *def,
        TyKind::Error
        | TyKind::Never
        | TyKind::Tuple(_)
        | TyKind::Fn { .. }
        | TyKind::Param(_)
        | TyKind::Dyn { .. } => return None,
    };
    let table = snapshot.table();
    if def.module != ModuleId::BUILTIN {
        return Some((table.get(def.module)?.path.clone(), Some(def.module)));
    }
    let &(_, segment) = builtin::HOMES.iter().find(|(home, _)| *home == def)?;
    let path = builtin::home_path(segment);
    let loaded = table
        .find(&path)
        .filter(|&id| snapshot.interfaces().get(id).is_some());
    Some((path, loaded))
}

/// Whether `module` has a function `name` that code in `view` may call, with
/// `count` parameters after a first one that accepts `base`.
fn takes(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    module: ModuleId,
    name: &str,
    count: usize,
    base: Ty,
) -> bool {
    let (Some(iface), Some(package)) = (
        snapshot.interfaces().get(module),
        snapshot.table().get(view.id()).map(|i| i.package),
    ) else {
        return false;
    };
    let own = module == view.id();
    let types = &snapshot.globals().types;
    iface.items.iter_enumerated().any(|(index, item)| {
        let ItemSig::Fn(sig) = &item.sig else {
            return false;
        };
        item.kind == DefKind::Fn
            && iface.is_current(index)
            && !iface.is_hidden(index)
            && (own || iface.visible_from(index, package))
            && snapshot.globals().names.get(item.name) == name
            && count.checked_add(1) == Some(sig.params.len())
            && sig
                .params
                .first()
                .is_some_and(|param| accepts(types, param.ty, base).unwrap_or(false))
    })
}

/// Whether a pipe in place of `call` would bind otherwise, so the call form
/// is written there (D-1749): it is an operand
/// of an operator that binds as tightly as `|>` or more (§5.4.1), other than
/// the left side of a `|>`, of a prefix operator, the receiver of a `.` or
/// the callee of a call. The same test as completion's after `xs.` (X §4.2).
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "a match on syntax kinds names only the kinds it handles"
)]
fn tight(call: SyntaxNode<'_>) -> bool {
    let Some(parent) = call.parent() else {
        return false;
    };
    match parent.kind() {
        K::BIN_EXPR => {
            let operator = parent
                .children_with_tokens()
                .filter_map(cheby_syntax::SyntaxElement::as_token)
                .find(|t| !t.kind().is_trivia() && t.kind() != K::NEWLINE);
            operator.is_some_and(|op| match op.kind() {
                K::PIPE_GT => parent.first_child() != Some(call),
                K::PIPE
                | K::CARET
                | K::AMP
                | K::SHL
                | K::SHR
                | K::PLUS
                | K::MINUS
                | K::STAR
                | K::SLASH
                | K::PERCENT => true,
                _ => false,
            })
        }
        K::PREFIX_EXPR | K::FIELD_EXPR => true,
        K::CALL_EXPR => parent.first_child() == Some(call),
        _ => false,
    }
}

#[cfg(test)]
#[path = "habits_tests.rs"]
pub(crate) mod tests;
