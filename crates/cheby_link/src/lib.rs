//! AOT object output and linking (D-915, D-933): what `cheby build` does
//! after code generation.
//!
//! - Each module's code record (`cheby_codegen_clif::record`, D-914) is
//!   replayed into one relocatable object ([`object::replay`]): ELF for
//!   Linux targets, Mach-O for macOS. Replay is serialization, never a
//!   second code generation, so an executable runs the bytes `cheby run`
//!   runs (ADR-0047). A record with source positions also gets DWARF line
//!   tables (`dwarf.rs`, D-938), which Linux executables keep and macOS
//!   ones drop with `-Wl,-S` until M6.
//! - The program object ([`program::program_object`]) holds the entries
//!   unit, the program image `cb$image`, the C `main` and the import of
//!   the runtime ID's symbol (D-915, D-925, D-930).
//! - The runtime archive `libcheby_rt_exe.a` is found next to the running
//!   `cheby` or through `CHEBY_RT_ARCHIVE`, and must define this
//!   compiler's runtime ID ([`archive`], D-929, D-930): `cheby build`
//!   needs it, so `cargo build -p cheby_rt_exe` comes first.
//! - [`link::link`] runs `cc` with the fixed flags and input order of
//!   D-933, and [`native_libs`] holds the archive's system libraries and
//!   the build resolver of E0053 (D-943).
//! - A package's `[native]` code (D-940): [`cc`] compiles each C source
//!   with D-942's fixed flags and reports the files it read, with
//!   `cheby.h` embedded ([`cc::HEADER`], D-945); [`shlib`] links the
//!   package's shared library with its forwarder for the commands that run
//!   code in the `cheby` process (D-941) and reads the names a C object
//!   defines (D-944). The same C objects go into executables
//!   ([`link::LinkJob::c_objects`]).
//!
//! See `docs/plan/architecture.md`.

use std::fmt;
use std::path::PathBuf;

use cheby_codegen_clif::record::RecordError;

pub mod archive;
pub mod cc;
mod dwarf;
#[cfg(test)]
mod dwarf_tests;
pub mod link;
pub mod native_libs;
pub mod object;
pub mod program;
pub mod shlib;

#[cfg(test)]
mod cc_tests;
#[cfg(test)]
mod link_tests;
#[cfg(test)]
mod object_tests;
#[cfg(test)]
mod program_tests;

/// Why an object could not be written or a program could not be linked.
/// Every variant but [`LinkError::Record`], [`LinkError::Object`] and
/// [`LinkError::Copies`] is a tool error of `cheby build`, whose text is
/// its `Display` (D-1114).
#[derive(Debug)]
pub enum LinkError {
    /// The code record is malformed or does not fit the target.
    Record(RecordError),
    /// `cranelift-object` or the `object` crate refused the object.
    Object(String),
    /// D-918's check: two local copies of one name differ (an internal
    /// error).
    Copies(String),
    /// The runtime archive does not exist (D-929).
    MissingArchive(PathBuf),
    /// `CHEBY_RT_ARCHIVE` names a relative path (D-929).
    RelativeArchiveVar(PathBuf),
    /// The runtime archive cannot be read as an archive.
    BadArchive { path: PathBuf, message: String },
    /// The runtime archive lacks this compiler's runtime ID (D-930).
    StaleArchive { path: PathBuf, symbol: String },
    /// `cc` could not be started (D-933).
    NoCc(String),
    /// The link failed: `cc`'s status and its output, unparsed (D-933).
    LinkFailed { status: String, output: String },
    /// A temporary directory, an object file or the output could not be
    /// written.
    Io(String),
}

impl LinkError {
    /// Whether the error is about the build's setting, a tool error of
    /// `cheby build`, rather than an internal failure.
    #[must_use]
    pub const fn is_tool_error(&self) -> bool {
        !matches!(self, Self::Record(_) | Self::Object(_) | Self::Copies(_))
    }
}

/// The help that the archive's tool errors end with (D-929, D-930).
const BUILD_ARCHIVE: &str =
    "build it with `cargo build -p cheby_rt_exe`, with the profile of this `cheby`";

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Record(error) => write!(f, "{error}"),
            Self::Object(message) => write!(f, "writing an object failed: {message}"),
            Self::Copies(message) | Self::Io(message) => write!(f, "{message}"),
            Self::MissingArchive(path) => write!(
                f,
                "the runtime archive `{}` does not exist, and `cheby build` links it into every \
                 executable; {BUILD_ARCHIVE}",
                path.display()
            ),
            Self::RelativeArchiveVar(path) => write!(
                f,
                "CHEBY_RT_ARCHIVE must be an absolute path, but it is `{}`",
                path.display()
            ),
            Self::BadArchive { path, message } => write!(
                f,
                "the runtime archive `{}` cannot be read ({message}); {BUILD_ARCHIVE}",
                path.display()
            ),
            Self::StaleArchive { path, symbol } => write!(
                f,
                "the runtime archive `{}` was built from other runtime sources than this `cheby` \
                 (it does not define `{symbol}`); {BUILD_ARCHIVE}",
                path.display()
            ),
            Self::NoCc(message) => write!(
                f,
                "`cheby build --target native` needs a C compiler and linker, but `cc` could not \
                 be run from PATH: {message}"
            ),
            Self::LinkFailed { status, output } => {
                write!(f, "linking with `cc` failed ({status})")?;
                let output = output.trim_end();
                if !output.is_empty() {
                    write!(f, ":\n{output}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for LinkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Record(error) => Some(error),
            _ => None,
        }
    }
}

impl From<RecordError> for LinkError {
    fn from(error: RecordError) -> Self {
        Self::Record(error)
    }
}
