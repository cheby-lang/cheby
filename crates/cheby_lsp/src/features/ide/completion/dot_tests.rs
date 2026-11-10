//! Completion after `xs.` (X §4.2, D-1394, plan §1.11): fields, the pipe
//! form that replaces only `.partial`, the call form where a pipe would
//! change meaning, `filterText` by client, imports with an alias on a
//! clash (D-1395), and the postfix `case`. Each test runs the front end,
//! so each carries the Windows ignore (D-545, D-641).

use cheby_driver::analysis::{ModuleView, TextRange};

use super::super::items::{Entry, EntryKind, rank};
use super::super::tests::GEO;
use super::super::{Completions, Options, StdAccess, complete};
use crate::features::DocText;
use crate::features::ide::hover::source;
use crate::features::ide::hover::tests::{Temp, analyze};

/// Every option on but auto-import, as the standard client has them.
pub(crate) const ON: Options = Options {
    snippets: true,
    type_driven: true,
    auto_import: false,
    zed: false,
};

/// [`ON`] with auto-import, which every import edit needs (D-1486).
pub(crate) const IMPORTING: Options = Options {
    auto_import: true,
    ..ON
};

/// The completions at the `‸` of `main`, the text of `src/main.cheby` in
/// a package that also holds `geo.cheby` and `files`, with the text after
/// each item's main edit and the edit's range. A `‸` marks the cursor,
/// since `|` is part of `|>`.
pub(crate) fn run(
    main: &str,
    files: &[(&str, &str)],
    options: Options,
    std: StdAccess<'_>,
) -> (Completions, String) {
    let offset = main.find('‸').expect("a cursor");
    let text = main.replacen('‸', "", 1);
    let mut all: Vec<(&str, &str)> = vec![("geo.cheby", GEO)];
    all.extend_from_slice(files);
    all.push(("main.cheby", &text));
    let temp = Temp::package("completion_b", &all);
    let path = temp.src("main.cheby");
    let analyzed = analyze(&temp.0, &[(path.clone(), &text)]);
    let offset = u32::try_from(offset).unwrap();
    let snapshot = &analyzed.snapshot;
    let doc = source(snapshot, &path).expect("the document");
    let view = snapshot.file_of(&path).and_then(|f| snapshot.module_at(f));
    let tree = view.and_then(ModuleView::tree).expect("a tree");
    let found = complete(snapshot, view, tree, doc.text, offset, options, std);
    (found, text)
}

/// [`run`] with `geo.cheby` only.
pub(crate) fn at(main: &str, options: Options) -> (Completions, String) {
    run(main, &[], options, StdAccess::None)
}

/// The entry labelled `label`.
pub(crate) fn entry<'a>(found: &'a Completions, label: &str) -> &'a Entry {
    found
        .items
        .iter()
        .find(|e| e.label == label)
        .unwrap_or_else(|| panic!("no `{label}` in {:?}", labels(found)))
}

/// The labels, in order.
pub(crate) fn labels(found: &Completions) -> Vec<&str> {
    found.items.iter().map(|e| e.label.as_str()).collect()
}

/// The text `range` of `text` covers.
pub(crate) fn slice(text: &str, range: TextRange) -> &str {
    range.slice(text).expect("a range of the text")
}

/// `snippet` expanded as a client expands it: `${1:f}` gives `f`, `$1`
/// and `$0` nothing, and `\$`, `\}` and `\\` the character.
pub(crate) fn expand(snippet: &str) -> String {
    let mut out = String::new();
    let mut open = 0_u32;
    let mut chars = snippet.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            '$' if chars.peek() == Some(&'{') => {
                chars.next();
                open = open.saturating_add(1);
                while chars.next_if(char::is_ascii_digit).is_some() {}
                chars.next_if_eq(&':');
            }
            '$' => while chars.next_if(char::is_ascii_digit).is_some() {},
            '}' if open > 0 => open = open.saturating_sub(1),
            other => out.push(other),
        }
    }
    out
}

