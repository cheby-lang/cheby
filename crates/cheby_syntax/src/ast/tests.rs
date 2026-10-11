//! Tests of the typed views: every node kind has exactly one view, every
//! example resolves through the views, targeted accessors, and accessors
//! on broken trees.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::{FileId, TextRange};

use super::*;
use crate::{Parse, SyntaxKind};

fn parse(text: &str) -> Parse {
    crate::parse(FileId::new(0), Arc::from(text))
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// All `.cheby` files under `dir` (relative to the repository), sorted.
pub(crate) fn cheby_files(dir: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![repo_root().join(dir)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "cheby") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// The first descendant of `parse`'s root with view `N`.
fn find<'t, N: AstNode<'t>>(parse: &'t Parse) -> N {
    parse
        .tree()
        .root()
        .descendants()
        .find_map(N::cast)
        .expect("a node of this view")
}

fn all<'t, N: AstNode<'t>>(parse: &'t Parse) -> Vec<N> {
    parse
        .tree()
        .root()
        .descendants()
        .filter_map(N::cast)
        .collect()
}

#[test]
fn every_node_kind_has_exactly_one_view() {
    let covered: Vec<SyntaxKind> = [
        items::KINDS,
        types::KINDS,
        patterns::KINDS,
        stmts::KINDS,
        exprs::KINDS,
    ]
    .concat();
    for &kind in SyntaxKind::ALL {
        let count = covered.iter().filter(|&&k| k == kind).count();
        let expected = usize::from(kind.is_node() && kind != SyntaxKind::ERROR);
        assert_eq!(count, expected, "{}", kind.name());
    }
}

#[test]
fn category_enums_cover_disjoint_kinds() {
    for &kind in SyntaxKind::ALL {
        let categories = [
            Item::can_cast(kind),
            Type::can_cast(kind),
            Pat::can_cast(kind),
            Expr::can_cast(kind),
        ];
        let count = categories.iter().filter(|&&c| c).count();
        assert!(count <= 1, "{} is in {count} categories", kind.name());
    }
    // Statements are expressions plus four statement kinds.
    assert!(Stmt::can_cast(SyntaxKind::BIN_EXPR));
    assert!(Stmt::can_cast(SyntaxKind::LOCAL_FN));
    assert!(!Stmt::can_cast(SyntaxKind::FN_DECL));
}

/// Calls every accessor of every view of every node of `parse`, checking
/// only that nothing panics and that children are inside their node. The
/// results that must hold for valid code are checked by
/// [`examples_resolve_through_views`].
#[expect(clippy::too_many_lines, reason = "one block per view")]
fn touch_everything(parse: &Parse) {
    let tree = parse.tree();
    let file = parse.source_file();
    let _ = file.module_docs().map(ModuleDoc::content).count();
    let _ = file.inner_attrs().count();
    let _ = file.imports().count();
    for item in file.items() {
        let _ = (item.name(), item.visibility(), item.attrs().count());
        let _ = item.doc_comments().map(DocComment::content).count();
        assert_eq!(item.id().get(tree), Some(item));
    }
    for node in tree.root().descendants() {
        let range = node.text_range();
        let inside = |span: cheby_diag::Span| assert!(range.contains_range(span.range));
        if let Some(n) = Attr::cast(node) {
            let _ = (n.name(), n.args().count());
        }
        if let Some(n) = InnerAttr::cast(node) {
            let _ = (n.name(), n.args().count());
        }
        if let Some(n) = ImportDecl::cast(node) {
            let _ = (n.list(), n.rename().and_then(Rename::name));
            let _ = n.module_path().map(|p| (p.segments().count(), p.last()));
            if let Some(list) = n.list() {
                list.items().for_each(|i| {
                    let _ = (i.name(), i.rename());
                });
            }
        }
        if let Some(n) = FnDecl::cast(node) {
            let _ = (
                n.name(),
                n.type_params(),
                n.ret_type().and_then(RetType::ty),
            );
            let _ = n.body();
            n.params().for_each(|p| {
                let _ = (p.name(), p.ty());
            });
        }
        if let Some(n) = TypeDecl::cast(node) {
            let _ = (n.name(), n.is_exposed(), n.type_params(), n.body());
        }
        if let Some(n) = AliasDecl::cast(node) {
            let _ = (n.name(), n.type_params(), n.ty());
        }
        if let Some(n) = ConstDecl::cast(node) {
            let _ = (n.name(), n.ty(), n.value());
        }
        if let Some(n) = InterfaceDecl::cast(node) {
            let _ = (n.name(), n.bounds(), n.fns().count());
        }
        if let Some(n) = InterfaceFn::cast(node) {
            let _ = (
                n.name(),
                n.param_types().map(|l| l.types().count()),
                n.ret_type(),
            );
        }
        if let Some(n) = TestDecl::cast(node) {
            let _ = (n.name().and_then(StringLit::plain_value), n.body());
        }
        if let Some(n) = Visibility::cast(node) {
            let _ = (n.kind(), n.is_pub());
        }
        if let Some(n) = TypeParam::cast(node) {
            let _ = (n.name(), n.bounds().map(|b| b.interfaces().count()));
        }
        if let Some(n) = InterfaceRef::cast(node) {
            let _ = (n.path(), n.type_args());
        }
        if let Some(n) = Variant::cast(node) {
            let _ = (n.name(), n.fields(), n.doc_comments().count());
        }
        if let Some(n) = RecordField::cast(node) {
            let _ = (n.name(), n.ty());
        }
        if let Some(n) = PathNode::cast(node) {
            let _ = (n.qualifier(), n.interface(), n.name(), n.segments().count());
        }
        if let Some(n) = PathType::cast(node) {
            let _ = (n.path(), n.type_args(), n.is_self());
        }
        if let Some(n) = TupleType::cast(node) {
            let _ = n.elements().count();
        }
        if let Some(n) = FnType::cast(node) {
            let _ = (n.param_types(), n.ret_type());
        }
        if let Some(n) = DynType::cast(node) {
            let _ = n.interface();
        }
        if let Some(n) = Block::cast(node) {
            for stmt in n.stmts() {
                inside(stmt.span());
            }
        }
        if let Some(n) = LetStmt::cast(node) {
            let _ = (n.is_assert(), n.pattern(), n.ty(), n.value(), n.message());
        }
        if let Some(n) = UseStmt::cast(node) {
            let _ = n.value();
            n.binders().for_each(|b| {
                let _ = (b.pattern(), b.ty());
            });
        }
        if let Some(n) = AssertStmt::cast(node) {
            let _ = (n.condition(), n.message().and_then(Message::value));
        }
        if let Some(n) = LocalFn::cast(node) {
            let _ = (
                n.visibility(),
                n.name(),
                n.type_params(),
                n.params().count(),
            );
            let _ = (n.ret_type(), n.body());
        }
        if let Some(n) = Literal::cast(node) {
            match n.kind() {
                Some(LiteralKind::Int(t)) => drop(t.value()),
                Some(LiteralKind::Float(t)) => drop(t.value()),
                Some(LiteralKind::RawString(t)) => drop(t.value()),
                None => {}
            }
        }
        if let Some(n) = StringLit::cast(node) {
            let _ = (n.parts().count(), n.is_terminated(), n.plain_value());
            for part in n.parts() {
                if let StringPart::Escape(escape) = part {
                    let _ = escape.value();
                }
            }
        }
        if let Some(n) = Interpolation::cast(node) {
            let _ = (n.root(), n.fields().count(), n.is_debug());
        }
        if let Some(n) = PathExpr::cast(node) {
            let _ = (n.path(), n.type_args());
        }
        if let Some(n) = RecordExpr::cast(node) {
            let _ = (n.path(), n.entries().count(), n.is_update());
            n.fields().for_each(|f| {
                let _ = (f.name(), f.value(), f.is_punned());
            });
        }
        if let Some(n) = Spread::cast(node) {
            let _ = n.value();
        }
        if let Some(n) = TupleExpr::cast(node) {
            let _ = n.elements().count();
        }
        if let Some(n) = ParenExpr::cast(node) {
            let _ = n.inner();
        }
        if let Some(n) = ListExpr::cast(node) {
            let _ = n.items().count();
        }
        if let Some(n) = Closure::cast(node) {
            let _ = (n.params().count(), n.ret_type(), n.body());
        }
        if let Some(n) = CaseExpr::cast(node) {
            let _ = (n.subjects().count(), n.is_subjectless(), n.arms().count());
        }
        if let Some(n) = CaseArm::cast(node) {
            let _ = (
                n.patterns().count(),
                n.guard().and_then(Guard::condition),
                n.body(),
            );
        }
        if let Some(n) = CondArm::cast(node) {
            let _ = (n.is_default(), n.condition(), n.body());
        }
        if let Some(n) = PanicExpr::cast(node) {
            let _ = n.message();
        }
        if let Some(n) = TodoExpr::cast(node) {
            let _ = n.message();
        }
        if let Some(n) = BinExpr::cast(node) {
            let _ = (n.lhs(), n.rhs(), n.op().map(BinOp::precedence));
        }
        if let Some(n) = PrefixExpr::cast(node) {
            let _ = (n.op(), n.operand(), n.negative_literal());
        }
        if let Some(n) = CallExpr::cast(node) {
            let _ = (n.callee(), n.args().count());
            let _ = n.arg_list().map(|l| l.holes().count());
        }
        if let Some(n) = FieldExpr::cast(node) {
            let _ = (n.receiver(), n.field());
        }
        if let Some(n) = IdentPat::cast(node) {
            let _ = n.name();
        }
        if let Some(n) = LiteralPat::cast(node) {
            let _ = (n.kind(), n.is_negative());
        }
        if let Some(n) = CtorPat::cast(node) {
            let _ = (n.path(), n.fields());
        }
        if let Some(n) = FieldPatList::cast(node) {
            let _ = (n.entries().count(), n.rest());
            n.fields().for_each(|f| {
                let _ = (f.name(), f.pattern());
            });
        }
        if let Some(n) = RestPat::cast(node) {
            let _ = n.name();
        }
        if let Some(n) = ListPat::cast(node) {
            let _ = (n.items().count(), n.rests().count());
        }
        if let Some(n) = ParenPat::cast(node) {
            let _ = n.inner();
        }
        if let Some(n) = OrPat::cast(node) {
            let _ = n.alternatives().count();
        }
        if let Some(n) = AsPat::cast(node) {
            let _ = (n.pattern(), n.name());
        }
        if node.kind() != SyntaxKind::ERROR {
            for category in [
                Item::cast(node).map(AstNode::syntax),
                Type::cast(node).map(AstNode::syntax),
                Pat::cast(node).map(AstNode::syntax),
                Expr::cast(node).map(AstNode::syntax),
                Stmt::cast(node).map(AstNode::syntax),
            ]
            .into_iter()
            .flatten()
            {
                assert_eq!(category, node);
            }
            inside(significant_span(node));
        }
    }
}

/// Collects a problem when `ok` is false.
macro_rules! check {
    ($report:expr, $path:expr, $node:expr, $ok:expr) => {
        if !$ok {
            writeln!(
                $report,
                "{}: {:?}: {}",
                $path.display(),
                $node.syntax(),
                stringify!($ok)
            )
            .unwrap();
        }
    };
}

/// Every example is valid, so every key accessor resolves.
#[test]
#[expect(clippy::too_many_lines, reason = "one block per view")]
fn examples_resolve_through_views() {
    let files = cheby_files("docs/examples");
    assert!(files.len() > 30, "examples found");
    let mut report = String::new();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed = parse(&text);
        touch_everything(&parsed);
        let tree = parsed.tree();
        let root = tree.root();
        let r = &mut report;
        // Top-level nodes are all module header entries or items.
        for node in root.children() {
            let known = InnerAttr::can_cast(node.kind())
                || ImportDecl::can_cast(node.kind())
                || Item::can_cast(node.kind());
            assert!(known, "{}: {node:?}", path.display());
        }
        let mut docs = Vec::new();
        for node in root.descendants() {
            assert_ne!(node.kind(), SyntaxKind::ERROR, "{}", path.display());
            if let Some(n) = FnDecl::cast(node) {
                check!(r, path, n, n.name().is_some());
                check!(r, path, n, n.param_list().is_some());
                check!(r, path, n, n.params().all(|p| p.name().is_some()));
                check!(r, path, n, n.params().all(|p| p.ty().is_some()));
                check!(r, path, n, n.ret_type().is_none_or(|t| t.ty().is_some()));
                let external = n
                    .attrs()
                    .any(|a| a.name().is_some_and(|i| i.text() == "external"));
                check!(r, path, n, n.body().is_some() || external);
                docs.extend(n.doc_comments());
            }
            if let Some(n) = TypeDecl::cast(node) {
                check!(r, path, n, n.name().is_some());
                docs.extend(n.doc_comments());
            }
            if let Some(n) = AliasDecl::cast(node) {
                check!(r, path, n, n.name().is_some() && n.ty().is_some());
                docs.extend(n.doc_comments());
            }
            if let Some(n) = ConstDecl::cast(node) {
                check!(r, path, n, n.name().is_some());
                check!(r, path, n, n.ty().is_some() && n.value().is_some());
                docs.extend(n.doc_comments());
            }
            if let Some(n) = InterfaceDecl::cast(node) {
                check!(r, path, n, n.name().is_some());
                docs.extend(n.doc_comments());
            }
            if let Some(n) = InterfaceFn::cast(node) {
                check!(r, path, n, n.name().is_some() && n.param_types().is_some());
                docs.extend(n.doc_comments());
            }
            if let Some(n) = TestDecl::cast(node) {
                check!(r, path, n, n.body().is_some());
                check!(
                    r,
                    path,
                    n,
                    n.name().and_then(StringLit::plain_value).is_some()
                );
                docs.extend(n.doc_comments());
            }
            if let Some(n) = Variant::cast(node) {
                check!(r, path, n, n.name().is_some());
                docs.extend(n.doc_comments());
            }
            if let Some(n) = RecordField::cast(node) {
                check!(r, path, n, n.name().is_some() && n.ty().is_some());
                docs.extend(n.doc_comments());
            }
            if let Some(n) = Attr::cast(node) {
                check!(r, path, n, n.name().is_some());
            }
            if let Some(n) = InnerAttr::cast(node) {
                check!(r, path, n, n.name().is_some());
            }
            if let Some(n) = ImportDecl::cast(node) {
                check!(
                    r,
                    path,
                    n,
                    n.module_path().and_then(ModulePath::last).is_some()
                );
            }
            if let Some(n) = ImportItem::cast(node) {
                check!(r, path, n, n.name().is_some());
            }
            if let Some(n) = TypeParam::cast(node) {
                check!(r, path, n, n.name().is_some());
            }
            if let Some(n) = InterfaceRef::cast(node) {
                check!(r, path, n, n.path().and_then(PathNode::name).is_some());
            }
            if let Some(n) = PathType::cast(node) {
                check!(r, path, n, n.path().and_then(PathNode::name).is_some());
            }
            if let Some(n) = TupleType::cast(node) {
                check!(r, path, n, n.elements().count() >= 2);
            }
            if let Some(n) = FnType::cast(node) {
                check!(r, path, n, n.param_types().is_some());
            }
            if let Some(n) = DynType::cast(node) {
                check!(r, path, n, n.interface().is_some());
            }
            if let Some(n) = LocalFn::cast(node) {
                check!(r, path, n, n.name().is_some() && n.body().is_some());
                check!(r, path, n, n.params().all(|p| p.pattern().is_some()));
            }
            if let Some(n) = Closure::cast(node) {
                check!(r, path, n, n.param_list().is_some() && n.body().is_some());
            }
            if let Some(n) = LetStmt::cast(node) {
                check!(r, path, n, n.pattern().is_some() && n.value().is_some());
            }
            if let Some(n) = UseStmt::cast(node) {
                check!(r, path, n, n.value().is_some());
                check!(r, path, n, n.binders().all(|b| b.pattern().is_some()));
            }
            if let Some(n) = AssertStmt::cast(node) {
                check!(r, path, n, n.condition().is_some());
            }
            if let Some(n) = Message::cast(node) {
                check!(r, path, n, n.value().is_some());
            }
            if let Some(n) = CallExpr::cast(node) {
                check!(r, path, n, n.callee().is_some() && n.arg_list().is_some());
            }
            if let Some(n) = BinExpr::cast(node) {
                check!(r, path, n, n.lhs().is_some() && n.rhs().is_some());
                check!(r, path, n, n.op().is_some());
                check!(r, path, n, n.lhs() != n.rhs());
            }
            if let Some(n) = PrefixExpr::cast(node) {
                check!(r, path, n, n.op().is_some() && n.operand().is_some());
            }
            if let Some(n) = FieldExpr::cast(node) {
                check!(r, path, n, n.receiver().is_some() && n.field().is_some());
            }
            if let Some(n) = PathExpr::cast(node) {
                check!(r, path, n, n.path().and_then(PathNode::name).is_some());
            }
            if let Some(n) = RecordExpr::cast(node) {
                check!(r, path, n, n.path().is_some());
                check!(r, path, n, n.fields().all(|f| f.name().is_some()));
                check!(
                    r,
                    path,
                    n,
                    n.fields().all(|f| f.is_punned() == f.value().is_none())
                );
            }
            if let Some(n) = Spread::cast(node) {
                check!(r, path, n, n.value().is_some());
            }
            if let Some(n) = ParenExpr::cast(node) {
                check!(r, path, n, n.inner().is_some());
            }
            if let Some(n) = TupleExpr::cast(node) {
                check!(r, path, n, n.elements().count() >= 2);
            }
            if let Some(n) = Literal::cast(node) {
                let value = match n.kind() {
                    Some(LiteralKind::Int(t)) => t.value().is_some(),
                    Some(LiteralKind::Float(t)) => t.value().is_some(),
                    Some(LiteralKind::RawString(_)) => true,
                    None => false,
                };
                check!(r, path, n, value);
            }
            if let Some(n) = StringLit::cast(node) {
                check!(r, path, n, n.is_terminated());
                check!(
                    r,
                    path,
                    n,
                    n.plain_value().is_some() != n.has_interpolation()
                );
            }
            if let Some(n) = Interpolation::cast(node) {
                check!(r, path, n, n.root().is_some());
            }
            if let Some(n) = CaseExpr::cast(node) {
                check!(r, path, n, n.arms().next().is_some());
                check!(
                    r,
                    path,
                    n,
                    n.is_subjectless() == n.subjects().next().is_none()
                );
            }
            if let Some(n) = CaseArm::cast(node) {
                check!(
                    r,
                    path,
                    n,
                    n.patterns().next().is_some() && n.body().is_some()
                );
            }
            if let Some(n) = CondArm::cast(node) {
                check!(r, path, n, n.body().is_some());
                check!(r, path, n, n.is_default() != n.condition().is_some());
            }
            if let Some(n) = Guard::cast(node) {
                check!(r, path, n, n.condition().is_some());
            }
            if let Some(n) = ArgList::cast(node) {
                check!(r, path, n, n.holes().count() <= 1);
            }
            if let Some(n) = IdentPat::cast(node) {
                check!(r, path, n, n.name().is_some());
            }
            if let Some(n) = LiteralPat::cast(node) {
                check!(r, path, n, n.kind().is_some());
            }
            if let Some(n) = CtorPat::cast(node) {
                check!(r, path, n, n.path().and_then(PathNode::name).is_some());
            }
            if let Some(n) = FieldPat::cast(node) {
                check!(r, path, n, n.name().is_some());
            }
            if let Some(n) = AsPat::cast(node) {
                check!(r, path, n, n.pattern().is_some() && n.name().is_some());
            }
            if let Some(n) = OrPat::cast(node) {
                check!(r, path, n, n.alternatives().count() >= 2);
            }
            if let Some(n) = ParenPat::cast(node) {
                check!(r, path, n, n.inner().is_some());
            }
            if let Some(n) = Hole::cast(node) {
                check!(
                    r,
                    path,
                    n,
                    node.parent().is_some_and(|p| ArgList::can_cast(p.kind()))
                );
            }
            // Every child of a block, list, tuple, … that is not a token
            // resolves through its category.
            if let Some(n) = Block::cast(node) {
                check!(r, path, n, n.stmts().count() == node.children().count());
            }
        }
        // Every `///` in an example documents something.
        let doc_tokens = root
            .tokens()
            .filter(|t| t.kind() == SyntaxKind::DOC_COMMENT)
            .count();
        assert_eq!(docs.len(), doc_tokens, "{}", path.display());
    }
    assert!(report.is_empty(), "{report}");
}

#[test]
fn ast_ids_resolve_in_their_tree() {
    let parsed = parse("fn a() {}\ntype B { C }\n");
    let tree = parsed.tree();
    let f: FnDecl<'_> = find(&parsed);
    let id: AstId<FnDecl<'static>> = f.id();
    assert_eq!(id.get(tree), Some(f));
    assert_eq!(id.node_id(), f.syntax().id());
    let variant: Variant<'_> = find(&parsed);
    let wrong = AstId::<FnDecl<'static>>::new(variant.syntax().id());
    assert_eq!(wrong.get(tree), None);
    let item_id = Item::Fn(f).id();
    assert_eq!(item_id.get(tree), Some(Item::Fn(f)));
}

#[test]
fn source_file_header_and_items() {
    let parsed = parse(
        "//! Module.\n@!target(js)\nimport std::io\nimport a::{B, c as d}\n/// Doc.\n@inline\npub fn f() {}\nconst x: Int = 1\n",
    );
    let file = parsed.source_file();
    assert_eq!(
        file.module_docs()
            .map(ModuleDoc::content)
            .collect::<Vec<_>>(),
        [" Module."]
    );
    let inner = file.inner_attrs().next().unwrap();
    assert_eq!(inner.name().unwrap().text(), "target");
    assert!(matches!(inner.args().next(), Some(AttrArg::Name(n)) if n.text() == "js"));
    let imports: Vec<_> = file.imports().collect();
    assert_eq!(imports.len(), 2);
    let segments: Vec<_> = imports[0]
        .module_path()
        .unwrap()
        .segments()
        .map(AstToken::text)
        .collect();
    assert_eq!(segments, ["std", "io"]);
    let list: Vec<_> = imports[1].list().unwrap().items().collect();
    assert_eq!(list[0].name().unwrap().text(), "B");
    assert!(list[0].name().unwrap().is_type_name());
    assert_eq!(list[1].rename().unwrap().name().unwrap().text(), "d");
    let items: Vec<_> = file.items().collect();
    assert_eq!(items.len(), 2);
    let Item::Fn(f) = items[0] else {
        panic!("a function")
    };
    assert_eq!(f.name().unwrap().text(), "f");
    assert_eq!(
        f.doc_comments()
            .map(DocComment::content)
            .collect::<Vec<_>>(),
        [" Doc."]
    );
    assert_eq!(f.attrs().next().unwrap().name().unwrap().text(), "inline");
    assert_eq!(f.visibility().unwrap().kind(), Some(VisibilityKind::Pub));
    assert_eq!(items[1].name().unwrap().text(), "x");
}

#[test]
fn item_keywords() {
    let parsed = parse(
        "pub fn f() {}
const x: Int = 1
priv type T {
  T,
}
type A = Int
interface I {
}
test \"t\" {
}
",
    );
    let keywords: Vec<(String, u32)> = parsed
        .source_file()
        .items()
        .map(|item| {
            let keyword = item.keyword().unwrap();
            (keyword.text().to_owned(), keyword.span().range.start())
        })
        .collect();
    assert_eq!(
        keywords,
        [
            ("fn".to_owned(), 4),
            ("const".to_owned(), 14),
            ("type".to_owned(), 36),
            ("type".to_owned(), 52),
            ("interface".to_owned(), 65),
            ("test".to_owned(), 81),
        ]
    );
}

