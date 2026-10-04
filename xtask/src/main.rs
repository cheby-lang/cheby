//! Developer commands (docs/plan/m0-foundation.md, task 0.10):
//!
//! - `bless`: rewrites the goldens of examples and UI tests from the
//!   current `cheby` binary.
//! - `gen-bench`: generates a synthetic benchmark project.
//! - `check-determinism`: checks that outputs are the same byte for byte
//!   from different paths (D-264).
//! - `coverage`: lists decisions cited in the spec that no UI test names.
//! - `cross-check`: parses examples and UI tests with `tree-sitter-cheby`
//!   at the pinned commit (D-272).

mod cross_check;

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
        /// Approximate number of lines
        #[arg(long, default_value_t = 100_000)]
        lines: usize,
        /// small, large, deep, wide or generic
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
    /// Check that outputs do not depend on the absolute path (D-264)
    CheckDeterminism {
        /// Lines per generated project
        #[arg(long, default_value_t = 50_000)]
        lines: usize,
    },
    /// List decisions cited in the spec that no UI test names
    Coverage,
    /// Parse examples and UI tests with tree-sitter-cheby at the pinned commit
    CrossCheck(cross_check::Args),
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
        Cmd::CheckDeterminism { lines } => check_determinism(&root, lines),
        Cmd::Coverage => coverage(&root),
        Cmd::CrossCheck(args) => cross_check::run(&root, &args),
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

/// Before M1 there is no compiler output, so this checks the benchmark
/// generator: each shape is generated into two different directories and
/// the trees are compared. From M1 it also compares interface and
/// Cranelift IR dumps of the examples (docs/plan/testing.md §6).
fn check_determinism(root: &Path, lines: usize) -> Result<(), String> {
    let base = root.join("target/determinism");
    if base.exists() {
        std::fs::remove_dir_all(&base).map_err(|e| format!("{}: {e}", base.display()))?;
    }
    let mut failures: usize = 0;
    for shape in Shape::ALL {
        let lines = if shape == Shape::Large {
            lines.max(40_000)
        } else {
            lines
        };
        let config = Config {
            lines,
            shape,
            seed: 1,
        };
        let a = base.join("a").join(shape.name());
        let b = base.join("b/nested").join(shape.name());
        for dir in [&a, &b] {
            cheby_bench_gen::generate(&config)
                .write(dir)
                .map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        match compare_trees(&a, &b)? {
            None => println!("ok    bench `{}`: identical", shape.name()),
            Some(diff) => {
                failures = failures.saturating_add(1);
                println!("FAIL  bench `{}`: {diff}", shape.name());
            }
        }
    }
    println!("compiler artifacts: none before M1, nothing else to compare");
    match failures {
        0 => Ok(()),
        n => Err(format!("{n} outputs are not deterministic")),
    }
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
    let uncovered: Vec<_> = cited.difference(&covered).copied().collect();
    let unknown: Vec<_> = covered.difference(&cited).copied().collect();
    println!(
        "D-number coverage: {}/{} decisions cited in the spec have a UI test",
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
