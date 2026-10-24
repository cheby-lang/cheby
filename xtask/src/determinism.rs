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
//! 3. The cache (D-972, first half): each build's `cheby check` writes a
//!    fresh cache of its own, under its own absolute path; `CHEBY_DUMP_DIR`
//!    bypasses reads and still writes (D-965). The two caches' `v1/blob/`
//!    and `v1/rec/` trees must be identical byte for byte; `v1/hint/` is
//!    left out. D-972's warm rebuild is K8's (M3 wave 10).
//! 4. Executables (D-934): every runnable example (not `skip-run`) is also
//!    built with `cheby build -o <dir>/<example>` from each of the two
//!    copies, with a fresh cache of its own and the same basename. The two
//!    executables must be identical byte for byte; where `cheby build`
//!    refuses the program (a "not available yet" tool error, D-924, or a
//!    compile error), the two refusals' status and standard error must be.
//!    Any other failure, and a refusal of an example in [`MUST_BUILD`],
//!    fails the check.
//!    The runtime archive is built with `cheby` ([`crate::build_cheby`],
//!    D-929).

use std::path::{Path, PathBuf};
use std::process::Command;

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

pub fn run(root: &Path, lines: usize, filter: Option<&str>) -> Result<(), String> {
    let base = root.join("target/determinism");
    if base.exists() {
        std::fs::remove_dir_all(&base).map_err(|e| format!("{}: {e}", base.display()))?;
    }
    let (projects, mut failures) = generate(&base, lines)?;

    // The compiler.
    let cheby = crate::build_cheby(root)?;
    let xtask = std::env::current_exe().map_err(|e| format!("cannot find xtask itself: {e}"))?;
    let examples = example::all(root)?;
    let runnable: Vec<String> = examples
        .iter()
        .filter(|ex| !ex.config.skip_run)
        .map(|ex| ex.name.clone())
        .collect();
    let mut all: Vec<(String, PathBuf)> =
        examples.into_iter().map(|ex| (ex.name, ex.dir)).collect();
    all.extend(projects);
    let mut compared: usize = 0;
    let mut executables: usize = 0;
    for (name, dir) in &all {
        if filter.is_some_and(|f| !name.contains(f)) {
            continue;
        }
        for (label, top, jobs) in BUILDS {
            let copy = base.join(top).join(name);
            copy_tree(dir, &copy)?;
            let out = base.join("out").join(label).join(name);
            std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
            // A fresh cache of its own, under the build's own path (D-972).
            let cache = cache_dir(&base, top, name);
            let mut check = Command::new(&cheby);
            check
                .args(jobs_args(jobs))
                .args(["--color=never", "check"])
                .env("CHEBY_CACHE", &cache)
                .env_remove("CHEBY_CACHE_LOG");
            record(
                check,
                &copy,
                &out.join("check"),
                &out.join("check.out"),
                true,
            )?;
            // Its standard output is the build's times.
            let mut build = Command::new(&xtask);
            build.arg("bench-build").args(jobs_args(jobs)).arg(&copy);
            record(build, &copy, &out.join("all"), &out.join("all.out"), false)?;
            if runnable.contains(name) {
                build_executable(&cheby, &copy, &base, top, label, name, jobs)?;
            }
        }
        if runnable.contains(name) {
            if report_executables(&base, name)? {
                executables = executables.saturating_add(1);
            } else {
                failures = failures.saturating_add(1);
            }
        }
        compared = compared.saturating_add(1);
        let a = base.join("out/a").join(name);
        let b = base.join("out/b").join(name);
        match compare_dumps(&a, &b)? {
            None => println!("ok    {name}: {} files identical", count_files(&a)?),
            Some(diff) => {
                failures = failures.saturating_add(1);
                println!("FAIL  {name}: {diff}");
            }
        }
        match compare_caches(&base, name)? {
            Ok(files) => println!("ok    {name}: cache, {files} blobs and records identical"),
            Err(diff) => {
                failures = failures.saturating_add(1);
                println!("FAIL  {name}: cache {diff}");
            }
        }
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
    match failures {
        0 => Ok(()),
        n => Err(format!("{n} outputs are not deterministic")),
    }
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

/// `cheby build -o <executable>` in `copy` (D-934), with a fresh cache of
/// its own, recording its status and standard error.
fn build_executable(
    cheby: &Path,
    copy: &Path,
    base: &Path,
    top: &str,
    label: &str,
    name: &str,
    jobs: Option<usize>,
) -> Result<(), String> {
    let exe = executable_path(base, label, name);
    let dir = exe.parent().unwrap_or(base);
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let cache = base.join("exe-cache").join(top).join(name);
    let out = Command::new(cheby)
        .args(jobs_args(jobs))
        .args(["--color=never", "build", "-o"])
        .arg(&exe)
        .current_dir(copy)
        .env("CHEBY_CACHE", &cache)
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

/// Prints the comparison of `name`'s executables; whether they agree.
fn report_executables(base: &Path, name: &str) -> Result<bool, String> {
    Ok(match compare_executables(base, name)? {
        Ok(what) => {
            println!("ok    {name}: {what}");
            true
        }
        Err(diff) => {
            println!("FAIL  {name}: {diff}");
            false
        }
    })
}

/// D-934: the two executables of `name`, byte for byte, or, when the
/// build refused the program, the two refusals. `Ok` says what was
/// compared, naming the executable; `Err` gives the first difference.
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
        return Ok(refusal(name, &log_a));
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

/// The runnable examples `cheby build` must build in this milestone's
/// state (D-1164): D-1114's 001, 003 and 024 (D-924's
/// first slice and what N4's not-yet criteria let through), and the
/// examples whose only refusal was their user types or `std::io::Error`'s
/// layout, which the module records now carry (D-919, D-920; N5a): 002,
/// 004, 008, 025 and 032. A refusal of one of them is a failure; the
/// others may still be refused with D-1114's set (a "not available yet"
/// tool error or a compile error, status 1). The list grows as later
/// packages lift the not-yet errors, up to every runnable example with
/// N6.
const MUST_BUILD: [&str; 8] = [
    "001_hello_world",
    "002_greet",
    "003_fizzbuzz",
    "004_tictactoe",
    "008_json",
    "024_two_sum",
    "025_linked_list",
    "032_aoc_2024_day_1",
];

/// Whether a `cheby build` that failed alike twice, as `log` records it,
/// is a refusal module documentation item 4 allows: a "not available yet"
/// tool error (D-924) or a compile error, with exit status 1, of an
/// example not in [`MUST_BUILD`]. Any other failure (a missing or stale
/// runtime archive, D-929 and D-930, no C compiler, a failed link, an
/// internal error, a signal) fails the check, since nothing may skip on it.
fn refusal(name: &str, log: &str) -> Result<String, String> {
    let stderr = log.split_once("--- stderr\n").map_or("", |(_, e)| e);
    let first = stderr
        .lines()
        .find(|l| l.starts_with("error"))
        .unwrap_or("(no error line)");
    let allowed = log.starts_with("exit: Some(1)\n")
        && !stderr.contains("internal compiler error")
        && (first.starts_with("error: `cheby build` is not available yet")
            || first.starts_with("error: `cheby build --target native` is not available yet")
            || first.starts_with("error[E"));
    if !allowed {
        return Err(format!(
            "`cheby build` failed twice alike, and not with a \"not available yet\" tool error \
             or a compile error: {first}"
        ));
    }
    if MUST_BUILD.contains(&name) {
        return Err(format!(
            "`cheby build` must build this example, but refused it twice alike: {first}"
        ));
    }
    Ok(format!("`cheby build` refused it twice alike: {first}"))
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

    fn log(exit: &str, stderr: &str) -> String {
        format!("exit: {exit}\n--- stdout\n--- stderr\n{stderr}")
    }

    /// Module documentation item 4: only a "not available yet" tool error
    /// or a compile error counts as a refusal, and never for an example
    /// that must build.
    #[test]
    fn refusals_allowed_only_for_not_yet_and_compile_errors() {
        let not_yet = log(
            "Some(1)",
            "error: `cheby build` is not available yet for programs that use constants, such as \
             `x::y` (planned for a later milestone)\n",
        );
        assert!(refusal("029_csv_report", &not_yet).is_ok());
        assert!(refusal("001_hello_world", &not_yet).is_err());
        let compile = log(
            "Some(1)",
            "error[E0053]: no symbol\n --> src/main.cheby:1:1\n",
        );
        assert!(refusal("029_csv_report", &compile).is_ok());
        assert!(refusal("002_greet", &compile).is_err());
        assert!(refusal("025_linked_list", &not_yet).is_err());
        for stderr in [
            "error: the runtime archive `/nonexistent/libcheby_rt_exe.a` does not exist\n",
            "error: the runtime archive `x` was built from another runtime\n",
            "error: internal compiler error: something\n",
            "error: the link failed\n",
            "",
        ] {
            assert!(
                refusal("029_csv_report", &log("Some(1)", stderr)).is_err(),
                "{stderr}"
            );
        }
        assert!(refusal("029_csv_report", &log("None", "error[E0053]: x\n")).is_err());
        assert!(refusal("029_csv_report", &log("Some(101)", "error[E0053]: x\n")).is_err());
    }
}
