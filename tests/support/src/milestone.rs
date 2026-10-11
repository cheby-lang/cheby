//! Milestones and expected-fail tracking.
//!
//! `tests/milestone` names the milestone in progress. A test whose
//! milestone is earlier must pass. A test of the milestone in progress is
//! pending: it may pass or fail, and the result is only reported. A test
//! of a later milestone must fail, so an unexpected pass is an error and
//! the milestone table in docs/plan/README.md stays accurate (D-288).
//!
//! Strict runs (D-544) are opt-in through three environment variables,
//! and CI sets none of them:
//!
//! - `CHEBY_MILESTONE=<Mn>` overrides `tests/milestone` for one run
//!   ([`in_effect`]), so with `M2` every M1 test is required.
//! - `CHEBY_PENDING=require` turns a failing pending test into a failure,
//!   except tests whose name matches a pattern of the file that
//!   `CHEBY_PENDING_ALLOW` names ([`Policy`], [`AllowList`]).
//! - `CHEBY_RT_POISON=1` is set by the harnesses for every `cheby` they
//!   start ([`POISON_ENV`]), so freed objects are poisoned.
//!
//! Only a test pending because its milestone is the one in progress is
//! made strict ([`Expectation::Pending`]). A test that is pending by a
//! rule that holds at every milestone, such as a `skip-run` example's
//! `cheby test` trial (D-445, D-472), stays report-only
//! ([`Expectation::Tolerated`], D-566). So does a test
//! of a milestone later than `tests/milestone` that the override made
//! pending: `CHEBY_MILESTONE=M2` requires every M1 test, not the M2 ones
//! ([`Setting::expectation`]).

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

/// Overrides `tests/milestone` for one run (D-544).
pub const MILESTONE_ENV: &str = "CHEBY_MILESTONE";
/// `require` makes failing pending tests failures (D-544).
pub const PENDING_ENV: &str = "CHEBY_PENDING";
/// Names the allow list of a strict run (D-544).
pub const PENDING_ALLOW_ENV: &str = "CHEBY_PENDING_ALLOW";
/// Set to `1` for every `cheby` the harnesses start (D-544).
pub const POISON_ENV: &str = "CHEBY_RT_POISON";

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

/// The milestone this run treats as in progress: `CHEBY_MILESTONE` when it
/// is set and not empty, `tests/milestone` otherwise (D-544).
///
/// # Errors
///
/// Returns a message if the variable names no milestone, or if the file
/// is read and cannot be.
pub fn in_effect(root: &Path) -> Result<Milestone, String> {
    with_override(std::env::var_os(MILESTONE_ENV), || current(root))
}

/// The milestones of one run: the one in effect and the one
/// `tests/milestone` names. They differ when `CHEBY_MILESTONE` overrides
/// the file (D-544).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setting {
    /// The milestone this run treats as in progress.
    pub current: Milestone,
    /// The milestone `tests/milestone` names.
    pub file: Milestone,
}

impl Setting {
    /// Reads `tests/milestone` and applies `CHEBY_MILESTONE`.
    ///
    /// # Errors
    ///
    /// Returns a message if the file cannot be read or names no milestone,
    /// or if the variable names no milestone.
    pub fn read(root: &Path) -> Result<Self, String> {
        let file = current(root)?;
        let current = with_override(std::env::var_os(MILESTONE_ENV), || Ok(file))?;
        Ok(Self { current, file })
    }

    /// What a test of milestone `test` is expected to do in this run.
    ///
    /// A test that is pending only because the override moved the
    /// milestone in progress past the file's, such as an M2 test in a run
    /// with `CHEBY_MILESTONE=M2` while `tests/milestone` says M1, is
    /// [`Expectation::Tolerated`]: the override makes every test of the
    /// file's milestone required (D-544) and says nothing about the tests
    /// of the next one, which may still fail at the exit of the file's
    /// milestone (D-566).
    #[must_use]
    pub fn expectation(self, test: Milestone) -> Expectation {
        match Expectation::new(test, self.current) {
            Expectation::Pending if test > self.file => Expectation::Tolerated,
            other => other,
        }
    }
}

/// [`in_effect`] with the variable's value passed in.
fn with_override(
    value: Option<OsString>,
    file: impl FnOnce() -> Result<Milestone, String>,
) -> Result<Milestone, String> {
    let Some(value) = value else {
        return file();
    };
    let text = value
        .into_string()
        .map_err(|v| format!("{MILESTONE_ENV}: not valid UTF-8: {}", v.display()))?;
    if text.trim().is_empty() {
        return file();
    }
    text.parse().map_err(|e| format!("{MILESTONE_ENV}: {e}"))
}

