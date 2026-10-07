//! Constraints resolved at the end of the top-level body (§3.12.4, D-234,
//! D-237): operators whose meaning depends on the operand type,
//! interface-qualified calls, the `Show` requirement of interpolation,
//! conversions to `dyn` and the bounds of generic callees. They are
//! recorded while checking, with a placeholder in the THIR they complete,
//! and resolved in recorded order after literal defaulting and zonking,
//! through the `crate::ifaces` API.
//!
//! Resolving a constraint never changes a type (interfaces have no type
//! parameters and names are not overloaded, D-045, D-165), so the order
//! only decides the order of diagnostics. A constraint whose type still
//! contains an error or an unknown part is skipped: the error, or the
//! E0057 of the unknown part, is already reported.

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{DefId, Ice, IndexVec, Ty};

use super::BodyCx;
use crate::ifaces::{self, IfaceCx, Need, Unsatisfied};
use crate::infer::{InferTy, contains_error, is_float, is_integer};
use crate::thir::{BinOpKind, Callee, Dict, Expr, ExprId, ExprKind, InterpPart, OpImpl};

/// A constraint whose resolution waits for the end of the body.
#[derive(Clone, Debug)]
pub(crate) enum Deferred {
    /// A binary operator other than `==`, `!=`, `&&` and `||` on operands
    /// of type `operand`; `expr` is the [`ExprKind::Binary`] whose `imp`
    /// is filled in. `%`, the bitwise operators and the shifts are checked
    /// here (E0072); the others go through [`ifaces::operator`].
    Operator {
        expr: ExprId,
        op: BinOpKind,
        operand: InferTy,
    },
    /// Unary `-` on `operand`; `expr` is the [`ExprKind::Neg`].
    Negation { expr: ExprId, operand: InferTy },
    /// `{x}` in the interpolation `expr`, part `part`, of type `ty`: its
    /// `show` callee.
    Show {
        expr: ExprId,
        part: usize,
        ty: InferTy,
    },
    /// The interface call `expr` of `iface_fn` with `Self` = `self_ty`:
    /// its callee, which is a placeholder until then.
    IfaceCall {
        expr: ExprId,
        iface_fn: DefId,
        self_ty: InferTy,
    },
    /// The conversion `expr` ([`ExprKind::DynCoerce`]) of a value of type
    /// `from` to `dyn iface`: its dictionary.
    DynCoerce {
        expr: ExprId,
        iface: DefId,
        from: InferTy,
    },
    /// Bound `iface` of a generic callee's type parameter, instantiated to
    /// `ty`: dictionary `slot` of the [`Callee::Fn`] of the call `expr`,
    /// or of the [`ExprKind::FnRef`] `expr` (plan §4.3 order: for each
    /// type parameter, for each of its bounds).
    Bound {
        expr: ExprId,
        slot: usize,
        ty: InferTy,
        iface: DefId,
        callee: DefId,
    },
}

