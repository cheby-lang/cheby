//! Tests of the index: every identifier of every example and every std
//! module is classified (F §5.1), and the rows of F §5.3 on clean text, a
//! type error and a grammar error. The helpers of `ide::scope` and
//! `ide::print` are tested here too, on the same packages.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_driver::analysis::{
    Globals, ModulePath, Overlay, OverlayFile, PackageKind, Session, SessionConfig, Severity,
    Snapshot, StdBase, Symbol, SymbolNs, TextRange,
};
use cheby_syntax::SyntaxKind;

use super::{
    Classified, ImplicitKind, Index, Meaning, ModuleIndex, NameKind, Partial, Role, Unknown,
    Unnamed,
};
use crate::features::ide::print::Printer;
use crate::features::ide::scope;
use crate::features::{Analyzed, CancelToken};

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
        let dir =
            std::env::temp_dir().join(format!("cheby_lsp_index_{}_{name}", std::process::id()));
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

/// The example packages of `docs/examples`.
fn examples() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
    let mut roots: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("docs/examples")
        .map(|e| e.expect("an entry").path())
        .filter(|p| p.join("cheby.toml").is_file())
        .collect();
    roots.sort();
    assert!(roots.len() >= 32, "{roots:?}");
    roots
}

/// What [`covered`] counted.
#[derive(Debug, Default)]
struct Coverage {
    modules: usize,
    tokens: usize,
    occurrences: usize,
    unknown: Vec<(String, TextRange, Unknown, Option<SyntaxKind>)>,
    unnamed: usize,
    operators: usize,
    shows: usize,
    dyns: usize,
}

/// Checks that every identifier token of every indexed module of
/// `index` is classified once, that no token is both placed and unnamed,
/// and that every symbol named maps back into the snapshot (D-1365).
fn covered(index: Index<'_>, what: &str) -> Coverage {
    let snapshot = index.snapshot();
    let mut seen = Coverage::default();
    for module in index.modules() {
        let view = snapshot.module(module.module).expect("the module");
        let tree = view.tree().expect("an indexed module has a tree");
        let mut tokens = 0_usize;
        for token in tree.root().tokens() {
            if !matches!(
                token.kind(),
                SyntaxKind::LOWER_IDENT | SyntaxKind::UPPER_IDENT
            ) {
                continue;
            }
            tokens = tokens.saturating_add(1);
            let range = token.text_range();
            let placed = module.at(range).is_some();
            let unnamed = module.unnamed.iter().any(|u| u.range == range);
            assert!(
                !(placed && unnamed),
                "{what} {}: {token:?} twice",
                module.path
            );
            match module.classify(range) {
                Classified::Missing => {
                    panic!("{what} {}: {token:?} is not classified", module.path)
                }
                Classified::Occurrence(_) => seen.occurrences = seen.occurrences.saturating_add(1),
                Classified::Unnamed(_) => seen.unnamed = seen.unnamed.saturating_add(1),
                Classified::Unknown(name) => {
                    assert_eq!(&*name.text, token.text());
                    let holder = token.parent().map(cheby_syntax::SyntaxNode::kind);
                    seen.unknown
                        .push((module.path.to_string(), range, name.why, holder));
                }
            }
        }
        assert_eq!(
            tokens,
            [
                module
                    .occurrences
                    .iter()
                    .filter(|o| !o.role.is_implicit())
                    .count(),
                module.unnamed.len(),
                module
                    .unknown
                    .iter()
                    .filter(|u| module.at(u.range).is_none())
                    .count(),
            ]
            .into_iter()
            .sum::<usize>(),
            "{what} {}: one entry per token",
            module.path
        );
        // An item whose signature has errors keeps no type parameters
        // to find, so only a run without errors maps every symbol back.
        let lost: Vec<_> = module
            .symbols
            .iter()
            .filter(|entry| snapshot.find(&entry.symbol).is_none())
            .collect();
        assert!(
            lost.is_empty() || has_errors(snapshot),
            "{what} {}: these do not map back: {lost:#?}\n{:#?}",
            module.path,
            snapshot.baseline()
        );
        for o in module.occurrences.iter().filter(|o| o.role.is_implicit()) {
            let text = o.range.slice(tree.text()).unwrap_or_default();
            match o.role {
                Role::Implicit(ImplicitKind::Operator) => {
                    let token = tree.root().token_at_offset(o.range.start()).unwrap();
                    assert_eq!(token.text_range(), o.range, "{what}: {text}");
                    assert!(!token.kind().is_trivia(), "{what}: {text}");
                    seen.operators = seen.operators.saturating_add(1);
                }
                Role::Implicit(ImplicitKind::Show) => {
                    assert!(
                        text.starts_with('{') && text.ends_with('}'),
                        "{what}: {text}"
                    );
                    seen.shows = seen.shows.saturating_add(1);
                }
                _ => seen.dyns = seen.dyns.saturating_add(1),
            }
        }
        seen.tokens = seen.tokens.saturating_add(tokens);
        seen.modules = seen.modules.saturating_add(1);
    }
    seen
}

