//! Shared code for the conformance, UI and REPL harnesses and for `xtask`:
//! milestones and expected-fail tracking, example, UI-test and transcript
//! metadata, golden files, the platforms that skip run trials (D-545),
//! running commands with a timeout scaled by `CHEBY_HARNESS_SCALE`, the
//! thread variants of the conformance trials (D-722, docs/plan/testing.md),
//! each harness's fresh `CHEBY_CACHE` (D-959), the `@aot` trial of the
//! conformance harness (D-1025) and the HTTP fixture of the examples
//! (D-1027).

pub mod aot;
pub mod cache;
pub mod decisions;
pub mod example;
pub mod golden;
pub mod http_fixture;
pub mod milestone;
pub mod platform;
pub mod process;
pub mod progen;
pub mod repl;
pub mod threads;
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
