//! `cargo xtask bench-run`: run-time numbers of the programs under
//! `bench/runtime` (D-718, D-702, `bench/runtime/README.md`). Reported,
//! never enforced, except that a checksum must match the baseline's.
//!
//! Each group is a directory `bench/runtime/<group>/` whose subdirectories
//! with a `cheby.toml` are the benchmarks. A benchmark prints `size: <n>`
//! and `checksum: <n>`. Each one runs `--runs` times with a release
//! `cheby run` (built here unless `--cheby` names a binary), timed from
//! spawn to exit, and the median, minimum and maximum are reported. One
//! more run with `CHEBY_RT_STATS=1` collects the runtime's counters from
//! standard error, parsed tolerantly since their text is not a stable
//! interface (D-635).
//!
//! With `--baseline FILE` (the JSON of an earlier run, such as
//! `bench/runtime/baseline-m1.json`), the table shows each median's change
//! and the geometric mean of the ratios, and the command fails if a
//! checksum or a size differs from the baseline's.
//!
//! With `--rust`, each benchmark of the `collections` group is followed by
//! its Rust baselines (`bench/baselines`, D-664): rows
//! `<name>@rust-<variant>`, each the same number of timed runs of
//! `cheby_bench_baselines <name> <variant> <n>` at the size the Cheby
//! program printed, which must print the Cheby program's checksum. The
//! binary is built with `--release` first, or `--rust-bin` names a
//! prebuilt one and cargo is not run for it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The environment variable that turns on the runtime's counters (D-635):
/// its events are counted only when it is set (D-768), so the timed runs
/// do not pay for them.
const STATS_ENV: &str = "CHEBY_RT_STATS";

/// The group whose benchmarks have Rust baselines (D-664).
const RUST_GROUP: &str = "collections";

/// The package of the Rust baselines, `bench/baselines`.
const RUST_PACKAGE: &str = "cheby_bench_baselines";

/// What a Rust baseline's row adds to its benchmark's name.
const RUST_MARK: &str = "@rust-";

/// The command's options.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// One run per benchmark (unless `--runs` says otherwise)
    #[arg(long)]
    quick: bool,
    /// The group to run: a directory of `bench/runtime`, or `all`
    #[arg(long, default_value = "all")]
    group: String,
    /// Run with `CHEBY_THREADS=N`
    #[arg(long)]
    threads: Option<usize>,
    /// Timed runs per benchmark (default 5, or 1 with `--quick`)
    #[arg(long)]
    runs: Option<usize>,
    /// The JSON of an earlier run to compare with
    #[arg(long)]
    baseline: Option<PathBuf>,
    /// The `cheby` binary to measure, instead of building one
    #[arg(long)]
    cheby: Option<PathBuf>,
    /// The commit the measured `cheby` was built from, for the JSON's
    /// metadata (default: this tree's `HEAD`, or `unknown` with `--cheby`)
    #[arg(long)]
    commit: Option<String>,
    /// An environment variable for every run, as `KEY=VALUE`; repeatable
    #[arg(long = "env", value_parser = parse_env)]
    env: Vec<(String, String)>,
    /// Write the results as JSON to this file
    #[arg(long)]
    json: Option<PathBuf>,
    /// Also time the Rust baselines of the `collections` group, built with
    /// `--release` (rows `<name>@rust-<variant>`)
    #[arg(long)]
    rust: bool,
    /// The prebuilt `cheby_bench_baselines` binary for `--rust` (implies
    /// it); cargo is then not run for the baselines
    #[arg(long)]
    rust_bin: Option<PathBuf>,
}

/// Parses `KEY=VALUE`.
pub fn parse_env(text: &str) -> Result<(String, String), String> {
    match text.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key.to_owned(), value.to_owned())),
        _ => Err(format!("`{text}` is not KEY=VALUE")),
    }
}

/// One benchmark's results.
#[derive(Debug, Clone, PartialEq)]
struct Measured {
    group: String,
    name: String,
    /// The size the program printed, if it printed one.
    n: Option<u64>,
    runs: Vec<Duration>,
    checksum: String,
    stats: Value,
}

