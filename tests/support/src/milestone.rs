//! Milestones and expected-fail tracking.
//!
//! `tests/milestone` names the milestone in progress. A test whose
//! milestone is earlier must pass. A test of the milestone in progress is
//! pending: it may pass or fail, and the result is only reported. A test
//! of a later milestone must fail, so an unexpected pass is an error and
//! the milestone table in docs/plan/README.md stays accurate.

use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Milestone {
    M0,
    M1,
    M2,
    M3,
    M4,
    M5,
    M6,
}

impl FromStr for Milestone {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.trim() {
            "M0" => Self::M0,
            "M1" => Self::M1,
            "M2" => Self::M2,
            "M3" => Self::M3,
            "M4" => Self::M4,
            "M5" => Self::M5,
            "M6" => Self::M6,
            other => return Err(format!("unknown milestone `{other}`, expected M0 to M6")),
        })
    }
}

impl fmt::Display for Milestone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Reads the milestone in progress from `tests/milestone`.
///
/// # Errors
///
/// Returns a message if the file cannot be read or names no milestone.
pub fn current(root: &Path) -> Result<Milestone, String> {
    let path = root.join("tests/milestone");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    text.parse()
}

/// What a test is expected to do, given its milestone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expectation {
    /// The milestone is finished: the test must pass.
    Pass,
    /// The milestone is in progress: the result is reported only.
    Pending,
    /// The milestone is later: the test must fail.
    Fail,
}

impl Expectation {
    #[must_use]
    pub fn new(test: Milestone, current: Milestone) -> Self {
        match test.cmp(&current) {
            std::cmp::Ordering::Less => Self::Pass,
            std::cmp::Ordering::Equal => Self::Pending,
            std::cmp::Ordering::Greater => Self::Fail,
        }
    }

    /// The label shown next to the test name.
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Pending => "pending",
            Self::Fail => "xfail",
        }
    }
}

/// Turns raw test results into verdicts, and remembers pending results so
/// they can be summarized after the run.
#[derive(Debug, Clone, Default)]
pub struct Verdicts {
    pending: Arc<Mutex<Vec<(String, bool)>>>,
}

impl Verdicts {
    /// Turns the raw `result` of a test into its verdict.
    ///
    /// # Errors
    ///
    /// Returns a message if a test that must pass failed, or a test that
    /// must fail passed.
    pub fn judge(
        &self,
        name: &str,
        milestone: Milestone,
        expectation: Expectation,
        result: Result<(), String>,
    ) -> Result<(), String> {
        match (expectation, result) {
            (Expectation::Pass, result) => result,
            (Expectation::Pending, result) => {
                self.pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((name.to_owned(), result.is_ok()));
                Ok(())
            }
            (Expectation::Fail, Ok(())) => Err(format!(
                "unexpected pass: `{name}` is marked for {milestone}, which has not started. \
                 Move it to an earlier milestone if it now works as intended."
            )),
            (Expectation::Fail, Err(_)) => Ok(()),
        }
    }

    /// Prints how many pending tests passed, and which ones still fail.
    pub fn report(&self, what: &str) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if pending.is_empty() {
            return;
        }
        pending.sort();
        let failing: Vec<_> = pending
            .iter()
            .filter(|(_, ok)| !ok)
            .map(|(n, _)| n.as_str())
            .collect();
        println!(
            "pending {what}: {} of {} pass",
            pending.len().saturating_sub(failing.len()),
            pending.len()
        );
        for name in failing {
            println!("  still failing: {name}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expectations_follow_the_current_milestone() {
        assert_eq!(
            Expectation::new(Milestone::M0, Milestone::M1),
            Expectation::Pass
        );
        assert_eq!(
            Expectation::new(Milestone::M1, Milestone::M1),
            Expectation::Pending
        );
        assert_eq!(
            Expectation::new(Milestone::M2, Milestone::M1),
            Expectation::Fail
        );
    }

    #[test]
    fn unexpected_pass_is_an_error() {
        let v = Verdicts::default();
        assert!(
            v.judge("a", Milestone::M2, Expectation::Fail, Ok(()))
                .is_err()
        );
        assert!(
            v.judge("a", Milestone::M2, Expectation::Fail, Err("x".into()))
                .is_ok()
        );
        assert!(
            v.judge("a", Milestone::M1, Expectation::Pending, Err("x".into()))
                .is_ok()
        );
        assert!(
            v.judge("a", Milestone::M0, Expectation::Pass, Err("x".into()))
                .is_err()
        );
    }

    #[test]
    fn parses_milestones() {
        assert_eq!("M3\n".parse::<Milestone>(), Ok(Milestone::M3));
        assert!("M7".parse::<Milestone>().is_err());
    }
}