/// Whether the run reported an error.
fn has_errors(snapshot: &Snapshot) -> bool {
    snapshot
        .baseline()
        .iter()
        .any(|(code, _, _)| code.severity() == Severity::Error)
        || !snapshot.status().broken.is_empty()
}

/// The examples that do not check: 030 needs a dependency, which comes
/// with M5, so its import of `sql` does not resolve.
const UNCLEAN: &[&str] = &["030_shop_service"];

/// F §5.1: every identifier token of every example gets one entry, and
/// none is unknown in an example that checks. Only the examples of
/// [`UNCLEAN`] may fail to check, and there only coverage holds.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs the front end, which needs JIT code; Windows is M6 (D-545, D-641)"
)]
fn index_covers_every_identifier() {
    let mut total = Coverage::default();
    let mut failed: Vec<String> = Vec::new();
    for root in examples() {
        let analyzed = analyze(&root, &[]);
        let seen = covered(analyzed.index(), &root.display().to_string());
        if has_errors(&analyzed.snapshot) {
            let name = root.file_name().map(|n| n.to_string_lossy().into_owned());
            failed.push(name.unwrap_or_default());
        } else {
            assert!(
                seen.unknown.is_empty(),
                "{}: {:#?}",
                root.display(),
                seen.unknown
            );
        }
        total.modules = total.modules.saturating_add(seen.modules);
        total.tokens = total.tokens.saturating_add(seen.tokens);
        total.occurrences = total.occurrences.saturating_add(seen.occurrences);
        total.operators = total.operators.saturating_add(seen.operators);
        total.shows = total.shows.saturating_add(seen.shows);
        total.dyns = total.dyns.saturating_add(seen.dyns);
    }
    eprintln!("{total:?}");
    failed.sort();
    assert_eq!(failed, UNCLEAN, "the examples that do not check");
    assert!(total.modules >= 32, "{total:?}");
    assert!(total.tokens > 10_000, "{total:?}");
    assert!(total.shows > 100 && total.dyns > 0, "{total:?}");
}

/// F §5.1 on std: every identifier of every shipped module gets one
/// entry. Only `test` bodies, which std's base does not check (D-1370),
/// may hold fields the index cannot place.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs the front end, which needs JIT code; Windows is M6 (D-545, D-641)"
)]
fn index_covers_every_identifier_of_std() {
    let base = StdBase::build(Arc::new(Globals::new())).expect("std checks");
    let analyzed = Analyzed::new(Arc::clone(base.snapshot()));
    let seen = covered(analyzed.index(), "std");
    assert!(seen.modules >= 20, "{seen:?}");
    let snapshot = &analyzed.snapshot;
    let stray: Vec<_> = seen
        .unknown
        .iter()
        .filter(|(path, range, why, _)| {
            *why != Unknown::Unchecked || !in_test(snapshot, path, *range)
        })
        .collect();
    assert!(stray.is_empty(), "{stray:#?}");
}

