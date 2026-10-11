//! Views of patterns (App. A.6, ch. 6).

use cheby_diag::TextRange;

use super::{
    AstChildren, AstNode, AstToken, FloatLit, Ident, IntLit, PathNode, RawStringLit, StringLit,
    child, child_token, children, token,
};
use crate::{SyntaxKind, SyntaxNode, SyntaxToken};

ast_nodes! {
    /// `_` (§6.2).
    WildcardPat = WILDCARD_PAT;
    /// A binding `name` (§6.2).
    IdentPat = IDENT_PAT;
    /// A literal pattern: `1`, `-1`, `"text"` or a raw string (§6.3).
    LiteralPat = LITERAL_PAT;
    /// A constructor pattern: `None`, `Some(x)`, `Point { x, .. }` (§6.4).
    CtorPat = CTOR_PAT;
    /// The `(p, q)` positional fields of a constructor pattern.
    PosPatList = POS_PAT_LIST;
    /// The `{ x, y: p, .. }` named fields of a constructor pattern.
    FieldPatList = FIELD_PAT_LIST;
    /// One named field pattern `x` or `x: p`.
    FieldPat = FIELD_PAT;
    /// `..` or `..name` in a list pattern, `..` in a field pattern list.
    RestPat = REST_PAT;
    /// A tuple pattern `(a, b)` (§6.5).
    TuplePat = TUPLE_PAT;
    /// A list pattern `[a, ..rest]` (§6.6).
    ListPat = LIST_PAT;
    /// A parenthesized pattern `(p)`.
    ParenPat = PAREN_PAT;
    /// Alternatives `A | B` (§6.7).
    OrPat = OR_PAT;
    /// `p as name` (§6.8).
    AsPat = AS_PAT;
}

ast_enum! {
    /// A pattern (App. A.6).
    Pat {
        Wildcard(WildcardPat),
        Ident(IdentPat),
        Literal(LiteralPat),
        Ctor(CtorPat),
        Tuple(TuplePat),
        List(ListPat),
        Paren(ParenPat),
        Or(OrPat),
        As(AsPat),
    }
}

impl<'t> IdentPat<'t> {
    /// The bound name.
    #[must_use]
    pub fn name(self) -> Option<Ident<'t>> {
        child_token(self.0)
    }
}

/// What a literal pattern matches.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LiteralPatKind<'t> {
    Int(IntLit<'t>),
    /// Not allowed (D-113, E0033), kept so that the error can point at it.
    Float(FloatLit<'t>),
    String(StringLit<'t>),
    RawString(RawStringLit<'t>),
}

impl<'t> LiteralPat<'t> {
    /// The literal.
    #[must_use]
    pub fn kind(self) -> Option<LiteralPatKind<'t>> {
        if let Some(string) = child::<StringLit<'t>>(self.0) {
            return Some(LiteralPatKind::String(string));
        }
        child_token(self.0)
            .map(LiteralPatKind::Int)
            .or_else(|| child_token(self.0).map(LiteralPatKind::Float))
            .or_else(|| child_token(self.0).map(LiteralPatKind::RawString))
    }

    /// The `-` of a negative number, if written.
    #[must_use]
    pub fn minus(self) -> Option<SyntaxToken<'t>> {
        token(self.0, SyntaxKind::MINUS)
    }

    /// Whether the number is negative: whether it has a `-`. Only `-5`,
    /// with the `-` directly before the number, is a valid negative
    /// pattern; `- 5` is rejected by validation (E0056), so later phases
    /// never see one.
    #[must_use]
    pub fn is_negative(self) -> bool {
        self.minus().is_some()
    }

    /// The range between the `-` and the number: empty for `-5`, the
    /// space for `- 5`. `None` without a `-` or a number.
    #[must_use]
    pub fn minus_gap(self) -> Option<TextRange> {
        let minus = self.minus()?.text_range();
        let number = match self.kind()? {
            LiteralPatKind::Int(int) => int.syntax().text_range(),
            LiteralPatKind::Float(float) => float.syntax().text_range(),
            LiteralPatKind::String(_) | LiteralPatKind::RawString(_) => return None,
        };
        (minus.end() <= number.start()).then(|| TextRange::new(minus.end(), number.start()))
    }
}