impl Measured {
    fn median(&self) -> Duration {
        median(&self.runs)
    }

    fn to_json(&self) -> Value {
        let nanos = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        json!({
            "group": self.group,
            "name": self.name,
            "n": self.n,
            "median_ns": nanos(self.median()),
            "min_ns": self.runs.iter().copied().min().map(nanos),
            "max_ns": self.runs.iter().copied().max().map(nanos),
            "runs_ns": self.runs.iter().copied().map(nanos).collect::<Vec<_>>(),
            "checksum": self.checksum,
            "stats": self.stats,
        })
    }
}

pub fn run(root: &Path, args: &Args) -> Result<(), String> {
    let runs = args.runs.unwrap_or(if args.quick { 1 } else { 5 });
    if runs == 0 {
        return Err("--runs must be at least 1".to_owned());
    }
    let benchmarks = find_benchmarks(&root.join("bench/runtime"), &args.group)?;
    let baseline = args.baseline.as_deref().map(read_baseline).transpose()?;
    let cheby = match &args.cheby {
        Some(path) => crate::absolute(path)?,
        None => crate::build_cheby_profile(root, true)?,
    };
    let rust = rust_baselines(root, args)?;
    if rust.is_some() && !benchmarks.iter().any(|(group, _)| group == RUST_GROUP) {
        eprintln!("bench-run: --rust: only the `{RUST_GROUP}` group has Rust baselines");
    }
    let mut env = args.env.clone();
    if let Some(threads) = args.threads {
        env.push(("CHEBY_THREADS".to_owned(), threads.to_string()));
    }
    let commit = match (&args.commit, &args.cheby) {
        (Some(commit), _) => commit.clone(),
        (None, None) => head_commit(root),
        (None, Some(_)) => "unknown".to_owned(),
    };
    let load_before = load_average();
    let mut results = Vec::with_capacity(benchmarks.len());
    for (group, dir) in &benchmarks {
        let name = file_name(dir);
        eprintln!("bench-run: {group}/{name}");
        let measured = measure(&cheby, group, dir, &env, runs)?;
        let mut baselines = Vec::new();
        if let Some(rust) = rust.as_ref().filter(|_| group == RUST_GROUP) {
            for (_, variant) in rust.variants.iter().filter(|(program, _)| *program == name) {
                eprintln!("bench-run: {group}/{name}{RUST_MARK}{variant}");
                baselines.push(measure_rust(&rust.bin, &measured, variant, runs)?);
            }
        }
        results.push(measured);
        results.append(&mut baselines);
    }
    let meta = Meta {
        commit,
        cheby: relative_to(root, &cheby),
        env: env.clone(),
        threads: args.threads,
        runs,
        load_before,
    };
    let report = render(&results, baseline.as_ref());
    print!("{}", report.table);
    if let Some(out) = &args.json {
        let mut meta = meta.to_json();
        if let (Some(rust), Some(fields)) = (&rust, meta.as_object_mut()) {
            fields.insert(
                "rust_baselines".to_owned(),
                json!(relative_to(root, &rust.bin)),
            );
        }
        let value = json!({
            "meta": meta,
            "results": results.iter().map(Measured::to_json).collect::<Vec<_>>(),
        });
        let text = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
        std::fs::write(out, format!("{text}\n")).map_err(|e| format!("{}: {e}", out.display()))?;
        eprintln!("bench-run: wrote {}", out.display());
    }
    if report.mismatches.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the results differ from the baseline's:\n  {}",
            report.mismatches.join("\n  ")
        ))
    }
}

