//! Blocks and statements (§5.1, §5.2, §11.4, §11.5).

use cheby_diag::Span;
use cheby_iface::{Ice, Ty};
use cheby_syntax::ast::{AssertStmt, AstNode, Block, LetStmt, Message, Stmt};

use super::expr::string_message;
use super::{BodyCx, Expected, PatSite};
use crate::infer::InferTy;
use crate::thir::{self, ExprId, ExprKind};

impl BodyCx<'_> {
    /// A block. Its value is its last statement when that is an
    /// expression, checked against `expected`, or the value of a `use`;
    /// otherwise `Nil` (§5.1).
    pub(crate) fn block(
        &mut self,
        block: Block<'_>,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        let stmts: Vec<Stmt<'_>> = block.stmts().collect();
        self.block_stmts(&stmts, block.significant_span(), expected)
    }

    /// The statements `stmts` of a block spanning `span`: a whole block,
    /// or the rest of one after a `use`.
    pub(crate) fn block_stmts<'a>(
        &mut self,
        stmts: &'a [Stmt<'a>],
        span: Span,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        let mut out = Vec::new();
        let mut tail = None;
        for (n, stmt) in stmts.iter().enumerate() {
            let last = n.checked_add(1) == Some(stmts.len());
            match *stmt {
                // A `use` consumes the rest of its block (§5.10).
                Stmt::Use(u) => {
                    let rest = stmts.get(n.saturating_add(1)..).unwrap_or_default();
                    let call = self.use_stmt(u, rest, expected)?;
                    tail = Some(self.coerce_expr(call, expected)?);
                    break;
                }
                Stmt::Expr(e) if last => tail = Some(self.check_expr(e, expected)?),
                _ => {
                    if let Some(stmt) = self.stmt(*stmt)? {
                        out.push(stmt);
                    }
                }
            }
        }
        let ty = if let Some(id) = tail {
            self.ty_of(id)?
        } else {
            // The value `Nil`, at the last statement or the empty block.
            let at = stmts.last().map_or(span, |s| s.significant_span());
            self.coerce(&InferTy::NIL, expected, at)?
        };
        self.push(
            ExprKind::Block {
                stmts: out.into(),
                tail,
            },
            ty,
            span,
        )
    }

    /// A statement that is not a block's value. `None` if it was rejected.
    fn stmt(&mut self, stmt: Stmt<'_>) -> Result<Option<thir::Stmt>, Ice> {
        match stmt {
            Stmt::Let(l) => self.let_stmt(l),
            Stmt::Expr(e) => {
                let id = self.check_expr(e, None)?;
                Ok(Some(thir::Stmt::Expr(id)))
            }
            Stmt::Assert(a) => self.assert_stmt(a),
            Stmt::LocalFn(f) => Ok(self.local_fn(f)?.map(thir::Stmt::Expr)),
            Stmt::Use(_) => Err(Ice::new("a `use` statement outside a block's statements")),
        }
    }

    /// `let p = e`, `let p: T = e` (§5.2) and `let assert p = e as m`
    /// (§11.4). The value is checked first, against the annotation if
    /// there is one; then the pattern binds.
    fn let_stmt(&mut self, stmt: LetStmt<'_>) -> Result<Option<thir::Stmt>, Ice> {
        let annotation = match stmt.ty() {
            Some(written) => {
                let span = written.significant_span();
                let ty = self.lower_written_ty(written)?;
                Some(Expected::new(
                    ty,
                    Some((span, "expected because of this annotation".to_owned())),
                ))
            }
            None => None,
        };
        let init = match stmt.value() {
            Some(value) => self.check_expr(value, annotation.as_ref())?,
            // A `let` without a value is a syntax error.
            None => self.error_expr(stmt.significant_span())?,
        };
        let ty = match annotation {
            Some(expected) => expected.ty,
            None => self.ty_of(init)?,
        };
        let Some(pattern) = stmt.pattern() else {
            self.errored = true;
            return Ok(None);
        };
        if !stmt.is_assert() {
            let pat = self.check_pat(pattern, &ty, PatSite::Let)?;
            return Ok(Some(thir::Stmt::Let { pat, init }));
        }
        let pat = self.check_pat(pattern, &ty, PatSite::LetAssert)?;
        let message = self.message(stmt.message().and_then(Message::value))?;
        Ok(Some(thir::Stmt::LetAssert { pat, init, message }))
    }

    /// `assert cond as message` (§11.5, D-096, D-160).
    fn assert_stmt(&mut self, stmt: AssertStmt<'_>) -> Result<Option<thir::Stmt>, Ice> {
        let Some(condition) = stmt.condition() else {
            self.errored = true;
            return Ok(None);
        };
        let bool_ty = Expected::plain(InferTy::Known(Ty::BOOL));
        let cond = self.check_expr(condition, Some(&bool_ty))?;
        let message = self.message(stmt.message().and_then(Message::value))?;
        Ok(Some(thir::Stmt::Assert { cond, message }))
    }

    /// The `as` message of `assert`, `let assert`, `panic` or `todo`: a
    /// `String` (D-159, D-160, D-161).
    fn message(
        &mut self,
        value: Option<cheby_syntax::ast::Expr<'_>>,
    ) -> Result<Option<ExprId>, Ice> {
        match value {
            Some(m) => Ok(Some(self.check_expr(m, Some(&string_message()))?)),
            None => Ok(None),
        }
    }
}
