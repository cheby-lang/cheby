//! `cargo xtask bench-run`: run-time numbers of the programs under
//! `bench/runtime` (D-718, D-702, `bench/runtime/README.md`). Reported;
//! a checksum must match the baseline's, and a time is enforced only by
//! `--fail-above` (D-895).
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
//! With `--against PATH`, a second `cheby` (such as a build of the base
//! branch) is timed too, its runs interleaved with the measured binary's
//! so both see the same machine, and its medians are the baseline. A
//! benchmark that it cannot run, or for which it prints another size or
//! checksum, gets no baseline: its row is reported with a note and left
//! out of the gate (D-897). With
//! `--fail-above PCT`, the command fails when the geometric mean of the
//! ratios to the baseline, or the ratio of a benchmark that `--gate` names,
//! is more than `PCT` percent above 1: the one case where a time is
//! enforced, CI's `bench-gate` job (D-895). `--accepted-by D-NNN` names a
//! decision that accepts such a slowdown, which is then only reported; with
//! `--accepted-new-since LOG`, the decision must also be absent from that
//! older log, so a change can only cite a decision it adds (D-896).
//!
//! With `--rust`, each benchmark of the `collections` group is followed by
//! its Rust baselines (`bench/baselines`, D-664): rows
//! `<name>@rust-<variant>`, each the same number of timed runs of
//! `cheby_bench_baselines <name> <variant> <n>` at the size the Cheby
//! program printed, which must print the Cheby program's checksum. The
//! binary is built with `--release` first, or `--rust-bin` names a
//! prebuilt one and cargo is not run for it.
//!
//! With `--split` (D-1254, GAP(split-buckets)), each benchmark's timed runs and stats run are
//! followed by the split of the time outside `main`, read only from what
//! `cheby` already prints. Each binary first runs one untimed `cheby
//! check` and `cheby run` (the warm-up), and builds the benchmark once with
//! `cheby build -o` into a temporary directory, never into the package.
//! Then `--split-runs` times (default 5), taking turns binary by binary
//! and run by run on the round's cache, it times `cheby check`, `cheby run`
//! and the executable, the last two with `CHEBY_RT_STATS=1` for `main_ms`
//! and `entries_ms` (D-927). Every split run sets `CHEBY_RT_STATS=1`, which
//! turns event counting on (D-768), so their `main` is not the timed runs'.
//! A binary whose `cheby build` fails, such as one without `-o`, gets no
//! executable and a note. Last, one more untimed `check` and `run` with
//! `CHEBY_CACHE_LOG` give the counts of the lines that show what each
//! parsed and missed, for a binary that writes the log. The JSON gets a
//! `split` object per result; `.github/scripts/bench_split.py` renders the
//! buckets.

use std::ffi::OsStr;
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

/// The environment variable that names the cache's lookup log (D-973).
const CACHE_LOG_ENV: &str = "CHEBY_CACHE_LOG";

/// `--split`'s timed runs per command when `--split-runs` is not given.
const SPLIT_RUNS: usize = 5;

/// `--split`'s untimed `check` and `run` per binary before the timed ones:
/// the first `check` after only `run`s filled the cache may still miss
/// work that only `check` stores.
const SPLIT_WARMUP: usize = 1;

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
    /// Another `cheby` to time as the baseline, its runs interleaved with
    /// the measured binary's (instead of `--baseline`)
    #[arg(long, conflicts_with = "baseline")]
    against: Option<PathBuf>,
    /// Fail when the geometric mean of the ratios to the baseline, or a
    /// `--gate` benchmark's ratio, is more than PCT percent above 1
    #[arg(long, value_name = "PCT")]
    fail_above: Option<f64>,
    /// A benchmark, as `group/name`, whose own ratio `--fail-above` checks
    /// too; repeatable
    #[arg(long, requires = "fail_above")]
    gate: Vec<String>,
    /// A decision that accepts the slowdown, as `D-NNN`: what `--fail-above`
    /// rejects is then reported and the command succeeds. The decision must
    /// be in `docs/decisions/LOG.md` and not struck through; repeatable
    /// (D-896)
    #[arg(long, requires = "fail_above", value_name = "D-NNN")]
    accepted_by: Vec<String>,
    /// The base's decision log: each `--accepted-by` decision must be absent
    /// from it, live or struck through, so it is new in the change (D-896)
    #[arg(long, requires = "accepted_by", value_name = "LOG")]
    accepted_new_since: Option<PathBuf>,
    /// After each benchmark's timed runs, also time `cheby check`, `cheby
    /// run` and the executable `cheby build` makes, with the counters, for
    /// the split of the time outside `main` (D-1254)
    #[arg(long)]
    split: bool,
    /// Timed runs of each command of `--split` per benchmark (default 5)
    #[arg(long, value_name = "N", requires = "split")]
    split_runs: Option<usize>,
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
    /// The split of the time outside `main`, with `--split` (D-1254).
    split: Option<Split>,
}

impl Measured {
    fn median(&self) -> Duration {
        median(&self.runs)
    }

    fn to_json(&self) -> Value {
        let mut value = json!({
            "group": self.group,
            "name": self.name,
            "n": self.n,
            "median_ns": nanos(self.median()),
            "min_ns": self.runs.iter().copied().min().map(nanos),
            "max_ns": self.runs.iter().copied().max().map(nanos),
            "runs_ns": self.runs.iter().copied().map(nanos).collect::<Vec<_>>(),
            "checksum": self.checksum,
            "stats": self.stats,
        });
        if let (Some(split), Some(fields)) = (&self.split, value.as_object_mut()) {
            fields.insert("split".to_owned(), split.to_json());
        }
        value
    }
}

