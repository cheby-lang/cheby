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
//!
//! The run the three agreed on is then compared with the reference
//! evaluator's [`Expected`] (D-719, D-1028), the third oracle, which
//! computes the program's output from the spec in process: standard
//! output and the exit status must be equal, and standard error too
//! wherever the evaluator predicts it exactly ([`Expected::stderr`],
//! GAP(progen-eval-compared-output)). A difference is
//! [`DiffErrorKind::Oracle`]; a program outside the evaluator's subset is
//! [`DiffErrorKind::NotEvaluated`], never a pass.
//!
//! Every `cheby` the differential starts gets `CHEBY_CACHE` set to the
//! process's own fresh cache ([`shared_cache`], a
//! [`HarnessCache`](crate::cache::HarnessCache)), so no run reads or fills
//! the developer's cache (D-959, GAP(cache-tests-isolated)).

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use super::Program;
use super::eval::{EvalError, Expected, evaluate};
use crate::cache::{CACHE_ENV, HarnessCache};
use crate::process::{self, RunOutput};

/// The process's cache for the `cheby` commands of the differential: one
/// fresh [`HarnessCache`] shared by every holder, created when the first
/// holder asks and deleted when the last one drops it. Callers that run
/// several differentials hold it for their whole run, so that their runs
/// share one cache.
///
/// # Errors
///
/// As [`HarnessCache::create`].
pub fn shared_cache() -> Result<Arc<HarnessCache>, String> {
    static SHARED: Mutex<Weak<HarnessCache>> = Mutex::new(Weak::new());
    let mut slot = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(cache) = slot.upgrade() {
        return Ok(cache);
    }
    let cache = Arc::new(HarnessCache::create("progen")?);
    *slot = Arc::downgrade(&cache);
    Ok(cache)
}

/// The package in `dir` as a [`Program`]: its `cheby.toml` and its
/// modules under `src/`, for the evaluator.
///
/// # Errors
///
/// The first error from reading the directory or a file.
pub fn program_in(dir: &Path) -> std::io::Result<Program> {
    let mut files = vec![(
        "cheby.toml".to_owned(),
        std::fs::read_to_string(dir.join("cheby.toml"))?,
    )];
    let mut sources = Vec::new();
    for entry in std::fs::read_dir(dir.join("src"))? {
        let path = entry?.path();
        if path.is_file() && path.extension().is_some_and(|e| e == "cheby") {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            sources.push((format!("src/{name}"), std::fs::read_to_string(&path)?));
        }
    }
    sources.sort();
    files.extend(sources);
    Ok(Program { seed: 0, files })
}

/// How [`differential_with`] runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffOptions {
    /// The limit of each `cheby` command.
    pub timeout: Duration,
    /// Where the Perceus run writes its dumps (`CHEBY_DUMP_DIR`), if
    /// anywhere.
    pub dump_dir: Option<PathBuf>,
    /// The `CHEBY_CACHE` of every `cheby` the differential starts, a
    /// directory the caller keeps fresh and owns, such as the path of the
    /// [`shared_cache`] it holds. `None` holds [`shared_cache`] for the
    /// call.
    pub cache: Option<PathBuf>,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            dump_dir: None,
            cache: None,
        }
    }
}

/// The run every mode agreed on, and what the evaluator expected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffReport {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    /// Whether the program has a planted final panic.
    pub planted: bool,
    /// The evaluator's output, which the runs matched; its `stderr` is
    /// `Some` when standard error was compared too.
    pub expected: Expected,
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
    /// The runs agree with each other but not with the reference
    /// evaluator (D-1028): the message names the first differing line.
    Oracle,
    /// The reference evaluator gives no output for the program: it is
    /// outside the evaluator's subset, runs past its budget, or is not a
    /// well-formed package. The fuzz target discards such inputs, as it
    /// does those that do not check.
    NotEvaluated,
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

fn command(cheby: &Path, dir: &Path, verb: &str, cache: &Path) -> Command {
    let mut cmd = Command::new(cheby);
    cmd.arg("--color=never")
        .arg(verb)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, cache);
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

fn not_evaluated(e: &EvalError) -> DiffError {
    error(
        DiffErrorKind::NotEvaluated,
        format!("the reference evaluator gives no output: {e}"),
    )
}

