//! The naive-versus-Perceus differential (D-718) on generated programs,
//! and the generator's self-check.
//!
//! `rc_differential_*` generates 40 programs from fixed seeds
//! (`cheby_test_support::progen`), checks each with `cheby check` (a
//! program that does not check fails the test: generation is typed by
//! construction), then runs it with the workspace's `cheby` under
//! `CHEBY_RC=naive` and under the default reference counting (Perceus),
//! both with `CHEBY_RT_POISON=1`, and under Perceus once more with
//! `CHEBY_RT_POISON=0`, and compares standard output, standard error and
//! the exit status. Poisoning sends every count and reuse operation
//! through the runtime's checked entries, so the third run is the only
//! one that executes the inline paths code generation emits. Every run
//! must end through `main`: status 0, or 101 when the generator planted
//! the final panic, never 102 (a leak, D-458, D-710).
//!
//! The first Perceus run dumps its MIR (`CHEBY_DUMP_DIR`). Two floors
//! keep the comparison from being vacuous, and when a program falls
//! below one, the generator is extended, never the floor lowered: at
//! least 30 of the 40 programs have a borrowed parameter in a generated
//! function (D-715, D-787, D-809), and at least 25 have a `drop_reuse`
//! in a generated function or a closure of one (D-716, D-844). A
//! separate run of 5 seeds with `CHEBY_RT_STATS=1` must count reuse hits,
//! so that reuse happens at run time too, not only in the MIR.
//!
//! `progen_*` checks the generator itself on 200 seeds: each program
//! checks with no diagnostic at all and finishes within the timeout.
//!
//! With the `@rc=naive` conformance variant (D-853), this test is the
//! oracle of borrowing and reuse on generated programs (D-718, D-844).
//! The packages stay under `CARGO_TARGET_TMPDIR/rc_differential/` for
//! reproduction: run `cheby run` there with and without `CHEBY_RC=naive`,
//! and with `CHEBY_RT_POISON=0`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use cheby_test_support::process;
use cheby_test_support::progen::{self, DiffOptions, GenConfig};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");

/// The seeds of the stable differential.
const DIFF_SEEDS: std::ops::Range<u64> = 1_000..1_040;
/// The seeds of the generator's self-check.
const CHECK_SEEDS: std::ops::Range<u64> = 0..200;
/// The seeds of the reuse-hit run, the first 5 of the differential's.
const STATS_SEEDS: std::ops::Range<u64> = 1_000..1_005;

/// The dump marker of a borrowed parameter in a function header
/// (`crates/cheby_mir/src/dump.rs`, P1b): `fn …(borrow _N: …`. Without
/// the `: ` after the digits, `borrow _N` is a borrowed argument of a
/// call, not a parameter.
const BORROWED_PARAM: &str = "borrow _";

/// The least number of the 40 programs that must have a borrowed
/// parameter in a generated function, so that the differential tests
/// borrowing (D-715, D-787). If fewer do, the generator is extended,
/// never this threshold.
const MIN_BORROWING: usize = 30;

/// The dump form of a reuse source (`crates/cheby_mir/src/dump.rs`,
/// P2): a statement `_N: value = drop_reuse _M of <layout>`.
const DROP_REUSE: &str = "= drop_reuse _";

/// The least number of the 40 programs that must have a `drop_reuse` in
/// a generated function or a closure of one, so that the differential
/// tests reuse (D-716, D-844). If fewer do, the generator is extended,
/// never this threshold.
const MIN_REUSING: usize = 25;

/// The `CHEBY_RT_STATS` line of reuse hits (`crates/cheby_runtime/src/stats.rs`,
/// P2): `  reuse hits: N` on standard error.
const REUSE_HITS: &str = "reuse hits: ";

/// The borrowed parameters of one dumped function header line.
fn borrowed_in_header(line: &str) -> usize {
    line.match_indices(BORROWED_PARAM)
        .filter(|(at, marker)| {
            let rest = line.get(at.saturating_add(marker.len())..).unwrap_or("");
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            digits > 0 && rest.get(digits..).is_some_and(|r| r.starts_with(": "))
        })
        .count()
}

