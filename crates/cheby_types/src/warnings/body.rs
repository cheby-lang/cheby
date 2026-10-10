//! The warnings found in checked bodies (THIR):
//!
//! - W0006: an expression statement of type `Result` that is not its
//!   block's last statement discards a possible error (§5.1, D-153). The
//!   last statement of a block is its value, never a discarded statement,
//!   and `let _ = …` is a binding, so neither warns.
//! - W0009: every `todo` (§5.14, D-068), reachable or not.
//! - W0011: `==` or `!=` whose operands' type is statically known to
//!   contain a function type or an external type (a handle), through
//!   tuples, built-in containers and the fields of every constructor of a
//!   nominal type, with type arguments in place (§3.13, D-070, D-233). A
//!   type parameter of the enclosing item is not known, and neither is the
//!   value inside a `dyn`. A type with `@std_hooks` compares through its
//!   hook, so its fields are not looked into (D-342)
//!   (GAP(equality-static-contents)).
//! - W0013: a negative integer literal written as the argument of a
//!   parameter that a standard-library function clamps (`@std_clamped`,
//!   D-298, D-303). Only a literal counts, including one that a capture
//!   moved into a synthetic local (`list::take(_, -1)`,
//!   GAP(negative-count-captures)); constants and other expressions are
//!   not looked through.

use cheby_diag::{Diagnostic, codes};
use cheby_iface::{
    CtorFields, DefId, DefKind, Generics, Ice, ItemSig, ModuleInterface, PackageId, Ty, TyKind,
    TypeDefKind, builtin, display,
};
use cheby_resolve::LocalId;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::TypesInput;
use crate::thir::{self, BinOpKind, Callee, ExprId, ExprKind, Lit, PanicKind, PatKind, Stmt};

/// The THIR warnings of every checked body of the module, by item, then
/// by expression.
///
/// # Errors
///
/// An [`Ice`] when a body refers to an expression it does not have.
pub(super) fn body_warnings(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    module: &thir::Module,
) -> Result<Vec<Diagnostic>, Ice> {
    let mut out = Vec::new();
    for f in &module.fns {
        if let Some(body) = &f.body {
            let generics = match own.item(f.def.index).map(|i| &i.sig) {
                Some(ItemSig::Fn(sig)) => Some(&sig.generics),
                _ => None,
            };
            BodyCx {
                input,
                own,
                body,
                generics,
            }
            .run(&mut out)?;
        }
    }
    for c in &module.consts {
        BodyCx {
            input,
            own,
            body: &c.body,
            generics: None,
        }
        .run(&mut out)?;
    }
    // The tests of a std module are not part of user builds.
    if input.package != PackageId::STD {
        for t in &module.tests {
            BodyCx {
                input,
                own,
                body: &t.body,
                generics: None,
            }
            .run(&mut out)?;
        }
    }
    Ok(out)
}

struct BodyCx<'a, 'i> {
    input: &'a TypesInput<'i>,
    own: &'a ModuleInterface,
    body: &'a thir::Body,
    generics: Option<&'a Generics>,
}

impl BodyCx<'_, '_> {
    fn expr(&self, id: ExprId) -> Result<&thir::Expr, Ice> {
        self.body
            .expr(id)
            .ok_or_else(|| Ice::new("a THIR expression id out of range"))
    }

    fn show(&self, ty: Ty) -> String {
        display(ty, &self.input.display_cx(self.own, self.generics))
    }

