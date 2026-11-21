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
    published_with(name, main, &[], extra)
}

/// [`published`] in a package that also holds `others`.
fn published_with(name: &str, main: &str, others: &[(&str, &str)], extra: &Value) -> Value {
    let mut files = vec![("src/main.cheby", main)];
    files.extend_from_slice(others);
    let package = Temp::package(name, &files);
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

const UNUSED: &str = "//! Root.\n\nfn main() {\n  let total = 1\n}\n";

const DEPRECATED_USE: &str =
    "//! Root.\n\nimport app::text\n\nfn main() {\n  let _ = text::old()\n}\n";

const DEPRECATED: &str = "//! Text.\n\n@deprecated(\"use new\")\npub fn old() -> Int {\n  1\n}\n";

/// A client whose `textDocument.publishDiagnostics` is `publish` and whose
/// `textDocument.codeAction` is `actions`, with `settings` as its
/// `initializationOptions`.
fn client_with(publish: &Value, actions: &Value, settings: &Value) -> Value {
    let mut options = json!({ "analysis": { "debounceMs": 0 } });
    if let (Some(options), Some(settings)) = (options.as_object_mut(), settings.as_object()) {
        for (key, value) in settings {
            options.insert(key.clone(), value.clone());
        }
    }
    json!({
        "capabilities": {
            "textDocument": { "publishDiagnostics": publish, "codeAction": actions },
        },
        "initializationOptions": options,
    })
}

// D-1400, X §8.2: `codeDescription` links a registered code's explanation
// when the client shows it and `diagnostics.explainLinks` is on, its
// default.
#[test]
fn code_description_follows_the_setting_and_the_capability() {
    let shows = json!({ "codeDescriptionSupport": true });
    let d = published(
        "cd_on",
        UNUSED,
        &client_with(&shows, &json!({}), &json!({})),
    );
    assert_eq!(d["code"], "W0002", "{d}");
    assert_eq!(
        d["codeDescription"]["href"],
        "https://github.com/cheby-lang/cheby/blob/main/docs/diagnostics/W0002.md"
    );
    let off = json!({ "diagnostics": { "explainLinks": false } });
    let d = published("cd_setting", UNUSED, &client_with(&shows, &json!({}), &off));
    assert!(d.get("codeDescription").is_none_or(Value::is_null), "{d}");
    let d = published(
        "cd_client",
        UNUSED,
        &client_with(&json!({}), &json!({}), &json!({})),
    );
    assert!(d.get("codeDescription").is_none_or(Value::is_null), "{d}");
}

// D-1750: a change of `diagnostics.explainLinks` publishes the lists again
// at once, with no run and no edit between, and they follow the setting.
#[test]
fn a_change_of_explain_links_republishes() {
    let shows = json!({ "codeDescriptionSupport": true });
    let package = Temp::package("cd_change", &[("src/main.cheby", UNUSED)]);
    let path = package.at("src/main.cheby");
    let (mut client, _) = Client::start(
        package.params(&client_with(&shows, &json!({}), &json!({}))),
        Vec::new(),
    );
    let linked = |publish: &Value| {
        publish["diagnostics"]
            .as_array()
            .is_some_and(|list| list.iter().any(|d| !d["codeDescription"].is_null()))
    };
    let first = client.wait_publish(&path, has_diagnostics);
    assert!(linked(&first), "{first}");
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "cheby": { "diagnostics": { "explainLinks": false } } } }),
    );
    let off = client.wait_publish(&path, |p| has_diagnostics(p) && !linked(p));
    assert_eq!(off["version"], first["version"], "{off}");
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "cheby": { "diagnostics": { "explainLinks": true } } } }),
    );
    let on = client.wait_publish(&path, |p| has_diagnostics(p) && linked(p));
    assert_eq!(on["diagnostics"], first["diagnostics"]);
    client.stop();
}

