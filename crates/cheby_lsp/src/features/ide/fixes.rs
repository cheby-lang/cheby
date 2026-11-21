//! The server's fix rows (X §6.2's S rows), keyed on facts, the tree and a
//! diagnostic's code and primary span, never its message (D-1388), with
//! the titles of D-1736.
//!
//! Every fix edits the request's file only and carries its edits at
//! listing, so the frame formats them (D-1735) and no resolve computes
//! anything. [`resolve`] still answers an [`ActionId::Fix`] with the same
//! edits, the `index`-th fix of its code at the range.
//!
//! A row offers nothing where its condition does not hold:
//!
//! - E0010 "Close the string": the line of the opening quote holds no
//!   other `"`, and the rest of it no `{` or `}` and no final backslash, which
//!   would start an interpolation or escape the new quote. The quote goes at
//!   the end of the line, or before the closers that end it when they
//!   balance openers before the string, so `io::println("hello)` becomes
//!   `io::println("hello")` (D-1748).
//! - E0015 "Insert `)`": the diagnostic names one opener and no other
//!   brace that may be the one missing its `}`, so one place is possible:
//!   right after the last token before the place the parser expected the
//!   closer.
//! - E0045 "Remove the attribute": the primary span is a whole attribute.
//! - E0047 "Wrap in `Some(…)`": the mismatch's expected type is
//!   `Option<F>`, `Result<F, E>` or `Result<T, F>` and the found type is
//!   `F`, and the module does not hide the constructor (D-1678).
//! - E0048: a call written with an argument list, not the call of a
//!   `use`, whose callee has a signature (`CallFact.params`).
//! - E0052 "Write `dyn I`": an interface named where a type is expected,
//!   outside a `dyn`.
//! - E0058 "Use the field `name`": a field the receiver's type does not
//!   have, at a step of a path too (D-1694), with one field close enough
//!   to be a likely typo, among those every variant has.
//! - E0059: a record construction with a missing field, or a field written
//!   twice or unknown, and no field path (D-1641, D-1643).
//! - E0060: a `let` with neither `assert` nor `guard`.
//! - E0065 "Add bound `T: I`": the type parameter that lacks a
//!   descriptor, and an interface the module names bare.
//! - E0067: the used item exists on one target; `@target(t)` when the
//!   using item has no `@target`, `@!target(t)` when the module has none.
//! - W0004 and W0005 "Remove the unused `f`": a top-level item, removed
//!   with its doc comment and attributes (`removes`, D-1360).
//! - W0006: an expression statement that is not its block's last.
//! - W0008: the `let assert` or `let guard` whose pattern is the span.
//! - W0010 "Remove the arm": the arm whose pattern is the span (D-1687),
//!   on lines of its own.

use std::path::Path;
use std::sync::Arc;

use cheby_driver::analysis::codes;
use cheby_driver::analysis::{
    CtorFields, DefId, DefKind, ItemSig, Namespace, ParamIdx, PlainDiagnostic, PlainEdit, Reason,
    Res, Target, TextRange, TextSize, Ty, TyKind, TypeDefKind, Unsatisfied, Want, builtin,
    structure_visible,
};
use cheby_syntax::{SyntaxElement, SyntaxKind as K, SyntaxNode, SyntaxToken};

use super::Refusal;
use super::actions::{Action, ActionId, ActionKind, DiagRef, FixCx};
use super::cursor::is_significant;
use super::print::hides_prelude;
use super::scope;

/// One fix of a row, before it becomes an [`Action`].
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fix {
    /// Its title (D-1736).
    title: String,
    /// Replacements in the request's file, by byte range.
    edits: Vec<(TextRange, String)>,
    /// X §6.2's "Pref" column.
    preferred: bool,
    /// A removal for W0004 or W0005 (D-1360).
    removes: bool,
}

impl Fix {
    /// A fix that is not a removal.
    fn new(title: String, edits: Vec<(TextRange, String)>, preferred: bool) -> Self {
        Self {
            title,
            edits,
            preferred,
            removes: false,
        }
    }

    /// Its edits, in the file at `path`.
    fn plain(&self, path: &Path) -> Vec<PlainEdit> {
        self.edits
            .iter()
            .map(|(range, replacement)| PlainEdit {
                path: path.to_path_buf(),
                range: *range,
                replacement: replacement.clone(),
            })
            .collect()
    }

    /// The action of the fix of the report's diagnostic `index`.
    fn action(self, index: usize, path: &Path) -> Action {
        let edit = self.plain(path);
        Action {
            title: self.title,
            kind: ActionKind::QuickFix,
            diagnostic: Some(DiagRef::Report(index)),
            preferred: self.preferred,
            removes: self.removes,
            edit: Some(edit),
            data: None,
            disabled: None,
            command: None,
        }
    }
}

/// The server rows' fixes of `diagnostics`, the report's that overlap the
/// range.
pub(crate) fn provide(
    cx: &FixCx<'_>,
    _range: TextRange,
    diagnostics: &[(usize, &PlainDiagnostic)],
) -> Vec<Action> {
    let Some(site) = Site::of(cx) else {
        return Vec::new();
    };
    let mut actions = Vec::new();
    for &(index, diagnostic) in diagnostics {
        for fix in site.fixes(diagnostic) {
            actions.push(fix.action(index, site.path));
        }
    }
    actions
}

