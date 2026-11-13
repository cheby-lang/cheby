//! The hidden child command, `cheby __backend` (A §11, plan §1.8, D-1349,
//! D-1410).
//!
//! ```text
//! cheby [-j N] __backend --schema 1 --root <dir> --want constants [--max-text 4096]
//! cheby [-j N] __backend --schema 1 --root <dir> --want ownership [--mir MODULE ITEM]
//! ```
//!
//! The server starts it from `current_exe` (`ServerOptions::child`), so
//! the versions cannot differ, and `--schema` only guards against a
//! binary replaced under a running server. One process runs one job and
//! writes one [`wire::Envelope`] to stdout, then exits with status 0 when
//! the document is complete, whatever its diagnostics say (A §11.3). It
//! reads saved files only, with `Mode::Check` (D-1410), and no build cache
//! (D-1363). It is hidden from `--help`, and absent from the spec and
//! `docs/`.
//!
//! `cheby_cli` parses the arguments with `clap` and fills a plain [`Args`],
//! so this crate has no `clap` (A §11.1). Its `Command::Backend` is hidden
//! and joins `builds`, so `-j` sets the worker pool as for `cheby check`.
//!
//! What [`main`] does:
//!
//! - It starts the watchdog first: a thread reads stdin to its end, then
//!   exits the process with status [`WATCHDOG_EXIT`]. A server that dies by
//!   any means closes the pipe, and a kernel call in generated code cannot
//!   be interrupted any other way (A §11.5, D-1410). So the server keeps the
//!   child's stdin open while it waits.
//! - A `--schema` other than [`wire::SCHEMA`], or a `--want` other than
//!   `constants` and `ownership`, is an `internal-error` document.
//! - `--want constants` calls `cheby_driver::analysis::backend_query` with
//!   `BackendJob::Constants` and turns its result into the wire types:
//!   relative `/` paths, hex hashes, symbols as [`wire::SymbolData`]. A
//!   panic in the back end is an `internal-error` document; its report goes
//!   to stderr, as every panic report does.
//! - On Windows, `--want constants` evaluates nothing and its section is
//!   `skipped`, so tests can run the argument path there (A §11.7,
//!   D-1414).
//! - `--want ownership` belongs to the W15 wave. Until then the document is
//!   `ok` with no front end run and an ownership section
//!   `{"status": "skipped", "reason": …}`.
//! - Nothing but the one document goes to stdout, written at once at the
//!   end; the back end prints nothing, and generated code that would print
//!   is E0077 (A §11.8). The status is 0 whenever the document was written
//!   whole, whatever it says, and 1 when stdout failed.

pub mod wire;

use std::io::{Read, Write as _};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::ExitCode;

use cheby_driver::analysis::{
    self, BackendError, BackendJob, BackendRequest, BackendResult, ConstantsOut, FrontOutcome,
    Target, backend_query,
};

use self::wire::{
    ConstantsSection, Envelope, FileHash, Front, Section, Sections, Status, SymbolData,
    WireDiagnostic, WireHash, WireValue,
};

/// The default of `--max-text`: the cap on a constant's text, in
/// characters (D-1413).
pub const MAX_TEXT: usize = 4096;

/// The arguments of `cheby __backend`, as `cheby_cli` parsed them (A §11.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    /// `--schema`: the server's [`wire::SCHEMA`].
    pub schema: u32,
    /// `--root`: the package root.
    pub root: PathBuf,
    /// `--target`, as `cheby check --target` (D-550). `None` is the
    /// manifest's targets.
    pub target: Option<Target>,
    /// `--want`: `constants` or `ownership`, one job per process (D-1410).
    /// Kept as written, so an unknown job is this crate's error.
    pub want: String,
    /// `--max-text`, [`MAX_TEXT`] by default.
    pub max_text: usize,
    /// `--mir MODULE ITEM`: the item whose MIR the ownership view shows
    /// (W15). `None` for the constants job.
    pub mir: Option<(String, String)>,
}

/// The status of a child whose stdin reached its end before it finished:
/// the server is gone (A §11.5).
pub const WATCHDOG_EXIT: u8 = 3;

/// The reason of the constants section on Windows (D-1414).
const WINDOWS: &str = "constants are not evaluated on Windows until M6 (D-1414)";

/// The reason of the ownership section until the W15 wave gives it a body.
const OWNERSHIP_LATER: &str = "the ownership job is not implemented yet";

/// Runs the child: one JSON document on stdout (module docs).
#[must_use]
pub fn main(args: Args) -> ExitCode {
    watch_stdin();
    write(&answer(args))
}

/// Starts the watchdog thread (module docs, D-1410). Without it the child
/// still answers, so a failure to start it is only reported.
fn watch_stdin() {
    let spawned = std::thread::Builder::new()
        .name("cheby-backend-watchdog".to_owned())
        .spawn(|| {
            drain(std::io::stdin().lock());
            server_gone();
        });
    if let Err(e) = spawned {
        eprintln!("cheby __backend: cannot watch stdin: {e}");
    }
}

