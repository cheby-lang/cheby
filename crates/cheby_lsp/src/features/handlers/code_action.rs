//! `textDocument/codeAction` and `codeAction/resolve`: the frame of code
//! actions (X §6.1, D-1682 to D-1691, D-1731 to D-1735).
//!
//! `textDocument/codeAction` is a `Doc` request, answered from a snapshot,
//! so every action reads the facts, the reports and the tree of one version
//! (D-1682). A client without code-action literals gets `[]` (D-1734), and
//! so does a document that is not one of the session's own modules. Else:
//!
//! 1. The published report of the document is read, and kept only while
//!    its hash is that of the snapshot's text (D-1393), so no edit comes
//!    from old text; until the report of the text comes, the fixes that
//!    ride on it are missing (D-1685).
//! 2. Its diagnostics that overlap the range go to every provider of
//!    `ide::actions::registry`, with the request's own
//!    `context.diagnostics` as codes and ranges, each code registered
//!    (`FixCx::context`, G1 plan §1.6).
//! 3. An action that removes code is dropped while the report is
//!    substituted, or while a file of the package is broken, cut by an
//!    import cycle or lost by the run, so every report is read against
//!    what that run did not see (D-1360, D-1739), a `disabled` one
//!    for a client that cannot show it (D-1690), and the rest are filtered
//!    by `only`; organize imports with nothing to do is kept only when
//!    `only` names `source` or `source.organizeImports` (D-1691). A
//!    `disabled` action goes out with no `edit` and nothing to resolve.
//! 4. A client that cannot resolve an action's `edit` gets every edit in
//!    the listing, and no fix whose edit touches another file (D-1684) or
//!    that a resolve checks through `verify`, E0006's rename and the `for`
//!    rewrite (D-1753, D-1754).
//! 5. Fixes come first, by their diagnostic's start, then rewrites by
//!    source order, then source actions, then Explain (X §8.3). A fix is
//!    `isPreferred` when its row says so and its diagnostic has no other
//!    fix with an edit (X §6.1).
//! 6. Each edit within the file is formatted on the lines it touches, as
//!    `cheby fmt` would write them, and kept raw when the file has a
//!    grammar error; E0044's and E0006's, which rename, are never formatted
//!    (D-1735, D-1741, `actions::formatted`). An edit that starts on a line ending in CRLF
//!    writes CRLF (D-1444).
//!    Edits go through the shared conversion (`handlers::edit`), whose
//!    effects go with the answer.
//! 7. Every action carries `data` with the document's URI, the text's
//!    hash and the range, and the id of what a resolve computes, if any
//!    (D-1683, D-1733).
//!
//! `codeAction/resolve` is a `Package` request, routed by the URI in the
//! action's `data`, so a fix that edits another open document reads its
//! newest text (D-1683). It answers `ContentModified` when the snapshot's
//! hash of that document differs from `data`'s, and the action as it was
//! sent when `data` names nothing to compute (D-1733). Else the resolver
//! computes the edits. Edits within the file are formatted as in the
//! listing. Edits that touch another file run `verify` through
//! `rename::check` (D-1684), and a refusal answers as a refused rename
//! does (D-1731): `RequestFailed` with its text, except a file that
//! changed on disk, which answers the action with no `edit`, a warning and
//! a sweep of the package (D-1711). E0006's fix of a used name is a
//! rename, run through rename with its checks, whose refusals answer the
//! same way (D-1741). The `for` rewrite's edits, within the file, run
//! `verify` too, where only an error in the text it wrote refuses it
//! (D-1753). Where the route finds no snapshot it answers
//! `RequestFailed` (D-1732, `no_analysis`).

use std::path::Path;