/// The benchmarks of `group` (or of every group, for `all`), as `(group,
/// directory)`, sorted by group and name.
fn find_benchmarks(base: &Path, group: &str) -> Result<Vec<(String, PathBuf)>, String> {
    let groups: Vec<String> = if group == "all" {
        subdirectories(base)?.iter().map(|d| file_name(d)).collect()
    } else {
        if !base.join(group).is_dir() {
            return Err(format!(
                "there is no benchmark group `{group}` in {}",
                base.display()
            ));
        }
        vec![group.to_owned()]
    };
    let mut out = Vec::new();
    for group in groups {
        for dir in subdirectories(&base.join(&group))? {
            if dir.join("cheby.toml").is_file() {
                out.push((group.clone(), dir));
            }
        }
    }
    if out.is_empty() {
        return Err(format!("no benchmark found for the group `{group}`"));
    }
    Ok(out)
}

/// The subdirectories of `dir`, sorted.
fn subdirectories(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.is_dir() {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Runs the benchmark in `dir` `runs` times, then once with the counters.
fn measure(
    cheby: &Path,
    group: &str,
    dir: &Path,
    env: &[(String, String)],
    runs: usize,
) -> Result<Measured, String> {
    let name = file_name(dir);
    let mut times = Vec::with_capacity(runs);
    let mut first: Option<(Option<u64>, String)> = None;
    for _ in 0..runs {
        let (time, stdout, _) = run_once(cheby, dir, env, false)?;
        let printed = parse_output(&stdout)
            .ok_or_else(|| format!("{group}/{name} printed no `checksum: <n>` line:\n{stdout}"))?;
        match &first {
            Some(seen) if *seen != printed => {
                return Err(format!(
                    "{group}/{name} printed checksum {} and then {}",
                    seen.1, printed.1
                ));
            }
            Some(_) => {}
            None => first = Some(printed),
        }
        times.push(time);
    }
    let (n, checksum) = first.ok_or("no run")?;
    let (_, stdout, stderr) = run_once(cheby, dir, env, true)?;
    if parse_output(&stdout).as_ref().map(|p| &p.1) != Some(&checksum) {
        return Err(format!(
            "{group}/{name} printed another checksum with {STATS_ENV}=1:\n{stdout}"
        ));
    }
    Ok(Measured {
        group: group.to_owned(),
        name,
        n,
        runs: times,
        checksum,
        stats: parse_stats(&stderr),
    })
}

/// Runs `cheby run` in `dir` once; it must succeed. Returns the wall time,
/// standard output and standard error.
fn run_once(
    cheby: &Path,
    dir: &Path,
    env: &[(String, String)],
    stats: bool,
) -> Result<(Duration, String, String), String> {
    let mut cmd = Command::new(cheby);
    cmd.args(["--color=never", "run"]).current_dir(dir);
    for (key, value) in env {
        cmd.env(key, value);
    }
    if stats {
        cmd.env(STATS_ENV, "1");
    } else {
        cmd.env_remove(STATS_ENV);
    }
    let start = Instant::now();
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run {}: {e}", cheby.display()))?;
    let elapsed = start.elapsed();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        return Err(format!(
            "`cheby run` failed in {} ({})\n{stdout}{stderr}",
            dir.display(),
            out.status
        ));
    }
    Ok((elapsed, stdout, stderr))
}

/// The Rust baselines of `--rust`: the binary and the `(program, variant)`
/// pairs its `--list` printed.
struct RustBaselines {
    bin: PathBuf,
    variants: Vec<(String, String)>,
}

/// The Rust baselines when `--rust` or `--rust-bin` asks for them: the
/// binary `--rust-bin` names, or `cheby_bench_baselines` built with
/// `--release` here.
fn rust_baselines(root: &Path, args: &Args) -> Result<Option<RustBaselines>, String> {
    let bin = match (&args.rust_bin, args.rust) {
        (Some(path), _) => crate::absolute(path)?,
        (None, true) => build_rust_baselines(root)?,
        (None, false) => return Ok(None),
    };
    let out = Command::new(&bin)
        .arg("--list")
        .output()
        .map_err(|e| format!("cannot run {}: {e}", bin.display()))?;
    if !out.status.success() {
        return Err(format!(
            "`{} --list` failed ({})\n{}",
            bin.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let variants = parse_variants(&String::from_utf8_lossy(&out.stdout))?;
    Ok(Some(RustBaselines { bin, variants }))
}

/// Builds `cheby_bench_baselines` with `--release` and returns its path.
fn build_rust_baselines(root: &Path) -> Result<PathBuf, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let status = Command::new(cargo)
        .current_dir(root)
        .args(["build", "--quiet", "--release", "--package", RUST_PACKAGE])
        .status()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    if !status.success() {
        return Err(format!("cannot build `{RUST_PACKAGE}`"));
    }
    let target =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let name = format!("{RUST_PACKAGE}{}", std::env::consts::EXE_SUFFIX);
    Ok(target.join("release").join(name))
}

/// The `<program> <variant>` lines of `cheby_bench_baselines --list`.
fn parse_variants(text: &str) -> Result<Vec<(String, String)>, String> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            match words.as_slice() {
                [program, variant] => Ok(((*program).to_owned(), (*variant).to_owned())),
                _ => Err(format!(
                    "`{RUST_PACKAGE} --list` printed `{line}`, not `<program> <variant>`"
                )),
            }
        })
        .collect()
}

