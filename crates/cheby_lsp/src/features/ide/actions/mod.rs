//! Code actions as `ide` computes them, free of protocol types (X §6.1,
//! G1 plan §4.2).
//!
//! Each provider of [`registry`] lists the [`Action`]s it offers for a
//! range, from a [`FixCx`], and each resolver computes the edits of an
//! action whose `data` it named. The frame, `handlers::code_action`, turns
//! them into protocol actions: it selects the diagnostics, applies the hash
//! rule (D-1393) and D-1360's rule, filters by `only`, orders, sets
//! `isPreferred`, and converts every edit through `handlers::edit`.
//!
//! - Every edit is a [`PlainEdit`] with a byte range in its file's live
//!   text (G1 plan §1.1).
//! - A same-file fix's edits are formatted on the lines they touch by the
//!   frame ([`formatted`]), the fixes that rename excepted, E0044's and
//!   E0006's (D-1741), and kept raw when the file has a grammar error; a
//!   cross-file fix's resolver formats its own the same way, with the same
//!   function (D-1735).
//! - A cross-file resolver returns its edits, and the frame runs `verify`
//!   on them through `rename::check` with `rename::Action::Fix`
//!   (D-1684, D-1731).
//! - `removes` marks an action that removes code for W0003, W0004 or
//!   W0005, or organize imports' removals; the frame drops them while the
//!   report is substituted, so no provider repeats D-1360's rule.
//! - Every provider reads the session's modules through `is_own` and
//!   `own_modules`, and a std item through `StdIndex` only outside a std
//!   checkout (D-1706).
//! - Titles: an H or C row takes its help's text (X §6.2), D-1694's two
//!   rows and the server rows theirs (D-1694, D-1736).

pub(crate) mod generic;
pub(crate) mod registry;

use std::path::Path;
use std::sync::Arc;

use cheby_driver::analysis::codes::{self, Code};
use cheby_driver::analysis::{
    DefId, Diagnostic, FileLocation, FileReport, ModuleView, PlainDiagnostic, PlainEdit, PlainHelp,
    Snapshot, TextRange, TextSize, builtin,
};
use cheby_syntax::{SyntaxKind, SyntaxNode};
use serde::{Deserialize, Serialize};

use super::Refusal;
use super::edits::{AtCap, format_touching_with};
use super::print::hides_prelude;
use crate::features::stdlib::StdService;
use crate::features::{Analyzed, CancelToken};

/// One action a provider offers (X §6.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Action {
    /// The title the client shows (X §6.2, D-1736).
    pub title: String,
    /// The kind, which `only` filters by.
    pub kind: ActionKind,
    /// The diagnostic the action fixes, if any.
    pub diagnostic: Option<DiagRef>,
    /// The row marks it preferred (X §6.1); the frame also sets
    /// `isPreferred` when a diagnostic has exactly one edit-bearing fix.
    pub preferred: bool,
    /// It removes code for W0003, W0004 or W0005, or is organize imports'
    /// removal, which the frame drops while the report is substituted
    /// (D-1360).
    pub removes: bool,
    /// The edits, when they are computed at listing. `Some` of an empty
    /// list for an action with nothing to do, such as organize imports on
    /// imports in order (D-1691): it goes out with no `edit`, a resolve
    /// answers it as it was sent (D-1733), and with no `only` that names
    /// `source` or `source.organizeImports` the frame omits it.
    pub edit: Option<Vec<PlainEdit>>,
    /// What a resolve computes, when the edits are not computed at listing
    /// (D-1683).
    pub data: Option<ActionId>,
    /// Why the action cannot be applied, for a client that shows it; the
    /// frame omits such an action for any other client (D-1690).
    pub disabled: Option<String>,
    /// The server command the action runs instead of an edit.
    pub command: Option<ActionCommand>,
}

/// The kind of an action (X §6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActionKind {
    /// `quickfix`: a fix of a diagnostic, and Explain.
    QuickFix,
    /// `refactor.rewrite`: a rewrite such as "Use a field path".
    RefactorRewrite,
    /// `source.organizeImports` (D-1691).
    SourceOrganizeImports,
    /// `source`: "Satisfy interface…" (D-1696).
    Source,
}

/// A command an action runs (X §8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActionCommand {
    /// `cheby.explain` with the code as its argument (D-1693).
    Explain(Code),
}

