//! The `huge_literals` shapes (D-720): one function that builds a single
//! literal of n computed elements, and a `main` that prints a checksum of
//! it.
//!
//! D-643 left a literal of computed elements compiling in superlinear time,
//! and D-720 schedules the fix. These projects are its fixed workload: the
//! same literal kind at 25k, 50k, 100k and 200k elements gives the slope.
//! Each kind is a shape of its own, so that one build measures one kind:
//!
//! | Shape                | The literal                                      | `main` prints          |
//! | -------------------- | ------------------------------------------------ | ---------------------- |
//! | `huge_list`          | `[x + r, …]`, a list of n elements               | its length and sum     |
//! | `huge_interpolation` | `"{v0},{v1},…"`, a string of n `Int` parts       | the string's length    |
//! | `huge_map`           | `map::from_list([(x + i, x + r), …])`, n pairs   | its size and its sum   |
//! | `huge_tuple`         | `(x + r, …)`, a tuple of n fields                | its `:?` text's length |
//!
//! `x` is a parameter of the function that builds the literal, so every
//! element is computed at run time, and each `r` is drawn from the seed. The
//! interpolation's parts cycle through ten locals `v0` to `v9`, each `x + r`,
//! since an interpolation names locals only (Appendix A, `interpolation`).
//! A tuple needs two fields, so n is at least 2 for every kind.
//!
//! The project also holds `expected.stdout`, the checksum `cheby run` must
//! print, computed here.

use std::fmt::Write as _;

use crate::{Project, Rng, Shape};

/// The value `main` passes as `x`.
const X: u64 = 1;

/// Locals an interpolation's parts cycle through.
const LOCALS: usize = 10;

/// The smallest literal: a tuple has at least two fields.
const MIN_ELEMENTS: usize = 2;

/// The literal kinds of the `huge_literals` shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    List,
    Interpolation,
    Map,
    Tuple,
}

/// Generates the project of a `huge_literals` shape with `n` elements.
pub(crate) fn generate(kind: Kind, n: usize, seed: u64) -> Project {
    let n = n.max(MIN_ELEMENTS);
    let mut rng = Rng::new(seed);
    let (source, expected) = match kind {
        Kind::List => list(n, &mut rng),
        Kind::Interpolation => interpolation(n, &mut rng),
        Kind::Map => map(n, &mut rng),
        Kind::Tuple => tuple(n, &mut rng),
    };
    let mut files = vec![
        (
            "cheby.toml".to_owned(),
            format!("name = \"{}\"\nversion = \"0.1.0\"\n", crate::PACKAGE),
        ),
        ("expected.stdout".to_owned(), format!("{expected}\n")),
        ("src/main.cheby".to_owned(), source),
    ];
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Project { files }
}

/// The offset `r` of one element: a number below 1000.
fn offset(rng: &mut Rng) -> u64 {
    u64::try_from(rng.below(1000)).unwrap_or(0)
}

/// The module comment and the imports.
fn header(out: &mut String, shape: Shape, n: usize, what: &str, imports: &[&str]) {
    let _ = write!(
        out,
        "//! Generated `{}` benchmark project (D-720): {what} of {n} computed\n\
         //! elements, built in one function, and a checksum of it.\n\
         \n\
         import std::io\n",
        shape.name()
    );
    for import in imports {
        let _ = writeln!(out, "import std::{import}");
    }
}

fn list(n: usize, rng: &mut Rng) -> (String, String) {
    let mut out = String::with_capacity(n.saturating_mul(16).saturating_add(512));
    header(&mut out, Shape::HugeList, n, "one list literal", &["list"]);
    out.push_str("\n/// The literal.\nfn build(x: Int) -> List<Int> {\n  [\n");
    let mut total: u64 = 0;
    for _ in 0..n {
        let r = offset(rng);
        total = total.saturating_add(X.saturating_add(r));
        let _ = writeln!(out, "    x + {r},");
    }
    out.push_str(
        "  ]\n}\n\
         \n\
         fn main() {\n\
         \x20 let items = build(1)\n\
         \x20 let count = list::length(items)\n\
         \x20 let total = list::sum(items, 0)\n\
         \x20 io::println(\"{count} {total}\")\n\
         }\n",
    );
    (out, format!("{n} {total}"))
}