/// The edits of a server row's fix: the `index`-th fix of the report's
/// diagnostics of `code` that touch `range`, as the listing offered it.
///
/// # Errors
///
/// A refusal when the id is not a server row's, or the fix is no longer
/// offered.
pub(crate) fn resolve(
    cx: &FixCx<'_>,
    range: TextRange,
    id: &ActionId,
) -> Result<Vec<PlainEdit>, Refusal> {
    let gone = || Refusal("the fix is no longer offered".to_owned());
    let ActionId::Fix { code, index } = id else {
        return Err(Refusal("not a fix of a server row".to_owned()));
    };
    let site = Site::of(cx).ok_or_else(gone)?;
    let report = cx.report.ok_or_else(gone)?;
    let index = usize::try_from(*index).map_err(|_| gone())?;
    report
        .diagnostics
        .iter()
        .filter(|d| {
            d.code == *code
                && d.range
                    .is_some_and(|at| at.start() <= range.end() && range.start() <= at.end())
        })
        .flat_map(|d| site.fixes(d))
        .nth(index)
        .map(|fix| fix.plain(site.path))
        .ok_or_else(gone)
}

/// The request's module, as the rows read it.
struct Site<'s> {
    cx: &'s FixCx<'s>,
    /// The module's path, which every edit names.
    path: &'s Path,
    /// Its text, the snapshot's, which its tree holds too.
    text: &'s str,
    /// The root of its tree.
    root: SyntaxNode<'s>,
}

impl<'s> Site<'s> {
    /// The module of `cx`, when its tree is of its text.
    fn of(cx: &'s FixCx<'s>) -> Option<Self> {
        let path = cx.path()?;
        let tree = cx.view.tree()?;
        (tree.text() == cx.text).then(|| Self {
            cx,
            path,
            text: cx.text,
            root: tree.root(),
        })
    }

    /// The fixes of `d` (X §6.2's S rows).
    fn fixes(&self, d: &PlainDiagnostic) -> Vec<Fix> {
        let Some(at) = d.range else {
            return Vec::new();
        };
        match d.code {
            codes::UNTERMINATED_STRING => self.close_string(at).into_iter().collect(),
            codes::UNCLOSED_DELIMITER => self.insert_closer(at, d).into_iter().collect(),
            codes::INVALID_ATTRIBUTE => self.remove_attribute(at).into_iter().collect(),
            codes::MISSING_BODY => self.add_body(at).into_iter().collect(),
            codes::TYPE_MISMATCH => self.wrap(at),
            codes::WRONG_ARGUMENT_COUNT => self.argument_count(at).into_iter().collect(),
            codes::NOT_A_TYPE => self.write_dyn(at).into_iter().collect(),
            codes::UNKNOWN_FIELD => self.closest_field(at).into_iter().collect(),
            codes::INVALID_RECORD => self.record_fields(at).into_iter().collect(),
            codes::REFUTABLE_PATTERN => self.refutable_let(at),
            codes::DYN_NEEDS_BOUND => self.add_bound(at).into_iter().collect(),
            codes::TARGET_MISMATCH => self.restrict_target(at),
            codes::UNUSED_PRIVATE_ITEM | codes::UNUSED_PACKAGE_ITEM => {
                self.remove_item(at).into_iter().collect()
            }
            codes::DISCARDED_RESULT => self.discard(at).into_iter().collect(),
            codes::IRREFUTABLE_LET_ASSERT => self.plain_let(at).into_iter().collect(),
            codes::UNREACHABLE_ARM => self.remove_arm(at).into_iter().collect(),
            _ => Vec::new(),
        }
    }

    // ---- the rows ---------------------------------------------------------

    /// E0010: a `"` at the end of the opening quote's line, or before the
    /// closers that end it when they close openers left open before the
    /// string, innermost first (D-1748). Those openers are read from the
    /// tokens of the statement that holds the string, earlier lines
    /// included, less those that a later line closes, and a closer that
    /// balances an opener inside the string's text stays in it. No fix when
    /// `//` follows the quote on its line, where the quote may belong in a
    /// comment, as in an unclosed URL (D-1755).
    fn close_string(&self, at: TextRange) -> Option<Fix> {
        if at.slice(self.text)? != "\"" {
            return None;
        }
        let start = self.line_start(at.start());
        let end = self.line_end(at.start());
        let before = TextRange::new(start, at.start()).slice(self.text)?;
        let rest = TextRange::new(at.end(), end).slice(self.text)?;
        let content = rest.trim_end_matches([' ', '\t']);
        if before.contains('"')
            || rest.contains(['"', '{', '}'])
            || rest.contains("//")
            || content.ends_with('\\')
        {
            return None;
        }
        let open = self
            .open_before(at)
            .unwrap_or_else(|| left_open(before.chars()));
        let open = self.closed_later(at, end, open);
        let kept = content
            .len()
            .checked_sub(balancing_closers(&open, content))?;
        let point = at.end().checked_add(size(kept)?)?;
        Some(Fix::new(
            "Close the string".to_owned(),
            vec![(TextRange::empty(point), "\"".to_owned())],
            false,
        ))
    }

    /// E0015: the closer of the one opener the diagnostic names, after the
    /// last token before the place the parser expected it.
    fn insert_closer(&self, at: TextRange, d: &PlainDiagnostic) -> Option<Fix> {
        let [opener] = d.related.as_slice() else {
            return None;
        };
        if opener.path != self.path {
            return None;
        }
        let close = match opener.range.slice(self.text)? {
            "(" => ")",
            "[" => "]",
            "{" => "}",
            "<" => ">",
            _ => return None,
        };
        let point = self
            .root
            .tokens()
            .take_while(|t| t.text_range().end() <= at.start())
            .filter(|t| is_significant(t.kind()))
            .last()?
            .text_range()
            .end();
        if point < opener.range.end() {
            return None;
        }
        Some(Fix::new(
            format!("Insert `{close}`"),
            vec![(TextRange::empty(point), close.to_owned())],
            false,
        ))
    }

    /// E0045: the attribute, with its line when it has one of its own.
    fn remove_attribute(&self, at: TextRange) -> Option<Fix> {
        let attr = self
            .node_at(at)?
            .ancestors()
            .find(|n| matches!(n.kind(), K::ATTR | K::INNER_ATTR))
            .filter(|n| n.text_range() == at)?;
        Some(Fix::new(
            "Remove the attribute".to_owned(),
            vec![(self.removal(attr.text_range()), String::new())],
            false,
        ))
    }

