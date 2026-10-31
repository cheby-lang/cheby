//! `cargo xtask bench-compile [--quick] [--shapes 10k,100k,1m] [--cheby
//! PATH] [--env K=V]... [--json OUT] [--summary FILE]`: compile-speed
//! numbers (I1, D-247, plan §8.5, docs/plan/testing.md §7). Reported,
//! never enforced. `--oqp11` and `--restarts` (with `--lines N`, `--rounds
//! N` and `--from DIR`) measure OQ-P11's edit-to-run rounds and RB-3's
//! restart causes instead (`bench_oqp11.rs`; D-974, D-1256).
//!
//! Every row also has the cold columns (D-975, D-1266,
//! GAP(cold-columns)): a cold `cheby check` and a cold `cheby build -o
//! <temporary directory>/bench` (not for the `huge_literals` shapes), each
//! the fastest of the row's runs on an empty cache of its own, so std's
//! first use is inside them, and the same two on a cache that `cheby
//! check` of `small-10000` (seed 2) filled with std's records first, so
//! std's first use reads apart ([`cold_columns`]).
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
//! - std's first use (D-980, K6): the median over [`FIRST_USE_ROUNDS`]
//!   rounds of `cheby run` of `bench/gen`'s `small` project of 10k lines
//!   (seed 1) on an empty cache, minus the same `cheby run` on a cache that
//!   `cheby run` of the project generated with seed 2 filled first, so
//!   std's records are in it and the seed-1 package's are not
//!   ([`std_first_use`]). Each measurement has a fresh cache of its own
//!   (`cache.rs`).
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
    /// Only OQ-P11's rounds (D-974): `wide` and `deep` at `--lines`, a cold
    /// run, then `--rounds` body edits and `main` edits, under
    /// `CHEBY_PHASES`
    #[arg(long, conflicts_with_all = ["quick", "shapes"])]
    oqp11: bool,
    /// RB-3's restart causes (D-1256), at 100k lines and at `--lines`;
    /// with `--oqp11` or alone
    #[arg(long, conflicts_with_all = ["quick", "shapes"])]
    restarts: bool,
    /// The lines of `--oqp11`'s and `--restarts`'s projects
    #[arg(long, default_value_t = 1_000_000)]
    lines: usize,
    /// The rounds of each edit kind (`--restarts`: at most 3 at 1M)
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u16).range(1..))]
    rounds: u16,
    /// Copy `DIR/<shape>-<lines>` instead of generating the projects of
    /// `--oqp11` and `--restarts`
    #[arg(long, value_name = "DIR")]
    from: Option<PathBuf>,
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
    /// The cold `check` and `build` columns (D-975, D-1266).
    cold: Cold,
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
    let meta = |runs: usize| meta_of(root, args, &cheby, runs, load_before);
    if args.oqp11 || args.restarts {
        return run_oqp11(root, args, &cheby, &base, &meta(usize::from(args.rounds)));
    }
    let fill = std_fill_project(&base)?;
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
        let cold = cold_columns(&cheby, &dir, &fill, runs, env, !huge)?;
        rows.push(Row {
            lines: project.lines(),
            elements: huge.then_some(lines),
            shape,
            modules: project.modules(),
            check,
            build,
            size,
            edit_to_run,
            cold,
        });
    }
    eprintln!("bench-compile: std's first use, {FIRST_USE_ROUNDS} rounds");
    let first_use = std_first_use(&cheby, &base, env)?;
    let table = format!(
        "{}{}{}",
        render(&rows),
        render_cold(&rows),
        render_first_use(first_use)
    );
    print!("{table}");
    if let Some(summary) = &args.summary {
        crate::append(
            summary,
            &format!("### Compile speed (I1, D-247)\n\n{table}\n"),
        )?;
    }
    if let Some(out) = &args.json {
        write_json(root, out, &meta(RUNS), &rows, first_use)?;
    }
    Ok(())
}

/// The JSON's machine and run metadata.
fn meta_of(
    root: &Path,
    args: &Args,
    cheby: &Path,
    runs: usize,
    load_before: Option<[f64; 3]>,
) -> crate::bench_run::Meta {
    crate::bench_run::Meta {
        commit: if args.cheby.is_some() {
            "unknown".to_owned()
        } else {
            crate::bench_run::head_commit(root)
        },
        cheby: crate::bench_run::relative_to(root, cheby),
        env: args.env.clone(),
        threads: None,
        runs,
        load_before,
    }
}

