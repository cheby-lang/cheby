//! The naive-versus-Perceus differential (D-718): `cheby check`, then
//! three runs of the same package, compared.
//!
//! A program's output cannot be captured in process, so every run starts
//! `cheby` as a subprocess. The naive run and the first Perceus run set
//! `CHEBY_RT_POISON=1`, as the harnesses do; poisoning makes every count
//! operation, reuse included, call the runtime's checked entries. The
//! third run is Perceus with `CHEBY_RT_POISON=0`, the only one that
//! executes the inline count and reuse paths code generation emits
//! (D-844). A debug `cheby` exits 102 when objects
//! leak (D-458), also after a panic (D-710), so every run is a leak check
//! too.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::process::{self, RunOutput};

/// How [`differential_with`] runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffOptions {
    /// The limit of each `cheby` command.
    pub timeout: Duration,
    /// Where the Perceus run writes its dumps (`CHEBY_DUMP_DIR`), if
    /// anywhere.
    pub dump_dir: Option<PathBuf>,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            dump_dir: None,
        }
    }
}

/// The run both modes agreed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffReport {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    /// Whether the program has a planted final panic.
    pub planted: bool,
}

/// What went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffErrorKind {
    /// `cheby check` failed: a generator bug, or an input the fuzz
    /// target discards.
    NotChecked,
    /// Two of the runs differ.
    Mismatch,
    /// Every run exited 102: objects leaked (D-458, D-710).
    Leak,
    /// Every run ended with a status other than 0, or than 101 for a
    /// planted panic.
    BadStatus,
    /// A command did not finish in time.
    Timeout,
    /// `cheby` could not be started.
    Io,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffError {
    pub kind: DiffErrorKind,
    pub message: String,
}

impl fmt::Display for DiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for DiffError {}

fn error(kind: DiffErrorKind, message: impl Into<String>) -> DiffError {
    DiffError {
        kind,
        message: message.into(),
    }
}

/// The variables that select what a run does; each run sets its own.
/// `CHEBY_RT_STATS` would add lines to standard error.
const CONTROLLED: [&str; 4] = [
    "CHEBY_RC",
    "CHEBY_RT_POISON",
    "CHEBY_DUMP_DIR",
    "CHEBY_RT_STATS",
];

fn command(cheby: &Path, dir: &Path, verb: &str) -> Command {
    let mut cmd = Command::new(cheby);
    cmd.arg("--color=never")
        .arg(verb)
        .current_dir(dir)
        .env("NO_COLOR", "1");
    for name in CONTROLLED {
        cmd.env_remove(name);
    }
    cmd
}

fn run(cmd: Command, timeout: Duration, what: &str) -> Result<RunOutput, DiffError> {
    let out = process::run(cmd, None, timeout).map_err(|e| error(DiffErrorKind::Io, e))?;
    if out.timed_out {
        return Err(error(
            DiffErrorKind::Timeout,
            format!("{what} did not finish within {timeout:?}"),
        ));
    }
    Ok(out)
}

/// The first line where `a` (of the run named `a_name`) and `b` differ,
/// for a message.
fn first_difference(a_name: &str, a: &str, b_name: &str, b: &str) -> String {
    let mut left = a.lines();
    let mut right = b.lines();
    let mut line = 1usize;
    loop {
        match (left.next(), right.next()) {
            (None, None) => return "the texts differ only in their line ends".to_owned(),
            (l, r) if l != r => {
                return format!(
                    "line {line}: {a_name} {:?}, {b_name} {:?}",
                    l.unwrap_or("<end>"),
                    r.unwrap_or("<end>")
                );
            }
            _ => line = line.saturating_add(1),
        }
    }
}

