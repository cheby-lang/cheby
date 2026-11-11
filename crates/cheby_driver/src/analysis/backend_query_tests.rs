//! Tests of `backend_query`, the child's constants job (D-1410, D-1412,
//! D-1413). Every test that runs the front end evaluates constants or
//! could, so it carries the Windows ignore (D-545, plan §0.1).

use std::path::{Path, PathBuf};

use cheby_diag::TextRange;
use cheby_package::ModulePath;

use super::*;
use crate::analysis::{exact_key, normalize};

/// A package directory in the temporary directory, removed on drop.
struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Temp {
    /// A fresh package `app` with `src/main.cheby` holding `main`.
    fn package(name: &str, main: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("cheby_backend_query_{}_{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/main.cheby"), main).unwrap();
        Self(dir)
    }
}

fn request(root: &Path) -> BackendRequest {
    BackendRequest {
        root: root.to_path_buf(),
        target: None,
        job: BackendJob::Constants,
        max_text: 4096,
    }
}

/// The constant `name` of the root module `app`.
fn app(name: &str) -> Symbol {
    Symbol::item(ModulePath::root("app"), SymbolNs::Value, name)
}

/// The constants section, which must have run.
fn section(result: &BackendResult) -> &ConstantsOut {
    match &result.constants {
        Some(Section::Ok(out)) => out,
        other => panic!("no constants section: {other:?}"),
    }
}

/// The range of the first `needle` in `text`.
fn range_of(text: &str, needle: &str) -> TextRange {
    let start = text.find(needle).unwrap();
    TextRange::at(
        u32::try_from(start).unwrap(),
        u32::try_from(needle.len()).unwrap(),
    )
}

const PANICS: &str = "//! A constant that panics.\n\n\
                      const checked: Int = must_be_positive(-3)\n\n\
                      fn must_be_positive(n: Int) -> Int {\n  \
                      assert n > 0 as \"a size must be positive\"\n  n\n}\n\n\
                      fn main() {\n  let _ = checked\n}\n";

/// A panicking constant is E0075 at its name, marked with its symbol, and
/// `build` gives no value on `Err` (D-1413).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs generated code, which Windows does not until M6 (D-545)"
)]
fn panicking_constant_gives_e0075_on_its_symbol() {
    let dir = Temp::package("panics", PANICS);
    let result = backend_query(&request(&dir.0)).unwrap();
    assert_eq!(result.front, FrontOutcome::Clean);
    let out = section(&result);
    assert_eq!(out.values, []);
    let [diag] = out.diagnostics.as_slice() else {
        panic!("{:?}", out.diagnostics);
    };
    assert_eq!(diag.diagnostic.code.to_string(), "E0075");
    assert_eq!(diag.path, Path::new("src/main.cheby"));
    assert_eq!(diag.symbol, Some(app("checked")));
    assert_eq!(diag.diagnostic.range, Some(range_of(PANICS, "checked")));
    // As `cheby check` reports it.
    let checked = crate::check(&dir.0).unwrap();
    let codes: Vec<String> = checked
        .diagnostics
        .iter()
        .map(|d| d.code.to_string())
        .collect();
    assert_eq!(codes, ["E0075"]);
    assert_eq!(
        result.files,
        [BackendFile {
            path: PathBuf::from("src/main.cheby"),
            hash: exact_key(PANICS),
        }]
    );
    assert_eq!(
        result.manifest,
        exact_key("name = \"app\"\nversion = \"0.1.0\"\n")
    );
}

const GOOD: &str = "//! Constants.\n\n\
                    const limit: Int = 20\n\n\
                    const name: String = \"cheby\"\n\n\
                    const doubled: Int = limit * 2\n\n\
                    fn main() {\n  let _ = limit + doubled\n  let _ = name\n}\n";

/// A clean package gives every local constant's value, by symbol, as
/// `debug_texts` prints it.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs generated code, which Windows does not until M6 (D-545)"
)]
fn values_come_by_symbol() {
    let dir = Temp::package("values", GOOD);
    let result = backend_query(&request(&dir.0)).unwrap();
    let out = section(&result);
    assert_eq!(out.diagnostics, []);
    let values: Vec<(Symbol, &str, bool)> = out
        .values
        .iter()
        .map(|v| (v.symbol.clone(), v.text.as_str(), v.truncated))
        .collect();
    assert_eq!(
        values,
        [
            (app("doubled"), "40", false),
            (app("limit"), "20", false),
            (app("name"), "\"cheby\"", false),
        ]
    );
}

/// A value longer than the cap is cut at that many characters (D-1413).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs generated code, which Windows does not until M6 (D-545)"
)]
fn long_values_are_cut() {
    let dir = Temp::package("cut", GOOD);
    let mut request = request(&dir.0);
    request.max_text = 3;
    let result = backend_query(&request).unwrap();
    let name = section(&result)
        .values
        .iter()
        .find(|v| v.symbol == app("name"))
        .unwrap();
    assert_eq!((name.text.as_str(), name.truncated), ("\"ch", true));
}

const ONE_FAILS: &str = "//! One failing constant among good ones.\n\n\
                         const limit: Int = 20\n\n\
                         const broken: Int = must_be_positive(-3)\n\n\
                         const label: String = \"ok\"\n\n\
                         fn must_be_positive(n: Int) -> Int {\n  \
                         assert n > 0 as \"a size must be positive\"\n  n\n}\n\n\
                         fn main() {\n  let _ = limit + broken\n  let _ = label\n}\n";

