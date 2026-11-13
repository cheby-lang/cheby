//! `workspace/symbol` (X §5.2): items of every package the server serves,
//! then std's from its plain index (D-1372).
//!
//! The request names no document, so it reads no one snapshot: it takes
//! the newest snapshot of each package (A §3.3's `Workspace` class) and
//! answers once for all of them.
//!
//! 1. A package's items are the outline of each of its modules
//!    (`ide::symbols`, X §5.1): items, variants and interface functions,
//!    with no fields and no tests. The outline reads the snapshot's tree,
//!    so a file with grammar errors gives what its tree holds.
//! 2. Std's items come from [`StdIndex`], and only for a query of at
//!    least two characters.
//! 3. A name matches without regard to case: exactly, else by prefix,
//!    else as a subsequence. An empty query matches every package item.
//! 4. The list is sorted by match class, package before std, name, module
//!    path, file and position, and holds at most [`LIMIT`] entries (X §1.4).

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use cheby_driver::analysis::{DefKind, StdIndex, StdItem, TypeHead};
use cheby_driver::std_lib;
use lsp_types::{
    Location, Position, Range, SymbolInformation, SymbolKind, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
};
use serde_json::Value;

use super::hierarchy::Locator;
use crate::features::ide::symbols::{OutlineItem, OutlineKind, outline};
use crate::features::{Analyzed, HandlerOutput, RequestCx, RequestError, params};
use crate::text::lines::{Bom, LspLines, PositionEncoding};
use crate::text::uri::path_to_uri;

/// The most entries an answer holds (X §5.2).
pub(crate) const LIMIT: usize = 200;

/// The shortest query that searches std (X §5.2).
const STD_QUERY: usize = 2;

/// What `workspace/symbol` reads of std beyond a snapshot: its index
/// (D-1372) and where its sources are on disk (D-1369).
#[derive(Clone, Copy, Debug)]
pub(crate) struct StdSymbols<'a> {
    /// The index of std's items.
    pub index: &'a StdIndex,
    /// The directory `materialize_std` wrote.
    pub dir: &'a Path,
}

/// `workspace/symbol` over `packages`, the newest snapshot of each package
/// the server serves, and `std` once it is ready.
///
/// # Errors
///
/// `InvalidParams` for parameters of another shape, `Cancelled` when the
/// client cancels.
pub(crate) fn workspace_symbol(
    cx: &RequestCx<'_>,
    packages: &[&Analyzed],
    params_value: Value,
    std: Option<StdSymbols<'_>>,
) -> Result<HandlerOutput, RequestError> {
    let p: WorkspaceSymbolParams = params(params_value)?;
    let query = p.query.to_lowercase();
    let mut found = Vec::new();
    for analyzed in packages {
        package_symbols(cx, analyzed, &query, &mut found)?;
    }
    if let Some(std) = std.filter(|_| query.chars().count() >= STD_QUERY) {
        cx.cancel.check()?;
        std_symbols(std, &query, cx.encoding, &mut found);
    }
    found.sort_by(Found::order);
    found.dedup_by(|a, b| a.order(b) == Ordering::Equal);
    found.truncate(LIMIT);
    let answer = WorkspaceSymbolResponse::Flat(found.into_iter().map(Found::into_info).collect());
    serde_json::to_value(answer)
        .map(HandlerOutput::value)
        .map_err(|e| RequestError::Failed(e.to_string()))
}

/// How a name matches a query (X §5.2), best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Match {
    /// The same name, case aside.
    Exact,
    /// The name starts with the query.
    Prefix,
    /// The query's characters appear in the name in order.
    Subsequence,
}

/// How `name` matches `query`, which is lower case already.
fn matches(name: &str, query: &str) -> Option<Match> {
    let name = name.to_lowercase();
    if name == query {
        return Some(Match::Exact);
    }
    if name.starts_with(query) {
        return Some(Match::Prefix);
    }
    let mut chars = name.chars();
    query
        .chars()
        .all(|q| chars.any(|c| c == q))
        .then_some(Match::Subsequence)
}

/// Where an entry comes from: packages sort before std.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    Package,
    Std,
}

/// One entry, with what it sorts by.
#[derive(Debug)]
struct Found {
    class: Match,
    source: Source,
    name: String,
    /// The module path, `None` in single-file mode (D-1477).
    container: Option<String>,
    path: PathBuf,
    location: Location,
    kind: SymbolKind,
}

impl Found {
    /// The order of X §5.2: class, source, name, module path, then file and
    /// position.
    fn order(&self, other: &Self) -> Ordering {
        let start = |f: &Self| {
            (
                f.location.range.start.line,
                f.location.range.start.character,
            )
        };
        (
            self.class,
            self.source,
            &self.name,
            &self.container,
            &self.path,
        )
            .cmp(&(
                other.class,
                other.source,
                &other.name,
                &other.container,
                &other.path,
            ))
            .then_with(|| start(self).cmp(&start(other)))
    }

