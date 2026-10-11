//! Module discovery: mapping `src/` files to module paths (§7.2, B2).
//!
//! - Each `.cheby` file under `src/` is a module. Its path is the package
//!   name followed by the file's path relative to `src/`, with `::`
//!   between directories and the extension removed (D-162).
//! - `src/main.cheby` is the root module, whose path is just the package
//!   name (D-142).
//! - The names of `.cheby` files and of directories must be valid `LOWER`
//!   names (E0037). Hidden entries are ignored, and so are other files,
//!   such as JS helpers (D-208).
//!
//! The result is sorted by file path, so it does not depend on the order
//! in which the file system lists a directory (D-264).

use std::fmt;
use std::path::{Path, PathBuf};

use cheby_diag::{Diagnostic, codes};

use crate::PackageError;
use crate::names::{is_valid_name, suggest_name};

/// The directory of a package that holds its modules (§13.2).
pub const SOURCE_DIR: &str = "src";
/// The extension of module files.
pub const EXTENSION: &str = "cheby";
/// The file of the root module, relative to `src/` (D-142).
pub const ROOT_FILE: &str = "main.cheby";
/// How deep directories under `src/` may nest. It bounds the walk when
/// symbolic links form a cycle.
const MAX_DEPTH: usize = 64;

/// A module path, such as `my_app::web::router` (§7.2). The first segment
/// is the package name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModulePath {
    segments: Vec<String>,
}

impl ModulePath {
    /// The path of a package's root module: just the package name.
    #[must_use]
    pub fn root(package: &str) -> Self {
        Self {
            segments: vec![package.to_owned()],
        }
    }

    /// This path extended by one segment.
    #[must_use]
    pub fn child(&self, name: &str) -> Self {
        let mut segments = self.segments.clone();
        segments.push(name.to_owned());
        Self { segments }
    }

    /// The segments, package name first.
    #[must_use]
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// The package name, the first segment.
    #[must_use]
    pub fn package(&self) -> &str {
        self.segments.first().map_or("", String::as_str)
    }

    /// Whether this is the root module of its package.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.segments.len() == 1
    }
}

impl fmt::Display for ModulePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.segments.join("::"))
    }
}

/// A module of the package and the file that holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleFile {
    pub path: ModulePath,
    /// The file, relative to the package root, such as `src/web/router.cheby`.
    pub file: PathBuf,
    /// `file` with `/` separators, the path shown in diagnostics (D-421).
    pub display: String,
}

/// Finds every module of the package `package` at `root`. Files and
/// directories with invalid names are reported as E0037 and skipped,
/// with everything inside them.
///
/// # Errors
///
/// Returns an error if `src/` is missing or a directory cannot be read.
pub fn discover(
    root: &Path,
    package: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Vec<ModuleFile>, PackageError> {
    let src = root.join(SOURCE_DIR);
    if !src.is_dir() {
        return Err(PackageError::new(format!(
            "the package has no `{SOURCE_DIR}` directory"
        )));
    }
    let mut walk = Walk {
        package,
        modules: Vec::new(),
        diagnostics,
    };
    walk.dir(&src, &[], 0)?;
    let mut modules = walk.modules;
    modules.sort_by(|a, b| a.display.cmp(&b.display));
    Ok(modules)
}

struct Walk<'a> {
    package: &'a str,
    modules: Vec<ModuleFile>,
    diagnostics: &'a mut Vec<Diagnostic>,
}

impl Walk<'_> {
    /// Walks `dir`, whose names relative to `src/` are `prefix`.
    fn dir(&mut self, dir: &Path, prefix: &[String], depth: usize) -> Result<(), PackageError> {
        let shown = display(prefix, None);
        if depth > MAX_DEPTH {
            return Err(PackageError::new(format!(
                "{shown}: directories nest more than {MAX_DEPTH} levels deep"
            )));
        }
        let read = std::fs::read_dir(dir).map_err(|e| PackageError::io(&shown, &e))?;
        let mut entries = Vec::new();
        for entry in read {
            let entry = entry.map_err(|e| PackageError::io(&shown, &e))?;
            entries.push(entry.file_name());
        }
        entries.sort();
        for name in entries {
            let path = dir.join(&name);
            let name = name.to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let is_cheby_file = name
                .strip_suffix(EXTENSION)
                .is_some_and(|s| s.ends_with('.'));
            // `metadata` follows symbolic links; MAX_DEPTH bounds cycles.
            let meta = match std::fs::metadata(&path) {
                Ok(meta) => meta,
                // A dangling symbolic link that is not named like a module
                // is neither a `.cheby` file nor a directory, so discovery
                // ignores it (D-208).
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && !is_cheby_file => continue,
                Err(e) => return Err(PackageError::io(&display(prefix, Some(&name)), &e)),
            };
            if meta.is_dir() {
                if self.check_name(prefix, &name, &name, "directory") {
                    let mut inner = prefix.to_vec();
                    inner.push(name);
                    self.dir(&path, &inner, depth.saturating_add(1))?;
                }
            } else if let Some(stem) = name
                .strip_suffix(EXTENSION)
                .and_then(|s| s.strip_suffix('.'))
                && self.check_name(prefix, &name, stem, "file")
            {
                self.module(prefix, stem, &name);
            }
        }
        Ok(())
    }

    /// Records the module in file `name`, with stem `stem`, under `prefix`.
    fn module(&mut self, prefix: &[String], stem: &str, name: &str) {
        let mut path = ModulePath::root(self.package);
        for segment in prefix {
            path = path.child(segment);
        }
        if !(prefix.is_empty() && name == ROOT_FILE) {
            path = path.child(stem);
        }
        let display = display(prefix, Some(name));
        let mut file = PathBuf::from(SOURCE_DIR);
        for segment in prefix {
            file.push(segment);
        }
        file.push(name);
        self.modules.push(ModuleFile {
            path,
            file,
            display,
        });
    }

    /// Reports E0037 unless `stem` is a valid module name.
    fn check_name(&mut self, prefix: &[String], name: &str, stem: &str, what: &str) -> bool {
        if is_valid_name(stem) {
            return true;
        }
        let shown = display(prefix, Some(name));
        let mut diag = Diagnostic::new(
            codes::INVALID_MODULE_NAME,
            format!("`{shown}` is not a valid module name"),
        )
        .with_note(format!(
            "the name of a {what} under `{SOURCE_DIR}/` becomes part of a module path, \
             so it must be a snake_case identifier that is not a keyword"
        ));
        if let Some(suggestion) = suggest_name(stem) {
            let renamed = if what == "file" {
                format!("{suggestion}.{EXTENSION}")
            } else {
                suggestion
            };
            diag = diag.with_help(format!(
                "rename it to `{}`",
                display(prefix, Some(&renamed))
            ));
        }
        self.diagnostics.push(diag);
        false
    }
}

/// `src/a/b/name` with `/` separators (D-421).
fn display(prefix: &[String], name: Option<&str>) -> String {
    let mut out = String::from(SOURCE_DIR);
    for segment in prefix.iter().map(String::as_str).chain(name) {
        out.push('/');
        out.push_str(segment);
    }
    out
}
