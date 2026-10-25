//! `cheby new <name>` (D-1032, spec §13.1): creates the directory `<name>`
//! in the working directory with the smallest package that runs, in the
//! form `cheby fmt` produces at every width CP2 may choose (D-1150).
//!
//! D-1214: the files' texts, the silent success and the
//! error texts are the user's answer to the wave plan's §7 Q6 (1).

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The `.gitignore` of a new package: the build's output directory.
pub(crate) const GITIGNORE: &str = "build/\n";

/// The package name the standard library reserves (spec §7.1, D-058), which
/// `cheby_package` refuses in a manifest although it is a `LOWER` name.
const RESERVED_NAME: &str = "std";

/// The `cheby.toml` of a new package `name` (D-097).
pub(crate) fn manifest(name: &str) -> String {
    format!("name = \"{name}\"\nversion = \"0.1.0\"\n")
}

/// The `src/main.cheby` of a new package `name`: a `//!` comment and a
/// `main` that prints 001's greeting.
pub(crate) fn main_source(name: &str) -> String {
    format!(
        "//! The {name} program.\n\nimport std::io\n\nfn main() {{\n  io::println(\"Hello, world!\")\n}}\n"
    )
}

/// Why `cheby new` failed. Each is a tool error without a code, status 1
/// (D-464).
#[derive(Debug)]
pub(crate) enum NewError {
    /// `name` is not a valid package name; `suggestion` is a valid one
    /// close to it, when `cheby_package` has one.
    InvalidName {
        name: String,
        suggestion: Option<String>,
    },
    /// `name` is a valid name, but the one the standard library reserves.
    ReservedName { name: String },
    /// A file, a link or a directory `name` exists already; it is not
    /// touched.
    Exists { name: String },
    /// `path` (as the user sees it, with `/`) could not be created or
    /// written. Everything the command created before is removed.
    Create { path: String, error: std::io::Error },
}

impl NewError {
    /// The `error:` line's message.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::InvalidName { name, .. } => format!(
                "`{name}` is not a valid package name: package names are lower-case identifiers, \
                 such as `my_app`"
            ),
            // D-1215: the answer's form, with the
            // manifest's reason (D-058).
            Self::ReservedName { name } => format!(
                "`{name}` is not a valid package name: it is reserved for the standard library"
            ),
            Self::Exists { name } => format!("`{name}` already exists"),
            Self::Create { path, error } => format!("cannot create `{path}`: {error}"),
        }
    }

    /// The `help:` line that follows the error, if any.
    pub(crate) fn help(&self) -> Option<String> {
        match self {
            Self::InvalidName {
                suggestion: Some(suggestion),
                ..
            } => Some(format!("try `{suggestion}`")),
            _ => None,
        }
    }
}

/// Creates the package `name` under `base` (the working directory when
/// `base` is empty) with exactly `cheby.toml`, `src/main.cheby` and
/// `.gitignore`. It runs no `git init` and prints nothing. An invalid name
/// or an existing path writes nothing.
pub(crate) fn new_package(base: &Path, name: &str) -> Result<(), NewError> {
    new_package_with(base, name, &mut write_new_file)
}

