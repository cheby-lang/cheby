//! `cheby add`, `cheby update` and `cheby fetch` (§13.4, D-1881, D-1882).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use cheby_diag::{Diagnostic, SourceMap, render_all};
use cheby_package::{
    DependencySource, GitReference, GitUrl, Kind, MANIFEST_FILE, Manifest, is_valid_name,
    is_valid_rev, is_valid_tag,
};

use crate::git::Mirror;
use crate::hash::File;
use crate::resolve::{Candidate, Requirement, Source, resolve};
use crate::{LOCK_FILE, LockedPackage, Lockfile, PkgError, Store, manifest_edit};

/// What `cheby add` was asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AddOptions {
    pub url: String,
    pub tag: Option<String>,
    pub rev: Option<String>,
    /// The key; the package's own name without it.
    pub name: Option<String>,
}

/// Git as the source of packages: each repository is fetched once, and each
/// commit is read once.
struct Fetcher<'a> {
    store: &'a Store,
    mirrors: BTreeMap<String, Mirror>,
    /// The files of the commits read so far, by URL and commit.
    trees: BTreeMap<(String, String), Rc<[File]>>,
    /// What `cheby add` keeps as it is locked (D-1895).
    pins: Pins,
}

/// The locked packages whose requirements are not resolved again.
#[derive(Default)]
struct Pins {
    lockfile: Lockfile,
    /// The requirement that is, whatever the lockfile says: the added one.
    fresh: Option<Requirement>,
}

impl Pins {
    /// The package that `requirement` is locked to.
    fn of(&self, requirement: &Requirement) -> Option<&LockedPackage> {
        let is_fresh = self.fresh.as_ref().is_some_and(|fresh| {
            fresh.url.canonical() == requirement.url.canonical()
                && fresh.reference == requirement.reference
        });
        if is_fresh {
            return None;
        }
        self.lockfile.find(&requirement.url, &requirement.reference)
    }
}

impl<'a> Fetcher<'a> {
    fn new(store: &'a Store) -> Self {
        Self {
            store,
            mirrors: BTreeMap::new(),
            trees: BTreeMap::new(),
            pins: Pins::default(),
        }
    }

    /// The candidate that `package` is, from the store, downloaded first when
    /// the store does not have it.
    fn pinned(&self, package: &LockedPackage) -> Result<Candidate, PkgError> {
        let dir = if let Ok(dir) = self.store.open(package)? {
            dir
        } else {
            download(package, self.store)?;
            self.store.tree(&package.source, &package.commit)?
        };
        let shown = package.shown();
        let mut sources = SourceMap::new();
        let mut diagnostics = Vec::new();
        let manifest = Manifest::load_as(
            &dir,
            &format!("{shown}/{MANIFEST_FILE}"),
            &mut sources,
            &mut diagnostics,
        )
        .map_err(|e| PkgError::new(e.message()))?;
        let manifest = usable(manifest, &mut sources, diagnostics)?;
        Ok(Candidate {
            commit: package.commit.clone(),
            version: package.version.clone(),
            requirements: git_requirements(&manifest),
        })
    }

    /// The mirror of `url`, fetched by this command.
    fn mirror(&mut self, url: &GitUrl) -> Result<Mirror, PkgError> {
        if let Some(mirror) = self.mirrors.get(url.canonical()) {
            return Ok(mirror.clone());
        }
        let mirror = Mirror::open(&self.store.mirror(url)?, url)?;
        mirror.fetch()?;
        self.mirrors
            .insert(url.canonical().to_owned(), mirror.clone());
        Ok(mirror)
    }

    fn files(
        &mut self,
        url: &GitUrl,
        reference: &GitReference,
    ) -> Result<(String, Rc<[File]>), PkgError> {
        let mirror = self.mirror(url)?;
        let commit = mirror.commit(reference)?.ok_or_else(|| {
            let (kind, name) = match reference {
                GitReference::Tag(tag) => ("tag", tag),
                GitReference::Rev(rev) => ("commit", rev),
            };
            PkgError::new(format!("`{}` has no {kind} `{name}`", url.canonical()))
        })?;
        let files = self.tree(url, &commit)?;
        Ok((commit, files))
    }

