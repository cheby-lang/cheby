//! Navigation requests (X §2.2 to §2.7) over `ide::nav`, answered from a
//! snapshot of the request's text (D-1367):
//!
//! - `textDocument/definition`, `declaration` and `typeDefinition`: class
//!   `Package`, since a target may lie in another open document, whose
//!   text must be the client's for the target's position to be right.
//! - `textDocument/references`: class `Package`, for the same reason.
//! - `textDocument/documentHighlight`: class `Doc`, since it reads only the
//!   request's own document.
//!
//! A go-to answer is `LocationLink`s when the client has `linkSupport`
//! for the method, else one `Location`, or several. A target in std is the
//! materialized std file (A §6.2, D-1369), so the editor opens a real
//! file, which routes to std's analysis (D-1370). Nothing found is `null`
//! (X §1.4).

use std::path::Path;

use cheby_driver::analysis::{ModuleView, TextSize};
use lsp_types::{
    DocumentHighlight, DocumentHighlightKind, DocumentHighlightParams, GotoDefinitionParams,
    Location, LocationLink, MessageType, Range, ReferenceParams, TextDocumentPositionParams, Uri,
};
use serde_json::Value;

use crate::features::handlers::doc::{Places, RequestDoc};
use crate::features::ide::nav::{self, HighlightKind, NavAnswer, NavFile};
use crate::features::index::{Index, Partial};
use crate::features::{Analyzed, HandlerOutput, RequestCx, RequestError, params};
use crate::server::Effect;

/// Which go-to requests the client takes `LocationLink`s for, read once
/// from `initialize`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LinkCaps {
    /// `textDocument.definition.linkSupport`.
    pub definition: bool,
    /// `textDocument.declaration.linkSupport`.
    pub declaration: bool,
    /// `textDocument.typeDefinition.linkSupport`.
    pub type_definition: bool,
}