/// The diagnostic an action names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiagRef {
    /// An index into the diagnostics a provider gets, which are the
    /// published report's.
    Report(usize),
    /// An index into [`FixCx::context`], the request's own diagnostics, for
    /// a diagnostic that is in no report, such as a child's code (G1 plan
    /// §1.6).
    Context(usize),
}

/// What a resolve computes (X §6.1, W11's part). The wire `data` holds it
/// with the document's URI and the text's hash (D-1683, D-1733), and the
/// action's range, which the resolver gets beside it. A provider that needs
/// data no variant carries asks the lead (G1 plan §0).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum ActionId {
    /// A server row's fix of the diagnostic `code` at the range, the
    /// `index`-th that its row offers (`fixes`).
    Fix {
        /// The fixed diagnostic's code.
        #[serde(with = "code_text")]
        code: Code,
        /// Which of the row's fixes.
        index: u32,
    },
    /// Import `module`, as the E0042 rows do (`imports_actions`).
    AddImport {
        /// The module path, as `std::list`.
        module: String,
    },
    /// Remove the import at the range, W0003 (`imports_actions`).
    RemoveImport,
    /// Organize the file's imports (`imports_actions`, D-1691).
    OrganizeImports,
    /// Add the missing arms of the `case` at the range (`fill_arms`).
    FillArms,
    /// Add stubs for `interface`: `function` alone, or every missing
    /// function when it is `None` (`stubs`, D-1696).
    Stub {
        /// The interface, as its path.
        interface: String,
        /// The one function, or `None` for all of them.
        function: Option<String>,
    },
    /// Raise `item` of `module` to the visibility a use needs (`stubs`,
    /// D-1706).
    MakeVisible {
        /// The module that declares the item, as its path.
        module: String,
        /// The item's name.
        item: String,
    },
    /// The `index`-th habit rewrite at the range (`habits`, X §6.5).
    Habit {
        /// Which rewrite.
        index: u32,
    },
    /// E0006's fix of a name that is used: the name declared at `at`
    /// renamed to `name` through rename, with its checks (`generic`,
    /// D-1741).
    Rename {
        /// The start of the declaration's name, a byte offset in the text
        /// of the action's hash.
        at: u32,
        /// The name the help writes.
        name: String,
    },
}

/// What a provider and a resolver read besides the range (G1 plan §4.2).
/// Capabilities come as booleans, never as `ClientCaps`.
#[derive(Clone, Copy)]
pub(crate) struct FixCx<'a> {
    /// The snapshot that answers the request.
    pub analyzed: &'a Analyzed,
    /// The request's module, one for which `view.is_own()` holds: the
    /// frame offers nothing in any other.
    pub view: ModuleView<'a>,
    /// The module's published report, kept only while its hash is that of
    /// the snapshot's text (D-1393).
    pub report: Option<&'a FileReport>,
    /// The report's run read a last good text or a stub (D-1360).
    pub substituted: bool,
    /// The codes and ranges of the request's `context.diagnostics`, each
    /// code registered (`codes::lookup`), so Explain sees the child's codes
    /// and those of a lagging report (G1 plan §1.6, D-1685).
    pub context: &'a [(Code, TextRange)],
    /// The module's text in the snapshot.
    pub text: &'a str,
    /// The std base and std's sources.
    pub std: &'a StdService,
    /// The client resolves an action's `edit`, so a cross-file fix may be
    /// offered (D-1684).
    pub resolve_edits: bool,
    /// The client shows `disabled` actions (D-1690).
    pub disabled_support: bool,
    /// Set by `$/cancelRequest`.
    pub cancel: &'a CancelToken,
}

impl FixCx<'_> {
    /// The path of the request's module, as edits name it.
    pub(crate) fn path(&self) -> Option<&Path> {
        let file = self.view.file()?;
        match self.analyzed.snapshot.location(file)? {
            FileLocation::Package(path) => Some(path),
            FileLocation::Std(_) | FileLocation::Dependency(_) | FileLocation::Synthetic => None,
        }
    }
}

