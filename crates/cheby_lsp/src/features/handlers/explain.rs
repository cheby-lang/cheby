//! `workspace/executeCommand` and `cheby/explain` (X §8.3, D-1400, D-1693).
//!
//! Both are `Workspace` requests: they name no document and read no
//! snapshot (D-1683). Of the server's commands (D-1378) only
//! [`EXPLAIN_COMMAND`] is served, so it is the only one `caps` advertises.
//!
//! `cheby.explain`, the Explain action's command, takes a code. For a
//! client with `window/showDocument` it writes the explanation that
//! `cheby explain` prints to `<code>.md` in `StdService::explain_dir`,
//! read-only, and asks the client to open it with `takeFocus` (D-1693).
//! When that directory cannot be written it writes under
//! `cheby-explain-<build id>` in the temporary directory, as std's sources
//! fall back. A client without `showDocument`, or a server that can write
//! neither, gets the code's summary and its page through
//! `window/showMessage` instead.
//!
//! `cheby/explain` answers `{ "code", "summary", "markdown" }` and writes
//! no file (D-1400). An unknown code is `InvalidParams` in both, worded as
//! `cheby explain` words it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use cheby_driver::analysis::codes::{self, Code, CodeInfo};
use lsp_types::{ExecuteCommandParams, MessageType};
use serde_json::{Value, json};

use crate::features::stdlib::StdService;
use crate::features::{Analyzed, HandlerOutput, RequestCx, RequestError, params};
use crate::server::Effect;
use crate::text::uri::path_to_uri;

/// The command of the Explain action, with the code as its one argument
/// (X §8.3, D-1378).
pub(crate) const EXPLAIN_COMMAND: &str = "cheby.explain";

/// The page that explains a code, as `codeDescription.href` names it
/// (X §8.2): the same text as `server::publish`'s, which is private to the
/// server.
const DIAGNOSTICS_URL: &str = "https://github.com/cheby-lang/cheby/blob/main/docs/diagnostics/";

/// `workspace/executeCommand`: `cheby.explain` (module doc).
///
/// # Errors
///
/// `InvalidParams` when the parameters do not have the method's shape,
/// name another command, or name no registered code.
pub(crate) fn execute_command(
    cx: &RequestCx<'_>,
    _packages: &[&Analyzed],
    params_value: Value,
) -> Result<HandlerOutput, RequestError> {
    let command: ExecuteCommandParams = params(params_value)?;
    if command.command != EXPLAIN_COMMAND {
        return Err(RequestError::InvalidParams(format!(
            "`workspace/executeCommand`: the server has no command `{}`",
            command.command
        )));
    }
    let code = command
        .arguments
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RequestError::InvalidParams(format!("`{EXPLAIN_COMMAND}`: no code that is a string"))
        })?;
    let info = registered(code)?;
    let opened = cx
        .caps
        .show_document
        .then(|| written(cx.std, info))
        .flatten()
        .and_then(|path| path_to_uri(&path).ok());
    let effect = match opened {
        Some(uri) => Effect::Request {
            method: "window/showDocument",
            params: json!({ "uri": uri.as_str(), "takeFocus": true }),
        },
        None => Effect::show(MessageType::INFO, summary_line(info)),
    };
    Ok(HandlerOutput {
        value: Value::Null,
        effects: vec![effect],
    })
}

/// `cheby/explain`, whose parameters are `{ "code": "E0044" }` (X §8.3).
///
/// # Errors
///
/// `InvalidParams` when the parameters have no `code` string, or it is no
/// registered code.
pub(crate) fn explain(
    _cx: &RequestCx<'_>,
    _packages: &[&Analyzed],
    params_value: Value,
) -> Result<HandlerOutput, RequestError> {
    let explain: serde_json::Map<String, Value> = params(params_value)?;
    let code = explain.get("code").and_then(Value::as_str).ok_or_else(|| {
        RequestError::InvalidParams("`cheby/explain`: no `code` that is a string".to_owned())
    })?;
    let info = registered(code)?;
    Ok(HandlerOutput::value(json!({
        "code": info.code.to_string(),
        "summary": info.summary,
        "markdown": info.explanation,
    })))
}

/// The registered code `text` names.
///
/// # Errors
///
/// `InvalidParams` worded as `cheby explain` words it.
fn registered(text: &str) -> Result<&'static CodeInfo, RequestError> {
    Code::parse(text)
        .and_then(codes::lookup)
        .ok_or_else(|| RequestError::InvalidParams(format!("no diagnostic has the code `{text}`")))
}

