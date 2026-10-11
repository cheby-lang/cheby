//! Written types to closed [`Ty`]s, for signatures and annotations (C2).
//!
//! Paths were resolved by `cheby_resolve`; this module checks what they
//! name: a type with the right number of type arguments (E0051, D-462),
//! and not an interface where a type is expected or the reverse (E0052).
//! Aliases are expanded with their type arguments substituted (§4.4,
//! D-081), so no alias ever reaches a [`Ty`]. A type with an error
//! contains [`Ty::ERROR`]; its diagnostic is already reported.
//!
//! # The API ([`TyLowering`])
//!
//! A [`TyLowering`] is built from the module's [`TypesInput`] and the path
//! resolutions of the code being lowered:
//!
//! - in a signature, `paths` is `input.resolved.sig_paths`; type
//!   parameters resolve to the item's own `ParamIdx`s and `Self` (inside an
//!   interface declaration) to `ParamIdx` 0;
//! - in a body, `paths` is the body's `BodyRes::paths`; type parameters
//!   resolve to the enclosing top-level item's `ParamIdx`s, so the result
//!   is a closed type over that item's generics, as `BodyCx::generics`
//!   expects.
//!
//! Methods:
//!
//! - [`TyLowering::lower_in_body`] lowers one type written in a body
//!   (`let` annotations, closure parameters, turbofish arguments) and
//!   checks every `dyn I` in it with `ifaces::check_object_safe` (E0064)
//!   against the module's own interface; it returns [`Ty::ERROR`] when one
//!   fails.
//! - [`TyLowering::ret_in_body`] lowers an optional `-> T` written in a
//!   body; an omitted return type is `Nil` (D-083).
//! - [`TyLowering::lower_sig`] and [`TyLowering::ret_sig`] are the
//!   signature versions: they collect the `dyn I`s instead, which
//!   [`crate::build_interface`] checks once the interface exists.
//! - [`TyLowering::interface`] resolves an interface named in a bound or an
//!   embedding (E0052 for anything else).
//!
//! Own-module aliases are expanded from the module's tree on demand, each
//! at most once per call, so lowering needs no table and the order of
//! items does not matter (§4.1.1). The alias's own errors are reported once,
//! when [`crate::build_interface`] lowers the alias item
//! ([`TyLowering::alias_def`]); an expansion is silent and gives
//! [`Ty::ERROR`] for an alias with errors.

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{
    AliasDef, DefId, DefIndex, DefKind, GenericParam, Generics, Ice, IfaceItem, IndexVec, ItemSig,
    ModuleInterface, ParamIdx, Ty, TyInterner, TyKind, builtin, sym,
};
use cheby_resolve::Res;
use cheby_syntax::NodeId;
use cheby_syntax::ast::{
    AliasDecl, AstNode, AstToken, InterfaceRef, Item, PathNode, RetType, Type, TypeParamList,
};
use rustc_hash::FxHashMap;

use crate::TypesInput;
use crate::infer::{contains_error, substitute};

/// Lowers the written types of one signature or body. See the module
/// documentation for which `paths` to give.
pub(crate) struct TyLowering<'a> {
    pub input: &'a TypesInput<'a>,
    /// The resolutions of the paths: `sig_paths` or a body's `paths`.
    pub paths: &'a FxHashMap<NodeId, Res>,
}

/// A `dyn I` written in a lowered type: the interface and the span of the
/// whole `dyn I`, for the object-safety check (E0064, §8.6).
pub(crate) type WrittenDyn = (DefId, Span);

/// Where an interface is named, for the wording of E0052.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum IfaceSite {
    /// A bound `T: I` (§8.4).
    Bound,
    /// `dyn I` (§8.6).
    Dyn,
    /// An embedding `interface J: I` (§8.3).
    Embed,
}

