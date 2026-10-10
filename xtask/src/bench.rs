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
//! Checking and checking with code generation run twice: with `--jobs 1`,
//! which gives the lines per second per core, and with the default
//! number of threads (the available parallelism), which gives the scaling
//! with cores (D-247, D-263). `bench_build` runs in a child process
//! (`xtask bench-build`), since rayon's global pool is sized once per
//! process.
//!
//! The numbers come from an optimized toolchain: the command re-runs
//! itself with `--release` when it was built without optimizations, and
//! runs the release `cheby`. Each measurement is the fastest of a few
//! runs.

use std::fmt::Write as _;
use std::path::Path;
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
    /// `cheby check` with `--jobs 1` and with the default.
    check: [Duration; 2],
    /// `bench_build`'s front end and back end, with `--jobs 1` and with
    /// the default.
    build: [(Duration, Duration); 2],
    edit_to_run: Option<Duration>,
}

pub fn run(root: &Path, quick: bool, summary: Option<&Path>) -> Result<(), String> {
    if cfg!(debug_assertions) && std::env::var_os(RERUN_ENV).is_none() {
        return rerun_optimized(root, quick, summary);
    }
    let cheby = crate::build_cheby(root)?;
    let xtask = std::env::current_exe().map_err(|e| format!("cannot find xtask itself: {e}"))?;
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
            let mut check = [Duration::ZERO; 2];
            let mut build = [(Duration::ZERO, Duration::ZERO); 2];
            for (n, jobs) in [Some(1), None].into_iter().enumerate() {
                let time = fastest(runs, || time_cheby(&cheby, &dir, jobs, "check"))?;
                let pair = fastest_pair(runs, || bench_build(&xtask, &dir, jobs))?;
                if let (Some(c), Some(b)) = (check.get_mut(n), build.get_mut(n)) {
                    *c = time;
                    *b = pair;
                }
            }
            let edit_to_run = if shape == Shape::Wide {
                Some(fastest(runs, || {
                    edit_one_body(&dir)?;
                    time_cheby(&cheby, &dir, None, "run")
                })?)
            } else {
                None
            };
            rows.push(Row {
                lines: project.lines(),
                shape,
                modules: project.modules(),
                check,
                build,
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

/// Times `cheby [--jobs N] <command>` in `dir`; it must succeed.
fn time_cheby(
    cheby: &Path,
    dir: &Path,
    jobs: Option<usize>,
    command: &str,
) -> Result<Duration, String> {
    let mut cmd = Command::new(cheby);
    if let Some(n) = jobs {
        cmd.args(["--jobs", &n.to_string()]);
    }
    let start = Instant::now();
    let out = cmd
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

/// The front end's and the back end's times of `xtask bench-build
/// [--jobs N] <dir>`, which builds in a process of its own.
fn bench_build(
    xtask: &Path,
    dir: &Path,
    jobs: Option<usize>,
) -> Result<(Duration, Duration), String> {
    let mut cmd = Command::new(xtask);
    cmd.arg("bench-build");
    if let Some(n) = jobs {
        cmd.args(["--jobs", &n.to_string()]);
    }
    let out = cmd
        .arg(dir)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", xtask.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        return Err(format!(
            "`xtask bench-build` failed in {} ({})\n{}",
            dir.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let mut times = stdout.split_whitespace().map(str::parse::<u64>);
    match (times.next(), times.next()) {
        (Some(Ok(front)), Some(Ok(back))) => {
            Ok((Duration::from_nanos(front), Duration::from_nanos(back)))
        }
        _ => Err(format!("`xtask bench-build` printed `{}`", stdout.trim())),
    }
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

/// How many times faster the second time is than the first, as `2.4x`.
fn speedup(one: Duration, many: Duration) -> String {
    let many = many.as_secs_f64();
    if many <= 0.0 {
        return "-".to_owned();
    }
    format!("{:.1}x", one.as_secs_f64() / many)
}

fn render(rows: &[Row]) -> String {
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "| Project | Modules | Check, 1 thread | Check lines/s/core | Check, {threads} threads | Check speedup | \
         Front end, 1 / {threads} threads | Code generation, 1 / {threads} threads | \
         Check + codegen lines/s/core | Check + codegen speedup | Edit to run |"
    );
    out.push_str(
        "| ------- | ------- | --------------- | ------------------ | ------------- | ------------- | \
         ------------------- | ------------------------- | ---------------------------- | \
         ----------------------- | ----------- |\n",
    );
    let secs = |d: Duration| format!("{:.3} s", d.as_secs_f64());
    for row in rows {
        let [check_one, check_many] = row.check;
        let [(front_one, back_one), (front_many, back_many)] = row.build;
        let both_one = front_one.saturating_add(back_one);
        let both_many = front_many.saturating_add(back_many);
        let latency = row.edit_to_run.map_or_else(|| "-".to_owned(), secs);
        let _ = writeln!(
            out,
            "| {} lines, `{}` | {} | {} | {} | {} | {} | {} / {} | {} / {} | {} | {} | {} |",
            row.lines,
            row.shape.name(),
            row.modules,
            secs(check_one),
            rate(row.lines, check_one),
            secs(check_many),
            speedup(check_one, check_many),
            secs(front_one),
            secs(front_many),
            secs(back_one),
            secs(back_many),
            rate(row.lines, both_one),
            speedup(both_one, both_many),
            latency,
        );
    }
    let _ = write!(
        out,
        "\nRates are per core, from the 1-thread runs; the speedups compare them with {threads} \
         threads (`--jobs` default, the available parallelism). The front end runs modules and \
         function bodies in parallel (G1); code generation is one batch until W5-C. Edit to run \
         uses the default threads. Budgets (D-247): check and generate code for 100k lines/s per \
         core and scale with cores; edit to run under 500 ms at 1M lines. Reported, never \
         enforced.\n"
    );
    out
}
