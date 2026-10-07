//! Parser events, the input of the tree builder (D-280).
//!
//! The parser does not build the tree itself. It emits a flat list of
//! events, and [`crate::tree::build`] turns them into a [`crate::SyntaxTree`].
//! The parser never emits trivia: the builder attaches every raw token that
//! no `Token` or `Split` event names, so the tree is lossless whatever the
//! parser does.

use std::num::NonZeroU32;

use crate::SyntaxKind;

/// One step of building a tree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// Opens a node of `kind`.
    ///
    /// `forward_parent` is the distance, in events, to a later `Start` that
    /// becomes this node's parent. The parser sets it when it wraps an
    /// already completed node in a new one (`precede`, as in
    /// rust-analyzer). The later `Start` is opened first, at this position,
    /// and is then skipped where it stands.
    Start {
        kind: SyntaxKind,
        forward_parent: Option<NonZeroU32>,
    },
    /// Closes the innermost open node.
    Finish,
    /// The whole raw token `raw` (an index into [`crate::lexer::Lexed`]),
    /// with its lexed kind.
    Token { raw: u32 },
    /// Bytes `offset..offset + len` of raw token `raw`, as a token of `kind`.
    ///
    /// The parser splits `>>` into two `>` and `>=` into `>` and `=` in type
    /// contexts (§2.6, D-430). Every part of the raw token is named by one
    /// `Split`, in order.
    Split {
        raw: u32,
        kind: SyntaxKind,
        offset: u8,
        len: u8,
    },
    /// An abandoned `Start`, or a `Start` already opened through
    /// `forward_parent`. The builder skips it.
    Tombstone,
}
