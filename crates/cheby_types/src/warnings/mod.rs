//! The warnings of §13.8 (C14): the module-local pass, run with every
//! module's check, and the whole-package pass for package-visible items
//! that nothing in the package uses (D-152, D-266), which `cheby check`
//! runs after every local module is checked and `cheby run` skips
//! (D-516).
//!
//! | Code  | Warning                                      | File            |
//! | ----- | -------------------------------------------- | --------------- |
//! | W0002 | unused local binding or parameter            | `locals.rs`     |
//! | W0003 | unused import                                | `imports.rs`    |
//! | W0004 | unused `priv` item                           | `items.rs`      |
//! | W0005 | unused package-visible item (package pass)   | `items.rs`      |
//! | W0006 | `Result` discarded in statement position     | `body.rs`       |
//! | W0009 | `todo`                                       | `body.rs`       |
//! | W0011 | `==` on a type with a function or a handle   | `body.rs`       |
//! | W0012 | use of a `@deprecated` item                  | `deprecated.rs` |
//! | W0013 | negative literal for a clamped std count     | `body.rs`       |
//!
//! W0001 is phase A's, W0007 name resolution's, W0008 and W0010 the
//! exhaustiveness checker's.
//!
//! Unused-code warnings (W0002-W0005) need every use. Uses come from
//! name resolution (every
//! path and name of every signature and body, bodies with type errors
//! included) and from THIR (implicit uses through operators,
//! interpolation, `dyn` conversions and dictionaries). Since a body with
//! a type error is not in THIR, unused imports and items (W0003-W0005)
//! are reported only when every body of the module (of the package, for
//! W0005) checked; unused locals need only the resolution (D-516).
//!
//! The tests of a std module are skipped in user builds (they are checked
//! when std itself is tested), so nothing in them is reported here
//! (D-479).
//!
//! Locality (D-246): the module pass reads only the module's tree,
//! resolution and THIR and its dependencies' interfaces. The package pass
//! reads every local module's, which is why it is a separate pass.

mod body;
mod deprecated;
mod imports;
mod items;
mod locals;
mod uses;

#[cfg(test)]
mod tests;

use cheby_diag::Diagnostic;
use cheby_iface::{Ice, ModuleInterface, PackageId};

use crate::TypesInput;
use crate::thir;

/// One checked module of the local package, for [`package_warnings`].
#[derive(Clone, Copy, Debug)]
pub struct PackageModule<'a> {
    pub input: TypesInput<'a>,
    pub own: &'a ModuleInterface,
    pub thir: &'a thir::Module,
}

/// The module-local warnings: unused bindings, imports and `priv` items,
/// discarded `Result`s, `todo`, `==` on functions and handles, uses of
/// `@deprecated` items, negative count literals (W0002-W0004, W0006,
/// W0009, W0011-W0013). Called by [`crate::check_bodies`] after every
/// item is checked. The result is sorted by position.
///
/// # Errors
///
/// An [`Ice`] if THIR, the resolution and the tree disagree.
pub(crate) fn module_warnings(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    thir: &thir::Module,
) -> Result<Vec<Diagnostic>, Ice> {
    let paths = uses::path_uses(input)?;
    let mut out = Vec::new();
    out.extend(locals::unused_locals(input)?);
    if uses::thir_is_complete(input, thir) {
        let thir_refs = uses::thir_refs(thir);
        let hooks = uses::hook_refs(own, input.module);
        out.extend(imports::unused_imports(input, own, &paths));
        let refs = items::item_refs(input, &paths, &thir_refs, &hooks);
        out.extend(items::unused_private(input, own, &refs));
    }
    out.extend(deprecated::deprecated_uses(input, own, &paths));
    out.extend(body::body_warnings(input, own, thir)?);
    sort(&mut out);
    Ok(out)
}

/// The whole-package warnings over every module of the local package:
/// package-visible items that no module of the package uses (W0005,
/// D-152, D-266). The driver calls it once after all local modules are
/// checked, only for `cheby check`. Nothing is reported when a body of
/// the package did not check, since it could hold the only uses. The result is sorted by position.
///
/// # Errors
///
/// An [`Ice`] if THIR, the resolution and the tree of a module disagree.
pub fn package_warnings(modules: &[PackageModule<'_>]) -> Result<Vec<Diagnostic>, Ice> {
    let local: Vec<&PackageModule<'_>> = modules
        .iter()
        .filter(|m| m.input.package == PackageId::LOCAL)
        .collect();
    if !local
        .iter()
        .all(|m| uses::thir_is_complete(&m.input, m.thir))
    {
        return Ok(Vec::new());
    }
    let mut refs = Vec::new();
    for m in &local {
        let paths = uses::path_uses(&m.input)?;
        let thir_refs = uses::thir_refs(m.thir);
        let hooks = uses::hook_refs(m.own, m.input.module);
        refs.extend(items::item_refs(&m.input, &paths, &thir_refs, &hooks));
    }
    let mut out = Vec::new();
    for m in &local {
        out.extend(items::unused_in_package(&m.input, m.own, &refs));
    }
    sort(&mut out);
    Ok(out)
}

/// Sorts warnings by primary position, then code and message, so output
/// never depends on hash-map order (D-264). The driver sorts every
/// diagnostic again with the others.
fn sort(diags: &mut [Diagnostic]) {
    diags.sort_by(|a, b| {
        let key = |d: &Diagnostic| {
            d.primary_span()
                .map(|s| (s.file, s.range.start(), s.range.end()))
        };
        key(a)
            .cmp(&key(b))
            .then_with(|| a.code.cmp(&b.code))
            .then_with(|| a.message.cmp(&b.message))
    });
}
