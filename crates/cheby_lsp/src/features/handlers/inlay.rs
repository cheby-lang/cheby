//! `textDocument/inlayHint` handler (X §7.1, plan W8): the hints of
//! `ide::hints` that lie in the request's range, as `InlayHint`s with
//! plain-string labels: of kind `Type` for a type, and `Parameter` for a
//! parameter's name (D-1515). A label carries its own separator and space,
//! so no hint sets `paddingLeft` or `paddingRight`.
//!
//! Class `Doc`: the answer reads the snapshot of the document's exact text
//! and no other document's. There is no `inlayHint/resolve`, so the
//! server advertises `resolveProvider: false` and every hint is whole
//! when sent (plan §1.10, D-1379). It reads no client capability.
//!
//! Settings (D-1376, D-1480): `inlayHints.enable` off answers `null`, and
//! `letTypes`, `patternBinders`, `useBinders`, `closureTypes`,
//! `localFnTypes`, `pipeStages` and `dyn` turn their rows off.
//! `parameterNames`, off by default, turns the parameter names on (D-1515).
//! `constants` joins with W9.

use cheby_driver::analysis::{FileId, FileLocation, Snapshot};
use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, InlayHintParams, Range, TextEdit};
use serde_json::Value;

use crate::features::ide::hints::{HintKind, Switches, hints};
use crate::features::{Analyzed, HandlerOutput, RequestCx, RequestError, params};
use crate::text::lines::{Bom, LspLines};
use crate::text::uri::{normalize, uri_to_path};

/// `textDocument/inlayHint`, class `Doc`: the hints whose position lies in
/// the request's range, in text order. `null` when `inlayHints.enable` is
/// off or the snapshot has no module for the document.
///
/// # Errors
///
/// `InvalidParams` when the parameters do not have the method's shape, and
/// `Cancelled` on `$/cancelRequest`.
pub(crate) fn inlay_hint(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
) -> Result<HandlerOutput, RequestError> {
    let p: InlayHintParams = params(params_value)?;
    let settings = &cx.settings.inlay_hints;
    if !settings.enable {
        return Ok(HandlerOutput::value(Value::Null));
    }
    let snapshot: &Snapshot = &analyzed.snapshot;
    let Some(path) = uri_to_path(&p.text_document.uri).map(|path| normalize(&path)) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    let Some(file) = file_of(snapshot, &path) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    let (Some(view), Some(source)) = (snapshot.module_at(file), snapshot.source(file)) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    let text = source.text();
    // The client's own text counts a BOM, disk text hides it (D-1368).
    let bom = if snapshot.version_of(&path).is_some() {
        Bom::Counted
    } else {
        Bom::Hidden
    };
    let lines = LspLines::new(text, bom);
    let within = lines.text_range(text, p.range, cx.encoding);
    let switches = Switches {
        let_types: settings.let_types,
        pattern_binders: settings.pattern_binders,
        use_binders: settings.use_binders,
        closure_types: settings.closure_types,
        local_fn_types: settings.local_fn_types,
        pipe_stages: settings.pipe_stages,
        dyn_: settings.dyn_,
        parameter_names: settings.parameter_names,
    };
    let found = hints(snapshot, view, Some(within), switches, cx.cancel)?;
    let answer: Vec<InlayHint> = found
        .into_iter()
        .map(|hint| {
            let position = lines.position(text, hint.offset, cx.encoding);
            InlayHint {
                position,
                label: InlayHintLabel::String(hint.label),
                kind: Some(match hint.kind {
                    HintKind::Type => InlayHintKind::TYPE,
                    HintKind::Parameter => InlayHintKind::PARAMETER,
                }),
                text_edits: hint.edit.map(|new_text| {
                    vec![TextEdit {
                        range: Range::new(position, position),
                        new_text,
                    }]
                }),
                tooltip: None,
                padding_left: None,
                padding_right: None,
                data: None,
            }
        })
        .collect();
    cx.cancel.check()?;
    serde_json::to_value(answer)
        .map(HandlerOutput::value)
        .map_err(|e| RequestError::Failed(e.to_string()))
}

/// The file at `path`: a file of the package, or a std source opened from
/// its materialized directory, whose display path ends it (A §6.3).
fn file_of(snapshot: &Snapshot, path: &std::path::Path) -> Option<FileId> {
    snapshot.file_of(path).or_else(|| {
        snapshot
            .sources()
            .files()
            .find_map(|(file, _)| match snapshot.location(file)? {
                FileLocation::Std(display) => path.ends_with(display).then_some(file),
                FileLocation::Package(_) | FileLocation::Synthetic => None,
            })
    })
}

#[cfg(test)]
mod tests;