/// Whether a dumped function header belongs to a generated function,
/// `progen::f<N>` or `progen::helpers::h<N>`, and not to `run`, `main`,
/// `finale` or a closure. `run`'s `name` is borrowed in every program, so
/// counting it would make the count say nothing.
fn generated_function(line: &str) -> bool {
    line.strip_prefix("fn ")
        .and_then(|rest| rest.split('(').next())
        .is_some_and(generated_name)
}

/// Whether a dumped function header belongs to a generated function or
/// to a closure or local function of one (`fn closure progen::f3#1(…`),
/// whose bodies the generator's rebuilds also reach.
fn generated_or_closure(line: &str) -> bool {
    if generated_function(line) {
        return true;
    }
    let Some(name) = line
        .strip_prefix("fn closure ")
        .and_then(|rest| rest.split('(').next())
    else {
        return false;
    };
    name.rsplit_once('#').is_some_and(|(owner, index)| {
        !index.is_empty() && index.chars().all(|c| c.is_ascii_digit()) && generated_name(owner)
    })
}

/// `progen::f<N>` or `progen::helpers::h<N>`.
fn generated_name(name: &str) -> bool {
    let last = match name.strip_prefix("progen::helpers::") {
        Some(last) => last.strip_prefix('h'),
        None => name
            .strip_prefix("progen::")
            .and_then(|l| l.strip_prefix('f')),
    };
    last.is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
}

/// The `drop_reuse` statements of one dumped module text that are in a
/// generated function or a closure of one.
fn reuses_in_text(text: &str) -> usize {
    let mut inside = false;
    let mut count = 0usize;
    for line in text.lines() {
        if line.starts_with("fn ") {
            inside = generated_or_closure(line);
        } else if inside && line.contains(DROP_REUSE) {
            count = count.saturating_add(1);
        }
    }
    count
}

#[expect(
    clippy::unwrap_used,
    reason = "a test helper: a bad CHEBY_HARNESS_SCALE fails the test"
)]
fn timeout() -> Duration {
    process::scaled_timeout(Duration::from_secs(120)).unwrap()
}

/// Runs `job` on every seed of `seeds` on a few threads; returns the
/// results in seed order.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: a poisoned lock means a job panicked, which fails the test"
)]
fn parallel<T: Send>(seeds: &[u64], job: impl Fn(u64) -> T + Sync) -> Vec<(u64, T)> {
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(1, 8));
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(&seed) = seeds.get(index) else {
                        break;
                    };
                    let out = job(seed);
                    results.lock().unwrap().push((seed, out));
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|(seed, _)| *seed);
    results
}

#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to clear the directory fails the test"
)]
fn package_dir(kind: &str, seed: u64) -> PathBuf {
    let dir = Path::new(TMP)
        .join("rc_differential")
        .join(kind)
        .join(seed.to_string());
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    dir
}

/// What the dumped MIR of the program's own modules (`mir/progen*.mir`,
/// not the tests units or std) holds.
#[derive(Debug, Clone, Copy, Default)]
struct DumpCounts {
    /// Borrowed parameters of the generated functions.
    borrowed: usize,
    /// `drop_reuse` statements in the generated functions and their
    /// closures.
    reuses: usize,
}

#[expect(
    clippy::unwrap_used,
    reason = "a test helper: an unreadable dump fails the test"
)]
fn dump_counts(dump: &Path) -> DumpCounts {
    let mut counts = DumpCounts::default();
    let Ok(entries) = std::fs::read_dir(dump.join("mir")) else {
        return counts;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.starts_with("progen") || name.ends_with(".tests.mir") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let borrowed: usize = text
            .lines()
            .filter(|line| generated_function(line))
            .map(borrowed_in_header)
            .sum();
        counts.borrowed = counts.borrowed.saturating_add(borrowed);
        counts.reuses = counts.reuses.saturating_add(reuses_in_text(&text));
    }
    counts
}

