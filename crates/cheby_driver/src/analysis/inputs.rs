//! What a run reads: the editor's unsaved texts ([`Overlay`]), what the
//! session read from disk ([`DiskView`]), and the module loading of a run
//! (A §1.7), which builds a [`Package`] by hand from those texts.
//!
//! Loading is phase A of `cheby check` on the given texts: the manifest,
//! the walk of `src/`, then each module through [`parse_file`]. Nothing
//! here stops at a phase-A error. A file that is not UTF-8 gets the tree
//! of an empty text, so the front end never meets a module without a tree.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use cheby_diag::{Diagnostic, FileId, SourceMap};
use cheby_package::{MANIFEST_FILE, Manifest, ModuleFile, ModulePath, Target, Targets};

use crate::{Error, Module, Package, SourceHashes, parse_file};

/// The editor's unsaved truth: the open documents, by path. Paths are
/// absolute and normalized, as the session's root is.
#[derive(Clone, Debug, Default)]
pub struct Overlay {
    /// Each open document's text.
    pub files: BTreeMap<PathBuf, OverlayFile>,
}

/// One open document.
#[derive(Clone, Debug)]
pub struct OverlayFile {
    /// The document's text as the editor holds it.
    pub text: Arc<str>,
    /// The editor's version of the document.
    pub version: i32,
    /// Grows with each edit, across files, so the newest edit is known:
    /// a cycle cut prefers an import of the module edited last (D-1358).
    pub stamp: u64,
}

/// Where the text of a file of a run came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextOrigin {
    /// An open document of the [`Overlay`].
    Editor,
    /// The file on disk.
    Disk,
}

/// A file read from disk, with what [`DiskView::sweep`] compares.
#[derive(Debug)]
struct DiskFile {
    bytes: Arc<[u8]>,
    stamp: Option<FileStamp>,
}

/// What a stat of a file gives: its length and modification time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl FileStamp {
    /// The stamp of the file at `path`, `None` when it cannot be read.
    fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}

/// The walk of `src/` under one package name: its modules, or the
/// message of the error that stopped it, with its E0037 diagnostics.
#[derive(Debug)]
struct Walk {
    package: String,
    found: Result<Vec<ModuleFile>, String>,
    /// E0037 diagnostics, which carry no span, so they hold no handle of a
    /// run and can be replayed into any run.
    diagnostics: Vec<Diagnostic>,
}

/// What the session read from disk: the walk of `src/` and the bytes of
/// each file, until a change invalidates them.
#[derive(Debug)]
pub(crate) struct DiskView {
    root: PathBuf,
    walk: Option<Walk>,
    files: BTreeMap<PathBuf, DiskFile>,
}

impl DiskView {
    /// A view of the package at `root` that has read nothing yet.
    pub(crate) const fn new(root: PathBuf) -> Self {
        Self {
            root,
            walk: None,
            files: BTreeMap::new(),
        }
    }

    /// The bytes of the file at the absolute `path`, read once.
    ///
    /// # Errors
    ///
    /// An [`Error`] naming the file by its display path when it cannot be
    /// read.
    pub(crate) fn read(&mut self, path: &Path) -> Result<Arc<[u8]>, Error> {
        if let Some(file) = self.files.get(path) {
            return Ok(Arc::clone(&file.bytes));
        }
        let stamp = FileStamp::of(path);
        let bytes: Arc<[u8]> = std::fs::read(path)
            .map_err(|e| {
                Error::new(format!(
                    "{}: {e}",
                    cheby_diag::display_path(path, &self.root)
                ))
            })?
            .into();
        self.files.insert(
            path.to_path_buf(),
            DiskFile {
                bytes: Arc::clone(&bytes),
                stamp,
            },
        );
        Ok(bytes)
    }

