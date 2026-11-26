//! Feature logic over the analysis API, free of protocol types.

pub(crate) mod actions;
pub(crate) mod completion;
pub(crate) mod cursor;
pub(crate) mod edits;
pub(crate) mod explain;
pub(crate) mod field_path;
pub(crate) mod file_ops;
pub(crate) mod fill_arms;
pub(crate) mod fixes;
pub(crate) mod habits;
pub(crate) mod hierarchy;
pub(crate) mod hints;
pub(crate) mod hover;
pub(crate) mod imports;
pub(crate) mod imports_actions;
pub(crate) mod nav;
pub(crate) mod near_miss;
pub(crate) mod print;
pub(crate) mod rename;
pub(crate) mod scope;
pub(crate) mod signature;
pub(crate) mod stubs;
pub(crate) mod symbols;
pub(crate) mod tokens;

#[cfg(test)]
pub(crate) mod checkout_tests;

use cheby_driver::analysis::{ModuleId, ModuleView, Snapshot};

/// Why a request the user asked for cannot be done, in one line: the
/// handler answers it as `RequestFailed`, or as `null` with a
/// `window/showMessage` where the protocol has no error to show (X §1.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refusal(pub String);

/// `module` is the one module of single-file mode's synthetic package,
/// whose invented name `script` no answer shows as a container (D-1477).
pub(crate) fn is_synthetic(snapshot: &Snapshot, module: ModuleId) -> bool {
    snapshot.is_single_file() && snapshot.module(module).is_some_and(ModuleView::is_own)
}

/// The name of the session's package: the first segment of its modules'
/// paths, `std` in a std checkout and `script` in single-file mode. The
/// formatter reads it to tell the package's own imports from other
/// packages' (D-1953), and a file move to map paths to modules.
pub(crate) fn package_name(snapshot: &Snapshot) -> Option<String> {
    snapshot
        .own_modules()
        .find_map(|view| view.path()?.segments().first().cloned())
}
