//! Body-local inference types (C1): type variables, union-find with path
//! compression, unification with an occurs check, literal defaulting
//! (D-084, D-133) and zonking into closed [`Ty`]s.
//!
//! The state is local to one top-level body, so bodies check in parallel
//! (D-263). Closed types stay interned [`Ty`] handles ([`InferTy::Known`]);
//! only types that contain variables are spelled out structurally.
//!
//! Unification has no subtyping (D-235). [`Ty::ERROR`] unifies with
//! anything, so a reported error does not cascade. The `Never` conversion
//! happens in the checker, where an expression meets its expected type,
//! never inside unification.

use std::fmt::Write as _;

use cheby_diag::Span;
use cheby_iface::{DefId, DisplayCx, Globals, Ice, IndexVec, Ty, TyInterner, TyKind, display};

cheby_iface::newtype_index! {
    /// A type variable of one body.
    pub struct TyVid;
}

/// A type during inference.
///
/// There is no variant for `dyn`: interfaces have no type parameters
/// (D-165), so a `dyn I` type never holds a variable and is `Known`
/// (GAP(infer-dyn-not-needed)).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum InferTy {
    /// A closed type: no variables inside.
    Known(Ty),
    Var(TyVid),
    Named {
        def: DefId,
        args: Box<[InferTy]>,
    },
    Tuple(Box<[InferTy]>),
    Fn {
        params: Box<[InferTy]>,
        ret: Box<InferTy>,
    },
}

impl InferTy {
    pub const ERROR: Self = Self::Known(Ty::ERROR);
    pub const NIL: Self = Self::Known(Ty::NIL);
    pub const STRING: Self = Self::Known(Ty::STRING);
    pub const NEVER: Self = Self::Known(Ty::NEVER);
}

/// What a variable may become.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum VarKind {
    /// Any type.
    General,
    /// The type of an integer literal: any integer type, `Int` by default
    /// (§3.3.4).
    IntLit,
    /// The type of a float literal: `Float` or `F32`, `Float` by default.
    FloatLit,
}

impl VarKind {
    /// The kind of a variable that must satisfy both kinds, if any.
    const fn meet(self, other: Self) -> Option<Self> {
        match (self, other) {
            (Self::General, k) | (k, Self::General) => Some(k),
            (Self::IntLit, Self::IntLit) => Some(Self::IntLit),
            (Self::FloatLit, Self::FloatLit) => Some(Self::FloatLit),
            (Self::IntLit, Self::FloatLit) | (Self::FloatLit, Self::IntLit) => None,
        }
    }

    /// Whether a variable of this kind may become `ty`.
    fn admits(self, ty: Ty) -> bool {
        match self {
            Self::General => true,
            Self::IntLit => is_integer(ty) || ty == Ty::ERROR,
            Self::FloatLit => is_float(ty) || ty == Ty::ERROR,
        }
    }
}

/// Whether `ty` is an integer type (§3.3).
#[must_use]
pub fn is_integer(ty: Ty) -> bool {
    [
        Ty::INT,
        Ty::I8,
        Ty::I16,
        Ty::I32,
        Ty::I64,
        Ty::U8,
        Ty::U16,
        Ty::U32,
        Ty::U64,
    ]
    .contains(&ty)
}

/// Whether `ty` is a float type (§3.3).
#[must_use]
pub fn is_float(ty: Ty) -> bool {
    ty == Ty::FLOAT || ty == Ty::F32
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum VarState {
    /// A root without a type. `rank` bounds the depth of its tree.
    Unbound {
        origin: Span,
        kind: VarKind,
        rank: u32,
    },
    /// A non-root: `Var` links to its parent in the union-find forest; any
    /// other type is the root's binding.
    Bound(InferTy),
}

/// Why two types do not unify.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Mismatch {
    /// Different types.
    Types,
    /// A variable would contain itself (`T = List<T>`).
    Occurs,
}

/// The inference state of one body.
#[derive(Debug)]
pub struct InferCtxt<'g> {
    globals: &'g Globals,
    vars: IndexVec<TyVid, VarState>,
    /// Old states of the variables written by the current [`Self::unify`],
    /// so a failed unification leaves no partial bindings.
    undo: Vec<(TyVid, VarState)>,
    logging: bool,
}

impl<'g> InferCtxt<'g> {
    #[must_use]
    pub const fn new(globals: &'g Globals) -> Self {
        Self {
            globals,
            vars: IndexVec::new(),
            undo: Vec::new(),
            logging: false,
        }
    }

