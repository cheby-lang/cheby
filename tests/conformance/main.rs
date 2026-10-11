//! Conformance harness: runs every example in `docs/examples/` with the
//! `cheby` binary and compares its output and exit code with the goldens
//! next to it (D-275, docs/plan/testing.md §1).
//!
//! An example whose `test.toml` sets `skip-run` is only checked with
//! `cheby check` (D-282). Its result is pending, never an unexpected
//! pass, until its milestone (see [`check_only_expectation`]).
//! Expected-fail tracking follows `tests/milestone` (see
//! `cheby_test_support::milestone`). With `CHEBY_BLESS=1`, set by
//! `cargo xtask bless`, goldens of examples at or before the milestone in
//! progress are rewritten instead of compared, unless the exit code
//! changes (see `bless_example`).
//!
//! Each example also gets a second trial, `<name>::test`, that runs
//! `cheby test` in its directory and requires exit code 0. It has no
//! goldens, so blessing never writes anything for it. It follows the
//! example's milestone like the run, except that a failure before the
//! milestone is pending, never required, and that a `skip-run` example's
//! trial stays pending until the harness can give `cheby test` the
//! example's fixture (see [`test_expectation`]).
//!
//! Strict runs (D-544): `CHEBY_MILESTONE` overrides `tests/milestone`,
//! `CHEBY_PENDING=require` makes a failing pending trial a failure unless
//! the file `CHEBY_PENDING_ALLOW` names lists it, and every `cheby` runs
//! with `CHEBY_RT_POISON=1` (see `cheby_test_support::milestone`). The
//! trials that D-445 and D-472 keep pending at every milestone are
//! [`Expectation::Tolerated`] and stay report-only, and so are the trials
//! of a milestone later than `tests/milestone` that the override made
//! pending (`Setting::expectation`).
//!
//! On Windows the run trials and the `::test` trials are ignored until M6
//! (D-545, `cheby_test_support::platform`): M1 is judged on Linux and
//! macOS. A `skip-run` example's trial only runs `cheby check`, which
//! D-545 does not name, so it stays; its result is report-only before its
//! milestone anyway.

use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Duration;

use cheby_test_support::example::{self, Example};
use cheby_test_support::milestone::{Expectation, POISON_ENV, Setting, Verdicts};
use cheby_test_support::{bless_enabled, golden, platform, process, repo_root};
use libtest_mimic::{Arguments, Trial};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    let args = Arguments::from_args();
    let root = repo_root();
    let setup =
        Setting::read(&root).and_then(|m| Ok((m, example::all(&root)?, Verdicts::from_env()?)));
    let (current, examples, verdicts) = match setup {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("conformance harness: {message}");
            return ExitCode::from(2);
        }
    };
    let bless = bless_enabled();
    if let Some(reason) = platform::run_skip_reason() {
        eprintln!("conformance harness: runs are ignored: {reason}");
    }

    let trials = examples
        .into_iter()
        .flat_map(|ex| {
            let expectation = check_only_expectation(&ex, current);
            let verdicts = verdicts.clone();
            let name = ex.name.clone();
            // D-545: a run is skipped where programs do not run; a
            // `skip-run` example's trial only checks.
            let skipped = platform::runs_skipped() && !ex.config.skip_run;
            let test_trial = test_trial(ex.clone(), current, verdicts.clone());
            let run_trial = Trial::test(name.clone(), move || {
                let result = if bless && expectation != Expectation::Fail {
                    bless_example(&ex)
                } else {
                    check_example(&ex)
                };
                verdicts.judge(&name, ex.config.milestone, expectation, result)?;
                Ok(())
            })
            .with_kind(expectation.kind())
            .with_ignored_flag(skipped);
            [run_trial, test_trial]
        })
        .collect();

    let conclusion = libtest_mimic::run(&args, trials);
    verdicts.report("examples");
    conclusion.exit_code()
}

/// What the harness expects of `ex`. A `skip-run` example is only
/// checked, and its milestone names when its fixture arrives, not when
/// `cheby check` can handle it. So `cheby check` passing on it before that
/// milestone is reported as pending, not as an unexpected pass (D-445).
/// An example with dependencies, such as 030, has milestone M5, when
/// dependencies resolve (D-446). That check is tolerated: a strict run
/// does not require it before the milestone.
fn check_only_expectation(ex: &Example, current: Setting) -> Expectation {
    match current.expectation(ex.config.milestone) {
        Expectation::Fail if ex.config.skip_run => Expectation::Tolerated,
        other => other,
    }
}

