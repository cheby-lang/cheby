//! The `git` found on `PATH` (D-1876), over bare mirrors of repositories.
//!
//! A URL, a tag and a commit reach git only after `cheby_package` checked
//! them, and after `--` or `--end-of-options`. Nothing is checked out:
//! files are read from the object database, so no filter, attribute or hook
//! of a repository runs.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cheby_package::{GitReference, GitUrl, Version};

use crate::PkgError;
use crate::hash::File;

/// The variables that would point git at another repository.
const REPOSITORY_VARS: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

/// How the contents of a Git LFS pointer start.
const LFS: &[u8] = b"version https://git-lfs.github.com/spec/";

/// A bare mirror of a repository.
#[derive(Clone, Debug)]
pub struct Mirror {
    dir: PathBuf,
    url: GitUrl,
}

fn git() -> Command {
    let mut cmd = Command::new("git");
    for var in REPOSITORY_VARS {
        cmd.env_remove(var);
    }
    cmd.args([
        "-c",
        "protocol.ext.allow=never",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "gc.auto=0",
    ]);
    cmd.env("GIT_LFS_SKIP_SMUDGE", "1");
    cmd
}

fn spawn_error(e: &std::io::Error) -> PkgError {
    if e.kind() == std::io::ErrorKind::NotFound {
        PkgError::new("git dependencies need `git` on `PATH`")
    } else {
        PkgError::new(format!("cannot run `git`: {e}"))
    }
}

/// What git wrote to standard error, without control characters.
fn said(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .trim()
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect()
}

