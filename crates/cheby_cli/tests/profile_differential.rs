//! The profile differential: every example of `docs/examples` that is
//! run, and every program of `bench/runtime`, gives the same standard
//! output, standard error and exit status in the release profile as in
//! the debug profile.
//!
//! Each program runs three times with the workspace's `cheby`: `cheby
//! run` (debug, the reference), `cheby run --release` and the executable
//! of `cheby build --release`, which runs in the program's directory with
//! its arguments, standard input and environment. An example takes its
//! inputs from the files beside its goldens (`args`, `stdin`, `env`, with
//! `{http_fixture}` expanded, D-1027); a benchmark gets the small size
//! `BENCH_SIZE` as its argument, so the whole group runs in seconds; its
//! checksum depends only on the size. A debug run that leaks (status 102,
//! D-458) fails the test, so no comparison passes on a leak.
//!
//! The programs run a few at a time; the examples that wait for real time
//! (`timing = true`, D-725) run after the others, one at a time. Every
//! `cheby` gets one fresh `CHEBY_CACHE` of the test process (D-959,
//! D-1128), and `CHEBY_THREADS`, `CHEBY_RC` and `CHEBY_RELEASE_OFF` are
//! removed from its environment, so neither profile runs a variant.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use cheby_test_support::cache::{FreshDir, HarnessCache};
use cheby_test_support::example::{self, Example};
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");

/// The size every benchmark of `bench/runtime` gets as its first argument.
const BENCH_SIZE: &str = "200";

/// The variables no run inherits: each would select a variant (D-722,
/// D-746) or turn a release lever off.
const REMOVED: [&str; 4] = [
    "CHEBY_THREADS",
    "CHEBY_RC",
    "CHEBY_RELEASE_OFF",
    "CHEBY_DUMP_DIR",
];

/// One program to run in both profiles.
#[derive(Clone, Debug)]
struct Program {
    /// Its name in messages: the example's directory, or `<group>/<name>`.
    name: String,
    /// Its package directory, where it runs.
    dir: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
    stdin: Option<String>,
    /// Whether it waits for real time, so it runs alone.
    timing: bool,
}

#[expect(
    clippy::unwrap_used,
    reason = "a test helper: a bad CHEBY_HARNESS_SCALE fails the test"
)]
fn timeout() -> Duration {
    process::scaled_timeout(Duration::from_secs(300)).unwrap()
}

/// Every example that is run, with its inputs.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: an unreadable example fails the test"
)]
fn examples(root: &Path) -> Vec<Program> {
    let mut out = Vec::new();
    for ex in example::all(root).unwrap() {
        if ex.config.skip_run {
            continue;
        }
        out.push(example_program(&ex));
    }
    out
}

#[expect(
    clippy::unwrap_used,
    reason = "a test helper: an unreadable example fails the test"
)]
fn example_program(ex: &Example) -> Program {
    let goldens = ex.goldens().unwrap();
    let run_args = ex.run_args(&goldens).unwrap();
    Program {
        name: ex.name.clone(),
        dir: ex.dir.clone(),
        args: run_args.args,
        env: run_args.env,
        stdin: goldens.stdin,
        timing: ex.config.timing,
    }
}

/// Every benchmark of `bench/runtime`: each directory of a group that
/// holds a `cheby.toml`, sorted.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: an unreadable directory fails the test"
)]
fn benchmarks(root: &Path) -> Vec<Program> {
    let mut out = Vec::new();
    let runtime = root.join("bench/runtime");
    let mut groups: Vec<PathBuf> = std::fs::read_dir(&runtime)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    groups.sort();
    for group in groups {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&group)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.join("cheby.toml").is_file())
            .collect();
        dirs.sort();
        for dir in dirs {
            let name = format!(
                "{}/{}",
                group.file_name().unwrap().to_string_lossy(),
                dir.file_name().unwrap().to_string_lossy()
            );
            out.push(Program {
                name,
                dir,
                args: vec![BENCH_SIZE.to_owned()],
                env: Vec::new(),
                stdin: None,
                timing: false,
            });
        }
    }
    out
}

/// `cmd` with `program`'s environment and the test's cache, run with its
/// standard input.
fn run(mut cmd: Command, program: &Program, cache: &HarnessCache) -> Result<RunOutput, String> {
    cmd.current_dir(&program.dir).env("NO_COLOR", "1");
    for name in REMOVED {
        cmd.env_remove(name);
    }
    for (key, value) in &program.env {
        cmd.env(key, value);
    }
    cache.apply(&mut cmd);
    process::run(cmd, program.stdin.as_deref().map(str::as_bytes), timeout())
}

