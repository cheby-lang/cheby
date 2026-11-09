//! Tests of semantic tokens (X §5.5, D-1396): each kind of name on clean
//! text, a type error and a grammar error, `range`, the setting, position
//! encodings, and every example.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_driver::analysis::{
    Globals, Overlay, OverlayFile, PackageKind, Session, SessionConfig, TextRange,
};
use cheby_syntax::SyntaxKind;
use lsp_types::Position;
use serde_json::{Value, json};

use super::{TOKEN_MODIFIERS, TOKEN_TYPES, full, legend, provider, range};
use crate::features::ide::tokens::{Modifiers, TokenType};
use crate::features::{Analyzed, CancelToken, RequestCx, RequestError};
use crate::server::{ClientCaps, Settings};
use crate::text::lines::{Bom, LspLines, PositionEncoding};
use crate::text::uri::path_to_uri;

/// A package in a fresh temporary directory, removed on drop.
struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Temp {
    /// The package `app` with `src/main.cheby` holding `text`.
    fn package(name: &str, text: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("cheby_lsp_tokens_{}_{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/main.cheby"), text).unwrap();
        Self(std::path::absolute(dir).unwrap())
    }

    fn main(&self) -> PathBuf {
        self.0.join("src/main.cheby")
    }
}

/// One Q run of `root` with `open` as open documents, as the server runs
/// it.
fn analyze(root: &Path, open: &[(PathBuf, &str)]) -> Analyzed {
    let mut session = Session::new(SessionConfig {
        root: root.to_path_buf(),
        kind: PackageKind::Package,
        target: None,
        globals: Arc::new(Globals::new()),
    });
    let overlay = Overlay {
        files: open
            .iter()
            .map(|(path, text)| {
                let file = OverlayFile {
                    text: Arc::from(*text),
                    version: 1,
                    stamp: 1,
                };
                (path.clone(), file)
            })
            .collect(),
    };
    let live = session.analyze_live(&overlay).expect("a run");
    Analyzed::new(live.snapshot.expect("a snapshot"))
}

