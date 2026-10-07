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
/// which are rejected until M5, and a D-243 warning (W0001), which is an
/// open issue in the example.
#[test]
fn examples_pass_the_syntax_layer() {
    for dir in examples() {
        let out = check_syntax(&dir).unwrap();
        let rendered = out.render();
        if dir.ends_with("030_shop_service") {
            assert_eq!(out.diagnostics.len(), 2, "{rendered}");
            assert!(rendered.starts_with("error[E0036]: dependencies are not supported"));
            assert!(rendered.contains("warning[W0001]"), "{rendered}");
        } else {
            assert_eq!(rendered, "", "{}", dir.display());
        }
    }
}

/// With every implemented phase, a valid example reports only constructs
/// the compiler does not support yet (E0038) and the known W0001, so the
/// support gate never hides a real error (D-451). 001 is clean, and
/// 030 still stops after phase A.
#[test]
fn examples_report_only_unsupported_constructs() {
    for dir in examples() {
        let out = check(&dir).unwrap();
        let rendered = out.render();
        let codes: Vec<String> = out.diagnostics.iter().map(|d| d.code.to_string()).collect();
        if dir.ends_with("001_hello_world") {
            assert_eq!(rendered, "", "{rendered}");
        } else if dir.ends_with("030_shop_service") {
            assert_eq!(codes, ["E0036", "W0001"], "{rendered}");
        } else {
            assert!(
                codes.iter().all(|c| c == "E0038" || c == "W0001"),
                "{}:\n{rendered}",
                dir.display()
            );
        }
    }
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
        let out = check(&dir).unwrap();
        assert_eq!(out.render(), "", "std::{}", module.path);
        std::fs::remove_dir_all(&dir).unwrap();
    }
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
                b"//! Imports.\n\nimport std::list\nimport std::ioo\n\nfn main() {\n  Nil\n}\n",
            ),
        ],
    );
    let rendered = check(&dir).unwrap().render();
    assert!(
        rendered.contains("error[E0038]: the module `std::list` is not supported yet"),
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
    let (out, mir) = dump_mir(&test_package("lets")).unwrap();
    assert_eq!(out.render(), "");
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
    assert!(rendered.contains("--> src/ok.cheby:2:7"), "{rendered}");
    assert!(
        rendered.contains("error[E0013]: expected an expression"),
        "{rendered}"
    );
    // E0006 and E0011 are token-level, so they are reported.
    assert!(rendered.contains("--> src/main.cheby:2:7"), "{rendered}");
    assert!(rendered.contains("error[E0011]"), "{rendered}");
    // E0030 is a shape check, so it is not.
    assert!(!rendered.contains("E0030"), "{rendered}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Reserved words that cause a grammar error get their own help (E0007).
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
    assert!(
        rendered.contains("error[E0007]: `if` is a reserved word"),
        "{rendered}"
    );
    assert!(rendered.contains("use `case`"), "{rendered}");
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
