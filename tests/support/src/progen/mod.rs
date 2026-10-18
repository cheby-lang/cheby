//! The program generator and the naive-versus-Perceus differential
//! (D-718, D-719).
//!
//! [`generate`] builds a well-typed, terminating Cheby package from a
//! seed ([`generate_from_bytes`] from a fuzzer's bytes); [`differential`]
//! checks it with `cheby check`, runs it with `cheby run` under
//! `CHEBY_RC=naive` and under the default reference counting (Perceus),
//! the latter also with poisoning off, and compares the three runs. The stable test
//! `crates/cheby_cli/tests/rc_differential.rs` runs it on fixed seeds in
//! every CI run, and the fuzz target `fuzz/fuzz_targets/rc_differential.rs`
//! drives the generator from the fuzzer's bytes (D-781).
//!
//! D-809: the generator lives in the workspace's
//! test-support crate on stable Rust, not in `fuzz/`, so that the seeded
//! differential runs on every CI run and the nightly fuzz target shares
//! it.
//!
//! What a program does is described in `generator.rs`: `Int`
//! arithmetic that may overflow or divide by zero, strings and
//! interpolation, lists with `[x, ..rest]`, `std::map`, records and sum
//! types, `priv`, package-visible and `pub` functions (some in a second
//! module, called from the first), tail and non-tail recursion bounded
//! by counters, local functions, closures, `case` with guards, and an
//! optional planted panic at the end of `main`. Values are never
//! handles, so every function is a candidate for borrowing (D-715,
//! D-787).
//!
//! Programs also rebuild values, so that the differential covers reuse
//! (D-716, spec §9.3): record updates whose base is at its last use,
//! `case` arms that build the matched variant again with one field
//! changed, and bounded recursive rebuilds (a record or a variant
//! rebuilt on each step of a counted recursion, or a recursive field
//! rebuilt around the recursive call, as a tree insert does).
//!
//! Most programs also hold a projection run (D-871):
//! a function reading 9 to 14 projections of one record that it consumes
//! after the record's last use, sometimes in one arm of a `case` only.
//! Fusion sinks at most 8 `dup`s to one point that neither cancel nor
//! fold there (D-865); the run goes beyond that bound, so the
//! differential and the fuzz target reach it.

mod diff;
mod generator;
mod model;
mod source;

use std::path::Path;

pub use diff::{DiffError, DiffErrorKind, DiffOptions, DiffReport, differential_with};

/// The line of the main module's documentation that marks a program
/// whose `main` ends with a planted panic, so that it exits with 101.
pub const PLANTED_MARKER: &str = "It ends with a planted panic.";

/// Sizes and switches of the generator. [`Default`] gives the stable
/// test's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenConfig {
    /// The most top-level functions of the main module (at least 1).
    pub functions: usize,
    /// The most functions of the helper module; 0 leaves the module out.
    pub helper_functions: usize,
    /// The most record types (at least 1).
    pub records: usize,
    /// The most randomly drawn sum types (at least 1). Every program
    /// also declares one more sum type, not counted here: the
    /// shared-field type whose variants hold `k0` alike and differ in
    /// size (`generator::shared_rebuild`, D-071).
    pub sums: usize,
    /// How deep expressions nest.
    pub max_depth: u32,
    /// The most statements before a function's result.
    pub max_stmts: usize,
    /// The chance, in percent, that `main` ends with a planted panic.
    pub final_panic_percent: usize,
    /// The chance, in percent, that the program holds a projection run:
    /// a function that reads 9 to 14 projections of one record and
    /// consumes them after the record's last use, sometimes in one arm of
    /// a `case` only, so that fusion's bound on sinking (D-865) is
    /// reached. It is drawn after every other choice, so the rest of the
    /// program is the one this seed gives without it.
    pub projection_run_percent: usize,
}