/// Runs `handler` on `params` with `settings`, in `encoding`.
fn ask(
    analyzed: &Analyzed,
    settings: &Settings,
    encoding: PositionEncoding,
    handler: fn(&RequestCx<'_>, &Analyzed, Value) -> Result<super::HandlerOutput, RequestError>,
    params: Value,
) -> Value {
    let caps = ClientCaps::default();
    let cancel = CancelToken::new();
    let cx = RequestCx {
        settings,
        caps: &caps,
        encoding,
        cancel: &cancel,
        std: &crate::features::stdlib::StdService::none(),
    };
    handler(&cx, analyzed, params).expect("an answer").value
}

/// The `textDocument` parameter of `path`.
fn document(path: &Path) -> Value {
    json!({ "uri": path_to_uri(path).unwrap().as_str() })
}

/// The tokens of `value`, decoded against `text`: one line per token,
/// `line:col text type modifiers`, zero-based, in `encoding`.
fn decode(value: &Value, text: &str, encoding: PositionEncoding) -> Vec<String> {
    let data: Vec<u32> = value["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|n| u32::try_from(n.as_u64().unwrap()).unwrap())
        .collect();
    assert_eq!(data.len() % 5, 0, "{data:?}");
    let lines = LspLines::new(text, Bom::Counted);
    let (mut line, mut col) = (0_u32, 0_u32);
    let mut out = Vec::new();
    for chunk in data.chunks(5) {
        let &[dl, ds, len, ty, mods] = chunk else {
            panic!("{chunk:?}")
        };
        if dl == 0 {
            col = col.checked_add(ds).unwrap();
        } else {
            line = line.checked_add(dl).unwrap();
            col = ds;
        }
        let start = lines.offset(text, Position::new(line, col), encoding);
        let end = lines.offset(
            text,
            Position::new(line, col.checked_add(len).unwrap()),
            encoding,
        );
        let word = TextRange::new(start, end).slice(text).unwrap();
        let ty = TOKEN_TYPES[usize::try_from(ty).unwrap()];
        let mut entry = format!("{line}:{col} {word} {ty}");
        for name in modifier_names(mods) {
            entry.push(' ');
            entry.push_str(name);
        }
        out.push(entry);
    }
    out
}

/// The names of the modifier bits of `bits`.
fn modifier_names(bits: u32) -> Vec<&'static str> {
    TOKEN_MODIFIERS
        .iter()
        .enumerate()
        .filter(|&(bit, _)| bits & (1 << bit) != 0)
        .map(|(_, &name)| name)
        .collect()
}

/// The decoded full answer for `text` as the package's only module, open
/// in the editor.
fn full_of(name: &str, text: &str, encoding: PositionEncoding) -> Vec<String> {
    let temp = Temp::package(name, text);
    let analyzed = analyze(&temp.0, &[(temp.main(), text)]);
    let value = ask(
        &analyzed,
        &Settings::default(),
        encoding,
        full,
        json!({ "textDocument": document(&temp.main()) }),
    );
    decode(&value, text, encoding)
}

/// Asserts that `got` holds each of `want`, in order.
fn assert_holds(got: &[String], want: &[&str]) {
    let mut rest = got.iter();
    for line in want {
        assert!(
            rest.any(|g| g == line),
            "missing or out of order: {line}\n{}",
            got.join("\n")
        );
    }
}

#[test]
fn legend_follows_the_enums() {
    for ty in TokenType::ALL {
        assert_eq!(TOKEN_TYPES[usize::try_from(ty.index()).unwrap()], ty.name());
    }
    let bits = [
        Modifiers::DECLARATION,
        Modifiers::DEPRECATED,
        Modifiers::DEFAULT_LIBRARY,
        Modifiers::ASYNC,
        Modifiers::IMPLICIT_DYN,
    ];
    for (bit, m) in bits.iter().enumerate() {
        assert_eq!(m.bits(), 1 << bit);
        assert_eq!(m.names(), vec![TOKEN_MODIFIERS[bit]]);
    }
    let legend = legend();
    assert_eq!(legend.token_types.len(), 12);
    assert_eq!(legend.token_modifiers.len(), 5);
    let provider = serde_json::to_value(provider()).unwrap();
    assert_eq!(provider["full"], json!(true));
    assert_eq!(provider["range"], json!(true));
    assert_eq!(provider["legend"]["tokenTypes"][3], json!("struct"));
    assert_eq!(
        provider["legend"]["tokenModifiers"][4],
        json!("implicitDyn")
    );
}

const CLEAN: &str = "\
import std::io
import std::list
import std::option as opt

pub type Point {
  x: Int,
  y: Int,
}

type Shape {
  Dot,
  Circle(Float),
}

interface Named {
  fn name(Self) -> String
}

const origin: Point = Point { x: 0, y: 0 }

@deprecated(\"use area\")
pub fn old(s: Shape) -> Float {
  area(s)
}

fn area(s: Shape) -> Float {
  case s {
    Dot => 0.0
    Circle(r) => r *. r
  }
}

fn total<T>(xs: List<T>, f: fn(T) -> Int) -> Int {
  fn add(a, b) {
    a + b
  }
  list::fold(xs, 0, fn(acc, x) { add(acc, f(x)) })
}

fn main() {
  let x = 1
  let p = Point { x, y: 2 }
  let Point { x: px, y } = p
  let shown = opt::Some(px)
  io::println(\"{x} {p.y} {y}\")
  let _ = shown
  let _ = old(Dot)
  let _ = origin.x
}
";

#[test]
#[cfg_attr(windows, ignore = "builds and checks a package (D-545)")]
fn clean_file_gives_each_kind() {
    let got = full_of("clean", CLEAN, PositionEncoding::Utf16);
    assert_holds(
        &got,
        &[
            // `std` is a package, not a module: the syntax's namespace.
            "0:7 std namespace",
            "0:12 io namespace defaultLibrary",
            "2:12 option namespace defaultLibrary",
            "2:22 opt namespace declaration defaultLibrary",
            "4:9 Point struct declaration",
            "5:2 x property declaration",
            "5:5 Int type defaultLibrary",
            "9:5 Shape enum declaration",
            "10:2 Dot enumMember declaration",
            "11:2 Circle enumMember declaration",
            "11:9 Float type defaultLibrary",
            "14:10 Named interface declaration",
            "15:5 name method declaration",
            "15:10 Self typeParameter",
            "18:6 origin variable declaration",
            "18:14 Point struct",
            // A record's constructor is the record.
            "18:22 Point struct",
            "18:30 x property",
            "21:7 old function declaration deprecated",
            "21:11 s parameter declaration",
            "21:14 Shape enum",
            "22:2 area function",
            "22:7 s parameter",
            "27:4 Dot enumMember",
            "28:4 Circle enumMember",
            "28:11 r variable declaration",
            "28:17 r variable",
            "32:3 total function declaration",
            "32:9 T typeParameter declaration",
            "32:12 xs parameter declaration",
            "32:16 List type defaultLibrary",
            "32:21 T typeParameter",
            "33:5 add function declaration",
            "33:9 a parameter declaration",
            "34:4 a parameter",
            "36:2 list namespace defaultLibrary",
            "36:8 fold function defaultLibrary",
            "36:13 xs parameter",
            "36:23 acc parameter declaration",
            "36:33 add function",
            "40:6 x variable declaration",
            // A punned field shows its value, and a punned field pattern
            // its binding (X §5.5).
            "41:18 x variable",
            "41:21 y property",
            "42:14 x property",
            "42:17 px variable declaration",
            "42:21 y variable declaration",
            "43:14 opt namespace defaultLibrary",
            "43:19 Some enumMember defaultLibrary",
            "44:2 io namespace defaultLibrary",
            "44:6 println function defaultLibrary",
            "44:16 x variable",
            "44:20 p variable",
            "44:22 y property",
            "44:26 y variable",
            "46:10 old function deprecated",
            "46:14 Dot enumMember",
            "47:10 origin variable",
            "47:17 x property",
        ],
    );
    // An attribute's name and arguments stay tree-sitter's.
    assert!(!got.iter().any(|t| t.starts_with("20:")), "{got:?}");
}

/// `implicitDyn` marks a name converted to a `dyn` type where it stands,
/// and only when the name is the whole expression (X §5.5).
#[test]
#[cfg_attr(windows, ignore = "builds and checks a package (D-545)")]
fn a_converted_name_is_implicit_dyn() {
    let text = "fn shown(value: dyn Show) -> String {\n  \"{value}\"\n}\n\nfn main() {\n  let n = 1\n  let a = shown(n)\n  let b = shown(n + 1)\n  let c: dyn Show = n\n  a\n}\n";
    let got = full_of("dyn", text, PositionEncoding::Utf16);
    assert_holds(
        &got,
        &[
            "1:4 value parameter",
            "6:16 n variable implicitDyn",
            "7:16 n variable",
            "8:20 n variable implicitDyn",
        ],
    );
    assert!(got.iter().any(|t| t == "7:16 n variable"), "{got:?}");
}

#[test]
#[cfg_attr(windows, ignore = "builds and checks a package (D-545)")]
fn type_error_keeps_the_kinds() {
    let text = "fn main() {\n  let n: Int = \"no\"\n  let m = nope(n)\n  m\n}\n";
    let got = full_of("type_error", text, PositionEncoding::Utf16);
    assert_eq!(
        got,
        [
            "0:3 main function declaration",
            "1:6 n variable declaration",
            "1:9 Int type defaultLibrary",
            "2:6 m variable declaration",
            // Unresolved: the callee row of the fallback.
            "2:10 nope function",
            "2:15 n variable",
            "3:2 m variable",
        ]
    );
}

#[test]
#[cfg_attr(windows, ignore = "builds and checks a package (D-545)")]
fn grammar_error_falls_back_on_the_syntax() {
    let text =
        "type T {\n  A,\n}\n\nfn f(x: Int) -> Int {\n  let y = x +\n}\n\nfn g() {\n  f(1) A\n}\n";
    let got = full_of("grammar_error", text, PositionEncoding::Utf16);
    assert_holds(
        &got,
        &[
            "0:5 T enum declaration",
            "1:2 A enumMember declaration",
            "4:3 f function declaration",
            "4:5 x parameter declaration",
            "4:8 Int type defaultLibrary",
            "5:6 y variable declaration",
            "5:10 x parameter",
            "8:3 g function declaration",
            "9:2 f function",
        ],
    );
}

#[test]
#[cfg_attr(windows, ignore = "builds and checks a package (D-545)")]
fn range_gives_the_overlapping_tokens() {
    let text = "fn main() {\n  let alpha = 1\n  let beta = alpha\n  beta\n}\n";
    let temp = Temp::package("range", text);
    let analyzed = analyze(&temp.0, &[(temp.main(), text)]);
    let value = ask(
        &analyzed,
        &Settings::default(),
        PositionEncoding::Utf16,
        range,
        json!({
            "textDocument": document(&temp.main()),
            // From inside `alpha` on line 1 to just before `alpha` on line 2.
            "range": { "start": { "line": 1, "character": 8 }, "end": { "line": 2, "character": 13 } },
        }),
    );
    // The first token is relative to the start of the document.
    assert_eq!(value["data"][0], json!(1));
    assert_eq!(
        decode(&value, text, PositionEncoding::Utf16),
        [
            "1:6 alpha variable declaration",
            "2:6 beta variable declaration",
        ]
    );
}

#[test]
#[cfg_attr(windows, ignore = "builds and checks a package (D-545)")]
fn the_setting_turns_them_off() {
    let text = "fn main() {\n  1\n}\n";
    let temp = Temp::package("off", text);
    let analyzed = analyze(&temp.0, &[(temp.main(), text)]);
    let mut settings = Settings::default();
    settings.semantic_tokens.enabled = false;
    let p = json!({ "textDocument": document(&temp.main()) });
    let value = ask(
        &analyzed,
        &settings,
        PositionEncoding::Utf16,
        full,
        p.clone(),
    );
    assert_eq!(value, Value::Null);
    let p = json!({
        "textDocument": document(&temp.main()),
        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 3, "character": 0 } },
    });
    let value = ask(&analyzed, &settings, PositionEncoding::Utf16, range, p);
    assert_eq!(value, Value::Null);
}

#[test]
#[cfg_attr(windows, ignore = "builds and checks a package (D-545)")]
fn columns_and_lengths_use_the_encoding() {
    let text = "fn main() {\n  let pair = (\"😀é\", main)\n  pair\n}\n";
    // `main` starts after 15 ASCII characters, an emoji and é, then 3 more:
    // 6 bytes in UTF-8, 3 units in UTF-16, 2 in UTF-32.
    for (encoding, col) in [
        (PositionEncoding::Utf8, 24),
        (PositionEncoding::Utf16, 21),
        (PositionEncoding::Utf32, 20),
    ] {
        let got = full_of("encoding", text, encoding);
        assert_eq!(
            got,
            [
                "0:3 main function declaration".to_owned(),
                "1:6 pair variable declaration".to_owned(),
                format!("1:{col} main function"),
                "2:2 pair variable".to_owned(),
            ],
            "{encoding:?}"
        );
    }
}

#[test]
#[cfg_attr(windows, ignore = "builds and checks a package (D-545)")]
fn a_document_outside_the_snapshot_gets_null() {
    let text = "fn main() {\n  1\n}\n";
    let temp = Temp::package("outside", text);
    let analyzed = analyze(&temp.0, &[(temp.main(), text)]);
    let other = temp.0.join("src/other.cheby");
    let value = ask(
        &analyzed,
        &Settings::default(),
        PositionEncoding::Utf16,
        full,
        json!({ "textDocument": document(&other) }),
    );
    assert_eq!(value, Value::Null);
}

/// The example packages of `docs/examples`.
fn examples() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
    // The server's paths are normalized, with no `..` (A §6.4).
    let dir = crate::text::uri::normalize(&dir);
    let mut roots: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("docs/examples")
        .map(|e| e.expect("an entry").path())
        .filter(|p| p.join("cheby.toml").is_file())
        .collect();
    roots.sort();
    assert!(roots.len() >= 32, "{roots:?}");
    roots
}

