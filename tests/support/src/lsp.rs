//! The client side of the language server harness, `cargo test -p
//! cheby_cli --test lsp` (D-1352, docs/plan/testing.md).
//!
//! - [`frame`]: the `Content-Length` framing of JSON-RPC over stdio, read
//!   strictly, so that a stray byte on the server's standard output is a
//!   failure and not a skipped line (A §3.7).
//! - [`client`]: [`Client`], one `cheby lsp` child process spoken to over
//!   pipes, with timeouts on every wait, automatic answers to the server's
//!   own requests, and the deterministic wait for diagnostics: an open
//!   document always gets a publish with its version (D-1373).
//! - [`uri`]: `file:` URIs and paths, with the Windows rules testable on
//!   every platform.
//! - [`text`]: positions in the three encodings (D-1368), the compiler's
//!   code-point columns (D-421), and applying text and workspace edits.
//! - [`case`]: the golden tests under `tests/lsp/`, their header and their
//!   caret markers (`tests/lsp/README.md`).
//! - [`parity`]: diagnostics as `cheby check` prints them and as the server
//!   publishes them, reduced to one comparable key (D-1348).
//!
//! `serde_json` is re-exported, so the harness, a test target of
//! `cheby_cli`, needs no dependency of its own.

pub use serde_json;

pub mod case;
pub mod client;
pub mod frame;
pub mod parity;
pub mod text;
pub mod uri;

pub use client::{Client, ClientKind, Ended, Paths, RpcError, StartOptions};
pub use text::Encoding;
