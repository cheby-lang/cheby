//! Colored output end to end: `cheby check` with `--color=always` prints
//! the same text as with `--color=never`, plus SGR escapes, on every UI
//! test and every example. And the environment decides `auto` as
//! `src/color.rs` documents.
//!
//! A `libtest_mimic` target (D-1557): one trial per UI test,
//! `ui/<name>`, one per example, `examples/<name>`, and four more, so that
//! nextest spreads the checks over its workers instead of running 900 of
//! them in one test.
//!
//! `cheby check` evaluates constants with the JIT once the front end
//! reports no error, so on Windows the checks of such packages are ignored
//! until M6 (D-545): the examples, and the UI tests that the UI harness
//! ignores there too ([`ui::UiTest::builds_a_jit`]). The other UI tests
//! and the test of quoted escapes, which stops in the front end, still
//! run.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use cheby_diag::strip_colors;
use cheby_test_support::cache::HarnessCache;
use cheby_test_support::process::{self, RunOutput};
use cheby_test_support::ui::{self, Layout};
use cheby_test_support::{platform, repo_root};
use libtest_mimic::{Arguments, Trial};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
const TIMEOUT: Duration = Duration::from_secs(60);

/// The variables that decide `--color=auto`.
const COLOR_VARS: [&str; 4] = ["NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE", "TERM"];

/// Runs `cheby` in `dir` with the color variables removed, then `env`
/// set. Output is piped, so `auto` means no color unless `env` forces it.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to spawn `cheby` fails the test"
)]
fn cheby(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> RunOutput {
    // A fresh build cache of this command (D-1128).
    let cache = HarnessCache::create("cli-color").unwrap();
    let mut cmd = Command::new(CHEBY);
    cmd.args(args).current_dir(dir);
    for var in COLOR_VARS {
        cmd.env_remove(var);
    }
    cmd.envs(env.iter().copied());
    cache.apply(&mut cmd);
    process::run(cmd, None, TIMEOUT).unwrap()
}

/// Checks `dir` plain and colored, and compares the two.
fn compare(dir: &Path, what: &str) {
    let plain = cheby(dir, &["--color=never", "check"], &[]);
    let colored = cheby(dir, &["--color=always", "check"], &[]);
    assert_eq!(plain.status, colored.status, "{what}: exit status");
    assert_eq!(plain.stdout, colored.stdout, "{what}: stdout");
    assert!(!plain.stderr.contains('\x1b'), "{what}: {:?}", plain.stderr);
    assert_eq!(strip_colors(&colored.stderr), plain.stderr, "{what}");
    if !plain.stderr.is_empty() {
        assert_ne!(colored.stderr, plain.stderr, "{what}: not colored");
    }
}

/// Copies `from` to `to`, skipping hidden entries.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to copy the package fails the test"
)]
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A fresh package for the UI test: the single file as `src/main.cheby`,
/// or a copy of the package directory.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to write the package fails the test"
)]
fn package(test: &ui::UiTest) -> PathBuf {
    let dir = Path::new(TMP)
        .join("color")
        .join(test.name.replace('/', "__"));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    match test.layout {
        Layout::File => {
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(
                dir.join("cheby.toml"),
                "name = \"ui_test\"\nversion = \"0.0.0\"\n",
            )
            .unwrap();
            std::fs::copy(&test.path, dir.join("src/main.cheby")).unwrap();
        }
        Layout::Package => copy_dir(&test.path, &dir),
    }
    dir
}

fn main() -> ExitCode {
    let args = Arguments::from_args();
    if let Some(reason) = platform::run_skip_reason() {
        eprintln!("color tests: the checks that build a JIT are ignored: {reason}");
    }
    let trials = match trials() {
        Ok(trials) => trials,
        Err(message) => {
            eprintln!("color tests: {message}");
            return ExitCode::from(2);
        }
    };
    libtest_mimic::run(&args, trials).exit_code()
}

