//! Borrowed views of nodes and tokens, and navigation between them.
//!
//! A [`SyntaxNode`] or [`SyntaxToken`] is a tree reference plus an index,
//! so it is `Copy` and cheap. Navigation reads the preorder layout of
//! [`SyntaxTree`]: the children of a node start right after it, and each
//! child's subtree end is the index of the next child.

use std::fmt;

use cheby_diag::{Span, TextRange, TextSize};

use super::{Element, NO_PARENT, NodeId, SyntaxTree, TokenId};
use crate::SyntaxKind;

/// A node of a [`SyntaxTree`].
#[derive(Clone, Copy)]
pub struct SyntaxNode<'t> {
    tree: &'t SyntaxTree,
    id: u32,
}

/// A token of a [`SyntaxTree`].
#[derive(Clone, Copy)]
pub struct SyntaxToken<'t> {
    tree: &'t SyntaxTree,
    id: u32,
}

/// A node or a token.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SyntaxElement<'t> {
    Node(SyntaxNode<'t>),
    Token(SyntaxToken<'t>),
}

// Views compare by identity: equal when they name the same element of the
// same tree.
macro_rules! identity_eq {
    ($ty:ident) => {
        impl PartialEq for $ty<'_> {
            fn eq(&self, other: &Self) -> bool {
                std::ptr::eq(self.tree, other.tree) && self.id == other.id
            }
        }

        impl Eq for $ty<'_> {}

        impl std::hash::Hash for $ty<'_> {
            fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                std::ptr::hash(self.tree, state);
                self.id.hash(state);
            }
        }
    };
}

identity_eq!(SyntaxNode);
identity_eq!(SyntaxToken);

/// The element at `i`, or a harmless placeholder for an invalid index.
fn element(tree: &SyntaxTree, i: u32) -> Element {
    tree.elem(i).copied().unwrap_or(Element {
        kind: SyntaxKind::ERROR,
        parent: NO_PARENT,
        start: 0,
        end: 0,
        subtree_end: i.saturating_add(1),
    })
}

/// The element at `i` as a node or token view, if `i` is in the tree.
fn view(tree: &SyntaxTree, i: u32) -> Option<SyntaxElement<'_>> {
    let e = tree.elem(i)?;
    Some(if e.kind.is_node() {
        SyntaxElement::Node(SyntaxNode { tree, id: i })
    } else {
        SyntaxElement::Token(SyntaxToken { tree, id: i })
    })
}

/// The node at `i`, if `i` is a node of the tree.
fn node_at(tree: &SyntaxTree, i: u32) -> Option<SyntaxNode<'_>> {
    match view(tree, i)? {
        SyntaxElement::Node(node) => Some(node),
        SyntaxElement::Token(_) => None,
    }
}

/// The token at `i`, if `i` is a token of the tree.
fn token_at(tree: &SyntaxTree, i: u32) -> Option<SyntaxToken<'_>> {
    match view(tree, i)? {
        SyntaxElement::Token(token) => Some(token),
        SyntaxElement::Node(_) => None,
    }
}

/// Iterates over the children of a node: from the first child, each next
/// child starts at the previous one's subtree end.
#[derive(Clone, Debug)]
pub struct Children<'t> {
    tree: &'t SyntaxTree,
    next: u32,
    end: u32,
}

impl<'t> Iterator for Children<'t> {
    type Item = SyntaxElement<'t>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.end {
            return None;
        }
        let i = self.next;
        let e = element(self.tree, i);
        // A subtree end always lies past its element; guard anyway so the
        // iterator always advances.
        self.next = if e.subtree_end > i {
            e.subtree_end
        } else {
            self.end
        };
        view(self.tree, i)
    }
}

impl<'t> SyntaxNode<'t> {
    pub(super) const fn new_unchecked(tree: &'t SyntaxTree, id: u32) -> Self {
        Self { tree, id }
    }

    fn elem(self) -> Element {
        element(self.tree, self.id)
    }

