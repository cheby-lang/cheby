//! What `cheby check`, `cheby run` and `cheby test` work on: the package
//! around the working directory, or a single file (D-1949).
//!
//! An argument that ends in `.cheby` names a file, and `-` the source on
//! standard input, which is kept in a file of its own while the command
//! runs and shown as `<stdin>`. A file under the `src/` of a package is a
//! module of that package and is refused, since alone its imports of the
//! package would not resolve.

use std::io::Read as _;
use std::path::PathBuf;

use cheby_driver::{Origin, SingleFile};

/// How standard input is shown in diagnostics, as `cheby fmt -` shows it.
const STDIN: &str = "<stdin>";

/// A package's root, or a single file.
pub(crate) enum Work {
    Package(PathBuf),
    File(Loaded),
}

impl Work {
    pub(crate) fn origin(&self) -> Origin<'_> {
        match self {
            Self::Package(root) => Origin::Package(root),
            Self::File(loaded) => Origin::File(&loaded.file),
        }
    }

    /// Whether the source came from standard input, so that no file of the
    /// user's holds it.
    pub(crate) const fn is_stdin(&self) -> bool {
        matches!(self, Self::File(Loaded { spool: Some(_), .. }))
    }
}

/// A single file, and the directory that holds it when it was read from
/// standard input.
pub(crate) struct Loaded {
    pub(crate) file: SingleFile,
    spool: Option<Spool>,
}

/// The directory of a source read from standard input, removed when the
/// command ends.
struct Spool(PathBuf);

impl Drop for Spool {
    fn drop(&mut self) {
        // Nothing can be done about a directory that stays.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Whether `arg` names a file where a module path or a filter may stand:
/// neither of those ends in `.cheby` or is `-`.
pub(crate) fn names_file(arg: &str) -> bool {
    arg == "-" || is_cheby(arg)
}

fn is_cheby(arg: &str) -> bool {
    std::path::Path::new(arg)
        .extension()
        .is_some_and(|extension| extension == cheby_driver::EXTENSION)
}

/// The single file that `arg` names for `cheby <command>`.
///
/// # Errors
///
/// The message of a tool error: for an argument that is no `.cheby` file,
/// a file that does not exist, a module of a package, or standard input
/// that cannot be read or that the command does not take.
pub(crate) fn load(arg: &str, command: &str) -> Result<Loaded, String> {
    if arg == "-" {
        if command == "test" {
            return Err(
                "`cheby test` takes no source from standard input; give it a `.cheby` file"
                    .to_owned(),
            );
        }
        return spool();
    }
    if !is_cheby(arg) {
        return Err(format!(
            "`{arg}` is not a `.cheby` file; `cheby {command}` takes one, or works on the \
             package without it"
        ));
    }
    let path = std::path::absolute(arg).map_err(|e| format!("{arg}: {e}"))?;
    if !path.is_file() {
        return Err(format!("there is no file `{arg}`"));
    }
    let display = arg.replace('\\', "/");
    let display = display.trim_start_matches("./").to_owned();
    let package = path.parent().and_then(cheby_driver::find_package_root);
    if let Some(root) = package.filter(|root| path.starts_with(root.join("src"))) {
        let name = cheby_driver::package_name(&root).unwrap_or_else(|| {
            root.file_name()
                .map_or_default(|name| name.to_string_lossy().into_owned())
        });
        return Err(format!(
            "`{display}` is a module of the package `{name}`, so it is not taken alone; run \
             `cheby {command}` in the package"
        ));
    }
    Ok(Loaded {
        file: SingleFile { path, display },
        spool: None,
    })
}

/// Standard input, written to a file in a directory of its own.
fn spool() -> Result<Loaded, String> {
    let mut source = Vec::new();
    std::io::stdin()
        .lock()
        .read_to_end(&mut source)
        .map_err(|e| format!("cannot read standard input: {e}"))?;
    let dir = std::env::temp_dir().join(format!("cheby-stdin-{}", std::process::id()));
    let failed = |e: std::io::Error| format!("{}: {e}", dir.display());
    // A directory an earlier process of this id left is not this one's.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(failed)?;
    let spool = Spool(dir.clone());
    let path = dir.join(format!("stdin.{}", cheby_driver::EXTENSION));
    std::fs::write(&path, source).map_err(failed)?;
    Ok(Loaded {
        file: SingleFile {
            path,
            display: STDIN.to_owned(),
        },
        spool: Some(spool),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_argument_ends_in_cheby_or_is_a_dash() {
        for arg in ["probe.cheby", "a/b/probe.cheby", "-", "./x.cheby"] {
            assert!(names_file(arg), "{arg}");
        }
        // A module path and a test filter never do.
        for arg in [
            "my_app::tools",
            "probe",
            "probe.txt",
            "cheby",
            "--",
            "adds up",
        ] {
            assert!(!names_file(arg), "{arg}");
        }
    }

    #[test]
    fn what_is_no_lone_file_is_refused() {
        let no_file = load("surely_not_here.cheby", "check").err().unwrap();
        assert_eq!(no_file, "there is no file `surely_not_here.cheby`");
        let not_cheby = load("notes.txt", "check").err().unwrap();
        assert!(
            not_cheby.starts_with("`notes.txt` is not a `.cheby` file"),
            "{not_cheby}"
        );
        let stdin = load("-", "test").err().unwrap();
        assert!(
            stdin.contains("takes no source from standard input"),
            "{stdin}"
        );
    }
}
