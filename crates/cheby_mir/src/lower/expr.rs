//! Bodies and expressions (D2): evaluation order §5.15.1, tail position
//! §5.15.2.
//!
//! What the back end lowers today: string literals, locals, the builtin
//! unit constructors (`Nil`, `True`, `False`, `Less`, `Equal`, `Greater`,
//! `None`), direct calls of top-level functions without dictionaries,
//! blocks, `let` with a name or `_`, and expression statements, with
//! values in the representation of their types where no conversion
//! between an unboxed and the uniform representation is needed (D6).
//! Everything else is E0038 at the outermost construct (D-510): the
//! construct is not lowered, its value is a placeholder local, and the
//! function is dropped at the end, so the rest of the body is still
//! checked for other such constructs.

use cheby_diag::{Diagnostic, Span};
use cheby_iface::{CtorFields, DefId, Generics, Ice, Idx, ItemSig, ModuleId, Name, Ty, builtin};
use cheby_types::thir::{
    self, Callee as ThirCallee, ExprId, ExprKind, Lit, PanicKind, ParamOrigin, PatKind,
};
use rustc_hash::FxHashSet;

use crate::abi::abi_sig;
use crate::builder::FunctionBuilder;
use crate::ir::{Abi, Callee, Const, FnKey, Function, Local, Operand, Rvalue, Stmt, Terminator};
use crate::lower::{LowerCx, LowerError, not_supported};
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
) -> Result<Function, LowerError> {
    let generics = match key {
        FnKey::Item(def) => Some(&cx.defs.fn_sig(def)?.generics),
        _ => None,
    };
    let mut b = FunctionBuilder::new(key, Abi::Cheby, Repr::Value, span)?;
    b.set_entry_loc(&cx.location(span)?)?;
    let mut lx = BodyCx {
        cx,
        body,
        generics,
        b,
        locals: vec![None; body.locals.len()],
        diags: Vec::new(),
        poisoned: FxHashSet::default(),
    };
    let lowered = lx.run(dicts);
    // A broken invariant after an unsupported construct is most likely
    // the placeholder's doing: the construct is what to report.
    if !lx.diags.is_empty() {
        return Err(LowerError::Unsupported(lx.diags));
    }
    lowered?;
    Ok(lx.b.finish()?)
}

struct BodyCx<'a, 'cx> {
    cx: &'a LowerCx<'cx>,
    body: &'a thir::Body,
    /// The enclosing item's type parameters, to name types in E0038.
    generics: Option<&'cx Generics>,
    b: FunctionBuilder,
    /// The MIR local of each THIR local, by the resolver's id.
    locals: Vec<Option<Local>>,
    /// E0038 for each outermost unsupported construct.
    diags: Vec<Diagnostic>,
    /// Placeholder locals that stand for unsupported constructs. They are
    /// never defined; the function is dropped. Membership tests only, so
    /// the hash set's order never shows (D-264).
    poisoned: FxHashSet<Local>,
}