/// Runs the Rust baseline `variant` of the benchmark measured as `of`
/// `runs` times, at the size `of` printed. Every run must succeed and
/// print the same checksum; `render` compares it with `of`'s.
fn measure_rust(bin: &Path, of: &Measured, variant: &str, runs: usize) -> Result<Measured, String> {
    let name = format!("{}{RUST_MARK}{variant}", of.name);
    let mut cmd_args = vec![of.name.clone(), variant.to_owned()];
    if let Some(n) = of.n {
        cmd_args.push(n.to_string());
    }
    let mut times = Vec::with_capacity(runs);
    let mut first: Option<(Option<u64>, String)> = None;
    for _ in 0..runs {
        let start = Instant::now();
        let out = Command::new(bin)
            .args(&cmd_args)
            .output()
            .map_err(|e| format!("cannot run {}: {e}", bin.display()))?;
        let elapsed = start.elapsed();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            return Err(format!(
                "`{} {}` failed ({})\n{stdout}{}",
                bin.display(),
                cmd_args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let printed = parse_output(&stdout)
            .ok_or_else(|| format!("{name} printed no `checksum: <n>` line:\n{stdout}"))?;
        match &first {
            Some(seen) if *seen != printed => {
                return Err(format!(
                    "{name} printed checksum {} and then {}",
                    seen.1, printed.1
                ));
            }
            Some(_) => {}
            None => first = Some(printed),
        }
        times.push(elapsed);
    }
    let (n, checksum) = first.ok_or("no run")?;
    Ok(Measured {
        group: of.group.clone(),
        name,
        n,
        runs: times,
        checksum,
        stats: json!({}),
    })
}

/// The size and the checksum a benchmark printed: `size: <n>` (optional)
/// and `checksum: <text>`.
fn parse_output(stdout: &str) -> Option<(Option<u64>, String)> {
    let mut n = None;
    let mut checksum = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("size: ") {
            n = rest.trim().parse().ok();
        } else if let Some(rest) = line.strip_prefix("checksum: ") {
            checksum = Some(rest.trim().to_owned());
        }
    }
    Some((n, checksum?))
}

/// The counters of `CHEBY_RT_STATS=1` (D-635), read tolerantly: the
/// summary line `runtime stats: main <t> ms, <n> entries <t> ms, peak <n>
/// bytes` gives `main_ms`, `entries_ms` and `peak_bytes` where they
/// parse, and each later line `<name>: <integer>` a counter. Lines that do
/// not parse are kept under `unparsed`.
fn parse_stats(stderr: &str) -> Value {
    let mut stats = serde_json::Map::new();
    let mut counters = serde_json::Map::new();
    let mut unparsed = Vec::new();
    let mut seen_summary = false;
    for line in stderr.lines() {
        if let Some(summary) = line.strip_prefix("runtime stats:") {
            seen_summary = true;
            for part in summary.split(',') {
                let words: Vec<&str> = part.split_whitespace().collect();
                match words.as_slice() {
                    ["main", time, "ms"] => insert_float(&mut stats, "main_ms", time),
                    [_, "entries", time, "ms"] => insert_float(&mut stats, "entries_ms", time),
                    ["peak", bytes, "bytes"] => {
                        if let Ok(bytes) = bytes.parse::<u64>() {
                            stats.insert("peak_bytes".to_owned(), json!(bytes));
                        }
                    }
                    _ => unparsed.push(part.trim().to_owned()),
                }
            }
        } else if seen_summary {
            match line.trim().rsplit_once(':') {
                Some((name, value)) if value.trim().parse::<u64>().is_ok() => {
                    let value = value.trim().parse::<u64>().unwrap_or(0);
                    counters.insert(name.trim().to_owned(), json!(value));
                }
                _ if line.trim().is_empty() => {}
                _ => unparsed.push(line.trim().to_owned()),
            }
        }
    }
    if !counters.is_empty() {
        stats.insert("counters".to_owned(), Value::Object(counters));
    }
    if !unparsed.is_empty() {
        stats.insert("unparsed".to_owned(), json!(unparsed));
    }
    Value::Object(stats)
}

fn insert_float(stats: &mut serde_json::Map<String, Value>, key: &str, text: &str) {
    if let Ok(value) = text.parse::<f64>() {
        stats.insert(key.to_owned(), json!(value));
    }
}

/// A baseline's results, by `(group, name)`: the median, the size and the
/// checksum.
type Baseline = std::collections::BTreeMap<(String, String), (Duration, Option<u64>, String)>;

fn read_baseline(path: &Path) -> Result<Baseline, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{}: no `results` array", path.display()))?;
    let mut out = Baseline::new();
    for result in results {
        let field = |key: &str| result.get(key).and_then(Value::as_str).map(str::to_owned);
        let (Some(group), Some(name), Some(checksum), Some(median)) = (
            field("group"),
            field("name"),
            field("checksum"),
            result.get("median_ns").and_then(Value::as_u64),
        ) else {
            return Err(format!("{}: a result lacks a field", path.display()));
        };
        let n = result.get("n").and_then(Value::as_u64);
        out.insert((group, name), (Duration::from_nanos(median), n, checksum));
    }
    Ok(out)
}