/// The `reuse hits` count of a `CHEBY_RT_STATS=1` run's standard error,
/// if the line is there.
fn reuse_hits(stderr: &str) -> Option<u64> {
    stderr
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(REUSE_HITS))
        .and_then(|n| n.trim().parse().ok())
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn rc_differential_naive_and_perceus_agree_on_40_programs() {
    let config = GenConfig::default();
    let seeds: Vec<u64> = DIFF_SEEDS.collect();
    let options = |dump: PathBuf| DiffOptions {
        timeout: timeout(),
        dump_dir: Some(dump),
    };
    let results = parallel(&seeds, |seed| {
        let dir = package_dir("diff", seed);
        progen::write_package(&progen::generate(seed, &config), &dir).unwrap();
        let dump = dir.join("dump");
        std::fs::create_dir_all(&dump).unwrap();
        let outcome = progen::differential_with(Path::new(CHEBY), &dir, &options(dump.clone()));
        let counts = dump_counts(&dump);
        (dir, outcome, counts)
    });
    let mut failures = Vec::new();
    let mut planted = 0usize;
    let mut borrowing = 0usize;
    let mut reusing = 0usize;
    let mut lines = 0usize;
    for (seed, (dir, outcome, counts)) in &results {
        match outcome {
            Ok(report) => {
                planted = planted.saturating_add(usize::from(report.planted));
                lines = lines.saturating_add(report.stdout.lines().count());
            }
            Err(e) => failures.push(format!("seed {seed} ({}): {e}", dir.display())),
        }
        borrowing = borrowing.saturating_add(usize::from(counts.borrowed > 0));
        reusing = reusing.saturating_add(usize::from(counts.reuses > 0));
        println!(
            "seed {seed}: {} borrowed parameters in generated functions, {} drop_reuse",
            counts.borrowed, counts.reuses
        );
    }
    println!(
        "{} programs, {planted} with a planted panic, {lines} lines of output, \
         {borrowing} with a borrowed parameter in a generated function, \
         {reusing} with a drop_reuse",
        results.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    assert_eq!(results.len(), 40);
    assert!(
        borrowing >= MIN_BORROWING,
        "only {borrowing} of 40 programs borrow a parameter of a generated function \
         (at least {MIN_BORROWING} must): extend the generator"
    );
    assert!(
        reusing >= MIN_REUSING,
        "only {reusing} of 40 programs have a drop_reuse in a generated function \
         (at least {MIN_REUSING} must): extend the generator"
    );
}

/// Reuse happens at run time too: 5 of the differential's programs, run
/// with `CHEBY_RT_STATS=1` and poisoning off (the inline paths, which
/// count hits behind the counting flag), count reuse hits. Kept apart
/// from the differential because the stats lines go to standard error.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn rc_differential_reuse_hits_in_5_programs() {
    let config = GenConfig::default();
    let seeds: Vec<u64> = STATS_SEEDS.collect();
    let results = parallel(&seeds, |seed| {
        let dir = package_dir("stats", seed);
        let program = progen::generate(seed, &config);
        progen::write_package(&program, &dir).unwrap();
        let mut run = Command::new(CHEBY);
        run.args(["--color=never", "run"])
            .current_dir(&dir)
            .env("NO_COLOR", "1")
            .env("CHEBY_RT_STATS", "1")
            .env("CHEBY_RT_POISON", "0")
            .env_remove("CHEBY_RC")
            .env_remove("CHEBY_DUMP_DIR");
        let ran = process::run(run, None, timeout()).unwrap();
        let expected = if program.planted_panic() { 101 } else { 0 };
        if ran.status != Some(expected) {
            return Err(format!(
                "`cheby run` ended with {}, expected exit code {expected}:\n{}",
                ran.describe_status(),
                ran.stderr
            ));
        }
        reuse_hits(&ran.stderr)
            .ok_or_else(|| format!("no `{REUSE_HITS}` line in the stats:\n{}", ran.stderr))
    });
    let mut failures = Vec::new();
    let mut hits = 0u64;
    for (seed, outcome) in &results {
        match outcome {
            Ok(n) => {
                println!("seed {seed}: {n} reuse hits");
                hits = hits.saturating_add(*n);
            }
            Err(e) => failures.push(format!("seed {seed}: {e}")),
        }
    }
    println!("{hits} reuse hits in {} programs", results.len());
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    assert_eq!(results.len(), 5);
    assert!(
        hits > 0,
        "no reuse hits in the 5 programs: extend the generator"
    );
}