/// `d` in whole nanoseconds, saturating.
fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

pub fn run(root: &Path, args: &Args) -> Result<(), String> {
    let runs = args.runs.unwrap_or(if args.quick { 1 } else { 5 });
    if runs == 0 {
        return Err("--runs must be at least 1".to_owned());
    }
    let benchmarks = find_benchmarks(&root.join("bench/runtime"), &args.group)?;
    if args.fail_above.is_some() && args.baseline.is_none() && args.against.is_none() {
        return Err("--fail-above needs --baseline or --against".to_owned());
    }
    check_accepted(root, &args.accepted_by, args.accepted_new_since.as_deref())?;
    let mut baseline = args.baseline.as_deref().map(read_baseline).transpose()?;
    let cheby = match &args.cheby {
        Some(path) => crate::absolute(path)?,
        None => crate::build_cheby_profile(root, true)?,
    };
    let against = args.against.as_deref().map(crate::absolute).transpose()?;
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
    // The executables and logs of `--split` go to a fresh directory,
    // deleted when the command ends, never into a benchmark's package.
    let split_dir = match (args.split, args.split_runs) {
        (false, _) => None,
        (true, Some(0)) => return Err("--split-runs must be at least 1".to_owned()),
        (true, n) => Some((
            n.unwrap_or(SPLIT_RUNS),
            cheby_test_support::cache::FreshDir::create("cheby-bench-split")?,
        )),
    };
    let split = split_dir.as_ref().map(|(runs, dir)| SplitSetup {
        runs: *runs,
        scratch: dir.path().to_path_buf(),
        prepare: crate::cache::apply,
    });
    let load_before = load_average();
    let (results, against_results, notes) = measure_all(
        &benchmarks,
        &cheby,
        against.as_deref(),
        rust.as_ref(),
        &env,
        runs,
        split.as_ref(),
    )?;
    let meta = Meta {
        commit,
        cheby: relative_to(root, &cheby),
        env: env.clone(),
        threads: args.threads,
        runs,
        load_before,
    };
    let mut notes = notes;
    if against.is_some() {
        baseline = Some(as_baseline(&against_results, &results, &mut notes));
    }
    let report = render(&results, baseline.as_ref());
    print!("{}", report.table);
    for note in &notes {
        println!("\nNote: {note}");
    }
    let gate_failures = match (args.fail_above, &baseline) {
        (Some(pct), Some(baseline)) => gate(&results, baseline, pct, &args.gate)?,
        _ => Vec::new(),
    };
    if let Some(out) = &args.json {
        write_json(
            root,
            out,
            &meta,
            rust.as_ref(),
            &results,
            against.as_deref(),
            &against_results,
        )?;
    }
    verdict(
        &report.mismatches,
        &gate_failures,
        args.fail_above,
        &args.accepted_by,
    )
}

/// Checks that each decision `--accepted-by` names is a live entry of the
/// decision log: `- **D-NNN …` at the start of a line, not struck through;
/// and, with `new_since`, that the older log does not have it at all.
fn check_accepted(root: &Path, ids: &[String], new_since: Option<&Path>) -> Result<(), String> {
    if ids.is_empty() {
        return Ok(());
    }
    let read =
        |path: &Path| std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()));
    let log = read(&root.join("docs/decisions/LOG.md"))?;
    let older = new_since.map(read).transpose()?;
    ids.iter().try_for_each(|id| {
        accepted_in(&log, id)?;
        match &older {
            Some(older) if in_log(older, id) => Err(format!(
                "--accepted-by {id}: the base's log already has it; cite a decision this change adds"
            )),
            _ => Ok(()),
        }
    })
}

/// Whether `log` has an entry for `id`, live or struck through.
fn in_log(log: &str, id: &str) -> bool {
    let (live, struck) = (format!("- **{id} "), format!("- ~~**{id} "));
    log.lines()
        .any(|line| line.starts_with(&live) || line.starts_with(&struck))
}

/// Whether `log` holds the live entry `id` (see [`check_accepted`]).
fn accepted_in(log: &str, id: &str) -> Result<(), String> {
    let digits = id.strip_prefix("D-").unwrap_or_default();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "--accepted-by {id}: not a decision id such as D-893"
        ));
    }
    let entry = format!("- **{id} ");
    if log.lines().any(|line| line.starts_with(&entry)) {
        Ok(())
    } else {
        Err(format!(
            "--accepted-by {id}: no such decision in docs/decisions/LOG.md, or it is struck through"
        ))
    }
}

/// The `--against` binary's medians as a baseline, for the benchmarks
/// where it printed the measured binary's size and checksum; each other
/// one gets a note and no baseline, so it is reported only (D-897).
fn as_baseline(against: &[Measured], measured: &[Measured], notes: &mut Vec<String>) -> Baseline {
    let mut out = Baseline::new();
    for base in against {
        let same = measured
            .iter()
            .find(|m| m.group == base.group && m.name == base.name)
            .is_some_and(|m| m.n == base.n && m.checksum == base.checksum);
        if same {
            out.insert(
                (base.group.clone(), base.name.clone()),
                (base.median(), base.n, base.checksum.clone()),
            );
        } else {
            notes.push(format!(
                "{}/{}: the base printed another size or checksum (size {}, checksum {}), so \
                 this row has no baseline and is not gated",
                base.group,
                base.name,
                show_n(base.n),
                base.checksum
            ));
        }
    }
    out
}

/// What [`measure_all`] returns: the measured rows, the `--against`
/// binary's rows, and notes on rows that have no baseline.
type Measurements = (Vec<Measured>, Vec<Measured>, Vec<String>);

