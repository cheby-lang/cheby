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
    let mut failures: usize = 0;

    // The generator.
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

    // The compiler.
    let cheby = crate::build_cheby(root)?;
    let xtask = std::env::current_exe().map_err(|e| format!("cannot find xtask itself: {e}"))?;
    let mut all: Vec<(String, PathBuf)> = example::all(root)?
        .into_iter()
        .map(|ex| (ex.name, ex.dir))
        .collect();
    all.extend(projects);
    let mut compared: usize = 0;
    for (name, dir) in &all {
        if filter.is_some_and(|f| !name.contains(f)) {
            continue;
        }
        for (label, top, jobs) in BUILDS {
            let copy = base.join(top).join(name);
            copy_tree(dir, &copy)?;
            let out = base.join("out").join(label).join(name);
            std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
            let mut check = Command::new(&cheby);
            check.args(jobs_args(jobs)).args(["--color=never", "check"]);
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
    }
    println!(
        "compiler artifacts: {compared} projects built with --jobs 1 and the default from two \
         paths; interfaces, MIR, CLIF, constants and command output compared"
    );
    match failures {
        0 => Ok(()),
        n => Err(format!("{n} outputs are not deterministic")),
    }
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