/// Whether `range` of the std module `path` is inside a `test` item.
fn in_test(snapshot: &Snapshot, path: &str, range: TextRange) -> bool {
    let Some(tree) = snapshot
        .all_modules()
        .find(|m| m.path().is_some_and(|p| p.to_string() == path))
        .and_then(cheby_driver::analysis::ModuleView::tree)
    else {
        return false;
    };
    tree.root()
        .token_at_offset(range.start())
        .and_then(cheby_syntax::SyntaxToken::parent)
        .is_some_and(|n| n.ancestors().any(|a| a.kind() == SyntaxKind::TEST_DECL))
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

pub interface Area {
  fn area(Self) -> Int
}
";

const MAIN: &str = "//! Root.

import app::shapes
import app::shapes::{Point as P}
import std::io
import std::list as lists

fn first<T>(xs: List<T>, d: T) -> T {
  case xs {
    [x, ..] => x
    [] => d
  }
}

fn main() {
  let x = 1
  let p = P { x, y: 2 }
  let P { x: px, y } = p
  let total = shapes::add(p, p)
  io::println(\"{total.x} {y} {px}\")
  let n = lists::length([1, 2]) + x
  let k = first([n], 0)
  io::println(\"{n} {k}\")
}
";

/// A module that names `shapes` nowhere, for printing.
const OTHER: &str = "//! Other.

pub fn other() -> Int {
  1
}
";

/// The package of [`SHAPES`], `main` and [`OTHER`], with `main`'s text.
fn shapes_package(name: &str, main: &str) -> (Temp, Analyzed) {
    let temp = Temp::package(
        name,
        &[
            ("main.cheby", main),
            ("shapes.cheby", SHAPES),
            ("other.cheby", OTHER),
        ],
    );
    let main_path = temp.0.join("src/main.cheby");
    let analyzed = analyze(&temp.0, &[(main_path, main)]);
    (temp, analyzed)
}

fn app(segments: &[&str]) -> ModulePath {
    segments
        .iter()
        .fold(ModulePath::root("app"), |path, seg| path.child(seg))
}

fn std_path(segment: &str) -> ModulePath {
    ModulePath::root("std").child(segment)
}

/// The index of the module at `path`.
fn module_named<'a>(index: Index<'a>, path: &ModulePath) -> &'a ModuleIndex {
    index
        .modules()
        .find(|m| m.path == *path)
        .unwrap_or_else(|| panic!("no module {path}"))
}

/// The range of the `n`th (from 0) whole-word `word` in `text`.
fn nth(text: &str, word: &str, n: usize) -> TextRange {
    let bytes = text.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let start = text
        .match_indices(word)
        .map(|(at, _)| at)
        .filter(|&at| {
            let before = at.checked_sub(1).and_then(|b| bytes.get(b)).copied();
            let after = bytes.get(at.saturating_add(word.len())).copied();
            !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
        })
        .nth(n)
        .unwrap_or_else(|| panic!("no {word} #{n}"));
    let start = u32::try_from(start).unwrap();
    TextRange::at(start, u32::try_from(word.len()).unwrap())
}

/// The symbol, second symbol, role and kind at the `n`th `word`.
fn at(
    module: &ModuleIndex,
    text: &str,
    word: &str,
    n: usize,
) -> (Symbol, Option<Symbol>, Role, NameKind) {
    let range = nth(text, word, n);
    let Classified::Occurrence(o) = module.classify(range) else {
        panic!("{word} #{n}: {:?}", module.classify(range));
    };
    let entry = module.entry(o.symbol).unwrap();
    let also = o.also.map(|a| module.entry(a).unwrap().symbol.clone());
    (entry.symbol.clone(), also, o.role, entry.kind)
}

fn local(item: &str, name: &str, ordinal: u32) -> Symbol {
    Symbol::local(app(&[]), SymbolNs::Value, item, name, ordinal)
}

fn field(name: &str) -> Symbol {
    Symbol::field(app(&["shapes"]), "Point", name)
}

/// The rows of F §5.3 in [`MAIN`].
fn clean_main(main: &ModuleIndex) {
    let w = |word, n| at(main, MAIN, word, n);

    // Type parameters: the declaration and every use (`T` #0 is the
    // declaration).
    let t = Symbol::type_param(app(&[]), SymbolNs::Value, "first", "T");
    assert_eq!(
        w("T", 0),
        (t.clone(), None, Role::Decl, NameKind::TypeParam)
    );
    for n in 1..4 {
        assert_eq!(w("T", n), (t.clone(), None, Role::Use, NameKind::TypeParam));
    }
    // Locals are per item, with their kind.
    assert_eq!(
        w("x", 0),
        (local("first", "x", 0), None, Role::Decl, NameKind::Local)
    );
    assert_eq!(w("x", 1).0, local("first", "x", 0));
    assert_eq!(w("d", 0).3, NameKind::Param);
    let main_x = local("main", "x", 0);
    assert_eq!(
        w("x", 2),
        (main_x.clone(), None, Role::Decl, NameKind::Local)
    );
    // A punned initializer is the field, with the value as `also`.
    assert_eq!(
        w("x", 3),
        (
            field("x"),
            Some(main_x.clone()),
            Role::FieldInit,
            NameKind::Field
        )
    );
    assert_eq!(
        w("y", 0),
        (field("y"), None, Role::FieldInit, NameKind::Field)
    );
    // A pattern's field, its binding, and a punned field pattern.
    assert_eq!(
        w("x", 4),
        (field("x"), None, Role::FieldPat, NameKind::Field)
    );
    assert_eq!(w("px", 0).0, local("main", "px", 0));
    assert_eq!(
        w("y", 1),
        (
            field("y"),
            Some(local("main", "y", 0)),
            Role::FieldPat,
            NameKind::Field
        )
    );
}

/// The rows of F §5.3 for imports and paths in [`MAIN`].
fn clean_main_paths(main: &ModuleIndex) {
    let w = |word, n| at(main, MAIN, word, n);
    // An import alias is the alias, with the kind of what it names.
    let p_value = Symbol::import_alias(app(&[]), SymbolNs::Value, "P");
    assert_eq!(
        w("P", 1),
        (p_value.clone(), None, Role::Use, NameKind::Constructor)
    );
    assert_eq!(w("P", 2).0, p_value.clone());
    let p_type = Symbol::import_alias(app(&[]), SymbolNs::Type, "P");
    assert_eq!(
        w("P", 0),
        (p_type, Some(p_value), Role::Decl, NameKind::Type)
    );
    let lists = Symbol::import_alias(app(&[]), SymbolNs::Value, "lists");
    assert_eq!(
        w("lists", 0),
        (lists.clone(), None, Role::Decl, NameKind::Module)
    );
    assert_eq!(
        w("lists", 1),
        (lists, None, Role::Qualifier, NameKind::Module)
    );
    // Qualifiers and the items they lead to.
    let shapes = Symbol::of_module(app(&["shapes"]));
    assert_eq!(
        w("shapes", 2),
        (shapes.clone(), None, Role::Qualifier, NameKind::Module)
    );
    assert_eq!(
        w("add", 0),
        (
            Symbol::item(app(&["shapes"]), SymbolNs::Value, "add"),
            None,
            Role::Use,
            NameKind::Function
        )
    );
    assert_eq!(w("io", 1).0, Symbol::of_module(std_path("io")));
    assert_eq!(
        w("println", 0).0,
        Symbol::item(std_path("io"), SymbolNs::Value, "println")
    );
    // Imports: path segments, an imported item and its two namespaces.
    assert_eq!(
        w("app", 0),
        (
            Symbol::of_module(app(&[])),
            None,
            Role::Import,
            NameKind::Module
        )
    );
    assert_eq!(
        w("shapes", 0),
        (shapes, None, Role::Import, NameKind::Module)
    );
    assert_eq!(
        w("Point", 0),
        (
            Symbol::item(app(&["shapes"]), SymbolNs::Type, "Point"),
            Some(Symbol::item(app(&["shapes"]), SymbolNs::Value, "Point")),
            Role::Import,
            NameKind::Type
        )
    );
    assert_eq!(
        main.classify(nth(MAIN, "std", 0)),
        Classified::Unnamed(Unnamed::Package)
    );
    // Interpolations: the root and a field after it.
    assert_eq!(
        w("total", 1),
        (
            local("main", "total", 0),
            None,
            Role::InterpRoot,
            NameKind::Local
        )
    );
    assert_eq!(
        w("x", 5),
        (field("x"), None, Role::InterpField, NameKind::Field)
    );
}

/// The rows of F §5.3 in [`SHAPES`].
fn clean_shapes(shapes_index: &ModuleIndex) {
    let s = |word, n| at(shapes_index, SHAPES, word, n);
    // A record type's name is the type, with its constructor as `also`.
    assert_eq!(
        s("Point", 0),
        (
            Symbol::item(app(&["shapes"]), SymbolNs::Type, "Point"),
            Some(Symbol::item(app(&["shapes"]), SymbolNs::Value, "Point")),
            Role::Decl,
            NameKind::Type
        )
    );
    assert_eq!(s("x", 0), (field("x"), None, Role::Decl, NameKind::Field));
    assert_eq!(s("x", 1).2, Role::FieldInit);
    assert_eq!(
        s("x", 2),
        (field("x"), None, Role::FieldRead, NameKind::Field)
    );
    assert_eq!(
        s("area", 0),
        (
            Symbol::interface_fn(app(&["shapes"]), "Area", "area"),
            None,
            Role::Decl,
            NameKind::InterfaceFn
        )
    );
    assert_eq!(
        shapes_index.classify(nth(SHAPES, "Self", 0)),
        Classified::Unnamed(Unnamed::SelfType)
    );
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs the front end, which needs JIT code; Windows is M6 (D-545, D-641)"
)]
fn classification_on_clean_text() {
    let (_temp, analyzed) = shapes_package("clean", MAIN);
    assert!(
        !has_errors(&analyzed.snapshot),
        "{:?}",
        analyzed.snapshot.baseline()
    );
    let index = analyzed.index();
    let seen = covered(index, "clean");
    assert!(seen.unknown.is_empty(), "{:#?}", seen.unknown);
    let main = module_named(index, &app(&[]));
    clean_main(main);
    clean_main_paths(main);
    let shapes_index = module_named(index, &app(&["shapes"]));
    clean_shapes(shapes_index);

    // References cross modules, and count both meanings of a pun.
    let refs = index.references(&field("x"), &CancelToken::new()).unwrap();
    assert_eq!(refs.partial, None);
    let in_main = refs
        .hits
        .iter()
        .filter(|h| h.module.path == app(&[]))
        .count();
    let in_shapes = refs
        .hits
        .iter()
        .filter(|h| h.module.file == shapes_index.file)
        .count();
    assert_eq!((in_main, in_shapes), (3, 4), "{:#?}", refs.hits);
    assert!(refs.hits.iter().any(|h| h.occurrence.role.is_decl()));
    let main_x = local("main", "x", 0);
    let ids = (main.id_of(&field("x")), main.id_of(&main_x));
    let punned = main.at(nth(MAIN, "x", 3)).unwrap();
    assert!(punned.names(ids.0.unwrap()) && punned.names(ids.1.unwrap()));
    assert_eq!(main.occurrences_of(&main_x).count(), 3);

    // The `show` of an interpolation is an implicit occurrence at its
    // braces.
    let brace = nth(MAIN, "n", 2).start().saturating_sub(1);
    let shown = main.implicit_at(brace);
    let show = shown.first().expect("the show of {n}");
    assert_eq!(show.role, Role::Implicit(ImplicitKind::Show));
    // `Int`'s `show` is a function that satisfies `Show`.
    let entry = main.entry(show.symbol).unwrap();
    assert_eq!(
        (entry.symbol.text(), entry.kind),
        ("show", NameKind::Function)
    );
    assert!(main.at(show.range).is_none());

    let file = analyzed
        .snapshot
        .module(main.module)
        .unwrap()
        .file()
        .unwrap();
    assert!(std::ptr::eq(index.of_file(file).unwrap(), main));
}

