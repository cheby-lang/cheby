//! `cargo xtask bench-compile [--quick] [--shapes 10k,100k,1m] [--cheby
//! PATH] [--env K=V]... [--json OUT] [--summary FILE]`: compile-speed
//! numbers (I1, D-247, plan §8.5, docs/plan/testing.md §7). Reported,
//! never enforced.
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
//!
//! The build rows also give the size of what was built (D-718): the
//! machine code the JIT defined (`code KiB`) and the reference-count
//! operations of every MIR unit (`RC ops`), both from the 1-thread
//! `bench-build`, which builds the same code at any thread count.
//!
//! `--shapes` picks the project sizes (`10k`, `100k`, `1m`; `--quick` is
//! `10k`), `--cheby` measures another `cheby` binary (`bench-build` still
//! runs in this xtask, so for another commit run the command from that
//! commit's tree), `--env KEY=VALUE` sets a variable for every child
//! process, and `--json` writes the rows with the machine's metadata.
//!
//! `--shapes huge` selects the `huge_literals` shapes of `bench/gen`
//! (D-720): one literal of n computed elements per project, for each kind
//! (list, interpolation, map, tuple) at n = 25k, 50k, 100k and 200k
//! ([`HUGE_SIZES`]), so that the build times give the slope per doubling.
//! Alone it measures only those; it combines with sizes, as in
//! `--shapes 10k,huge`. They are opt-in: with no `--shapes`, or with
//! `--quick`, the default projects ([`SIZES`]) are measured exactly as
//! before, so that later rows stay comparable with D-766's (D-776). Rows of
//! 100k elements or more take one run each, and the 100k and 200k builds
//! need gigabytes of memory before D-720's splitting (D-643). JSON rows of
//! these shapes carry `elements`, the n of the literal.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use cheby_bench_gen::{Config, Shape};
use serde_json::{Value, json};

/// The command's options.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Only the 10k-line projects
    #[arg(long, conflicts_with = "shapes")]
    quick: bool,
    /// The project sizes to measure, such as `10k,100k,1m`; `huge` adds the
    /// `huge_literals` shapes at 25k to 200k elements (D-720)
    #[arg(long, value_delimiter = ',', value_parser = parse_selection)]
    shapes: Vec<Selection>,
    /// The `cheby` binary to time, instead of building one
    #[arg(long)]
    cheby: Option<PathBuf>,
    /// An environment variable for every child process (`cheby` and
    /// `bench-build`), as `KEY=VALUE`; repeatable
    #[arg(long = "env", value_parser = crate::bench_run::parse_env)]
    env: Vec<(String, String)>,
    /// Write the rows as JSON to this file
    #[arg(long)]
    json: Option<PathBuf>,
    /// Append the Markdown table to this file, such as
    /// `$GITHUB_STEP_SUMMARY`
    #[arg(long)]
    summary: Option<PathBuf>,
}

/// One entry of `--shapes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Selection {
    /// The default projects of one size of [`SIZES`].
    Lines(usize),
    /// The `huge_literals` shapes at every size of [`HUGE_SIZES`].
    Huge,
}

impl Selection {
    /// The name `--shapes` takes.
    fn name(self) -> String {
        match self {
            Self::Lines(lines) => size_name(lines),
            Self::Huge => HUGE.to_owned(),
        }
    }
}

/// The `--shapes` entry that selects the `huge_literals` shapes.
const HUGE: &str = "huge";

/// Parses a `--shapes` entry: a project size or `huge`.
fn parse_selection(text: &str) -> Result<Selection, String> {
    if text == HUGE {
        return Ok(Selection::Huge);
    }
    parse_size(text).map(Selection::Lines)
}

/// Parses a project size: `10k`, `100k` or `1m`.
fn parse_size(text: &str) -> Result<usize, String> {
    SIZES
        .iter()
        .map(|&(lines, _)| lines)
        .find(|&lines| size_name(lines) == text)
        .ok_or_else(|| format!("`{text}` is not a project size; use 10k, 100k, 1m or huge"))
}

