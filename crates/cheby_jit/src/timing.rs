//! What the last batch spent in each step of loading (D-974): the hidden
//! `CHEBY_PHASES` variable's phases `codegen`, `declare`, `define`,
//! `finalize` and `register`, which the driver moves out of the span
//! around [`crate::Jit::compile_units`] (D-1277).
//!
//! The clock is read only when `CHEBY_PHASES` names a file; the variable
//! is read once per process. Unset, every time is zero.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// What the last [`crate::Jit::compile_units`] spent generating code for
/// fresh units (`records_for_jit`) and declaring, defining, finalizing and
/// registering its records (D-974's "declaring"). All zero unless
/// `CHEBY_PHASES` names a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadTimes {
    /// Compiling the batch's MIR units alone into code records, in
    /// parallel, as the calling thread waited for it.
    pub codegen: Duration,
    /// Declaring every record's symbols in the JIT's module.
    pub declare: Duration,
    /// Defining every record's functions and data.
    pub define: Duration,
    /// `finalize_definitions`: relocations applied, memory protected.
    pub finalize: Duration,
    /// Registering the records' module records with the runtime.
    pub register: Duration,
}

/// Whether loading is timed: `CHEBY_PHASES` names a file.
fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("CHEBY_PHASES").is_some_and(|value| !value.is_empty()))
}

/// A clock started only when loading is timed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stopwatch(Option<Instant>);

impl Stopwatch {
    /// Starts the clock when loading is timed.
    pub(crate) fn start() -> Self {
        Self(enabled().then(Instant::now))
    }

    /// The time since [`Stopwatch::start`]; zero when untimed.
    pub(crate) fn elapsed(self) -> Duration {
        self.0.map_or(Duration::ZERO, |start| start.elapsed())
    }
}
