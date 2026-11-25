//! The fixture repositories of the examples' git dependencies (D-1889).
//!
//! `tests/fixtures/cheby-sql` becomes a git repository with one commit,
//! tagged `v1.4.0`, in a directory of the test's own. Its author, dates and
//! settings are fixed, so the commit is always the one that the
//! `cheby.lock` of `docs/examples/030_shop_service` names. A git
//! configuration of its own maps `https://github.com/example/` to it
//! (`insteadOf`), so the manifest has a stable URL and nothing uses the
//! network or reads the developer's configuration.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cache::{CACHE_ENV, FreshDir};
use crate::repo_root;

/// What every `git` and `cheby` of a fixture runs with.
const FIXED_ENV: [(&str, &str); 6] = [
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z"),
    ("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z"),
    ("LC_ALL", "C"),
    ("TZ", "UTC"),
];

/// The variables that would point git elsewhere, as in a git hook.
const CLEARED: [&str; 10] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];

/// Makes `cmd`, a `git` or a `cheby`, use the repositories it names and the
/// git configuration `config`, and nothing of the developer's: a test may run
/// from a git hook, whose variables point git at their repository.
pub fn isolate(cmd: &mut Command, config: &Path) {
    for var in CLEARED {
        cmd.env_remove(var);
    }
    cmd.envs(FIXED_ENV).env("GIT_CONFIG_GLOBAL", config);
}

/// The fixture repositories, deleted when dropped.
#[derive(Debug)]
pub struct Repositories {
    dir: FreshDir,
}

impl Repositories {
    /// Makes the repositories in a fresh directory.
    ///
    /// # Errors
    ///
    /// A message when the fixture cannot be copied or git fails.
    pub fn create() -> Result<Self, String> {
        let repositories = Self {
            dir: FreshDir::create("cheby-git-fixture")?,
        };
        let config = format!(
            "[url \"file://{}/\"]\n\tinsteadOf = https://github.com/example/\n\
             [user]\n\tname = Cheby\n\temail = cheby@example.com\n\
             [core]\n\tautocrlf = false\n\tfileMode = true\n\
             [commit]\n\tgpgSign = false\n[tag]\n\tgpgSign = false\n\
             [init]\n\tdefaultBranch = main\n",
            repositories.dir.path().display()
        );
        write(&repositories.config(), config.as_bytes())?;
        let sql = repositories.dir.path().join("cheby-sql");
        copy(&repo_root().join("tests/fixtures/cheby-sql"), &sql)?;
        for args in [
            ["init", "--quiet"].as_slice(),
            &["add", "--all"],
            &["commit", "--quiet", "--message", "cheby-sql 1.4.0"],
            &["tag", "v1.4.0"],
        ] {
            let mut cmd = Command::new("git");
            cmd.args(args).current_dir(&sql);
            repositories.apply(&mut cmd);
            let out = cmd.output().map_err(|e| format!("cannot run `git`: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "git {args:?}: {}",
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
        }
        Ok(repositories)
    }

    fn config(&self) -> PathBuf {
        self.dir.path().join("gitconfig")
    }

    /// Makes `cmd`, a `git` or a `cheby`, see the repositories.
    pub fn apply(&self, cmd: &mut Command) {
        isolate(cmd, &self.config());
    }

    /// Runs `cheby <command>`, `fetch` or `update`, in `package`, into the
    /// store of the cache at `cache`.
    ///
    /// # Errors
    ///
    /// A message when it fails.
    pub fn cheby(
        &self,
        cheby: &Path,
        command: &str,
        package: &Path,
        cache: &Path,
    ) -> Result<(), String> {
        let mut cmd = Command::new(cheby);
        cmd.args(["--color=never", command])
            .current_dir(package)
            .env(CACHE_ENV, cache);
        self.apply(&mut cmd);
        let out = cmd
            .output()
            .map_err(|e| format!("cannot run {}: {e}", cheby.display()))?;
        if out.status.success() {
            return Ok(());
        }
        Err(format!(
            "`cheby {command}` failed; after an edit of tests/fixtures, `cargo xtask bless 030_shop_service` writes the example's cheby.lock again\n{}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Copies the files of `from`, without `build/`. A Windows checkout's line
/// ends become `\n`, as in the repository.
fn copy(from: &Path, to: &Path) -> Result<(), String> {
    let shown = |e: std::io::Error| format!("{}: {e}", from.display());
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    for entry in std::fs::read_dir(from).map_err(shown)? {
        let entry = entry.map_err(shown)?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        if source.is_dir() {
            if entry.file_name() != "build" {
                copy(&source, &target)?;
            }
        } else {
            let text = std::fs::read_to_string(&source)
                .map_err(|e| format!("{}: {e}", source.display()))?;
            write(&target, text.replace("\r\n", "\n").as_bytes())?;
        }
    }
    Ok(())
}