use cheby_driver::analysis::codes::{self, Code};
use cheby_driver::analysis::{
    Hash128, PlainDiagnostic, PlainEdit, Snapshot, TextRange, TextSize, VerifyRequest, exact_key,
};
use lsp_types::{
    CodeAction, CodeActionDisabled, CodeActionKind, CodeActionParams, Command, Diagnostic,
    DiagnosticSeverity, MessageType, NumberOrString, TextDocumentIdentifier,
    TextDocumentPositionParams, Uri,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::features::handlers::doc::RequestDoc;
use crate::features::handlers::edit;
use crate::features::handlers::explain::EXPLAIN_COMMAND;
use crate::features::ide::actions::{
    Action, ActionCommand, ActionId, ActionKind, DiagRef, FixCx, Provide, Resolve, formatted,
    registry,
};
use crate::features::ide::rename::{self, RenameError};
use crate::features::ide::{habits, package_name};
use crate::features::{Analyzed, HandlerOutput, PublishedReport, RequestCx, RequestError, params};
use crate::server::Effect;

/// What every listed action carries as `data` (D-1683, D-1733).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ActionData {
    /// The document the action was listed for, as the client spelled it.
    uri: String,
    /// [`exact_key`] of the text it was listed for, in hex (D-1393).
    hash: String,
    /// The request's range, as byte offsets in that text.
    range: [u32; 2],
    /// What a resolve computes; `None` for an action with nothing to
    /// compute, such as Explain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<ActionId>,
}

/// `textDocument/codeAction`.
///
/// # Errors
///
/// `InvalidParams` when the parameters do not have the method's shape,
/// `Cancelled`, and `Failed` for an edit that cannot be sent.
pub(crate) fn code_action(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
) -> Result<HandlerOutput, RequestError> {
    let p: CodeActionParams = params(params_value)?;
    list(cx, analyzed, &p, registry::PROVIDERS, registry::resolver)
}

/// `codeAction/resolve`.
///
/// # Errors
///
/// `InvalidParams` when the parameters are not a listed `CodeAction`,
/// `ContentModified` when the text moved since the listing, `Failed` for
/// a refusal (D-1731), and `Cancelled`.
pub(crate) fn resolve(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
) -> Result<HandlerOutput, RequestError> {
    let action: CodeAction = params(params_value)?;
    resolve_with(cx, analyzed, action, resolvers())
}

/// The answer of `codeAction/resolve` where its route finds no snapshot,
/// which may not be `null` (D-1732).
///
/// # Errors
///
/// Always `RequestFailed`.
pub(crate) fn no_analysis() -> Result<Value, RequestError> {
    Err(RequestError::Failed(
        "`codeAction/resolve`: the document has no analysis".to_owned(),
    ))
}

/// The resolver of an action's id.
pub(crate) type Resolvers = fn(&ActionId) -> Resolve;

/// Resolvers a test sets for a whole server, whose requests reach no other
/// test code (`server::tests`). Nextest runs each test in a process of its
/// own, so no other test sees them.
#[cfg(test)]
pub(crate) static TEST_RESOLVERS: std::sync::OnceLock<Resolvers> = std::sync::OnceLock::new();

/// The resolvers of `codeAction/resolve`: the registry's, or a test's.
fn resolvers() -> Resolvers {
    #[cfg(test)]
    if let Some(resolvers) = TEST_RESOLVERS.get() {
        return *resolvers;
    }
    registry::resolver
}

/// The request's document, as the frame reads it.
struct Frame<'s> {
    /// The document.
    doc: RequestDoc<'s>,
    /// [`exact_key`] of its text.
    hash: Hash128,
    /// Its published report while the report's hash is `hash` (D-1393).
    report: Option<&'s PublishedReport>,
}

impl<'s> Frame<'s> {
    /// The document `uri` names in `analyzed`, when it is one of the
    /// session's own modules (G1 plan §4.2), with its current report.
    fn of(cx: &'s RequestCx<'_>, analyzed: &'s Analyzed, uri: &Uri) -> Option<Self> {
        let pos = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: lsp_types::Position::default(),
        };
        let doc = RequestDoc::of(&analyzed.snapshot, &pos, None)?;
        if !doc.view.is_own() {
            return None;
        }
        let hash = exact_key(doc.text);
        let report = cx
            .reports
            .as_deref()
            .and_then(|reports| reports.files.get(&doc.path))
            .filter(|published| published.report.hash == hash);
        Some(Self { doc, hash, report })
    }

    /// The report's diagnostics, empty while it lags (D-1685).
    fn diagnostics(&self) -> &'s [PlainDiagnostic] {
        self.report
            .map_or(&[], |published| published.report.diagnostics.as_slice())
    }

    /// What providers and resolvers read.
    fn fix_cx<'a>(
        &'a self,
        cx: &'a RequestCx<'_>,
        analyzed: &'a Analyzed,
        context: &'a [(Code, TextRange)],
    ) -> FixCx<'a> {
        let actions = &cx.caps.actions;
        FixCx {
            analyzed,
            view: self.doc.view,
            report: self.report.map(|published| &*published.report),
            // The report's run read a substitute, or the snapshot's run is
            // not the check: a file of the package is broken, cut by an
            // import cycle or lost now, and every other file's report,
            // maybe of a clean run that planned no new one, is read against
            // what that run did not see (D-1360, D-1739).
            substituted: self.report.is_some_and(|published| published.substituted)
                || not_the_check(&analyzed.snapshot),
            context,
            text: self.doc.text,
            std: cx.std,
            resolve_edits: actions.resolves_edits(),
            disabled_support: actions.disabled,
            cancel: cx.cancel,
        }
    }
}

