//! Tests of rename over one package (X §3.2 to §3.5, D-1389 to D-1392,
//! D-1640, D-1645, D-1667 to D-1673, D-1698), with the helpers of
//! `ide::hover::tests`. Each test runs only the front end, so it runs on
//! Windows too (D-1516, G1 plan §1.10).

use std::sync::{Arc, OnceLock};

use std::path::PathBuf;

use cheby_driver::analysis::{
    FileId, Globals, Introduced, Member, PlainDiagnostic, Severity, StdBase, TextRange, TextSize,
    codes,
};

use super::{Action, RenameError, RenamePlan, prepare, rename, would_cause, write};
use crate::features::ide::Refusal;
use crate::features::ide::hover::tests::{Temp, analyze};
use crate::features::index::Meaning;
use crate::features::{Analyzed, CancelToken};

/// The cursor in a test's text.
const CURSOR: char = '‸';

/// A package of `files` under `src/`, one of which holds the cursor `‸`,
/// with its Q run.
struct Case {
    temp: Temp,
    analyzed: Analyzed,
    /// The files as written, the cursor taken out.
    files: Vec<(String, String)>,
    file: FileId,
    offset: TextSize,
}

impl Case {
    fn new(files: &[(&str, &str)]) -> Self {
        let mut cursor = None;
        let mut clean = Vec::new();
        for &(name, text) in files {
            if let Some(at) = text.find(CURSOR) {
                assert!(cursor.is_none(), "one cursor");
                cursor = Some((name, at));
            }
            clean.push((name.to_owned(), text.replace(CURSOR, "")));
        }
        let (name, at) = cursor.expect("a cursor");
        let written: Vec<(&str, &str)> = clean
            .iter()
            .map(|(n, t)| (n.as_str(), t.as_str()))
            .collect();
        let temp = Temp::package("rename", &written);
        let analyzed = analyze(&temp.0, &[]);
        let file = analyzed
            .snapshot
            .file_of(&temp.src(name))
            .expect("the file");
        Self {
            temp,
            analyzed,
            files: clean,
            file,
            offset: TextSize::try_from(at).unwrap(),
        }
    }

    fn prepare(&self) -> Result<Option<(TextRange, String)>, Refusal> {
        prepare(&self.analyzed, self.file, self.offset)
    }

    fn rename(&self, new: &str) -> Result<Option<RenamePlan>, RenameError> {
        rename(
            &self.analyzed,
            self.file,
            self.offset,
            new,
            &CancelToken::new(),
        )
    }

    /// The files that `plan` changes, by name, with its edits applied.
    fn applied(&self, plan: &RenamePlan) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (name, text) in &self.files {
            let path = self.temp.src(name);
            let mut edits: Vec<_> = plan.edits.iter().filter(|e| e.path == path).collect();
            if edits.is_empty() {
                continue;
            }
            edits.sort_by_key(|e| std::cmp::Reverse(e.range.start()));
            let mut text = text.clone();
            for edit in edits {
                let start = usize::try_from(edit.range.start()).unwrap();
                let end = usize::try_from(edit.range.end()).unwrap();
                text.replace_range(start..end, &edit.replacement);
            }
            out.push((name.clone(), text));
        }
        out
    }
}

/// The plan of renaming the cursor's name in `files` to `new`, which
/// must be accepted.
#[track_caller]
fn plan(files: &[(&str, &str)], new: &str) -> (Case, RenamePlan) {
    let case = Case::new(files);
    let plan = match case.rename(new) {
        Ok(Some(plan)) => plan,
        other => panic!("expected a plan, got {other:?}"),
    };
    (case, plan)
}

/// `main.cheby` after renaming its cursor's name to `new`, with no note.
#[track_caller]
fn renamed(main: &str, new: &str) -> String {
    let (case, plan) = plan(&[("main.cheby", main)], new);
    assert_eq!(plan.note, None);
    let mut changed = case.applied(&plan);
    assert_eq!(changed.len(), 1, "{changed:?}");
    changed.pop().unwrap().1
}