    /// The tree this node belongs to.
    #[must_use]
    pub const fn tree(self) -> &'t SyntaxTree {
        self.tree
    }

    #[must_use]
    pub const fn id(self) -> NodeId {
        NodeId(self.id)
    }

    #[must_use]
    pub fn kind(self) -> SyntaxKind {
        self.elem().kind
    }

    /// The text range, from the start of the first token to the end of the
    /// last. An empty node has an empty range at its position.
    #[must_use]
    pub fn text_range(self) -> TextRange {
        let e = self.elem();
        TextRange::new(e.start, e.end)
    }

    /// The text range in the tree's file.
    #[must_use]
    pub fn span(self) -> Span {
        Span::new(self.tree.file, self.text_range())
    }

    /// The source text of the node, including its trivia.
    #[must_use]
    pub fn text(self) -> &'t str {
        self.text_range().slice(&self.tree.text).unwrap_or_default()
    }

    /// The parent node, `None` for the root.
    #[must_use]
    pub fn parent(self) -> Option<Self> {
        let parent = self.elem().parent;
        (parent != NO_PARENT)
            .then(|| node_at(self.tree, parent))
            .flatten()
    }

    /// This node, its parent, and so on up to the root.
    pub fn ancestors(self) -> impl Iterator<Item = Self> + 't {
        std::iter::successors(Some(self), |node| node.parent())
    }

    /// The child nodes and tokens, in order.
    #[must_use]
    pub fn children_with_tokens(self) -> Children<'t> {
        Children {
            tree: self.tree,
            next: self.id.saturating_add(1),
            end: self.elem().subtree_end,
        }
    }

    /// The child nodes, in order.
    pub fn children(self) -> impl Iterator<Item = Self> + 't {
        self.children_with_tokens().filter_map(|child| match child {
            SyntaxElement::Node(node) => Some(node),
            SyntaxElement::Token(_) => None,
        })
    }

    /// The first child node.
    #[must_use]
    pub fn first_child(self) -> Option<Self> {
        self.children().next()
    }

    /// The first child node of `kind`.
    #[must_use]
    pub fn first_child_of_kind(self, kind: SyntaxKind) -> Option<Self> {
        self.children().find(|child| child.kind() == kind)
    }

    /// The first child token of `kind`.
    #[must_use]
    pub fn first_token_of_kind(self, kind: SyntaxKind) -> Option<SyntaxToken<'t>> {
        self.children_with_tokens().find_map(|child| match child {
            SyntaxElement::Token(token) if token.kind() == kind => Some(token),
            _ => None,
        })
    }

    /// The next sibling node, skipping tokens.
    #[must_use]
    pub fn next_sibling(self) -> Option<Self> {
        let mut next = self.next_sibling_or_token();
        while let Some(element) = next {
            match element {
                SyntaxElement::Node(node) => return Some(node),
                SyntaxElement::Token(token) => next = token.next_sibling(),
            }
        }
        None
    }

    /// The previous sibling node, skipping tokens.
    #[must_use]
    pub fn prev_sibling(self) -> Option<Self> {
        self.parent()?
            .children()
            .take_while(|child| child.id < self.id)
            .last()
    }

    /// The next sibling node or token.
    #[must_use]
    pub fn next_sibling_or_token(self) -> Option<SyntaxElement<'t>> {
        next_sibling(self.tree, self.id)
    }

    /// The previous sibling node or token.
    #[must_use]
    pub fn prev_sibling_or_token(self) -> Option<SyntaxElement<'t>> {
        prev_sibling(self.tree, self.id)
    }

    /// This node and all nodes below it, in preorder.
    pub fn descendants(self) -> impl Iterator<Item = Self> + 't {
        let tree = self.tree;
        (self.id..self.elem().subtree_end).filter_map(move |i| node_at(tree, i))
    }

    /// This node and all nodes and tokens below it, in preorder.
    pub fn descendants_with_tokens(self) -> impl Iterator<Item = SyntaxElement<'t>> + 't {
        let tree = self.tree;
        (self.id..self.elem().subtree_end).filter_map(move |i| view(tree, i))
    }

    /// All tokens of the subtree, in source order. Their texts concatenate
    /// to the node's text.
    pub fn tokens(self) -> impl DoubleEndedIterator<Item = SyntaxToken<'t>> + 't {
        let tree = self.tree;
        (self.id.saturating_add(1)..self.elem().subtree_end).filter_map(move |i| token_at(tree, i))
    }

    /// The first token of the subtree, which may be trivia.
    #[must_use]
    pub fn first_token(self) -> Option<SyntaxToken<'t>> {
        self.tokens().next()
    }

    /// The last token of the subtree, which may be trivia.
    #[must_use]
    pub fn last_token(self) -> Option<SyntaxToken<'t>> {
        self.tokens().next_back()
    }

    /// The first token that is neither trivia nor a `NEWLINE`.
    #[must_use]
    pub fn first_significant_token(self) -> Option<SyntaxToken<'t>> {
        self.tokens().find(|t| t.kind().is_significant())
    }

    /// The token whose range contains `offset`. At the end of the node, the
    /// last non-empty token that ends there.
    #[must_use]
    pub fn token_at_offset(self, offset: TextSize) -> Option<SyntaxToken<'t>> {
        let range = self.text_range();
        if offset < range.start() || offset > range.end() {
            return None;
        }
        let at_end = offset == range.end();
        let mut node = self;
        loop {
            let mut found = None;
            for child in node.children_with_tokens() {
                let r = child.text_range();
                if r.contains(offset) {
                    found = Some(child);
                    break;
                }
                if at_end && r.end() == offset && !r.is_empty() {
                    found = Some(child);
                }
            }
            match found? {
                SyntaxElement::Token(token) => return Some(token),
                SyntaxElement::Node(child) => node = child,
            }
        }
    }

    /// The smallest element whose range contains `range`, if this node's
    /// range does. Empty elements are never chosen over their parent.
    #[must_use]
    pub fn covering_element(self, range: TextRange) -> Option<SyntaxElement<'t>> {
        if !self.text_range().contains_range(range) {
            return None;
        }
        let mut node = self;
        loop {
            let child = node.children_with_tokens().find(|child| {
                let r = child.text_range();
                !r.is_empty() && r.contains_range(range)
            });
            match child {
                Some(SyntaxElement::Node(child)) => node = child,
                Some(token @ SyntaxElement::Token(_)) => return Some(token),
                None => return Some(SyntaxElement::Node(node)),
            }
        }
    }
}

