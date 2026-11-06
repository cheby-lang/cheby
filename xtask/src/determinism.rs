//! `cargo xtask check-determinism` (D-264, D-551, docs/plan/testing.md §6).
//!
//! 1. The benchmark generator: each shape is generated into two different
//!    directories and the trees are compared (D-291).
//!    The `huge_*` shapes (one literal of [`HUGE_ELEMENTS`] computed
//!    elements of each kind) are generated too, so that the continuations
//!    lowering splits a large literal into (D-720, D-780) are compared.
//! 2. The compiler: every example and a generated project of each shape is
//!    copied under two different absolute paths and built in each, once
//!    with `--jobs 1` and once with the default number of threads. Each
//!    build is
//!    - `cheby check` (constants evaluated, `Roots::Constants`), and
//!    - `xtask bench-build`, which checks and compiles every function of
//!      the package (`Roots::AllFunctions`),
//!
//!    both with `CHEBY_DUMP_DIR` set, so the driver writes the interface
//!    encodings (`iface/`), the MIR dumps (`mir/`), the CLIF dump
//!    (`clif.txt`) and the constants' debug texts (`consts.txt`) (plan
//!    §4.5). The commands' exit status and output are recorded too. The
//!    two dump trees must be identical byte for byte.
//! 3. The cache (D-972): each build writes a fresh cache of its own, under
//!    its own absolute path. Its `cheby check` writes it with
//!    `CHEBY_DUMP_DIR` set, which bypasses reads and still writes (D-965);
//!    for a runnable example, its `cheby build` (item 4) and a `cheby run`
//!    with the example's arguments, environment and input
//!    ([`example::Example::run_args`]: 031 against the HTTP fixture,
//!    D-1027), and for an example with tests, a `cheby test`, write into
//!    the same cache, so code, tests, std, constant, warm-facts, reuse-line
//!    and C object records (020) join the comparison. The runs' and tests'
//!    exit status and output are recorded beside the dumps and compared
//!    with them. The two caches' `v1/blob/` and `v1/rec/` trees must be
//!    identical byte for byte, with no exception; `v1/hint/` is left out.
//!    The summary line names the commands that wrote into the cache, in the
//!    order `check, build, run, test`.
//! 4. Executables (D-934): every runnable example (one whose `test.toml`
//!    has no `skip-run`, [`must_build`]) is also built with
//!    `cheby build -o <dir>/<example>` from each of the two copies, into
//!    the build's cache, with the same basename. Every one of them must
//!    build, and its two executables must be identical byte for byte
//!    (D-1164, D-934): a refusal of a runnable example, even one "not
//!    available yet" (D-924) or a compile error, fails the check like any
//!    other failure of `cheby build`. There is no list: an example joins
//!    the moment its `skip-run` goes. Examples with `skip-run` are only
//!    checked, never built into executables.
//!    The runtime archive is built with `cheby` ([`crate::build_cheby`],
//!    D-929).
//! 5. The warm rebuild (D-972, D-268): once the two caches are compared,
//!    every example is checked again with `cheby check` on the first
//!    build's cache and copy, and a runnable example is run again with
//!    `cheby run`, each without `CHEBY_DUMP_DIR`, so they read the cache.
//!    Their exit status, standard output and standard error must equal the
//!    first build's cold ones. D-1283: a constant's value is
//!    compared through its cache entry. Every `const` lookup of the warm
//!    commands (`CHEBY_CACHE_LOG`, D-1224) must hit, so each value the warm
//!    commands use is the entry the cold build wrote, which item 3 compared
//!    byte for byte; and the program's output, which prints what it uses,
//!    must equal the cold run's. After the warm commands the first cache
//!    must still equal the second byte for byte. A warm command never
//!    prints the port of 031's fixture, and nothing here compares it
//!    (D-1027).
//!
//! Projects are built a few at a time on threads of their own; the
//! examples that wait for real time (`timing` in `test.toml`, D-725) are
//! built afterwards, one at a time. The lines are printed in the projects'
//! order.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cheby_bench_gen::{Config, Shape};
use cheby_test_support::example;

/// The two builds of each project: a label, the directory the project is
/// copied into (a different absolute path each time) and `--jobs`.
const BUILDS: [(&str, &str, Option<usize>); 2] =
    [("a", "a", Some(1)), ("b", "b/nested/deeper", None)];