/// `text` with `entry`'s main edit and its additional edits applied, its
/// snippet expanded as a client would.
pub(crate) fn applied(text: &str, entry: &Entry) -> String {
    let mut edits: Vec<(TextRange, String)> = entry.additional.clone();
    let main = if entry.insert.snippet {
        expand(&entry.insert.text)
    } else {
        entry.insert.text.clone()
    };
    edits.push((entry.insert.range, main));
    edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start()));
    let mut out = text.to_owned();
    for (range, insert) in edits {
        let start = usize::try_from(range.start()).unwrap();
        let end = usize::try_from(range.end()).unwrap();
        out.replace_range(start..end, &insert);
    }
    out
}

/// Whether `text` parses with no syntax error.
pub(crate) fn parses(text: &str) -> bool {
    let doc = DocText::new("parse.cheby".into(), None, text.into()).expect("a text");
    doc.errors.is_empty()
}

/// No snippets, as a plain-text client has it.
const PLAIN: Options = Options {
    snippets: false,
    ..ON
};

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_list_offers_its_module_in_the_pipe_form_with_an_import() {
    let main = "fn main() {\n  let xs = [1, 2]\n  let ys = xs.ma‸\n  assert ys == xs\n}\n";
    let (found, text) = at(main, IMPORTING);
    let map = entry(&found, "map");
    assert_eq!(map.rank, rank::LOCAL, "the head type's module ranks 10");
    // Only `.ma` is replaced (D-1394).
    assert_eq!(slice(&text, map.insert.range), ".ma");
    assert_eq!(map.insert.text, " |> list::map(_, ${1:f})$0");
    assert!(map.insert.snippet);
    assert_eq!(map.filter_text.as_deref(), Some(".map"));
    assert_eq!(
        map.additional,
        vec![(TextRange::empty(0), "import std::list\n\n".to_owned())]
    );
    // A one-parameter function is bare (D-245).
    let length = entry(&found, "length");
    assert_eq!(length.insert.text, " |> list::length");
    assert!(!length.insert.snippet);
    // Functions of the list's elements are not offered.
    assert!(!found.items.iter().any(|e| e.label == "to_string"));
    let done = applied(&text, map);
    assert!(done.contains("let ys = xs |> list::map(_, f)\n"), "{done}");
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn auto_import_off_offers_no_function_that_needs_an_import() {
    // `std::list` is the head's module, but the file does not import it
    // (D-1486).
    let main = "fn main() {\n  let xs = [1, 2]\n  let ys = xs.ma‸\n}\n";
    let (found, _) = at(main, ON);
    assert!(
        found.items.iter().all(|e| e.additional.is_empty()),
        "{:?}",
        labels(&found)
    );
    assert!(!found.items.iter().any(|e| e.label == "map"));
    assert!(!found.incomplete);
    // With the module imported, its functions need no import.
    let (found, _) = at(&format!("import std::list\n\n{main}"), ON);
    assert_eq!(entry(&found, "map").additional, Vec::new());
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_plain_client_gets_parameter_names() {
    let main = "import std::list\n\nfn main() {\n  let xs = [1, 2]\n  let ys = xs.‸\n}\n";
    let (found, text) = at(main, PLAIN);
    let fold = entry(&found, "fold");
    assert_eq!(fold.insert.text, " |> list::fold(_, initial, f)");
    assert!(!fold.insert.snippet);
    assert!(fold.additional.is_empty(), "`list` is imported");
    assert!(parses(&applied(&text, fold)));
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn zed_filters_on_the_name() {
    let main = "fn main() {\n  let xs = [1, 2]\n  let ys = xs.ma‸\n}\n";
    let zed = Options {
        zed: true,
        ..IMPORTING
    };
    let (found, _) = at(main, zed);
    assert_eq!(entry(&found, "map").filter_text.as_deref(), Some("map"));
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_taken_module_name_imports_with_an_alias() {
    let main = "fn main() {\n  let list = [1]\n  let xs = [1, 2]\n  let ys = xs.ma‸\n}\n";
    let (found, _) = at(main, IMPORTING);
    let map = entry(&found, "map");
    assert_eq!(map.insert.text, " |> std_list::map(_, ${1:f})$0");
    assert_eq!(
        map.additional,
        vec![(
            TextRange::empty(0),
            "import std::list as std_list\n\n".to_owned()
        )]
    );
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn an_operand_gets_the_call_form() {
    // `1 + xs |> f` would be `(1 + xs) |> f`... and `xs |> f + 1` would
    // pipe into `f + 1`: the call form keeps the meaning.
    let main = "import std::list\n\nfn main() {\n  let xs = [1, 2]\n  let n = 1 + xs.le‸\n}\n";
    let (found, text) = at(main, ON);
    let length = entry(&found, "length");
    assert_eq!(slice(&text, length.insert.range), "xs.le");
    assert_eq!(length.insert.text, "list::length(xs)");
    assert_eq!(length.filter_text.as_deref(), Some("xs.length"));
    let fold = entry(&found, "fold");
    assert_eq!(fold.insert.text, "list::fold(xs, ${1:initial}, ${2:f})$0");
    // So do a prefix operand, a further `.` and a callee.
    for line in ["!xs.le‸", "xs.le‸.x", "xs.le‸(1)", "xs.le‸ * 2"] {
        let main = format!("import std::list\n\nfn main() {{\n  let xs = [1, 2]\n  {line}\n}}\n");
        let (found, _) = at(&main, ON);
        let length = entry(&found, "length");
        assert_eq!(length.insert.text, "list::length(xs)", "{line}");
    }
    // A comparison binds looser than `|>`: the pipe form keeps the meaning.
    let main = "import std::list\n\nfn main() {\n  let xs = [1, 2]\n  assert xs.‸ == 2\n}\n";
    let (found, _) = at(main, ON);
    assert_eq!(entry(&found, "length").insert.text, " |> list::length");
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_receiver_on_several_lines_keeps_the_edit_on_one_line() {
    let main = "import std::list\n\nfn main() {\n  let ys = [\n    1,\n    2,\n  ].ma‸\n}\n";
    let (found, text) = at(main, ON);
    let map = entry(&found, "map");
    assert_eq!(slice(&text, map.insert.range), ".ma");
    // An operand on several lines has no form that keeps the meaning on
    // one line, so only fields would be offered.
    let main = "import std::list\n\nfn main() {\n  let n = 1 + [\n    1,\n  ].le‸\n}\n";
    let (found, _) = at(main, ON);
    assert!(
        !found.items.iter().any(|e| e.kind == EntryKind::Function),
        "{:?}",
        labels(&found)
    );
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_receiver_in_a_snippet_is_escaped() {
    let main = "import std::string\n\nfn main() {\n  let n = 1 + \"a$b}\\\\c\".le‸\n}\n";
    let (found, _) = at(main, ON);
    let length = entry(&found, "length");
    // One parameter: no placeholder, so plain text, unescaped.
    assert_eq!(length.insert.text, "string::length(\"a$b}\\\\c\")");
    assert!(!length.insert.snippet);
    let slice_fn = entry(&found, "slice");
    assert_eq!(
        slice_fn.insert.text,
        "string::slice(\"a\\$b\\}\\\\\\\\c\", ${1:start}, ${2:end})$0"
    );
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn fields_are_those_every_variant_has_with_one_type() {
    let shapes = (
        "shapes.cheby",
        "pub exposed type Shape {\n  Circle { name: String, r: Float },\n  Square { name: String, side: Float },\n}\n\npub fn circle() -> Shape {\n  Circle { name: \"c\", r: 1.0 }\n}\n",
    );
    let main = "import app::shapes\n\nfn main() {\n  let s = shapes::circle()\n  let n = s.‸\n}\n";
    let (found, _) = run(main, &[shapes], ON, StdAccess::None);
    let fields: Vec<&str> = found
        .items
        .iter()
        .filter(|e| e.kind == EntryKind::Field)
        .map(|e| e.label.as_str())
        .collect();
    assert_eq!(fields, ["name"]);
    let name = entry(&found, "name");
    assert_eq!(name.rank, rank::FIELD);
    assert_eq!(name.detail.as_deref(), Some("String"));
    // A type whose structure is hidden from other packages has no fields
    // there (D-048).
    let main =
        "import std::duration\n\nfn main() {\n  let d = duration::seconds(1)\n  let n = d.‸\n}\n";
    let (found, _) = at(main, ON);
    assert!(
        !found.items.iter().any(|e| e.kind == EntryKind::Field),
        "{:?}",
        labels(&found)
    );
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn an_interpolation_offers_fields_only() {
    let main = "import app::geo\n\nfn main() {\n  let p = geo::origin\n  let s = \"{p.‸}\"\n}\n";
    let (found, text) = at(main, ON);
    assert_eq!(labels(&found), ["x", "y"]);
    assert!(entry(&found, "x").insert.range.is_empty());
    assert!(parses(&applied(&text, entry(&found, "x"))));
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn an_unknown_receiver_offers_nothing() {
    let main = "fn main() {\n  let n = nowhere.‸\n}\n";
    let (found, _) = at(main, ON);
    assert_eq!(labels(&found), Vec::<&str>::new());
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn type_driven_off_leaves_fields_only() {
    let main = "import app::geo\n\nfn main() {\n  let p = geo::origin\n  let n = p.‸\n}\n";
    let off = Options {
        type_driven: false,
        ..ON
    };
    let (found, _) = at(main, off);
    assert_eq!(labels(&found), ["x", "y"]);
    let (found, _) = at(main, ON);
    let add = entry(&found, "add");
    assert_eq!(add.insert.text, " |> geo::add(_, ${1:b})$0");
    assert!(add.deprecated.eq(&false));
    assert!(entry(&found, "plus").deprecated);
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_later_parameter_ranks_five_later() {
    let main = "fn put(n: Int, xs: List<Int>) -> List<Int> {\n  xs\n}\n\nfn main() {\n  let xs = [1, 2]\n  let ys = xs.p‸\n}\n";
    let (found, _) = at(main, ON);
    let put = entry(&found, "put");
    assert_eq!(put.rank, rank::MODULE + 5);
    assert_eq!(put.insert.text, " |> put(${1:n}, _)$0");
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn postfix_case_writes_an_arm_per_constructor() {
    let main = "fn main() {\n  let o = Some(1)\n  let n = o.ca‸\n}\n";
    let (found, text) = at(main, ON);
    let case = entry(&found, "case");
    assert_eq!(case.kind, EntryKind::Snippet);
    assert_eq!(slice(&text, case.insert.range), "o.ca");
    assert_eq!(
        case.insert.text,
        "case o {\n    Some(_) => ${1:todo}\n    None => ${2:todo}\n  }$0"
    );
    assert_eq!(case.filter_text.as_deref(), Some("o.case"));
    let done = applied(&text, case);
    assert!(
        done.contains("  let n = case o {\n    Some(_) => todo\n    None => todo\n  }\n"),
        "{done}"
    );
    assert!(parses(&done));
    // No snippets, no postfix `case`.
    let (found, _) = at(main, PLAIN);
    assert!(!found.items.iter().any(|e| e.label == "case"));
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn functions_of_one_name_from_two_modules_both_stay() {
    let main = "import std::list\n\nfn length(xs: List<Int>, extra: Int) -> Int {\n  extra\n}\n\nfn main() {\n  let xs = [1]\n  let n = xs.le‸\n  n\n}\n";
    let (found, _) = at(main, ON);
    let lengths: Vec<(Option<&str>, &str)> = found
        .items
        .iter()
        .filter(|e| e.label == "length")
        .map(|e| (e.description.as_deref(), e.insert.text.as_str()))
        .collect();
    assert_eq!(
        lengths,
        [
            (Some("list"), " |> list::length"),
            (None, " |> length(_, ${1:extra})$0"),
        ]
    );
}