/// Whether a file of `snapshot` is broken, cut by an import cycle, or
/// lost by its run, so a report read with it counts as substituted
/// (D-1739).
fn not_the_check(snapshot: &Snapshot) -> bool {
    let status = snapshot.status();
    !status.broken.is_empty() || !status.cut_imports.is_empty() || !snapshot.lost().is_empty()
}

/// The listing of `p` from `providers`, with `resolvers` for a client that
/// cannot resolve an edit (module doc).
fn list(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    p: &CodeActionParams,
    providers: &[(&str, Provide)],
    resolvers: Resolvers,
) -> Result<HandlerOutput, RequestError> {
    let empty = || Ok(HandlerOutput::value(json!([])));
    if !cx.caps.actions.literals {
        return empty();
    }
    let Some(frame) = Frame::of(cx, analyzed, &p.text_document.uri) else {
        return empty();
    };
    let doc = &frame.doc;
    let range = doc.lines.text_range(doc.text, p.range, cx.encoding);
    let all = frame.diagnostics();
    let overlapping: Vec<(usize, &PlainDiagnostic)> = all
        .iter()
        .enumerate()
        .filter(|(_, d)| {
            d.range
                .is_some_and(|at| overlaps(doc.lines.widen_empty(doc.text, at), range))
        })
        .collect();
    let context: Vec<(Code, TextRange)> = p
        .context
        .diagnostics
        .iter()
        .filter_map(|d| {
            let code = code_of(d)?;
            Some((code, doc.lines.text_range(doc.text, d.range, cx.encoding)))
        })
        .collect();
    let fcx = frame.fix_cx(cx, analyzed, &context);
    let mut actions: Vec<Action> = Vec::new();
    for (_, provide) in providers {
        cx.cancel.check()?;
        actions.extend(provide(&fcx, range, &overlapping));
    }
    let only = p.context.only.as_deref();
    actions.retain(|a| {
        !(a.removes && fcx.substituted)
            && (a.disabled.is_none() || fcx.disabled_support)
            && wanted(a, only)
    });
    if !fcx.resolve_edits {
        actions = at_listing(&fcx, range, actions, resolvers, &doc.path)?;
        // An edit computed now may be one with nothing to do (D-1691).
        actions.retain(|a| wanted(a, only));
    }
    // A cross-file edit is checked on resolve only (D-1684).
    actions.retain(|a| a.edit.as_deref().is_none_or(|e| within(e, &doc.path)));
    let start = |r: DiagRef| match r {
        DiagRef::Report(i) => all.get(i).and_then(|d| d.range).map(TextRange::start),
        DiagRef::Context(i) => context.get(i).map(|(_, at)| at.start()),
    };
    actions.sort_by_key(|a| order(a, start));
    let preferred: Vec<bool> = actions
        .iter()
        .map(|a| {
            a.preferred
                && a.diagnostic.is_some()
                && has_edit(a)
                && actions
                    .iter()
                    .filter(|b| b.diagnostic == a.diagnostic && has_edit(b))
                    .count()
                    == 1
        })
        .collect();
    let mut out: Vec<CodeAction> = Vec::with_capacity(actions.len());
    let mut effects: Vec<Effect> = Vec::new();
    for (action, preferred) in actions.into_iter().zip(preferred) {
        cx.cancel.check()?;
        let diagnostic = action
            .diagnostic
            .and_then(|r| protocol_diagnostic(&frame, cx, p, all, &context, r));
        let (wire, more) = wire(cx, analyzed, &frame, range, action, diagnostic, preferred)?;
        out.push(wire);
        effects.extend(more);
    }
    Ok(HandlerOutput {
        value: to_value(&out)?,
        effects,
    })
}