/// The elements of each `huge_*` project (D-720's literal kinds, kept out
/// of `--lines`): large enough that lowering splits every kind into
/// several continuations (D-780), whose numbers and split points must not
/// depend on the path or on `--jobs` (D-264), yet small enough to build
/// in seconds.
const HUGE_ELEMENTS: usize = 2_000;

/// How long one `cheby run` or `cheby test` of an example may take, before
/// `CHEBY_HARNESS_SCALE` (D-722).
const RUN_TIMEOUT: Duration = Duration::from_secs(600);

pub fn run(root: &Path, lines: usize, filter: Option<&str>) -> Result<(), String> {
    let base = root.join("target/determinism");
    if base.exists() {
        std::fs::remove_dir_all(&base).map_err(|e| format!("{}: {e}", base.display()))?;
    }
    let (projects, mut failures) = generate(&base, lines)?;

    // The compiler.
    let cheby = crate::build_cheby(root)?;
    let xtask = std::env::current_exe().map_err(|e| format!("cannot find xtask itself: {e}"))?;
    let mut all = Vec::new();
    for ex in example::all(root)? {
        if filter.is_none_or(|f| ex.name.contains(f)) {
            all.push(Project::of_example(&ex)?);
        }
    }
    all.extend(
        projects
            .into_iter()
            .filter(|(name, _)| filter.is_none_or(|f| name.contains(f)))
            .map(|(name, dir)| Project::shape(name, dir)),
    );
    let ctx = Ctx {
        base: &base,
        cheby: &cheby,
        xtask: &xtask,
    };
    let mut compared: usize = 0;
    let mut executables: usize = 0;
    let mut warm: usize = 0;
    for outcome in build_all(&ctx, &all)? {
        for line in &outcome.lines {
            println!("{line}");
        }
        compared = compared.saturating_add(1);
        executables = executables.saturating_add(outcome.executables);
        warm = warm.saturating_add(outcome.warm);
        failures = failures.saturating_add(outcome.failures);
    }
    println!(
        "compiler artifacts: {compared} projects built with --jobs 1 and the default from two \
         paths into two fresh caches; interfaces, MIR, CLIF, constants, command output and the \
         caches' blobs and records compared"
    );
    println!(
        "executables: {executables} runnable examples built twice with `cheby build` from two \
         paths into two fresh caches under one basename; executables or refusals compared"
    );
    println!(
        "warm: {warm} examples run again warm; exit status, outputs and constant values equal \
         the cold build's"
    );
    match failures {
        0 => Ok(()),
        n => Err(format!("{n} outputs are not deterministic")),
    }
}

/// What the builds of every project share.
#[derive(Debug)]
struct Ctx<'a> {
    /// `target/determinism`.
    base: &'a Path,
    /// The `cheby` under test.
    cheby: &'a Path,
    /// This `xtask`, for `bench-build`.
    xtask: &'a Path,
}

/// A project the compiler builds twice: an example or a generated shape.
#[derive(Debug)]
struct Project {
    name: String,
    dir: PathBuf,
    /// Whether it is an example, which the warm rebuild runs again.
    example: bool,
    /// A runnable example's run (D-1164): built with `cheby build` and run
    /// with `cheby run`.
    run: Option<Run>,
    /// Whether an example has tests, which `cheby test` runs.
    tests: bool,
    /// Whether the example waits for real time (D-725): built after the
    /// others, one at a time.
    timing: bool,
}

/// How a runnable example runs: its standard input, and its arguments and
/// environment with `{http_fixture}` expanded (D-1027).
#[derive(Debug)]
struct Run {
    stdin: Option<String>,
    args: example::RunArgs,
}

impl Project {
    /// The project of `ex`. A runnable example's `{http_fixture}` resolves
    /// to this process's one server for its `site/` (D-1027), which both
    /// builds and the warm run share, so their arguments are equal.
    fn of_example(ex: &example::Example) -> Result<Self, String> {
        let run = if must_build(ex) {
            let goldens = ex.goldens()?;
            Some(Run {
                args: ex.run_args(&goldens)?,
                stdin: goldens.stdin,
            })
        } else {
            None
        };
        Ok(Self {
            name: ex.name.clone(),
            dir: ex.dir.clone(),
            example: true,
            run,
            tests: has_tests(ex)?,
            timing: ex.config.timing,
        })
    }

    /// A generated shape's project: checked and built, never run.
    const fn shape(name: String, dir: PathBuf) -> Self {
        Self {
            name,
            dir,
            example: false,
            run: None,
            tests: false,
            timing: false,
        }
    }