/// Every token of every module of every example is an identifier, in
/// order, with no overlap.
#[test]
#[cfg_attr(windows, ignore = "builds and checks packages (D-545)")]
fn examples_give_ordered_identifier_tokens() {
    let mut total = 0_usize;
    for root in examples() {
        let analyzed = analyze(&root, &[]);
        let snapshot = &*analyzed.snapshot;
        for module in snapshot.local_modules() {
            let (Some(file), Some(tree)) = (module.file(), module.tree()) else {
                continue;
            };
            let text = snapshot.source(file).unwrap().text();
            let path = match snapshot.location(file).unwrap() {
                cheby_driver::analysis::FileLocation::Package(path) => path.clone(),
                other => panic!("{other:?}"),
            };
            let value = ask(
                &analyzed,
                &Settings::default(),
                PositionEncoding::Utf16,
                full,
                json!({ "textDocument": document(&path) }),
            );
            let lines = LspLines::new(text, Bom::Hidden);
            let idents: Vec<TextRange> = tree
                .root()
                .tokens()
                .filter(|t| matches!(t.kind(), SyntaxKind::LOWER_IDENT | SyntaxKind::UPPER_IDENT))
                .map(cheby_syntax::SyntaxToken::text_range)
                .collect();
            let data = value["data"]
                .as_array()
                .unwrap_or_else(|| panic!("{}: {value}", path.display()));
            let (mut line, mut col) = (0_u32, 0_u32);
            let mut last: Option<TextRange> = None;
            for chunk in data.chunks(5) {
                let n = |i: usize| u32::try_from(chunk[i].as_u64().unwrap()).unwrap();
                if n(0) == 0 {
                    col = col.checked_add(n(1)).unwrap();
                } else {
                    line = line.checked_add(n(0)).unwrap();
                    col = n(1);
                }
                let start = lines.offset(text, Position::new(line, col), PositionEncoding::Utf16);
                let end = lines.offset(
                    text,
                    Position::new(line, col.checked_add(n(2)).unwrap()),
                    PositionEncoding::Utf16,
                );
                let range = TextRange::new(start, end);
                assert!(
                    idents.contains(&range),
                    "{}: {range:?} is no identifier",
                    path.display()
                );
                assert!(
                    last.is_none_or(|l| l.end() <= range.start()),
                    "{}",
                    path.display()
                );
                last = Some(range);
                total = total.checked_add(1).unwrap();
            }
            assert!(data.len() / 5 <= idents.len());
        }
    }
    assert!(total > 10_000, "{total}");
}
