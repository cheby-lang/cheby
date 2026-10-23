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
        path: "channel",
        source: include_str!("../../../std/channel.cheby"),
    },
    StdModule {
        path: "duration",
        source: include_str!("../../../std/duration.cheby"),
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
        path: "fiber",
        source: include_str!("../../../std/fiber.cheby"),
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
        path: "multimap",
        source: include_str!("../../../std/multimap.cheby"),
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
        path: "ordered_map",
        source: include_str!("../../../std/ordered_map.cheby"),
    },
    StdModule {
        path: "ordered_multimap",
        source: include_str!("../../../std/ordered_multimap.cheby"),
    },
    StdModule {
        path: "ordered_set",
        source: include_str!("../../../std/ordered_set.cheby"),
    },
    StdModule {
        path: "random",
        source: include_str!("../../../std/random.cheby"),
    },
    StdModule {
        path: "result",
        source: include_str!("../../../std/result.cheby"),
    },
    StdModule {
        path: "runtime",
        source: include_str!("../../../std/runtime.cheby"),
    },
    StdModule {
        path: "selector",
        source: include_str!("../../../std/selector.cheby"),
    },
    StdModule {
        path: "set",
        source: include_str!("../../../std/set.cheby"),
    },
    StdModule {
        path: "sorted_map",
        source: include_str!("../../../std/sorted_map.cheby"),
    },
    StdModule {
        path: "sorted_multimap",
        source: include_str!("../../../std/sorted_multimap.cheby"),
    },
    StdModule {
        path: "sorted_set",
        source: include_str!("../../../std/sorted_set.cheby"),
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
/// the rest of tier 2 and tier 3 (`docs/plan/stdlib.md`). Importing one is
/// E0038, not E0039 (D-451). Sorted.
pub const PLANNED_STD_MODULES: &[&str] = &[
    "context",
    "file",
    "http",
    "net",
    "process",
    "time",
    "time::instant",
    "url",
];

/// Spec tests of shipped modules that `std/` may lack while a milestone is
/// in progress (D-728), as `(module, test name, milestone)`: the module's
/// path under `std`, the name of a `test` block of its page in
/// `docs/stdlib/`, and the milestone, such as `"M2"`. A module may ship
/// before all its behavior exists. The spec-test guard, a unit test of
/// this module, accepts a listed test as absent while its milestone is the
/// one in effect (`tests/milestone`, or `CHEBY_MILESTONE`, D-544). Past
/// that milestone the entry fails the guard, and so does an entry whose
/// test is in `std/`.
///
/// M2's first slice shipped `std::fiber`, `std::channel` and
/// `std::duration` before cancellation, timers, selectors and detached
/// fibers existed (D-754). Their spec tests have all moved into `std/`
/// since (D-728). An entry's comment names what its test needs and the
/// package that brings it.
pub const PENDING_SPEC_TESTS: &[(&str, &str, &str)] = &[
    // The public `fiber::cancel` and `fiber::race` come with S5 (D-1060,
    // D-1061).
    (
        "fiber",
        "cancel stops the fibers of a scope but not its body",
        "M3",
    ),
    ("fiber", "fibers spawned after cancel run normally", "M3"),
    (
        "fiber",
        "a fiber that cancels its own scope unwinds at once",
        "M3",
    ),
    (
        "fiber",
        "joining a fiber that cancel reached fails the scope",
        "M3",
    ),
    (
        "fiber",
        "race returns the first value and cancels the rest",
        "M3",
    ),
    ("fiber", "race of no contenders is None", "M3"),
    ("fiber", "a panic before the race is decided fails it", "M3"),
    // `io.md`'s file examples became tests in a temporary directory, which
    // need `file::with_temp_dir`, S3 (D-1066, D-1068). They live in
    // `std/file.cheby`, since `std::io` cannot import `std::file`.
    ("io", "read_file returns the text as stored", "M3"),
    ("io", "a file that is not UTF-8 is invalid data", "M3"),
    ("io", "creating a directory twice is already exists", "M3"),
    ("io", "opening a directory as a file is a directory", "M3"),
    ("io", "listing a file is not a directory", "M3"),
    (
        "io",
        "removing a directory with entries is directory not empty",
        "M3",
    ),
    ("io", "closing a file twice is closed", "M3"),
    // `io::Source`, `io::Sink`, `copy` and `other`, S7 (D-1072). The `copy`
    // test also needs S3's `file::with_temp_dir`, so it lives in
    // `std/file.cheby`.
    ("io", "copy moves every byte of one file to another", "M3"),
    ("io", "other builds an error with its own text", "M3"),
    // Seeded random streams need `std::context`, M4's workstream 4.6
    // (D-1048, D-1049).
    (
        "random",
        "with_generator makes host draws follow the seeded draws",
        "M4",
    ),
    (
        "random",
        "with_generator gives the same draws for the same generator",
        "M4",
    ),
    ("random", "a spawn leaves the parent's stream alone", "M4"),
    ("random", "spawned fibers draw reproducibly and apart", "M4"),
    // `fiber::raise` needs the private kernel `sys_raise` (D-P18), and
    // `fiber::map` and `try_map` need it and S5's `fiber::cancel` (D-P19).
    // Both are built after M3.
    (
        "fiber",
        "raise keeps the message and location of a caught panic",
        "M4",
    ),
    (
        "fiber",
        "a raised panic fails a scope like the original",
        "M4",
    ),
    ("fiber", "map keeps the order of its input", "M4"),
    ("fiber", "map clamps its limit to at least 1", "M4"),
    ("fiber", "map raises the panic of a call again", "M4"),
    ("fiber", "try_map returns the Err of the lowest index", "M4"),
    (
        "fiber",
        "try_map calls each element before the Err exactly once",
        "M4",
    ),
    ("fiber", "try_map cancels the calls in flight", "M4"),
    // `channel::send_now`, `receive_now` and `Refused` need the kernels
    // `sys_send_now` and `sys_receive_now`, and `selector::select_now`
    // needs `sys_select_now`, after M3 (D-P20).
    (
        "channel",
        "send_now reports a full buffer and gives the value back",
        "M4",
    ),
    ("channel", "send_now without receivers is Closed", "M4"),
    (
        "channel",
        "send_now on capacity 0 needs a waiting receiver",
        "M4",
    ),
    ("channel", "receive_now never waits", "M4"),
    ("selector", "select_now takes from a ready arm", "M4"),
    (
        "selector",
        "a timer arm is ready in select_now only when it is zero",
        "M4",
    ),
    // `fiber::yield_now` needs the `@std_context` kernel `sys_yield_now`
    // and the JS scheduler, M4 (D-P23). Its first test also needs
    // `channel::receive_now` (D-P20).
    ("fiber", "a loop that yields lets other fibers run", "M4"),
    ("fiber", "a cancellation is delivered at yield_now", "M4"),
];

include!(concat!(env!("OUT_DIR"), "/std_hashes.rs"));

/// The source hashes of the std module at `path` (under `std`), computed
/// from its file when `cheby` was built (D-963,
/// GAP(source-hash-width)); `None` for a path with no file.
#[must_use]
pub fn source_hashes(path: &str) -> Option<crate::SourceHashes> {
    let at = STD_SOURCE_HASHES
        .binary_search_by(|(p, _, _)| (*p).cmp(path))
        .ok()?;
    STD_SOURCE_HASHES
        .get(at)
        .map(|&(_, wide, narrow)| crate::SourceHashes {
            sip128: crate::cache::Hash128(wide),
            sip64: narrow,
        })
}

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

    /// The module paths of the files under `dir` whose names end with
    /// `suffix`: `io`, `json::decode`.
    fn module_files(dir: &Path, prefix: &str, suffix: &str, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                module_files(&path, &format!("{prefix}{name}::"), suffix, out);
            } else if let Some(stem) = name.strip_suffix(suffix) {
                out.push(format!("{prefix}{stem}"));
            }
        }
    }

    #[test]
    fn std_files_are_the_shipped_and_staged_modules() {
        let mut files = Vec::new();
        module_files(&repo().join("std"), "", ".cheby", &mut files);
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

    /// Every std constant of `build.rs` is the hash of the text `cheby`
    /// embeds, both widths (D-963, D-538, GAP(source-hash-width)), and
    /// every embedded module has one.
    #[test]
    fn front_source_hash_std_constants_match_embedded_text() {
        let paths: Vec<&str> = STD_SOURCE_HASHES.iter().map(|(p, _, _)| *p).collect();
        let mut listed: Vec<&str> = STD_MODULES
            .iter()
            .chain(STAGED_STD_MODULES)
            .map(|m| m.path)
            .collect();
        listed.sort_unstable();
        assert_eq!(paths, listed);
        for module in STD_MODULES.iter().chain(STAGED_STD_MODULES) {
            let hashes = source_hashes(module.path).unwrap();
            assert_eq!(
                hashes,
                crate::SourceHashes::of(module.source.as_bytes()),
                "{}",
                module.path
            );
            assert_eq!(
                hashes.sip64,
                cheby_runtime::consteval::source_hash(module.source.as_bytes())
            );
            assert_eq!(
                hashes.sip128,
                crate::cache::Hash128::of(module.source.as_bytes())
            );
        }
        assert_eq!(source_hashes("no_such_module"), None);
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

    /// M2 ships tier 2's concurrency modules: `std::fiber` and
    /// `std::channel`, `std::duration` with them, because `fiber::timeout`
    /// takes a `Duration` (D-645, D-646), and with timers and selectors
    /// `std::selector`, `std::random` and `std::runtime` (D-653, D-657,
    /// D-673). The rest of tier 2 stays planned, and `std::http` is the
    /// unshipped module of the probes (D-729).
    #[test]
    fn m2_tier2_modules_ship() {
        // The modules of timers and selectors first, then the first slice's.
        for path in [
            "random", "runtime", "selector", "channel", "duration", "fiber",
        ] {
            assert!(find(path).is_some(), "std::{path} is not shipped");
            assert!(!is_planned(path), "std::{path} is still planned");
        }
        for path in ["file", "http", "net", "process", "time"] {
            assert!(is_planned(path), "std::{path} is not planned");
            assert!(find(path).is_none(), "std::{path} is shipped");
        }
    }

    /// M2 ships tier 1's seven collections, written in Cheby (D-664,
    /// D-336). Each has its file in `std/`, is shipped and is no longer
    /// planned, so importing it is no longer E0038 (D-480).
    #[test]
    fn m2_collections_ship() {
        for path in [
            "multimap",
            "ordered_map",
            "ordered_multimap",
            "ordered_set",
            "sorted_map",
            "sorted_multimap",
            "sorted_set",
        ] {
            assert!(
                repo().join(display_path(path)).exists(),
                "std/{path}.cheby is missing"
            );
            assert!(find(path).is_some(), "std::{path} is not shipped");
            assert!(!is_planned(path), "std::{path} is still planned");
        }
    }

    /// The `test "name"` lines of a source or a stdlib-spec page.
    fn test_names(text: &str) -> Vec<&str> {
        text.lines()
            .filter_map(|line| line.strip_prefix("test \"")?.split_once("\" {"))
            .map(|(name, _)| name)
            .collect()
    }

    /// The std modules a source imports, by their `import std::…` lines:
    /// `import std::ops::{Compare}` imports `ops`, `import std::json::tree`
    /// imports `json::tree`.
    fn std_imports(source: &str) -> Vec<&str> {
        source
            .lines()
            .filter_map(|line| line.strip_prefix("import std::"))
            .map(|rest| {
                let rest = rest.split_once(" as ").map_or(rest, |(path, _)| path);
                let rest = rest.split_once("::{").map_or(rest, |(path, _)| path);
                rest.trim()
            })
            .collect()
    }

    /// Whether a test of `module`'s page, named `wanted` in `std/`, is in
    /// `module` itself or in a module that imports it (D-479, D-630). A
    /// test of the same name in an unrelated module does not count: the
    /// seven collections' pages share many names with `std::map` and
    /// `std::set`.
    fn spec_test_present(modules: &[StdModule], module: &str, wanted: &str) -> bool {
        modules.iter().any(|m| {
            (m.path == module || std_imports(m.source).contains(&module))
                && test_names(m.source).contains(&wanted)
        })
    }

    #[test]
    fn std_imports_name_modules() {
        let source = "//! x\n\nimport std::json::tree\nimport std::list\n\
                      import std::ops::{Add, Compare}\n\nfn f() {}\n";
        assert_eq!(std_imports(source), ["json::tree", "list", "ops"]);
    }

    /// The guard is per module: a page test counts only in its own module
    /// or in one that imports it.
    #[test]
    fn spec_tests_count_only_in_their_module_or_an_importer() {
        let modules = [
            StdModule {
                path: "a",
                source: "test \"new is empty\" {\n}\n",
            },
            StdModule {
                path: "b",
                source: "import std::c\n\ntest \"moved\" {\n}\n",
            },
            StdModule {
                path: "c",
                source: "test \"own\" {\n}\n",
            },
        ];
        assert!(spec_test_present(&modules, "a", "new is empty"));
        assert!(!spec_test_present(&modules, "c", "new is empty"));
        assert!(spec_test_present(&modules, "c", "own"));
        assert!(spec_test_present(&modules, "c", "moved"));
        assert!(!spec_test_present(&modules, "a", "moved"));
    }

    /// A milestone `M0` to `M6` as its number.
    fn milestone_number(text: &str) -> Result<u8, String> {
        let text = text.trim();
        match text.as_bytes() {
            [b'M', digit @ b'0'..=b'6'] => Ok(digit.saturating_sub(b'0')),
            _ => Err(format!("unknown milestone `{text}`, expected M0 to M6")),
        }
    }

    /// The milestone in effect: `CHEBY_MILESTONE` when it is set and not
    /// empty, `tests/milestone` otherwise, as the harnesses read it (D-544,
    /// D-567, `cheby_test_support::milestone::in_effect`).
    fn milestone_in_effect() -> u8 {
        if let Some(value) = std::env::var_os("CHEBY_MILESTONE") {
            let value = value.into_string().unwrap();
            if !value.trim().is_empty() {
                return milestone_number(&value)
                    .map_err(|e| format!("CHEBY_MILESTONE: {e}"))
                    .unwrap();
            }
        }
        let file = std::fs::read_to_string(repo().join("tests/milestone")).unwrap();
        milestone_number(&file).unwrap()
    }

    /// One test of a shipped module's page in `docs/stdlib/`.
    struct SpecTest<'a> {
        module: &'a str,
        name: &'a str,
        /// Whether the page's module, or a std module that imports it, has
        /// the test under its name in `std/`.
        present: bool,
    }

    /// What is wrong with the spec tests and the pending list `pending`
    /// while milestone `in_effect` is in progress (D-728). A test missing
    /// from `std/` is wrong unless an entry lists it for `in_effect` or a
    /// later milestone. An entry is wrong if its milestone has ended, if
    /// its test is in `std/`, if it names no spec test of a shipped
    /// module, or if it repeats an earlier entry.
    ///
    /// An entry for a milestone after the one in effect accepts the test
    /// as absent, as D-288 expects a later milestone's test to fail
    /// (D-741).
    fn spec_test_problems(
        tests: &[SpecTest<'_>],
        pending: &[(&str, &str, &str)],
        in_effect: u8,
    ) -> Vec<String> {
        let mut problems = Vec::new();
        for (k, &(module, name, milestone)) in pending.iter().enumerate() {
            let shown = format!("PENDING_SPEC_TESTS entry std::{module} \"{name}\" ({milestone})");
            if pending
                .iter()
                .take(k)
                .any(|&(m, n, _)| m == module && n == name)
            {
                problems.push(format!("{shown} is listed twice"));
            }
            let milestone = match milestone_number(milestone) {
                Ok(n) => n,
                Err(e) => {
                    problems.push(format!("{shown}: {e}"));
                    continue;
                }
            };
            match tests.iter().find(|t| t.module == module && t.name == name) {
                None => problems.push(format!(
                    "{shown} names no test of a shipped module's page in docs/stdlib"
                )),
                Some(test) if test.present => problems.push(format!(
                    "{shown} is in std/; remove it from PENDING_SPEC_TESTS"
                )),
                Some(_) if milestone < in_effect => problems.push(format!(
                    "{shown} is still missing from std/, and M{milestone} has ended \
                     (M{in_effect} is in effect)"
                )),
                Some(_) => {}
            }
        }
        for test in tests {
            let listed = pending
                .iter()
                .any(|&(m, n, _)| m == test.module && n == test.name);
            if !test.present && !listed {
                problems.push(format!(
                    "std::{} \"{}\" is missing from std/",
                    test.module, test.name
                ));
            }
        }
        problems
    }

    /// Every test of the stdlib spec for a shipped module is in `std/`
    /// (H3, D-479), unless [`PENDING_SPEC_TESTS`] lists it for the
    /// milestone in effect (D-728). The check is per module: a page's test
    /// counts only in the page's module, or in a module that imports it,
    /// where a test that needs that module lives (D-145, D-630). Two of
    /// those are renamed there, because `std::list` holds both and test
    /// names are unique in a module.
    #[test]
    fn stdlib_spec_tests_are_in_std() {
        const RENAMED: &[(&str, &str, &str)] = &[
            (
                "float",
                "add is plus as a function",
                "float add is plus as a function",
            ),
            (
                "int",
                "add is plus as a function",
                "int add is plus as a function",
            ),
        ];
        let mut spec_pages = Vec::new();
        module_files(&repo().join("docs/stdlib"), "", ".md", &mut spec_pages);
        spec_pages.sort();
        let pages: Vec<(String, String)> = spec_pages
            .into_iter()
            .filter(|module| find(module).is_some())
            .map(|module| {
                let page = std::fs::read_to_string(
                    repo().join(format!("docs/stdlib/{}.md", module.replace("::", "/"))),
                )
                .unwrap();
                (module, page)
            })
            .collect();
        let mut tests = Vec::new();
        for (module, page) in &pages {
            for name in test_names(page) {
                let wanted = RENAMED
                    .iter()
                    .find(|(m, n, _)| *m == module.as_str() && *n == name)
                    .map_or(name, |(_, _, renamed)| *renamed);
                let present = spec_test_present(STD_MODULES, module, wanted);
                tests.push(SpecTest {
                    module,
                    name,
                    present,
                });
            }
        }
        let problems = spec_test_problems(&tests, PENDING_SPEC_TESTS, milestone_in_effect());
        assert!(problems.is_empty(), "{problems:#?}");
        assert!(tests.len() > 300, "{} spec tests", tests.len());
    }

    /// The rules of D-728, on a synthetic list.
    #[test]
    fn pending_spec_tests_follow_the_milestone() {
        let tests = [
            SpecTest {
                module: "fiber",
                name: "spawn runs",
                present: true,
            },
            SpecTest {
                module: "fiber",
                name: "timeout cancels",
                present: false,
            },
        ];
        // Not listed and absent: missing.
        assert_eq!(
            spec_test_problems(&tests, &[], 2),
            ["std::fiber \"timeout cancels\" is missing from std/"]
        );
        // Listed and absent while its milestone is in effect: accepted.
        let listed = [("fiber", "timeout cancels", "M2")];
        let none: Vec<String> = Vec::new();
        assert_eq!(spec_test_problems(&tests, &listed, 2), none);
        // ... and while it has not started.
        assert_eq!(spec_test_problems(&tests, &listed, 1), none);
        // Past its milestone, as with CHEBY_MILESTONE=M3: the entry fails.
        let past = spec_test_problems(&tests, &listed, 3);
        assert_eq!(past.len(), 1, "{past:?}");
        assert!(past[0].contains("M2 has ended"), "{past:?}");
        // A listed test that is in std/ fails at any milestone.
        let present = [("fiber", "spawn runs", "M2")];
        for in_effect in [1, 2, 3] {
            let problems = spec_test_problems(&tests, &[listed[0], present[0]], in_effect);
            assert!(
                problems.iter().any(|p| p.contains("is in std/")),
                "{problems:?}"
            );
        }
        // Stale, malformed and repeated entries fail.
        let unknown = spec_test_problems(&tests, &[listed[0], ("fiber", "no such", "M2")], 2);
        assert_eq!(unknown.len(), 1, "{unknown:?}");
        assert!(unknown[0].contains("names no test"), "{unknown:?}");
        let malformed = spec_test_problems(&tests, &[("fiber", "timeout cancels", "M9")], 2);
        assert!(
            malformed
                .iter()
                .any(|p| p.contains("unknown milestone `M9`")),
            "{malformed:?}"
        );
        let twice = spec_test_problems(&tests, &[listed[0], listed[0]], 2);
        assert_eq!(twice.len(), 1, "{twice:?}");
        assert!(twice[0].contains("listed twice"), "{twice:?}");
    }

    #[test]
    fn milestone_numbers() {
        assert_eq!(milestone_number("M2\n"), Ok(2));
        assert_eq!(milestone_number(" M0 "), Ok(0));
        assert_eq!(milestone_number("M6"), Ok(6));
        for bad in ["M7", "M", "2", "m2", "M02", "M2x", ""] {
            assert!(milestone_number(bad).is_err(), "{bad}");
        }
        assert!(milestone_in_effect() <= 6);
    }

    /// The list is in the guard's format: every milestone parses.
    #[test]
    fn pending_spec_tests_name_milestones() {
        for (module, name, milestone) in PENDING_SPEC_TESTS {
            assert!(
                milestone_number(milestone).is_ok(),
                "std::{module} \"{name}\": {milestone}"
            );
        }
    }

    #[test]
    fn display_paths() {
        assert_eq!(display_path("io"), "std/io.cheby");
        assert_eq!(display_path("json::decode"), "std/json/decode.cheby");
    }
}