/// `10k`, `100k` or `1m`.
fn size_name(lines: usize) -> String {
    if lines >= 1_000_000 && lines.is_multiple_of(1_000_000) {
        format!("{}m", lines / 1_000_000)
    } else {
        format!("{}k", lines / 1_000)
    }
}

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

/// The elements of each literal under `--shapes huge`, for every shape of
/// [`Shape::HUGE_LITERALS`] (D-720): each size doubles the last, so the
/// ratio of two neighbors is the slope.
const HUGE_SIZES: [usize; 4] = [25_000, 50_000, 100_000, 200_000];

/// The projects to measure, in order: the lines (or, for the
/// `huge_literals` shapes, the elements) and the shape.
fn projects(quick: bool, shapes: &[Selection]) -> Vec<(usize, Shape)> {
    let mut out = Vec::new();
    for (lines, sized) in SIZES {
        let wanted = if quick {
            lines == 10_000
        } else {
            shapes.is_empty() || shapes.contains(&Selection::Lines(lines))
        };
        if wanted {
            out.extend(sized.iter().map(|&shape| (lines, shape)));
        }
    }
    if !quick && shapes.contains(&Selection::Huge) {
        for elements in HUGE_SIZES {
            out.extend(
                Shape::HUGE_LITERALS
                    .into_iter()
                    .map(|shape| (elements, shape)),
            );
        }
    }
    out
}

/// Runs per measurement; the fastest counts.
const RUNS: usize = 3;
/// Runs per measurement on the 1M-line projects.
const RUNS_1M: usize = 1;

/// The environment variable that marks the optimized re-run.
const RERUN_ENV: &str = "CHEBY_BENCH_RERUN";

/// One row of the report.
struct Row {
    lines: usize,
    /// The literal's elements, for the `huge_literals` shapes.
    elements: Option<usize>,
    shape: Shape,
    modules: usize,
    /// `cheby check` with `--jobs 1` and with the default.
    check: [Duration; 2],
    /// `bench_build`'s front end and back end, with `--jobs 1` and with
    /// the default.
    build: [(Duration, Duration); 2],
    /// What `bench_build` built with `--jobs 1`.
    size: BuildSize,
    edit_to_run: Option<Duration>,
}

/// The size of what `bench_build` built (D-718).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BuildSize {
    /// Bytes of machine code the JIT defined.
    code_bytes: u64,
    /// `dup` and `drop` statements of every MIR unit.
    rc_ops: u64,
}

/// One `bench_build`: its times and what it built.
#[derive(Clone, Copy, Debug)]
struct BuildRun {
    front: Duration,
    back: Duration,
    size: BuildSize,
}

