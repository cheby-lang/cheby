//! The grammar: one function per rule of Appendix A.
//!
//! This module holds the source-file loop (App. A.1) and the helpers shared
//! by the rules: comma lists, paths and names. The rules themselves are in
//! the submodules, by appendix section.

mod attrs;
mod exprs;
mod items;
mod patterns;
mod stmts;
mod types;

use cheby_diag::codes;

use super::recovery::{ITEM_BODY_FIRST, LIST_STOP};
use super::{CompletedMarker, Delim, Parser};
use crate::SyntaxKind::{
    self, AT, AT_BANG, COLON2, COMMA, EOF, IMPORT_KW, L_BRACE, LOWER_IDENT, NEWLINE, PATH,
    SOURCE_FILE, UPPER_IDENT,
};
use crate::TokenSet;

/// Parses a whole file (App. A.1).
///
/// Inner attributes, imports and items may come in any order here; the
/// order is checked by validation (E0018), because `tree-sitter-cheby`
/// accepts any order. Line breaks at the start and the end of the file are
/// ignored (D-426).
pub(super) fn source_file(p: &mut Parser<'_>) {
    let m = p.start();
    loop {
        while p.eat(NEWLINE) {}
        p.end_unwinding();
        if p.at(EOF) {
            break;
        }
        let before = p.position();
        let needs_separator = match p.current() {
            AT_BANG => {
                attrs::inner_attr(p);
                true
            }
            IMPORT_KW => {
                items::import_decl(p);
                true
            }
            kind if kind == AT || ITEM_BODY_FIRST.contains(kind) => items::item(p),
            _ => {
                p.expected("an item");
                p.skip_until(TokenSet::EMPTY, true);
                false
            }
        };
        if p.position() == before {
            p.bump_error();
            continue;
        }
        if !needs_separator || p.at(EOF) || p.eat(NEWLINE) || p.at_line_start() {
            continue;
        }
        p.expected("a line break after the item");
        p.skip_until(TokenSet::EMPTY, true);
    }
    m.complete(p, SOURCE_FILE);
}

/// A comma-separated list in delimiters (`(…)`, `[…]`, `{…}` or a type
/// `<…>`), with an optional trailing comma.
pub(super) struct List {
    /// The opening delimiter.
    pub(super) open: SyntaxKind,
    /// The node kind of the list.
    pub(super) kind: SyntaxKind,
    /// Tokens that start an element.
    pub(super) first: TokenSet,
    /// The element, for "expected …" messages, such as "a parameter".
    pub(super) what: &'static str,
    /// Whether an empty list is a grammar error.
    pub(super) non_empty: bool,
}

/// Parses `list`, calling `elem` at each token in `list.first`. `elem`
/// must consume at least one token.
///
/// On a token that cannot start an element or follow one, it reports and
/// skips to the next `,` or the closer, or stops at a closer of an enclosing
/// list, a statement keyword or an item keyword at the start of a line. A
/// missing closer is E0015.
pub(super) fn comma_list(
    p: &mut Parser<'_>,
    list: &List,
    elem: impl FnMut(&mut Parser<'_>),
) -> CompletedMarker {
    let m = p.start();
    let delim = p.open_delim(list.open);
    list_rest(p, delim, list, 0, false, elem);
    m.complete(p, list.kind)
}

/// The rest of a comma list whose opener `delim` is already consumed, after
/// `count` elements, the last of which was just parsed if `after_elem`.
/// Consumes the closer (or reports E0015). Makes no node: `list.kind` is
/// not used.
pub(super) fn list_rest(
    p: &mut Parser<'_>,
    delim: Delim,
    list: &List,
    mut count: usize,
    after_elem: bool,
    mut elem: impl FnMut(&mut Parser<'_>),
) {
    let stop = if list.open == L_BRACE {
        LIST_STOP.union(TokenSet::new(&[NEWLINE]))
    } else {
        LIST_STOP.union(TokenSet::new(&[L_BRACE]))
    };
    let skip_stop = stop.union(TokenSet::new(&[COMMA]));
    let close_text = closer_text(list.open);
    let mut need_comma = after_elem;
    loop {
        if need_comma {
            if p.at(NEWLINE) {
                // A line break where the list needs `,` or its closer
                // (E0014); the list ends here.
                p.expected(&format!("`,` or {close_text}"));
                break;
            }
            if p.at_close(&delim)
                || p.at(EOF)
                || p.at_ts(stop)
                || (p.at_item_recovery() && !p.at_member_attr(list.first))
            {
                break;
            }
            if !p.eat(COMMA) {
                p.expected(&format!("`,` or {close_text}"));
                // A missing comma before an element: go on with it.
                if !p.at_ts(list.first) {
                    p.skip_until(skip_stop, true);
                    if !p.eat(COMMA) {
                        break;
                    }
                }
            }
        }
        if p.at_close(&delim)
            || p.at(EOF)
            || (p.at_item_recovery() && !p.at_member_attr(list.first))
        {
            break;
        }
        let before = p.position();
        if p.at_ts(list.first) {
            elem(p);
            count = count.saturating_add(1);
        } else if p.at_ts(stop) {
            break;
        } else {
            p.expected(list.what);
            p.skip_until(skip_stop, true);
        }
        if p.position() == before && !p.at(COMMA) {
            p.bump_error();
        }
        need_comma = true;
    }
    if count == 0 && list.non_empty && p.at_close(&delim) {
        p.expected(list.what);
    }
    p.close_delim(delim);
}

/// The text of a closing delimiter, quoted, for messages.
const fn closer_text(open: SyntaxKind) -> &'static str {
    match open {
        SyntaxKind::L_PAREN => "`)`",
        SyntaxKind::L_BRACK => "`]`",
        SyntaxKind::LT => "`>`",
        _ => "`}`",
    }
}

/// What the last name of a path is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum PathEnd {
    /// `name` or `module::name`.
    Lower,
    /// `Name` or `module::Name`.
    Upper,
    /// `Interface::function` or `module::Interface::function` (D-173).
    Interface,
    /// No name at all (an error was reported).
    Missing,
}

