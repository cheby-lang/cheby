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
fn examples_check_cleanly() {
    for dir in examples() {
        let out = check(&dir).unwrap();
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