    /// The commands that write into each build's cache, in the summary's
    /// order.
    fn commands(&self) -> Vec<&'static str> {
        let mut out = vec!["check"];
        if self.run.is_some() {
            out.extend(["build", "run"]);
        }
        if self.tests {
            out.push("test");
        }
        out
    }
}

/// Whether `ex` has a `test` block: a line of a source that starts with
/// `test "` (`cheby fmt` puts every test at the top level, unindented).
fn has_tests(ex: &example::Example) -> Result<bool, String> {
    for source in ex.sources()? {
        let text =
            std::fs::read_to_string(&source).map_err(|e| format!("{}: {e}", source.display()))?;
        if text.lines().any(|line| line.starts_with("test \"")) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What the builds of one project printed and counted.
#[derive(Debug, Default)]
struct Outcome {
    lines: Vec<String>,
    failures: usize,
    executables: usize,
    warm: usize,
}

impl Outcome {
    fn ok(&mut self, mut line: String) {
        line.insert_str(0, "ok    ");
        self.lines.push(line);
    }

    fn fail(&mut self, mut line: String) {
        self.failures = self.failures.saturating_add(1);
        line.insert_str(0, "FAIL  ");
        self.lines.push(line);
    }
}

/// Builds every project, a few at a time on threads of their own, then the
/// ones that wait for real time one at a time (D-725), and returns their
/// outcomes in the projects' order.
fn build_all(ctx: &Ctx<'_>, all: &[Project]) -> Result<Vec<Outcome>, String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, PoisonError};

    let slots: Mutex<Vec<Option<Result<Outcome, String>>>> =
        Mutex::new(all.iter().map(|_| None).collect());
    let store = |i: usize, outcome: Result<Outcome, String>| {
        let mut slots = slots.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = slots.get_mut(i) {
            *slot = Some(outcome);
        }
    };
    let workers = std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).clamp(1, 4));
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(project) = all.get(i) else {
                        break;
                    };
                    if !project.timing {
                        store(i, build_project(ctx, project));
                    }
                }
            });
        }
    });
    for (i, project) in all.iter().enumerate() {
        if project.timing {
            store(i, build_project(ctx, project));
        }
    }
    slots
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
        .into_iter()
        .zip(all)
        .map(|(slot, project)| slot.unwrap_or_else(|| Err(format!("{}: not built", project.name))))
        .collect()
}

/// Builds `project` twice (module documentation, items 2 to 4), compares
/// the two builds, and runs an example again warm (item 5).
fn build_project(ctx: &Ctx<'_>, project: &Project) -> Result<Outcome, String> {
    let name = project.name.as_str();
    let base = ctx.base;
    let mut outcome = Outcome::default();
    for (label, top, jobs) in BUILDS {
        let copy = base.join(top).join(name);
        copy_tree(&project.dir, &copy)?;
        let out = base.join("out").join(label).join(name);
        std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
        // A fresh cache of its own, under the build's own path (D-972).
        let cache = cache_dir(base, top, name);
        let check = cheby_command(ctx.cheby, &cache, jobs, &["check"]);
        record(
            check,
            &copy,
            &out.join("check"),
            &out.join("check.out"),
            true,
        )?;
        // Its standard output is the build's times.
        let mut build = Command::new(ctx.xtask);
        build.arg("bench-build").args(jobs_args(jobs)).arg(&copy);
        record(build, &copy, &out.join("all"), &out.join("all.out"), false)?;
        if let Some(run) = &project.run {
            build_executable(ctx.cheby, &copy, &cache, base, label, name, jobs)?;
            let cmd = run_command(ctx.cheby, &cache, jobs, run);
            record_output(cmd, &copy, run.stdin.as_deref(), &out.join("run.out"))?;
        }
        if project.tests {
            let cmd = cheby_command(ctx.cheby, &cache, jobs, &["test"]);
            record_output(cmd, &copy, None, &out.join("test.out"))?;
        }
    }
    if project.run.is_some() {
        match compare_executables(base, name)? {
            Ok(what) => {
                outcome.executables = 1;
                outcome.ok(format!("{name}: {what}"));
            }
            Err(diff) => outcome.fail(format!("{name}: {diff}")),
        }
    }
    let a = base.join("out/a").join(name);
    let b = base.join("out/b").join(name);
    match compare_dumps(&a, &b)? {
        None => outcome.ok(format!("{name}: {} files identical", count_files(&a)?)),
        Some(diff) => outcome.fail(format!("{name}: {diff}")),
    }
    let commands = project.commands().join(", ");
    match compare_caches(base, name)? {
        Ok(files) => outcome.ok(format!(
            "{name}: cache, {files} blobs and records identical ({commands})"
        )),
        Err(diff) => outcome.fail(format!("{name}: cache {diff} ({commands})")),
    }
    if project.example {
        warm_again(ctx, project, &mut outcome)?;
    }
    Ok(outcome)
}