/// What a resolved type path names.
enum Named {
    /// A type with this many type parameters.
    Type {
        arity: usize,
    },
    /// An alias from a dependency's interface.
    Alias {
        arity: usize,
        target: Ty,
    },
    /// An alias of this module, expanded from the tree.
    OwnAlias(DefIndex),
    Interface,
    /// An erroneous item: silent.
    Error,
    Other(&'static str),
}

/// The expansion of one of this module's aliases.
#[derive(Clone, Copy)]
enum OwnAlias {
    Ok {
        arity: usize,
        target: Ty,
    },
    /// A cycle, an unused parameter or another error: uses are silent
    /// [`Ty::ERROR`]s.
    Bad,
}

/// A cycle of aliases found while lowering.
struct Cycle {
    /// The aliases from the first one on the cycle back to it.
    chain: Vec<DefIndex>,
    /// The path, at the reporting depth, whose expansion led to the cycle.
    span: Option<Span>,
}

/// The state of one lowering call.
struct Cx<'d> {
    diags: &'d mut Vec<Diagnostic>,
    dyns: Vec<WrittenDyn>,
    /// The own aliases being expanded, outermost first.
    stack: Vec<DefIndex>,
    /// Diagnostics and `dyn`s are kept only while `stack.len() <= base`:
    /// inside an alias's expansion they belong to the alias.
    base: usize,
    memo: FxHashMap<DefIndex, OwnAlias>,
    /// How many cycle hits were seen, to tell which expansions met one.
    hits: u32,
    /// The first cycle found.
    cycle: Option<Cycle>,
    /// The path at the reporting depth being expanded.
    entry: Option<Span>,
}

impl<'d> Cx<'d> {
    fn new(diags: &'d mut Vec<Diagnostic>) -> Self {
        Self {
            diags,
            dyns: Vec::new(),
            stack: Vec::new(),
            base: 0,
            memo: FxHashMap::default(),
            hits: 0,
            cycle: None,
            entry: None,
        }
    }

    const fn reporting(&self) -> bool {
        self.stack.len() <= self.base
    }

    fn report(&mut self, diag: Diagnostic) {
        if self.reporting() {
            self.diags.push(diag);
        }
    }
}