/// The rendered report and what differs from the baseline.
struct Report {
    table: String,
    mismatches: Vec<String>,
}

fn render(results: &[Measured], baseline: Option<&Baseline>) -> Report {
    let secs = |d: Duration| format!("{:.3} s", d.as_secs_f64());
    let mut table = String::new();
    let mut mismatches = Vec::new();
    let mut ratios = Vec::new();
    table.push_str(
        "| Benchmark | n | Median | Min | Max | Baseline | Δ% | Checksum | Allocations | Peak bytes |\n",
    );
    table.push_str(
        "| --------- | - | ------ | --- | --- | -------- | -- | -------- | ----------- | ---------- |\n",
    );
    for m in results {
        let median = m.median();
        let base = baseline.and_then(|b| b.get(&(m.group.clone(), m.name.clone())));
        let (base_text, delta) = match base {
            Some((base_median, base_n, base_checksum)) => {
                if *base_n != m.n {
                    mismatches.push(format!(
                        "{}/{}: size {} here, {} in the baseline",
                        m.group,
                        m.name,
                        show_n(m.n),
                        show_n(*base_n)
                    ));
                } else if *base_checksum != m.checksum {
                    mismatches.push(format!(
                        "{}/{}: checksum {} here, {} in the baseline",
                        m.group, m.name, m.checksum, base_checksum
                    ));
                }
                match ratio(median, *base_median) {
                    Some(r) => {
                        ratios.push(r);
                        (secs(*base_median), format!("{:+.1}%", (r - 1.0) * 100.0))
                    }
                    None => (secs(*base_median), "-".to_owned()),
                }
            }
            None => ("-".to_owned(), "-".to_owned()),
        };
        let stat = |key: &str| {
            m.stats
                .get(key)
                .or_else(|| m.stats.get("counters").and_then(|c| c.get(key)))
                .and_then(Value::as_u64)
                .map_or_else(|| "-".to_owned(), |v| v.to_string())
        };
        let runs = &m.runs;
        let _ = writeln!(
            table,
            "| {}/{} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            m.group,
            m.name,
            show_n(m.n),
            secs(median),
            secs(runs.iter().copied().min().unwrap_or_default()),
            secs(runs.iter().copied().max().unwrap_or_default()),
            base_text,
            delta,
            m.checksum,
            stat("allocations"),
            stat("peak_bytes"),
        );
    }
    if let Some(mean) = geometric_mean(&ratios) {
        let _ = writeln!(
            table,
            "\nGeometric mean of the ratios to the baseline over {} benchmarks: {:.3} ({:+.1}%).",
            ratios.len(),
            mean,
            (mean - 1.0) * 100.0
        );
    }
    let factors = rust_factors(results, &mut mismatches);
    if !factors.is_empty() {
        table.push_str(
            "\n| Benchmark | Rust variant | Cheby median | Rust median | Factor |\n\
             | --------- | ------------ | ------------ | ----------- | ------ |\n",
        );
        table.push_str(&factors);
        table.push_str(
            "\nFactor: the Cheby program's median over the Rust baseline's, both timed from spawn \
             to exit (D-664).\n",
        );
    }
    let runs = results.first().map_or(0, |m| m.runs.len());
    let _ = writeln!(
        table,
        "\nWall time of `cheby run` (compilation included), median of {runs} runs. Reported, never \
         enforced (D-718, D-702)."
    );
    Report { table, mismatches }
}