/// Measures every benchmark, with the `--against` binary interleaved when
/// there is one, and the Rust baselines after their benchmark. A benchmark
/// the `--against` binary cannot run is measured alone, with a note. With
/// `split`, each benchmark's split follows its stats runs, for every binary
/// that ran it.
fn measure_all(
    benchmarks: &[(String, PathBuf)],
    cheby: &Path,
    against: Option<&Path>,
    rust: Option<&RustBaselines>,
    env: &[(String, String)],
    runs: usize,
    split: Option<&SplitSetup>,
) -> Result<Measurements, String> {
    let mut results = Vec::with_capacity(benchmarks.len());
    let mut against_results = Vec::new();
    let mut notes = Vec::new();
    for (group, dir) in benchmarks {
        let name = file_name(dir);
        eprintln!("bench-run: {group}/{name}");
        let probe = against.map(|other| run_once(other, dir, env, false));
        let mut measured = match (against, probe) {
            (Some(other), Some(Ok(_))) => {
                let [mut base, mut measured] =
                    measure_interleaved([other, cheby], group, dir, env, runs)?;
                if let Some(setup) = split {
                    eprintln!("bench-run: {group}/{name}: split");
                    let binaries = [
                        (other, base.checksum.as_str()),
                        (cheby, measured.checksum.as_str()),
                    ];
                    let [base_split, split] = measure_split(binaries, group, dir, env, setup)?;
                    base.split = Some(base_split);
                    measured.split = Some(split);
                }
                against_results.push(base);
                measured
            }
            (Some(_), Some(Err(e))) => {
                notes.push(format!(
                    "{group}/{name}: the base cannot run it, so this row has no baseline and is \
                     not gated: {}",
                    e.lines().next().unwrap_or_default()
                ));
                measure(cheby, group, dir, env, runs)?
            }
            _ => measure(cheby, group, dir, env, runs)?,
        };
        if let (Some(setup), None) = (split, &measured.split) {
            eprintln!("bench-run: {group}/{name}: split");
            let binaries = [(cheby, measured.checksum.as_str())];
            let [split] = measure_split(binaries, group, dir, env, setup)?;
            measured.split = Some(split);
        }
        let mut baselines = Vec::new();
        if let Some(rust) = rust.filter(|_| group == RUST_GROUP) {
            for (_, variant) in rust.variants.iter().filter(|(program, _)| *program == name) {
                eprintln!("bench-run: {group}/{name}{RUST_MARK}{variant}");
                baselines.push(measure_rust(&rust.bin, &measured, variant, runs)?);
            }
        }
        results.push(measured);
        results.append(&mut baselines);
    }
    Ok((results, against_results, notes))
}

/// The command's outcome: an error for a size or checksum that differs
/// from the baseline's, or for what `--fail-above` rejected unless a
/// decision accepted it.
fn verdict(
    mismatches: &[String],
    gate_failures: &[String],
    pct: Option<f64>,
    accepted_by: &[String],
) -> Result<(), String> {
    if !mismatches.is_empty() {
        return Err(format!(
            "the results differ from the baseline's:\n  {}",
            mismatches.join("\n  ")
        ));
    }
    if gate_failures.is_empty() {
        Ok(())
    } else if !accepted_by.is_empty() {
        println!(
            "\nSlower than the baseline by more than {}%, accepted by {} (D-896):\n\n- {}",
            pct.unwrap_or_default(),
            accepted_by.join(", "),
            gate_failures.join("\n- ")
        );
        Ok(())
    } else {
        Err(format!(
            "slower than the baseline by more than {}% (D-895):\n  {}",
            pct.unwrap_or_default(),
            gate_failures.join("\n  ")
        ))
    }
}

/// Writes the results, and with `--against` the other binary's, as JSON.
fn write_json(
    root: &Path,
    out: &Path,
    meta: &Meta,
    rust: Option<&RustBaselines>,
    results: &[Measured],
    against: Option<&Path>,
    against_results: &[Measured],
) -> Result<(), String> {
    let mut meta = meta.to_json();
    if let (Some(rust), Some(fields)) = (rust, meta.as_object_mut()) {
        fields.insert(
            "rust_baselines".to_owned(),
            json!(relative_to(root, &rust.bin)),
        );
    }
    let mut value = json!({
        "meta": meta,
        "results": results.iter().map(Measured::to_json).collect::<Vec<_>>(),
    });
    if let (Some(other), Some(fields)) = (against, value.as_object_mut()) {
        fields.insert(
            "against".to_owned(),
            json!({
                "cheby": relative_to(root, other),
                "results": against_results.iter().map(Measured::to_json).collect::<Vec<_>>(),
            }),
        );
    }
    let text = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    std::fs::write(out, format!("{text}\n")).map_err(|e| format!("{}: {e}", out.display()))?;
    eprintln!("bench-run: wrote {}", out.display());
    Ok(())
}