/// The message of a rename of the cursor's name in `files` to `new`,
/// which must be refused.
#[track_caller]
fn refused(files: &[(&str, &str)], new: &str) -> String {
    match Case::new(files).rename(new) {
        Err(RenameError::Refused(Refusal(message))) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The message of `prepareRename` at the cursor of `main`, which must
/// refuse.
#[track_caller]
fn prepare_refused(main: &str) -> String {
    match Case::new(&[("main.cheby", main)]).prepare() {
        Err(Refusal(message)) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

const BOOK: &str = r#"type Book {
  title: String,
  pages: Int,
}

fn make(title: String, t: String) -> Book {
  let first = Book { title, pages: 1 }
  let second = Book { ..first, title }
  let third = Book { title: t, pages: 2 }
  assert second.pages == third.pages - 1
  first
}

fn describe(book: Book) -> String {
  let Book { title, pages: _ } = book
  "{title} {book.title}"
}

fn main() {
  let book = make("a", "b")
  assert describe(book) == "a a"
}
"#;

/// `BOOK` with the cursor before the `n`th (from 0) `needle`, moved
/// `into` bytes into it.
fn book_at(needle: &str, n: usize, into: usize) -> String {
    let (at, _) = BOOK.match_indices(needle).nth(n).expect("the needle");
    let mut text = BOOK.to_owned();
    text.insert(at.checked_add(into).unwrap(), CURSOR);
    text
}

/// X §3.5, the field column: a punned field keeps its value as `new: old`,
/// in a construction, an update and a pattern, and a field read and an
/// interpolation's field change their token (D-1392).
#[test]
fn renaming_a_field_writes_every_row_of_x_3_5() {
    let text = book_at("title: String", 0, 2);
    assert_eq!(
        renamed(&text, "name"),
        BOOK.replace("  title: String,", "  name: String,")
            .replace("Book { title, pages: 1 }", "Book { name: title, pages: 1 }")
            .replace("Book { ..first, title }", "Book { ..first, name: title }")
            .replace("Book { title: t, pages: 2 }", "Book { name: t, pages: 2 }")
            .replace(
                "let Book { title, pages: _ }",
                "let Book { name: title, pages: _ }"
            )
            .replace("{book.title}", "{book.name}")
    );
}

/// X §3.5, the local column: on a punned field the value wins (D-1392),
/// and it is written `old: new`; an interpolation's root changes.
#[test]
fn renaming_a_local_writes_every_row_of_x_3_5() {
    // From the pun itself: the parameter `title`.
    let text = book_at("Book { title, pages: 1 }", 0, 9);
    assert_eq!(
        renamed(&text, "heading"),
        BOOK.replace("fn make(title: String", "fn make(heading: String")
            .replace(
                "Book { title, pages: 1 }",
                "Book { title: heading, pages: 1 }"
            )
            .replace(
                "Book { ..first, title }",
                "Book { ..first, title: heading }"
            )
    );
    // A punned pattern's binding, and the root of `{title}`.
    let text = book_at("let Book { title", 0, 13);
    assert_eq!(
        renamed(&text, "heading"),
        BOOK.replace(
            "let Book { title, pages: _ }",
            "let Book { title: heading, pages: _ }"
        )
        .replace("\"{title} {book.title}\"", "\"{heading} {book.title}\"")
    );
    // `Book { title: t }`: only the value's token.
    let text = book_at("t: String", 0, 0);
    assert_eq!(
        renamed(&text, "other"),
        BOOK.replace("t: String) -> Book", "other: String) -> Book")
            .replace(
                "Book { title: t, pages: 2 }",
                "Book { title: other, pages: 2 }"
            )
    );
    // The root of `{book.title}`.
    let text = book_at("book: Book", 0, 1);
    assert_eq!(
        renamed(&text, "item"),
        BOOK.replace("fn describe(book: Book)", "fn describe(item: Book)")
            .replace("} = book\n", "} = item\n")
            .replace("{book.title}", "{item.title}")
    );
}

/// Or-patterns: every alternative's binding changes (X §3.5).
#[test]
fn renaming_a_binding_of_an_or_pattern_writes_every_alternative() {
    let text = "type Shape {\n  Circle(Int),\n  Square(Int),\n}\n\nfn size(s: Shape) -> Int {\n  case s {\n    Circle(‸n) | Square(n) => n\n  }\n}\n\nfn main() {\n  assert size(Circle(1)) == 1\n}\n";
    assert_eq!(
        renamed(text, "side"),
        text.replace(CURSOR, "").replace(
            "Circle(n) | Square(n) => n",
            "Circle(side) | Square(side) => side"
        )
    );
}

const POINTS: &str = "type Point { x: Int, y: Int }

type Shape { origin: Point, size: Int }

fn lift(shape: Shape, y: Int) -> Shape {
  Shape { ..shape, origin.y }
}

fn shift(shape: Shape) -> Shape {
  Shape { ..shape, origin.x: shape.origin.x + 1 }
}

fn main() {
  let shape = Shape { origin: Point { x: 0, y: 0 }, size: 1 }
  assert lift(shift(shape), 2).origin.y == 2
}
";

/// `POINTS` with the cursor at the start of the `n`th `needle`.
fn points_at(needle: &str, n: usize) -> String {
    let (at, _) = POINTS.match_indices(needle).nth(n).expect("the needle");
    let mut text = POINTS.to_owned();
    text.insert(at, CURSOR);
    text
}

/// D-1645, D-1640: each step of a field path is a field of its own
/// step's type, and the punned end of a path names a field and a local.
#[test]
fn renaming_through_field_paths() {
    // The first step of both paths.
    assert_eq!(
        renamed(&points_at("origin: Point", 0), "start"),
        POINTS
            .replace("origin: Point,", "start: Point,")
            .replace("origin.y }", "start.y }")
            .replace("origin.x: shape.origin.x", "start.x: shape.start.x")
            .replace("{ origin: Point {", "{ start: Point {")
            .replace(".origin.y ==", ".start.y ==")
    );
    // The field at a punned end: `origin.new: y`.
    assert_eq!(
        renamed(&points_at("y: Int }", 0), "top"),
        POINTS
            .replace("x: Int, y: Int }", "x: Int, top: Int }")
            .replace("origin.y }", "origin.top: y }")
            .replace("x: 0, y: 0", "x: 0, top: 0")
            .replace(".origin.y ==", ".origin.top ==")
    );
    // The local from a punned end: `origin.y: new`.
    assert_eq!(
        renamed(&points_at("y }", 0), "height"),
        POINTS
            .replace("y: Int) -> Shape", "height: Int) -> Shape")
            .replace("origin.y }", "origin.y: height }")
    );
    // A later step that is not punned.
    assert_eq!(
        renamed(&points_at("x: Int,", 0), "left"),
        POINTS
            .replace("{ x: Int,", "{ left: Int,")
            .replace("origin.x: shape.origin.x", "origin.left: shape.origin.left")
            .replace("{ x: 0,", "{ left: 0,")
    );
}

/// A punned end names two symbols, and is written once.
#[test]
fn a_punned_end_is_written_once() {
    let (_, plan) = plan(&[("main.cheby", &points_at("y: Int }", 0))], "top");
    let mut ranges: Vec<_> = plan.edits.iter().map(|e| (&e.path, e.range)).collect();
    let all = ranges.len();
    ranges.dedup();
    assert_eq!(ranges.len(), all);
    assert!(plan.edits.iter().any(|e| e.replacement == "top: y"));
}

const UTIL: &str = "//! Util.\n\npub fn foo() -> Int {\n  1\n}\n";

/// X §3.5: an import alias keeps its text, so only the imported name and
/// the qualified segment change.
#[test]
fn renaming_an_item_keeps_import_aliases_and_writes_the_qualified_segment() {
    let main = "import app::util::{foo as bar}\nimport app::other\n\nfn main() {\n  assert bar() + other::twice() == 3\n}\n";
    let other = "//! Other.\n\nimport app::util\n\npub fn twice() -> Int {\n  util::foo() + util::foo()\n}\n";
    let util = UTIL.replace("fn foo", "fn ‸foo");
    let (case, plan) = plan(
        &[
            ("main.cheby", main),
            ("other.cheby", other),
            ("util.cheby", &util),
        ],
        "baz",
    );
    assert_eq!(
        case.applied(&plan),
        vec![
            (
                "main.cheby".to_owned(),
                main.replace("{foo as bar}", "{baz as bar}")
            ),
            (
                "other.cheby".to_owned(),
                other.replace("util::foo()", "util::baz()")
            ),
            ("util.cheby".to_owned(), UTIL.replace("fn foo", "fn baz")),
        ]
    );
    // The edits are sorted by path, then by start.
    let keys: Vec<_> = plan
        .edits
        .iter()
        .map(|e| (e.path.clone(), e.range.start()))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}

/// An import's `as` name is the importing module's: renaming it changes
/// the alias and its uses, never the item.
#[test]
fn renaming_an_import_alias() {
    let main = "import app::util::{foo as bar}\n\nfn main() {\n  assert ‸bar() == 1\n}\n";
    let (case, plan) = plan(&[("main.cheby", main), ("util.cheby", UTIL)], "qux");
    assert_eq!(
        case.applied(&plan),
        vec![(
            "main.cheby".to_owned(),
            main.replace(CURSOR, "").replace("bar", "qux")
        )]
    );
}

/// A record type and its constructor are one name (F §5.3): a use of the
/// constructor renames the type too. `type Id { Id(Int) }` is two symbols.
#[test]
fn a_record_type_renames_with_its_constructor_and_a_variant_does_not() {
    let record = "type Point { x: Int }\n\nfn origin(p: Point) -> Int {\n  p.x\n}\n\nfn main() {\n  assert origin(‸Point { x: 0 }) == 0\n}\n";
    assert_eq!(
        renamed(record, "Spot"),
        record.replace(CURSOR, "").replace("Point", "Spot")
    );
    let variant = "type Id { Id(Int) }\n\nfn number(id: Id) -> Int {\n  let Id(n) = id\n  n\n}\n\nfn main() {\n  assert number(‸Id(1)) == 1\n}\n";
    let clean = variant.replace(CURSOR, "");
    assert_eq!(
        renamed(variant, "Key"),
        clean
            .replace("{ Id(Int) }", "{ Key(Int) }")
            .replace("let Id(n)", "let Key(n)")
            .replace("number(Id(1))", "number(Key(1))")
    );
    let ty = clean.replace("id: Id)", "id: ‸Id)");
    assert_eq!(
        renamed(&ty, "Ident"),
        clean
            .replace("type Id {", "type Ident {")
            .replace("id: Id)", "id: Ident)")
    );
}

/// A type parameter is named in its item only.
#[test]
fn renaming_a_type_parameter() {
    let text = "fn first<‸T>(items: List<T>, default: T) -> T {\n  case items {\n    [x, ..] => x\n    [] => default\n  }\n}\n\nfn main() {\n  assert first([1], 0) == 1\n}\n";
    assert_eq!(
        renamed(text, "Item"),
        text.replace(CURSOR, "").replace(
            "<T>(items: List<T>, default: T) -> T",
            "<Item>(items: List<Item>, default: Item) -> Item"
        )
    );
}

/// X §3.2: renaming a name to itself is an empty plan.
#[test]
fn the_same_name_is_an_empty_plan() {
    let case = Case::new(&[("main.cheby", &book_at("title: String", 0, 0))]);
    assert_eq!(case.rename("title"), Ok(Some(RenamePlan::default())));
}

/// X §3.1: no name, no answer. A test's name is a string (X §3.2).
#[test]
fn nothing_to_rename_is_none() {
    for text in [
        "fn main() {\n  let x = ‸1\n  assert x == 1\n}\n",
        "fn main() {\n  ‸let x = 1\n  assert x == 1\n}\n",
        "fn main() {\n  let x = 1\n‸\n  assert x == 1\n}\n",
        "fn main() {\n  assert 1 == 1\n}\n\ntest \"ad‸ds\" {\n  assert 1 + 1 == 2\n}\n",
    ] {
        let case = Case::new(&[("main.cheby", text)]);
        assert_eq!(case.prepare(), Ok(None), "{text}");
        assert_eq!(case.rename("y"), Ok(None), "{text}");
    }
}

/// `prepareRename` answers the token and the name as written, the
/// value's on a pun (D-1392).
#[test]
fn prepare_gives_the_token_and_its_text() {
    let case = Case::new(&[("main.cheby", &book_at("Book { title, pages: 1 }", 0, 10))]);
    let (range, text) = case.prepare().unwrap().unwrap();
    assert_eq!(text, "title");
    let main = &case.files.first().unwrap().1;
    assert_eq!(range.slice(main), Some("title"));
}

/// D-1390's refusals of the target, at `prepareRename` and `rename` alike.
#[test]
fn targets_that_cannot_be_renamed_are_refused() {
    for (text, expected) in [
        (
            "import std::list\n\nfn main() {\n  assert list::‸map([1], fn(x) { x }) == [1]\n}\n",
            "`list::map` is in `std`, which cannot be edited here",
        ),
        (
            "import std::list\n\nfn main() {\n  assert ‸list::map([1], fn(x) { x }) == [1]\n}\n",
            "a module's name is its file's name: rename the file",
        ),
        (
            "fn main() {\n  let x: ‸Int = 1\n  assert x == 1\n}\n",
            "`Int` is part of the language",
        ),
        (
            "fn main() {\n  assert ‸nowhere() == 1\n}\n",
            "`nowhere` does not resolve",
        ),
        (
            "interface Area {\n  fn area(‸Self) -> Int\n}\n\nfn main() {\n  assert 1 == 1\n}\n",
            "`Self` is not a name you declare",
        ),
    ] {
        assert_eq!(prepare_refused(text), expected, "{text}");
        assert_eq!(
            refused(&[("main.cheby", text)], "fresh"),
            expected,
            "{text}"
        );
    }
}

/// D-1390: an implicit occurrence, here the `add` that `+` calls, is never
/// a rename target.
#[test]
fn an_operator_is_refused() {
    let text = "type V { V(Int) }\n\nfn add(a: V, b: V) -> V {\n  let V(x) = a\n  let V(y) = b\n  V(x + y)\n}\n\nfn main() {\n  assert V(1) ‸+ V(2) == V(3)\n}\n";
    assert_eq!(
        prepare_refused(text),
        "no name is written here for `add`: rename it where it is declared"
    );
}

/// Std's base, built once for these tests.
fn base() -> &'static StdBase {
    static BASE: OnceLock<Arc<StdBase>> = OnceLock::new();
    BASE.get_or_init(|| StdBase::build(Arc::new(Globals::new())).expect("std checks"))
}

/// D-1673: in a std source every name is refused, a local's too.
#[test]
fn every_name_of_a_std_document_is_refused() {
    let analyzed = Analyzed::new(Arc::clone(base().snapshot()));
    let index = analyzed.index();
    let snapshot = &*analyzed.snapshot;
    let list = snapshot
        .all_modules()
        .find(|m| m.path().is_some_and(|p| p.to_string() == "std::list"))
        .expect("std::list");
    let module = index.module(list.id()).expect("its index");
    let local = module
        .occurrences
        .iter()
        .find(|o| {
            module
                .entry(o.symbol)
                .is_some_and(|e| matches!(e.meaning, Meaning::Local { .. }))
        })
        .expect("a local");
    let file = list.file().unwrap();
    let text = snapshot.source(file).unwrap().text();
    let name = local.range.slice(text).unwrap();
    assert_eq!(
        prepare(&analyzed, file, local.range.start()),
        Err(Refusal(format!(
            "`{name}` is in `std`, which cannot be edited here"
        )))
    );
}

/// A refusal names the dependency without its major (D-1390, D-1884).
#[test]
fn a_refusal_names_the_dependency() {
    let text = "import geo\nimport geo::shape\n\nfn main() {\n  assert geo::area(1) == 3\n  assert shape::side() == 1\n}\n";
    let temp = Temp::package("rename_dependency", &[("main.cheby", text)]);
    let files = [
        (
            "cheby.toml",
            "name = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\ngeo = { path = \"geometry\" }\n",
        ),
        (
            "geometry/cheby.toml",
            "name = \"geometry\"\nversion = \"0.1.0\"\nkind = \"library\"\n",
        ),
        (
            "geometry/src/main.cheby",
            "//! G.\n\n/// The area.\npub fn area(side: Int) -> Int {\n  side * 3\n}\n",
        ),
        (
            "geometry/src/shape.cheby",
            "//! S.\n\n/// A side.\npub fn side() -> Int {\n  1\n}\n",
        ),
    ];
    for (path, text) in files {
        let path = temp.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let analyzed = analyze(&temp.0, &[]);
    let main = analyzed.snapshot.file_of(&temp.src("main.cheby")).unwrap();
    for (name, shown) in [("area", "geometry::area"), ("side", "shape::side")] {
        let at = TextSize::try_from(text.find(name).unwrap()).unwrap();
        assert_eq!(
            prepare(&analyzed, main, at),
            Err(Refusal(format!(
                "`{shown}` is in `geometry`, which cannot be edited here"
            )))
        );
    }
}

/// X §3.3: the new name must be one identifier of the old name's case.
#[test]
fn new_names_are_checked() {
    let text = book_at("book: Book", 0, 0);
    let files = [("main.cheby", text.as_str())];
    assert_eq!(refused(&files, "x-y"), "`x-y` is not a valid name");
    assert_eq!(refused(&files, "let"), "`let` is a keyword");
    assert_eq!(
        refused(&files, "Item"),
        "`Item` must start with a lower-case letter"
    );
    assert_eq!(
        refused(&files, "itemName"),
        "`itemName` is not in snake_case"
    );
    let ty = book_at("type Book", 0, 5);
    assert_eq!(
        refused(&[("main.cheby", ty.as_str())], "novel"),
        "`novel` must start with an upper-case letter"
    );
}

const BROKEN: &str = "//! Broken.\n\npub fn broken( -> Int {\n  1\n}\n";

/// D-1390: a local needs its own file to parse, an item every file.
#[test]
fn syntax_errors_gate_by_the_reach_of_the_symbol() {
    let local =
        "fn helper() -> Int {\n  1\n}\n\nfn main() {\n  let ‸x = helper()\n  assert x == 1\n}\n";
    let (case, plan) = plan(&[("main.cheby", local), ("broken.cheby", BROKEN)], "y");
    assert_eq!(case.applied(&plan).len(), 1);
    let item = local.replace(CURSOR, "").replace("fn helper", "fn ‸helper");
    assert_eq!(
        refused(&[("main.cheby", &item), ("broken.cheby", BROKEN)], "assist"),
        "`src/broken.cheby` has syntax errors"
    );
    let own = format!("{local}\nfn odd( -> Int {{\n  1\n}}\n");
    assert_eq!(
        refused(&[("main.cheby", &own)], "y"),
        "fix the syntax errors in this file first"
    );
}

/// D-1390: an item needs no unresolved path of its name in the package.
#[test]
fn an_unresolved_path_of_the_old_name_refuses_an_item() {
    let main = "import app::util\nimport app::caller\n\nfn main() {\n  assert util::foo() == caller::call()\n}\n";
    let caller = "//! Caller.\n\npub fn call() -> Int {\n  foo()\n}\n";
    let util = UTIL.replace("fn foo", "fn ‸foo");
    assert_eq!(
        refused(
            &[
                ("main.cheby", main),
                ("caller.cheby", caller),
                ("util.cheby", &util),
            ],
            "baz",
        ),
        "`src/caller.cheby:4` may refer to `foo` but does not resolve"
    );
}

/// D-1390, D-1645: a field needs a known receiver type at every token of
/// its name in a field position, each step of a path included.
#[test]
fn a_field_with_an_unknown_receiver_is_refused() {
    let text = "type Holder<T> {\n  item: T,\n}\n\ntype Point {\n  ‸x: Int,\n  y: Int,\n}\n\nfn empty<T>(_items: List<T>) -> Holder<T> {\n  panic as \"never called\"\n}\n\nfn main() {\n  let holder = empty([])\n  let moved = Holder { ..holder, item.x: 1 }\n  let typed: Holder<Point> = moved\n  assert typed.item.y == 0\n}\n";
    assert_eq!(
        refused(&[("main.cheby", text)], "left"),
        "cannot tell what `.x` at `src/main.cheby:16` refers to"
    );
}

/// D-1727: an item does not count a field position of its name whose
/// receiver is unknown, which cannot name it (D-061), while a field of
/// that name stays refused (above).
#[test]
fn an_item_ignores_a_field_with_an_unknown_receiver() {
    let text = "type Holder<T> {\n  item: T,\n}\n\ntype Point {\n  x: Int,\n  y: Int,\n}\n\nfn empty<T>(_items: List<T>) -> Holder<T> {\n  panic as \"never called\"\n}\n\nfn ‸x() -> Int {\n  0\n}\n\nfn main() {\n  let holder = empty([])\n  let moved = Holder { ..holder, item.x: 1 }\n  let typed: Holder<Point> = moved\n  assert typed.item.y == x()\n}\n";
    assert_eq!(
        renamed(text, "left"),
        text.replace(CURSOR, "")
            .replace("fn x()", "fn left()")
            .replace("== x()", "== left()")
    );
    let field = text.replace(CURSOR, "").replace("  x: Int", "  ‸x: Int");
    assert_eq!(
        refused(&[("main.cheby", &field)], "left"),
        "cannot tell what `.x` at `src/main.cheby:20` refers to"
    );
}

/// D-1727: a punned field whose receiver is unknown still names the value
/// it puns, so an item's rename spells the field out, while the field's
/// rename stays refused.
#[test]
fn an_item_renames_a_punned_field_with_an_unknown_receiver() {
    let text = "type Holder<T> {\n  item: T,\n}\n\ntype Point {\n  x: Int,\n  y: Int,\n}\n\nfn empty<T>(_items: List<T>) -> Holder<T> {\n  panic as \"never called\"\n}\n\nconst ‸x: Int = 0\n\nfn main() {\n  let holder = empty([])\n  let moved = Holder { ..holder, item.x }\n  let typed: Holder<Point> = moved\n  assert typed.item.y == x\n}\n";
    assert_eq!(
        renamed(text, "left"),
        text.replace(CURSOR, "")
            .replace("const x", "const left")
            .replace("item.x }", "item.x: left }")
            .replace("== x\n", "== left\n")
    );
    let field = text.replace(CURSOR, "").replace("  x: Int,", "  ‸x: Int,");
    assert_eq!(
        refused(&[("main.cheby", &field)], "left"),
        "cannot tell what `.x` at `src/main.cheby:18` refers to"
    );
}

/// D-1727: a punned field whose receiver and value are both unknown still
/// counts against an item of its name.
#[test]
fn a_punned_field_unknown_both_ways_refuses_an_item() {
    let main = "import app::util\nimport app::caller\n\nfn main() {\n  assert util::x() == caller::call()\n}\n";
    let caller = "//! Caller.\n\ntype Holder<T> {\n  item: T,\n}\n\ntype Point {\n  x: Int,\n  y: Int,\n}\n\nfn empty<T>(_items: List<T>) -> Holder<T> {\n  panic as \"never called\"\n}\n\npub fn call() -> Int {\n  let holder = empty([])\n  let moved = Holder { ..holder, item.x }\n  let typed: Holder<Point> = moved\n  typed.item.y\n}\n";
    let util = UTIL.replace("fn foo", "fn ‸x");
    assert_eq!(
        refused(
            &[
                ("main.cheby", main),
                ("caller.cheby", caller),
                ("util.cheby", &util)
            ],
            "left",
        ),
        "cannot tell what `.x` at `src/caller.cheby:18` refers to"
    );
}

/// D-1391: renaming an interface function renames the functions that
/// satisfy it, through an interface that embeds it too (D-1552).
#[test]
fn an_interface_function_renames_its_satisfiers() {
    let text = "interface Named {\n  fn ‸name(Self) -> String\n}\n\ninterface Titled: Named {\n  fn title(Self) -> String\n}\n\ntype Book { Book(String) }\n\nfn name(b: Book) -> String {\n  let Book(text) = b\n  text\n}\n\nfn title(b: Book) -> String {\n  let n = name(b)\n  \"The {n}\"\n}\n\nfn main() {\n  assert Titled::title(Book(\"a\")) == \"The a\" && Named::name(Book(\"b\")) == \"b\"\n}\n";
    assert_eq!(
        renamed(text, "label"),
        text.replace(CURSOR, "")
            .replace("fn name(", "fn label(")
            .replace("= name(b)", "= label(b)")
            .replace("Named::name(", "Named::label(")
    );
}

/// D-1710, X §3.4: the builtin types whose std `show` satisfies the
/// package's interface by its name do not block the rename. The package's
/// satisfier is renamed with the interface function, and the note names
/// the types that will no longer satisfy the interface. When the package
/// uses such a satisfaction, `verify`'s E0063 refuses the rename.
#[test]
fn a_satisfier_outside_the_package_gets_a_note() {
    let text = "interface Pretty {\n  fn ‸show(Self) -> String\n}\n\ntype Point { Point(Int) }\n\nfn show(p: Point) -> String {\n  let Point(n) = p\n  \"p{n}\"\n}\n\nfn main() {\n  assert Pretty::show(Point(1)) == \"p1\"\n}\n";
    let (case, plan) = plan(&[("main.cheby", text)], "render");
    assert_eq!(
        case.applied(&plan),
        vec![(
            "main.cheby".to_owned(),
            text.replace(CURSOR, "")
                .replace("fn show(", "fn render(")
                .replace("Pretty::show(", "Pretty::render(")
        )]
    );
    let note = plan.note.expect("a note");
    let outside = note.rsplit("; ").next().unwrap_or_default();
    assert!(
        outside.ends_with(" will no longer satisfy `Pretty`")
            && outside.contains("`Int`")
            && outside.contains(" and ")
            && !outside.contains("<builtin>"),
        "{note}"
    );
    let used = text.replace(
        "Pretty::show(Point(1)) == \"p1\"",
        "Pretty::show(Point(1)) == \"p1\" && Pretty::show(1) == \"1\"",
    );
    let message = refused(&[("main.cheby", &used)], "render");
    assert!(
        message.starts_with("Renaming would cause: error[E0063]: "),
        "{message}"
    );
}

/// D-1717: a parameter of a function with no body, an `@external` one,
/// is renamed as its one occurrence, in the signature.
#[test]
fn a_parameter_of_a_function_with_no_body_is_renamed_alone() {
    let text = "@external(native, \"ext_write\")\n@external(js, \"./ext.mjs\", \"write\")\nfn write(‸entry: String, more: Int)\n\nfn main() {\n  write(\"entry\", 1)\n}\n";
    let case = Case::new(&[("main.cheby", text)]);
    let (range, placeholder) = case.prepare().expect("accepted").expect("a name");
    assert_eq!(placeholder, "entry");
    assert_eq!(
        range.slice(&case.files[0].1),
        Some("entry"),
        "the parameter's token"
    );
    let plan = match case.rename("line") {
        Ok(Some(plan)) => plan,
        other => panic!("expected a plan, got {other:?}"),
    };
    assert_eq!(plan.note, None);
    assert_eq!(
        case.applied(&plan),
        vec![(
            "main.cheby".to_owned(),
            text.replace(CURSOR, "")
                .replace("write(entry:", "write(line:")
        )]
    );
    // The new name is checked as any parameter's.
    assert!(matches!(
        case.rename("Line"),
        Err(RenameError::Refused(Refusal(_)))
    ));
    // The compiler checks no parameter name of a function with no body,
    // so the name of another parameter is refused here (D-1722).
    assert_eq!(
        case.rename("more"),
        Err(RenameError::Refused(Refusal(
            "`more` is already a parameter of `write`".to_owned()
        )))
    );
}

/// D-1718: `import m::{Id}` names a type and its constructor of the same
/// name with one token, so renaming either is refused up front, saying
/// so, from `prepareRename` on.
#[test]
fn a_shared_import_of_a_type_and_its_constructor_is_refused() {
    let shapes = "pub type ‸Id {\n  Id(Int),\n}\n";
    let main = "import app::shapes::{Id}\n\nfn main() {\n  let x: Id = Id(1)\n  let Id(n) = x\n  assert n == 1\n}\n";
    let files = [("shapes.cheby", shapes), ("main.cheby", main)];
    let expected =
        "the import's `Id` at `src/main.cheby:1` names both the type and its constructor";
    match Case::new(&files).prepare() {
        Err(Refusal(message)) => assert_eq!(message, expected),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(refused(&files, "Key"), expected);
    // The constructor, from its own name, is refused the same way.
    let ctor = [
        ("shapes.cheby", "pub type Id {\n  ‸Id(Int),\n}\n"),
        ("main.cheby", main),
    ];
    assert_eq!(refused(&ctor, "Key"), expected);
}

/// D-1723: one import token that names a type and a constructor of another
/// type is refused up front too, from either name, and the text names
/// both.
#[test]
fn a_shared_import_of_a_type_and_another_type_s_constructor_is_refused() {
    let shapes = "pub type ‸Shape {\n  Circle,\n}\n\npub type Other {\n  Shape,\n}\n";
    let main = "import app::shapes::{Other, Shape}\n\nfn pick(s: Shape) -> Other {\n  case s {\n    Circle => Shape\n  }\n}\n\nfn main() {\n  let _ = pick(Circle)\n}\n";
    let main = main.replace("{Other, Shape}", "{Circle, Other, Shape}");
    let files = [("shapes.cheby", shapes), ("main.cheby", main.as_str())];
    let expected = "the import's `Shape` at `src/main.cheby:1` names both the type `Shape` and the constructor `Shape` of `Other`";
    match Case::new(&files).prepare() {
        Err(Refusal(message)) => assert_eq!(message, expected),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(refused(&files, "Figure"), expected);
    let ctor = [
        (
            "shapes.cheby",
            "pub type Shape {\n  Circle,\n}\n\npub type Other {\n  ‸Shape,\n}\n",
        ),
        ("main.cheby", main.as_str()),
    ];
    assert_eq!(refused(&ctor, "Figure"), expected);
    // A type alias or an interface of the constructor's name, the same way.
    for (decl, kind) in [
        ("pub type Shape = (Int, Int)\n", "type alias"),
        (
            "pub interface Shape {\n  fn area(Self) -> Int\n}\n",
            "interface",
        ),
    ] {
        let shapes = format!("{decl}\npub type Other {{\n  Shape,\n  Blob,\n}}\n");
        let main = "import app::shapes::{Blob, Other, Shape}\n\nfn pick(b: Bool) -> Other {\n  case b {\n    true => Shape\n    false => Blob\n  }\n}\n\nfn main() {\n  let _ = pick(true)\n}\n";
        let expected = format!(
            "the import's `Shape` at `src/main.cheby:1` names both the {kind} `Shape` and the constructor `Shape` of `Other`"
        );
        for marked in [
            shapes.replacen("Shape", "‸Shape", 1),
            shapes.replacen("  Shape", "  ‸Shape", 1),
        ] {
            let files = [("shapes.cheby", marked.as_str()), ("main.cheby", main)];
            match Case::new(&files).prepare() {
                Err(Refusal(message)) => assert_eq!(message, expected),
                other => panic!("expected a refusal, got {other:?}"),
            }
            assert_eq!(refused(&files, "Figure"), expected);
        }
    }
}

/// D-1391: a satisfier that also satisfies another interface by its name
/// follows the interface function, and the note says what its type loses.
#[test]
fn a_satisfier_that_satisfies_another_interface_gets_a_note() {
    let text = "interface Named {\n  fn ‸name(Self) -> String\n}\n\ninterface Labeled {\n  fn name(Self) -> String\n}\n\ntype Book { Book(String) }\n\nfn name(b: Book) -> String {\n  let Book(text) = b\n  text\n}\n\nfn main() {\n  assert Named::name(Book(\"b\")) == \"b\"\n}\n";
    let (case, plan) = plan(&[("main.cheby", text)], "title");
    assert_eq!(
        case.applied(&plan),
        vec![(
            "main.cheby".to_owned(),
            text.replace(CURSOR, "")
                .replacen("fn name(Self)", "fn title(Self)", 1)
                .replace("fn name(b", "fn title(b")
                .replace("Named::name(", "Named::title(")
        )]
    );
    assert_eq!(
        plan.note.as_deref(),
        Some("`Book` will no longer satisfy `Labeled`")
    );
}

const AREA: &str = "interface Area {\n  fn area(Self) -> Int\n}\n\ntype Square { Square(Int) }\n\nfn ‸area(s: Square) -> Int {\n  let Square(side) = s\n  side * side\n}\n\n";

/// D-1391: a function whose satisfaction the package uses is refused by
/// `verify`'s E0063, which names the interface; one whose satisfaction
/// nothing uses gets a note.
#[test]
fn a_satisfying_function_is_refused_when_used_and_noted_when_not() {
    let used = format!(
        "{AREA}fn total<T: Area>(x: T) -> Int {{\n  Area::area(x)\n}}\n\nfn main() {{\n  assert total(Square(2)) == 4\n}}\n"
    );
    let message = refused(&[("main.cheby", &used)], "size");
    assert!(
        message.starts_with("Renaming would cause: error[E0063]: ") && message.contains("Area"),
        "{message}"
    );
    let unused = format!("{AREA}fn main() {{\n  assert area(Square(2)) == 4\n}}\n");
    let (_, plan) = plan(&[("main.cheby", &unused)], "size");
    assert_eq!(
        plan.note.as_deref(),
        Some("`Square` will no longer satisfy `Area`")
    );
    assert_eq!(plan.edits.len(), 2);
}

const ADD: &str = "type V { V(Int) }\n\nfn ‸add(a: V, b: V) -> V {\n  let V(x) = a\n  let V(y) = b\n  V(x + y)\n}\n\n";

/// D-1391: an operator's function is found by name and signature; a used
/// one is refused, an unused one noted.
#[test]
fn an_operator_function_is_refused_when_used_and_noted_when_not() {
    let unused = format!("{ADD}fn main() {{\n  assert add(V(1), V(2)) == V(3)\n}}\n");
    let (_, plan) = plan(&[("main.cheby", &unused)], "plus");
    let note = plan.note.expect("a note");
    assert!(note.contains("`V`"), "{note}");
    let used = format!("{ADD}fn main() {{\n  assert V(1) + V(2) == V(3)\n}}\n");
    let message = refused(&[("main.cheby", &used)], "plus");
    assert!(
        message.starts_with("Renaming would cause: error["),
        "{message}"
    );
}

/// D-1391: `main` takes the note of the entry point.
#[test]
fn main_gets_a_note() {
    let text = "fn ‸main() {\n  assert 1 == 1\n}\n";
    let (_, plan) = plan(&[("main.cheby", text)], "start");
    assert_eq!(
        plan.note.as_deref(),
        Some("the package's entry point is `main`")
    );
}

/// D-1389, D-1671: a rename that adds an error is refused with its header
/// and its place in the edited text.
#[test]
fn a_new_error_is_refused_with_its_header() {
    let text = "fn helper() -> Int {\n  1\n}\n\nfn main() {\n  let ‸x = 2\n  assert x + helper() == 3\n}\n";
    let message = refused(&[("main.cheby", text)], "helper");
    assert!(
        message.starts_with("Renaming would cause: error[E0044]: "),
        "{message}"
    );
    assert!(
        message.ends_with(" at src/main.cheby:6:7 and 1 more"),
        "{message}"
    );
}

/// D-1671: the refusal names the first new error, then counts the rest.
/// A move's refusal says "Moving" (D-1724), and a fix's "Applying the fix"
/// (D-1731).
#[test]
fn would_cause_counts_the_rest() {
    let diag = |message: &str| PlainDiagnostic {
        code: codes::SHADOWED_NAME,
        severity: Severity::Error,
        message: message.to_owned(),
        range: None,
        primary_message: String::new(),
        related: Vec::new(),
        notes: Vec::new(),
        helps: Vec::new(),
    };
    let at = |place: &str, message: &str| Introduced {
        at: place.to_owned(),
        path: PathBuf::from("src/main.cheby"),
        diagnostic: diag(message),
    };
    // One with no span is placed at its file (D-1709).
    assert_eq!(
        would_cause(
            &[at("src/main.cheby", "`a` is already bound")],
            Action::Rename
        ),
        "Renaming would cause: error[E0044]: `a` is already bound at src/main.cheby"
    );
    let three = [
        at("src/main.cheby:2:7", "`a` is already bound"),
        at("src/main.cheby:3:7", "b"),
        at("cheby.toml", "c"),
    ];
    assert_eq!(
        would_cause(&three, Action::Rename),
        "Renaming would cause: error[E0044]: `a` is already bound at src/main.cheby:2:7 and 2 more"
    );
    assert_eq!(
        would_cause(&three, Action::Move),
        "Moving would cause: error[E0044]: `a` is already bound at src/main.cheby:2:7 and 2 more"
    );
    // A cross-file fix's refusal names its own action (D-1731).
    assert_eq!(
        would_cause(&three, Action::Fix),
        "Applying the fix would cause: error[E0044]: `a` is already bound at src/main.cheby:2:7 \
         and 2 more"
    );
}

/// D-1668, D-1698: a rename that would bind a name used unresolved is
/// refused, for a local here.
#[test]
fn a_rename_that_hides_an_error_is_refused() {
    let text = "fn main() {\n  let ‸x = 1\n  assert x == 1 && y == 1\n}\n";
    let message = refused(&[("main.cheby", text)], "y");
    assert_eq!(
        message,
        "Renaming would hide an error: error[E0042]: cannot find value `y` in this scope \
         at src/main.cheby:3:20"
    );
}

/// D-1698: the refusal quotes the error that goes away as the user sees
/// it, naming the old name, never as the rename would read it.
#[test]
fn a_hidden_error_is_quoted_as_it_was() {
    let text = "fn main() {\n  let total = 1\n  let ‸total = 2\n  assert total == 2\n}\n";
    assert_eq!(
        refused(&[("main.cheby", text)], "sum"),
        "Renaming would hide an error: error[E0044]: `total` is already bound \
         at src/main.cheby:3:7"
    );
}

/// D-1389, plan §1.7: a closed file that changed on disk refuses with
/// `Stale`, so the handler can ask for a sweep.
#[test]
fn a_file_changed_on_disk_is_stale() {
    let text = "fn main() {\n  let ‸x = 1\n  assert x == 1\n}\n";
    let case = Case::new(&[("main.cheby", text), ("util.cheby", UTIL)]);
    let util = case.temp.src("util.cheby");
    std::fs::write(&util, "//! Changed.\n").unwrap();
    assert_eq!(
        case.rename("y"),
        Err(RenameError::Stale {
            refusal: Refusal("`src/util.cheby` changed on disk, try again".to_owned()),
            path: util,
        })
    );
}

/// `verify`'s `Blocked` is a refusal with its reason: here an import
/// cycle, which a local rename meets though its own file is clean.
#[test]
fn a_cycle_blocks_even_a_local() {
    let main = "import app::a\n\nfn main() {\n  let ‸x = a::f()\n  assert x == 1\n}\n";
    let a = "//! A.\n\nimport app::b\n\npub fn f() -> Int {\n  b::g()\n}\n";
    let b = "//! B.\n\nimport app::a\n\npub fn g() -> Int {\n  a::f()\n}\n";
    let message = refused(&[("main.cheby", main), ("a.cheby", a), ("b.cheby", b)], "y");
    assert!(message.contains("cycle"), "{message}");
}

/// The writer alone, with no check, as W11's E0044 fix uses it (D-1668):
/// every written occurrence of a local, the pun included.
#[test]
fn the_writer_alone_renames_a_local() {
    let case = Case::new(&[("main.cheby", &book_at("fn make(title", 0, 8))]);
    let index = case.analyzed.index();
    let module = index.of_file(case.file).unwrap();
    let symbol = module
        .symbols
        .iter()
        .find(|e| matches!(&e.symbol.member, Member::Local { name, .. } if &**name == "title"))
        .map(|e| e.symbol.clone())
        .unwrap();
    // `t` is taken: the writer does not check.
    let edits = write(index, &[symbol], "t", &CancelToken::new()).unwrap();
    let plan = RenamePlan { edits, note: None };
    assert_eq!(
        case.applied(&plan),
        vec![(
            "main.cheby".to_owned(),
            BOOK.replace("fn make(title: String", "fn make(t: String")
                .replace("Book { title, pages: 1 }", "Book { title: t, pages: 1 }")
                .replace("Book { ..first, title }", "Book { ..first, title: t }")
        )]
    );
}

/// D-1390: a local needs every name of its text in its module placed.
#[test]
fn an_unresolved_name_of_a_local_s_text_refuses_it() {
    let text = "fn main() {\n  let ‸x = 1\n  assert x == 1\n}\n\nfn other() -> Int {\n  x\n}\n";
    assert_eq!(
        refused(&[("main.cheby", text)], "y"),
        "`src/main.cheby:7` may refer to `x` but does not resolve"
    );
}

/// D-1390, plan §1.12: an item needs no import through a cut cycle.
#[test]
fn a_cut_cycle_refuses_an_item() {
    let main = "import app::a\n\nfn main() {\n  assert a::f() == 1\n}\n";
    let a = "//! A.\n\nimport app::b\n\npub fn ‸f() -> Int {\n  b::g()\n}\n";
    let b = "//! B.\n\nimport app::a\n\npub fn g() -> Int {\n  a::f()\n}\n";
    let message = refused(&[("main.cheby", main), ("a.cheby", a), ("b.cheby", b)], "h");
    assert!(message.ends_with(" imports through a cycle"), "{message}");
}

/// X §3.5: in `m::I::f` only the segment that names the renamed function
/// changes, and the satisfier in the type's module follows (D-1391).
#[test]
fn renaming_an_interface_function_through_a_qualified_path() {
    let shape = "//! Shapes.\n\npub interface Measure {\n  fn area(Self) -> Int\n}\n\npub type Sq { Sq(Int) }\n\npub fn area(s: Sq) -> Int {\n  let Sq(n) = s\n  n\n}\n";
    let main =
        "import app::shape\n\nfn main() {\n  assert shape::Measure::‸area(shape::Sq(2)) == 2\n}\n";
    let (case, plan) = plan(&[("main.cheby", main), ("shape.cheby", shape)], "extent");
    assert_eq!(
        case.applied(&plan),
        vec![
            (
                "main.cheby".to_owned(),
                main.replace(CURSOR, "").replace("::area(", "::extent(")
            ),
            (
                "shape.cheby".to_owned(),
                shape
                    .replace("fn area(Self)", "fn extent(Self)")
                    .replace("fn area(s", "fn extent(s")
            ),
        ]
    );
}

/// D-111: an import alias of a record type, `Point as P`, names the type
/// and its constructor with one token, and `Id as Key` names the type `Id`
/// and its constructor `Id`. The alias is a symbol in each namespace, and
/// renaming it from any of its tokens writes the uses of both.
#[test]
fn renaming_an_import_alias_of_a_type_writes_both_namespaces() {
    let shapes = "pub type Point {\n  x: Int,\n}\n\npub type Id {\n  Id(Int),\n}\n";
    let record = "import app::shapes::{Point as P}\n\nfn main() {\n  let p: ‸P = P { x: 1 }\n  assert p.x == 1\n}\n";
    let (case, renaming) = plan(&[("main.cheby", record), ("shapes.cheby", shapes)], "Q");
    assert_eq!(
        case.applied(&renaming),
        vec![(
            "main.cheby".to_owned(),
            record
                .replace(CURSOR, "")
                .replace("as P}", "as Q}")
                .replace("p: P = P {", "p: Q = Q {")
        )]
    );
    let variant = "import app::shapes::{Id as Key}\n\nfn main() {\n  let k: Key = Key(1)\n  let Key(n) = k\n  assert n == 1\n}\n";
    for at in ["as Key", "k: Key", "= Key(", "let Key("] {
        let marked = variant.replacen(at, &at.replace("Key", "‸Key"), 1);
        let (case, renaming) = plan(
            &[("main.cheby", marked.as_str()), ("shapes.cheby", shapes)],
            "Name",
        );
        assert_eq!(
            case.applied(&renaming),
            vec![("main.cheby".to_owned(), variant.replace("Key", "Name"))],
            "from {at}"
        );
    }
}

/// D-1709, D-118: a function that serves an operator through no interface
/// gets the operator's note. It serves one as the checker matches it: a
/// type parameter bound by the first parameter can make the parameters of
/// two written types one operand type. A `compare` that does not return
/// `Order` serves no operator, so it gets no note.
#[test]
fn an_operator_note_names_the_operator_only_where_it_applies() {
    let cell = "type Cell<T> { Cell(T) }\n\nfn ‸add(a: Cell<Int>, b: Cell<Int>) -> Cell<Int> {\n  let Cell(x) = a\n  let Cell(y) = b\n  Cell(x + y)\n}\n\nfn main() {\n  let Cell(n) = add(Cell(1), Cell(2))\n  assert n == 3\n}\n";
    let (_, added) = plan(&[("main.cheby", cell)], "plus");
    assert_eq!(
        added.note.as_deref(),
        Some("`+` will no longer apply to `Cell`")
    );
    let compare = "type T { T(Int) }\n\nfn ‸compare(a: T, b: T) -> Int {\n  let T(x) = a\n  let T(y) = b\n  x - y\n}\n\nfn main() {\n  assert compare(T(2), T(1)) == 1\n}\n";
    let (_, compared) = plan(&[("main.cheby", compare)], "order");
    assert_eq!(compared.note, None);
    // `+` on `Cell<Int>`, which the checker accepts.
    let mixed = "type Cell<T> { Cell(T) }\n\nfn ‸add<T>(a: Cell<T>, b: Cell<Int>) -> Cell<T> {\n  let _ = b\n  a\n}\n\nfn main() {\n  let Cell(n) = add(Cell(1), Cell(2))\n  assert n == 1\n}\n";
    let (_, added) = plan(&[("main.cheby", mixed)], "plus");
    assert_eq!(
        added.note.as_deref(),
        Some("`+` will no longer apply to `Cell`")
    );
    // A bare type parameter takes every type of the module, each of which
    // then satisfies `Add`, so the satisfaction's note stands for the
    // operator's.
    let bare = "type A { A(Int) }\n\ntype B { B(Int) }\n\nfn ‸add<T>(a: T, b: T) -> T {\n  let _ = b\n  a\n}\n\nfn main() {\n  let A(n) = add(A(1), A(2))\n  let B(m) = add(B(3), B(4))\n  assert n + m == 4\n}\n";
    let (_, added) = plan(&[("main.cheby", bare)], "plus");
    assert_eq!(
        added.note.as_deref(),
        Some("`A` will no longer satisfy `Add`; `B` will no longer satisfy `Add`")
    );
    // A bounded type parameter takes only the types that meet its bound,
    // and each of those satisfies `Add`: `V` does not meet `Tagged`,
    // so `+` never applied to it and it gets no note.
    let bounded = "interface Tagged {\n  fn tag(Self) -> Int\n}\n\ntype V { V(Int) }\n\ntype W { W(Int) }\n\nfn tag(w: W) -> Int {\n  let W(n) = w\n  n\n}\n\nfn ‸add<T: Tagged>(a: T, b: T) -> T {\n  let _ = b\n  a\n}\n\nfn main() {\n  let V(v) = V(0)\n  assert tag(add(W(1), W(2))) == 1 + v\n}\n";
    let (_, added) = plan(&[("main.cheby", bounded)], "plus");
    assert_eq!(
        added.note.as_deref(),
        Some("`W` will no longer satisfy `Add`")
    );
    // A type parameter that the first parameter does not bind serves none
    // (D-504).
    let unbound = "type Cell<T> { Cell(T) }\n\nfn ‸add<T, U>(a: Cell<T>, b: Cell<U>) -> Cell<T> {\n  let _ = b\n  a\n}\n\nfn main() {\n  let Cell(n) = add(Cell(1), Cell(2))\n  assert n == 1\n}\n";
    let (_, added) = plan(&[("main.cheby", unbound)], "plus");
    assert_eq!(added.note, None);
}

/// Two items of one import list that share an alias, one in each
/// namespace, `{Circle as X, Shape as X}`: renaming the alias from any of
/// its tokens writes both. Two import statements with one alias each stay
/// apart.
#[test]
fn two_items_that_share_an_alias_rename_together() {
    let b = "pub type Shape {\n  Circle(Int),\n  Dot,\n}\n";
    let main = "import app::b::{Circle as X, Dot, Shape as X}\n\nfn radius(s: X) -> Int {\n  case s {\n    X(r) => r\n    Dot => 0\n  }\n}\n\nfn main() {\n  let s: X = X(1)\n  assert radius(s) == 1\n}\n";
    for at in [
        "Circle as X",
        "Shape as X",
        "s: X)",
        "X(r)",
        "s: X =",
        "= X(1)",
    ] {
        let marked = main.replacen(at, &at.replacen('X', "‸X", 1), 1);
        let (case, renaming) = plan(&[("main.cheby", marked.as_str()), ("b.cheby", b)], "Y");
        assert_eq!(
            case.applied(&renaming),
            vec![("main.cheby".to_owned(), main.replace('X', "Y"))],
            "from {at}"
        );
    }
    let a = "pub type Tag {\n  Red,\n}\n";
    let apart = "import app::a\nimport app::a::{Tag as X}\nimport app::b\nimport app::b::{Circle as X, Dot}\n\nfn radius(s: b::Shape) -> Int {\n  case s {\n    X(r) => r\n    Dot => 0\n  }\n}\n\nfn main() {\n  let t: ‸X = a::Red\n  let _ = t\n  assert radius(X(1)) == 1\n}\n";
    // The item names of such a list each rename their own item, the type
    // or the constructor, and keep the alias.
    for (at, new) in [("Circle as", "Round"), ("Shape as", "Figure")] {
        for list in [
            "{Circle as X, Dot, Shape as X}",
            "{Shape as X, Dot, Circle as X}",
        ] {
            let text = main.replace("{Circle as X, Dot, Shape as X}", list);
            let marked = text.replacen(at, &format!("‸{at}"), 1);
            let (case, renaming) = plan(&[("main.cheby", marked.as_str()), ("b.cheby", b)], new);
            let old = at.trim_end_matches(" as");
            assert_eq!(
                case.applied(&renaming),
                vec![
                    ("main.cheby".to_owned(), text.replace(old, new)),
                    ("b.cheby".to_owned(), b.replace(old, new)),
                ],
                "from {at} in {list}"
            );
        }
    }
    let (case, renaming) = plan(
        &[("main.cheby", apart), ("a.cheby", a), ("b.cheby", b)],
        "Y",
    );
    assert_eq!(
        case.applied(&renaming),
        vec![(
            "main.cheby".to_owned(),
            apart
                .replace(CURSOR, "")
                .replace("Tag as X", "Tag as Y")
                .replace("t: X", "t: Y")
        )]
    );
}

/// A name in an attribute's arguments. Outside std, `@std_hooks` is an
/// unknown attribute (E0045), so its names bind nothing and there is no
/// name to rename.
#[test]
fn a_name_in_an_unknown_attribute_is_no_rename_target() {
    let text = "@std_hooks(‸same)\npub type Pair {\n  Pair(Int, Int),\n}\n\npub fn same(a: Pair, b: Pair) -> Bool {\n  a == b\n}\n\nfn main() {\n  assert same(Pair(1, 2), Pair(1, 2))\n}\n";
    let case = Case::new(&[("main.cheby", text)]);
    assert_eq!(case.prepare(), Ok(None));
}

/// The rename tests in a std checkout's session (D-1706): the repository's
/// `std/` copied beside D-1699's marker, with probe modules of the tests'
/// own, as `ide::checkout_tests` makes it. Each runs only the front end,
/// so it runs on Windows too (D-1516, ws-plan §9).
mod checkout {
    use std::path::{Path, PathBuf};

    use cheby_driver::analysis::{DefId, DefKind, FileId, TextRange, TextSize};

    use super::super::{RenameError, RenamePlan, prepare, rename, satisfied};
    use super::CURSOR;
    use crate::features::ide::Refusal;
    use crate::features::ide::checkout_tests::Checkout;
    use crate::features::{Analyzed, CancelToken};

    /// Where std's API also lives, as D-1706's refusals end.
    const API: &str = "std's API also lives in `docs/stdlib/`, the examples and the tests";

    /// A probe with std's API and a satisfaction nothing uses.
    const PROBE_API: &str = "//! A probe of the rename tests: std's API and a satisfaction.

import std::int

/// A text for a value.
interface Shown {
  fn shown(Self) -> String
}

/// A length.
type Meter {
  Meter(Int),
}

/// A length as text, which makes `Meter` satisfy `Shown`.
fn shown(m: Meter) -> String {
  let Meter(n) = m
  \"{n} m\"
}

/// A point.
pub type Pt {
  px: Int,
}

/// A mark.
pub type Mark {
  Mark(Int),
}

/// The origin.
pub fn origin() -> Pt {
  Pt { px: 0 }
}
";

    /// A probe whose type has hooks that are `priv` functions, and a
    /// function with a body whose parameter `@std_clamped` names.
    /// A probe of the rename tests: names that `@std_call_site` and
    /// `@std_initializer` give (D-1706).
    const PROBE_SITES: &str = "//! A probe of the rename tests: a call site and an initializer.

import std::context

/// Returns `x`, through the function its call site names.
@std_call_site(echo_at)
pub fn echo(x: Int) -> Int {
  let _ = x
  panic as \"the compiler did not replace this call\"
}

/// What a call of `echo` does.
fn echo_at(x: Int, path: String, line: Int, column: Int) -> Int {
  let _ = (path, line, column)
  x
}

/// Returns a key, through the function its initializer names.
@std_initializer(cell_at)
pub fn cell<T>(default: T) -> context::Key<T> {
  let _ = default
  panic as \"the compiler did not replace this call\"
}

/// What a call of `cell` gives.
fn cell_at<T>(default: T, path: String, type_text: String) -> context::Key<T> {
  context::key_at(default, path, type_text)
}
";

    const PROBE_HOOKS: &str = "//! A probe of the rename tests: hooks and a clamped parameter.

import std::int

/// A gauge, compared, hashed and printed through its hooks.
@std_hooks(gauge_equal, gauge_hash, gauge_debug)
type Gauge {
  Gauge(Int),
}

priv fn gauge_equal(a: Gauge, b: Gauge) -> Bool {
  let Gauge(x) = a
  let Gauge(y) = b
  x == y
}

priv fn gauge_hash(g: Gauge) -> Int {
  let Gauge(x) = g
  x
}

priv fn gauge_debug(g: Gauge) -> String {
  let Gauge(x) = g
  \"g{x}\"
}

/// `count` plus one, at least one.
@std_clamped(count)
fn bumped(count: Int) -> Int {
  let next = count + 1
  next
}

test \"gauges\" {
  assert Gauge(1) == Gauge(1) && bumped(1) == 2
}
";

    /// A probe with a package-visible record type.
    const PROBE_CELL: &str = "//! A probe of the rename tests: a record type.

/// A cell with a weight.
type Cell {
  probe_weight: Int,
}

/// A cell of weight `n`.
fn cell(n: Int) -> Cell {
  Cell { probe_weight: n }
}
";

    /// A probe whose test body reads the field of `PROBE_CELL`'s type.
    const PROBE_READER: &str = "//! A probe of the rename tests: a test that reads a field.

import std::probe_cell

test \"weight\" {
  let c = probe_cell::cell(1)
  assert c.probe_weight == 1
}
";

    /// A probe with a function named as `PROBE_CELL`'s field.
    const PROBE_WEIGHER: &str = "//! A probe of the rename tests: a function named as a field.

/// A weight.
fn probe_weight() -> Int {
  1
}

/// Twice the weight.
fn doubled_weight() -> Int {
  probe_weight() * 2
}
";

    /// A checkout with `probes` on disk, beside `ide::checkout_tests`'s
    /// two, and the files of `open` open with their disk text.
    struct Case {
        checkout: Checkout,
        analyzed: Analyzed,
    }

    impl Case {
        fn new(name: &str, probes: &[(&str, &str)], open: &[&str]) -> Self {
            let checkout = Checkout::new(name);
            for (display, text) in probes {
                std::fs::write(checkout.at(display), text).unwrap();
            }
            let texts: Vec<(&str, String)> = open
                .iter()
                .map(|display| (*display, checkout.read(display)))
                .collect();
            let open: Vec<(&str, &str)> = texts.iter().map(|(d, t)| (*d, t.as_str())).collect();
            let analyzed = checkout.analyze(&open);
            Self { checkout, analyzed }
        }

        /// The file at `display` and the offset of the cursor in `needle`,
        /// which occurs once in the file's text with the cursor taken out.
        #[track_caller]
        fn point(&self, display: &str, needle: &str) -> (FileId, TextSize) {
            let text = self.checkout.read(display);
            let bar = needle.find(CURSOR).expect("a cursor");
            let plain = needle.replace(CURSOR, "");
            assert_eq!(text.matches(&plain).count(), 1, "`{plain}` occurs once");
            let at = text.find(&plain).unwrap().checked_add(bar).unwrap();
            let file = self
                .analyzed
                .snapshot
                .file_of(&self.checkout.at(display))
                .expect("a file of the session");
            (file, TextSize::try_from(at).unwrap())
        }

        fn prepare(
            &self,
            display: &str,
            needle: &str,
        ) -> Result<Option<(TextRange, String)>, Refusal> {
            let (file, offset) = self.point(display, needle);
            prepare(&self.analyzed, file, offset)
        }

        fn rename(
            &self,
            display: &str,
            needle: &str,
            new: &str,
        ) -> Result<Option<RenamePlan>, RenameError> {
            let (file, offset) = self.point(display, needle);
            rename(&self.analyzed, file, offset, new, &CancelToken::new())
        }

        /// The plan of a rename that must be accepted.
        #[track_caller]
        fn plan(&self, display: &str, needle: &str, new: &str) -> RenamePlan {
            match self.rename(display, needle, new) {
                Ok(Some(plan)) => plan,
                other => panic!("expected a plan, got {other:?}"),
            }
        }

        /// The message of a name that `prepareRename` and `rename` both
        /// refuse, which must be the same.
        #[track_caller]
        fn refused(&self, display: &str, needle: &str, new: &str) -> String {
            let at_prepare = match self.prepare(display, needle) {
                Err(Refusal(message)) => message,
                other => panic!("expected prepare to refuse, got {other:?}"),
            };
            match self.rename(display, needle, new) {
                Err(RenameError::Refused(Refusal(message))) => {
                    assert_eq!(message, at_prepare);
                    message
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
        }

        /// The message of a rename that must be refused, whatever
        /// `prepareRename` says.
        #[track_caller]
        fn rename_refused(&self, display: &str, needle: &str, new: &str) -> String {
            match self.rename(display, needle, new) {
                Err(RenameError::Refused(Refusal(message))) => message,
                other => panic!("expected a refusal, got {other:?}"),
            }
        }

        /// The checkout's directory.
        fn root(&self) -> PathBuf {
            self.checkout.at("std").parent().unwrap().to_path_buf()
        }

        /// Each file `plan` edits, by display path, with its disk text and
        /// the edits applied, sorted by path.
        fn applied(&self, plan: &RenamePlan) -> Vec<(String, String)> {
            let root = self.root();
            let mut paths: Vec<&Path> = plan.edits.iter().map(|e| e.path.as_path()).collect();
            paths.dedup();
            paths
                .into_iter()
                .map(|path| {
                    let display = path
                        .strip_prefix(&root)
                        .unwrap()
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/");
                    let mut text = std::fs::read_to_string(path).unwrap();
                    let mut edits: Vec<_> = plan.edits.iter().filter(|e| e.path == path).collect();
                    edits.sort_by_key(|e| std::cmp::Reverse(e.range.start()));
                    for edit in edits {
                        let start = usize::try_from(edit.range.start()).unwrap();
                        let end = usize::try_from(edit.range.end()).unwrap();
                        text.replace_range(start..end, &edit.replacement);
                    }
                    (display, text)
                })
                .collect()
        }
    }

    /// D-1706: std's API is refused at `prepareRename` and at `rename`, with
    /// the message that it also lives in `docs/stdlib/`, the examples and
    /// the tests: a `pub` item, a constructor and a field of a `pub` type,
    /// and an interface function, `pub` or not.
    #[test]
    fn checkout_refuses_std_api() {
        let case = Case::new(
            "rename_api",
            &[("std/probe_api.cheby", PROBE_API)],
            &["std/probe_api.cheby", "std/list.cheby"],
        );
        let api = "std/probe_api.cheby";
        assert_eq!(
            case.refused("std/list.cheby", "pub fn ‸map<", "transform"),
            format!("`list::map` is `pub`: {API}")
        );
        assert_eq!(
            case.refused(api, "pub fn ‸origin", "zero"),
            format!("`probe_api::origin` is `pub`: {API}")
        );
        assert_eq!(
            case.refused(api, "pub type ‸Pt", "Spot"),
            format!("`probe_api::Pt` is `pub`: {API}")
        );
        assert_eq!(
            case.refused(api, "  ‸Mark(Int)", "Tick"),
            format!("`Mark` is a constructor of the `pub` type `Mark`: {API}")
        );
        assert_eq!(
            case.refused(api, "  ‸px: Int", "x_pos"),
            format!("`px` is a field of the `pub` type `Pt`: {API}")
        );
        assert_eq!(
            case.refused(api, "Pt { ‸px: 0 }", "x_pos"),
            format!("`px` is a field of the `pub` type `Pt`: {API}")
        );
        assert_eq!(
            case.refused(api, "fn ‸shown(Self)", "text"),
            format!("`shown` is a function of the interface `Shown`: {API}")
        );
        // From a use in another module, the refusal is the same.
        assert_eq!(
            case.refused("std/probe_b.cheby", "list::‸map(doubled", "transform"),
            format!("`list::map` is `pub`: {API}")
        );
    }

    /// D-1706: a function that makes a type satisfy an interface is refused,
    /// naming the interface, though nothing in the session uses the
    /// satisfaction (D-1391's note does not apply). The type and the
    /// interface themselves are package-visible, and rename.
    #[test]
    fn checkout_refuses_a_satisfier() {
        let case = Case::new(
            "rename_satisfier",
            &[("std/probe_api.cheby", PROBE_API)],
            &["std/probe_api.cheby"],
        );
        let api = "std/probe_api.cheby";
        assert_eq!(
            case.refused(api, "fn ‸shown(m: Meter)", "text"),
            "`shown` makes `Meter` satisfy `Shown`, which packages outside the checkout may use"
        );
        let plan = case.plan(api, "type ‸Meter", "Metre");
        assert_eq!(
            case.applied(&plan),
            [(
                api.to_owned(),
                PROBE_API
                    .replace("type Meter", "type Metre")
                    .replace("m: Meter", "m: Metre")
            )]
        );
    }

    /// D-1706, D-1700: a package-visible helper renames across two std
    /// modules, a `priv` one in its own, and a parameter and a local in
    /// their function, each checked by `verify` over the checkout.
    #[test]
    fn checkout_renames_helpers_locals_and_parameters() {
        let case = Case::new(
            "rename_helpers",
            &[],
            &["std/probe_a.cheby", "std/probe_b.cheby"],
        );
        let a = case.checkout.read("std/probe_a.cheby");
        let b = case.checkout.read("std/probe_b.cheby");
        let plan = case.plan("std/probe_a.cheby", "fn ‸helper(", "doubler");
        assert_eq!(
            case.applied(&plan),
            [
                (
                    "std/probe_a.cheby".to_owned(),
                    a.replace("helper(", "doubler(")
                ),
                (
                    "std/probe_b.cheby".to_owned(),
                    b.replace("helper(", "doubler(")
                ),
            ]
        );
        let plan = case.plan("std/probe_a.cheby", "priv fn ‸secret", "twice");
        assert_eq!(
            case.applied(&plan),
            [("std/probe_a.cheby".to_owned(), a.replace("secret", "twice"))]
        );
        let plan = case.plan("std/probe_a.cheby", "fn helper(‸xs", "items");
        assert_eq!(
            case.applied(&plan),
            [(
                "std/probe_a.cheby".to_owned(),
                a.replace("(xs: List", "(items: List")
                    .replace("map(xs,", "map(items,")
            )]
        );
        let plan = case.plan("std/probe_b.cheby", "let ‸doubled", "twofold");
        assert_eq!(
            case.applied(&plan),
            [(
                "std/probe_b.cheby".to_owned(),
                b.replace("let doubled", "let twofold")
                    .replace("map(doubled", "map(twofold")
            )]
        );
    }

    /// D-1706: a package-visible item of `std::json::tree`, renamed from
    /// `std::json`, is written in both modules, and nowhere else.
    #[test]
    fn checkout_renames_a_json_tree_item_from_json() {
        let case = Case::new("rename_json", &[], &["std/json.cheby"]);
        let plan = case.plan(
            "std/json.cheby",
            "to_value(document: tree::Tree) -> Value {\n  tree::‸rebuild(",
            "rebuild_with",
        );
        let mut edited: Vec<String> = case.applied(&plan).into_iter().map(|(d, _)| d).collect();
        edited.sort();
        assert_eq!(edited, ["std/json.cheby", "std/json/tree.cheby"]);
        for (display, text) in case.applied(&plan) {
            let before = case.checkout.read(&display);
            // Doc comments keep the old name (D-1672).
            let expected = before
                .replace("fn rebuild<", "fn rebuild_with<")
                .replace("rebuild(", "rebuild_with(");
            assert!(text == expected, "{display} is not as expected");
        }
    }

    /// D-1706 (S-Q13 A): a name in a std-only attribute is an occurrence,
    /// written with the rest and never refused: a `priv` hook of
    /// `@std_hooks`, renamed from its declaration and from the attribute,
    /// and a parameter that `@std_clamped` names on a function with a
    /// body.
    #[test]
    fn checkout_renames_names_in_std_attributes() {
        let probe = "std/probe_hooks.cheby";
        let case = Case::new("rename_attrs", &[(probe, PROBE_HOOKS)], &[probe]);
        let renamed = PROBE_HOOKS.replace("gauge_equal", "gauge_same");
        let plan = case.plan(probe, "priv fn ‸gauge_equal", "gauge_same");
        assert_eq!(case.applied(&plan), [(probe.to_owned(), renamed.clone())]);
        let (range, old) = case
            .prepare(probe, "@std_hooks(‸gauge_equal")
            .unwrap()
            .expect("a name");
        assert_eq!(old, "gauge_equal");
        let (_, at) = case.point(probe, "@std_hooks(‸gauge_equal");
        assert_eq!(range.start(), at);
        let plan = case.plan(probe, "@std_hooks(‸gauge_equal", "gauge_same");
        assert_eq!(case.applied(&plan), [(probe.to_owned(), renamed)]);
        // The doc comment keeps the old name (D-1672).
        let clamped = PROBE_HOOKS
            .replace("(count)", "(amount)")
            .replace("(count: Int)", "(amount: Int)")
            .replace("count + 1", "amount + 1");
        let plan = case.plan(probe, "fn bumped(‸count", "amount");
        assert_eq!(case.applied(&plan), [(probe.to_owned(), clamped.clone())]);
        let plan = case.plan(probe, "@std_clamped(‸count)", "amount");
        assert_eq!(case.applied(&plan), [(probe.to_owned(), clamped)]);
    }

    /// D-1706: the names that `@std_call_site` and `@std_initializer` give
    /// are occurrences, renamed with their function from its declaration
    /// and from the attribute.
    #[test]
    fn checkout_renames_call_site_and_initializer_names() {
        let probe = "std/probe_sites.cheby";
        let case = Case::new("rename_sites", &[(probe, PROBE_SITES)], &[probe]);
        for (old, new, needles) in [
            (
                "echo_at",
                "echo_here",
                ["fn ‸echo_at(", "@std_call_site(‸echo_at)"],
            ),
            (
                "cell_at",
                "cell_here",
                ["fn ‸cell_at<", "@std_initializer(‸cell_at)"],
            ),
        ] {
            let renamed = PROBE_SITES.replace(old, new);
            for needle in needles {
                let plan = case.plan(probe, needle, new);
                assert_eq!(
                    case.applied(&plan),
                    [(probe.to_owned(), renamed.clone())],
                    "{needle}"
                );
            }
        }
    }

    /// D-1706: a parameter that `@std_clamped` names on a function with no
    /// body is a label the index does not record in the attribute, so its
    /// rename is refused, from the parameter and from the attribute.
    #[test]
    fn checkout_refuses_a_clamped_label() {
        let case = Case::new("rename_label", &[], &["std/runtime.cheby"]);
        let refusal = "`count` is named in `@std_clamped` of a function with no body, \
                       where rename cannot edit it";
        let runtime = "std/runtime.cheby";
        assert_eq!(
            case.refused(runtime, "fn set_thread_count(‸count: Int)", "threads"),
            refusal
        );
        assert_eq!(
            case.refused(runtime, "@std_clamped(‸count)", "threads"),
            refusal
        );
    }

    /// D-1706, D-1390, D-1701 (WR-6): a field whose name a closed module's
    /// test body reads has no known receiver there, since that body is not
    /// typed, so its rename is refused with D-1390's message; with that
    /// module open, its test body is typed and the rename writes it.
    #[test]
    fn checkout_field_in_a_closed_test_body_is_refused() {
        let probes = [
            ("std/probe_cell.cheby", PROBE_CELL),
            ("std/probe_reader.cheby", PROBE_READER),
        ];
        let closed = Case::new("rename_field_closed", &probes, &["std/probe_cell.cheby"]);
        assert_eq!(
            closed.rename_refused("std/probe_cell.cheby", "  ‸probe_weight: Int", "heft"),
            "cannot tell what `.probe_weight` at `std/probe_reader.cheby:7` refers to"
        );
        let open = Case::new(
            "rename_field_open",
            &probes,
            &["std/probe_cell.cheby", "std/probe_reader.cheby"],
        );
        let plan = open.plan("std/probe_cell.cheby", "  ‸probe_weight: Int", "heft");
        assert_eq!(
            open.applied(&plan),
            [
                (
                    "std/probe_cell.cheby".to_owned(),
                    PROBE_CELL.replace("probe_weight", "heft")
                ),
                (
                    "std/probe_reader.cheby".to_owned(),
                    PROBE_READER.replace("probe_weight", "heft")
                ),
            ]
        );
    }

    /// D-1727: an item does not count a field position of its name in a
    /// closed module's test body, which is not typed (D-1701) and cannot
    /// name the item (D-061), so it renames, writing neither that body nor
    /// the field; renaming the field stays refused there.
    #[test]
    fn checkout_item_named_as_a_field_in_a_closed_test_body() {
        let weigher = "std/probe_weigher.cheby";
        let probes = [
            ("std/probe_cell.cheby", PROBE_CELL),
            ("std/probe_reader.cheby", PROBE_READER),
            (weigher, PROBE_WEIGHER),
        ];
        let case = Case::new(
            "rename_item_field",
            &probes,
            &[weigher, "std/probe_cell.cheby"],
        );
        let plan = case.plan(weigher, "fn ‸probe_weight()", "heft");
        assert_eq!(
            case.applied(&plan),
            [(
                weigher.to_owned(),
                PROBE_WEIGHER.replace("probe_weight", "heft")
            )]
        );
        assert_eq!(
            case.rename_refused("std/probe_cell.cheby", "  ‸probe_weight: Int", "heft"),
            "cannot tell what `.probe_weight` at `std/probe_reader.cheby:7` refers to"
        );
        // The case the review found in std: test bodies in std that read a
        // field `next` while it is closed, as `std/ordered_map.cheby`'s did.
        let json = Case::new("rename_item_next", &[], &["std/json.cheby"]);
        let plan = json.plan("std/json.cheby", "priv fn ‸next(", "advance");
        let edited: Vec<String> = json.applied(&plan).into_iter().map(|(d, _)| d).collect();
        assert_eq!(edited, ["std/json.cheby"]);
    }

    /// D-1728: when rename cannot read which interfaces a function
    /// satisfies, it refuses with a text of its own. Here the module it
    /// reads from is another snapshot's, so no query can be built; with
    /// its own module it reads the satisfaction.
    #[test]
    fn checkout_unreadable_satisfactions_are_refused() {
        let api = "std/probe_api.cheby";
        let needle = "fn ‸shown(m: Meter)";
        let case = Case::new("rename_unreadable", &[(api, PROBE_API)], &[api]);
        let other = Case::new("rename_unreadable_other", &[(api, PROBE_API)], &[api]);
        let snapshot = &case.analyzed.snapshot;
        let view = snapshot.module_at(case.point(api, needle).0).unwrap();
        let name = snapshot.globals().names.lookup("shown").unwrap();
        let (index, _) = snapshot
            .interfaces()
            .get(view.id())
            .unwrap()
            .items
            .iter_enumerated()
            .find(|(_, item)| item.kind == DefKind::Fn && item.name == name)
            .unwrap();
        let def = DefId::new(view.id(), index);
        let foreign = other
            .analyzed
            .snapshot
            .module_at(other.point(api, needle).0)
            .unwrap();
        assert_eq!(
            satisfied(snapshot, foreign, def).unwrap_err().0,
            "cannot tell which interfaces `shown` satisfies"
        );
        assert!(satisfied(snapshot, view, def).unwrap().is_some());
    }
}
