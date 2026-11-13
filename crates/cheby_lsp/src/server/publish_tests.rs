//! Tests of the published message (D-1509): `cheby check`'s text without
//! its snippet, through `run_on`, for a client with `relatedInformation`
//! and one without it.

use std::path::PathBuf;

use serde_json::{Value, json};

use super::child_line;
use crate::server::tests::{Client, Temp};

/// A client that takes `relatedInformation`.
fn related_client() -> Value {
    json!({
        "capabilities": {
            "textDocument": { "publishDiagnostics": { "relatedInformation": true } },
        },
    })
}

/// The one diagnostic published for `src/main.cheby` of a package holding
/// `main`, as the first publish with an error gives it, for a client with
/// `extra` in its `initialize` parameters.
fn published(name: &str, main: &str, extra: &Value) -> Value {
    let package = Temp::package(name, &[("src/main.cheby", main)]);
    let path = package.at("src/main.cheby");
    let (mut client, _) = Client::start(package.params(extra), Vec::new());
    let publish = client.wait_publish(&path, has_diagnostics);
    client.stop();
    let diagnostics = publish["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 1, "{publish}");
    diagnostics[0].clone()
}

fn has_diagnostics(publish: &Value) -> bool {
    publish["diagnostics"]
        .as_array()
        .is_some_and(|list| !list.is_empty())
}

/// The related label's path, given by its segments, as `related_line`
/// shows it: with the platform's separator, since the driver joins the
/// segments (`src\main.cheby` on Windows, D-1516).
fn shown(segments: &[&str]) -> String {
    segments.iter().collect::<PathBuf>().display().to_string()
}

const RETURN_MISMATCH: &str =
    "//! Root.\n\nfn answer() -> Int {\n  \"forty-two\"\n}\n\nfn main() {\n  let _ = answer()\n}\n";

const PATTERN_MISMATCH: &str = "//! Root.\n\nfn name(n: Int) -> String {\n  case n {\n    \"one\" => \"one\"\n    _ => \"other\"\n  }\n}\n\nfn main() {\n  let _ = name(1)\n}\n";

const REFUTABLE_LET: &str = "//! Root.\n\nfn first(o: Option<Int>) -> Int {\n  let Some(x) = o\n  x\n}\n\nfn main() {\n  let _ = first(None)\n}\n";

const CALL_IN_INTERPOLATION: &str = "//! Root.\n\nimport std::io\n\nfn main() {\n  io::println(\"{f()}\")\n}\n\nfn f() -> Int {\n  1\n}\n";

// D-1509: the primary label follows the header, and a related label stays
// in `relatedInformation` for a client that takes it.
#[test]
fn primary_label_follows_the_header() {
    let d = published("msg_label", RETURN_MISMATCH, &related_client());
    assert_eq!(d["code"], "E0047", "{d}");
    assert_eq!(
        d["message"],
        "mismatched types\nexpected `Int`, found `String`"
    );
    let related = d["relatedInformation"].as_array().unwrap();
    assert_eq!(related.len(), 1, "{d}");
    assert_eq!(
        related[0]["message"],
        "expected because of this return type"
    );
}

// D-1509: notes, then helps, each on its own line after the label, in
// `cheby check`'s order.
#[test]
fn notes_and_helps_follow_the_label() {
    let d = published("msg_note_help", REFUTABLE_LET, &related_client());
    assert_eq!(d["code"], "E0060", "{d}");
    assert_eq!(
        d["message"],
        "refutable pattern in `let`\n\
         `None` not covered\n\
         note: this pattern must match every value of its type (§6.1)\n\
         help: use `case` to handle `None` as well, or `let assert` to panic when the value does \
         not match"
    );
    assert!(
        d.get("relatedInformation").is_none_or(Value::is_null),
        "{d}"
    );
}

// D-1509: an empty primary label adds no line.
#[test]
fn empty_primary_label_adds_no_line() {
    let d = published("msg_empty_label", CALL_IN_INTERPOLATION, &related_client());
    assert_eq!(d["code"], "E0012", "{d}");
    assert_eq!(
        d["message"],
        "unexpected `(` in string interpolation\n\
         help: an interpolation holds a name, optionally with fields and `:?`, such as \
         `{user.name}`; bind other values to a name first"
    );
}

// D-1509: for a client without `relatedInformation`, the related lines sit
// between the primary label and the notes.
#[test]
fn related_lines_come_before_notes_without_related_information() {
    let d = published("msg_no_related", PATTERN_MISMATCH, &json!({}));
    assert_eq!(d["code"], "E0047", "{d}");
    let related = shown(&["src", "main.cheby"]);
    assert_eq!(
        d["message"],
        format!(
            "mismatched types\n\
             expected `Int`, found `String`\n\
             {related}:4:8: this subject has type `Int`\n\
             note: a pattern matches values of the type of its subject"
        )
    );
    assert!(
        d.get("relatedInformation").is_none_or(Value::is_null),
        "{d}"
    );
}

// D-1509: a note of several lines keeps `cheby check`'s layout, its later
// lines under the first line's text.
#[test]
fn a_note_of_several_lines_is_indented_under_its_text() {
    assert_eq!(child_line("note", "one"), "\nnote: one");
    assert_eq!(
        child_line("note", "first line \nsecond line \n"),
        "\nnote: first line \n      second line\n"
    );
    assert_eq!(child_line("help", "a\nb"), "\nhelp: a\n      b");
}