    /// E0046: a body `{ todo }` after the signature of the function named
    /// at `at`.
    fn add_body(&self, at: TextRange) -> Option<Fix> {
        let name = self.token_at(at)?;
        let decl = name.parent().filter(|n| n.kind() == K::FN_DECL)?;
        if decl.children().any(|c| c.kind() == K::BLOCK) {
            return None;
        }
        let end = decl
            .tokens()
            .rev()
            .find(|t| is_significant(t.kind()))?
            .text_range()
            .end();
        Some(Fix::new(
            "Add the body `{ todo }`".to_owned(),
            vec![(TextRange::empty(end), " {\n  todo\n}".to_owned())],
            true,
        ))
    }

    /// E0047 (X §6.2.1): wrap the expression in the constructor whose
    /// payload is the found type.
    fn wrap(&self, at: TextRange) -> Vec<Fix> {
        let Some(mismatch) = self
            .body(at.start())
            .and_then(|body| body.mismatch_at(at).copied())
        else {
            return Vec::new();
        };
        let snapshot = &self.cx.analyzed.snapshot;
        let types = &snapshot.globals().types;
        if matches!(types.kind(mismatch.found), TyKind::Error) {
            return Vec::new();
        }
        let TyKind::Named { def, args } = types.kind(mismatch.expected) else {
            return Vec::new();
        };
        let args: &[Ty] = args.as_ref();
        let mut ctors: Vec<(DefId, &str)> = Vec::new();
        if *def == builtin::OPTION && args == [mismatch.found] {
            ctors.push((builtin::SOME, "Some"));
        } else if *def == builtin::RESULT
            && let [ok, err] = args
        {
            if *ok == mismatch.found {
                ctors.push((builtin::OK, "Ok"));
            }
            if *err == mismatch.found {
                ctors.push((builtin::ERR, "Err"));
            }
        }
        ctors
            .into_iter()
            .filter(|&(ctor, _)| !hides_prelude(snapshot, self.cx.view, ctor, None))
            .map(|(_, name)| {
                Fix::new(
                    format!("Wrap in `{name}(…)`"),
                    vec![
                        (TextRange::empty(at.start()), format!("{name}(")),
                        (TextRange::empty(at.end()), ")".to_owned()),
                    ],
                    false,
                )
            })
            .collect()
    }

    /// E0048: `todo` for each missing argument, or the extra ones removed,
    /// by the callee's parameters (`CallFact.params`).
    fn argument_count(&self, at: TextRange) -> Option<Fix> {
        let arg_list = self
            .node_at(at)?
            .ancestors()
            .find(|n| n.kind() == K::ARG_LIST)
            .filter(|n| n.text_range() == at)?;
        let call = arg_list.parent().filter(|n| n.kind() == K::CALL_EXPR)?;
        if call.parent().is_some_and(|p| p.kind() == K::USE_STMT) {
            return None;
        }
        let callee = call.first_child().filter(|c| *c != arg_list)?;
        let want = self.body(at.start())?.call(callee.id())?.params.len();
        let args: Vec<SyntaxNode<'_>> = arg_list.children().collect();
        if args.iter().any(|a| a.kind() == K::ERROR) {
            return None;
        }
        let open = arg_list.first_token().filter(|t| t.kind() == K::L_PAREN)?;
        if let Some(missing) = want.checked_sub(args.len()).filter(|&n| n > 0) {
            let todos = vec!["todo"; missing].join(", ");
            let edit = match args.last() {
                Some(last) => (
                    TextRange::empty(last.text_range().end()),
                    format!(", {todos}"),
                ),
                None => (TextRange::empty(open.text_range().end()), todos),
            };
            return Some(Fix::new(
                "Add the missing arguments as `todo`".to_owned(),
                vec![edit],
                false,
            ));
        }
        let last = args.last()?;
        let from = match want.checked_sub(1) {
            Some(kept) => args.get(kept)?.text_range().end(),
            None => open.text_range().end(),
        };
        (args.len() > want).then(|| {
            Fix::new(
                "Remove the extra arguments".to_owned(),
                vec![(TextRange::new(from, last.text_range().end()), String::new())],
                false,
            )
        })
    }

    /// E0052: `dyn` before an interface named where a type is expected.
    fn write_dyn(&self, at: TextRange) -> Option<Fix> {
        let ty = self
            .node_at(at)?
            .ancestors()
            .find(|n| n.kind() == K::PATH_TYPE)
            .filter(|n| n.text_range().start() == at.start())?;
        if ty.parent().is_some_and(|p| p.kind() == K::DYN_TYPE) {
            return None;
        }
        let path = ty.first_child().filter(|p| p.kind() == K::PATH)?;
        let Res::Def(def) = self.res_of(path)? else {
            return None;
        };
        if self.item(def)?.kind != DefKind::Interface {
            return None;
        }
        Some(Fix::new(
            format!("Write `dyn {}`", ty.text()),
            vec![(TextRange::empty(ty.text_range().start()), "dyn ".to_owned())],
            false,
        ))
    }

    /// E0058: the field of the receiver's type closest to the one written,
    /// also at a step of a field path (D-1694).
    fn closest_field(&self, at: TextRange) -> Option<Fix> {
        let fact = self.body(at.start())?.field_at(at.start())?;
        if fact.name_span.range != at || fact.ty.is_some() {
            return None;
        }
        let fields = self.common_fields(fact.owner?)?;
        let names = &self.cx.analyzed.snapshot.globals().names;
        if fields.contains(&names.get(fact.name)) {
            return None;
        }
        let written = at.slice(self.text)?;
        let name = closest_name(written, fields.iter().copied())?;
        Some(Fix::new(
            format!("Use the field `{name}`"),
            vec![(at, name.to_owned())],
            true,
        ))
    }

