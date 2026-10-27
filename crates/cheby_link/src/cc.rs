//! Compiling a package's C sources (D-942): `cc` from `PATH` turns one
//! source into one position-independent object, with fixed flags, and
//! reports every file the compile read.
//!
//! - The command is `cc -c -O2 -fPIC -g0 -ffile-prefix-map=<package
//!   root>=. -ffile-prefix-map=<header dir>=cheby -I<header dir> -MD -MF
//!   <dependency file> -o <object> <source>`, run with the package root
//!   as its working directory and the source as its package-relative
//!   path, `SOURCE_DATE_EPOCH=0`, and `CC`, `LD`, `CFLAGS`, `LDFLAGS` and
//!   `MACOSX_DEPLOYMENT_TARGET` removed from its environment (D-933,
//!   D-1112), so no path of the machine reaches the object: `-g0` writes
//!   no debug information, and the two prefix maps rewrite what
//!   `__FILE__` names.
//! - `-MD -MF` writes the files the compile read, which [`compile`]
//!   returns in the three forms of D-942 ([`Dependency`]): a file of the
//!   package by its package-relative path, the embedded header as
//!   [`HEADER_NAME`], any other file, such as a system header, by its
//!   absolute path. The caller keys and records them; nothing here
//!   depends on the cache.
//! - The object and the dependency file are written into a fresh private
//!   directory in the system's temporary directory, named like the link's
//!   (`cheby-objects-<pid>-<n>`, D-1112, so the same sweep removes a dead
//!   process's), and removed on every path that returns or unwinds.
//! - The compiler's output is shown only on failure ([`CcError`]).
//!
//! [`HEADER`] is `cheby.h` (D-945), embedded here and written by the
//! driver into the cache's include directory for every compile. Until N8b
//! it holds only its include guard and `cheby_value`'s typedef
//! (D-1196).

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::link::{CC, OBJECTS_PREFIX, PrivateDir, REMOVED_VARS, sweep_dead};

/// `cheby.h` (D-945): its include guard `CHEBY_H`, `typedef struct
/// cheby_value cheby_value;` (D-1196) and the runtime's `cheby_ffi_`
/// functions, in `cheby_runtime::ffi::NAMES`' order.
pub const HEADER: &str = include_str!("../include/cheby.h");

/// The header's file name in the include directory, and how a dependency
/// on it is recorded (D-942).
pub const HEADER_NAME: &str = "cheby.h";

/// The flags of every compile in their fixed form, as keys hold them: the
/// `-I` and `-ffile-prefix-map` arguments, whose only variable part is a
/// path, appear without it (D-942).
pub const KEY_FLAGS: &[&str] = &[
    "-c",
    "-O2",
    "-fPIC",
    "-g0",
    "-ffile-prefix-map=<package root>=.",
    "-ffile-prefix-map=<header dir>=cheby",
    "-I<header dir>",
];

/// The C compiler's identity, which keys every C object (D-942): the
/// file `cc` resolves to in `PATH` and what `cc --version` prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompilerIdentity {
    /// `cc` found in `PATH`, with symbolic links resolved.
    pub path: PathBuf,
    /// `cc --version`'s standard output.
    pub version: Vec<u8>,
}

/// One file a compile read, in the form D-942 records it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Dependency {
    /// A file inside the package, by its package-relative path with `/`.
    Package(String),
    /// The embedded `cheby.h`.
    Header,
    /// Any other file, by its absolute path.
    Other(PathBuf),
}

/// The result of one compile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compiled {
    /// The object's bytes.
    pub object: Vec<u8>,
    /// The files the compile read, sorted, the source included.
    pub dependencies: Vec<Dependency>,
}

/// Why a C compile failed.
#[derive(Debug)]
pub enum CcError {
    /// `cc` could not be started (or found): the system's message.
    NoCc(String),
    /// The compile failed: the source, `cc`'s status and its output,
    /// unparsed (D-942).
    Failed {
        source: String,
        status: String,
        output: String,
    },
    /// A temporary file could not be written or read back.
    Io(String),
}

impl fmt::Display for CcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The command that compiles words its own message for a
            // missing `cc` (plan §7 Q6 (4), D-1114); this is its detail.
            Self::NoCc(message) | Self::Io(message) => write!(f, "{message}"),
            Self::Failed {
                source,
                status,
                output,
            } => {
                write!(f, "compiling `{source}` with `cc` failed ({status})")?;
                let output = output.trim_end();
                if !output.is_empty() {
                    write!(f, ":\n{output}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for CcError {}

/// The C compiler's identity, found once per process (D-942).
///
/// # Errors
///
/// [`CcError::NoCc`] when `cc` is not in `PATH` or `cc --version` cannot
/// run.
pub fn compiler_identity() -> Result<CompilerIdentity, CcError> {
    static IDENTITY: OnceLock<Result<CompilerIdentity, String>> = OnceLock::new();
    IDENTITY
        .get_or_init(find_identity)
        .clone()
        .map_err(CcError::NoCc)
}

/// [`compiler_identity`]'s work.
fn find_identity() -> Result<CompilerIdentity, String> {
    let path = find_in_path(CC)
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound).to_string())?;
    let mut cmd = Command::new(CC);
    for var in REMOVED_VARS {
        cmd.env_remove(var);
    }
    let output = cmd.arg("--version").output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("`cc --version` failed ({})", output.status));
    }
    Ok(CompilerIdentity {
        path,
        version: output.stdout,
    })
}