/// Item 5: `project`, an example, checked again and, if it is runnable,
/// run again on the first build's copy and cache, without
/// `CHEBY_DUMP_DIR`. Their exit status and outputs must equal the first
/// build's cold ones, and every constant lookup must hit
/// (D-1283, module documentation).
fn warm_again(ctx: &Ctx<'_>, project: &Project, outcome: &mut Outcome) -> Result<(), String> {
    let name = project.name.as_str();
    let [(label, top, jobs), _] = BUILDS;
    let copy = ctx.base.join(top).join(name);
    let cache = cache_dir(ctx.base, top, name);
    let cold = ctx.base.join("out").join(label).join(name);
    let warm = ctx.base.join("warm").join(name);
    std::fs::create_dir_all(&warm).map_err(|e| format!("{}: {e}", warm.display()))?;
    let mut differences = Vec::new();

    let mut check = cheby_command(ctx.cheby, &cache, jobs, &["check"]);
    let log = warm.join("check.log");
    check.env("CHEBY_CACHE_LOG", &log);
    let text = record_output(check, &copy, None, &warm.join("check.out"))?;
    differences.extend(warm_difference(
        "check",
        &cold.join("check.out"),
        &text,
        &log,
    )?);
    let what = match &project.run {
        Some(run) => {
            let mut cmd = run_command(ctx.cheby, &cache, jobs, run);
            let log = warm.join("run.log");
            cmd.env("CHEBY_CACHE_LOG", &log);
            let text = record_output(cmd, &copy, run.stdin.as_deref(), &warm.join("run.out"))?;
            differences.extend(warm_difference("run", &cold.join("run.out"), &text, &log)?);
            "warm run"
        }
        None => "warm check",
    };
    // What the warm commands wrote, if anything, is what a cold build
    // writes: the first cache still equals the second.
    if let Err(diff) = compare_caches(ctx.base, name)? {
        differences.push(format!("after the warm commands, cache {diff}"));
    }
    if differences.is_empty() {
        outcome.warm = 1;
        outcome.ok(format!(
            "{name}: {what} equals the cold build (exit, stdout, stderr, constants)"
        ));
    } else {
        outcome.fail(format!(
            "{name}: {what} differs from the cold build: {}",
            differences.join("; ")
        ));
    }
    Ok(())
}

/// How the warm `command`'s record `text` differs from the cold record
/// `cold`, and which constant lookups its log `log` says missed; `None`
/// when it does not.
fn warm_difference(
    command: &str,
    cold: &Path,
    text: &str,
    log: &Path,
) -> Result<Option<String>, String> {
    let cold_text =
        std::fs::read_to_string(cold).map_err(|e| format!("{}: {e}", cold.display()))?;
    if cold_text != text {
        let at = cold_text
            .lines()
            .zip(text.lines())
            .position(|(c, w)| c != w)
            .unwrap_or_else(|| cold_text.lines().count().min(text.lines().count()));
        let line = |t: &str| t.lines().nth(at).unwrap_or("<end>").to_owned();
        return Ok(Some(format!(
            "`cheby {command}` differs at line {}: cold `{}`, warm `{}`",
            at.saturating_add(1),
            line(&cold_text),
            line(text)
        )));
    }
    // A command that looked nothing up writes no log.
    let lines = std::fs::read_to_string(log).unwrap_or_default();
    let missed: Vec<&str> = lines
        .lines()
        .filter(|l| l.starts_with("const ") && l.ends_with(" miss"))
        .collect();
    Ok((!missed.is_empty()).then(|| {
        format!(
            "`cheby {command}` evaluated constants again: {}",
            missed.join(", ")
        )
    }))
}

/// `cheby <args>` with `cache` and `jobs`, reading no dump directory and
/// writing no lookup log unless the caller sets one.
fn cheby_command(cheby: &Path, cache: &Path, jobs: Option<usize>, args: &[&str]) -> Command {
    let mut cmd = Command::new(cheby);
    cmd.args(jobs_args(jobs))
        .arg("--color=never")
        .args(args)
        .env("CHEBY_CACHE", cache)
        .env_remove("CHEBY_CACHE_LOG")
        .env_remove("CHEBY_DUMP_DIR");
    cmd
}

