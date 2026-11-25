//! Dependencies (§13.3, §13.4): loading the packages that a package names by
//! directory (D-1856) or by git URL (D-1881), and mapping the first segment of an import.
//!
//! A dependency's modules keep the paths of the package they come from,
//! under its identity in place of its name: `geometry#0.1::shape` is the
//! module `shape` of major 0.1 of `geometry` (D-1857, D-1863). No name in a
//! source file holds `#`, so such a path never meets the root package's or
//! std's, and everything made from module paths, symbols, type keys and
//! cache keys alike, holds the identity. The separator is not the `@` of a
//! location (D-1858), which ELF linkers read in a symbol as a version.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use cheby_diag::{Diagnostic, FileId, SourceMap, Span, codes};
use cheby_iface::PackageId;
use cheby_package::{
    DependencySource, GitReference, GitUrl, Kind, Manifest, ModulePath, Override, Targets,
};
use cheby_pkg::{LockedPackage, Lockfile, PkgError, Store};

use crate::cache::Hash128;
use crate::{Error, Module};

/// A loaded dependency.
#[derive(Debug)]
pub struct DependencyPackage {
    /// Its name or canonical URL, and its major: the first segment of its
    /// module paths.
    pub identity: String,
    /// Its directory, with symbolic links resolved.
    pub root: PathBuf,
    /// The canonical URL of a git package, or of the one it overrides.
    pub url: Option<String>,
    /// Its name or URL and its exact version, as locations show it.
    pub shown: String,
    pub manifest: Manifest,
    pub modules: Vec<Module>,
    /// The absolute path of each module's file.
    pub files: Vec<(FileId, PathBuf)>,
    /// Its own dependencies: each key, and the identity it names.
    pub keys: BTreeMap<String, String>,
}

/// The dependencies of a build, each once, a package after those it
/// depends on.
#[derive(Debug, Default)]
pub struct Dependencies {
    pub packages: Vec<DependencyPackage>,
    /// The root package's dependencies: each key, and the identity it names.
    pub keys: BTreeMap<String, String>,
}

/// Whether this platform stops the package at `dir` because it has a git
/// dependency, as Windows does until M6 (D-1878): the tests that take every
/// package through the front end leave such a package out.
#[cfg(test)]
pub(crate) fn not_yet_here(dir: &Path) -> bool {
    cfg!(windows)
        && std::fs::read_to_string(dir.join(cheby_package::MANIFEST_FILE))
            .is_ok_and(|manifest| manifest.lines().any(|line| line.contains("{ git = ")))
}

/// What the first segment of an import means in each package of a build
/// (§7.5, D-238).
#[derive(Clone, Debug, Default)]
pub(crate) struct PackageNames {
    /// By [`PackageId`] from [`PackageId::LOCAL`]: the package's name, the
    /// first segment of its module paths, and its keys.
    packages: Vec<(String, String, BTreeMap<String, String>)>,
}

impl PackageNames {
    pub(crate) fn new(name: &str, dependencies: &Dependencies) -> Self {
        let mut packages = vec![(name.to_owned(), name.to_owned(), dependencies.keys.clone())];
        packages.extend(
            dependencies
                .packages
                .iter()
                .map(|d| (d.manifest.name.clone(), d.identity.clone(), d.keys.clone())),
        );
        Self { packages }
    }

    /// The package that `first` names in a module of `from`, and the first
    /// segment of its module paths. `std` is not among them.
    pub(crate) fn find(&self, from: PackageId, first: &str) -> Option<(PackageId, &str)> {
        let at = from.as_u32().checked_sub(PackageId::LOCAL.as_u32())?;
        let (name, own, keys) = self.packages.get(usize::try_from(at).ok()?)?;
        if first == name {
            return Some((from, own));
        }
        let identity = keys.get(first)?;
        let at = self.packages.iter().position(|(_, id, _)| id == identity)?;
        Some((package_id(at)?, identity))
    }
}

/// The id of the package at `at`, counted from the root package.
fn package_id(at: usize) -> Option<PackageId> {
    let at = u32::try_from(at).ok()?;
    Some(PackageId::from_u32(
        PackageId::LOCAL.as_u32().checked_add(at)?,
    ))
}

/// The id of the dependency at `index` of [`Dependencies::packages`].
pub(crate) fn dependency_id(index: usize) -> Option<PackageId> {
    package_id(index.checked_add(1)?)
}