fn interpolation(n: usize, rng: &mut Rng) -> (String, String) {
    let mut out = String::with_capacity(n.saturating_mul(6).saturating_add(512));
    header(
        &mut out,
        Shape::HugeInterpolation,
        n,
        "one interpolation",
        &["string"],
    );
    out.push_str("\n/// The literal.\nfn build(x: Int) -> String {\n");
    let locals = n.min(LOCALS);
    let mut values = Vec::with_capacity(locals);
    for local in 0..locals {
        let r = offset(rng);
        values.push(X.saturating_add(r));
        let _ = writeln!(out, "  let v{local} = x + {r}");
    }
    out.push_str("  \"");
    let mut text = String::with_capacity(n.saturating_mul(5));
    for part in 0..n {
        if part > 0 {
            out.push(',');
            text.push(',');
        }
        let local = part.checked_rem(locals).unwrap_or(0);
        let _ = write!(out, "{{v{local}}}");
        let _ = write!(text, "{}", values.get(local).copied().unwrap_or(0));
    }
    out.push_str(
        "\"\n}\n\
         \n\
         fn main() {\n\
         \x20 let text = build(1)\n\
         \x20 let size = string::length(text)\n\
         \x20 io::println(\"{size}\")\n\
         }\n",
    );
    (out, text.len().to_string())
}

fn map(n: usize, rng: &mut Rng) -> (String, String) {
    let mut out = String::with_capacity(n.saturating_mul(28).saturating_add(512));
    header(
        &mut out,
        Shape::HugeMap,
        n,
        "one map built from a list literal of pairs",
        &["map"],
    );
    out.push_str(
        "\n/// The literal.\nfn build(x: Int) -> map::Map<Int, Int> {\n  map::from_list([\n",
    );
    let mut total: u64 = 0;
    for key in 0..n {
        let r = offset(rng);
        let key = u64::try_from(key).unwrap_or(u64::MAX);
        total = total
            .saturating_add(X.saturating_add(key))
            .saturating_add(X.saturating_add(r));
        let _ = writeln!(out, "    (x + {key}, x + {r}),");
    }
    out.push_str(
        "  ])\n}\n\
         \n\
         fn main() {\n\
         \x20 let pairs = build(1)\n\
         \x20 let size = map::size(pairs)\n\
         \x20 let total = map::fold(pairs, 0, fn(acc, key, value) { acc + key + value })\n\
         \x20 io::println(\"{size} {total}\")\n\
         }\n",
    );
    (out, format!("{n} {total}"))
}