// X §8.1: W0002 to W0005 and W0010 are tagged `UNNECESSARY` and W0012
// `DEPRECATED`, each only for a client that lists the tag.
#[test]
fn tags_follow_the_code_and_the_client() {
    let both = json!({ "tagSupport": { "valueSet": [1, 2] } });
    let d = published(
        "tag_unused",
        UNUSED,
        &client_with(&both, &json!({}), &json!({})),
    );
    assert_eq!(d["tags"], json!([1]), "{d}");
    let deprecated_only = json!({ "tagSupport": { "valueSet": [2] } });
    let d = published(
        "tag_unlisted",
        UNUSED,
        &client_with(&deprecated_only, &json!({}), &json!({})),
    );
    assert!(d.get("tags").is_none_or(Value::is_null), "{d}");
    let text = [("src/text.cheby", DEPRECATED)];
    let d = published_with(
        "tag_deprecated",
        DEPRECATED_USE,
        &text,
        &client_with(&deprecated_only, &json!({}), &json!({})),
    );
    assert_eq!(d["code"], "W0012", "{d}");
    assert_eq!(d["tags"], json!([2]), "{d}");
    let none = client_with(&json!({}), &json!({}), &json!({}));
    let d = published_with("tag_none", DEPRECATED_USE, &text, &none);
    assert!(d.get("tags").is_none_or(Value::is_null), "{d}");
}

// D-1686: a help whose edits are a quick fix leaves the message for a
// client that takes code-action literals, and stays as `help: …` for any
// other; a help with no edit stays for every client.
#[test]
fn a_fix_help_leaves_the_message_for_a_literal_client() {
    let literals = json!({
        "codeActionLiteralSupport": { "codeActionKind": { "valueSet": ["quickfix"] } },
    });
    let d = published(
        "help_literal",
        UNUSED,
        &client_with(&json!({}), &literals, &json!({})),
    );
    assert_eq!(d["message"], "unused binding `total`\nnever used", "{d}");
    let d = published(
        "help_plain",
        UNUSED,
        &client_with(&json!({}), &json!({}), &json!({})),
    );
    assert_eq!(
        d["message"],
        "unused binding `total`\nnever used\n\
         help: if this is intentional, start the name with an underscore: `_total`"
    );
    let d = published(
        "help_text",
        REFUTABLE_LET,
        &client_with(&json!({}), &literals, &json!({})),
    );
    assert_eq!(d["code"], "E0060", "{d}");
    assert!(
        d["message"]
            .as_str()
            .unwrap()
            .contains("\nhelp: use `case`"),
        "a help with no edit stays: {d}"
    );
}

// D-1754: E0006's help, whose fix on a used name is a rename checked on
// resolve, leaves the message only for a client that resolves `edit`; a
// client with literals that cannot keeps the line, and gets no fix.
#[test]
fn e0006_keeps_its_help_for_a_client_that_cannot_resolve() {
    const CAMEL: &str = "//! Root.\n\nfn main() {\n  let maxValue = 3\n  assert maxValue == 3\n}\n";
    let literals = json!({
        "codeActionLiteralSupport": { "codeActionKind": { "valueSet": ["quickfix"] } },
    });
    let d = published(
        "e6_no_resolve",
        CAMEL,
        &client_with(&json!({}), &literals, &json!({})),
    );
    assert_eq!(d["code"], "E0006", "{d}");
    assert!(
        d["message"]
            .as_str()
            .unwrap()
            .contains("\nhelp: write `max_value`"),
        "the help stays: {d}"
    );
    let resolving = json!({
        "codeActionLiteralSupport": { "codeActionKind": { "valueSet": ["quickfix"] } },
        "dataSupport": true,
        "resolveSupport": { "properties": ["edit"] },
    });
    let d = published(
        "e6_resolve",
        CAMEL,
        &client_with(&json!({}), &resolving, &json!({})),
    );
    assert!(
        !d["message"].as_str().unwrap().contains("help:"),
        "the help leaves: {d}"
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
