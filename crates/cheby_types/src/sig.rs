//! Signatures (C2): every item's [`ItemSig`] and the module's interface.
//!
//! Top-level functions declare every parameter type, and an omitted return
//! type means `Nil` (D-011, D-083). Every top-level `main` must have the
//! signature of an entry point (E0050, D-448), checked before the support
//! gate. In the slice, a signature with a type other than `String` and
//! `Nil` is E0038 (D-451) and the item becomes [`ItemSig::Unsupported`].

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{
    DefKind, FnSig, GenericParam, Generics, Ice, IfaceItem, IndexVec, ItemSig, ModuleInterface,
    Name, ParamSig, Ty, TyKind, builtin, display, interface_hash, sym,
};
use cheby_resolve::{ItemDef, ItemStatus};
use cheby_syntax::ast::{AstNode, AstToken, FnDecl, RetType, Type};

use crate::TypesInput;
use crate::gate::{not_supported, slice_type, unsupported_type};
use crate::infer::contains_error;
use crate::lower_ty::TyLowering;

/// Lowers every item's signature and returns the module's interface, with
/// its hash (B8). Items are in `DefIndex` order, private ones included.
///
/// # Errors
///
/// An [`Ice`] if the resolution and the tree disagree, or a path was not
/// resolved.
pub fn build_interface(input: &TypesInput<'_>) -> Result<(ModuleInterface, Vec<Diagnostic>), Ice> {
    let resolved = input.resolved;
    // Names and kinds first, so diagnostics can name this module's items.
    let skeleton_items: Vec<IfaceItem> = resolved
        .items
        .iter()
        .map(|item| skeleton(item, ItemSig::Error))
        .collect();
    let skeleton = ModuleInterface::new(
        input.module,
        input.package,
        IndexVec::from_raw(skeleton_items)?,
    )?;
    let mut diags = Vec::new();
    let mut items = Vec::with_capacity(resolved.items.len());
    for item in &resolved.items {
        let sig = item_sig(input, &skeleton, item, &mut diags)?;
        items.push(self::skeleton(item, sig));
    }
    let mut iface = ModuleInterface::new(input.module, input.package, IndexVec::from_raw(items)?)?;
    iface.hash = Some(interface_hash(
        &iface,
        &input.deps,
        input.modules,
        input.globals,
    )?);
    Ok((iface, diags))
}

fn skeleton(item: &ItemDef, sig: ItemSig) -> IfaceItem {
    IfaceItem {
        name: item.name,
        kind: item.kind,
        vis: item.vis,
        parent: item.parent,
        span: Some(item.name_span),
        sig,
    }
}

fn item_sig(
    input: &TypesInput<'_>,
    skeleton: &ModuleInterface,
    item: &ItemDef,
    diags: &mut Vec<Diagnostic>,
) -> Result<ItemSig, Ice> {
    if item.status == ItemStatus::Unsupported {
        return Ok(ItemSig::Unsupported);
    }
    let what = match item.kind {
        DefKind::Fn => return fn_sig(input, skeleton, item, diags),
        DefKind::Test => return Ok(ItemSig::Test),
        // Reported with their type or interface.
        DefKind::Ctor | DefKind::InterfaceFn => return Ok(ItemSig::Unsupported),
        // The resolver's gate rejects these in the slice; this is the
        // fallback if it ever lets one through (C2 replaces it).
        DefKind::Type => "type declarations",
        DefKind::Alias => "type aliases",
        DefKind::Const => "constants",
        DefKind::Interface => "interfaces",
    };
    diags.push(not_supported(item.name_span, what));
    Ok(ItemSig::Unsupported)
}

/// A function's signature as written, lowered.
pub(crate) struct LoweredFnSig {
    pub generics: Generics,
    pub params: Vec<ParamSig>,
    pub ret: Ty,
    /// The written parameter types, in order.
    pub param_spans: Vec<Option<Span>>,
    /// The written return type, if any.
    pub ret_span: Option<Span>,
    /// The `(…)` parameter list.
    pub param_list_span: Option<Span>,
}

