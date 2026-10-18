//! Developer commands (docs/plan/m0-foundation.md, task 0.10):
//!
//! - `bless`: rewrites the goldens of examples and UI tests from the
//!   current `cheby` binary, including the goldens beside package UI tests
//!   (directories with their own `cheby.toml`). An example's `cheby test`
//!   trial has no goldens, so blessing only runs it.
//! - `gen-bench`: generates a synthetic benchmark project.
//! - `check-determinism`: checks that outputs are the same byte for byte
//!   from different paths (D-264).
//! - `coverage`: lists decisions cited in the spec that no UI test or REPL
//!   transcript names, reading `@decisions` from each test's header
//!   (`src/main.cheby` of a package test) and from each transcript's
//!   header (D-638).
//! - `cross-check`: parses examples and UI tests with `tree-sitter-cheby`
//!   at the pinned commit and with the compiler parser, and fails if they
//!   disagree (D-272).
//! - `test-std`: checks every shipped std module with its `test` blocks
//!   and runs them, one batch per module (D-479, D-542).
//! - `bench-compile`: compile-speed numbers on generated projects (I1,
//!   D-247, plan §8.5), with the size of the code and its RC operations
//!   (D-718).
//! - `bench-run`: run-time numbers of the programs under `bench/runtime`
//!   against a baseline (D-718, D-702).
//! - `check-examples`: runs `cheby check` in every example directory and
//!   compares the outcome with the expected one (G6).
//! - `sanitize`: the nightly workflow's thread-sanitizer, address-sanitizer
//!   and Miri jobs, step by step, for the workflow and for local runs
//!   (D-721, D-862).
//! - `bench-build` (hidden): checks and compiles every function of a
//!   package on `--jobs` threads and prints the times, the bytes of code
//!   and the RC operations; `bench-compile` and `check-determinism` run it
//!   as a child process, since the size of rayon's global pool is fixed
//!   once per process.