const TYPED: &str = "//! Root.

import app::shapes::{Point}

fn main() {
  let p = Point { x: 1, y: 2 }
  let s: String = p.x
  let q = missing(p)
  let r = q.x
  let _ = (s, r)
}
";

#[test]
#[cfg_attr(
    windows,
    ignore = "runs the front end, which needs JIT code; Windows is M6 (D-545, D-641)"
)]
fn classification_on_a_type_error() {
    let (_temp, analyzed) = shapes_package("typed", TYPED);
    assert!(has_errors(&analyzed.snapshot), "the text has errors");
    let index = analyzed.index();
    let seen = covered(index, "typed");
    let main = module_named(index, &app(&[]));
    // The errored body keeps its facts: the read of a known type is
    // placed, the one of an unknown type is not.
    assert_eq!(at(main, TYPED, "x", 1).0, field("x"));
    assert_eq!(at(main, TYPED, "s", 0).3, NameKind::Local);
    let unknown: Vec<Unknown> = seen.unknown.iter().map(|(_, _, why, _)| *why).collect();
    assert_eq!(
        unknown,
        [Unknown::UnresolvedPath, Unknown::FieldOfUnknownType]
    );
    // So references of a field named `x` may be partial (F §5.5).
    let refs = index.references(&field("x"), &CancelToken::new()).unwrap();
    let Some(Partial::Unknown { module, name }) = refs.partial else {
        panic!("{:?}", refs.partial);
    };
    assert_eq!(module, main.module);
    assert_eq!(
        (name.range, name.why),
        (nth(TYPED, "x", 2), Unknown::FieldOfUnknownType)
    );
    assert_eq!(index.partial("p"), None);
}

