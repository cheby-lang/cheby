//! `cargo xtask bench-compile [--quick]`: compile-speed numbers (I1, D-247,
//! plan §8.5, docs/plan/testing.md §7). Reported, never enforced.
//!
//! For generated projects of 10k, 100k and 1M lines (`--quick`: 10k only)
//! it measures:
//!
//! - checking: `cheby check`, as lines per second per core;
//! - checking with code generation: [`cheby_driver::bench_build`], which
//!   runs the front end and then lowers and compiles every function of
//!   the package (`Roots::AllFunctions`), timed separately;
//! - the edit-to-run latency: one function body is changed, then
//!   `cheby run` is timed until `main` has run (a full rebuild in M1).
//!
//! The numbers come from an optimized toolchain: the command re-runs
//! itself with `--release` when it was built without optimizations, and
//! runs the release `cheby`. Each measurement is the fastest of a few
//! runs. The front end and code generation are single-threaded until G1
//! lands (W4-E, W5-C), so "per core" is per thread used, 1.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use cheby_bench_gen::{Config, Shape};

/// The projects measured: lines and shapes. The `wide` shape keeps `main`
/// cheap to run (its import graph is shallow), so it gives the edit-to-run
/// latency.
const SIZES: [(usize, &[Shape]); 3] = [
    (
        10_000,
        &[Shape::Small, Shape::Deep, Shape::Wide, Shape::Generic],
    ),
    (100_000, &[Shape::Small, Shape::Large, Shape::Wide]),
    (1_000_000, &[Shape::Large, Shape::Wide]),
];

/// Runs per measurement; the fastest counts.
const RUNS: usize = 3;
/// Runs per measurement on the 1M-line projects.
const RUNS_1M: usize = 1;

/// The environment variable that marks the optimized re-run.
const RERUN_ENV: &str = "CHEBY_BENCH_RERUN";

/// One row of the report.
struct Row {
    lines: usize,
    shape: Shape,
    modules: usize,
    check: Duration,
    front_end: Duration,
    back_end: Duration,
    edit_to_run: Option<Duration>,
}

pub fn run(root: &Path, quick: bool, summary: Option<&Path>) -> Result<(), String> {
    if cfg!(debug_assertions) && std::env::var_os(RERUN_ENV).is_none() {
        return rerun_optimized(root, quick, summary);
    }
    let cheby = build_cheby(root)?;
    let base = root.join("target/bench-compile");
    if base.exists() {
        std::fs::remove_dir_all(&base).map_err(|e| format!("{}: {e}", base.display()))?;
    }
    let mut rows = Vec::new();
    for (lines, shapes) in SIZES {
        if quick && lines > 10_000 {
            continue;
        }
        let runs = if lines >= 1_000_000 { RUNS_1M } else { RUNS };
        for &shape in shapes {
            let dir = base.join(format!("{lines}-{}", shape.name()));
            let project = cheby_bench_gen::generate(&Config {
                lines,
                shape,
                seed: 1,
            });
            project
                .write(&dir)
                .map_err(|e| format!("{}: {e}", dir.display()))?;
            eprintln!("bench-compile: {lines} lines, shape `{}`", shape.name());
            let check = fastest(runs, || time_cheby(&cheby, &dir, "check"))?;
            let (front_end, back_end) = fastest_pair(runs, || {
                let t = cheby_driver::bench_build(&dir).map_err(|e| e.message().to_owned())?;
                Ok((t.front_end, t.back_end))
            })?;
            let edit_to_run = if shape == Shape::Wide {
                Some(fastest(runs, || {
                    edit_one_body(&dir)?;
                    time_cheby(&cheby, &dir, "run")
                })?)
            } else {
                None
            };
            rows.push(Row {
                lines: project.lines(),
                shape,
                modules: project.modules(),
                check,
                front_end,
                back_end,
                edit_to_run,
            });
        }
    }
    let table = render(&rows);
    print!("{table}");
    if let Some(summary) = summary {
        crate::append(
            summary,
            &format!("### Compile speed (I1, D-247)\n\n{table}\n"),
        )?;
    }
    Ok(())
}

/// Runs this command again from an optimized build of xtask.
fn rerun_optimized(root: &Path, quick: bool, summary: Option<&Path>) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(root)
        .args(["run", "--quiet", "--release", "--package", "xtask", "--"])
        .arg("bench-compile")
        .env(RERUN_ENV, "1");
    if quick {
        cmd.arg("--quick");
    }
    if let Some(summary) = summary {
        cmd.arg("--summary").arg(summary);
    }
    let status = cmd.status().map_err(|e| format!("cannot run cargo: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("bench-compile failed; see the output above".to_owned())
    }
}

