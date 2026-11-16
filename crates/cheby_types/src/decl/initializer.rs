//! The constructor named by the standard library's
//! `@std_initializer(constructor)` on a function (D-1599, D-482). Where
//! the function is a constant's whole initializer, the checker calls the
//! constructor in its place with the function's type arguments and
//! arguments, followed by the constant's path text and the text of its
//! value type (D-1572, D-1587). So the function's result must be a named
//! type with exactly one type argument, as `context::Key<T>`, whose text is
//! the second string (D-1601), and the constructor must have:
//!
//! - as many type parameters as the function, with the same bounds;
//! - the function's parameter types, followed by two `String`s;
//! - the function's result type;
//! - a visibility other than `priv`, since a warm build has no signature
//!   for a `priv` item (D-1587).
//!
//! The resolver has already reported a constructor that is not a function
//! of the module (E0042), and a function or constructor whose signature
//! has errors is silent. A violation is E0045, the attribute's code, at
//! the constructor's name in the attribute. A function whose result has
//! another form is reported once, and its constructor is not looked at.

use cheby_diag::{Diagnostic, codes};
use cheby_iface::{
    DefKind, FnSig, ItemSig, ModuleInterface, Namespace, Ty, TyInterner, TyKind, Vis,
};

use crate::TypesInput;

/// E0045 for every `@std_initializer` constructor without the shape its
/// function gives it.
pub(super) fn check(input: &TypesInput<'_>, own: &ModuleInterface, diags: &mut Vec<Diagnostic>) {
    for (index, item) in input.resolved.items.iter_enumerated() {
        let Some((name, span)) = item.attrs.initializer else {
            continue;
        };
        if item.kind != DefKind::Fn {
            continue;
        }
        let Some(ItemSig::Fn(sig)) = own.item(index).map(|i| &i.sig) else {
            continue;
        };
        let fn_name = input.globals.names.get(item.name);
        match input.globals.types.kind(sig.ret) {
            // An error already reported where the result is written.
            TyKind::Error => continue,
            TyKind::Named { args, .. } if args.len() == 1 => {}
            TyKind::Never
            | TyKind::Named { .. }
            | TyKind::Tuple(_)
            | TyKind::Fn { .. }
            | TyKind::Param(_)
            | TyKind::Dyn { .. } => {
                diags.push(
                    Diagnostic::new(
                        codes::INVALID_ATTRIBUTE,
                        format!(
                            "`{fn_name}` cannot carry `@std_initializer`: its result type has not \
                             exactly one type argument"
                        ),
                    )
                    .with_primary(span, "named here")
                    .with_note(
                        "a function with `@std_initializer` returns a named type with one type \
                         argument, as `context::Key<T>`, and its constructor receives the text \
                         of that argument (D-1601)",
                    ),
                );
                continue;
            }
        }
        let Some(ctor_item) = own
            .lookup(Namespace::Value, name)
            .and_then(|ctor| own.item(ctor))
        else {
            continue;
        };
        let ItemSig::Fn(ctor) = &ctor_item.sig else {
            continue;
        };
        let ctor_name = input.globals.names.get(name);
        let fits = fits(&input.globals.types, ctor, sig);
        if !fits {
            diags.push(
                Diagnostic::new(
                    codes::INVALID_ATTRIBUTE,
                    format!("`{ctor_name}` cannot be the initializer constructor of `{fn_name}`"),
                )
                .with_primary(span, "named here")
                .with_note(format!(
                    "the constructor of `@std_initializer` has the type parameters, the \
                     parameters and the result of `{fn_name}`, with two more `String` parameters \
                     at the end, for the constant's path and the text of the one type argument \
                     of its result type (D-1599, D-1601)"
                )),
            );
        } else if ctor_item.vis == Vis::Priv {
            diags.push(
                Diagnostic::new(
                    codes::INVALID_ATTRIBUTE,
                    format!("the initializer constructor `{ctor_name}` of `{fn_name}` is `priv`"),
                )
                .with_primary(span, "named here")
                .with_note(
                    "a constant of another module calls the constructor, so it must not be \
                     `priv` (D-1587)",
                ),
            );
        }
    }
}

/// Whether `ctor` has the shape that `sig`'s `@std_initializer` asks of its
/// constructor: the same type parameters with the same bounds, `sig`'s
/// parameter types followed by two `String`s, and the same result, which
/// is a named type with one type argument (D-1601). A use
/// of the function in another module asks it again, through the
/// interfaces only (D-246), so that a constructor reported here is never
/// called (`check::key`).
pub(crate) fn fits(types: &TyInterner, ctor: &FnSig, sig: &FnSig) -> bool {
    let one_argument = matches!(types.kind(sig.ret), TyKind::Named { args, .. } if args.len() == 1);
    let same_generics = ctor.generics.len() == sig.generics.len()
        && ctor
            .generics
            .params
            .iter()
            .zip(sig.generics.params.iter())
            .all(|(c, f)| c.bounds == f.bounds);
    one_argument
        && same_generics
        && ctor.params.len() == sig.params.len().saturating_add(2)
        && ctor
            .params
            .iter()
            .zip(sig.params.iter())
            .all(|(c, f)| c.ty == f.ty)
        && ctor
            .params
            .iter()
            .skip(sig.params.len())
            .all(|p| p.ty == Ty::STRING)
        && ctor.ret == sig.ret
}
