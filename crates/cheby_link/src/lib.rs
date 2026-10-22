//! AOT object output and, from N4 on, linker invocation (D-915).
//!
//! Each module's code record (`cheby_codegen_clif::record`, D-914) is
//! replayed into one relocatable object ([`object::replay`]): ELF for
//! Linux targets, Mach-O for macOS. Replay is serialization, never a
//! second code generation, so an executable runs the bytes `cheby run`
//! runs (ADR-0047). The program object, the link line and `cheby build`
//! come with N4. See `docs/plan/architecture.md`.

use std::fmt;

use cheby_codegen_clif::record::RecordError;

pub mod object;

#[cfg(test)]
mod object_tests;

/// Why an object could not be written.
#[derive(Debug)]
pub enum LinkError {
    /// The code record is malformed or does not fit the target.
    Record(RecordError),
    /// `cranelift-object` or the `object` crate refused the object.
    Object(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Record(error) => write!(f, "{error}"),
            Self::Object(message) => write!(f, "writing an object failed: {message}"),
        }
    }
}

impl std::error::Error for LinkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Record(error) => Some(error),
            Self::Object(_) => None,
        }
    }
}

impl From<RecordError> for LinkError {
    fn from(error: RecordError) -> Self {
        Self::Record(error)
    }
}