    /// The modules on disk of the package named `package`, walked once per
    /// name, and their E0037 diagnostics. `Err` holds the message of a walk
    /// that failed, such as a missing `src/`.
    pub(crate) fn walk(&mut self, package: &str) -> (Result<&[ModuleFile], &str>, &[Diagnostic]) {
        if self
            .walk
            .as_ref()
            .is_none_or(|walk| walk.package != package)
        {
            let mut diagnostics = Vec::new();
            let found = cheby_package::discover(&self.root, package, &mut diagnostics)
                .map_err(|e| e.message().to_owned());
            self.walk = Some(Walk {
                package: package.to_owned(),
                found,
                diagnostics,
            });
        }
        match &self.walk {
            Some(walk) => (
                walk.found.as_deref().map_err(String::as_str),
                &walk.diagnostics,
            ),
            None => (Ok(&[]), &[]),
        }
    }

    /// Forgets what was read from these absolute paths. A path under `src/`
    /// or a directory also forgets the walk, since a file may have come or
    /// gone.
    pub(crate) fn invalidate(&mut self, paths: &[PathBuf]) {
        let src = self.root.join(cheby_package::SOURCE_DIR);
        for path in paths {
            self.files.remove(path);
            let prefix: Vec<PathBuf> = self
                .files
                .range(path.clone()..)
                .map(|(known, _)| known.clone())
                .take_while(|known| known.starts_with(path))
                .collect();
            for known in prefix {
                self.files.remove(&known);
            }
            if path.starts_with(&src) || src.starts_with(path) {
                self.walk = None;
            }
        }
    }

    /// Forgets the walk and every file read.
    pub(crate) fn invalidate_all(&mut self) {
        self.walk = None;
        self.files.clear();
    }

    /// Stats every file read and walks `src/` again. True when a file
    /// changed, appeared or went; what changed is forgotten.
    pub(crate) fn sweep(&mut self) -> bool {
        let changed: Vec<PathBuf> = self
            .files
            .iter()
            .filter(|(path, file)| FileStamp::of(path) != file.stamp)
            .map(|(path, _)| path.clone())
            .collect();
        let mut any = !changed.is_empty();
        for path in &changed {
            self.files.remove(path);
        }
        if let Some(walk) = &self.walk {
            let mut diagnostics = Vec::new();
            let found = cheby_package::discover(&self.root, &walk.package, &mut diagnostics)
                .map_err(|e| e.message().to_owned());
            let same = found == walk.found && diagnostics == walk.diagnostics;
            if !same {
                any = true;
                self.walk = None;
            }
        }
        any
    }
}

/// The text of one file of a run, as loading reads it.
#[derive(Debug)]
pub(crate) struct FileText {
    /// The file's bytes: the editor's text, or what disk holds.
    pub bytes: Arc<[u8]>,
    pub origin: TextOrigin,
    /// The editor's version, for an open document.
    pub version: Option<i32>,
}

impl FileText {
    /// The text of `path`: the overlay's when it is open, else the disk's.
    ///
    /// # Errors
    ///
    /// An [`Error`] when the file is not open and cannot be read.
    pub(crate) fn of(path: &Path, overlay: &Overlay, disk: &mut DiskView) -> Result<Self, Error> {
        match overlay.files.get(path) {
            Some(open) => Ok(Self {
                bytes: Arc::from(open.text.as_bytes()),
                origin: TextOrigin::Editor,
                version: Some(open.version),
            }),
            None => Ok(Self {
                bytes: disk.read(path)?,
                origin: TextOrigin::Disk,
                version: None,
            }),
        }
    }
}

/// One file that loading added to the run's source map.
#[derive(Debug)]
pub(crate) struct LoadedFile {
    /// Its absolute path.
    pub path: PathBuf,
    pub file: FileId,
    pub origin: TextOrigin,
    pub version: Option<i32>,
}

/// What loading gives: the package, unless its manifest cannot be used,
/// with the run's source map and its phase-A diagnostics, unsorted.
#[derive(Debug)]
pub(crate) struct Loaded {
    pub package: Option<Package>,
    pub sources: SourceMap,
    pub diagnostics: Vec<Diagnostic>,
    /// `cheby.toml` and then each module, in display order.
    pub files: Vec<LoadedFile>,
    /// The message of a walk of `src/` that failed (A §2.7).
    pub tool: Vec<String>,
}