    #[must_use]
    pub const fn types(&self) -> &'g TyInterner {
        &self.globals.types
    }

    /// A fresh variable, created for the expression or binding at
    /// `origin`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the body has more than `u32::MAX` variables.
    pub fn new_var(&mut self, origin: Span, kind: VarKind) -> Result<InferTy, Ice> {
        let vid = self.vars.push(VarState::Unbound {
            origin,
            kind,
            rank: 0,
        })?;
        Ok(InferTy::Var(vid))
    }

    /// The number of variables created so far.
    #[must_use]
    pub const fn var_count(&self) -> usize {
        self.vars.len()
    }

    /// Where variable `vid` was created.
    #[must_use]
    pub fn origin(&self, vid: TyVid) -> Option<Span> {
        let root = self.root(vid).ok()?;
        match self.vars.get(root)? {
            VarState::Unbound { origin, .. } => Some(*origin),
            VarState::Bound(_) => None,
        }
    }

    /// The kind of `t` if it is an unbound variable, after following
    /// bindings: `Some(VarKind::IntLit)` for a numeric literal whose type is
    /// not fixed yet.
    ///
    /// # Errors
    ///
    /// An [`Ice`] for a variable of another body.
    pub fn var_kind(&mut self, t: &InferTy) -> Result<Option<VarKind>, Ice> {
        let InferTy::Var(vid) = self.shallow(t)? else {
            return Ok(None);
        };
        Ok(match self.state(vid)? {
            VarState::Unbound { kind, .. } => Some(*kind),
            VarState::Bound(_) => None,
        })
    }

    /// The root of `vid`'s tree: the variable that stands for all the
    /// variables joined with it.
    ///
    /// # Errors
    ///
    /// An [`Ice`] for a variable of another body.
    pub fn root_of(&self, vid: TyVid) -> Result<TyVid, Ice> {
        self.root(vid)
    }

    fn state(&self, vid: TyVid) -> Result<&VarState, Ice> {
        self.vars.try_get(vid, "type variable")
    }

    fn write(&mut self, vid: TyVid, state: VarState) -> Result<(), Ice> {
        let slot = self
            .vars
            .get_mut(vid)
            .ok_or_else(|| Ice::new(format!("no type variable {vid:?}")))?;
        let old = std::mem::replace(slot, state);
        if self.logging {
            self.undo.push((vid, old));
        }
        Ok(())
    }

    /// The root of `vid`'s tree, without compressing the path.
    fn root(&self, vid: TyVid) -> Result<TyVid, Ice> {
        let mut current = vid;
        // Each step moves to a parent; a tree never has more nodes than
        // there are variables, so the bound catches a corrupted cycle.
        for _ in 0..=self.vars.len() {
            match self.state(current)? {
                VarState::Bound(InferTy::Var(parent)) => current = *parent,
                _ => return Ok(current),
            }
        }
        Err(Ice::new("cycle in the type variable forest"))
    }

    /// The root of `vid`'s tree, compressing the path to it.
    fn find(&mut self, vid: TyVid) -> Result<TyVid, Ice> {
        let root = self.root(vid)?;
        let mut current = vid;
        while current != root {
            let VarState::Bound(InferTy::Var(parent)) = *self.state(current)? else {
                break;
            };
            if parent != root {
                self.write(current, VarState::Bound(InferTy::Var(root)))?;
            }
            current = parent;
        }
        Ok(root)
    }

    /// `t` with its outermost variables replaced by their bindings: a
    /// `Var` result is an unbound root.
    ///
    /// # Errors
    ///
    /// An [`Ice`] for a variable of another body.
    pub fn shallow(&mut self, t: &InferTy) -> Result<InferTy, Ice> {
        let InferTy::Var(vid) = t else {
            return Ok(t.clone());
        };
        let root = self.find(*vid)?;
        Ok(match self.state(root)? {
            VarState::Bound(bound) => bound.clone(),
            VarState::Unbound { .. } => InferTy::Var(root),
        })
    }

    /// Like [`Self::shallow`], and a closed type with type arguments or
    /// parts is spelled out one level, so callers can match on its shape.
    ///
    /// # Errors
    ///
    /// An [`Ice`] for a variable of another body.
    pub fn resolve(&mut self, t: &InferTy) -> Result<InferTy, Ice> {
        let t = self.shallow(t)?;
        Ok(self.expand(&t))
    }

    /// One level of a closed type's structure; other types unchanged.
    #[must_use]
    pub fn expand(&self, t: &InferTy) -> InferTy {
        let InferTy::Known(ty) = t else {
            return t.clone();
        };
        let known = |tys: &[Ty]| tys.iter().map(|&t| InferTy::Known(t)).collect();
        match self.types().kind(*ty) {
            TyKind::Named { def, args } if !args.is_empty() => InferTy::Named {
                def: *def,
                args: known(args),
            },
            TyKind::Tuple(elems) => InferTy::Tuple(known(elems)),
            TyKind::Fn { params, ret } => InferTy::Fn {
                params: known(params),
                ret: Box::new(InferTy::Known(*ret)),
            },
            _ => t.clone(),
        }
    }

    /// Makes `a` and `b` the same type. On a mismatch nothing is bound.
    ///
    /// # Errors
    ///
    /// An [`Ice`] for a variable of another body.
    pub fn unify(&mut self, a: &InferTy, b: &InferTy) -> Result<Result<(), Mismatch>, Ice> {
        self.undo.clear();
        self.logging = true;
        let result = self.unify_inner(a, b);
        self.logging = false;
        let undo = std::mem::take(&mut self.undo);
        match result {
            Ok(Ok(())) => Ok(Ok(())),
            Ok(Err(mismatch)) => {
                for (vid, old) in undo.into_iter().rev() {
                    self.write(vid, old)?;
                }
                Ok(Err(mismatch))
            }
            Err(ice) => Err(ice),
        }
    }

    fn unify_inner(&mut self, a: &InferTy, b: &InferTy) -> Result<Result<(), Mismatch>, Ice> {
        let a = self.shallow(a)?;
        let b = self.shallow(b)?;
        match (&a, &b) {
            (InferTy::Var(x), InferTy::Var(y)) => self.union(*x, *y),
            (InferTy::Var(x), t) | (t, InferTy::Var(x)) => self.bind(*x, t),
            (InferTy::Known(x), InferTy::Known(y)) if x == y => Ok(Ok(())),
            (InferTy::Known(Ty::ERROR), other) | (other, InferTy::Known(Ty::ERROR)) => {
                self.poison(other)?;
                Ok(Ok(()))
            }
            _ => {
                let a = self.expand(&a);
                let b = self.expand(&b);
                match (&a, &b) {
                    (
                        InferTy::Named { def: d1, args: a1 },
                        InferTy::Named { def: d2, args: a2 },
                    ) if d1 == d2 && a1.len() == a2.len() => self.unify_all(a1, a2),
                    (InferTy::Tuple(e1), InferTy::Tuple(e2)) if e1.len() == e2.len() => {
                        self.unify_all(e1, e2)
                    }
                    (
                        InferTy::Fn {
                            params: p1,
                            ret: r1,
                        },
                        InferTy::Fn {
                            params: p2,
                            ret: r2,
                        },
                    ) if p1.len() == p2.len() => match self.unify_all(p1, p2)? {
                        Ok(()) => self.unify_inner(r1, r2),
                        Err(mismatch) => Ok(Err(mismatch)),
                    },
                    _ => Ok(Err(Mismatch::Types)),
                }
            }
        }
    }

    fn unify_all(&mut self, a: &[InferTy], b: &[InferTy]) -> Result<Result<(), Mismatch>, Ice> {
        for (x, y) in a.iter().zip(b) {
            if let Err(mismatch) = self.unify_inner(x, y)? {
                return Ok(Err(mismatch));
            }
        }
        Ok(Ok(()))
    }

    /// Joins two unbound roots, by rank.
    fn union(&mut self, x: TyVid, y: TyVid) -> Result<Result<(), Mismatch>, Ice> {
        if x == y {
            return Ok(Ok(()));
        }
        let (
            VarState::Unbound {
                origin: ox,
                kind: kx,
                rank: rx,
            },
            VarState::Unbound {
                origin: oy,
                kind: ky,
                rank: ry,
            },
        ) = (self.state(x)?.clone(), self.state(y)?.clone())
        else {
            return Err(Ice::new("union of a bound type variable"));
        };
        let Some(kind) = kx.meet(ky) else {
            return Ok(Err(Mismatch::Types));
        };
        // The joined variable keeps the origin of the one created first,
        // which E0057 points to: the expression that introduced the type.
        let origin = if x < y { ox } else { oy };
        let (child, parent, rank) = if rx < ry {
            (x, y, ry)
        } else {
            (y, x, if rx == ry { rx.saturating_add(1) } else { rx })
        };
        self.write(child, VarState::Bound(InferTy::Var(parent)))?;
        self.write(parent, VarState::Unbound { origin, kind, rank })?;
        Ok(Ok(()))
    }

    /// Binds the unbound root `x` to the non-variable type `t`.
    fn bind(&mut self, x: TyVid, t: &InferTy) -> Result<Result<(), Mismatch>, Ice> {
        let VarState::Unbound { kind, .. } = *self.state(x)? else {
            return Err(Ice::new("binding a bound type variable"));
        };
        if kind != VarKind::General {
            let InferTy::Known(ty) = t else {
                return Ok(Err(Mismatch::Types));
            };
            if !kind.admits(*ty) {
                return Ok(Err(Mismatch::Types));
            }
        }
        if self.occurs(x, t)? {
            return Ok(Err(Mismatch::Occurs));
        }
        self.write(x, VarState::Bound(t.clone()))?;
        Ok(Ok(()))
    }

    /// Binds every unbound variable inside `t` to [`Ty::ERROR`]: a type
    /// unified with an error learns nothing, so whatever its variables
    /// stand for is silent too (one error per root cause, plan §7).
    fn poison(&mut self, t: &InferTy) -> Result<(), Ice> {
        match self.shallow(t)? {
            InferTy::Known(_) => Ok(()),
            InferTy::Var(v) => {
                let root = self.find(v)?;
                self.write(root, VarState::Bound(InferTy::ERROR))
            }
            InferTy::Named { args: parts, .. } | InferTy::Tuple(parts) => {
                for part in &parts {
                    self.poison(part)?;
                }
                Ok(())
            }
            InferTy::Fn { params, ret } => {
                for part in &params {
                    self.poison(part)?;
                }
                self.poison(&ret)
            }
        }
    }

    /// Whether the root `x` occurs in `t`.
    fn occurs(&self, x: TyVid, t: &InferTy) -> Result<bool, Ice> {
        match t {
            InferTy::Known(_) => Ok(false),
            InferTy::Var(v) => {
                let root = self.root(*v)?;
                if root == x {
                    return Ok(true);
                }
                match self.state(root)? {
                    VarState::Bound(bound) => self.occurs(x, bound),
                    VarState::Unbound { .. } => Ok(false),
                }
            }
            InferTy::Named { args: parts, .. } | InferTy::Tuple(parts) => {
                for part in parts {
                    if self.occurs(x, part)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            InferTy::Fn { params, ret } => {
                for part in params {
                    if self.occurs(x, part)? {
                        return Ok(true);
                    }
                }
                self.occurs(x, ret)
            }
        }
    }

    /// Defaults every unbound literal variable: integer literals to `Int`,
    /// float literals to `Float` (D-084). Called once, after the whole body
    /// has been inferred (D-133).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the variable table is corrupted.
    pub fn default_literals(&mut self) -> Result<(), Ice> {
        let vids: Vec<TyVid> = self.vars.indices().collect();
        for vid in vids {
            if let VarState::Unbound { kind, .. } = *self.state(vid)? {
                let ty = match kind {
                    VarKind::General => continue,
                    VarKind::IntLit => Ty::INT,
                    VarKind::FloatLit => Ty::FLOAT,
                };
                self.write(vid, VarState::Bound(InferTy::Known(ty)))?;
            }
        }
        Ok(())
    }

    /// The closed type of `t`. An unbound literal variable gets its
    /// default; an unbound general variable becomes [`Ty::ERROR`] and its
    /// root is pushed to `unresolved` (the checker reports it).
    ///
    /// # Errors
    ///
    /// An [`Ice`] for a variable of another body.
    pub fn zonk(&self, t: &InferTy, unresolved: &mut Vec<TyVid>) -> Result<Ty, Ice> {
        let types = self.types();
        Ok(match t {
            InferTy::Known(ty) => *ty,
            InferTy::Var(v) => {
                let root = self.root(*v)?;
                match self.state(root)? {
                    VarState::Bound(bound) => self.zonk(bound, unresolved)?,
                    VarState::Unbound { kind, .. } => match kind {
                        VarKind::IntLit => Ty::INT,
                        VarKind::FloatLit => Ty::FLOAT,
                        VarKind::General => {
                            if !unresolved.contains(&root) {
                                unresolved.push(root);
                            }
                            Ty::ERROR
                        }
                    },
                }
            }
            InferTy::Named { def, args } => {
                let args = self.zonk_all(args, unresolved)?;
                types.named(*def, args)
            }
            InferTy::Tuple(elems) => {
                let elems = self.zonk_all(elems, unresolved)?;
                types.intern(TyKind::Tuple(elems.into()))
            }
            InferTy::Fn { params, ret } => {
                let params = self.zonk_all(params, unresolved)?;
                let ret = self.zonk(ret, unresolved)?;
                types.func(params, ret)
            }
        })
    }

    fn zonk_all(&self, ts: &[InferTy], unresolved: &mut Vec<TyVid>) -> Result<Vec<Ty>, Ice> {
        ts.iter().map(|t| self.zonk(t, unresolved)).collect()
    }

    /// `t` as users write it, for diagnostics: unknown parts as `_`, and
    /// literal types not fixed yet as `{integer}` and `{float}`.
    #[must_use]
    pub fn display(&self, t: &InferTy, cx: &DisplayCx<'_>) -> String {
        let mut out = String::new();
        self.write_display(&mut out, t, cx);
        out
    }

    fn write_display(&self, out: &mut String, t: &InferTy, cx: &DisplayCx<'_>) {
        match t {
            InferTy::Known(ty) => out.push_str(&display(*ty, cx)),
            InferTy::Var(v) => {
                let state = self.root(*v).ok().and_then(|root| self.vars.get(root));
                match state {
                    Some(VarState::Bound(bound)) => self.write_display(out, bound, cx),
                    Some(VarState::Unbound {
                        kind: VarKind::IntLit,
                        ..
                    }) => out.push_str("{integer}"),
                    Some(VarState::Unbound {
                        kind: VarKind::FloatLit,
                        ..
                    }) => out.push_str("{float}"),
                    _ => out.push('_'),
                }
            }
            InferTy::Named { def, args } => {
                // The name alone, through the closed-type display.
                let bare = cx.globals.types.named(*def, []);
                out.push_str(&display(bare, cx));
                if !args.is_empty() {
                    out.push('<');
                    self.write_list(out, args, cx);
                    out.push('>');
                }
            }
            InferTy::Tuple(elems) => {
                out.push('(');
                self.write_list(out, elems, cx);
                out.push(')');
            }
            InferTy::Fn { params, ret } => {
                out.push_str("fn(");
                self.write_list(out, params, cx);
                out.push_str(") -> ");
                self.write_display(out, ret, cx);
            }
        }
    }

    fn write_list(&self, out: &mut String, ts: &[InferTy], cx: &DisplayCx<'_>) {
        for (n, t) in ts.iter().enumerate() {
            if n > 0 {
                let _ = write!(out, ", ");
            }
            self.write_display(out, t, cx);
        }
    }
}

/// `ty` with type parameter `n` replaced by `args[n]`: the instantiation
/// of a generic signature at one use. A parameter without an argument
/// becomes [`Ty::ERROR`] (an arity error was already reported).
#[must_use]
pub fn instantiate(types: &TyInterner, ty: Ty, args: &[InferTy]) -> InferTy {
    if args.is_empty() || !mentions_param(types, ty) {
        return InferTy::Known(ty);
    }
    let all = |tys: &[Ty]| tys.iter().map(|&t| instantiate(types, t, args)).collect();
    match types.kind(ty) {
        TyKind::Param(p) => {
            let index = usize::try_from(p.as_u32()).unwrap_or(usize::MAX);
            args.get(index).cloned().unwrap_or(InferTy::ERROR)
        }
        TyKind::Named { def, args: inner } => InferTy::Named {
            def: *def,
            args: all(inner),
        },
        TyKind::Tuple(elems) => InferTy::Tuple(all(elems)),
        TyKind::Fn { params, ret } => InferTy::Fn {
            params: all(params),
            ret: Box::new(instantiate(types, *ret, args)),
        },
        // `mentions_param` is false for the others.
        TyKind::Error | TyKind::Never | TyKind::Dyn { .. } => InferTy::Known(ty),
    }
}

/// [`instantiate`] for closed arguments: substitution in closed types,
/// used for type aliases and constructor fields.
#[must_use]
pub fn substitute(types: &TyInterner, ty: Ty, args: &[Ty]) -> Ty {
    if args.is_empty() || !mentions_param(types, ty) {
        return ty;
    }
    let all =
        |tys: &[Ty]| -> Box<[Ty]> { tys.iter().map(|&t| substitute(types, t, args)).collect() };
    match types.kind(ty) {
        TyKind::Param(p) => {
            let index = usize::try_from(p.as_u32()).unwrap_or(usize::MAX);
            args.get(index).copied().unwrap_or(Ty::ERROR)
        }
        TyKind::Named { def, args: inner } => types.named(*def, all(inner)),
        TyKind::Tuple(elems) => types.intern(TyKind::Tuple(all(elems))),
        TyKind::Fn { params, ret } => types.func(all(params), substitute(types, *ret, args)),
        TyKind::Error | TyKind::Never | TyKind::Dyn { .. } => ty,
    }
}

/// Whether `ty` mentions a type parameter.
#[must_use]
pub fn mentions_param(types: &TyInterner, ty: Ty) -> bool {
    match types.kind(ty) {
        TyKind::Param(_) => true,
        TyKind::Named { args: parts, .. } | TyKind::Tuple(parts) => {
            parts.iter().any(|&t| mentions_param(types, t))
        }
        TyKind::Fn { params, ret } => {
            params.iter().any(|&t| mentions_param(types, t)) || mentions_param(types, *ret)
        }
        TyKind::Error | TyKind::Never | TyKind::Dyn { .. } => false,
    }
}

/// Whether `ty` contains [`Ty::ERROR`] anywhere.
#[must_use]
pub fn contains_error(types: &TyInterner, ty: Ty) -> bool {
    match types.kind(ty) {
        TyKind::Error => true,
        TyKind::Named { args: parts, .. }
        | TyKind::Tuple(parts)
        | TyKind::Dyn { args: parts, .. } => parts.iter().any(|&t| contains_error(types, t)),
        TyKind::Fn { params, ret } => {
            params.iter().any(|&t| contains_error(types, t)) || contains_error(types, *ret)
        }
        TyKind::Never | TyKind::Param(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use cheby_diag::{FileId, TextRange};
    use cheby_iface::{DepView, InterfaceSet, ModuleId, ModuleTable, ParamIdx, builtin};

    use super::*;

    fn span() -> Span {
        Span::new(FileId::new(0), TextRange::empty(0))
    }

    fn list(g: &Globals, t: InferTy) -> InferTy {
        let _ = g;
        InferTy::Named {
            def: builtin::LIST,
            args: Box::new([t]),
        }
    }

    #[test]
    fn variables_unify_with_types_and_each_other() {
        let g = Globals::new();
        let mut cx = InferCtxt::new(&g);
        let a = cx.new_var(span(), VarKind::General).unwrap();
        let b = cx.new_var(span(), VarKind::General).unwrap();
        assert_eq!(cx.unify(&a, &b).unwrap(), Ok(()));
        assert_eq!(cx.unify(&b, &InferTy::STRING).unwrap(), Ok(()));
        assert_eq!(cx.shallow(&a).unwrap(), InferTy::STRING);
        let mut unresolved = Vec::new();
        assert_eq!(cx.zonk(&a, &mut unresolved).unwrap(), Ty::STRING);
        assert_eq!(unresolved, []);
        assert_eq!(cx.unify(&a, &InferTy::NIL).unwrap(), Err(Mismatch::Types));
    }

    #[test]
    #[expect(
        clippy::many_single_char_names,
        reason = "short names for a type algebra test"
    )]
    fn structural_unification_reaches_inside_closed_types() {
        let g = Globals::new();
        let mut cx = InferCtxt::new(&g);
        let v = cx.new_var(span(), VarKind::General).unwrap();
        let list_int = g.types.named(builtin::LIST, [Ty::INT]);
        assert_eq!(
            cx.unify(&list(&g, v.clone()), &InferTy::Known(list_int))
                .unwrap(),
            Ok(())
        );
        let mut unresolved = Vec::new();
        assert_eq!(cx.zonk(&v, &mut unresolved).unwrap(), Ty::INT);
        // Different constructors, and different arities, do not unify.
        let opt_int = g.types.named(builtin::OPTION, [Ty::INT]);
        assert_eq!(
            cx.unify(&InferTy::Known(list_int), &InferTy::Known(opt_int))
                .unwrap(),
            Err(Mismatch::Types)
        );
        let pair = g
            .types
            .intern(TyKind::Tuple(Box::new([Ty::INT, Ty::STRING])));
        let w = cx.new_var(span(), VarKind::General).unwrap();
        let tuple = InferTy::Tuple(Box::new([InferTy::Known(Ty::INT), w.clone()]));
        assert_eq!(cx.unify(&tuple, &InferTy::Known(pair)).unwrap(), Ok(()));
        assert_eq!(cx.zonk(&w, &mut unresolved).unwrap(), Ty::STRING);
        let f = g.types.func([Ty::STRING], Ty::NIL);
        let r = cx.new_var(span(), VarKind::General).unwrap();
        let shape = InferTy::Fn {
            params: Box::new([InferTy::STRING]),
            ret: Box::new(r.clone()),
        };
        assert_eq!(cx.unify(&InferTy::Known(f), &shape).unwrap(), Ok(()));
        assert_eq!(cx.zonk(&r, &mut unresolved).unwrap(), Ty::NIL);
        let two = g.types.func([Ty::STRING, Ty::STRING], Ty::NIL);
        assert_eq!(
            cx.unify(&InferTy::Known(f), &InferTy::Known(two)).unwrap(),
            Err(Mismatch::Types)
        );
    }

    #[test]
    fn occurs_check() {
        let g = Globals::new();
        let mut cx = InferCtxt::new(&g);
        let v = cx.new_var(span(), VarKind::General).unwrap();
        let w = cx.new_var(span(), VarKind::General).unwrap();
        assert_eq!(cx.unify(&v, &w).unwrap(), Ok(()));
        // `v = List<w>` with `v` and `w` joined is `v = List<v>`.
        assert_eq!(
            cx.unify(&v, &list(&g, w.clone())).unwrap(),
            Err(Mismatch::Occurs)
        );
        // Nothing was bound.
        assert!(matches!(cx.shallow(&v).unwrap(), InferTy::Var(_)));
        let f = InferTy::Fn {
            params: Box::new([]),
            ret: Box::new(w.clone()),
        };
        assert_eq!(cx.unify(&f, &v).unwrap(), Err(Mismatch::Occurs));
    }

    #[test]
    fn an_error_poisons_the_variables_it_meets() {
        let g = Globals::new();
        let mut cx = InferCtxt::new(&g);
        let a = cx.new_var(span(), VarKind::General).unwrap();
        let b = cx.new_var(span(), VarKind::IntLit).unwrap();
        let shape = InferTy::Fn {
            params: Box::new([list(&g, a.clone())]),
            ret: Box::new(InferTy::Tuple(Box::new([b.clone(), InferTy::STRING]))),
        };
        assert_eq!(cx.unify(&InferTy::ERROR, &shape).unwrap(), Ok(()));
        assert_eq!(cx.shallow(&a).unwrap(), InferTy::ERROR);
        assert_eq!(cx.shallow(&b).unwrap(), InferTy::ERROR);
    }

    #[test]
    fn a_failed_unification_binds_nothing() {
        let g = Globals::new();
        let mut cx = InferCtxt::new(&g);
        let a = cx.new_var(span(), VarKind::General).unwrap();
        let b = cx.new_var(span(), VarKind::General).unwrap();
        let left = InferTy::Tuple(Box::new([a.clone(), b.clone(), InferTy::STRING]));
        let right = InferTy::Tuple(Box::new([InferTy::STRING, a.clone(), InferTy::NIL]));
        assert_eq!(cx.unify(&left, &right).unwrap(), Err(Mismatch::Types));
        assert!(matches!(cx.shallow(&a).unwrap(), InferTy::Var(_)));
        assert!(matches!(cx.shallow(&b).unwrap(), InferTy::Var(_)));
        assert_ne!(cx.shallow(&a).unwrap(), cx.shallow(&b).unwrap());
    }

    #[test]
    fn error_unifies_with_everything() {
        let g = Globals::new();
        let mut cx = InferCtxt::new(&g);
        assert_eq!(cx.unify(&InferTy::ERROR, &InferTy::STRING).unwrap(), Ok(()));
        let list_err = g.types.named(builtin::LIST, [Ty::ERROR]);
        let list_str = g.types.named(builtin::LIST, [Ty::STRING]);
        assert_eq!(
            cx.unify(&InferTy::Known(list_err), &InferTy::Known(list_str))
                .unwrap(),
            Ok(())
        );
        let lit = cx.new_var(span(), VarKind::IntLit).unwrap();
        assert_eq!(cx.unify(&lit, &InferTy::ERROR).unwrap(), Ok(()));
    }

    #[test]
    fn literal_variables_and_defaulting() {
        let g = Globals::new();
        let mut cx = InferCtxt::new(&g);
        let int = cx.new_var(span(), VarKind::IntLit).unwrap();
        let float = cx.new_var(span(), VarKind::FloatLit).unwrap();
        let general = cx.new_var(span(), VarKind::General).unwrap();
        assert_eq!(cx.unify(&int, &float).unwrap(), Err(Mismatch::Types));
        assert_eq!(
            cx.unify(&int, &InferTy::STRING).unwrap(),
            Err(Mismatch::Types)
        );
        assert_eq!(
            cx.unify(&float, &InferTy::Known(Ty::INT)).unwrap(),
            Err(Mismatch::Types)
        );
        // A general variable joined with a literal one becomes a literal.
        assert_eq!(cx.unify(&general, &int).unwrap(), Ok(()));
        assert_eq!(
            cx.unify(&general, &InferTy::NIL).unwrap(),
            Err(Mismatch::Types)
        );
        let other = cx.new_var(span(), VarKind::IntLit).unwrap();
        assert_eq!(cx.unify(&other, &InferTy::Known(Ty::U8)).unwrap(), Ok(()));
        cx.default_literals().unwrap();
        let mut unresolved = Vec::new();
        assert_eq!(cx.zonk(&general, &mut unresolved).unwrap(), Ty::INT);
        assert_eq!(cx.zonk(&float, &mut unresolved).unwrap(), Ty::FLOAT);
        assert_eq!(cx.zonk(&other, &mut unresolved).unwrap(), Ty::U8);
        assert_eq!(unresolved, []);
    }

    #[test]
    fn unbound_variables_zonk_to_error_once() {
        let g = Globals::new();
        let mut cx = InferCtxt::new(&g);
        let v = cx.new_var(span(), VarKind::General).unwrap();
        let w = cx.new_var(span(), VarKind::General).unwrap();
        assert_eq!(cx.unify(&v, &w).unwrap(), Ok(()));
        let pair = InferTy::Tuple(Box::new([v.clone(), w]));
        let mut unresolved = Vec::new();
        let ty = cx.zonk(&pair, &mut unresolved).unwrap();
        assert_eq!(
            g.types.kind(ty),
            &TyKind::Tuple(Box::new([Ty::ERROR, Ty::ERROR]))
        );
        assert_eq!(unresolved.len(), 1);
        assert_eq!(cx.origin(unresolved[0]), Some(span()));
    }

    #[test]
    fn long_chains_compress() {
        let g = Globals::new();
        let mut cx = InferCtxt::new(&g);
        let vars: Vec<InferTy> = (0..200)
            .map(|_| cx.new_var(span(), VarKind::General).unwrap())
            .collect();
        for pair in vars.windows(2) {
            assert_eq!(cx.unify(&pair[0], &pair[1]).unwrap(), Ok(()));
        }
        assert_eq!(cx.unify(&vars[0], &InferTy::STRING).unwrap(), Ok(()));
        for v in &vars {
            assert_eq!(cx.shallow(v).unwrap(), InferTy::STRING);
        }
        assert_eq!(cx.var_count(), 200);
    }

    #[test]
    fn instantiation_and_substitution() {
        let g = Globals::new();
        let t = &g.types;
        let p0 = t.param(ParamIdx::from_u32(0));
        let p1 = t.param(ParamIdx::from_u32(1));
        let sig = t.func(
            [t.named(builtin::LIST, [p0])],
            t.named(builtin::RESULT, [p0, p1]),
        );
        assert!(mentions_param(t, sig));
        assert!(!mentions_param(t, Ty::STRING));
        let closed = substitute(t, sig, &[Ty::INT, Ty::STRING]);
        assert_eq!(
            closed,
            t.func(
                [t.named(builtin::LIST, [Ty::INT])],
                t.named(builtin::RESULT, [Ty::INT, Ty::STRING])
            )
        );
        let mut cx = InferCtxt::new(&g);
        let v = cx.new_var(span(), VarKind::General).unwrap();
        let inst = instantiate(t, sig, &[v.clone(), InferTy::NIL]);
        assert_eq!(
            cx.unify(&inst, &InferTy::Known(closed)).unwrap(),
            Err(Mismatch::Types)
        );
        let expected = substitute(t, sig, &[Ty::INT, Ty::NIL]);
        assert_eq!(cx.unify(&inst, &InferTy::Known(expected)).unwrap(), Ok(()));
        let mut unresolved = Vec::new();
        assert_eq!(cx.zonk(&v, &mut unresolved).unwrap(), Ty::INT);
        assert!(contains_error(t, t.named(builtin::LIST, [Ty::ERROR])));
        assert!(!contains_error(t, closed));
    }

    #[test]
    fn display_shows_unknown_parts() {
        let g = Globals::new();
        let set = InterfaceSet::new(builtin::interface(&g));
        let modules = ModuleTable::new();
        let allowed = [ModuleId::BUILTIN];
        let deps = DepView::new(&set, &allowed);
        let dcx = DisplayCx {
            globals: &g,
            modules: &modules,
            own: None,
            deps: &deps,
            generics: None,
        };
        let mut cx = InferCtxt::new(&g);
        let v = cx.new_var(span(), VarKind::General).unwrap();
        let n = cx.new_var(span(), VarKind::IntLit).unwrap();
        let t = InferTy::Fn {
            params: Box::new([list(&g, v), n]),
            ret: Box::new(InferTy::Named {
                def: builtin::OPTION,
                args: Box::new([InferTy::STRING]),
            }),
        };
        assert_eq!(
            cx.display(&t, &dcx),
            "fn(List<_>, {integer}) -> Option<String>"
        );
    }
}