    #[expect(deprecated, reason = "the protocol's struct still has the field")]
    fn into_info(self) -> SymbolInformation {
        SymbolInformation {
            name: self.name,
            kind: self.kind,
            tags: None,
            deprecated: None,
            location: self.location,
            container_name: self.container,
        }
    }
}

/// The items of `analyzed`'s package modules that match `query`.
fn package_symbols(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    query: &str,
    found: &mut Vec<Found>,
) -> Result<(), RequestError> {
    let snapshot = &analyzed.snapshot;
    let mut locator = Locator::new(snapshot, None, cx.encoding);
    for view in snapshot.local_modules() {
        cx.cancel.check()?;
        let (Some(tree), Some(file), Some(module)) = (view.tree(), view.file(), view.path()) else {
            continue;
        };
        let Some(path) = locator.path(file) else {
            continue;
        };
        // A single-file document names no package (D-1477).
        let container = (!snapshot.is_single_file()).then(|| module.to_string());
        let mut add = |item: &OutlineItem| {
            let Some(class) = matches(&item.name, query) else {
                return;
            };
            // The name, as std's symbols give it: a client jumps to the
            // start, which for the whole item is its doc comment.
            let Some(location) = locator.location(file, item.selection) else {
                return;
            };
            found.push(Found {
                class,
                source: Source::Package,
                name: item.name.clone(),
                container: container.clone(),
                path: path.clone(),
                location,
                kind: outline_kind(item.kind),
            });
        };
        for item in outline(tree.root()) {
            if item.kind == OutlineKind::Test {
                continue;
            }
            add(&item);
            for child in &item.children {
                if matches!(child.kind, OutlineKind::Variant | OutlineKind::InterfaceFn) {
                    add(child);
                }
            }
        }
    }
    Ok(())
}

/// The protocol's kind of an outline entry, as `documentSymbol` gives it.
fn outline_kind(kind: OutlineKind) -> SymbolKind {
    match kind {
        OutlineKind::Function | OutlineKind::Test => SymbolKind::FUNCTION,
        OutlineKind::Enum => SymbolKind::ENUM,
        OutlineKind::Struct => SymbolKind::STRUCT,
        OutlineKind::Opaque => SymbolKind::CLASS,
        OutlineKind::Alias => SymbolKind::TYPE_PARAMETER,
        OutlineKind::Variant => SymbolKind::ENUM_MEMBER,
        OutlineKind::Field => SymbolKind::FIELD,
        OutlineKind::Constant => SymbolKind::CONSTANT,
        OutlineKind::Interface => SymbolKind::INTERFACE,
        OutlineKind::InterfaceFn => SymbolKind::METHOD,
    }
}

/// The items of std that match `query`, at their names in the files under
/// `std.dir`. A builtin item, which has no declaration, points at the
/// start of its home module's file.
fn std_symbols(std: StdSymbols<'_>, query: &str, enc: PositionEncoding, found: &mut Vec<Found>) {
    let ctors = constructors(std.index);
    let mut files: HashMap<&str, Option<(&'static str, LspLines)>> = HashMap::new();
    for item in &std.index.items {
        if item.kind == DefKind::Test {
            continue;
        }
        let name = item.key.text();
        let Some(class) = matches(name, query) else {
            continue;
        };
        let path = std.dir.join(&item.file);
        let Ok(uri) = path_to_uri(&path) else {
            continue;
        };
        let range = files
            .entry(item.file.as_str())
            .or_insert_with(|| {
                let text = std_text(&item.file)?;
                Some((text, LspLines::new(text, Bom::Hidden)))
            })
            .as_ref()
            .zip(item.name_range)
            .map_or_else(
                || Range::new(Position::new(0, 0), Position::new(0, 0)),
                |((text, lines), range)| lines.range(text, range, enc),
            );
        found.push(Found {
            class,
            source: Source::Std,
            name: name.to_owned(),
            container: Some(item.module.clone()),
            path,
            location: Location::new(uri, range),
            kind: std_kind(item, &ctors),
        });
    }
}

/// The embedded text of the std file shown as `display`, such as
/// `std/list.cheby`: the text `materialize_std` writes.
fn std_text(display: &str) -> Option<&'static str> {
    std_lib::STD_MODULES
        .iter()
        .find(|m| std_lib::display_path(m.path) == display)
        .map(|m| m.source)
}