/// The store of fetched packages (D-1877), in the cache directory of D-959.
///
/// # Errors
///
/// When the environment names no cache directory.
pub fn store() -> Result<Store, Error> {
    let root = crate::cache::cache_root(&crate::cache::CacheEnv::from_process())
        .map_err(|e| Error::new(e.to_string()))?;
    root.map(|root| Store::new(&root)).ok_or_else(|| {
        Error::new("git dependencies need a cache directory; set CHEBY_CACHE".to_owned())
    })
}

fn pkg_error(e: &PkgError) -> Error {
    Error::new(match e.help() {
        Some(help) => format!("{}; {help}", e.message()),
        None => e.message().to_owned(),
    })
}

/// Whether a module with this path and source hash is left unparsed, since
/// the package index vouches for it (D-962).
pub(crate) type Skip<'a> = dyn Fn(&ModulePath, Hash128) -> bool + Sync + 'a;

/// Whether the modules of dependencies have cache records (D-1896). The
/// hidden `CHEBY_DEP_CACHE=off` says they have none, so every command checks
/// and compiles them, as a check on the records (D-1898). It changes no
/// result.
pub(crate) fn are_cached() -> bool {
    std::env::var_os("CHEBY_DEP_CACHE").is_none_or(|value| value != "off")
}

/// Loads every package that `manifest`, of the package at `root`, depends
/// on, directly or not, for a build of `targets`. Errors about a dependency
/// are reported at its key, and errors in its files with its name and
/// version (D-1858); its warnings are dropped (D-1868). Nothing is
/// downloaded: a git dependency is the one `cheby.lock` names, from the
/// store (D-1881).
///
/// # Errors
///
/// An [`Error`] for failures that are not diagnostics, a checksum that does
/// not match included (D-1878).
pub(crate) fn load(
    root: &Path,
    manifest: &Manifest,
    targets: Targets,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
    skip: &Skip<'_>,
) -> Result<Dependencies, Error> {
    if manifest.dependencies.is_empty() {
        return Ok(Dependencies::default());
    }
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut loader = Loader {
        root,
        targets,
        overrides: &manifest.overrides,
        skip,
        lockfile: None,
        sources,
        diagnostics,
        loaded: Vec::new(),
        stack: vec![(canonical, manifest.name.clone())],
    };
    loader.check_overrides()?;
    let keys = loader.keys(root, manifest, false)?;
    Ok(Dependencies {
        packages: loader.loaded,
        keys,
    })
}

/// [`load`] for the language server, with the package's own targets: a
/// failure that is no diagnostic leaves the imports unresolved, and its
/// message goes to `tool`.
pub(crate) fn load_tolerant(
    root: &Path,
    manifest: &Manifest,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
    tool: &mut Vec<String>,
) -> Dependencies {
    let all = &|_: &ModulePath, _: Hash128| false;
    load(root, manifest, manifest.targets, sources, diagnostics, all).unwrap_or_else(|e| {
        tool.push(e.message().to_owned());
        Dependencies::default()
    })
}

/// Keeps of `diagnostics` what the user of `dependencies` sees: not the
/// warnings of their files (D-1868).
pub(crate) fn drop_warnings(files: &HashSet<FileId>, diagnostics: &mut Vec<Diagnostic>) {
    diagnostics.retain(|d| {
        d.is_error()
            || d.primary_span()
                .is_none_or(|span| !files.contains(&span.file))
    });
}

impl Dependencies {
    /// The files of every module.
    pub(crate) fn file_set(&self) -> HashSet<FileId> {
        self.packages
            .iter()
            .flat_map(|d| d.files.iter().map(|(file, _)| *file))
            .collect()
    }
}

struct Loader<'a> {
    /// The root package's directory, which holds `cheby.lock`.
    root: &'a Path,
    targets: Targets,
    /// The root package's overrides (D-1860).
    overrides: &'a [Override],
    skip: &'a Skip<'a>,
    /// The lockfile, once a git dependency needed it; empty without one.
    lockfile: Option<Lockfile>,
    sources: &'a mut SourceMap,
    diagnostics: &'a mut Vec<Diagnostic>,
    loaded: Vec<DependencyPackage>,
    /// The directories being loaded, the root package's first, with their
    /// packages' names (D-1864).
    stack: Vec<(PathBuf, String)>,
}

