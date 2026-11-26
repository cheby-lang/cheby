//! `cheby check --fix`: applying the sure fixes of a check's diagnostics
//! to the package's own files (§13.8, D-1945).
//!
//! A help is applied when its place marked it as a sure fix
//! ([`cheby_diag::Help::fix`]) and every edit of it lies in a file of the
//! package itself, never in the standard library or a dependency. Of a
//! diagnostic's helps only the first sure one is applied. A help whose
//! edits overlap those of a help taken before it, in the order of the
//! diagnostics, waits for the next check, and so does every help of a file
//! whose text on disk is no longer the text that was checked.

use std::collections::BTreeMap;
use std::path::PathBuf;

use cheby_diag::{Code, Diagnostic, FileId, Help, SourceMap, TextRange};

use crate::{CheckOutput, Error, Origin};

/// One help that [`apply_fixes`] applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedFix {
    /// The diagnostic's code.
    pub code: Code,
    /// The help's message, which says what the fix did.
    pub message: String,
    /// The file of its first edit, as diagnostics show it (D-421).
    pub path: String,
    /// The line of that edit's start, from 1.
    pub line: u32,
    /// Its column, from 1, in code points.
    pub column: u32,
}

/// Applies the sure fixes of `output`, the result of a check of the package
/// at `root`, and returns them in the order of the diagnostics. The files
/// are written whole.
///
/// # Errors
///
/// An [`Error`] if a file cannot be read or written; files written before
/// it keep their fixes.
pub fn apply_fixes<'a>(
    origin: impl Into<Origin<'a>>,
    output: &CheckOutput,
) -> Result<Vec<AppliedFix>, Error> {
    let origin = origin.into();
    let taken = select(&output.diagnostics, &output.sources, origin);
    let mut by_file: BTreeMap<FileId, Vec<(TextRange, &str)>> = BTreeMap::new();
    for &(_, help) in &taken {
        for edit in &help.edits {
            by_file
                .entry(edit.span.file)
                .or_default()
                .push((edit.span.range, &edit.replacement));
        }
    }
    let mut written = Vec::new();
    for (file, mut edits) in by_file {
        let Some(source) = output.sources.file(file) else {
            continue;
        };
        let Some(path) = own_file(origin, source.path()) else {
            continue;
        };
        let on_disk =
            std::fs::read(&path).map_err(|e| Error::new(format!("{}: {e}", source.path())))?;
        // The file changed since the check read it, or is not UTF-8, so
        // that the check saw other text: its fixes wait.
        if on_disk != source.text().as_bytes() {
            continue;
        }
        edits.sort_by_key(|&(range, _)| std::cmp::Reverse(range.start()));
        let mut text = source.text().to_owned();
        for (range, replacement) in edits {
            let range = usize::try_from(range.start()).unwrap_or(usize::MAX)
                ..usize::try_from(range.end()).unwrap_or(usize::MAX);
            if text.get(range.clone()).is_some() {
                text.replace_range(range, replacement);
            }
        }
        std::fs::write(&path, text).map_err(|e| Error::new(format!("{}: {e}", source.path())))?;
        written.push(file);
    }
    Ok(taken
        .into_iter()
        .filter(|(_, help)| {
            help.edits
                .first()
                .is_some_and(|edit| written.contains(&edit.span.file))
        })
        .filter_map(|(diagnostic, help)| {
            let span = help.edits.first()?.span;
            let place = output.sources.location(span)?;
            Some(AppliedFix {
                code: diagnostic.code,
                message: help.message.clone(),
                path: place.path,
                line: place.line,
                column: place.col,
            })
        })
        .collect())
}

/// The helps to apply: each diagnostic's first sure fix whose edits are all
/// in the package's own files and overlap no edit of a help taken before.
fn select<'a>(
    diagnostics: &'a [Diagnostic],
    sources: &SourceMap,
    origin: Origin<'_>,
) -> Vec<(&'a Diagnostic, &'a Help)> {
    let mut taken: Vec<(&Diagnostic, &Help)> = Vec::new();
    let mut ranges: Vec<(FileId, TextRange)> = Vec::new();
    for diagnostic in diagnostics {
        let Some(help) = diagnostic
            .helps
            .iter()
            .find(|help| help.fix && !help.edits.is_empty())
        else {
            continue;
        };
        let own = help.edits.iter().all(|edit| {
            sources
                .file(edit.span.file)
                .is_some_and(|file| own_file(origin, file.path()).is_some())
        });
        let mine: Vec<(FileId, TextRange)> = help
            .edits
            .iter()
            .map(|edit| (edit.span.file, edit.span.range))
            .collect();
        let clash = mine.iter().enumerate().any(|(index, a)| {
            ranges
                .iter()
                .chain(mine.iter().take(index))
                .any(|b| overlaps(*a, *b))
        });
        if own && !clash {
            ranges.extend(mine);
            taken.push((diagnostic, help));
        }
    }
    taken
}

