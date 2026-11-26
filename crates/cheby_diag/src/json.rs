//! Diagnostics as JSON Lines, for `--format json` (§13.8, D-1944).
//!
//! Every line is one JSON object with a `type` field. A diagnostic's line
//! has the parts of the text form ([`crate::render()`]): severity, code,
//! message, the place of the first primary label, every label, the notes
//! and the helps, each help with its edits and whether it is a sure fix
//! ([`crate::Help::fix`]). A place is given as the text form gives it, by
//! line and column from 1 with columns in code points (D-421), and as byte
//! offsets into the file's text, which an edit needs. The keys are written
//! in one fixed order and the text is never colored, so the output is
//! deterministic (D-264).

use std::fmt::Write as _;

use crate::diagnostic::{Diagnostic, LabelStyle, Severity};
use crate::source::SourceMap;
use crate::text::Span;

/// One line for `diag`, with its line break. `severity` is the one that is
/// reported, which is an error for a denied warning (D-549).
#[must_use]
pub fn render_json(diag: &Diagnostic, sources: &SourceMap, severity: Severity) -> String {
    let mut out = String::from("{\"type\":\"diagnostic\"");
    field(&mut out, "severity", severity.as_str());
    field(&mut out, "code", &diag.code.to_string());
    field(&mut out, "message", &diag.message);
    if let Some(span) = diag.primary_span() {
        place(&mut out, span, sources);
    }
    out.push_str(",\"labels\":[");
    let mut first = true;
    for label in &diag.labels {
        // A label in a file the map does not hold has no place to give.
        if sources.file(label.span.file).is_none() {
            continue;
        }
        out.push_str(if first { "{" } else { ",{" });
        first = false;
        let start = out.len();
        place(&mut out, label.span, sources);
        // `place` starts with a comma, which an object's first key has not.
        out.remove(start);
        field(&mut out, "message", &label.message);
        let _ = write!(out, ",\"primary\":{}}}", label.style == LabelStyle::Primary);
    }
    out.push_str("],\"notes\":[");
    for (index, note) in diag.notes.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        string(&mut out, note);
    }
    out.push_str("],\"helps\":[");
    for (index, help) in diag.helps.iter().enumerate() {
        out.push_str(if index == 0 { "{" } else { ",{" });
        out.push_str("\"message\":");
        string(&mut out, &help.message);
        let _ = write!(out, ",\"fix\":{},\"edits\":[", help.fix);
        let mut first = true;
        for edit in &help.edits {
            if sources.file(edit.span.file).is_none() {
                continue;
            }
            out.push_str(if first { "{" } else { ",{" });
            first = false;
            let start = out.len();
            place(&mut out, edit.span, sources);
            out.remove(start);
            field(&mut out, "replacement", &edit.replacement);
            out.push('}');
        }
        out.push_str("]}");
    }
    out.push_str("]}\n");
    out
}

/// One line with a `type` and a `message`, with its line break: a tool error
/// is `error` and the help that follows one is `help` (D-442, D-1944).
#[must_use]
pub fn render_json_message(kind: &str, message: &str) -> String {
    render_json_object(kind, &[("message", json_string(message))])
}

/// A JSON object's line from keys and values that are already JSON, with
/// its line break, for the lines other crates build, such as a test's
/// result. [`json_string`] makes a string's value.
#[must_use]
pub fn render_json_object(kind: &str, fields: &[(&str, String)]) -> String {
    let mut out = String::from("{\"type\":");
    string(&mut out, kind);
    for (key, value) in fields {
        out.push(',');
        string(&mut out, key);
        out.push(':');
        out.push_str(value);
    }
    out.push_str("}\n");
    out
}

/// `text` as a JSON string, with its quotes.
#[must_use]
pub fn json_string(text: &str) -> String {
    let mut out = String::new();
    string(&mut out, text);
    out
}

