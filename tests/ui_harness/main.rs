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
//! With `// @command: build` (D-1026), a run test builds the copied
//! package with `cheby --color=never build -o <tmp>/<name>` and its
//! `@flags`, into a fresh temporary directory removed at the end, never
//! into the copy's `build/native/`. If the build succeeds, the harness runs
//! the executable in the package's directory with the test's `.stdin` and
//! `.args` (no `--`). The goldens are a run test's, standard error as the
//! build's followed by the executable's; a failed build is the result
//! itself, so a test can pin what `cheby build` reports (E0053 at build
//! time, `13-tooling/build/missing_symbol`, D-943). Such a test is never
//! blessed: `CHEBY_BLESS=1` ignores it, and its goldens are written by
//! hand. It is ignored on Windows until M6 (D-935). `cheby build` needs
//! the runtime archive next to `cheby` (`cargo build -p cheby_rt_exe`,
//! which `cargo test` does not run); a missing or stale archive fails the
//! test with the build's tool error and never skips it (D-929, D-930).
//!
//! Every child, `cheby` or an executable, gets `CHEBY_CACHE` set to one
//! fresh owner-only directory in the system's temporary directory that
//! the harness creates when it starts and deletes when the run ends, also
//! after failing tests, overriding the developer's own value (D-959,
//! `cheby_test_support::cache`). With `CHEBY_TEST_KEEP_CACHE=<absolute
//! directory>` the harness uses that directory instead, creating it
//! owner-only if needed, and never deletes it, so two runs share one cache
//! (GAP(harness-kept-cache)).
//!
//! Every `cheby` inherits the harness's `CHEBY_RC`, so
//! `CHEBY_RC=naive cargo test --test ui` runs the suite under naive
//! counting (D-718), except a test whose flags include `--reuse-report`:
//! its report describes Perceus, so it runs with `CHEBY_RC` removed
//! (D-853).
//!
//! Strict runs (D-544): `CHEBY_MILESTONE` overrides `tests/milestone`,
//! `CHEBY_PENDING=require` makes a failing pending test a failure unless
//! the file `CHEBY_PENDING_ALLOW` names lists it, and every `cheby` runs
//! with `CHEBY_RT_POISON=1` (see `cheby_test_support::milestone`).
//! `CHEBY_HARNESS_SCALE` multiplies the timeout (D-722,
//! `cheby_test_support::process`).
//!
//! On Windows, the tests that run JIT-compiled code are ignored until M6
//! (D-545, `cheby_test_support::platform`): `run-pass` and `run-fail`
//! tests, and the check tests whose `.stderr` golden holds no front-end
//! error, since `cheby check` evaluates the package's constants with the
//! JIT once the front end reports no error. The other check tests still
//! run.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::time::Duration;

use cheby_test_support::cache::{FreshDir, HarnessCache};
use cheby_test_support::milestone::{Expectation, POISON_ENV, Setting, Verdicts};
use cheby_test_support::threads::RC_ENV;
use cheby_test_support::ui::{self, Layout, Mode, RunCommand, RunInput, UiTest};
use cheby_test_support::{aot, bless_enabled, golden, platform, process, repo_root};
use libtest_mimic::{Arguments, Trial};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
/// Generous, because `03-types/equality/deep_nested_maps` peaks at 2.3 GB
/// and, run beside the other tests, took over 60 s on CI's Intel macOS
/// runner. A test that hangs still fails, just later. `CHEBY_HARNESS_SCALE`
/// multiplies it (D-722).
const TIMEOUT: Duration = Duration::from_secs(180);
/// Exit code of `cheby check` when the package has errors.
const COMPILE_ERROR_EXIT: i32 = 1;
/// Exit code of a program whose root fiber panics (D-182).
const PANIC_EXIT: i32 = 101;
/// The flag whose tests always run under Perceus counting (D-717).
const REUSE_REPORT_FLAG: &str = "--reuse-report";