/// `cheby run` of a runnable example, with its arguments after `--` and
/// its environment, as the conformance harness runs it.
fn run_command(cheby: &Path, cache: &Path, jobs: Option<usize>, run: &Run) -> Command {
    let mut cmd = cheby_command(cheby, cache, jobs, &["run"]);
    if !run.args.args.is_empty() {
        cmd.arg("--").args(&run.args.args);
    }
    for (key, value) in &run.args.env {
        cmd.env(key, value);
    }
    cmd
}

/// Runs `cmd` in `dir` with `stdin`, writes its exit status, standard
/// output and standard error to `log` in [`record`]'s form, and returns
/// that text.
fn record_output(
    mut cmd: Command,
    dir: &Path,
    stdin: Option<&str>,
    log: &Path,
) -> Result<String, String> {
    cmd.current_dir(dir);
    let timeout = cheby_test_support::process::scaled_timeout(RUN_TIMEOUT)?;
    let out = cheby_test_support::process::run(cmd, stdin.map(str::as_bytes), timeout)?;
    let text = format!(
        "exit: {:?}{}\n--- stdout\n{}--- stderr\n{}",
        out.status,
        if out.timed_out { " (timed out)" } else { "" },
        out.stdout,
        out.stderr
    );
    std::fs::write(log, &text).map_err(|e| format!("{}: {e}", log.display()))?;
    Ok(text)
}

/// D-972: compares the two builds' caches of `name`, their blobs and
/// records byte for byte (hints are left out). `Ok(files)` when they are
/// identical, `Err` with the first difference otherwise.
fn compare_caches(base: &Path, name: &str) -> Result<Result<usize, String>, String> {
    let [(_, top_a, _), (_, top_b, _)] = BUILDS;
    let mut files: usize = 0;
    for kind in ["v1/blob", "v1/rec"] {
        let a = cache_dir(base, top_a, name).join(kind);
        let b = cache_dir(base, top_b, name).join(kind);
        files = files.saturating_add(count_files(&a)?);
        if let Some(diff) = compare_dumps(&a, &b)? {
            return Ok(Err(format!("{kind}: {diff}")));
        }
    }
    Ok(Ok(files))
}

/// Where the executable of `name` built under `label` goes: one directory
/// per build, one basename, the example's name (D-934's basename is an
/// input).
fn executable_path(base: &Path, label: &str, name: &str) -> PathBuf {
    base.join("exe").join(label).join(name).join(name)
}

/// The record of the executable build of `name` under `label`: its status
/// and standard error.
fn executable_log(base: &Path, label: &str, name: &str) -> PathBuf {
    base.join("exe").join(label).join(format!("{name}.build"))
}

