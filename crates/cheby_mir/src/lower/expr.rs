//! Expression dispatch, blocks and direct calls (D2): evaluation order
//! §5.15.1, tail position §5.15.2.
//!
//! [`BodyCx::expr`] sends each kind of expression to the method of the
//! file that owns it (plan §4.2). What the back end lowers here: locals,
//! blocks, and direct calls of top-level functions ([`BodyCx::call_fn`]),
//! with arguments and results converted between unboxed and uniform
//! representations at generic boundaries (D6, `coerce.rs`).

use cheby_iface::Ice;
use cheby_types::thir::{self, Callee as ThirCallee, ExprId, ExprKind};

use crate::abi::abi_sig;
use crate::ir::{Abi, Callee, Const, Operand, Rvalue, Terminator};
use crate::lower::body::BodyCx;
use crate::repr::Repr;

impl BodyCx<'_, '_> {
    /// Lowers `id`. In tail position (`tail`, §5.15.2) a call becomes the
    /// block's tail call and the result is `None`; `None` also means the
    /// block ended (the rest is unreachable).
    pub(super) fn expr(&mut self, id: ExprId, tail: bool) -> Result<Option<Operand>, Ice> {
        let e = self.expr_data(id)?;
        let (ty, span) = (e.ty, e.span);
        match &e.kind {
            ExprKind::Lit(lit) => self.lit(lit, ty, span),
            ExprKind::Local(l) => self.lookup(*l, ty).map(Some),
            ExprKind::Ctor { ctor, .. } => self.unit_ctor(*ctor, ty, span),
            ExprKind::Call { callee, args } => self.call(callee, args, ty, span, tail),
            ExprKind::Block { stmts, tail: value } => self.block(stmts, *value, span, tail),
            ExprKind::FnRef {
                def,
                ty_args,
                dicts,
            } => self.fn_ref(*def, ty_args, dicts, ty, span),
            ExprKind::ConstRef { def } => self.const_ref(*def, ty, span),
            ExprKind::Closure { params, body } => self.closure(params, *body, ty, span),
            ExprKind::LocalFn {
                local,
                params,
                body,
            } => self.local_fn(*local, params, *body, span),
            ExprKind::SelfRef { local } => self.self_ref(*local, ty, span),
            ExprKind::Case { subjects, arms } => self.case(subjects, arms, ty, span, tail),
            ExprKind::Field {
                base,
                index_per_ctor,
                ..
            } => self.field(*base, index_per_ctor, ty, span),
            ExprKind::Record {
                ctor,
                ty_args,
                fields,
            } => self.record(*ctor, ty_args, fields, ty, span),
            ExprKind::RecordUpdate { base, fields } => self.record_update(*base, fields, ty, span),
            ExprKind::Tuple(elems) => self.tuple(elems, ty, span),
            ExprKind::List { items } => self.list(items, ty, span),
            ExprKind::Binary { op, lhs, rhs, imp } => {
                self.binary(*op, *lhs, *rhs, imp, ty, span, tail)
            }
            ExprKind::Not(operand) => self.not(*operand, ty, span),
            ExprKind::Neg { operand, imp } => self.neg(*operand, imp, ty, span, tail),
            ExprKind::Interpolate { parts } => self.interpolate(parts, ty, span),
            ExprKind::Panic { kind, message } => self.panic_expr(*kind, *message, ty, span),
            ExprKind::DynCoerce { value, iface, dict } => {
                self.dyn_coerce(*value, *iface, dict, ty, span)
            }
            ExprKind::Error => Err(Ice::new(format!(
                "an expression with an error reached MIR lowering at {span:?}"
            ))),
            // A kind added to THIR later is E0038 until it is lowered.
            _ => self.unsupported(span, "this kind of expression", ty),
        }
    }

    /// Lowers `id` (not in tail position) and converts its value to
    /// `repr` (D6).
    pub(super) fn expr_as(&mut self, id: ExprId, repr: Repr) -> Result<Option<Operand>, Ice> {
        let Some(op) = self.expr(id, false)? else {
            return Ok(None);
        };
        let ty = self.ty(id)?;
        self.to_repr(op, self.repr_of(ty), repr, ty).map(Some)
    }