/// Whether `help`, a help of a diagnostic of `code` of the file at `path`,
/// the module `view` of `snapshot`, becomes a quick fix (`generic`): it
/// has edits, all in that file, so a help with an edit in another file
/// keeps its line (D-1752); in a std checkout none of them writes `pub`
/// (D-1706); none writes the prelude's `Nil`, or another name of
/// [`PRELUDE_WORDS`], where the module hides it (D-1678, D-1931); D-1661's escape of a brace leaves no grammar error (D-1742);
/// and E0006's help, whose fix on a used name is a rename checked on
/// resolve (D-1741), only for a client that `resolve_edits` (D-1754). For a
/// client that takes code-action literals its line then leaves the
/// published message (D-1686), so `server::publish` asks the same.
pub(crate) fn offers_fix(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    code: Code,
    help: &PlainHelp,
    path: &Path,
    resolve_edits: bool,
) -> bool {
    if code == codes::NAMING_CONVENTION && !resolve_edits {
        return false;
    }
    let writes = |word: &str| {
        help.edits
            .iter()
            .any(|edit| writes_word(&edit.replacement, word))
    };
    let in_file = !help.edits.is_empty() && help.edits.iter().all(|edit| edit.path == path);
    let writes_pub = snapshot.is_std_checkout() && writes("pub");
    let writes_hidden_nil = writes("Nil") && hides_nil(snapshot, view);
    let writes_hidden = PRELUDE_WORDS
        .iter()
        .any(|&(word, def)| writes(word) && hides_prelude(snapshot, view, def, None));
    in_file && !writes_pub && !writes_hidden_nil && !writes_hidden && !escape_breaks(view, help)
}

/// The prelude names that the habit helps write with their edits (D-1931),
/// as `Int` for `int` or `True` for `true`, with the items they name. A
/// help that writes one where the module hides it, with a type `Int` of
/// its own, say, is no fix (D-1678).
const PRELUDE_WORDS: &[(&str, DefId)] = &[
    ("Int", builtin::INT),
    ("Float", builtin::FLOAT),
    ("String", builtin::STRING),
    ("Bool", builtin::BOOL),
    ("True", builtin::TRUE),
    ("False", builtin::FALSE),
    ("List", builtin::LIST),
    ("Option", builtin::OPTION),
    ("Result", builtin::RESULT),
    ("Some", builtin::SOME),
    ("None", builtin::NONE),
    ("Ok", builtin::OK),
    ("Err", builtin::ERR),
    ("I8", builtin::I8),
    ("I16", builtin::I16),
    ("I32", builtin::I32),
    ("I64", builtin::I64),
    ("U8", builtin::U8),
    ("U16", builtin::U16),
    ("U32", builtin::U32),
    ("U64", builtin::U64),
    ("F32", builtin::F32),
];

/// Whether `help` is D-1661's escape of an interpolation's `{`, one `\`
/// written before a `{` (D-1694), and the text with it applied keeps a
/// grammar error (D-1742). Only the statement that holds the brace is
/// parsed, or the item when it lies outside a block: once alone and once
/// with the escape, inside a function's block for a statement. The escape
/// breaks the text when that leaves as many grammar errors or more, or an
/// error the text without it does not have at the same place. So
/// `"price {" + amount + "}"` keeps its fix, and so does each brace of
/// `"a {" + b + "} c {" + d + "}"`, whose other brace keeps its own error,
/// while `"{"x"}"`, which would read `"\{" x "}"`, gets none. Parsing one
/// statement keeps the cost of a publish, which asks this of every help
/// (`server::publish`), from growing with the file, and a statement longer
/// than [`ESCAPE_UNIT_LIMIT`] counts as broken, so its braces get no fix and
/// keep their help lines: the cost of a publish then grows with the file's
/// length, never with its square (D-1742).
fn escape_breaks(view: ModuleView<'_>, help: &PlainHelp) -> bool {
    let [edit] = help.edits.as_slice() else {
        return false;
    };
    if !edit.range.is_empty() || edit.replacement != "\\" {
        return false;
    }
    let (Some(file), Some(tree)) = (view.file(), view.tree()) else {
        return true;
    };
    let brace = TextRange::new(edit.range.start(), edit.range.start().saturating_add(1));
    if brace.slice(tree.text()) != Some("{") {
        return false;
    }
    let Some(token) = tree.root().covering_element(brace) else {
        return true;
    };
    let Some(unit) = token
        .parent()
        .into_iter()
        .flat_map(SyntaxNode::ancestors)
        .find(|node| {
            node.parent().is_none_or(|parent| {
                matches!(parent.kind(), SyntaxKind::BLOCK | SyntaxKind::SOURCE_FILE)
            })
        })
    else {
        return true;
    };
    let (prefix, suffix) = if unit
        .parent()
        .is_some_and(|parent| parent.kind() == SyntaxKind::BLOCK)
    {
        ("fn f() {\n", "\n}\n")
    } else {
        ("", "")
    };
    let (Some(unit_text), Ok(start), Ok(at)) = (
        unit.text_range().slice(tree.text()),
        usize::try_from(unit.text_range().start()),
        usize::try_from(edit.range.start()),
    ) else {
        return true;
    };
    if unit_text.len() > ESCAPE_UNIT_LIMIT {
        return true;
    }
    let Some(rel) = at.checked_sub(start) else {
        return true;
    };
    let (Some(head), Some(tail)) = (unit_text.get(..rel), unit_text.get(rel..)) else {
        return true;
    };
    let plain = cheby_syntax::parse(file, Arc::from(format!("{prefix}{unit_text}{suffix}")));
    let escaped = cheby_syntax::parse(file, Arc::from(format!("{prefix}{head}\\{tail}{suffix}")));
    if escaped.errors().len() >= plain.errors().len() {
        return true;
    }
    let at = prefix.len().saturating_add(rel);
    // Where an error starts in the text without the escape, which adds one
    // byte before the brace.
    let place = |error: &Diagnostic, shifted: bool| {
        let start = error
            .primary_span()
            .and_then(|span| usize::try_from(span.range.start()).ok())?;
        Some(if shifted && start > at {
            start.saturating_sub(1)
        } else {
            start
        })
    };
    let mut before: Vec<_> = plain
        .errors()
        .iter()
        .map(|error| (error.code, place(error, false)))
        .collect();
    escaped.errors().iter().any(|error| {
        let key = (error.code, place(error, true));
        match before.iter().position(|seen| *seen == key) {
            Some(found) => {
                before.swap_remove(found);
                false
            }
            None => true,
        }
    })
}