    /// E0059: the missing fields of a construction as `todo`, or a field
    /// written twice or unknown removed.
    fn record_fields(&self, at: TextRange) -> Option<Fix> {
        let element = self.root.covering_element(at)?;
        let record = element
            .parent()?
            .ancestors()
            .find(|n| n.kind() == K::RECORD_EXPR)?;
        let inits: Vec<SyntaxNode<'_>> = record
            .children()
            .filter(|c| c.kind() == K::FIELD_INIT)
            .collect();
        // Field paths and updates are D-1641's and D-1643's kinds.
        if record
            .children()
            .any(|c| matches!(c.kind(), K::SPREAD | K::ERROR))
            || inits
                .iter()
                .any(|init| init.first_token_of_kind(K::DOT).is_some())
        {
            return None;
        }
        let path = record.first_child().filter(|p| p.kind() == K::PATH)?;
        let Res::Def(ctor) = self.res_of(path)? else {
            return None;
        };
        let ItemSig::Ctor(sig) = &self.item(ctor)?.sig else {
            return None;
        };
        let CtorFields::Named(declared) = &sig.fields else {
            return None;
        };
        let names = &self.cx.analyzed.snapshot.globals().names;
        let declared: Vec<&str> = declared.iter().map(|(name, _)| names.get(*name)).collect();
        let written: Vec<&str> = inits.iter().filter_map(|init| field_name(*init)).collect();
        if path.text_range() == at {
            let missing: Vec<String> = declared
                .iter()
                .filter(|name| !written.contains(name))
                .map(|name| format!("{name}: todo"))
                .collect();
            if missing.is_empty() {
                return None;
            }
            let entries = missing.join(", ");
            let edit = if let Some(last) = inits.last() {
                (
                    TextRange::empty(last.text_range().end()),
                    format!(", {entries}"),
                )
            } else {
                let open = record.first_token_of_kind(K::L_BRACE)?;
                (
                    TextRange::empty(open.text_range().end()),
                    format!(" {entries} "),
                )
            };
            return Some(Fix::new(
                "Add the missing fields as `todo`".to_owned(),
                vec![edit],
                true,
            ));
        }
        let position = inits.iter().position(|init| {
            init.first_token_of_kind(K::LOWER_IDENT)
                .is_some_and(|t| t.text_range() == at)
        })?;
        let init = *inits.get(position)?;
        let name = field_name(init)?;
        let repeated = written.get(..position)?.contains(&name);
        if !repeated && declared.contains(&name) {
            return None;
        }
        Some(Fix::new(
            format!("Remove the field `{name}`"),
            vec![(self.entry_removal(init)?, String::new())],
            true,
        ))
    }

    /// E0060: `let assert`, or `let guard … else { todo }`.
    fn refutable_let(&self, at: TextRange) -> Vec<Fix> {
        let Some(stmt) = self.let_of(at) else {
            return Vec::new();
        };
        if stmt.kind() != K::LET_STMT || stmt.first_token_of_kind(K::ASSERT_KW).is_some() {
            return Vec::new();
        }
        let indent = self.indent(stmt.text_range().start());
        let end = stmt.text_range().end();
        vec![
            Fix::new(
                "Use `let assert`".to_owned(),
                vec![(TextRange::empty(at.start()), "assert ".to_owned())],
                false,
            ),
            Fix::new(
                "Use `let guard … else { todo }`".to_owned(),
                vec![
                    (TextRange::empty(at.start()), "guard ".to_owned()),
                    (
                        TextRange::empty(end),
                        format!(" else {{\n{indent}  todo\n{indent}}}"),
                    ),
                ],
                false,
            ),
        ]
    }

    /// E0065: a bound on the type parameter that lacks a descriptor, by
    /// the interface its leaf requires.
    fn add_bound(&self, at: TextRange) -> Option<Fix> {
        let item = scope::body_at(self.cx.view, at.start())?;
        let fact = self.cx.view.facts()?.body(item)?.unsatisfied_at(at)?;
        let (leaf, param) = descriptor_leaf(&fact.err)?;
        let Want::Iface(iface) = leaf.want else {
            return None;
        };
        let iface_name = self.bare_name(iface, Namespace::Type)?;
        let resolved = self.cx.view.resolved()?;
        let snapshot = &self.cx.analyzed.snapshot;
        let names = &snapshot.globals().names;
        let (name, _) = resolved
            .generics
            .get(&item)?
            .iter()
            .find(|(_, index)| *index == param)?;
        let name = names.get(*name);
        let decl = self.cx.view.tree()?.node(resolved.item(item)?.node)?;
        let ty_param = decl
            .children()
            .find(|c| c.kind() == K::TYPE_PARAM_LIST)?
            .children()
            .filter(|c| c.kind() == K::TYPE_PARAM)
            .find(|p| {
                p.first_token_of_kind(K::UPPER_IDENT)
                    .is_some_and(|t| t.text() == name)
            })?;
        let edit = match ty_param.children().find(|c| c.kind() == K::BOUND_LIST) {
            Some(bounds) => (
                TextRange::empty(bounds.text_range().end()),
                format!(" + {iface_name}"),
            ),
            None => (
                TextRange::empty(
                    ty_param
                        .first_token_of_kind(K::UPPER_IDENT)?
                        .text_range()
                        .end(),
                ),
                format!(": {iface_name}"),
            ),
        };
        Some(Fix::new(
            format!("Add bound `{name}: {iface_name}`"),
            vec![edit],
            false,
        ))
    }