#[test]
fn rc_differential_counts_the_dump_marker() {
    assert_eq!(
        borrowed_in_header("fn progen::f3(borrow _0: value, _1: int, borrow _2: value) -> value {"),
        2
    );
    assert_eq!(
        borrowed_in_header("  _4 = call progen::f3(borrow _3, _1)"),
        0
    );
    assert!(generated_function(
        "fn progen::f12(borrow _0: value) -> int {"
    ));
    assert!(generated_function(
        "fn progen::helpers::h0(_0: value) -> int {"
    ));
    assert!(!generated_function(
        "fn progen::run(borrow _0: value, _1: value) -> value {"
    ));
    assert!(!generated_function(
        "fn progen::finale(_0: int, borrow _1: value) -> int {"
    ));
    assert!(!generated_function(
        "fn closure progen::f0#0(_0: value) -> value {"
    ));
    assert!(generated_or_closure(
        "fn closure progen::f0#0(_0: value) -> value {"
    ));
    assert!(generated_or_closure(
        "fn closure progen::helpers::h2#11(_0: value) -> value {"
    ));
    assert!(generated_or_closure(
        "fn progen::f12(borrow _0: value) -> int {"
    ));
    assert!(!generated_or_closure(
        "fn closure progen::main#1(_0: value, _1: value) -> value {"
    ));
    assert!(!generated_or_closure(
        "fn shim std::int::show(_0: value, _1: value) -> value {"
    ));
    let text = "fn progen::f1(_0: value) -> value {\n\
                bb0:\n  _1: value = drop_reuse _0 of ctor progen::Rec0\n\
                \x20 _2: value = alloc_reuse _1, ctor progen::Rec0(_3)\n}\n\
                fn progen::main() -> value {\n\
                bb0:\n  _1: value = drop_reuse _0 of ctor progen::Rec0\n}\n\
                fn closure progen::f1#0(_0: value) -> value {\n\
                bb0:\n  _4: value = drop_reuse _2 of ctor progen::Sum0::S0B\n}\n";
    assert_eq!(reuses_in_text(text), 2);
    assert_eq!(
        reuse_hits("runtime stats: main 1 ms\n  allocations: 9\n  reuse hits: 12\n"),
        Some(12)
    );
    assert_eq!(reuse_hits("runtime stats: main 1 ms\n"), None);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn progen_200_seeds_check_cleanly_and_finish() {
    let config = GenConfig::default();
    let seeds: Vec<u64> = CHECK_SEEDS.collect();
    let results = parallel(&seeds, |seed| {
        let dir = package_dir("check", seed);
        let program = progen::generate(seed, &config);
        progen::write_package(&program, &dir).unwrap();
        let mut check = Command::new(CHEBY);
        check
            .args(["--color=never", "check"])
            .current_dir(&dir)
            .env("NO_COLOR", "1");
        let checked = process::run(check, None, timeout()).unwrap();
        if checked.status != Some(0) || !checked.stdout.is_empty() || !checked.stderr.is_empty() {
            return Err(format!(
                "`cheby check` ended with {}:\n{}{}",
                checked.describe_status(),
                checked.stdout,
                checked.stderr
            ));
        }
        let mut run = Command::new(CHEBY);
        run.args(["--color=never", "run"])
            .current_dir(&dir)
            .env("NO_COLOR", "1")
            .env("CHEBY_RT_POISON", "1")
            .env_remove("CHEBY_RC");
        let ran = process::run(run, None, timeout()).unwrap();
        let expected = if program.planted_panic() { 101 } else { 0 };
        if ran.status != Some(expected) {
            return Err(format!(
                "`cheby run` ended with {}, expected exit code {expected}:\n{}",
                ran.describe_status(),
                ran.stderr
            ));
        }
        Ok(())
    });
    let failures: Vec<String> = results
        .iter()
        .filter_map(|(seed, r)| r.as_ref().err().map(|e| format!("seed {seed}: {e}")))
        .collect();
    println!(
        "{} of {} seeds check and finish",
        results.len().saturating_sub(failures.len()),
        results.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    assert_eq!(results.len(), 200);
}
