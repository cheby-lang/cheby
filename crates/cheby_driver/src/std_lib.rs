//! The standard library's sources and catalog (G2, D-450, D-451).
//!
//! The sources in `std/` at the repository root are embedded in the
//! toolchain (D-450), so `cheby` works from any directory and builds are
//! deterministic. M3 replaces this with precompiled std (D-256).
//! Std modules are parsed and checked lazily, when a build reaches them.

/// One shipped standard-library module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StdModule {
    /// The path under `std`: `io`, `json::decode`.
    pub path: &'static str,
    pub source: &'static str,
}

/// The shipped modules, sorted by path. Exactly the `.cheby` files under
/// `std/` (a test checks it).
pub const STD_MODULES: &[StdModule] = &[StdModule {
    path: "io",
    source: include_str!("../../../std/io.cheby"),
}];

/// Modules the standard-library spec plans that `std/` does not ship yet:
/// the rest of tier 1 (`docs/stdlib/`), tier 2 and tier 3
/// (`docs/plan/stdlib.md`). Importing one is E0038, not E0039 (D-451).
/// Sorted.
pub const PLANNED_STD_MODULES: &[&str] = &[
    "bool",
    "bytes",
    "channel",
    "duration",
    "env",
    "f32",
    "fiber",
    "file",
    "float",
    "http",
    "i16",
    "i32",
    "i64",
    "i8",
    "int",
    "json",
    "json::decode",
    "list",
    "map",
    "multimap",
    "net",
    "nil",
    "ops",
    "option",
    "order",
    "ordered_map",
    "ordered_multimap",
    "ordered_set",
    "process",
    "random",
    "result",
    "runtime",
    "selector",
    "set",
    "sorted_map",
    "sorted_multimap",
    "sorted_set",
    "string",
    "time",
    "u16",
    "u32",
    "u64",
    "u8",
];

/// Shipped modules that do not have all of their spec yet. A name the spec
/// defines but the source lacks is E0038, not E0042 (D-451):
/// `std/io.cheby` has only the four printing functions in the slice.
pub const PARTIAL_STD_MODULES: &[&str] = &["io"];

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
    fn std_modules_are_the_files_under_std() {
        let mut files = Vec::new();
        cheby_files(&repo().join("std"), "", &mut files);
        files.sort();
        let listed: Vec<&str> = STD_MODULES.iter().map(|m| m.path).collect();
        assert_eq!(files, listed);
        for module in STD_MODULES {
            let text = std::fs::read_to_string(repo().join(display_path(module.path))).unwrap();
            assert_eq!(text, module.source);
        }
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
        assert!(is_planned("json::decode"));
        for module in PARTIAL_STD_MODULES {
            assert!(find(module).is_some());
        }
    }

    #[test]
    fn display_paths() {
        assert_eq!(display_path("io"), "std/io.cheby");
        assert_eq!(display_path("json::decode"), "std/json/decode.cheby");
    }
}