/// `cheby build -o <executable>` in `copy` (D-934), with the build's cache
/// `cache` (module documentation, item 3), recording its status and
/// standard error.
fn build_executable(
    cheby: &Path,
    copy: &Path,
    cache: &Path,
    base: &Path,
    label: &str,
    name: &str,
    jobs: Option<usize>,
) -> Result<(), String> {
    let exe = executable_path(base, label, name);
    let dir = exe.parent().unwrap_or(base);
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let out = Command::new(cheby)
        .args(jobs_args(jobs))
        .args(["--color=never", "build", "-o"])
        .arg(&exe)
        .current_dir(copy)
        .env("CHEBY_CACHE", cache)
        .env_remove("CHEBY_CACHE_LOG")
        .env_remove("CHEBY_DUMP_DIR")
        .output()
        .map_err(|e| format!("cannot run `cheby build`: {e}"))?;
    let text = format!(
        "exit: {:?}\n--- stdout\n{}--- stderr\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let log = executable_log(base, label, name);
    std::fs::write(&log, text).map_err(|e| format!("{}: {e}", log.display()))
}

/// The generator's part (module documentation, 1): each shape generated
/// into two directories and compared; the projects of the first copies,
/// and the number of failures.
fn generate(base: &Path, lines: usize) -> Result<(Vec<(String, PathBuf)>, usize), String> {
    let mut failures: usize = 0;
    let mut projects: Vec<(String, PathBuf)> = Vec::new();
    for shape in Shape::ALL.into_iter().chain(Shape::HUGE_LITERALS) {
        let lines = if shape == Shape::Large {
            lines.max(40_000)
        } else if shape.is_huge_literal() {
            HUGE_ELEMENTS
        } else {
            lines
        };
        let config = Config {
            lines,
            shape,
            seed: 1,
        };
        let a = base.join("gen/a").join(shape.name());
        let b = base.join("gen/b/nested").join(shape.name());
        for dir in [&a, &b] {
            cheby_bench_gen::generate(&config)
                .write(dir)
                .map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        match crate::compare_trees(&a, &b)? {
            None => println!("ok    generator `{}`: identical", shape.name()),
            Some(diff) => {
                failures = failures.saturating_add(1);
                println!("FAIL  generator `{}`: {diff}", shape.name());
            }
        }
        projects.push((format!("bench-{}", shape.name()), a));
    }
    Ok((projects, failures))
}

/// D-934: the two executables of `name`, a runnable example, byte for
/// byte. `Ok` says what was compared, naming the executable; `Err` gives
/// the first difference, or the failure of a build that did not succeed
/// twice, which no runnable example may have (D-1164).
fn compare_executables(base: &Path, name: &str) -> Result<Result<String, String>, String> {
    let [(a, _, _), (b, _, _)] = BUILDS;
    let read_log = |label: &str| {
        let log = executable_log(base, label, name);
        std::fs::read_to_string(&log).map_err(|e| format!("{}: {e}", log.display()))
    };
    let (log_a, log_b) = (read_log(a)?, read_log(b)?);
    if log_a != log_b {
        return Ok(Err(format!(
            "`cheby build` differs between the two builds:\n--- {a}\n{log_a}--- {b}\n{log_b}"
        )));
    }
    let (exe_a, exe_b) = (
        executable_path(base, a, name),
        executable_path(base, b, name),
    );
    if !log_a.starts_with("exit: Some(0)\n") {
        return Ok(Err(failed_build(&log_a)));
    }
    let read = |path: &Path| std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()));
    let (bytes_a, bytes_b) = (read(&exe_a)?, read(&exe_b)?);
    if bytes_a != bytes_b {
        let at = bytes_a
            .iter()
            .zip(&bytes_b)
            .position(|(x, y)| x != y)
            .unwrap_or_else(|| bytes_a.len().min(bytes_b.len()));
        return Ok(Err(format!(
            "executables `{}` and `{}` differ at byte {at} ({} and {} bytes)",
            exe_a.display(),
            exe_b.display(),
            bytes_a.len(),
            bytes_b.len()
        )));
    }
    Ok(Ok(format!(
        "executable `{}` identical to `{}` ({} bytes)",
        exe_a.display(),
        exe_b.display(),
        bytes_a.len()
    )))
}

/// Whether `cheby build` must build `example` into an executable: every
/// runnable example, the ones whose `test.toml` has no `skip-run` (D-1164,
/// D-934). The rule reads `skip-run`, so an example joins the moment it
/// runs; the others are only checked.
fn must_build(example: &example::Example) -> bool {
    !example.config.skip_run
}

/// The failure of a `cheby build` of a runnable example that failed alike
/// twice, as `log` records it. Every such failure fails the check (D-1164,
/// D-934): a "not available yet" tool error (D-924) or a compile error as
/// much as a missing or stale runtime archive (D-929, D-930), no C
/// compiler, a failed link, an internal error or a signal.
fn failed_build(log: &str) -> String {
    let status = log.lines().next().unwrap_or("exit: (none)");
    let stderr = log.split_once("--- stderr\n").map_or("", |(_, e)| e);
    let first = stderr
        .lines()
        .find(|l| l.starts_with("error"))
        .unwrap_or("(no error line)");
    format!(
        "`cheby build` must build every runnable example (D-1164, D-934), but failed twice \
         alike ({status}): {first}"
    )
}

/// The cache of the build of `name` under `top`: a different absolute
/// path for each build, fresh since `base` is removed first.
fn cache_dir(base: &Path, top: &str, name: &str) -> PathBuf {
    base.join("cache").join(top).join(name)
}

fn jobs_args(jobs: Option<usize>) -> Vec<String> {
    jobs.map(|n| vec!["--jobs".to_owned(), n.to_string()])
        .unwrap_or_default()
}