/// What `--fail-above pct` rejects: the geometric mean of the ratios of
/// the benchmarks the baseline holds, and each `gates` benchmark, when its
/// ratio is above `1 + pct / 100`. A gate the run did not measure, or one
/// the baseline lacks, is an error.
fn gate(
    results: &[Measured],
    baseline: &Baseline,
    pct: f64,
    gates: &[String],
) -> Result<Vec<String>, String> {
    let limit = 1.0 + pct / 100.0;
    // The Rust baselines' rows time other programs, so they are never gated.
    let ratio_of = |m: &Measured| {
        if m.name.contains(RUST_MARK) {
            return None;
        }
        baseline
            .get(&(m.group.clone(), m.name.clone()))
            .and_then(|(base, _, _)| ratio(m.median(), *base))
    };
    let mut failures = Vec::new();
    for name in gates {
        let row = results
            .iter()
            .find(|m| format!("{}/{}", m.group, m.name) == *name)
            .ok_or_else(|| format!("--gate {name}: not measured"))?;
        // A row with no baseline is reported only (D-897).
        if let Some(r) = ratio_of(row).filter(|r| *r > limit) {
            failures.push(format!("{name}: {:+.1}%", (r - 1.0) * 100.0));
        }
    }
    let ratios: Vec<f64> = results.iter().filter_map(ratio_of).collect();
    if let Some(mean) = geometric_mean(&ratios).filter(|mean| *mean > limit) {
        failures.push(format!(
            "geometric mean over {} benchmarks: {:+.1}%",
            ratios.len(),
            (mean - 1.0) * 100.0
        ));
    }
    Ok(failures)
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
    let [measured] = measure_interleaved([cheby], group, dir, env, runs)?;
    Ok(measured)
}

/// The size and checksum a benchmark printed.
type Printed = (Option<u64>, String);

/// Times each of `chebys` `runs` times on one benchmark, taking turns run
/// by run, so a change in the machine's load reaches them all alike; then
/// one stats run each. Each binary must print the same checksum every time.
fn measure_interleaved<const K: usize>(
    chebys: [&Path; K],
    group: &str,
    dir: &Path,
    env: &[(String, String)],
    runs: usize,
) -> Result<[Measured; K], String> {
    let name = file_name(dir);
    // Per binary: its times and the size and checksum it first printed.
    let mut seen: Vec<(Vec<Duration>, Option<Printed>)> = chebys
        .iter()
        .map(|_| (Vec::with_capacity(runs), None))
        .collect();
    for _ in 0..runs {
        for (cheby, (times, first)) in chebys.iter().zip(seen.iter_mut()) {
            let (time, stdout, _) = run_once(cheby, dir, env, false)?;
            let printed = parse_output(&stdout).ok_or_else(|| {
                format!("{group}/{name} printed no `checksum: <n>` line:\n{stdout}")
            })?;
            match first {
                Some(earlier) if *earlier != printed => {
                    return Err(format!(
                        "{group}/{name} printed checksum {} and then {}",
                        earlier.1, printed.1
                    ));
                }
                Some(_) => {}
                None => *first = Some(printed),
            }
            times.push(time);
        }
    }
    let mut out = Vec::with_capacity(K);
    for (cheby, (times, first)) in chebys.iter().zip(seen) {
        let (n, checksum) = first.ok_or("no run")?;
        let (_, stdout, stderr) = run_once(cheby, dir, env, true)?;
        if parse_output(&stdout).as_ref().map(|p| &p.1) != Some(&checksum) {
            return Err(format!(
                "{group}/{name} printed another checksum with {STATS_ENV}=1:\n{stdout}"
            ));
        }
        out.push(Measured {
            group: group.to_owned(),
            name: name.clone(),
            n,
            runs: times,
            checksum,
            stats: parse_stats(&stderr),
            split: None,
        });
    }
    out.try_into()
        .map_err(|_| "a binary's results are missing".to_owned())
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
    crate::cache::apply(&mut cmd)?;
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

/// What `--split` needs: the timed runs per command, the directory its
/// executables and logs go to, and what prepares every command it starts
/// (the round's cache, [`crate::cache::apply`]).
struct SplitSetup {
    runs: usize,
    scratch: PathBuf,
    prepare: fn(&mut Command) -> Result<(), String>,
}

/// One timed process of the split: its wall time, and the `main_ms` and
/// `entries_ms` of its `CHEBY_RT_STATS=1` line where they parse (D-635).
#[derive(Debug, Clone, PartialEq)]
struct StatsRun {
    wall: Duration,
    main_ms: Option<f64>,
    entries_ms: Option<f64>,
}

impl StatsRun {
    fn new(wall: Duration, stderr: &str) -> Self {
        let stats = parse_stats(stderr);
        let ms = |key: &str| stats.get(key).and_then(Value::as_f64);
        Self {
            wall,
            main_ms: ms("main_ms"),
            entries_ms: ms("entries_ms"),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "wall_ns": nanos(self.wall),
            "main_ms": self.main_ms,
            "entries_ms": self.entries_ms,
        })
    }
}

/// The executable `cheby build -o` made: how long the build took, and the
/// executable's timed runs.
#[derive(Debug, Clone, PartialEq)]
struct ExeSplit {
    build: Duration,
    runs: Vec<StatsRun>,
}

/// The counts of the lines of one `CHEBY_CACHE_LOG` file that show what a
/// command parsed and missed (D-973, D-1135): `parse <module>`, `parse.std
/// <module>`, and the `miss` lines of `front` and `front.std`, of every
/// `code.<kind>` and of `const`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LogCounts {
    parse: u64,
    parse_std: u64,
    front_miss: u64,
    code_miss: u64,
    const_miss: u64,
}

impl LogCounts {
    fn of(log: &str) -> Self {
        let mut counts = Self::default();
        for line in log.lines() {
            let words: Vec<&str> = line.split_whitespace().collect();
            let (Some(&artifact), Some(&last)) = (words.first(), words.last()) else {
                continue;
            };
            let kind = artifact.split('.').next().unwrap_or_default();
            let count = match (artifact, kind, last) {
                ("parse", _, _) => &mut counts.parse,
                ("parse.std", _, _) => &mut counts.parse_std,
                (_, "front", "miss") => &mut counts.front_miss,
                (_, "code", "miss") if artifact != "code" => &mut counts.code_miss,
                ("const", _, "miss") => &mut counts.const_miss,
                _ => continue,
            };
            *count = count.saturating_add(1);
        }
        counts
    }

