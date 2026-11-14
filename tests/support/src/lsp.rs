//! The client side of the language server harness,
//! `cargo xtask test -E 'binary_id(cheby_cli::lsp)'` (D-1352,
//! docs/plan/testing.md).
//!
//! - [`frame`]: the `Content-Length` framing of JSON-RPC over stdio, read
//!   strictly, so that a stray byte on the server's standard output is a
//!   failure and not a skipped line (A §3.7).
//! - [`client`]: [`Client`], one `cheby lsp` child process spoken to over
//!   pipes, with timeouts on every wait, automatic answers to the server's
//!   own requests, and the deterministic wait for diagnostics: each new
//!   version of an open document gets a publish with that version, and so
//!   does an opened document, even with an unchanged list (D-1373,
//!   D-1442).
//! - [`uri`]: `file:` URIs and paths, with the Windows rules testable on
//!   every platform.
//! - [`text`]: positions in the three encodings (D-1368), the compiler's
//!   code-point columns (D-421), and applying text and workspace edits.
//! - [`editing`]: completion, signature help and inlay hints: their
//!   readable golden forms, applying a completion item and checking its
//!   edit (D-1394), and where the examples group asks for them.
//! - [`case`]: the golden tests under `tests/lsp/`, their header and their
//!   caret markers (`tests/lsp/README.md`).
//! - [`parity`]: diagnostics as `cheby check` prints them and as the server
//!   publishes them, reduced to one comparable key (D-1348).
//! - [`outline`]: the outline a `documentSymbol` answer must give, read
//!   from the parser's item list (`features.md` §5.1).
//! - [`quality`]: the quality gate of plan W7, `facts.md` §0.1's seeded
//!   body mutants and the grading of hover on them.
//!
//! `serde_json` is re-exported, so the harness, a test target of
//! `cheby_cli`, needs no dependency of its own.

pub use serde_json;

pub mod case;
pub mod client;
pub mod editing;
pub mod frame;
pub mod outline;
pub mod parity;
pub mod quality;
pub mod text;
pub mod uri;

pub use client::{Client, ClientKind, Ended, Paths, RpcError, StartOptions};
pub use text::Encoding;