/// The fields of a constructor pattern.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CtorPatFields<'t> {
    Positional(PosPatList<'t>),
    Named(FieldPatList<'t>),
}

impl<'t> CtorPat<'t> {
    /// The constructor's path.
    #[must_use]
    pub fn path(self) -> Option<PathNode<'t>> {
        child(self.0)
    }

    /// The fields. `None` for a unit constructor or a bare `Name`.
    #[must_use]
    pub fn fields(self) -> Option<CtorPatFields<'t>> {
        child(self.0)
            .map(CtorPatFields::Positional)
            .or_else(|| child(self.0).map(CtorPatFields::Named))
    }
}

impl<'t> PosPatList<'t> {
    /// The patterns, in order.
    #[must_use]
    pub fn patterns(self) -> AstChildren<'t, Pat<'t>> {
        children(self.0)
    }
}

/// An entry of a field pattern list.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FieldPatEntry<'t> {
    Field(FieldPat<'t>),
    Rest(RestPat<'t>),
}

impl<'t> FieldPatList<'t> {
    /// The fields and the rest, in order.
    pub fn entries(self) -> impl Iterator<Item = FieldPatEntry<'t>> + 't {
        self.0.children().filter_map(|node| {
            FieldPat::cast(node)
                .map(FieldPatEntry::Field)
                .or_else(|| RestPat::cast(node).map(FieldPatEntry::Rest))
        })
    }

    /// The named fields, in order.
    #[must_use]
    pub fn fields(self) -> AstChildren<'t, FieldPat<'t>> {
        children(self.0)
    }

    /// The `..` that ignores the other fields, if written.
    #[must_use]
    pub fn rest(self) -> Option<RestPat<'t>> {
        child(self.0)
    }
}

impl<'t> FieldPat<'t> {
    /// The field's name.
    #[must_use]
    pub fn name(self) -> Option<Ident<'t>> {
        child_token(self.0)
    }

    /// The field's pattern. `None` when punned: `Point { x }` binds `x`.
    #[must_use]
    pub fn pattern(self) -> Option<Pat<'t>> {
        child(self.0)
    }
}

impl<'t> RestPat<'t> {
    /// The name that binds the rest of a list, if written.
    #[must_use]
    pub fn name(self) -> Option<Ident<'t>> {
        child_token(self.0)
    }
}

impl<'t> TuplePat<'t> {
    /// The element patterns, in order.
    #[must_use]
    pub fn elements(self) -> AstChildren<'t, Pat<'t>> {
        children(self.0)
    }
}

/// An item of a list pattern.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ListPatItem<'t> {
    Pat(Pat<'t>),
    Rest(RestPat<'t>),
}

impl<'t> ListPat<'t> {
    /// The items, in order.
    pub fn items(self) -> impl Iterator<Item = ListPatItem<'t>> + 't {
        self.0.children().filter_map(|node| {
            Pat::cast(node)
                .map(ListPatItem::Pat)
                .or_else(|| RestPat::cast(node).map(ListPatItem::Rest))
        })
    }

    /// The rests, in order. At most one is allowed (D-158, E0032).
    #[must_use]
    pub fn rests(self) -> AstChildren<'t, RestPat<'t>> {
        children(self.0)
    }
}

impl<'t> ParenPat<'t> {
    /// The pattern inside the parentheses.
    #[must_use]
    pub fn inner(self) -> Option<Pat<'t>> {
        child(self.0)
    }
}

impl<'t> OrPat<'t> {
    /// The alternatives, in order.
    #[must_use]
    pub fn alternatives(self) -> AstChildren<'t, Pat<'t>> {
        children(self.0)
    }
}

impl<'t> AsPat<'t> {
    /// The pattern before `as`.
    #[must_use]
    pub fn pattern(self) -> Option<Pat<'t>> {
        child(self.0)
    }

    /// The name after `as`.
    #[must_use]
    pub fn name(self) -> Option<Ident<'t>> {
        child_token(self.0)
    }
}
