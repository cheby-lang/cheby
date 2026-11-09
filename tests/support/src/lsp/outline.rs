//! The outline a `textDocument/documentSymbol` answer must give: the items
//! of a module, read from its syntax tree, with the kinds of
//! `features.md` §5.1 (syntax map §7.4).
//!
//! The harness cannot reach the checker's item list, so the item list here
//! is the parser's: [`expected`] walks `SourceFile::items` and recurses
//! into variants, record fields and interface functions. Local functions
//! and imports are not part of an outline, and an item with no name is
//! left out. [`check`] compares an answer with it by name, kind and
//! nesting, and checks that every `selectionRange` lies inside its
//! `range` and every child inside its parent.

use std::fmt::Write as _;

use cheby_syntax::ast::{AstNode as _, AstToken as _, Item, TypeBody, VariantFields};
use serde_json::Value;

use super::text::Position;

/// The protocol's `SymbolKind` numbers the outline uses.
pub mod kind {
    /// `Class`: a type with no body, opaque or external.
    pub const CLASS: u64 = 5;
    /// `Method`: an interface function.
    pub const METHOD: u64 = 6;
    /// `Field`: a record field.
    pub const FIELD: u64 = 8;
    /// `Enum`: a type with variants.
    pub const ENUM: u64 = 10;
    /// `Interface`.
    pub const INTERFACE: u64 = 11;
    /// `Function`: a function or a test.
    pub const FUNCTION: u64 = 12;
    /// `Constant`.
    pub const CONSTANT: u64 = 14;
    /// `EnumMember`: a variant.
    pub const ENUM_MEMBER: u64 = 22;
    /// `Struct`: a record type.
    pub const STRUCT: u64 = 23;
    /// `TypeParameter`: an alias, by the table's first choice.
    pub const TYPE_PARAMETER: u64 = 26;
}

/// One expected entry of an outline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The name: the item's, or a test's decoded string.
    pub name: String,
    /// The kinds the entry may have. An alias may be `TypeParameter` or
    /// `Struct`, since the table allows both.
    pub kinds: &'static [u64],
    /// The entries nested in it, in source order.
    pub children: Vec<Self>,
}

impl Entry {
    fn leaf(name: &str, kinds: &'static [u64]) -> Self {
        Self {
            name: name.to_owned(),
            kinds,
            children: Vec::new(),
        }
    }
}

/// The outline of `text`, in source order.
#[must_use]
pub fn expected(text: &str) -> Vec<Entry> {
    let parse = cheby_syntax::parse(cheby_diag::FileId::new(0), text.into());
    parse
        .source_file()
        .items()
        .filter_map(|item| item_entry(item))
        .collect()
}

fn item_entry(item: Item<'_>) -> Option<Entry> {
    let name = match item {
        Item::Test(test) => {
            let lit = test.name()?;
            lit.plain_value()
                .unwrap_or_else(|| lit.syntax().text().to_owned())
        }
        _ => item.name()?.text().to_owned(),
    };
    let mut entry = Entry::leaf(&name, &[kind::FUNCTION]);
    match item {
        Item::Fn(_) | Item::Test(_) => {}
        Item::Const(_) => entry.kinds = &[kind::CONSTANT],
        Item::Alias(_) => entry.kinds = &[kind::TYPE_PARAMETER, kind::STRUCT],
        Item::Interface(decl) => {
            entry.kinds = &[kind::INTERFACE];
            entry.children = decl
                .fns()
                .filter_map(|f| Some(Entry::leaf(f.name()?.text(), &[kind::METHOD])))
                .collect();
        }
        Item::Type(decl) => match decl.body() {
            None => entry.kinds = &[kind::CLASS],
            Some(TypeBody::Record(fields)) => {
                entry.kinds = &[kind::STRUCT];
                entry.children = field_entries(fields);
            }
            Some(TypeBody::Variants(variants)) => {
                entry.kinds = &[kind::ENUM];
                entry.children = variants
                    .variants()
                    .filter_map(|v| {
                        let mut child = Entry::leaf(v.name()?.text(), &[kind::ENUM_MEMBER]);
                        if let Some(VariantFields::Record(fields)) = v.fields() {
                            child.children = field_entries(fields);
                        }
                        Some(child)
                    })
                    .collect();
            }
        },
    }
    Some(entry)
}

fn field_entries(fields: cheby_syntax::ast::RecordFieldList<'_>) -> Vec<Entry> {
    fields
        .fields()
        .filter_map(|f| Some(Entry::leaf(f.name()?.text(), &[kind::FIELD])))
        .collect()
}

/// Compares a hierarchical `documentSymbol` answer with `expected`.
///
/// # Errors
///
/// Returns both outlines, one entry per line, when they differ by name,
/// kind or nesting, or a message for an answer that is not a list of
/// `DocumentSymbol`s or whose ranges do not nest.
pub fn check(expected: &[Entry], answer: &Value) -> Result<(), String> {
    let items = answer
        .as_array()
        .ok_or_else(|| format!("the answer is not a list: {answer}"))?;
    nesting(items, None)?;
    if matches(expected, items) {
        return Ok(());
    }
    let mut want = String::new();
    render_expected(expected, 0, &mut want);
    let mut got = String::new();
    render_answer(items, 0, &mut got);
    Err(format!(
        "the outline differs from the item list\nexpected:\n{want}answered:\n{got}"
    ))
}