    /// E0067: `@target(t)` on the using item, or `@!target(t)` on the
    /// module, for the one target the used item exists on.
    fn restrict_target(&self, at: TextRange) -> Vec<Fix> {
        let Some(target) = self.used_target(at) else {
            return Vec::new();
        };
        let Some(item) = self
            .node_at(at)
            .and_then(|n| n.ancestors().find(|a| a.parent() == Some(self.root)))
        else {
            return Vec::new();
        };
        let mut fixes = Vec::new();
        if !item
            .children()
            .any(|c| c.kind() == K::ATTR && attr_name(c) == Some("target"))
        {
            let before = item
                .children_with_tokens()
                .find(|e| {
                    !matches!(
                        e.kind(),
                        K::DOC_COMMENT | K::NEWLINE | K::WHITESPACE | K::COMMENT
                    )
                })
                .map(SyntaxElement::text_range);
            if let Some(before) = before {
                let indent = self.indent(before.start());
                fixes.push(Fix::new(
                    format!("Add `@target({target})`"),
                    vec![(
                        TextRange::empty(before.start()),
                        format!("@target({target})\n{indent}"),
                    )],
                    false,
                ));
            }
        }
        if !self
            .root
            .children()
            .any(|c| c.kind() == K::INNER_ATTR && attr_name(c) == Some("target"))
        {
            let point = self
                .root
                .children_with_tokens()
                .filter(|e| e.kind() == K::MODULE_DOC)
                .last()
                .map_or(0, |doc| self.after_newline(doc.text_range().end()));
            fixes.push(Fix::new(
                format!("Add `@!target({target})`"),
                vec![(TextRange::empty(point), format!("@!target({target})\n"))],
                false,
            ));
        }
        fixes
    }

    /// W0004, W0005: the top-level item named at `at`, with its doc
    /// comment, its attributes and a blank line beside it.
    fn remove_item(&self, at: TextRange) -> Option<Fix> {
        let name = self.token_at(at)?;
        let item = name.parent().filter(|n| {
            n.parent() == Some(self.root)
                && matches!(
                    n.kind(),
                    K::FN_DECL | K::TYPE_DECL | K::ALIAS_DECL | K::CONST_DECL | K::INTERFACE_DECL
                )
        })?;
        let lines = self.whole_lines(item.text_range())?;
        let range = if self.is_blank_line(lines.end()) {
            TextRange::new(lines.start(), self.after_newline(lines.end()))
        } else {
            match self.blank_line_before(lines.start()) {
                Some(blank) => TextRange::new(blank, lines.end()),
                None => lines,
            }
        };
        Some(Fix {
            title: format!("Remove the unused `{}`", name.text()),
            edits: vec![(range, String::new())],
            preferred: false,
            removes: true,
        })
    }

    /// W0006: `let _ =` before a discarded `Result`.
    fn discard(&self, at: TextRange) -> Option<Fix> {
        let expr = self
            .node_at(at)?
            .ancestors()
            .find(|n| n.text_range() == at && n.parent().is_some_and(|p| p.kind() == K::BLOCK))?;
        expr.next_sibling()?;
        Some(Fix::new(
            "Discard the value with `let _ =`".to_owned(),
            vec![(TextRange::empty(at.start()), "let _ = ".to_owned())],
            false,
        ))
    }

    /// W0008: `let assert p` to `let p`, or the `else` of a `let guard`
    /// dropped.
    fn plain_let(&self, at: TextRange) -> Option<Fix> {
        let stmt = self.let_of(at)?;
        if let Some(keyword) = stmt.first_token_of_kind(K::ASSERT_KW) {
            return Some(Fix::new(
                format!("Write `let {}`", at.slice(self.text)?),
                vec![(self.with_space_after(keyword.text_range()), String::new())],
                true,
            ));
        }
        let keyword = stmt.first_token_of_kind(K::GUARD_KW)?;
        let clause = stmt
            .children()
            .find(|c| c.kind() == K::ELSE_CLAUSE)?
            .text_range();
        Some(Fix::new(
            "Drop the `else`".to_owned(),
            vec![
                (self.with_space_after(keyword.text_range()), String::new()),
                (self.with_space_before(clause), String::new()),
            ],
            true,
        ))
    }

    /// W0010: the arm whose pattern is the span (D-1687).
    fn remove_arm(&self, at: TextRange) -> Option<Fix> {
        let arm = self
            .node_at(at)?
            .ancestors()
            .find(|n| n.kind() == K::CASE_ARM)?;
        if arm.first_child()?.text_range() != at {
            return None;
        }
        let mut range = arm.text_range();
        if let Some(SyntaxElement::Token(comma)) = arm.next_sibling_or_token()
            && comma.kind() == K::COMMA
        {
            range = range.cover(comma.text_range());
        }
        Some(Fix::new(
            "Remove the arm".to_owned(),
            vec![(self.whole_lines(range)?, String::new())],
            true,
        ))
    }

    // ---- reading the module -----------------------------------------------

