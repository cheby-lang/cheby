//! External values only as direct results (§12.5.1, D-948), E0079.
//!
//! C code cannot build a value that holds an external type, because it
//! cannot name the type's layout, which is assigned at start (D-454,
//! D-590). So outside the standard library, an `@external(native, …)`
//! whose result type holds an external type below its top level, such as
//! `Result<Db, String>` or `Option<Db>`, is an error at the attribute. The
//! check reads only the module's own `@external` signatures and the
//! interfaces of the types they name (D-246), so it runs with the other
//! declaration checks, and a module that reports it is never cached
//! (D-964).
//!
//! - The standard library is exempt: its kernels are Rust and can build
//!   any value (D-948).
//! - Parameters may hold external types at any depth, as opaque values.
//! - A type parameter is not checked, whatever a call instantiates it with.
//! - GAP(e0079-scope): the walk enters type arguments, tuple elements and
//!   the fields of types declared outside the standard library (an alias
//!   is already what it stands for); it does not enter function types or
//!   `dyn` values, which foreign code cannot build at all, or the fields of
//!   the standard library's own types, whose type arguments it enters. An
//!   external type is any type declared without a body, the standard
//!   library's handle types included.

use cheby_diag::{Diagnostic, codes};
use cheby_iface::{
    CtorFields, DefId, Ice, ItemSig, ModuleInterface, PackageId, Ty, TyKind, TypeDefKind, display,
};
use cheby_syntax::ast::{AstNode, Item, RetType};

use crate::TypesInput;

/// E0079 for every `@external(native, …)` of a module outside the standard
/// library whose result type holds an external type below its top level.
///
/// # Errors
///
/// An [`Ice`] if the resolution, the tree and `own` disagree.
pub(super) fn check(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    diags: &mut Vec<Diagnostic>,
) -> Result<(), Ice> {
    if input.package == PackageId::STD {
        return Ok(());
    }
    for (index, item) in input.resolved.items.iter_enumerated() {
        let Some((_, attr_span)) = item.attrs.native_external() else {
            continue;
        };
        let Some(ItemSig::Fn(sig)) = own.item(index).map(|i| &i.sig) else {
            continue;
        };
        let mut seen = Vec::new();
        let Some(held) = held_external(input, own, sig.ret, true, &mut seen)? else {
            continue;
        };
        let Item::Fn(f) = input
            .tree
            .node(item.node)
            .and_then(Item::cast)
            .ok_or_else(|| Ice::new("an external function's node is not an item"))?
        else {
            continue;
        };
        let Some(ret_span) = f.ret_type().and_then(RetType::ty).map(AstNode::span) else {
            continue;
        };
        let cx = input.display_cx(own, Some(&sig.generics));
        let result = display(sig.ret, &cx);
        let external = input.name_of(own, held);
        let outer = outer_name(input, own, sig.ret);
        diags.push(
            Diagnostic::new(
                codes::EXTERNAL_INSIDE_FOREIGN_RESULT,
                format!(
                    "the native foreign result type `{result}` holds the external type \
                     `{external}` below its top level"
                ),
            )
            .with_primary(attr_span, "native foreign code cannot build this result")
            .with_secondary(ret_span, format!("`{external}` is inside {}", outer.inside))
            .with_note(
                "native foreign code returns a value of an external type only as the \
                 function's direct result",
            )
            .with_help(format!(
                "return `{external}` from the foreign function and build {} in Cheby with a \
                 wrapper (§12.5)",
                outer.build
            )),
        );
    }
    Ok(())
}

/// How the messages name the result type's outermost type.
struct Outer {
    /// After "is inside": `` `Result` ``, "a tuple".
    inside: String,
    /// After "build": `` the `Result` ``, "the tuple".
    build: String,
}

/// The outermost type of `ty`, named for the messages.
fn outer_name(input: &TypesInput<'_>, own: &ModuleInterface, ty: Ty) -> Outer {
    match input.globals.types.kind(ty) {
        TyKind::Named { def, .. } => {
            let name = input.name_of(own, *def);
            Outer {
                inside: format!("`{name}`"),
                build: format!("the `{name}`"),
            }
        }
        _ => Outer {
            inside: "a tuple".to_owned(),
            build: "the tuple".to_owned(),
        },
    }
}

/// The first external type that `ty` holds below its top level (`top`: `ty`
/// is the result type itself), walking as the module documentation says.
/// `seen` holds the types whose fields were walked, so a recursive type is
/// walked once.
fn held_external(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    ty: Ty,
    top: bool,
    seen: &mut Vec<DefId>,
) -> Result<Option<DefId>, Ice> {
    let types = &input.globals.types;
    match types.kind(ty) {
        TyKind::Named { def, args } => {
            let (def, args) = (*def, args.to_vec());
            let item = input.item(own, def)?;
            let ItemSig::Type(type_def) = &item.sig else {
                return Ok(None);
            };
            if matches!(type_def.kind, TypeDefKind::External) {
                return Ok((!top).then_some(def));
            }
            for arg in args {
                if let Some(found) = held_external(input, own, arg, false, seen)? {
                    return Ok(Some(found));
                }
            }
            let TypeDefKind::Adt { ctors } = &type_def.kind else {
                return Ok(None);
            };
            let user = input
                .modules
                .try_get(def.module)
                .is_ok_and(|module| module.package != PackageId::STD);
            if !user || seen.contains(&def) {
                return Ok(None);
            }
            seen.push(def);
            for &ctor in ctors {
                let ctor_item = input.item(own, DefId::new(def.module, ctor))?;
                let ItemSig::Ctor(ctor_sig) = &ctor_item.sig else {
                    continue;
                };
                let fields: Vec<Ty> = match &ctor_sig.fields {
                    CtorFields::Unit => Vec::new(),
                    CtorFields::Positional(tys) => tys.to_vec(),
                    CtorFields::Named(named) => named.iter().map(|&(_, t)| t).collect(),
                };
                for field in fields {
                    if let Some(found) = held_external(input, own, field, false, seen)? {
                        return Ok(Some(found));
                    }
                }
            }
            Ok(None)
        }
        TyKind::Tuple(elems) => {
            for &elem in elems {
                if let Some(found) = held_external(input, own, elem, false, seen)? {
                    return Ok(Some(found));
                }
            }
            Ok(None)
        }
        TyKind::Fn { .. }
        | TyKind::Dyn { .. }
        | TyKind::Param(_)
        | TyKind::Error
        | TyKind::Never => Ok(None),
    }
}