    /// The files of `commit`.
    fn tree(&mut self, url: &GitUrl, commit: &str) -> Result<Rc<[File]>, PkgError> {
        let key = (url.canonical().to_owned(), commit.to_owned());
        if let Some(files) = self.trees.get(&key) {
            return Ok(Rc::clone(files));
        }
        let files: Rc<[File]> = self.mirror(url)?.files(commit)?.into();
        self.trees.insert(key, Rc::clone(&files));
        Ok(files)
    }
}

/// The manifest among the files of a fetched package.
fn fetched_manifest(shown: &str, files: &[File]) -> Result<Manifest, PkgError> {
    let file = files
        .iter()
        .find(|f| f.path == MANIFEST_FILE)
        .ok_or_else(|| PkgError::new(format!("`{shown}` has no `{MANIFEST_FILE}` at its root")))?;
    let text = String::from_utf8_lossy(&file.bytes).into_owned();
    let mut sources = SourceMap::new();
    let mut diagnostics = Vec::new();
    let id = sources
        .add_file(format!("{shown}/{MANIFEST_FILE}"), text.clone())
        .map_err(|_| PkgError::new(format!("the manifest of `{shown}` is too large")))?;
    let manifest = Manifest::parse(id, &text, &mut diagnostics);
    let manifest = usable(manifest, &mut sources, diagnostics)?;
    if manifest
        .dependencies
        .iter()
        .any(|d| matches!(d.source, DependencySource::Path { .. }))
    {
        return Err(PkgError::new(format!(
            "`{shown}` has a path dependency, which a package from git may not have"
        )));
    }
    if manifest.kind != Kind::Library {
        return Err(PkgError::new(format!(
            "`{shown}` is a program, not a library"
        )));
    }
    Ok(manifest)
}

/// A manifest without errors.
fn usable(
    manifest: Option<Manifest>,
    sources: &mut SourceMap,
    mut diagnostics: Vec<Diagnostic>,
) -> Result<Manifest, PkgError> {
    diagnostics.retain(Diagnostic::is_error);
    sources.sort(&mut diagnostics);
    match manifest {
        Some(manifest) if diagnostics.is_empty() => Ok(manifest),
        _ => Err(PkgError::new(format!(
            "a manifest has errors\n{}",
            render_all(&diagnostics, sources).trim_end()
        ))),
    }
}

fn git_requirements(manifest: &Manifest) -> Vec<Requirement> {
    manifest
        .dependencies
        .iter()
        .filter_map(|d| match &d.source {
            DependencySource::Git { git, reference } => Some(Requirement {
                url: git.clone(),
                reference: reference.clone(),
            }),
            DependencySource::Path { .. } => None,
        })
        .collect()
}

impl Source for Fetcher<'_> {
    fn candidate(&mut self, requirement: &Requirement) -> Result<Candidate, PkgError> {
        if let Some(package) = self.pins.of(requirement) {
            return self.pinned(package);
        }
        let (commit, files) = self.files(&requirement.url, &requirement.reference)?;
        let shown = format!(
            "{}@{}",
            requirement.url.canonical(),
            match &requirement.reference {
                GitReference::Tag(name) | GitReference::Rev(name) => name,
            }
        );
        let manifest = fetched_manifest(&shown, &files)?;
        Ok(Candidate {
            commit,
            version: manifest.version.clone(),
            requirements: git_requirements(&manifest),
        })
    }
}

/// What the local manifests of a build require of git: the root package's,
/// those of its path dependencies and of its overrides (D-1865), and what
/// these lead to. The root package's come first, and their number is the
/// second value.
fn local_requirements(
    root: &Path,
    text: Option<&str>,
) -> Result<(Vec<Requirement>, usize), PkgError> {
    let mut out = Vec::new();
    let mut of_root = 0;
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut queue: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut is_root = true;
    while let Some(dir) = queue.pop() {
        let dir = std::fs::canonicalize(&dir).map_err(|e| PkgError::io("read", &dir, &e))?;
        if !seen.insert(dir.clone()) {
            continue;
        }
        let mut sources = SourceMap::new();
        let mut diagnostics = Vec::new();
        let shown = if is_root {
            MANIFEST_FILE.to_owned()
        } else {
            dir.join(MANIFEST_FILE).display().to_string()
        };
        let manifest = match text.filter(|_| is_root) {
            Some(text) => {
                let id = sources
                    .add_file(shown, text)
                    .map_err(|_| PkgError::new("`cheby.toml` is too large"))?;
                Manifest::parse_at(id, text, &dir, &mut diagnostics)
            }
            None => Manifest::load_as(&dir, &shown, &mut sources, &mut diagnostics)
                .map_err(|e| PkgError::new(e.message()))?,
        };
        let manifest = usable(manifest, &mut sources, diagnostics)?;
        out.extend(git_requirements(&manifest));
        for dependency in &manifest.dependencies {
            if let DependencySource::Path { path, .. } = &dependency.source {
                queue.push(dir.join(path));
            }
        }
        if is_root {
            of_root = out.len();
            queue.extend(manifest.overrides.iter().map(|o| dir.join(&o.path)));
        }
        is_root = false;
    }
    Ok((out, of_root))
}

