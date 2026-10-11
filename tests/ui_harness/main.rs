//! UI-test harness: runs every UI test under `tests/ui/` and checks it
//! against its header and goldens (D-275, docs/plan/testing.md §2).
//!
//! A single-file test is copied into a temporary package as
//! `src/main.cheby`, with a generated `cheby.toml`. A package test, a
//! directory with its own `cheby.toml`, is copied as it is. Either way the
//! real `cheby check` and `cheby run` are exercised. Paths in the output
//! are rewritten to the test's own files, so goldens show
//! `tests/ui/…/name.cheby` or `tests/ui/…/name/src/….cheby` and no
//! absolute paths.
//!
//! - `check-pass`: `cheby check` exits with 0, stderr matches `.stderr`
//!   (empty if absent, so warnings need a golden).
//! - `check-fail`, `parse-fail`: `cheby check` exits with 1, the exit code
//!   of a compile error, and stderr matches `.stderr`, which is required.
//! - `run-pass`: `cheby run` exits with 0, stdout and stderr match `.stdout`
//!   and `.stderr` (empty if absent).
//! - `run-fail`: like `run-pass`, with the exit code of `@exit`, 101 (a
//!   panic) by default.
//!
//! A run test's `<name>.stdin` is its standard input, which is null
//! without one, and `<name>.args` its arguments, one per line, passed
//! after `--` (D-543). With `// @command: test`, a run test runs
//! `cheby test` instead of `cheby run`, and its goldens hold that
//! command's output, and `// @flags: …` adds words after the subcommand
//! (D-543).
//!
//! Strict runs (D-544): `CHEBY_MILESTONE` overrides `tests/milestone`,
//! `CHEBY_PENDING=require` makes a failing pending test a failure unless
//! the file `CHEBY_PENDING_ALLOW` names lists it, and every `cheby` runs
//! with `CHEBY_RT_POISON=1` (see `cheby_test_support::milestone`).
//!
//! On Windows, the tests that run JIT-compiled code are ignored until M6
//! (D-545, `cheby_test_support::platform`): `run-pass` and `run-fail`
//! tests, and the check tests whose `.stderr` golden holds no front-end
//! error, since `cheby check` evaluates the package's constants with the
//! JIT once the front end reports no error. The other check tests still
//! run.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use cheby_test_support::milestone::{Expectation, POISON_ENV, Setting, Verdicts};
use cheby_test_support::ui::{self, Layout, Mode, UiTest};
use cheby_test_support::{bless_enabled, golden, platform, process, repo_root};
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
    let setup = Setting::read(&root).and_then(|m| Ok((m, ui::all(&root)?, Verdicts::from_env()?)));
    let (current, tests, verdicts) = match setup {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("UI harness: {message}");
            return ExitCode::from(2);
        }
    };
    let bless = bless_enabled();
    if let Some(reason) = platform::run_skip_reason() {
        eprintln!(
            "UI harness: run tests, and check tests without a front-end error, are ignored: {reason}"
        );
    }

    let trials = tests
        .into_iter()
        .map(|test| {
            let expectation = current.expectation(test.header.milestone);
            let verdicts = verdicts.clone();
            let name = test.name.clone();
            let skipped = platform::runs_skipped() && test.builds_a_jit();
            Trial::test(name.clone(), move || {
                let result = run_test(&test, bless && expectation != Expectation::Fail);
                verdicts.judge(&name, test.header.milestone, expectation, result)?;
                Ok(())
            })
            .with_kind(expectation.kind())
            .with_ignored_flag(skipped)
        })
        .collect();

    let conclusion = libtest_mimic::run(&args, trials);
    // Each test removes its own package; the empty directory goes last.
    let _ = std::fs::remove_dir(packages_dir());
    verdicts.report("UI tests");
    conclusion.exit_code()
}

fn run_test(test: &UiTest, bless: bool) -> Result<(), String> {
    let (package, files) = make_package(test)?;
    let result = check_package(test, bless, &package, &files);
    // Best effort: a leftover package only costs disk space, and the
    // directory is this process's own (see `packages_dir`).
    let _ = std::fs::remove_dir_all(&package);
    result
}

/// This process's directory for the temporary packages. It is unique per
/// process, so concurrent runs of the harness on one checkout never
/// delete each other's packages mid-run.
fn packages_dir() -> PathBuf {
    Path::new(TMP).join(format!("ui-{}", std::process::id()))
}

