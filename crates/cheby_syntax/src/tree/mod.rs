//! The flat, arena-allocated, lossless concrete syntax tree (D-280).
//!
//! A [`SyntaxTree`] stores every node and token of a file in one vector, in
//! preorder. Each element knows its kind, its parent, its text range and the
//! index just past its subtree, so children are visited by jumping from one
//! subtree end to the next, and a subtree is a contiguous slice. The tree
//! owns the source text, shared with the [`cheby_diag::SourceMap`].
//!
//! The tree is lossless: the texts of its tokens, in order, are the source.
//! Node ids are stable for the life of a tree, so later phases may keep a
//! [`NodeId`] as a handle. Invalid ids give `None`, never a panic.

mod builder;
mod cursor;
mod dump;

use std::fmt;
use std::sync::Arc;

use cheby_diag::{FileId, TextSize};

pub use builder::build;
pub use cursor::{Children, SyntaxElement, SyntaxNode, SyntaxToken};

use crate::SyntaxKind;

/// One node or token of a tree. 20 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Element {
    kind: SyntaxKind,
    /// The parent node's index, or [`NO_PARENT`] for the root.
    parent: u32,
    start: TextSize,
    end: TextSize,
    /// For a node, the index just past its last descendant. For a token,
    /// its own index plus one.
    subtree_end: u32,
}

const _: () = assert!(size_of::<Element>() == 20);

/// The parent of the root.
const NO_PARENT: u32 = u32::MAX;

/// The id of a node in its tree.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct NodeId(u32);

/// The id of a token in its tree.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct TokenId(u32);

impl NodeId {
    /// The index of the node in the tree's preorder.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }
}

impl TokenId {
    /// The index of the token in the tree's preorder.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// The concrete syntax tree of one file.
#[derive(Clone)]
pub struct SyntaxTree {
    file: FileId,
    text: Arc<str>,
    elements: Vec<Element>,
}

/// Sizes of a tree, for benchmarks.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct TreeStats {
    pub nodes: usize,
    pub tokens: usize,
    /// Bytes allocated for the elements, not counting the shared text.
    pub heap_bytes: usize,
}

/// `i` as a vector index.
fn index(i: u32) -> usize {
    usize::try_from(i).unwrap_or(usize::MAX)
}

/// `i` as a `u32` element index. Trees with more than `u32::MAX` elements
/// cannot come from a text that fits a `SourceMap`.
fn id(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

impl SyntaxTree {
    /// The file this tree was parsed from.
    #[must_use]
    pub const fn file(&self) -> FileId {
        self.file
    }

    /// The full source text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The source text, shared.
    #[must_use]
    pub fn text_arc(&self) -> Arc<str> {
        Arc::clone(&self.text)
    }

    /// The root node, a `SOURCE_FILE`.
    #[must_use]
    pub const fn root(&self) -> SyntaxNode<'_> {
        SyntaxNode::new_unchecked(self, 0)
    }

    /// The node with id `id`, if it is a node of this tree.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<SyntaxNode<'_>> {
        self.elem(id.0)
            .filter(|e| e.kind.is_node())
            .map(|_| SyntaxNode::new_unchecked(self, id.0))
    }

    /// The token with id `id`, if it is a token of this tree.
    #[must_use]
    pub fn token(&self, id: TokenId) -> Option<SyntaxToken<'_>> {
        self.elem(id.0)
            .filter(|e| e.kind.is_token())
            .map(|_| SyntaxToken::new_unchecked(self, id.0))
    }

    /// The number of nodes and tokens and the memory they take.
    #[must_use]
    pub fn stats(&self) -> TreeStats {
        let nodes = self.elements.iter().filter(|e| e.kind.is_node()).count();
        TreeStats {
            nodes,
            tokens: self.elements.len().saturating_sub(nodes),
            heap_bytes: self
                .elements
                .capacity()
                .saturating_mul(size_of::<Element>()),
        }
    }

    /// The tree as indented text, one element per line, for snapshot tests:
    ///
    /// ```text
    /// SOURCE_FILE@0..12
    ///   FN_DECL@0..11
    ///     FN_KW@0..2 "fn"
    /// ```
    #[must_use]
    pub fn debug_dump(&self) -> String {
        dump::dump(self)
    }

    fn elem(&self, i: u32) -> Option<&Element> {
        self.elements.get(index(i))
    }

    fn len(&self) -> u32 {
        id(self.elements.len())
    }
}

impl fmt::Debug for SyntaxTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyntaxTree")
            .field("file", &self.file)
            .field("text_len", &self.text.len())
            .field("elements", &self.elements.len())
            .finish()
    }
}

#[cfg(test)]
mod tests;
