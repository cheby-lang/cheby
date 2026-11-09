//! A file's outline, for `textDocument/documentSymbol` (X §5.1).
//!
//! The outline is read from the syntax tree alone (level L0, X §1.3), so it
//! works on any text, broken or not. It lists the module's items in
//! source order, as `ResolvedModule::items` does: functions, types with
//! their variants and fields, aliases, constants, interfaces with their
//! functions, and tests. Local functions and imports are left out (X §5.1).
//! An item with no name yet, such as a lone `fn` while typing, is skipped.

use cheby_driver::analysis::TextRange;
use cheby_syntax::ast::{
    AstNode, AstToken, InterfaceDecl, Item, RecordFieldList, TypeBody, TypeDecl, Variant,
    VariantFields,
};
use cheby_syntax::{SyntaxKind, SyntaxNode};

use super::cursor::is_significant;

/// What an outline entry declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutlineKind {
    /// `fn`.
    Function,
    /// A type with variants.
    Enum,
    /// A record type.
    Struct,
    /// A type with no body: opaque or external.
    Opaque,
    /// `type A = T`.
    Alias,
    /// A variant of a type.
    Variant,
    /// A named field of a record type or variant.
    Field,
    /// `const`.
    Constant,
    /// `interface`.
    Interface,
    /// A function of an interface.
    InterfaceFn,
    /// `test "name"`.
    Test,
}

/// One entry of a file's outline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutlineItem {
    /// The declared name. A test's is its decoded string.
    pub name: String,
    /// What it declares.
    pub kind: OutlineKind,
    /// The signature or type text, on one line, if the kind has one.
    pub detail: Option<String>,
    /// The whole declaration, with its doc comments and attributes.
    pub range: TextRange,
    /// The name, or a test's string literal. Inside `range`.
    pub selection: TextRange,
    /// The declaration carries `@deprecated`.
    pub deprecated: bool,
    /// Variants, fields and interface functions, in source order.
    pub children: Vec<OutlineItem>,
}

/// The outline of the file whose tree is `root`, in source order.
#[must_use]
pub(crate) fn outline(root: SyntaxNode<'_>) -> Vec<OutlineItem> {
    root.children()
        .filter_map(Item::cast)
        .filter_map(item)
        .collect()
}

/// The entry of one top-level item.
fn item(item: Item<'_>) -> Option<OutlineItem> {
    let deprecated = item
        .attrs()
        .any(|attr| attr.name().is_some_and(|name| name.text() == "deprecated"));
    let node = item.syntax();
    let mut entry = match item {
        Item::Fn(f) => {
            let name = f.name()?.syntax();
            let detail = join(&[
                f.param_list().map(AstNode::syntax),
                f.ret_type().map(AstNode::syntax),
            ]);
            entry(
                node,
                name.text(),
                name.text_range(),
                OutlineKind::Function,
                detail,
            )
        }
        Item::Type(t) => type_entry(t)?,
        Item::Alias(a) => {
            let name = a.name()?.syntax();
            let detail = a.ty().map(|ty| compact(ty.syntax()));
            entry(
                node,
                name.text(),
                name.text_range(),
                OutlineKind::Alias,
                detail,
            )
        }
        Item::Const(c) => {
            let name = c.name()?.syntax();
            let detail = c.ty().map(|ty| compact(ty.syntax()));
            entry(
                node,
                name.text(),
                name.text_range(),
                OutlineKind::Constant,
                detail,
            )
        }
        Item::Interface(i) => interface_entry(i)?,
        Item::Test(t) => {
            let lit = t.name()?;
            // The decoded value, as `cheby test` shows and matches it
            // (X §0 item 4). A name that cannot be decoded keeps its
            // source text, since an outline name must not be empty.
            let name = lit
                .plain_value()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| lit.syntax().text().to_owned());
            let selection = lit.significant_span().range;
            entry(node, &name, selection, OutlineKind::Test, None)
        }
    };
    entry.deprecated = deprecated;
    Some(entry)
}

/// The entry of a type declaration, with its variants or fields.
fn type_entry(t: TypeDecl<'_>) -> Option<OutlineItem> {
    let name = t.name()?.syntax();
    let (kind, children) = match t.body() {
        Some(TypeBody::Variants(list)) => (
            OutlineKind::Enum,
            list.variants().filter_map(variant).collect(),
        ),
        Some(TypeBody::Record(fields)) => (OutlineKind::Struct, record_fields(fields)),
        None => (OutlineKind::Opaque, Vec::new()),
    };
    let mut e = entry(t.syntax(), name.text(), name.text_range(), kind, None);
    e.children = children;
    Some(e)
}

/// The entry of a variant, with the fields of a named variant.
fn variant(v: Variant<'_>) -> Option<OutlineItem> {
    let name = v.name()?.syntax();
    let (detail, children) = match v.fields() {
        Some(VariantFields::Positional(list)) => (Some(compact(list.syntax())), Vec::new()),
        Some(VariantFields::Record(fields)) => (None, record_fields(fields)),
        None => (None, Vec::new()),
    };
    let mut e = entry(
        v.syntax(),
        name.text(),
        name.text_range(),
        OutlineKind::Variant,
        detail,
    );
    e.children = children;
    Some(e)
}

