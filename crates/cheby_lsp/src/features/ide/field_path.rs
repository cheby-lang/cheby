//! The "Use a field path" rewrite (D-1645), from the syntax tree alone.
//!
//! A record update whose field's value is an update of the same base's
//! field, `Shape { ..shape, origin: Point { ..shape.origin, y } }`, becomes
//! `Shape { ..shape, origin.y }` (D-1638). Every level of that form below
//! is flattened too, so `G { ..g, p: U { ..g.p, q: V { ..g.p.q, z: 1 } } }`
//! becomes `G { ..g, p.q.z: 1 }`: one entry per innermost field, in
//! written order, which keeps the order of evaluation (D-1642).
//!
//! The rewrite is offered only when it keeps the program's meaning:
//!
//! - The outer base is a name or a field access of a name, so dropping the
//!   inner update's own reading of it drops no effect.
//! - Each inner base is the outer base followed by the field path to it,
//!   token for token.
//! - No other field of the outer update shares the path's beginning, which
//!   the written form already rejects (D-1641).
//! - The replaced text holds no comment, which the rewrite would drop.
//!
//! The cursor may be anywhere inside such a field. The outermost one around
//! it is rewritten.

use cheby_driver::analysis::TextRange;
use cheby_syntax::{SyntaxElement, SyntaxKind as K, SyntaxNode, SyntaxToken};

use super::cursor::token_at;

/// The title of the action.
pub(crate) const TITLE: &str = "Use a field path";

/// A replacement of `range` by `text`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Rewrite {
    pub range: TextRange,
    pub text: String,
}

/// The rewrite of the outermost field around `range`'s start that has the
/// form, in the tree `root` of `text`.
pub(crate) fn rewrite(root: SyntaxNode<'_>, text: &str, range: TextRange) -> Option<Rewrite> {
    let at = token_at(root, range.start());
    let token = at.right().or_else(|| at.left())?;
    token
        .parent()?
        .ancestors()
        .filter(|node| node.kind() == K::FIELD_INIT)
        .filter_map(|init| field(init, text))
        .last()
}

/// The rewrite of `init`, a `FIELD_INIT`, if it has the form.
fn field(init: SyntaxNode<'_>, text: &str) -> Option<Rewrite> {
    let record = init.parent().filter(|r| r.kind() == K::RECORD_EXPR)?;
    let base = pure_base(spread_value(record)?)?;
    let path = names(init);
    let siblings = record
        .children()
        .filter(|c| c.kind() == K::FIELD_INIT && *c != init);
    for sibling in siblings {
        let other = names(sibling);
        if other.starts_with(&path) || path.starts_with(&other) {
            return None;
        }
    }
    if init
        .descendants_with_tokens()
        .any(|e| matches!(e.kind(), K::COMMENT | K::DOC_COMMENT | K::MODULE_DOC))
    {
        return None;
    }
    let inner = nested(value(init)?, &extend(&base, &path))?;
    let mut entries = Vec::new();
    flatten(inner, &path, &extend(&base, &path), &mut entries)?;
    let separator = separator(record, init, text);
    Some(Rewrite {
        range: init.text_range(),
        text: entries.join(&separator),
    })
}

/// Pushes the entries of `record`, an update of the value at `base`, each
/// with its path after `prefix`. `None` when an entry is malformed.
fn flatten(
    record: SyntaxNode<'_>,
    prefix: &[&str],
    base: &[&str],
    out: &mut Vec<String>,
) -> Option<()> {
    let inits: Vec<SyntaxNode<'_>> = record
        .children()
        .filter(|c| c.kind() == K::FIELD_INIT)
        .collect();
    if inits.is_empty() {
        return None;
    }
    for init in inits {
        let own = names(init);
        if own.is_empty() {
            return None;
        }
        let path = extend_names(prefix, &own);
        if !has_token(init, K::COLON) {
            // A pun binds its last name either way (D-1640).
            out.push(path.join("."));
            continue;
        }
        let value = value(init)?;
        let inner_base = extend(base, &own);
        match nested(value, &inner_base) {
            Some(inner) => flatten(inner, &path, &inner_base, out)?,
            None => out.push(format!("{}: {}", path.join("."), value.text())),
        }
    }
    Some(())
}