/// `Err(Oracle)` naming the first difference between the run the modes
/// agreed on and the evaluator's [`Expected`]: the status, then standard
/// output, then standard error where the evaluator predicts it
/// (GAP(progen-eval-compared-output)).
fn against_oracle(
    status: i32,
    stdout: &str,
    stderr: &str,
    expected: &Expected,
) -> Result<(), DiffError> {
    if status != expected.status {
        let at = if stdout == expected.stdout {
            "standard output is equal".to_owned()
        } else {
            format!(
                "standard output differs at {}",
                first_difference("cheby", stdout, "evaluator", &expected.stdout)
            )
        };
        return Err(error(
            DiffErrorKind::Oracle,
            format!(
                "the status differs: cheby {status}, evaluator {}; {at}\ncheby stderr:\n{stderr}",
                expected.status
            ),
        ));
    }
    if stdout != expected.stdout {
        return Err(error(
            DiffErrorKind::Oracle,
            format!(
                "standard output differs from the evaluator's at {}",
                first_difference("cheby", stdout, "evaluator", &expected.stdout)
            ),
        ));
    }
    if let Some(want) = &expected.stderr
        && stderr != want
    {
        return Err(error(
            DiffErrorKind::Oracle,
            format!(
                "standard error differs from the evaluator's at {}",
                first_difference("cheby", stderr, "evaluator", want)
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
    // Held until the last `cheby` of this call has ended.
    let held = match &options.cache {
        Some(_) => None,
        None => Some(shared_cache().map_err(|e| error(DiffErrorKind::Io, e))?),
    };
    let cache = match (&options.cache, &held) {
        (Some(path), _) => path.clone(),
        (None, Some(shared)) => shared.path().to_path_buf(),
        (None, None) => return Err(error(DiffErrorKind::Io, "no cache directory")),
    };
    let cheby_cmd = |verb: &str| command(cheby, dir, verb, &cache);

    let check = run(cheby_cmd("check"), options.timeout, "`cheby check`")?;
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

    let mut naive = cheby_cmd("run");
    naive.env("CHEBY_RC", "naive").env("CHEBY_RT_POISON", "1");
    let naive = run(naive, options.timeout, "the naive run")?;

    let mut perceus = cheby_cmd("run");
    perceus.env("CHEBY_RT_POISON", "1");
    if let Some(dump) = &options.dump_dir {
        perceus.env("CHEBY_DUMP_DIR", dump);
    }
    let perceus = run(perceus, options.timeout, "the Perceus run")?;

    // Poisoning sends every count and reuse operation through the
    // runtime's checked entries, so only this run executes the inline
    // paths (D-844, refining D-809).
    let mut poison_off = cheby_cmd("run");
    poison_off.env("CHEBY_RT_POISON", "0");
    let poison_off = run(
        poison_off,
        options.timeout,
        "the Perceus run with poisoning off",
    )?;

    compare("naive", &naive, "perceus", &perceus)?;
    compare("perceus", &perceus, "perceus-poison-off", &poison_off)?;
    let expected_status = if planted { 101 } else { 0 };
    match perceus.status {
        Some(status) if status == expected_status => {
            // The third oracle (D-1028), on the run the others agreed on.
            let program = program_in(dir).map_err(|e| {
                error(
                    DiffErrorKind::Io,
                    format!("cannot read the package for the evaluator: {e}"),
                )
            })?;
            let expected = evaluate(&program).map_err(|e| not_evaluated(&e))?;
            against_oracle(status, &perceus.stdout, &perceus.stderr, &expected)?;
            Ok(DiffReport {
                status,
                stdout: perceus.stdout,
                stderr: perceus.stderr,
                planted,
                expected,
            })
        }
        Some(102) => Err(error(
            DiffErrorKind::Leak,
            format!("every run exited 102 (a leak):\n{}", perceus.stderr),
        )),
        _ => Err(error(
            DiffErrorKind::BadStatus,
            format!(
                "every run ended with {}, expected exit code {expected_status}:\n{}",
                perceus.describe_status(),
                perceus.stderr
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{DiffErrorKind, Expected, against_oracle, first_difference, shared_cache};

    #[test]
    fn progen_eval_oracle_names_the_first_difference() {
        let expected = Expected {
            stdout: "a\nb\n".to_owned(),
            status: 0,
            stderr: Some(String::new()),
        };
        assert!(against_oracle(0, "a\nb\n", "", &expected).is_ok());
        let e = against_oracle(0, "a\nc\n", "", &expected).unwrap_err();
        assert_eq!(e.kind, DiffErrorKind::Oracle);
        assert!(
            e.message.contains("line 2: cheby \"c\", evaluator \"b\""),
            "{e}"
        );
        let e = against_oracle(101, "a\nb\n", "x", &expected).unwrap_err();
        assert!(e.message.contains("cheby 101, evaluator 0"), "{e}");
        let e = against_oracle(0, "a\nb\n", "warning\n", &expected).unwrap_err();
        assert!(e.message.contains("standard error"), "{e}");
        // Standard error is not compared where the evaluator does not
        // predict it.
        let open = Expected {
            stderr: None,
            ..expected
        };
        assert!(against_oracle(0, "a\nb\n", "anything", &open).is_ok());
    }

    /// One fresh cache while any holder lives, and a new one after.
    #[test]
    fn progen_shared_cache_is_fresh_per_holder_set() {
        // A cache kept on purpose (K3a's `CHEBY_TEST_KEEP_CACHE`) is
        // neither fresh nor deleted.
        if std::env::var_os("CHEBY_TEST_KEEP_CACHE").is_some_and(|v| !v.is_empty()) {
            return;
        }
        let a = shared_cache().unwrap();
        let b = shared_cache().unwrap();
        assert_eq!(a.path(), b.path());
        let path = a.path().to_path_buf();
        assert!(path.is_dir());
        drop((a, b));
        assert!(!path.exists());
        let c = shared_cache().unwrap();
        assert_ne!(c.path(), path.as_path());
    }

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