/// Every trial, in a fixed order: the UI tests, the examples, then the
/// tests of the environment, the REPL and tool errors.
fn trials() -> Result<Vec<Trial>, String> {
    let root = repo_root();
    let tests = ui::all(&root)?;
    if tests.is_empty() {
        return Err("no UI tests".to_owned());
    }
    let examples = example_dirs(&root.join("docs/examples"))?;
    if examples.is_empty() {
        return Err("no examples".to_owned());
    }
    let jit_skip = platform::run_skip_reason();
    let mut trials = Vec::new();
    // Every UI test: color adds escapes and changes nothing else. Where
    // programs do not run, the tests that build a JIT are ignored.
    for test in tests {
        let ignore = jit_skip.filter(|_| test.builds_a_jit());
        let trial = Trial::test(format!("ui/{}", test.name), move || {
            compare(&package(&test), &test.name);
            Ok(())
        });
        trials.push(ignore_on(trial, ignore));
    }
    // Every example: color adds escapes and changes nothing else.
    for dir in examples {
        let name = dir
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let trial = Trial::test(format!("examples/{name}"), move || {
            compare(&dir, &dir.display().to_string());
            Ok(())
        });
        trials.push(ignore_on(trial, jit_skip));
    }
    let plain: [(&str, fn()); 3] = [
        (
            "quoted_escapes_are_shown_as_pictures",
            quoted_escapes_are_shown_as_pictures,
        ),
        (
            "environment_and_flag_precedence",
            environment_and_flag_precedence,
        ),
        (
            "tool_and_usage_errors_follow_the_flag",
            tool_and_usage_errors_follow_the_flag,
        ),
    ];
    for (name, body) in plain {
        trials.push(Trial::test(name, move || {
            body();
            Ok(())
        }));
    }
    let repl = Trial::test("repl_follows_the_flag", || {
        repl_follows_the_flag();
        Ok(())
    });
    trials.push(ignore_on(repl, jit_skip));
    Ok(trials)
}

/// `trial`, ignored when there is a `reason`, which `main` prints.
fn ignore_on(trial: Trial, reason: Option<&str>) -> Trial {
    trial.with_ignored_flag(reason.is_some())
}

/// Every example directory under `dir`, sorted.
fn example_dirs(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut examples = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.join("cheby.toml").is_file() {
            examples.push(path);
        }
    }
    examples.sort();
    Ok(examples)
}

/// Source text quoted in a message cannot write a terminal escape: an ESC
/// in a native symbol shows as its control picture, colored or not. The
/// package stops in the front end (E0054), so it builds no JIT and runs on
/// every platform.
#[expect(
    clippy::unwrap_used,
    reason = "a trial: failing to write the package fails it"
)]
fn quoted_escapes_are_shown_as_pictures() {
    let dir = Path::new(TMP).join("color/quoted_escape");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("cheby.toml"),
        "name = \"app\"\nversion = \"0.1.0\"\ntargets = [\"native\"]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/main.cheby"),
        "//! p\n\n@external(native, \"ab\x1b[31mcd\")\nfn a(s: String) -> Nil\n\n\
         @external(native, \"ab\x1b[31mcd\")\nfn b() -> String\n\n\
         fn main() -> Nil {\n  a(b())\n}\n",
    )
    .unwrap();
    compare(&dir, "quoted escape");
    let plain = cheby(&dir, &["--color=never", "check"], &[]);
    assert!(
        plain.stderr.contains("`ab\u{241b}[31mcd`"),
        "{:?}",
        plain.stderr
    );
}

/// A package with a syntax error.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to write the package fails the test"
)]
fn failing_package() -> PathBuf {
    let dir = Path::new(TMP).join("color/env_precedence");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("cheby.toml"),
        "name = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/main.cheby"), "fn main() -> Int {\n  x\n").unwrap();
    dir
}