/// `--oqp11` and `--restarts` (`bench_oqp11.rs`): their rounds instead of
/// the rows, and the JSON's `oqp11` and `restarts` objects beside empty
/// `rows` (plan §2.1).
fn run_oqp11(
    root: &Path,
    args: &Args,
    cheby: &Path,
    base: &Path,
    meta: &crate::bench_run::Meta,
) -> Result<(), String> {
    let from = args.from.as_deref().map(crate::absolute).transpose()?;
    let cx = crate::bench_oqp11::Cx {
        cheby,
        env: &args.env,
        work: base.to_path_buf(),
        lines: args.lines,
        rounds: usize::from(args.rounds),
        from: from.as_deref(),
    };
    let oqp11 = if args.oqp11 {
        Some(crate::bench_oqp11::oqp11(&cx)?)
    } else {
        None
    };
    let restarts = if args.restarts {
        Some(crate::bench_oqp11::restarts(&cx)?)
    } else {
        None
    };
    let table = crate::bench_oqp11::render(oqp11.as_ref(), restarts.as_ref());
    print!("{table}");
    if let Some(summary) = &args.summary {
        crate::append(
            summary,
            &format!("### OQ-P11 and RB-3's restarts (D-974, D-1256)\n{table}\n"),
        )?;
    }
    if let Some(out) = &args.json {
        let mut value = json!({
            "meta": meta.to_json(),
            "xtask_commit": crate::bench_run::head_commit(root),
            "rows": [],
        });
        if let Some(fields) = value.as_object_mut() {
            if let Some(oqp11) = oqp11 {
                fields.insert("oqp11".to_owned(), oqp11);
            }
            if let Some(restarts) = restarts {
                fields.insert("restarts".to_owned(), restarts);
            }
        }
        let text = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
        std::fs::write(out, format!("{text}\n")).map_err(|e| format!("{}: {e}", out.display()))?;
        eprintln!("bench-compile: wrote {}", out.display());
    }
    Ok(())
}