/// The element right after the subtree of `i`, if it is still inside the
/// parent of `i`.
fn next_sibling(tree: &SyntaxTree, i: u32) -> Option<SyntaxElement<'_>> {
    let e = element(tree, i);
    if e.parent == NO_PARENT {
        return None;
    }
    let parent_end = element(tree, e.parent).subtree_end;
    (e.subtree_end < parent_end)
        .then(|| view(tree, e.subtree_end))
        .flatten()
}

/// The child of the parent of `i` that comes right before `i`.
fn prev_sibling(tree: &SyntaxTree, i: u32) -> Option<SyntaxElement<'_>> {
    let parent = node_at(tree, element(tree, i).parent)?;
    parent
        .children_with_tokens()
        .take_while(|child| child.index() < i)
        .last()
}

impl SyntaxKind {
    /// Whether a token of this kind is neither trivia nor a `NEWLINE`.
    const fn is_significant(self) -> bool {
        !self.is_trivia() && !matches!(self, Self::NEWLINE)
    }
}

impl<'t> SyntaxToken<'t> {
    pub(super) const fn new_unchecked(tree: &'t SyntaxTree, id: u32) -> Self {
        Self { tree, id }
    }

    fn elem(self) -> Element {
        element(self.tree, self.id)
    }

    /// The tree this token belongs to.
    #[must_use]
    pub const fn tree(self) -> &'t SyntaxTree {
        self.tree
    }

    #[must_use]
    pub const fn id(self) -> TokenId {
        TokenId(self.id)
    }

    #[must_use]
    pub fn kind(self) -> SyntaxKind {
        self.elem().kind
    }

    #[must_use]
    pub fn text_range(self) -> TextRange {
        let e = self.elem();
        TextRange::new(e.start, e.end)
    }

    /// The text range in the tree's file.
    #[must_use]
    pub fn span(self) -> Span {
        Span::new(self.tree.file, self.text_range())
    }

    #[must_use]
    pub fn text(self) -> &'t str {
        self.text_range().slice(&self.tree.text).unwrap_or_default()
    }

    /// The node that contains this token.
    #[must_use]
    pub fn parent(self) -> Option<SyntaxNode<'t>> {
        node_at(self.tree, self.elem().parent)
    }

    /// The next token in source order, anywhere in the tree.
    #[must_use]
    pub fn next_token(self) -> Option<Self> {
        let tree = self.tree;
        (self.id.saturating_add(1)..tree.len()).find_map(|i| token_at(tree, i))
    }

    /// The previous token in source order, anywhere in the tree.
    #[must_use]
    pub fn prev_token(self) -> Option<Self> {
        let tree = self.tree;
        (0..self.id).rev().find_map(|i| token_at(tree, i))
    }

    /// The next sibling node or token.
    #[must_use]
    pub fn next_sibling(self) -> Option<SyntaxElement<'t>> {
        next_sibling(self.tree, self.id)
    }

    /// The previous sibling node or token.
    #[must_use]
    pub fn prev_sibling(self) -> Option<SyntaxElement<'t>> {
        prev_sibling(self.tree, self.id)
    }
}

impl<'t> SyntaxElement<'t> {
    #[must_use]
    pub fn kind(self) -> SyntaxKind {
        match self {
            Self::Node(node) => node.kind(),
            Self::Token(token) => token.kind(),
        }
    }

    #[must_use]
    pub fn text_range(self) -> TextRange {
        match self {
            Self::Node(node) => node.text_range(),
            Self::Token(token) => token.text_range(),
        }
    }

    #[must_use]
    pub fn span(self) -> Span {
        match self {
            Self::Node(node) => node.span(),
            Self::Token(token) => token.span(),
        }
    }

    #[must_use]
    pub fn text(self) -> &'t str {
        match self {
            Self::Node(node) => node.text(),
            Self::Token(token) => token.text(),
        }
    }

    #[must_use]
    pub fn parent(self) -> Option<SyntaxNode<'t>> {
        match self {
            Self::Node(node) => node.parent(),
            Self::Token(token) => token.parent(),
        }
    }

    #[must_use]
    pub const fn as_node(self) -> Option<SyntaxNode<'t>> {
        match self {
            Self::Node(node) => Some(node),
            Self::Token(_) => None,
        }
    }

    #[must_use]
    pub const fn as_token(self) -> Option<SyntaxToken<'t>> {
        match self {
            Self::Token(token) => Some(token),
            Self::Node(_) => None,
        }
    }

    const fn index(self) -> u32 {
        match self {
            Self::Node(node) => node.id,
            Self::Token(token) => token.id,
        }
    }
}

impl fmt::Debug for SyntaxNode<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.kind().name(), self.text_range())
    }
}

impl fmt::Debug for SyntaxToken<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}@{} {:?}",
            self.kind().name(),
            self.text_range(),
            self.text()
        )
    }
}