/// Reads `input` until its end or an error other than an interruption.
fn drain(mut input: impl Read) {
    let mut buffer = [0_u8; 512];
    loop {
        match input.read(&mut buffer) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

#[expect(
    clippy::exit,
    reason = "the server is gone and generated code may be running (A §11.5)"
)]
fn server_gone() -> ! {
    std::process::exit(i32::from(WATCHDOG_EXIT))
}

/// The document for `args` (module docs).
fn answer(args: Args) -> Envelope {
    if args.schema != wire::SCHEMA {
        return refusal(Status::InternalError {
            message: format!(
                "the server asked for schema {}, and this `cheby` writes schema {}",
                args.schema,
                wire::SCHEMA
            ),
        });
    }
    match args.want.as_str() {
        "constants" if cfg!(windows) => ok_with(Sections {
            constants: Some(Section::Skipped {
                reason: WINDOWS.to_owned(),
            }),
            ownership: None,
        }),
        "constants" => constants(args),
        "ownership" => ok_with(Sections {
            constants: None,
            ownership: Some(serde_json::json!({
                "status": "skipped",
                "reason": OWNERSHIP_LATER,
            })),
        }),
        other => refusal(Status::InternalError {
            message: format!("unknown job `{other}`: expected `constants` or `ownership`"),
        }),
    }
}

/// The constants job (D-1410, D-1413).
fn constants(args: Args) -> Envelope {
    let request = BackendRequest {
        root: args.root,
        target: args.target,
        job: BackendJob::Constants,
        max_text: args.max_text,
    };
    match catch_unwind(AssertUnwindSafe(|| backend_query(&request))) {
        Ok(Ok(result)) => envelope(&result),
        Ok(Err(BackendError::NoPackage)) => refusal(Status::NoPackage),
        Ok(Err(BackendError::Tool(message))) => refusal(Status::ToolError { message }),
        Ok(Err(BackendError::Internal(message))) => refusal(Status::InternalError { message }),
        Err(payload) => {
            let text = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("a panic with no message");
            refusal(Status::InternalError {
                message: format!("the back end panicked: {text}"),
            })
        }
    }
}

/// `result` as the wire carries it (A §11.3, D-1412).
fn envelope(result: &BackendResult) -> Envelope {
    Envelope {
        schema: wire::SCHEMA,
        status: Status::Ok,
        manifest: Some(WireHash(result.manifest)),
        files: result
            .files
            .iter()
            .map(|file| FileHash {
                path: wire::path_text(&file.path),
                hash: WireHash(file.hash),
            })
            .collect(),
        front: Some(match result.front {
            FrontOutcome::Clean => Front::Clean,
            FrontOutcome::Errors => Front::Errors,
        }),
        sections: Sections {
            constants: result.constants.as_ref().map(constants_section),
            ownership: None,
        },
    }
}

/// The constants section on the wire.
fn constants_section(section: &analysis::Section<ConstantsOut>) -> Section<ConstantsSection> {
    match section {
        analysis::Section::Ok(out) => Section::Ok(ConstantsSection {
            diagnostics: out
                .diagnostics
                .iter()
                .map(|d| WireDiagnostic::from_plain(&d.path, &d.diagnostic, d.symbol.as_ref()))
                .collect(),
            values: out
                .values
                .iter()
                .map(|value| WireValue {
                    symbol: SymbolData::from(&value.symbol),
                    text: value.text.clone(),
                    truncated: value.truncated,
                    modules: value
                        .modules
                        .as_ref()
                        .map(|paths| paths.iter().map(|path| wire::path_text(path)).collect()),
                })
                .collect(),
        }),
        analysis::Section::Skipped(reason) => Section::Skipped {
            reason: reason.clone(),
        },
        analysis::Section::Failed(message) => Section::Failed {
            message: message.clone(),
        },
    }
}

/// An `ok` document that ran no front end, with `sections`.
fn ok_with(sections: Sections) -> Envelope {
    Envelope {
        schema: wire::SCHEMA,
        status: Status::Ok,
        manifest: None,
        files: Vec::new(),
        front: None,
        sections,
    }
}

/// A document whose job did not run.
fn refusal(status: Status) -> Envelope {
    Envelope {
        status,
        ..ok_with(Sections::default())
    }
}