/// What a client without `showDocument` is shown: the code, its summary
/// and its page.
fn summary_line(info: &CodeInfo) -> String {
    format!(
        "{}: {}. See {DIAGNOSTICS_URL}{}.md",
        info.code, info.summary, info.code
    )
}

/// The file that holds `info`'s explanation, written if it was not:
/// under `StdService::explain_dir`, else under the temporary directory's
/// `cheby-explain-<build id>` (D-1693). `None` when neither can be
/// written, or the server has no std.
fn written(std: &StdService, info: &CodeInfo) -> Option<PathBuf> {
    let dir = std.explain_dir()?;
    if let Ok(path) = write_in(&dir, info) {
        return Some(path);
    }
    let name = dir.file_name()?.to_str()?;
    let build = name.strip_prefix(TEMP_PREFIX).unwrap_or(name);
    let fallback = std::env::temp_dir().join(format!("{TEMP_PREFIX}{build}"));
    (fallback != dir)
        .then(|| write_in(&fallback, info).ok())
        .flatten()
}

/// The start of the fallback directory's name, before the build id
/// (`features::stdlib`).
const TEMP_PREFIX: &str = "cheby-explain-";

/// Writes `info`'s explanation to `<dir>/<code>.md`, read-only, unless
/// it holds that text already. The text goes to a file of its own first
/// and is then renamed into place, so a reader never sees half of it.
///
/// # Errors
///
/// Any error of the file system.
fn write_in(dir: &Path, info: &CodeInfo) -> io::Result<PathBuf> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let path = dir.join(format!("{}.md", info.code));
    if holds(&path, info) {
        return Ok(path);
    }
    std::fs::create_dir_all(dir)?;
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let part = dir.join(format!(".{}.md.{}.{n}", info.code, std::process::id()));
    let moved = std::fs::write(&part, info.explanation)
        .and_then(|()| {
            let mut permissions = std::fs::metadata(&part)?.permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(&part, permissions)
        })
        .and_then(|()| std::fs::rename(&part, &path));
    if let Err(error) = moved {
        let _ = std::fs::remove_file(&part);
        // Another request may have written it meanwhile.
        if !holds(&path, info) {
            return Err(error);
        }
    }
    Ok(path)
}

/// Whether the file at `path` holds `info`'s explanation.
fn holds(path: &Path, info: &CodeInfo) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| text == info.explanation)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use cheby_driver::analysis::codes;

    use super::{registered, summary_line, write_in};
    use crate::features::RequestError;

    /// A scratch directory, removed on drop.
    struct Dir(PathBuf);

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = make_writable(&self.0);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Clears the read-only flag of every file in `dir`, which Windows
    /// needs before it removes them.
    fn make_writable(dir: &std::path::Path) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let mut permissions = std::fs::metadata(&path)?.permissions();
            #[expect(
                clippy::permissions_set_readonly_false,
                reason = "a test's scratch files are removed"
            )]
            permissions.set_readonly(false);
            std::fs::set_permissions(&path, permissions)?;
        }
        Ok(())
    }

    /// The explanation is written read-only, with the text `cheby explain`
    /// prints, once: a second write finds it and keeps it.
    #[test]
    fn the_explanation_is_written_once_and_read_only() {
        let dir =
            Dir(std::env::temp_dir().join(format!("cheby_lsp_explain_{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&dir.0);
        let info = codes::lookup(codes::SHADOWED_NAME).unwrap();
        let path = write_in(&dir.0.join("build"), info).unwrap();
        assert_eq!(path.file_name().unwrap(), "E0044.md");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            codes::explanation(codes::SHADOWED_NAME).unwrap()
        );
        assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
        assert_eq!(write_in(&dir.0.join("build"), info).unwrap(), path);
        let left: Vec<_> = std::fs::read_dir(dir.0.join("build")).unwrap().collect();
        assert_eq!(left.len(), 1, "no part file is left");
        let _ = make_writable(&dir.0.join("build"));
    }

    /// A code is read as `cheby explain` reads it, and an unknown one is
    /// refused in its words.
    #[test]
    fn codes_are_read_as_cheby_explain_reads_them() {
        assert_eq!(registered("E0044").unwrap().code, codes::SHADOWED_NAME);
        assert_eq!(
            registered("E9999"),
            Err(RequestError::InvalidParams(
                "no diagnostic has the code `E9999`".to_owned()
            ))
        );
        assert!(registered("e0044").is_err());
        let line = summary_line(registered("W0002").unwrap());
        assert_eq!(
            line,
            "W0002: unused local binding or parameter. See \
             https://github.com/cheby-lang/cheby/blob/main/docs/diagnostics/W0002.md"
        );
    }
}
