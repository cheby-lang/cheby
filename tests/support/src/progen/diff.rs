//! The naive-versus-Perceus differential (D-718): `cheby check`, then
//! two runs of the same package, compared.
//!
//! A program's output cannot be captured in process, so both runs start
//! `cheby` as a subprocess, with `CHEBY_RT_POISON=1` as the harnesses
//! do. A debug `cheby` exits 102 when objects leak (D-458), also after
//! a panic (D-710), so every run is a leak check too.

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
    /// The two runs differ.
    Mismatch,
    /// Both runs exited 102: objects leaked (D-458, D-710).
    Leak,
    /// Both runs ended with a status other than 0, or than 101 for a
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
const CONTROLLED: [&str; 3] = ["CHEBY_RC", "CHEBY_RT_POISON", "CHEBY_DUMP_DIR"];

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

/// The first line where `a` and `b` differ, for a message.
fn first_difference(a: &str, b: &str) -> String {
    let mut left = a.lines();
    let mut right = b.lines();
    let mut line = 1usize;
    loop {
        match (left.next(), right.next()) {
            (None, None) => return "the texts differ only in their line ends".to_owned(),
            (l, r) if l != r => {
                return format!(
                    "line {line}: naive {:?}, perceus {:?}",
                    l.unwrap_or("<end>"),
                    r.unwrap_or("<end>")
                );
            }
            _ => line = line.saturating_add(1),
        }
    }
}

/// [`super::differential`] with options, returning what both runs gave.
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

    if naive.status != perceus.status {
        return Err(error(
            DiffErrorKind::Mismatch,
            format!(
                "the status differs: naive {}, perceus {}\nnaive stderr:\n{}\nperceus stderr:\n{}",
                naive.describe_status(),
                perceus.describe_status(),
                naive.stderr,
                perceus.stderr
            ),
        ));
    }
    if naive.stdout != perceus.stdout {
        return Err(error(
            DiffErrorKind::Mismatch,
            format!(
                "standard output differs at {}",
                first_difference(&naive.stdout, &perceus.stdout)
            ),
        ));
    }
    if naive.stderr != perceus.stderr {
        return Err(error(
            DiffErrorKind::Mismatch,
            format!(
                "standard error differs at {}",
                first_difference(&naive.stderr, &perceus.stderr)
            ),
        ));
    }
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
            format!("both runs exited 102 (a leak):\n{}", perceus.stderr),
        )),
        _ => Err(error(
            DiffErrorKind::BadStatus,
            format!(
                "both runs ended with {}, expected exit code {expected}:\n{}",
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
            first_difference("a\nb\n", "a\nc\n"),
            "line 2: naive \"b\", perceus \"c\""
        );
        assert_eq!(
            first_difference("a\n", "a\nb\n"),
            "line 2: naive \"<end>\", perceus \"b\""
        );
    }
}