#[test]
fn type_declarations() {
    let parsed = parse(
        "pub exposed type Shape<T> {\n  /// A dot.\n  Dot,\n  Circle(Float),\n  Rect { w: T },\n}\ntype P { x: Int }\ntype Ext\ntype A = (Int, B)\n",
    );
    let decls: Vec<TypeDecl<'_>> = all(&parsed);
    assert!(decls[0].is_exposed());
    assert_eq!(decls[0].type_params().unwrap().params().count(), 1);
    let Some(TypeBody::Variants(variants)) = decls[0].body() else {
        panic!("variants")
    };
    let variants: Vec<_> = variants.variants().collect();
    assert_eq!(variants[0].doc_comments().count(), 1);
    assert!(variants[0].fields().is_none());
    assert!(
        matches!(variants[1].fields(), Some(VariantFields::Positional(l)) if l.types().count() == 1)
    );
    assert!(matches!(
        variants[2].fields(),
        Some(VariantFields::Record(_))
    ));
    assert!(matches!(decls[1].body(), Some(TypeBody::Record(r)) if r.fields().count() == 1));
    assert!(decls[2].body().is_none());
    let alias: AliasDecl<'_> = find(&parsed);
    assert!(matches!(alias.ty(), Some(Type::Tuple(t)) if t.elements().count() == 2));
}

