//! Conformance harness: runs every example in `docs/examples/` with the
//! `cheby` binary and compares its output and exit code with the goldens
//! next to it (D-275, docs/plan/testing.md §1).
//!
//! An example whose `test.toml` sets `skip-run` is only checked with
//! `cheby check` (D-282). Expected-fail tracking follows `tests/milestone`
//! (see `cheby_test_support::milestone`). With `CHEBY_BLESS=1`, set by
//! `cargo xtask bless`, goldens of examples at or before the milestone in
//! progress are rewritten instead of compared.

use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Duration;

use cheby_test_support::example::{self, Example};
use cheby_test_support::milestone::{self, Expectation, Verdicts};
use cheby_test_support::{bless_enabled, golden, process, repo_root};
use libtest_mimic::{Arguments, Trial};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    let args = Arguments::from_args();
    let root = repo_root();
    let setup = milestone::current(&root).and_then(|m| Ok((m, example::all(&root)?)));
    let (current, examples) = match setup {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("conformance harness: {message}");
            return ExitCode::from(2);
        }
    };
    let verdicts = Verdicts::default();
    let bless = bless_enabled();

    let trials = examples
        .into_iter()
        .map(|ex| {
            let expectation = Expectation::new(ex.config.milestone, current);
            let verdicts = verdicts.clone();
            let name = ex.name.clone();
            Trial::test(name.clone(), move || {
                let result = if bless && expectation != Expectation::Fail {
                    bless_example(&ex)
                } else {
                    check_example(&ex)
                };
                verdicts.judge(&name, ex.config.milestone, expectation, result)?;
                Ok(())
            })
            .with_kind(expectation.kind())
        })
        .collect();

    let conclusion = libtest_mimic::run(&args, trials);
    verdicts.report("examples");
    conclusion.exit_code()
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
    cmd.args(args).current_dir(dir).env("NO_COLOR", "1");
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    process::run(cmd, stdin, TIMEOUT)
}
