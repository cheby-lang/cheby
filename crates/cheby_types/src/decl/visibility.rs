//! Types in signatures must be as visible as their item (§4.1.3, §7.4,
//! D-148, D-149, D-198), E0074.
//!
//! The written signature is walked, so each error points at the type as
//! written:
//!
//! - functions: bounds, parameter types and the return type;
//! - constants: the declared type; aliases: the definition;
//! - interfaces: embedded interfaces and every function's types;
//! - types: the fields, where they are visible (§4.3.4): a `pub exposed`
//!   type's fields need `pub` types, a `pub` or package-visible type's
//!   fields must not mention `priv` types. A `priv` item can mention
//!   anything it can name.
//!
//! Interfaces in bounds, `dyn` and embeddings count like types. An alias
//! is transparent (D-081), so the alias's own visibility does not matter:
//! the types it stands for do (D-488). Each
//! offending type is reported once per item, at its first mention.

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{DefId, DefKind, Ice, IfaceItem, ItemSig, ModuleInterface, Ty, TyKind, Vis};
use cheby_resolve::{ItemDef, Res};
use cheby_syntax::ast::{
    AstNode, BoundList, InterfaceRef, Item, PathNode, RetType, Type, TypeArgList, TypeBody,
    TypeList, VariantFields,
};

use crate::TypesInput;
use crate::lower_ty::path_text;

/// Reports E0074 for every item of the module.
pub(super) fn check(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    diags: &mut Vec<Diagnostic>,
) -> Result<(), Ice> {
    for item in &input.resolved.items {
        if item.parent.is_some() {
            continue;
        }
        let node = input
            .tree
            .node(item.node)
            .and_then(Item::cast)
            .ok_or_else(|| Ice::new("an item's node is not an item"))?;
        let Some(owner) = Owner::of(item, node) else {
            continue;
        };
        let mut cx = VisCx {
            input,
            own,
            item,
            owner,
            reported: Vec::new(),
            diags,
        };
        match node {
            Item::Fn(f) => {
                if let Some(list) = f.type_params() {
                    for param in list.params() {
                        for iface in param.bounds().into_iter().flat_map(BoundList::interfaces) {
                            cx.interface_ref(iface)?;
                        }
                    }
                }
                for param in f.params() {
                    cx.opt_ty(param.ty())?;
                }
                cx.ret(f.ret_type())?;
            }
            Item::Const(c) => cx.opt_ty(c.ty())?,
            Item::Alias(a) => cx.opt_ty(a.ty())?,
            Item::Interface(i) => {
                for iface in i.bounds().into_iter().flat_map(BoundList::interfaces) {
                    cx.interface_ref(iface)?;
                }
                for f in i.fns() {
                    for ty in f.param_types().into_iter().flat_map(TypeList::types) {
                        cx.ty(ty)?;
                    }
                    cx.ret(f.ret_type())?;
                }
            }
            Item::Type(t) => match t.body() {
                Some(TypeBody::Record(fields)) => {
                    for field in fields.fields() {
                        cx.opt_ty(field.ty())?;
                    }
                }
                Some(TypeBody::Variants(list)) => {
                    for variant in list.variants() {
                        match variant.fields() {
                            Some(VariantFields::Positional(fields)) => {
                                for ty in fields.types() {
                                    cx.ty(ty)?;
                                }
                            }
                            Some(VariantFields::Record(fields)) => {
                                for field in fields.fields() {
                                    cx.opt_ty(field.ty())?;
                                }
                            }
                            None => {}
                        }
                    }
                }
                None => {}
            },
            Item::Test(_) => {}
        }
    }
    Ok(())
}

/// What is being checked, and the least visibility it needs.
#[derive(Clone, Copy)]
struct Owner {
    /// A type's fields rather than an item's signature.
    fields: bool,
    /// For fields: whether the type is `pub exposed`.
    exposed: bool,
    vis: Vis,
    /// The least visibility a mentioned type must have.
    required: Vis,
}

impl Owner {
    fn of(item: &ItemDef, node: Item<'_>) -> Option<Self> {
        let (fields, exposed, required) = match node {
            Item::Test(_) => return None,
            Item::Type(t) => {
                let exposed = t.is_exposed() && item.vis == Vis::Pub;
                // A `pub` type without `exposed` hides its fields outside
                // the package, so they may mention package-visible types
                // (D-198).
                let required = match item.vis {
                    Vis::Pub if exposed => Vis::Pub,
                    Vis::Pub | Vis::Package => Vis::Package,
                    Vis::Priv => Vis::Priv,
                };
                (true, exposed, required)
            }
            _ => (false, false, item.vis),
        };
        // Nothing is less visible than `priv`.
        (required != Vis::Priv).then_some(Self {
            fields,
            exposed,
            vis: item.vis,
            required,
        })
    }
}

struct VisCx<'a, 'd> {
    input: &'a TypesInput<'a>,
    own: &'a ModuleInterface,
    item: &'a ItemDef,
    owner: Owner,
    /// The types already reported for this item.
    reported: Vec<DefId>,
    diags: &'d mut Vec<Diagnostic>,
}

impl VisCx<'_, '_> {
    fn opt_ty(&mut self, ty: Option<Type<'_>>) -> Result<(), Ice> {
        match ty {
            Some(ty) => self.ty(ty),
            None => Ok(()),
        }
    }

    fn ret(&mut self, ret: Option<RetType<'_>>) -> Result<(), Ice> {
        self.opt_ty(ret.and_then(RetType::ty))
    }