/// The flag beats the environment, `NO_COLOR` beats `CLICOLOR_FORCE`, and
/// `CLICOLOR_FORCE` beats `CLICOLOR=0`, `TERM=dumb` and a pipe.
fn environment_and_flag_precedence() {
    let dir = failing_package();
    let colored = |args: &[&str], env: &[(&str, &str)]| {
        let out = cheby(&dir, args, env);
        assert_eq!(out.status, Some(1), "{}", out.stderr);
        assert_ne!(out.stderr, "");
        out.stderr.contains('\x1b')
    };
    let check = ["check"];
    // auto through a pipe: plain.
    assert!(!colored(&check, &[]));
    assert!(colored(&check, &[("CLICOLOR_FORCE", "1")]));
    assert!(!colored(&check, &[("CLICOLOR_FORCE", "0")]));
    assert!(!colored(&check, &[("CLICOLOR_FORCE", "")]));
    assert!(!colored(
        &check,
        &[("CLICOLOR_FORCE", "1"), ("NO_COLOR", "1")]
    ));
    assert!(colored(
        &check,
        &[("CLICOLOR_FORCE", "1"), ("NO_COLOR", "")]
    ));
    assert!(colored(
        &check,
        &[("CLICOLOR_FORCE", "1"), ("CLICOLOR", "0"), ("TERM", "dumb")]
    ));
    // The flag, before or after the command, beats the environment.
    assert!(colored(&["--color=always", "check"], &[("NO_COLOR", "1")]));
    assert!(colored(
        &["check", "--color", "always"],
        &[("TERM", "dumb")]
    ));
    assert!(!colored(
        &["--color=never", "check"],
        &[("CLICOLOR_FORCE", "1")]
    ));
    assert!(!colored(&["--color=auto", "check"], &[]));
    // The last flag wins.
    assert!(colored(&["--color=never", "check", "--color=always"], &[]));
}

/// The REPL's diagnostics and tool errors follow `--color` too. Ignored
/// where programs do not run: the REPL does not run on Windows until M6
/// (D-545).
#[expect(
    clippy::unwrap_used,
    reason = "a trial: failing to start `cheby` fails it"
)]
fn repl_follows_the_flag() {
    let dir = Path::new(TMP);
    let input: &[u8] = b"let x: Int = \"a\"\n:bogus\n:quit\n";
    let session = |flag: &str| {
        let cache = HarnessCache::create("cli-color").unwrap();
        let mut cmd = Command::new(CHEBY);
        cmd.args([flag, "repl"]).current_dir(dir);
        for var in COLOR_VARS {
            cmd.env_remove(var);
        }
        cache.apply(&mut cmd);
        process::run(cmd, Some(input), TIMEOUT).unwrap()
    };
    let plain = session("--color=never");
    let colored = session("--color=always");
    assert!(plain.stderr.contains("error[E0047]"), "{}", plain.stderr);
    assert!(!plain.stderr.contains('\x1b'), "{:?}", plain.stderr);
    assert!(colored.stderr.contains('\x1b'), "{:?}", colored.stderr);
    assert_eq!(strip_colors(&colored.stderr), plain.stderr);
    assert_eq!(colored.stdout, plain.stdout);
    assert_eq!(colored.status, plain.status);
}

/// Tool errors without a code follow the same choice, and clap's usage
/// errors do too.
fn tool_and_usage_errors_follow_the_flag() {
    let dir = Path::new(TMP);
    let build = cheby(dir, &["--color=always", "build", "--target", "js"], &[]);
    assert_eq!(build.status, Some(1));
    assert_eq!(
        build.stderr,
        "\x1b[1;91merror\x1b[0m\x1b[1m: `cheby build --target js` is not available yet \
         (planned for M4)\x1b[0m\n\x1b[1;96mhelp\x1b[0m: use `cheby check` to type-check the \
         package, or `cheby run` to run it\n"
    );
    let plain = cheby(dir, &["build", "--target", "js"], &[]);
    assert_eq!(strip_colors(&build.stderr), plain.stderr);

    let usage = cheby(dir, &["--color=always", "bogus"], &[]);
    assert_eq!(usage.status, Some(2));
    assert!(usage.stderr.contains('\x1b'), "{}", usage.stderr);
    let usage = cheby(
        dir,
        &["bogus"],
        &[("CLICOLOR_FORCE", "1"), ("NO_COLOR", "1")],
    );
    assert!(!usage.stderr.contains('\x1b'), "{}", usage.stderr);
}