    fn to_json(self) -> Value {
        json!({
            "parse": self.parse,
            "parse.std": self.parse_std,
            "front_miss": self.front_miss,
            "code_miss": self.code_miss,
            "const_miss": self.const_miss,
        })
    }
}

/// One binary's split of one benchmark (D-1254): the timed `cheby check`s,
/// `cheby run`s and executable runs, every one with `CHEBY_RT_STATS=1`.
#[derive(Debug, Clone, PartialEq)]
struct Split {
    /// The untimed `check` and `run` before the timed ones.
    warmup: usize,
    check: Vec<Duration>,
    run: Vec<StatsRun>,
    /// The executable, or why `cheby build` made none.
    exe: Result<ExeSplit, String>,
    /// The log's counts for `check` and for `run`, or `None` for a binary
    /// that writes no log (one without a cache).
    cache_log: Option<(LogCounts, LogCounts)>,
}

impl Split {
    fn to_json(&self) -> Value {
        let exe = match &self.exe {
            Ok(exe) => json!({
                "build_ns": nanos(exe.build),
                "runs": exe.runs.iter().map(StatsRun::to_json).collect::<Vec<_>>(),
            }),
            Err(_) => Value::Null,
        };
        let cache_log = self.cache_log.map_or(
            Value::Null,
            |(check, run)| json!({"check": check.to_json(), "run": run.to_json()}),
        );
        let mut value = json!({
            "runs": self.check.len(),
            "stats": true,
            "warmup": self.warmup,
            "check_ns": self.check.iter().copied().map(nanos).collect::<Vec<_>>(),
            "run": self.run.iter().map(StatsRun::to_json).collect::<Vec<_>>(),
            "exe": exe,
            "cache_log": cache_log,
        });
        if let (Err(note), Some(fields)) = (&self.exe, value.as_object_mut()) {
            fields.insert("exe_note".to_owned(), json!(note));
        }
        value
    }
}

/// A finished command: its wall time, its status and its output.
struct Finished {
    wall: Duration,
    success: bool,
    status: String,
    stdout: String,
    stderr: String,
}

impl Finished {
    /// The first line of standard error that is not empty, else of
    /// standard output: what a note says about a failure.
    fn first_line(&self) -> &str {
        self.stderr
            .lines()
            .chain(self.stdout.lines())
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default()
    }
}

