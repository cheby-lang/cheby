//! "Add the missing arms" for E0061 and as a refactor on a `case`, and
//! "Add arms" on an empty `case x { }` (X §6.4, F §8, D-1387, D-1692,
//! D-1736).
//!
//! The rows come from the `case`'s `CaseFact`, never from E0061's message
//! (D-1388): one `<row> => todo` arm per missing value, after the last arm, at
//! its indentation, or inside the braces of a `case` with no arm. The frame
//! formats the edit on the lines it touches (D-1735), and keeps it raw when
//! the file has a grammar error, so the raw edit already carries each
//! line's whole indentation (D-1692).
//!
//! - A constructor is written as the module names it: bare when the module
//!   declares it, imports it by name or has it from the prelude, else
//!   through the binding of its module's import, `shape::Circle`. A
//!   constructor with fields by name is `Circle { .. }`, one with positional
//!   fields `Some(_)`, with its fields' rows.
//! - A row that names a constructor the module cannot name, one of a
//!   module it does not import or a prelude one it hides (D-1678), becomes
//!   one `_ => todo` arm, written last. A fact with more rows than it holds,
//!   or one whose check ran out of budget, gives that arm alone (F §8).
//! - On E0061 the fix is a quick fix with its edit in the listing. On any
//!   other `case` with missing rows, such as one in a body with another
//!   error, where E0061 is not reported (D-494), it is a `refactor.rewrite`
//!   whose edit a resolve computes, for the innermost such `case` around the
//!   range. A `case` with no arm, E0013, is "Add arms".

use cheby_driver::analysis::codes;
use cheby_driver::analysis::{
    CtorFields, DefId, ItemSig, MissingPat, Namespace, PlainDiagnostic, PlainEdit, Res, TextRange,
    TextSize, builtin,
};
use cheby_syntax::{SyntaxKind as K, SyntaxNode};

use super::Refusal;
use super::actions::{Action, ActionId, ActionKind, DiagRef, FixCx};
use super::cursor::token_at;
use super::print::hides_prelude;
use super::scope;

/// The title of the fix on E0061 and of the refactor (D-1736).
pub(crate) const MISSING_ARMS: &str = "Add the missing arms";

/// The title of the refactor on a `case` with no arm (D-1736).
pub(crate) const ADD_ARMS: &str = "Add arms";

/// The fill-arms actions for `range`: a quick fix on each E0061 among
/// `diagnostics`, and the refactor on the innermost `case` around `range` with
/// missing rows when no such fix is for it.
pub(crate) fn provide(
    cx: &FixCx<'_>,
    range: TextRange,
    diagnostics: &[(usize, &PlainDiagnostic)],
) -> Vec<Action> {
    let Some(tree) = cx.view.tree() else {
        return Vec::new();
    };
    let root = tree.root();
    let mut out = Vec::new();
    let mut fixed = Vec::new();
    for &(index, diagnostic) in diagnostics {
        if diagnostic.code != codes::NON_EXHAUSTIVE_CASE {
            continue;
        }
        let Some(at) = diagnostic.range else {
            continue;
        };
        let Some(case) = cases_around(root, at).find(|case| has_rows(cx, *case)) else {
            continue;
        };
        let Some(edits) = fill(cx, case) else {
            continue;
        };
        fixed.push(case.id());
        out.push(action(
            MISSING_ARMS,
            ActionKind::QuickFix,
            Some(DiagRef::Report(index)),
            Some(edits),
        ));
    }
    if let Some(case) = target(cx, root, range)
        && !fixed.contains(&case.id())
    {
        let title = if arms(case).next().is_none() {
            ADD_ARMS
        } else {
            MISSING_ARMS
        };
        let mut refactor = action(title, ActionKind::RefactorRewrite, None, None);
        refactor.data = Some(ActionId::FillArms);
        out.push(refactor);
    }
    out
}

/// The edits of the refactor at `range`: the rows of the innermost `case`
/// around it that has missing rows, as [`provide`] found it.
///
/// # Errors
///
/// A [`Refusal`] when no such `case` is around `range` any more, or its rows
/// cannot be written.
pub(crate) fn resolve(
    cx: &FixCx<'_>,
    range: TextRange,
    id: &ActionId,
) -> Result<Vec<PlainEdit>, Refusal> {
    let refuse = || Refusal("no `case` with missing arms is here".to_owned());
    match id {
        ActionId::FillArms => {}
        ActionId::Fix { .. }
        | ActionId::AddImport { .. }
        | ActionId::RemoveImport
        | ActionId::OrganizeImports
        | ActionId::Stub { .. }
        | ActionId::MakeVisible { .. }
        | ActionId::Habit { .. }
        | ActionId::Rename { .. } => return Err(refuse()),
    }
    let tree = cx.view.tree().ok_or_else(refuse)?;
    let case = target(cx, tree.root(), range).ok_or_else(refuse)?;
    fill(cx, case).ok_or_else(refuse)
}