/// Whether `a` and `b` share a point, ends included, so a cursor right
/// after a diagnostic still finds it.
fn overlaps(a: TextRange, b: TextRange) -> bool {
    a.start() <= b.end() && b.start() <= a.end()
}

/// The registered code of a diagnostic the client sent, if any (P-3).
fn code_of(d: &Diagnostic) -> Option<Code> {
    let Some(NumberOrString::String(text)) = &d.code else {
        return None;
    };
    Code::parse(text).filter(|code| codes::lookup(*code).is_some())
}

/// Whether `edits` all lie in the file at `path`.
fn within(edits: &[PlainEdit], path: &Path) -> bool {
    edits.iter().all(|edit| edit.path == path)
}

/// Whether `a` passes the request's `only` (X §6.1, D-1691).
fn wanted(a: &Action, only: Option<&[CodeActionKind]>) -> bool {
    let kind = kind_of(a.kind);
    let nothing = a.edit.as_ref().is_some_and(Vec::is_empty);
    match only {
        // Organize imports with nothing to do would show on every line.
        None => !(a.kind == ActionKind::SourceOrganizeImports && nothing),
        Some(only) => {
            let named = only.iter().any(|k| {
                let k = k.as_str();
                k.is_empty() || kind.as_str() == k || kind.as_str().starts_with(&format!("{k}."))
            });
            let asked = || {
                only.iter()
                    .any(|k| matches!(k.as_str(), "source" | "source.organizeImports"))
            };
            named && (a.kind != ActionKind::SourceOrganizeImports || !nothing || asked())
        }
    }
}

/// The protocol kind of `kind`.
fn kind_of(kind: ActionKind) -> CodeActionKind {
    match kind {
        ActionKind::QuickFix => CodeActionKind::QUICKFIX,
        ActionKind::RefactorRewrite => CodeActionKind::REFACTOR_REWRITE,
        ActionKind::SourceOrganizeImports => CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
        ActionKind::Source => CodeActionKind::SOURCE,
    }
}

/// Whether `a` is a fix with an edit, at listing or on resolve. A
/// `disabled` one goes out with no edit (D-1690).
fn has_edit(a: &Action) -> bool {
    a.kind == ActionKind::QuickFix
        && a.command.is_none()
        && a.disabled.is_none()
        && (a.data.is_some() || a.edit.as_ref().is_some_and(|e| !e.is_empty()))
}

/// Where `a` goes in the listing (X §6.1): fixes by their diagnostic's
/// start, then rewrites by the start of their edit, then source actions,
/// then Explain, last in every list (X §8.3), each group in the providers'
/// order otherwise.
fn order(a: &Action, start: impl Fn(DiagRef) -> Option<TextSize>) -> (u8, TextSize) {
    let last = TextSize::MAX;
    match (a.kind, a.command) {
        (ActionKind::QuickFix, None) => (0, a.diagnostic.and_then(&start).unwrap_or(last)),
        (ActionKind::QuickFix, Some(_)) => (4, a.diagnostic.and_then(&start).unwrap_or(last)),
        (ActionKind::RefactorRewrite, _) => (
            1,
            a.edit
                .as_ref()
                .and_then(|e| e.iter().map(|edit| edit.range.start()).min())
                .unwrap_or(last),
        ),
        (ActionKind::SourceOrganizeImports | ActionKind::Source, _) => (2, 0),
    }
}