impl Default for GenConfig {
    fn default() -> Self {
        Self {
            functions: 8,
            helper_functions: 4,
            records: 3,
            sums: 2,
            max_depth: 3,
            max_stmts: 4,
            final_panic_percent: 30,
            projection_run_percent: 60,
        }
    }
}

/// A generated package: relative path to text (`cheby.toml`,
/// `src/main.cheby`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub seed: u64,
    pub files: Vec<(String, String)>,
}

impl Program {
    /// The text of the file at `path`, if the program has one.
    #[must_use]
    pub fn file(&self, path: &str) -> Option<&str> {
        self.files
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, text)| text.as_str())
    }

    /// Whether `main` ends with a planted panic (status 101).
    #[must_use]
    pub fn planted_panic(&self) -> bool {
        self.file("src/main.cheby")
            .is_some_and(|text| text.contains(PLANTED_MARKER))
    }
}

/// The manifest of every generated package.
const MANIFEST: &str = "name = \"progen\"\nversion = \"0.1.0\"\n";

fn package(seed: u64, output: generator::Output) -> Program {
    let mut files = vec![
        ("cheby.toml".to_owned(), MANIFEST.to_owned()),
        ("src/main.cheby".to_owned(), output.main),
    ];
    if let Some(helpers) = output.helpers {
        files.push(("src/helpers.cheby".to_owned(), helpers));
    }
    let _ = (output.planted, output.rebuilds, output.projection_runs);
    Program { seed, files }
}

/// The same seed and config always give the same program.
#[must_use]
pub fn generate(seed: u64, config: &GenConfig) -> Program {
    let mut generator = generator::Gen::new(source::Source::Seeded(seed), config);
    let output = generator.program(&format!("seed {seed}"));
    package(seed, output)
}

/// A program chosen by `data`, a fuzzer's input: each byte drives one
/// choice, and once the bytes run out every choice takes its simplest
/// option. The same bytes always give the same program; its `seed` is a
/// hash of the bytes, for messages.
#[must_use]
pub fn generate_from_bytes(data: &[u8], config: &GenConfig) -> Program {
    // FNV-1a, so the name does not depend on the platform.
    let hash = data.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    });
    let mut generator = generator::Gen::new(
        source::Source::Bytes {
            data: data.to_vec(),
            pos: 0,
        },
        config,
    );
    let output = generator.program(&format!("fuzz input {hash:016x}"));
    package(hash, output)
}

/// Writes `program`'s files under `dir`, creating directories. Files of
/// an earlier program in `dir` that this one does not have are removed
/// from `dir/src`.
///
/// # Errors
///
/// The first error from creating a directory or writing a file.
pub fn write_package(program: &Program, dir: &Path) -> std::io::Result<()> {
    let src = dir.join("src");
    if src.is_dir() {
        for entry in std::fs::read_dir(&src)? {
            let path = entry?.path();
            let relative = format!(
                "src/{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            if program.file(&relative).is_none() && path.is_file() {
                std::fs::remove_file(&path)?;
            }
        }
    }
    for (path, text) in &program.files {
        let path = dir.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)?;
    }
    Ok(())
}

