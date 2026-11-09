//! Request handlers: protocol in, protocol out, over `features::ide`.

pub(crate) mod folding;
pub(crate) mod format;
pub(crate) mod hierarchy;
pub(crate) mod hover;
pub(crate) mod links;
pub(crate) mod nav;
pub(crate) mod selection;
pub(crate) mod symbols;
pub(crate) mod tokens;
pub(crate) mod workspace_symbol;

use std::path::Path;

use cheby_driver::analysis::{FileId, FileLocation, ModuleView, Snapshot};

/// The file of `snapshot` that the materialized std file at `path` holds,
/// with std's sources materialized under `std_dir` (D-1369).
pub(crate) fn std_file(snapshot: &Snapshot, path: &Path, std_dir: &Path) -> Option<FileId> {
    let below = path.strip_prefix(std_dir).ok()?;
    let display: Vec<String> = below
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let display = display.join("/");
    snapshot.all_modules().filter_map(ModuleView::file).find(
        |&file| matches!(snapshot.location(file), Some(FileLocation::Std(d)) if *d == display),
    )
}