/// One failing constant marks only itself, so the server keeps the other
/// constants' values from the last good run, marked stale (D-1413, S-20):
/// the run gives no values, since `build` gives none on `Err`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs generated code, which Windows does not until M6 (D-545)"
)]
fn one_failing_constant_marks_only_itself() {
    let dir = Temp::package("one_fails", ONE_FAILS);
    let result = backend_query(&request(&dir.0)).unwrap();
    let out = section(&result);
    let marked: Vec<Option<Symbol>> = out.diagnostics.iter().map(|d| d.symbol.clone()).collect();
    assert_eq!(marked, [Some(app("broken"))]);
    assert_eq!(out.values, []);
    // The good constants' values, from a run without the failing one.
    let fixed = ONE_FAILS.replace("must_be_positive(-3)", "must_be_positive(3)");
    std::fs::write(dir.0.join("src/main.cheby"), fixed).unwrap();
    let result = backend_query(&request(&dir.0)).unwrap();
    let symbols: Vec<Symbol> = section(&result)
        .values
        .iter()
        .map(|v| v.symbol.clone())
        .collect();
    assert_eq!(symbols, [app("broken"), app("label"), app("limit")]);
}

/// Ranges and hashes are over normalized text: a BOM and `\r\n` do not
/// move them (D-1412).
fn normalized(name: &str, text: &str) {
    let dir = Temp::package(name, text);
    let result = backend_query(&request(&dir.0)).unwrap();
    let normal = normalize(text);
    assert_eq!(normal, PANICS);
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files.first().unwrap().hash, exact_key(PANICS));
    let out = section(&result);
    let [diag] = out.diagnostics.as_slice() else {
        panic!("{:?}", out.diagnostics);
    };
    assert_eq!(diag.symbol, Some(app("checked")));
    let range = diag.diagnostic.range.unwrap();
    assert_eq!(range, range_of(PANICS, "checked"));
    assert_eq!(range.slice(&normal), Some("checked"));
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs generated code, which Windows does not until M6 (D-545)"
)]
fn bom_file_ranges_are_in_normalized_text() {
    normalized("bom", &format!("\u{feff}{PANICS}"));
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs generated code, which Windows does not until M6 (D-545)"
)]
fn crlf_file_ranges_are_in_normalized_text() {
    normalized("crlf", &format!("\u{feff}{}", PANICS.replace('\n', "\r\n")));
    normalized("crlf_only", &PANICS.replace('\n', "\r\n"));
}

/// A front end with an error evaluates nothing (D-459, D-534).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs generated code, which Windows does not until M6 (D-545)"
)]
fn front_end_errors_skip_evaluation() {
    let typed = "//! A type error.\n\nconst x: Int = \"no\"\n\nfn main() {\n  let _ = x\n}\n";
    let dir = Temp::package("typed", typed);
    let result = backend_query(&request(&dir.0)).unwrap();
    assert_eq!(result.front, FrontOutcome::Errors);
    assert_eq!(
        result.constants,
        Some(Section::Skipped(FRONT_ERRORS.to_owned()))
    );
    assert_eq!(result.files.len(), 1);
}

const OUT_OF_JS_RANGE: &str = "//! A constant past the JS range.\n\n\
                               const big: Int = 9_007_199_254_740_991 * 2\n\n\
                               fn main() {\n  assert big > 0\n}\n";

/// The request's target narrows the package's targets as `cheby check
/// --target` does (D-550): in a package that targets `native` and `js`,
/// an `Int` outside the JS range is E0078 with no target and nothing with
/// `native` (D-540, D-1489).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs generated code, which Windows does not until M6 (D-545)"
)]
fn target_narrows_the_js_range_check() {
    let dir = Temp::package("target", OUT_OF_JS_RANGE);
    std::fs::write(
        dir.0.join("cheby.toml"),
        "name = \"app\"\nversion = \"0.1.0\"\ntargets = [\"native\", \"js\"]\n",
    )
    .unwrap();
    let both = backend_query(&request(&dir.0)).unwrap();
    let codes: Vec<String> = section(&both)
        .diagnostics
        .iter()
        .map(|d| d.diagnostic.code.to_string())
        .collect();
    assert_eq!(codes, ["E0078"]);
    assert_eq!(section(&both).values, []);
    let native = BackendRequest {
        target: Some(Target::Native),
        ..request(&dir.0)
    };
    let native = backend_query(&native).unwrap();
    assert_eq!(section(&native).diagnostics, []);
    let values: Vec<(Symbol, &str)> = section(&native)
        .values
        .iter()
        .map(|v| (v.symbol.clone(), v.text.as_str()))
        .collect();
    assert_eq!(values, [(app("big"), "18014398509481982")]);
    // As `cheby check` and `cheby check --target native` report it.
    let codes = |target| {
        let options = crate::CheckOptions {
            target,
            deny_warnings: false,
        };
        crate::check_with(&dir.0, &options)
            .unwrap()
            .diagnostics
            .iter()
            .map(|d| d.code.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(codes(None), ["E0078"]);
    assert_eq!(codes(Some(Target::Native)), Vec::<String>::new());
}

/// A root without `cheby.toml` is `no-package`.
#[test]
fn no_manifest_is_no_package() {
    let dir = std::env::temp_dir().join(format!("cheby_backend_query_{}_none", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let result = backend_query(&request(&dir));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(result, Err(BackendError::NoPackage));
}

#[test]
fn cut_counts_characters() {
    assert_eq!(cut_text("héllo".to_owned(), 2), ("hé".to_owned(), true));
    assert_eq!(cut_text("héllo".to_owned(), 5), ("héllo".to_owned(), false));
    assert_eq!(cut_text(String::new(), 0), (String::new(), false));
}