impl Mirror {
    /// The mirror of `url` in `dir`, created when there is none.
    ///
    /// # Errors
    ///
    /// When git fails.
    pub fn open(dir: &Path, url: &GitUrl) -> Result<Self, PkgError> {
        if !dir.join("HEAD").is_file() {
            let output = git()
                .args(["init", "--quiet", "--bare", "--template="])
                .arg(dir)
                .output()
                .map_err(|e| spawn_error(&e))?;
            if !output.status.success() {
                return Err(PkgError::new(format!(
                    "cannot create `{}`: {}",
                    dir.display(),
                    said(&output)
                )));
            }
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            url: url.clone(),
        })
    }

    fn command(&self) -> Command {
        let mut cmd = git();
        cmd.arg("--git-dir").arg(&self.dir);
        cmd
    }

    /// Runs git for what it prints; `None` when it exits with a failure.
    fn read(&self, args: &[&str]) -> Result<Option<Vec<u8>>, PkgError> {
        let output = self
            .command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| spawn_error(&e))?;
        Ok(output.status.success().then_some(output.stdout))
    }

    /// Downloads every branch and tag.
    ///
    /// # Errors
    ///
    /// When git fails, with what it said.
    pub fn fetch(&self) -> Result<(), PkgError> {
        let output = self
            .command()
            .args([
                "fetch",
                "--quiet",
                "--force",
                "--prune",
                "--no-tags",
                "--no-recurse-submodules",
                "--no-write-fetch-head",
                "--",
                self.url.source(),
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
            ])
            .output()
            .map_err(|e| spawn_error(&e))?;
        if output.status.success() {
            return Ok(());
        }
        Err(PkgError::new(format!(
            "cannot fetch `{}`: {}",
            self.url.canonical(),
            said(&output)
        )))
    }

    /// Asks the server for one commit by its hash (D-1887). A server may
    /// refuse, and one that deleted the commit has none, so a failure is no
    /// error here.
    ///
    /// # Errors
    ///
    /// When git cannot run.
    pub fn fetch_commit(&self, commit: &str) -> Result<(), PkgError> {
        self.command()
            .args([
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-recurse-submodules",
                "--no-write-fetch-head",
                "--",
                self.url.source(),
                commit,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| spawn_error(&e))?;
        Ok(())
    }

    /// The full hash of the commit `reference` names, if the mirror has it.
    ///
    /// # Errors
    ///
    /// When git cannot run.
    pub fn commit(&self, reference: &GitReference) -> Result<Option<String>, PkgError> {
        let tag = match reference {
            GitReference::Tag(tag) => tag,
            GitReference::Rev(rev) => return self.commit_that_starts(rev),
        };
        let found = self.read(&[
            "rev-parse",
            "--quiet",
            "--verify",
            "--end-of-options",
            &format!("refs/tags/{tag}^{{commit}}"),
        ])?;
        Ok(found
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
            .filter(|commit| crate::lock::is_commit(commit)))
    }

    /// The one commit whose hash starts with `rev`. A branch or a tag may be
    /// named like a hash, and git would take it first, so no name is looked
    /// up: git lists the objects.
    fn commit_that_starts(&self, rev: &str) -> Result<Option<String>, PkgError> {
        let prefix = rev.to_ascii_lowercase();
        let listed = self
            .read(&["rev-parse", &format!("--disambiguate={prefix}")])?
            .unwrap_or_default();
        let mut found = None;
        for object in String::from_utf8_lossy(&listed).lines() {
            if !crate::lock::is_commit(object) || !object.starts_with(&prefix) {
                continue;
            }
            let kind = self.read(&["cat-file", "-t", object])?;
            if kind.is_some_and(|kind| kind.trim_ascii() == b"commit")
                && found.replace(object.to_owned()).is_some()
            {
                return Ok(None);
            }
        }
        Ok(found)
    }

    /// The newest release among the tags: the tag whose name is a version
    /// without a pre-release, with or without `v` in front.
    ///
    /// # Errors
    ///
    /// When git cannot run.
    pub fn newest_release(&self) -> Result<Option<String>, PkgError> {
        let tags = self
            .read(&["for-each-ref", "--format=%(refname:strip=2)", "refs/tags"])?
            .unwrap_or_default();
        Ok(String::from_utf8_lossy(&tags)
            .lines()
            .filter_map(|tag| {
                let version = Version::parse(tag.strip_prefix('v').unwrap_or(tag))?;
                version.pre.is_none().then(|| (version, tag.to_owned()))
            })
            .max_by(|(a, x), (b, y)| a.cmp_precedence(b).then_with(|| x.cmp(y)))
            .map(|(_, tag)| tag))
    }

    /// The files of `commit`, which must hold only plain files (D-1879).
    ///
    /// # Errors
    ///
    /// When git fails, or for a file that a package may not have.
    pub(crate) fn files(&self, commit: &str) -> Result<Vec<File>, PkgError> {
        let listing = self
            .read(&[
                "ls-tree",
                "-r",
                "-z",
                "--full-tree",
                "--end-of-options",
                commit,
            ])?
            .ok_or_else(|| {
                PkgError::new(format!(
                    "`{}` has no commit `{commit}`",
                    self.url.canonical()
                ))
            })?;
        let mut files = Vec::new();
        let mut hashes = Vec::new();
        for entry in listing.split(|&b| b == 0).filter(|e| !e.is_empty()) {
            let entry = std::str::from_utf8(entry)
                .map_err(|_| self.refuse("has a file name that is not UTF-8"))?;
            let (head, path) = entry
                .split_once('\t')
                .ok_or_else(|| self.refuse("has a tree that git lists in an unknown form"))?;
            let mut head = head.split(' ');
            let (mode, hash) = (head.next().unwrap_or(""), head.nth(1).unwrap_or(""));
            let executable = match mode {
                "100644" | "100664" => false,
                "100755" => true,
                "120000" => return Err(self.refuse(&format!("has a symbolic link, `{path}`"))),
                "160000" => return Err(self.refuse(&format!("has a submodule, `{path}`"))),
                _ => return Err(self.refuse(&format!("has `{path}`, which is no file"))),
            };
            if !is_safe_path(path) {
                return Err(self.refuse(&format!(
                    "has a file whose name is not allowed, `{}`",
                    path.escape_default()
                )));
            }
            files.push(File {
                path: path.to_owned(),
                executable,
                bytes: Vec::new(),
            });
            hashes.push(hash.to_owned());
        }
        let contents = self.contents(&hashes)?;
        for (file, bytes) in files.iter_mut().zip(contents) {
            if bytes.starts_with(LFS) {
                return Err(self.refuse(&format!("keeps `{}` in Git LFS", file.path)));
            }
            file.bytes = bytes;
        }
        Ok(files)
    }

    /// The contents of the blobs `hashes`, in their order.
    fn contents(&self, hashes: &[String]) -> Result<Vec<Vec<u8>>, PkgError> {
        let broken = || PkgError::new(format!("cannot read `{}`", self.dir.display()));
        let mut child = self
            .command()
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| spawn_error(&e))?;
        let mut stdin = child.stdin.take().ok_or_else(broken)?;
        let mut stdout = child.stdout.take().ok_or_else(broken)?;
        let mut input = hashes.join("\n");
        input.push('\n');
        // Written from another thread: git answers while it still reads.
        let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
        let mut answer = Vec::new();
        let read = stdout.read_to_end(&mut answer);
        let status = child.wait();
        let wrote = writer.join();
        if read.is_err() || !status.is_ok_and(|s| s.success()) || !matches!(wrote, Ok(Ok(()))) {
            return Err(broken());
        }
        let mut rest = answer.as_slice();
        let mut out = Vec::with_capacity(hashes.len());
        for hash in hashes {
            let end = rest.iter().position(|&b| b == b'\n').ok_or_else(broken)?;
            let (head, after) = rest.split_at_checked(end).ok_or_else(broken)?;
            let head = String::from_utf8_lossy(head);
            let size = head
                .strip_prefix(&format!("{hash} blob "))
                .and_then(|n| n.parse::<usize>().ok())
                .ok_or_else(broken)?;
            let after = after.strip_prefix(b"\n").ok_or_else(broken)?;
            let (bytes, after) = after.split_at_checked(size).ok_or_else(broken)?;
            out.push(bytes.to_vec());
            rest = after.strip_prefix(b"\n").ok_or_else(broken)?;
        }
        Ok(out)
    }

    fn refuse(&self, problem: &str) -> PkgError {
        PkgError::new(format!("`{}` {problem}", self.url.canonical())).with_help(
            "a package from git holds only plain files, with no submodules, symbolic links or Git LFS",
        )
    }
}

/// Whether `path` stays inside its package on every platform.
fn is_safe_path(path: &str) -> bool {
    !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != ".." && !s.eq_ignore_ascii_case(".git"))
}

#[cfg(test)]
mod tests {
    use super::is_safe_path;

    #[test]
    fn safe_paths() {
        for path in ["cheby.toml", "src/a b.cheby", ".gitignore"] {
            assert!(is_safe_path(path), "{path}");
        }
        for path in [
            "",
            "/a",
            "a//b",
            "../a",
            "a/../b",
            "a/.",
            ".GIT/config",
            "a\\b",
            "c:a",
            "a\nb",
        ] {
            assert!(!is_safe_path(path), "{path}");
        }
    }
}