/// The rows of the factor table, one per Rust baseline in `results` whose
/// benchmark was measured too. A baseline whose size or checksum differs
/// from its benchmark's adds to `mismatches`.
fn rust_factors(results: &[Measured], mismatches: &mut Vec<String>) -> String {
    let secs = |d: Duration| format!("{:.3} s", d.as_secs_f64());
    let mut rows = String::new();
    for baseline in results {
        let Some((program, variant)) = baseline.name.split_once(RUST_MARK) else {
            continue;
        };
        let Some(of) = results
            .iter()
            .find(|m| m.group == baseline.group && m.name == program)
        else {
            continue;
        };
        if of.n != baseline.n || of.checksum != baseline.checksum {
            mismatches.push(format!(
                "{}/{}: size {} and checksum {}, but the Cheby program printed size {} and \
                 checksum {}",
                baseline.group,
                baseline.name,
                show_n(baseline.n),
                baseline.checksum,
                show_n(of.n),
                of.checksum
            ));
        }
        let factor = ratio(of.median(), baseline.median())
            .map_or_else(|| "-".to_owned(), |r| format!("{r:.2}"));
        let _ = writeln!(
            rows,
            "| {}/{} | {} | {} | {} | {} |",
            of.group,
            of.name,
            variant,
            secs(of.median()),
            secs(baseline.median()),
            factor
        );
    }
    rows
}

fn show_n(n: Option<u64>) -> String {
    n.map_or_else(|| "-".to_owned(), |n| n.to_string())
}

/// `time / base`, if both are positive.
fn ratio(time: Duration, base: Duration) -> Option<f64> {
    let (time, base) = (time.as_secs_f64(), base.as_secs_f64());
    (time > 0.0 && base > 0.0).then(|| time / base)
}

fn geometric_mean(ratios: &[f64]) -> Option<f64> {
    if ratios.is_empty() {
        return None;
    }
    let count = u32::try_from(ratios.len()).map_or(f64::from(u32::MAX), f64::from);
    let sum: f64 = ratios.iter().map(|r| r.ln()).sum();
    Some((sum / count).exp())
}