/// What the harness expects of the example's `test` blocks. An example
/// whose `test` blocks pass before its milestone is not an error: its
/// tests may need nothing of that milestone, or it may have none. So
/// before the milestone a result is pending, as for a `skip-run` example
/// (D-445). At the milestone it is pending too, and after it `cheby test`
/// must pass (D-288).
///
/// A `skip-run` example's tests need its fixture as much as its run does:
/// 020's tests call the C library's symbols, which must resolve when the
/// program loads (D-455), and 030's modules import a git dependency that
/// only the fixture repository serves. The harness has no way yet to give
/// `cheby test` a fixture (OQ-P9), so that trial is always pending.
///
/// Both kinds of "pending before the milestone" are tolerated: a strict run
/// (D-544) requires the trial only at its milestone and after it.
fn test_expectation(ex: &Example, current: Setting) -> Expectation {
    if ex.config.skip_run {
        return Expectation::Tolerated;
    }
    match current.expectation(ex.config.milestone) {
        Expectation::Fail => Expectation::Tolerated,
        other => other,
    }
}

/// The trial that runs `cheby test` in the example's directory, with no
/// fixture (see [`test_expectation`]).
fn test_trial(ex: Example, current: Setting, verdicts: Verdicts) -> Trial {
    let expectation = test_expectation(&ex, current);
    let name = format!("{}::test", ex.name);
    Trial::test(name.clone(), move || {
        let result = cheby(&ex.dir, &["test"], &[], None).and_then(|out| match out.status {
            Some(0) => Ok(()),
            _ => Err(format!(
                "`cheby test` failed ({})\nstdout:\n{}\nstderr:\n{}",
                out.describe_status(),
                out.stdout,
                out.stderr
            )),
        });
        verdicts.judge(&name, ex.config.milestone, expectation, result)?;
        Ok(())
    })
    .with_kind(expectation.kind())
    .with_ignored_flag(platform::runs_skipped())
}

/// Runs the example and compares the result with its goldens.
fn check_example(ex: &Example) -> Result<(), String> {
    if ex.config.skip_run {
        let out = cheby(&ex.dir, &["check"], &[], None)?;
        return match out.status {
            Some(0) => Ok(()),
            _ => Err(format!(
                "`cheby check` failed ({})\n{}",
                out.describe_status(),
                out.stderr
            )),
        };
    }
    let goldens = ex.goldens()?;
    let expected_stdout = goldens
        .stdout
        .as_deref()
        .ok_or("missing expected.stdout; record it with `cargo xtask bless`")?;
    let out = run_example(ex, &goldens)?;
    let expected_exit = goldens.exit.unwrap_or(0);
    if out.status != Some(expected_exit) {
        return Err(format!(
            "expected exit code {expected_exit}, got {}\nstderr:\n{}",
            out.describe_status(),
            out.stderr
        ));
    }
    golden::compare("stdout", expected_stdout, &out.stdout)?;
    golden::compare(
        "stderr",
        goldens.stderr.as_deref().unwrap_or(""),
        &out.stderr,
    )
}

/// Runs the example and records its output as the new goldens.
fn bless_example(ex: &Example) -> Result<(), String> {
    if ex.config.skip_run {
        return check_example(ex);
    }
    let goldens = ex.goldens()?;
    let out = run_example(ex, &goldens)?;
    let exit = out
        .status
        .ok_or_else(|| format!("cannot bless: the example {}", out.describe_status()))?;
    // An exit code that changes is almost always a toolchain failure, such
    // as a command that is not implemented yet, not a new expected result.
    // Blessing it would replace good goldens with an error message (D-445).
    let expected_exit = goldens.exit.unwrap_or(0);
    if exit != expected_exit {
        return Err(format!(
            "cannot bless: expected exit code {expected_exit}, got {exit}; \
             edit expected.exit by hand if the change is intended\nstderr:\n{}",
            out.stderr
        ));
    }
    golden::write(&ex.dir.join("expected.stdout"), &out.stdout, true)?;
    golden::write(&ex.dir.join("expected.stderr"), &out.stderr, false)?;
    let exit_text = if exit == 0 {
        String::new()
    } else {
        format!("{exit}\n")
    };
    golden::write(&ex.dir.join("expected.exit"), &exit_text, false)
}

fn run_example(ex: &Example, goldens: &example::Goldens) -> Result<process::RunOutput, String> {
    // Program arguments follow `--`, so they are never taken for a module
    // path or a flag of `cheby run`.
    let mut args = vec!["run"];
    if !goldens.args.is_empty() {
        args.push("--");
        args.extend(goldens.args.iter().map(String::as_str));
    }
    cheby(
        &ex.dir,
        &args,
        &goldens.env,
        goldens.stdin.as_deref().map(str::as_bytes),
    )
}

fn cheby(
    dir: &Path,
    args: &[&str],
    env: &[(String, String)],
    stdin: Option<&[u8]>,
) -> Result<process::RunOutput, String> {
    let mut cmd = Command::new(CHEBY);
    // `--color=never` keeps goldens plain even if the environment forces
    // color (`CLICOLOR_FORCE`), and the example's own `env` cannot undo it.
    // Freed objects are poisoned, so a reference-count mistake stops the
    // program where it happens (D-544); the example's `env` cannot undo that
    // either.
    cmd.arg("--color=never")
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1");
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    cmd.env(POISON_ENV, "1");
    process::run(cmd, stdin, TIMEOUT)
}