/// The project whose `cheby check` fills a cache with std's records before
/// a cold column's "std's records" run (`small-10000`, seed 2, as
/// [`std_first_use`]'s): any command's std miss is std's first use
/// (D-1266), so the cache holds every std record and none of the measured
/// package's.
fn std_fill_project(base: &Path) -> Result<PathBuf, String> {
    let dir = base.join("cold-fill-small-10000-seed-2");
    cheby_bench_gen::generate(&Config {
        lines: 10_000,
        shape: Shape::Small,
        seed: 2,
    })
    .write(&dir)
    .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// The cold columns of a row (D-975, D-1266, GAP(cold-columns)).
#[derive(Clone, Copy, Debug, Default)]
struct Cold {
    /// `cheby check` on an empty cache: std's first use inside it.
    check: Duration,
    /// `cheby build -o` on an empty cache; `None` for the shapes not
    /// built (`huge_literals`).
    build: Option<Duration>,
    /// `cheby check` on a cache that std's records already fill.
    check_std_warm: Duration,
    /// `cheby build -o` on a cache that std's records already fill.
    build_std_warm: Option<Duration>,
}

/// The cold columns of the project in `dir` (D-975, D-1266,
/// GAP(cold-columns)): a cold `cheby check` and a cold `cheby build -o
/// <temporary directory>/bench` (when `build`), each the fastest of `runs`
/// runs on an empty cache of its own, so std's first use is inside them;
/// then the same two on a cache that `cheby check` of `fill` filled with
/// std's records first, so std's first use reads apart.
fn cold_columns(
    cheby: &Path,
    dir: &Path,
    fill: &Path,
    runs: usize,
    env: &[(String, String)],
    build: bool,
) -> Result<Cold, String> {
    let out = dir.with_extension("build");
    let exe = out.join("bench");
    let exe = exe
        .to_str()
        .ok_or_else(|| format!("{}: not UTF-8", exe.display()))?
        .to_owned();
    let check_args = ["check"];
    let build_args = ["build", "-o", exe.as_str()];
    let time = |args: &[&str], std_warm: bool| {
        fastest(runs, || {
            let cache = crate::cache::fresh("cold")?;
            if std_warm {
                time_on(cheby, fill, cache.path(), &check_args, env)?;
            }
            time_on(cheby, dir, cache.path(), args, env)
        })
    };
    let cold = Cold {
        check: time(&check_args, false)?,
        build: if build {
            Some(time(&build_args, false)?)
        } else {
            None
        },
        check_std_warm: time(&check_args, true)?,
        build_std_warm: if build {
            Some(time(&build_args, true)?)
        } else {
            None
        },
    };
    let _ = std::fs::remove_dir_all(&out);
    Ok(cold)
}

/// Times `cheby <args>` in `dir` on the cache `cache` with `env` set; it
/// must succeed. `CHEBY_CACHE_LOG` and `CHEBY_PHASES` are never set.
fn time_on(
    cheby: &Path,
    dir: &Path,
    cache: &Path,
    args: &[&str],
    env: &[(String, String)],
) -> Result<Duration, String> {
    let mut cmd = Command::new(cheby);
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.env("CHEBY_CACHE", cache)
        .env_remove("CHEBY_CACHE_LOG")
        .env_remove("CHEBY_PHASES");
    let start = Instant::now();
    let out = cmd
        .arg("--color=never")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", cheby.display()))?;
    let elapsed = start.elapsed();
    if !out.status.success() {
        return Err(format!(
            "`cheby {}` failed in {} ({})\n{}",
            args.join(" "),
            dir.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(elapsed)
}

/// The cold columns' table (D-975, D-1266).
fn render_cold(rows: &[Row]) -> String {
    let secs = |d: Option<Duration>| {
        d.map_or_else(|| "-".to_owned(), |d| format!("{:.3} s", d.as_secs_f64()))
    };
    let mut out = String::from(
        "\n| Project | Cold check | Cold check, std's records | Cold build | Cold build, std's records |\n\
         | ------- | ---------- | ------------------------- | ---------- | ------------------------- |\n",
    );
    for row in rows {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} |",
            row.label(),
            secs(Some(row.cold.check)),
            secs(Some(row.cold.check_std_warm)),
            secs(row.cold.build),
            secs(row.cold.build_std_warm),
        );
    }
    out.push_str(
        "\nCold columns (D-975, D-1266): `cheby check` and `cheby build -o` on an empty cache, \
         std's first use inside them, and on a cache that std's records already fill; each the \
         fastest of the row's runs, every run on a fresh cache. Reported, never enforced.\n",
    );
    out
}

/// The rounds of "std first use" ([`std_first_use`]).
const FIRST_USE_ROUNDS: usize = 5;

/// D-980's first-use cost ("about 0.15 s once per machine"; K6): per
/// round, `cheby run` of `small-10000` (seed 1) on an empty cache, and the
/// same run on a fresh cache that `cheby run` of `small-10000` generated
/// with seed 2 filled first: std's records are in it, the seed-1 package's
/// are not, so the difference is what std's first use costs. The median
/// over the rounds of the differences, in nanoseconds (negative when the
/// noise exceeds the cost). Each run logs `CHEBY_CACHE_LOG`, and a round
/// fails when the seed-2 run did not look up every std code record the
/// seed-1 run on the empty cache looked up ([`check_first_use`]).
fn std_first_use(cheby: &Path, base: &Path, env: &[(String, String)]) -> Result<i128, String> {
    let mut dirs = Vec::new();
    for seed in [1, 2] {
        let dir = base.join(format!("first-use-small-10000-seed-{seed}"));
        cheby_bench_gen::generate(&Config {
            lines: 10_000,
            shape: Shape::Small,
            seed,
        })
        .write(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
        dirs.push(dir);
    }
    let [seed1, seed2] = dirs.as_slice() else {
        return Err("std first use: two projects".to_owned());
    };
    let mut diffs = Vec::with_capacity(FIRST_USE_ROUNDS);
    for round in 0..FIRST_USE_ROUNDS {
        let log = |name: &str| base.join(format!("first-use-{round}-{name}.log"));
        let empty = crate::cache::fresh("first-use-empty")?;
        let cold = time_logged(cheby, seed1, empty.path(), &log("cold"), env)?;
        let filled = crate::cache::fresh("first-use-filled")?;
        time_logged(cheby, seed2, filled.path(), &log("fill"), env)?;
        let warm = time_logged(cheby, seed1, filled.path(), &log("warm"), env)?;
        let read = |name: &str| {
            std::fs::read_to_string(log(name)).map_err(|e| format!("std first use: {e}"))
        };
        check_first_use(&read("cold")?, &read("fill")?)?;
        let nanos = |d: Duration| i128::try_from(d.as_nanos()).unwrap_or(i128::MAX);
        diffs.push(nanos(cold).saturating_sub(nanos(warm)));
    }
    median(&mut diffs).ok_or_else(|| "std first use: no round".to_owned())
}

/// The middle of `values` once sorted (the upper middle of an even
/// count), or `None` when there is none.
fn median(values: &mut [i128]) -> Option<i128> {
    values.sort_unstable();
    values.get(values.len() / 2).copied()
}

/// The `code.std` modules of a `CHEBY_CACHE_LOG` text.
fn std_code_modules(log: &str) -> std::collections::BTreeSet<&str> {
    log.lines()
        .filter_map(|line| line.strip_prefix("code.std "))
        .filter_map(|rest| rest.split_whitespace().next())
        .collect()
}

/// Fails when the seed-1 run on the empty cache (`cold`) logged no
/// `code.std` line (no std lookup to measure), or when the seed-2 run that
/// filled the other cache (`fill`) lacks a `code.std` line of a module the
/// seed-1 run has: then the filled cache need not hold that module's
/// record, and the difference would not be std's first use.
fn check_first_use(cold: &str, fill: &str) -> Result<(), String> {
    let wanted = std_code_modules(cold);
    if wanted.is_empty() {
        return Err(
            "std first use: the run on the empty cache logged no `code.std` line".to_owned(),
        );
    }
    let filled = std_code_modules(fill);
    let missing: Vec<&str> = wanted.difference(&filled).copied().collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "std first use: the seed-2 run logged no `code.std` line of {}",
            missing.join(", ")
        ))
    }
}

