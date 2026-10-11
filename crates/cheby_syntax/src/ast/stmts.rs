//! Views of blocks and statements (App. A.4, §5.1, §5.2, §5.8, §5.10).

use super::{
    AstChildren, Expr, Ident, Param, ParamList, Pat, RetType, Type, TypeParamList, Visibility,
    child, child_after, child_token, children, token,
};
use crate::{SyntaxKind, SyntaxNode, SyntaxToken};

ast_nodes! {
    /// A block `{ stmt … }`, also an expression (§5.1).
    Block = BLOCK;
    /// `let p = e` or `let assert p = e as msg` (§5.2, §11.4).
    LetStmt = LET_STMT;
    /// `use a, b <- f(x)` (§5.10).
    UseStmt = USE_STMT;
    /// One binder of a `use`: a pattern with an optional type.
    UseBinder = USE_BINDER;
    /// `assert cond as msg` (D-160).
    AssertStmt = ASSERT_STMT;
    /// A local named function (§5.8).
    LocalFn = LOCAL_FN;
    /// `as expr`, the message of `let assert`, `assert`, `panic` and
    /// `todo` (D-159, D-160, D-161).
    Message = MESSAGE;
}

ast_enum! {
    /// A statement of a block (App. A.4).
    Stmt {
        Let(LetStmt),
        Use(UseStmt),
        Assert(AssertStmt),
        LocalFn(LocalFn),
        Expr(Expr),
    }
}

impl<'t> Block<'t> {
    /// The statements, in order.
    #[must_use]
    pub fn stmts(self) -> AstChildren<'t, Stmt<'t>> {
        children(self.0)
    }
}

impl<'t> LetStmt<'t> {
    /// The `assert` of `let assert`, if written.
    #[must_use]
    pub fn assert_keyword(self) -> Option<SyntaxToken<'t>> {
        token(self.0, SyntaxKind::ASSERT_KW)
    }

    /// Whether this is `let assert` (§11.4).
    #[must_use]
    pub fn is_assert(self) -> bool {
        self.assert_keyword().is_some()
    }

    /// The pattern.
    #[must_use]
    pub fn pattern(self) -> Option<Pat<'t>> {
        child(self.0)
    }

    /// The written type, if any.
    #[must_use]
    pub fn ty(self) -> Option<Type<'t>> {
        child(self.0)
    }

    /// The value after `=`.
    #[must_use]
    pub fn value(self) -> Option<Expr<'t>> {
        child(self.0)
    }

    /// The `as` message. Allowed only with `assert` (E0013).
    #[must_use]
    pub fn message(self) -> Option<Message<'t>> {
        child(self.0)
    }
}

impl<'t> UseStmt<'t> {
    /// The binders, in order.
    #[must_use]
    pub fn binders(self) -> AstChildren<'t, UseBinder<'t>> {
        children(self.0)
    }

    /// The `<-` arrow.
    #[must_use]
    pub fn arrow(self) -> Option<SyntaxToken<'t>> {
        token(self.0, SyntaxKind::LEFT_ARROW)
    }

    /// The call or function after `<-`.
    #[must_use]
    pub fn value(self) -> Option<Expr<'t>> {
        child_after(self.0, SyntaxKind::LEFT_ARROW)
    }
}

impl<'t> UseBinder<'t> {
    /// The pattern.
    #[must_use]
    pub fn pattern(self) -> Option<Pat<'t>> {
        child(self.0)
    }

    /// The written type, if any.
    #[must_use]
    pub fn ty(self) -> Option<Type<'t>> {
        child(self.0)
    }
}

impl<'t> AssertStmt<'t> {
    /// The condition.
    #[must_use]
    pub fn condition(self) -> Option<Expr<'t>> {
        child(self.0)
    }

    /// The `as` message, if written.
    #[must_use]
    pub fn message(self) -> Option<Message<'t>> {
        child(self.0)
    }
}

impl<'t> LocalFn<'t> {
    /// `pub` or `priv`, which a local function may not have (E0013).
    #[must_use]
    pub fn visibility(self) -> Option<Visibility<'t>> {
        child(self.0)
    }

    /// The function's name.
    #[must_use]
    pub fn name(self) -> Option<Ident<'t>> {
        child_token(self.0)
    }

    /// Type parameters, which a local function may not have (E0013).
    #[must_use]
    pub fn type_params(self) -> Option<TypeParamList<'t>> {
        child(self.0)
    }

    /// The `(…)` parameter list.
    #[must_use]
    pub fn param_list(self) -> Option<ParamList<'t>> {
        child(self.0)
    }

    /// The parameters, in order. Empty if the list is missing.
    pub fn params(self) -> impl Iterator<Item = Param<'t>> + 't {
        self.param_list().into_iter().flat_map(ParamList::params)
    }

    /// The `-> T` return type, if written.
    #[must_use]
    pub fn ret_type(self) -> Option<RetType<'t>> {
        child(self.0)
    }

    /// The body. Required (E0013).
    #[must_use]
    pub fn body(self) -> Option<Block<'t>> {
        child(self.0)
    }
}

impl<'t> Message<'t> {
    /// The message expression, a `String` (D-159).
    #[must_use]
    pub fn value(self) -> Option<Expr<'t>> {
        child(self.0)
    }
}
