//! Signatures (C2): every item's [`ItemSig`] and the module's interface.
//!
//! Only declarations are read, never bodies, so editing a body never
//! changes the interface or its hash (B8, D-246, D-460).
//!
//! - Functions declare every parameter type, and an omitted return type
//!   means `Nil` (D-011, D-083). Bounds name interfaces (§8.4, E0052).
//!   Every top-level `main` must have the signature of an entry point
//!   (E0050, D-448).
//! - Types: generics without bounds, constructors in declaration order,
//!   the single-variant shorthand (D-082), `exposed`, and external types
//!   without a body (§12.5.3).
//! - Aliases are expanded wherever they are used (§4.4); their own errors
//!   (E0070) are reported here.
//! - Constants carry their declared type (D-121). Interfaces carry their
//!   embeddings and functions, whose `Self` is `ParamIdx` 0 (D-063).
//!
//! A function, alias or constant whose written types have an error becomes
//! [`ItemSig::Error`], so its uses are silent (GAP(signature-error-shape)),
//! and so does a function whose bound fails to lower
//! (GAP(failed-bound-signature)). A constructor or an
//! interface function keeps its shape, with [`Ty::ERROR`] in place of the
//! erroneous types.
//!
//! The module-wide checks run at the end: object safety of every `dyn I`
//! written in a signature (E0064), declaration checks (`decl`) and
//! interface declarations (`ifaces`).

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{
    ConstSig, CtorFields, CtorSig, DefIndex, DefKind, FnSig, GenericParam, Generics, Ice,
    IfaceItem, IndexVec, InterfaceDef, InterfaceFnSig, ItemSig, ModuleInterface, Name, ParamIdx,
    ParamSig, Ty, TyKind, TypeDef, TypeDefKind, TypeHooks, builtin, interface_hash, sym,
};
use cheby_resolve::ItemDef;
use cheby_syntax::ast::{
    AstNode, AstToken, FnDecl, InterfaceFn, Item, RecordFieldList, RetType, Type, Variant,
    VariantFields,
};

use crate::infer::contains_error;
use crate::lower_ty::{IfaceSite, TyLowering, WrittenDyn, plain_generics};
use crate::{TypesInput, decl, ifaces};

/// Lowers every item's signature and returns the module's interface, with
/// its hash (B8). Items are in `DefIndex` order, private ones included.
///
/// # Errors
///
/// An [`Ice`] if the resolution and the tree disagree, or a path was not
/// resolved.
pub fn build_interface(input: &TypesInput<'_>) -> Result<(ModuleInterface, Vec<Diagnostic>), Ice> {
    let resolved = input.resolved;
    let mut children: Vec<Vec<DefIndex>> = vec![Vec::new(); resolved.items.len()];
    for (index, item) in resolved.items.iter_enumerated() {
        if let Some(parent) = item.parent {
            let siblings = usize::try_from(parent.as_u32())
                .ok()
                .and_then(|p| children.get_mut(p))
                .ok_or_else(|| Ice::new("an item's parent is missing"))?;
            siblings.push(index);
        }
    }
    let mut cx = SigCx {
        input,
        lowering: TyLowering {
            input,
            paths: &resolved.sig_paths,
        },
        children: &children,
        diags: Vec::new(),
        dyns: Vec::new(),
    };
    let mut items = Vec::with_capacity(resolved.items.len());
    for (index, item) in resolved.items.iter_enumerated() {
        let sig = cx.item_sig(index, item)?;
        items.push(iface_item(item, sig));
    }
    let SigCx {
        mut diags, dyns, ..
    } = cx;
    let mut iface = ModuleInterface::new(input.module, input.package, IndexVec::from_raw(items)?)?;
    iface.hash = Some(interface_hash(
        &iface,
        &input.deps,
        input.modules,
        input.globals,
    )?);
    for (def, span) in dyns {
        if let Some(diag) = ifaces::check_object_safe(input, &iface, def, span)? {
            diags.push(diag);
        }
    }
    diags.extend(decl::check_decls(input, &iface)?);
    diags.extend(ifaces::check_interface_decls(input, &iface)?);
    Ok((iface, diags))
}

fn iface_item(item: &ItemDef, sig: ItemSig) -> IfaceItem {
    IfaceItem {
        name: item.name,
        kind: item.kind,
        vis: item.vis,
        parent: item.parent,
        span: Some(item.name_span),
        targets: item.attrs.targets,
        deprecated: item.attrs.deprecated.clone(),
        sig,
    }
}

/// The state of [`build_interface`].
struct SigCx<'a> {
    input: &'a TypesInput<'a>,
    lowering: TyLowering<'a>,
    /// The constructors of each type and the functions of each interface,
    /// in declaration order, by parent index.
    children: &'a [Vec<DefIndex>],
    diags: Vec<Diagnostic>,
    /// Every `dyn I` written in a signature, checked at the end.
    dyns: Vec<WrittenDyn>,
}

