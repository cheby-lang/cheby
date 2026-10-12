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
//! macOS. So is every `skip-run` example's trial, which only runs `cheby
//! check`: `cheby check` evaluates the package's constants with the JIT
//! once the front end reports no error, and 020 passes the front end. 030
//! and 031 stop in the front end and build no JIT, but are ignored with
//! it: their result is report-only before their milestone anyway.
//!
//! Thread variants (D-722, `cheby_test_support::threads`): the run trial
//! and the `::test` trial of every example that is run also run as
//! `<name>@threads=1`, `<name>@threads=2`, `<name>::test@threads=1` and
//! `<name>::test@threads=2`, with `CHEBY_THREADS` set after the example's
//! `env` file and after `CHEBY_RT_POISON`. The base trials remove
//! `CHEBY_THREADS` from the child's environment. A `CHEBY_THREADS=N` in
//! the harness's own environment adds the variants `<name>@threads=N`
//! and `<name>::test@threads=N` when N is neither 1 nor 2, and a value
//! that is not a whole number of at least 1 stops the harness with status
//! 2; the harness prints the thread counts it runs. A variant has its base
//! trial's expectation and goldens; blessing writes goldens from the base
//! trial only, so the variants are ignored when `CHEBY_BLESS=1`. A
//! `skip-run` example gets no variants. Unless `--test-threads` is given,
//! the harness runs at most half as many trials at once as the machine
//! has hardware threads, at least one, since every trial may start
//! several scheduler threads. `CHEBY_HARNESS_SCALE` multiplies the
//! timeout (`cheby_test_support::process`).

use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Duration;

use cheby_test_support::example::{self, Example};
use cheby_test_support::milestone::{Expectation, Setting, Verdicts};
use cheby_test_support::threads::{self, Variant};
use cheby_test_support::{bless_enabled, golden, platform, process, repo_root};
use libtest_mimic::{Arguments, Trial};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
/// Before `CHEBY_HARNESS_SCALE` (D-722).
const TIMEOUT: Duration = Duration::from_secs(60);

/// How one trial starts `cheby`: the thread variant and the timeout.
#[derive(Debug, Clone, Copy)]
struct Run {
    variant: Variant,
    timeout: Duration,
}

fn main() -> ExitCode {
    let mut args = Arguments::from_args();
    let root = repo_root();
    let setup = Setting::read(&root).and_then(|m| {
        Ok((
            m,
            example::all(&root)?,
            Verdicts::from_env()?,
            process::scaled_timeout(TIMEOUT)?,
            threads::variants()?,
        ))
    });
    let (current, examples, verdicts, timeout, variants) = match setup {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("conformance harness: {message}");
            return ExitCode::from(2);
        }
    };
    let bless = bless_enabled();
    // So that a thread count the harness does not run cannot pass
    // unnoticed (D-722).
    eprintln!(
        "conformance harness: CHEBY_THREADS of the run trials: {}",
        threads::describe(&variants)
    );
    if let Some(reason) = platform::run_skip_reason() {
        eprintln!("conformance harness: runs and checks are ignored: {reason}");
    }
    // D-722: the trials' scheduler threads compete for the machine.
    args.test_threads = Some(threads::capped_test_threads(
        args.test_threads,
        std::thread::available_parallelism().ok().map(Into::into),
    ));

    let trials = examples
        .into_iter()
        .flat_map(|ex| {
            // A `skip-run` example is only checked, and its `::test` trial
            // stays pending, so it runs once, as the base trial.
            let variants: &[Variant] = if ex.config.skip_run {
                &[Variant::Unset]
            } else {
                &variants
            };
            variants
                .iter()
                .flat_map(|&variant| {
                    let run = Run { variant, timeout };
                    // Variants share the base trial's goldens, which only
                    // the base trial blesses.
                    let ignored = bless && !variant.is_base();
                    [
                        run_trial(ex.clone(), current, verdicts.clone(), bless, run),
                        test_trial(ex.clone(), current, verdicts.clone(), run),
                    ]
                    .map(|trial| {
                        if ignored {
                            trial.with_ignored_flag(true)
                        } else {
                            trial
                        }
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect();

    let conclusion = libtest_mimic::run(&args, trials);
    verdicts.report("examples");
    conclusion.exit_code()
}

/// The trial that runs the example and compares its output with the
/// goldens, or blesses them; or only checks a `skip-run` example.
fn run_trial(ex: Example, current: Setting, verdicts: Verdicts, bless: bool, run: Run) -> Trial {
    let expectation = check_only_expectation(&ex, current);
    let name = run.variant.trial_name(&ex.name);
    Trial::test(name.clone(), move || {
        let result = if bless && expectation != Expectation::Fail {
            bless_example(&ex, run)
        } else {
            check_example(&ex, run)
        };
        verdicts.judge(&name, ex.config.milestone, expectation, result)?;
        Ok(())
    })
    .with_kind(expectation.kind())
    // D-545: a run is skipped where programs do not run, and so is a
    // `skip-run` example's check, which builds a JIT for its constants.
    .with_ignored_flag(platform::runs_skipped())
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
fn test_trial(ex: Example, current: Setting, verdicts: Verdicts, run: Run) -> Trial {
    let expectation = test_expectation(&ex, current);
    let name = run.variant.trial_name(&format!("{}::test", ex.name));
    Trial::test(name.clone(), move || {
        let result = cheby(&ex.dir, &["test"], &[], None, run).and_then(|out| match out.status {
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
fn check_example(ex: &Example, run: Run) -> Result<(), String> {
    if ex.config.skip_run {
        let out = cheby(&ex.dir, &["check"], &[], None, run)?;
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
    let out = run_example(ex, &goldens, run)?;
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
fn bless_example(ex: &Example, run: Run) -> Result<(), String> {
    if ex.config.skip_run {
        return check_example(ex, run);
    }
    let goldens = ex.goldens()?;
    let out = run_example(ex, &goldens, run)?;
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

fn run_example(
    ex: &Example,
    goldens: &example::Goldens,
    run: Run,
) -> Result<process::RunOutput, String> {
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
        run,
    )
}

fn cheby(
    dir: &Path,
    args: &[&str],
    env: &[(String, String)],
    stdin: Option<&[u8]>,
    run: Run,
) -> Result<process::RunOutput, String> {
    let mut cmd = Command::new(CHEBY);
    // `--color=never` keeps goldens plain even if the environment forces
    // color (`CLICOLOR_FORCE`), and the example's own `env` cannot undo it.
    // After the example's `env`, freed objects are poisoned, so a
    // reference-count mistake stops the program where it happens (D-544),
    // and the variant sets or removes `CHEBY_THREADS` (D-722); the
    // example's `env` cannot undo either.
    cmd.arg("--color=never").args(args).current_dir(dir);
    threads::apply(&mut cmd, &threads::child_env(env, run.variant));
    process::run(cmd, stdin, run.timeout)
}
