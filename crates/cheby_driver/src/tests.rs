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

/// The examples that need tier-2 or tier-3 std modules move to the
/// milestone that ships those modules: their imports of them are E0038
/// (D-451).
const LATER_EXAMPLES: [&str; 8] = [
    "005_prime_sieve",
    "009_counter_actor",
    "013_dining_philosophers",
    "018_worker_pool",
    "019_supervisor",
    "022_bank_accounts",
    "028_money_and_rationals",
    "031_web_scraper",
];

/// Every example of the M1 exit criterion, and 020, checks with no
/// diagnostics at all, warnings included. The cross-target check covers
/// every target the manifest declares, so this is the check for both
/// targets (plan §1 item 4).
#[test]
fn examples_check_cleanly() {
    let mut checked = 0;
    for dir in examples() {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        if name == "030_shop_service" || LATER_EXAMPLES.contains(&name.as_str()) {
            continue;
        }
        let out = check(&dir).unwrap();
        assert_eq!(out.render(), "", "{name}");
        checked += 1;
    }
    assert_eq!(checked, 23);
}

/// An example that needs a std module of a later tier reports E0038 at
/// each import of one, and nothing else: paths through such an import are
/// silent, and its other code checks. 022 keeps its deliberate `todo`
/// (W0009, D-068). 030 still stops after phase A (D-446).
#[test]
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
            if code == "W0009" && name == "022_bank_accounts" {
                continue;
            }
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
        let todos = out
            .diagnostics
            .iter()
            .filter(|d| d.code.to_string() == "W0009")
            .count();
        assert_eq!(todos, usize::from(name == "022_bank_accounts"), "{name}");
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
/// an internal compiler error (D-464, plan §8). Nothing runs: 001 runs in
/// `hello_world_runs_without_leaks`, and the others may read input.
#[test]
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

/// A package whose `main` calls a function with a construct the back end
/// cannot lower, and a function with one that nothing calls.
fn unsupported_package(name: &str, main_calls_it: bool) -> PathBuf {
    let call = if main_calls_it {
        "  let _ = pick(\"a\")\n"
    } else {
        ""
    };
    let source = format!(
        "//! Back-end gate.\n\nimport std::io\n\nfn main() {{\n  io::println(\"start\")\n{call}}}\n\nfn pick(text: String) -> String {{\n  case text {{\n    \"a\" => \"first\"\n    _ => text\n  }}\n}}\n"
    );
    package(name, &[MANIFEST, ("src/main.cheby", source.as_bytes())])
}

/// A reachable `case` stops `cheby run` with E0038 at the `case`, before
/// any code is generated, whichever phase reports it (the front end did
/// until C5). `cheby check` reports nothing the back end would.
#[test]
fn reachable_unsupported_constructs_stop_the_run() {
    let dir = unsupported_package("gate_reachable", true);
    let RunResult::CompileFailed(out) = run(&dir, RunOptions::default()).unwrap() else {
        panic!("a program with a reachable `case` ran");
    };
    let rendered = out.render();
    assert!(
        rendered.starts_with(
            "error[E0038]: `case` expressions are not supported yet\n  --> src/main.cheby:11:3"
        ),
        "{rendered}"
    );
    let codes: Vec<String> = out.diagnostics.iter().map(|d| d.code.to_string()).collect();
    assert!(codes.iter().all(|c| c == "E0038"), "{rendered}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The same `case` in a function `main` never calls does not stop the run,
/// once the front end checks `case` (C5); until then `cheby check` reports
/// it and nothing more can be asserted.
#[test]
fn unreachable_unsupported_constructs_do_not_stop_the_run() {
    let dir = unsupported_package("gate_unreachable", false);
    let checked = check(&dir).unwrap();
    if checked.has_errors() {
        let codes: Vec<String> = checked
            .diagnostics
            .iter()
            .map(|d| d.code.to_string())
            .collect();
        assert!(codes.iter().all(|c| c == "E0038"), "{}", checked.render());
    } else {
        let Prepared::Ready(program) = prepare_run(&dir, RunOptions::default()).unwrap() else {
            panic!("an unreachable `case` stopped the run");
        };
        assert_eq!(program.output().render(), "");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The dumps show what running the root `main` compiles: the items it
/// reaches. `cheby check` lowers nothing, so it reports no back-end E0038.
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
                b"//! Imports.\n\nimport std::fiber\nimport std::ioo\n\nfn main() {\n  Nil\n}\n",
            ),
        ],
    );
    let rendered = check(&dir).unwrap().render();
    assert!(
        rendered.contains("error[E0038]: the module `std::fiber` is not supported yet"),
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

/// Runs 001 in this process: status 0 and, in debug builds, no object
/// left live (the leak check would give 102).
#[test]
fn hello_world_runs_without_leaks() {
    let result = run(&example("001_hello_world"), RunOptions::default()).unwrap();
    let RunResult::Ran { output, exit } = result else {
        panic!("001 failed to compile");
    };
    assert_eq!(output.render(), "");
    assert_eq!(exit, 0);
    assert_eq!(cheby_runtime::live_objects(), 0);
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
        args: Vec::new(),
    };
    assert_eq!(
        run(&dir, missing).unwrap_err().message(),
        "there is no module `app::nope` in this package"
    );
    let no_main = RunOptions {
        module_path: Some("app::lib".to_owned()),
        args: Vec::new(),
    };
    assert_eq!(
        run(&dir, no_main).unwrap_err().message(),
        "module `app::lib` has no `main` function"
    );
    let bad = RunOptions {
        module_path: Some("app::".to_owned()),
        args: Vec::new(),
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
        render_all(&diags, &sources),
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