impl BodyCx<'_> {
    /// Records `d` for resolution at the end of the body.
    pub fn defer(&mut self, d: Deferred) {
        self.deferred.push(d);
    }

    /// Resolves every recorded constraint into `exprs`, the zonked THIR.
    pub(super) fn resolve_deferred(
        &mut self,
        exprs: &mut IndexVec<ExprId, Expr>,
    ) -> Result<(), Ice> {
        let deferred = std::mem::take(&mut self.deferred);
        for d in deferred {
            self.resolve_one(exprs, d)?;
        }
        Ok(())
    }

    /// The closed type of `t`, or `None` if it has an error or an unknown
    /// part, both already reported.
    fn closed(&self, t: &InferTy) -> Result<Option<Ty>, Ice> {
        let mut unresolved = Vec::new();
        let ty = self.infcx.zonk(t, &mut unresolved)?;
        let types = &self.input.globals.types;
        Ok((unresolved.is_empty() && !contains_error(types, ty)).then_some(ty))
    }

    fn resolve_one(&mut self, exprs: &mut IndexVec<ExprId, Expr>, d: Deferred) -> Result<(), Ice> {
        match d {
            Deferred::Operator { expr, op, operand } => {
                self.resolve_operator(exprs, expr, op, &operand)
            }
            Deferred::Negation { expr, operand } => self.resolve_negation(exprs, expr, &operand),
            Deferred::Show { expr, part, ty } => self.resolve_show(exprs, expr, part, &ty),
            Deferred::IfaceCall {
                expr,
                iface_fn,
                self_ty,
            } => self.resolve_iface_call(exprs, expr, iface_fn, &self_ty),
            Deferred::DynCoerce { expr, iface, from } => {
                self.resolve_dyn(exprs, expr, iface, &from)
            }
            Deferred::Bound {
                expr,
                slot,
                ty,
                iface,
                callee,
            } => self.resolve_bound(exprs, expr, slot, &ty, iface, callee),
        }
    }

    /// Reports the E0063 or E0065 of `err` at `span`.
    fn unsatisfied(&mut self, err: &Unsatisfied, span: Span, need: Need) {
        let cx = IfaceCx {
            input: self.input,
            own: self.own,
            generics: &self.generics,
        };
        let diag = ifaces::unsatisfied_diagnostic(&cx, err, span, need);
        self.report(diag);
    }

    fn resolve_operator(
        &mut self,
        exprs: &mut IndexVec<ExprId, Expr>,
        expr: ExprId,
        op: BinOpKind,
        operand: &InferTy,
    ) -> Result<(), Ice> {
        let Some(ty) = self.closed(operand)? else {
            return Ok(());
        };
        let span = exprs.try_get(expr, "operator")?.span;
        let overloadable = !matches!(
            op,
            BinOpKind::Rem
                | BinOpKind::BitOr
                | BinOpKind::BitXor
                | BinOpKind::BitAnd
                | BinOpKind::Shl
                | BinOpKind::Shr
        );
        let imp = if !overloadable {
            let defined = if op == BinOpKind::Rem {
                is_integer(ty) || is_float(ty)
            } else {
                is_integer(ty)
            };
            if !defined && ty != Ty::NEVER {
                let diag = self.invalid_operand(op, ty, span);
                self.report(diag);
                return Ok(());
            }
            OpImpl::Builtin
        } else if is_integer(ty) || is_float(ty) || ty == Ty::NEVER {
            // The built-in numeric operations, and `Never` operands, which
            // never produce a value.
            OpImpl::Builtin
        } else {
            let cx = IfaceCx {
                input: self.input,
                own: self.own,
                generics: &self.generics,
            };
            match ifaces::operator(&cx, op, ty)? {
                Ok(imp) => imp,
                Err(err) => {
                    self.unsatisfied(&err, span, Need::Operator(op));
                    return Ok(());
                }
            }
        };
        let ExprKind::Binary { imp: slot, .. } = &mut expr_mut(exprs, expr, "operator")?.kind
        else {
            return Err(Ice::new("an operator constraint on a non-operator"));
        };
        *slot = imp;
        Ok(())
    }

    fn resolve_negation(
        &mut self,
        exprs: &mut IndexVec<ExprId, Expr>,
        expr: ExprId,
        operand: &InferTy,
    ) -> Result<(), Ice> {
        let Some(ty) = self.closed(operand)? else {
            return Ok(());
        };
        let span = exprs.try_get(expr, "negation")?.span;
        let imp = if is_integer(ty) || is_float(ty) || ty == Ty::NEVER {
            OpImpl::Builtin
        } else {
            let cx = IfaceCx {
                input: self.input,
                own: self.own,
                generics: &self.generics,
            };
            match ifaces::negation(&cx, ty)? {
                Ok(imp) => imp,
                Err(err) => {
                    self.unsatisfied(&err, span, Need::Neg);
                    return Ok(());
                }
            }
        };
        let ExprKind::Neg { imp: slot, .. } = &mut expr_mut(exprs, expr, "negation")?.kind else {
            return Err(Ice::new("a negation constraint on a non-negation"));
        };
        *slot = imp;
        Ok(())
    }

    fn resolve_show(
        &mut self,
        exprs: &mut IndexVec<ExprId, Expr>,
        expr: ExprId,
        part: usize,
        ty: &InferTy,
    ) -> Result<(), Ice> {
        let Some(ty) = self.closed(ty)? else {
            return Ok(());
        };
        let ExprKind::Interpolate { parts } = &exprs.try_get(expr, "interpolation")?.kind else {
            return Err(Ice::new("a `Show` constraint on a non-interpolation"));
        };
        let Some(InterpPart::Value { expr: value, .. }) = parts.get(part) else {
            return Err(Ice::new("a `Show` constraint on a text part"));
        };
        let span = exprs.try_get(*value, "interpolated value")?.span;
        let cx = IfaceCx {
            input: self.input,
            own: self.own,
            generics: &self.generics,
        };
        let callee = match ifaces::show(&cx, ty)? {
            Ok(callee) => callee,
            Err(err) => {
                self.unsatisfied(&err, span, Need::Interpolation);
                return Ok(());
            }
        };
        let ExprKind::Interpolate { parts } = &mut expr_mut(exprs, expr, "interpolation")?.kind
        else {
            return Err(Ice::new("a `Show` constraint on a non-interpolation"));
        };
        let Some(InterpPart::Value { show, .. }) = parts.get_mut(part) else {
            return Err(Ice::new("a `Show` constraint on a text part"));
        };
        *show = Some(callee);
        Ok(())
    }

    fn resolve_iface_call(
        &mut self,
        exprs: &mut IndexVec<ExprId, Expr>,
        expr: ExprId,
        iface_fn: DefId,
        self_ty: &InferTy,
    ) -> Result<(), Ice> {
        let Some(ty) = self.closed(self_ty)? else {
            return Ok(());
        };
        let span = exprs.try_get(expr, "interface call")?.span;
        let cx = IfaceCx {
            input: self.input,
            own: self.own,
            generics: &self.generics,
        };
        let resolved = match ifaces::iface_call(&cx, iface_fn, ty)? {
            Ok(callee) => callee,
            Err(err) => {
                self.unsatisfied(&err, span, Need::IfaceCall { iface_fn });
                return Ok(());
            }
        };
        let ExprKind::Call { callee, .. } = &mut expr_mut(exprs, expr, "interface call")?.kind
        else {
            return Err(Ice::new("an interface-call constraint on a non-call"));
        };
        *callee = resolved;
        Ok(())
    }

    fn resolve_dyn(
        &mut self,
        exprs: &mut IndexVec<ExprId, Expr>,
        expr: ExprId,
        iface: DefId,
        from: &InferTy,
    ) -> Result<(), Ice> {
        let Some(ty) = self.closed(from)? else {
            return Ok(());
        };
        let span = exprs.try_get(expr, "conversion")?.span;
        let cx = IfaceCx {
            input: self.input,
            own: self.own,
            generics: &self.generics,
        };
        let dict = match ifaces::dyn_coerce(&cx, ty, iface)? {
            Ok(dict) => dict,
            Err(err) => {
                self.unsatisfied(&err, span, Need::Dyn);
                return Ok(());
            }
        };
        let ExprKind::DynCoerce { dict: slot, .. } = &mut expr_mut(exprs, expr, "conversion")?.kind
        else {
            return Err(Ice::new("a `dyn` constraint on a non-conversion"));
        };
        *slot = dict;
        Ok(())
    }

    fn resolve_bound(
        &mut self,
        exprs: &mut IndexVec<ExprId, Expr>,
        expr: ExprId,
        slot: usize,
        ty: &InferTy,
        iface: DefId,
        callee: DefId,
    ) -> Result<(), Ice> {
        let Some(ty) = self.closed(ty)? else {
            return Ok(());
        };
        let span = exprs.try_get(expr, "bounded use")?.span;
        let cx = IfaceCx {
            input: self.input,
            own: self.own,
            generics: &self.generics,
        };
        let dict = match ifaces::satisfy(&cx, ty, iface)? {
            Ok(dict) => dict,
            Err(err) => {
                self.unsatisfied(&err, span, Need::Bound { callee });
                return Ok(());
            }
        };
        let (ExprKind::Call {
            callee: Callee::Fn { dicts, .. },
            ..
        }
        | ExprKind::FnRef { dicts, .. }) = &mut expr_mut(exprs, expr, "bounded use")?.kind
        else {
            return Err(Ice::new("a bound constraint on an unbounded use"));
        };
        let entry = dicts
            .get_mut(slot)
            .ok_or_else(|| Ice::new("a bound constraint past the dictionaries"))?;
        *entry = dict;
        Ok(())
    }

    /// E0072: `%`, a bitwise operator or a shift on a type it is not
    /// defined for (§5.4.2, §5.4.6, D-118).
    fn invalid_operand(&self, op: BinOpKind, ty: Ty, span: Span) -> Diagnostic {
        let shown = self.show(&InferTy::Known(ty));
        let text = op_text(op);
        let (defined, help) = match op {
            BinOpKind::Rem => ("numeric types", None),
            BinOpKind::BitAnd if ty == Ty::BOOL => {
                ("integer types", Some("for logical and, write `&&`"))
            }
            BinOpKind::BitOr if ty == Ty::BOOL => {
                ("integer types", Some("for logical or, write `||`"))
            }
            _ => ("integer types", None),
        };
        let mut diag = Diagnostic::new(
            codes::INVALID_OPERAND,
            format!("`{text}` cannot be applied to `{shown}`"),
        )
        .with_primary(span, format!("`{text}` on `{shown}`"))
        .with_note(format!(
            "`{text}` is defined only on the {defined} and cannot be overloaded"
        ));
        if let Some(help) = help {
            diag = diag.with_help(help);
        }
        diag
    }
}