/// Writes `envelope` as the only thing on stdout, in one write.
fn write(envelope: &Envelope) -> ExitCode {
    let text = match envelope.to_json() {
        Ok(text) => text + "\n",
        Err(e) => {
            eprintln!("cheby __backend: cannot write the document: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cheby __backend: cannot write to stdout: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use cheby_driver::analysis::{
        BackendFile, Code, ConstDiagnostic, ConstValue, Hash128, ModulePath, PlainDiagnostic,
        Severity, Symbol, SymbolNs, TextRange,
    };

    use super::*;

    fn args(schema: u32, want: &str) -> Args {
        Args {
            schema,
            root: PathBuf::from("/nowhere"),
            target: None,
            want: want.to_owned(),
            max_text: MAX_TEXT,
            mir: None,
        }
    }

    fn constant(name: &str) -> Symbol {
        Symbol::item(ModulePath::root("app"), SymbolNs::Value, name)
    }

    #[test]
    fn a_schema_mismatch_is_an_internal_error() {
        let envelope = answer(args(wire::SCHEMA + 1, "constants"));
        assert_eq!(envelope.schema, wire::SCHEMA);
        assert!(
            matches!(&envelope.status, Status::InternalError { message } if message.contains("schema 2")),
            "{envelope:?}"
        );
        assert_eq!(envelope.front, None);
        assert_eq!(envelope.sections, Sections::default());
    }

    #[test]
    fn an_unknown_job_is_an_internal_error() {
        let envelope = answer(args(wire::SCHEMA, "reuse"));
        assert!(
            matches!(&envelope.status, Status::InternalError { message } if message.contains("`reuse`")),
            "{envelope:?}"
        );
    }

    #[test]
    fn ownership_is_skipped_until_its_wave() {
        let envelope = answer(args(wire::SCHEMA, "ownership"));
        assert_eq!(envelope.status, Status::Ok);
        assert_eq!(envelope.sections.constants, None);
        assert_eq!(
            envelope
                .sections
                .ownership
                .as_ref()
                .and_then(|o| o.get("status")),
            Some(&serde_json::json!("skipped"))
        );
    }

    /// D-1414: on Windows the child evaluates nothing.
    #[test]
    #[cfg(windows)]
    fn windows_skips_constants() {
        let envelope = answer(args(wire::SCHEMA, "constants"));
        assert_eq!(envelope.status, Status::Ok);
        assert!(matches!(
            envelope.sections.constants,
            Some(Section::Skipped { .. })
        ));
    }

    #[test]
    fn a_result_becomes_a_document() {
        let diagnostic = PlainDiagnostic {
            code: Code::parse("E0075").expect("a code"),
            severity: Severity::Error,
            message: "constant `boom` panicked".to_owned(),
            range: Some(TextRange::new(10, 14)),
            primary_message: String::new(),
            related: Vec::new(),
            notes: Vec::new(),
            helps: Vec::new(),
        };
        let path: PathBuf = ["src", "main.cheby"].iter().collect();
        let result = BackendResult {
            manifest: Hash128([1; 16]),
            files: vec![BackendFile {
                path: path.clone(),
                hash: Hash128([2; 16]),
            }],
            front: FrontOutcome::Clean,
            constants: Some(analysis::Section::Ok(ConstantsOut {
                diagnostics: vec![ConstDiagnostic {
                    path: path.clone(),
                    diagnostic: diagnostic.clone(),
                    symbol: Some(constant("boom")),
                }],
                values: vec![ConstValue {
                    symbol: constant("limit"),
                    text: "20".to_owned(),
                    truncated: true,
                    modules: Some(vec![path]),
                }],
            })),
        };
        let document = envelope(&result);
        let back =
            Envelope::parse(document.to_json().expect("serializes").as_bytes()).expect("parses");
        assert_eq!(back, document);
        assert_eq!(back.status, Status::Ok);
        assert_eq!(back.manifest, Some(WireHash(Hash128([1; 16]))));
        assert_eq!(back.front, Some(Front::Clean));
        let file = back.files.first().expect("a file");
        assert_eq!(file.path, "src/main.cheby");
        assert_eq!(file.hash, WireHash(Hash128([2; 16])));
        let Some(Section::Ok(section)) = back.sections.constants else {
            panic!("an ok section: {back:?}");
        };
        let wire = section.diagnostics.first().expect("a diagnostic");
        assert_eq!(wire.path, "src/main.cheby");
        assert_eq!(wire.to_plain(), Some(diagnostic));
        assert_eq!(
            wire.symbol.as_ref().and_then(SymbolData::to_symbol),
            Some(constant("boom"))
        );
        let value = section.values.first().expect("a value");
        assert_eq!(value.symbol.to_symbol(), Some(constant("limit")));
        assert_eq!(value.text, "20");
        assert!(value.truncated);
        // The reached modules travel as `/` paths (D-1510).
        assert_eq!(value.modules, Some(vec!["src/main.cheby".to_owned()]));
        assert_eq!(back.sections.ownership, None);
    }

    #[test]
    fn front_end_errors_skip_the_section() {
        let result = BackendResult {
            manifest: Hash128([1; 16]),
            files: Vec::new(),
            front: FrontOutcome::Errors,
            constants: Some(analysis::Section::Skipped("front-end errors".to_owned())),
        };
        let document = envelope(&result);
        assert_eq!(document.front, Some(Front::Errors));
        assert_eq!(
            document.sections.constants,
            Some(Section::Skipped {
                reason: "front-end errors".to_owned()
            })
        );
        let failed = BackendResult {
            constants: Some(analysis::Section::Failed("stopped".to_owned())),
            ..result
        };
        assert!(matches!(
            envelope(&failed).sections.constants,
            Some(Section::Failed { .. })
        ));
    }

    #[test]
    fn the_watchdog_returns_at_the_end_of_input() {
        drain(std::io::Cursor::new(vec![7_u8; 2000]));
        drain(std::io::empty());
    }
}