impl<'a> SigCx<'a> {
    fn item_sig(&mut self, index: DefIndex, item: &ItemDef) -> Result<ItemSig, Ice> {
        match item.kind {
            DefKind::Fn => self.fn_sig(item),
            DefKind::Test => Ok(ItemSig::Test),
            DefKind::Type => self.type_def(index, item),
            DefKind::Ctor => self.ctor_sig(index, item),
            DefKind::Alias => self.alias_def(index, item),
            DefKind::Const => self.const_sig(item),
            DefKind::Interface => self.interface_def(index, item),
            DefKind::InterfaceFn => self.interface_fn_sig(item),
        }
    }

    fn item_node(&self, item: &ItemDef) -> Result<Item<'a>, Ice> {
        self.input
            .tree
            .node(item.node)
            .and_then(Item::cast)
            .ok_or_else(|| Ice::new("an item's node is not an item"))
    }

    fn children_of(&self, index: DefIndex) -> Result<&[DefIndex], Ice> {
        usize::try_from(index.as_u32())
            .ok()
            .and_then(|i| self.children.get(i))
            .map(Vec::as_slice)
            .ok_or_else(|| Ice::new("an item without a children list"))
    }

    fn lower(&mut self, ty: Option<Type<'_>>) -> Result<Ty, Ice> {
        match ty {
            Some(ty) => self.lowering.lower_sig(ty, &mut self.diags, &mut self.dyns),
            // A missing type is a syntax error (E0020).
            None => Ok(Ty::ERROR),
        }
    }

    fn fn_sig(&mut self, item: &ItemDef) -> Result<ItemSig, Ice> {
        let decl = fn_decl(self.input, item)?;
        let sig = lower_fn_sig_into(self.input, decl, &mut self.diags, &mut self.dyns)?;
        if item.name == sym::MAIN && !main_ok(self.input, &sig, item, &mut self.diags) {
            return Ok(ItemSig::Error);
        }
        if sig.has_error(self.input) {
            return Ok(ItemSig::Error);
        }
        let clamped = clamped_params(&sig.params, item);
        Ok(ItemSig::Fn(FnSig {
            generics: sig.generics,
            params: sig.params.into(),
            ret: sig.ret,
            clamped,
        }))
    }

    fn type_def(&mut self, index: DefIndex, item: &ItemDef) -> Result<ItemSig, Ice> {
        let Item::Type(decl) = self.item_node(item)? else {
            return Err(Ice::new("a type item's node is not a type declaration"));
        };
        let generics = plain_generics(self.input, decl.type_params())?;
        // A type without a body is an external type, whose values are
        // foreign handles (§12.5.3); its `@external` is optional.
        let kind = match decl.body() {
            None => TypeDefKind::External,
            Some(_) => TypeDefKind::Adt {
                ctors: self.children_of(index)?.into(),
            },
        };
        let hooks = item.attrs.hooks.map(|[equal, hash, debug]| TypeHooks {
            equal: equal.0,
            hash: hash.0,
            debug: debug.0,
        });
        Ok(ItemSig::Type(TypeDef {
            generics,
            kind,
            exposed: decl.is_exposed(),
            home: None,
            hooks,
        }))
    }

    fn ctor_sig(&mut self, index: DefIndex, item: &ItemDef) -> Result<ItemSig, Ice> {
        let ty = item
            .parent
            .ok_or_else(|| Ice::new("a constructor without a type"))?;
        let tag = self
            .children_of(ty)?
            .iter()
            .position(|&c| c == index)
            .and_then(|tag| u32::try_from(tag).ok())
            .ok_or_else(|| Ice::new("a constructor missing from its type"))?;
        let node = self
            .input
            .tree
            .node(item.node)
            .ok_or_else(|| Ice::new("a constructor without a node"))?;
        let fields = if let Some(variant) = Variant::cast(node) {
            match variant.fields() {
                None => CtorFields::Unit,
                Some(VariantFields::Positional(list)) => {
                    let mut tys = Vec::new();
                    for field in list.types() {
                        tys.push(self.lower(Some(field))?);
                    }
                    CtorFields::Positional(tys.into())
                }
                Some(VariantFields::Record(list)) => self.named_fields(list)?,
            }
        } else if let Some(list) = RecordFieldList::cast(node) {
            // The single-variant shorthand (§4.3.3, D-082).
            self.named_fields(list)?
        } else {
            return Err(Ice::new("a constructor's node is not a variant"));
        };
        Ok(ItemSig::Ctor(CtorSig { ty, tag, fields }))
    }

    fn named_fields(&mut self, list: RecordFieldList<'_>) -> Result<CtorFields, Ice> {
        let mut fields = Vec::new();
        for field in list.fields() {
            let name = intern_or_underscore(self.input, field.name().map(AstToken::text));
            let ty = self.lower(field.ty())?;
            fields.push((name, ty));
        }
        Ok(CtorFields::Named(fields.into()))
    }

    fn alias_def(&mut self, index: DefIndex, item: &ItemDef) -> Result<ItemSig, Ice> {
        let Item::Alias(decl) = self.item_node(item)? else {
            return Err(Ice::new("an alias item's node is not an alias declaration"));
        };
        let def = self
            .lowering
            .alias_def(index, decl, &mut self.diags, &mut self.dyns)?;
        Ok(def.map_or(ItemSig::Error, ItemSig::Alias))
    }

    fn const_sig(&mut self, item: &ItemDef) -> Result<ItemSig, Ice> {
        let Item::Const(decl) = self.item_node(item)? else {
            return Err(Ice::new(
                "a constant item's node is not a constant declaration",
            ));
        };
        let ty = self.lower(decl.ty())?;
        if contains_error(&self.input.globals.types, ty) {
            return Ok(ItemSig::Error);
        }
        Ok(ItemSig::Const(ConstSig { ty }))
    }

    fn interface_def(&mut self, index: DefIndex, item: &ItemDef) -> Result<ItemSig, Ice> {
        let Item::Interface(decl) = self.item_node(item)? else {
            return Err(Ice::new("an interface item's node is not an interface"));
        };
        let mut embeds = Vec::new();
        if let Some(bounds) = decl.bounds() {
            for iface in bounds.interfaces() {
                if let Some(def) =
                    self.lowering
                        .interface(iface, IfaceSite::Embed, &mut self.diags)?
                {
                    embeds.push(def);
                }
            }
        }
        Ok(ItemSig::Interface(InterfaceDef {
            embeds: embeds.into(),
            fns: self.children_of(index)?.into(),
        }))
    }

    fn interface_fn_sig(&mut self, item: &ItemDef) -> Result<ItemSig, Ice> {
        let f = self
            .input
            .tree
            .node(item.node)
            .and_then(InterfaceFn::cast)
            .ok_or_else(|| Ice::new("an interface function's node is not one"))?;
        let mut params = Vec::new();
        if let Some(list) = f.param_types() {
            for ty in list.types() {
                params.push(self.lower(Some(ty))?);
            }
        }
        // A missing return type means `Nil` (D-083).
        let ret = self
            .lowering
            .ret_sig(f.ret_type(), &mut self.diags, &mut self.dyns)?;
        Ok(ItemSig::InterfaceFn(InterfaceFnSig {
            params: params.into(),
            ret,
        }))
    }
}