/// Starts `program` with `args` in `dir`, with `env`, `setup`'s
/// preparation, `CHEBY_RT_STATS=1` when `stats`, and `CHEBY_CACHE_LOG` set
/// to `log` or removed, and waits for it. Only a failure to start it is an
/// error.
fn run_timed<S: AsRef<OsStr>>(
    program: &Path,
    args: &[S],
    dir: &Path,
    env: &[(String, String)],
    setup: &SplitSetup,
    stats: bool,
    log: Option<&Path>,
) -> Result<Finished, String> {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(dir);
    for (key, value) in env {
        cmd.env(key, value);
    }
    (setup.prepare)(&mut cmd)?;
    if stats {
        cmd.env(STATS_ENV, "1");
    } else {
        cmd.env_remove(STATS_ENV);
    }
    match log {
        Some(path) => cmd.env(CACHE_LOG_ENV, path),
        None => cmd.env_remove(CACHE_LOG_ENV),
    };
    let start = Instant::now();
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run {}: {e}", program.display()))?;
    Ok(Finished {
        wall: start.elapsed(),
        success: out.status.success(),
        status: out.status.to_string(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// `done` if it succeeded, else an error naming `what` and its output.
fn succeeded(done: Finished, what: &str) -> Result<Finished, String> {
    if done.success {
        Ok(done)
    } else {
        Err(format!(
            "{what} failed ({})\n{}{}",
            done.status, done.stdout, done.stderr
        ))
    }
}

/// A run of a benchmark (`cheby run` or its executable) that succeeded and
/// printed `checksum`, as a [`StatsRun`].
fn checked_run(done: Finished, what: &str, checksum: &str) -> Result<StatsRun, String> {
    let done = succeeded(done, what)?;
    match parse_output(&done.stdout) {
        Some((_, printed)) if printed == checksum => Ok(StatsRun::new(done.wall, &done.stderr)),
        _ => Err(format!(
            "{what} did not print `checksum: {checksum}` with {STATS_ENV}=1:\n{}",
            done.stdout
        )),
    }
}

/// The split of one benchmark for each of `binaries`, given with the
/// checksum its timed runs printed (D-1254; see the module's comment). An
/// executable is built into `setup.scratch`, never into the package: a
/// `build/` directory that appears in `dir` is an error.
fn measure_split<const K: usize>(
    binaries: [(&Path, &str); K],
    group: &str,
    dir: &Path,
    env: &[(String, String)],
    setup: &SplitSetup,
) -> Result<[Split; K], String> {
    let name = file_name(dir);
    let what =
        |cheby: &Path, command: &str| format!("`{} {command}` of {group}/{name}", cheby.display());
    let check = |cheby: &Path, log: Option<&Path>| -> Result<Duration, String> {
        let done = run_timed(
            cheby,
            &["--color=never", "check"],
            dir,
            env,
            setup,
            true,
            log,
        )?;
        Ok(succeeded(done, &what(cheby, "check"))?.wall)
    };
    let run = |cheby: &Path, checksum: &str, log: Option<&Path>| -> Result<StatsRun, String> {
        let done = run_timed(cheby, &["--color=never", "run"], dir, env, setup, true, log)?;
        checked_run(done, &what(cheby, "run"), checksum)
    };
    for _ in 0..SPLIT_WARMUP {
        for (cheby, checksum) in binaries {
            check(cheby, None)?;
            run(cheby, checksum, None)?;
        }
    }
    let had_build_dir = dir.join("build").exists();
    let mut splits = Vec::with_capacity(K);
    let mut exes = Vec::with_capacity(K);
    for (i, (cheby, _)) in binaries.iter().enumerate() {
        let out = setup.scratch.join(format!("exe-{i}")).join(&name);
        let made = build_exe(cheby, dir, env, setup, &out)?;
        if !had_build_dir && dir.join("build").exists() {
            return Err(format!(
                "`{} build -o` wrote {}: the split builds into a temporary directory only",
                cheby.display(),
                dir.join("build").display()
            ));
        }
        exes.push(made.as_ref().ok().map(|_| out));
        splits.push(Split {
            warmup: SPLIT_WARMUP,
            check: Vec::with_capacity(setup.runs),
            run: Vec::with_capacity(setup.runs),
            exe: made.map(|build| ExeSplit {
                build,
                runs: Vec::with_capacity(setup.runs),
            }),
            cache_log: None,
        });
    }
    for _ in 0..setup.runs {
        for (((cheby, checksum), split), exe) in binaries.iter().zip(&mut splits).zip(&exes) {
            split.check.push(check(cheby, None)?);
            split.run.push(run(cheby, checksum, None)?);
            if let (Some(path), Ok(made)) = (exe, &mut split.exe) {
                let no_args: [&str; 0] = [];
                let done = run_timed(path, &no_args, dir, env, setup, true, None)?;
                let label = format!("the executable {} of {group}/{name}", path.display());
                made.runs.push(checked_run(done, &label, checksum)?);
            }
        }
    }
    for (i, ((cheby, checksum), split)) in binaries.iter().zip(&mut splits).enumerate() {
        let logs =
            ["check", "run"].map(|command| setup.scratch.join(format!("log-{i}-{command}.txt")));
        for log in &logs {
            remove_if_there(log)?;
        }
        check(cheby, Some(&logs[0]))?;
        run(cheby, checksum, Some(&logs[1]))?;
        split.cache_log = read_counts(&logs)?;
    }
    splits
        .try_into()
        .map_err(|_| "a binary's split is missing".to_owned())
}

/// Builds the benchmark in `dir` with `cheby build -o out`: the build's
/// wall time, or a note on why there is no executable (a binary without
/// `cheby build` or without `-o`, or a failed build).
fn build_exe(
    cheby: &Path,
    dir: &Path,
    env: &[(String, String)],
    setup: &SplitSetup,
    out: &Path,
) -> Result<Result<Duration, String>, String> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let args = [
        OsStr::new("--color=never"),
        OsStr::new("build"),
        OsStr::new("-o"),
        out.as_os_str(),
    ];
    let done = run_timed(cheby, &args, dir, env, setup, false, None)?;
    Ok(if done.success && out.is_file() {
        Ok(done.wall)
    } else if done.success {
        Err(format!(
            "`cheby build -o` exited 0 but wrote no {}",
            out.display()
        ))
    } else {
        Err(format!(
            "`cheby build -o` failed ({}): {}",
            done.status,
            done.first_line()
        ))
    })
}

/// Deletes `path` if it exists.
fn remove_if_there(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("{}: {e}", path.display()))
        }
        _ => Ok(()),
    }
}