/// Runs `cheby check`, then `cheby run` in `dir` under `CHEBY_RC=naive`
/// and without it (both with `CHEBY_RT_POISON=1`), and once more without
/// it and with `CHEBY_RT_POISON=0`, and compares stdout, stderr and the
/// exit status; `Err` describes a program that does not check, the first
/// difference, a status other than 0 or a planted 101, or a 102.
/// `cheby` is always a path the caller gives (`CARGO_BIN_EXE_cheby`,
/// `CHEBY_DIFF_BIN`).
///
/// # Errors
///
/// As above.
pub fn differential(cheby: &Path, dir: &Path) -> Result<(), String> {
    differential_with(cheby, dir, &DiffOptions::default())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::{GenConfig, generate, generate_from_bytes, generator, source};

    /// The seeds of `rc_differential`'s 40 programs
    /// (`crates/cheby_cli/tests/rc_differential.rs`).
    const DIFF_SEEDS: std::ops::Range<u64> = 1_000..1_040;

    /// D-716 through the generator: most of the differential's programs
    /// hold a rebuild (a record update of a value at its last use, or a
    /// matched variant built again), so that `rc_differential`'s floor
    /// of 25 programs with a `drop_reuse` has a margin.
    #[test]
    fn progen_most_programs_rebuild() {
        let config = GenConfig::default();
        let rebuilding = DIFF_SEEDS
            .filter(|seed| {
                let mut generator = generator::Gen::new(source::Source::Seeded(*seed), &config);
                generator.program("test").rebuilds > 0
            })
            .count();
        assert!(rebuilding >= 34, "only {rebuilding} of 40 programs rebuild");
    }

    /// D-865 through the generator (D-871): many of
    /// the differential's programs hold a projection run, so that
    /// `rc_differential`'s floor of 10 has a margin.
    #[test]
    fn progen_many_programs_hold_a_projection_run() {
        let config = GenConfig::default();
        let holding = DIFF_SEEDS
            .filter(|seed| {
                let mut generator = generator::Gen::new(source::Source::Seeded(*seed), &config);
                generator.program("test").projection_runs > 0
            })
            .count();
        assert!(
            holding >= 15,
            "only {holding} of 40 programs hold a projection run"
        );
    }

    /// The projection run is drawn after every other choice: without it,
    /// a seed gives the same helpers and a main module whose lines all
    /// appear, in order, in the one with it.
    #[test]
    fn progen_projection_run_is_drawn_last() {
        let config = GenConfig::default();
        let without = GenConfig {
            projection_run_percent: 0,
            ..GenConfig::default()
        };
        let mut added = 0;
        for seed in 0..60 {
            let with_run = generate(seed, &config);
            let plain = generate(seed, &without);
            assert_eq!(
                with_run.file("src/helpers.cheby"),
                plain.file("src/helpers.cheby"),
                "seed {seed}"
            );
            let full = with_run.file("src/main.cheby").unwrap_or_default();
            let base = plain.file("src/main.cheby").unwrap_or_default();
            let mut lines = full.lines();
            for line in base.lines() {
                assert!(
                    lines.any(|l| l == line),
                    "seed {seed}: `{line}` is missing or out of order in:\n{full}"
                );
            }
            if full != base {
                assert!(full.contains("let result = f"), "seed {seed}");
                added += 1;
            }
        }
        assert!(added > 0);
    }

    #[test]
    fn progen_same_seed_same_program() {
        let config = GenConfig::default();
        for seed in 0..20 {
            assert_eq!(generate(seed, &config), generate(seed, &config));
        }
        assert_ne!(generate(1, &config), generate(2, &config));
    }

    #[test]
    fn progen_same_bytes_same_program() {
        let config = GenConfig::default();
        for data in [
            &b""[..],
            b"\x01\x02\x03",
            b"\xff\xfe\xfd\xfc\xfb\xfa\x00\x09",
        ] {
            assert_eq!(
                generate_from_bytes(data, &config),
                generate_from_bytes(data, &config)
            );
        }
    }

    #[test]
    fn progen_programs_follow_the_writing_rules() {
        let config = GenConfig::default();
        for seed in 0..50 {
            let program = generate(seed, &config);
            for (path, text) in &program.files {
                if std::path::Path::new(path)
                    .extension()
                    .is_none_or(|e| e != "cheby")
                {
                    continue;
                }
                for word in ["if ", "else", "for ", "while ", "loop ", ";", "\t"] {
                    assert!(
                        !text.contains(word),
                        "seed {seed}: {path} contains `{word}`:\n{text}"
                    );
                }
                assert!(text.starts_with("//! "), "seed {seed}: {path}");
                assert!(!text.contains(['\u{1}', '\u{2}']), "seed {seed}: {path}");
            }
        }
    }
}
