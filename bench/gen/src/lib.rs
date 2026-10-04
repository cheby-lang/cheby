//! Synthetic Cheby projects for compile-speed measurements
//! (docs/plan/testing.md §7, D-247).
//!
//! The output depends only on the [`Config`]: the same configuration gives
//! the same files, byte for byte, on every platform. Every generated module
//! is valid Cheby in `cheby fmt` style, and is meant to type-check, so the
//! projects measure the whole front end and not just the parser.
//!
//! Modules live in `src/gNN/mNNNNN.cheby` and import only modules with a
//! lower number, so the import graph is acyclic (D-145). The shape decides
//! module sizes and the import graph:
//!
//! | Shape     | Module size  | Imports of module `i`                    |
//! | --------- | ------------ | ---------------------------------------- |
//! | `small`   | ~250 lines   | up to 4 random earlier modules           |
//! | `large`   | ~20k lines   | up to 4 random earlier modules           |
//! | `deep`    | ~250 lines   | module `i - 1` only, one long chain      |
//! | `wide`    | ~250 lines   | 1 to 4 of the first 8 modules            |
//! | `generic` | ~250 lines   | like `small`, mostly generic functions   |

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Name of the generated package.
pub const PACKAGE: &str = "bench";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Small,
    Large,
    Deep,
    Wide,
    Generic,
}

impl Shape {
    pub const ALL: [Self; 5] = [
        Self::Small,
        Self::Large,
        Self::Deep,
        Self::Wide,
        Self::Generic,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Large => "large",
            Self::Deep => "deep",
            Self::Wide => "wide",
            Self::Generic => "generic",
        }
    }

    const fn module_lines(self) -> usize {
        match self {
            Self::Large => 20_000,
            _ => 250,
        }
    }
}

impl FromStr for Shape {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|shape| shape.name() == s)
            .ok_or_else(|| {
                format!("unknown shape `{s}`, expected small, large, deep, wide or generic")
            })
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Approximate number of lines to generate, over all modules.
    pub lines: usize,
    pub shape: Shape,
    pub seed: u64,
}

/// A generated project: file paths relative to the project root, with `/`
/// separators, and their contents, sorted by path.
#[derive(Debug, Clone)]
pub struct Project {
    pub files: Vec<(String, String)>,
}

impl Project {
    #[must_use]
    pub fn lines(&self) -> usize {
        self.files
            .iter()
            .map(|(_, text)| text.lines().count())
            .sum()
    }

    #[must_use]
    pub fn bytes(&self) -> usize {
        self.files.iter().map(|(_, text)| text.len()).sum()
    }

    /// Number of `.cheby` files.
    #[must_use]
    pub fn modules(&self) -> usize {
        self.files
            .iter()
            .filter(|(path, _)| {
                Path::new(path)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("cheby"))
            })
            .count()
    }

    /// Writes the project under `out`, which must not exist or be empty of
    /// files from another run, since stale files are not removed.
    ///
    /// # Errors
    ///
    /// Returns the first error from creating a directory or writing a file.
    pub fn write(&self, out: &Path) -> io::Result<Vec<PathBuf>> {
        let mut written = Vec::with_capacity(self.files.len());
        for (path, text) in &self.files {
            let full = out.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&full, text)?;
            written.push(full);
        }
        Ok(written)
    }
}

/// Generates a project.
#[must_use]
pub fn generate(config: &Config) -> Project {
    let per_module = config.shape.module_lines();
    let count = config.lines.div_ceil(per_module).max(1);
    let mut rng = Rng::new(config.seed);
    let mut files = Vec::with_capacity(count.saturating_add(2));
    files.push((
        "cheby.toml".to_owned(),
        format!("name = \"{PACKAGE}\"\nversion = \"0.1.0\"\n"),
    ));
    let mut modules = Vec::with_capacity(count);
    for index in 0..count {
        let imports = imports_of(config.shape, index, &mut rng);
        let text = module(config.shape, index, &imports, per_module, &mut rng);
        modules.push((format!("src/{}.cheby", file_path(index)), text));
    }
    files.push(("src/main.cheby".to_owned(), main_module(count)));
    files.extend(modules);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Project { files }
}

/// Modules per directory.
const PER_DIR: usize = 100;

fn dir_name(index: usize) -> String {
    format!("g{:03}", index / PER_DIR)
}

fn module_name(index: usize) -> String {
    format!("m{index:05}")
}

fn file_path(index: usize) -> String {
    format!("{}/{}", dir_name(index), module_name(index))
}

fn import_path(index: usize) -> String {
    format!("{PACKAGE}::{}::{}", dir_name(index), module_name(index))
}