/// What a test is expected to do, given its milestone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expectation {
    /// The milestone is finished: the test must pass.
    Pass,
    /// The milestone is in progress: the result is reported only, or, with
    /// `CHEBY_PENDING=require`, required unless the allow list names it.
    Pending,
    /// Pending by a rule that holds at every milestone, such as a check-only
    /// example before its milestone (D-445) or an example's `cheby test`
    /// trial before its milestone or without its fixture (D-472). Reported
    /// only, even with `CHEBY_PENDING=require`.
    Tolerated,
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
            Self::Pending | Self::Tolerated => "pending",
            Self::Fail => "xfail",
        }
    }
}

/// The patterns of a `CHEBY_PENDING_ALLOW` file: test names whose failure
/// a strict run still accepts.
///
/// One pattern per line. `#` starts a comment that runs to the end of the
/// line, and blank lines are ignored. The pattern is the first word of
/// what is left; the rest of the line is a note, such as the reason and
/// the gate a carried test must pass by. In a pattern, `*` matches any
/// run of characters (`/` and `:` included), `?` matches one character,
/// and every other character matches itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowList {
    patterns: Vec<String>,
}

impl AllowList {
    /// Parses the text of an allow-list file.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let patterns = text
            .lines()
            .filter_map(|line| {
                let code = line.split_once('#').map_or(line, |(code, _)| code);
                code.split_whitespace().next().map(str::to_owned)
            })
            .collect();
        Self { patterns }
    }

    /// The first pattern that matches `name`, if any.
    #[must_use]
    pub fn matching(&self, name: &str) -> Option<&str> {
        self.patterns
            .iter()
            .find(|p| glob_matches(p, name))
            .map(String::as_str)
    }

    /// The patterns, in file order.
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }
}

/// Whether `name` matches the glob `pattern` (`*` and `?`, see
/// [`AllowList`]). Iterative, with one backtracking point per `*`.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0_usize, 0_usize);
    // The position after the last `*` seen, and the name position it was
    // tried at.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                p = p.saturating_add(1);
                star = Some((p, n));
            }
            Some(&c) if c == '?' || name.get(n) == Some(&c) => {
                p = p.saturating_add(1);
                n = n.saturating_add(1);
            }
            _ => match star {
                Some((after, tried)) => {
                    let next = tried.saturating_add(1);
                    star = Some((after, next));
                    p = after;
                    n = next;
                }
                None => return false,
            },
        }
    }
    pattern
        .get(p..)
        .is_some_and(|rest| rest.iter().all(|&c| c == '*'))
}

/// How failing pending tests are judged in this run (D-544).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    /// `CHEBY_PENDING=require`: a failing pending test fails the run.
    pub require: bool,
    /// The patterns of `CHEBY_PENDING_ALLOW`, used only when `require`.
    pub allow: AllowList,
}

impl Policy {
    /// Reads `CHEBY_PENDING` and `CHEBY_PENDING_ALLOW`. Without them the
    /// policy is D-288's.
    ///
    /// # Errors
    ///
    /// Returns a message for an unknown `CHEBY_PENDING` value or an allow
    /// list that cannot be read.
    pub fn from_env() -> Result<Self, String> {
        Self::from_vars(
            std::env::var_os(PENDING_ENV),
            std::env::var_os(PENDING_ALLOW_ENV).map(PathBuf::from),
        )
    }

    /// [`Policy::from_env`] with the variables' values passed in. An empty
    /// or absent `CHEBY_PENDING`, or `report`, is D-288's behavior;
    /// `require` is strict; anything else is an error, so a typo cannot
    /// make a strict run lenient. The allow list is read only when strict.
    ///
    /// # Errors
    ///
    /// As [`Policy::from_env`].
    pub fn from_vars(pending: Option<OsString>, allow: Option<PathBuf>) -> Result<Self, String> {
        let pending = pending
            .map(|v| {
                v.into_string()
                    .map_err(|v| format!("{PENDING_ENV}: not valid UTF-8: {}", v.display()))
            })
            .transpose()?;
        let require = match pending.as_deref().map(str::trim) {
            None | Some("" | "report") => false,
            Some("require") => true,
            Some(other) => {
                return Err(format!(
                    "{PENDING_ENV}: unknown value `{other}`, expected `require` or `report`"
                ));
            }
        };
        let allow = match allow {
            Some(path) if require && !path.as_os_str().is_empty() => {
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("{PENDING_ALLOW_ENV}: {}: {e}", path.display()))?;
                AllowList::parse(&text)
            }
            _ => AllowList::default(),
        };
        Ok(Self { require, allow })
    }
}

