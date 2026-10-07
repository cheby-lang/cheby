//! Expressions (chapter 5): the dispatch, literals, paths and items used
//! as values, tuples, lists, operators, `panic` and `todo`.

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{CtorFields, CtorSig, DefId, Ice, ItemSig, ParamIdx, Ty};
use cheby_resolve::Res;
use cheby_syntax::ast::{
    AstNode, BinExpr, BinOp, Expr, ListExpr, ListItem as AstListItem, Literal, LiteralKind,
    Message, PathExpr, PrefixExpr, PrefixOp, TodoExpr, TupleExpr, TypeArgList,
};

use super::constraints::placeholder_dict;
use super::{BodyCx, Deferred, Expected, VarNote};
use crate::infer::{InferTy, VarKind, instantiate};
use crate::thir::{BinOpKind, Dict, ExprId, ExprKind, ListItem, Lit, OpImpl, PanicKind};

/// An item used at a path, with its type arguments: the turbofish's
/// (D-049), or fresh variables.
pub(crate) struct Instance {
    pub args: Box<[InferTy]>,
}

impl BodyCx<'_> {
    /// Checks `expr`, against `expected` if given, and returns its THIR,
    /// already converted to the expected type where §3.12.5 allows.
    pub fn check_expr(
        &mut self,
        expr: Expr<'_>,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        let span = expr.significant_span();
        let id = match expr {
            // These check against `expected` themselves; a `dyn`
            // expectation is applied here, where the conversion can wrap
            // the result.
            Expr::Case(case) => self.check_case(case, expected)?,
            Expr::Record(rec) => {
                let inner = self.non_dyn(expected)?;
                self.check_record(rec, inner.as_ref())?
            }
            Expr::Field(field) => {
                let inner = self.non_dyn(expected)?;
                self.check_field(field, inner.as_ref())?
            }
            Expr::Literal(lit) => self.literal(lit, None)?,
            Expr::String(s) => match s.plain_value() {
                Some(value) => {
                    self.push(ExprKind::Lit(Lit::Str(value.into())), InferTy::STRING, span)?
                }
                None => self.interpolation(s)?,
            },
            Expr::Path(p) => self.path_expr(p)?,
            Expr::Call(c) => self.call(c, expected)?,
            Expr::Tuple(t) => self.tuple(t, expected)?,
            // No THIR node: the inner expression in its place.
            Expr::Paren(p) => match p.inner() {
                Some(inner) => return self.check_expr(inner, expected),
                None => self.error_expr(span)?,
            },
            Expr::List(l) => self.list(l, expected)?,
            Expr::Block(b) => self.block(b, expected)?,
            Expr::Closure(c) => self.closure(c, expected)?,
            Expr::Panic(p) => self.panic_expr(PanicKind::Panic, p.message(), span)?,
            Expr::Todo(t) => self.todo_expr(t, span)?,
            Expr::Binary(b) => self.binary(b, expected)?,
            Expr::Prefix(p) => self.prefix(p)?,
            // A `_` outside an argument list is E0025 (phase A).
            Expr::Hole(_) => self.error_expr(span)?,
        };
        self.coerce_expr(id, expected)
    }

    /// `expected` without a `dyn` type, which the caller converts to.
    fn non_dyn(&mut self, expected: Option<&Expected>) -> Result<Option<Expected>, Ice> {
        Ok(match expected {
            Some(e) if self.dyn_iface(&e.ty)?.is_none() => Some(e.clone()),
            _ => None,
        })
    }

    // ---- literals ------------------------------------------------------

    /// A number or raw string literal; `negative` is the `-` of a negative
    /// literal (§5.3.1), whose span covers the whole literal.
    pub(crate) fn literal(
        &mut self,
        lit: Literal<'_>,
        negative: Option<Span>,
    ) -> Result<ExprId, Ice> {
        let span = negative.unwrap_or_else(|| lit.significant_span());
        match lit.kind() {
            Some(LiteralKind::Int(int)) => {
                // A malformed literal was reported by the lexer.
                let Some(value) = int.value() else {
                    return self.error_expr(span);
                };
                let value = i128::try_from(value).ok().and_then(|v| {
                    if negative.is_some() {
                        v.checked_neg()
                    } else {
                        Some(v)
                    }
                });
                let Some(value) = value else {
                    self.report(
                        Diagnostic::new(codes::LITERAL_OUT_OF_RANGE, "literal out of range")
                            .with_primary(span, "this is too large for any integer type"),
                    );
                    return self.error_expr(span);
                };
                let ty = self.infcx.new_var(span, VarKind::IntLit)?;
                self.push(ExprKind::Lit(Lit::Int(value)), ty, span)
            }
            Some(LiteralKind::Float(float)) => {
                let Some(value) = float.value() else {
                    return self.error_expr(span);
                };
                let value = if negative.is_some() { -value } else { value };
                let ty = self.infcx.new_var(span, VarKind::FloatLit)?;
                self.push(ExprKind::Lit(Lit::Float(value)), ty, span)
            }
            Some(LiteralKind::RawString(raw)) => self.push(
                ExprKind::Lit(Lit::Str(raw.value().into_owned().into())),
                InferTy::STRING,
                span,
            ),
            None => self.error_expr(span),
        }
    }

    // ---- paths ---------------------------------------------------------

    fn path_expr(&mut self, p: PathExpr<'_>) -> Result<ExprId, Ice> {
        let span = p.significant_span();
        let Some(path) = p.path() else {
            return self.error_expr(span);
        };
        match self.path_res(path.syntax().id())? {
            Res::Local(local) => {
                if p.type_args().is_some() {
                    let name = self
                        .res
                        .locals
                        .try_get(local, "local")
                        .map(|l| self.input.globals.names.get(l.name))?;
                    self.report(no_type_args(span, name));
                    return self.error_expr(span);
                }
                let ty = self.local_ty(local)?;
                let kind = if self.local_fns.contains(&local) {
                    ExprKind::SelfRef { local }
                } else {
                    ExprKind::Local(local)
                };
                self.push(kind, ty, span)
            }
            Res::Def(def) => self.def_value(def, p.type_args(), span),
            Res::Err => self.error_expr(span),
            Res::Module(_) | Res::TyParam(_) | Res::SelfTy => {
                Err(Ice::new("a value path resolved to a module or a type"))
            }
        }
    }

    /// The type arguments of a use of `def` at `p`: the turbofish's, or
    /// fresh variables, `arity` of them (E0051 for a wrong count). `None`
    /// after an error.
    pub(crate) fn instance(
        &mut self,
        def: DefId,
        params: &[(ParamIdx, cheby_iface::Name)],
        type_args: Option<TypeArgList<'_>>,
        span: Span,
    ) -> Result<Option<Instance>, Ice> {
        let name = self.input.name_of(self.own, def);
        let Some(list) = type_args else {
            let args = params
                .iter()
                .map(|&(_, param)| self.noted_var(span, VarNote::TyArg { callee: def, param }))
                .collect::<Result<Box<[InferTy]>, Ice>>()?;
            return Ok(Some(Instance { args }));
        };
        let written: Vec<_> = list.types().collect();
        let mut args = Vec::with_capacity(written.len());
        for ty in &written {
            args.push(self.lower_written_ty(*ty)?);
        }
        if written.len() != params.len() {
            let plural = |n: usize| if n == 1 { "" } else { "s" };
            let message = if params.is_empty() {
                format!("`{name}` takes no type arguments")
            } else {
                format!(
                    "`{name}` takes {} type argument{} but {} {} given",
                    params.len(),
                    plural(params.len()),
                    written.len(),
                    if written.len() == 1 { "was" } else { "were" }
                )
            };
            self.report(
                Diagnostic::new(codes::WRONG_TYPE_ARGUMENT_COUNT, message)
                    .with_primary(list.significant_span(), ""),
            );
            return Ok(None);
        }
        Ok(Some(Instance { args: args.into() }))
    }

    /// An item used as a value: a function ([`ExprKind::FnRef`]), a
    /// constant ([`ExprKind::ConstRef`]), a unit constructor
    /// ([`ExprKind::Ctor`]), or a positional constructor or interface
    /// function, which become closures (GAP(interface-fn-value)).
    pub(crate) fn def_value(
        &mut self,
        def: DefId,
        type_args: Option<TypeArgList<'_>>,
        span: Span,
    ) -> Result<ExprId, Ice> {
        let item = self.input.item(self.own, def)?;
        match (&item.sig, item.kind) {
            (ItemSig::Unsupported | ItemSig::Error, _) => self.error_expr(span),
            (ItemSig::Fn(sig), _) => {
                let params = item_params(&sig.generics);
                let Some(inst) = self.instance(def, &params, type_args, span)? else {
                    return self.error_expr(span);
                };
                let types = &self.input.globals.types;
                let ty = InferTy::Fn {
                    params: sig
                        .params
                        .iter()
                        .map(|param| instantiate(types, param.ty, &inst.args))
                        .collect(),
                    ret: Box::new(instantiate(types, sig.ret, &inst.args)),
                };
                let bounds = bounds_of(&sig.generics, &inst.args);
                let dicts: Box<[Dict]> = bounds
                    .iter()
                    .map(|(_, iface)| placeholder_dict(*iface))
                    .collect();
                let id = self.push(
                    ExprKind::FnRef {
                        def,
                        ty_args: Box::new([]),
                        dicts,
                    },
                    ty,
                    span,
                )?;
                self.record_instance(id, def, inst.args, bounds);
                Ok(id)
            }
            (ItemSig::Const(sig), _) => {
                if type_args.is_some() {
                    let name = self.input.name_of(self.own, def);
                    self.report(no_type_args(span, name));
                    return self.error_expr(span);
                }
                self.const_deps.push(def);
                self.push(ExprKind::ConstRef { def }, InferTy::Known(sig.ty), span)
            }
            (ItemSig::Ctor(ctor), _) => match &ctor.fields {
                CtorFields::Unit => {
                    let Some((ty, args)) = self.ctor_instance(def, ctor, type_args, span)? else {
                        return self.error_expr(span);
                    };
                    let id = self.push(
                        ExprKind::Ctor {
                            ctor: def,
                            ty_args: Box::new([]),
                        },
                        ty,
                        span,
                    )?;
                    if !args.is_empty() {
                        self.inst.push((id, args));
                    }
                    Ok(id)
                }
                CtorFields::Positional(fields) => {
                    let fields = fields.clone();
                    let Some((ty, args)) = self.ctor_instance(def, ctor, type_args, span)? else {
                        return self.error_expr(span);
                    };
                    self.ctor_closure(def, &fields, ty, args, span)
                }
                CtorFields::Named(_) => {
                    let name = self.input.name_of(self.own, def);
                    self.report(named_ctor_as_value(span, name, false));
                    self.error_expr(span)
                }
            },
            (ItemSig::InterfaceFn(_), _) => self.iface_fn_closure(def, span),
            (_, kind) => Err(Ice::new(format!(
                "a value path resolved to a {}",
                kind.describe()
            ))),
        }
    }

    /// Records the type arguments of the generic use `id` of `def`, and a
    /// deferred constraint for each bound (plan §4.3 order).
    pub(crate) fn record_instance(
        &mut self,
        id: ExprId,
        def: DefId,
        args: Box<[InferTy]>,
        bounds: Vec<(InferTy, DefId)>,
    ) {
        for (slot, (ty, iface)) in bounds.into_iter().enumerate() {
            self.defer(Deferred::Bound {
                expr: id,
                slot,
                ty,
                iface,
                callee: def,
            });
        }
        if !args.is_empty() {
            self.inst.push((id, args));
        }
    }

    /// The type a constructor builds and its type arguments: the
    /// turbofish's, or fresh variables. `None` after an error.
    pub(crate) fn ctor_instance(
        &mut self,
        ctor_def: DefId,
        ctor: &CtorSig,
        type_args: Option<TypeArgList<'_>>,
        span: Span,
    ) -> Result<Option<Instantiated>, Ice> {
        let ty_def = DefId::new(ctor_def.module, ctor.ty);
        let generics = match &self.input.item(self.own, ty_def)?.sig {
            ItemSig::Type(def) => def.generics.clone(),
            _ => return Err(Ice::new("a constructor's type is not a type")),
        };
        let params = item_params(&generics);
        let Some(inst) = self.instance(ty_def, &params, type_args, span)? else {
            return Ok(None);
        };
        Ok(Some((self.named_ty(ty_def, &inst.args), inst.args)))
    }

    /// The type a constructor builds, with fresh variables for the type's
    /// parameters, and those variables.
    pub(crate) fn ctor_type(
        &mut self,
        ctor_def: DefId,
        ctor: &CtorSig,
        span: Span,
    ) -> Result<(InferTy, Box<[InferTy]>), Ice> {
        let ty_def = DefId::new(ctor_def.module, ctor.ty);
        let generics = match &self.input.item(self.own, ty_def)?.sig {
            ItemSig::Type(def) => def.generics.clone(),
            _ => return Err(Ice::new("a constructor's type is not a type")),
        };
        let args = generics
            .params
            .iter()
            .map(|p| {
                self.noted_var(
                    span,
                    VarNote::TyArg {
                        callee: ty_def,
                        param: p.name,
                    },
                )
            })
            .collect::<Result<Box<[InferTy]>, Ice>>()?;
        Ok((self.named_ty(ty_def, &args), args))
    }

    /// The nominal type `def<args>`.
    pub(crate) fn named_ty(&self, def: DefId, args: &[InferTy]) -> InferTy {
        if args.is_empty() {
            InferTy::Known(self.input.globals.types.named(def, []))
        } else {
            InferTy::Named {
                def,
                args: args.into(),
            }
        }
    }

    // ---- tuples and lists ---------------------------------------------

    /// A tuple (§5.12): elements left to right, each against the element
    /// of an expected tuple type of the same length.
    fn tuple(&mut self, t: TupleExpr<'_>, expected: Option<&Expected>) -> Result<ExprId, Ice> {
        let elems: Vec<Expr<'_>> = t.elements().collect();
        let expected_elems = match expected {
            Some(e) => match self.infcx.resolve(&e.ty)? {
                InferTy::Tuple(tys) if tys.len() == elems.len() => Some(tys),
                _ => None,
            },
            None => None,
        };
        let mut ids = Vec::with_capacity(elems.len());
        let mut tys = Vec::with_capacity(elems.len());
        for (n, elem) in elems.iter().enumerate() {
            let exp = expected_elems
                .as_ref()
                .and_then(|tys| tys.get(n))
                .map(|ty| Expected::plain(ty.clone()));
            let id = self.check_expr(*elem, exp.as_ref())?;
            tys.push(self.ty_of(id)?);
            ids.push(id);
        }
        self.push(
            ExprKind::Tuple(ids.into()),
            InferTy::Tuple(tys.into()),
            t.significant_span(),
        )
    }

    /// A list literal (§5.13): every element against the element type,
    /// every spread against the list type. Without an expected list type,
    /// the first element that is not `Never` fixes the element type.
    fn list(&mut self, l: ListExpr<'_>, expected: Option<&Expected>) -> Result<ExprId, Ice> {
        let span = l.significant_span();
        let mut elem_ty = match expected {
            Some(e) => match self.infcx.resolve(&e.ty)? {
                InferTy::Named { def, args } if def == cheby_iface::builtin::LIST => {
                    args.first().cloned()
                }
                _ => None,
            },
            None => None,
        };
        let from_context = elem_ty.is_some();
        let mut items = Vec::new();
        let mut first: Option<(Span, InferTy)> = None;
        for item in l.items() {
            match item {
                AstListItem::Expr(e) => {
                    let exp = elem_ty.clone().map(|ty| match &first {
                        Some((at, shown)) if !from_context => Expected::new(
                            ty,
                            Some((
                                at.to_owned(),
                                format!("this element has type `{}`", self.show(shown)),
                            )),
                        ),
                        _ => Expected::plain(ty),
                    });
                    let id = self.check_list_elem(e, exp.as_ref(), from_context)?;
                    let ty = self.ty_of(id)?;
                    if elem_ty.is_none() && self.infcx.shallow(&ty)? != InferTy::NEVER {
                        first = Some((self.span_of(id)?, ty.clone()));
                        elem_ty = Some(ty);
                    }
                    items.push(ListItem::Elem(id));
                }
                AstListItem::Spread(s) => {
                    let Some(value) = s.value() else {
                        let id = self.error_expr(s.significant_span())?;
                        items.push(ListItem::Spread(id));
                        continue;
                    };
                    let var = match &elem_ty {
                        Some(ty) => ty.clone(),
                        None => self.fresh_var(s.significant_span())?,
                    };
                    let list_ty = self.list_of(var.clone());
                    let id = self.check_expr(value, Some(&Expected::plain(list_ty)))?;
                    if elem_ty.is_none() && self.infcx.shallow(&self.ty_of(id)?)? != InferTy::NEVER
                    {
                        elem_ty = Some(var);
                    }
                    items.push(ListItem::Spread(id));
                }
            }
        }
        let elem = match elem_ty {
            Some(ty) => ty,
            // `[]`, or only `Never` elements: nothing fixes the element type
            // here; the context may.
            None if items.is_empty() => self.fresh_var(span)?,
            None => InferTy::NEVER,
        };
        let ty = self.list_of(elem);
        self.push(
            ExprKind::List {
                items: items.into(),
            },
            ty,
            span,
        )
    }

    /// One element of a list literal. Without an expected list type, an
    /// element of another type than the first is E0047 with a help about
    /// `dyn` element types (§3.12.5).
    fn check_list_elem(
        &mut self,
        e: Expr<'_>,
        expected: Option<&Expected>,
        from_context: bool,
    ) -> Result<ExprId, Ice> {
        let before = self.diags.len();
        let id = self.check_expr(e, expected)?;
        if !from_context
            && expected.is_some()
            && let Some(diag) = self.diags.get_mut(before..).and_then(|d| d.last_mut())
            && diag.code == codes::TYPE_MISMATCH
        {
            let help = "to mix values of different types that share an interface `I`, \
                        annotate the list as `List<dyn I>`";
            *diag = diag.clone().with_help(help);
        }
        Ok(id)
    }

    /// `List<elem>`.
    pub(crate) fn list_of(&self, elem: InferTy) -> InferTy {
        let list = cheby_iface::builtin::LIST;
        match elem {
            InferTy::Known(t) => InferTy::Known(self.input.globals.types.named(list, [t])),
            other => InferTy::Named {
                def: list,
                args: Box::new([other]),
            },
        }
    }

    // ---- panic and todo -------------------------------------------------

    /// `panic` or `todo` (§5.14): type `Never`, with a `String` message
    /// (D-161).
    pub(crate) fn panic_expr(
        &mut self,
        kind: PanicKind,
        message: Option<Message<'_>>,
        span: Span,
    ) -> Result<ExprId, Ice> {
        let message = match message.and_then(Message::value) {
            Some(m) => Some(self.check_expr(m, Some(&string_message()))?),
            None => None,
        };
        self.push(ExprKind::Panic { kind, message }, InferTy::NEVER, span)
    }

    fn todo_expr(&mut self, t: TodoExpr<'_>, span: Span) -> Result<ExprId, Ice> {
        self.panic_expr(PanicKind::Todo, t.message(), span)
    }

    // ---- operators -----------------------------------------------------

    /// A binary operator (§5.4). The meaning of arithmetic, comparisons,
    /// `%` and the bitwise operators depends on the operand type, which is
    /// resolved at the end of the body (D-234); their types are known now
    /// (D-118).
    fn binary(&mut self, b: BinExpr<'_>, expected: Option<&Expected>) -> Result<ExprId, Ice> {
        let span = b.significant_span();
        let Some(op) = b.op() else {
            return self.error_expr(span);
        };
        let op = match op {
            BinOp::Pipe => return self.pipe(b, expected),
            BinOp::Or => BinOpKind::Or,
            BinOp::And => BinOpKind::And,
            BinOp::Eq => BinOpKind::Eq,
            BinOp::Ne => BinOpKind::Ne,
            BinOp::Lt => BinOpKind::Lt,
            BinOp::Le => BinOpKind::Le,
            BinOp::Gt => BinOpKind::Gt,
            BinOp::Ge => BinOpKind::Ge,
            BinOp::BitOr => BinOpKind::BitOr,
            BinOp::BitXor => BinOpKind::BitXor,
            BinOp::BitAnd => BinOpKind::BitAnd,
            BinOp::Shl => BinOpKind::Shl,
            BinOp::Shr => BinOpKind::Shr,
            BinOp::Add => BinOpKind::Add,
            BinOp::Sub => BinOpKind::Sub,
            BinOp::Mul => BinOpKind::Mul,
            BinOp::Div => BinOpKind::Div,
            BinOp::Rem => BinOpKind::Rem,
        };
        let (Some(lhs), Some(rhs)) = (b.lhs(), b.rhs()) else {
            return self.error_expr(span);
        };
        if matches!(op, BinOpKind::And | BinOpKind::Or) {
            // `&&` and `||` take and return `Bool` (§5.4.5).
            let operand = Expected::new(
                InferTy::Known(Ty::BOOL),
                b.op_token().map(|t| {
                    (
                        t.span(),
                        format!(
                            "`{}` takes `Bool` operands",
                            super::constraints::op_text(op)
                        ),
                    )
                }),
            );
            let l = self.check_expr(lhs, Some(&operand))?;
            let r = self.check_expr(rhs, Some(&operand))?;
            return self.push(
                ExprKind::Binary {
                    op,
                    lhs: l,
                    rhs: r,
                    imp: OpImpl::Builtin,
                },
                InferTy::Known(Ty::BOOL),
                span,
            );
        }
        let l = self.check_expr(lhs, None)?;
        let r = self.check_expr(rhs, None)?;
        let operand = self.same_operands(l, r, op)?;
        let compares = matches!(
            op,
            BinOpKind::Eq
                | BinOpKind::Ne
                | BinOpKind::Lt
                | BinOpKind::Le
                | BinOpKind::Gt
                | BinOpKind::Ge
        );
        let ty = if compares {
            InferTy::Known(Ty::BOOL)
        } else {
            operand.clone()
        };
        let id = self.push(
            ExprKind::Binary {
                op,
                lhs: l,
                rhs: r,
                imp: OpImpl::Builtin,
            },
            ty,
            span,
        )?;
        // `==` and `!=` are always structural (§5.4.4).
        if !matches!(op, BinOpKind::Eq | BinOpKind::Ne) && operand != InferTy::ERROR {
            self.defer(Deferred::Operator {
                expr: id,
                op,
                operand,
            });
        }
        Ok(id)
    }

    /// The common type of two operands that must have the same type
    /// (D-118, §5.4.4). A `Never` operand takes the other's type.
    fn same_operands(&mut self, l: ExprId, r: ExprId, op: BinOpKind) -> Result<InferTy, Ice> {
        let lt = self.ty_of(l)?;
        let rt = self.ty_of(r)?;
        if self.infcx.shallow(&lt)? == InferTy::NEVER {
            return Ok(rt);
        }
        if self.infcx.shallow(&rt)? == InferTy::NEVER {
            return Ok(lt);
        }
        match self.infcx.unify(&lt, &rt)? {
            Ok(()) => Ok(lt),
            Err(mismatch) => {
                let origin = Some((
                    self.span_of(l)?,
                    format!(
                        "the left operand of `{}` has type `{}`",
                        super::constraints::op_text(op),
                        self.show(&lt)
                    ),
                ));
                let span = self.span_of(r)?;
                self.mismatch(&lt, &rt, mismatch, span, origin);
                Ok(InferTy::ERROR)
            }
        }
    }

    /// `-x`, `!x`, and negative literals (§5.3.1).
    fn prefix(&mut self, p: PrefixExpr<'_>) -> Result<ExprId, Ice> {
        let span = p.significant_span();
        if let Some(lit) = p.negative_literal() {
            return self.literal(lit, Some(span));
        }
        let (Some(op), Some(operand)) = (p.op(), p.operand()) else {
            return self.error_expr(span);
        };
        match op {
            PrefixOp::Not => {
                let bool_ty = Expected::new(
                    InferTy::Known(Ty::BOOL),
                    p.op_token()
                        .map(|t| (t.span(), "`!` takes a `Bool` operand".to_owned())),
                );
                let x = self.check_expr(operand, Some(&bool_ty))?;
                self.push(ExprKind::Not(x), InferTy::Known(Ty::BOOL), span)
            }
            PrefixOp::Neg => {
                let x = self.check_expr(operand, None)?;
                let ty = self.ty_of(x)?;
                let id = self.push(
                    ExprKind::Neg {
                        operand: x,
                        imp: OpImpl::Builtin,
                    },
                    ty.clone(),
                    span,
                )?;
                if ty != InferTy::ERROR {
                    self.defer(Deferred::Negation {
                        expr: id,
                        operand: ty,
                    });
                }
                Ok(id)
            }
        }
    }
}