    /// The facts of the body that holds `offset`.
    fn body(&self, offset: TextSize) -> Option<&'s cheby_driver::analysis::BodyFacts> {
        let item = scope::body_at(self.cx.view, offset)?;
        self.cx.view.facts()?.body(item)
    }

    /// The item `def` of the run.
    fn item(&self, def: DefId) -> Option<&'s cheby_driver::analysis::IfaceItem> {
        self.cx
            .analyzed
            .snapshot
            .interfaces()
            .get(def.module)?
            .item(def.index)
    }

    /// What the `PATH` node `path` names: a signature's path or a body's.
    fn res_of(&self, path: SyntaxNode<'_>) -> Option<Res> {
        let resolved = self.cx.view.resolved()?;
        if let Some(res) = resolved.sig_paths.get(&path.id()) {
            return Some(*res);
        }
        let item = scope::body_at(self.cx.view, path.text_range().start())?;
        resolved.bodies.get(&item)?.paths.get(&path.id()).copied()
    }

    /// The name by which the module writes `def` bare, in `ns`: its own
    /// item or an import's, or a prelude item it does not hide (D-1678).
    fn bare_name(&self, def: DefId, ns: Namespace) -> Option<&'static str> {
        let snapshot = &self.cx.analyzed.snapshot;
        let item = self.item(def)?;
        let in_scope = self
            .cx
            .view
            .resolved()
            .and_then(|resolved| resolved.scope.get(ns, item.name))
            .is_some_and(|entry| entry.res == Res::Def(def));
        let prelude = builtin::is_prelude(def) && !hides_prelude(snapshot, self.cx.view, def, None);
        (in_scope || prelude).then(|| snapshot.globals().names.get(item.name))
    }

    /// The field names every constructor of the type `owner` has, when
    /// the module may see its structure.
    fn common_fields(&self, owner: DefId) -> Option<Vec<&'static str>> {
        let snapshot = &self.cx.analyzed.snapshot;
        let iface = snapshot.interfaces().get(owner.module)?;
        let ty = iface.item(owner.index)?;
        let ItemSig::Type(def) = &ty.sig else {
            return None;
        };
        let viewer = snapshot.table().get(self.cx.view.id())?;
        if !structure_visible(
            ty,
            owner.module,
            iface.package,
            self.cx.view.id(),
            viewer.package,
        ) {
            return None;
        }
        let TypeDefKind::Adt { ctors } = &def.kind else {
            return None;
        };
        let names = &snapshot.globals().names;
        let mut common: Option<Vec<&'static str>> = None;
        for &ctor in ctors {
            let ItemSig::Ctor(sig) = &iface.item(ctor)?.sig else {
                return None;
            };
            let CtorFields::Named(fields) = &sig.fields else {
                return None;
            };
            let own: Vec<&'static str> = fields.iter().map(|(name, _)| names.get(*name)).collect();
            common = Some(match common {
                None => own,
                Some(before) => before.into_iter().filter(|n| own.contains(n)).collect(),
            });
        }
        common
    }

    /// The one target the item used at `at` exists on (E0067).
    fn used_target(&self, at: TextRange) -> Option<&'static str> {
        let path = self
            .node_at(at)?
            .ancestors()
            .find(|n| n.kind() == K::PATH)?;
        let Res::Def(def) = self.res_of(path)? else {
            return None;
        };
        let mut targets = self.item(def)?.targets.iter();
        let (Some(target), None) = (targets.next(), targets.next()) else {
            return None;
        };
        Some(match target {
            Target::Native => "native",
            Target::Js => "js",
        })
    }

    /// The openers left open before `at`, the quote of an unclosed string,
    /// in the statement that holds it, or in the item outside a block, read
    /// from the tree's tokens, outermost first (D-1748).
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "a match on syntax kinds names only the kinds it handles"
    )]
    fn open_before(&self, at: TextRange) -> Option<Vec<char>> {
        let quote = self.token_at(at)?;
        let unit = quote.parent()?.ancestors().find(|node| {
            node.parent()
                .is_none_or(|parent| matches!(parent.kind(), K::BLOCK | K::SOURCE_FILE))
        })?;
        let mut open = Vec::new();
        for token in unit
            .tokens()
            .take_while(|token| token.text_range().end() <= at.start())
        {
            match token.kind() {
                K::L_PAREN => open.push('('),
                K::L_BRACK => open.push('['),
                K::L_BRACE | K::INTERP_START => open.push('{'),
                K::R_PAREN | K::R_BRACK | K::R_BRACE | K::INTERP_END => {
                    let _ = open.pop();
                }
                _ => {}
            }
        }
        Some(open)
    }

    /// `open`, the openers left open before `at`, less those that a closer
    /// on a later line closes (D-1748). An unclosed string runs to the end
    /// of the file, so the later lines are read from the text with the
    /// string closed at `end`, the end of its line, parsed in memory: its
    /// tokens after that line, up to the end of the block that holds the
    /// statement, or of the file, close the outermost opener first, once the
    /// openers they open themselves are closed, since the closers that end
    /// the string's line close the innermost ones. So `say(\n  "hello)\n)`
    /// leaves none, and the quote goes at the end of the line, while in the
    /// arm `1 => say("hello)` the case's `}` on a later line leaves the
    /// call's `(`, and the quote goes before the `)`.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "a match on syntax kinds names only the kinds it handles"
    )]
    fn closed_later(&self, at: TextRange, end: TextSize, mut open: Vec<char>) -> Vec<char> {
        let (Some(tree), Ok(cut)) = (self.cx.view.tree(), usize::try_from(end)) else {
            return open;
        };
        let (Some(head), Some(tail)) = (self.text.get(..cut), self.text.get(cut..)) else {
            return open;
        };
        if open.is_empty() || tail.trim().is_empty() {
            return open;
        }
        let parsed = cheby_syntax::parse(tree.file(), Arc::from(format!("{head}\"{tail}")));
        let root = parsed.tree().root();
        let Some(unit) = root
            .covering_element(at)
            .and_then(|element| match element {
                SyntaxElement::Node(node) => Some(node),
                SyntaxElement::Token(token) => token.parent(),
            })
            .and_then(|node| {
                node.ancestors().find(|node| {
                    node.parent()
                        .is_none_or(|parent| matches!(parent.kind(), K::BLOCK | K::SOURCE_FILE))
                })
            })
        else {
            return open;
        };
        let scope = unit.parent().unwrap_or(unit);
        // A block's own `}` closes none of the statement's openers.
        let stop = scope
            .tokens()
            .rev()
            .find(|token| !token.kind().is_trivia())
            .filter(|token| scope.kind() == K::BLOCK && token.kind() == K::R_BRACE)
            .map_or_else(
                || scope.text_range().end(),
                |token| token.text_range().start(),
            );
        let after = end.saturating_add(1);
        let mut later = 0_usize;
        // How many of the outermost openers a later closer closed.
        let mut outer = 0_usize;
        for token in scope
            .tokens()
            .filter(|token| token.text_range().start() >= after && token.text_range().end() <= stop)
        {
            if outer >= open.len() {
                break;
            }
            match token.kind() {
                K::L_PAREN | K::L_BRACK | K::L_BRACE | K::INTERP_START => {
                    later = later.saturating_add(1);
                }
                K::R_PAREN | K::R_BRACK | K::R_BRACE | K::INTERP_END => {
                    match later.checked_sub(1) {
                        Some(left) => later = left,
                        None => outer = outer.saturating_add(1),
                    }
                }
                _ => {}
            }
        }
        open.split_off(outer.min(open.len()))
    }

    /// The `let` or `let guard` statement whose pattern is `at`.
    fn let_of(&self, at: TextRange) -> Option<SyntaxNode<'s>> {
        let stmt = self
            .node_at(at)?
            .ancestors()
            .find(|n| matches!(n.kind(), K::LET_STMT | K::GUARD_STMT))?;
        let pattern = stmt.children().next()?;
        (pattern.text_range() == at).then_some(stmt)
    }

    /// The innermost node that holds `at`.
    fn node_at(&self, at: TextRange) -> Option<SyntaxNode<'s>> {
        match self.root.covering_element(at)? {
            SyntaxElement::Node(node) => Some(node),
            SyntaxElement::Token(token) => token.parent(),
        }
    }

    /// The token whose range is `at`.
    fn token_at(&self, at: TextRange) -> Option<SyntaxToken<'s>> {
        self.root
            .covering_element(at)?
            .as_token()
            .filter(|t| t.text_range() == at)
    }

    // ---- reading the text -------------------------------------------------

    /// The start of the line that holds `offset`.
    fn line_start(&self, offset: TextSize) -> TextSize {
        TextRange::new(0, offset)
            .slice(self.text)
            .and_then(|before| before.rfind('\n'))
            .and_then(|at| size(at.checked_add(1)?))
            .unwrap_or(0)
    }

    /// The end of the text of the line that holds `offset`, before its
    /// line break.
    fn line_end(&self, offset: TextSize) -> TextSize {
        let Some(rest) = usize::try_from(offset)
            .ok()
            .and_then(|start| self.text.get(start..))
        else {
            return offset;
        };
        let line = rest.split('\n').next().unwrap_or(rest);
        let line = line.strip_suffix('\r').unwrap_or(line);
        size(line.len())
            .and_then(|len| offset.checked_add(len))
            .unwrap_or(offset)
    }

    /// The offset after the line break of the line that holds `offset`, or
    /// the end of the text.
    fn after_newline(&self, offset: TextSize) -> TextSize {
        let Some(rest) = usize::try_from(offset)
            .ok()
            .and_then(|start| self.text.get(start..))
        else {
            return offset;
        };
        let len = rest
            .find('\n')
            .map_or(rest.len(), |at| at.saturating_add(1));
        size(len)
            .and_then(|len| offset.checked_add(len))
            .unwrap_or(offset)
    }

    /// The indentation of the line that holds `offset`.
    fn indent(&self, offset: TextSize) -> &'s str {
        let start = self.line_start(offset);
        let line = TextRange::new(start, self.line_end(start))
            .slice(self.text)
            .unwrap_or("");
        let text = line.trim_start_matches([' ', '\t']);
        line.get(..line.len().saturating_sub(text.len()))
            .unwrap_or("")
    }

    /// Whether the line that starts at `offset` holds nothing but spaces.
    fn is_blank_line(&self, offset: TextSize) -> bool {
        offset < size(self.text.len()).unwrap_or(0)
            && TextRange::new(offset, self.line_end(offset))
                .slice(self.text)
                .is_some_and(|line| line.trim().is_empty())
    }

    /// The start of the blank line right before the line at `start`.
    fn blank_line_before(&self, start: TextSize) -> Option<TextSize> {
        let previous = self.line_start(start.checked_sub(1)?);
        self.is_blank_line(previous).then_some(previous)
    }

    /// `range` widened to the whole lines it spans, their line breaks
    /// included, when nothing but spaces shares those lines with it, and a
    /// comment after it, which goes with it.
    fn whole_lines(&self, range: TextRange) -> Option<TextRange> {
        let start = self.line_start(range.start());
        let end = self.line_end(range.end());
        let before = TextRange::new(start, range.start()).slice(self.text)?;
        let after = TextRange::new(range.end(), end).slice(self.text)?.trim();
        (before.trim().is_empty() && (after.is_empty() || after.starts_with("//")))
            .then(|| TextRange::new(start, self.after_newline(range.end())))
    }

    /// What removing the node at `range` deletes: its lines when it has
    /// them to itself, else it and the spaces after it.
    fn removal(&self, range: TextRange) -> TextRange {
        self.whole_lines(range)
            .unwrap_or_else(|| self.with_space_after(range))
    }

    /// `range` with the spaces and tabs after it.
    fn with_space_after(&self, range: TextRange) -> TextRange {
        let rest = TextRange::new(range.end(), self.line_end(range.end()))
            .slice(self.text)
            .unwrap_or("");
        let spaces = rest
            .len()
            .saturating_sub(rest.trim_start_matches([' ', '\t']).len());
        size(spaces)
            .and_then(|n| range.end().checked_add(n))
            .map_or(range, |end| TextRange::new(range.start(), end))
    }

    /// `range` with the spaces and tabs before it on its line.
    fn with_space_before(&self, range: TextRange) -> TextRange {
        let before = TextRange::new(self.line_start(range.start()), range.start())
            .slice(self.text)
            .unwrap_or("");
        let spaces = before
            .len()
            .saturating_sub(before.trim_end_matches([' ', '\t']).len());
        size(spaces)
            .and_then(|n| range.start().checked_sub(n))
            .map_or(range, |start| TextRange::new(start, range.end()))
    }

    /// What removing the record entry `init` deletes: its lines and comma
    /// when it has lines of its own, else it and the comma and spaces
    /// after it, or before it when it is the last entry.
    fn entry_removal(&self, init: SyntaxNode<'_>) -> Option<TextRange> {
        let comma_after = init
            .next_sibling_or_token()
            .and_then(SyntaxElement::as_token)
            .filter(|t| t.kind() == K::COMMA);
        if let Some(comma) = comma_after {
            let range = init.text_range().cover(comma.text_range());
            return Some(
                self.whole_lines(range)
                    .unwrap_or_else(|| self.with_space_after(range)),
            );
        }
        let comma_before = std::iter::successors(init.prev_sibling_or_token(), |e| match e {
            SyntaxElement::Token(t) if !is_significant(t.kind()) => t.prev_sibling(),
            SyntaxElement::Token(_) | SyntaxElement::Node(_) => None,
        })
        .find_map(|e| e.as_token().filter(|t| t.kind() == K::COMMA))?;
        Some(init.text_range().cover(comma_before.text_range()))
    }
}

