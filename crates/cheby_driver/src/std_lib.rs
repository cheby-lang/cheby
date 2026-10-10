//! The standard library's sources and catalog (G2, D-450, D-451).
//!
//! The sources in `std/` at the repository root are embedded in the
//! toolchain (D-450), so `cheby` works from any directory and builds are
//! deterministic. M3 replaces this with precompiled std (D-256).
//! Std modules are parsed and checked lazily, when a build reaches them.
//! A user build skips their `test` blocks, which only testing std itself
//! checks and runs (D-479).

/// One shipped standard-library module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StdModule {
    /// The path under `std`: `io`, `json::decode`.
    pub path: &'static str,
    pub source: &'static str,
}

/// The shipped modules, sorted by path. Exactly the `.cheby` files under
/// `std/` (a test checks it).
pub const STD_MODULES: &[StdModule] = &[
    StdModule {
        path: "bool",
        source: include_str!("../../../std/bool.cheby"),
    },
    StdModule {
        path: "bytes",
        source: include_str!("../../../std/bytes.cheby"),
    },
    StdModule {
        path: "env",
        source: include_str!("../../../std/env.cheby"),
    },
    StdModule {
        path: "f32",
        source: include_str!("../../../std/f32.cheby"),
    },
    StdModule {
        path: "float",
        source: include_str!("../../../std/float.cheby"),
    },
    StdModule {
        path: "i16",
        source: include_str!("../../../std/i16.cheby"),
    },
    StdModule {
        path: "i32",
        source: include_str!("../../../std/i32.cheby"),
    },
    StdModule {
        path: "i64",
        source: include_str!("../../../std/i64.cheby"),
    },
    StdModule {
        path: "i8",
        source: include_str!("../../../std/i8.cheby"),
    },
    StdModule {
        path: "int",
        source: include_str!("../../../std/int.cheby"),
    },
    StdModule {
        path: "io",
        source: include_str!("../../../std/io.cheby"),
    },
    StdModule {
        path: "json",
        source: include_str!("../../../std/json.cheby"),
    },
    StdModule {
        path: "json::decode",
        source: include_str!("../../../std/json/decode.cheby"),
    },
    // The internal module of D-361 that `json` and `json::decode` both
    // import; its items are package-visible only (D-483).
    StdModule {
        path: "json::tree",
        source: include_str!("../../../std/json/tree.cheby"),
    },
    StdModule {
        path: "list",
        source: include_str!("../../../std/list.cheby"),
    },
    StdModule {
        path: "map",
        source: include_str!("../../../std/map.cheby"),
    },
    StdModule {
        path: "nil",
        source: include_str!("../../../std/nil.cheby"),
    },
    StdModule {
        path: "ops",
        source: include_str!("../../../std/ops.cheby"),
    },
    StdModule {
        path: "option",
        source: include_str!("../../../std/option.cheby"),
    },
    StdModule {
        path: "order",
        source: include_str!("../../../std/order.cheby"),
    },
    StdModule {
        path: "result",
        source: include_str!("../../../std/result.cheby"),
    },
    StdModule {
        path: "set",
        source: include_str!("../../../std/set.cheby"),
    },
    StdModule {
        path: "string",
        source: include_str!("../../../std/string.cheby"),
    },
    StdModule {
        path: "u16",
        source: include_str!("../../../std/u16.cheby"),
    },
    StdModule {
        path: "u32",
        source: include_str!("../../../std/u32.cheby"),
    },
    StdModule {
        path: "u64",
        source: include_str!("../../../std/u64.cheby"),
    },
    StdModule {
        path: "u8",
        source: include_str!("../../../std/u8.cheby"),
    },
];

/// Modules written before the checker can check them (D-486):
/// embedded so tests can run phase A on them, but never loaded. Each stays
/// in [`PLANNED_STD_MODULES`], so importing one is still E0038 (D-451),
/// until it is promoted to [`STD_MODULES`]. Sorted by path. Empty: every
/// M1 module is promoted; later tiers may stage modules here again.
pub const STAGED_STD_MODULES: &[StdModule] = &[];

/// Modules the standard-library spec plans that `std/` does not ship yet:
/// the rest of tier 1 (`docs/stdlib/`), tier 2 and tier 3
/// (`docs/plan/stdlib.md`). Importing one is E0038, not E0039 (D-451).
/// Sorted.
pub const PLANNED_STD_MODULES: &[&str] = &[
    "channel",
    "duration",
    "fiber",
    "file",
    "http",
    "multimap",
    "net",
    "ordered_map",
    "ordered_multimap",
    "ordered_set",
    "process",
    "random",
    "runtime",
    "selector",
    "sorted_map",
    "sorted_multimap",
    "sorted_set",
    "time",
];

/// The shipped module at `path` (under `std`).
#[must_use]
pub fn find(path: &str) -> Option<&'static StdModule> {
    STD_MODULES.iter().find(|m| m.path == path)
}