impl TyLowering<'_> {
    /// The type `ty`, written in a body. Errors are reported to `diags`,
    /// and the result then contains [`Ty::ERROR`]. Every `dyn I` is also
    /// checked for object safety (E0064), and a type with a failing one is
    /// [`Ty::ERROR`]. `own` is the module's own interface.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a path was not resolved, or an interface cannot be
    /// read.
    pub fn lower_in_body(
        &self,
        own: &ModuleInterface,
        ty: Type<'_>,
        diags: &mut Vec<Diagnostic>,
    ) -> Result<Ty, Ice> {
        let mut dyns = Vec::new();
        let lowered = self.lower_sig(ty, diags, &mut dyns)?;
        self.check_dyns(own, lowered, &dyns, diags)
    }

    /// The interface named by a bound `T: I` or an embedding
    /// `interface J: I`. `None` after an error, already reported.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the path was not resolved.
    pub fn interface(
        &self,
        iface: InterfaceRef<'_>,
        site: IfaceSite,
        diags: &mut Vec<Diagnostic>,
    ) -> Result<Option<DefId>, Ice> {
        let mut cx = Cx::new(diags);
        self.interface_in(iface, site, &mut cx)
    }

    /// A type written in a signature, like [`Self::lower_in_body`] except
    /// that the `dyn I`s written in it are pushed to `dyns`, to be checked
    /// once the module's interface exists.
    pub(crate) fn lower_sig(
        &self,
        ty: Type<'_>,
        diags: &mut Vec<Diagnostic>,
        dyns: &mut Vec<WrittenDyn>,
    ) -> Result<Ty, Ice> {
        let mut cx = Cx::new(diags);
        let lowered = self.lower_in(ty, &mut cx)?;
        dyns.append(&mut cx.dyns);
        Ok(lowered)
    }

    /// A return type written in a signature: the written type, or `Nil`
    /// when it is omitted (D-083), collecting `dyn`s like
    /// [`Self::lower_sig`].
    pub(crate) fn ret_sig(
        &self,
        ret: Option<RetType<'_>>,
        diags: &mut Vec<Diagnostic>,
        dyns: &mut Vec<WrittenDyn>,
    ) -> Result<Ty, Ice> {
        let mut cx = Cx::new(diags);
        let lowered = self.ret_in(ret, &mut cx)?;
        dyns.append(&mut cx.dyns);
        Ok(lowered)
    }

    /// Checks each of `dyns` for object safety; `ty` or [`Ty::ERROR`].
    fn check_dyns(
        &self,
        own: &ModuleInterface,
        ty: Ty,
        dyns: &[WrittenDyn],
        diags: &mut Vec<Diagnostic>,
    ) -> Result<Ty, Ice> {
        let mut ok = true;
        for &(iface, span) in dyns {
            if let Some(diag) = crate::ifaces::check_object_safe(self.input, own, iface, span)? {
                diags.push(diag);
                ok = false;
            }
        }
        Ok(if ok { ty } else { Ty::ERROR })
    }

    /// The definition of this module's alias `index`, declared by `decl`,
    /// with its errors reported: E0070 for an alias that refers to itself,
    /// directly or through other aliases, or has a type parameter its
    /// definition does not use (§4.4), and the errors of its written type.
    /// `None` if it has an error. The `dyn`s written in it go to `dyns`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a path was not resolved or `index` is not an alias.
    pub(crate) fn alias_def(
        &self,
        index: DefIndex,
        decl: AliasDecl<'_>,
        diags: &mut Vec<Diagnostic>,
        dyns: &mut Vec<WrittenDyn>,
    ) -> Result<Option<AliasDef>, Ice> {
        let mut cx = Cx::new(diags);
        cx.base = 1;
        cx.stack.push(index);
        let target = match decl.ty() {
            Some(ty) => self.lower_in(ty, &mut cx)?,
            // A missing type is a syntax error.
            None => Ty::ERROR,
        };
        cx.stack.pop();
        dyns.append(&mut cx.dyns);
        let generics = plain_generics(self.input, decl.type_params())?;
        let name = self.alias_name(index);
        if let Some(cycle) = cx.cycle.take()
            && cycle.chain.first() == Some(&index)
        {
            cx.diags.push(self.cycle_error(index, &name, &cycle));
            return Ok(None);
        }
        if contains_error(&self.input.globals.types, target) {
            return Ok(None);
        }
        let mut ok = true;
        if let Some(list) = decl.type_params() {
            let mut seen: Vec<&str> = Vec::new();
            for (n, param) in (0_u32..).zip(list.params()) {
                let Some(param_name) = param.name() else {
                    continue;
                };
                // A repeated name is E0041 from name resolution, and the
                // repetition binds nothing.
                if seen.contains(&param_name.text()) {
                    continue;
                }
                seen.push(param_name.text());
                if mentions(&self.input.globals.types, target, ParamIdx::from_u32(n)) {
                    continue;
                }
                ok = false;
                cx.diags.push(
                    Diagnostic::new(
                        codes::INVALID_ALIAS,
                        format!(
                            "the type parameter `{}` of the alias `{name}` is not used",
                            param_name.text()
                        ),
                    )
                    .with_primary(param_name.span(), "not used in the definition")
                    .with_note(
                        "an alias is replaced by its definition, so an unused parameter \
                         would make different-looking types the same (§4.4)",
                    )
                    .with_help(format!(
                        "use `{}` in the definition, or remove it",
                        param_name.text()
                    )),
                );
            }
        }
        Ok(ok.then_some(AliasDef { generics, target }))
    }

    fn alias_name(&self, index: DefIndex) -> String {
        self.input.resolved.item(index).map_or_else(
            || "?".to_owned(),
            |item| self.input.globals.names.get(item.name).to_owned(),
        )
    }

    fn cycle_error(&self, index: DefIndex, name: &str, cycle: &Cycle) -> Diagnostic {
        let mut diag = Diagnostic::new(
            codes::INVALID_ALIAS,
            format!("the type alias `{name}` refers to itself"),
        );
        let name_span = self.input.resolved.item(index).map(|item| item.name_span);
        if let Some(span) = name_span {
            diag = diag.with_primary(span, "");
        }
        let direct = cycle.chain.len() <= 2;
        if let Some(span) = cycle.span {
            let label = if direct {
                "used in its own definition here".to_owned()
            } else {
                "which leads back to it through this alias".to_owned()
            };
            diag = diag.with_secondary(span, label);
        }
        if !direct {
            let names: Vec<String> = cycle
                .chain
                .iter()
                .map(|&i| format!("`{}`", self.alias_name(i)))
                .collect();
            diag = diag.with_note(format!("the aliases form a cycle: {}", names.join(" → ")));
        }
        diag.with_help("an alias never finishes expanding if it contains itself; declare a type for a recursive structure (§4.4)")
    }

    fn ret_in(&self, ret: Option<RetType<'_>>, cx: &mut Cx<'_>) -> Result<Ty, Ice> {
        match ret {
            None => Ok(Ty::NIL),
            Some(ret) => match ret.ty() {
                Some(ty) => self.lower_in(ty, cx),
                // `->` without a type is a syntax error.
                None => Ok(Ty::ERROR),
            },
        }
    }

    fn lower_in(&self, ty: Type<'_>, cx: &mut Cx<'_>) -> Result<Ty, Ice> {
        let types = &self.input.globals.types;
        match ty {
            Type::Path(path_ty) => {
                let args = match path_ty.type_args() {
                    Some(list) => Some(
                        list.types()
                            .map(|arg| self.lower_in(arg, cx))
                            .collect::<Result<Vec<Ty>, Ice>>()?,
                    ),
                    None => None,
                };
                let Some(path) = path_ty.path() else {
                    return Ok(Ty::ERROR);
                };
                self.path_type(path, args, path_ty.significant_span(), cx)
            }
            Type::Tuple(tuple) => {
                let elems = tuple
                    .elements()
                    .map(|elem| self.lower_in(elem, cx))
                    .collect::<Result<Box<[Ty]>, Ice>>()?;
                // `(T)` and `()` are syntax errors (E0030, D-436).
                if elems.len() < 2 {
                    return Ok(Ty::ERROR);
                }
                Ok(types.intern(TyKind::Tuple(elems)))
            }
            Type::Fn(f) => {
                let params = match f.param_types() {
                    Some(list) => list
                        .types()
                        .map(|param| self.lower_in(param, cx))
                        .collect::<Result<Vec<Ty>, Ice>>()?,
                    None => Vec::new(),
                };
                let ret = self.ret_in(f.ret_type(), cx)?;
                Ok(types.func(params, ret))
            }
            Type::Dyn(d) => match d.interface() {
                Some(iface) => match self.interface_in(iface, IfaceSite::Dyn, cx)? {
                    Some(def) => {
                        if cx.reporting() {
                            cx.dyns.push((def, d.significant_span()));
                        }
                        Ok(types.intern(TyKind::Dyn {
                            iface: def,
                            args: Box::new([]),
                        }))
                    }
                    None => Ok(Ty::ERROR),
                },
                None => Ok(Ty::ERROR),
            },
        }
    }

    fn res(&self, path: PathNode<'_>) -> Result<Res, Ice> {
        self.paths
            .get(&path.syntax().id())
            .copied()
            .ok_or_else(|| Ice::new("a type path was not resolved"))
    }

    fn path_type(
        &self,
        path: PathNode<'_>,
        args: Option<Vec<Ty>>,
        span: Span,
        cx: &mut Cx<'_>,
    ) -> Result<Ty, Ice> {
        let types = &self.input.globals.types;
        let text = path_text(path);
        let given = args.as_ref().map_or(0, Vec::len);
        match self.res(path)? {
            Res::Err => Ok(Ty::ERROR),
            Res::TyParam(param) => {
                if given > 0 {
                    cx.report(arity_error(
                        span,
                        &text,
                        0,
                        given,
                        "a type parameter takes no type arguments",
                    ));
                    return Ok(Ty::ERROR);
                }
                Ok(types.param(param))
            }
            // `Self` is type parameter 0 of an interface function (D-063).
            Res::SelfTy => {
                if given > 0 {
                    cx.report(arity_error(
                        span,
                        &text,
                        0,
                        given,
                        "`Self` takes no type arguments",
                    ));
                    return Ok(Ty::ERROR);
                }
                Ok(types.param(ParamIdx::from_u32(0)))
            }
            Res::Def(def) => match self.named(def)? {
                Named::Error => Ok(Ty::ERROR),
                Named::Type { arity } => {
                    if given != arity {
                        cx.report(arity_error(
                            span,
                            &text,
                            arity,
                            given,
                            "this type has no type parameters",
                        ));
                        return Ok(Ty::ERROR);
                    }
                    if def == builtin::NEVER {
                        return Ok(Ty::NEVER);
                    }
                    Ok(types.named(def, args.unwrap_or_default()))
                }
                Named::Alias { arity, target } => {
                    if given != arity {
                        cx.report(arity_error(
                            span,
                            &text,
                            arity,
                            given,
                            "this alias has no type parameters",
                        ));
                        return Ok(Ty::ERROR);
                    }
                    Ok(substitute(types, target, &args.unwrap_or_default()))
                }
                Named::OwnAlias(index) => {
                    let expanded = self.expand_at(index, span, cx)?;
                    match expanded {
                        OwnAlias::Bad => Ok(Ty::ERROR),
                        OwnAlias::Ok { arity, target } => {
                            if given != arity {
                                cx.report(arity_error(
                                    span,
                                    &text,
                                    arity,
                                    given,
                                    "this alias has no type parameters",
                                ));
                                return Ok(Ty::ERROR);
                            }
                            Ok(substitute(types, target, &args.unwrap_or_default()))
                        }
                    }
                }
                Named::Interface => {
                    cx.report(interface_as_type(span, &text));
                    Ok(Ty::ERROR)
                }
                Named::Other(what) => Err(Ice::new(format!("a type path resolved to a {what}"))),
            },
            Res::Local(_) | Res::Module(_) => {
                Err(Ice::new("a type path resolved to a value or a module"))
            }
        }
    }

    /// Expands this module's alias `index`, written at `span`, remembering
    /// the span for a cycle's report.
    fn expand_at(&self, index: DefIndex, span: Span, cx: &mut Cx<'_>) -> Result<OwnAlias, Ice> {
        let entry = cx.entry;
        if cx.reporting() {
            cx.entry = Some(span);
        }
        let expanded = self.own_alias(index, cx);
        cx.entry = entry;
        expanded
    }

    /// Expands this module's alias `index`, once per lowering call.
    fn own_alias(&self, index: DefIndex, cx: &mut Cx<'_>) -> Result<OwnAlias, Ice> {
        if let Some(pos) = cx.stack.iter().position(|&i| i == index) {
            cx.hits = cx.hits.saturating_add(1);
            if cx.cycle.is_none() {
                let mut chain: Vec<DefIndex> = cx.stack.iter().skip(pos).copied().collect();
                chain.push(index);
                cx.cycle = Some(Cycle {
                    chain,
                    span: cx.entry,
                });
            }
            return Ok(OwnAlias::Bad);
        }
        if let Some(&known) = cx.memo.get(&index) {
            return Ok(known);
        }
        let decl = self.alias_decl(index)?;
        let hits = cx.hits;
        cx.stack.push(index);
        // The alias's definition is part of the module's signatures, also
        // when the alias is used in a body.
        let in_sig = TyLowering {
            input: self.input,
            paths: &self.input.resolved.sig_paths,
        };
        let target = match decl.ty() {
            Some(ty) => in_sig.lower_in(ty, cx),
            None => Ok(Ty::ERROR),
        };
        cx.stack.pop();
        let target = target?;
        let types = &self.input.globals.types;
        let arity = decl.type_params().map_or(0, |list| list.params().count());
        let all_used = (0..arity).all(|n| {
            u32::try_from(n).is_ok_and(|n| mentions(types, target, ParamIdx::from_u32(n)))
        });
        let expanded = if cx.hits == hits && all_used && !contains_error(types, target) {
            OwnAlias::Ok { arity, target }
        } else {
            OwnAlias::Bad
        };
        cx.memo.insert(index, expanded);
        Ok(expanded)
    }

    fn alias_decl(&self, index: DefIndex) -> Result<AliasDecl<'_>, Ice> {
        let item = self
            .input
            .resolved
            .item(index)
            .ok_or_else(|| Ice::new("an alias path resolved to a missing item"))?;
        match self.input.tree.node(item.node).and_then(Item::cast) {
            Some(Item::Alias(decl)) => Ok(decl),
            _ => Err(Ice::new("an alias item's node is not an alias declaration")),
        }
    }

    fn interface_in(
        &self,
        iface: InterfaceRef<'_>,
        site: IfaceSite,
        cx: &mut Cx<'_>,
    ) -> Result<Option<DefId>, Ice> {
        let Some(path) = iface.path() else {
            return Ok(None);
        };
        let span = iface.significant_span();
        let text = path_text(path);
        // Type arguments on an interface are a syntax error (E0023,
        // D-462); interfaces have none (D-165).
        let def = match self.res(path)? {
            Res::Err => return Ok(None),
            Res::Def(def) => def,
            Res::TyParam(_) | Res::SelfTy => {
                cx.report(not_an_interface(span, &text, site));
                return Ok(None);
            }
            Res::Local(_) | Res::Module(_) => {
                return Err(Ice::new(
                    "an interface path resolved to a value or a module",
                ));
            }
        };
        match self.named(def)? {
            Named::Error => Ok(None),
            Named::Interface => Ok(Some(def)),
            Named::Type { .. } | Named::Alias { .. } | Named::OwnAlias(_) => {
                cx.report(not_an_interface(span, &text, site));
                Ok(None)
            }
            Named::Other(what) => Err(Ice::new(format!("an interface path resolved to a {what}"))),
        }
    }

    /// What `def` is, as far as written types care.
    fn named(&self, def: DefId) -> Result<Named, Ice> {
        if def.module == self.input.module {
            let item = self
                .input
                .resolved
                .item(def.index)
                .ok_or_else(|| Ice::new("a type path resolved to a missing item"))?;
            return Ok(match item.kind {
                DefKind::Type => Named::Type {
                    arity: own_arity(self.input, def.index)?,
                },
                DefKind::Alias => Named::OwnAlias(def.index),
                DefKind::Interface => Named::Interface,
                other => Named::Other(other.describe()),
            });
        }
        let item: &IfaceItem = self
            .input
            .deps
            .item(def)
            .ok_or_else(|| Ice::new(format!("no interface item for {def:?}")))?;
        Ok(match (&item.sig, item.kind) {
            (ItemSig::Error, _) => Named::Error,
            (ItemSig::Type(ty), _) => Named::Type {
                arity: ty.generics.len(),
            },
            (ItemSig::Alias(alias), _) => Named::Alias {
                arity: alias.generics.len(),
                target: alias.target,
            },
            (ItemSig::Interface(_), _) => Named::Interface,
            (_, kind) => Named::Other(kind.describe()),
        })
    }
}