/// `exprs[id]`, mutably.
fn expr_mut<'e>(
    exprs: &'e mut IndexVec<ExprId, Expr>,
    id: ExprId,
    what: &str,
) -> Result<&'e mut Expr, Ice> {
    exprs
        .get_mut(id)
        .ok_or_else(|| Ice::new(format!("no {what} {id:?}")))
}

/// A placeholder dictionary, replaced when its constraint is resolved.
pub(super) fn placeholder_dict(iface: DefId) -> Dict {
    Dict::Impl {
        iface,
        ty: Ty::ERROR,
        fns: Box::new([]),
    }
}

/// The text of `op`.
pub(super) const fn op_text(op: BinOpKind) -> &'static str {
    match op {
        BinOpKind::Or => "||",
        BinOpKind::And => "&&",
        BinOpKind::Eq => "==",
        BinOpKind::Ne => "!=",
        BinOpKind::Lt => "<",
        BinOpKind::Le => "<=",
        BinOpKind::Gt => ">",
        BinOpKind::Ge => ">=",
        BinOpKind::BitOr => "|",
        BinOpKind::BitXor => "^",
        BinOpKind::BitAnd => "&",
        BinOpKind::Shl => "<<",
        BinOpKind::Shr => ">>",
        BinOpKind::Add => "+",
        BinOpKind::Sub => "-",
        BinOpKind::Mul => "*",
        BinOpKind::Div => "/",
        BinOpKind::Rem => "%",
    }
}
