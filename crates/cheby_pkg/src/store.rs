//! The machine's store of fetched packages (D-1877), beside the build
//! cache's entries and never trimmed:
//!
//! - `git/<hash of the canonical URL>`: a bare mirror;
//! - `src/<hash of the canonical URL>/<commit>`: the files of a commit,
//!   read-only, moved into place as a whole.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cheby_package::GitUrl;

use crate::hash::{self, File};
use crate::{LockedPackage, PkgError};

/// The store's directory in the cache's root.
pub const STORE_DIR: &str = "pkg";

/// How deep a package's directories may nest.
const MAX_DEPTH: usize = 128;

/// The store at a directory, which need not exist yet.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

/// Held while a command changes the store.
#[derive(Debug)]
pub struct StoreLock(#[expect(dead_code, reason = "unlocked when dropped")] fs::File);

impl Store {
    /// The store in `cache_root`, the directory of D-959.
    #[must_use]
    pub fn new(cache_root: &Path) -> Self {
        Self {
            root: cache_root.join(STORE_DIR).join("v1"),
        }
    }

    /// Locks the store against other commands that change it.
    ///
    /// # Errors
    ///
    /// When the store cannot be written.
    pub fn lock(&self) -> Result<StoreLock, PkgError> {
        create_private(&self.root)?;
        let path = self.root.join("lock");
        let file = fs::File::create(&path).map_err(|e| PkgError::io("write", &path, &e))?;
        file.lock().map_err(|e| PkgError::io("lock", &path, &e))?;
        Ok(StoreLock(file))
    }

    /// The directory of the mirror of `url`.
    ///
    /// # Errors
    ///
    /// On Windows, until M6.
    pub fn mirror(&self, url: &GitUrl) -> Result<PathBuf, PkgError> {
        let name = hash::sha256(&[url.canonical().as_bytes()])?;
        let dir = self.root.join("git");
        create_private(&dir)?;
        Ok(dir.join(name))
    }

    /// The directory of the files of `commit`, whether or not it exists.
    ///
    /// # Errors
    ///
    /// On Windows, until M6.
    pub fn tree(&self, url: &GitUrl, commit: &str) -> Result<PathBuf, PkgError> {
        let name = hash::sha256(&[url.canonical().as_bytes()])?;
        Ok(self.root.join("src").join(name).join(commit))
    }

    /// Stores `files` as the files of `commit` and returns their checksum.
    /// Files that are stored already are replaced when they differ.
    pub(crate) fn put(
        &self,
        url: &GitUrl,
        commit: &str,
        files: &[File],
    ) -> Result<String, PkgError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let checksum = hash::checksum(files)?;
        let tree = self.tree(url, commit)?;
        if tree.is_dir() {
            if read_tree(&tree).and_then(|f| hash::checksum(&f)).as_deref() == Ok(&checksum) {
                return Ok(checksum);
            }
            fs::remove_dir_all(&tree).map_err(|e| PkgError::io("replace", &tree, &e))?;
        }
        let parent = tree.parent().unwrap_or(&self.root);
        create_private(parent)?;
        let staged = parent.join(format!(
            ".{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let written = write_tree(&staged, files).and_then(|()| {
            fs::rename(&staged, &tree).map_err(|e| PkgError::io("write", &tree, &e))
        });
        if written.is_err() {
            let _ = fs::remove_dir_all(&staged);
        }
        written.map(|()| checksum)
    }

    /// The directory of a locked package whose files match its checksum
    /// (D-1878).
    ///
    /// # Errors
    ///
    /// [`Missing`] when it is not downloaded, or an error when the files are
    /// not those of the lockfile.
    pub fn open(&self, package: &LockedPackage) -> Result<Result<PathBuf, Missing>, PkgError> {
        let tree = self.tree(&package.source, &package.commit)?;
        if !tree.is_dir() {
            return Ok(Err(Missing));
        }
        let found = hash::checksum(&read_tree(&tree)?)?;
        if found == package.checksum {
            return Ok(Ok(tree));
        }
        Err(PkgError::new(format!(
            "the files of `{}` do not match the checksum in `cheby.lock`",
            package.shown()
        ))
        .with_help(format!(
            "if the lockfile is right, delete `{}` and run `cheby fetch`",
            tree.display()
        )))
    }
}

/// A locked package is not downloaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Missing;

fn create_private(dir: &Path) -> Result<(), PkgError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|e| PkgError::io("create", dir, &e))
}

/// Writes `files`, read-only, into the new directory `root`.
fn write_tree(root: &Path, files: &[File]) -> Result<(), PkgError> {
    use std::io::Write as _;
    create_private(root)?;
    for file in files {
        let path = root.join(&file.path);
        if let Some(dir) = path.parent() {
            create_private(dir)?;
        }
        // Two names that the file system takes for one are an error here.
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| PkgError::io("write", &path, &e))?;
        out.write_all(&file.bytes)
            .map_err(|e| PkgError::io("write", &path, &e))?;
        set_mode(&out, file.executable).map_err(|e| PkgError::io("write", &path, &e))?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(file: &fs::File, executable: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = if executable { 0o500 } else { 0o400 };
    file.set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the Unix version can fail, and the caller handles both alike"
)]
fn set_mode(_file: &fs::File, _executable: bool) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o100 != 0
}

#[cfg(not(unix))]
const fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

/// Every file under `root`. A link is an error: the store writes none.
fn read_tree(root: &Path) -> Result<Vec<File>, PkgError> {
    let mut files = Vec::new();
    read_dir(root, "", 0, &mut files)?;
    Ok(files)
}

fn read_dir(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<File>) -> Result<(), PkgError> {
    let strange = || {
        PkgError::new(format!(
            "`{}` holds what no package has; delete it and run `cheby fetch`",
            dir.display()
        ))
    };
    if depth > MAX_DEPTH {
        return Err(strange());
    }
    for entry in fs::read_dir(dir).map_err(|e| PkgError::io("read", dir, &e))? {
        let entry = entry.map_err(|e| PkgError::io("read", dir, &e))?;
        let path = entry.path();
        let name = entry.file_name().into_string().map_err(|_| strange())?;
        let relative = format!("{prefix}{name}");
        let metadata = fs::symlink_metadata(&path).map_err(|e| PkgError::io("read", &path, &e))?;
        if metadata.is_dir() {
            read_dir(&path, &format!("{relative}/"), depth.saturating_add(1), out)?;
        } else if metadata.is_file() {
            out.push(File {
                path: relative,
                executable: is_executable(&metadata),
                bytes: fs::read(&path).map_err(|e| PkgError::io("read", &path, &e))?,
            });
        } else {
            return Err(strange());
        }
    }
    Ok(())
}