/// Resolves and downloads the packages of the build at `root`, whose manifest
/// has `text` when it is given. Returns the lockfile, not yet written.
fn lock(root: &Path, text: Option<&str>, fetcher: &mut Fetcher<'_>) -> Result<Lockfile, PkgError> {
    let (roots, of_root) = local_requirements(root, text)?;
    let mut packages = Vec::new();
    for selected in resolve(&roots, of_root, fetcher)? {
        let pin = fetcher.pins.lockfile.packages.iter().find(|p| {
            p.source.canonical() == selected.url.canonical() && p.commit == selected.commit
        });
        let checksum = match pin {
            Some(pin) if fetcher.store.open(pin)?.is_ok() => pin.checksum.clone(),
            _ => {
                let files = fetcher.tree(&selected.url, &selected.commit)?;
                fetcher.store.put(&selected.url, &selected.commit, &files)?
            }
        };
        packages.push(LockedPackage {
            source: selected.url,
            version: selected.version,
            commit: selected.commit,
            checksum,
            required: selected.required,
        });
    }
    Ok(Lockfile { packages })
}

/// Replaces `path` as a whole.
fn write(path: &Path, text: &str) -> Result<(), PkgError> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let staged = path.with_file_name(name);
    let written = std::fs::write(&staged, text).and_then(|()| std::fs::rename(&staged, path));
    written.map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        PkgError::io("write", path, &e)
    })
}

/// Writes the lockfile, unless there is nothing to lock and none to replace.
fn write_lock(root: &Path, lockfile: &Lockfile) -> Result<(), PkgError> {
    let path = root.join(LOCK_FILE);
    if lockfile.packages.is_empty() && !path.exists() {
        return Ok(());
    }
    write(&path, &lockfile.render())
}

/// What changed between two lockfiles, a line for each package.
fn changes(old: &Lockfile, new: &Lockfile) -> Vec<String> {
    let find = |lock: &Lockfile, package: &LockedPackage| {
        lock.packages
            .iter()
            .find(|p| {
                p.source.canonical() == package.source.canonical() && p.major() == package.major()
            })
            .cloned()
    };
    let mut out = Vec::new();
    for package in &new.packages {
        match find(old, package) {
            None => out.push(format!("added {}", package.shown())),
            Some(was) if was.version != package.version => {
                out.push(format!("changed {} from {}", package.shown(), was.version));
            }
            Some(was) if was.commit != package.commit => out.push(format!(
                "changed {}, whose commit moved from {} to {}",
                package.shown(),
                was.commit,
                package.commit
            )),
            Some(_) => {}
        }
    }
    for package in &old.packages {
        if find(new, package).is_none() {
            out.push(format!("removed {}", package.shown()));
        }
    }
    out
}

/// `cheby update`: resolves every tag again and writes the lockfile.
///
/// # Errors
///
/// When a package cannot be fetched, or requirements conflict.
pub fn update(root: &Path, store: &Store) -> Result<Vec<String>, PkgError> {
    let _held = store.lock()?;
    // A lockfile that cannot be read is replaced.
    let old = Lockfile::load(root).ok().flatten().unwrap_or_default();
    let new = lock(root, None, &mut Fetcher::new(store))?;
    write_lock(root, &new)?;
    Ok(changes(&old, &new))
}

