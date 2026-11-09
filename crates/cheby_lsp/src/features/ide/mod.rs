//! Feature logic over the analysis API, free of protocol types.

pub(crate) mod cursor;
pub(crate) mod edits;
pub(crate) mod hierarchy;
pub(crate) mod hover;
pub(crate) mod nav;
pub(crate) mod print;
pub(crate) mod scope;
pub(crate) mod symbols;
pub(crate) mod tokens;

use cheby_driver::analysis::{ModuleId, ModuleView, Snapshot};

/// `module` is the one module of single-file mode's synthetic package,
/// whose invented name `script` no answer shows as a container (D-1477).
pub(crate) fn is_synthetic(snapshot: &Snapshot, module: ModuleId) -> bool {
    snapshot.is_single_file() && snapshot.module(module).is_some_and(ModuleView::is_local)
}