/// The longest statement, in bytes, in which D-1661's escape is checked
/// (D-1742). Each help parses its statement twice, and a statement of this
/// length holds at most a few dozen such braces, so the bound keeps a
/// publish's work at a few parses of this size per brace.
const ESCAPE_UNIT_LIMIT: usize = 1024;

/// Whether `code` holds `word` as a whole name.
pub(crate) fn writes_word(code: &str, word: &str) -> bool {
    code.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|part| part == word)
}

/// Whether the module `view` hides the prelude's `Nil`, its constructor or
/// its type, with a declaration of its own (D-1678).
pub(crate) fn hides_nil(snapshot: &Snapshot, view: ModuleView<'_>) -> bool {
    hides_prelude(snapshot, view, builtin::NIL, None)
        || hides_prelude(snapshot, view, builtin::NIL_TY, None)
}

/// `edits`, all of the file at `path` whose text is `text`, in `cheby
/// fmt`'s form on the lines they touch (D-1735): applied in memory, the
/// formatter's hunks that touch them kept, and the result written as one
/// replacement of what changed. The edits stay as they are when they
/// already leave those lines formatted, when the file has a grammar error,
/// which the formatter refuses, when they overlap, for a fix that
/// `renames` as rename writes, E0044's and E0006's (D-1668, D-1741), when
/// the formatter's hunks are not closed after `edits::MAX_ROUNDS` rounds
/// (D-1738), and when `cancel` is set, which the caller then answers.
pub(crate) fn formatted(
    text: &str,
    path: &Path,
    edits: Vec<PlainEdit>,
    renames: bool,
    cancel: &CancelToken,
) -> Vec<PlainEdit> {
    if renames || edits.iter().any(|edit| edit.path != path) {
        return edits;
    }
    let mut sorted: Vec<&PlainEdit> = edits.iter().collect();
    sorted.sort_by_key(|edit| (edit.range.start(), edit.range.end()));
    let mut new = String::with_capacity(text.len());
    let mut touched: Vec<TextRange> = Vec::with_capacity(sorted.len());
    let mut at = 0_usize;
    for edit in &sorted {
        let (Ok(start), Ok(end)) = (
            usize::try_from(edit.range.start()),
            usize::try_from(edit.range.end()),
        ) else {
            return edits;
        };
        let Some(kept) = text.get(at..start) else {
            return edits;
        };
        new.push_str(kept);
        let from = new.len();
        new.push_str(&edit.replacement);
        let (Ok(from), Ok(to)) = (TextSize::try_from(from), TextSize::try_from(new.len())) else {
            return edits;
        };
        touched.push(TextRange::new(from, to));
        at = end;
    }
    let Some(rest) = text.get(at..) else {
        return edits;
    };
    new.push_str(rest);
    let Ok(Some(changes)) = format_touching_with(&new, &touched, AtCap::GiveUp, cancel) else {
        return edits;
    };
    if changes.is_empty() {
        return edits;
    }
    let mut out = String::with_capacity(new.len());
    let mut at = 0_usize;
    for change in &changes {
        let (Ok(start), Ok(end)) = (
            usize::try_from(change.range.start()),
            usize::try_from(change.range.end()),
        ) else {
            return edits;
        };
        let Some(kept) = new.get(at..start) else {
            return edits;
        };
        out.push_str(kept);
        out.push_str(&change.text);
        at = end;
    }
    let Some(rest) = new.get(at..) else {
        return edits;
    };
    out.push_str(rest);
    one_replacement(text, &out, path).map_or(edits, |edit| vec![edit])
}