/// `actions` for a client that cannot resolve an `edit` (D-1684): each
/// action whose edits a resolve would compute gets them now, and one whose
/// edits touch another file, or whose resolver refuses, is dropped. A fix
/// that a resolve checks through `verify`, E0006's rename and the `for`
/// rewrite, is not offered to such a client by its provider (D-1753,
/// D-1754), so no `verify` runs here.
fn at_listing(
    fcx: &FixCx<'_>,
    range: TextRange,
    actions: Vec<Action>,
    resolvers: Resolvers,
    path: &Path,
) -> Result<Vec<Action>, RequestError> {
    let mut kept = Vec::with_capacity(actions.len());
    for mut action in actions {
        fcx.cancel.check()?;
        if action.edit.is_none()
            && let Some(id) = action.data.take()
        {
            let resolved = resolvers(&id)(fcx, range, &id);
            fcx.cancel.check()?;
            match resolved {
                Ok(edits) if within(&edits, path) => action.edit = Some(edits),
                Ok(_) | Err(_) => continue,
            }
        }
        kept.push(action);
    }
    Ok(kept)
}

/// The diagnostic an action names, as the client knows it: the one of the
/// request's `context` with its code and range, else one made from the
/// report's.
fn protocol_diagnostic(
    frame: &Frame<'_>,
    cx: &RequestCx<'_>,
    p: &CodeActionParams,
    all: &[PlainDiagnostic],
    context: &[(Code, TextRange)],
    r: DiagRef,
) -> Option<Diagnostic> {
    let doc = &frame.doc;
    let (code, at, d) = match r {
        DiagRef::Report(i) => {
            let d = all.get(i)?;
            (d.code, d.range?, Some(d))
        }
        DiagRef::Context(i) => {
            let &(code, at) = context.get(i)?;
            (code, at, None)
        }
    };
    let sent = p.context.diagnostics.iter().find(|sent| {
        code_of(sent) == Some(code) && doc.lines.text_range(doc.text, sent.range, cx.encoding) == at
    });
    if let Some(sent) = sent {
        return Some(sent.clone());
    }
    let d = d?;
    Some(Diagnostic {
        range: doc
            .lines
            .range(doc.text, doc.lines.widen_empty(doc.text, at), cx.encoding),
        severity: Some(match d.severity {
            cheby_driver::analysis::Severity::Error => DiagnosticSeverity::ERROR,
            cheby_driver::analysis::Severity::Warning => DiagnosticSeverity::WARNING,
        }),
        code: Some(NumberOrString::String(code.to_string())),
        source: Some("cheby".to_owned()),
        message: d.message.clone(),
        ..Diagnostic::default()
    })
}

/// `action` in protocol form, with the effects of its edit.
fn wire(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    frame: &Frame<'_>,
    range: TextRange,
    action: Action,
    diagnostic: Option<Diagnostic>,
    preferred: bool,
) -> Result<(CodeAction, Vec<Effect>), RequestError> {
    let fixed = action.diagnostic.and_then(|r| match r {
        DiagRef::Report(i) => frame.diagnostics().get(i).map(|d| d.code),
        DiagRef::Context(_) => None,
    });
    // A disabled action carries no edit and nothing to resolve, so a client
    // that ignores `disabled` cannot apply it (D-1690).
    let disabled = action.disabled.is_some();
    // A fix that renames writes as rename does, unformatted (D-1735,
    // D-1741).
    let renames =
        fixed == Some(codes::SHADOWED_NAME) || matches!(action.data, Some(ActionId::Rename { .. }));
    let (edit, effects) = match action.edit.filter(|e| !e.is_empty() && !disabled) {
        Some(edits) => {
            let package = package_name(&analyzed.snapshot);
            let edits = formatted(
                frame.doc.text,
                &frame.doc.path,
                package.as_deref(),
                edits,
                renames,
                cx.cancel,
            );
            // A cancel during the widening leaves the raw edits, which
            // the cancelled request must not answer with (D-1738).
            cx.cancel.check()?;
            let edits = line_breaks(frame.doc.text, &frame.doc.path, edits);
            let out = edit::to_workspace_edit(cx, analyzed, &edits, None)?;
            (Some(out.edit), out.effects)
        }
        None => (None, Vec::new()),
    };
    // An action with its edit in hand has nothing left to compute.
    let id = if edit.is_some() || disabled {
        None
    } else {
        action.data
    };
    let data = ActionData {
        uri: frame.doc.uri.as_str().to_owned(),
        hash: frame.hash.to_hex(),
        range: [range.start(), range.end()],
        id,
    };
    let command = action.command.map(|command| match command {
        ActionCommand::Explain(code) => Command {
            title: action.title.clone(),
            command: EXPLAIN_COMMAND.to_owned(),
            arguments: Some(vec![json!(code.to_string())]),
        },
    });
    let wire = CodeAction {
        title: action.title,
        kind: Some(kind_of(action.kind)),
        diagnostics: diagnostic.map(|d| vec![d]),
        edit,
        command,
        is_preferred: (preferred && cx.caps.actions.is_preferred).then_some(true),
        disabled: action.disabled.map(|reason| CodeActionDisabled { reason }),
        data: Some(to_value(&data)?),
    };
    Ok((wire, effects))
}