/// The file `name` resolves to in `PATH`, with symbolic links resolved:
/// the first directory of `PATH` that has a file of that name.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .map(|found| std::fs::canonicalize(&found).unwrap_or(found))
}

/// What one compile takes.
#[derive(Clone, Copy, Debug)]
pub struct CompileJob<'a> {
    /// The package root, the compile's working directory: an absolute
    /// path.
    pub root: &'a Path,
    /// The source, package-relative, as `[native]` lists it.
    pub source: &'a str,
    /// The directory that holds [`HEADER`] as [`HEADER_NAME`]: an
    /// absolute path.
    pub header_dir: &'a Path,
}

/// The arguments of a compile after `cc`, the object's and the
/// dependency file's paths given (module documentation).
#[must_use]
pub fn compile_args(job: &CompileJob<'_>, object: &Path, depfile: &Path) -> Vec<String> {
    let mut args: Vec<String> = ["-c", "-O2", "-fPIC", "-g0"].map(str::to_owned).to_vec();
    args.push(format!("-ffile-prefix-map={}=.", job.root.display()));
    args.push(format!(
        "-ffile-prefix-map={}=cheby",
        job.header_dir.display()
    ));
    args.push(format!("-I{}", job.header_dir.display()));
    args.extend([
        "-MD".to_owned(),
        "-MF".to_owned(),
        depfile.display().to_string(),
        "-o".to_owned(),
        object.display().to_string(),
        job.source.to_owned(),
    ]);
    args
}

/// Compiles `job.source` (module documentation).
///
/// # Errors
///
/// [`CcError::NoCc`] when `cc` cannot be started, [`CcError::Failed`] with
/// its output when it fails, [`CcError::Io`] when a temporary file cannot
/// be written or read.
pub fn compile(job: &CompileJob<'_>) -> Result<Compiled, CcError> {
    let tmp = std::env::temp_dir();
    sweep_dead(&tmp, OBJECTS_PREFIX);
    let dir = PrivateDir::create(&tmp, OBJECTS_PREFIX).map_err(|e| CcError::Io(e.to_string()))?;
    let object = dir.path().join("source.o");
    let depfile = dir.path().join("source.d");
    let mut cmd = Command::new(CC);
    for var in REMOVED_VARS {
        cmd.env_remove(var);
    }
    cmd.env("SOURCE_DATE_EPOCH", "0")
        .current_dir(job.root)
        .args(compile_args(job, &object, &depfile));
    let output = cmd.output().map_err(|e| CcError::NoCc(e.to_string()))?;
    if !output.status.success() {
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        return Err(CcError::Failed {
            source: job.source.to_owned(),
            status: output.status.to_string(),
            output: text,
        });
    }
    let read = |path: &Path| {
        std::fs::read(path)
            .map_err(|e| CcError::Io(format!("cannot read `{}`: {e}", path.display())))
    };
    let bytes = read(&object)?;
    let deps = read(&depfile)?;
    let mut dependencies: Vec<Dependency> = parse_depfile(&String::from_utf8_lossy(&deps))
        .into_iter()
        .map(|path| classify(Path::new(&path), job.root, job.header_dir))
        .collect();
    dependencies.sort();
    dependencies.dedup();
    Ok(Compiled {
        object: bytes,
        dependencies,
    })
}

/// The prerequisites of a make-style dependency file, in order: every
/// path after the first rule's `:`, with line continuations joined and
/// `\ ` and `$$` unescaped.
#[must_use]
pub fn parse_depfile(text: &str) -> Vec<String> {
    let joined = text.replace("\\\r\n", " ").replace("\\\n", " ");
    // The target is the first word, ended by `:` followed by whitespace or
    // the end of the line.
    let Some(start) = rule_colon(&joined) else {
        return Vec::new();
    };
    let rest = joined.get(start..).unwrap_or("");
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => {
                current.push(' ');
                chars.next();
            }
            '$' if chars.peek() == Some(&'$') => {
                current.push('$');
                chars.next();
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                if c == '\n' {
                    // Later lines are the phony targets of `-MP`, or
                    // nothing.
                    break;
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// The byte after the `:` that ends a rule's target: the first `:`
/// followed by whitespace or the end of the text.
fn rule_colon(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    bytes.iter().enumerate().find_map(|(at, &b)| {
        let next = bytes.get(at.saturating_add(1));
        (b == b':' && next.is_none_or(u8::is_ascii_whitespace)).then(|| at.saturating_add(1))
    })
}

/// The recorded form of the dependency `path` of a compile run in `root`
/// with the header in `header_dir` (D-942).
#[must_use]
pub fn classify(path: &Path, root: &Path, header_dir: &Path) -> Dependency {
    let absolute = if path.is_absolute() {
        normalize(path)
    } else {
        normalize(&root.join(path))
    };
    if absolute == normalize(&header_dir.join(HEADER_NAME)) {
        return Dependency::Header;
    }
    match absolute.strip_prefix(normalize(root)) {
        Ok(relative) => Dependency::Package(slashed(relative)),
        Err(_) => Dependency::Other(absolute),
    }
}

/// `path` with `.` dropped and `..` applied lexically.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A relative path with `/` between its components.
fn slashed(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}
