//! Tokens at a cursor (X §1.5, "Token at a position").
//!
//! `SyntaxNode::token_at_offset` is right-biased, and a cursor right after
//! a name sits on the token after it. Features want either side: hover and
//! go-to the name on whichever side holds one, completion and selection
//! the token on the left. [`token_at`] gives both.

use cheby_driver::analysis::{TextRange, TextSize};
use cheby_syntax::{SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};

/// The tokens at an offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenAt<'t> {
    /// The offset is outside the tree, or the tree has no token.
    None,
    /// The offset is inside one token, or at the very start or end of the
    /// tree.
    One(SyntaxToken<'t>),
    /// The offset is where one token ends and the next starts: the left
    /// one first.
    Between(SyntaxToken<'t>, SyntaxToken<'t>),
}

impl<'t> TokenAt<'t> {
    /// The token on the left: the one that ends at the offset, or the one
    /// that holds it.
    #[must_use]
    pub(crate) fn left(self) -> Option<SyntaxToken<'t>> {
        match self {
            Self::None => None,
            Self::One(token) | Self::Between(token, _) => Some(token),
        }
    }

    /// The token on the right: the one that starts at the offset, or the
    /// one that holds it.
    #[must_use]
    pub(crate) fn right(self) -> Option<SyntaxToken<'t>> {
        match self {
            Self::None => None,
            Self::One(token) | Self::Between(_, token) => Some(token),
        }
    }
}

