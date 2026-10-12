//! The header of a REPL transcript (D-638, D-726).
//!
//! A transcript `tests/repl/<name>.repl` may start with directive lines
//! `// @key: value`, before its first input line, as a UI test does. They
//! are not part of the session: the harness neither feeds them to
//! `cheby repl` nor expects them as output, and blessing keeps them. A
//! session's output always starts with the echo of its first input line,
//! `> …`, so the header cannot be mistaken for output. There are two
//! directives:
//!
//! - `@decisions: D-NNN, …`, the decision-log entries the session pins,
//!   which `cargo xtask coverage` counts as it counts a UI test's (D-638);
//! - `@milestone: M<n>`, the milestone in which the transcript must pass,
//!   M1 by default, judged against `tests/milestone` like a UI test
//!   (D-726, D-288).

use std::collections::BTreeSet;

use crate::milestone::Milestone;

/// The directives of a transcript's header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub decisions: BTreeSet<u32>,
    /// The milestone in which the transcript must pass (D-726).
    pub milestone: Milestone,
}

impl Default for Header {
    fn default() -> Self {
        Self {
            decisions: BTreeSet::new(),
            milestone: Milestone::M1,
        }
    }
}

/// Splits a transcript into its header, the leading lines that start with
/// `// @`, and the session after it.
#[must_use]
pub fn split(text: &str) -> (&str, &str) {
    let end: usize = text
        .split_inclusive('\n')
        .take_while(|line| line.starts_with("// @"))
        .map(str::len)
        .sum();
    (
        text.get(..end).unwrap_or_default(),
        text.get(end..).unwrap_or(text),
    )
}

impl Header {
    /// Parses the header lines that [`split`] returned.
    ///
    /// # Errors
    ///
    /// Returns a message for a malformed or unknown directive.
    pub fn parse(header: &str) -> Result<Self, String> {
        let mut decisions = BTreeSet::new();
        let mut milestone = None;
        for line in header.lines() {
            let directive = line
                .strip_prefix("// @")
                .ok_or_else(|| format!("`{line}` is not a directive"))?;
            let (key, value) = directive
                .split_once(':')
                .ok_or_else(|| format!("directive `{line}` has no `:`"))?;
            match key.trim() {
                "decisions" => {
                    for d in value.split(',') {
                        decisions.insert(crate::decisions::parse(d)?);
                    }
                }
                "milestone" => {
                    if milestone.is_some() {
                        return Err("`@milestone` is given twice".to_owned());
                    }
                    milestone = Some(
                        value
                            .trim()
                            .parse::<Milestone>()
                            .map_err(|e| format!("@milestone: {e}"))?,
                    );
                }
                other => {
                    return Err(format!(
                        "unknown transcript directive `@{other}`; only `@decisions` and \
                         `@milestone` are allowed"
                    ));
                }
            }
        }
        Ok(Self {
            decisions,
            milestone: milestone.unwrap_or(Milestone::M1),
        })
    }

    /// Reads the header of the transcript `text`.
    ///
    /// # Errors
    ///
    /// Returns a message for a malformed or unknown directive.
    pub fn of(text: &str) -> Result<Self, String> {
        Self::parse(split(text).0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_the_header_from_the_session() {
        let text = "// @decisions: D-624, D-098\n> 1 + 1\n2 : Int\n// @x: not a header\n";
        let (header, session) = split(text);
        assert_eq!(header, "// @decisions: D-624, D-098\n");
        assert_eq!(session, "> 1 + 1\n2 : Int\n// @x: not a header\n");
        let parsed = Header::parse(header);
        assert_eq!(
            parsed.map(|h| h.decisions.into_iter().collect::<Vec<_>>()),
            Ok(vec![98, 624])
        );
    }

    #[test]
    fn a_transcript_without_a_header() {
        let text = "> 1\n1 : Int\n";
        assert_eq!(split(text), ("", text));
        assert_eq!(Header::of(text), Ok(Header::default()));
        assert_eq!(split(""), ("", ""));
    }

    #[test]
    fn bad_directives_are_errors() {
        assert!(Header::of("// @mode: check-pass\n> 1\n").is_err());
        assert!(Header::of("// @decisions D-001\n> 1\n").is_err());
        assert!(Header::of("// @decisions: D-1\n> 1\n").is_err());
        assert!(Header::of("// @milestone: M9\n> 1\n").is_err());
        assert!(Header::of("// @milestone: 2\n> 1\n").is_err());
        assert!(Header::of("// @milestone: M2\n// @milestone: M3\n> 1\n").is_err());
    }

    /// D-726: the milestone defaults to M1.
    #[test]
    fn the_milestone_key() {
        assert_eq!(Header::of("> 1\n").map(|h| h.milestone), Ok(Milestone::M1));
        let header = Header::of("// @milestone: M2\n// @decisions: D-726, D-672\n> 1\n");
        assert_eq!(
            header,
            Ok(Header {
                decisions: BTreeSet::from([672, 726]),
                milestone: Milestone::M2,
            })
        );
    }
}
