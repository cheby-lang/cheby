//! Declaration checks other than interfaces (C2, C4): the rules of
//! chapter 4 that need the module's whole interface. The alias rules
//! (E0070) are checked when the alias is lowered (`crate::lower_ty`), the
//! declared types of constants by `crate::consts`, and interface
//! declarations by `crate::ifaces`.
//!
//! - Field names must be distinct within a variant (§4.3.1, E0041).
//! - A signature must not mention a type or interface less visible than
//!   its item (§4.1.3, §7.4, D-148, D-149, D-198): see [`visibility`].
//! - The functions of a standard-library type's `@std_hooks` have the
//!   hooks' shapes (D-342): see [`hooks`].
//! - Outside std, a native foreign result type holds an external type only
//!   at its top level (§12.5.1, D-948, E0079): see [`foreign`].

mod foreign;
mod hooks;
mod visibility;

#[cfg(test)]
mod tests;

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{DefKind, Ice, ModuleInterface, Name};
use cheby_syntax::ast::{AstNode, AstToken, RecordFieldList, Variant, VariantFields};

use crate::TypesInput;

/// The declaration errors of the module whose interface is `own`. Called
/// at the end of [`crate::build_interface`], after every signature is
/// lowered.
///
/// # Errors
///
/// An [`Ice`] if the resolution, the tree and `own` disagree.
pub(crate) fn check_decls(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
) -> Result<Vec<Diagnostic>, Ice> {
    let mut diags = Vec::new();
    duplicate_fields(input, &mut diags)?;
    visibility::check(input, own, &mut diags)?;
    hooks::check(input, own, &mut diags);
    foreign::check(input, own, &mut diags)?;
    Ok(diags)
}

/// E0041 for a field name declared twice in one variant (§4.3.1). The same
/// name in different variants is allowed (D-071).
fn duplicate_fields(input: &TypesInput<'_>, diags: &mut Vec<Diagnostic>) -> Result<(), Ice> {
    for item in &input.resolved.items {
        if item.kind != DefKind::Ctor {
            continue;
        }
        let node = input
            .tree
            .node(item.node)
            .ok_or_else(|| Ice::new("a constructor without a node"))?;
        let list = match Variant::cast(node) {
            Some(variant) => match variant.fields() {
                Some(VariantFields::Record(list)) => list,
                _ => continue,
            },
            None => RecordFieldList::cast(node)
                .ok_or_else(|| Ice::new("a constructor's node is not a variant"))?,
        };
        let ctor = input.globals.names.get(item.name);
        let mut seen: Vec<(Name, Span)> = Vec::new();
        for field in list.fields() {
            let Some(name) = field.name() else { continue };
            let interned = input.globals.names.intern(name.text());
            if let Some(&(_, first)) = seen.iter().find(|(n, _)| *n == interned) {
                diags.push(
                    Diagnostic::new(
                        codes::DUPLICATE_NAME,
                        format!("the field `{}` is declared twice in `{ctor}`", name.text()),
                    )
                    .with_primary(name.span(), "declared again here")
                    .with_secondary(first, "first declared here")
                    .with_note("field names must be distinct within a variant (§4.3.1)"),
                );
            } else {
                seen.push((interned, name.span()));
            }
        }
    }
    Ok(())
}
