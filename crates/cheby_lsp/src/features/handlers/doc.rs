//! Shared request code (G1 plan §1.3): the request's document in a
//! snapshot, offsets and ranges in protocol form, and the BOM rule (an
//! open file counts its BOM, a closed one hides it, D-1368), which every
//! multi-file answer needs. Navigation (`nav`) and the edit layer
//! (`edit`) read them from here.

use std::path::{Path, PathBuf};

use cheby_driver::analysis::{FileId, FileLocation, ModuleView, Snapshot, TextRange, TextSize};
use cheby_driver::std_lib;
use lsp_types::{Position, Range, TextDocumentPositionParams, Uri};

use crate::features::handlers::std_file;
use crate::features::ide::nav::{NavFile, NavTarget};
use crate::text::lines::{Bom, LspLines, PositionEncoding};
use crate::text::uri::{normalize, path_to_uri, uri_to_path};

/// The request's document in the snapshot.
pub(crate) struct RequestDoc<'s> {
    /// The URI as the client spelled it.
    pub uri: Uri,
    /// Its normalized path.
    pub path: PathBuf,
    /// Its module.
    pub view: ModuleView<'s>,
    /// Its text in the snapshot, which is the client's (D-1367).
    pub text: &'s str,
    /// Its lines.
    pub lines: LspLines,
}

impl<'s> RequestDoc<'s> {
    /// The document `pos` names, when `snapshot` holds it as a module: a
    /// package file, or a materialized std file under `std_dir`.
    pub(crate) fn of(
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
    pub(crate) fn offset(&self, position: Position, enc: PositionEncoding) -> TextSize {
        self.lines.offset(self.text, position, enc)
    }

    /// `range` in protocol form.
    pub(crate) fn range(&self, range: TextRange, enc: PositionEncoding) -> Range {
        self.lines.range(self.text, range, enc)
    }
}

/// How the text of `file` counts a BOM: as the client sent it when the
/// file is open, as disk holds it otherwise (D-1368).
pub(crate) fn bom(snapshot: &Snapshot, file: FileId) -> Bom {
    match snapshot.location(file) {
        Some(FileLocation::Package(path)) if snapshot.version_of(path).is_some() => Bom::Counted,
        _ => Bom::Hidden,
    }
}

/// Converts targets of one snapshot to protocol locations, with each
/// file's lines made once.
pub(crate) struct Places<'s, 'd> {
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
    pub(crate) const fn new(
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
    pub(crate) fn target(&mut self, target: &NavTarget) -> Option<(Uri, Range, Range)> {
        let (uri, name) = self.convert(&target.file, target.name)?;
        let (_, full) = self.convert(&target.file, target.full)?;
        Some((uri, name, full))
    }

    /// `range` of `file` as a URI and a protocol range. `None` for a file
    /// with no place on disk, or in std when no std directory is known.
    pub(crate) fn convert(&mut self, file: &NavFile, range: TextRange) -> Option<(Uri, Range)> {
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
                    FileLocation::Package(path) | FileLocation::Dependency(path) => {
                        path_to_uri(path).ok()?
                    }
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
    pub(crate) fn std_uri(&self, display: &str) -> Option<Uri> {
        let path = display
            .split('/')
            .fold(self.std_dir?.to_path_buf(), |path, part| path.join(part));
        path_to_uri(&path).ok()
    }
}
