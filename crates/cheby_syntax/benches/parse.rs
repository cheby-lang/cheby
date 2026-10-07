//! Syntax-layer benchmark (A3, testing.md §7): lexes and parses projects
//! from `bench/gen` and reports throughput and tree size.
//!
//! Run with `cargo bench -p cheby_syntax`. Pass a number to change the
//! line counts, such as `cargo bench -p cheby_syntax -- 1000000`. Each
//! measurement is the fastest of a few runs, single-threaded.

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cheby_bench_gen::{Config, Shape};
use cheby_diag::FileId;
use cheby_syntax::lexer::lex;
use cheby_syntax::{TreeStats, parse};

const RUNS: usize = 5;

fn main() {
    // `cargo bench` passes `--bench`; a number sets the largest size.
    let largest = std::env::args()
        .skip(1)
        .find_map(|a| a.parse::<usize>().ok())
        .unwrap_or(100_000);
    let mut sizes = vec![10_000, largest];
    sizes.dedup();
    for shape in [Shape::Small, Shape::Large, Shape::Generic] {
        for &lines in &sizes {
            bench(shape, lines);
        }
    }
}

fn bench(shape: Shape, lines: usize) {
    let project = cheby_bench_gen::generate(&Config {
        lines,
        shape,
        seed: 1,
    });
    let files: Vec<Arc<str>> = project
        .files
        .iter()
        .filter(|(path, _)| {
            std::path::Path::new(path)
                .extension()
                .is_some_and(|ext| ext == "cheby")
        })
        .map(|(_, text)| Arc::from(text.as_str()))
        .collect();
    let bytes: usize = files.iter().map(|t| t.len()).sum();
    let line_count: usize = files.iter().map(|t| t.lines().count()).sum();

    let lexing = fastest(|| {
        for text in &files {
            black_box(lex(FileId::new(0), text));
        }
    });
    let mut stats = TreeStats::default();
    let parsing = fastest(|| {
        stats = TreeStats::default();
        for text in &files {
            let parse = parse(FileId::new(0), Arc::clone(text));
            assert!(parse.errors().is_empty(), "generated code must parse");
            let s = parse.tree().stats();
            stats.nodes = stats.nodes.saturating_add(s.nodes);
            stats.tokens = stats.tokens.saturating_add(s.tokens);
            stats.heap_bytes = stats.heap_bytes.saturating_add(s.heap_bytes);
            black_box(parse);
        }
    });

    println!(
        "{:<8} {:>8} lines {:>6} files {:>6.1} MB | lex {:>7.2} ms {:>7.1} MB/s | \
         lex+parse+tree {:>7.2} ms {:>6.2} M lines/s {:>7.1} MB/s | {} nodes, {} tokens, {:.1} MB tree",
        shape.name(),
        line_count,
        files.len(),
        mb(bytes),
        ms(lexing),
        mb(bytes) / lexing.as_secs_f64(),
        ms(parsing),
        lossy(line_count) / 1e6 / parsing.as_secs_f64(),
        mb(bytes) / parsing.as_secs_f64(),
        stats.nodes,
        stats.tokens,
        mb(stats.heap_bytes),
    );
}

fn fastest(mut f: impl FnMut()) -> Duration {
    (0..RUNS)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed()
        })
        .min()
        .unwrap_or_default()
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn mb(bytes: usize) -> f64 {
    lossy(bytes) / 1e6
}

#[expect(clippy::cast_precision_loss, reason = "sizes for display only")]
#[expect(clippy::as_conversions, reason = "sizes for display only")]
fn lossy(n: usize) -> f64 {
    n as f64
}