pub fn run(root: &Path, args: &Args) -> Result<(), String> {
    if cfg!(debug_assertions) && std::env::var_os(RERUN_ENV).is_none() {
        return rerun_optimized(root, args);
    }
    let cheby = match &args.cheby {
        Some(path) => crate::absolute(path)?,
        None => crate::build_cheby(root)?,
    };
    let env = args.env.as_slice();
    let xtask = std::env::current_exe().map_err(|e| format!("cannot find xtask itself: {e}"))?;
    let base = root.join("target/bench-compile");
    if base.exists() {
        std::fs::remove_dir_all(&base).map_err(|e| format!("{}: {e}", base.display()))?;
    }
    let load_before = crate::bench_run::load_average();
    let mut rows = Vec::new();
    for (lines, shape) in projects(args.quick, &args.shapes) {
        let huge = shape.is_huge_literal();
        let runs = if lines >= 1_000_000 || (huge && lines >= 100_000) {
            RUNS_1M
        } else {
            RUNS
        };
        let dir = base.join(format!("{lines}-{}", shape.name()));
        let project = cheby_bench_gen::generate(&Config {
            lines,
            shape,
            seed: 1,
        });
        project
            .write(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        let unit = if huge { "elements" } else { "lines" };
        eprintln!("bench-compile: {lines} {unit}, shape `{}`", shape.name());
        let mut check = [Duration::ZERO; 2];
        let mut build = [(Duration::ZERO, Duration::ZERO); 2];
        let mut size = BuildSize::default();
        for (n, jobs) in [Some(1), None].into_iter().enumerate() {
            let time = fastest(runs, || time_cheby(&cheby, &dir, jobs, "check", env))?;
            let best = fastest_build(runs, || bench_build(&xtask, &dir, jobs, env))?;
            if jobs == Some(1) {
                size = best.size;
            }
            if let (Some(c), Some(b)) = (check.get_mut(n), build.get_mut(n)) {
                *c = time;
                *b = (best.front, best.back);
            }
        }
        let edit_to_run = if shape == Shape::Wide {
            Some(fastest(runs, || {
                edit_one_body(&dir)?;
                time_cheby(&cheby, &dir, None, "run", env)
            })?)
        } else {
            None
        };
        rows.push(Row {
            lines: project.lines(),
            elements: huge.then_some(lines),
            shape,
            modules: project.modules(),
            check,
            build,
            size,
            edit_to_run,
        });
    }
    let table = render(&rows);
    print!("{table}");
    if let Some(summary) = &args.summary {
        crate::append(
            summary,
            &format!("### Compile speed (I1, D-247)\n\n{table}\n"),
        )?;
    }
    if let Some(out) = &args.json {
        let meta = crate::bench_run::Meta {
            commit: if args.cheby.is_some() {
                "unknown".to_owned()
            } else {
                crate::bench_run::head_commit(root)
            },
            cheby: crate::bench_run::relative_to(root, &cheby),
            env: args.env.clone(),
            threads: None,
            runs: RUNS,
            load_before,
        };
        write_json(root, out, &meta, &rows)?;
    }
    Ok(())
}

/// Writes the rows with the metadata to `out`. `xtask_commit` names the
/// tree that ran `bench-build`, which `--cheby` does not replace.
fn write_json(
    root: &Path,
    out: &Path,
    meta: &crate::bench_run::Meta,
    rows: &[Row],
) -> Result<(), String> {
    let value = json!({
        "meta": meta.to_json(),
        "xtask_commit": crate::bench_run::head_commit(root),
        "rows": rows.iter().map(Row::to_json).collect::<Vec<_>>(),
    });
    let text = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    std::fs::write(out, format!("{text}\n")).map_err(|e| format!("{}: {e}", out.display()))?;
    eprintln!("bench-compile: wrote {}", out.display());
    Ok(())
}

impl Row {
    fn to_json(&self) -> Value {
        let nanos = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        let [check_one, check_many] = self.check;
        let [(front_one, back_one), (front_many, back_many)] = self.build;
        let mut row = json!({
            "lines": self.lines,
            "shape": self.shape.name(),
            "modules": self.modules,
            "check_ns": [nanos(check_one), nanos(check_many)],
            "front_end_ns": [nanos(front_one), nanos(front_many)],
            "back_end_ns": [nanos(back_one), nanos(back_many)],
            "code_bytes": self.size.code_bytes,
            "rc_ops": self.size.rc_ops,
            "edit_to_run_ns": self.edit_to_run.map(nanos),
        });
        // Only the `huge_literals` rows: the default rows keep their keys.
        if let (Some(elements), Some(fields)) = (self.elements, row.as_object_mut()) {
            fields.insert("elements".to_owned(), json!(elements));
        }
        row
    }

    /// The first column: the lines and the shape, or the elements of a
    /// `huge_literals` shape.
    fn label(&self) -> String {
        match self.elements {
            Some(elements) => format!("{elements} elements, `{}`", self.shape.name()),
            None => format!("{} lines, `{}`", self.lines, self.shape.name()),
        }
    }
}

/// Runs this command again from an optimized build of xtask, with the
/// same options.
fn rerun_optimized(root: &Path, args: &Args) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(root)
        .args(["run", "--quiet", "--release", "--package", "xtask", "--"])
        .arg("bench-compile")
        .env(RERUN_ENV, "1");
    if args.quick {
        cmd.arg("--quick");
    }
    if !args.shapes.is_empty() {
        let names: Vec<String> = args.shapes.iter().map(|s| s.name()).collect();
        cmd.arg("--shapes").arg(names.join(","));
    }
    // Paths are made absolute: the re-run starts in `root`.
    if let Some(cheby) = &args.cheby {
        cmd.arg("--cheby").arg(crate::absolute(cheby)?);
    }
    for (key, value) in &args.env {
        cmd.arg("--env").arg(format!("{key}={value}"));
    }
    if let Some(json) = &args.json {
        cmd.arg("--json").arg(crate::absolute(json)?);
    }
    if let Some(summary) = &args.summary {
        cmd.arg("--summary").arg(crate::absolute(summary)?);
    }
    let status = cmd.status().map_err(|e| format!("cannot run cargo: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("bench-compile failed; see the output above".to_owned())
    }
}

/// Times `cheby [--jobs N] <command>` in `dir` with `env` set; it must
/// succeed.
fn time_cheby(
    cheby: &Path,
    dir: &Path,
    jobs: Option<usize>,
    command: &str,
    env: &[(String, String)],
) -> Result<Duration, String> {
    let mut cmd = Command::new(cheby);
    if let Some(n) = jobs {
        cmd.args(["--jobs", &n.to_string()]);
    }
    for (key, value) in env {
        cmd.env(key, value);
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
/// [--jobs N] <dir>`, which builds in a process of its own with `env`
/// set, and the size of what it built.
fn bench_build(
    xtask: &Path,
    dir: &Path,
    jobs: Option<usize>,
    env: &[(String, String)],
) -> Result<BuildRun, String> {
    let mut cmd = Command::new(xtask);
    cmd.arg("bench-build");
    if let Some(n) = jobs {
        cmd.args(["--jobs", &n.to_string()]);
    }
    for (key, value) in env {
        cmd.env(key, value);
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
    let numbers: Vec<Option<u64>> = stdout
        .split_whitespace()
        .map(|word| word.parse::<u64>().ok())
        .collect();
    match numbers.as_slice() {
        [Some(front), Some(back), Some(code_bytes), Some(rc_ops)] => Ok(BuildRun {
            front: Duration::from_nanos(*front),
            back: Duration::from_nanos(*back),
            size: BuildSize {
                code_bytes: *code_bytes,
                rc_ops: *rc_ops,
            },
        }),
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

/// The build whose total time is the fastest. Every run builds the same
/// code, so a size that changes between runs is an error.
fn fastest_build(
    runs: usize,
    mut f: impl FnMut() -> Result<BuildRun, String>,
) -> Result<BuildRun, String> {
    let mut best = f()?;
    for _ in 1..runs {
        let next = f()?;
        if next.size != best.size {
            return Err(format!(
                "two builds of one project differ: {:?} and {:?}",
                best.size, next.size
            ));
        }
        if next.front.saturating_add(next.back) < best.front.saturating_add(best.back) {
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
         Check + codegen lines/s/core | Check + codegen speedup | code KiB | RC ops | Edit to run |"
    );
    out.push_str(
        "| ------- | ------- | --------------- | ------------------ | ------------- | ------------- | \
         ------------------- | ------------------------- | ---------------------------- | \
         ----------------------- | -------- | ------ | ----------- |\n",
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
            "| {} | {} | {} | {} | {} | {} | {} / {} | {} / {} | {} | {} | {} | {} | {} |",
            row.label(),
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
            kib(row.size.code_bytes),
            row.size.rc_ops,
            latency,
        );
    }
    let _ = write!(
        out,
        "\nRates are per core, from the 1-thread runs; the speedups compare them with {threads} \
         threads (`--jobs` default, the available parallelism). The front end, MIR lowering and \
         code generation run modules and functions in parallel (G1). Edit to run \
         uses the default threads. Code KiB is the machine code the JIT defined and RC ops the \
         `dup` and `drop` statements of the MIR, for the whole build (D-718). Budgets (D-247): \
         check and generate code for 100k lines/s per core and scale with cores; edit to run \
         under 500 ms at 1M lines. Reported, never enforced.\n"
    );
    out
}

/// Bytes as KiB with one decimal.
fn kib(bytes: u64) -> String {
    let bytes = u32::try_from(bytes).map_or(f64::from(u32::MAX), f64::from);
    format!("{:.1}", bytes / 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_by_name() {
        assert_eq!(parse_size("10k"), Ok(10_000));
        assert_eq!(parse_size("100k"), Ok(100_000));
        assert_eq!(parse_size("1m"), Ok(1_000_000));
        assert!(parse_size("2m").is_err());
        for (lines, _) in SIZES {
            assert_eq!(parse_size(&size_name(lines)), Ok(lines));
        }
    }

    #[test]
    fn selections_parse_by_name() {
        assert_eq!(parse_selection("huge"), Ok(Selection::Huge));
        assert_eq!(parse_selection("100k"), Ok(Selection::Lines(100_000)));
        assert!(parse_selection("huge_list").is_err());
        assert!(parse_selection("25k").is_err());
        for selection in [Selection::Huge, Selection::Lines(1_000_000)] {
            assert_eq!(parse_selection(&selection.name()), Ok(selection));
        }
    }

    /// `SIZES` is what D-766's and C1's rows measured (D-776), and the
    /// `huge_literals` shapes come only with `--shapes huge`, at 25k, 50k,
    /// 100k and 200k elements (D-720).
    #[test]
    fn huge_literals_are_opt_in() {
        let names = |shapes: &[Shape]| shapes.iter().map(|s| s.name()).collect::<Vec<_>>();
        let sizes: Vec<(usize, Vec<&str>)> = SIZES
            .iter()
            .map(|&(lines, shapes)| (lines, names(shapes)))
            .collect();
        assert_eq!(
            sizes,
            [
                (10_000, vec!["small", "deep", "wide", "generic"]),
                (100_000, vec!["small", "large", "wide"]),
                (1_000_000, vec!["large", "wide"]),
            ]
        );

        let every_size: Vec<(usize, Shape)> = SIZES
            .iter()
            .flat_map(|&(lines, shapes)| shapes.iter().map(move |&shape| (lines, shape)))
            .collect();
        assert_eq!(projects(false, &[]), every_size);
        let quick: Vec<(usize, Shape)> = every_size
            .iter()
            .copied()
            .filter(|&(lines, _)| lines == 10_000)
            .collect();
        assert_eq!(projects(true, &[]), quick);

        let huge = projects(false, &[Selection::Huge]);
        assert_eq!(huge.len(), 16);
        for elements in [25_000, 50_000, 100_000, 200_000] {
            let kinds: Vec<Shape> = huge
                .iter()
                .filter(|&&(n, _)| n == elements)
                .map(|&(_, shape)| shape)
                .collect();
            assert_eq!(kinds, Shape::HUGE_LITERALS, "{elements}");
        }

        let both = projects(false, &[Selection::Huge, Selection::Lines(10_000)]);
        let (first, rest) = both.split_at(quick.len());
        assert_eq!(first, quick.as_slice());
        assert_eq!(rest, huge.as_slice());
    }

    #[test]
    fn kib_has_one_decimal() {
        assert_eq!(kib(0), "0.0");
        assert_eq!(kib(1536), "1.5");
    }
}
