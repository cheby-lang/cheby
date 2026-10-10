//! `panic`, `todo` and `assert` (D9, §5.14, §11.5, §11.6, D-096, D-528).
//!
//! `panic` and `todo` end the block with [`Terminator::Panic`]; the
//! runtime supplies the default messages and the `not implemented: `
//! prefix of `todo as m` (D-449). A failed `assert e` panics with
//! `assertion failed: <source text of e>`, or `assertion failed: <m>`
//! with `as m`. When `e` is a comparison, both operands are evaluated
//! once, compared, and on failure their debug text follows on the lines
//! `  left: …` and ` right: …` (D-528). The message is built only on
//! failure, and `m` is evaluated only then (D-160).
//!
//! THIR keeps no span for the `assert` statement itself, so a failed
//! assertion reports the location of its condition, GAP(assert-location).

use cheby_diag::Span;
use cheby_iface::{Ice, Ty};
use cheby_types::thir::{ExprId, ExprKind, PanicKind};

use crate::ir::{Const, Local, Operand, RuntimeFn, Terminator};
use crate::lower::body::BodyCx;
use crate::lower::scalar::{BinaryOperands, is_comparison};
use crate::repr::Repr;

impl BodyCx<'_, '_> {
    /// `panic` or `todo` with an optional message, of type `ty`. It never
    /// has a value, so the result is `None`.
    pub(super) fn panic_expr(
        &mut self,
        kind: PanicKind,
        message: Option<ExprId>,
        _ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let kind = match kind {
            PanicKind::Todo => crate::ir::PanicKind::Todo,
            PanicKind::Panic => crate::ir::PanicKind::Explicit,
        };
        let message = match message {
            Some(m) => match self.expr_as(m, Repr::Value)? {
                Some(op) => Some(op),
                None => return Ok(None),
            },
            None => None,
        };
        let loc = self.location_id(span)?;
        self.terminate(Terminator::Panic { kind, message, loc })?;
        Ok(None)
    }

    /// `assert cond as message` (D-096, D-160, D-528). `None` means it
    /// diverged.
    pub(super) fn assert_stmt(
        &mut self,
        cond: ExprId,
        message: Option<ExprId>,
    ) -> Result<Option<()>, Ice> {
        let e = self.expr_data(cond)?;
        let span = e.span;
        let (ok, shown) = match &e.kind {
            ExprKind::Binary { op, lhs, rhs, imp } if is_comparison(*op) => {
                let operand_ty = self.operand_ty(*lhs, *rhs)?;
                let Some((l, r)) = self.operands(*lhs, *rhs, operand_ty)? else {
                    return Ok(None);
                };
                let operands = BinaryOperands {
                    lhs: l.clone(),
                    rhs: r.clone(),
                    ty: operand_ty,
                };
                let Some(ok) = self.apply_binary(*op, operands, imp, Ty::BOOL, span, false)? else {
                    return Ok(None);
                };
                (ok, Some((l, r, operand_ty)))
            }
            _ => {
                let Some(ok) = self.expr_as(cond, Repr::Bool)? else {
                    return Ok(None);
                };
                (ok, None)
            }
        };
        let pass = self.new_block()?;
        let fail = self.new_block()?;
        self.terminate(Terminator::Branch {
            cond: ok,
            then_: pass,
            else_: fail,
        })?;
        self.switch_to(fail)?;
        if let Some(text) = self.assert_failure(span, message, shown)? {
            let loc = self.location_id(span)?;
            self.terminate(Terminator::Panic {
                kind: crate::ir::PanicKind::Assert,
                message: Some(Operand::Local(text)),
                loc,
            })?;
        }
        self.switch_to(pass)?;
        Ok(Some(()))
    }

    /// Builds the message of a failed assertion in the current block.
    /// `None` if evaluating `message` diverged.
    fn assert_failure(
        &mut self,
        span: Span,
        message: Option<ExprId>,
        shown: Option<(Operand, Operand, Ty)>,
    ) -> Result<Option<Local>, Ice> {
        let capacity = Operand::Const(Const::Int(ASSERT_CAPACITY));
        let mut acc = self.runtime(RuntimeFn::StrWithCapacity, vec![capacity])?;
        if let Some(m) = message {
            acc = self.append_text(acc, "assertion failed: ")?;
            let Some(text) = self.expr_as(m, Repr::Value)? else {
                return Ok(None);
            };
            acc = self.append(acc, text)?;
        } else {
            let source = self.source_text(span)?;
            acc = self.append_text(acc, &format!("assertion failed: {source}"))?;
        }
        if let Some((l, r, ty)) = shown {
            acc = self.append_text(acc, "\n  left: ")?;
            let left = self.debug_text(l, ty)?;
            acc = self.append(acc, left)?;
            acc = self.append_text(acc, "\n right: ")?;
            let right = self.debug_text(r, ty)?;
            acc = self.append(acc, right)?;
        }
        Ok(Some(acc))
    }

    /// The debug text (§3.13) of `op`, a value of type `ty` in its type's
    /// representation, as a new `String`.
    pub(super) fn debug_text(&mut self, op: Operand, ty: Ty) -> Result<Operand, Ice> {
        let value = self.to_uniform(op, ty)?;
        Ok(Operand::Local(self.runtime(RuntimeFn::Debug, vec![value])?))
    }

    /// `acc` (an owned `String`) with `piece` (a `String`) appended.
    pub(super) fn append(&mut self, acc: Local, piece: Operand) -> Result<Local, Ice> {
        self.runtime(RuntimeFn::StrAppend, vec![Operand::Local(acc), piece])
    }

    /// `acc` (an owned `String`) with the text `piece` appended.
    pub(super) fn append_text(&mut self, acc: Local, piece: &str) -> Result<Local, Ice> {
        let piece = self.string_const(piece)?;
        self.append(acc, piece)
    }

    /// The result of the runtime helper `func`, which must have one.
    fn runtime(&mut self, func: RuntimeFn, args: Vec<Operand>) -> Result<Local, Ice> {
        self.b
            .call_runtime(func, args)?
            .ok_or_else(|| Ice::new(format!("runtime helper {} has no result", func.name())))
    }
}

/// The initial capacity of an assertion message: enough for most first
/// lines and short operands; longer ones grow it.
const ASSERT_CAPACITY: i64 = 64;
