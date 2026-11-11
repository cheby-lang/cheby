//! Tests of the navigation handlers on one package, in the three states
//! of the plan (W7): clean text, a type error and a grammar error. Each
//! answer is written as `file line:col-line:col`, lines and columns from 0
//! as the protocol counts them, files relative to the package's `src/`
//! or to the std directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_driver::analysis::{Globals, Overlay, OverlayFile, PackageKind, Session, SessionConfig};
use serde_json::{Value, json};

use super::{
    LinkCaps, NavEnv, declaration, definition, document_highlight, references, type_definition,
};
use crate::features::{Analyzed, CancelToken, HandlerOutput, RequestCx, RequestError};
use crate::server::{ClientCaps, Settings};
use crate::text::lines::PositionEncoding;
use crate::text::uri::{normalize, path_to_uri};

/// The std directory the tests name. Nothing is read from it.
const STD_DIR: &str = "/nav-tests-std";

/// A package in a fresh temporary directory, removed on drop.
struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Temp {
    /// The package `app` with these files under `src/`.
    fn package(name: &str, files: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!("cheby_lsp_nav_{}_{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        for (path, text) in files {
            std::fs::write(dir.join("src").join(path), text).unwrap();
        }
        Self(normalize(&std::path::absolute(&dir).unwrap()))
    }

    fn src(&self, file: &str) -> PathBuf {
        self.0.join("src").join(file)
    }
}

const SHAPES: &str = "//! Shapes.

/// A point.
pub type Point {
  x: Int,
  y: Int,
}

pub fn add(a: Point, b: Point) -> Point {
  Point { x: a.x + b.x, y: a.y + b.y }
}

pub fn show(p: Point) -> String {
  \"({p.x}, {p.y})\"
}
";

const MAIN: &str = "//! Root.

import app::shapes
import app::shapes::{Point as P}
import std::io

fn main() {
  let p = P { x: 1, y: 2 }
  let q = shapes::add(p, p) + p
  let P { x, y } = q
  io::println(\"{x} {q} {y}\")
}
";

/// The package of [`SHAPES`] and `main`, with `main` open.
struct Case {
    temp: Temp,
    analyzed: Analyzed,
}

impl Case {
    fn new(name: &str, main: &str) -> Self {
        let temp = Temp::package(name, &[("shapes.cheby", SHAPES), ("main.cheby", main)]);
        let mut session = Session::new(SessionConfig {
            root: temp.0.clone(),
            kind: PackageKind::Package,
            target: None,
            globals: Arc::new(Globals::new()),
        });
        let overlay = Overlay {
            files: [(
                temp.src("main.cheby"),
                OverlayFile {
                    text: Arc::from(main),
                    version: 1,
                    stamp: 1,
                },
            )]
            .into_iter()
            .collect(),
        };
        let live = session.analyze_live(&overlay).expect("a run");
        let analyzed = Analyzed::new(live.snapshot.expect("a snapshot"));
        Self { temp, analyzed }
    }

    /// The params of a position request at `line:col` of `file`.
    fn at(&self, file: &str, line: u32, col: u32) -> Value {
        let uri = path_to_uri(&self.temp.src(file)).unwrap();
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": line, "character": col },
            "context": { "includeDeclaration": true },
        })
    }

    /// Runs `handler` with `links` and writes its locations as text.
    fn run(&self, handler: Handler, links: bool, params: Value) -> Vec<String> {
        let output = self.output(handler, links, params);
        self.show(&output.value)
    }

    fn output(&self, handler: Handler, links: bool, params: Value) -> HandlerOutput {
        let settings = Settings::default();
        let caps = ClientCaps::default();
        let cancel = CancelToken::new();
        let cx = RequestCx {
            settings: &settings,
            caps: &caps,
            encoding: PositionEncoding::Utf16,
            cancel: &cancel,
            std: &crate::features::stdlib::StdService::none(),
            constants: None,
        };
        let env = NavEnv {
            links: LinkCaps {
                definition: links,
                declaration: links,
                type_definition: links,
            },
            std_dir: Some(Path::new(STD_DIR)),
        };
        handler(&cx, &self.analyzed, params, &env).expect("an answer")
    }

    /// A location, link or highlight as `file line:col-line:col`, with a
    /// link's origin after `<-` and a highlight's kind after it.
    fn show(&self, value: &Value) -> Vec<String> {
        let items = match value {
            Value::Null => return Vec::new(),
            Value::Array(items) => items.clone(),
            one => vec![one.clone()],
        };
        items
            .iter()
            .map(|item| {
                if let Some(uri) = item.get("targetUri") {
                    format!(
                        "{} {} <- {}",
                        self.file(uri),
                        range(&item["targetSelectionRange"]),
                        range(&item["originSelectionRange"])
                    )
                } else if let Some(uri) = item.get("uri") {
                    format!("{} {}", self.file(uri), range(&item["range"]))
                } else {
                    format!("{} {}", range(&item["range"]), item["kind"])
                }
            })
            .collect()
    }

    /// A URI as a path relative to `src/` or the std directory.
    fn file(&self, uri: &Value) -> String {
        let uri: lsp_types::Uri = uri.as_str().unwrap().parse().unwrap();
        let path = crate::text::uri::uri_to_path(&uri).unwrap();
        let src = self.temp.0.join("src");
        let shown = path
            .strip_prefix(&src)
            .or_else(|_| path.strip_prefix(STD_DIR))
            .unwrap_or(&path);
        shown.to_string_lossy().replace('\\', "/")
    }
}