fn intern_or_underscore(input: &TypesInput<'_>, text: Option<&str>) -> Name {
    text.map_or(sym::UNDERSCORE, |t| input.globals.names.intern(t))
}

/// A function's signature as written, lowered.
pub(crate) struct LoweredFnSig {
    pub generics: Generics,
    pub params: Vec<ParamSig>,
    pub ret: Ty,
    /// The written return type, if any.
    pub ret_span: Option<Span>,
    /// The `(…)` parameter list.
    pub param_list_span: Option<Span>,
    /// The `<…>` type parameter list, if written.
    pub type_params_span: Option<Span>,
    /// The type parameters with a bound that failed to lower (already
    /// reported). They are missing that bound in `generics`.
    pub bound_errors: Vec<ParamIdx>,
}

impl LoweredFnSig {
    /// Whether any part has an error, already reported. A bound that failed
    /// to lower counts: the function's interface would otherwise promise
    /// less than it was written with.
    pub fn has_error(&self, input: &TypesInput<'_>) -> bool {
        let types = &input.globals.types;
        !self.bound_errors.is_empty()
            || self.params.iter().any(|p| contains_error(types, p.ty))
            || contains_error(types, self.ret)
    }
}

/// The function declaration of item `item`.
pub(crate) fn fn_decl<'t>(input: &TypesInput<'t>, item: &ItemDef) -> Result<FnDecl<'t>, Ice> {
    input
        .tree
        .node(item.node)
        .and_then(FnDecl::cast)
        .ok_or_else(|| Ice::new("a function item's node is not a function declaration"))
}

/// Lowers a top-level function's written signature. Errors go to `diags`.
/// `dyn` types are not checked for object safety here; that happens once,
/// in [`build_interface`].
pub(crate) fn lower_fn_sig(
    input: &TypesInput<'_>,
    decl: FnDecl<'_>,
    diags: &mut Vec<Diagnostic>,
) -> Result<LoweredFnSig, Ice> {
    lower_fn_sig_into(input, decl, diags, &mut Vec::new())
}