/// The file on disk of a source shown as `path`, when it is one of the
/// package itself: its sources are shown from the package root, under
/// `src/`, while a file of the standard library is shown under `std/` and
/// one of a dependency after its package and version (D-421, D-450, D-468).
/// For a single file (D-1949), the file itself.
fn own_file(origin: Origin<'_>, path: &str) -> Option<PathBuf> {
    match origin {
        Origin::Package(root) => path.starts_with("src/").then(|| root.join(path)),
        Origin::File(file) => (path == file.display).then(|| file.path.clone()),
    }
}

/// Whether two edits touch the same text: ranges of one file that share a
/// byte, or that start at the same place, as two insertions there do.
fn overlaps(a: (FileId, TextRange), b: (FileId, TextRange)) -> bool {
    a.0 == b.0
        && (a.1.start() == b.1.start() || (a.1.start() < b.1.end() && b.1.start() < a.1.end()))
}

#[cfg(test)]
mod tests {
    use cheby_diag::{Edit, Span, codes};

    use super::*;

    fn span(file: FileId, start: u32, end: u32) -> Span {
        Span::new(file, TextRange::new(start, end))
    }

    fn fix(file: FileId, message: &str, edits: &[(u32, u32, &str)]) -> Diagnostic {
        Diagnostic::new(codes::SYNTAX_ERROR, "m").with_fix_edits(
            message,
            edits
                .iter()
                .map(|&(start, end, replacement)| Edit {
                    span: span(file, start, end),
                    replacement: replacement.to_owned(),
                })
                .collect(),
        )
    }

    /// The sure fixes of the package's files are written, a suggestion and
    /// a fix in a dependency are not, and of two fixes on one text the
    /// first is applied.
    #[test]
    fn applies_sure_fixes_of_own_files() {
        let dir = std::env::temp_dir().join(format!("cheby-fix-unit-{}", std::process::id()));
        let root = dir.as_path();
        let _ = std::fs::remove_dir_all(root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let text = "let a = true;\nlet b = 'x'\n";
        std::fs::write(root.join("src/main.cheby"), text).unwrap();
        let mut sources = SourceMap::new();
        let main = sources.add_file("src/main.cheby", text).unwrap();
        let dep = sources
            .add_file("geometry@0.1.0/src/shape.cheby", "x;\n")
            .unwrap();
        let diagnostics = vec![
            fix(main, "write True", &[(8, 12, "True")]),
            fix(main, "remove the semicolon", &[(12, 13, "")]),
            fix(main, "double quotes", &[(22, 23, "\""), (24, 25, "\"")]),
            // Overlaps the first fix: it waits.
            fix(main, "another reading", &[(8, 12, "False")]),
            fix(dep, "not ours", &[(1, 2, "")]),
            Diagnostic::new(codes::SYNTAX_ERROR, "m").with_suggestion(
                "a guess",
                span(main, 4, 5),
                "c",
            ),
        ];
        let output = CheckOutput {
            sources,
            diagnostics,
            deny_warnings: false,
        };
        let applied = apply_fixes(root, &output).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("src/main.cheby")).unwrap(),
            "let a = True\nlet b = \"x\"\n"
        );
        let shown: Vec<(&str, u32, u32)> = applied
            .iter()
            .map(|f| (f.message.as_str(), f.line, f.column))
            .collect();
        assert_eq!(
            shown,
            [
                ("write True", 1, 9),
                ("remove the semicolon", 1, 13),
                ("double quotes", 2, 9),
            ]
        );
        assert!(applied.iter().all(|f| f.path == "src/main.cheby"));
        // The file is not what was checked any more, so nothing is applied.
        assert_eq!(apply_fixes(root, &output).unwrap(), Vec::new());
        std::fs::remove_dir_all(root).unwrap();
    }
}
