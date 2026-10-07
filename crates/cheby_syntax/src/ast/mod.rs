//! Typed views over the syntax tree (A9).
//!
//! Every node kind except `ERROR` has a view: a `Copy` newtype over a
//! [`SyntaxNode`] named after the kind (`FN_DECL` → [`FnDecl`]). Category
//! enums group views that may stand in the same place: [`Item`], [`Type`],
//! [`Stmt`], [`Expr`] and [`Pat`]. Tokens with a meaning of their own have
//! token views ([`Ident`], [`IntLit`], …).
//!
//! Trees can be broken, so accessors return `Option` or iterators and never
//! panic. A missing child is `None`; an `ERROR` node is skipped by every
//! accessor. Later phases use only these views and [`AstId`], never raw
//! [`SyntaxKind`] matching.
//!
//! Views compare by identity: two views are equal when they wrap the same
//! node of the same tree.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

use cheby_diag::{Span, TextRange};

use crate::tree::Children;
use crate::{NodeId, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken, SyntaxTree};

/// Defines node views: one newtype per node kind, with its [`AstNode`] and
/// [`AstKind`] impls. In tests, also a `KINDS` list of the kinds covered.
macro_rules! ast_nodes {
    ($( $(#[$meta:meta])* $name:ident = $kind:ident; )*) => {
        $(
            $(#[$meta])*
            #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
            pub struct $name<'t>(SyntaxNode<'t>);

            impl<'t> $crate::ast::AstNode<'t> for $name<'t> {
                type Kind = $name<'static>;

                fn can_cast(kind: $crate::SyntaxKind) -> bool {
                    kind == $crate::SyntaxKind::$kind
                }

                fn cast(node: SyntaxNode<'t>) -> Option<Self> {
                    (node.kind() == $crate::SyntaxKind::$kind).then_some(Self(node))
                }

                fn syntax(self) -> SyntaxNode<'t> {
                    self.0
                }
            }

            impl $crate::ast::AstKind for $name<'static> {
                type View<'t> = $name<'t>;
            }
        )*

        /// The node kinds that the views of this module cover.
        #[cfg(test)]
        pub(super) const KINDS: &[$crate::SyntaxKind] = &[$($crate::SyntaxKind::$kind),*];
    };
}