/// The one edit that turns `old` into `new`: the bytes between their
/// common start and common end, cut on character boundaries. `None` when
/// they are equal.
pub(crate) fn one_replacement(old: &str, new: &str, path: &Path) -> Option<PlainEdit> {
    if old == new {
        return None;
    }
    let mut prefix = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(prefix) || !new.is_char_boundary(prefix) {
        prefix = prefix.checked_sub(1)?;
    }
    let room = old.len().min(new.len()).checked_sub(prefix)?;
    let mut suffix = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(old.len().checked_sub(suffix)?)
        || !new.is_char_boundary(new.len().checked_sub(suffix)?)
    {
        suffix = suffix.checked_sub(1)?;
    }
    let start = TextSize::try_from(prefix).ok()?;
    let end = TextSize::try_from(old.len().checked_sub(suffix)?).ok()?;
    let replacement = new.get(prefix..new.len().checked_sub(suffix)?)?;
    Some(PlainEdit {
        path: path.to_path_buf(),
        range: TextRange::new(start, end),
        replacement: replacement.to_owned(),
    })
}

/// A provider: the actions it offers for `range`, given the report's
/// diagnostics that overlap it, each with its index in the report.
pub(crate) type Provide = fn(&FixCx<'_>, TextRange, &[(usize, &PlainDiagnostic)]) -> Vec<Action>;

/// A resolver: the edits of the action `id` at `range`, or why there are
/// none. The frame runs `verify` on a cross-file result (D-1684, D-1731).
pub(crate) type Resolve = fn(&FixCx<'_>, TextRange, &ActionId) -> Result<Vec<PlainEdit>, Refusal>;

/// A [`Code`] in `data` as it is written, `E0044`.
mod code_text {
    use cheby_driver::analysis::codes::{self, Code};
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    /// Writes `code` as `E0044`.
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's `with` passes the field by reference"
    )]
    pub(super) fn serialize<S: Serializer>(code: &Code, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(code)
    }

    /// Reads a registered code written as `E0044`.
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Code, D::Error> {
        let text = String::deserialize(deserializer)?;
        Code::parse(&text)
            .filter(|code| codes::lookup(*code).is_some())
            .ok_or_else(|| D::Error::custom(format!("`{text}` is not a diagnostic code")))
    }
}

#[cfg(test)]
mod tests {
    use cheby_driver::analysis::codes;
    use serde_json::json;

    use super::ActionId;

    /// An action's id goes to the wire and back unchanged, its code written
    /// as `cheby check` writes it, and an unknown code is refused.
    #[test]
    fn action_ids_round_trip_through_data() {
        let fix = ActionId::Fix {
            code: codes::SHADOWED_NAME,
            index: 1,
        };
        let wire = serde_json::to_value(&fix).unwrap();
        assert_eq!(wire, json!({ "kind": "fix", "code": "E0044", "index": 1 }));
        assert_eq!(serde_json::from_value::<ActionId>(wire).unwrap(), fix);
        let stub = ActionId::Stub {
            interface: "app::Shape".to_owned(),
            function: None,
        };
        let wire = serde_json::to_value(&stub).unwrap();
        assert_eq!(serde_json::from_value::<ActionId>(wire).unwrap(), stub);
        let unknown = json!({ "kind": "fix", "code": "E9999", "index": 0 });
        assert!(serde_json::from_value::<ActionId>(unknown).is_err());
    }
}