/// [`resolve`] with `resolvers` (module doc).
fn resolve_with(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    mut action: CodeAction,
    resolvers: Resolvers,
) -> Result<HandlerOutput, RequestError> {
    let malformed = || {
        RequestError::InvalidParams(
            "`codeAction/resolve`: `data` is not the data of a listed action".to_owned(),
        )
    };
    let data: ActionData = action
        .data
        .clone()
        .and_then(|data| serde_json::from_value(data).ok())
        .ok_or_else(malformed)?;
    let uri: Uri = data.uri.parse().map_err(|_| malformed())?;
    let hash = Hash128::from_hex(&data.hash).ok_or_else(malformed)?;
    let [start, end] = data.range;
    if start > end {
        return Err(malformed());
    }
    let range = TextRange::new(start, end);
    // The path holds no module of the snapshot any more, as after a move.
    let Some(frame) = Frame::of(cx, analyzed, &uri) else {
        return no_analysis().map(HandlerOutput::value);
    };
    if frame.hash != hash {
        return Err(RequestError::ContentModified);
    }
    let Some(id) = data.id else {
        return Ok(HandlerOutput::value(to_value(&action)?));
    };
    if frame
        .doc
        .text
        .get(usize::try_from(end).map_err(|_| malformed())?..)
        .is_none()
    {
        return Err(malformed());
    }
    if let ActionId::Rename { at, name } = &id {
        return renamed(cx, analyzed, &frame, action, *at, name);
    }
    let fcx = frame.fix_cx(cx, analyzed, &[]);
    let edits =
        resolvers(&id)(&fcx, range, &id).map_err(|refusal| RequestError::Failed(refusal.0))?;
    cx.cancel.check()?;
    let renames = match &id {
        ActionId::Fix { code, .. } => *code == codes::SHADOWED_NAME,
        ActionId::AddImport { .. }
        | ActionId::RemoveImport
        | ActionId::OrganizeImports
        | ActionId::FillArms
        | ActionId::Stub { .. }
        | ActionId::MakeVisible { .. }
        | ActionId::Habit { .. }
        | ActionId::Rename { .. } => false,
    };
    let edits = if within(&edits, &frame.doc.path) {
        // The `for` rewrite, the one habit resolved through `data`, must
        // check where it wrote, through `verify` (D-1753).
        if matches!(id, ActionId::Habit { .. }) {
            match habits::check_rewrite(analyzed, &edits) {
                Ok(()) => {}
                Err(RenameError::Refused(refusal)) => return Err(RequestError::Failed(refusal.0)),
                Err(RenameError::Cancelled) => return Err(RequestError::Cancelled),
                Err(RenameError::Stale { refusal, .. }) => {
                    return stale(analyzed, action, refusal.0);
                }
            }
            cx.cancel.check()?;
        }
        let package = package_name(&analyzed.snapshot);
        let edits = formatted(
            frame.doc.text,
            &frame.doc.path,
            package.as_deref(),
            edits,
            renames,
            cx.cancel,
        );
        cx.cancel.check()?;
        line_breaks(frame.doc.text, &frame.doc.path, edits)
    } else {
        let request = VerifyRequest {
            edits: &edits,
            moves: &[],
            renamed: &[],
            hides: false,
            written: &[],
        };
        match rename::check(analyzed, &request, rename::Action::Fix) {
            Ok(()) => edits,
            Err(RenameError::Refused(refusal)) => return Err(RequestError::Failed(refusal.0)),
            Err(RenameError::Cancelled) => return Err(RequestError::Cancelled),
            Err(RenameError::Stale { refusal, .. }) => return stale(analyzed, action, refusal.0),
        }
    };
    if edits.is_empty() {
        return Ok(HandlerOutput::value(to_value(&action)?));
    }
    let out = edit::to_workspace_edit(cx, analyzed, &edits, None)?;
    action.edit = Some(out.edit);
    Ok(HandlerOutput {
        value: to_value(&action)?,
        effects: out.effects,
    })
}

