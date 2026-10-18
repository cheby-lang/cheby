//! Rust baselines for the `collections` benchmark group (D-664,
//! `bench/runtime/README.md`).
//!
//! `cheby_bench_baselines <program> <variant> [n]` does the work of
//! `bench/runtime/collections/<program>` in Rust and prints the same
//! `size: <n>` and `checksum: <n>` lines; `n` defaults to the Cheby
//! program's default size. Every variant computes the same checksum as
//! the Cheby program. `cheby_bench_baselines --list` prints one
//! `<program> <variant>` line per baseline, which `cargo xtask bench-run
//! --rust` reads.
//!
//! The variants:
//!
//! - `rc`: a persistent weight-balanced tree with `Rc` nodes, the
//!   algorithm of `std/sorted_map.cheby`, which copies every path it
//!   changes (D-039: a collection stays persistent). The sorted programs
//!   use it as the Cheby modules do; the ordered programs and `multimap`
//!   use it where the Cheby modules use the runtime's `Map`.
//! - `btree`: std's `BTreeMap` and `BTreeSet`, mutable (the sorted
//!   family).
//! - `hash-btree`: a `HashMap` index with a `BTreeMap` of insertion
//!   places, mutable (the ordered family).
//! - `hash-vec`: a `HashMap<K, Vec<V>>`, mutable (`multimap`).

mod common;
mod multimap;
mod ordered;
mod sorted;
mod wbt;

use std::process::ExitCode;

/// One baseline: a variant of a program and the function that runs it.
type Variant = (&'static str, fn(i64) -> i64);

/// Each program, its default size (the Cheby program's `default_size`)
/// and its variants.
const PROGRAMS: &[(&str, i64, &[Variant])] = &[
    (
        "multimap",
        1_600_000,
        &[
            ("rc", multimap::multimap_rc),
            ("hash-vec", multimap::multimap_hash_vec),
        ],
    ),
    (
        "ordered_map",
        450_000,
        &[
            ("rc", ordered::ordered_map_rc),
            ("hash-btree", ordered::ordered_map_hash_btree),
        ],
    ),
    (
        "ordered_multimap",
        1_100_000,
        &[
            ("rc", ordered::ordered_multimap_rc),
            ("hash-btree", ordered::ordered_multimap_hash_btree),
        ],
    ),
    (
        "ordered_set",
        700_000,
        &[
            ("rc", ordered::ordered_set_rc),
            ("hash-btree", ordered::ordered_set_hash_btree),
        ],
    ),
    (
        "sorted_map",
        600_000,
        &[
            ("rc", sorted::sorted_map_rc),
            ("btree", sorted::sorted_map_btree),
        ],
    ),
    (
        "sorted_multimap",
        800_000,
        &[
            ("rc", sorted::sorted_multimap_rc),
            ("btree", sorted::sorted_multimap_btree),
        ],
    ),
    (
        "sorted_set",
        300_000,
        &[
            ("rc", sorted::sorted_set_rc),
            ("btree", sorted::sorted_set_btree),
        ],
    ),
];

const USAGE: &str = "usage: cheby_bench_baselines <program> <variant> [n]\n       \
                     cheby_bench_baselines --list";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("cheby_bench_baselines: {message}\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// What the command prints for `args`, or why it cannot run.
fn run(args: &[String]) -> Result<String, String> {
    match args {
        [flag] if flag == "--list" => Ok(PROGRAMS
            .iter()
            .flat_map(|(program, _, variants)| {
                variants
                    .iter()
                    .map(move |(variant, _)| format!("{program} {variant}\n"))
            })
            .collect()),
        [program, variant, rest @ ..] if rest.len() <= 1 => {
            let (_, default, variants) = PROGRAMS
                .iter()
                .find(|(name, _, _)| name == program)
                .ok_or_else(|| format!("no program `{program}`"))?;
            let (_, work) = variants
                .iter()
                .find(|(name, _)| name == variant)
                .ok_or_else(|| format!("`{program}` has no variant `{variant}`"))?;
            let n = match rest.first() {
                Some(text) => text
                    .parse::<i64>()
                    .ok()
                    .filter(|n| *n >= 0)
                    .ok_or_else(|| format!("`{text}` is not a size"))?,
                None => *default,
            };
            let checksum = work(n);
            Ok(format!("size: {n}\nchecksum: {checksum}\n"))
        }
        _ => Err("wrong arguments".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Cheby program's `default_size`.
    fn cheby_default(program: &str) -> i64 {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../runtime/collections")
            .join(program)
            .join("src/main.cheby");
        let text = std::fs::read_to_string(&path).unwrap();
        let line = text
            .lines()
            .find_map(|line| line.strip_prefix("const default_size: Int = "))
            .unwrap();
        line.trim().replace('_', "").parse().unwrap()
    }

    #[test]
    fn every_program_has_the_cheby_programs_default_size() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../runtime/collections");
        let mut on_disk: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        on_disk.sort();
        let listed: Vec<&str> = PROGRAMS.iter().map(|(name, _, _)| *name).collect();
        assert_eq!(on_disk, listed);
        for (program, default, _) in PROGRAMS {
            assert_eq!(cheby_default(program), *default, "{program}");
        }
    }

    /// Checksums the Cheby programs printed for small sizes (`cheby run --
    /// <n>`, the same under `CHEBY_RC` unset and `naive`): every variant
    /// must print them. `cargo xtask bench-run --rust` checks the default
    /// sizes the same way.
    const CHEBY_CHECKSUMS: &[(&str, i64, i64)] = &[
        ("multimap", 17, 366_796_766),
        ("multimap", 3000, 415_181_071),
        ("ordered_map", 17, 253_657_728),
        ("ordered_map", 3000, 550_711_720),
        ("ordered_multimap", 17, 839_548_719),
        ("ordered_multimap", 3000, 322_801_727),
        ("ordered_set", 17, 8_436_842),
        ("ordered_set", 3000, 771_899_310),
        ("sorted_map", 17, 20_968_059),
        ("sorted_map", 3000, 151_621_499),
        ("sorted_multimap", 17, 769_016_426),
        ("sorted_multimap", 3000, 239_815_810),
        ("sorted_set", 17, 484_221_194),
        ("sorted_set", 3000, 469_561_237),
    ];

    #[test]
    fn every_variant_prints_the_cheby_programs_checksum() {
        for (program, n, expected) in CHEBY_CHECKSUMS {
            let (_, _, variants) = PROGRAMS
                .iter()
                .find(|(name, _, _)| name == program)
                .unwrap();
            for (variant, work) in *variants {
                assert_eq!(work(*n), *expected, "{program} {variant} at {n}");
            }
        }
    }

    #[test]
    fn arguments_are_checked() {
        let args = |words: &[&str]| words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        assert!(run(&args(&["--list"])).unwrap().contains("sorted_map rc\n"));
        assert!(run(&args(&["nothing", "rc"])).is_err());
        assert!(run(&args(&["sorted_map", "nothing"])).is_err());
        assert!(run(&args(&["sorted_map", "rc", "-1"])).is_err());
        assert!(run(&args(&[])).is_err());
        assert_eq!(
            run(&args(&["sorted_map", "btree", "0"])).unwrap(),
            "size: 0\nchecksum: 0\n"
        );
    }
}