/// `Err(Mismatch)` naming the first difference between two runs.
fn compare(a_name: &str, a: &RunOutput, b_name: &str, b: &RunOutput) -> Result<(), DiffError> {
    if a.status != b.status {
        return Err(error(
            DiffErrorKind::Mismatch,
            format!(
                "the status differs: {a_name} {}, {b_name} {}\n{a_name} stderr:\n{}\n{b_name} stderr:\n{}",
                a.describe_status(),
                b.describe_status(),
                a.stderr,
                b.stderr
            ),
        ));
    }
    if a.stdout != b.stdout {
        return Err(error(
            DiffErrorKind::Mismatch,
            format!(
                "standard output differs at {}",
                first_difference(a_name, &a.stdout, b_name, &b.stdout)
            ),
        ));
    }
    if a.stderr != b.stderr {
        return Err(error(
            DiffErrorKind::Mismatch,
            format!(
                "standard error differs at {}",
                first_difference(a_name, &a.stderr, b_name, &b.stderr)
            ),
        ));
    }
    Ok(())
}

/// [`super::differential`] with options, returning what every run gave.
///
/// # Errors
///
/// A [`DiffError`] whose kind says what failed.
pub fn differential_with(
    cheby: &Path,
    dir: &Path,
    options: &DiffOptions,
) -> Result<DiffReport, DiffError> {
    let main = std::fs::read_to_string(dir.join("src/main.cheby")).map_err(|e| {
        error(
            DiffErrorKind::Io,
            format!("cannot read src/main.cheby: {e}"),
        )
    })?;
    let planted = main.contains(super::PLANTED_MARKER);

    let check = run(
        command(cheby, dir, "check"),
        options.timeout,
        "`cheby check`",
    )?;
    if check.status != Some(0) {
        return Err(error(
            DiffErrorKind::NotChecked,
            format!(
                "`cheby check` ended with {}:\n{}{}",
                check.describe_status(),
                check.stdout,
                check.stderr
            ),
        ));
    }

    let mut naive = command(cheby, dir, "run");
    naive.env("CHEBY_RC", "naive").env("CHEBY_RT_POISON", "1");
    let naive = run(naive, options.timeout, "the naive run")?;

    let mut perceus = command(cheby, dir, "run");
    perceus.env("CHEBY_RT_POISON", "1");
    if let Some(dump) = &options.dump_dir {
        perceus.env("CHEBY_DUMP_DIR", dump);
    }
    let perceus = run(perceus, options.timeout, "the Perceus run")?;

    // Poisoning sends every count and reuse operation through the
    // runtime's checked entries, so only this run executes the inline
    // paths (D-844, refining D-809).
    let mut poison_off = command(cheby, dir, "run");
    poison_off.env("CHEBY_RT_POISON", "0");
    let poison_off = run(
        poison_off,
        options.timeout,
        "the Perceus run with poisoning off",
    )?;

    compare("naive", &naive, "perceus", &perceus)?;
    compare("perceus", &perceus, "perceus-poison-off", &poison_off)?;
    let expected = if planted { 101 } else { 0 };
    match perceus.status {
        Some(status) if status == expected => Ok(DiffReport {
            status,
            stdout: perceus.stdout,
            stderr: perceus.stderr,
            planted,
        }),
        Some(102) => Err(error(
            DiffErrorKind::Leak,
            format!("every run exited 102 (a leak):\n{}", perceus.stderr),
        )),
        _ => Err(error(
            DiffErrorKind::BadStatus,
            format!(
                "every run ended with {}, expected exit code {expected}:\n{}",
                perceus.describe_status(),
                perceus.stderr
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::first_difference;

    #[test]
    fn progen_first_difference_names_the_line() {
        assert_eq!(
            first_difference("naive", "a\nb\n", "perceus", "a\nc\n"),
            "line 2: naive \"b\", perceus \"c\""
        );
        assert_eq!(
            first_difference("perceus", "a\n", "perceus-poison-off", "a\nb\n"),
            "line 2: perceus \"<end>\", perceus-poison-off \"b\""
        );
    }
}