/// The counts of the `check` and `run` logs, or `None` when the binary
/// wrote neither (it has no cache). One log without the other is an error.
fn read_counts(logs: &[PathBuf; 2]) -> Result<Option<(LogCounts, LogCounts)>, String> {
    let read = |path: &Path| match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(LogCounts::of(&text))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    };
    let [check, run] = logs;
    match (read(check)?, read(run)?) {
        (Some(check), Some(run)) => Ok(Some((check, run))),
        (None, None) => Ok(None),
        _ => Err(format!(
            "only one of {} and {} was written",
            check.display(),
            run.display()
        )),
    }
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
        split: None,
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
        "\nWall time of `cheby run` (compilation included), median of {runs} runs. Reported, and \
         enforced only by `--fail-above` (D-718, D-702, D-895)."
    );
    if let Some(split) = results.iter().find_map(|m| m.split.as_ref()) {
        let _ = writeln!(
            table,
            "\nThe split of the time outside `main` (D-1254): {} timed runs each of `cheby check`, \
             `cheby run` and the executable per binary, with `{STATS_ENV}=1`, in the JSON's \
             `split` objects; `.github/scripts/bench_split.py` renders them.",
            split.check.len()
        );
    }
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
            split: None,
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
    fn the_gate_checks_the_mean_and_named_benchmarks() {
        let at = |name: &str, ms: u64| Measured {
            group: "rc".to_owned(),
            name: name.to_owned(),
            n: Some(5),
            runs: vec![Duration::from_millis(ms)],
            checksum: "1".to_owned(),
            stats: json!({}),
            split: None,
        };
        let results = [at("a", 120), at("b", 95)];
        let mut baseline = Baseline::new();
        for name in ["a", "b"] {
            baseline.insert(
                ("rc".to_owned(), name.to_owned()),
                (Duration::from_millis(100), Some(5), "1".to_owned()),
            );
        }
        // The mean is about +6.8%, under 10%; `a` alone is +20%.
        assert_eq!(gate(&results, &baseline, 10.0, &[]), Ok(vec![]));
        let failures = gate(&results, &baseline, 10.0, &["rc/a".to_owned()]).unwrap();
        assert_eq!(failures, vec!["rc/a: +20.0%".to_owned()]);
        let failures = gate(&results, &baseline, 5.0, &["rc/b".to_owned()]).unwrap();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].starts_with("geometric mean over 2"),
            "{failures:?}"
        );
        assert!(gate(&results, &baseline, 10.0, &["rc/c".to_owned()]).is_err());
        // A gated row with no baseline, or a Rust row, is not gated.
        baseline.remove(&("rc".to_owned(), "a".to_owned()));
        assert_eq!(
            gate(&results, &baseline, 10.0, &["rc/a".to_owned()]),
            Ok(vec![])
        );
        let rust = [at("b", 95), at(&format!("b{RUST_MARK}std"), 500)];
        assert_eq!(gate(&rust, &baseline, 10.0, &[]), Ok(vec![]));
    }

    #[test]
    fn a_base_that_prints_another_checksum_gives_no_baseline() {
        let at = |checksum: &str| Measured {
            group: "rc".to_owned(),
            name: "x".to_owned(),
            n: Some(5),
            runs: vec![Duration::from_millis(100)],
            checksum: checksum.to_owned(),
            stats: json!({}),
            split: None,
        };
        let mut notes = Vec::new();
        assert_eq!(as_baseline(&[at("1")], &[at("1")], &mut notes).len(), 1);
        assert_eq!(notes, Vec::<String>::new());
        assert!(as_baseline(&[at("2")], &[at("1")], &mut notes).is_empty());
        assert_eq!(notes.len(), 1, "{notes:?}");
    }

    #[test]
    fn an_accepted_slowdown_passes_and_names_a_live_decision() {
        let failures = vec!["rc/a: +20.0%".to_owned()];
        assert!(verdict(&[], &failures, Some(10.0), &[]).is_err());
        assert_eq!(
            verdict(&[], &failures, Some(10.0), &["D-893".to_owned()]),
            Ok(())
        );
        // A checksum that differs is never accepted, with or without a
        // slowdown.
        assert!(verdict(&["x".to_owned()], &[], Some(10.0), &["D-893".to_owned()]).is_err());
        let accepted = ["D-893".to_owned()];
        assert!(verdict(&["x".to_owned()], &failures, Some(10.0), &accepted).is_err());
        let log = "- **D-893 Owned slices**: text.\n- ~~**D-12 Old**: text.~~ Superseded.\n";
        assert_eq!(accepted_in(log, "D-893"), Ok(()));
        assert!(accepted_in(log, "D-12").is_err(), "struck through");
        assert!(accepted_in(log, "D-89").is_err(), "a prefix of another id");
        assert!(accepted_in(log, "D-").is_err());
        assert!(accepted_in(log, "893").is_err());
        assert!(in_log(log, "D-893") && in_log(log, "D-12"));
        assert!(!in_log(log, "D-89") && !in_log(log, "D-894"));
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
            split: None,
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

    #[test]
    fn bench_split_options_parse() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: Args,
        }
        use clap::Parser as _;
        let cli = Cli::try_parse_from(["bench-run", "--split"]).unwrap();
        assert!(cli.args.split);
        assert_eq!(cli.args.split_runs, None);
        let cli = Cli::try_parse_from(["bench-run", "--split", "--split-runs", "3"]).unwrap();
        assert_eq!(cli.args.split_runs, Some(3));
        assert!(Cli::try_parse_from(["bench-run", "--split-runs", "3"]).is_err());
        assert!(!Cli::try_parse_from(["bench-run"]).unwrap().args.split);
    }

    #[test]
    fn bench_split_counts_the_cache_log_lines() {
        let log = "code.plain app hit\ncode.run app miss\ncode.std std::list miss\n\
                   code.std std::map hit\nconst app::LIMIT miss\nconst app::SEED hit\n\
                   facts app hit\nfront app miss\nfront other hit\nfront.std std::list miss\n\
                   key code.run app 7d04e0f6\nmasks app miss\nparse app\nparse other\n\
                   parse.std std::list\n\n";
        assert_eq!(
            LogCounts::of(log),
            LogCounts {
                parse: 2,
                parse_std: 1,
                front_miss: 2,
                code_miss: 2,
                const_miss: 1,
            }
        );
        assert_eq!(LogCounts::of(""), LogCounts::default());
        assert_eq!(
            LogCounts::of("parse app\n").to_json(),
            json!({"parse": 1, "parse.std": 0, "front_miss": 0, "code_miss": 0, "const_miss": 0})
        );
    }

    #[test]
    fn bench_split_reads_a_stats_run() {
        let stderr = "runtime stats: main 301.2 ms, 2 entries 309.8 ms, peak 10 bytes\n  \
                      allocations: 5\n";
        let run = StatsRun::new(Duration::from_millis(412), stderr);
        assert_eq!(run.main_ms, Some(301.2));
        assert_eq!(run.entries_ms, Some(309.8));
        assert_eq!(
            run.to_json(),
            json!({"wall_ns": 412_000_000, "main_ms": 301.2, "entries_ms": 309.8})
        );
        let none = StatsRun::new(Duration::from_millis(1), "no stats\n");
        assert_eq!(
            none.to_json(),
            json!({"wall_ns": 1_000_000, "main_ms": null, "entries_ms": null})
        );
    }

    #[test]
    fn bench_split_json_has_the_shape_the_gate_reads() {
        let run = |main| StatsRun {
            wall: Duration::from_millis(10),
            main_ms: Some(main),
            entries_ms: Some(main),
        };
        let counts = LogCounts {
            parse: 1,
            ..LogCounts::default()
        };
        let head = Split {
            warmup: 1,
            check: vec![Duration::from_millis(4)],
            run: vec![run(6.0)],
            exe: Ok(ExeSplit {
                build: Duration::from_millis(100),
                runs: vec![run(5.0)],
            }),
            cache_log: Some((LogCounts::default(), counts)),
        }
        .to_json();
        assert_eq!(head["runs"], json!(1));
        assert_eq!(head["stats"], json!(true));
        assert_eq!(head["warmup"], json!(1));
        assert_eq!(head["check_ns"], json!([4_000_000]));
        assert_eq!(head["run"][0]["main_ms"], json!(6.0));
        assert_eq!(head["exe"]["build_ns"], json!(100_000_000));
        assert_eq!(head["exe"]["runs"][0]["wall_ns"], json!(10_000_000));
        assert_eq!(head["cache_log"]["run"]["parse"], json!(1));
        assert_eq!(head["cache_log"]["check"]["parse.std"], json!(0));
        assert!(head.get("exe_note").is_none());
        let base = Split {
            warmup: 1,
            check: vec![],
            run: vec![],
            exe: Err("`cheby build -o` failed (exit status: 2): error".to_owned()),
            cache_log: None,
        }
        .to_json();
        assert_eq!(base["exe"], Value::Null);
        assert_eq!(base["cache_log"], Value::Null);
        assert_eq!(
            base["exe_note"],
            json!("`cheby build -o` failed (exit status: 2): error")
        );
        let measured = Measured {
            group: "rc".to_owned(),
            name: "x".to_owned(),
            n: None,
            runs: vec![Duration::from_millis(1)],
            checksum: "1".to_owned(),
            stats: json!({}),
            split: None,
        };
        assert!(measured.to_json().get("split").is_none());
    }

    /// Writes an executable shell script.
    #[cfg(unix)]
    fn script(path: &Path, text: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The whole split against two fake `cheby`s: one with `cheby build -o`
    /// and a cache log, one without either (as `bbed11b`).
    #[cfg(unix)]
    #[test]
    fn bench_split_times_check_run_and_the_executable() {
        let fresh = cheby_test_support::cache::FreshDir::create("bench-split-test").unwrap();
        let root = fresh.path();
        let bench = root.join("rc").join("app");
        std::fs::create_dir_all(&bench).unwrap();
        let measured = root.join("cheby-head");
        script(
            &measured,
            r#"#!/bin/sh
log() { if [ -n "$CHEBY_CACHE_LOG" ]; then printf '%s\n' "$@" > "$CHEBY_CACHE_LOG"; fi; }
case "$2" in
  check) log "front app hit" "parse.std std::list"; exit 0 ;;
  run)
    log "code.run app miss" "const app::X miss" "front app hit" "key code.run app 0" "parse app"
    echo "size: 3"; echo "checksum: 42"
    if [ "$CHEBY_RT_STATS" = 1 ]; then
      echo "runtime stats: main 2.5 ms, 2 entries 3.0 ms, peak 10 bytes" >&2
    fi
    exit 0 ;;
  build)
    printf '%s\n' '#!/bin/sh' 'echo "size: 3"' 'echo "checksum: 42"' \
      'if [ "$CHEBY_RT_STATS" = 1 ]; then echo "runtime stats: main 2.0 ms, 1 entries 2.0 ms, peak 1 bytes" >&2; fi' \
      > "$4"
    chmod +x "$4"; exit 0 ;;
