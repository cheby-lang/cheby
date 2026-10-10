//! W0012: uses of `@deprecated` items from another module (§4.8, D-200).
//!
//! Every written path or name that resolves to an item of another module
//! whose interface item carries a deprecation message warns, with the
//! message. A constructor or an interface function is deprecated with its
//! type or interface. Uses in the item's own module are not reported, and
//! neither are implicit uses that nobody wrote, such as a deprecated
//! `show` called by an interpolation (GAP(deprecated-use-sites)). Item
//! imports of a deprecated item do not warn either: the uses of the name
//! they bind do, so each use is reported once.

use cheby_diag::{Diagnostic, codes};
use cheby_iface::{DefId, ModuleInterface, PackageId};
use cheby_resolve::Res;

use super::uses::PathUse;
use crate::TypesInput;

/// The W0012 warnings of the module, in the order of `paths`.
pub(super) fn deprecated_uses(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    paths: &[PathUse],
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for path in paths {
        let Res::Def(def) = path.res else {
            continue;
        };
        if def.module == input.module {
            continue;
        }
        // The tests of a std module are not part of user builds.
        if input.package == PackageId::STD && in_test(input, path) {
            continue;
        }
        let Ok(item) = input.item(own, def) else {
            continue;
        };
        let message = item.deprecated.as_deref().or_else(|| {
            let parent = DefId::new(def.module, item.parent?);
            input.item(own, parent).ok()?.deprecated.as_deref()
        });
        let Some(message) = message else {
            continue;
        };
        out.push(
            Diagnostic::new(
                codes::DEPRECATED,
                format!(
                    "use of deprecated {} `{}`: {message}",
                    item.kind.describe(),
                    path.text
                ),
            )
            .with_primary(path.span, "deprecated"),
        );
    }
    out
}

fn in_test(input: &TypesInput<'_>, path: &PathUse) -> bool {
    path.owner
        .and_then(|owner| input.resolved.item(owner))
        .is_some_and(|item| item.kind == cheby_iface::DefKind::Test)
}