/// What happened to a pending or tolerated test, for the summary.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Passed,
    /// Failed and only reported (D-288, or a tolerated test).
    StillFailing,
    /// Failed in a strict run, and the allow list names it (the pattern).
    Allowed(String),
    /// Failed in a strict run, not on the allow list: the run fails.
    Required,
}

impl Outcome {
    /// The position of the outcome's group in the summary.
    const fn rank(&self) -> u8 {
        match self {
            Self::Passed => 0,
            Self::Allowed(_) => 1,
            Self::Required => 2,
            Self::StillFailing => 3,
        }
    }
}

/// Turns raw test results into verdicts, and remembers pending results so
/// they can be summarized after the run.
#[derive(Debug, Clone, Default)]
pub struct Verdicts {
    policy: Arc<Policy>,
    pending: Arc<Mutex<Vec<(String, Outcome)>>>,
}

impl Verdicts {
    /// Verdicts under `policy`. [`Verdicts::default`] is D-288's policy.
    #[must_use]
    pub fn new(policy: Policy) -> Self {
        Self {
            policy: Arc::new(policy),
            pending: Arc::default(),
        }
    }

    /// Verdicts under the policy of the environment ([`Policy::from_env`]).
    ///
    /// # Errors
    ///
    /// As [`Policy::from_env`].
    pub fn from_env() -> Result<Self, String> {
        Policy::from_env().map(Self::new)
    }

    /// Whether this run is strict (`CHEBY_PENDING=require`).
    #[must_use]
    pub fn strict(&self) -> bool {
        self.policy.require
    }

    /// Turns the raw `result` of a test into its verdict.
    ///
    /// # Errors
    ///
    /// Returns a message if a test that must pass failed, a test that must
    /// fail passed, or, in a strict run, a pending test failed and the
    /// allow list does not name it.
    pub fn judge(
        &self,
        name: &str,
        milestone: Milestone,
        expectation: Expectation,
        result: Result<(), String>,
    ) -> Result<(), String> {
        match (expectation, result) {
            (Expectation::Pass, result) => result,
            (Expectation::Pending | Expectation::Tolerated, Ok(())) => {
                self.record(name, Outcome::Passed);
                Ok(())
            }
            (Expectation::Tolerated, Err(_)) => {
                self.record(name, Outcome::StillFailing);
                Ok(())
            }
            (Expectation::Pending, Err(message)) => {
                if !self.policy.require {
                    self.record(name, Outcome::StillFailing);
                    return Ok(());
                }
                if let Some(pattern) = self.policy.allow.matching(name) {
                    self.record(name, Outcome::Allowed(pattern.to_owned()));
                    return Ok(());
                }
                self.record(name, Outcome::Required);
                Err(format!(
                    "`{name}` is pending ({milestone} is in progress) and failed; \
                     {PENDING_ENV}=require makes that a failure, and no pattern of \
                     {PENDING_ALLOW_ENV} names it\n{message}"
                ))
            }
            (Expectation::Fail, Ok(())) => Err(format!(
                "unexpected pass: `{name}` is marked for {milestone}, which has not started. \
                 Move it to an earlier milestone if it now works as intended."
            )),
            (Expectation::Fail, Err(_)) => Ok(()),
        }
    }

