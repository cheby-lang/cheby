//! Blocks and statements (§5.1, §5.2).

use cheby_iface::{Ice, Ty, display};
use cheby_syntax::ast::{AstNode, Block, LetStmt, Stmt};

use super::{BodyCx, Expected};
use crate::gate::{slice_type, unsupported_type};
use crate::infer::{InferTy, contains_error};
use crate::lower_ty::TyLowering;
use crate::thir::{self, ExprId, ExprKind};

impl BodyCx<'_> {
    /// A block. Its value is its last statement when that is an
    /// expression, checked against `expected`; otherwise `Nil` (§5.1).
    pub(crate) fn block(
        &mut self,
        block: Block<'_>,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        let stmts: Vec<Stmt<'_>> = block.stmts().collect();
        let tail = match stmts.last() {
            Some(Stmt::Expr(e)) => Some(*e),
            _ => None,
        };
        let leading = if tail.is_some() {
            stmts.len().saturating_sub(1)
        } else {
            stmts.len()
        };
        let mut out = Vec::new();
        for stmt in stmts.iter().take(leading) {
            if let Some(stmt) = self.stmt(*stmt)? {
                out.push(stmt);
            }
        }
        let (tail, ty) = if let Some(e) = tail {
            let id = self.check_expr(e, expected)?;
            let ty = self.ty_of(id)?;
            (Some(id), ty)
        } else {
            // The value `Nil`, at the last statement or the empty block.
            let span = stmts
                .last()
                .map_or_else(|| block.significant_span(), |s| s.significant_span());
            let ty = self.coerce(&InferTy::NIL, expected, span)?;
            (None, ty)
        };
        self.push(
            ExprKind::Block {
                stmts: out.into(),
                tail,
            },
            ty,
            block.significant_span(),
        )
    }

    /// A statement that is not a block's value. `None` if it was rejected.
    fn stmt(&mut self, stmt: Stmt<'_>) -> Result<Option<thir::Stmt>, Ice> {
        let span = stmt.significant_span();
        let what = match stmt {
            Stmt::Let(l) if l.is_assert() => "`let assert` statements",
            Stmt::Let(l) => return self.let_stmt(l),
            Stmt::Expr(e) => {
                let id = self.check_expr(e, None)?;
                return Ok(Some(thir::Stmt::Expr(id)));
            }
            Stmt::Use(_) => "`use` statements",
            Stmt::Assert(_) => "`assert` statements",
            Stmt::LocalFn(_) => "local functions",
        };
        self.unsupported(span, what)?;
        Ok(None)
    }

    /// `let p = e` and `let p: T = e` (§5.2). The value is checked first,
    /// against the annotation if there is one; then the pattern binds.
    fn let_stmt(&mut self, stmt: LetStmt<'_>) -> Result<Option<thir::Stmt>, Ice> {
        let annotation = match stmt.ty() {
            Some(written) => {
                let lowering = TyLowering {
                    input: self.input,
                    paths: &self.res.paths,
                };
                let mut diags = Vec::new();
                let ty = lowering.lower(written, &mut diags)?;
                for diag in diags {
                    self.report(diag);
                }
                let span = written.significant_span();
                let ty = if contains_error(&self.input.globals.types, ty) {
                    self.errored = true;
                    Ty::ERROR
                } else if slice_type(ty) {
                    ty
                } else {
                    let cx = self.input.display_cx(self.own, Some(&self.generics));
                    let shown = display(ty, &cx);
                    self.report(unsupported_type(span, &shown));
                    Ty::ERROR
                };
                Some(Expected::new(
                    InferTy::Known(ty),
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
        let Some(pat) = stmt.pattern() else {
            self.errored = true;
            return Ok(None);
        };
        let pat = self.let_pattern(pat, ty)?;
        Ok(pat.map(|pat| thir::Stmt::Let { pat, init }))
    }
}