/// The entries of named fields.
fn record_fields(fields: RecordFieldList<'_>) -> Vec<OutlineItem> {
    fields
        .fields()
        .filter_map(|field| {
            let name = field.name()?.syntax();
            let detail = field.ty().map(|ty| compact(ty.syntax()));
            Some(entry(
                field.syntax(),
                name.text(),
                name.text_range(),
                OutlineKind::Field,
                detail,
            ))
        })
        .collect()
}

/// The entry of an interface, with its functions.
fn interface_entry(i: InterfaceDecl<'_>) -> Option<OutlineItem> {
    let name = i.name()?.syntax();
    let children = i
        .fns()
        .filter_map(|f| {
            let fname = f.name()?.syntax();
            let detail = join(&[
                f.param_types().map(AstNode::syntax),
                f.ret_type().map(AstNode::syntax),
            ]);
            Some(entry(
                f.syntax(),
                fname.text(),
                fname.text_range(),
                OutlineKind::InterfaceFn,
                detail,
            ))
        })
        .collect();
    let mut e = entry(
        i.syntax(),
        name.text(),
        name.text_range(),
        OutlineKind::Interface,
        None,
    );
    e.children = children;
    Some(e)
}

/// An entry with no children, over `node`, whose range holds `selection`.
fn entry(
    node: SyntaxNode<'_>,
    name: &str,
    selection: TextRange,
    kind: OutlineKind,
    detail: Option<String>,
) -> OutlineItem {
    // Nodes take in their doc comments (`AstNode::span`), and a name is
    // a token of its node, so the range holds the selection. The cover
    // keeps that true even for a tree recovery left odd.
    let range = node.text_range().cover(selection);
    OutlineItem {
        name: name.to_owned(),
        kind,
        detail: detail.filter(|d| !d.is_empty()),
        range,
        selection,
        deprecated: false,
        children: Vec::new(),
    }
}

