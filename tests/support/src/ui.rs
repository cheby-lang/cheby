//! UI tests in `tests/ui/`: one `.cheby` file per rule, with a header of
//! `// @key: value` directives before any other line.
//!
//! ```cheby
//! // @mode: check-fail
//! // @decisions: D-244, D-245
//! // @milestone: M1
//! //! A pipe into a call needs an explicit hole.
//! ```
//!
//! Directives:
//!
//! - `@mode` (required): `check-pass`, `check-fail`, `parse-fail`,
//!   `run-pass` or `run-fail`. `parse-fail` is a `check-fail` that both
//!   the compiler parser and tree-sitter must reject.
//! - `@decisions`: the decision-log entries the test pins, for coverage.
//! - `@milestone`: the milestone in which the test must pass, M1 by default.
//! - `@exit`: the expected exit code of `run-fail`, 101 by default.
//!
//! Goldens sit next to the file: `.stderr` for diagnostics, `.stdout` for
//! the output of `run-*` tests.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::milestone::Milestone;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    CheckPass,
    CheckFail,
    ParseFail,
    RunPass,
    RunFail,
}

impl Mode {
    fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "check-pass" => Self::CheckPass,
            "check-fail" => Self::CheckFail,
            "parse-fail" => Self::ParseFail,
            "run-pass" => Self::RunPass,
            "run-fail" => Self::RunFail,
            other => {
                return Err(format!(
                    "unknown mode `{other}`, expected check-pass, check-fail, parse-fail, \
                     run-pass or run-fail"
                ));
            }
        })
    }

    /// Whether the compiler must reject the file.
    #[must_use]
    pub const fn fails_to_compile(self) -> bool {
        matches!(self, Self::CheckFail | Self::ParseFail)
    }

    /// Whether the file is meant to be a syntax error.
    #[must_use]
    pub const fn is_syntax_error(self) -> bool {
        matches!(self, Self::ParseFail)
    }

    /// Whether the test runs the program, not just checks it.
    #[must_use]
    pub const fn runs(self) -> bool {
        matches!(self, Self::RunPass | Self::RunFail)
    }
}

#[derive(Debug, Clone)]
pub struct Header {
    pub mode: Mode,
    pub decisions: BTreeSet<u32>,
    pub milestone: Milestone,
    pub exit: Option<i32>,
}

impl Header {
    /// Parses the leading `// @key: value` lines of `source`.
    ///
    /// # Errors
    ///
    /// Returns a message for a malformed or unknown directive, a missing
    /// `@mode`, or `@exit` without `@mode: run-fail`.
    pub fn parse(source: &str) -> Result<Self, String> {
        let mut mode = None;
        let mut decisions = BTreeSet::new();
        let mut milestone = Milestone::M1;
        let mut exit = None;
        for line in source.lines() {
            let Some(directive) = line.strip_prefix("// @") else {
                break;
            };
            let (key, value) = directive
                .split_once(':')
                .ok_or_else(|| format!("directive `{line}` has no `:`"))?;
            let value = value.trim();
            match key.trim() {
                "mode" => mode = Some(Mode::parse(value)?),
                "decisions" => {
                    for d in value.split(',') {
                        decisions.insert(crate::decisions::parse(d)?);
                    }
                }
                "milestone" => milestone = value.parse()?,
                "exit" => exit = Some(value.parse().map_err(|e| format!("@exit: {e}"))?),
                other => return Err(format!("unknown directive `@{other}`")),
            }
        }
        let mode = mode.ok_or("missing `// @mode:` directive")?;
        if exit.is_some() && mode != Mode::RunFail {
            return Err("`@exit` is only allowed with `@mode: run-fail`".to_owned());
        }
        Ok(Self {
            mode,
            decisions,
            milestone,
            exit,
        })
    }
}

#[derive(Debug, Clone)]
pub struct UiTest {
    /// Path relative to `tests/ui`, with `/` separators and no extension.
    pub name: String,
    pub path: PathBuf,
    pub header: Header,
}

impl UiTest {
    /// The golden file next to the test, with extension `ext`.
    #[must_use]
    pub fn golden(&self, ext: &str) -> PathBuf {
        self.path.with_extension(ext)
    }
}

/// Every UI test, sorted by path.
///
/// # Errors
///
/// Returns a message if a file cannot be read or has a bad header.
pub fn all(root: &Path) -> Result<Vec<UiTest>, String> {
    let dir = root.join("tests/ui");
    let files = crate::files_with_extension(&dir, "cheby").map_err(|e| e.to_string())?;
    files
        .into_iter()
        .map(|path| {
            let source =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let header = Header::parse(&source)
                .map_err(|e| format!("{}: {e}", crate::display_relative(&path, root)))?;
            let name = crate::display_relative(&path.with_extension(""), &dir);
            Ok(UiTest { name, path, header })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_header() {
        let h = Header::parse(
            "// @mode: run-fail\n// @decisions: D-244, D-245\n// @exit: 1\n//! Doc.\n// @mode: x\n",
        )
        .unwrap();
        assert_eq!(h.mode, Mode::RunFail);
        assert_eq!(h.decisions.into_iter().collect::<Vec<_>>(), vec![244, 245]);
        assert_eq!(h.milestone, Milestone::M1);
        assert_eq!(h.exit, Some(1));
    }

    #[test]
    fn rejects_bad_headers() {
        assert!(Header::parse("//! no mode\n").is_err());
        assert!(Header::parse("// @mode: check-pass\n// @colour: red\n").is_err());
        assert!(Header::parse("// @mode: check-pass\n// @exit: 1\n").is_err());
    }
}
