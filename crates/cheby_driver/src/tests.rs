//! Tests of `check` on the examples and on small temporary packages.

use std::path::{Path, PathBuf};

use super::*;

fn examples() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
    let mut dirs: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join(MANIFEST_FILE).is_file())
        .collect();
    dirs.sort();
    dirs
}

/// A package in a fresh temporary directory with these files.
fn package(name: &str, files: &[(&str, &[u8])]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cheby_driver_{}_{name}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    for (path, contents) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    dir
}

const MANIFEST: (&str, &[u8]) = ("cheby.toml", b"name = \"app\"\nversion = \"0.1.0\"\n");

/// Every example passes the syntax layer. Example 030 has dependencies,
/// which are rejected until M5.
#[test]
fn examples_pass_the_syntax_layer() {
    for dir in examples() {
        let out = check_syntax(&dir).unwrap();
        let rendered = out.render();
        if dir.ends_with("030_shop_service") {
            assert_eq!(out.diagnostics.len(), 1, "{rendered}");
            assert!(rendered.starts_with("error[E0036]: dependencies are not supported"));
        } else {
            assert_eq!(rendered, "", "{}", dir.display());
        }
    }
}

/// The examples that need std modules not shipped yet move to the
/// milestone that ships those modules: their imports of them are E0038
/// (D-451, D-480). M2's first slice shipped `std::fiber`, `std::channel`
/// and `std::duration`, so 005, 018, 022 and 028 left the list (D-729),
/// and with `std::selector`, `std::random` and `std::runtime` 009, 013 and
/// 019 left it too.
const LATER_EXAMPLES: [&str; 1] = ["031_web_scraper"];

/// The one example allowed a diagnostic in the clean check: 022 keeps the
/// `todo` of `money::apply_interest`, which warns with W0009 on purpose
/// (D-068, D-663).
const TODO_EXAMPLE: &str = "022_bank_accounts";

/// Every example of the M1 exit criterion, 020 and the examples of M2's
/// slices check with no diagnostics at all, warnings included
/// (D-477), except 022's one W0009 at `src/money.cheby` (D-663). The
/// cross-target check covers every target the manifest declares, so this
/// is the check for both targets (D-512).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn examples_check_cleanly() {
    let mut checked = 0;
    for dir in examples() {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        if name == "030_shop_service" || LATER_EXAMPLES.contains(&name.as_str()) {
            continue;
        }
        let out = check(&dir).unwrap();
        let rendered = out.render();
        if name == TODO_EXAMPLE {
            let codes: Vec<String> = out.diagnostics.iter().map(|d| d.code.to_string()).collect();
            assert_eq!(codes, ["W0009"], "{name}:\n{rendered}");
            assert!(rendered.contains("src/money.cheby:"), "{name}:\n{rendered}");
        } else {
            assert_eq!(rendered, "", "{name}");
        }
        checked += 1;
    }
    assert_eq!(checked, 30);
}

/// An example that needs a std module of a later tier reports E0038 at
/// each import of one, and nothing else: paths through such an import are
/// silent, and its other code checks. 030 still stops after phase A
/// (D-446).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn later_examples_report_only_unshipped_modules() {
    for dir in examples() {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let out = check(&dir).unwrap();
        let rendered = out.render();
        if name == "030_shop_service" {
            let codes: Vec<String> = out.diagnostics.iter().map(|d| d.code.to_string()).collect();
            assert_eq!(codes, ["E0036"], "{rendered}");
            continue;
        }
        if !LATER_EXAMPLES.contains(&name.as_str()) {
            continue;
        }
        let mut imports = 0;
        for d in &out.diagnostics {
            let code = d.code.to_string();
            let module = d
                .message
                .strip_prefix("the module `std::")
                .and_then(|m| m.strip_suffix("` is not supported yet"));
            assert!(
                code == "E0038"
                    && module.is_some_and(|m| std_lib::PLANNED_STD_MODULES.contains(&m)),
                "{name}:\n{rendered}"
            );
            imports += 1;
        }
        assert!(imports > 0, "{name}");
    }
}

/// What one front-end run over a package produces, by module path: the
/// rendered diagnostics, and each checked module's canonical interface
/// encoding and hash.
fn front_end_output(dir: &Path) -> (String, Vec<(String, Vec<u8>, String)>) {
    let mut out = CheckOutput::default();
    let front = front_end(dir, frontend::Mode::Check, &mut out).unwrap();
    out.sources.sort(&mut out.diagnostics);
    let mut ifaces = Vec::new();
    if let Some((front, _)) = front {
        for &id in &front.order {
            let Some(checked) = front.checked(id) else {
                continue;
            };
            let iface = front.set.get(id).unwrap();
            let deps = cheby_iface::DepView::new(&front.set, &checked.deps);
            let bytes = cheby_iface::encode(iface, &deps, &front.modules, &front.globals).unwrap();
            let hash =
                cheby_iface::interface_hash(iface, &deps, &front.modules, &front.globals).unwrap();
            let path = front.modules.get(id).unwrap().path.to_string();
            ifaces.push((path, bytes, format!("{hash:?}")));
        }
    }
    ifaces.sort();
    (out.render(), ifaces)
}

/// Determinism (D-264): two front-end runs over every example give
/// byte-identical rendered diagnostics, interface encodings and interface
/// hashes, std modules included.
#[test]
fn the_front_end_is_deterministic() {
    let mut modules = 0;
    for dir in examples() {
        let first = front_end_output(&dir);
        let second = front_end_output(&dir);
        assert_eq!(first.0, second.0, "{}", dir.display());
        assert_eq!(first.1.len(), second.1.len(), "{}", dir.display());
        for (a, b) in first.1.iter().zip(&second.1) {
            assert_eq!(a, b, "{}: {}", dir.display(), a.0);
        }
        modules += first.1.len();
    }
    assert!(modules > 300, "{modules}");
}

