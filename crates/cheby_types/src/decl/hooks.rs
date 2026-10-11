//! The functions named by the standard library's `@std_hooks(equal_fn,
//! hash_fn, debug_fn)` on a type (D-342, D-482). The runtime
//! routine of D-252 calls them with no function packages, so each must
//! have the shape below and no bounds (D-482):
//!
//! - `equal_fn(a: T, b: T) -> Bool`;
//! - `hash_fn(value: T) -> K`, for any `K`: the runtime hashes the `K`
//!   with the built-in structural hash, so a hook returns a canonical value
//!   such as a sorted list of entries;
//! - `debug_fn(value: T) -> String`.
//!
//! `T` is the type itself, with any type arguments. The resolver has
//! already reported a hook that is not a function of the module (E0042);
//! a hook whose signature has errors is silent. A violation is E0045, the
//! attribute's code.

use cheby_diag::{Diagnostic, codes};
use cheby_iface::{DefKind, ItemSig, ModuleInterface, Namespace, Ty, TyKind};

use crate::TypesInput;

/// The three hooks, in attribute order: role, parameter count, result.
const ROLES: [(&str, usize, Option<Ty>, &str); 3] = [
    (
        "equality",
        2,
        Some(Ty::BOOL),
        "takes two values of the type and returns `Bool`",
    ),
    ("hash", 1, None, "takes one value of the type"),
    (
        "debug",
        1,
        Some(Ty::STRING),
        "takes one value of the type and returns `String`",
    ),
];

/// E0045 for every `@std_hooks` function without its hook's shape.
pub(super) fn check(input: &TypesInput<'_>, own: &ModuleInterface, diags: &mut Vec<Diagnostic>) {
    for (index, item) in input.resolved.items.iter_enumerated() {
        let Some(hooks) = &item.attrs.hooks else {
            continue;
        };
        if item.kind != DefKind::Type {
            continue;
        }
        let ty_def = input.def_id(index);
        let ty_name = input.globals.names.get(item.name);
        for (&(name, span), (role, arity, ret, shape)) in hooks.iter().zip(ROLES) {
            let Some(fn_index) = own.lookup(Namespace::Value, name) else {
                continue;
            };
            let Some(ItemSig::Fn(sig)) = own.item(fn_index).map(|i| &i.sig) else {
                continue;
            };
            let types = &input.globals.types;
            let of_type =
                |ty: Ty| matches!(types.kind(ty), TyKind::Named { def, .. } if *def == ty_def);
            let fits = sig.params.len() == arity
                && sig.params.iter().all(|p| of_type(p.ty))
                && ret.is_none_or(|r| sig.ret == r)
                && sig.generics.params.iter().all(|p| p.bounds.is_empty());
            if !fits {
                let fn_name = input.globals.names.get(name);
                diags.push(
                    Diagnostic::new(
                        codes::INVALID_ATTRIBUTE,
                        format!("`{fn_name}` cannot be the {role} hook of `{ty_name}`"),
                    )
                    .with_primary(span, "named here")
                    .with_note(format!(
                        "the {role} hook of `@std_hooks` {shape}, with no bounds (D-342)"
                    )),
                );
            }
        }
    }
}