impl LinkCaps {
    /// Reads the capabilities of `params`, the `initialize` parameters. A
    /// missing or odd capability is absent.
    pub(crate) fn from_params(params: &Value) -> Self {
        let link = |method: &str| {
            params
                .pointer(&format!("/capabilities/textDocument/{method}/linkSupport"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        Self {
            definition: link("definition"),
            declaration: link("declaration"),
            type_definition: link("typeDefinition"),
        }
    }
}

/// What navigation reads besides the request: how the client takes
/// go-to answers, and where std's sources are materialized (A §6.2).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NavEnv<'a> {
    /// The client's `linkSupport`s.
    pub links: LinkCaps,
    /// The directory `materialize_std` wrote, whose `std/…` files a std
    /// location names. `None`: a target in std is dropped.
    pub std_dir: Option<&'a Path>,
}

/// `textDocument/definition` (X §2.2). Class `Package`.
///
/// # Errors
///
/// `InvalidParams` when the parameters do not have the method's shape, and
/// `Cancelled`.
pub(crate) fn definition(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
    env: &NavEnv<'_>,
) -> Result<HandlerOutput, RequestError> {
    let p: GotoDefinitionParams = params(params_value)?;
    goto(
        cx,
        analyzed,
        &p.text_document_position_params,
        env,
        env.links.definition,
        nav::definition,
    )
}

/// `textDocument/declaration` (X §2.3). Class `Package`.
///
/// # Errors
///
/// As [`definition`].
pub(crate) fn declaration(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
    env: &NavEnv<'_>,
) -> Result<HandlerOutput, RequestError> {
    let p: GotoDefinitionParams = params(params_value)?;
    goto(
        cx,
        analyzed,
        &p.text_document_position_params,
        env,
        env.links.declaration,
        nav::declaration,
    )
}

/// `textDocument/typeDefinition` (X §2.4). Class `Package`.
///
/// # Errors
///
/// As [`definition`].
pub(crate) fn type_definition(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
    env: &NavEnv<'_>,
) -> Result<HandlerOutput, RequestError> {
    let p: GotoDefinitionParams = params(params_value)?;
    goto(
        cx,
        analyzed,
        &p.text_document_position_params,
        env,
        env.links.type_definition,
        nav::type_definition,
    )
}

/// `textDocument/references` (X §2.6). Class `Package`. A list that may
/// miss some uses (F §5.5) is still answered, with one
/// `window/logMessage` that says why.
///
/// # Errors
///
/// As [`definition`].
pub(crate) fn references(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
    env: &NavEnv<'_>,
) -> Result<HandlerOutput, RequestError> {
    let p: ReferenceParams = params(params_value)?;
    let snapshot = &*analyzed.snapshot;
    let Some(doc) = RequestDoc::of(snapshot, &p.text_document_position, env.std_dir) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    cx.cancel.check()?;
    let index = analyzed.index();
    let offset = doc.offset(p.text_document_position.position, cx.encoding);
    let Some(found) = nav::references(
        index,
        doc.view,
        offset,
        p.context.include_declaration,
        cx.cancel,
    )?
    else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    let mut places = Places::new(snapshot, &doc, env.std_dir, cx.encoding);
    let mut located: Vec<(String, Range, Location)> = Vec::with_capacity(found.hits.len());
    for &(file, range) in &found.hits {
        cx.cancel.check()?;
        if let Some((uri, range)) = places.convert(&NavFile::Run(file), range) {
            let key = uri.as_str().to_owned();
            located.push((key, range, Location::new(uri, range)));
        }
    }
    // Sorted by path, then start, then end, with no duplicates (X §1.4).
    located.sort_by(|a, b| (&a.0, a.1.start, a.1.end).cmp(&(&b.0, b.1.start, b.1.end)));
    located.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    let locations: Vec<Location> = located.into_iter().map(|(_, _, l)| l).collect();
    let effects = found
        .partial
        .as_ref()
        .map(|partial| {
            Effect::log(
                MessageType::INFO,
                partial_message(index, &found.text, partial),
            )
        })
        .into_iter()
        .collect();
    let value = to_value(&locations)?;
    Ok(HandlerOutput { value, effects })
}

/// `textDocument/documentHighlight` (X §2.7). Class `Doc`.
///
/// # Errors
///
/// As [`definition`].
pub(crate) fn document_highlight(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
    env: &NavEnv<'_>,
) -> Result<HandlerOutput, RequestError> {
    let p: DocumentHighlightParams = params(params_value)?;
    let pos = &p.text_document_position_params;
    let snapshot = &*analyzed.snapshot;
    let Some(doc) = RequestDoc::of(snapshot, pos, env.std_dir) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    cx.cancel.check()?;
    let offset = doc.offset(pos.position, cx.encoding);
    let Some(found) = nav::highlights(analyzed.index(), doc.view, offset) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    let highlights: Vec<DocumentHighlight> = found
        .into_iter()
        .map(|(range, kind)| DocumentHighlight {
            range: doc.range(range, cx.encoding),
            kind: Some(match kind {
                HighlightKind::Text => DocumentHighlightKind::TEXT,
                HighlightKind::Read => DocumentHighlightKind::READ,
                HighlightKind::Write => DocumentHighlightKind::WRITE,
            }),
        })
        .collect();
    to_value(&highlights).map(HandlerOutput::value)
}

/// What a go-to request finds at a position of a module.
type Finder = fn(Index<'_>, ModuleView<'_>, TextSize) -> Option<NavAnswer>;

/// A go-to request at `pos`: `find`'s answer as links when `link`, else as
/// locations.
fn goto(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    pos: &TextDocumentPositionParams,
    env: &NavEnv<'_>,
    link: bool,
    find: Finder,
) -> Result<HandlerOutput, RequestError> {
    let snapshot = &*analyzed.snapshot;
    let Some(doc) = RequestDoc::of(snapshot, pos, env.std_dir) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    cx.cancel.check()?;
    let offset = doc.offset(pos.position, cx.encoding);
    let Some(found) = find(analyzed.index(), doc.view, offset) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    cx.cancel.check()?;
    let origin = doc.range(found.origin, cx.encoding);
    let mut places = Places::new(snapshot, &doc, env.std_dir, cx.encoding);
    let converted: Vec<(Uri, Range, Range)> = found
        .targets
        .iter()
        .filter_map(|target| places.target(target))
        .collect();
    let value = match converted.as_slice() {
        [] => Value::Null,
        _ if link => to_value(
            &converted
                .into_iter()
                .map(|(uri, name, full)| LocationLink {
                    origin_selection_range: Some(origin),
                    target_uri: uri,
                    target_range: full,
                    target_selection_range: name,
                })
                .collect::<Vec<_>>(),
        )?,
        [(uri, name, _)] => to_value(&Location::new(uri.clone(), *name))?,
        _ => to_value(
            &converted
                .into_iter()
                .map(|(uri, name, _)| Location::new(uri, name))
                .collect::<Vec<_>>(),
        )?,
    };
    Ok(HandlerOutput::value(value))
}

/// `value` as JSON.
fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, RequestError> {
    serde_json::to_value(value).map_err(|e| RequestError::Failed(e.to_string()))
}

/// The line `references` logs when its list may be incomplete (F §5.5).
fn partial_message(index: Index<'_>, text: &str, partial: &Partial) -> String {
    let path = |module| {
        index
            .snapshot()
            .module(module)
            .and_then(ModuleView::path)
            .map_or_else(|| "a module".to_owned(), |p| format!("`{p}`"))
    };
    match partial {
        Partial::Unknown { module, .. } => format!(
            "references of `{text}` may be incomplete: {} has a `{text}` that could not be resolved",
            path(*module)
        ),
        Partial::Lost { module } => format!(
            "references of `{text}` may be incomplete: {} could not be analyzed",
            path(*module)
        ),
    }
}

#[cfg(test)]
#[path = "nav_tests.rs"]
mod tests;