fn imports_of(shape: Shape, index: usize, rng: &mut Rng) -> Vec<usize> {
    if index == 0 {
        return Vec::new();
    }
    let mut imports: Vec<usize> = match shape {
        Shape::Deep => vec![index.saturating_sub(1)],
        Shape::Wide => {
            let roots = index.min(8);
            (0..=rng.below(4)).map(|_| rng.below(roots)).collect()
        }
        Shape::Small | Shape::Large | Shape::Generic => {
            (0..rng.below(5)).map(|_| rng.below(index)).collect()
        }
    };
    imports.sort_unstable();
    imports.dedup();
    imports
}

fn main_module(count: usize) -> String {
    let last = count.saturating_sub(1);
    let name = module_name(last);
    format!(
        "//! Entry point of a generated benchmark project. It calls into the\n\
         //! last module, which reaches the rest through its imports.\n\
         \n\
         import std::io\n\
         import {path}\n\
         \n\
         fn main() {{\n\
         \x20 let total = {name}::entry(10)\n\
         \x20 io::println(\"{{total}}\")\n\
         }}\n",
        path = import_path(last),
    )
}

fn module(shape: Shape, index: usize, imports: &[usize], target: usize, rng: &mut Rng) -> String {
    let mut out = String::with_capacity(target.saturating_mul(40));
    let _ = writeln!(
        out,
        "//! Generated module {index} of a `{}` benchmark project.",
        shape.name()
    );
    out.push('\n');
    if shape == Shape::Generic || rng.below(2) == 0 {
        out.push_str("import std::list\n");
    }
    for &dep in imports {
        let _ = writeln!(out, "import {}", import_path(dep));
    }
    if !imports.is_empty() || out.ends_with("list\n") {
        out.push('\n');
    }

    // `entry` is what other modules and `main` call.
    out.push_str("/// The function other modules call.\npub fn entry(n: Int) -> Int {\n");
    let mut sum = "count_0(n)".to_owned();
    for &dep in imports {
        let _ = write!(sum, " + {}::entry(n)", module_name(dep));
    }
    let _ = writeln!(out, "  {sum}\n}}");

    let mut unit: usize = 0;
    let mut lines = out.lines().count();
    let mut chunk = String::new();
    while lines < target || unit == 0 {
        chunk.clear();
        chunk.push('\n');
        let kind = if shape == Shape::Generic {
            rng.below(4)
        } else {
            rng.below(6)
        };
        match kind {
            0 => generic_unit(&mut chunk, unit, rng),
            1 => list_unit(&mut chunk, unit, rng),
            2 => record_unit(&mut chunk, unit, rng),
            3 => variant_unit(&mut chunk, unit, rng),
            _ => counting_unit(&mut chunk, unit, rng),
        }
        lines = lines.saturating_add(chunk.bytes().filter(|&b| b == b'\n').count());
        out.push_str(&chunk);
        unit = unit.saturating_add(1);
    }
    // `entry` calls `count_0`, so the module always has one.
    if !out.contains("pub fn count_0(") {
        out.push('\n');
        counting_function(&mut out, 0, 3);
    }
    out
}

fn counting_unit(out: &mut String, unit: usize, rng: &mut Rng) {
    counting_function(out, unit, rng.below(9).saturating_add(1));
}

fn counting_function(out: &mut String, unit: usize, factor: usize) {
    let _ = write!(
        out,
        "/// Adds up `i * {factor}` for every `i` from 0 to `n`.\n\
         pub fn count_{unit}(n: Int) -> Int {{\n\
         \x20 fn step(i: Int, acc: Int) -> Int {{\n\
         \x20   case {{\n\
         \x20     i > n => acc\n\
         \x20     _ => step(i + 1, acc + i * {factor})\n\
         \x20   }}\n\
         \x20 }}\n\
         \x20 step(0, 0)\n\
         }}\n"
    );
}

fn generic_unit(out: &mut String, unit: usize, rng: &mut Rng) {
    let n = rng.below(100);
    let _ = write!(
        out,
        "/// Returns `a` if `first` holds, and `b` otherwise.\n\
         pub fn pick_{unit}<T>(first: Bool, a: T, b: T) -> T {{\n\
         \x20 case first {{\n\
         \x20   True => a\n\
         \x20   False => b\n\
         \x20 }}\n\
         }}\n\
         \n\
         /// Pairs `value` with itself.\n\
         pub fn twice_{unit}<T>(value: T) -> (T, T) {{\n\
         \x20 (value, value)\n\
         }}\n\
         \n\
         test \"pick_{unit} picks the first value\" {{\n\
         \x20 assert pick_{unit}(True, {n}, 0) == {n}\n\
         }}\n"
    );
}

