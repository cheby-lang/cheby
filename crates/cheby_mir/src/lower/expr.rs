//! Bodies and expressions (D2): evaluation order §5.15.1, tail position
//! §5.15.2.

use cheby_diag::Span;
use cheby_iface::{CtorFields, DefId, Ice, Idx, ItemSig, Name, Ty, builtin};
use cheby_types::thir::{self, Callee as ThirCallee, ExprId, ExprKind, Lit, Pat};

use crate::abi::abi_sig;
use crate::builder::FunctionBuilder;
use crate::ir::{Abi, Callee, Const, FnKey, Function, Local, Operand, Rvalue, Stmt, Terminator};
use crate::lower::LowerCx;
use crate::repr::{Repr, repr_of};

/// Lowers a function, constant or test body into a Cheby-ABI function.
/// `dicts` uniform parameters come first (one per bound, D-453),
/// then the body's parameters.
pub(super) fn lower_body(
    key: FnKey,
    span: Span,
    body: &thir::Body,
    dicts: usize,
    cx: &LowerCx<'_>,
) -> Result<Function, Ice> {
    let mut b = FunctionBuilder::new(key, Abi::Cheby, Repr::Value, span)?;
    b.set_entry_loc(&cx.location(span)?)?;
    let mut lx = BodyCx {
        cx,
        body,
        b,
        locals: vec![None; body.locals.len()],
    };
    for _ in 0..dicts {
        lx.b.add_param(Repr::Value, None, None)?;
    }
    for &p in &body.params {
        let decl = body
            .local(p)
            .ok_or_else(|| Ice::new("a parameter without its local"))?;
        let repr = repr_of(decl.ty, cx.defs.globals);
        let local = lx.b.add_param(repr, Some(decl.name), Some(decl.ty))?;
        lx.bind(p, local)?;
    }
    let value = lx.expr(body.value, true)?;
    if let Some(value) = value {
        let ty = lx.ty(body.value)?;
        let value = lx.coerce(value, repr_of(ty, cx.defs.globals), Repr::Value)?;
        lx.b.terminate(Terminator::Return(value))?;
    }
    lx.b.finish()
}

struct BodyCx<'a, 'cx> {
    cx: &'a LowerCx<'cx>,
    body: &'a thir::Body,
    b: FunctionBuilder,
    /// The MIR local of each THIR local, by the resolver's id.
    locals: Vec<Option<Local>>,
}

/// An unsupported construct: the gates (D-451) reject it first.
fn gated(what: &str) -> Ice {
    Ice::new(format!(
        "MIR lowering does not support {what} yet; it should have been rejected with E0038"
    ))
}

