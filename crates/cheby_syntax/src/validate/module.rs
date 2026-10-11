//! Module-level checks: doc comment placement (E0017), the order of the
//! module header (E0018) and attributes that apply to nothing (E0019).
//!
//! `tree-sitter-cheby` accepts doc comments anywhere, imports and inner
//! attributes after items, and attributes before nothing, so these are
//! validation errors (D-424).

use std::collections::HashMap;

use cheby_diag::{Diagnostic, Span, TextRange, TextSize, codes};

use crate::ast::{
    AstNode, AstToken, Attr, ImportDecl, InnerAttr, InterfaceFn, Item, RecordField, SourceFile,
    Variant,
};
use crate::{NodeId, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken, SyntaxTree};

/// Runs the module-level checks of `tree`.
pub(super) fn check(tree: &SyntaxTree, out: &mut Vec<Diagnostic>) {
    let file = SourceFile::new(tree.root());
    doc_comments(file, out);
    module_order(file, out);
    dangling_attrs(file, out);
}

/// Whether `node` starts the module header or the items: an inner
/// attribute, an import or an item.
fn is_header_or_item(node: SyntaxNode<'_>) -> bool {
    InnerAttr::can_cast(node.kind())
        || ImportDecl::can_cast(node.kind())
        || Item::can_cast(node.kind())
}

/// E0017: a `///` comment must come right before an item, a variant, a
/// field or an interface function, before its attributes (§2.2, App. A.2,
/// D-211, D-227). §2.2 is unclear about a `///` between attributes, and
/// App. A.2 forbids it (D-434). A `//!` comment must come before the first
/// inner attribute, import or item (§2.2, App. A.1).
///
/// A `///` documents its parent when it lies in the parent's leading
/// trivia, as [`Item::doc_comments`] defines. The end of that trivia is
/// found once per parent, so a long run of doc comments costs linear time.
fn doc_comments(file: SourceFile<'_>, out: &mut Vec<Diagnostic>) {
    let root = file.syntax();
    let header_start = root
        .children()
        .find(|node| is_header_or_item(*node))
        .map(|node| node.text_range().start());
    let mut leading_end: HashMap<NodeId, Option<TextSize>> = HashMap::new();
    for token in root.tokens() {
        match token.kind() {
            SyntaxKind::DOC_COMMENT => {
                let attached = token.parent().filter(|parent| is_doc_attached(*parent));
                let documented = attached.is_some_and(|parent| {
                    let end = *leading_end
                        .entry(parent.id())
                        .or_insert_with(|| end_of_leading_trivia(parent));
                    end.is_none_or(|end| token.text_range().start() < end)
                });
                if !documented {
                    out.push(misplaced_doc(token, attached.is_some()));
                }
            }
            SyntaxKind::MODULE_DOC => {
                let at_top = token.parent() == Some(root)
                    && header_start.is_none_or(|start| token.text_range().start() < start);
                if !at_top {
                    out.push(
                        Diagnostic::new(
                            codes::MISPLACED_DOC_COMMENT,
                            "module doc comment after the start of the module",
                        )
                        .with_primary(token.span(), "")
                        .with_help(
                            "move it to the top of the file, before the imports and items, or \
                             use `//` for an ordinary comment",
                        ),
                    );
                }
            }
            _ => {}
        }
    }
}

/// Whether `node` is a kind that doc comments attach to.
fn is_doc_attached(node: SyntaxNode<'_>) -> bool {
    let kind = node.kind();
    Item::can_cast(kind)
        || Variant::can_cast(kind)
        || RecordField::can_cast(kind)
        || InterfaceFn::can_cast(kind)
}

/// The start of the first child of `node` that is not trivia or a line
/// break, or `None` if every child is.
fn end_of_leading_trivia(node: SyntaxNode<'_>) -> Option<TextSize> {
    node.children_with_tokens()
        .find(|child| match child {
            SyntaxElement::Token(token) => {
                !(token.kind().is_trivia() || token.kind() == SyntaxKind::NEWLINE)
            }
            SyntaxElement::Node(_) => true,
        })
        .map(|child| child.text_range().start())
}

/// E0017 for one `///` comment. `attached_kind` says whether its parent is
/// a kind that doc comments attach to, so that the comment comes after the
/// attributes.
fn misplaced_doc(token: SyntaxToken<'_>, attached_kind: bool) -> Diagnostic {
    let diag = Diagnostic::new(
        codes::MISPLACED_DOC_COMMENT,
        "doc comment documents nothing",
    )
    .with_primary(token.span(), "");
    if attached_kind {
        return diag.with_help("doc comments come before the attributes; move it above them");
    }
    let slashes = Span::new(
        token.span().file,
        TextRange::at(token.text_range().start(), 3),
    );
    diag.with_note(
        "a `///` comment documents the item, variant, field or interface function right after it",
    )
    .with_suggestion("use `//` for an ordinary comment", slashes, "//")
}