fn list_unit(out: &mut String, unit: usize, rng: &mut Rng) {
    let len = rng.below(6).saturating_add(2);
    let items: Vec<String> = (0..len).map(|_| rng.below(1000).to_string()).collect();
    let _ = write!(
        out,
        "/// Sums a fixed list of numbers.\n\
         pub fn total_{unit}() -> Int {{\n\
         \x20 let items = [{}]\n\
         \x20 sum_{unit}(items, 0)\n\
         }}\n\
         \n\
         fn sum_{unit}(items: List<Int>, acc: Int) -> Int {{\n\
         \x20 case items {{\n\
         \x20   [] => acc\n\
         \x20   [first, ..rest] => sum_{unit}(rest, acc + first)\n\
         \x20 }}\n\
         }}\n",
        items.join(", ")
    );
}

fn record_unit(out: &mut String, unit: usize, rng: &mut Rng) {
    let x = rng.below(100);
    let y = rng.below(100);
    let _ = write!(
        out,
        "/// A pair of counters.\n\
         pub type Pair{unit} {{\n\
         \x20 left: Int,\n\
         \x20 right: Int,\n\
         }}\n\
         \n\
         /// Swaps the counters of `pair`.\n\
         pub fn swap_{unit}(pair: Pair{unit}) -> Pair{unit} {{\n\
         \x20 Pair{unit} {{ left: pair.right, right: pair.left }}\n\
         }}\n\
         \n\
         /// Adds `n` to the left counter.\n\
         pub fn bump_{unit}(pair: Pair{unit}, n: Int) -> Pair{unit} {{\n\
         \x20 Pair{unit} {{ ..pair, left: pair.left + n }}\n\
         }}\n\
         \n\
         test \"swap_{unit} swaps\" {{\n\
         \x20 let pair = Pair{unit} {{ left: {x}, right: {y} }}\n\
         \x20 assert swap_{unit}(pair).left == {y}\n\
         }}\n"
    );
}

fn variant_unit(out: &mut String, unit: usize, rng: &mut Rng) {
    let a = rng.below(50);
    let _ = write!(
        out,
        "/// A small shape.\n\
         pub type Shape{unit} {{\n\
         \x20 Square(Int),\n\
         \x20 Rect(Int, Int),\n\
         \x20 Empty,\n\
         }}\n\
         \n\
         /// Returns the area of `shape`.\n\
         pub fn area_{unit}(shape: Shape{unit}) -> Int {{\n\
         \x20 case shape {{\n\
         \x20   Square(side) => side * side\n\
         \x20   Rect(width, height) => width * height\n\
         \x20   Empty => 0\n\
         \x20 }}\n\
         }}\n\
         \n\
         test \"area_{unit} of a square\" {{\n\
         \x20 assert area_{unit}(Square({a})) == {}\n\
         }}\n",
        a.saturating_mul(a)
    );
}

/// `SplitMix64`: small, fast and the same on every platform.
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    const fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n`, or 0 if `n` is 0.
    fn below(&mut self, n: usize) -> usize {
        let bound = u64::try_from(n).unwrap_or(u64::MAX);
        let value = self.next().checked_rem(bound).unwrap_or(0);
        // `value < n`, so it always fits.
        usize::try_from(value).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_config_same_output() {
        for shape in Shape::ALL {
            let config = Config {
                lines: 3_000,
                shape,
                seed: 7,
            };
            assert_eq!(
                generate(&config).files,
                generate(&config).files,
                "{}",
                shape.name()
            );
        }
    }

    #[test]
    fn seed_changes_output() {
        let a = generate(&Config {
            lines: 3_000,
            shape: Shape::Small,
            seed: 1,
        });
        let b = generate(&Config {
            lines: 3_000,
            shape: Shape::Small,
            seed: 2,
        });
        assert_ne!(a.files, b.files);
    }

    #[test]
    fn reaches_the_requested_size() {
        let project = generate(&Config {
            lines: 10_000,
            shape: Shape::Small,
            seed: 1,
        });
        let lines = project.lines();
        assert!((10_000..12_000).contains(&lines), "{lines} lines");
    }

    #[test]
    fn imports_only_earlier_modules() {
        let mut rng = Rng::new(3);
        for shape in Shape::ALL {
            for index in 0..300 {
                for dep in imports_of(shape, index, &mut rng) {
                    assert!(dep < index);
                }
            }
        }
    }

    #[test]
    fn every_module_has_entry_and_count_0() {
        let project = generate(&Config {
            lines: 2_000,
            shape: Shape::Generic,
            seed: 5,
        });
        for (path, text) in &project.files {
            if path.starts_with("src/g") {
                assert!(text.contains("pub fn entry(n: Int) -> Int"), "{path}");
                assert!(text.contains("pub fn count_0(n: Int) -> Int"), "{path}");
            }
        }
    }
}