fn check_package(
    test: &UiTest,
    bless: bool,
    package: &Path,
    files: &[ShownFile],
) -> Result<(), String> {
    let mode = test.header.mode;
    let command = if mode.runs() {
        test.header.command.name()
    } else {
        "check"
    };
    let input = test.input()?;
    let mut cmd = Command::new(CHEBY);
    // `--color=never` and `NO_COLOR` keep goldens plain even if the
    // developer's environment forces color (`CLICOLOR_FORCE`). Freed
    // objects are poisoned, so a reference-count mistake stops the program
    // where it happens (D-544).
    cmd.args(["--color=never", command])
        .args(&test.header.flags)
        .current_dir(package)
        .env("NO_COLOR", "1")
        .env(POISON_ENV, "1");
    if !input.args.is_empty() {
        cmd.arg("--").args(&input.args);
    }
    let out = process::run(cmd, input.stdin.as_deref(), TIMEOUT)?;
    let stdout = rewrite_paths(&out.stdout, package, files);
    let stderr = rewrite_paths(&out.stderr, package, files);

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

/// A file of the temporary package, relative to it with `/` separators,
/// and the path that goldens show for it.
type ShownFile = (String, String);

/// Writes the test's package into a fresh temporary directory and returns
/// the directory, with the files whose paths are rewritten in the output.
fn make_package(test: &UiTest) -> Result<(PathBuf, Vec<ShownFile>), String> {
    let dir = packages_dir().join(test.name.replace('/', "__"));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let files = match test.layout {
        Layout::File => {
            let src = dir.join("src");
            std::fs::create_dir_all(&src).map_err(|e| format!("{}: {e}", src.display()))?;
            std::fs::write(
                dir.join("cheby.toml"),
                "name = \"ui_test\"\nversion = \"0.0.0\"\n",
            )
            .map_err(|e| e.to_string())?;
            std::fs::copy(&test.path, src.join("main.cheby"))
                .map_err(|e| format!("{}: {e}", test.path.display()))?;
            vec![(
                "src/main.cheby".to_owned(),
                format!("tests/ui/{}.cheby", test.name),
            )]
        }
        Layout::Package => {
            let mut copied = Vec::new();
            copy_dir(&test.path, &dir, "", &mut copied)?;
            copied
                .into_iter()
                .map(|relative| {
                    let shown = format!("tests/ui/{}/{relative}", test.name);
                    (relative, shown)
                })
                .collect()
        }
    };
    Ok((dir, files))
}

/// Copies the directory `from` to `to`, skipping hidden entries, and
/// appends the copied files' paths below `prefix` to `copied`.
fn copy_dir(from: &Path, to: &Path, prefix: &str, copied: &mut Vec<String>) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let entries = std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", from.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let relative = format!("{prefix}{name}");
        let (source, target) = (entry.path(), to.join(&name));
        let is_dir = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", source.display()))?
            .is_dir();
        if is_dir {
            copy_dir(&source, &target, &format!("{relative}/"), copied)?;
        } else {
            std::fs::copy(&source, &target).map_err(|e| format!("{}: {e}", source.display()))?;
            copied.push(relative);
        }
    }
    Ok(())
}

/// Removes the temporary package's directory from paths in `text`, then
/// replaces each of the package's files, written relative to it with `/`
/// or `\`, by the path goldens show for it. A file's path is replaced
/// only where a path starts, so `src/a.cheby` inside `lib/src/a.cheby` is
/// left alone, and the replacement is never rewritten again. A path quoted
/// in backticks is message text, such as "unknown key `colour` in
/// `cheby.toml`", and is left alone too.
fn rewrite_paths(text: &str, package: &Path, files: &[ShownFile]) -> String {
    let package = package.to_string_lossy();
    let text = text
        .replace(&format!("{package}/"), "")
        .replace(&format!("{package}\\"), "");
    let mut patterns: Vec<(String, &str)> = files
        .iter()
        .flat_map(|(relative, shown)| {
            [
                (relative.clone(), shown.as_str()),
                (relative.replace('/', "\\"), shown.as_str()),
            ]
        })
        .collect();
    // Longest first, so `src/a/b.cheby` wins over a shorter file that is
    // a prefix of it.
    patterns.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    let mut previous: Option<char> = None;
    while let Some(c) = rest.chars().next() {
        let at_start = previous.is_none_or(|p| !is_path_char(p));
        let matched = at_start
            .then(|| {
                patterns.iter().find(|(relative, _)| {
                    rest.strip_prefix(relative.as_str())
                        .is_some_and(|after| !(previous == Some('`') && after.starts_with('`')))
                })
            })
            .flatten();
        if let Some((relative, shown)) = matched {
            out.push_str(shown);
            previous = relative.chars().next_back();
            rest = rest.get(relative.len()..).unwrap_or_default();
        } else {
            out.push(c);
            previous = Some(c);
            rest = rest.get(c.len_utf8()..).unwrap_or_default();
        }
    }
    out
}

/// Whether `c` can be part of a relative path, so a file's path cannot
/// start right after it.
fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '\\')
}