/// The number of type parameters declared by this module's type or alias
/// `index`, counted as written, so that `ParamIdx` `n` is the `n`th one.
fn own_arity(input: &TypesInput<'_>, index: DefIndex) -> Result<usize, Ice> {
    let item = input
        .resolved
        .item(index)
        .ok_or_else(|| Ice::new("a missing item"))?;
    let list = match input.tree.node(item.node).and_then(Item::cast) {
        Some(Item::Type(decl)) => decl.type_params(),
        Some(Item::Alias(decl)) => decl.type_params(),
        Some(Item::Fn(decl)) => decl.type_params(),
        _ => return Err(Ice::new("a type item's node is not a declaration")),
    };
    Ok(list.map_or(0, |list| list.params().count()))
}

/// The type parameters of a type or alias declaration, which have no
/// bounds (§4.3, the syntax layer rejects them, E0023).
pub(crate) fn plain_generics(
    input: &TypesInput<'_>,
    list: Option<TypeParamList<'_>>,
) -> Result<Generics, Ice> {
    let params: Vec<GenericParam> = list
        .into_iter()
        .flat_map(TypeParamList::params)
        .map(|param| GenericParam {
            name: param
                .name()
                .map_or(sym::UNDERSCORE, |n| input.globals.names.intern(n.text())),
            bounds: Box::new([]),
        })
        .collect();
    Ok(Generics {
        params: IndexVec::from_raw(params)?,
    })
}