/// [`new_package`], with the function that writes each file, so that a
/// test can fail a write part-way.
fn new_package_with(
    base: &Path,
    name: &str,
    write: &mut dyn FnMut(&Path, &str) -> std::io::Result<()>,
) -> Result<(), NewError> {
    if !cheby_driver::is_valid_name(name) {
        return Err(NewError::InvalidName {
            name: name.to_owned(),
            suggestion: cheby_driver::suggest_name(name),
        });
    }
    if name == RESERVED_NAME {
        return Err(NewError::ReservedName {
            name: name.to_owned(),
        });
    }
    let dir = base.join(name);
    let create_error = |path: String, error: std::io::Error| NewError::Create { path, error };
    // A dangling link exists too: `symlink_metadata` does not follow it.
    match std::fs::symlink_metadata(&dir) {
        Ok(_) => {
            return Err(NewError::Exists {
                name: name.to_owned(),
            });
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(create_error(name.to_owned(), e)),
    }
    // `create_dir`, not `create_dir_all`: a path made in the meantime is
    // never written into.
    if let Err(e) = std::fs::create_dir(&dir) {
        return Err(if e.kind() == std::io::ErrorKind::AlreadyExists {
            NewError::Exists {
                name: name.to_owned(),
            }
        } else {
            create_error(name.to_owned(), e)
        });
    }
    let mut created = vec![Created::Dir(dir.clone())];
    let filled = fill(&dir, name, write, &mut created);
    if filled.is_err() {
        // Remove what this command created, the newest first. Anything
        // else found in the directory stays, and so does the directory
        // that holds it. A removal that fails cannot be reported better
        // than the error that caused it.
        for entry in created.iter().rev() {
            let _ = match entry {
                Created::Dir(path) => std::fs::remove_dir(path),
                Created::File(path) => std::fs::remove_file(path),
            };
        }
    }
    filled
}

/// A path `cheby new` created, removed again when a later step fails.
enum Created {
    Dir(PathBuf),
    File(PathBuf),
}

/// Writes the package's files into the new directory `dir`, recording each
/// path it creates in `created`.
fn fill(
    dir: &Path,
    name: &str,
    write: &mut dyn FnMut(&Path, &str) -> std::io::Result<()>,
    created: &mut Vec<Created>,
) -> Result<(), NewError> {
    let shown = |rel: &str| format!("{name}/{rel}");
    let src = dir.join("src");
    std::fs::create_dir(&src).map_err(|error| NewError::Create {
        path: shown("src"),
        error,
    })?;
    created.push(Created::Dir(src));
    let files = [
        ("cheby.toml", manifest(name)),
        ("src/main.cheby", main_source(name)),
        (".gitignore", GITIGNORE.to_owned()),
    ];
    for (rel, text) in files {
        let path = rel
            .split('/')
            .fold(dir.to_path_buf(), |p, part| p.join(part));
        let written = write(&path, &text);
        // A file that was opened is removed even when its write failed;
        // one that another process made first (`create_new` refused it)
        // is not this command's.
        let theirs = matches!(&written, Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists);
        if !theirs && std::fs::symlink_metadata(&path).is_ok() {
            created.push(Created::File(path));
        }
        written.map_err(|error| NewError::Create {
            path: shown(rel),
            error,
        })?;
    }
    Ok(())
}

/// Writes `text` to the new file `path`, which must not exist.
fn write_new_file(path: &Path, text: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    file.flush()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use cheby_test_support::cache::FreshDir;

    use super::{GITIGNORE, NewError, main_source, manifest, new_package, new_package_with};

    /// The widths CP2 chooses from (D-1150).
    const WIDTHS: [usize; 3] = [80, 100, 120];

    /// The skeleton is a fixed point of the formatter at every candidate
    /// width, for short and long names (D-1032, D-1150).
    #[test]
    fn cheby_new_skeleton_is_a_fmt_fixed_point() {
        let long = "a".repeat(130);
        for name in ["app", "my_app", "x1", long.as_str()] {
            let source = main_source(name);
            for width in WIDTHS {
                let formatted = cheby_fmt::format_source(&source, width).unwrap();
                assert_eq!(formatted, source, "{name} at {width}");
            }
        }
    }

    /// The texts of the user's answer (§7 Q6 (1)).
    #[test]
    fn cheby_new_skeleton_texts() {
        assert_eq!(
            manifest("my_app"),
            "name = \"my_app\"\nversion = \"0.1.0\"\n"
        );
        assert_eq!(
            main_source("my_app"),
            "//! The my_app program.\n\nimport std::io\n\nfn main() {\n  io::println(\"Hello, world!\")\n}\n"
        );
        assert_eq!(GITIGNORE, "build/\n");
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn cheby_new_writes_exactly_three_files() {
        let base = FreshDir::create("cheby-new").unwrap();
        new_package(base.path(), "my_app").unwrap();
        let dir = base.path().join("my_app");
        assert_eq!(names_in(&dir), [".gitignore", "cheby.toml", "src"]);
        assert_eq!(names_in(&dir.join("src")), ["main.cheby"]);
        let read = |rel: &str| std::fs::read_to_string(dir.join(rel)).unwrap();
        assert_eq!(read("cheby.toml"), manifest("my_app"));
        assert_eq!(read("src/main.cheby"), main_source("my_app"));
        assert_eq!(read(".gitignore"), GITIGNORE);
    }

    /// The error texts and help lines of the answer; nothing is written.
    #[test]
    fn cheby_new_name_errors() {
        let base = FreshDir::create("cheby-new").unwrap();
        let err = new_package(base.path(), "MyApp").unwrap_err();
        assert_eq!(
            err.message(),
            "`MyApp` is not a valid package name: package names are lower-case identifiers, \
             such as `my_app`"
        );
        assert_eq!(err.help().as_deref(), Some("try `my_app`"));
        let err = new_package(base.path(), "---").unwrap_err();
        assert!(matches!(err, NewError::InvalidName { .. }));
        assert_eq!(err.help(), None);
        let err = new_package(base.path(), "type").unwrap_err();
        assert_eq!(err.help().as_deref(), Some("try `type_`"));
        let err = new_package(base.path(), "std").unwrap_err();
        assert_eq!(
            err.message(),
            "`std` is not a valid package name: it is reserved for the standard library"
        );
        assert_eq!(err.help(), None);
        assert_eq!(names_in(base.path()), Vec::<String>::new());
    }

    /// A write that fails after the directory exists removes what the
    /// command created and reports the write's error.
    #[test]
    fn cheby_new_failed_write_removes_what_it_created() {
        let base = FreshDir::create("cheby-new").unwrap();
        let mut calls = 0;
        let mut write = |path: &Path, text: &str| {
            calls += 1;
            if calls == 2 {
                // The file is created, then the write fails.
                std::fs::write(path, "")?;
                return Err(std::io::Error::other("disk full"));
            }
            std::fs::write(path, text)
        };
        let err = new_package_with(base.path(), "my_app", &mut write).unwrap_err();
        assert_eq!(
            err.message(),
            "cannot create `my_app/src/main.cheby`: disk full"
        );
        assert_eq!(err.help(), None);
        assert_eq!(names_in(base.path()), Vec::<String>::new());
    }

    /// The removal leaves what the command did not create.
    #[test]
    fn cheby_new_failed_write_keeps_other_files() {
        let base = FreshDir::create("cheby-new").unwrap();
        let dir = base.path().join("my_app");
        let mut write = |path: &Path, _: &str| {
            std::fs::write(dir.join("other"), "")?;
            Err(std::io::Error::other(format!("no {}", path.display())))
        };
        let err = new_package_with(base.path(), "my_app", &mut write).unwrap_err();
        assert!(
            err.message()
                .starts_with("cannot create `my_app/cheby.toml`: ")
        );
        assert_eq!(names_in(base.path()), ["my_app"]);
        assert_eq!(names_in(&dir), ["other"]);
    }
}
