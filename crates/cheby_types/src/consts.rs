//! Constant restrictions and cycles (§4.5, C13): a constant's declared
//! type contains no function, `dyn`, external or handle type (E0068,
//! D-139, D-237), and constants do not refer to each other in a cycle
//! through direct references (E0069, GAP(const-cycle-scope)). Constants
//! are not evaluated until E6 (GAP(const-eval-interim)).
//!
//! W3-K owns this file.

use cheby_diag::Diagnostic;
use cheby_iface::{Ice, ModuleInterface};

use crate::TypesInput;
use crate::thir;

/// The constant errors of the module. Called by [`crate::check_bodies`]
/// after every item is checked.
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
