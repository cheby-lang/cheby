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

use std::path::{Path, PathBuf};

use cheby_driver::analysis::{FileId, FileLocation, ModuleView, Snapshot, TextRange, TextSize};
use cheby_driver::std_lib;
use lsp_types::{
    DocumentHighlight, DocumentHighlightKind, DocumentHighlightParams, GotoDefinitionParams,
    Location, LocationLink, MessageType, Position, Range, ReferenceParams,
    TextDocumentPositionParams, Uri,
};
use serde_json::Value;

use crate::features::handlers::std_file;
use crate::features::ide::nav::{self, HighlightKind, NavAnswer, NavFile, NavTarget};
use crate::features::index::{Index, Partial};
use crate::features::{Analyzed, HandlerOutput, RequestCx, RequestError, params};
use crate::server::Effect;
use crate::text::lines::{Bom, LspLines, PositionEncoding};
use crate::text::uri::{normalize, path_to_uri, uri_to_path};

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

/// The request's document in the snapshot.
struct RequestDoc<'s> {
    /// The URI as the client spelled it.
    uri: Uri,
    /// Its normalized path.
    path: PathBuf,
    /// Its module.
    view: ModuleView<'s>,
    /// Its text in the snapshot, which is the client's (D-1367).
    text: &'s str,
    /// Its lines.
    lines: LspLines,
}

impl<'s> RequestDoc<'s> {
    /// The document `pos` names, when `snapshot` holds it as a module: a
    /// package file, or a materialized std file under `std_dir`.
    fn of(
        snapshot: &'s Snapshot,
        pos: &TextDocumentPositionParams,
        std_dir: Option<&Path>,
    ) -> Option<Self> {
        let uri = pos.text_document.uri.clone();
        let path = normalize(&uri_to_path(&uri)?);
        let file = snapshot
            .file_of(&path)
            .or_else(|| std_file(snapshot, &path, std_dir?))?;
        let view = snapshot.module_at(file)?;
        let text = snapshot.source(file)?.text();
        let lines = LspLines::new(text, bom(snapshot, file));
        Some(Self {
            uri,
            path,
            view,
            text,
            lines,
        })
    }

    /// The offset of `position`.
    fn offset(&self, position: Position, enc: PositionEncoding) -> TextSize {
        self.lines.offset(self.text, position, enc)
    }

    /// `range` in protocol form.
    fn range(&self, range: TextRange, enc: PositionEncoding) -> Range {
        self.lines.range(self.text, range, enc)
    }
}

/// How the text of `file` counts a BOM: as the client sent it when the
/// file is open, as disk holds it otherwise (D-1368).
fn bom(snapshot: &Snapshot, file: FileId) -> Bom {
    match snapshot.location(file) {
        Some(FileLocation::Package(path)) if snapshot.version_of(path).is_some() => Bom::Counted,
        _ => Bom::Hidden,
    }
}

/// Converts targets of one snapshot to protocol locations, with each
/// file's lines made once.
struct Places<'s, 'd> {
    snapshot: &'s Snapshot,
    doc: &'d RequestDoc<'s>,
    std_dir: Option<&'d Path>,
    enc: PositionEncoding,
    /// Each file read so far, with its place when it has one.
    lines: Vec<(NavFile, Option<Placed<'s>>)>,
}

/// A file's URI, text and lines.
type Placed<'s> = (Uri, &'s str, LspLines);

impl<'s, 'd> Places<'s, 'd> {
    /// No file read yet.
    const fn new(
        snapshot: &'s Snapshot,
        doc: &'d RequestDoc<'s>,
        std_dir: Option<&'d Path>,
        enc: PositionEncoding,
    ) -> Self {
        Self {
            snapshot,
            doc,
            std_dir,
            enc,
            lines: Vec::new(),
        }
    }

    /// `target` as its URI, its name's range and its declaration's range.
    fn target(&mut self, target: &NavTarget) -> Option<(Uri, Range, Range)> {
        let (uri, name) = self.convert(&target.file, target.name)?;
        let (_, full) = self.convert(&target.file, target.full)?;
        Some((uri, name, full))
    }

    /// `range` of `file` as a URI and a protocol range. `None` for a file
    /// with no place on disk, or in std when no std directory is known.
    fn convert(&mut self, file: &NavFile, range: TextRange) -> Option<(Uri, Range)> {
        let n = if let Some(n) = self.lines.iter().position(|(f, _)| f == file) {
            n
        } else {
            let placed = self.place(file);
            self.lines.push((file.clone(), placed));
            self.lines.len().checked_sub(1)?
        };
        let (uri, text, lines) = self.lines.get(n)?.1.as_ref()?;
        Some((uri.clone(), lines.range(text, range, self.enc)))
    }

    /// The URI, text and lines of `file`.
    fn place(&self, file: &NavFile) -> Option<Placed<'s>> {
        let (uri, text, bom) = match file {
            NavFile::Run(id) => {
                let text = self.snapshot.source(*id)?.text();
                let uri = match self.snapshot.location(*id)? {
                    FileLocation::Package(path) if *path == self.doc.path => self.doc.uri.clone(),
                    FileLocation::Package(path) => path_to_uri(path).ok()?,
                    FileLocation::Std(display) => self.std_uri(display)?,
                    FileLocation::Synthetic => return None,
                };
                (uri, text, bom(self.snapshot, *id))
            }
            NavFile::Std(display) => {
                let text = std_lib::STD_MODULES
                    .iter()
                    .find(|m| std_lib::display_path(m.path) == *display)
                    .map_or("", |m| m.source);
                (self.std_uri(display)?, text, Bom::Hidden)
            }
        };
        Some((uri, text, LspLines::new(text, bom)))
    }

    /// The URI of std's file at `display`, such as `std/io.cheby`.
    fn std_uri(&self, display: &str) -> Option<Uri> {
        let path = display
            .split('/')
            .fold(self.std_dir?.to_path_buf(), |path, part| path.join(part));
        path_to_uri(&path).ok()
    }
}

#[cfg(test)]
#[path = "nav_tests.rs"]
mod tests;