/// Parses a `PATH`: `[LOWER "::"] (LOWER | UPPER)`, or with `interface`,
/// also `[LOWER "::"] UPPER "::" LOWER` (App. A.5, D-173).
///
/// A path has at most one module qualifier, as in `tree-sitter-cheby`;
/// more segments are kept in the path and reported (E0013).
pub(super) fn path(p: &mut Parser<'_>, interface: bool) -> PathEnd {
    let m = p.start();
    // The text of each segment, for the help of E0013.
    let mut segments: Vec<&str> = Vec::new();
    if p.at(LOWER_IDENT) && p.nth_at(1, COLON2) && matches!(p.nth(2), LOWER_IDENT | UPPER_IDENT) {
        segments.push(p.current_text());
        p.bump(LOWER_IDENT);
        p.bump(COLON2);
    }
    let mut end = match p.current() {
        LOWER_IDENT => {
            segments.push(p.current_text());
            p.bump(LOWER_IDENT);
            PathEnd::Lower
        }
        UPPER_IDENT => {
            segments.push(p.current_text());
            p.bump(UPPER_IDENT);
            if interface && p.at(COLON2) && p.nth_at(1, LOWER_IDENT) {
                p.bump(COLON2);
                segments.push(p.current_text());
                p.bump(LOWER_IDENT);
                PathEnd::Interface
            } else {
                PathEnd::Upper
            }
        }
        _ => {
            p.expected("a name");
            PathEnd::Missing
        }
    };
    if end != PathEnd::Missing && p.at(COLON2) && matches!(p.nth(1), LOWER_IDENT | UPPER_IDENT) {
        let span = p.current_span();
        while p.at(COLON2) && matches!(p.nth(1), LOWER_IDENT | UPPER_IDENT) {
            p.bump(COLON2);
            end = if p.at(LOWER_IDENT) {
                PathEnd::Lower
            } else {
                PathEnd::Upper
            };
            segments.push(p.current_text());
            p.bump_any();
        }
        let diag = cheby_diag::Diagnostic::new(
            codes::SYNTAX_ERROR,
            "a path has at most one module qualifier",
        )
        .with_primary(span, "")
        .with_help(long_path_help(&segments));
        p.report(diag);
    }
    m.complete(p, PATH);
    end
}

/// The help of E0013 for a path with more than one module qualifier,
/// such as `std::io::println`: import the modules before the item and
/// qualify with the last one, `import std::io` and `io::println`. The
/// modules are the lowercase segments before the first capitalized one,
/// leaving at least the last segment as the item.
fn long_path_help(segments: &[&str]) -> String {
    let item_start = segments
        .iter()
        .position(|s| s.starts_with(|c: char| c.is_ascii_uppercase()))
        .unwrap_or(segments.len().saturating_sub(1))
        .min(segments.len().saturating_sub(1));
    match segments.split_at_checked(item_start) {
        Some((modules @ [_, .., last], item)) if !item.is_empty() => format!(
            "import the inner module and qualify with its last name: `import {}` and `{last}::{}`",
            modules.join("::"),
            item.join("::")
        ),
        _ => {
            "import the inner module and qualify with its last name, such as `ops::Add`".to_owned()
        }
    }
}

/// Consumes a name of kind `kind` (`LOWER_IDENT` or `UPPER_IDENT`). A name
/// of the other case is reported and consumed, so that the rest of the
/// declaration still parses.
pub(super) fn name(p: &mut Parser<'_>, kind: SyntaxKind, what: &str) -> bool {
    if p.eat(kind) {
        return true;
    }
    p.expected(what);
    if matches!(p.current(), LOWER_IDENT | UPPER_IDENT) {
        p.bump_any();
    }
    false
}