/// `,"file":…,"line":…,"column":…,"end_line":…,"end_column":…,"start":…,"end":…`
/// for `span`; nothing for a file the map does not hold.
fn place(out: &mut String, span: Span, sources: &SourceMap) {
    let Some(file) = sources.file(span.file) else {
        return;
    };
    let start = file.line_col(span.range.start());
    let end = file.line_col(span.range.end());
    field(out, "file", file.path());
    let _ = write!(
        out,
        ",\"line\":{},\"column\":{},\"end_line\":{},\"end_column\":{},\"start\":{},\"end\":{}",
        start.line,
        start.col,
        end.line,
        end.col,
        span.range.start(),
        span.range.end(),
    );
}

/// `,"key":"value"`.
fn field(out: &mut String, key: &str, value: &str) {
    out.push(',');
    string(out, key);
    out.push(':');
    string(out, value);
}

/// Appends `text` as a JSON string. Control characters are escaped, and
/// every other character is written as it is, in UTF-8.
fn string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codes;
    use crate::text::{FileId, TextRange};

    fn sources() -> (SourceMap, FileId) {
        let mut map = SourceMap::new();
        let file = map
            .add_file("src/main.cheby", "let é = true\nlet b = 2\n")
            .unwrap();
        (map, file)
    }

    #[test]
    fn a_diagnostic_is_one_line_with_every_part() {
        let (map, file) = sources();
        // `true`: bytes 9 to 13, after the two bytes of `é`.
        let span = Span::new(file, TextRange::new(9, 13));
        let other = Span::new(file, TextRange::new(14, 17));
        let diag = Diagnostic::new(codes::UNRESOLVED_NAME, "cannot find value `true`")
            .with_primary(span, "not found")
            .with_secondary(other, "")
            .with_note("a \"note\"")
            .with_help("words only")
            .with_fix("write `True`", span, "True");
        assert_eq!(
            render_json(&diag, &map, Severity::Error),
            concat!(
                r#"{"type":"diagnostic","severity":"error","code":"E0042","#,
                r#""message":"cannot find value `true`","#,
                r#""file":"src/main.cheby","line":1,"column":9,"end_line":1,"end_column":13,"start":9,"end":13,"#,
                r#""labels":[{"file":"src/main.cheby","line":1,"column":9,"end_line":1,"end_column":13,"start":9,"end":13,"message":"not found","primary":true},"#,
                r#"{"file":"src/main.cheby","line":2,"column":1,"end_line":2,"end_column":4,"start":14,"end":17,"message":"","primary":false}],"#,
                r#""notes":["a \"note\""],"#,
                r#""helps":[{"message":"words only","fix":false,"edits":[]},"#,
                r#"{"message":"write `True`","fix":true,"edits":[{"file":"src/main.cheby","line":1,"column":9,"end_line":1,"end_column":13,"start":9,"end":13,"replacement":"True"}]}]}"#,
                "\n"
            )
        );
    }

    /// A diagnostic without a span has no place, and a denied warning is
    /// reported as an error under its own code (D-549).
    #[test]
    fn no_place_without_a_span_and_the_reported_severity() {
        let (map, _) = sources();
        let diag = Diagnostic::new(codes::TUPLE_SHAPE, "m");
        assert_eq!(
            render_json(&diag, &map, Severity::Error),
            "{\"type\":\"diagnostic\",\"severity\":\"error\",\"code\":\"W0001\",\"message\":\"m\",\
             \"labels\":[],\"notes\":[],\"helps\":[]}\n"
        );
    }

    #[test]
    fn strings_escape_what_json_requires() {
        assert_eq!(
            json_string("a\"b\\c\nd\te\r\u{1}\u{1b}é‸"),
            r#""a\"b\\c\nd\te\r\u0001\u001bé‸""#
        );
        assert_eq!(
            render_json_message("error", "could not find `cheby.toml`"),
            "{\"type\":\"error\",\"message\":\"could not find `cheby.toml`\"}\n"
        );
        assert_eq!(
            render_json_object(
                "summary",
                &[("passed", "3".to_owned()), ("name", json_string("x"))]
            ),
            "{\"type\":\"summary\",\"passed\":3,\"name\":\"x\"}\n"
        );
    }
}
