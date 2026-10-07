//! The warnings of §13.8 (C14): the module-local pass, run with every
//! module's check, and the whole-package pass for package-visible items
//! that nothing in the package uses (D-152, D-266), which `cheby check`
//! runs after every local module is checked and `cheby run` skips
//! (GAP(package-warnings-in-run)).
//!
//! W3-J owns this directory.

use cheby_diag::Diagnostic;
use cheby_iface::{Ice, ModuleInterface};

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
/// item is checked.
///
/// # Errors
///
/// An [`Ice`] if THIR and the interfaces disagree.
#[expect(clippy::unnecessary_wraps, reason = "stub: W3-J fills it in")]
pub(crate) fn module_warnings(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    thir: &thir::Module,
) -> Result<Vec<Diagnostic>, Ice> {
    let _ = (input, own, thir);
    Ok(Vec::new())
}

/// The whole-package warnings over every module of the local package:
/// package-visible items that no module of the package uses (W0005,
/// D-152, D-266). The driver calls it once after all local modules are
/// checked, only for `cheby check`.
///
/// # Errors
///
/// An [`Ice`] if THIR and the interfaces disagree.
pub fn package_warnings(modules: &[PackageModule<'_>]) -> Result<Vec<Diagnostic>, Ice> {
    let _ = modules;
    Ok(Vec::new())
}
