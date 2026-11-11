//! Tests of hover: each row of F §1.8 and X §2.1 on clean text, a type
//! error and a grammar error. Each test runs the front end, so each
//! carries the Windows ignore (D-545, D-641).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use std::collections::BTreeMap;

use cheby_driver::analysis::{
    Globals, ModulePath, Overlay, OverlayFile, PackageKind, Session, SessionConfig, StdBase,
    StdIndex, Symbol, SymbolNs, codes,
};

use super::{
    HoverText, cheby_fences, hover, inline_code, show_initializer, source, written_initializer,
};
use crate::features::Analyzed;
use crate::server::ConstValues;

/// A package in a fresh temporary directory, removed on drop.
pub(crate) struct Temp(pub PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Temp {
    /// The package `app` with these files under `src/`.
    pub(crate) fn package(name: &str, files: &[(&str, &str)]) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("cheby_lsp_hover_{}_{name}_{n}", std::process::id()));
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
        Self(std::path::absolute(dir).unwrap())
    }

    /// The absolute path of `src/<file>`.
    pub(crate) fn src(&self, file: &str) -> PathBuf {
        self.0.join("src").join(file)
    }
}

/// One Q run of `root` with `open` as open documents, as the server runs
/// it.
pub(crate) fn analyze(root: &Path, open: &[(PathBuf, &str)]) -> Analyzed {
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

/// The offset of the `|` in `needle`, which must occur once in `text`
/// with the `|` removed.
fn offset_of(text: &str, needle: &str) -> u32 {
    let bar = needle.find('|').expect("a cursor");
    let plain = needle.replace('|', "");
    let at = text
        .find(&plain)
        .unwrap_or_else(|| panic!("`{plain}` in the text"));
    assert_eq!(text.matches(&plain).count(), 1, "`{plain}` occurs once");
    u32::try_from(at.checked_add(bar).unwrap()).unwrap()
}

/// The hover at the `|` of `needle` in `path`, whose text is `text`.
fn at(
    analyzed: &Analyzed,
    path: &Path,
    text: &str,
    needle: &str,
    std: Option<&StdIndex>,
) -> Option<HoverText> {
    let doc = source(&analyzed.snapshot, path).expect("the document");
    assert_eq!(doc.text, text);
    hover(analyzed, &doc, offset_of(text, needle), std.into(), None)
}

const GEO: &str = r#"//! Points on a plane.
//!
//! A second paragraph.

/// A point.
pub exposed type Point {
  /// Across.
  x: Int,
  y: Int,
}

/// Adds two points.
pub fn add(a: Point, b: Point) -> Point {
  Point { x: a.x + b.x, y: a.y + b.y }
}

/// Shows a point.
///
/// ```
/// show(p)
/// ```
pub fn show(p: Point) -> String {
  "({p.x}, {p.y})"
}
"#;

const MAIN: &str = r#"//! The app.

import app::geo
import std::list

/// Something that can be doubled.
interface Twice: Show {
  fn twice(Self) -> Self
}

/// The first of `xs`, or `fallback`.
fn first_or<T: Show>(xs: List<T>, fallback: T) -> T {
  case xs {
    [head, ..] => head
    [] => fallback
  }
}

fn plus(a: Int, b: Int) -> Int {
  a + b
}

fn total(n: Int) -> Int {
  todo
}

fn main() {
  let start = geo::Point { x: 1, y: 2 }
  let x = 3
  let other = geo::Point { x, y: 4 }
  let sum = start + other
  let label = "{sum}"
  let across = sum.x
  let shown = [start, other] |> list::map(_, geo::show)
  let bigger = list::map([1, 2], plus(_, 1))
  let picked = first_or([start], other)
  assert label == "" && across == x && shown == [] && bigger == [] && picked == sum
  assert total(1) == 0
}
"#;

/// `MAIN` with `edit` replacing `from`.
fn edited(from: &str, edit: &str) -> String {
    assert_eq!(MAIN.matches(from).count(), 1, "`{from}` occurs once");
    MAIN.replace(from, edit)
}

/// The rendered hover at `needle` of `main` in a package with `GEO`.
fn hovers(main: &str, needles: &[&str], std: Option<&StdIndex>) -> Vec<Option<String>> {
    let temp = Temp::package("rows", &[("geo.cheby", GEO), ("main.cheby", main)]);
    let path = temp.src("main.cheby");
    let analyzed = analyze(&temp.0, &[(path.clone(), main)]);
    needles
        .iter()
        .map(|needle| at(&analyzed, &path, main, needle, std).map(|h| h.render(true)))
        .collect()
}

/// A code block of Markdown.
fn code(text: &str) -> String {
    format!("```cheby\n{text}\n```")
}

/// The rows every state answers the same way: names outside the edit.
const STABLE: &[(&str, &str)] = &[
    ("let st|art =", "```cheby\nlet start: geo::Point\n```"),
    (
        "fn first_or<T: Show>(x|s: List<T>",
        "```cheby\n(parameter) xs: List<T>\n```",
    ),
    (
        "fn first_or<|T: Show>",
        "```cheby\nT: Show\n```\n\n---\n\nType parameter of `first_or`.",
    ),
    (
        "fn twice(Se|lf)",
        "```cheby\nSelf\n```\n\n---\n\nThe type that satisfies `Twice`.",
    ),
    (
        "import app::g|eo",
        "```cheby\nimport app::geo\n```\n\n---\n\nPoints on a plane.",
    ),
    ("  t|odo", "```cheby\nInt\n```"),
    ("[head, ..] => he|ad", "```cheby\nlet head: T\n```"),
];

fn check_stable(main: &str) {
    let needles: Vec<&str> = STABLE.iter().map(|(n, _)| *n).collect();
    let got = hovers(main, &needles, None);
    for ((needle, want), got) in STABLE.iter().zip(got) {
        assert_eq!(got.as_deref(), Some(*want), "at `{needle}`");
    }
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn rows_on_clean_text() {
    check_stable(MAIN);
    let rows: &[(&str, Option<String>)] = &[
        // An item at a use, with its module and docs.
        (
            "let sum = start + oth|er",
            Some(code("let other: geo::Point")),
        ),
        (
            "let across = sum.|x",
            Some(format!(
                "{}\n\n---\n\nField of `geo::Point`.\n\n---\n\nAcross.",
                code("x: Int")
            )),
        ),
        // A punned field shows the local, with the field as a note.
        (
            "geo::Point { |x, y: 4 }",
            Some(format!(
                "{}\n\n---\n\nAlso the field `x` of `geo::Point`.",
                code("let x: Int")
            )),
        ),
        // The operator calls `geo::add` (§8.7).
        (
            "start |+ other",
            Some(format!(
                "{}\n\n---\n\n`+` calls this function.\n\nDefined in `app::geo`.\n\n---\n\nAdds two points.",
                code("pub fn add(a: geo::Point, b: geo::Point) -> geo::Point")
            )),
        ),
        // `{sum}` calls `geo::show`, and a bare fence becomes Cheby.
        (
            "\"|{sum}\"",
            Some(format!(
                "{}\n\n---\n\nThis interpolation calls this function.\n\nDefined in `app::geo`.\n\n---\n\nShows a point.\n\n```cheby\nshow(p)\n```",
                code("pub fn show(p: geo::Point) -> String")
            )),
        ),
        // A generic function at a call: declared, then instantiated.
        (
            "let picked = first|_or(",
            Some(format!(
                "{}\n\n---\n\nDefined in `app`.\n\n---\n\nThe first of `xs`, or `fallback`.",
                [
                    code("fn first_or<T: Show>(xs: List<T>, fallback: T) -> T"),
                    code("fn(List<geo::Point>, geo::Point) -> geo::Point"),
                ]
                .join("\n")
            )),
        ),
        // Holes: a pipe stage's and a capture's.
        (
            "list::map(|_, geo::show)",
            Some(format!(
                "{}\n\n---\n\n`_` is filled with a value of this type.",
                code("List<geo::Point>")
            )),
        ),
        (
            "plus(|_, 1)",
            Some(format!(
                "{}\n\n---\n\n`_` is filled with a value of this type.",
                code("Int")
            )),
        ),
        // A literal and a keyword.
        ("let x = |3", Some(code("Int"))),
        ("|let x = 3", None),
        // `==` is built in (D-047).
        (
            "label |== \"\"",
            Some("`==` is built in and compares structurally.".to_owned()),
        ),
    ];
    let needles: Vec<&str> = rows.iter().map(|(n, _)| *n).collect();
    let got = hovers(MAIN, &needles, None);
    for ((needle, want), got) in rows.iter().zip(got) {
        assert_eq!(&got, want, "at `{needle}`");
    }
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn rows_with_a_type_error() {
    let main = edited(
        "  let label = \"{sum}\"\n",
        "  let label = \"{sum}\"\n  let wrong: Int = sum\n",
    );
    check_stable(&main);
    let got = hovers(
        &main,
        &[
            "let wrong: Int = s|um",
            "let picked = first|_or(",
            "let across = sum.|x",
        ],
        None,
    );
    assert_eq!(
        got[0].as_deref(),
        Some(
            format!(
                "{}\n\n---\n\nExpected `Int`, found `geo::Point`.",
                code("let sum: geo::Point")
            )
            .as_str()
        )
    );
    assert!(
        got[1]
            .as_deref()
            .is_some_and(|h| h.contains(&code("fn(List<geo::Point>, geo::Point) -> geo::Point")))
    );
    assert!(
        got[2]
            .as_deref()
            .is_some_and(|h| h.starts_with(&code("x: Int")))
    );
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn rows_with_a_grammar_error() {
    let main = edited(
        "  assert total(1) == 0\n",
        "  assert total(1) == 0\n  let broken = sum.\n",
    );
    check_stable(&main);
    let got = hovers(
        &main,
        &[
            "let broken = s|um.",
            "let picked = first|_or(",
            "list::map(|_, geo::show)",
        ],
        None,
    );
    assert_eq!(
        got[0].as_deref(),
        Some(code("let sum: geo::Point").as_str())
    );
    assert!(
        got[1]
            .as_deref()
            .is_some_and(|h| h.contains(&code("fn(List<geo::Point>, geo::Point) -> geo::Point")))
    );
    assert!(
        got[2]
            .as_deref()
            .is_some_and(|h| h.starts_with(&code("List<geo::Point>")))
    );
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_name_in_an_error_node_falls_back_to_the_local() {
    // `(sum` never closes, so `sum` sits where nothing is resolved, and
    // the local in scope with its text answers (F §1.8).
    let main = edited(
        "  assert total(1) == 0\n",
        "  assert total(1) == 0\n  let c = ) sum\n",
    );
    let got = hovers(&main, &["let c = ) s|um"], None);
    assert_eq!(
        got[0].as_deref(),
        Some(code("let sum: geo::Point").as_str())
    );
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_name_that_cannot_be_a_local_has_no_fallback() {
    // `q` has no known type, so its fields are unplaced, and so are the
    // later segments of paths that do not resolve. None of them can name
    // the local `x` in scope (F §1.8). Nor is `z`, which `geo::Point`
    // does not declare, a field of it.
    let main = edited(
        "  assert total(1) == 0\n",
        "  assert total(1) == 0\n  let q = mystery(start)\n  let r1 = q.x\n  \
         let r2 = list::x\n  let r3 = nope::x\n  let r4 = \"{q.x}\"\n  let r5 = start.z\n",
    );
    let got = hovers(
        &main,
        &[
            "let r1 = q.|x",
            "let r2 = list::|x",
            "let r3 = nope::|x",
            "let r4 = \"{q.|x}",
            "let r5 = start.|z",
        ],
        None,
    );
    assert_eq!(got, vec![None, None, None, None, None]);
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn std_docs_come_from_the_std_index() {
    let base = StdBase::build(Arc::new(Globals::new())).expect("std checks");
    let got = hovers(
        MAIN,
        &["import std::l|ist", "let bigger = list::m|ap("],
        Some(base.index()),
    );
    let module = got[0].as_deref().expect("a module hover");
    assert!(module.starts_with(&code("import std::list")), "{module}");
    assert!(
        module.contains("\n\n---\n\n"),
        "the module's docs: {module}"
    );
    let map = got[1].as_deref().expect("an item hover");
    assert!(map.starts_with("```cheby\npub fn map<T, U>("), "{map}");
    assert!(
        map.contains(&code("fn(List<Int>, fn(Int) -> Int) -> List<Int>")),
        "{map}"
    );
    assert!(map.contains("Defined in `std::list`."), "{map}");
    assert!(
        map.matches("\n\n---\n\n").count() == 2,
        "docs follow: {map}"
    );
}

#[test]
fn bare_fences_become_cheby() {
    let docs = "Text.\n\n```\nlet x = 1\n```\n\n```text\nplain\n```\n  ```\n  y\n  ```";
    assert_eq!(
        cheby_fences(docs),
        "Text.\n\n```cheby\nlet x = 1\n```\n\n```text\nplain\n```\n  ```cheby\n  y\n  ```"
    );
}

#[test]
fn plain_text_has_no_fences() {
    let hover = HoverText {
        code: vec!["let x: Int".to_owned(), "Int".to_owned()],
        notes: vec!["A note.".to_owned()],
        docs: Some("Docs.".to_owned()),
        ..HoverText::default()
    };
    assert_eq!(
        hover.render(false),
        "let x: Int\nInt\n\n---\n\nA note.\n\n---\n\nDocs."
    );
    assert_eq!(
        hover.render(true),
        "```cheby\nlet x: Int\n```\n```cheby\nInt\n```\n\n---\n\nA note.\n\n---\n\nDocs."
    );
}

/// A package whose root module declares the constants `limit`, `twice`
/// and `broken`, and reads `int::max_value` from std.
const CONSTANTS: &str = r"import std::int

/// The most.
const limit: Int = 20

const twice: Int = limit * 2

const broken: Int = limit / 0

fn main() {
  assert limit + twice + broken < int::max_value
}
";

/// The symbol of the root module's value `name`.
fn app_value(name: &str) -> Symbol {
    Symbol::item(ModulePath::root("app"), SymbolNs::Value, name)
}

/// The rendered hovers at `needles` of [`CONSTANTS`], with `values` as
/// what the last evaluation said.
fn constant_hovers(
    needles: &[&str],
    values: Option<&ConstValues>,
    std: Option<&StdIndex>,
) -> Vec<Option<String>> {
    let temp = Temp::package("constants", &[("main.cheby", CONSTANTS)]);
    let path = temp.src("main.cheby");
    let analyzed = analyze(&temp.0, &[(path.clone(), CONSTANTS)]);
    let doc = source(&analyzed.snapshot, &path).expect("the document");
    needles
        .iter()
        .map(|needle| {
            let offset = offset_of(CONSTANTS, needle);
            hover(&analyzed, &doc, offset, std.into(), values).map(|h| h.render(true))
        })
        .collect()
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_failed_constant_names_its_code() {
    // S-20: the diagnostic marks its own constant, and only that one. A
    // value reaching hover, and an older value marked stale, are
    // `tests/lsp/constants/`'s, through the real child.
    // A failed run marks every value stale (D-1413), and a constant with
    // none says it is not evaluated, not that a save computes it (D-1490).
    let values = ConstValues {
        failed: BTreeMap::from([(app_value("broken"), codes::CONSTANT_PANICKED)]),
        stale: true,
        ..ConstValues::default()
    };
    let got = constant_hovers(&["const bro|ken", "assert li|mit +"], Some(&values), None);
    assert_eq!(
        got[0].as_deref(),
        Some(
            "```cheby\nconst broken: Int\n```\n\n---\n\nEvaluation failed, see `E0075`.\n\n\
             Defined in `app`."
        )
    );
    assert_eq!(
        got[1].as_deref(),
        Some(
            "```cheby\nconst limit: Int\n```\n\n---\n\nNot evaluated: the last evaluation failed \
             or was cut short (see `cheby/status`).\n\nDefined in `app`.\n\n---\n\nThe most."
        )
    );
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn no_evaluation_yet_says_so_and_no_evaluation_says_nothing() {
    let values = ConstValues::default();
    let got = constant_hovers(&["const tw|ice"], Some(&values), None);
    assert_eq!(
        got[0].as_deref(),
        Some(
            "```cheby\nconst twice: Int\n```\n\n---\n\nNot evaluated yet: values are computed on \
             save.\n\nDefined in `app`."
        )
    );
    // Single-file mode, Windows, or `constants.evaluate` off: no line.
    let got = constant_hovers(&["const tw|ice"], None, None);
    assert_eq!(
        got[0].as_deref(),
        Some("```cheby\nconst twice: Int\n```\n\n---\n\nDefined in `app`.")
    );
}

/// D-1497: after a run skipped because the saved files have front-end
/// errors, a constant with no value says that saving gives it, not that
/// the evaluation failed (D-1490).
#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_run_skipped_for_errors_on_disk_says_so() {
    let values = ConstValues {
        stale: true,
        front_errors: true,
        ..ConstValues::default()
    };
    let got = constant_hovers(&["const tw|ice"], Some(&values), None);
    assert_eq!(
        got[0].as_deref(),
        Some(
            "```cheby\nconst twice: Int\n```\n\n---\n\nNot evaluated yet: the saved files have \
             errors; values are computed on save.\n\nDefined in `app`."
        )
    );
}

/// D-1501: after a save that left front-end errors on disk, so the
/// server started no run, a constant with no value says what it says
/// after a run the child skipped (D-1497), not that a save computes it.
#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_run_the_server_never_started_for_errors_says_so() {
    let values = ConstValues {
        disk_errors: true,
        ..ConstValues::default()
    };
    let got = constant_hovers(&["const tw|ice"], Some(&values), None);
    assert_eq!(
        got[0].as_deref(),
        Some(
            "```cheby\nconst twice: Int\n```\n\n---\n\nNot evaluated yet: the saved files have \
             errors; values are computed on save.\n\nDefined in `app`."
        )
    );
}

/// D-1496: a written initializer, std's or a dependency's, is cut at
/// 4,096 characters with `…`, as the child cuts a value (D-1413). No std
/// constant is that long and no run has a dependency before M5, so the
/// cut is checked on the text hover joins to the header.
#[test]
fn a_long_written_initializer_is_cut() {
    let short = "[1, 2, 3]".to_owned();
    assert_eq!(written_initializer(short.clone()), short);
    let exact = "é".repeat(4096);
    assert_eq!(written_initializer(exact.clone()), exact);
    let long = format!("[{}]", "1, ".repeat(2000));
    let shown = written_initializer(long.clone());
    assert_eq!(shown.chars().count(), 4097, "4,096 characters and the mark");
    assert!(shown.ends_with('…'), "{shown}");
    assert!(long.starts_with(shown.trim_end_matches('…')));
    // Characters, not bytes, as the child counts them.
    let wide = "é".repeat(5000);
    assert_eq!(written_initializer(wide), format!("{}…", "é".repeat(4096)));
}

/// D-1496: the initializer hover joins to a std or dependency constant's
/// header is the cut one.
#[test]
fn a_long_initializer_is_cut_in_the_header() {
    let mut hover = HoverText {
        code: vec!["const table: List(Int)".to_owned()],
        ..HoverText::default()
    };
    let long = format!("[{}]", "1, ".repeat(2000));
    show_initializer(&mut hover, long.clone());
    let header = hover.code.first().unwrap();
    let shown = header.strip_prefix("const table: List(Int) = ").unwrap();
    assert_eq!(shown.chars().count(), 4097, "4,096 characters and the mark");
    assert!(shown.ends_with('…'), "{shown}");
    assert!(long.starts_with(shown.trim_end_matches('…')));
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_std_constant_shows_its_initializer() {
    // S-20, D-1413: never "not evaluated yet", whatever the package's
    // evaluation says.
    let base = StdBase::build(Arc::new(Globals::new())).expect("std checks");
    let values = ConstValues::default();
    let got = constant_hovers(&["int::max_v|alue"], Some(&values), Some(base.index()));
    let shown = got[0].as_deref().expect("a hover");
    assert!(
        shown.starts_with(&code(
            "pub const max_value: Int = 9_223_372_036_854_775_807"
        )),
        "{shown}"
    );
    assert!(!shown.contains("Not evaluated yet"), "{shown}");
}

/// A constant of another package whose tree the run dropped, and that no
/// index knows, shows its initializer read from its text: a dependency's
/// constant, which the child never evaluates (D-1491). Std with no index
/// stands in for a dependency, which no run has before M5.
#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_dependency_constant_shows_its_initializer() {
    let values = ConstValues::default();
    let got = constant_hovers(&["int::max_v|alue"], Some(&values), None);
    let shown = got[0].as_deref().expect("a hover");
    assert!(
        shown.starts_with(&code(
            "pub const max_value: Int = 9_223_372_036_854_775_807"
        )),
        "{shown}"
    );
    assert!(!shown.contains("Not evaluated"), "{shown}");
}

#[test]
fn inline_code_fences_backticks() {
    assert_eq!(inline_code("20"), "`20`");
    assert_eq!(inline_code("\"a`b\""), "``\"a`b\"``");
    assert_eq!(inline_code("`x`"), "`` `x` ``");
}