/// `cheby --color=never <args>` for `program`.
fn cheby(program: &Program, args: &[&str], cache: &HarnessCache) -> Result<RunOutput, String> {
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never").args(args);
    run(cmd, program, cache)
}

/// The first difference between `reference` and `actual`, the run `what`.
fn compare(what: &str, reference: &RunOutput, actual: &RunOutput) -> Result<(), String> {
    if actual.timed_out || actual.status != reference.status {
        return Err(format!(
            "{what}: {} where debug gave {}\nstderr:\n{}",
            actual.describe_status(),
            reference.describe_status(),
            actual.stderr
        ));
    }
    for (stream, want, got) in [
        ("stdout", &reference.stdout, &actual.stdout),
        ("stderr", &reference.stderr, &actual.stderr),
    ] {
        if want != got {
            return Err(format!(
                "{what}: {stream} differs\n--- debug\n{want}\n--- release\n{got}"
            ));
        }
    }
    Ok(())
}

/// Runs `program` in both profiles, as an executable too, and compares.
fn differential(program: &Program, cache: &HarnessCache) -> Result<(), String> {
    let mut run_args = vec!["run"];
    let mut release_args = vec!["run", "--release"];
    if !program.args.is_empty() {
        run_args.push("--");
        release_args.push("--");
        for arg in &program.args {
            run_args.push(arg);
            release_args.push(arg);
        }
    }
    let debug = cheby(program, &run_args, cache)?;
    if debug.timed_out || debug.status == Some(102) {
        return Err(format!(
            "the debug run {}\nstderr:\n{}",
            debug.describe_status(),
            debug.stderr
        ));
    }
    compare(
        "cheby run --release",
        &debug,
        &cheby(program, &release_args, cache)?,
    )?;
    let out = FreshDir::create(&format!("cheby-profile-{}", program.name.replace('/', "-")))?;
    let executable = out.path().join("program");
    let output = executable.to_string_lossy().into_owned();
    let built = cheby(program, &["build", "--release", "-o", &output], cache)?;
    if built.timed_out || built.status != Some(0) {
        // A program whose build fails stops before its run in debug too
        // (an error), with the same report.
        return compare("cheby build --release", &debug, &built);
    }
    let mut cmd = Command::new(&executable);
    cmd.args(&program.args);
    let ran = run(cmd, program, cache)?;
    let joined = RunOutput {
        stdout: format!("{}{}", built.stdout, ran.stdout),
        stderr: format!("{}{}", built.stderr, ran.stderr),
        ..ran
    };
    compare("the release executable", &debug, &joined)
}

/// Runs `job` on every program of `programs` on a few threads; the
/// results in the programs' order.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: a poisoned lock means a job panicked, which fails the test"
)]
fn parallel(
    programs: &[Program],
    job: impl Fn(&Program) -> Result<(), String> + Sync,
) -> Vec<(String, Result<(), String>)> {
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(1, 4));
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(program) = programs.get(index) else {
                        break;
                    };
                    let result = job(program);
                    results
                        .lock()
                        .unwrap()
                        .push((index, program.name.clone(), result));
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|(index, _, _)| *index);
    results
        .into_iter()
        .map(|(_, name, result)| (name, result))
        .collect()
}

/// Runs every program, those that wait for real time one at a time after
/// the others, and fails with every difference.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: a cache that cannot be created fails the test"
)]
fn check_all(programs: &[Program]) {
    let cache = HarnessCache::create("profile-differential").unwrap();
    let (timing, others): (Vec<Program>, Vec<Program>) =
        programs.iter().cloned().partition(|p| p.timing);
    let mut results = parallel(&others, |p| differential(p, &cache));
    for program in &timing {
        results.push((program.name.clone(), differential(program, &cache)));
    }
    let failures: Vec<String> = results
        .into_iter()
        .filter_map(|(name, result)| result.err().map(|e| format!("{name}: {e}")))
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} programs differ between the profiles:\n\n{}",
        failures.len(),
        programs.len(),
        failures.join("\n\n")
    );
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code and builds executables; Windows support is M6 (D-545, D-935)"
)]
fn profile_differential_examples() {
    let programs = examples(&cheby_test_support::repo_root());
    assert!(programs.len() >= 30, "{} examples", programs.len());
    check_all(&programs);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code and builds executables; Windows support is M6 (D-545, D-935)"
)]
fn profile_differential_benchmarks() {
    let programs = benchmarks(&cheby_test_support::repo_root());
    assert!(programs.len() >= 20, "{} benchmarks", programs.len());
    check_all(&programs);
}