/// The name of the field `init` sets, when it is one name.
fn field_name(init: SyntaxNode<'_>) -> Option<&str> {
    init.first_token_of_kind(K::LOWER_IDENT)
        .map(SyntaxToken::text)
}

/// The name of the attribute `attr`.
fn attr_name(attr: SyntaxNode<'_>) -> Option<&str> {
    attr.first_token_of_kind(K::LOWER_IDENT)
        .map(SyntaxToken::text)
}

/// The leaf of `err` whose type parameter has no bound to supply its
/// descriptor (E0065), with that parameter.
fn descriptor_leaf(err: &Unsatisfied) -> Option<(&Unsatisfied, ParamIdx)> {
    match &err.reason {
        Reason::Descriptor { param } => Some((err, *param)),
        Reason::Element { inner, .. } | Reason::Bound { inner, .. } => descriptor_leaf(inner),
        Reason::Missing { .. }
        | Reason::LessVisible { .. }
        | Reason::KeyFunction { .. }
        | Reason::CallSiteFunction { .. }
        | Reason::Signature { .. }
        | Reason::HomeNotInDeps { .. }
        | Reason::NoModule
        | Reason::Param { .. }
        | Reason::Dyn { .. }
        | Reason::Undetermined { .. }
        | Reason::TooDeep => None,
    }
}