/// A generic type with its type arguments.
pub(crate) type Instantiated = (InferTy, Box<[InferTy]>);

/// The expectation of an `as` message: `String`.
pub(crate) const fn string_message() -> Expected {
    Expected::plain(InferTy::STRING)
}

/// The type parameters of a generic item: a function's, or the type of a
/// constructor.
pub(crate) fn item_params(generics: &cheby_iface::Generics) -> Vec<(ParamIdx, cheby_iface::Name)> {
    generics
        .params
        .iter_enumerated()
        .map(|(idx, p)| (idx, p.name))
        .collect()
}

/// For each type parameter of `generics` in order, for each of its bounds
/// in written order: the instantiated type and the interface (plan §4.3).
pub(crate) fn bounds_of(
    generics: &cheby_iface::Generics,
    args: &[InferTy],
) -> Vec<(InferTy, DefId)> {
    let mut out = Vec::new();
    for ((_, param), arg) in generics.params.iter_enumerated().zip(args) {
        for &iface in &param.bounds {
            out.push((arg.clone(), iface));
        }
    }
    out
}

/// E0051 for type arguments on something that takes none.
fn no_type_args(span: Span, name: &str) -> Diagnostic {
    Diagnostic::new(
        codes::WRONG_TYPE_ARGUMENT_COUNT,
        format!("`{name}` takes no type arguments"),
    )
    .with_primary(span, "")
}

/// E0059: a named-field constructor called positionally or used as a
/// value (D-138).
pub(crate) fn named_ctor_as_value(span: Span, name: &str, called: bool) -> Diagnostic {
    let message = if called {
        format!("`{name}` has named fields and cannot be called positionally")
    } else {
        format!("`{name}` has named fields and cannot be used as a function")
    };
    Diagnostic::new(codes::INVALID_RECORD, message)
        .with_primary(span, "")
        .with_help(format!("build it with its field names: `{name} {{ … }}`"))
}