/// Defines a category enum over node views, with its [`AstNode`] and
/// [`AstKind`] impls.
macro_rules! ast_enum {
    (
        $(#[$meta:meta])*
        $name:ident { $( $(#[$vmeta:meta])* $variant:ident($view:ident), )* }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub enum $name<'t> {
            $( $(#[$vmeta])* $variant($view<'t>), )*
        }

        impl<'t> $crate::ast::AstNode<'t> for $name<'t> {
            type Kind = $name<'static>;

            fn can_cast(kind: $crate::SyntaxKind) -> bool {
                $( <$view<'t> as $crate::ast::AstNode<'t>>::can_cast(kind) )||*
            }

            fn cast(node: SyntaxNode<'t>) -> Option<Self> {
                $(
                    if let Some(view) = <$view<'t> as $crate::ast::AstNode<'t>>::cast(node) {
                        return Some(Self::$variant(view));
                    }
                )*
                None
            }

            fn syntax(self) -> SyntaxNode<'t> {
                match self {
                    $( Self::$variant(view) => $crate::ast::AstNode::syntax(view), )*
                }
            }
        }

        impl $crate::ast::AstKind for $name<'static> {
            type View<'t> = $name<'t>;
        }
    };
}

/// Defines token views: one newtype per group of token kinds.
macro_rules! ast_tokens {
    ($( $(#[$meta:meta])* $name:ident = $($kind:ident)|+; )*) => {
        $(
            $(#[$meta])*
            #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
            pub struct $name<'t>(SyntaxToken<'t>);

            impl<'t> $crate::ast::AstToken<'t> for $name<'t> {
                fn can_cast(kind: $crate::SyntaxKind) -> bool {
                    matches!(kind, $($crate::SyntaxKind::$kind)|+)
                }

                fn cast(token: SyntaxToken<'t>) -> Option<Self> {
                    Self::can_cast(token.kind()).then_some(Self(token))
                }

                fn syntax(self) -> SyntaxToken<'t> {
                    self.0
                }
            }
        )*
    };
}

mod exprs;
mod items;
mod patterns;
mod stmts;
mod tokens;
mod types;

pub use exprs::{
    Arg, ArgList, BinExpr, BinOp, CallExpr, CaseArm, CaseArmKind, CaseBody, CaseExpr, Closure,
    CondArm, Expr, FieldExpr, FieldInit, Guard, Hole, Interpolation, ListExpr, ListItem, Literal,
    LiteralKind, PanicExpr, ParenExpr, PathExpr, PrefixExpr, PrefixOp, RecordEntry, RecordExpr,
    Spread, StringLit, StringPart, TodoExpr, TupleExpr,
};
pub use items::{
    AliasDecl, Attr, AttrArg, AttrArgList, BoundList, ConstDecl, FnDecl, ImportDecl, ImportItem,
    ImportList, InnerAttr, InterfaceBody, InterfaceDecl, InterfaceFn, InterfaceRef, Item,
    ModulePath, Param, ParamList, PosFieldList, RecordField, RecordFieldList, Rename, RetType,
    SourceFile, TestDecl, TypeBody, TypeDecl, TypeList, TypeParam, TypeParamList, Variant,
    VariantFields, VariantList, Visibility, VisibilityKind,
};
pub use patterns::{
    AsPat, CtorPat, CtorPatFields, FieldPat, FieldPatEntry, FieldPatList, IdentPat, ListPat,
    ListPatItem, LiteralPat, LiteralPatKind, OrPat, ParenPat, Pat, PosPatList, RestPat, TuplePat,
    WildcardPat,
};
pub use stmts::{AssertStmt, Block, LetStmt, LocalFn, Message, Stmt, UseBinder, UseStmt};
pub use tokens::{
    DocComment, FloatLit, Ident, IntLit, ModuleDoc, Name, RawStringLit, StringEscape, StringText,
    TypeName,
};
pub use types::{DynType, FnType, PathNode, PathType, TupleType, Type, TypeArgList};

/// A typed view of a node.
pub trait AstNode<'t>: Sized + Copy {
    /// The view type with its lifetime erased, which names it in an
    /// [`AstId`].
    type Kind: AstKind;

    /// Whether a node of `kind` has this view.
    fn can_cast(kind: SyntaxKind) -> bool;

    /// The view of `node`, if its kind has this view.
    fn cast(node: SyntaxNode<'t>) -> Option<Self>;

    /// The underlying node.
    fn syntax(self) -> SyntaxNode<'t>;

    /// The node's span, including any doc comments and attributes inside
    /// it.
    fn span(self) -> Span {
        self.syntax().span()
    }

    /// The span from the first to the last token that is neither trivia
    /// nor a line break. For an empty node, its empty span.
    fn significant_span(self) -> Span {
        significant_span(self.syntax())
    }

    /// A handle to this node that later phases can store.
    fn id(self) -> AstId<Self::Kind> {
        AstId::new(self.syntax().id())
    }
}

/// A view type with its lifetime erased: `FnDecl<'static>` names the
/// `FnDecl` view in an [`AstId`], and [`AstKind::View`] gives it back for
/// any tree lifetime.
pub trait AstKind: 'static {
    /// The view for trees borrowed for `'t`.
    type View<'t>: AstNode<'t>;
}

/// A typed view of a token.
pub trait AstToken<'t>: Sized + Copy {
    /// Whether a token of `kind` has this view.
    fn can_cast(kind: SyntaxKind) -> bool;

    /// The view of `token`, if its kind has this view.
    fn cast(token: SyntaxToken<'t>) -> Option<Self>;

    /// The underlying token.
    fn syntax(self) -> SyntaxToken<'t>;

    /// The token's text.
    fn text(self) -> &'t str {
        self.syntax().text()
    }

    /// The token's span.
    fn span(self) -> Span {
        self.syntax().span()
    }
}

/// A typed handle to a node, which later phases store instead of a
/// borrowed view. `K` is the view with its lifetime erased, such as
/// `AstId<FnDecl<'static>>`.
///
/// An id is meaningful only for the tree it was taken from (node ids are
/// stable for the life of a tree, §3.6 of the phase-A design).
pub struct AstId<K> {
    id: NodeId,
    kind: PhantomData<fn() -> K>,
}

impl<K> AstId<K> {
    const fn new(id: NodeId) -> Self {
        Self {
            id,
            kind: PhantomData,
        }
    }

    /// The untyped node id.
    #[must_use]
    pub const fn node_id(self) -> NodeId {
        self.id
    }
}

impl<K: AstKind> AstId<K> {
    /// The view of this node in `tree`. `None` if `tree` has no such node
    /// or it is not of this view's kind, which happens only when the id
    /// comes from another tree.
    #[must_use]
    pub fn get<'t>(self, tree: &'t SyntaxTree) -> Option<K::View<'t>> {
        tree.node(self.id)
            .and_then(<K::View<'t> as AstNode<'t>>::cast)
    }
}

impl<K> Clone for AstId<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for AstId<K> {}

impl<K> PartialEq for AstId<K> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<K> Eq for AstId<K> {}

impl<K> PartialOrd for AstId<K> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<K> Ord for AstId<K> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