/// Whether `ty` mentions type parameter `param`.
pub(crate) fn mentions(types: &TyInterner, ty: Ty, param: ParamIdx) -> bool {
    match types.kind(ty) {
        TyKind::Param(p) => *p == param,
        TyKind::Named { args: parts, .. }
        | TyKind::Tuple(parts)
        | TyKind::Dyn { args: parts, .. } => parts.iter().any(|&t| mentions(types, t, param)),
        TyKind::Fn { params, ret } => {
            params.iter().any(|&t| mentions(types, t, param)) || mentions(types, *ret, param)
        }
        TyKind::Error | TyKind::Never => false,
    }
}

/// The path as written, without trivia: `map::Map`.
pub(crate) fn path_text(path: PathNode<'_>) -> String {
    let segments: Vec<&str> = path.segments().map(AstToken::text).collect();
    segments.join("::")
}

/// E0051. `none` is the label when `expected` is 0.
fn arity_error(span: Span, text: &str, expected: usize, given: usize, none: &str) -> Diagnostic {
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let message = if expected == 0 {
        format!("`{text}` takes no type arguments")
    } else {
        format!(
            "`{text}` takes {expected} type argument{} but {given} {} given",
            plural(expected),
            if given == 1 { "was" } else { "were" }
        )
    };
    let label = if expected == 0 {
        none.to_owned()
    } else {
        format!("expected {expected} type argument{}", plural(expected))
    };
    Diagnostic::new(codes::WRONG_TYPE_ARGUMENT_COUNT, message).with_primary(span, label)
}

fn interface_as_type(span: Span, text: &str) -> Diagnostic {
    Diagnostic::new(
        codes::NOT_A_TYPE,
        format!("`{text}` is an interface, not a type"),
    )
    .with_primary(span, "expected a type")
    .with_help(format!(
        "write `dyn {text}` for a value of any type that satisfies it, or use a type parameter \
         bounded by it, such as `T: {text}`"
    ))
}

fn not_an_interface(span: Span, text: &str, site: IfaceSite) -> Diagnostic {
    let note = match site {
        IfaceSite::Dyn => "only an interface may follow `dyn` (§8.6)",
        IfaceSite::Bound => "a bound names interfaces the type parameter must satisfy (§8.4)",
        IfaceSite::Embed => "an interface may embed only other interfaces (§8.3)",
    };
    Diagnostic::new(codes::NOT_A_TYPE, format!("`{text}` is not an interface"))
        .with_primary(span, "expected an interface")
        .with_note(note)
}