    /// A call, by what it calls.
    fn call(
        &mut self,
        callee: &ThirCallee,
        args: &[ExprId],
        ty: cheby_iface::Ty,
        span: cheby_diag::Span,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        match callee {
            ThirCallee::Fn { def, dicts, .. } => {
                // Dictionaries first (D-453); they are not expressions, so
                // evaluation order is unchanged (§5.15.1).
                let dict_ops = match self.call_dicts(dicts, ty, span)? {
                    Ok(ops) => ops,
                    Err(early) => return Ok(early),
                };
                let sig = abi_sig(self.cx.defs.fn_sig(*def)?, self.cx.defs.globals);
                let want = sig.params.get(dicts.len()..).unwrap_or_default();
                if want.len() != args.len() {
                    return Err(Ice::new(format!(
                        "call of {def:?} with {} arguments for {} parameters",
                        args.len(),
                        want.len()
                    )));
                }
                // Arguments left to right (§5.15.1), each converted to the
                // callee's parameter representation.
                let mut ops = Vec::with_capacity(args.len());
                for (&arg, &repr) in args.iter().zip(want) {
                    let Some(op) = self.expr_as(arg, repr)? else {
                        return Ok(None);
                    };
                    ops.push(op);
                }
                self.call_fn(*def, dict_ops, ops, ty, tail)
            }
            ThirCallee::Ctor { ctor, ty_args } => self.ctor_call(*ctor, ty_args, args, ty, span),
            ThirCallee::Iface { iface_fn, dict } => {
                self.call_iface(*iface_fn, dict, args, ty, span, tail)
            }
            ThirCallee::Dyn { iface_fn } => self.call_dyn(*iface_fn, args, ty, span, tail),
            ThirCallee::Value(callee) => self.call_value(*callee, args, ty, span, tail),
            _ => self.unsupported(span, "this kind of call", ty),
        }
    }

    /// The dictionary objects of a call at `span` whose result has type
    /// `ty` (D-453), or `Err` with what the call evaluates to instead:
    /// `None` if a dictionary diverged, a placeholder if one is not
    /// lowered (reported at the call, so the arguments are not lowered
    /// and nothing inside them is reported).
    pub(super) fn call_dicts(
        &mut self,
        dicts: &[thir::Dict],
        ty: cheby_iface::Ty,
        span: cheby_diag::Span,
    ) -> Result<Result<Vec<Operand>, Option<Operand>>, Ice> {
        let mut ops = Vec::with_capacity(dicts.len());
        for d in dicts {
            let Some(op) = self.dict(d, span)? else {
                return Ok(Err(None));
            };
            if self.is_poisoned(&op) {
                return Ok(Err(Some(self.placeholder(self.repr_of(ty))?)));
            }
            ops.push(op);
        }
        Ok(Ok(ops))
    }

    /// A direct call of the top-level function `def` (through its slot,
    /// D-043): `dicts` are its dictionary objects and `args` its
    /// arguments, already lowered and in the callee's parameter
    /// representations. The result, a uniform word (D-453), is converted
    /// to the representation of `ret_ty`. In tail position (`tail`) the
    /// call is the block's tail call and the result is `None` (D-015).
    pub(super) fn call_fn(
        &mut self,
        def: cheby_iface::DefId,
        dicts: Vec<Operand>,
        args: Vec<Operand>,
        ret_ty: cheby_iface::Ty,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        let mut ops = dicts;
        ops.extend(args);
        let callee = Callee::Direct(def);
        if tail && self.b.abi() == Abi::Cheby {
            // Every Cheby function returns a uniform word, so the callee's
            // result is ours unchanged (D-453, D-015).
            self.terminate(Terminator::TailCall { callee, args: ops })?;
            return Ok(None);
        }
        let result = self.define(Repr::Value, Rvalue::Call { callee, args: ops })?;
        self.from_uniform(Operand::Local(result), ret_ty).map(Some)
    }

    /// A block: its statements, then its tail (`Nil` when missing, §5.1).
    fn block(
        &mut self,
        stmts: &[thir::Stmt],
        value: Option<ExprId>,
        span: cheby_diag::Span,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        for stmt in stmts {
            if self.stmt(stmt, span)?.is_none() {
                return Ok(None);
            }
        }
        match value {
            Some(e) => self.expr(e, tail),
            None => Ok(Some(Operand::Const(Const::Nil))),
        }
    }
}