/// `cheby run` on every example compiles it or reports diagnostics, never
/// an internal compiler error (D-464, D-510). Nothing runs: 001 runs in
/// `hello_world_runs_without_leaks`, and the others may read input.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn run_never_fails_internally_on_examples() {
    let mut failures = Vec::new();
    for dir in examples() {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        match prepare_run(&dir, RunOptions::default()) {
            Ok(Prepared::Ready(program)) => {
                assert!(!program.output().has_errors(), "{name}");
            }
            Ok(Prepared::CompileFailed(out)) => assert!(out.has_errors(), "{name}"),
            Err(e) => failures.push(format!("{name}: {}", e.message())),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The same on every UI test source: a single file as `src/main.cheby` of
/// a package, like the UI harness, or a package test as it is. Tool errors
/// for a package without `main` are fine; an internal error is not.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn run_never_fails_internally_on_ui_tests() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/ui");
    let mut cases = Vec::new();
    collect_ui_tests(&root, &mut cases);
    assert!(cases.len() > 50, "found only {} UI tests", cases.len());
    let mut failures = Vec::new();
    for (n, case) in cases.iter().enumerate() {
        let dir = if case.is_dir() {
            case.clone()
        } else {
            let source = std::fs::read(case).unwrap();
            package(
                &format!("ui_{n}"),
                &[
                    ("cheby.toml", b"name = \"ui_test\"\nversion = \"0.1.0\"\n"),
                    ("src/main.cheby", &source),
                ],
            )
        };
        if let Err(e) = prepare_run(&dir, RunOptions::default())
            && e.message().starts_with("internal compiler error")
        {
            failures.push(format!("{}: {}", case.display(), e.message()));
        }
        if !case.is_dir() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Whether every E0038 in `out` is the resolver's, for an import of a std
/// module that is planned but not shipped (D-480, D-451).
fn only_unshipped_imports_are_e0038(out: &CheckOutput) -> bool {
    out.diagnostics
        .iter()
        .filter(|d| d.code == cheby_diag::codes::NOT_SUPPORTED_YET)
        .all(|d| {
            d.message
                .strip_prefix("the module `std::")
                .and_then(|m| m.strip_suffix("` is not supported yet"))
                .is_some_and(|m| std_lib::PLANNED_STD_MODULES.contains(&m))
        })
}

/// Lowers every root of the package at `dir` that the front end accepts
/// (its root module's `main`, its tests, its constants). `Ok(None)` when
/// the front end reports an error; `Err` with the problem otherwise.
fn lower_package_roots(dir: &Path) -> Result<Option<usize>, String> {
    let mut out = CheckOutput::default();
    let front =
        front_end(dir, frontend::Mode::Build, &mut out).map_err(|e| e.message().to_owned())?;
    if !only_unshipped_imports_are_e0038(&out) {
        return Err(out.render());
    }
    let Some((front, package)) = front.filter(|_| !out.has_errors()) else {
        return Ok(None);
    };
    let main = front
        .local_module(&ModulePath::root(&package))
        .and_then(|m| front.main_of(m));
    let units = backend::lower_every_root(&front, &out.sources, main)
        .map_err(|ice| ice.message().to_owned())?;
    Ok(Some(units.len()))
}

/// MIR lowering emits no E0038 (D-554): every example and every run-pass
/// and run-fail UI test lowers all it can build (what `main` reaches, its
/// tests on native, its constants) with no diagnostic and no internal
/// error. E0038 is left only for imports of unshipped std modules, which
/// the resolver reports before anything is lowered (D-480); the later-tier
/// examples are such packages.
#[test]
fn lowering_reports_no_e0038() {
    let mut failures = Vec::new();
    let mut lowered = 0_usize;
    for dir in examples() {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        match lower_package_roots(&dir) {
            Ok(Some(units)) => {
                assert!(units > 0, "{name}");
                lowered += 1;
            }
            Ok(None) => assert!(
                name == "030_shop_service" || LATER_EXAMPLES.contains(&name.as_str()),
                "{name} has front-end errors"
            ),
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    assert_eq!(lowered, 30, "{}", failures.join("\n"));
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/ui");
    let mut cases = Vec::new();
    collect_ui_tests(&root, &mut cases);
    let mut runs = 0_usize;
    for (n, case) in cases.iter().enumerate() {
        let main = if case.is_dir() {
            case.join("src/main.cheby")
        } else {
            case.clone()
        };
        let source = std::fs::read(&main).unwrap();
        let header = String::from_utf8_lossy(&source);
        let mode = header
            .lines()
            .take_while(|l| l.starts_with("//"))
            .map(|l| l.trim_start_matches('/').trim())
            .find_map(|l| l.strip_prefix("@mode:"))
            .map(str::trim);
        let fails = match mode {
            Some("run-pass") => false,
            Some("run-fail") => true,
            _ => continue,
        };
        let dir = if case.is_dir() {
            case.clone()
        } else {
            package(
                &format!("lower_ui_{n}"),
                &[
                    ("cheby.toml", b"name = \"ui_test\"\nversion = \"0.1.0\"\n"),
                    ("src/main.cheby", &source),
                ],
            )
        };
        match lower_package_roots(&dir) {
            Ok(Some(_)) => runs += 1,
            // A run-fail test may fail to compile on purpose, such as
            // `cheby test` stopping at a compile error (D-541); its
            // diagnostics hold no E0038 all the same.
            Ok(None) if fails => {}
            Ok(None) => failures.push(format!("{}: front-end errors", case.display())),
            Err(e) => failures.push(format!("{}: {e}", case.display())),
        }
        if !case.is_dir() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(runs > 190, "lowered only {runs} run UI tests");
}

/// Single-file and package UI tests under `dir`, sorted.
fn collect_ui_tests(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if path.join(MANIFEST_FILE).is_file() {
                out.push(path);
            } else {
                collect_ui_tests(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "cheby") {
            out.push(path);
        }
    }
}

/// A package whose `main` converts a `String` to `dyn Show`, or does not
/// call the function that does.
fn dyn_package(name: &str, main_calls_it: bool) -> PathBuf {
    let call = if main_calls_it {
        "  io::println(pick(\"a\"))\n"
    } else {
        ""
    };
    let source = format!(
        "//! Back-end gate.\n\nimport std::io\n\nfn main() {{\n  io::println(\"start\")\n{call}}}\n\nfn pick(text: String) -> String {{\n  let shown: dyn Show = text\n  Show::show(shown)\n}}\n"
    );
    package(name, &[MANIFEST, ("src/main.cheby", source.as_bytes())])
}

/// Conversions to `dyn` were the last construct whose E0038 stopped `cheby
/// run` before any code was generated (D-510); since W4-A they compile,
/// reachable or not, and the program runs without leaks.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn conversions_to_dyn_compile_and_run() {
    for (name, calls) in [("dyn_reachable", true), ("dyn_unreachable", false)] {
        let dir = dyn_package(name, calls);
        let checked = check(&dir).unwrap();
        let codes: Vec<String> = checked
            .diagnostics
            .iter()
            .map(|d| d.code.to_string())
            .collect();
        // An uncalled `pick` is W0005 under `cheby check` only (D-266).
        let unused = if calls {
            vec![]
        } else {
            vec!["W0005".to_owned()]
        };
        assert_eq!(codes, unused, "{name}");
        let RunResult::Ran { output, exit } = run(&dir, RunOptions::default()).unwrap() else {
            panic!("{name} did not compile");
        };
        assert_eq!(output.render(), "", "{name}");
        assert_eq!(exit, 0, "{name}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// The dumps show what running the root `main` compiles: the items it
/// reaches. A package without a root `main` dumps nothing.
#[test]
fn dumps_follow_reachability() {
    let (out, mir) = dump_mir(&test_package("lets")).unwrap();
    assert!(!out.has_errors(), "{}", out.render());
    // `eprint` is never called.
    assert!(!mir.contains("std::io::eprint("), "{mir}");
    assert!(mir.contains("fn std::io::eprintln("), "{mir}");
    let dir = package(
        "dump_no_main",
        &[
            MANIFEST,
            ("src/main.cheby", b"//! No main.\n\nfn f() {\n  Nil\n}\n"),
        ],
    );
    let (out, mir) = dump_mir(&dir).unwrap();
    assert_eq!(out.render(), "");
    assert_eq!(mir, "");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Every shipped std module checks cleanly when a package imports it.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_checks_cleanly() {
    for module in std_lib::STD_MODULES {
        let source = format!(
            "//! Imports a std module.\n\nimport std::{}\n\nfn main() {{\n  Nil\n}}\n",
            module.path
        );
        let dir = package(
            &format!("std_{}", module.path.replace("::", "_")),
            &[MANIFEST, ("src/main.cheby", source.as_bytes())],
        );
        let mut out = check(&dir).unwrap();
        // The import itself is unused on purpose.
        let sources = &out.sources;
        let in_main = |d: &Diagnostic| {
            d.labels.iter().all(|l| {
                sources
                    .file(l.span.file)
                    .is_some_and(|f| f.path() == "src/main.cheby")
            })
        };
        out.diagnostics
            .retain(|d| !(d.code.to_string() == "W0003" && in_main(d)));
        assert_eq!(out.render(), "", "std::{}", module.path);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// A user build skips the `test` blocks of std modules: they are checked
/// and run only when testing std itself (G4).
#[test]
fn user_builds_skip_std_tests() {
    assert!(std_lib::find("io").unwrap().source.contains("\ntest \""));
    let mut out = CheckOutput::default();
    let (front, _) = front_end(&example("001_hello_world"), frontend::Mode::Check, &mut out)
        .unwrap()
        .unwrap();
    assert_eq!(out.render(), "");
    let mut std_modules = 0;
    for &id in &front.order {
        if front.modules.get(id).unwrap().package == cheby_iface::PackageId::STD {
            std_modules += 1;
            assert_eq!(front.checked(id).unwrap().thir.tests.len(), 0);
        }
    }
    assert!(std_modules > 1, "{std_modules}");
}

/// Modules that import each other are one E0040, and modules on the cycle
/// are not checked; others are.
#[test]
fn import_cycles_are_reported() {
    let dir = package(
        "cycle",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                b"//! Root.\n\nimport app::a\n\nfn main() {\n  Nil\n}\n",
            ),
            (
                "src/a.cheby",
                b"//! A.\n\nimport app::b\n\nfn f() {\n  Nil\n}\n",
            ),
            (
                "src/b.cheby",
                b"//! B.\n\nimport app::a\n\nfn g() {\n  Nil\n}\n",
            ),
            (
                "src/c.cheby",
                b"//! C, outside the cycle.\n\nfn h() -> String {\n  Nil\n}\n",
            ),
        ],
    );
    let out = check(&dir).unwrap();
    let rendered = out.render();
    assert!(
        rendered.starts_with("error[E0040]: modules import each other in a cycle"),
        "{rendered}"
    );
    assert!(rendered.contains("app::a → app::b → app::a"), "{rendered}");
    // `c` is still checked: its mismatch is reported.
    assert!(rendered.contains("error[E0047]"), "{rendered}");
    assert_eq!(out.diagnostics.len(), 2, "{rendered}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn missing_and_planned_std_modules() {
    let dir = package(
        "std_imports",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                b"//! Imports.\n\nimport std::http\nimport std::ioo\n\nfn main() {\n  Nil\n}\n",
            ),
        ],
    );
    let rendered = check(&dir).unwrap().render();
    // D-729: `std::http` stays unshipped until tier 3.
    assert!(
        rendered.contains("error[E0038]: the module `std::http` is not supported yet"),
        "{rendered}"
    );
    assert!(
        rendered.contains("error[E0039]: there is no module `std::ioo`"),
        "{rendered}"
    );
    assert!(rendered.contains("`std::io`"), "{rendered}");
    std::fs::remove_dir_all(&dir).unwrap();
}

fn golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var_os("CHEBY_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (bless with CHEBY_BLESS=1)", path.display()));
    assert_eq!(actual, expected, "{name} differs from {}", path.display());
}

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/examples")
        .join(name)
}

fn test_package(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/packages")
        .join(name)
}

/// The MIR of 001 after reference counting (D1), the same twice
/// (D-264).
#[test]
fn mir_of_hello_world() {
    let (out, mir) = dump_mir(&example("001_hello_world")).unwrap();
    assert_eq!(out.render(), "");
    golden("001_hello_world.mir", &mir);
    assert_eq!(dump_mir(&example("001_hello_world")).unwrap().1, mir);
}

/// `let`, dups, drops, tail calls within and across modules.
#[test]
fn mir_of_lets_and_tail_calls() {
    // `unused` is unused on purpose (W0002): its value is dropped.
    let (out, mir) = dump_mir(&test_package("lets")).unwrap();
    assert!(!out.has_errors(), "{}", out.render());
    golden("lets.mir", &mir);
}

/// The C convention's name differs between hosts; goldens use `system_v`.
fn normalize_clif(text: &str) -> String {
    text.replace("apple_aarch64", "system_v")
        .replace("windows_fastcall", "system_v")
}

/// The CLIF of 001 before Cranelift compiles it: the stack check, the slot
/// loads, `return_call` for the tail call, the static strings. The same
/// twice (D-264).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn clif_of_hello_world() {
    let (out, clif) = dump_clif(&example("001_hello_world")).unwrap();
    assert_eq!(out.render(), "");
    let clif = normalize_clif(&clif);
    golden("001_hello_world.clif", &clif);
    let again = normalize_clif(&dump_clif(&example("001_hello_world")).unwrap().1);
    assert_eq!(again, clif);
}

#[test]
fn dumps_of_a_package_with_errors_are_empty() {
    let dir = package(
        "dump_errors",
        &[
            MANIFEST,
            ("src/main.cheby", b"//! Bad.\n\nfn main() {\n  1 + 2\n}\n"),
        ],
    );
    let (out, mir) = dump_mir(&dir).unwrap();
    assert!(out.has_errors());
    assert_eq!(mir, "");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `cheby run` reports every native symbol it cannot resolve, one E0053
/// each at the `@external` that names it, sorted by position, before
/// anything runs (D-532); `cheby check` reports none of them (D-455).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn every_missing_native_symbol_is_reported() {
    let main = b"//! Two missing natives.\n\n\
        @external(native, \"cheby_test_missing_second\")\n\
        fn second()\n\n\
        @external(native, \"cheby_test_missing_first\")\n\
        fn first()\n\n\
        fn main() {\n  first()\n  second()\n}\n";
    let dir = package(
        "missing_natives",
        &[
            (
                "cheby.toml",
                b"name = \"app\"\nversion = \"0.1.0\"\ntargets = [\"native\"]\n",
            ),
            ("src/main.cheby", main),
        ],
    );
    assert_eq!(check(&dir).unwrap().render(), "");
    let RunResult::CompileFailed(out) = run(&dir, RunOptions::default()).unwrap() else {
        panic!("a program with missing natives ran");
    };
    let codes: Vec<String> = out.diagnostics.iter().map(|d| d.code.to_string()).collect();
    assert_eq!(codes, ["E0053", "E0053"], "{}", out.render());
    let rendered = out.render();
    let second = rendered
        .find("`cheby_test_missing_second` is not defined")
        .unwrap();
    let first = rendered
        .find("`cheby_test_missing_first` is not defined")
        .unwrap();
    assert!(second < first, "not sorted by position:\n{rendered}");
    assert!(rendered.contains("--> src/main.cheby:3:1"), "{rendered}");
    assert!(rendered.contains("--> src/main.cheby:6:1"), "{rendered}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Runs 001 in this process: status 0 and, in debug builds, no object
/// left live (the leak check would give 102).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn hello_world_runs_without_leaks() {
    let result = run(&example("001_hello_world"), RunOptions::default()).unwrap();
    let RunResult::Ran { output, exit } = result else {
        panic!("001 failed to compile");
    };
    assert_eq!(output.render(), "");
    assert_eq!(exit, 0);
    assert_eq!(cheby_runtime::live_objects(), 0);
}

/// A type with a constructor of 255 value fields, more than a header's
/// scan count holds (D-279): building only its nullary sibling registers
/// the whole type's layouts, and the run succeeds with no internal error
/// and no leak (D-555).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn wide_constructors_escape_the_scan_count() {
    let fields: String = (0..255).map(|_| "    String,\n").collect();
    let source = format!(
        "//! A constructor with 255 value fields.\n\nimport std::io\n\npub type Big {{\n  Wide(\n{fields}  ),\n  Small,\n}}\n\npub fn small() -> Big {{\n  Small\n}}\n\nfn main() {{\n  let _b = small()\n  io::println(\"ok\")\n}}\n"
    );
    let dir = package(
        "wide_ctor",
        &[MANIFEST, ("src/main.cheby", source.as_bytes())],
    );
    let result = run(&dir, RunOptions::default()).unwrap();
    let RunResult::Ran { output, exit } = result else {
        panic!("the program failed to compile");
    };
    assert_eq!(output.render(), "");
    assert_eq!(exit, 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// D-369: under `cheby run`, `env::program_name()` is the package's name
/// from `cheby.toml`, also when another module's `main` runs, and the
/// arguments after `--` are `env::args()`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn program_name_is_the_package_name() {
    let tools = b"//! A tool with its own `main`.\n\nimport std::env\nimport std::io\nimport std::string\n\nfn main() {\n  io::println(env::program_name())\n  io::println(string::join(env::args(), \",\"))\n}\n";
    let dir = package(
        "program_name",
        &[
            MANIFEST,
            ("src/main.cheby", b"//! The root module.\n\nfn main() {}\n"),
            ("src/tools.cheby", tools),
        ],
    );
    let options = RunOptions {
        module_path: Some("app::tools".to_owned()),
        args: vec!["a".to_owned(), "b c".to_owned()],
        ..RunOptions::default()
    };
    let Prepared::Ready(program) = prepare_run(&dir, options).unwrap() else {
        panic!("the program failed to compile");
    };
    assert_eq!(program.config.program_name, "app");
    assert_eq!(program.config.args, ["a", "b c"]);
    assert_eq!(program.output().render(), "");
    assert_eq!(program.run().unwrap(), 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn run_tool_errors() {
    let dir = package(
        "run_errors",
        &[
            MANIFEST,
            ("src/main.cheby", b"//! Root.\n\nfn main() {\n  Nil\n}\n"),
            ("src/lib.cheby", b"//! No main.\n\nfn f() {\n  Nil\n}\n"),
        ],
    );
    let missing = RunOptions {
        module_path: Some("app::nope".to_owned()),
        ..RunOptions::default()
    };
    assert_eq!(
        run(&dir, missing).unwrap_err().message(),
        "there is no module `app::nope` in this package"
    );
    let no_main = RunOptions {
        module_path: Some("app::lib".to_owned()),
        ..RunOptions::default()
    };
    assert_eq!(
        run(&dir, no_main).unwrap_err().message(),
        "module `app::lib` has no `main` function"
    );
    let bad = RunOptions {
        module_path: Some("app::".to_owned()),
        ..RunOptions::default()
    };
    assert_eq!(
        run(&dir, bad).unwrap_err().message(),
        "`app::` is not a module path"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn compile_errors_stop_before_running() {
    let dir = package(
        "run_compile_error",
        &[
            MANIFEST,
            ("src/main.cheby", b"//! Bad.\n\nfn main() {\n  nope()\n}\n"),
        ],
    );
    let RunResult::CompileFailed(out) = run(&dir, RunOptions::default()).unwrap() else {
        panic!("a package with errors ran");
    };
    assert!(out.render().contains("error[E0042]"), "{}", out.render());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn modules_are_loaded_in_path_order() {
    let dir = examples()
        .into_iter()
        .find(|d| d.ends_with("004_tictactoe"))
        .unwrap();
    let mut sources = SourceMap::new();
    let mut diags = Vec::new();
    let package = load_package(&dir, &mut sources, &mut diags)
        .unwrap()
        .unwrap();
    let paths: Vec<_> = package
        .modules
        .iter()
        .map(|m| {
            let file = sources.file(m.source.file).unwrap();
            format!("{} {}", file.path(), m.path)
        })
        .collect();
    assert_eq!(
        paths,
        [
            "src/ai.cheby tictactoe::ai",
            "src/board.cheby tictactoe::board",
            "src/main.cheby tictactoe",
            "src/player.cheby tictactoe::player",
        ]
    );
    assert!(package.modules.iter().all(|m| m.source.syntax.is_some()));
    assert!(package.modules.iter().all(|m| !m.source.has_syntax_errors));
}

#[test]
fn diagnostics_are_sorted_by_path_and_relative() {
    let dir = package(
        "sorted",
        &[
            MANIFEST,
            ("src/main.cheby", b"fn main() {\n  /* no */ Nil\n}\n"),
            ("src/a.cheby", b"fn f() {\n  1 $ 2\n}\n"),
        ],
    );
    let out = check(&dir).unwrap();
    assert!(out.has_errors());
    let rendered = out.render();
    let a = rendered.find("--> src/a.cheby:2:5").unwrap();
    let main = rendered.find("--> src/main.cheby:2:3").unwrap();
    assert!(a < main, "{rendered}");
    assert!(!rendered.contains(&dir.to_string_lossy().into_owned()));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn invalid_utf8_is_reported_and_not_parsed() {
    let dir = package(
        "utf8",
        &[
            MANIFEST,
            ("src/main.cheby", b"fn main() {\n  \"caf\xe9\"\n}\n"),
        ],
    );
    let mut sources = SourceMap::new();
    let mut diags = Vec::new();
    let package = load_package(&dir, &mut sources, &mut diags)
        .unwrap()
        .unwrap();
    let module = package.modules.first().unwrap();
    assert!(module.source.syntax.is_none());
    assert!(module.source.has_syntax_errors);
    assert_eq!(
        cheby_diag::render_all(&diags, &sources),
        "\
error[E0001]: source file is not valid UTF-8
 --> src/main.cheby:2:7
  |
2 |   \"caf\u{fffd}\"
  |       ^ this byte is not valid UTF-8
  |
  = help: save the file as UTF-8

"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_bad_manifest_stops_before_the_modules() {
    let dir = package(
        "bad_manifest",
        &[
            ("cheby.toml", b"name = \"App\"\nversion = \"0.1.0\"\n"),
            ("src/main.cheby", b"/* not read */\n"),
        ],
    );
    let out = check(&dir).unwrap();
    assert_eq!(out.diagnostics.len(), 1, "{}", out.render());
    assert!(out.render().starts_with("error[E0036]: package name `App`"));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Full validation runs only on files without grammar errors. A broken
/// file gets its grammar errors and the token-level checks, which do not
/// depend on the tree's shape, but no shape checks such as the short tuple
/// an unclosed `(` would look like.
#[test]
fn validation_of_files_with_grammar_errors() {
    let dir = package(
        "validate",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                b"fn main() {\n  let myValue = (1 +\n}\n\nfn g() -> String {\n  \"\\q\"\n}\n",
            ),
            (
                "src/ok.cheby",
                b"fn f() -> Int {\n  let myValue = 1\n  myValue\n}\n",
            ),
        ],
    );
    let out = check(&dir).unwrap();
    let rendered = out.render();
    assert!(
        rendered.contains("error[E0013]: expected an expression"),
        "{rendered}"
    );
    // E0011 is token-level, so it is reported.
    assert!(rendered.contains("error[E0011]"), "{rendered}");
    // E0030 is a shape check, so it is not.
    assert!(!rendered.contains("E0030"), "{rendered}");
    // Naming (E0006) is checked by name resolution, which a package with
    // phase-A errors does not reach (D-452), in either file.
    assert!(!rendered.contains("E0006"), "{rendered}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A reserved word that causes a grammar error explains it with a help on
/// that error, not with an E0007 of its own.
#[test]
fn reserved_words_explain_grammar_errors() {
    let dir = package(
        "reserved",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                b"fn main(x: Int) -> Int {\n  if x > 0 { 1 } else { 0 }\n}\n",
            ),
        ],
    );
    let out = check(&dir).unwrap();
    let rendered = out.render();
    assert!(rendered.starts_with("error[E0013]"), "{rendered}");
    assert!(
        rendered.contains("help: Cheby has no `if`; use `case`"),
        "{rendered}"
    );
    assert!(!rendered.contains("E0007"), "{rendered}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A file whose only problem is a badly spelled name reaches name
/// resolution, which reports it once, where it is bound.
#[test]
fn naming_is_reported_by_resolution() {
    let dir = package(
        "naming",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                b"fn main() {\n  let myValue = main\n  let go = myValue\n}\n",
            ),
        ],
    );
    let out = check(&dir).unwrap();
    let rendered = out.render();
    assert_eq!(rendered.matches("error[E0006]").count(), 1, "{rendered}");
    assert!(rendered.contains("--> src/main.cheby:2:7"), "{rendered}");
    assert_eq!(rendered.matches("error[E0007]").count(), 1, "{rendered}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn tool_errors() {
    let dir = package("no_src", &[MANIFEST]);
    let err = check(&dir).unwrap_err();
    assert_eq!(err.message(), "the package has no `src` directory");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn package_root_is_found_upwards() {
    let dir = package("root", &[MANIFEST, ("src/web/router.cheby", b"")]);
    assert_eq!(find_package_root(&dir.join("src/web")), Some(dir.clone()));
    assert_eq!(find_package_root(&dir), Some(dir.clone()));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn warnings_alone_do_not_fail() {
    let mut out = CheckOutput::default();
    assert!(!out.has_errors());
    out.diagnostics
        .push(Diagnostic::new(codes::TUPLE_SHAPE, "a warning"));
    assert!(!out.has_errors());
    out.diagnostics
        .push(Diagnostic::new(codes::SYNTAX_ERROR, "an error"));
    assert!(out.has_errors());
}

/// `code: message` of each diagnostic.
fn codes_of(out: &CheckOutput) -> Vec<String> {
    out.diagnostics
        .iter()
        .map(|d| format!("{}: {}", d.code, d.message))
        .collect()
}

/// `cheby check` evaluates the package's constants only after a front end
/// without errors (D-459, D-534): a panicking initializer is E0075 in a
/// clean package, and nothing beyond the type error in a package with one.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn check_evaluates_constants_only_without_front_end_errors() {
    let clean = package(
        "check_consts_clean",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                b"//! Root.\n\nconst boom: Int = 1 / zero()\n\nfn zero() -> Int {\n  0\n}\n\n\
                  fn main() {\n  let _ = boom\n}\n",
            ),
        ],
    );
    let found = codes_of(&check(&clean).unwrap());
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("E0075"), "{found:?}");
    std::fs::remove_dir_all(&clean).unwrap();

    let broken = package(
        "check_consts_broken",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                b"//! Root.\n\nconst boom: Int = 1 / zero()\n\nfn zero() -> Int {\n  0\n}\n\n\
                  fn main() {\n  let n: Int = \"one\"\n  let _ = boom + n\n}\n",
            ),
        ],
    );
    let found = codes_of(&check(&broken).unwrap());
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("E0047"), "{found:?}");
    std::fs::remove_dir_all(&broken).unwrap();
}

/// A package with two modules of tests: the selection by filter and by
/// module, and the report's counts (D-541).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn tests_are_selected_and_counted() {
    let dir = package(
        "test_selection",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                b"//! Root.\n\ntest \"root passes\" {\n  assert 1 == 1\n}\n",
            ),
            (
                "src/util.cheby",
                b"//! Util.\n\ntest \"util passes\" {\n  assert 2 == 2\n}\n\n\
                  test \"util fails\" {\n  assert 2 == 3\n}\n",
            ),
        ],
    );
    let run = |options: &TestOptions| -> (TestReport, String) {
        let PreparedTests::Ready(program) = prepare_test(&dir, options).unwrap() else {
            panic!("the package does not compile");
        };
        let mut out = Vec::new();
        let report = program.run(&mut out, &mut std::io::sink()).unwrap();
        (report, String::from_utf8(out).unwrap())
    };
    let (report, text) = run(&TestOptions::default());
    assert_eq!(
        report.counts(),
        TestCounts {
            passed: 2,
            failed: 1,
            not_built: 0
        }
    );
    assert!(!report.success());
    assert_eq!(report.exit_status(), 1);
    assert!(text.starts_with(
        "test app \"root passes\" ... ok\ntest app::util \"util passes\" ... ok\n\
         test app::util \"util fails\" ... FAILED\n\n---- app::util \"util fails\" ----\n"
    ));
    assert!(text.ends_with("\ntest result: FAILED. 2 passed; 1 failed; 0 filtered out\n"));

    let (report, text) = run(&TestOptions {
        filter: Some("passes".to_owned()),
        ..TestOptions::default()
    });
    assert!(report.success());
    assert_eq!(report.filtered_out, 1);
    assert!(text.ends_with("test result: ok. 2 passed; 0 failed; 1 filtered out\n"));

    let (report, _) = run(&TestOptions {
        module: Some("app::util".to_owned()),
        ..TestOptions::default()
    });
    assert_eq!(report.modules.len(), 1);
    assert_eq!(report.modules[0].path, "app::util");
    assert_eq!(report.filtered_out, 1);

    for (module, message) in [
        ("app::", "`app::` is not a module path"),
        (
            "app::nope",
            "there is no module `app::nope` in this package",
        ),
    ] {
        let options = TestOptions {
            module: Some(module.to_owned()),
            ..TestOptions::default()
        };
        assert_eq!(prepare_test(&dir, &options).unwrap_err().message(), message);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The std self-test runs a filtered selection, one batch per module, and
/// renders the counts per module (D-542).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_self_test_runs_a_selection() {
    let report = test_std(Some("std::bool ")).unwrap();
    assert!(report.success(), "{}", report.render());
    assert_eq!(report.modules.len(), 1, "{}", report.render());
    assert_eq!(report.modules[0].path, "std::bool");
    assert!(report.counts().passed >= 1);
    assert!(report.filtered_out > 300);
    let text = report.render();
    assert!(
        text.contains("\ncounts per module:\n  std::bool: "),
        "{text}"
    );
    assert!(text.ends_with(" filtered out\n"), "{text}");
}

/// `std::bytes` builds and passes now that its kernels are registered
/// (F2), and the report counts it per module (D-542).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_bytes_builds_and_passes() {
    let report = test_std(Some("std::bytes ")).unwrap();
    let text = report.render();
    match report.modules.as_slice() {
        [bytes] => {
            assert_eq!(bytes.path, "std::bytes");
            assert!(bytes.not_built.is_none(), "{text}");
        }
        other => panic!("unexpected modules: {other:?}"),
    }
    assert!(report.success(), "{text}");
    assert!(report.counts().passed >= 19, "{text}");
    assert!(text.contains("  std::bytes: "), "{text}");
}

// ---------------------------------------------------------------------
// Command-line flags (plan §8.4)

/// A package whose `main` has two unused bindings (W0002).
fn warning_package(name: &str) -> PathBuf {
    package(
        name,
        &[
            MANIFEST,
            (
                "src/main.cheby",
                b"fn main() {\n  let unused = 1\n  let other = 2\n}\n",
            ),
        ],
    )
}

/// D-549: `--deny-warnings` renders each warning as an error with its own
/// code, the first with a note, and the output then has errors, so `run`
/// and `test` compile and run nothing.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn denied_warnings_are_errors_with_their_own_codes() {
    let dir = warning_package("deny_warnings");
    let allowed = check(&dir).unwrap();
    assert!(!allowed.has_errors());
    let text = allowed.render();
    assert!(
        text.starts_with("warning[W0002]: unused binding `unused`"),
        "{text}"
    );
    assert!(!text.contains(DENIED_NOTE), "{text}");

    let options = CheckOptions {
        deny_warnings: true,
        ..CheckOptions::default()
    };
    let denied = check_with(&dir, &options).unwrap();
    assert!(denied.has_errors());
    let text = denied.render();
    assert!(
        text.starts_with("error[W0002]: unused binding `unused`"),
        "{text}"
    );
    assert!(
        text.contains("error[W0002]: unused binding `other`"),
        "{text}"
    );
    assert!(!text.contains("warning["), "{text}");
    assert_eq!(text.matches(DENIED_NOTE).count(), 1, "{text}");
    let first = text.split("\n\n").next().unwrap();
    assert!(
        first.contains(DENIED_NOTE),
        "the note is on the first: {text}"
    );
    // In color, the header and the labels take the error's style.
    let colored = denied.render_with(Colors::On);
    assert!(colored.starts_with("\x1b[1;91merror[W0002]"), "{colored:?}");
    assert!(!colored.contains("\x1b[1;93m"), "{colored:?}");

    let run = RunOptions {
        deny_warnings: true,
        ..RunOptions::default()
    };
    let Prepared::CompileFailed(out) = prepare_run(&dir, run).unwrap() else {
        panic!("a denied warning stops `cheby run`");
    };
    assert_eq!(out.render(), text);
    let tests = TestOptions {
        deny_warnings: true,
        ..TestOptions::default()
    };
    let PreparedTests::CompileFailed(out) = prepare_test(&dir, &tests).unwrap() else {
        panic!("a denied warning stops `cheby test`");
    };
    assert_eq!(out.render(), text);
    // Without warnings the flag changes nothing.
    let Prepared::Ready(_) = prepare_run(
        &example("001_hello_world"),
        RunOptions {
            deny_warnings: true,
            ..RunOptions::default()
        },
    )
    .unwrap() else {
        panic!("001 has no warnings");
    };
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn rendered_warnings_become_errors() {
    assert_eq!(as_error("warning[W0002]: x\n"), "error[W0002]: x\n");
    assert_eq!(
        as_error("\x1b[1;93mwarning[W0002]\x1b[0m: x\n  \x1b[1;93m^^\x1b[0m\n"),
        "\x1b[1;91merror[W0002]\x1b[0m: x\n  \x1b[1;91m^^\x1b[0m\n"
    );
}

/// A module whose `main` calls a function that exists only on JS.
const JS_ONLY_MAIN: &[u8] = b"import std::io\n\n\
@external(js, \"./clock.mjs\", \"now\")\nfn js_now() -> Int\n\n\
fn main() {\n  let _ = js_now()\n  io::println(\"done\")\n}\n";

/// D-550: `cheby check --target` checks the code as it runs on that
/// target only, and a target the manifest does not declare is a tool
/// error.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn check_target_narrows_the_cross_target_check() {
    let dir = package(
        "check_target",
        &[MANIFEST, ("src/main.cheby", JS_ONLY_MAIN)],
    );
    let codes = |target| {
        let options = CheckOptions {
            target,
            deny_warnings: false,
        };
        let out = check_with(&dir, &options).unwrap();
        out.diagnostics
            .iter()
            .map(|d| d.code.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(codes(None), ["E0067"]);
    assert_eq!(codes(Some(Target::Native)), ["E0067"]);
    assert_eq!(codes(Some(Target::Js)), Vec::<String>::new());
    std::fs::remove_dir_all(&dir).unwrap();

    let dir = package(
        "check_target_native",
        &[
            (
                "cheby.toml",
                b"name = \"app\"\nversion = \"0.1.0\"\ntargets = [\"native\"]\n",
            ),
            ("src/main.cheby", b"fn main() {}\n"),
        ],
    );
    let options = CheckOptions {
        target: Some(Target::Js),
        deny_warnings: false,
    };
    assert_eq!(
        check_with(&dir, &options).unwrap_err().message(),
        "the package does not target `js`"
    );
    let options = CheckOptions {
        target: Some(Target::Native),
        deny_warnings: false,
    };
    assert_eq!(check_with(&dir, &options).unwrap().render(), "");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Everything a build prints or dumps for `dir`: diagnostics, the MIR
/// and the CLIF of `main`.
fn build_outputs(dir: &Path) -> (String, String, String) {
    let checked = check(dir).unwrap().render();
    let (_, mir) = dump_mir(dir).unwrap();
    let (_, clif) = dump_clif(dir).unwrap();
    (checked, mir, clif)
}

/// D-551, D-264: the number of worker threads never changes the output.
/// Each build runs inside a pool of its own size (`install`), which the
/// build graph uses as it would use the global pool `configure_jobs`
/// builds.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn the_number_of_jobs_does_not_change_the_output() {
    let warnings = warning_package("jobs");
    let dirs = [
        example("014_word_count"),
        example("021_base64"),
        example("024_two_sum"),
        warnings.clone(),
    ];
    let with_threads = |threads: usize| {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .stack_size(WORKER_STACK_SIZE)
            .build()
            .unwrap();
        pool.install(|| {
            dirs.iter()
                .map(|dir| build_outputs(dir))
                .collect::<Vec<_>>()
        })
    };
    let one = with_threads(1);
    let four = with_threads(4);
    assert_eq!(one, four);
    assert!(
        one.iter()
            .take(3)
            .all(|(_, mir, clif)| !mir.is_empty() && !clif.is_empty()),
        "the examples compile"
    );
    std::fs::remove_dir_all(&warnings).unwrap();
}

/// D-551: `configure_jobs` builds the global pool once; asking again for
/// the same size is fine, another size is a tool error.
#[test]
fn configure_jobs_builds_the_global_pool_once() {
    configure_jobs(None).unwrap();
    configure_jobs(None).unwrap();
    let threads = rayon::current_num_threads();
    assert_eq!(threads, available_parallelism());
    let same = NonZeroUsize::new(threads).unwrap();
    configure_jobs(Some(same)).unwrap();
    let other = NonZeroUsize::new(threads + 1).unwrap();
    let error = configure_jobs(Some(other)).unwrap_err();
    assert!(error.message().starts_with("cannot start"), "{error}");
}

/// Round-trips every interface of `front` (D-954) and checks that cut and
/// mutated copies of each real encoding decode or fail, never panic
/// (D-953, D-422). Returns the paths of the modules round-tripped.
fn roundtrip_all(front: &frontend::Frontend) -> Vec<String> {
    let mut done = Vec::new();
    for &id in &front.order {
        let checked = front.checked(id).unwrap();
        let path = front.modules.get(id).unwrap().path.to_string();
        let deps = cheby_iface::DepView::new(&front.set, &checked.deps);
        cheby_iface::check_roundtrip(&checked.interface, &deps, &front.modules, &front.globals)
            .unwrap_or_else(|ice| panic!("{path}: {}", ice.message()));
        let bytes =
            cheby_iface::encode(&checked.interface, &deps, &front.modules, &front.globals).unwrap();
        let decode = |b: &[u8]| {
            cheby_iface::decode_interface(
                id,
                checked.interface.package,
                b,
                &deps,
                &front.modules,
                &front.globals,
            )
        };
        assert!(decode(&bytes).is_ok(), "{path}");
        let step = (bytes.len() / 64).max(1);
        for len in (0..bytes.len()).step_by(step) {
            assert!(decode(&bytes[..len]).is_err(), "{path} cut to {len}");
        }
        for at in (0..bytes.len()).step_by(step) {
            for value in [0x00, 0x03, 0xFF] {
                let mut mutated = bytes.clone();
                mutated[at] = value;
                let _ = decode(&mutated);
            }
        }
        done.push(path);
    }
    done
}

/// D-954: the interface of every module of every example and of every
/// std module decodes to itself, spans aside. The front end already does
/// this in debug builds; this test does it explicitly, so a release test
/// run checks it too.
#[test]
fn iface_roundtrip_every_example_and_std() {
    let mut example_modules = 0_usize;
    for dir in examples() {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        if name == "030_shop_service" || LATER_EXAMPLES.contains(&name.as_str()) {
            continue;
        }
        let mut out = CheckOutput::default();
        let (front, _) = front_end(&dir, frontend::Mode::Build, &mut out)
            .unwrap()
            .unwrap();
        assert!(!out.has_errors(), "{name}: {}", out.render());
        example_modules += roundtrip_all(&front)
            .iter()
            .filter(|path| !path.starts_with("std::"))
            .count();
    }
    let mut sources = SourceMap::new();
    let mut diagnostics = Vec::new();
    let package = test::std_test_package(&mut sources, &mut diagnostics).unwrap();
    let front = frontend::run(
        package,
        frontend::Mode::StdTests,
        &mut sources,
        &mut diagnostics,
    )
    .unwrap();
    assert!(
        !diagnostics.iter().any(Diagnostic::is_error),
        "std does not check"
    );
    let std_modules = roundtrip_all(&front)
        .into_iter()
        .filter(|p| p.starts_with("std::"))
        .count();
    assert_eq!(std_modules, std_lib::STD_MODULES.len());
    println!(
        "iface round trip: {example_modules} example modules in {} example builds, {std_modules} std modules",
        examples().len() - 1 - LATER_EXAMPLES.len()
    );
    assert!(example_modules > 30);
}

/// D-954 and D-422: a public signature whose type nests deeper than any
/// fixed bound checks, and its interface round-trips, both through a
/// chain of 200 aliases and through 255 levels written out (the parser's
/// bound is 256, D-441). Review R1: a decoder depth limit of 128 made
/// both an internal compiler error in debug builds.
#[test]
fn iface_roundtrip_types_of_any_depth() {
    use std::fmt::Write as _;
    let mut src = String::from("//! Deep types.\n\nimport std::io\n\ntype A0 = Int\n");
    for i in 1..=200 {
        writeln!(src, "type A{i} = Option<A{}>", i - 1).unwrap();
    }
    let nested = format!("{}Int{}", "Option<".repeat(254), ">".repeat(254));
    write!(
        src,
        "\npub fn wrap(x: A200) -> A200 {{\n  x\n}}\n\n\
         pub fn wrap_written(x: {nested}) -> {nested} {{\n  x\n}}\n\n\
         fn main() {{\n  io::println(\"ok\")\n}}\n"
    )
    .unwrap();
    let dir = package(
        "deep_types",
        &[
            ("cheby.toml", b"name = \"deep\"\nversion = \"0.1.0\"\n"),
            ("src/main.cheby", src.as_bytes()),
        ],
    );
    let mut out = CheckOutput::default();
    let (front, _) = front_end(&dir, frontend::Mode::Build, &mut out)
        .unwrap()
        .unwrap();
    assert!(!out.has_errors(), "{}", out.render());
    assert!(roundtrip_all(&front).iter().any(|path| path == "deep"));
    std::fs::remove_dir_all(&dir).unwrap();
}