type Handler =
    fn(&RequestCx<'_>, &Analyzed, Value, &NavEnv<'_>) -> Result<HandlerOutput, RequestError>;

fn range(range: &Value) -> String {
    let p = |at: &Value| format!("{}:{}", at["line"], at["character"]);
    format!("{}-{}", p(&range["start"]), p(&range["end"]))
}

/// The line and column, from 0, of the first `needle` in std's
/// `module`, which is ASCII before it.
fn in_std(module: &str, needle: &str) -> (usize, usize) {
    let source = cheby_driver::std_lib::STD_MODULES
        .iter()
        .find(|m| m.path == module)
        .unwrap()
        .source;
    let (before, _) = source.split_once(needle).unwrap();
    let line = before.matches('\n').count();
    let col = before.rsplit('\n').next().unwrap().len();
    (line, col)
}

#[test]
fn link_caps_follow_each_method() {
    let params = json!({ "capabilities": { "textDocument": {
        "definition": { "linkSupport": true },
        "typeDefinition": { "linkSupport": "yes" },
    } } });
    let caps = LinkCaps::from_params(&params);
    assert_eq!(
        caps,
        LinkCaps {
            definition: true,
            declaration: false,
            type_definition: false,
        }
    );
    assert_eq!(LinkCaps::from_params(&json!({})), LinkCaps::default());
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs the front end, which needs JIT code; Windows is M6 (D-545, D-641)"
)]
fn definition_on_clean_text() {
    let case = Case::new("clean", MAIN);
    let def = |line, col| case.run(definition, false, case.at("main.cheby", line, col));
    // An alias's use leads to its `as` name.
    assert_eq!(def(7, 10), ["main.cheby 3:30-3:31"]);
    // An item in another module.
    assert_eq!(def(8, 19), ["shapes.cheby 8:7-8:10"]);
    // `+` leads to the function it calls (D-1353).
    assert_eq!(def(8, 28), ["shapes.cheby 8:7-8:10"]);
    // A module, at its start.
    assert_eq!(def(8, 12), ["shapes.cheby 0:0-0:0"]);
    // A field of a record expression.
    assert_eq!(def(7, 14), ["shapes.cheby 4:2-4:3"]);
    // A punned field of a pattern is its binding (D-1392).
    assert_eq!(def(9, 10), ["main.cheby 9:10-9:11"]);
    // The name in `{x}`, and the braces of `{q}`, which call `show`.
    assert_eq!(def(10, 16), ["main.cheby 9:10-9:11"]);
    assert_eq!(def(10, 19), ["shapes.cheby 12:7-12:11"]);
    // Into std: the materialized file (D-1369).
    assert_eq!(def(10, 3), ["std/io.cheby 0:0-0:0"]);
    let (line, col) = in_std("io", "println(text");
    assert_eq!(
        def(10, 8),
        [format!(
            "std/io.cheby {line}:{col}-{line}:{}",
            col.saturating_add(7)
        )]
    );
    // An import's module path segment, and nothing on a keyword.
    assert_eq!(def(2, 13), ["shapes.cheby 0:0-0:0"]);
    assert_eq!(def(7, 3), Vec::<String>::new());
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs the front end, which needs JIT code; Windows is M6 (D-545, D-641)"
)]
fn links_carry_the_origin_and_the_declaration() {
    let case = Case::new("links", MAIN);
    let params = case.at("main.cheby", 8, 19);
    let output = case.output(definition, true, params);
    assert_eq!(
        case.show(&output.value),
        ["shapes.cheby 8:7-8:10 <- 8:18-8:21"]
    );
    // The target range is the whole function.
    assert_eq!(range(&output.value[0]["targetRange"]), "8:0-10:1");
    let braces = case.run(definition, true, case.at("main.cheby", 10, 19));
    assert_eq!(braces, ["shapes.cheby 12:7-12:11 <- 10:19-10:22"]);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs the front end, which needs JIT code; Windows is M6 (D-545, D-641)"
)]
fn declaration_and_type_definition() {
    let case = Case::new("decl", MAIN);
    // `add` satisfies `Add`, so its declaration is `Add`'s function.
    let (line, col) = in_std("ops", "add(Self");
    assert_eq!(
        case.run(declaration, false, case.at("shapes.cheby", 8, 8)),
        [format!(
            "std/ops.cheby {line}:{col}-{line}:{}",
            col.saturating_add(3)
        )]
    );
    // Any other name: the definition.
    assert_eq!(
        case.run(declaration, false, case.at("main.cheby", 8, 6)),
        ["main.cheby 8:6-8:7"]
    );
    // The type of a local, of a field, and of a function's signature.
    let ty = |line, col| case.run(type_definition, false, case.at("main.cheby", line, col));
    assert_eq!(ty(8, 6), ["shapes.cheby 3:9-3:14"]);
    assert_eq!(ty(9, 10), ["std/int.cheby 0:0-0:0"]);
    assert_eq!(ty(8, 19), ["shapes.cheby 3:9-3:14"]);
    // A literal, through its expression's type.
    assert_eq!(ty(7, 17), ["std/int.cheby 0:0-0:0"]);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs the front end, which needs JIT code; Windows is M6 (D-545, D-641)"
)]
fn references_and_highlights() {
    let case = Case::new("refs", MAIN);
    let mut at = case.at("main.cheby", 7, 6);
    assert_eq!(
        case.run(references, false, at.clone()),
        [
            "main.cheby 7:6-7:7",
            "main.cheby 8:22-8:23",
            "main.cheby 8:25-8:26",
            "main.cheby 8:30-8:31",
        ]
    );
    at["context"]["includeDeclaration"] = json!(false);
    assert_eq!(case.run(references, false, at).len(), 3);
    // A function: its uses across modules and the operator that calls it.
    let mut add = case.at("shapes.cheby", 8, 8);
    add["context"]["includeDeclaration"] = json!(false);
    assert_eq!(
        case.run(references, false, add),
        ["main.cheby 8:18-8:21", "main.cheby 8:28-8:29"]
    );
    // A clean package: nothing logged.
    let output = case.output(references, false, case.at("main.cheby", 7, 6));
    assert!(output.effects.is_empty());
    // A local: written at its binding, read in a path and in `{q}`.
    assert_eq!(
        case.run(document_highlight, false, case.at("main.cheby", 8, 6)),
        ["8:6-8:7 3", "9:19-9:20 2", "10:20-10:21 2"]
    );
    // The braces of `{q}` call `show`: an implicit occurrence is text.
    assert_eq!(
        case.run(document_highlight, false, case.at("main.cheby", 10, 19)),
        ["10:19-10:22 1"]
    );
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs the front end, which needs JIT code; Windows is M6 (D-545, D-641)"
)]
fn navigation_on_a_type_error() {
    let main = "//! Root.

import app::shapes

fn main() {
  let p = shapes::Point { x: 1, y: 2 }
  let n: Int = p
  let m = undefined + n
  assert shapes::add(p, p).x == n
}
";
    let case = Case::new("type_error", main);
    let def = |line, col| case.run(definition, false, case.at("main.cheby", line, col));
    // Names around the errors still resolve.
    assert_eq!(def(6, 15), ["main.cheby 5:6-5:7"]);
    assert_eq!(def(8, 18), ["shapes.cheby 8:7-8:10"]);
    assert_eq!(def(8, 28), ["shapes.cheby 4:2-4:3"]);
    // An unresolved name has no definition.
    assert_eq!(def(7, 10), Vec::<String>::new());
    assert_eq!(
        case.run(references, false, case.at("main.cheby", 6, 6)),
        [
            "main.cheby 6:6-6:7",
            "main.cheby 7:22-7:23",
            "main.cheby 8:32-8:33"
        ]
    );
    // No unknown name has the text `n`, so the list is whole (F §5.5).
    let output = case.output(references, false, case.at("main.cheby", 6, 6));
    assert!(output.effects.is_empty(), "{:?}", output.effects);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs the front end, which needs JIT code; Windows is M6 (D-545, D-641)"
)]
fn navigation_on_a_grammar_error() {
    let main = "//! Root.

import app::shapes

fn main() {
  let p = shapes::Point { x: 1, y: 2 }
  let q = shapes::add(p, p)
  let r = q q
}
";
    let case = Case::new("grammar_error", main);
    let def = |line, col| case.run(definition, false, case.at("main.cheby", line, col));
    assert_eq!(def(6, 22), ["main.cheby 5:6-5:7"]);
    assert_eq!(def(6, 18), ["shapes.cheby 8:7-8:10"]);
    assert_eq!(
        case.run(document_highlight, false, case.at("main.cheby", 5, 6)),
        ["5:6-5:7 3", "6:22-6:23 2", "6:25-6:26 2"]
    );
    let output = case.output(references, false, case.at("shapes.cheby", 8, 8));
    assert_eq!(
        case.show(&output.value),
        ["main.cheby 6:18-6:21", "shapes.cheby 8:7-8:10"]
    );
    // The second `q` is in an `ERROR` node and cannot be placed, so
    // references of `q` answer what they have and log that they may be
    // partial (F §5.5).
    let output = case.output(references, false, case.at("main.cheby", 6, 6));
    assert_eq!(
        case.show(&output.value),
        ["main.cheby 6:6-6:7", "main.cheby 7:10-7:11"]
    );
    assert_eq!(output.effects.len(), 1, "{:?}", output.effects);
}
