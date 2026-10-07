//! Lexer, event-based parser, flat lossless CST and typed AST views (D-272, D-280).
//!
//! [`parse`] turns one file into a [`SyntaxTree`] plus its grammar errors:
//! [`lexer::lex`] splits the text into raw tokens, the parser turns them into
//! [`event::Event`]s, and [`tree::build`] builds the flat tree. The tree is
//! lossless and always covers the whole text, whatever the errors.
//! [`validate`] then reports the rules that are not grammar errors.
//!
//! Grammar errors (class G) are the inputs that both the spec and
//! `tree-sitter-cheby` reject; the parser cross-check (D-272) compares only
//! those. Stricter spec rules are validation errors (class V).

pub mod ast;
pub mod event;
pub mod lexer;
mod parser;
mod syntax_kind;
mod token_set;
pub mod tree;
mod validate;

use std::sync::Arc;

use cheby_diag::{Diagnostic, FileId};

pub use syntax_kind::SyntaxKind;
pub use token_set::TokenSet;
pub use tree::{NodeId, SyntaxElement, SyntaxNode, SyntaxToken, SyntaxTree, TokenId, TreeStats};

/// The result of parsing one file: the tree and the grammar errors.
#[derive(Clone, Debug)]
pub struct Parse {
    tree: SyntaxTree,
    errors: Vec<Diagnostic>,
}

impl Parse {
    /// The syntax tree, which covers the whole text.
    #[must_use]
    pub const fn tree(&self) -> &SyntaxTree {
        &self.tree
    }

    /// The grammar errors (class G): the lexer's first, then the parser's,
    /// each in source order.
    #[must_use]
    pub fn errors(&self) -> &[Diagnostic] {
        &self.errors
    }

    #[must_use]
    pub fn into_parts(self) -> (SyntaxTree, Vec<Diagnostic>) {
        (self.tree, self.errors)
    }

    /// The typed view of the root.
    #[must_use]
    pub const fn source_file(&self) -> ast::SourceFile<'_> {
        ast::SourceFile::new(self.tree.root())
    }
}

/// Lexes and parses one file. Never fails; the tree covers all of `text`.
#[must_use]
pub fn parse(file: FileId, text: Arc<str>) -> Parse {
    let mut lexed = lexer::lex(file, &text);
    let (events, parse_errors) = parser::parse(file, &lexed, &text);
    let tree = tree::build(file, text, &lexed, events);
    let mut errors = lexed.take_errors();
    errors.extend(parse_errors);
    Parse { tree, errors }
}

/// Checks that are not grammar errors (class V), sorted by position.
#[must_use]
pub fn validate(tree: &SyntaxTree) -> Vec<Diagnostic> {
    validate::validate(tree)
}

/// The token-level part of [`validate`], sorted by position: lone carriage
/// returns (E0002), naming (E0006), reserved words (E0007), number shapes
/// (E0008), escapes (E0011) and whitespace in interpolations (E0012).
///
/// These checks look at single tokens, not at the shape of the tree, so
/// they are meaningful on a file with grammar errors, where the rest of
/// [`validate`] is not (D-425).
#[must_use]
pub fn validate_tokens(tree: &SyntaxTree) -> Vec<Diagnostic> {
    validate::validate_tokens(tree)
}