/// An action of this file with nothing else set.
fn action(
    title: &str,
    kind: ActionKind,
    diagnostic: Option<DiagRef>,
    edit: Option<Vec<PlainEdit>>,
) -> Action {
    Action {
        title: title.to_owned(),
        kind,
        diagnostic,
        preferred: diagnostic.is_some(),
        removes: false,
        edit,
        data: None,
        disabled: None,
        command: None,
    }
}

/// The `CASE_EXPR`s around `range`, innermost first.
fn cases_around(root: SyntaxNode<'_>, range: TextRange) -> impl Iterator<Item = SyntaxNode<'_>> {
    let at = token_at(root, range.start());
    at.right()
        .or_else(|| at.left())
        .and_then(cheby_syntax::SyntaxToken::parent)
        .into_iter()
        .flat_map(SyntaxNode::ancestors)
        .filter(move |node| {
            node.kind() == K::CASE_EXPR
                && node.text_range().start() <= range.start()
                && range.end() <= node.text_range().end()
        })
}

/// The innermost `case` around `range` with missing rows.
fn target<'t>(cx: &FixCx<'_>, root: SyntaxNode<'t>, range: TextRange) -> Option<SyntaxNode<'t>> {
    cases_around(root, range).find(|case| has_rows(cx, *case))
}

/// The `CaseFact`'s rows of `case`: one text per missing value, each the
/// subjects' patterns joined by `, `, and a `_` last for whatever no row
/// names. `None` when the `case` has no fact or misses nothing.
fn rows(cx: &FixCx<'_>, case: SyntaxNode<'_>) -> Option<Vec<String>> {
    let item = scope::body_at(cx.view, case.text_range().start())?;
    let fact = cx.view.facts()?.body(item)?.case(case.id())?;
    if fact.missing.is_empty() && !fact.more && !fact.too_complex {
        return None;
    }
    let mut named: Vec<String> = Vec::new();
    let mut rest = fact.more || fact.too_complex;
    if !rest {
        for row in &fact.missing {
            let written: Option<Vec<String>> = row.iter().map(|pat| pattern(cx, pat)).collect();
            match written {
                Some(pats) if pats.iter().any(|p| p != "_") => {
                    let text = pats.join(", ");
                    if !named.contains(&text) {
                        named.push(text);
                    }
                }
                Some(_) | None => rest = true,
            }
        }
    }
    if rest {
        let subjects = fact.missing.first().map_or(1, Vec::len).max(1);
        named.push(vec!["_"; subjects].join(", "));
    }
    Some(named)
}

/// Whether `case` has missing rows.
fn has_rows(cx: &FixCx<'_>, case: SyntaxNode<'_>) -> bool {
    rows(cx, case).is_some_and(|rows| !rows.is_empty())
}

/// The arms of `case`, in order.
fn arms(case: SyntaxNode<'_>) -> impl Iterator<Item = SyntaxNode<'_>> {
    case.children()
        .filter(|c| c.kind() == K::CASE_BODY)
        .flat_map(SyntaxNode::children)
        .filter(|c| c.kind() == K::CASE_ARM)
}

/// The edit that adds `case`'s missing rows (module doc).
fn fill(cx: &FixCx<'_>, case: SyntaxNode<'_>) -> Option<Vec<PlainEdit>> {
    let path = cx.path()?;
    let rows = rows(cx, case)?;
    if rows.is_empty() {
        return None;
    }
    let text = cx.text;
    let body = case.children().find(|c| c.kind() == K::CASE_BODY)?;
    let close = body
        .children_with_tokens()
        .filter_map(cheby_syntax::SyntaxElement::as_token)
        .find(|t| t.kind() == K::R_BRACE)?;
    let newline = newline_at(text, case.text_range().start());
    let case_indent = indent_at(text, case.text_range().start());
    let lines = |indent: &str| -> String {
        let mut out = String::new();
        for row in &rows {
            out.push_str(newline);
            out.push_str(indent);
            out.push_str(row);
            out.push_str(" => todo");
        }
        out
    };
    let (range, replacement) = if let Some(last) = arms(case).last() {
        let end = last.text_range().end();
        let close_at = close.text_range().start();
        let line_end = line_end(text, end);
        if close_at < line_end {
            // `case x { A => 1 }` on one line: the rows and the `}` go on
            // lines of their own.
            let indent = format!("{case_indent}  ");
            let between = TextRange::new(end, close_at);
            (between, format!("{}{newline}{case_indent}", lines(&indent)))
        } else {
            let indent = indent_at(text, last.text_range().start());
            (TextRange::empty(line_end), lines(&indent))
        }
    } else {
        let open = body
            .children_with_tokens()
            .filter_map(cheby_syntax::SyntaxElement::as_token)
            .find(|t| t.kind() == K::L_BRACE)?;
        let inside = TextRange::new(open.text_range().end(), close.text_range().start());
        // Keep a comment written inside the braces.
        if !inside.slice(text)?.trim().is_empty() {
            return None;
        }
        let indent = format!("{case_indent}  ");
        (inside, format!("{}{newline}{case_indent}", lines(&indent)))
    };
    Some(vec![PlainEdit {
        path: path.to_path_buf(),
        range,
        replacement,
    }])
}

/// `pat` as the module writes it, `None` when it names a constructor the
/// module cannot name (module doc).
fn pattern(cx: &FixCx<'_>, pat: &MissingPat) -> Option<String> {
    Some(match pat {
        MissingPat::Wild => "_".to_owned(),
        MissingPat::Int(n) => n.to_string(),
        MissingPat::Str(s) => string_literal(s),
        MissingPat::Tuple(items) => {
            let items: Option<Vec<String>> = items.iter().map(|p| pattern(cx, p)).collect();
            format!("({})", items?.join(", "))
        }
        MissingPat::List { items, rest_at } => {
            let mut parts: Vec<String> = items
                .iter()
                .map(|p| pattern(cx, p))
                .collect::<Option<_>>()?;
            if let Some(at) = rest_at {
                let at = usize::try_from(*at).ok()?.min(parts.len());
                parts.insert(at, "..".to_owned());
            }
            format!("[{}]", parts.join(", "))
        }
        MissingPat::Ctor { ctor, fields } => {
            let name = ctor_name(cx, *ctor)?;
            let snapshot = &cx.analyzed.snapshot;
            let item = snapshot.interfaces().get(ctor.module)?.item(ctor.index)?;
            let ItemSig::Ctor(sig) = &item.sig else {
                return None;
            };
            match &sig.fields {
                CtorFields::Unit => name,
                CtorFields::Named(_) => format!("{name} {{ .. }}"),
                CtorFields::Positional(_) => {
                    let fields: Option<Vec<String>> =
                        fields.iter().map(|p| pattern(cx, p)).collect();
                    format!("{name}({})", fields?.join(", "))
                }
            }
        }
    })
}

/// A string pattern for `text`, with the escapes a string needs.
fn string_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len().saturating_add(2));
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '{' => out.push_str("\\{"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// How the module names the constructor `ctor` (module doc).
pub(crate) fn ctor_name(cx: &FixCx<'_>, ctor: DefId) -> Option<String> {
    let snapshot = &cx.analyzed.snapshot;
    let item = snapshot.interfaces().get(ctor.module)?.item(ctor.index)?;
    let name = snapshot.globals().names.get(item.name);
    if hides_prelude(snapshot, cx.view, ctor, None) {
        return None;
    }
    if ctor.module == cx.view.id() || builtin::is_prelude(ctor) {
        return Some(name.to_owned());
    }
    let resolved = cx.view.resolved()?;
    if resolved
        .scope
        .get(Namespace::Value, item.name)
        .is_some_and(|entry| entry.res == Res::Def(ctor))
    {
        return Some(name.to_owned());
    }
    let imports = resolved.scope.import_names();
    let module = imports.modules.get(&ctor.module)?;
    Some(format!("{}::{name}", snapshot.globals().names.get(*module)))
}

/// The start of the line that holds `offset`.
pub(crate) fn line_start(text: &str, offset: TextSize) -> TextSize {
    let at = usize::try_from(offset)
        .unwrap_or(text.len())
        .min(text.len());
    let start = text
        .get(..at)
        .and_then(|before| before.rfind('\n'))
        .map_or(0, |n| n.saturating_add(1));
    TextSize::try_from(start).unwrap_or(offset)
}

/// The end of the line that holds `offset`, before its line break.
pub(crate) fn line_end(text: &str, offset: TextSize) -> TextSize {
    let at = usize::try_from(offset)
        .unwrap_or(text.len())
        .min(text.len());
    let rest = text.get(at..).unwrap_or_default();
    let end = rest.find('\n').map_or(text.len(), |n| at.saturating_add(n));
    let end = if end > at && text.as_bytes().get(end.saturating_sub(1)) == Some(&b'\r') {
        end.saturating_sub(1)
    } else {
        end
    };
    TextSize::try_from(end).unwrap_or(offset)
}

/// The spaces and tabs that start the line holding `offset`.
pub(crate) fn indent_at(text: &str, offset: TextSize) -> String {
    let start = usize::try_from(line_start(text, offset)).unwrap_or(0);
    text.get(start..)
        .unwrap_or_default()
        .chars()
        .take_while(|c| matches!(c, ' ' | '\t'))
        .collect()
}

/// The line break the line holding `offset` ends with, `\n` when it ends
/// the text.
pub(crate) fn newline_at(text: &str, offset: TextSize) -> &'static str {
    let end = usize::try_from(line_end(text, offset)).unwrap_or(text.len());
    if text.get(end..).is_some_and(|rest| rest.starts_with("\r\n")) {
        "\r\n"
    } else {
        "\n"
    }
}

#[cfg(test)]
#[path = "fill_arms_tests.rs"]
pub(crate) mod tests;