/// `value` when it is a record update whose base is the tokens `base`.
fn nested<'t>(value: SyntaxNode<'t>, base: &[&str]) -> Option<SyntaxNode<'t>> {
    if value.kind() != K::RECORD_EXPR || value.children().any(|c| c.kind() == K::ERROR) {
        return None;
    }
    let tokens = pure_base(spread_value(value)?)?;
    (tokens == base).then_some(value)
}

/// The expression after `..` in `record`, when it has exactly one spread,
/// first.
fn spread_value(record: SyntaxNode<'_>) -> Option<SyntaxNode<'_>> {
    let mut spreads = record.children().filter(|c| c.kind() == K::SPREAD);
    let spread = spreads.next()?;
    if spreads.next().is_some()
        || record
            .children()
            .find(|c| matches!(c.kind(), K::SPREAD | K::FIELD_INIT))
            != Some(spread)
    {
        return None;
    }
    spread.first_child()
}

/// The tokens of `expr` when it is a name, or field accesses of a name:
/// `shape`, `game.player`. Reading it has no effect.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "a match on syntax kinds names only the kinds it handles"
)]
fn pure_base(expr: SyntaxNode<'_>) -> Option<Vec<&str>> {
    match expr.kind() {
        K::PATH_EXPR => {
            let path = expr.first_child().filter(|p| p.kind() == K::PATH)?;
            let tokens = significant(expr);
            let [(kind, name, range)] = tokens.as_slice() else {
                return None;
            };
            (*kind == K::LOWER_IDENT && path.text_range().contains_range(*range))
                .then(|| vec![*name])
        }
        K::FIELD_EXPR => {
            let mut tokens = pure_base(expr.first_child()?)?;
            let field = expr
                .children_with_tokens()
                .filter_map(SyntaxElement::as_token)
                .filter(|t| !t.kind().is_trivia())
                .collect::<Vec<_>>();
            let [dot, name] = field.as_slice() else {
                return None;
            };
            if dot.kind() != K::DOT || name.kind() != K::LOWER_IDENT {
                return None;
            }
            tokens.push(".");
            tokens.push(name.text());
            Some(tokens)
        }
        _ => None,
    }
}

/// The kind, text and range of each significant token under `node`.
fn significant(node: SyntaxNode<'_>) -> Vec<(K, &str, TextRange)> {
    node.tokens()
        .filter(|t| !t.kind().is_trivia())
        .map(|t| (t.kind(), t.text(), t.text_range()))
        .collect()
}