fn lower_fn_sig_into(
    input: &TypesInput<'_>,
    decl: FnDecl<'_>,
    diags: &mut Vec<Diagnostic>,
    dyns: &mut Vec<WrittenDyn>,
) -> Result<LoweredFnSig, Ice> {
    let lowering = TyLowering {
        input,
        paths: &input.resolved.sig_paths,
    };
    let mut params_out = Vec::new();
    let mut bound_errors = Vec::new();
    if let Some(list) = decl.type_params() {
        for param in list.params() {
            let name = intern_or_underscore(input, param.name().map(AstToken::text));
            let mut bounds = Vec::new();
            if let Some(list) = param.bounds() {
                for iface in list.interfaces() {
                    if let Some(def) = lowering.interface(iface, IfaceSite::Bound, diags)? {
                        bounds.push(def);
                    } else {
                        let index = u32::try_from(params_out.len())
                            .map_err(|_| Ice::new("too many type parameters"))?;
                        bound_errors.push(ParamIdx::from_u32(index));
                    }
                }
            }
            params_out.push(GenericParam {
                name,
                bounds: bounds.into(),
            });
        }
    }
    let generics = Generics {
        params: IndexVec::from_raw(params_out)?,
    };
    let mut params = Vec::new();
    for param in decl.params() {
        let name = intern_or_underscore(input, param.name().map(AstToken::text));
        let ty = match param.ty() {
            Some(ty) => lowering.lower_sig(ty, diags, dyns)?,
            // A missing type is a syntax error (E0020).
            None => Ty::ERROR,
        };
        params.push(ParamSig { name, ty });
    }
    let ret_type = decl.ret_type();
    let ret = lowering.ret_sig(ret_type, diags, dyns)?;
    Ok(LoweredFnSig {
        generics,
        params,
        ret,
        ret_span: ret_type.and_then(RetType::ty).map(Type::significant_span),
        param_list_span: decl.param_list().map(AstNode::significant_span),
        type_params_span: decl.type_params().map(AstNode::significant_span),
        bound_errors,
    })
}

/// The indexes of the parameters named by `@std_clamped(…)`, sorted and
/// without duplicates (D-298, D-303). The resolver has checked that each
/// name is a parameter; a name that is not is skipped.
fn clamped_params(params: &[ParamSig], item: &ItemDef) -> Box<[u32]> {
    let mut indexes: Vec<u32> = item
        .attrs
        .clamped
        .iter()
        .filter_map(|&(name, _)| params.iter().position(|p| p.name == name))
        .filter_map(|index| u32::try_from(index).ok())
        .collect();
    indexes.sort_unstable();
    indexes.dedup();
    indexes.into()
}

/// Checks the signature of a top-level `main`: no type parameters, no
/// parameters, and `Nil` or `Result<Nil, E>` (§4.2.1, D-073, D-448). Any
/// module's `main` can be run (D-137), so every one is checked. Visibility
/// is free (D-163).
///
/// A `main` with type parameters is rejected too: `cheby run` has no way
/// to choose its type arguments or pass dictionaries for its bounds
/// (GAP(main-type-params)).
fn main_ok(
    input: &TypesInput<'_>,
    sig: &LoweredFnSig,
    item: &ItemDef,
    diags: &mut Vec<Diagnostic>,
) -> bool {
    let types = &input.globals.types;
    let ret_ok = sig.ret == Ty::NIL
        || contains_error(types, sig.ret)
        || matches!(
            types.kind(sig.ret),
            TyKind::Named { def, args }
                if *def == builtin::RESULT && args.first() == Some(&Ty::NIL)
        );
    let params_ok = sig.params.is_empty();
    let generics_ok = sig.generics.params.is_empty();
    if params_ok && ret_ok && generics_ok {
        return true;
    }
    let (span, label, help) = if !generics_ok {
        (
            sig.type_params_span.unwrap_or(item.name_span),
            "`main` takes no type parameters",
            "`cheby run` cannot choose type arguments; remove the type parameters, or give \
             the function another name if it is not an entry point",
        )
    } else if params_ok {
        (
            sig.ret_span.unwrap_or(item.name_span),
            "`main` must return `Nil` or `Result<Nil, E>`",
            "return `Nil`, or `Result<Nil, E>` to report an error; give the function another \
             name if it is not an entry point",
        )
    } else {
        (
            sig.param_list_span.unwrap_or(item.name_span),
            "`main` takes no parameters",
            "program arguments come from `env::args()`; give the function another name if it \
             is not an entry point",
        )
    };
    diags.push(
        Diagnostic::new(
            codes::INVALID_MAIN,
            "`main` must take no parameters and return `Nil` or `Result<Nil, E>`",
        )
        .with_primary(span, label)
        .with_note("any module's `main` can be run with `cheby run <module>` (D-137)")
        .with_help(help),
    );
    false
}