impl<'a> BodyCx<'a, '_> {
    fn bind<I: Idx>(&mut self, id: I, local: Local) -> Result<(), Ice> {
        let slot = self
            .locals
            .get_mut(id.index())
            .ok_or_else(|| Ice::new("a THIR local outside the body's table"))?;
        *slot = Some(local);
        Ok(())
    }

    fn lookup<I: Idx>(&self, id: I) -> Result<Local, Ice> {
        self.locals
            .get(id.index())
            .copied()
            .flatten()
            .ok_or_else(|| Ice::new(format!("THIR local {id:?} used before its binding")))
    }

    fn expr_data(&self, id: ExprId) -> Result<&'a thir::Expr, Ice> {
        let body: &'a thir::Body = self.body;
        body.expr(id)
            .ok_or_else(|| Ice::new(format!("no THIR expression {id:?}")))
    }

    fn ty(&self, id: ExprId) -> Result<Ty, Ice> {
        Ok(self.expr_data(id)?.ty)
    }

    fn repr(&self, ty: Ty) -> Repr {
        repr_of(ty, self.cx.defs.globals)
    }

    /// `op` converted from `from` to `to`: boxing into or unboxing out of
    /// the uniform representation.
    fn coerce(&mut self, op: Operand, from: Repr, to: Repr) -> Result<Operand, Ice> {
        if from == to {
            return Ok(op);
        }
        let value = match (from, to) {
            (_, Repr::Value) => Rvalue::Box { value: op, from },
            (Repr::Value, _) => Rvalue::Unbox { value: op, to },
            _ => {
                return Err(Ice::new(format!(
                    "no conversion from {from} to {to} in MIR lowering"
                )));
            }
        };
        Ok(Operand::Local(self.b.assign(to, value)?))
    }

    /// Lowers `id`. In tail position a call becomes the block's tail call
    /// and the result is `None`; `None` also means the block ended (the
    /// rest is unreachable).
    fn expr(&mut self, id: ExprId, tail: bool) -> Result<Option<Operand>, Ice> {
        let e = self.expr_data(id)?;
        let (ty, span) = (e.ty, e.span);
        match &e.kind {
            ExprKind::Lit(Lit::Str(text)) => {
                let s = self.b.string(text)?;
                let l = self
                    .b
                    .assign(Repr::Value, Rvalue::Use(Operand::Const(Const::Str(s))))?;
                Ok(Some(Operand::Local(l)))
            }
            ExprKind::Lit(Lit::Int(_)) => Err(gated("integer literals")),
            ExprKind::Lit(Lit::Float(_)) => Err(gated("float literals")),
            ExprKind::Local(l) => Ok(Some(Operand::Local(self.lookup(*l)?))),
            ExprKind::Ctor { ctor, .. } => self.unit_ctor(*ctor, ty).map(Some),
            ExprKind::Call { callee, args } => self.call(callee, args, ty, tail),
            ExprKind::Block { stmts, tail: value } => self.block(stmts, *value, tail),
            ExprKind::FnRef { .. } => Err(gated("function values")),
            ExprKind::ConstRef { .. } => Err(gated("constants")),
            ExprKind::Closure { .. } => Err(gated("closures")),
            ExprKind::LocalFn { .. } | ExprKind::SelfRef { .. } => Err(gated("local functions")),
            ExprKind::Case { .. } => Err(gated("`case` expressions")),
            ExprKind::Field { .. } => Err(gated("field access")),
            ExprKind::Record { .. } => Err(gated("records")),
            ExprKind::Tuple(_) => Err(gated("tuples")),
            ExprKind::List { .. } => Err(gated("lists")),
            ExprKind::Binary { .. } | ExprKind::Not(_) | ExprKind::Neg(_) => {
                Err(gated("operators"))
            }
            ExprKind::Interpolate { .. } => Err(gated("interpolated strings")),
            ExprKind::Panic { .. } => Err(gated("`panic` and `todo`")),
            ExprKind::DynCoerce { .. } => Err(gated("`dyn` values")),
            ExprKind::Error => Err(Ice::new(format!(
                "an expression with an error reached MIR lowering at {span:?}"
            ))),
        }
    }

    /// A nullary constructor as a value: static objects, `Nil` and unboxed
    /// `Bool`s as constants (D-454).
    fn unit_ctor(&self, ctor: DefId, ty: Ty) -> Result<Operand, Ice> {
        let item = self.cx.defs.item(ctor)?;
        if !matches!(&item.sig, ItemSig::Ctor(sig) if sig.fields == CtorFields::Unit) {
            return Err(gated("constructors with fields as values"));
        }
        let repr = self.repr(ty);
        let c = match ctor {
            c if c == builtin::NIL => Const::Nil,
            c if c == builtin::TRUE && repr == Repr::Bool => Const::Bool(true),
            c if c == builtin::FALSE && repr == Repr::Bool => Const::Bool(false),
            c => Const::StaticCtor(c),
        };
        Ok(Operand::Const(c))
    }

    fn call(
        &mut self,
        callee: &ThirCallee,
        args: &[ExprId],
        ty: Ty,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        let ThirCallee::Fn { def, dicts, .. } = callee else {
            return Err(gated("calls of constructors and function values"));
        };
        if !dicts.is_empty() {
            return Err(gated("calls that pass interface implementations"));
        }
        let sig = abi_sig(self.cx.defs.fn_sig(*def)?, self.cx.defs.globals);
        if sig.params.len() != args.len() {
            return Err(Ice::new(format!(
                "call of {def:?} with {} arguments for {} parameters",
                args.len(),
                sig.params.len()
            )));
        }
        // Arguments left to right (§5.15.1), each converted to the
        // callee's parameter representation.
        let mut ops = Vec::with_capacity(args.len());
        for (&arg, &want) in args.iter().zip(&sig.params) {
            let Some(op) = self.expr(arg, false)? else {
                return Ok(None);
            };
            let from = self.repr(self.ty(arg)?);
            ops.push(self.coerce(op, from, want)?);
        }
        let callee = Callee::Direct(*def);
        if tail && self.b.abi() == Abi::Cheby {
            // Every Cheby function returns a uniform word, so the callee's
            // result is ours unchanged (D-453, D-015).
            self.b
                .terminate(Terminator::TailCall { callee, args: ops })?;
            return Ok(None);
        }
        let result = self
            .b
            .assign(Repr::Value, Rvalue::Call { callee, args: ops })?;
        let want = self.repr(ty);
        self.coerce(Operand::Local(result), Repr::Value, want)
            .map(Some)
    }

    fn block(
        &mut self,
        stmts: &[thir::Stmt],
        value: Option<ExprId>,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        for stmt in stmts {
            match stmt {
                thir::Stmt::Let { pat, init } => {
                    let Some(op) = self.expr(*init, false)? else {
                        return Ok(None);
                    };
                    match pat {
                        Pat::Bind(id) => {
                            let decl = self
                                .body
                                .local(*id)
                                .ok_or_else(|| Ice::new("a `let` without its local"))?;
                            let (name, ty) = (decl.name, decl.ty);
                            self.let_bind(*id, name, ty, op, *init)?;
                        }
                        Pat::Wild => {}
                        _ => return Err(gated("`let` with a pattern")),
                    }
                }
                thir::Stmt::Expr(e) => {
                    // The value is unused; the RC pass drops it.
                    if self.expr(*e, false)?.is_none() {
                        return Ok(None);
                    }
                }
                thir::Stmt::Assert { .. } => return Err(gated("`assert`")),
                thir::Stmt::LetAssert { .. } => return Err(gated("`let assert`")),
            }
        }
        match value {
            Some(e) => self.expr(e, tail),
            None => Ok(Some(Operand::Const(Const::Nil))),
        }
    }

    /// Binds a `let` name. A fresh temporary takes the name; anything else
    /// (a constant, another binding) is moved into a new local, so every
    /// binding is its own local.
    fn let_bind<I: Idx>(
        &mut self,
        id: I,
        name: Name,
        ty: Ty,
        op: Operand,
        init: ExprId,
    ) -> Result<(), Ice> {
        let repr = self.repr(ty);
        let from = self.repr(self.ty(init)?);
        let op = self.coerce(op, from, repr)?;
        let local = match op {
            Operand::Local(l) if self.b.name_local(l, name, ty)? => l,
            op => {
                let l = self.b.new_local(repr, Some(name), Some(ty))?;
                self.b.push(Stmt::Assign {
                    dst: l,
                    value: Rvalue::Use(op),
                })?;
                l
            }
        };
        self.bind(id, local)
    }
}