/// Times `cheby run` in `dir` on the cache `cache`, its lookups logged to
/// `log` (`CHEBY_CACHE_LOG`), with `env` set; it must succeed.
fn time_logged(
    cheby: &Path,
    dir: &Path,
    cache: &Path,
    log: &Path,
    env: &[(String, String)],
) -> Result<Duration, String> {
    let mut cmd = Command::new(cheby);
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.env("CHEBY_CACHE", cache).env("CHEBY_CACHE_LOG", log);
    let start = Instant::now();
    let out = cmd
        .args(["--color=never", "run"])
        .current_dir(dir)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", cheby.display()))?;
    let elapsed = start.elapsed();
    if !out.status.success() {
        return Err(format!(
            "`cheby run` failed in {} ({})\n{}",
            dir.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(elapsed)
}

/// The "std first use" table: one column (D-980, K6).
fn render_first_use(nanos: i128) -> String {
    // Milliseconds fit an `i32` for any run this measures; a larger
    // difference shows as the bound.
    let millis = i32::try_from(nanos / 1_000_000).unwrap_or(i32::MAX);
    let seconds = f64::from(millis) / 1000.0;
    format!(
        "\n| Std first use |\n| ------------- |\n| {seconds:.3} s |\n\nStd first use: the median over \
         {FIRST_USE_ROUNDS} rounds of `cheby run` of `small-10000` (seed 1) on an empty cache, minus \
         the same run on a cache that `cheby run` of `small-10000` generated with seed 2 filled \
         first: std's records are in it, the seed-1 package's are not (D-980).\n"
    )
}

/// Writes the rows with the metadata to `out`. `xtask_commit` names the
/// tree that ran `bench-build`, which `--cheby` does not replace.
fn write_json(
    root: &Path,
    out: &Path,
    meta: &crate::bench_run::Meta,
    rows: &[Row],
    first_use: i128,
) -> Result<(), String> {
    let value = json!({
        "meta": meta.to_json(),
        "xtask_commit": crate::bench_run::head_commit(root),
        "rows": rows.iter().map(Row::to_json).collect::<Vec<_>>(),
        "std_first_use_ns": i64::try_from(first_use).unwrap_or(i64::MAX),
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
            "cold_check_ns": nanos(self.cold.check),
            "cold_build_ns": self.cold.build.map(nanos),
            "cold_check_std_warm_ns": nanos(self.cold.check_std_warm),
            "cold_build_std_warm_ns": self.cold.build_std_warm.map(nanos),
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
        .args(rerun_args(args)?)
        .env(RERUN_ENV, "1");
    let status = cmd.status().map_err(|e| format!("cannot run cargo: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("bench-compile failed; see the output above".to_owned())
    }
}

/// `bench-compile`'s options for the optimized re-run: every one given,
/// paths made absolute, since the re-run starts in the repository's root
/// (`bench.yml` runs the debug `cargo xtask` alias, K7).
fn rerun_args(args: &Args) -> Result<Vec<std::ffi::OsString>, String> {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    if args.quick {
        out.push("--quick".into());
    }
    if !args.shapes.is_empty() {
        let names: Vec<String> = args.shapes.iter().map(|s| s.name()).collect();
        out.push("--shapes".into());
        out.push(names.join(",").into());
    }
    if let Some(cheby) = &args.cheby {
        out.push("--cheby".into());
        out.push(crate::absolute(cheby)?.into());
    }
    for (key, value) in &args.env {
        out.push("--env".into());
        out.push(format!("{key}={value}").into());
    }
    if let Some(json) = &args.json {
        out.push("--json".into());
        out.push(crate::absolute(json)?.into());
    }
    if let Some(summary) = &args.summary {
        out.push("--summary".into());
        out.push(crate::absolute(summary)?.into());
    }
    if args.oqp11 {
        out.push("--oqp11".into());
    }
    if args.restarts {
        out.push("--restarts".into());
    }
    out.push("--lines".into());
    out.push(args.lines.to_string().into());
    out.push("--rounds".into());
    out.push(args.rounds.to_string().into());
    if let Some(from) = &args.from {
        out.push("--from".into());
        out.push(crate::absolute(from)?.into());
    }
    Ok(out)
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
    crate::cache::apply(&mut cmd)?;
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
pub(crate) fn edit_one_body(dir: &Path) -> Result<(), String> {
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

    /// `bench-compile`'s options as `cargo xtask bench-compile` parses
    /// them.
    fn parse_args(argv: &[&str]) -> Result<Args, String> {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: Args,
        }
        <Cli as clap::Parser>::try_parse_from(
            std::iter::once("bench-compile").chain(argv.iter().copied()),
        )
        .map(|cli| cli.args)
        .map_err(|e| e.to_string())
    }

    /// K7: `bench.yml` runs the debug `cargo xtask` alias, which re-runs
    /// itself optimized: every new flag reaches the re-run, `--from` made
    /// absolute, and the re-run parses what it is given.
    #[test]
    fn oqp11_rerun_forwards_flags() {
        let args = parse_args(&[
            "--cheby",
            "target/release/cheby",
            "--oqp11",
            "--restarts",
            "--lines",
            "10000",
            "--rounds",
            "1",
            "--from",
            "gen",
            "--json",
            "out.json",
        ])
        .unwrap_or_else(|e| panic!("{e}"));
        let forwarded = rerun_args(&args).unwrap_or_else(|e| panic!("{e}"));
        let text: Vec<String> = forwarded
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        for flag in ["--oqp11", "--restarts"] {
            assert!(text.iter().any(|a| a == flag), "{text:?}");
        }
        let after = |flag: &str| {
            text.iter()
                .position(|a| a == flag)
                .and_then(|i| text.get(i.saturating_add(1)))
                .cloned()
                .unwrap_or_default()
        };
        assert_eq!(after("--lines"), "10000");
        assert_eq!(after("--rounds"), "1");
        for flag in ["--from", "--cheby", "--json"] {
            assert!(Path::new(&after(flag)).is_absolute(), "{flag}: {text:?}");
        }
        assert!(after("--from").ends_with("gen"));
        let again: Vec<&str> = text.iter().map(String::as_str).collect();
        let reparsed = parse_args(&again).unwrap_or_else(|e| panic!("{e}"));
        assert!(reparsed.oqp11 && reparsed.restarts);
        assert_eq!((reparsed.lines, reparsed.rounds), (10_000, 1));
        assert_eq!(reparsed.from.as_deref(), Some(Path::new(&after("--from"))));
        // The defaults reach the re-run unchanged.
        let plain = rerun_args(&parse_args(&["--quick"]).unwrap_or_else(|e| panic!("{e}")))
            .unwrap_or_else(|e| panic!("{e}"));
        let plain: Vec<String> = plain
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(plain, ["--quick", "--lines", "1000000", "--rounds", "5"]);
        // `--oqp11` takes no `--quick` and no `--shapes`; `--rounds` is at
        // least 1.
        assert!(parse_args(&["--oqp11", "--quick"]).is_err());
        assert!(parse_args(&["--restarts", "--shapes", "10k"]).is_err());
        assert!(parse_args(&["--oqp11", "--rounds", "0"]).is_err());
    }

    /// D-975, D-1266: every row's JSON has the four cold columns, a build
    /// column `null` for a shape not built.
    #[test]
    fn cold_columns_are_in_every_row() {
        let ms = Duration::from_millis;
        let row = |shape, build| Row {
            lines: 10_000,
            elements: None,
            shape,
            modules: 40,
            check: [ms(1); 2],
            build: [(ms(1), ms(2)); 2],
            size: BuildSize::default(),
            edit_to_run: None,
            cold: Cold {
                check: ms(100),
                build,
                check_std_warm: ms(20),
                build_std_warm: build.map(|_| ms(30)),
            },
        };
        let field = |row: &Value, key: &str| row.get(key).cloned();
        let built = row(Shape::Wide, Some(ms(200))).to_json();
        assert_eq!(field(&built, "cold_check_ns"), Some(json!(100_000_000)));
        assert_eq!(field(&built, "cold_build_ns"), Some(json!(200_000_000)));
        assert_eq!(
            field(&built, "cold_check_std_warm_ns"),
            Some(json!(20_000_000))
        );
        assert_eq!(
            field(&built, "cold_build_std_warm_ns"),
            Some(json!(30_000_000))
        );
        let not_built = row(Shape::HugeList, None).to_json();
        assert_eq!(field(&not_built, "cold_build_ns"), Some(Value::Null));
        assert_eq!(
            field(&not_built, "cold_build_std_warm_ns"),
            Some(Value::Null)
        );
        let table = render_cold(&[row(Shape::Wide, Some(ms(200)))]);
        assert!(
            table.contains("| 10000 lines, `wide` | 0.100 s | 0.020 s | 0.200 s | 0.030 s |"),
            "{table}"
        );
    }

    #[test]
    fn kib_has_one_decimal() {
        assert_eq!(kib(0), "0.0");
        assert_eq!(kib(1536), "1.5");
    }

    /// K6 (D-980): the seed-2 run must look up every std code record the
    /// seed-1 run on the empty cache looked up, and the empty-cache run must
    /// look one up at all.
    #[test]
    fn first_use_check_needs_every_std_line_of_the_cold_run() {
        let cold = "code.run bench miss\ncode.std std::io miss\ncode.std std::list miss\n";
        let fill = "code.std std::io miss\ncode.std std::list miss\ncode.std std::map miss\n";
        assert_eq!(check_first_use(cold, fill), Ok(()));
        let short = "code.std std::io miss\n";
        assert_eq!(
            check_first_use(cold, short),
            Err("std first use: the seed-2 run logged no `code.std` line of std::list".to_owned())
        );
        assert!(check_first_use("code.run bench miss\n", fill).is_err());
    }

    /// The column is the middle difference, negative ones included.
    #[test]
    fn first_use_median_is_the_middle_difference() {
        assert_eq!(median(&mut [30, -5, 10, 20, 0]), Some(10));
        assert_eq!(median(&mut [2, 1]), Some(2));
        assert_eq!(median(&mut []), None);
        assert!(render_first_use(150_000_000).contains("| 0.150 s |"));
    }
}
