//! `textDocument/formatting` and `textDocument/rangeFormatting` (X §5.4),
//! from the live text (class `Text`, D-1367).
//!
//! Both run `cheby fmt`'s formatter on the whole text, with no options:
//! `FormattingOptions` are ignored (D-079). The formatter gets the name of
//! the document's package from the `DocText`, as `cheby fmt` reads it from
//! the manifest, so a package's own imports go in a block of their own
//! (D-1953). The answer is the line diff
//! against its output, one `TextEdit` per hunk, with LF line breaks, so a
//! CRLF text is converted as `cheby fmt` converts it (`ide::edits`).
//! Range formatting keeps the hunks that touch the
//! range's lines (D-1399), widened to their neighbours until what it
//! leaves out only reformats, and every hunk after eight rounds (D-1738). A text with a grammar error gets `null`, as
//! `cheby fmt` refuses it (§13.7), and a formatted text gets `[]`.
//!
//! A document that is not editable, such as a std source, never reaches
//! these handlers: the dispatch answers the route's fallback, `null`
//! (X §5.4).

use lsp_types::{DocumentFormattingParams, DocumentRangeFormattingParams, TextEdit};
use serde_json::Value;

use crate::features::ide::edits::{AtCap, TextChange, format_edits, format_touching_with};
use crate::features::{DocText, HandlerOutput, RequestCx, RequestError, params};

/// `textDocument/formatting`.
///
/// # Errors
///
/// `InvalidParams` when the parameters do not have the method's shape.
pub(crate) fn formatting(
    cx: &RequestCx<'_>,
    doc: &DocText,
    params_value: Value,
) -> Result<HandlerOutput, RequestError> {
    let _: DocumentFormattingParams = params(params_value)?;
    cx.cancel.check()?;
    answer(cx, doc, format_edits(&doc.text, doc.package.as_deref()))
}

/// `textDocument/rangeFormatting` (D-1399, D-1738).
///
/// # Errors
///
/// `InvalidParams` when the parameters do not have the method's shape,
/// and `Cancelled`.
pub(crate) fn range_formatting(
    cx: &RequestCx<'_>,
    doc: &DocText,
    params_value: Value,
) -> Result<HandlerOutput, RequestError> {
    let p: DocumentRangeFormattingParams = params(params_value)?;
    let range = doc.lines.text_range(&doc.text, p.range, cx.encoding);
    cx.cancel.check()?;
    let changes = format_touching_with(
        &doc.text,
        doc.package.as_deref(),
        &[range],
        AtCap::KeepEvery,
        cx.cancel,
    )?;
    answer(cx, doc, changes)
}