/// Where a package is loaded from.
struct Place {
    dir: PathBuf,
    /// The canonical URL of a git package or of the one a directory
    /// overrides; `None` for a path dependency.
    url: Option<String>,
    /// Whether it was fetched, so it may not have path dependencies
    /// (D-1859). An override may (D-1866).
    fetched: bool,
}

impl Loader<'_> {
    fn error(&mut self, message: String, at: Span, label: &str) {
        self.diagnostics
            .push(Diagnostic::new(codes::INVALID_MANIFEST, message).with_primary(at, label));
    }

    fn error_with_help(&mut self, message: String, at: Span, label: &str, help: &str) {
        self.diagnostics.push(
            Diagnostic::new(codes::INVALID_MANIFEST, message)
                .with_primary(at, label)
                .with_help(help),
        );
    }

    fn lockfile(&mut self) -> Result<&Lockfile, Error> {
        let lockfile = match self.lockfile.take() {
            Some(lockfile) => lockfile,
            None => Lockfile::load(self.root)
                .map_err(|e| pkg_error(&e))?
                .unwrap_or_default(),
        };
        Ok(self.lockfile.insert(lockfile))
    }

    /// The version of each override's directory is in a major of the package
    /// it replaces (D-1865).
    fn check_overrides(&mut self) -> Result<(), Error> {
        for entry in self.overrides {
            let dir = self.root.join(&entry.path);
            let Some((_, version)) = Manifest::peek_version(&dir) else {
                self.error(
                    format!("there is no package at `{}`", entry.path),
                    entry.span,
                    "this override has no usable `cheby.toml`",
                );
                continue;
            };
            let majors: Vec<String> = self
                .lockfile()?
                .packages
                .iter()
                .filter(|p| p.source.canonical() == entry.git.canonical())
                .map(LockedPackage::major)
                .collect();
            if !majors.is_empty() && !majors.contains(&version.major_part()) {
                let message = format!(
                    "the override of `{}` has version {version}, and the build uses major {}",
                    entry.git.canonical(),
                    majors.join(" and ")
                );
                self.error(message, entry.span, "");
            }
        }
        Ok(())
    }

    /// Loads the dependencies of `manifest`, of the package at `root`.
    fn keys(
        &mut self,
        root: &Path,
        manifest: &Manifest,
        fetched: bool,
    ) -> Result<BTreeMap<String, String>, Error> {
        let mut keys = BTreeMap::new();
        for dependency in &manifest.dependencies {
            let place = match &dependency.source {
                DependencySource::Path { .. } if fetched => {
                    self.error(
                        format!(
                            "`{}` is a path dependency of a package from git",
                            dependency.key
                        ),
                        dependency.span,
                        "a package from git depends only on packages from git",
                    );
                    continue;
                }
                DependencySource::Path { path, .. } => Place {
                    dir: std::fs::canonicalize(root.join(path))
                        .map_err(|e| Error::new(format!("cannot read `{path}`: {e}")))?,
                    url: None,
                    fetched: false,
                },
                DependencySource::Git { git, reference } => {
                    let Some(place) =
                        self.locked(&dependency.key, dependency.span, git, reference)?
                    else {
                        continue;
                    };
                    place
                }
            };
            if let Some(identity) = self.package(&place, &dependency.key, dependency.span)? {
                keys.insert(dependency.key.clone(), identity);
            }
        }
        Ok(keys)
    }

    /// Where the git dependency `key` is: the directory that overrides it, or
    /// what the lockfile names, in the store.
    fn locked(
        &mut self,
        key: &str,
        at: Span,
        git: &GitUrl,
        reference: &GitReference,
    ) -> Result<Option<Place>, Error> {
        let Some(locked) = self.lockfile()?.find(git, reference).cloned() else {
            self.error_with_help(
                format!("dependency `{key}` is not in `cheby.lock` as the manifest requires it"),
                at,
                "",
                "run `cheby update`",
            );
            return Ok(None);
        };
        let url = Some(git.canonical().to_owned());
        let replaced = self
            .overrides
            .iter()
            .find(|o| o.git.canonical() == git.canonical());
        if let Some(entry) = replaced
            && let dir = self.root.join(&entry.path)
            && Manifest::peek_version(&dir).is_some_and(|(_, v)| v.major_part() == locked.major())
        {
            return Ok(Some(Place {
                dir: std::fs::canonicalize(&dir)
                    .map_err(|e| Error::new(format!("cannot read `{}`: {e}", entry.path)))?,
                url,
                fetched: false,
            }));
        }
        if let Ok(dir) = store()?.open(&locked).map_err(|e| pkg_error(&e))? {
            return Ok(Some(Place {
                dir,
                url,
                fetched: true,
            }));
        }
        self.error_with_help(
            format!("`{}` is not downloaded", locked.shown()),
            at,
            "",
            "run `cheby fetch`",
        );
        Ok(None)
    }

    /// Loads the package at `place`, named by `key` at `at`, unless it is
    /// loaded already, and returns its identity.
    fn package(&mut self, place: &Place, key: &str, at: Span) -> Result<Option<String>, Error> {
        let dir = place.dir.as_path();
        if let Some(from) = self.stack.iter().position(|(d, _)| d == dir) {
            let cycle: Vec<&str> = self
                .stack
                .iter()
                .skip(from)
                .chain(self.stack.get(from))
                .map(|(_, name)| name.as_str())
                .collect();
            let message = format!("packages depend on each other: {}", cycle.join(" -> "));
            self.error(message, at, "this dependency closes the cycle");
            return Ok(None);
        }
        if let Some(found) = self
            .loaded
            .iter()
            .find(|d| d.root == dir && d.url == place.url)
        {
            return Ok(Some(found.identity.clone()));
        }
        let shown = match (Manifest::peek_version(dir), &place.url) {
            (Some((_, version)), Some(url)) => format!("{url}@{version}"),
            (Some((name, version)), None) => format!("{name}@{version}"),
            (None, Some(url)) => url.clone(),
            (None, None) => key.to_owned(),
        };
        let mut found = Vec::new();
        let manifest = Manifest::load_as(
            dir,
            &format!("{shown}/{}", cheby_package::MANIFEST_FILE),
            self.sources,
            &mut found,
        );
        let manifest = manifest.map_err(|e| Error::new(e.message().to_owned()))?;
        self.diagnostics
            .extend(found.into_iter().filter(Diagnostic::is_error));
        let Some(manifest) = manifest else {
            return Ok(None);
        };
        if manifest.kind != Kind::Library {
            let help = format!(
                "add `kind = \"library\"` to the `cheby.toml` of `{}`",
                manifest.name
            );
            let message = format!("dependency `{key}` is a program, not a library");
            self.error_with_help(message, at, "", &help);
            return Ok(None);
        }
        if let Some(target) = self.targets.iter().find(|&t| !manifest.targets.contains(t)) {
            let message = format!("dependency `{key}` does not target `{target}`");
            self.error(message, at, "");
            return Ok(None);
        }
        let base = place.url.as_deref().unwrap_or(&manifest.name);
        let identity = format!("{base}#{}", manifest.version.major_part());
        if self.loaded.iter().any(|d| d.identity == identity) {
            self.two_directories(&manifest, at);
            return Ok(None);
        }
        self.stack.push((dir.to_path_buf(), manifest.name.clone()));
        let keys = self.keys(dir, &manifest, place.fetched);
        self.stack.pop();
        let keys = keys?;
        let mut found = Vec::new();
        let mut files = cheby_package::discover(dir, &identity, &mut found)
            .map_err(|e| Error::new(format!("{shown}: {}", e.message())))?;
        for file in &mut files {
            file.display = format!("{shown}/{}", file.display);
        }
        let paths: Vec<PathBuf> = files.iter().map(|file| dir.join(&file.file)).collect();
        let modules =
            crate::frontend::read_modules(dir, files, self.sources, &mut found, self.skip)?;
        let files = modules.iter().map(|m| m.source.file).zip(paths).collect();
        self.diagnostics
            .extend(found.into_iter().filter(Diagnostic::is_error));
        // What it depends on may have loaded its identity from elsewhere.
        if self.loaded.iter().any(|d| d.identity == identity) {
            self.two_directories(&manifest, at);
            return Ok(None);
        }
        self.loaded.push(DependencyPackage {
            identity: identity.clone(),
            root: dir.to_path_buf(),
            url: place.url.clone(),
            shown,
            manifest,
            modules,
            files,
            keys,
        });
        Ok(Some(identity))
    }

    /// Two directories hold one identity (D-1857).
    fn two_directories(&mut self, manifest: &Manifest, at: Span) {
        let message = format!(
            "two directories hold major {} of package `{}`",
            manifest.version.major_part(),
            manifest.name
        );
        self.error_with_help(
            message,
            at,
            "this directory is not the one loaded before",
            "use one directory for the package in the whole build",
        );
    }
}