#[test]
fn paths() {
    let parsed =
        parse("fn f() {\n  a\n  m::B\n  Show::show\n  shape::Shape::area\n  list::new::<Int>\n}\n");
    let paths: Vec<PathNode<'_>> = all(&parsed);
    let summary: Vec<_> = paths
        .iter()
        .map(|p| {
            (
                p.qualifier().map(AstToken::text),
                p.interface().map(AstToken::text),
                p.name().map(AstToken::text),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (None, None, Some("a")),
            (Some("m"), None, Some("B")),
            (None, Some("Show"), Some("show")),
            (Some("shape"), Some("Shape"), Some("area")),
            (Some("list"), None, Some("new")),
            (None, None, Some("Int")),
        ]
    );
    let turbofish = all::<PathExpr<'_>>(&parsed)[4];
    assert_eq!(turbofish.type_args().unwrap().types().count(), 1);
    let self_type = parse("interface I {\n  fn f(Self) -> Self\n}\n");
    assert!(find::<PathType<'_>>(&self_type).is_self());
}

#[test]
fn expressions() {
    let parsed = parse(
        "fn f() {\n  a + b * c\n  -1\n  -x\n  !ok\n  g(x, _)\n  p.name\n  P { x, y: 1 }\n  P { ..p, x: 2 }\n  [1, ..xs]\n  (a, b)\n  (a)\n  - 2\n  -(3)\n}\n",
    );
    let bins: Vec<BinExpr<'_>> = all(&parsed);
    assert_eq!(bins[0].op(), Some(BinOp::Add));
    assert!(matches!(bins[0].rhs(), Some(Expr::Binary(b)) if b.op() == Some(BinOp::Mul)));
    assert_eq!(BinOp::Mul.precedence(), 10);
    assert!(BinOp::Le.is_comparison());
    assert_eq!(BinOp::Pipe.text(), "|>");
    let prefixes: Vec<PrefixExpr<'_>> = all(&parsed);
    assert!(prefixes[0].negative_literal().is_some());
    assert!(prefixes[1].negative_literal().is_none());
    assert_eq!(prefixes[2].op(), Some(PrefixOp::Not));
    // `-1` has an empty gap. `- 2` negates a number but is not a negative
    // literal (E0056), and `-(3)` negates a parenthesized expression.
    assert!(prefixes[0].minus_gap().is_some_and(TextRange::is_empty));
    assert!(prefixes[3].negative_literal().is_none());
    assert!(prefixes[3].negated_number().is_some());
    assert_eq!(prefixes[3].minus_gap().map(TextRange::len), Some(1));
    assert!(prefixes[4].negated_number().is_none());
    assert!(prefixes[4].minus_gap().is_none());
    let call: CallExpr<'_> = find(&parsed);
    let args: Vec<_> = call.args().collect();
    assert!(matches!(args[0], Arg::Expr(Expr::Path(_))));
    assert!(matches!(args[1], Arg::Hole(_)));
    let field: FieldExpr<'_> = find(&parsed);
    assert_eq!(field.field().unwrap().text(), "name");
    let records: Vec<RecordExpr<'_>> = all(&parsed);
    assert!(!records[0].is_update());
    let fields: Vec<_> = records[0].fields().collect();
    assert!(fields[0].is_punned() && fields[0].value().is_none());
    assert!(!fields[1].is_punned() && fields[1].value().is_some());
    assert!(records[1].is_update());
    assert!(matches!(
        records[1].entries().next(),
        Some(RecordEntry::Spread(_))
    ));
    let list: ListExpr<'_> = find(&parsed);
    let items: Vec<_> = list.items().collect();
    assert!(matches!(items[0], ListItem::Expr(Expr::Literal(_))));
    assert!(matches!(items[1], ListItem::Spread(_)));
    assert_eq!(find::<TupleExpr<'_>>(&parsed).elements().count(), 2);
    assert!(find::<ParenExpr<'_>>(&parsed).inner().is_some());
}

#[test]
fn strings_and_literals() {
    let parsed = parse(
        "fn f() {\n  \"a\\n{b.c:?} d\"\n  \"plain\\u{41}\"\n  r#\"raw\"#\n  0x1F\n  1.5\n}\n",
    );
    let strings: Vec<StringLit<'_>> = all(&parsed);
    let parts: Vec<_> = strings[0].parts().collect();
    assert_eq!(parts.len(), 4);
    assert!(matches!(parts[1], StringPart::Escape(e) if e.value() == Ok('\n')));
    let StringPart::Interpolation(interp) = parts[2] else {
        panic!("an interpolation")
    };
    assert_eq!(interp.root().unwrap().text(), "b");
    assert_eq!(
        interp.fields().map(AstToken::text).collect::<Vec<_>>(),
        ["c"]
    );
    assert!(interp.is_debug());
    assert_eq!(strings[0].plain_value(), None);
    assert_eq!(strings[1].plain_value().as_deref(), Some("plainA"));
    let literals: Vec<Literal<'_>> = all(&parsed);
    assert!(matches!(literals[0].kind(), Some(LiteralKind::RawString(r)) if r.value() == "raw"));
    assert!(matches!(literals[1].kind(), Some(LiteralKind::Int(i)) if i.value() == Some(31)));
    assert!(matches!(literals[2].kind(), Some(LiteralKind::Float(f)) if f.value() == Some(1.5)));
}

#[test]
fn statements_and_case() {
    let parsed = parse(
        "fn f() {\n  let assert Some(x): Option<Int> = y as \"m\"\n  use a, (b, c) <- g()\n  assert x as \"n\"\n  fn h(n) { n }\n  case x, y {\n    Some(1) | None, _ when ok => 1\n    _, _ => 2\n  }\n  case {\n    x > 1 => 1\n    _ => 2\n  }\n}\n",
    );
    let block: Block<'_> = find(&parsed);
    let stmts: Vec<_> = block.stmts().collect();
    assert_eq!(stmts.len(), 6);
    let Stmt::Let(stmt) = stmts[0] else {
        panic!("a let")
    };
    assert!(stmt.is_assert());
    assert!(matches!(stmt.pattern(), Some(Pat::Ctor(_))));
    assert!(matches!(stmt.ty(), Some(Type::Path(_))));
    assert!(matches!(stmt.value(), Some(Expr::Path(_))));
    assert!(stmt.message().and_then(Message::value).is_some());
    let Stmt::Use(stmt) = stmts[1] else {
        panic!("a use")
    };
    assert_eq!(stmt.binders().count(), 2);
    assert!(matches!(stmt.value(), Some(Expr::Call(_))));
    assert!(matches!(stmts[2], Stmt::Assert(a) if a.message().is_some()));
    assert!(matches!(stmts[3], Stmt::LocalFn(l) if l.name().is_some()));
    let cases: Vec<CaseExpr<'_>> = all(&parsed);
    assert!(!cases[0].is_subjectless());
    assert_eq!(cases[0].subjects().count(), 2);
    let arms: Vec<_> = cases[0].arms().collect();
    let CaseArmKind::Case(arm) = arms[0] else {
        panic!("a case arm")
    };
    assert_eq!(arm.patterns().count(), 2);
    assert!(matches!(arm.patterns().next(), Some(Pat::Or(o)) if o.alternatives().count() == 2));
    assert!(arm.guard().and_then(Guard::condition).is_some());
    assert!(matches!(arm.body(), Some(Expr::Literal(_))));
    assert!(cases[1].is_subjectless());
    let conds: Vec<CondArm<'_>> = all(&parsed);
    assert!(!conds[0].is_default() && conds[0].condition().is_some());
    assert!(conds[1].is_default() && conds[1].condition().is_none());
    assert!(conds[1].body().is_some());
}

#[test]
fn patterns() {
    let parsed = parse(
        "fn f() {\n  case x {\n    -1 => 1\n    \"s\" => 2\n    P { a, b: _, .. } => 3\n    [h, ..t] => 4\n    (a, b) as pair => 5\n    (p) => 6\n    - 7 => 7\n  }\n}\n",
    );
    let lits: Vec<LiteralPat<'_>> = all(&parsed);
    assert!(lits[0].is_negative());
    assert!(lits[0].minus_gap().is_some_and(TextRange::is_empty));
    assert!(lits[1].minus_gap().is_none());
    assert_eq!(lits[2].minus_gap().map(TextRange::len), Some(1));
    assert!(matches!(lits[0].kind(), Some(LiteralPatKind::Int(_))));
    assert!(matches!(lits[1].kind(), Some(LiteralPatKind::String(_))));
    let ctor: CtorPat<'_> = find(&parsed);
    let Some(CtorPatFields::Named(fields)) = ctor.fields() else {
        panic!("named fields")
    };
    assert_eq!(fields.fields().count(), 2);
    assert!(fields.rest().is_some());
    let entries: Vec<_> = fields.entries().collect();
    assert!(matches!(entries[0], FieldPatEntry::Field(f) if f.pattern().is_none()));
    assert!(matches!(entries[2], FieldPatEntry::Rest(_)));
    let list: ListPat<'_> = find(&parsed);
    assert!(matches!(list.items().nth(1), Some(ListPatItem::Rest(r)) if r.name().is_some()));
    let as_pat: AsPat<'_> = find(&parsed);
    assert_eq!(as_pat.name().unwrap().text(), "pair");
    assert!(matches!(as_pat.pattern(), Some(Pat::Tuple(_))));
    assert!(find::<ParenPat<'_>>(&parsed).inner().is_some());
}

#[test]
fn broken_trees_give_none() {
    let parsed = parse("fn (\n");
    let f: FnDecl<'_> = find(&parsed);
    assert!(f.name().is_none());
    assert!(f.body().is_none());
    let parsed = parse("fn f() {\n  let = \n  a +\n  g(\n}\n");
    touch_everything(&parsed);
    let lets: Vec<LetStmt<'_>> = all(&parsed);
    assert!(lets[0].pattern().is_none());
}

const fn xorshift(mut state: u64) -> u64 {
    state ^= state.wrapping_shl(13);
    state ^= state.wrapping_shr(7);
    state ^= state.wrapping_shl(17);
    state
}

/// Fragments of Cheby glued at random never make an accessor panic.
#[test]
fn accessors_never_panic_on_fragments() {
    const FRAGMENTS: &[&str] = &[
        "fn",
        "f",
        "(",
        ")",
        "{",
        "}",
        "[",
        "]",
        "<",
        ">",
        ",",
        ":",
        "::",
        "=",
        "=>",
        "->",
        "<-",
        "|>",
        "|",
        "_",
        "..",
        ".",
        "case",
        "let",
        "assert",
        "use",
        "type",
        "T",
        "as",
        "\"s{x}\"",
        "1",
        "-",
        "!",
        "\n",
        " ",
        "@a",
        "@!t",
        "import",
        "pub",
        "exposed",
        "when",
        "///d\n",
        "//!m\n",
        "0.5",
        "dyn",
        "interface",
        "const",
        "test",
        "panic",
        "todo",
        "r\"x\"",
    ];
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    for _ in 0..1500 {
        let mut text = String::new();
        state = xorshift(state);
        for _ in 0..state % 40 {
            state = xorshift(state);
            let index = usize::try_from(state).unwrap() % FRAGMENTS.len();
            text.push_str(FRAGMENTS[index]);
            text.push(' ');
        }
        let parsed = parse(&text);
        touch_everything(&parsed);
    }
    for path in cheby_files("tests/ui") {
        let text = std::fs::read_to_string(&path).unwrap();
        touch_everything(&parse(&text));
    }
}