fn main() -> ExitCode {
    let args = Arguments::from_args();
    let root = repo_root();
    let setup = Setting::read(&root).and_then(|m| {
        Ok((
            m,
            ui::all(&root)?,
            Verdicts::from_env()?,
            process::scaled_timeout(TIMEOUT)?,
            // D-959: one fresh cache for every child of this process,
            // deleted when `main` and the last test that holds it are done,
            // whatever the tests' results.
            Arc::new(HarnessCache::create("ui")?),
        ))
    });
    let (current, tests, verdicts, timeout, cache) = match setup {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("UI harness: {message}");
            return ExitCode::from(2);
        }
    };
    let bless = bless_enabled();
    eprintln!("UI harness: {}", cache.describe());
    if let Some(reason) = platform::run_skip_reason() {
        eprintln!(
            "UI harness: run tests, and check tests without a front-end error, are ignored: {reason}"
        );
    }
    if let Some(reason) = platform::executable_skip_reason() {
        eprintln!("UI harness: tests with `// @command: build` are ignored: {reason}");
    }

    let trials = tests
        .into_iter()
        .map(|test| {
            let expectation = current.expectation(test.header.milestone);
            let verdicts = verdicts.clone();
            let cache = Arc::clone(&cache);
            let name = test.name.clone();
            // GAP(ui-build-bless): a build test is never blessed, so `cargo
            // xtask bless` ignores it, as it ignores the `@aot` trials
            // (D-853, D-1025), and its goldens are written by hand.
            let skipped = (platform::runs_skipped() && test.builds_a_jit())
                || (test.builds_an_executable() && (bless || platform::executables_skipped()));
            Trial::test(name.clone(), move || {
                let bless = bless && expectation != Expectation::Fail;
                let result = run_test(&test, bless, &cache, timeout);
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

fn run_test(
    test: &UiTest,
    bless: bool,
    cache: &HarnessCache,
    timeout: Duration,
) -> Result<(), String> {
    let (package, files) = make_package(test)?;
    let result = check_package(test, bless, &package, &files, cache, timeout);
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
    cache: &HarnessCache,
    timeout: Duration,
) -> Result<(), String> {
    let mode = test.header.mode;
    let input = test.input()?;
    let (what, out) = if mode.runs() && test.header.command == RunCommand::Build {
        (
            "`cheby build` and the executable".to_owned(),
            build_and_run(test, package, &input, cache, timeout)?,
        )
    } else {
        let command = if mode.runs() {
            test.header.command.name()
        } else {
            "check"
        };
        let mut cmd = Command::new(CHEBY);
        // `--color=never` keeps goldens plain even if the developer's
        // environment forces color (`CLICOLOR_FORCE`).
        cmd.args(["--color=never", command])
            .args(&test.header.flags)
            .current_dir(package);
        child_env(&mut cmd, test, cache);
        if !input.args.is_empty() {
            cmd.arg("--").args(&input.args);
        }
        (
            format!("`cheby {command}`"),
            process::run(cmd, input.stdin.as_deref(), timeout)?,
        )
    };
    let stdout = rewrite_paths(&out.stdout, package, files);
    let stderr = rewrite_paths(&out.stderr, package, files);

    let expected_exit = match mode {
        Mode::CheckPass | Mode::RunPass => 0,
        Mode::CheckFail | Mode::ParseFail => COMPILE_ERROR_EXIT,
        Mode::RunFail => test.header.exit.unwrap_or(PANIC_EXIT),
    };
    if out.status != Some(expected_exit) {
        return Err(format!(
            "{what}: expected exit code {expected_exit}, got {}\nstderr:\n{stderr}",
            out.describe_status()
        ));
    }

    let stderr_golden = test.golden("stderr");
    let stdout_golden = test.golden("stdout");
    // A build test's goldens are written by hand, never blessed (D-1026);
    // the harness ignores it under `CHEBY_BLESS=1` anyway.
    if bless && !test.builds_an_executable() {
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

/// A build test (D-1026): `cheby --color=never build -o <tmp>/<name>` and
/// the test's `@flags` in the copied package, then, if the build
/// succeeded, the executable with the test's `.stdin` and `.args` in the
/// package's directory. The result is what both printed, standard error
/// as the build's followed by the executable's, or the failed build's own
/// status and output, so a test can pin an error that `cheby build`
/// reports (`aot::build_then_run`): GAP(ui-build-failed-build), a
/// `run-fail` test with `// @exit: 1` pins E0053 at build time, as D-1026
/// asks, the way a run test pins it under `cheby run`.
///
/// GAP(ui-build-output): the executable goes to a fresh temporary
/// directory with `-o`, as the `@aot` trial's does (D-1025), never to
/// `build/native/` of the copied package; the directory is removed at the
/// end.
fn build_and_run(
    test: &UiTest,
    package: &Path,
    input: &RunInput,
    cache: &HarnessCache,
    timeout: Duration,
) -> Result<process::RunOutput, String> {
    let out_dir = FreshDir::create("cheby-ui-build")?;
    let name = test.name.rsplit('/').next().unwrap_or("main");
    let executable = out_dir.path().join(name);
    let output = executable.to_string_lossy().into_owned();
    aot::build_then_run(
        || {
            let mut cmd = Command::new(CHEBY);
            cmd.args(["--color=never", "build", "-o", &output])
                .args(&test.header.flags)
                .current_dir(package);
            child_env(&mut cmd, test, cache);
            process::run(cmd, None, timeout)
        },
        || {
            let mut cmd = Command::new(&executable);
            cmd.args(&input.args).current_dir(package);
            child_env(&mut cmd, test, cache);
            process::run(cmd, input.stdin.as_deref(), timeout)
        },
    )
}

/// The environment of every child, `cheby` or a test's executable.
/// `NO_COLOR` keeps goldens plain even if the developer's environment
/// forces color (`CLICOLOR_FORCE`). Freed objects are poisoned, so a
/// reference-count mistake stops the program where it happens (D-544).
/// `CHEBY_CACHE` names the harness's own cache (D-959).
fn child_env(cmd: &mut Command, test: &UiTest, cache: &HarnessCache) {
    cmd.env("NO_COLOR", "1").env(POISON_ENV, "1");
    cache.apply(cmd);
    // The reuse report describes Perceus counting, so a test of it runs
    // with `CHEBY_RC` removed even when the harness runs under
    // `CHEBY_RC=naive` (D-718, D-853).
    if test
        .header
        .flags
        .iter()
        .any(|flag| flag == REUSE_REPORT_FLAG)
    {
        cmd.env_remove(RC_ENV);
    }
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
