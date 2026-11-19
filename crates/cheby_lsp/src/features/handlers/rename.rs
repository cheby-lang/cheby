//! `textDocument/prepareRename` and `textDocument/rename` (X §3,
//! D-1389 to D-1392, D-1667 to D-1673, D-1698), over `ide::rename`.
//!
//! - `prepareRename`: class `Doc`. It edits nothing, so it also runs in a
//!   read-only document, such as a std source, where it refuses every
//!   name with D-1390's message (D-1673). A name that can be renamed is
//!   answered with its token's range and its text as the placeholder, a
//!   refusal is `RequestFailed` with its reason, and a place with no name,
//!   such as whitespace, a keyword, a literal or a test's name, is `null`.
//! - `rename`: class `Package`, since its edit spans the package's open
//!   documents. A read-only document gets `null` by the route rule, and a
//!   client that skipped `prepareRename` hears no reason (D-1673). The
//!   plan goes through the shared edit layer, which carries a note as an
//!   annotation or a `window/showMessage` (D-1391). A refusal is
//!   `RequestFailed`. A file that changed on disk since the snapshot
//!   (D-1389, D-1669) is answered `null` with a `window/showMessage`
//!   warning that says so, since an error answer carries no effects, and
//!   the package is swept first (`Effect::Sweep`, G1 plan §3 E), so the
//!   next try reads the file again (D-1711).

use std::path::Path;

use lsp_types::{MessageType, PrepareRenameResponse, RenameParams, TextDocumentPositionParams};
use serde_json::Value;

use crate::features::handlers::doc::RequestDoc;
use crate::features::handlers::edit;
use crate::features::ide::Refusal;
use crate::features::ide::rename::{self, RenameError};
use crate::features::{Analyzed, HandlerOutput, RequestCx, RequestError, params};
use crate::server::Effect;

/// `textDocument/prepareRename` (class `Doc`).
///
/// # Errors
///
/// `InvalidParams` for parameters of another shape, `Failed` for a name
/// that cannot be renamed, and `Cancelled`.
pub(crate) fn prepare_rename(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
) -> Result<HandlerOutput, RequestError> {
    prepare_in(cx, analyzed, params_value, cx.std.dir())
}

/// `textDocument/rename` (class `Package`).
///
/// # Errors
///
/// `InvalidParams` for parameters of another shape, `Failed` for a
/// rename that is refused or an edit that cannot be sent, and
/// `Cancelled`. A file that changed on disk is no error (module doc).
pub(crate) fn rename(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
) -> Result<HandlerOutput, RequestError> {
    rename_in(cx, analyzed, params_value, cx.std.dir())
}

/// [`prepare_rename`] with std's sources under `std_dir`, where a std
/// document is found.
fn prepare_in(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
    std_dir: Option<&Path>,
) -> Result<HandlerOutput, RequestError> {
    let p: TextDocumentPositionParams = params(params_value)?;
    let snapshot = &*analyzed.snapshot;
    let Some(doc) = RequestDoc::of(snapshot, &p, std_dir) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    let Some(file) = doc.view.file() else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    cx.cancel.check()?;
    let offset = doc.offset(p.position, cx.encoding);
    let Some((range, placeholder)) = rename::prepare(analyzed, file, offset).map_err(refused)?
    else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    let answer = PrepareRenameResponse::RangeWithPlaceholder {
        range: doc.range(range, cx.encoding),
        placeholder,
    };
    to_value(&answer).map(HandlerOutput::value)
}

/// [`rename()`] with std's sources under `std_dir`, where a std document is
/// found. The route never runs it on one (D-1673), and `ide::rename`
/// refuses every name there if it did.
fn rename_in(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
    std_dir: Option<&Path>,
) -> Result<HandlerOutput, RequestError> {
    let p: RenameParams = params(params_value)?;
    let pos = &p.text_document_position;
    let snapshot = &*analyzed.snapshot;
    let Some(doc) = RequestDoc::of(snapshot, pos, std_dir) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    let Some(file) = doc.view.file() else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    cx.cancel.check()?;
    let offset = doc.offset(pos.position, cx.encoding);
    let plan = match rename::rename(analyzed, file, offset, &p.new_name, cx.cancel) {
        Ok(Some(plan)) => plan,
        Ok(None) => return Ok(HandlerOutput::value(Value::Null)),
        Err(RenameError::Refused(refusal)) => return Err(refused(refusal)),
        Err(RenameError::Cancelled) => return Err(RequestError::Cancelled),
        Err(RenameError::Stale { refusal, .. }) => {
            return Ok(HandlerOutput {
                value: Value::Null,
                effects: vec![
                    Effect::Sweep {
                        root: snapshot.root().to_path_buf(),
                    },
                    Effect::show(MessageType::WARNING, refusal.0),
                ],
            });
        }
    };
    let out = edit::to_workspace_edit(cx, analyzed, &plan.edits, plan.note.as_deref())?;
    Ok(HandlerOutput {
        value: to_value(&out.edit)?,
        effects: out.effects,
    })
}

/// A refusal as the request's error: `RequestFailed` with its reason.
fn refused(refusal: Refusal) -> RequestError {
    RequestError::Failed(refusal.0)
}

/// `value` as JSON.
fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, RequestError> {
    serde_json::to_value(value).map_err(|e| RequestError::Failed(e.to_string()))
}

#[cfg(test)]
#[path = "rename_tests.rs"]
mod tests;