/// The tokens at `offset` in the tree under `root`. Empty tokens are
/// skipped, so `Between` holds two tokens that touch at `offset`.
#[must_use]
pub(crate) fn token_at(root: SyntaxNode<'_>, offset: TextSize) -> TokenAt<'_> {
    let range = root.text_range();
    if offset < range.start() || offset > range.end() {
        return TokenAt::None;
    }
    // `token_at_offset` takes the token that holds the offset, so it is the
    // right one unless the offset is the tree's end. An empty range under
    // `covering_element` takes the token that ends there: the left one.
    let right = root
        .token_at_offset(offset)
        .filter(|t| t.text_range().contains(offset));
    let left = match root.covering_element(TextRange::empty(offset)) {
        Some(SyntaxElement::Token(token)) => Some(token),
        Some(SyntaxElement::Node(_)) | None => None,
    };
    match (left, right) {
        (Some(l), Some(r)) if l != r && l.text_range().end() == offset => TokenAt::Between(l, r),
        (_, Some(r)) => TokenAt::One(r),
        (Some(l), None) => TokenAt::One(l),
        (None, None) => TokenAt::None,
    }
}

/// The token a name feature should look at: a name if either side has one,
/// else an operator, else a keyword (X §1.5). Each class looks right first,
/// then left.
#[must_use]
pub(crate) fn pick_ident(at: TokenAt<'_>) -> Option<SyntaxToken<'_>> {
    let (left, right) = match at {
        TokenAt::None => return None,
        TokenAt::One(token) => (None, token),
        TokenAt::Between(l, r) => (Some(l), r),
    };
    let classes: [fn(SyntaxKind) -> bool; 5] = [
        |k| k == SyntaxKind::LOWER_IDENT,
        |k| k == SyntaxKind::UPPER_IDENT,
        |k| k == SyntaxKind::UNDERSCORE,
        is_operator,
        SyntaxKind::is_keyword,
    ];
    classes.iter().find_map(|class| {
        std::iter::once(right)
            .chain(left)
            .find(|token| class(token.kind()))
    })
}

/// The nearest token before `token` that is neither trivia nor a line
/// break.
#[cfg_attr(not(test), expect(dead_code, reason = "completion uses it from W8"))]
#[must_use]
pub(crate) fn prev_significant(token: SyntaxToken<'_>) -> Option<SyntaxToken<'_>> {
    std::iter::successors(token.prev_token(), |t| t.prev_token()).find(|t| is_significant(t.kind()))
}

/// Whether a token of `kind` is neither trivia nor a line break.
#[must_use]
pub(crate) fn is_significant(kind: SyntaxKind) -> bool {
    !kind.is_trivia() && kind != SyntaxKind::NEWLINE
}

/// Whether `kind` is a binary or prefix operator, which may name an
/// interface function (§8.8).
fn is_operator(kind: SyntaxKind) -> bool {
    use SyntaxKind as K;
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
            | K::PIPE_GT
    )
}

#[cfg(test)]
mod tests {
    use cheby_driver::analysis::TextSize;
    use cheby_syntax::{SyntaxKind, SyntaxToken, SyntaxTree};

    use super::{TokenAt, pick_ident, prev_significant, token_at};
    use crate::features::handlers::symbols::tests::doc;

    fn parse(text: &str) -> SyntaxTree {
        doc(text).tree
    }

    /// The text at `|`, with the marker removed, and its offset.
    fn marked(text: &str) -> (String, TextSize) {
        let at = text.find('|').expect("a marker");
        let offset = TextSize::try_from(at).expect("a small text");
        (text.replacen('|', "", 1), offset)
    }

    fn texts(at: TokenAt<'_>) -> Vec<&str> {
        match at {
            TokenAt::None => vec![],
            TokenAt::One(t) => vec![t.text()],
            TokenAt::Between(l, r) => vec![l.text(), r.text()],
        }
    }

    #[test]
    fn token_at_sees_both_sides() {
        let cases: &[(&str, &[&str])] = &[
            ("fn f() { x.fo|\n}\n", &["fo", "\n"]),
            ("fn f() { list::|\n}\n", &["::", "\n"]),
            ("fn f() { fo|o }\n", &["foo"]),
            ("fn f() { foo|(1) }\n", &["foo", "("]),
            ("|fn f() {}\n", &["fn"]),
            ("fn f() {}\n|", &["\n"]),
            ("ab|", &["ab"]),
        ];
        for (text, want) in cases {
            let (text, offset) = marked(text);
            let tree = parse(&text);
            assert_eq!(texts(token_at(tree.root(), offset)), *want, "{text:?}");
        }
    }

    #[test]
    fn token_at_outside_the_tree_is_none() {
        let tree = parse("fn f() {}\n");
        assert_eq!(token_at(tree.root(), 99), TokenAt::None);
        let empty = parse("");
        assert_eq!(token_at(empty.root(), 0), TokenAt::None);
    }

    #[test]
    fn left_and_right() {
        let (text, offset) = marked("fn f() { foo|(1) }\n");
        let tree = parse(&text);
        let at = token_at(tree.root(), offset);
        assert_eq!(at.left().map(SyntaxToken::text), Some("foo"));
        assert_eq!(at.right().map(SyntaxToken::text), Some("("));
        let (text, offset) = marked("fn f() { fo|o }\n");
        let tree = parse(&text);
        let at = token_at(tree.root(), offset);
        assert_eq!(at.left().map(SyntaxToken::text), Some("foo"));
        assert_eq!(at.right().map(SyntaxToken::text), Some("foo"));
    }

    #[test]
    fn pick_ident_prefers_names_then_operators_then_keywords() {
        let cases: &[(&str, Option<&str>)] = &[
            ("fn f() { foo|(1) }\n", Some("foo")),
            ("fn f() { (|foo) }\n", Some("foo")),
            ("fn f() { a|+b }\n", Some("a")),
            ("fn f() { 1 +| 2 }\n", Some("+")),
            ("fn f() { case| x {} }\n", Some("case")),
            ("fn f() { x |}\n", None),
            ("fn f() { Some|(1) }\n", Some("Some")),
        ];
        for (text, want) in cases {
            let (text, offset) = marked(text);
            let tree = parse(&text);
            let got = pick_ident(token_at(tree.root(), offset)).map(SyntaxToken::text);
            assert_eq!(got, *want, "{text:?}");
        }
    }

    #[test]
    fn prev_significant_skips_trivia_and_line_breaks() {
        let (text, offset) = marked("fn f() {\n  foo(1, // c\n  |\n}\n");
        let tree = parse(&text);
        let at = token_at(tree.root(), offset).left().expect("a token");
        let prev = prev_significant(at).expect("a token before");
        assert_eq!((prev.kind(), prev.text()), (SyntaxKind::COMMA, ","));
        let first = tree.root().first_token().expect("a token");
        assert_eq!(prev_significant(first), None);
    }
}