/// Builds the release `cheby` and returns its path.
fn build_cheby(root: &Path) -> Result<PathBuf, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let status = Command::new(cargo)
        .current_dir(root)
        .args(["build", "--quiet", "--release", "--package", "cheby_cli"])
        .status()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    if !status.success() {
        return Err("cannot build the release `cheby`".to_owned());
    }
    let name = format!("cheby{}", std::env::consts::EXE_SUFFIX);
    Ok(root.join("target/release").join(name))
}

/// Times `cheby <command>` in `dir`; it must succeed.
fn time_cheby(cheby: &Path, dir: &Path, command: &str) -> Result<Duration, String> {
    let start = Instant::now();
    let out = Command::new(cheby)
        .args(["--color=never", command])
        .current_dir(dir)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", cheby.display()))?;
    let elapsed = start.elapsed();
    if !out.status.success() {
        return Err(format!(
            "`cheby {command}` failed in {} ({})\n{}",
            dir.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(elapsed)
}

/// Changes one function body: the counter of the first `step(0, …)` call
/// in the module `main` imports, so the next run must rebuild it. Each call
/// gives the body a new value, so no two runs see the same source.
fn edit_one_body(dir: &Path) -> Result<(), String> {
    let main = std::fs::read_to_string(dir.join("src/main.cheby"))
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let import = main
        .lines()
        .find_map(|line| line.strip_prefix("import bench::"))
        .ok_or("the benchmark's main module imports no module")?;
    let file = dir
        .join("src")
        .join(format!("{}.cheby", import.replace("::", "/")));
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let marker = "  step(0, ";
    let at = text
        .find(marker)
        .ok_or_else(|| format!("{}: no function body to edit", file.display()))?;
    let start = at.saturating_add(marker.len());
    let rest = text.get(start..).unwrap_or_default();
    let digits = rest.find(')').unwrap_or(0);
    let old: u64 = rest.get(..digits).unwrap_or("0").parse().unwrap_or(0);
    let edited = format!(
        "{}{}{}",
        text.get(..start).unwrap_or_default(),
        old.saturating_add(1),
        rest.get(digits..).unwrap_or_default()
    );
    std::fs::write(&file, edited).map_err(|e| format!("{}: {e}", file.display()))
}

fn fastest(
    runs: usize,
    mut f: impl FnMut() -> Result<Duration, String>,
) -> Result<Duration, String> {
    let mut best = f()?;
    for _ in 1..runs {
        best = best.min(f()?);
    }
    Ok(best)
}

/// The run whose total is fastest.
fn fastest_pair(
    runs: usize,
    mut f: impl FnMut() -> Result<(Duration, Duration), String>,
) -> Result<(Duration, Duration), String> {
    let mut best = f()?;
    for _ in 1..runs {
        let next = f()?;
        if next.0.saturating_add(next.1) < best.0.saturating_add(best.1) {
            best = next;
        }
    }
    Ok(best)
}

/// Lines per second, rounded.
fn rate(lines: usize, time: Duration) -> String {
    let seconds = time.as_secs_f64();
    if seconds <= 0.0 {
        return "-".to_owned();
    }
    let lines = u32::try_from(lines).map_or(f64::from(u32::MAX), f64::from);
    let per_second = lines / seconds;
    format!("{:.0}k", per_second / 1000.0)
}

fn render(rows: &[Row]) -> String {
    let mut out = String::new();
    out.push_str(
        "| Project | Modules | `cheby check` | Check lines/s/core | Front end | Code generation | Check + codegen lines/s/core | Edit to run |\n\
         | ------- | ------- | ------------- | ------------------ | --------- | --------------- | ---------------------------- | ----------- |\n",
    );
    for row in rows {
        let both = row.front_end.saturating_add(row.back_end);
        let latency = row
            .edit_to_run
            .map_or_else(|| "-".to_owned(), |t| format!("{:.3} s", t.as_secs_f64()));
        let _ = writeln!(
            out,
            "| {} lines, `{}` | {} | {:.3} s | {} | {:.3} s | {:.3} s | {} | {} |",
            row.lines,
            row.shape.name(),
            row.modules,
            row.check.as_secs_f64(),
            rate(row.lines, row.check),
            row.front_end.as_secs_f64(),
            row.back_end.as_secs_f64(),
            rate(row.lines, both),
            latency,
        );
    }
    out.push_str(
        "\nOne thread (G1's parallel front end and code generation are later work). Budgets (D-247): \
         check and generate code for 100k lines/s per core; edit to run under 500 ms at 1M lines. \
         Reported, never enforced.\n",
    );
    out
}