/// A stale file's answer to a resolve: an error answer carries no effects,
/// so it is the action with no edit, a warning and a sweep of the package
/// (D-1711, D-1731).
fn stale(
    analyzed: &Analyzed,
    mut action: CodeAction,
    warning: String,
) -> Result<HandlerOutput, RequestError> {
    action.edit = None;
    Ok(HandlerOutput {
        value: to_value(&action)?,
        effects: vec![
            Effect::Sweep {
                root: analyzed.snapshot.root().to_path_buf(),
            },
            Effect::show(MessageType::WARNING, warning),
        ],
    })
}

/// The resolve of E0006's fix of a used name (D-1741): the name declared
/// at `at` renamed to `name` through rename, with its checks, where only
/// the declaration's E0006 may go away. Its edits are rename's, written
/// as rename writes them and unformatted, with its note, and a refusal
/// answers as a refused rename (D-1709, D-1731).
fn renamed(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    frame: &Frame<'_>,
    mut action: CodeAction,
    at: TextSize,
    name: &str,
) -> Result<HandlerOutput, RequestError> {
    let gone = || RequestError::Failed("the name can no longer be renamed here".to_owned());
    let file = frame.doc.view.file().ok_or_else(gone)?;
    let plan = match rename::rename_fixing_case(analyzed, file, at, name, cx.cancel) {
        Ok(Some(plan)) => plan,
        Ok(None) => return Err(gone()),
        Err(RenameError::Refused(refusal)) => return Err(RequestError::Failed(refusal.0)),
        Err(RenameError::Cancelled) => return Err(RequestError::Cancelled),
        Err(RenameError::Stale { refusal, .. }) => return stale(analyzed, action, refusal.0),
    };
    if plan.edits.is_empty() {
        return Ok(HandlerOutput::value(to_value(&action)?));
    }
    let out = edit::to_workspace_edit(cx, analyzed, &plan.edits, plan.note.as_deref())?;
    action.edit = Some(out.edit);
    Ok(HandlerOutput {
        value: to_value(&action)?,
        effects: out.effects,
    })
}

/// `edits` with the line breaks of the file at `path`, whose text is
/// `text`: an edit of that file that starts on a line ending in CRLF writes
/// CRLF, so a fix keeps a CRLF file's line ends (D-1444). Formatting writes
/// a kept hunk with CRLF already; the raw edits of a rewrite, and of a file
/// that `actions::formatted` leaves as it is, are written with LF.
fn line_breaks(text: &str, path: &Path, mut edits: Vec<PlainEdit>) -> Vec<PlainEdit> {
    if !text.contains("\r\n") {
        return edits;
    }
    for edit in edits.iter_mut().filter(|edit| edit.path == path) {
        if edit.replacement.contains('\n') && crlf_at(text, edit.range.start()) {
            edit.replacement = edit.replacement.replace("\r\n", "\n").replace('\n', "\r\n");
        }
    }
    edits
}

/// Whether the line of `text` that holds `offset` ends in CRLF; for the
/// last line, which has no line break, the line before it.
fn crlf_at(text: &str, offset: TextSize) -> bool {
    let at = usize::try_from(offset).unwrap_or(0).min(text.len());
    let after = text
        .get(at..)
        .and_then(|rest| rest.find('\n'))
        .and_then(|n| n.checked_add(at));
    let before = || text.get(..at).and_then(|head| head.rfind('\n'));
    after
        .or_else(before)
        .and_then(|n| n.checked_sub(1))
        .is_some_and(|cr| text.as_bytes().get(cr) == Some(&b'\r'))
}

/// `value` as JSON.
fn to_value<T: Serialize>(value: &T) -> Result<Value, RequestError> {
    serde_json::to_value(value).map_err(|e| RequestError::Failed(e.to_string()))
}

#[cfg(test)]
#[path = "code_action_tests.rs"]
mod tests;
