//! REPL session (§13.5).
//!
//! A placeholder until wave 4 of the M1 back end (plan §4.5): [`run`] is
//! wired to `cheby repl` and reports that the command is not implemented
//! yet. [`ReplOptions`] already carries the flags the CLI passes, so the
//! command line does not change when the session lands (plan §8.3, §8.4).

use std::io::Write as _;
use std::process::ExitCode;

/// How `cheby repl` runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplOptions {
    /// `--deny-warnings`: every warning is an error (plan §8.4).
    pub deny_warnings: bool,
}

/// Runs an interactive session and returns the process's exit status.
///
/// Not implemented yet: prints a tool error (D-442) and returns status 1.
#[must_use]
#[expect(
    clippy::needless_pass_by_value,
    reason = "plan §4.5 fixes the signature; the session (W4-B) consumes the options"
)]
pub fn run(options: ReplOptions) -> ExitCode {
    let _ = options;
    // A failed write to stderr cannot be reported anywhere else.
    let _ = std::io::stderr()
        .lock()
        .write_all(b"error: `cheby repl` is not implemented yet\n");
    ExitCode::FAILURE
}