mod bench;
mod bench_run;
mod cross_check;
mod determinism;
mod examples;
mod sanitize;

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use cheby_bench_gen::{Config, Shape};
use cheby_test_support::{decisions, repo_root, ui};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "cargo xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Rewrite goldens of examples and UI tests from the current `cheby`
    Bless {
        /// Only bless tests whose name contains this filter
        filter: Option<String>,
    },
    /// Generate a synthetic benchmark project
    GenBench {
        /// Approximate number of lines; for a `huge_*` shape, the number of
        /// elements of its one literal
        #[arg(long, default_value_t = 100_000)]
        lines: usize,
        /// small, large, deep, wide or generic, or one of the opt-in literal
        /// shapes `huge_list`, `huge_interpolation`, `huge_map` or `huge_tuple`
        #[arg(long, default_value = "small")]
        shape: Shape,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Output directory, which must not exist yet
        #[arg(long)]
        out: PathBuf,
        /// Append a Markdown summary to this file, such as
        /// `$GITHUB_STEP_SUMMARY`
        #[arg(long)]
        summary: Option<PathBuf>,
    },
    /// Check that outputs depend neither on the absolute path nor on the
    /// number of threads (D-264, D-551)
    CheckDeterminism {
        /// Lines per generated project (at least 40k for `large`)
        #[arg(long, default_value_t = 20_000)]
        lines: usize,
        /// Only build the examples and projects whose name contains this
        filter: Option<String>,
    },
    /// Run `cheby check` in every example and compare with the expected
    /// outcome
    CheckExamples,
    /// List decisions cited in the spec that no UI test names
    Coverage,
    /// Parse examples and UI tests with tree-sitter-cheby at the pinned
    /// commit and with the compiler parser, and compare
    CrossCheck(cross_check::Args),
    /// Check and run the standard library's own tests, one module at a time
    TestStd {
        /// Only run tests whose full name, `std::<module> "<name>"`, contains this text
        filter: Option<String>,
    },
    /// Measure compile speed on generated projects (D-247)
    BenchCompile(bench::Args),
    /// Measure the run time of the programs under `bench/runtime` (D-718)
    BenchRun(bench_run::Args),
    /// Run the nightly workflow's sanitizer or Miri job, or some of its
    /// steps, on the pinned nightly (D-721, D-862)
    Sanitize(sanitize::Args),
    /// Check and compile every function of a package and print the front
    /// end's and the back end's times in nanoseconds, the bytes of machine
    /// code and the RC operations of the MIR
    #[command(hide = true)]
    BenchBuild {
        /// Worker threads (default: the available parallelism)
        #[arg(long)]
        jobs: Option<NonZeroUsize>,
        /// The package directory
        dir: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = repo_root();
    let result = match cli.command {
        Cmd::Bless { filter } => bless(&root, filter.as_deref()),
        Cmd::GenBench {
            lines,
            shape,
            seed,
            out,
            summary,
        } => gen_bench(&Config { lines, shape, seed }, &out, summary.as_deref()),
        Cmd::CheckDeterminism { lines, filter } => {
            determinism::run(&root, lines, filter.as_deref())
        }
        Cmd::CheckExamples => examples::run(&root),
        Cmd::Coverage => coverage(&root),
        Cmd::CrossCheck(args) => cross_check::run(&root, &args),
        Cmd::TestStd { filter } => test_std(filter.as_deref()),
        Cmd::BenchCompile(args) => bench::run(&root, &args),
        Cmd::BenchRun(args) => bench_run::run(&root, &args),
        Cmd::Sanitize(args) => sanitize::run(&root, &args),
        Cmd::BenchBuild { jobs, dir } => bench_build(jobs, &dir),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn bless(root: &Path, filter: Option<&str>) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(root)
        .args([
            "test",
            "--package",
            "cheby_cli",
            "--test",
            "conformance",
            "--test",
            "ui",
            "--",
        ])
        .env("CHEBY_BLESS", "1");
    if let Some(filter) = filter {
        cmd.arg(filter);
    }
    let status = cmd.status().map_err(|e| format!("cannot run cargo: {e}"))?;
    if status.success() {
        println!("goldens blessed; review the diff like code");
        Ok(())
    } else {
        Err("blessing failed; see the test output above".to_owned())
    }
}

/// Tests the standard library (D-542) and prints the report: a line per
/// test, the modules and tests that did not build with their codes, a
/// block per failure, and the counts per module and in total. Fails when a
/// test failed or did not build.
fn test_std(filter: Option<&str>) -> Result<(), String> {
    let report = cheby_driver::test_std(filter).map_err(|e| e.message().to_owned())?;
    print!("{}", report.render());
    if report.success() {
        Ok(())
    } else {
        let counts = report.counts();
        Err(format!(
            "{} std tests failed and {} did not build",
            counts.failed, counts.not_built
        ))
    }
}

fn gen_bench(config: &Config, out: &Path, summary: Option<&Path>) -> Result<(), String> {
    if out.exists() {
        return Err(format!("{} already exists; remove it first", out.display()));
    }
    let start = Instant::now();
    let project = cheby_bench_gen::generate(config);
    project
        .write(out)
        .map_err(|e| format!("{}: {e}", out.display()))?;
    let elapsed = start.elapsed();
    let line = format!(
        "generated {} lines ({} bytes) in {} modules, shape `{}`, seed {}, in {:.2} s",
        project.lines(),
        project.bytes(),
        project.modules(),
        config.shape.name(),
        config.seed,
        elapsed.as_secs_f64(),
    );
    println!("{line} -> {}", out.display());
    if let Some(summary) = summary {
        append(summary, &format!("- {line}\n"))?;
    }
    Ok(())
}

/// Checks and compiles every function of the package in `dir` with
/// `jobs` worker threads and prints `<front end ns> <back end ns> <code
/// bytes> <RC operations>`.
fn bench_build(jobs: Option<NonZeroUsize>, dir: &Path) -> Result<(), String> {
    cheby_driver::configure_jobs(jobs).map_err(|e| e.message().to_owned())?;
    let t = cheby_driver::bench_build(dir).map_err(|e| e.message().to_owned())?;
    println!(
        "{} {} {} {}",
        t.front_end.as_nanos(),
        t.back_end.as_nanos(),
        t.code_bytes,
        t.rc_ops
    );
    Ok(())
}

/// `path` made absolute against the current directory.
fn absolute(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Builds `cheby` with the profile this xtask was built with and returns
/// its path.
fn build_cheby(root: &Path) -> Result<PathBuf, String> {
    build_cheby_profile(root, !cfg!(debug_assertions))
}

/// Builds `cheby`, optimized if `release`, and returns its path.
fn build_cheby_profile(root: &Path, release: bool) -> Result<PathBuf, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(root)
        .args(["build", "--quiet", "--package", "cheby_cli"]);
    if release {
        cmd.arg("--release");
    }
    let status = cmd.status().map_err(|e| format!("cannot run cargo: {e}"))?;
    if !status.success() {
        return Err("cannot build `cheby`".to_owned());
    }
    let target =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let name = format!("cheby{}", std::env::consts::EXE_SUFFIX);
    Ok(target
        .join(if release { "release" } else { "debug" })
        .join(name))
}

/// Describes the first difference between two directory trees.
fn compare_trees(a: &Path, b: &Path) -> Result<Option<String>, String> {
    let list = |dir: &Path| -> Result<Vec<PathBuf>, String> {
        let files =
            cheby_test_support::files_with_extension(dir, "cheby").map_err(|e| e.to_string())?;
        files
            .into_iter()
            .map(|f| {
                f.strip_prefix(dir)
                    .map(Path::to_path_buf)
                    .map_err(|e| format!("{}: {e}", f.display()))
            })
            .collect()
    };
    let (files_a, files_b) = (list(a)?, list(b)?);
    if files_a != files_b {
        return Ok(Some("different file lists".to_owned()));
    }
    for file in files_a
        .iter()
        .map(PathBuf::as_path)
        .chain([Path::new("cheby.toml")])
    {
        let read = |dir: &Path| std::fs::read(dir.join(file)).map_err(|e| e.to_string());
        if read(a)? != read(b)? {
            return Ok(Some(format!("{} differs", file.display())));
        }
    }
    Ok(None)
}

fn coverage(root: &Path) -> Result<(), String> {
    let spec = cheby_test_support::files_with_extension(&root.join("docs/spec"), "md")
        .map_err(|e| e.to_string())?;
    let mut cited = std::collections::BTreeSet::new();
    for file in spec {
        let text =
            std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        cited.extend(decisions::mentioned_in(&text));
    }
    let mut covered = std::collections::BTreeSet::new();
    for test in ui::all(root)? {
        covered.extend(test.header.decisions);
    }
    // REPL transcripts pin decisions through their header too (D-638).
    let transcripts = root.join("tests/repl");
    for path in cheby_test_support::files_with_extension(&transcripts, "repl")
        .map_err(|e| format!("{}: {e}", transcripts.display()))?
    {
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let header = cheby_test_support::repl::Header::of(&text)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        covered.extend(header.decisions);
    }
    let uncovered: Vec<_> = cited.difference(&covered).copied().collect();
    let unknown: Vec<_> = covered.difference(&cited).copied().collect();
    println!(
        "D-number coverage: {}/{} decisions cited in the spec have a UI test or a REPL transcript",
        cited.len().saturating_sub(uncovered.len()),
        cited.len()
    );
    print_wrapped("uncovered", &uncovered);
    // Plan-level and tooling decisions may be tested without being cited
    // in the spec, so this is informative only.
    print_wrapped("named by UI tests but not cited in the spec", &unknown);
    Ok(())
}

fn print_wrapped(label: &str, numbers: &[u32]) {
    if numbers.is_empty() {
        return;
    }
    println!("{label} ({}):", numbers.len());
    for chunk in numbers.chunks(12) {
        let line: Vec<_> = chunk.iter().map(|&n| decisions::format(n)).collect();
        println!("  {}", line.join(" "));
    }
}

fn append(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|e| format!("{}: {e}", path.display()))
}