/// Whether `path` (under `std`) is planned but not shipped.
#[must_use]
pub fn is_planned(path: &str) -> bool {
    PLANNED_STD_MODULES.contains(&path)
}

/// Every std module path the toolchain knows, shipped or planned.
#[must_use]
pub fn known() -> Vec<&'static str> {
    let mut all: Vec<&'static str> = STD_MODULES
        .iter()
        .map(|m| m.path)
        .chain(PLANNED_STD_MODULES.iter().copied())
        .collect();
    all.sort_unstable();
    all
}

/// The path a std file is shown with in diagnostics and panic locations:
/// `std/io.cheby`, `std/json/decode.cheby` (D-450).
#[must_use]
pub fn display_path(path: &str) -> String {
    format!("std/{}.cheby", path.replace("::", "/"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use cheby_diag::{SourceMap, render_all};

    use super::*;

    fn repo() -> &'static Path {
        Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
    }

    fn cheby_files(dir: &Path, prefix: &str, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                cheby_files(&path, &format!("{prefix}{name}::"), out);
            } else if let Some(stem) = name.strip_suffix(".cheby") {
                out.push(format!("{prefix}{stem}"));
            }
        }
    }

    #[test]
    fn std_files_are_the_shipped_and_staged_modules() {
        let mut files = Vec::new();
        cheby_files(&repo().join("std"), "", &mut files);
        files.sort();
        let mut listed: Vec<&str> = STD_MODULES
            .iter()
            .chain(STAGED_STD_MODULES)
            .map(|m| m.path)
            .collect();
        listed.sort_unstable();
        assert_eq!(files, listed);
        for module in STD_MODULES.iter().chain(STAGED_STD_MODULES) {
            let text = std::fs::read_to_string(repo().join(display_path(module.path))).unwrap();
            assert_eq!(text, module.source, "{} is stale", module.path);
        }
    }

    #[test]
    fn module_lists_are_sorted() {
        for list in [STD_MODULES, STAGED_STD_MODULES] {
            let paths: Vec<&str> = list.iter().map(|m| m.path).collect();
            let mut sorted = paths.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(paths, sorted);
        }
    }

    #[test]
    fn staged_modules_are_planned_and_not_shipped() {
        for module in STAGED_STD_MODULES {
            assert!(is_planned(module.path), "{} is not planned", module.path);
            assert!(find(module.path).is_none(), "{} is shipped", module.path);
        }
    }

    /// Phase A, exactly as a build runs it on a std module: lexing,
    /// parsing and syntax validation, with no diagnostics at all.
    #[test]
    fn staged_modules_pass_phase_a() {
        let mut failures = String::new();
        for module in STAGED_STD_MODULES {
            let mut sources = SourceMap::new();
            let mut diagnostics = Vec::new();
            let path = display_path(module.path);
            let parsed = crate::parse_file(
                &mut sources,
                &path,
                module.source.as_bytes().to_vec(),
                &mut diagnostics,
            )
            .unwrap();
            assert!(parsed.syntax.is_some());
            if !diagnostics.is_empty() || parsed.has_syntax_errors {
                failures.push_str(&render_all(&diagnostics, &sources));
            }
        }
        assert!(failures.is_empty(), "{failures}");
    }

    #[test]
    fn planned_modules_are_not_shipped_and_cover_the_spec() {
        let mut sorted = PLANNED_STD_MODULES.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, PLANNED_STD_MODULES);
        for module in STD_MODULES {
            assert!(!is_planned(module.path), "{} is shipped", module.path);
        }
        for entry in std::fs::read_dir(repo().join("docs/stdlib")).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".md") else {
                continue;
            };
            if stem != "README" {
                assert!(
                    is_planned(stem) || find(stem).is_some(),
                    "docs/stdlib/{name} has no catalog entry"
                );
            }
        }
        assert!(find("json::decode").is_some());
        assert!(find("json::tree").is_some());
    }

    /// Every M1 module ships: tier 1 except `multimap`, `sorted_*` and
    /// `ordered_*` (D-451), every home module of a builtin type among them
    /// (D-447).
    #[test]
    fn m1_modules_ship() {
        for path in [
            "bool",
            "bytes",
            "env",
            "f32",
            "float",
            "i8",
            "i16",
            "i32",
            "i64",
            "int",
            "io",
            "json",
            "json::decode",
            "json::tree",
            "list",
            "map",
            "nil",
            "ops",
            "option",
            "order",
            "result",
            "set",
            "string",
            "u8",
            "u16",
            "u32",
            "u64",
        ] {
            assert!(find(path).is_some(), "std::{path} is not shipped");
        }
        for (_, home) in cheby_iface::builtin::HOMES {
            assert!(find(home).is_some(), "std::{home} is not shipped");
        }
        assert_eq!(STAGED_STD_MODULES, &[]);
    }

    #[test]
    fn display_paths() {
        assert_eq!(display_path("io"), "std/io.cheby");
        assert_eq!(display_path("json::decode"), "std/json/decode.cheby");
    }
}
