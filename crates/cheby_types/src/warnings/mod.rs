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

use cheby_diag::{Diagnostic, Span};
use cheby_iface::{DefId, DefIndex, Ice, ModuleId, ModuleInterface, Name, PackageId};
use rayon::prelude::*;
use rustc_hash::FxHashMap;

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
        out.extend(imports::unused_imports(input, own, &paths, &thir_refs));
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
    // Each module's references, collected in parallel (D-263) and kept in
    // module order, then grouped by the module they refer to, so each
    // module's pass reads only the references to its own items instead
    // of every reference of the package.
    let per_module: Vec<Result<Vec<items::ItemRef>, Ice>> = local
        .par_iter()
        .map(|m| {
            let paths = uses::path_uses(&m.input)?;
            let thir_refs = uses::thir_refs(m.thir);
            let hooks = uses::hook_refs(m.own, m.input.module);
            Ok(items::item_refs(&m.input, &paths, &thir_refs, &hooks))
        })
        .collect();
    let mut refs_to: FxHashMap<ModuleId, Vec<items::ItemRef>> = FxHashMap::default();
    for refs in per_module {
        for r in refs? {
            refs_to.entry(r.def.module).or_default().push(r);
        }
    }
    let found: Vec<Vec<Diagnostic>> = local
        .par_iter()
        .map(|m| {
            let refs = refs_to.get(&m.input.module).map_or(&[][..], Vec::as_slice);
            items::unused_in_package(&m.input, m.own, refs)
        })
        .collect();
    let mut out: Vec<Diagnostic> = found.into_iter().flatten().collect();
    sort(&mut out);
    Ok(out)
}

/// A package-visible item that W0005 may report (D-152, D-516).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackageCandidate {
    /// The item, a top-level item of the module.
    pub index: DefIndex,
    /// Its kind's word in the warning, such as `function`.
    pub kind: &'static str,
    /// Its name.
    pub name: Name,
    /// Its name's span.
    pub span: Span,
}

/// What one module of the local package contributes to the build cache's
/// summaries (D-965, D-967): the references of its THIR and of its types'
/// hooks, and what W0005 needs from it, so that the whole-package pass can
/// run over summaries of modules that were not checked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleRefs {
    /// What THIR refers to, as `(owner, item)`: from the body or entry of
    /// each function, constant and test of the module (callees, function
    /// values, constants, constructors, dictionaries' functions and
    /// interfaces), in THIR order.
    pub mentions: Vec<(DefIndex, DefId)>,
    /// The functions each type of the module names as hooks (D-342), as
    /// `(type, function)`.
    pub hooks: Vec<(DefIndex, DefId)>,
    /// The top-level items of the local package that the module uses
    /// outside their own declarations, tests included (W0005's uses,
    /// D-516), sorted.
    pub uses: Vec<DefId>,
    /// The module's package-visible items W0005 may report.
    pub candidates: Vec<PackageCandidate>,
    /// Whether every body of the module checked: W0005 is given only when
    /// every module's did.
    pub complete: bool,
}

/// The references of module `input.module` (a module of the local
/// package) for the build cache's summaries: [`ModuleRefs`]. `input.deps`
/// should not record reads, since the module's check is over.
///
/// # Errors
///
/// An [`Ice`] if THIR, the resolution and the tree disagree.
pub fn module_refs(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    thir: &thir::Module,
) -> Result<ModuleRefs, Ice> {
    let paths = uses::path_uses(input)?;
    let thir_refs = uses::thir_refs(thir);
    let hooks = uses::hook_refs(own, input.module);
    let refs = items::item_refs(input, &paths, &thir_refs, &hooks);
    Ok(ModuleRefs {
        mentions: thir_refs.iter().map(|r| (r.owner, r.def)).collect(),
        hooks: hooks.iter().map(|r| (r.owner, r.def)).collect(),
        uses: items::package_uses(input, &refs),
        candidates: items::package_candidates(input, own)
            .into_iter()
            .map(|(index, kind, name, span)| PackageCandidate {
                index,
                kind,
                name,
                span,
            })
            .collect(),
        complete: uses::thir_is_complete(input, thir),
    })
}

/// The W0005 warning of the package-visible `kind` (a
/// [`PackageCandidate::kind`]) `name`, whose name is at `span`, which no
/// module of the package uses (D-152, D-516): what [`package_warnings`]
/// reports, for a driver that runs the pass over summaries (D-965).
#[must_use]
pub fn unused_package_item(kind: &str, name: &str, span: Span) -> Diagnostic {
    items::unused_package_item(kind, name, span)
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