/// Downloads a locked package into the store, whatever its tags say now.
fn download(package: &LockedPackage, store: &Store) -> Result<(), PkgError> {
    let mirror = Mirror::open(&store.mirror(&package.source)?, &package.source)?;
    let commit = GitReference::Rev(package.commit.clone());
    if mirror.commit(&commit)?.is_none() {
        mirror.fetch()?;
    }
    // A tag that moved may leave its old commit on no branch (D-1887).
    if mirror.commit(&commit)?.is_none() {
        mirror.fetch_commit(&package.commit)?;
    }
    if mirror.commit(&commit)?.is_none() {
        return Err(PkgError::new(format!(
            "`{}` no longer has the commit `{}` of `{}`, which `{LOCK_FILE}` names",
            package.source.canonical(),
            package.commit,
            package.shown()
        ))
        .with_help("its tag may have moved; `cheby update` locks what the tags name now"));
    }
    let files = mirror.files(&package.commit)?;
    if crate::hash::checksum(&files)? != package.checksum {
        return Err(PkgError::new(format!(
            "the files of `{}` do not match the checksum in `{LOCK_FILE}`",
            package.shown()
        )));
    }
    store.put(&package.source, &package.commit, &files)?;
    Ok(())
}

/// `cheby fetch`: downloads the packages of the lockfile that the store does
/// not have.
///
/// # Errors
///
/// When a package cannot be fetched or is not the one that was locked.
pub fn fetch(root: &Path, store: &Store) -> Result<Vec<String>, PkgError> {
    let Some(lockfile) = Lockfile::load(root)? else {
        return Ok(Vec::new());
    };
    let _held = store.lock()?;
    let mut fetched = Vec::new();
    for package in &lockfile.packages {
        if store.open(package)?.is_ok() {
            continue;
        }
        download(package, store)?;
        fetched.push(format!("fetched {}", package.shown()));
    }
    Ok(fetched)
}

/// `cheby add`: adds a dependency to the manifest and the lockfile. Only the
/// added package is looked up: what is locked already stays, unless the added
/// package needs a higher version of it (D-1895). Nothing is written when the
/// build cannot be resolved.
///
/// # Errors
///
/// As [`update`], and when the manifest cannot take the dependency.
pub fn add(root: &Path, store: &Store, options: &AddOptions) -> Result<Vec<String>, PkgError> {
    let url = GitUrl::parse(&options.url).map_err(|problem| {
        PkgError::new(format!("the git URL {problem}"))
            .with_help("write `https://host/path`, `ssh://git@host/path` or `git@host:path`")
    })?;
    let _held = store.lock()?;
    let mut fetcher = Fetcher::new(store);
    let reference = match (&options.tag, &options.rev) {
        (Some(tag), None) if is_valid_tag(tag) => GitReference::Tag(tag.clone()),
        (Some(tag), None) => {
            return Err(PkgError::new(format!(
                "`{tag}` is not the name of a git tag"
            )));
        }
        (None, Some(rev)) if is_valid_rev(rev) => GitReference::Rev(rev.clone()),
        (None, Some(rev)) => {
            return Err(PkgError::new(format!(
                "`{rev}` is not a commit hash of at least seven digits"
            )));
        }
        (Some(_), Some(_)) => return Err(PkgError::new("give `--tag` or `--rev`, not both")),
        (None, None) => {
            let newest = fetcher.mirror(&url)?.newest_release()?;
            GitReference::Tag(newest.ok_or_else(|| {
                PkgError::new(format!("`{}` has no tag of a release", url.canonical()))
                    .with_help("choose a commit with `--rev` or a tag with `--tag`")
            })?)
        }
    };
    let (_, files) = fetcher.files(&url, &reference)?;
    let manifest = fetched_manifest(url.canonical(), &files)?;
    let key = options.name.clone().unwrap_or(manifest.name);
    if !is_valid_name(&key) || key == "std" {
        return Err(PkgError::new(format!(
            "`{key}` is not a valid dependency key"
        )));
    }
    let path = root.join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| PkgError::new(format!("{MANIFEST_FILE}: {e}")))?;
    let text = manifest_edit::add(&text, &key, &url, &reference)?;
    // A lockfile that cannot be read is an error: its pins would be lost.
    let old = Lockfile::load(root)?.unwrap_or_default();
    fetcher.pins = Pins {
        lockfile: old.clone(),
        fresh: Some(Requirement { url, reference }),
    };
    let new = lock(root, Some(&text), &mut fetcher)?;
    write_lock(root, &new)?;
    write(&path, &text)?;
    Ok(changes(&old, &new))
}