fn tuple(n: usize, rng: &mut Rng) -> (String, String) {
    let mut out = String::with_capacity(n.saturating_mul(16).saturating_add(512));
    header(
        &mut out,
        Shape::HugeTuple,
        n,
        "one tuple literal",
        &["string"],
    );
    out.push_str("\n/// The literal.\nfn build(x: Int) -> String {\n  let fields = (\n");
    // `:?` shows a tuple as `(a, b, …)`.
    let mut text = String::with_capacity(n.saturating_mul(5));
    text.push('(');
    for field in 0..n {
        let r = offset(rng);
        if field > 0 {
            text.push_str(", ");
        }
        let _ = write!(text, "{}", X.saturating_add(r));
        let _ = writeln!(out, "    x + {r},");
    }
    text.push(')');
    out.push_str(
        "  )\n\
         \x20 \"{fields:?}\"\n\
         }\n\
         \n\
         fn main() {\n\
         \x20 let text = build(1)\n\
         \x20 let size = string::length(text)\n\
         \x20 io::println(\"{size}\")\n\
         }\n",
    );
    (out, text.len().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, generate as generate_project};

    fn source(project: &Project) -> &str {
        project
            .files
            .iter()
            .find(|(path, _)| path == "src/main.cheby")
            .map(|(_, text)| text.as_str())
            .unwrap()
    }

    fn expected(project: &Project) -> &str {
        project
            .files
            .iter()
            .find(|(path, _)| path == "expected.stdout")
            .map(|(_, text)| text.as_str())
            .unwrap()
    }

    fn project(shape: Shape, lines: usize, seed: u64) -> Project {
        generate_project(&Config { lines, shape, seed })
    }

    /// The literal has exactly n elements, for every kind.
    #[test]
    fn each_literal_has_n_elements() {
        for shape in Shape::HUGE_LITERALS {
            for n in [2, 3, 10, 11, 257] {
                let project = project(shape, n, 1);
                let text = source(&project);
                let elements = match shape {
                    Shape::HugeInterpolation => text.matches("{v").count(),
                    Shape::HugeMap => text.matches("\n    (x + ").count(),
                    _ => text.matches("\n    x + ").count(),
                };
                assert_eq!(elements, n, "{} at {n}", shape.name());
                assert_eq!(project.modules(), 1);
            }
        }
    }

    #[test]
    fn at_least_two_elements() {
        for shape in Shape::HUGE_LITERALS {
            assert_eq!(
                project(shape, 0, 1).files,
                project(shape, 2, 1).files,
                "{}",
                shape.name()
            );
        }
    }

    #[test]
    fn seed_changes_the_literal() {
        for shape in Shape::HUGE_LITERALS {
            assert_ne!(
                project(shape, 50, 1).files,
                project(shape, 50, 2).files,
                "{}",
                shape.name()
            );
        }
    }

    /// The checksum is the one the program computes, for three elements
    /// whose offsets come from the same generator.
    #[test]
    fn expected_output_is_the_checksum() {
        let mut rng = Rng::new(9);
        let [a, b, c] = [offset(&mut rng), offset(&mut rng), offset(&mut rng)];
        let sum = 3 + a + b + c;
        assert_eq!(
            expected(&project(Shape::HugeList, 3, 9)),
            format!("3 {sum}\n")
        );
        // Keys 1, 2 and 3, values `1 + r`.
        assert_eq!(
            expected(&project(Shape::HugeMap, 3, 9)),
            format!("3 {}\n", 6 + sum)
        );
        let tuple = format!("({}, {}, {})", 1 + a, 1 + b, 1 + c);
        assert_eq!(
            expected(&project(Shape::HugeTuple, 3, 9)),
            format!("{}\n", tuple.len())
        );
        let text = format!("{},{},{}", 1 + a, 1 + b, 1 + c);
        assert_eq!(
            expected(&project(Shape::HugeInterpolation, 3, 9)),
            format!("{}\n", text.len())
        );
    }

    /// Past ten parts, the interpolation cycles through its ten locals.
    #[test]
    fn interpolation_cycles_its_locals() {
        let project = project(Shape::HugeInterpolation, 12, 1);
        let text = source(&project);
        assert!(text.contains("  let v9 = x + "), "{text}");
        assert!(!text.contains("let v10"), "{text}");
        assert!(text.contains("\"{v0},{v1},"), "{text}");
        assert!(text.contains(",{v9},{v0},{v1}\""), "{text}");
    }

    #[test]
    fn shapes_parse_by_name() {
        for shape in Shape::HUGE_LITERALS {
            assert_eq!(shape.name().parse::<Shape>(), Ok(shape));
            assert!(shape.is_huge_literal());
            assert!(!Shape::ALL.contains(&shape));
        }
        for shape in Shape::ALL {
            assert!(!shape.is_huge_literal());
        }
    }
}