impl LoweredFnSig {
    /// Whether any part has an error, already reported.
    pub fn has_error(&self, input: &TypesInput<'_>) -> bool {
        let types = &input.globals.types;
        self.params.iter().any(|p| contains_error(types, p.ty)) || contains_error(types, self.ret)
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
pub(crate) fn lower_fn_sig(
    input: &TypesInput<'_>,
    decl: FnDecl<'_>,
    diags: &mut Vec<Diagnostic>,
) -> Result<LoweredFnSig, Ice> {
    let lowering = TyLowering {
        input,
        paths: &input.resolved.sig_paths,
    };
    let mut params_out = Vec::new();
    if let Some(list) = decl.type_params() {
        for param in list.params() {
            let name = param
                .name()
                .map_or(sym::UNDERSCORE, |n| input.globals.names.intern(n.text()));
            let mut bounds = Vec::new();
            if let Some(list) = param.bounds() {
                for iface in list.interfaces() {
                    if let Some(def) = lowering.interface(iface, diags)? {
                        bounds.push(def);
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
    let mut param_spans = Vec::new();
    for param in decl.params() {
        let name: Name = param
            .name()
            .map_or(sym::UNDERSCORE, |n| input.globals.names.intern(n.text()));
        let (ty, span) = match param.ty() {
            Some(ty) => (lowering.lower(ty, diags)?, Some(ty.significant_span())),
            // A missing type is a syntax error (E0020).
            None => (Ty::ERROR, None),
        };
        params.push(ParamSig { name, ty });
        param_spans.push(span);
    }
    let ret_type = decl.ret_type();
    let ret = lowering.ret(ret_type, diags)?;
    Ok(LoweredFnSig {
        generics,
        params,
        ret,
        param_spans,
        ret_span: ret_type.and_then(RetType::ty).map(Type::significant_span),
        param_list_span: decl.param_list().map(AstNode::significant_span),
    })
}

fn fn_sig(
    input: &TypesInput<'_>,
    skeleton: &ModuleInterface,
    item: &ItemDef,
    diags: &mut Vec<Diagnostic>,
) -> Result<ItemSig, Ice> {
    let decl = fn_decl(input, item)?;
    let sig = lower_fn_sig(input, decl, diags)?;
    if item.name == sym::MAIN && !main_ok(input, &sig, item, diags) {
        return Ok(ItemSig::Error);
    }
    // The semantic gate: one E0038 per written type the slice cannot run.
    let generics = Some(&sig.generics);
    let cx = input.display_cx(skeleton, generics);
    let mut gated = false;
    let written = sig
        .params
        .iter()
        .map(|p| p.ty)
        .zip(sig.param_spans.iter().copied())
        .chain(std::iter::once((sig.ret, sig.ret_span)));
    for (ty, span) in written {
        if slice_type(ty) || contains_error(&input.globals.types, ty) {
            continue;
        }
        let span = span.unwrap_or(item.name_span);
        diags.push(unsupported_type(span, &display(ty, &cx)));
        gated = true;
    }
    if gated {
        return Ok(ItemSig::Unsupported);
    }
    if sig.has_error(input) {
        return Ok(ItemSig::Error);
    }
    Ok(ItemSig::Fn(FnSig {
        generics: sig.generics,
        params: sig.params.into(),
        ret: sig.ret,
        targets: item.attrs.targets,
        deprecated: item.attrs.deprecated.clone(),
    }))
}

/// Checks the signature of a top-level `main`: no parameters, and `Nil`
/// or `Result<Nil, E>` (§4.2.1, D-073). Any module's `main` can be run
/// (D-137), so every one is checked, before the support gate (D-448).
/// Visibility is free (D-163).
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
    if params_ok && ret_ok {
        return true;
    }
    let (span, label) = if params_ok {
        (
            sig.ret_span.unwrap_or(item.name_span),
            "`main` must return `Nil` or `Result<Nil, E>`",
        )
    } else {
        (
            sig.param_list_span.unwrap_or(item.name_span),
            "`main` takes no parameters",
        )
    };
    diags.push(
        Diagnostic::new(
            codes::INVALID_MAIN,
            "`main` must take no parameters and return `Nil` or `Result<Nil, E>`",
        )
        .with_primary(span, label)
        .with_note("any module's `main` can be run with `cheby run <module>` (D-137)")
        .with_help("program arguments come from `env::args()`; give the function another name if it is not an entry point"),
    );
    false
}
