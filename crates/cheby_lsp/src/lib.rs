//! The language server, `cheby lsp` (§13.11).
//!
//! [`run`] serves the protocol over stdio; [`run_on`] serves any
//! [`lsp_server::Connection`], for tests in process. Stdout carries
//! protocol frames only, so nothing in this crate prints to it.
//!
//! Layout (plan §1.4): `server` holds the main loop and all server state,
//! `analysis_thread` the one thread that runs every package's sessions,
//! and `text` the line index and `file:` URIs. Generation handles of the
//! driver stay out of all of them (D-1366): the crate's `clippy.toml`
//! bans their types, and handlers, which alone may read a snapshot, return
//! JSON. A handle cannot even be turned into JSON, since the driver's types
//! have no `Serialize`:
//!
//! ```compile_fail,E0277
//! fn leak(ty: cheby_driver::analysis::Ty) -> serde_json::Value {
//!     serde_json::to_value(ty).unwrap_or_default()
//! }
//! ```
//!
//! while the same type, passed along, and plain data, such as a path, can.
//! This block is what keeps the one above failing for its own reason, since
//! rustdoc does not check the error code of a `compile_fail` block on
//! stable: a renamed type would fail the block above with E0412, and here:
//!
//! ```
//! fn exists(ty: cheby_driver::analysis::Ty) -> cheby_driver::analysis::Ty {
//!     ty
//! }
//! fn plain(path: std::path::PathBuf) -> serde_json::Value {
//!     serde_json::to_value(path).unwrap_or_default()
//! }
//! # let _ = exists;
//! # let _ = plain(std::path::PathBuf::from("src/main.cheby"));
//! ```

#![deny(clippy::print_stdout)]

mod analysis_thread;
// The one module that may name a generation handle's type (plan §1.4,
// D-1366). The lint sees written type names only, so a value held through
// inference elsewhere is review's job (R-10).
#[expect(
    clippy::disallowed_types,
    reason = "features may hold generation handles (D-1366)"
)]
mod features;
mod server;
mod text;

use std::path::PathBuf;
use std::process::ExitCode;

use cheby_driver::cache::BuildId;
use lsp_server::Connection;

/// How the server runs.
#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// The compiler's build ID (D-957).
    pub build: BuildId,
    /// The `cheby` executable for child jobs: `Some(current_exe())` under
    /// [`run`], `None` in tests in process, so a test binary never runs as
    /// the child.
    pub child: Option<PathBuf>,
}

/// `cheby lsp`: serves stdio until `exit` or the end of input. The status
/// is 0 after `shutdown`, else 1.
#[must_use]
pub fn run(options: ServerOptions) -> ExitCode {
    // The std base is checked once per process. It starts now, so the
    // first request that needs it seldom waits (A §7).
    features::stdlib::warm();
    let (connection, io) = Connection::stdio();
    match server::serve(connection, options) {
        Ok(ended) => {
            // The reader ends at `exit`, at the end of input, or at a
            // frame whose body is not a JSON-RPC message. A client that
            // stopped reading may leave it blocked on stdin, so the threads
            // are joined only in those cases. A malformed frame ends the
            // server with status 1 and this one line on stderr (D-1430).
            if ended.joinable
                && let Err(e) = io.join()
            {
                eprintln!("cheby lsp: {e}");
            }
            ended.exit_code()
        }
        Err(message) => {
            eprintln!("cheby lsp: {message}");
            ExitCode::FAILURE
        }
    }
}

/// The server over any connection, for tests in process.
///
/// # Errors
///
/// A message when the server cannot start, such as when its analysis
/// thread cannot be spawned.
pub fn run_on(connection: Connection, options: ServerOptions) -> Result<ExitCode, String> {
    server::serve(connection, options).map(server::Ended::exit_code)
}