esac
exit 3
"#,
        );
        let base = root.join("cheby-base");
        script(
            &base,
            r#"#!/bin/sh
case "$2" in
  check) exit 0 ;;
  run)
    echo "size: 3"; echo "checksum: 42"
    if [ "$CHEBY_RT_STATS" = 1 ]; then
      echo "runtime stats: main 2.2 ms, 2 entries 2.6 ms, peak 10 bytes" >&2
    fi
    exit 0 ;;
  build) echo "error: unexpected argument '-o' found" >&2; exit 2 ;;
esac
exit 3
"#,
        );
        let scratch = root.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let setup = SplitSetup {
            runs: 2,
            scratch: scratch.clone(),
            prepare: |_| Ok(()),
        };
        let binaries = [(base.as_path(), "42"), (measured.as_path(), "42")];
        let [old, new] = measure_split(binaries, "rc", &bench, &[], &setup).unwrap();
        assert_eq!((new.warmup, new.check.len(), new.run.len()), (1, 2, 2));
        assert_eq!(new.run[0].main_ms, Some(2.5));
        assert_eq!(new.run[1].entries_ms, Some(3.0));
        let exe = new.exe.as_ref().unwrap();
        assert_eq!(exe.runs.len(), 2);
        assert_eq!(exe.runs[0].main_ms, Some(2.0));
        assert!(scratch.join("exe-1").join("app").is_file());
        let (check, run) = new.cache_log.unwrap();
        assert_eq!(
            check,
            LogCounts {
                parse_std: 1,
                ..LogCounts::default()
            }
        );
        assert_eq!(
            run,
            LogCounts {
                parse: 1,
                code_miss: 1,
                const_miss: 1,
                ..LogCounts::default()
            }
        );
        assert_eq!((old.check.len(), old.run.len()), (2, 2));
        assert_eq!(old.run[0].main_ms, Some(2.2));
        let note = old.exe.as_ref().unwrap_err();
        assert!(note.contains("unexpected argument '-o'"), "{note}");
        assert_eq!(old.cache_log, None);
        // Nothing was written into the package.
        assert_eq!(std::fs::read_dir(&bench).unwrap().count(), 0);
        // A run that prints another checksum fails the split.
        let err =
            measure_split([(measured.as_path(), "7")], "rc", &bench, &[], &setup).unwrap_err();
        assert!(err.contains("did not print `checksum: 7`"), "{err}");
    }
}