const BROKEN: &str = "//! Root.

import app::shapes::{Point}

fn main() {
  let p = Point { x: 1, y: 2 }
  let n = p.x
  let m = ) n p
  io::println(\"{n}\")
}

fn after(a: Int) -> Int {
  a
}
";

#[test]
#[cfg_attr(
    windows,
    ignore = "runs the front end, which needs JIT code; Windows is M6 (D-545, D-641)"
)]
fn classification_on_a_grammar_error() {
    let (_temp, analyzed) = shapes_package("broken", BROKEN);
    assert!(
        !analyzed.snapshot.status().broken.is_empty(),
        "the text does not parse"
    );
    let index = analyzed.index();
    let seen = covered(index, "broken");
    let main = module_named(index, &app(&[]));
    // Names around the error are placed; names in it are unknown.
    assert_eq!(at(main, BROKEN, "x", 1).0, field("x"));
    assert_eq!(at(main, BROKEN, "n", 0).0, local("main", "n", 0));
    assert_eq!(at(main, BROKEN, "a", 1).0, local("after", "a", 0));
    assert!(
        seen.unknown.iter().all(|(_, _, why, _)| matches!(
            why,
            Unknown::InErrorNode | Unknown::UnresolvedPath | Unknown::NotRecorded
        )),
        "{:#?}",
        seen.unknown
    );
    assert_ne!(seen.unknown.len(), 0);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs the front end, which needs JIT code; Windows is M6 (D-545, D-641)"
)]
fn scope_helpers() {
    let (_temp, analyzed) = shapes_package("scope", MAIN);
    let snapshot = &analyzed.snapshot;
    let main = snapshot
        .local_modules()
        .find(|m| m.path() == Some(&app(&[])))
        .unwrap();
    let after_p = nth(MAIN, "p", 0).end() + 1;
    let names: Vec<&str> = scope::locals_at(main, after_p)
        .iter()
        .map(|l| snapshot.globals().names.get(l.decl.name))
        .collect();
    assert_eq!(names, ["x"], "`p` is visible after its statement only");
    let end = nth(MAIN, "io", 2).start();
    let names: Vec<&str> = scope::locals_at(main, end)
        .iter()
        .map(|l| snapshot.globals().names.get(l.decl.name))
        .collect();
    assert_eq!(names, ["k", "n", "total", "y", "px", "p", "x"]);
    let first = scope::locals_at(main, end).first().copied().unwrap();
    assert_eq!(
        scope::local_decl(main, first.item, first.slot),
        Some(first.decl)
    );
    let ty = scope::local_fact(main, first.item, first.slot).unwrap().ty;
    let printer = Printer::new(snapshot, main);
    assert_eq!(printer.type_display(ty, None), "Int");
    assert!(!printer.is_unknown(ty));
    assert_eq!(
        scope::body_at(main, nth(MAIN, "T", 0).start()),
        Some(first_item(&analyzed))
    );

    for (name, free) in [("x", false), ("px", false), ("io", false), ("lists", false)] {
        assert_eq!(scope::is_free(snapshot, main, end, name), free, "{name}");
    }
    assert!(
        !scope::is_free(snapshot, main, end, "first"),
        "a function of the module"
    );
    assert!(scope::is_free(snapshot, main, end, "zzz"));
    assert!(
        scope::is_free(snapshot, main, end, "d"),
        "a local of another body"
    );
    assert!(!scope::is_free_in_module(snapshot, main, "d"));
    assert!(scope::is_free_in_module(snapshot, main, "list"));
    assert_eq!(scope::fresh(snapshot, main, end, "count", &[]), "count");
    assert_eq!(scope::fresh(snapshot, main, end, "x", &[]), "x_value");
    let taken = ["x_value".to_owned()];
    assert_eq!(scope::fresh(snapshot, main, end, "x", &taken), "x_value_2");
    assert_eq!(scope::fresh(snapshot, main, end, "type", &[]), "type_value");
}

