//! UI-test harness: runs every `.cheby` file under `tests/ui/` and checks
//! it against its header and goldens (D-275, docs/plan/testing.md §2).
//!
//! Each file is copied into a temporary package as `src/main.cheby`, so
//! the real `cheby check` and `cheby run` are exercised. Paths in the
//! output are rewritten to the test's own path, so goldens show
//! `tests/ui/…/name.cheby` and no absolute paths.
//!
//! - `check-pass`: `cheby check` exits with 0, stderr matches `.stderr`
//!   (empty if absent, so warnings need a golden).
//! - `check-fail`, `parse-fail`: `cheby check` exits with 1, the exit code
//!   of a compile error, and stderr matches `.stderr`, which is required.
//! - `run-pass`: `cheby run` exits with 0, stdout and stderr match `.stdout`
//!   and `.stderr` (empty if absent).
//! - `run-fail`: like `run-pass`, with the exit code of `@exit`, 101 (a
//!   panic) by default.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use cheby_test_support::milestone::{self, Expectation, Verdicts};
use cheby_test_support::ui::{self, Mode, UiTest};
use cheby_test_support::{bless_enabled, golden, process, repo_root};
use libtest_mimic::{Arguments, Trial};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
const TIMEOUT: Duration = Duration::from_secs(60);
/// Exit code of `cheby check` when the package has errors.
const COMPILE_ERROR_EXIT: i32 = 1;
/// Exit code of a program whose root fiber panics (D-182).
const PANIC_EXIT: i32 = 101;

fn main() -> ExitCode {
    let args = Arguments::from_args();
    let root = repo_root();
    let setup = milestone::current(&root).and_then(|m| Ok((m, ui::all(&root)?)));
    let (current, tests) = match setup {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("UI harness: {message}");
            return ExitCode::from(2);
        }
    };
    let verdicts = Verdicts::default();
    let bless = bless_enabled();

    let trials = tests
        .into_iter()
        .map(|test| {
            let expectation = Expectation::new(test.header.milestone, current);
            let verdicts = verdicts.clone();
            let name = test.name.clone();
            Trial::test(name.clone(), move || {
                let result = run_test(&test, bless && expectation != Expectation::Fail);
                verdicts.judge(&name, test.header.milestone, expectation, result)?;
                Ok(())
            })
            .with_kind(expectation.kind())
        })
        .collect();

    let conclusion = libtest_mimic::run(&args, trials);
    verdicts.report("UI tests");
    conclusion.exit_code()
}

fn run_test(test: &UiTest, bless: bool) -> Result<(), String> {
    let package = make_package(test)?;
    let mode = test.header.mode;
    let command = if mode.runs() { "run" } else { "check" };
    let mut cmd = Command::new(CHEBY);
    cmd.arg(command).current_dir(&package).env("NO_COLOR", "1");
    let out = process::run(cmd, None, TIMEOUT)?;
    let shown_path = format!("tests/ui/{}.cheby", test.name);
    let stdout = rewrite_paths(&out.stdout, &package, &shown_path);
    let stderr = rewrite_paths(&out.stderr, &package, &shown_path);

    let expected_exit = match mode {
        Mode::CheckPass | Mode::RunPass => 0,
        Mode::CheckFail | Mode::ParseFail => COMPILE_ERROR_EXIT,
        Mode::RunFail => test.header.exit.unwrap_or(PANIC_EXIT),
    };
    if out.status != Some(expected_exit) {
        return Err(format!(
            "`cheby {command}`: expected exit code {expected_exit}, got {}\nstderr:\n{stderr}",
            out.describe_status()
        ));
    }

    let stderr_golden = test.golden("stderr");
    let stdout_golden = test.golden("stdout");
    if bless {
        golden::write(&stderr_golden, &stderr, false)?;
        return golden::write(&stdout_golden, &stdout, false);
    }
    let expected_stderr = match golden::read(&stderr_golden)? {
        Some(golden) => golden,
        None if mode.fails_to_compile() => {
            return Err("missing .stderr golden; record it with `cargo xtask bless`".to_owned());
        }
        None => String::new(),
    };
    golden::compare("stderr", &expected_stderr, &stderr)?;
    let expected_stdout = golden::read(&stdout_golden)?.unwrap_or_default();
    golden::compare("stdout", &expected_stdout, &stdout)
}

/// Writes a one-module package holding the test file and returns its
/// directory.
fn make_package(test: &UiTest) -> Result<PathBuf, String> {
    let dir = Path::new(TMP).join("ui").join(test.name.replace('/', "__"));
    let src = dir.join("src");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::create_dir_all(&src).map_err(|e| format!("{}: {e}", src.display()))?;
    std::fs::write(
        dir.join("cheby.toml"),
        "name = \"ui_test\"\nversion = \"0.0.0\"\n",
    )
    .map_err(|e| e.to_string())?;
    std::fs::copy(&test.path, src.join("main.cheby"))
        .map_err(|e| format!("{}: {e}", test.path.display()))?;
    Ok(dir)
}

/// Replaces the temporary package's paths with the test's path.
fn rewrite_paths(text: &str, package: &Path, shown_path: &str) -> String {
    let package = package.to_string_lossy();
    text.replace(&format!("{package}/"), "")
        .replace(&format!("{package}\\"), "")
        .replace("src/main.cheby", shown_path)
        .replace("src\\main.cheby", shown_path)
}