    fn record(&self, name: &str, outcome: Outcome) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((name.to_owned(), outcome));
    }

    /// Prints how many pending tests passed, then lists the pending tests
    /// that passed, the failures the allow list accepted, the failures a
    /// strict run rejected, and the failures that are only reported.
    pub fn report(&self, what: &str) {
        print!("{}", self.summary(what));
    }

    /// The text [`Verdicts::report`] prints, empty when nothing was pending.
    #[must_use]
    pub fn summary(&self, what: &str) -> String {
        use std::fmt::Write as _;
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if pending.is_empty() {
            return String::new();
        }
        // Grouped by outcome, in the order of `Outcome`, then by name.
        pending.sort_by(|(a, x), (b, y)| x.rank().cmp(&y.rank()).then_with(|| a.cmp(b)));
        let passed = pending
            .iter()
            .filter(|(_, o)| *o == Outcome::Passed)
            .count();
        let mut out = String::new();
        let mode = if self.policy.require {
            format!(" ({PENDING_ENV}=require)")
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "pending {what}{mode}: {passed} of {} pass",
            pending.len()
        );
        for (name, outcome) in &pending {
            let _ = match outcome {
                Outcome::Passed => writeln!(out, "  passed: {name}"),
                Outcome::Allowed(pattern) => {
                    writeln!(out, "  allowed failure: {name} (allowed by `{pattern}`)")
                }
                Outcome::Required => writeln!(out, "  required, failed: {name}"),
                Outcome::StillFailing => writeln!(out, "  still failing: {name}"),
            };
        }
        out
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

    #[test]
    fn the_override_wins_over_the_file() {
        let file = || Ok(Milestone::M1);
        assert_eq!(with_override(None, file), Ok(Milestone::M1));
        assert_eq!(with_override(Some("M2".into()), file), Ok(Milestone::M2));
        assert_eq!(with_override(Some(" M3\n".into()), file), Ok(Milestone::M3));
        // An empty value is no override.
        assert_eq!(
            with_override(Some(String::new().into()), file),
            Ok(Milestone::M1)
        );
        let err = with_override(Some("M9".into()), file).unwrap_err();
        assert!(
            err.starts_with("CHEBY_MILESTONE: unknown milestone `M9`"),
            "{err}"
        );
        // The file is not read when the override is set.
        let unreadable = || Err("no file".to_owned());
        assert_eq!(
            with_override(Some("M2".into()), unreadable),
            Ok(Milestone::M2)
        );
        assert!(with_override(None, unreadable).is_err());
    }

    #[test]
    fn with_m2_every_m1_test_is_required() {
        let current = with_override(Some("M2".into()), || Ok(Milestone::M1)).unwrap();
        assert_eq!(Expectation::new(Milestone::M1, current), Expectation::Pass);
        let v = Verdicts::default();
        assert!(
            v.judge(
                "a",
                Milestone::M1,
                Expectation::new(Milestone::M1, current),
                Err("x".into())
            )
            .is_err()
        );
    }

    /// Gate 6 runs `CHEBY_MILESTONE=M2 CHEBY_PENDING=require` with an empty
    /// allow list. M1 tests must pass, while M2 tests, such as the
    /// check-only examples that still report E0038 at M1's exit (D-477),
    /// are only reported.
    #[test]
    fn the_override_requires_only_the_file_milestone() {
        let setting = Setting {
            current: Milestone::M2,
            file: Milestone::M1,
        };
        assert_eq!(setting.expectation(Milestone::M1), Expectation::Pass);
        assert_eq!(setting.expectation(Milestone::M2), Expectation::Tolerated);
        assert_eq!(setting.expectation(Milestone::M3), Expectation::Fail);
        let strict = Verdicts::new(Policy {
            require: true,
            allow: AllowList::parse("# empty\n"),
        });
        assert!(
            strict
                .judge(
                    "005_prime_sieve",
                    Milestone::M2,
                    setting.expectation(Milestone::M2),
                    Err("E0038".into())
                )
                .is_ok()
        );
        assert!(
            strict
                .judge(
                    "001_hello_world",
                    Milestone::M1,
                    setting.expectation(Milestone::M1),
                    Err("x".into())
                )
                .is_err()
        );
        // Without an override, the milestone in progress stays strict.
        let plain = Setting {
            current: Milestone::M1,
            file: Milestone::M1,
        };
        assert_eq!(plain.expectation(Milestone::M1), Expectation::Pending);
        assert_eq!(plain.expectation(Milestone::M2), Expectation::Fail);
    }

    #[test]
    fn pending_values_are_checked() {
        let none = Policy::from_vars(None, None).unwrap();
        assert!(!none.require);
        assert!(!Policy::from_vars(Some("".into()), None).unwrap().require);
        assert!(
            !Policy::from_vars(Some("report".into()), None)
                .unwrap()
                .require
        );
        assert!(
            Policy::from_vars(Some("require".into()), None)
                .unwrap()
                .require
        );
        let err = Policy::from_vars(Some("requre".into()), None).unwrap_err();
        assert!(err.contains("unknown value `requre`"), "{err}");
    }

    #[test]
    fn the_allow_list_is_read_only_when_strict() {
        let missing = PathBuf::from("/nonexistent/cheby/carried.txt");
        // Not strict: the file is never opened.
        let lenient = Policy::from_vars(None, Some(missing.clone())).unwrap();
        assert_eq!(lenient.allow, AllowList::default());
        // Strict: a missing file is an error, not an empty list.
        let err = Policy::from_vars(Some("require".into()), Some(missing)).unwrap_err();
        assert!(err.starts_with("CHEBY_PENDING_ALLOW: "), "{err}");

        let dir = std::env::temp_dir().join(format!("cheby-allow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("carried.txt");
        std::fs::write(&file, "# carried\n002_greet  # W2-E, gate 2\n").unwrap();
        let strict = Policy::from_vars(Some("require".into()), Some(file)).unwrap();
        assert_eq!(strict.allow.patterns(), ["002_greet"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn allow_lists_have_comments_and_notes() {
        let list = AllowList::parse(
            "# The carried list.\n\
             \n\
             002_greet          # W2-E: needs env::args (gate 2)\n\
             \t05-expressions/case/*   W2-B decision trees, gate 2\n\
             #006_game_of_life\n\
             ???_two_sum::test\n",
        );
        assert_eq!(
            list.patterns(),
            ["002_greet", "05-expressions/case/*", "???_two_sum::test"]
        );
        assert_eq!(list.matching("002_greet"), Some("002_greet"));
        assert_eq!(list.matching("002_greet::test"), None);
        assert_eq!(list.matching("006_game_of_life"), None);
        assert_eq!(
            list.matching("05-expressions/case/nested/deep"),
            Some("05-expressions/case/*")
        );
        assert_eq!(
            list.matching("024_two_sum::test"),
            Some("???_two_sum::test")
        );
        assert_eq!(list.matching("24_two_sum::test"), None);
    }

    #[test]
    fn globs() {
        assert!(glob_matches("*", ""));
        assert!(glob_matches("*", "anything/at::all"));
        assert!(glob_matches("a*b*c", "a__b__b__c"));
        assert!(glob_matches("*::test", "001_hello_world::test"));
        assert!(!glob_matches("*::test", "001_hello_world"));
        assert!(glob_matches("0??_*", "012_shapes"));
        assert!(!glob_matches("0??_*", "12_shapes"));
        assert!(!glob_matches("abc", "abcd"));
        assert!(!glob_matches("abcd", "abc"));
        assert!(glob_matches("abc**", "abc"));
        assert!(glob_matches("é?", "éé"));
    }

    #[test]
    fn strict_runs_require_pending_tests_except_allowed_ones() {
        let policy = Policy {
            require: true,
            allow: AllowList::parse("002_*\n"),
        };
        let v = Verdicts::new(policy);
        assert!(v.strict());
        let pending = Expectation::Pending;
        assert!(v.judge("001_a", Milestone::M1, pending, Ok(())).is_ok());
        assert!(
            v.judge("002_b", Milestone::M1, pending, Err("boom".into()))
                .is_ok()
        );
        let err = v
            .judge("003_c", Milestone::M1, pending, Err("boom".into()))
            .unwrap_err();
        assert!(
            err.contains("CHEBY_PENDING=require") && err.ends_with("\nboom"),
            "{err}"
        );
        // A tolerated test is never made strict.
        assert!(
            v.judge(
                "020_d::test",
                Milestone::M3,
                Expectation::Tolerated,
                Err("x".into())
            )
            .is_ok()
        );
        // The allow list does not cover tests that must pass or fail.
        assert!(
            v.judge("002_e", Milestone::M0, Expectation::Pass, Err("x".into()))
                .is_err()
        );
        assert!(
            v.judge("002_f", Milestone::M2, Expectation::Fail, Ok(()))
                .is_err()
        );
        assert_eq!(
            v.summary("examples"),
            "pending examples (CHEBY_PENDING=require): 1 of 4 pass\n  \
             passed: 001_a\n  \
             allowed failure: 002_b (allowed by `002_*`)\n  \
             required, failed: 003_c\n  \
             still failing: 020_d::test\n"
        );
    }

    #[test]
    fn default_runs_only_report() {
        let v = Verdicts::default();
        assert!(!v.strict());
        assert_eq!(v.summary("UI tests"), "");
        assert!(
            v.judge("b", Milestone::M1, Expectation::Pending, Err("x".into()))
                .is_ok()
        );
        assert!(
            v.judge("a", Milestone::M1, Expectation::Pending, Ok(()))
                .is_ok()
        );
        assert_eq!(
            v.summary("UI tests"),
            "pending UI tests: 1 of 2 pass\n  passed: a\n  still failing: b\n"
        );
    }
}