/// `base` followed by `.name` for each of `names`.
fn extend<'t>(base: &[&'t str], names: &[&'t str]) -> Vec<&'t str> {
    let mut out = base.to_vec();
    for name in names {
        out.push(".");
        out.push(name);
    }
    out
}

/// `prefix` followed by `names`.
fn extend_names<'t>(prefix: &[&'t str], names: &[&'t str]) -> Vec<&'t str> {
    let mut out = prefix.to_vec();
    out.extend_from_slice(names);
    out
}

/// The names of `init`'s field path.
fn names(init: SyntaxNode<'_>) -> Vec<&str> {
    init.children_with_tokens()
        .filter_map(SyntaxElement::as_token)
        .filter(|t| t.kind() == K::LOWER_IDENT)
        .map(SyntaxToken::text)
        .collect()
}

/// The value of `init`, after its `:`.
fn value(init: SyntaxNode<'_>) -> Option<SyntaxNode<'_>> {
    has_token(init, K::COLON).then(|| init.first_child())?
}

/// Whether `node` has a direct token of `kind`.
fn has_token(node: SyntaxNode<'_>, kind: K) -> bool {
    node.children_with_tokens()
        .filter_map(SyntaxElement::as_token)
        .any(|t| t.kind() == kind)
}

/// What goes between two entries: a line break and `init`'s indentation
/// when `record` spans lines, else `, `.
fn separator(record: SyntaxNode<'_>, init: SyntaxNode<'_>, text: &str) -> String {
    if !record.text().contains('\n') {
        return ", ".to_owned();
    }
    let line = usize::try_from(init.text_range().start())
        .ok()
        .and_then(|start| text.get(..start))
        .map_or("", |before| before.rsplit('\n').next().unwrap_or(""));
    if line.chars().all(|c| c == ' ' || c == '\t') {
        format!(",\n{line}")
    } else {
        ", ".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use cheby_driver::analysis::{TextRange, TextSize};

    use super::rewrite;
    use crate::features::DocText;

    /// `source` with the cursor at its `|`, rewritten, or `None`.
    fn apply(source: &str) -> Option<String> {
        let at = source.find('|').expect("a cursor");
        let text: String = source.replacen('|', "", 1);
        let doc =
            DocText::new(PathBuf::from("a.cheby"), None, Arc::from(text.as_str())).expect("parses");
        let offset = TextSize::from(u32::try_from(at).expect("small"));
        let found = rewrite(doc.tree.root(), &doc.text, TextRange::empty(offset))?;
        let start = usize::try_from(found.range.start()).expect("fits");
        let end = usize::try_from(found.range.end()).expect("fits");
        let (before, after) = (text.get(..start)?, text.get(end..)?);
        Some(format!("{before}{}{after}", found.text))
    }

    #[test]
    fn one_level_with_a_pun() {
        assert_eq!(
            apply("fn f() { Shape { ..shape, origin: |Point { ..shape.origin, y } } }").as_deref(),
            Some("fn f() { Shape { ..shape, origin.y } }"),
        );
    }

    #[test]
    fn every_level_flattens_in_order() {
        let source = "fn f() {\n  Game {\n    ..game,\n    turn: 1,\n    player: Player {\n      ..game.player,\n      position: |Point { ..game.player.position, x: 2 },\n      moves: m,\n    },\n  }\n}";
        let expected = "fn f() {\n  Game {\n    ..game,\n    turn: 1,\n    player.position.x: 2,\n    player.moves: m,\n  }\n}";
        assert_eq!(apply(source).as_deref(), Some(expected));
    }

    #[test]
    fn a_qualified_inner_type_and_a_field_base() {
        assert_eq!(
            apply("fn f() { S { ..a.b, c: |m::C { ..a.b.c, d: 1, e: g(2) } } }").as_deref(),
            Some("fn f() { S { ..a.b, c.d: 1, c.e: g(2) } }"),
        );
    }

    #[test]
    fn an_inner_value_of_another_base_stays() {
        assert_eq!(
            apply("fn f() { S { ..s, a: |A { ..s.a, b: B { ..t.b, c: 1 } } } }").as_deref(),
            Some("fn f() { S { ..s, a.b: B { ..t.b, c: 1 } } }"),
        );
    }

    #[test]
    fn not_offered() {
        // The base is a call, whose effects the rewrite would drop.
        assert_eq!(
            apply("fn f() { S { ..g(), a: |A { ..g().a, b: 1 } } }"),
            None
        );
        // The inner base is another value.
        assert_eq!(apply("fn f() { S { ..s, a: |A { ..t.a, b: 1 } } }"), None);
        // Another field shares the path.
        assert_eq!(
            apply("fn f() { S { ..s, a.c: 2, a: |A { ..s.a, b: 1 } } }"),
            None
        );
        // A comment would be lost.
        assert_eq!(
            apply(
                "fn f() {\n  S {\n    ..s,\n    a: |A {\n      ..s.a,\n      // why\n      b: 1,\n    },\n  }\n}"
            ),
            None
        );
        // A construction, not an update.
        assert_eq!(apply("fn f() { S { a: |A { ..s.a, b: 1 } } }"), None);
        // The value is not an update.
        assert_eq!(apply("fn f() { S { ..s, a: |A { b: 1 } } }"), None);
    }
}