/// E0018: inner attributes come before imports, and imports before items
/// (D-143, D-201). Each node out of place is reported once.
fn module_order(file: SourceFile<'_>, out: &mut Vec<Diagnostic>) {
    let mut first_import = None;
    let mut first_item = None;
    for node in file.syntax().children() {
        if let Some(attr) = InnerAttr::cast(node) {
            let after = first_import
                .map(|span| (span, "import"))
                .or_else(|| first_item.map(|span| (span, "item")));
            if let Some((span, what)) = after {
                out.push(
                    Diagnostic::new(
                        codes::MODULE_ORDER,
                        format!("inner attribute after an {what}"),
                    )
                    .with_primary(attr.significant_span(), "")
                    .with_secondary(span, format!("first {what} here"))
                    .with_help(
                        "move inner attributes to the top of the file, after the `//!` docs \
                         and before the imports (D-201)",
                    ),
                );
            }
        } else if let Some(import) = ImportDecl::cast(node) {
            if let Some(span) = first_item {
                out.push(
                    Diagnostic::new(codes::MODULE_ORDER, "`import` after an item")
                        .with_primary(import.significant_span(), "")
                        .with_secondary(span, "first item here")
                        .with_help("move all imports before the first item (D-143)"),
                );
            } else if first_import.is_none() {
                first_import = Some(head_span(
                    import.syntax(),
                    import.module_path().map(AstNode::significant_span),
                ));
            }
        } else if let Some(item) = Item::cast(node)
            && first_item.is_none()
        {
            first_item = Some(head_span(item.syntax(), item.name().map(AstToken::span)));
        }
    }
}

/// The head of a declaration, for a secondary label: from its first
/// significant token to the end of `name` (or of that token), so that the
/// label stays on one line.
fn head_span(node: SyntaxNode<'_>, name: Option<Span>) -> Span {
    let first = node
        .children_with_tokens()
        .find_map(|child| match child {
            SyntaxElement::Token(token)
                if !token.kind().is_trivia() && token.kind() != SyntaxKind::NEWLINE =>
            {
                Some(token.text_range())
            }
            SyntaxElement::Node(child) if Attr::can_cast(child.kind()) => None,
            SyntaxElement::Node(child) => {
                child.first_significant_token().map(SyntaxToken::text_range)
            }
            SyntaxElement::Token(_) => None,
        })
        .unwrap_or_else(|| node.text_range());
    let end = name.map_or(first.end(), |span| span.range.end().max(first.end()));
    Span::new(node.tree().file(), TextRange::new(first.start(), end))
}

/// E0019: attributes that no item follows. The parser keeps them in an
/// `ERROR` node at the top level without a grammar error.
fn dangling_attrs(file: SourceFile<'_>, out: &mut Vec<Diagnostic>) {
    for node in file.syntax().children() {
        if node.kind() != SyntaxKind::ERROR || !only_attrs(node) {
            continue;
        }
        for attr in node.children().filter_map(Attr::cast) {
            out.push(
                Diagnostic::new(
                    codes::DANGLING_ATTRIBUTE,
                    "attribute not followed by an item",
                )
                .with_primary(attr.significant_span(), "")
                .with_help("put the attribute on the line right before the item it applies to"),
            );
        }
    }
}

/// Whether an `ERROR` node holds only attributes and line breaks.
fn only_attrs(node: SyntaxNode<'_>) -> bool {
    let mut any = false;
    for child in node.children_with_tokens() {
        match child {
            SyntaxElement::Node(child) if Attr::can_cast(child.kind()) => any = true,
            SyntaxElement::Token(token)
                if token.kind().is_trivia() || token.kind() == SyntaxKind::NEWLINE => {}
            _ => return false,
        }
    }
    any
}

/// E0019: a space between `@` or `@!` and the attribute name (§2.7).
pub(super) fn attr(attr: Attr<'_>, out: &mut Vec<Diagnostic>) {
    attr_space(attr.syntax(), out);
}

/// E0019 for an inner attribute.
pub(super) fn inner_attr(attr: InnerAttr<'_>, out: &mut Vec<Diagnostic>) {
    attr_space(attr.syntax(), out);
}

fn attr_space(node: SyntaxNode<'_>, out: &mut Vec<Diagnostic>) {
    let at = node
        .first_token_of_kind(SyntaxKind::AT)
        .or_else(|| node.first_token_of_kind(SyntaxKind::AT_BANG));
    let Some(SyntaxElement::Token(space)) = at.and_then(SyntaxToken::next_sibling) else {
        return;
    };
    if space.kind() != SyntaxKind::WHITESPACE {
        return;
    }
    out.push(
        Diagnostic::new(
            codes::DANGLING_ATTRIBUTE,
            "space between `@` and the attribute name",
        )
        .with_primary(space.span(), "")
        .with_suggestion("remove the space", space.span(), ""),
    );
}