/// `changes` as the protocol's edits, or `null` for a text the formatter
/// refused.
fn answer(
    cx: &RequestCx<'_>,
    doc: &DocText,
    changes: Option<Vec<TextChange>>,
) -> Result<HandlerOutput, RequestError> {
    // The formatter and the diff ran; converting is cheap, but the
    // answer may be large (A §3.5).
    cx.cancel.check()?;
    let Some(changes) = changes else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    let edits: Vec<TextEdit> = changes
        .into_iter()
        .map(|change| TextEdit {
            range: doc.lines.range(&doc.text, change.range, cx.encoding),
            new_text: change.text,
        })
        .collect();
    serde_json::to_value(edits)
        .map(HandlerOutput::value)
        .map_err(|e| RequestError::Failed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use lsp_types::TextEdit;
    use serde_json::{Value, json};

    use super::{formatting, range_formatting};
    use crate::features::RequestError;
    use crate::features::handlers::symbols::tests::{doc, text_document, with_cx};
    use crate::text::lines::{PositionEncoding, apply_change};

    fn format(text: &str) -> Value {
        format_in(text, None)
    }

    /// `format` in a file of the package `package`.
    fn format_in(text: &str, package: Option<&str>) -> Value {
        let mut doc = doc(text);
        doc.package = package.map(str::to_owned);
        let p = json!({
            "textDocument": text_document(),
            "options": { "tabSize": 8, "insertSpaces": false }
        });
        with_cx(|cx| formatting(cx, &doc, p))
            .expect("an answer")
            .value
    }

    fn format_range(text: &str, start: (u32, u32), end: (u32, u32)) -> Value {
        format_range_in(text, None, start, end)
    }

    /// `format_range` in a file of the package `package`.
    fn format_range_in(
        text: &str,
        package: Option<&str>,
        start: (u32, u32),
        end: (u32, u32),
    ) -> Value {
        let mut doc = doc(text);
        doc.package = package.map(str::to_owned);
        let p = json!({
            "textDocument": text_document(),
            "range": {
                "start": { "line": start.0, "character": start.1 },
                "end": { "line": end.0, "character": end.1 }
            },
            "options": { "tabSize": 2, "insertSpaces": true }
        });
        with_cx(|cx| range_formatting(cx, &doc, p))
            .expect("an answer")
            .value
    }

    /// `text` with the protocol's `edits` applied, as a client does.
    fn apply(text: &str, edits: &Value) -> String {
        let edits: Vec<TextEdit> = serde_json::from_value(edits.clone()).expect("edits");
        let mut out = text.to_owned();
        for edit in edits.iter().rev() {
            let applied = apply_change(
                &mut out,
                Some(edit.range),
                &edit.new_text,
                PositionEncoding::Utf16,
            );
            assert!(applied, "{edit:?}");
        }
        out
    }

    fn cheby_fmt(text: &str) -> String {
        cheby_fmt::format_source(text, cheby_fmt::WIDTH).expect("formats")
    }

    /// Every `.cheby` file under `dir`, sorted.
    fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("a directory")
            .map(|e| e.expect("an entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "cheby") {
                out.push(path);
            }
        }
    }

    /// Formatting gives no edit on any example, in the example's package
    /// (plan W6's exit, D-1953). In CRLF the same file is converted to the
    /// example's own text (X §5.4).
    #[test]
    fn every_example_formats_to_no_edit() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
        let mut files = Vec::new();
        sources(&dir, &mut files);
        assert!(files.len() >= 32, "{} files", files.len());
        for file in files {
            let text = std::fs::read_to_string(&file).expect("a source");
            let package = cheby_driver::find_package_root(&file)
                .and_then(|root| cheby_driver::package_name(&root));
            let package = package.as_deref();
            assert_eq!(format_in(&text, package), json!([]), "{}", file.display());
            let crlf = text.replace('\n', "\r\n");
            assert_eq!(
                apply(&crlf, &format_in(&crlf, package)),
                text,
                "{} in CRLF",
                file.display()
            );
        }
    }

    const MESSY: &str = "import std::list\nimport std::io\n\nfn  main( ) {\n  let s =  \"é🙂\"\n  io::println(s)\n}\n\n\n\nfn f(xs: List<Int>) -> Int { list::length(xs) }\n";

    #[test]
    fn edits_reach_cheby_fmt_byte_for_byte() {
        let edits = format(MESSY);
        assert!(edits.as_array().is_some_and(|e| e.len() > 1), "{edits}");
        assert_eq!(apply(MESSY, &edits), cheby_fmt(MESSY));
    }

    /// X §5.4: CRLF is converted, as `cheby fmt` converts it.
    #[test]
    fn crlf_is_converted() {
        let crlf = MESSY.replace('\n', "\r\n");
        let edits = format(&crlf);
        for edit in edits.as_array().expect("edits") {
            let new_text = edit["newText"].as_str().expect("text");
            assert!(!new_text.contains('\r'), "{new_text:?}");
        }
        assert_eq!(apply(&crlf, &edits), cheby_fmt(MESSY));
    }

    #[test]
    fn a_grammar_error_gives_null() {
        assert_eq!(format("fn f() {\n  x.\n}\n"), Value::Null);
        assert_eq!(format_range("fn f( {\n", (0, 0), (0, 1)), Value::Null);
    }

    #[test]
    fn range_formatting_leaves_other_lines_alone() {
        // Line 10 is `fn f`. The imports and `main` stay as written.
        let edits = format_range(MESSY, (10, 3), (10, 3));
        let got = apply(MESSY, &edits);
        // The hunk that rewrites `fn f` also drops the extra blank lines
        // just above it, which no matching line separates from it.
        let want = MESSY.replace(
            "\n\n\n\nfn f(xs: List<Int>) -> Int { list::length(xs) }\n",
            "\n\nfn f(xs: List<Int>) -> Int {\n  list::length(xs)\n}\n",
        );
        assert_eq!(got, want);
        // `main`'s lines: a range that ends at a line's start leaves it.
        let edits = format_range(MESSY, (3, 0), (7, 0));
        let got = apply(MESSY, &edits);
        assert!(got.starts_with("import std::list\nimport std::io\n\nfn main() {\n  let s = "));
        assert!(got.ends_with("}\n\n\n\nfn f(xs: List<Int>) -> Int { list::length(xs) }\n"));
    }

    #[test]
    fn range_formatting_of_an_import_sorts_the_block() {
        let edits = format_range(MESSY, (1, 0), (1, 4));
        let got = apply(MESSY, &edits);
        assert!(got.starts_with("import std::io\nimport std::list\n\nfn  main( ) {\n"));
    }

    /// D-1953: in a file of the package `app`, its own imports come last,
    /// in a block of their own, as `cheby fmt` writes them in the package;
    /// in a file of no package `app` is another package. Range formatting
    /// one import moves every import the order moves, each with its
    /// comments, and the text it leaves formats as the whole text does.
    #[test]
    fn a_package_s_own_imports_come_last() {
        let text = "import app::util // mine\nimport json\n// about io\nimport std::io\n\nfn main() {\n  io::println(util::name() + json::x())\n}\n";
        let body = "\n\nfn main() {\n  io::println(util::name() + json::x())\n}\n";
        let own =
            format!("// about io\nimport std::io\n\nimport json\n\nimport app::util // mine{body}");
        let formatted = apply(text, &format_in(text, Some("app")));
        assert_eq!(formatted, own);
        let fmt =
            cheby_fmt::format_source_in(text, cheby_fmt::WIDTH, Some("app")).expect("formats");
        assert_eq!(fmt, own);
        assert_eq!(
            apply(text, &format(text)),
            format!("// about io\nimport std::io\n\nimport app::util // mine\nimport json{body}")
        );
        for line in 0..4 {
            let got = apply(
                text,
                &format_range_in(text, Some("app"), (line, 0), (line, 1)),
            );
            assert_eq!(
                apply(&got, &format_in(&got, Some("app"))),
                own,
                "line {line}: {got}"
            );
            assert!(
                got.contains("import app::util // mine\n"),
                "line {line}: {got}"
            );
            assert_eq!(got.matches("import").count(), 3, "line {line}: {got}");
        }
        // A range on an import that moves moves the section, not `main`'s
        // body.
        let messy = text.replace("  io::println(", "  io::println(  ");
        let got = apply(
            &messy,
            &format_range_in(&messy, Some("app"), (0, 0), (0, 1)),
        );
        assert!(
            got.starts_with(own.split("fn main").next().unwrap()),
            "{got}"
        );
        assert!(got.contains("  io::println(  util"), "{got}");
    }

    #[test]
    fn bad_params_are_invalid_params() {
        let doc = doc(MESSY);
        let got = with_cx(|cx| range_formatting(cx, &doc, json!({ "range": 1 })));
        assert!(matches!(got, Err(RequestError::InvalidParams(_))));
    }
}
