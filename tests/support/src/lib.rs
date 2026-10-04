//! Shared code for the conformance and UI harnesses and for `xtask`:
//! milestones and expected-fail tracking, example and UI-test metadata,
//! golden files, and running commands with a timeout
//! (docs/plan/testing.md).

pub mod decisions;
pub mod example;
pub mod golden;
pub mod milestone;
pub mod process;
pub mod ui;

use std::path::{Path, PathBuf};

/// The root of the repository, two levels above `tests/support`.
#[must_use]
pub fn repo_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .nth(2)
        .unwrap_or(manifest)
        .to_path_buf()
}

/// `path` relative to `base`, with `/` separators, for stable output.
#[must_use]
pub fn display_relative(path: &Path, base: &Path) -> String {
    let relative = path.strip_prefix(base).unwrap_or(path);
    relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether the harnesses should rewrite goldens instead of comparing them.
/// Set by `cargo xtask bless`.
#[must_use]
pub fn bless_enabled() -> bool {
    std::env::var_os("CHEBY_BLESS").is_some_and(|v| v == "1")
}

/// Lists files with extension `ext` under `dir`, recursively, sorted by
/// path. Hidden files and directories are skipped.
///
/// # Errors
///
/// Returns the first error from reading a directory.
pub fn files_with_extension(dir: &Path, ext: &str) -> std::io::Result<Vec<PathBuf>> {
    fn walk(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if entry.file_type()?.is_dir() {
                walk(&path, ext, out)?;
            } else if path.extension().is_some_and(|e| e == ext) {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    if dir.exists() {
        walk(dir, ext, &mut out)?;
    }
    out.sort();
    Ok(out)
}
