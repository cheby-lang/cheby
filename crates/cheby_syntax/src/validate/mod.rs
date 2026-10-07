//! Validation: the checks that are not grammar errors (class V, A10).
//!
//! `tree-sitter-cheby` accepts these inputs, so the parser builds a tree
//! for them without a grammar error, and validation reports what the spec
//! forbids (D-424). The lexical checks run first ([`checks::check_tokens`]);
//! then one preorder walk over the tree dispatches each node to the checks
//! of its kind, which work on the typed views ([`crate::ast`]).
//!
//! - [`module`]: doc comments (E0017), module order (E0018), dangling
//!   attributes (E0019).
//! - [`items`]: signatures (E0020, E0021), `exposed` (E0022), bounds
//!   (E0023), and statement and declaration shapes (E0013).
//! - [`exprs`]: holes (E0026, E0027), pipes (E0028), chained comparisons
//!   (E0029), one-element tuples (E0030), record updates (E0031), type
//!   arguments on interface functions (E0023).
//! - [`patterns`]: tuple patterns (E0030), rests (E0032), float and
//!   interpolated patterns (E0033, E0034), pattern shapes (E0013).
//! - [`case`]: subjectless `case` (E0035) and empty `case` bodies (E0013).
//! - [`types`]: tuple types (E0030) and the tuple-shape warning (W0001).

mod case;
mod exprs;
mod items;
mod module;
mod patterns;
mod types;

use cheby_diag::{Diagnostic, Span, TextRange};

use crate::ast::AstNode;
use crate::lexer::checks;
use crate::{SyntaxKind, SyntaxNode, SyntaxTree};

/// Reports the validation diagnostics of `tree`, sorted by span start.
pub(crate) fn validate(tree: &SyntaxTree) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    checks::check_tokens(tree, &mut out);
    module::check(tree, &mut out);
    for node in tree.root().descendants() {
        check_node(node, &mut out);
    }
    out.sort_by_key(|d| d.primary_span().map(|s| s.range.start()));
    out
}

/// The token-level checks only, sorted by span start.
pub(crate) fn validate_tokens(tree: &SyntaxTree) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    checks::check_tokens(tree, &mut out);
    out.sort_by_key(|d| d.primary_span().map(|s| s.range.start()));
    out
}

/// Runs the checks of `node`'s kind.
fn check_node(node: SyntaxNode<'_>, out: &mut Vec<Diagnostic>) {
    match node.kind() {
        SyntaxKind::ATTR => run(node, module::attr, out),
        SyntaxKind::INNER_ATTR => run(node, module::inner_attr, out),
        SyntaxKind::FN_DECL => run(node, items::fn_decl, out),
        SyntaxKind::CONST_DECL => run(node, items::const_decl, out),
        SyntaxKind::TYPE_DECL => run(node, items::type_decl, out),
        SyntaxKind::ALIAS_DECL => run(node, items::alias_decl, out),
        SyntaxKind::INTERFACE_REF => run(node, items::interface_ref, out),
        SyntaxKind::VARIANT => run(node, items::variant, out),
        SyntaxKind::IMPORT_DECL => run(node, items::import_decl, out),
        SyntaxKind::IMPORT_ITEM => run(node, items::import_item, out),
        SyntaxKind::LOCAL_FN => run(node, items::local_fn, out),
        SyntaxKind::LET_STMT => run(node, items::let_stmt, out),
        SyntaxKind::USE_STMT => run(node, items::use_stmt, out),
        SyntaxKind::ARG_LIST => run(node, exprs::arg_list, out),
        SyntaxKind::PATH_EXPR => run(node, exprs::path_expr, out),
        SyntaxKind::BIN_EXPR => run(node, exprs::bin_expr, out),
        SyntaxKind::TUPLE_EXPR => run(node, exprs::tuple_expr, out),
        SyntaxKind::RECORD_EXPR => run(node, exprs::record_expr, out),
        SyntaxKind::CASE_EXPR => run(node, case::case_expr, out),
        SyntaxKind::TUPLE_PAT => run(node, patterns::tuple_pat, out),
        SyntaxKind::LIST_PAT => run(node, patterns::list_pat, out),
        SyntaxKind::FIELD_PAT_LIST => run(node, patterns::field_pat_list, out),
        SyntaxKind::POS_PAT_LIST => run(node, patterns::pos_pat_list, out),
        SyntaxKind::LITERAL_PAT => run(node, patterns::literal_pat, out),
        SyntaxKind::OR_PAT => run(node, patterns::or_pat, out),
        SyntaxKind::AS_PAT => run(node, patterns::as_pat, out),
        SyntaxKind::TUPLE_TYPE => run(node, types::tuple_type, out),
        _ => {}
    }
}

/// Casts `node` to view `N` and runs `check` on it.
fn run<'t, N: AstNode<'t>>(
    node: SyntaxNode<'t>,
    check: fn(N, &mut Vec<Diagnostic>),
    out: &mut Vec<Diagnostic>,
) {
    if let Some(view) = N::cast(node) {
        check(view, out);
    }
}

/// The significant text of a view: from its first to its last significant
/// token.
fn source<'t, N: AstNode<'t>>(view: N) -> &'t str {
    let span = view.significant_span();
    span.range
        .slice(view.syntax().tree().text())
        .unwrap_or_default()
}

/// An empty span at `offset` in `node`'s file.
fn empty_at(node: SyntaxNode<'_>, offset: u32) -> Span {
    Span::new(node.tree().file(), TextRange::empty(offset))
}

/// Whether `node` has an `ERROR` child, where the parser already reported
/// a grammar error.
fn has_error_child(node: SyntaxNode<'_>) -> bool {
    node.children()
        .any(|child| child.kind() == SyntaxKind::ERROR)
}

#[cfg(test)]
mod tests;
