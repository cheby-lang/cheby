//! The places of the repository that name a decision (D-1940, D-1942).

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use cheby_test_support::decisions;

/// One `D-NNN` in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cite {
    /// The file's path from the repository root, with `/`.
    pub(crate) file: String,
    pub(crate) line: usize,
    pub(crate) id: u32,
    /// Whether the line is a test's `@decisions` directive, which pins the
    /// decision (D-638).
    pub(crate) directive: bool,
}

/// Every citation in the files git tracks and in the new files it does not
/// ignore, the log's own lines included. A file that is not UTF-8 is
/// skipped.
pub(crate) fn all(root: &Path) -> Result<Vec<Cite>, String> {
    let mut out = Vec::new();
    for file in files(root)? {
        let path = root.join(&file);
        // The index may name a file that is deleted, and a submodule is a
        // directory.
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if let Ok(text) = String::from_utf8(bytes) {
            in_text(&file, &text, &mut out);
        }
    }
    Ok(out)
}

/// The citations of one file's text, in the order of its lines.
pub(crate) fn in_text(file: &str, text: &str, out: &mut Vec<Cite>) {
    if !text.contains("D-") {
        return;
    }
    for (index, line) in text.lines().enumerate() {
        if !line.contains("D-") {
            continue;
        }
        // A golden may quote a directive inside a line; a test's own one
        // starts its comment.
        let directive = line.trim_start().starts_with("// @decisions");
        for id in decisions::mentioned_in(line) {
            out.push(Cite {
                file: file.to_owned(),
                line: index.saturating_add(1),
                id,
                directive,
            });
        }
    }
}

/// The tracked files and the untracked ones that are not ignored, sorted.
fn files(root: &Path) -> Result<BTreeSet<String>, String> {
    let out = Command::new("git")
        .current_dir(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_ids_with_their_lines() {
        let mut out = Vec::new();
        in_text(
            "tests/ui/a.cheby",
            "// @decisions: D-001, D-244\n\nfn f() {} // D-002 and D-002\nno id here\n\
             \"text\": \"// @decisions: D-003\"\n",
            &mut out,
        );
        let found: Vec<(usize, u32, bool)> =
            out.iter().map(|c| (c.line, c.id, c.directive)).collect();
        assert_eq!(
            found,
            [(1, 1, true), (1, 244, true), (3, 2, false), (5, 3, false)]
        );
        assert!(out.iter().all(|c| c.file == "tests/ui/a.cheby"));
    }
}