fn matches(expected: &[Entry], items: &[Value]) -> bool {
    expected.len() == items.len()
        && expected.iter().zip(items).all(|(entry, item)| {
            item.get("name").and_then(Value::as_str) == Some(entry.name.as_str())
                && item
                    .get("kind")
                    .and_then(Value::as_u64)
                    .is_some_and(|k| entry.kinds.contains(&k))
                && matches(&entry.children, children(item))
        })
}

fn children(item: &Value) -> &[Value] {
    item.get("children")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// A range as a pair of positions, ordered.
fn range_of(item: &Value, key: &str) -> Result<(Position, Position), String> {
    let range = item
        .get(key)
        .ok_or_else(|| format!("a symbol without `{key}`: {item}"))?;
    let start = Position::from_json(range.get("start").unwrap_or(&Value::Null))?;
    let end = Position::from_json(range.get("end").unwrap_or(&Value::Null))?;
    if end < start {
        return Err(format!("`{key}` ends before it starts: {item}"));
    }
    Ok((start, end))
}

fn inside(inner: (Position, Position), outer: (Position, Position)) -> bool {
    outer.0 <= inner.0 && inner.1 <= outer.1
}

/// Every `selectionRange` inside its `range`, every child inside its
/// parent.
fn nesting(items: &[Value], parent: Option<(Position, Position)>) -> Result<(), String> {
    for item in items {
        let range = range_of(item, "range")?;
        let selection = range_of(item, "selectionRange")?;
        if !inside(selection, range) {
            return Err(format!("`selectionRange` is not inside `range`: {item}"));
        }
        if parent.is_some_and(|p| !inside(range, p)) {
            return Err(format!("a child is not inside its parent: {item}"));
        }
        nesting(children(item), Some(range))?;
    }
    Ok(())
}

fn render_expected(entries: &[Entry], depth: usize, out: &mut String) {
    for entry in entries {
        let kinds: Vec<String> = entry.kinds.iter().map(u64::to_string).collect();
        let _ = writeln!(
            out,
            "{:indent$}{} {}",
            "",
            entry.name,
            kinds.join("|"),
            indent = depth.saturating_mul(2)
        );
        render_expected(&entry.children, depth.saturating_add(1), out);
    }
}

fn render_answer(items: &[Value], depth: usize, out: &mut String) {
    for item in items {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("<none>");
        let kind = item
            .get("kind")
            .map_or_else(|| "<none>".to_owned(), Value::to_string);
        let _ = writeln!(
            out,
            "{:indent$}{name} {kind}",
            "",
            indent = depth.saturating_mul(2)
        );
        render_answer(children(item), depth.saturating_add(1), out);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const TEXT: &str = "type Shape {\n  Dot,\n  Rect { w: Int },\n}\n\ntype Pair = (Int, Int)\n\n\
                        interface Show {\n  fn show(Self) -> String\n}\n\nfn main() {\n  fn local() \
                        {\n    1\n  }\n}\n\ntest \"a \\\"b\\\"\" {\n  assert true\n}\n\nfn\n";

    fn names(entries: &[Entry]) -> Vec<String> {
        entries.iter().map(|e| e.name.clone()).collect()
    }

    #[test]
    fn items_kinds_and_children() {
        let outline = expected(TEXT);
        assert_eq!(
            names(&outline),
            ["Shape", "Pair", "Show", "main", "a \"b\""]
        );
        let shape = &outline[0];
        assert_eq!(shape.kinds, [kind::ENUM]);
        assert_eq!(names(&shape.children), ["Dot", "Rect"]);
        assert_eq!(names(&shape.children[1].children), ["w"]);
        assert_eq!(outline[1].kinds, [kind::TYPE_PARAMETER, kind::STRUCT]);
        assert_eq!(names(&outline[2].children), ["show"]);
        // A local function is not part of the outline.
        assert_eq!(outline[3].children, []);
    }

    fn symbol(name: &str, kind: u64, lines: (u32, u32), children: &Value) -> Value {
        let range = json!({
            "start": {"line": lines.0, "character": 0},
            "end": {"line": lines.1, "character": 1},
        });
        json!({
            "name": name, "kind": kind, "range": range, "selectionRange": range,
            "children": children,
        })
    }

    #[test]
    fn answers_are_compared() {
        let outline = expected("type P {\n  x: Int,\n}\n");
        let field = symbol("x", kind::FIELD, (1, 1), &json!([]));
        let good = json!([symbol("P", kind::STRUCT, (0, 2), &json!([field]))]);
        assert_eq!(check(&outline, &good), Ok(()));
        let wrong_kind = json!([symbol("P", kind::CLASS, (0, 2), &json!([field]))]);
        assert!(check(&outline, &wrong_kind).is_err());
        let missing = json!([symbol("P", kind::STRUCT, (0, 2), &json!([]))]);
        assert!(check(&outline, &missing).is_err());
        let outside = json!([symbol("P", kind::STRUCT, (0, 0), &json!([field]))]);
        let message = check(&outline, &outside).unwrap_err();
        assert!(message.contains("not inside its parent"), "{message}");
        assert!(check(&outline, &json!(null)).is_err());
    }
}
