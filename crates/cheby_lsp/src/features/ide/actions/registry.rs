//! Every provider and resolver of W11 (G1 plan §1.6, §4.2), each in its
//! package's file. Packages fill their own files and never edit this list.
//!
//! The frame calls the providers in this order, then sorts their actions
//! by X §6.2's order.

use super::{ActionId, Provide, Resolve, generic};
use crate::features::ide::{explain, fill_arms, fixes, habits, imports_actions, stubs};

/// Each provider, by the name of the file that fills it.
pub(crate) const PROVIDERS: &[(&str, Provide)] = &[
    ("generic", generic::provide),
    ("field_path", generic::use_field_path),
    ("imports_actions", imports_actions::provide),
    ("fill_arms", fill_arms::provide),
    ("stubs", stubs::provide),
    ("habits", habits::provide),
    ("explain", explain::provide),
    ("fixes", fixes::provide),
];

/// The resolver of `id`. Explain carries a command and no id, so it has
/// none (D-1733).
pub(crate) const fn resolver(id: &ActionId) -> Resolve {
    match id {
        ActionId::Fix { .. } => fixes::resolve,
        ActionId::AddImport { .. } | ActionId::RemoveImport | ActionId::OrganizeImports => {
            imports_actions::resolve
        }
        ActionId::FillArms => fill_arms::resolve,
        ActionId::Stub { .. } => stubs::resolve,
        ActionId::MakeVisible { .. } => stubs::resolve_make_visible,
        ActionId::Habit { .. } => habits::resolve,
        ActionId::Rename { .. } => generic::resolve_rename,
    }
}