/// The median of `times`; the mean of the two middle ones for an even
/// count.
fn median(times: &[Duration]) -> Duration {
    let mut sorted = times.to_vec();
    sorted.sort();
    let mid = sorted.len() / 2;
    match (sorted.get(mid.wrapping_sub(1)), sorted.get(mid)) {
        (Some(a), Some(b)) if sorted.len().is_multiple_of(2) => {
            a.saturating_add(*b).checked_div(2).unwrap_or_default()
        }
        (_, Some(b)) => *b,
        _ => Duration::ZERO,
    }
}

/// What the JSON says about the machine and the run.
pub struct Meta {
    pub commit: String,
    pub cheby: String,
    pub env: Vec<(String, String)>,
    pub threads: Option<usize>,
    pub runs: usize,
    pub load_before: Option<[f64; 3]>,
}

impl Meta {
    pub fn to_json(&self) -> Value {
        let env: serde_json::Map<String, Value> = self
            .env
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        json!({
            "commit": self.commit,
            "cheby": self.cheby,
            "rustc": rustc_version(),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "cpu": cpu_name(),
            "cores": std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
            "date": utc_now(),
            "load": self.load_before,
            "load_after": load_average(),
            "env": env,
            "threads": self.threads,
            "runs": self.runs,
        })
    }
}

/// `path` relative to the repository `root` when it is inside it, so the
/// JSON names no home directory; otherwise as given.
pub fn relative_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map_or_else(|_| path.display().to_string(), |p| p.display().to_string())
}