impl<'a> BodyCx<'a, '_> {
    fn run(&mut self, dicts: usize) -> Result<(), Ice> {
        for _ in 0..dicts {
            self.b.add_param(Repr::Value, None, None)?;
        }
        let body = self.body;
        for &p in &body.params {
            let decl = body
                .local(p)
                .ok_or_else(|| Ice::new("a parameter without its local"))?;
            let repr = self.repr(decl.ty);
            let local = self.b.add_param(repr, Some(decl.name), Some(decl.ty))?;
            self.bind(p, local)?;
        }
        let value = self.expr(body.value, true)?;
        if let Some(value) = value {
            let ty = self.ty(body.value)?;
            let at = self.tail_span(body.value)?;
            let value = self.coerce(value, self.repr(ty), Repr::Value, ty, at)?;
            self.b.terminate(Terminator::Return(value))?;
        }
        Ok(())
    }

    fn bind<I: Idx>(&mut self, id: I, local: Local) -> Result<(), Ice> {
        let slot = self
            .locals
            .get_mut(id.index())
            .ok_or_else(|| Ice::new("a THIR local outside the body's table"))?;
        *slot = Some(local);
        Ok(())
    }

    /// The value of local `id`, of type `ty`.
    fn lookup<I: Idx>(&mut self, id: I, ty: Ty) -> Result<Operand, Ice> {
        if let Some(local) = self.locals.get(id.index()).copied().flatten() {
            return Ok(Operand::Local(local));
        }
        if self.diags.is_empty() {
            return Err(Ice::new(format!(
                "THIR local {id:?} used before its binding"
            )));
        }
        // Bound by a construct that was not lowered.
        self.placeholder(self.repr(ty))
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

    /// The span of the value a body returns: the innermost tail of its
    /// blocks.
    fn tail_span(&self, mut id: ExprId) -> Result<Span, Ice> {
        loop {
            let e = self.expr_data(id)?;
            match &e.kind {
                ExprKind::Block { tail: Some(t), .. } => id = *t,
                _ => return Ok(e.span),
            }
        }
    }

    /// `ty` as users write it (D-463).
    fn show(&self, ty: Ty) -> String {
        self.cx.show(ty, self.generics)
    }

    /// A local of `repr` that stands for something not lowered.
    fn placeholder(&mut self, repr: Repr) -> Result<Operand, Ice> {
        let local = self.b.new_local(repr, None, None)?;
        self.poisoned.insert(local);
        Ok(Operand::Local(local))
    }

    fn is_poisoned(&self, op: &Operand) -> bool {
        matches!(op, Operand::Local(l) if self.poisoned.contains(l))
    }

    /// Reports E0038 for the construct at `span` and returns a placeholder
    /// for its value of type `ty`.
    fn unsupported(&mut self, span: Span, what: &str, ty: Ty) -> Result<Option<Operand>, Ice> {
        self.diags.push(not_supported(span, what));
        self.placeholder(self.repr(ty)).map(Some)
    }

    /// `op`, a value of type `ty`, converted from `from` to `to`. Boxing
    /// into and unboxing out of the uniform representation is not
    /// implemented yet (D6): E0038 at `span`.
    fn coerce(
        &mut self,
        op: Operand,
        from: Repr,
        to: Repr,
        ty: Ty,
        span: Span,
    ) -> Result<Operand, Ice> {
        if from == to {
            return Ok(op);
        }
        if !self.is_poisoned(&op) {
            let what = format!("values of type `{}`", self.show(ty));
            self.diags.push(not_supported(span, &what));
        }
        self.placeholder(to)
    }

    /// Lowers `id`. In tail position a call becomes the block's tail call
    /// and the result is `None`; `None` also means the block ended (the
    /// rest is unreachable).
    fn expr(&mut self, id: ExprId, tail: bool) -> Result<Option<Operand>, Ice> {
        let e = self.expr_data(id)?;
        let (ty, span) = (e.ty, e.span);
        let what = match &e.kind {
            ExprKind::Lit(Lit::Str(text)) => {
                let s = self.b.string(text)?;
                let l = self
                    .b
                    .assign(Repr::Value, Rvalue::Use(Operand::Const(Const::Str(s))))?;
                return Ok(Some(Operand::Local(l)));
            }
            ExprKind::Local(l) => return self.lookup(*l, ty).map(Some),
            ExprKind::Ctor { ctor, .. } => return self.unit_ctor(*ctor, ty, span),
            ExprKind::Call { callee, args } => return self.call(callee, args, ty, span, tail),
            ExprKind::Block { stmts, tail: value } => {
                return self.block(stmts, *value, span, tail);
            }
            ExprKind::LocalFn { local, .. } => {
                // Its name is in scope after the statement.
                let fn_ty = self.body.local(*local).map_or(Ty::ERROR, |l| l.ty);
                let placeholder = self.placeholder(self.repr(fn_ty))?;
                if let Operand::Local(l) = placeholder {
                    self.bind(*local, l)?;
                }
                "local functions"
            }
            ExprKind::Error => {
                return Err(Ice::new(format!(
                    "an expression with an error reached MIR lowering at {span:?}"
                )));
            }
            kind => unsupported_expr(kind),
        };
        self.unsupported(span, what, ty)
    }

    /// A nullary constructor as a value: `Nil` and unboxed `Bool`s as
    /// constants, the other builtin ones as their static objects (D-454).
    fn unit_ctor(&mut self, ctor: DefId, ty: Ty, span: Span) -> Result<Option<Operand>, Ice> {
        let item = self.cx.defs.item(ctor)?;
        if !matches!(&item.sig, ItemSig::Ctor(sig) if sig.fields == CtorFields::Unit) {
            return Err(Ice::new(format!(
                "a constructor with fields as a unit constructor at {span:?}"
            )));
        }
        if ctor.module != ModuleId::BUILTIN {
            // Their static objects need registered headers (D-454).
            return self.unsupported(span, "constructors of user-defined types", ty);
        }
        let repr = self.repr(ty);
        let c = match ctor {
            c if c == builtin::NIL => Const::Nil,
            c if c == builtin::TRUE && repr == Repr::Bool => Const::Bool(true),
            c if c == builtin::FALSE && repr == Repr::Bool => Const::Bool(false),
            c => Const::StaticCtor(c),
        };
        Ok(Some(Operand::Const(c)))
    }

    fn call(
        &mut self,
        callee: &ThirCallee,
        args: &[ExprId],
        ty: Ty,
        span: Span,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        let def = match callee {
            ThirCallee::Fn { def, dicts, .. } if dicts.is_empty() => *def,
            ThirCallee::Fn { .. } => {
                return self.unsupported(span, "calls of functions with interface bounds", ty);
            }
            ThirCallee::Ctor { .. } => {
                return self.unsupported(span, "constructors with fields", ty);
            }
            ThirCallee::Iface { .. } => {
                return self.unsupported(span, "calls of interface functions", ty);
            }
            ThirCallee::Dyn { .. } => {
                return self.unsupported(span, "calls through `dyn` values", ty);
            }
            ThirCallee::Value(_) => {
                return self.unsupported(span, "calls of function values", ty);
            }
            _ => return self.unsupported(span, "this kind of call", ty),
        };
        let sig = abi_sig(self.cx.defs.fn_sig(def)?, self.cx.defs.globals);
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
            let arg_ty = self.ty(arg)?;
            let at = self.expr_data(arg)?.span;
            ops.push(self.coerce(op, self.repr(arg_ty), want, arg_ty, at)?);
        }
        let callee = Callee::Direct(def);
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
        self.coerce(Operand::Local(result), Repr::Value, want, ty, span)
            .map(Some)
    }

    fn block(
        &mut self,
        stmts: &[thir::Stmt],
        value: Option<ExprId>,
        span: Span,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        for stmt in stmts {
            match stmt {
                thir::Stmt::Let { pat, init } => {
                    let Some(op) = self.expr(*init, false)? else {
                        return Ok(None);
                    };
                    match &pat.kind {
                        PatKind::Bind(id) => {
                            let decl = self
                                .body
                                .local(*id)
                                .ok_or_else(|| Ice::new("a `let` without its local"))?;
                            let (name, ty) = (decl.name, decl.ty);
                            self.let_bind(*id, name, ty, op, *init)?;
                        }
                        PatKind::Wild => {}
                        _ => {
                            self.diags.push(not_supported(
                                pat.span,
                                "patterns other than a name or `_` in `let`",
                            ));
                            self.bind_pattern(pat)?;
                        }
                    }
                }
                thir::Stmt::Expr(e) => {
                    // The value is unused; the RC pass drops it.
                    if self.expr(*e, false)?.is_none() {
                        return Ok(None);
                    }
                }
                thir::Stmt::Assert { cond, .. } => {
                    let at = self.expr_data(*cond)?.span;
                    self.diags.push(not_supported(at, "`assert` statements"));
                }
                thir::Stmt::LetAssert { pat, .. } => {
                    self.diags
                        .push(not_supported(pat.span, "`let assert` statements"));
                    self.bind_pattern(pat)?;
                }
                _ => self
                    .diags
                    .push(not_supported(span, "this kind of statement")),
            }
        }
        match value {
            Some(e) => self.expr(e, tail),
            None => Ok(Some(Operand::Const(Const::Nil))),
        }
    }

    /// Binds every name of a pattern that is not lowered to a
    /// placeholder, so later uses lower without an error of their own.
    fn bind_pattern(&mut self, pat: &thir::Pat) -> Result<(), Ice> {
        let mut bound = Vec::new();
        pat.walk(&mut |p| match &p.kind {
            PatKind::Bind(l) | PatKind::As { local: l, .. } => bound.push((*l, p.ty)),
            _ => {}
        });
        for (local, ty) in bound {
            let placeholder = self.placeholder(self.repr(ty))?;
            if let Operand::Local(l) = placeholder {
                self.bind(local, l)?;
            }
        }
        Ok(())
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
        let init_ty = self.ty(init)?;
        let at = self.expr_data(init)?.span;
        let op = self.coerce(op, self.repr(init_ty), repr, init_ty, at)?;
        let local = match op {
            Operand::Local(l) if self.poisoned.contains(&l) => l,
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

/// What to call an expression the back end does not lower yet, in E0038
/// (the front end's old wording, D-451). A kind added to THIR later is
/// reported too, so additions never break the build.
fn unsupported_expr(kind: &ExprKind) -> &'static str {
    match kind {
        ExprKind::Lit(Lit::Int(_) | Lit::Float(_)) => "numeric literals",
        ExprKind::Lit(_) => "this kind of literal",
        ExprKind::FnRef { .. } => "function values",
        ExprKind::ConstRef { .. } => "constants",
        ExprKind::Closure { params, .. } => match params.first().map(|p| p.origin) {
            Some(ParamOrigin::Use) => "`use` statements",
            Some(ParamOrigin::Capture) => "function captures",
            _ => "anonymous functions",
        },
        ExprKind::LocalFn { .. } | ExprKind::SelfRef { .. } => "local functions",
        ExprKind::Case { .. } => "`case` expressions",
        ExprKind::Field { .. } => "field access",
        ExprKind::Record { .. } => "record expressions",
        ExprKind::RecordUpdate { .. } => "record updates",
        ExprKind::Tuple(_) => "tuples",
        ExprKind::List { .. } => "list expressions",
        ExprKind::Binary { .. } | ExprKind::Not(_) | ExprKind::Neg { .. } => "operators",
        ExprKind::Interpolate { .. } => "interpolated strings",
        ExprKind::Panic {
            kind: PanicKind::Todo,
            ..
        } => "`todo` expressions",
        ExprKind::Panic { .. } => "`panic` expressions",
        ExprKind::DynCoerce { .. } => "conversions to `dyn`",
        _ => "this kind of expression",
    }
}