/// Loads the package at `root` from `overlay` and disk (A §1.7): the
/// manifest, the modules the walk of `src/` finds plus the open documents
/// [`cheby_package::module_file`] accepts, each parsed and validated as
/// `cheby check` does. `target` narrows the manifest's targets as
/// `--target` does.
///
/// # Errors
///
/// An [`Error`] for a file that cannot be read, a file too large for the
/// source map, or a `target` the package does not have.
pub(crate) fn load(
    root: &Path,
    overlay: &Overlay,
    disk: &mut DiskView,
    target: Option<Target>,
) -> Result<Loaded, Error> {
    let mut sources = SourceMap::new();
    let mut diagnostics = Vec::new();
    let manifest_path = root.join(MANIFEST_FILE);
    let text = FileText::of(&manifest_path, overlay, disk)?;
    let (file, manifest) = if let Ok(manifest_text) = std::str::from_utf8(&text.bytes) {
        let file = sources
            .add_file(MANIFEST_FILE, manifest_text)
            .map_err(|_| Error::from(cheby_package::PackageError::too_large(MANIFEST_FILE)))?;
        let manifest = Manifest::parse_at(file, manifest_text, root, &mut diagnostics);
        (file, manifest)
    } else {
        // Only a file on disk can be other than UTF-8: `Manifest::load`
        // reports it as `cheby check` does.
        let manifest = Manifest::load(root, &mut sources, &mut diagnostics)?;
        let file = sources
            .files()
            .next()
            .map(|(file, _)| file)
            .ok_or_else(|| Error::new(format!("{MANIFEST_FILE} was not read")))?;
        (file, manifest)
    };
    let mut files = vec![LoadedFile {
        path: manifest_path,
        file,
        origin: text.origin,
        version: text.version,
    }];
    let Some(mut manifest) = manifest else {
        return Ok(Loaded {
            package: None,
            sources,
            diagnostics,
            files,
            tool: Vec::new(),
        });
    };
    if let Some(target) = target {
        if !manifest.targets.contains(target) {
            return Err(Error::new(format!(
                "the package does not target `{target}`"
            )));
        }
        manifest.targets = Targets::only(target);
    }
    let mut tool = Vec::new();
    let mut listed: BTreeMap<String, ModuleFile> = BTreeMap::new();
    let (walked, walk_diagnostics) = disk.walk(&manifest.name);
    diagnostics.extend(walk_diagnostics.iter().cloned());
    match walked {
        Ok(found) => {
            for module in found {
                listed.insert(module.display.clone(), module.clone());
            }
        }
        Err(message) => tool.push(message.to_owned()),
    }
    // An open document under `src/` is a module before it exists on disk.
    for path in overlay.files.keys() {
        if let Some(module) = cheby_package::module_file(root, &manifest.name, path) {
            listed.insert(module.display.clone(), module);
        }
    }
    let mut modules = Vec::with_capacity(listed.len());
    for module in listed.into_values() {
        let path = root.join(&module.file);
        let text = FileText::of(&path, overlay, disk)?;
        let loaded = load_module(
            &mut sources,
            module.path,
            &module.display,
            &text,
            &mut diagnostics,
        )?;
        files.push(LoadedFile {
            path,
            file: loaded.source.file,
            origin: text.origin,
            version: text.version,
        });
        modules.push(loaded);
    }
    Ok(Loaded {
        package: Some(Package {
            root: root.to_path_buf(),
            manifest,
            modules,
        }),
        sources,
        diagnostics,
        files,
        tool,
    })
}

/// Adds one module's text to `sources` and parses it as [`parse_file`]
/// does. A file that is not UTF-8 gets the tree of an empty text, so the
/// front end has a tree for every module.
pub(super) fn load_module(
    sources: &mut SourceMap,
    path: ModulePath,
    display: &str,
    text: &FileText,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Module, Error> {
    let mut own = Vec::new();
    let mut source = parse_file(sources, display, text.bytes.to_vec(), &mut own)?;
    if source.syntax.is_none() {
        let (tree, _) = cheby_syntax::parse(source.file, Arc::from("")).into_parts();
        source.syntax = Some(tree);
    }
    diagnostics.extend(own.iter().cloned());
    Ok(Module {
        path,
        source,
        hashes: SourceHashes::of(&text.bytes),
        parsed: true,
        syntax_diagnostics: own,
    })
}