    fn ty(&mut self, ty: Type<'_>) -> Result<(), Ice> {
        match ty {
            Type::Path(path_ty) => {
                if let Some(path) = path_ty.path() {
                    self.path(path, path_ty.significant_span())?;
                }
                for arg in path_ty.type_args().into_iter().flat_map(TypeArgList::types) {
                    self.ty(arg)?;
                }
            }
            Type::Tuple(tuple) => {
                for elem in tuple.elements() {
                    self.ty(elem)?;
                }
            }
            Type::Fn(f) => {
                for param in f.param_types().into_iter().flat_map(TypeList::types) {
                    self.ty(param)?;
                }
                self.ret(f.ret_type())?;
            }
            Type::Dyn(d) => {
                if let Some(iface) = d.interface() {
                    self.interface_ref(iface)?;
                }
            }
        }
        Ok(())
    }

    fn res(&self, path: PathNode<'_>) -> Option<Res> {
        self.input
            .resolved
            .sig_paths
            .get(&path.syntax().id())
            .copied()
    }

    fn interface_ref(&mut self, iface: InterfaceRef<'_>) -> Result<(), Ice> {
        if let Some(path) = iface.path() {
            self.path(path, iface.significant_span())?;
        }
        Ok(())
    }

    fn lookup(&self, def: DefId) -> Option<&IfaceItem> {
        if def.module == self.input.module {
            self.own.item(def.index)
        } else {
            self.input.deps.item(def)
        }
    }

    fn path(&mut self, path: PathNode<'_>, span: Span) -> Result<(), Ice> {
        let Some(Res::Def(def)) = self.res(path) else {
            return Ok(());
        };
        let Some(named) = self.lookup(def) else {
            return Err(Ice::new(format!("no interface item for {def:?}")));
        };
        match named.kind {
            DefKind::Alias => {
                // Transparent: what the alias stands for counts (D-081).
                if let ItemSig::Alias(alias) = &named.sig
                    && let Some(inner) = self.first_less_visible(alias.target)
                {
                    let text = path_text(path);
                    self.report(inner, span, Some(&text));
                }
            }
            DefKind::Type | DefKind::Interface if named.vis < self.owner.required => {
                self.report(def, span, None);
            }
            _ => {}
        }
        Ok(())
    }

    /// The first type or interface in `ty` less visible than required.
    fn first_less_visible(&self, ty: Ty) -> Option<DefId> {
        let types = &self.input.globals.types;
        let (own, parts): (Option<DefId>, Vec<Ty>) = match types.kind(ty) {
            TyKind::Named { def, args } => (Some(*def), args.to_vec()),
            TyKind::Dyn { iface, args } => (Some(*iface), args.to_vec()),
            TyKind::Tuple(elems) => (None, elems.to_vec()),
            TyKind::Fn { params, ret } => {
                let mut parts = params.to_vec();
                parts.push(*ret);
                (None, parts)
            }
            TyKind::Error | TyKind::Never | TyKind::Param(_) => (None, Vec::new()),
        };
        if let Some(def) = own
            && self
                .lookup(def)
                .is_some_and(|item| item.vis < self.owner.required)
        {
            return Some(def);
        }
        parts.into_iter().find_map(|t| self.first_less_visible(t))
    }

    fn report(&mut self, def: DefId, span: Span, through: Option<&str>) {
        if self.reported.contains(&def) {
            return;
        }
        self.reported.push(def);
        let Some(named) = self.lookup(def) else {
            return;
        };
        let interner = &self.input.globals.names;
        let name = interner.get(named.name);
        let what = if named.kind == DefKind::Interface {
            "interface"
        } else {
            "type"
        };
        let mentioned = format!("{} {what} `{name}`", vis_word(named.vis));
        let owner_name = interner.get(self.item.name);
        let message = if self.owner.fields {
            let kind = if self.owner.exposed {
                "`pub exposed`".to_owned()
            } else {
                vis_word(self.owner.vis)
            };
            format!("the fields of the {kind} type `{owner_name}` mention the {mentioned}")
        } else {
            format!(
                "the {} {} `{owner_name}` mentions the {mentioned}",
                vis_word(self.owner.vis),
                self.item.kind.describe()
            )
        };
        let label = match through {
            Some(alias) => format!("`{alias}` stands for a type that mentions `{name}`"),
            None => format!("{} {what}", vis_word(named.vis)),
        };
        let mut diag =
            Diagnostic::new(codes::LESS_VISIBLE_IN_SIGNATURE, message).with_primary(span, label);
        if let Some(declared) = named.span {
            diag = diag.with_secondary(declared, "declared here");
        }
        let (note, help) = self.advice(owner_name, name);
        self.diags.push(diag.with_note(note).with_help(help));
    }

    fn advice(&self, owner: &str, name: &str) -> (String, String) {
        let raise = match self.owner.required {
            Vis::Pub => "`pub`",
            _ => "package-visible",
        };
        if self.owner.fields {
            if self.owner.exposed {
                return (
                    "the fields of a `pub exposed` type are visible in every package, so \
                     their types must be `pub` (D-198)"
                        .to_owned(),
                    format!("make `{name}` `pub`, or remove `exposed` from `{owner}`"),
                );
            }
            return (
                format!(
                    "the fields of `{owner}` are visible in every module of the package, \
                     where a `priv` type cannot be named (D-149)"
                ),
                format!("make `{name}` package-visible, or make `{owner}` `priv`"),
            );
        }
        let (decision, lower) = match self.owner.required {
            Vis::Pub => ("D-148", "package-visible"),
            _ => ("D-149", "`priv`"),
        };
        (
            format!("code that can use `{owner}` could not name `{name}` ({decision})"),
            format!("make `{name}` {raise}, or make `{owner}` {lower}"),
        )
    }
}

fn vis_word(vis: Vis) -> String {
    match vis {
        Vis::Pub => "`pub`",
        Vis::Package => "package-visible",
        Vis::Priv => "`priv`",
    }
    .to_owned()
}