/// The texts of the nodes there are, by [`compact`], one space apart:
/// `(xs: List<Int>) -> Int`.
fn join(nodes: &[Option<SyntaxNode<'_>>]) -> Option<String> {
    let parts: Vec<String> = nodes.iter().flatten().map(|&n| compact(n)).collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// The text of `node` on one line: comments dropped, each run of spaces
/// and line breaks one space, none just inside brackets (`<` and `>`
/// included) or before a comma,
/// and no trailing comma before a closing bracket.
fn compact(node: SyntaxNode<'_>) -> String {
    let mut out = String::new();
    let mut gap = false;
    for token in node.tokens() {
        let kind = token.kind();
        if !is_significant(kind) {
            gap = true;
            continue;
        }
        // Only types and parameters are printed, so `<` and `>` are
        // brackets here.
        let closer = matches!(
            kind,
            SyntaxKind::R_PAREN | SyntaxKind::R_BRACK | SyntaxKind::R_BRACE | SyntaxKind::GT
        );
        if closer && out.ends_with(',') {
            out.pop();
        }
        let after_opener = out.ends_with(['(', '[', '{', '<']);
        if gap && !out.is_empty() && !after_opener && !closer && kind != SyntaxKind::COMMA {
            out.push(' ');
        }
        gap = false;
        out.push_str(token.text());
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use cheby_driver::analysis::{
        Globals, Overlay, PackageKind, Session, SessionConfig, TextRange, TextSize,
    };

    use super::{OutlineItem, OutlineKind, outline};
    use crate::features::handlers::symbols::tests::doc;

    fn parse(text: &str) -> cheby_syntax::SyntaxTree {
        doc(text).tree
    }

    /// The outline as `(depth, kind, name, detail)` rows.
    fn rows(items: &[OutlineItem]) -> Vec<(usize, OutlineKind, String, Option<String>)> {
        fn walk(
            items: &[OutlineItem],
            depth: usize,
            out: &mut Vec<(usize, OutlineKind, String, Option<String>)>,
        ) {
            for item in items {
                out.push((depth, item.kind, item.name.clone(), item.detail.clone()));
                walk(&item.children, depth.saturating_add(1), out);
            }
        }
        let mut out = Vec::new();
        walk(items, 0, &mut out);
        out
    }

    fn slice(text: &str, range: TextRange) -> &str {
        range.slice(text).expect("a range of the text")
    }

    const ALL: &str = r#"//! A module.

import std::io

/// Adds.
@deprecated("use plus")
pub fn add(a: Int, b: Int) -> Int {
  fn local() -> Int {
    1
  }
  a + b
}

pub type Shape {
  Circle(Float),
  Rect { w: Float, h: Float },
  Empty,
}

pub type Point {
  x: Int,
  y: Int,
}

pub type Handle

type Pair<A> = (A, A)

const limit: Int = 10

pub interface Show {
  fn show(Self) -> String
}

test "adds \"two\"" {
  assert add(1, 1) == 2
}
"#;

    #[test]
    fn every_row_of_the_syntax_map() {
        use OutlineKind as K;
        let tree = parse(ALL);
        let got = rows(&outline(tree.root()));
        let s = |x: &str| x.to_owned();
        let want = vec![
            (0, K::Function, s("add"), Some(s("(a: Int, b: Int) -> Int"))),
            (0, K::Enum, s("Shape"), None),
            (1, K::Variant, s("Circle"), Some(s("(Float)"))),
            (1, K::Variant, s("Rect"), None),
            (2, K::Field, s("w"), Some(s("Float"))),
            (2, K::Field, s("h"), Some(s("Float"))),
            (1, K::Variant, s("Empty"), None),
            (0, K::Struct, s("Point"), None),
            (1, K::Field, s("x"), Some(s("Int"))),
            (1, K::Field, s("y"), Some(s("Int"))),
            (0, K::Opaque, s("Handle"), None),
            (0, K::Alias, s("Pair"), Some(s("(A, A)"))),
            (0, K::Constant, s("limit"), Some(s("Int"))),
            (0, K::Interface, s("Show"), None),
            (1, K::InterfaceFn, s("show"), Some(s("(Self) -> String"))),
            (0, K::Test, s("adds \"two\""), None),
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn ranges_hold_docs_and_selections_hold_names() {
        fn check(items: &[OutlineItem]) {
            for item in items {
                assert!(item.range.contains_range(item.selection), "{item:?}");
                for child in &item.children {
                    assert!(item.range.contains_range(child.range), "{child:?}");
                }
                check(&item.children);
            }
        }
        let tree = parse(ALL);
        let items = outline(tree.root());
        let add = items.first().expect("add");
        assert!(slice(ALL, add.range).starts_with("/// Adds."), "{add:?}");
        assert!(slice(ALL, add.range).ends_with("a + b\n}"));
        assert_eq!(slice(ALL, add.selection), "add");
        assert!(add.deprecated);
        assert!(items.iter().skip(1).all(|i| !i.deprecated));
        let test = items.last().expect("the test");
        assert_eq!(slice(ALL, test.selection), r#""adds \"two\"""#);
        check(&items);
    }

    #[test]
    fn multi_line_signatures_print_on_one_line() {
        let text = "fn f(\n  a: Int, // first\n  b: List<\n    Int,\n  >,\n) -> Int {\n  a\n}\n";
        let tree = parse(text);
        let got = rows(&outline(tree.root()));
        let detail = got.first().and_then(|row| row.3.clone());
        assert_eq!(detail.as_deref(), Some("(a: Int, b: List<Int>) -> Int"));
    }

    #[test]
    fn unnamed_items_are_skipped_and_broken_trees_give_what_they_hold() {
        let text = "fn\n\nfn g() {\n  1\n}\n\ntype\n\nfn h( {\n";
        let tree = parse(text);
        let names: Vec<String> = outline(tree.root()).into_iter().map(|i| i.name).collect();
        assert!(names.contains(&"g".to_owned()), "{names:?}");
        assert!(!names.iter().any(String::is_empty));
    }

    /// The example packages of `docs/examples`.
    pub(crate) fn examples() -> Vec<PathBuf> {
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

    /// The outline names every item of `ResolvedModule::items`, at the
    /// item's name, and nothing else but record fields, on every module of
    /// every example (X §5.1).
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs the front end, which needs JIT code; Windows is M6 (D-545, D-641)"
    )]
    fn the_outline_equals_the_item_list_on_all_examples() {
        let globals = Arc::new(Globals::new());
        let mut modules = 0_usize;
        for root in examples() {
            let mut session = Session::new(SessionConfig {
                root: root.clone(),
                kind: PackageKind::Package,
                target: None,
                globals: Arc::clone(&globals),
            });
            let live = session.analyze_live(&Overlay::default()).expect("a run");
            let snapshot = live.snapshot.expect("a snapshot");
            for module in snapshot.local_modules() {
                let (Some(tree), Some(resolved)) = (module.tree(), module.resolved()) else {
                    panic!("{}: a module with no tree or resolution", root.display());
                };
                let items: BTreeSet<(TextSize, TextSize)> = resolved
                    .items
                    .iter()
                    .map(|item| (item.name_span.range.start(), item.name_span.range.end()))
                    .collect();
                let mut names = BTreeSet::new();
                let mut stack = outline(tree.root());
                while let Some(entry) = stack.pop() {
                    if entry.kind != OutlineKind::Field {
                        let key = (entry.selection.start(), entry.selection.end());
                        assert!(names.insert(key), "{}: {entry:?} twice", root.display());
                    }
                    stack.extend(entry.children);
                }
                assert_eq!(names, items, "{}", root.display());
                modules = modules.saturating_add(1);
            }
        }
        assert!(modules >= 32, "{modules} modules");
    }
}