impl<K> Hash for AstId<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<K> fmt::Debug for AstId<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AstId({})", self.id.index())
    }
}

/// The child nodes of a node that have view `N`, in order.
pub struct AstChildren<'t, N> {
    inner: Children<'t>,
    view: PhantomData<fn() -> N>,
}

impl<'t, N: AstNode<'t>> AstChildren<'t, N> {
    fn new(parent: SyntaxNode<'t>) -> Self {
        Self {
            inner: parent.children_with_tokens(),
            view: PhantomData,
        }
    }
}

impl<'t, N: AstNode<'t>> Iterator for AstChildren<'t, N> {
    type Item = N;

    fn next(&mut self) -> Option<N> {
        self.inner
            .by_ref()
            .find_map(|child| child.as_node().and_then(N::cast))
    }
}

impl<N> Clone for AstChildren<'_, N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            view: PhantomData,
        }
    }
}

impl<N> fmt::Debug for AstChildren<'_, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AstChildren").finish_non_exhaustive()
    }
}

/// The child tokens of a node that have view `T`, in order.
pub struct AstTokens<'t, T> {
    inner: Children<'t>,
    view: PhantomData<fn() -> T>,
}

impl<'t, T: AstToken<'t>> AstTokens<'t, T> {
    fn new(parent: SyntaxNode<'t>) -> Self {
        Self {
            inner: parent.children_with_tokens(),
            view: PhantomData,
        }
    }
}

impl<'t, T: AstToken<'t>> Iterator for AstTokens<'t, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.inner
            .by_ref()
            .find_map(|child| child.as_token().and_then(T::cast))
    }
}

impl<T> Clone for AstTokens<'_, T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            view: PhantomData,
        }
    }
}

impl<T> fmt::Debug for AstTokens<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AstTokens").finish_non_exhaustive()
    }
}

/// The first child node of `parent` with view `N`.
fn child<'t, N: AstNode<'t>>(parent: SyntaxNode<'t>) -> Option<N> {
    AstChildren::new(parent).next()
}

/// The child nodes of `parent` with view `N`.
fn children<'t, N: AstNode<'t>>(parent: SyntaxNode<'t>) -> AstChildren<'t, N> {
    AstChildren::new(parent)
}

/// The first child token of `parent` with view `T`.
fn child_token<'t, T: AstToken<'t>>(parent: SyntaxNode<'t>) -> Option<T> {
    AstTokens::new(parent).next()
}

/// The first child token of `parent` of `kind`.
fn token(parent: SyntaxNode<'_>, kind: SyntaxKind) -> Option<SyntaxToken<'_>> {
    parent.first_token_of_kind(kind)
}

/// The first child node of `parent` with view `N` that comes after the
/// child token of `kind`. `None` if there is no such token.
fn child_after<'t, N: AstNode<'t>>(parent: SyntaxNode<'t>, kind: SyntaxKind) -> Option<N> {
    let mut seen = false;
    parent.children_with_tokens().find_map(|child| match child {
        SyntaxElement::Token(token) if token.kind() == kind => {
            seen = true;
            None
        }
        SyntaxElement::Node(node) if seen => N::cast(node),
        _ => None,
    })
}

/// The first child node of `parent` with view `N` that comes before the
/// first child token of `kind` (or anywhere, if there is no such token).
fn child_before<'t, N: AstNode<'t>>(parent: SyntaxNode<'t>, kind: SyntaxKind) -> Option<N> {
    parent
        .children_with_tokens()
        .take_while(|child| child.kind() != kind)
        .find_map(|child| child.as_node().and_then(N::cast))
}

/// The doc comments of a doc-attached node: the `///` tokens among its
/// leading trivia, before its first attribute or significant token (§2.2,
/// App. A.2). Misplaced doc comments are not included (E0017).
fn leading_docs(node: SyntaxNode<'_>) -> impl Iterator<Item = DocComment<'_>> {
    node.children_with_tokens()
        .map_while(|child| match child {
            SyntaxElement::Token(token)
                if token.kind().is_trivia() || token.kind() == SyntaxKind::NEWLINE =>
            {
                Some(token)
            }
            _ => None,
        })
        .filter_map(DocComment::cast)
}

/// The range from the first to the last significant token of `node`.
fn significant_span(node: SyntaxNode<'_>) -> Span {
    let significant =
        |t: &SyntaxToken<'_>| !t.kind().is_trivia() && t.kind() != SyntaxKind::NEWLINE;
    let mut tokens = node.tokens().filter(significant);
    let Some(first) = tokens.next() else {
        return node.span();
    };
    let last = tokens.next_back().unwrap_or(first);
    Span::new(
        node.tree().file(),
        TextRange::new(first.text_range().start(), last.text_range().end()),
    )
}

#[cfg(test)]
pub(crate) mod tests;
