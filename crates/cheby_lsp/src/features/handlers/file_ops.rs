//! `workspace/willRenameFiles` (X §3.6, D-1674, D-1675), over
//! `ide::file_ops`.
//!
//! Class `Package`, routed by its first file's old URI. A move that has
//! no module meaning is refused with `null` and a `window/showMessage`
//! that says why, since the client moves the files whatever the answer.
//! So is a move onto a module file, and a move that `verify` refuses, as
//! "moving would cause: …" (X §3.6 step 5, D-1724). A move of a closed
//! file that changed on disk is refused with its own warning, "`path`
//! changed on disk, so imports were not updated", and also makes the
//! package read disk again (`Effect::Sweep`), as rename does (D-1711). A
//! move with nothing to edit answers `null` with no message. A move in a
//! std checkout is refused, with a message that std's modules are listed in
//! `crates/cheby_driver/src/std_lib.rs` and specified in `docs/stdlib/`
//! (D-1706): a file's or a folder's under the checkout's `std/`, or that
//! `std/` itself, refused by the main loop from routing alone
//! ([`std_move_refused`]), with or without the checkout's session.

use std::path::PathBuf;

use lsp_types::{MessageType, RenameFilesParams, Uri};
use serde_json::Value;

use crate::features::handlers::edit::to_workspace_edit;
use crate::features::ide::Refusal;
use crate::features::ide::file_ops::{STD_MOVE, will_rename};
use crate::features::ide::rename::RenameError;
use crate::features::{Analyzed, HandlerOutput, RequestCx, RequestError, params};
use crate::server::Effect;
use crate::text::uri::{normalize, uri_to_path};

/// `workspace/willRenameFiles` (class `Package`).
///
/// # Errors
///
/// `InvalidParams` for parameters of another shape, `Cancelled`, and
/// `Failed` for an edit the shared conversion cannot write.
pub(crate) fn will_rename_files(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
) -> Result<HandlerOutput, RequestError> {
    let params: RenameFilesParams = params(params_value)?;
    // A pair whose URIs name no file is no file of a package.
    let moves: Vec<(PathBuf, PathBuf)> = params
        .files
        .iter()
        .filter_map(|file| Some((path_of(&file.old_uri)?, path_of(&file.new_uri)?)))
        .collect();
    match will_rename(analyzed, &moves, cx.cancel) {
        Ok(Some(plan)) if !plan.edits.is_empty() => {
            let out = to_workspace_edit(cx, analyzed, &plan.edits, plan.note.as_deref())?;
            let value =
                serde_json::to_value(&out.edit).map_err(|e| RequestError::Failed(e.to_string()))?;
            Ok(HandlerOutput {
                value,
                effects: out.effects,
            })
        }
        Ok(Some(_) | None) => Ok(HandlerOutput::value(Value::Null)),
        Err(RenameError::Refused(Refusal(reason))) => Ok(refused(&reason)),
        // The refusal already says the imports were not updated (D-1711).
        Err(RenameError::Stale { refusal, .. }) => Ok(HandlerOutput {
            value: Value::Null,
            effects: vec![
                Effect::show(MessageType::WARNING, refusal.0),
                Effect::Sweep {
                    root: analyzed.snapshot.root().to_path_buf(),
                },
            ],
        }),
        Err(RenameError::Cancelled) => Err(RequestError::Cancelled),
    }
}

/// D-1706's answer to a move in a std checkout: `null`, with the warning
/// that std's modules are listed in `std_lib.rs` and specified in
/// `docs/stdlib/`. The main loop sends it from routing alone, before any
/// session is looked up (`dispatch_query`).
pub(crate) fn std_move_refused() -> HandlerOutput {
    refused(STD_MOVE)
}

/// The normalized path `uri` names, `None` for a URI that names no file.
fn path_of(uri: &str) -> Option<PathBuf> {
    let uri: Uri = uri.parse().ok()?;
    uri_to_path(&uri).map(|path| normalize(&path))
}

/// The answer to a refused move: `null`, with a warning that says why the
/// imports were not updated (D-1675).
fn refused(reason: &str) -> HandlerOutput {
    let mut chars = reason.chars();
    let reason: String = chars
        .next()
        .map_or_default(|first| first.to_lowercase().chain(chars).collect());
    HandlerOutput {
        value: Value::Null,
        effects: vec![Effect::show(
            MessageType::WARNING,
            format!("The imports were not updated: {reason}"),
        )],
    }
}
