//! Patterns (D3): `case`, `let`, `let assert` and parameter patterns, as
//! decision trees (chapter 6, §5.9, §11.4).
//!
//! Each construct evaluates its subjects into locals, compiles its
//! patterns into a decision tree ([`tree`], which documents the column
//! heuristic) and lowers the tree ([`emit`]):
//!
//! - `case`: one row per arm; the arms' bodies join with the `case`'s
//!   value, and inherit its tail position (§5.15.2). A value no arm
//!   matches is impossible after the exhaustiveness check (§6.9) and is
//!   `unreachable`, never a panic.
//! - `let` and parameters (D-242): one irrefutable row; the names are
//!   bound for what follows.
//! - `let assert`: one row; a value it does not match panics with
//!   `` `let assert` pattern did not match `` (`: m` with `as m`) and a
//!   `  value: <debug text>` line (D-528), located at the pattern.

mod emit;
#[cfg(test)]
mod tests;
pub(crate) mod tree;

use cheby_diag::Span;
use cheby_iface::{Ice, Ty};
use cheby_types::thir::{self, Arm, ExprId, ExprKind, PatKind};

use crate::ir::{Local, Operand, Rvalue};
use crate::lower::body::BodyCx;
use crate::lower::pat::emit::{ArmPlan, OnFail, Then};
use crate::lower::pat::tree::{Input, bound_locals, decide};
use crate::repr::Repr;

impl BodyCx<'_, '_> {
    /// `case subjects { arms }` of type `ty`; `tail` marks §5.15.2 tail
    /// position, which the arms' bodies inherit.
    pub(super) fn case(
        &mut self,
        subjects: &[ExprId],
        arms: &[Arm],
        ty: Ty,
        span: Span,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        // Subjects left to right (§5.9.1).
        let mut locals = Vec::with_capacity(subjects.len());
        let mut tys = Vec::with_capacity(subjects.len());
        for &s in subjects {
            let Some(op) = self.expr(s, false)? else {
                return Ok(None);
            };
            let sty = self.ty(s)?;
            locals.push(self.as_local(op, self.repr_of(sty))?);
            tys.push(sty);
        }
        let input = Input {
            subjects: tys,
            rows: arms
                .iter()
                .enumerate()
                .map(|(i, a)| (a.pats.iter().collect(), i))
                .collect(),
            guarded: arms.iter().map(|a| a.guard.is_some()).collect(),
        };
        let decision = decide(input, &self.cx.defs)?;
        let plans = arms
            .iter()
            .map(|a| ArmPlan {
                guard: a.guard,
                then: Then::Body {
                    body: a.body,
                    tail,
                    ty,
                },
                locals: bound_locals(a.pats.iter()),
                span: a.pats.first().map(|p| p.span),
            })
            .collect();
        self.emit_decision(&decision, &locals, plans, OnFail::Unreachable, span)
    }

    /// `let pat = init`. `None` means `init` diverged.
    pub(super) fn let_stmt(&mut self, pat: &thir::Pat, init: ExprId) -> Result<Option<()>, Ice> {
        let Some(op) = self.expr(init, false)? else {
            return Ok(None);
        };
        let init_ty = self.ty(init)?;
        match &pat.kind {
            PatKind::Bind(id) => {
                let ty = self.local_ty(*id)?;
                let op = self.to_repr(op, self.repr_of(init_ty), self.repr_of(ty), init_ty)?;
                self.bind_local(*id, op)?;
                Ok(Some(()))
            }
            // The value is unused; the RC pass drops it. `let _ = x` of a
            // named counted value is a use of `x` (D-205, §9.2 guarantee 3:
            // a program keeps a handle alive by using it again), so the
            // value moves into a fresh local here, which the RC pass then
            // drops: `x`'s last use is this statement, not an earlier one
            // (D-736).
            PatKind::Wild => {
                let named = matches!(self.expr_data(init)?.kind, ExprKind::Local(_));
                if named && matches!(op, Operand::Local(_)) && self.repr_of(init_ty) == Repr::Value
                {
                    self.define(Repr::Value, Rvalue::Use(op))?;
                }
                Ok(Some(()))
            }
            _ => {
                let value = self.as_local(op, self.repr_of(init_ty))?;
                self.destructure(pat, value, init_ty, OnFail::Unreachable)
            }
        }
    }

    /// `let assert pat = init as message` (§11.4). `None` means it
    /// diverged.
    pub(super) fn let_assert(
        &mut self,
        pat: &thir::Pat,
        init: ExprId,
        message: Option<ExprId>,
    ) -> Result<Option<()>, Ice> {
        let Some(op) = self.expr(init, false)? else {
            return Ok(None);
        };
        let ty = self.ty(init)?;
        let value = self.as_local(op, self.repr_of(ty))?;
        // The report's location is the start of the pattern that did not
        // match (D-570).
        let on_fail = OnFail::LetAssert {
            value,
            ty,
            message,
            at: pat.span,
        };
        self.destructure(pat, value, ty, on_fail)
    }

    /// Binds a closure, local-function, `use`, capture or pipe parameter
    /// pattern (irrefutable, D-242) to `value`, a uniform word (D-453).
    pub(super) fn bind_param(&mut self, pat: &thir::Pat, value: Operand) -> Result<(), Ice> {
        let op = self.from_uniform(value, pat.ty)?;
        match &pat.kind {
            PatKind::Bind(id) => self.bind_local(*id, op),
            // The RC pass drops the unused value.
            PatKind::Wild => Ok(()),
            _ => {
                let value = self.as_local(op, self.repr_of(pat.ty))?;
                self.destructure(pat, value, pat.ty, OnFail::Unreachable)?
                    .ok_or_else(|| Ice::new("an irrefutable parameter pattern diverged"))
            }
        }
    }

    /// Matches `value` (of type `ty`) against `pat` and binds its names
    /// for what follows. `None` means no path goes on.
    fn destructure(
        &mut self,
        pat: &thir::Pat,
        value: Local,
        ty: Ty,
        on_fail: OnFail,
    ) -> Result<Option<()>, Ice> {
        let input = Input {
            subjects: vec![ty],
            rows: vec![(vec![pat], 0)],
            guarded: vec![false],
        };
        let decision = decide(input, &self.cx.defs)?;
        let plan = ArmPlan {
            guard: None,
            then: Then::Continue,
            locals: bound_locals([pat]),
            span: Some(pat.span),
        };
        let done = self.emit_decision(&decision, &[value], vec![plan], on_fail, pat.span)?;
        Ok(done.map(|_| ()))
    }
}