/// The short hash of this tree's `HEAD`, with `+dirty` if tracked files
/// changed; `unknown` without git.
pub fn head_commit(root: &Path) -> String {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    let Some(head) = git(&["rev-parse", "--short=7", "HEAD"]) else {
        return "unknown".to_owned();
    };
    let dirty =
        git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    if dirty { format!("{head}+dirty") } else { head }
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

fn rustc_version() -> String {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    command_output(&rustc, &["--version"]).unwrap_or_else(|| "unknown".to_owned())
}

fn cpu_name() -> String {
    if cfg!(target_os = "macos")
        && let Some(name) = command_output("sysctl", &["-n", "machdep.cpu.brand_string"])
    {
        return name;
    }
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("model name"))
                .and_then(|rest| rest.split_once(':'))
                .map(|(_, name)| name.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The 1, 5 and 15-minute load averages, where the system reports them.
pub fn load_average() -> Option<[f64; 3]> {
    let text = if cfg!(target_os = "macos") {
        command_output("sysctl", &["-n", "vm.loadavg"])?
    } else {
        std::fs::read_to_string("/proc/loadavg").ok()?
    };
    let mut values = text
        .split_whitespace()
        .filter_map(|word| word.parse::<f64>().ok());
    Some([values.next()?, values.next()?, values.next()?])
}

/// The current time in UTC, as `YYYY-MM-DDTHH:MM:SSZ`.
fn utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format_utc(secs)
}

/// `secs` since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ` (the civil-date
/// algorithm of Howard Hinnant's `civil_from_days`).
#[expect(
    clippy::arithmetic_side_effects,
    reason = "days fit in 48 bits, so no step of the algorithm overflows an i64"
)]
fn format_utc(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rest = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_pairs_parse() {
        assert_eq!(parse_env("A=b=c"), Ok(("A".to_owned(), "b=c".to_owned())));
        assert_eq!(parse_env("A="), Ok(("A".to_owned(), String::new())));
        assert!(parse_env("A").is_err());
        assert!(parse_env("=b").is_err());
    }

    #[test]
    fn output_gives_size_and_checksum() {
        assert_eq!(
            parse_output("size: 10\nchecksum: 42\n"),
            Some((Some(10), "42".to_owned()))
        );
        assert_eq!(parse_output("checksum: 7"), Some((None, "7".to_owned())));
        assert_eq!(parse_output("nothing"), None);
    }

    #[test]
    fn stats_parse_tolerantly() {
        let stderr = "runtime stats: main 1.333 ms, 5 entries 1.476 ms, peak 18280 bytes\n  \
                      allocations: 5887\n  list push in place: 29960\n  something new\n";
        let stats = parse_stats(stderr);
        assert_eq!(stats["main_ms"], json!(1.333));
        assert_eq!(stats["entries_ms"], json!(1.476));
        assert_eq!(stats["peak_bytes"], json!(18_280));
        assert_eq!(stats["counters"]["allocations"], json!(5_887));
        assert_eq!(stats["counters"]["list push in place"], json!(29_960));
        assert_eq!(stats["unparsed"], json!(["something new"]));
        assert_eq!(parse_stats(""), json!({}));
    }

    #[test]
    fn medians_and_means() {
        let ms = Duration::from_millis;
        assert_eq!(median(&[ms(3), ms(1), ms(2)]), ms(2));
        assert_eq!(
            median(&[ms(4), ms(1), ms(2), ms(3)]),
            Duration::from_micros(2_500)
        );
        assert_eq!(median(&[]), Duration::ZERO);
        let mean = geometric_mean(&[2.0, 0.5]).unwrap();
        assert!((mean - 1.0).abs() < 1e-12);
        assert_eq!(geometric_mean(&[]), None);
    }

    #[test]
    fn paths_inside_the_repository_are_relative() {
        let root = Path::new("/repo");
        assert_eq!(
            relative_to(root, Path::new("/repo/target/release/cheby")),
            "target/release/cheby"
        );
        assert_eq!(relative_to(root, Path::new("/opt/cheby")), "/opt/cheby");
    }

    #[test]
    fn dates_are_civil_utc() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_791_763_199), "2026-10-11T23:59:59Z");
    }

    #[test]
    fn a_checksum_that_differs_from_the_baseline_is_reported() {
        let measured = Measured {
            group: "rc".to_owned(),
            name: "x".to_owned(),
            n: Some(5),
            runs: vec![Duration::from_millis(110)],
            checksum: "1".to_owned(),
            stats: json!({}),
        };
        let mut baseline = Baseline::new();
        baseline.insert(
            ("rc".to_owned(), "x".to_owned()),
            (Duration::from_millis(100), Some(5), "2".to_owned()),
        );
        let report = render(std::slice::from_ref(&measured), Some(&baseline));
        assert_eq!(report.mismatches.len(), 1, "{}", report.table);
        assert!(report.table.contains("+10.0%"), "{}", report.table);
        baseline.insert(
            ("rc".to_owned(), "x".to_owned()),
            (Duration::from_millis(100), Some(5), "1".to_owned()),
        );
        assert_eq!(
            render(&[measured], Some(&baseline)).mismatches,
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_rust_baselines_list_parses() {
        assert_eq!(
            parse_variants("sorted_map rc\nsorted_map btree\n\n").unwrap(),
            vec![
                ("sorted_map".to_owned(), "rc".to_owned()),
                ("sorted_map".to_owned(), "btree".to_owned())
            ]
        );
        assert!(parse_variants("sorted_map\n").is_err());
        assert!(parse_variants("a b c\n").is_err());
    }

    #[test]
    fn a_rust_baseline_gets_a_factor_and_must_print_the_programs_checksum() {
        let row = |name: &str, millis: u64, checksum: &str| Measured {
            group: RUST_GROUP.to_owned(),
            name: name.to_owned(),
            n: Some(5),
            runs: vec![Duration::from_millis(millis)],
            checksum: checksum.to_owned(),
            stats: json!({}),
        };
        let results = [
            row("sorted_map", 900, "7"),
            row("sorted_map@rust-rc", 300, "7"),
            row("sorted_map@rust-btree", 100, "8"),
        ];
        let report = render(&results, None);
        assert!(
            report
                .table
                .contains("| collections/sorted_map | rc | 0.900 s | 0.300 s | 3.00 |"),
            "{}",
            report.table
        );
        assert!(
            report
                .table
                .contains("| btree | 0.900 s | 0.100 s | 9.00 |"),
            "{}",
            report.table
        );
        assert_eq!(report.mismatches.len(), 1, "{:?}", report.mismatches);
        assert!(report.mismatches[0].contains("sorted_map@rust-btree"));
    }
}
