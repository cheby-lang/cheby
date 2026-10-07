//! The cross-target check (§12.6, C12, D-056, D-190, D-207): every use of
//! an item exists on every target the using code runs on, for every target
//! the package declares (E0067). Item targets are
//! [`cheby_iface::IfaceItem::targets`].
//!
//! W3-K owns this file.

use cheby_diag::Diagnostic;
use cheby_iface::{Ice, ModuleInterface};

use crate::TypesInput;
use crate::thir;

/// The cross-target errors of the module's checked bodies. Called by
/// [`crate::check_bodies`] after every item is checked.
///
/// # Errors
///
/// An [`Ice`] if THIR and the interfaces disagree.
#[expect(clippy::unnecessary_wraps, reason = "stub: W3-K fills it in")]
pub(crate) fn check_module(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    thir: &thir::Module,
) -> Result<Vec<Diagnostic>, Ice> {
    let _ = (input, own, thir);
    Ok(Vec::new())
}
