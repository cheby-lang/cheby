//! Doc comments of items, read from their module's tree (D-1385). Nothing
//! about them enters an interface.

use cheby_syntax::SyntaxTree;
use cheby_syntax::ast::{
    AstNode, AstToken, InterfaceFn, Item, RecordField, TypeDecl, Variant, doc_text,
};

use crate::ItemDef;

/// The doc comment of `item`, from its module's tree, if it has one with
/// text. The constructor of a single-variant shorthand (§4.3.3) has no
/// declaration of its own, so it gives its type's.
#[must_use]
pub fn item_docs(tree: &SyntaxTree, item: &ItemDef) -> Option<String> {
    let node = tree.node(item.node)?;
    let text = if let Some(decl) = Item::cast(node) {
        doc_text(decl.doc_comments())
    } else if let Some(variant) = Variant::cast(node) {
        doc_text(variant.doc_comments())
    } else if let Some(function) = InterfaceFn::cast(node) {
        doc_text(function.doc_comments())
    } else {
        doc_text(node.ancestors().find_map(TypeDecl::cast)?.doc_comments())
    };
    with_text(text)
}

/// The doc comment of field `name` of the type `item`. Variants may each
/// document a field of that name, and the first with text wins.
#[must_use]
pub fn field_docs(tree: &SyntaxTree, item: &ItemDef, name: &str) -> Option<String> {
    tree.node(item.node)?
        .descendants()
        .filter_map(RecordField::cast)
        .filter(|field| field.name().is_some_and(|ident| ident.text() == name))
        .find_map(|field| with_text(doc_text(field.doc_comments())))
}

/// `text`, unless nothing but white space is in it.
fn with_text(text: String) -> Option<String> {
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cheby_diag::FileId;
    use cheby_iface::{
        DefKind, DepView, Globals, InterfaceSet, ModuleId, ModuleInfo, ModuleTable, PackageId,
        builtin,
    };
    use cheby_package::{ModulePath, Targets};

    use super::*;
    use crate::{ResolveInput, ResolvedModule, StdCatalog, resolve_module};

    const SOURCE: &str = r#"//! Test module.

/// Adds.
/// Twice.
fn add(a: Int, b: Int) -> Int {
  a + b
}

fn bare() {
  Nil
}

/// A limit.
const limit: Int = 3

/// Names.
type Names = List<String>

///
type Blank = List<Int>

/// A shape.
type Shape {
  /// Round.
  Circle {
    /// How wide.
    radius: Float,
    tag: String,
  },
  Dot {
    /// The first text.
    tag: String,
  },
  Square {
    /// The second text.
    tag: String,
  },
}

/// A point.
type Point {
  /// Across.
  x: Float,
  y: Float,
}

/// Can be sized.
interface Sized {
  /// The size.
  fn size(Self) -> Int
  fn other(Self) -> Int
}

/// Checks addition.
test "adds" {
  assert add(1, 2) == 3
}
"#;

    /// The tree of [`SOURCE`] and its resolution, with the names of its
    /// items.
    fn resolved() -> (SyntaxTree, ResolvedModule, Globals) {
        let globals = Globals::new();
        let mut modules = ModuleTable::new();
        let module = modules
            .add(ModuleInfo {
                path: ModulePath::root("app"),
                package: PackageId::LOCAL,
                file: None,
            })
            .unwrap();
        let (tree, errors) =
            cheby_syntax::parse(FileId::new(module.as_u32()), Arc::from(SOURCE)).into_parts();
        assert!(errors.is_empty(), "{errors:?}");
        let set = InterfaceSet::new(builtin::interface(&globals));
        let allowed: [ModuleId; 0] = [];
        let (module, diags) = resolve_module(&ResolveInput {
            module,
            package: PackageId::LOCAL,
            tree: &tree,
            imports: &[],
            targets: &[],
            deps: DepView::new(&set, &allowed),
            modules: &modules,
            globals: &globals,
            std: StdCatalog::default(),
            package_targets: Targets::ALL,
        })
        .unwrap();
        assert!(diags.is_empty(), "{diags:?}");
        (tree, module, globals)
    }

    fn find<'m>(
        module: &'m ResolvedModule,
        globals: &Globals,
        kind: DefKind,
        name: &str,
    ) -> &'m ItemDef {
        module
            .items
            .iter()
            .find(|item| item.kind == kind && globals.names.get(item.name) == name)
            .unwrap()
    }

    #[test]
    fn every_kind_of_item_has_docs() {
        let (tree, module, globals) = resolved();
        let docs = |kind, name| item_docs(&tree, find(&module, &globals, kind, name));
        assert_eq!(docs(DefKind::Fn, "add").unwrap(), "Adds.\nTwice.");
        assert_eq!(docs(DefKind::Const, "limit").unwrap(), "A limit.");
        assert_eq!(docs(DefKind::Alias, "Names").unwrap(), "Names.");
        assert_eq!(docs(DefKind::Type, "Shape").unwrap(), "A shape.");
        assert_eq!(docs(DefKind::Ctor, "Circle").unwrap(), "Round.");
        assert_eq!(docs(DefKind::Interface, "Sized").unwrap(), "Can be sized.");
        assert_eq!(docs(DefKind::InterfaceFn, "size").unwrap(), "The size.");
        assert_eq!(docs(DefKind::Test, "adds").unwrap(), "Checks addition.");
    }

    #[test]
    fn a_shorthand_constructor_has_its_types_docs() {
        let (tree, module, globals) = resolved();
        let point = find(&module, &globals, DefKind::Ctor, "Point");
        assert_eq!(item_docs(&tree, point).unwrap(), "A point.");
    }

    #[test]
    fn no_text_is_no_docs() {
        let (tree, module, globals) = resolved();
        let docs = |kind, name| item_docs(&tree, find(&module, &globals, kind, name));
        assert_eq!(docs(DefKind::Fn, "bare"), None);
        assert_eq!(docs(DefKind::Alias, "Blank"), None);
        assert_eq!(docs(DefKind::Ctor, "Dot"), None);
        assert_eq!(docs(DefKind::InterfaceFn, "other"), None);
    }

    #[test]
    fn fields_have_docs() {
        let (tree, module, globals) = resolved();
        let shape = find(&module, &globals, DefKind::Type, "Shape");
        let point = find(&module, &globals, DefKind::Type, "Point");
        assert_eq!(field_docs(&tree, shape, "radius").unwrap(), "How wide.");
        assert_eq!(field_docs(&tree, point, "x").unwrap(), "Across.");
        assert_eq!(field_docs(&tree, point, "y"), None);
        assert_eq!(field_docs(&tree, shape, "missing"), None);
    }

    #[test]
    fn the_first_field_with_text_wins() {
        let (tree, module, globals) = resolved();
        let shape = find(&module, &globals, DefKind::Type, "Shape");
        // `Circle`'s `tag` has no docs, `Dot`'s has, `Square`'s comes later.
        assert_eq!(field_docs(&tree, shape, "tag").unwrap(), "The first text.");
    }

    #[test]
    fn a_constructor_has_only_its_own_fields() {
        let (tree, module, globals) = resolved();
        let square = find(&module, &globals, DefKind::Ctor, "Square");
        assert_eq!(
            field_docs(&tree, square, "tag").unwrap(),
            "The second text."
        );
    }
}
