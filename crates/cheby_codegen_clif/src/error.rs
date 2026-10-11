//! Errors of code generation.

use std::fmt;

use cheby_iface::Ice;

/// Why code generation failed. None of these is a user error: MIR
/// lowering accepts every program the front end accepts and code
/// generation handles all valid MIR (D-554), so every variant is a bug in
/// the compiler or an unsupported host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodegenError {
    /// A broken invariant in the MIR or in this crate.
    Ice(Ice),
    /// The Cranelift module refused a declaration or definition.
    Module(String),
    /// Cranelift's verifier or compiler rejected a function.
    Compile(String),
    /// The host is not one code generation supports: not 64-bit, or not
    /// little-endian.
    UnsupportedHost(String),
}

impl CodegenError {
    pub(crate) fn ice(message: impl Into<String>) -> Self {
        Self::Ice(Ice::new(message))
    }

    pub(crate) fn module(e: &cranelift_module::ModuleError) -> Self {
        Self::Module(e.to_string())
    }
}

impl From<Ice> for CodegenError {
    fn from(ice: Ice) -> Self {
        Self::Ice(ice)
    }
}

impl fmt::Display for CodegenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ice(ice) => write!(f, "{}", ice.message()),
            Self::Module(message) => write!(f, "code generation failed: {message}"),
            Self::Compile(message) => write!(f, "Cranelift rejected a function:\n{message}"),
            Self::UnsupportedHost(message) => write!(f, "unsupported host: {message}"),
        }
    }
}

impl std::error::Error for CodegenError {}