/// Runs `cmd` in `dir` with `CHEBY_DUMP_DIR=dump` and writes its exit
/// status, its standard error and, with `stdout`, its standard output to
/// `log`.
fn record(
    mut cmd: Command,
    dir: &Path,
    dump: &Path,
    log: &Path,
    stdout: bool,
) -> Result<(), String> {
    let out = cmd
        .current_dir(dir)
        .env("CHEBY_DUMP_DIR", dump)
        .output()
        .map_err(|e| format!("cannot run {cmd:?}: {e}"))?;
    let text = format!(
        "exit: {:?}\n--- stdout\n{}--- stderr\n{}",
        out.status.code(),
        if stdout {
            String::from_utf8_lossy(&out.stdout)
        } else {
            "(not compared)\n".into()
        },
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::write(log, text).map_err(|e| format!("{}: {e}", log.display()))
}

/// Copies the directory `from` to `to`, which must not exist.
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let entries = std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", from.display()))?;
        let path = entry.path();
        let target = to.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if kind.is_dir() {
            copy_tree(&path, &target)?;
        } else {
            std::fs::copy(&path, &target).map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    Ok(())
}

/// Every file under `dir`, relative to it, sorted.
fn files_under(dir: &Path) -> Result<Vec<PathBuf>, String> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
        if !dir.exists() {
            return Ok(());
        }
        let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
            let path = entry.path();
            if path.is_dir() {
                walk(&path, base, out)?;
            } else {
                let relative = path
                    .strip_prefix(base)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                out.push(relative.to_path_buf());
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn count_files(dir: &Path) -> Result<usize, String> {
    files_under(dir).map(|files| files.len())
}

/// Describes the first difference between two dump trees: a file only one
/// has, or the first line where a file differs.
fn compare_dumps(a: &Path, b: &Path) -> Result<Option<String>, String> {
    let (files_a, files_b) = (files_under(a)?, files_under(b)?);
    if let Some(only) = files_a.iter().find(|f| !files_b.contains(f)) {
        return Ok(Some(format!(
            "{} exists only with --jobs 1",
            only.display()
        )));
    }
    if let Some(only) = files_b.iter().find(|f| !files_a.contains(f)) {
        return Ok(Some(format!(
            "{} exists only with the default jobs",
            only.display()
        )));
    }
    for file in &files_a {
        let read = |dir: &Path| std::fs::read(dir.join(file)).map_err(|e| e.to_string());
        let (x, y) = (read(a)?, read(b)?);
        if x == y {
            continue;
        }
        let (x, y) = (String::from_utf8_lossy(&x), String::from_utf8_lossy(&y));
        let at = x
            .lines()
            .zip(y.lines())
            .position(|(l, r)| l != r)
            .unwrap_or_else(|| x.lines().count().min(y.lines().count()));
        let line = |text: &str| text.lines().nth(at).unwrap_or("<end>").to_owned();
        return Ok(Some(format!(
            "{} differs at line {}:\n        --jobs 1: {}\n        default:  {}",
            file.display(),
            at.saturating_add(1),
            line(&x),
            line(&y)
        )));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    use cheby_test_support::example::{Example, TestConfig};
    use cheby_test_support::milestone::Milestone;

    fn log(exit: &str, stderr: &str) -> String {
        format!("exit: {exit}\n--- stdout\n--- stderr\n{stderr}")
    }

    fn example(name: &str, skip_run: bool) -> Example {
        Example {
            name: name.to_owned(),
            dir: PathBuf::from(name),
            config: TestConfig {
                milestone: Milestone::M1,
                skip_run,
                reason: skip_run.then(|| "checked only".to_owned()),
                timing: false,
            },
        }
    }

    /// A scratch `base` of `check-determinism` for one test, removed first.
    fn scratch(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("xtask-determinism-{test}-{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        dir
    }

    /// Writes the two builds of `name` under `base` as `build_executable`
    /// records them: each label's log and, if given, its executable.
    fn builds(base: &Path, name: &str, logs: [&str; 2], exes: Option<[&[u8]; 2]>) {
        let [(a, _, _), (b, _, _)] = BUILDS;
        for (i, label) in [a, b].into_iter().enumerate() {
            let exe = executable_path(base, label, name);
            std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
            std::fs::write(executable_log(base, label, name), logs[i]).unwrap();
            if let Some(bytes) = exes {
                std::fs::write(&exe, bytes[i]).unwrap();
            }
        }
    }

    /// Module documentation item 4, the logged rule (D-1164, D-934): every
    /// runnable example (no `skip-run`) must build, so no refusal is
    /// accepted for one, neither a "not available yet" tool error (D-924)
    /// nor a compile error, which D-1114 accepted before the rule covered
    /// every runnable example; any other failure fails as before, and two
    /// identical executables pass. An example with `skip-run` is not built.
    /// The test keeps the name it had under D-1114's accepted refusals, the
    /// rule it pinned before D-1164 reached every runnable example.
    #[test]
    fn refusals_allowed_only_for_not_yet_and_compile_errors() {
        assert!(must_build(&example("001_hello_world", false)));
        assert!(must_build(&example("020_ffi_handle", false)));
        // A synthetic check-only example, named after no example of
        // `docs/examples/` (the log's wave-9 Open list): it once borrowed
        // 031's name, and the real 031 runs since D-1273.
        assert!(!must_build(&example("999_synthetic_check_only", true)));

        let base = scratch("refusals");
        let not_yet = log(
            "Some(1)",
            "error: `cheby build --target native` is not available yet on Windows (planned for \
             M6)\n",
        );
        let compile = log(
            "Some(1)",
            "error[E0053]: no symbol\n --> src/main.cheby:1:1\n",
        );
        let mut refused = vec![not_yet, compile];
        for stderr in [
            "error: the runtime archive `/nonexistent/libcheby_rt_exe.a` does not exist\n",
            "error: the runtime archive `x` was built from another runtime\n",
            "error: internal compiler error: something\n",
            "error: the link failed\n",
            "",
        ] {
            refused.push(log("Some(1)", stderr));
        }
        refused.push(log("None", "error[E0053]: x\n"));
        refused.push(log("Some(101)", "error[E0053]: x\n"));
        for (i, failed) in refused.iter().enumerate() {
            let name = format!("{i:03}_example");
            builds(&base, &name, [failed, failed], None);
            let diff = compare_executables(&base, &name).unwrap().unwrap_err();
            assert!(
                diff.contains("must build every runnable example (D-1164, D-934)"),
                "{diff}"
            );
        }

        let ok = log("Some(0)", "");
        builds(&base, "100_same", [&ok, &ok], Some([b"exe", b"exe"]));
        let same = compare_executables(&base, "100_same").unwrap().unwrap();
        assert!(same.contains("identical"), "{same}");
        builds(&base, "101_other", [&ok, &ok], Some([b"exe", b"exf"]));
        let other = compare_executables(&base, "101_other")
            .unwrap()
            .unwrap_err();
        assert!(other.contains("differ at byte 2"), "{other}");
        builds(
            &base,
            "102_one_failed",
            [&ok, &refused[0]],
            Some([b"exe", b"exe"]),
        );
        let one = compare_executables(&base, "102_one_failed")
            .unwrap()
            .unwrap_err();
        assert!(one.contains("differs between the two builds"), "{one}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Module documentation item 5 (D-1283): a warm command
    /// equals the cold build when its record is the cold one's text and
    /// no constant lookup missed; another output line or a constant
    /// evaluated again is a difference, and a command that looked nothing
    /// up (no log) is none.
    #[test]
    fn determinism_warm_compares_outputs_and_constants() {
        let base = scratch("warm");
        std::fs::create_dir_all(&base).unwrap();
        let cold = base.join("run.out");
        let text = log("Some(0)", "warning[W0002]: x\n");
        std::fs::write(&cold, &text).unwrap();
        let lookups = base.join("run.log");
        std::fs::write(&lookups, "code.run app hit\nconst app::limit hit\n").unwrap();
        assert_eq!(
            warm_difference("run", &cold, &text, &lookups).unwrap(),
            None
        );
        assert_eq!(
            warm_difference("run", &cold, &text, &base.join("none.log")).unwrap(),
            None
        );

        let other = log("Some(1)", "warning[W0002]: x\n");
        let diff = warm_difference("run", &cold, &other, &lookups)
            .unwrap()
            .unwrap();
        assert!(diff.contains("`cheby run` differs at line 1"), "{diff}");

        std::fs::write(&lookups, "const app::limit miss\nconst app::other hit\n").unwrap();
        let diff = warm_difference("run", &cold, &text, &lookups)
            .unwrap()
            .unwrap();
        assert!(
            diff.contains("evaluated constants again: const app::limit miss"),
            "{diff}"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Module documentation item 3: the commands that write into each
    /// build's cache, in the summary line's order.
    #[test]
    fn determinism_warm_cache_lines_name_the_commands() {
        let mut project = Project::shape("bench-small".to_owned(), PathBuf::from("x"));
        assert_eq!(project.commands(), ["check"]);
        project.tests = true;
        assert_eq!(project.commands(), ["check", "test"]);
        project.run = Some(Run {
            stdin: None,
            args: example::RunArgs::default(),
        });
        assert_eq!(project.commands(), ["check", "build", "run", "test"]);
    }
}