/// The candidate closest to `target` by edit distance, when it is close
/// enough to be a likely typo: at most a third of the length away, one
/// edit at least, as the compiler's did-you-mean reads it. Ties go to the
/// earliest candidate.
fn closest_name<'a>(
    target: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let limit = (target.chars().count() / 3).max(1);
    let mut best: Option<(usize, &'a str)> = None;
    for candidate in candidates {
        if candidate == target {
            continue;
        }
        let distance = edit_distance(target, candidate);
        if distance <= limit && best.is_none_or(|(d, _)| distance < d) {
            best = Some((distance, candidate));
        }
    }
    best.map(|(_, name)| name)
}

/// The Levenshtein distance of `a` and `b`, by characters.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len().saturating_add(1)];
    for (i, ca) in a.chars().enumerate() {
        if let Some(first) = cur.first_mut() {
            *first = i.saturating_add(1);
        }
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let delete = prev
                .get(j.saturating_add(1))
                .map_or(usize::MAX, |d| d.saturating_add(1));
            let insert = cur.get(j).map_or(usize::MAX, |d| d.saturating_add(1));
            let replace = prev.get(j).map_or(usize::MAX, |d| d.saturating_add(cost));
            if let Some(slot) = cur.get_mut(j.saturating_add(1)) {
                *slot = delete.min(insert).min(replace);
            }
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev.last().copied().unwrap_or(0)
}

/// The openers that `text` leaves open, outermost first.
fn left_open(text: impl Iterator<Item = char>) -> Vec<char> {
    let mut open: Vec<char> = Vec::new();
    for c in text {
        match c {
            '(' | '[' | '{' => open.push(c),
            ')' | ']' | '}' => {
                let _ = open.pop();
            }
            _ => {}
        }
    }
    open
}

/// How many bytes at the end of `content`, the text after an unclosed
/// string's quote on its line, are closers that close `open`, the openers
/// left open before the quote, outermost first: the longest run of `)`
/// and `]` ending `content` whose closers match those openers, innermost
/// first (D-1748). The first closers of the run that balance openers of
/// `content` itself stay in the string, so `"(done)` keeps its `)`.
/// Zero when there is none.
fn balancing_closers(open: &[char], content: &str) -> usize {
    let run: Vec<char> = content
        .chars()
        .rev()
        .take_while(|c| matches!(c, ')' | ']'))
        .collect();
    let inner = content
        .get(..content.len().saturating_sub(run.len()))
        .map_or(0, |text| left_open(text.chars()).len());
    // The closers in written order: the first closes the innermost opener.
    let written: Vec<char> = run.iter().rev().skip(inner).copied().collect();
    let mut best = 0_usize;
    for len in 1..=written.len() {
        let Some(tail) = written.get(written.len().saturating_sub(len)..) else {
            break;
        };
        let matches = tail.len() <= open.len()
            && tail
                .iter()
                .zip(open.iter().rev())
                .all(|(&closer, &opener)| matches!((opener, closer), ('(', ')') | ('[', ']')));
        if matches {
            best = len;
        }
    }
    best
}

/// `n` as an offset.
fn size(n: usize) -> Option<TextSize> {
    TextSize::try_from(n).ok()
}

#[cfg(test)]
#[path = "fixes_tests.rs"]
mod tests;