/// The constructors of each type of `index`, by the type's head.
fn constructors(index: &StdIndex) -> BTreeMap<&TypeHead, Vec<&str>> {
    let mut ctors: BTreeMap<&TypeHead, Vec<&str>> = BTreeMap::new();
    for item in index.items.iter().filter(|i| i.kind == DefKind::Ctor) {
        ctors.entry(&item.ret).or_default().push(item.key.text());
    }
    ctors
}

/// The protocol's kind of a std item, as an outline of its file would give
/// it: a type with one constructor of its own name is a record, a type
/// with others an enum, and one with none opaque.
fn std_kind(item: &StdItem, ctors: &BTreeMap<&TypeHead, Vec<&str>>) -> SymbolKind {
    match item.kind {
        DefKind::Fn | DefKind::Test => SymbolKind::FUNCTION,
        DefKind::Const => SymbolKind::CONSTANT,
        DefKind::Alias => SymbolKind::TYPE_PARAMETER,
        DefKind::Interface => SymbolKind::INTERFACE,
        DefKind::InterfaceFn => SymbolKind::METHOD,
        DefKind::Ctor => SymbolKind::ENUM_MEMBER,
        DefKind::Type => {
            let name = item.key.text();
            let own = [
                TypeHead::Named {
                    module: item.key.module.to_string(),
                    name: name.to_owned(),
                },
                TypeHead::Builtin(name.to_owned()),
            ];
            match own
                .iter()
                .find_map(|head| ctors.get(head))
                .map(Vec::as_slice)
            {
                Some([only]) if *only == name => SymbolKind::STRUCT,
                Some([_, ..]) => SymbolKind::ENUM,
                Some([]) | None => SymbolKind::CLASS,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::path::Path;
    use std::sync::Arc;

    use cheby_driver::analysis::{Globals, StdBase};
    use serde_json::{Value, json};

    use super::{LIMIT, Match, StdSymbols, matches, workspace_symbol};
    use crate::features::Analyzed;
    use crate::features::handlers::symbols::tests::with_cx;
    use crate::features::ide::hierarchy::tests::{Temp, analyze};
    use crate::text::uri::path_to_uri;

    const SHAPES: &str = "/// A shape.\npub type Shape {\n  Circle(Float),\n  Square(Float),\n}\n\npub fn shape_area(s: Shape) -> Float {\n  case s {\n    Circle(r) => r * r\n    Square(w) => w * w\n  }\n}\n\ntest \"shape\" {\n  assert shape_area(Square(2.0)) == 4.0\n}\n";

    fn query(packages: &[&Analyzed], q: &str, std: Option<StdSymbols<'_>>) -> Value {
        let p = json!({ "query": q });
        with_cx(|cx| workspace_symbol(cx, packages, p, std))
            .unwrap()
            .value
    }

    /// The answer as `(name, kind, container)`.
    fn rows(got: &Value) -> Vec<(String, u64, String)> {
        got.as_array()
            .expect("a list")
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().unwrap().to_owned(),
                    s["kind"].as_u64().unwrap(),
                    s["containerName"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn match_classes() {
        assert_eq!(matches("Shape", "shape"), Some(Match::Exact));
        assert_eq!(matches("shape_area", "shape"), Some(Match::Prefix));
        assert_eq!(matches("shape_area", "sa"), Some(Match::Subsequence));
        assert_eq!(matches("shape_area", "sea"), Some(Match::Subsequence));
        assert_eq!(matches("area", "sa"), None);
        assert_eq!(matches("anything", ""), Some(Match::Prefix));
    }

    fn check_package(name: &str, main_text: &str) {
        let temp = Temp::package(name, &[("main.cheby", main_text), ("shapes.cheby", SHAPES)]);
        let main = temp.src("main.cheby");
        let analyzed = analyze(&temp.0, &[(main.clone(), main_text)]);
        let s = |x: &str| x.to_owned();
        let got = query(&[&analyzed], "shape", None);
        assert_eq!(
            rows(&got),
            [
                (s("Shape"), 10, s("app::shapes")),
                (s("shape_area"), 12, s("app::shapes")),
            ],
            "{name}"
        );
        // The name, not the whole item with its doc comment.
        let shapes = temp.src("shapes.cheby");
        assert_eq!(
            got[0]["location"],
            json!({
                "uri": path_to_uri(&shapes).unwrap().as_str(),
                "range": {
                    "start": { "line": 1, "character": 9 },
                    "end": { "line": 1, "character": 14 },
                },
            }),
            "{name}"
        );
        // Variants come too, tests never; an empty query lists everything.
        let all = rows(&query(&[&analyzed], "", None));
        assert!(
            all.contains(&(s("Circle"), 22, s("app::shapes"))),
            "{name}: {all:?}"
        );
        assert!(!all.iter().any(|(n, _, _)| n == "shape"), "{name}: {all:?}");
        assert!(
            all.iter().any(|(n, _, c)| n == "main" && c == "app"),
            "{name}: {all:?}"
        );
    }

    #[test]
    #[cfg_attr(windows, ignore = "builds a JIT until M6 (D-545, D-641)")]
    fn package_items_on_clean_text() {
        check_package(
            "symbols_clean",
            "import app::shapes\n\nfn main() {\n  assert shapes::shape_area(shapes::Circle(1.0)) == 1.0\n}\n",
        );
    }

    #[test]
    #[cfg_attr(windows, ignore = "builds a JIT until M6 (D-545, D-641)")]
    fn package_items_with_a_type_error() {
        check_package(
            "symbols_type_error",
            "import app::shapes\n\nfn main() {\n  assert shapes::shape_area(1) == 1.0\n}\n",
        );
    }

    #[test]
    #[cfg_attr(windows, ignore = "builds a JIT until M6 (D-545, D-641)")]
    fn package_items_with_a_grammar_error() {
        check_package(
            "symbols_grammar_error",
            "import app::shapes\n\nfn main() {\n  assert shapes::shape_area(\n}\n",
        );
    }

    #[test]
    #[cfg_attr(windows, ignore = "builds a JIT until M6 (D-545, D-641)")]
    fn packages_merge_and_the_list_is_capped() {
        let many = (0..150).fold(String::new(), |mut text, n| {
            writeln!(text, "pub fn item_{n:03}() -> Int {{\n  {n}\n}}\n").unwrap();
            text
        });
        let one = Temp::package(
            "symbols_one",
            &[("main.cheby", "fn main() {}\n"), ("a.cheby", &many)],
        );
        let two = Temp::package(
            "symbols_two",
            &[("main.cheby", "fn main() {}\n"), ("b.cheby", &many)],
        );
        let a = analyze(&one.0, &[]);
        let b = analyze(&two.0, &[]);
        let got = rows(&query(&[&a, &b], "item", None));
        assert_eq!(got.len(), LIMIT);
        // Same name: the module path decides, so both packages' `item_000`
        // come first, `a` before `b`.
        assert_eq!(got[0].0, "item_000");
        assert_eq!(got[0].2, "app::a");
        assert_eq!(got[1].0, "item_000");
        assert_eq!(got[1].2, "app::b");
    }

    #[test]
    #[cfg_attr(windows, ignore = "builds a JIT until M6 (D-545, D-641)")]
    fn std_comes_after_the_package_and_needs_two_characters() {
        let temp = Temp::package(
            "symbols_std",
            &[(
                "main.cheby",
                "fn print_all() {}\n\nfn main() {\n  print_all()\n}\n",
            )],
        );
        let analyzed = analyze(&temp.0, &[]);
        let base = StdBase::build(Arc::new(Globals::new())).expect("std checks");
        let dir = Path::new("/std-src");
        let std = StdSymbols {
            index: base.index(),
            dir,
        };
        let got = query(&[&analyzed], "println", Some(std));
        let found = rows(&got);
        let first = found.first().expect("a match");
        assert_eq!((first.0.as_str(), first.2.as_str()), ("println", "std::io"));
        let uri = got[0]["location"]["uri"].as_str().unwrap();
        assert!(uri.ends_with("/std-src/std/io.cheby"), "{uri}");
        let line = got[0]["location"]["range"]["start"]["line"]
            .as_u64()
            .unwrap();
        assert!(line > 0);
        // Within a match class the package comes first: `print` is exactly
        // std's `print`, and a prefix of both `print_all` and `println`.
        let got = rows_of(&query(&[&analyzed], "print", Some(std)));
        let at = |name: &str| got.iter().position(|(n, _)| n == name);
        assert!(at("print") < at("print_all"), "{got:?}");
        assert!(at("print_all") < at("println"), "{got:?}");
        // One character searches the package only.
        let got = rows_of(&query(&[&analyzed], "p", Some(std)));
        assert!(got.iter().all(|(_, c)| c == "app"), "{got:?}");
        // A record type is a struct, a type with variants an enum.
        let got = rows(&query(&[&analyzed], "option", Some(std)));
        assert!(
            got.iter().any(|(n, k, _)| n == "Option" && *k == 10),
            "{got:?}"
        );
    }

    fn rows_of(got: &Value) -> Vec<(String, String)> {
        rows(got).into_iter().map(|(n, _, c)| (n, c)).collect()
    }

    #[test]
    fn bad_params_are_invalid_params() {
        let got = with_cx(|cx| workspace_symbol(cx, &[], json!(3), None));
        assert!(matches!(
            got,
            Err(crate::features::RequestError::InvalidParams(_))
        ));
    }
}