    fn run(&self, out: &mut Vec<Diagnostic>) -> Result<(), Ice> {
        let synthetic = self.synthetic_inits();
        for expr in &self.body.exprs {
            match &expr.kind {
                ExprKind::Block { stmts, .. } => {
                    for stmt in stmts {
                        if let Stmt::Expr(e) = stmt {
                            self.discarded(*e, out)?;
                        }
                    }
                }
                ExprKind::Panic {
                    kind: PanicKind::Todo,
                    ..
                } => out.push(
                    Diagnostic::new(codes::TODO, "`todo` left in the code")
                        .with_primary(expr.span, "this panics when it is reached")
                        .with_help("write the missing code in place of the `todo`"),
                ),
                ExprKind::Binary {
                    op: op @ (BinOpKind::Eq | BinOpKind::Ne),
                    lhs,
                    ..
                } => self.equality(*op, expr, *lhs, out)?,
                ExprKind::Call {
                    callee: Callee::Fn { def, .. },
                    args,
                } => self.counts(*def, args, &synthetic, out)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// W0006 for the expression statement `id`.
    fn discarded(&self, id: ExprId, out: &mut Vec<Diagnostic>) -> Result<(), Ice> {
        let expr = self.expr(id)?;
        let is_result = matches!(
            self.input.globals.types.kind(expr.ty),
            TyKind::Named { def, .. } if *def == builtin::RESULT
        );
        if is_result {
            out.push(
                Diagnostic::new(codes::DISCARDED_RESULT, "this `Result` is discarded")
                    .with_primary(
                        expr.span,
                        format!("this `{}` may hold an error", self.show(expr.ty)),
                    )
                    .with_help(
                        "handle the error with `case` or `use`, or discard the value \
                         explicitly with `let _ = …`",
                    ),
            );
        }
        Ok(())
    }

    /// W0011 for `lhs == rhs` or `lhs != rhs`.
    fn equality(
        &self,
        op: BinOpKind,
        expr: &thir::Expr,
        lhs: ExprId,
        out: &mut Vec<Diagnostic>,
    ) -> Result<(), Ice> {
        let ty = self.expr(lhs)?.ty;
        let mut visited = FxHashSet::default();
        let Some(found) = self.find_opaque(ty, &[], &mut visited) else {
            return Ok(());
        };
        let op = if op == BinOpKind::Eq { "==" } else { "!=" };
        let shown = self.show(ty);
        let (what, label) = match found {
            Found::Fn if matches!(self.input.globals.types.kind(ty), TyKind::Fn { .. }) => {
                ("a function", format!("`{shown}` is a function type"))
            }
            Found::Fn => ("a function", format!("`{shown}` contains a function type")),
            Found::External(def) if self.is_named(ty, def) => {
                ("a handle", format!("`{shown}` is an external type"))
            }
            Found::External(def) => (
                "a handle",
                format!(
                    "`{shown}` contains the external type `{}`",
                    self.input.name_of(self.own, def)
                ),
            ),
        };
        out.push(
            Diagnostic::new(
                codes::FUNCTION_EQUALITY,
                format!("`{op}` on a type that contains {what}"),
            )
            .with_primary(expr.span, label)
            .with_note(format!(
                "`==` and `!=` panic when they reach {what} value at run time"
            ))
            .with_help("compare the parts that can be compared"),
        );
        Ok(())
    }

    fn is_named(&self, ty: Ty, def: DefId) -> bool {
        matches!(self.input.globals.types.kind(ty), TyKind::Named { def: d, .. } if *d == def)
    }

    /// Whether `ty`, with its type parameters standing for types whose
    /// answer is `params` (outside any type declaration: none known),
    /// contains a function type or an external type, and which.
    fn find_opaque(
        &self,
        ty: Ty,
        params: &[Option<Found>],
        visited: &mut FxHashSet<(DefId, Vec<Option<Found>>)>,
    ) -> Option<Found> {
        match self.input.globals.types.kind(ty) {
            TyKind::Fn { .. } => Some(Found::Fn),
            TyKind::Param(p) => usize::try_from(p.as_u32())
                .ok()
                .and_then(|i| params.get(i).copied().flatten()),
            TyKind::Tuple(elems) => elems
                .iter()
                .find_map(|&e| self.find_opaque(e, params, visited)),
            TyKind::Named { def, args } => {
                let args: Vec<Option<Found>> = args
                    .iter()
                    .map(|&a| self.find_opaque(a, params, visited))
                    .collect();
                self.find_in_type(*def, &args, visited)
            }
            _ => None,
        }
    }

    /// [`Self::find_opaque`] for the nominal type `def` whose type
    /// arguments give `args`.
    fn find_in_type(
        &self,
        def: DefId,
        args: &[Option<Found>],
        visited: &mut FxHashSet<(DefId, Vec<Option<Found>>)>,
    ) -> Option<Found> {
        let item = self.input.item(self.own, def).ok()?;
        let ItemSig::Type(ty_def) = &item.sig else {
            return None;
        };
        if ty_def.hooks.is_some() {
            return None;
        }
        match &ty_def.kind {
            TypeDefKind::External => Some(Found::External(def)),
            // `List`, `Map`, `Set` and the like hold values of their
            // arguments' types.
            TypeDefKind::Builtin(_) => args.iter().copied().flatten().next(),
            TypeDefKind::Adt { ctors } => {
                if !visited.insert((def, args.to_vec())) {
                    return None;
                }
                ctors.iter().find_map(|&ctor| {
                    let ctor = self
                        .input
                        .item(self.own, DefId::new(def.module, ctor))
                        .ok()?;
                    let ItemSig::Ctor(sig) = &ctor.sig else {
                        return None;
                    };
                    match &sig.fields {
                        CtorFields::Unit => None,
                        CtorFields::Positional(tys) => {
                            tys.iter().find_map(|&t| self.find_opaque(t, args, visited))
                        }
                        CtorFields::Named(fields) => fields
                            .iter()
                            .find_map(|&(_, t)| self.find_opaque(t, args, visited)),
                    }
                })
            }
        }
    }

    /// The synthetic locals a capture binds its arguments to, with the
    /// expression each one holds.
    fn synthetic_inits(&self) -> FxHashMap<LocalId, ExprId> {
        let mut out = FxHashMap::default();
        for expr in &self.body.exprs {
            let ExprKind::Block { stmts, .. } = &expr.kind else {
                continue;
            };
            for stmt in stmts {
                if let Stmt::Let { pat, init } = stmt
                    && let PatKind::Bind(local) = pat.kind
                    && self.body.local(local).is_some_and(|l| l.synthetic)
                {
                    out.insert(local, *init);
                }
            }
        }
        out
    }

    /// W0013 for a call of `def` with `args`.
    fn counts(
        &self,
        def: DefId,
        args: &[ExprId],
        synthetic: &FxHashMap<LocalId, ExprId>,
        out: &mut Vec<Diagnostic>,
    ) -> Result<(), Ice> {
        let Ok(item) = self.input.item(self.own, def) else {
            return Ok(());
        };
        let ItemSig::Fn(sig) = &item.sig else {
            return Ok(());
        };
        if item.kind != DefKind::Fn || sig.clamped.is_empty() {
            return Ok(());
        }
        for &index in &sig.clamped {
            let Some(&arg) = usize::try_from(index).ok().and_then(|i| args.get(i)) else {
                continue;
            };
            let mut arg = self.expr(arg)?;
            if let ExprKind::Local(local) = arg.kind
                && let Some(&init) = synthetic.get(&local)
            {
                arg = self.expr(init)?;
            }
            let ExprKind::Lit(Lit::Int(value)) = arg.kind else {
                continue;
            };
            if value >= 0 {
                continue;
            }
            let param = usize::try_from(index)
                .ok()
                .and_then(|i| sig.params.get(i))
                .map_or("count", |p| self.input.globals.names.get(p.name));
            let callee = self.input.name_of(self.own, def);
            out.push(
                Diagnostic::new(
                    codes::NEGATIVE_COUNT,
                    format!("negative `{param}` passed to `{callee}`"),
                )
                .with_primary(arg.span, format!("this `{param}` is negative"))
                .with_note(format!(
                    "`{callee}` clamps a `{param}` that is out of range, so a negative one \
                     acts like the smallest useful value"
                ))
                .with_help(format!("pass the `{param}` you mean")),
            );
        }
        Ok(())
    }
}

/// What makes a type unsafe to compare.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Found {
    /// A function type.
    Fn,
    /// An external type: a handle.
    External(DefId),
}
