//! The naive-versus-Perceus differential (D-718) on generated programs,
//! and the generator's self-check.
//!
//! `rc_differential_*` generates 40 programs from fixed seeds
//! (`cheby_test_support::progen`), checks each with `cheby check` (a
//! program that does not check fails the test: generation is typed by
//! construction), then runs it with the workspace's `cheby` under
//! `CHEBY_RC=naive` and under the default reference counting, both with
//! `CHEBY_RT_POISON=1`, and compares standard output, standard error and
//! the exit status. Every run must end through `main`: status 0, or 101
//! when the generator planted the final panic, never 102 (a leak, D-458,
//! D-710). The Perceus run dumps its MIR (`CHEBY_DUMP_DIR`), and at least
//! 30 of the 40 programs must have a borrowed parameter in a generated
//! function (D-715, D-787), so that the comparison tests borrowing.
//!
//! `progen_*` checks the generator itself on 200 seeds: each program
//! checks with no diagnostic at all and finishes within the timeout.
//!
//! Until P4's `@rc=naive` conformance variant exists, this test is the
//! oracle of borrowing (D-718). The packages stay under
//! `CARGO_TARGET_TMPDIR/rc_differential/` for reproduction: run `cheby
//! run` there with and without `CHEBY_RC=naive`.

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
    let Some(name) = line
        .strip_prefix("fn ")
        .and_then(|rest| rest.split('(').next())
    else {
        return false;
    };
    let last = match name.strip_prefix("progen::helpers::") {
        Some(last) => last.strip_prefix('h'),
        None => name
            .strip_prefix("progen::")
            .and_then(|l| l.strip_prefix('f')),
    };
    last.is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
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

/// The number of borrowed parameters of the generated functions in the
/// dumped MIR of the program's own modules (`mir/progen*.mir`, not the
/// tests units or std).
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: an unreadable dump fails the test"
)]
fn borrowed_params(dump: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dump.join("mir")) else {
        return 0;
    };
    let mut count = 0usize;
    for entry in entries {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.starts_with("progen") || name.ends_with(".tests.mir") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let found: usize = text
            .lines()
            .filter(|line| generated_function(line))
            .map(borrowed_in_header)
            .sum();
        count = count.saturating_add(found);
    }
    count
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
        let borrowed = borrowed_params(&dump);
        (dir, outcome, borrowed)
    });
    let mut failures = Vec::new();
    let mut planted = 0usize;
    let mut borrowing = 0usize;
    let mut lines = 0usize;
    for (seed, (dir, outcome, borrowed)) in &results {
        match outcome {
            Ok(report) => {
                planted = planted.saturating_add(usize::from(report.planted));
                lines = lines.saturating_add(report.stdout.lines().count());
            }
            Err(e) => failures.push(format!("seed {seed} ({}): {e}", dir.display())),
        }
        borrowing = borrowing.saturating_add(usize::from(*borrowed > 0));
        println!("seed {seed}: {borrowed} borrowed parameters in generated functions");
    }
    println!(
        "{} programs, {planted} with a planted panic, {lines} lines of output, \
         {borrowing} with a borrowed parameter in a generated function",
        results.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    assert_eq!(results.len(), 40);
    assert!(
        borrowing >= MIN_BORROWING,
        "only {borrowing} of 40 programs borrow a parameter of a generated function \
         (at least {MIN_BORROWING} must): extend the generator"
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
