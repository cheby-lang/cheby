//! The constructor named by the standard library's
//! `@std_call_site(constructor)` on a function (D-1628, D-482). The checker
//! calls the constructor in place of every call of the function, with the
//! call's type arguments and arguments followed by the path, the line and
//! the column of the call site (`check::call_site`), so the constructor
//! must have (D-1630):
//!
//! - as many type parameters as the function, with the same bounds;
//! - the function's parameter types, followed by a `String` and two `Int`s;
//! - the function's result type;
//! - a visibility other than `priv`, since a call in another module calls
//!   it and a warm build has no signature for a `priv` item (D-1587);
//! - every target the function exists on;
//! - neither `@std_call_site` nor `@std_initializer`, so a replaced call is
//!   never replaced again.
//!
//! The resolver has already reported a constructor that is not a function
//! of the module (E0042), and the rules of the function itself (a body, and
//! none of `@external`, `@std_initializer` and `@std_clamped`), and a
//! function or constructor whose signature has errors is silent. A
//! violation is E0045, the attribute's code, at the constructor's name in
//! the attribute. Only the first rule a constructor breaks is reported.

use cheby_diag::{Diagnostic, codes};
use cheby_iface::{DefKind, FnSig, ItemSig, ModuleInterface, Namespace, Ty, Vis};

use crate::TypesInput;

/// E0045 for every `@std_call_site` constructor without the shape its
/// function gives it.
pub(super) fn check(input: &TypesInput<'_>, own: &ModuleInterface, diags: &mut Vec<Diagnostic>) {
    for (index, item) in input.resolved.items.iter_enumerated() {
        let Some((name, span)) = item.attrs.call_site else {
            continue;
        };
        if item.kind != DefKind::Fn {
            continue;
        }
        let Some(fn_item) = own.item(index) else {
            continue;
        };
        let ItemSig::Fn(sig) = &fn_item.sig else {
            continue;
        };
        let Some(ctor_index) = own.lookup(Namespace::Value, name) else {
            continue;
        };
        let Some(ctor_item) = own.item(ctor_index) else {
            continue;
        };
        let ItemSig::Fn(ctor) = &ctor_item.sig else {
            continue;
        };
        let fn_name = input.globals.names.get(item.name);
        let ctor_name = input.globals.names.get(name);
        let ctor_attrs = input.resolved.item(ctor_index).map(|def| &def.attrs);
        let marked = ctor_attrs.and_then(|attrs| {
            if attrs.call_site.is_some() {
                Some("@std_call_site")
            } else if attrs.initializer.is_some() {
                Some("@std_initializer")
            } else {
                None
            }
        });
        let (message, note) = if !fits(ctor, sig) {
            (
                format!("`{ctor_name}` cannot be the call-site constructor of `{fn_name}`"),
                format!(
                    "the constructor of `@std_call_site` has the type parameters, the parameters \
                     and the result of `{fn_name}`, with a `String` and two `Int` parameters more \
                     at the end, for the path, the line and the column of the call site (D-1630)"
                ),
            )
        } else if ctor_item.vis == Vis::Priv {
            (
                format!("the call-site constructor `{ctor_name}` of `{fn_name}` is `priv`"),
                "a call in another module calls the constructor, so it must not be `priv` \
                 (D-1630)"
                    .to_owned(),
            )
        } else if !fn_item
            .targets
            .iter()
            .all(|target| ctor_item.targets.contains(target))
        {
            (
                format!(
                    "the call-site constructor `{ctor_name}` of `{fn_name}` does not exist on \
                     every target of `{fn_name}`"
                ),
                format!(
                    "every call of `{fn_name}` calls the constructor, so it exists wherever the \
                     function does (D-1630)"
                ),
            )
        } else if let Some(attr) = marked {
            (
                format!("the call-site constructor `{ctor_name}` of `{fn_name}` carries `{attr}`"),
                "a call is replaced once, so the constructor carries neither `@std_call_site` \
                 nor `@std_initializer` (D-1630)"
                    .to_owned(),
            )
        } else {
            continue;
        };
        diags.push(
            Diagnostic::new(codes::INVALID_ATTRIBUTE, message)
                .with_primary(span, "named here")
                .with_note(note),
        );
    }
}

/// Whether `ctor` has the shape that `sig`'s `@std_call_site` asks of its
/// constructor: the same type parameters with the same bounds, `sig`'s
/// parameter types followed by a `String` and two `Int`s, and the same
/// result. A use of the function in another module asks it again, through
/// the interfaces only (D-246), so that a constructor reported here is
/// never called (`check::call_site`).
pub(crate) fn fits(ctor: &FnSig, sig: &FnSig) -> bool {
    let same_generics = ctor.generics.len() == sig.generics.len()
        && ctor
            .generics
            .params
            .iter()
            .zip(sig.generics.params.iter())
            .all(|(c, f)| c.bounds == f.bounds);
    let tail: Vec<Ty> = ctor
        .params
        .iter()
        .skip(sig.params.len())
        .map(|p| p.ty)
        .collect();
    same_generics
        && ctor.params.len() == sig.params.len().saturating_add(3)
        && ctor
            .params
            .iter()
            .zip(sig.params.iter())
            .all(|(c, f)| c.ty == f.ty)
        && tail == [Ty::STRING, Ty::INT, Ty::INT]
        && ctor.ret == sig.ret
}