/// The item `first` of [`MAIN`].
fn first_item(analyzed: &Analyzed) -> cheby_driver::analysis::DefIndex {
    let main = analyzed.index();
    let module = module_named(main, &app(&[]));
    let range = nth(MAIN, "first", 0);
    let Some(Meaning::Item(def)) = module
        .at(range)
        .and_then(|o| module.entry(o.symbol))
        .map(|e| e.meaning)
    else {
        panic!("no item at `first`");
    };
    def.index
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs the front end, which needs JIT code; Windows is M6 (D-545, D-641)"
)]
fn printing_follows_the_reader() {
    let (_temp, analyzed) = shapes_package("print", MAIN);
    let snapshot = &analyzed.snapshot;
    let index = analyzed.index();
    let main = module_named(index, &app(&[]));
    let view = |path: &ModulePath| {
        snapshot
            .local_modules()
            .find(|m| m.path() == Some(path))
            .unwrap()
    };
    let add = main
        .at(nth(MAIN, "add", 0))
        .and_then(|o| main.entry(o.symbol))
        .unwrap();
    let Meaning::Item(add) = add.meaning else {
        panic!("{add:?}");
    };
    // D-1384: the reader's names for what it imports.
    let header = |path: &ModulePath| Printer::new(snapshot, view(path)).item_header(add).unwrap();
    assert_eq!(header(&app(&[])), "pub fn add(a: P, b: P) -> P");
    assert_eq!(
        header(&app(&["shapes"])),
        "pub fn add(a: Point, b: Point) -> Point"
    );
    let total = main
        .at(nth(MAIN, "total", 0))
        .and_then(|o| main.entry(o.symbol))
        .unwrap();
    let Meaning::Local { item, slot } = total.meaning else {
        panic!("{total:?}");
    };
    let ty = scope::local_fact(view(&app(&[])), item, slot).unwrap().ty;
    let source = |path: &ModulePath| Printer::new(snapshot, view(path)).type_source(ty, None);
    let in_main = source(&app(&[])).unwrap();
    assert_eq!((in_main.text.as_str(), in_main.imports.len()), ("P", 0));
    let in_other = source(&app(&["other"])).unwrap();
    assert_eq!(in_other.text, "shapes::Point");
    assert_eq!(in_other.imports, [app(&["shapes"])]);
    let header = Printer::new(snapshot, view(&app(&["other"])));
    let point = snapshot
        .find(&Symbol::item(app(&["shapes"]), SymbolNs::Type, "Point"))
        .unwrap();
    let cheby_driver::analysis::SymbolTarget::Item(point) = point else {
        panic!("{point:?}");
    };
    assert_eq!(
        header.item_header(point).unwrap(),
        "pub type Point {\n  x: Int,\n  y: Int,\n}"
    );
}
