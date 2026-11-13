//! The completion handlers: capabilities, the protocol form of items, the
//! stray trigger, and resolve. Tests that analyze run only the front end,
//! so they run on Windows too (D-1516).

use serde_json::{Value, json};

use super::{CompletionCaps, completion, resolve};
use crate::features::Analyzed;
use crate::features::handlers::symbols::tests::with_cx;
use crate::features::ide::completion::StdAccess;
use crate::features::ide::completion::tests::GEO;
use crate::features::ide::hover::tests::{Temp, analyze};
use crate::text::uri::path_to_uri;

/// Every capability on.
const CAPS: CompletionCaps = CompletionCaps {
    snippets: true,
    label_details: true,
    deprecated_tag: true,
    deprecated_flag: true,
    markdown: true,
    zed: false,
    adjust_indentation: true,
};

const MAIN: &str = "import app::geo\n\nfn main() {\n  let é = 1\n  let y = \n}\n";

/// The package of `GEO` and `MAIN`, analyzed, with `MAIN`'s URI.
fn package() -> (Temp, Analyzed, String) {
    package_with(MAIN)
}

/// The package of `GEO` and `main` as `src/main.cheby`, analyzed, with
/// `main`'s URI.
fn package_with(main: &str) -> (Temp, Analyzed, String) {
    let temp = Temp::package("handler", &[("geo.cheby", GEO), ("main.cheby", main)]);
    let path = temp.src("main.cheby");
    let analyzed = analyze(&temp.0, &[(path.clone(), main)]);
    let uri = path_to_uri(&path).unwrap().as_str().to_owned();
    (temp, analyzed, uri)
}

/// The answer at `line`:`character` of `uri`, with `trigger` as the
/// trigger character if any.
fn ask(
    analyzed: &Analyzed,
    uri: &str,
    (line, character): (u32, u32),
    trigger: Option<&str>,
    caps: CompletionCaps,
) -> Value {
    let context = trigger.map_or_else(
        || json!({ "triggerKind": 1 }),
        |t| json!({ "triggerKind": 2, "triggerCharacter": t }),
    );
    let p = json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character },
        "context": context,
    });
    with_cx(|cx| completion(cx, analyzed, p, caps, StdAccess::None))
        .unwrap()
        .value
}

/// The item labelled `label` of `answer`.
fn item<'a>(answer: &'a Value, label: &str) -> &'a Value {
    answer["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["label"] == label)
        .unwrap_or_else(|| panic!("no `{label}` in {answer}"))
}

#[test]
fn capabilities_are_read_from_initialize() {
    let caps = CompletionCaps::from_params(&json!({
        "clientInfo": { "name": "Zed", "version": "1.27.0" },
        "capabilities": { "textDocument": { "completion": { "completionItem": {
            "snippetSupport": true,
            "labelDetailsSupport": true,
            "tagSupport": { "valueSet": [1] },
            "documentationFormat": ["plaintext", "markdown"],
            "insertTextModeSupport": { "valueSet": [1, 2] },
        } } } },
    }));
    assert_eq!(
        caps,
        CompletionCaps {
            snippets: true,
            label_details: true,
            deprecated_tag: true,
            deprecated_flag: false,
            markdown: true,
            zed: true,
            adjust_indentation: true,
        }
    );
    // `asIs` alone is not `adjustIndentation` (D-1506).
    let as_is = CompletionCaps::from_params(&json!({
        "capabilities": { "textDocument": { "completion": { "completionItem": {
            "insertTextModeSupport": { "valueSet": [1] },
        } } } },
    }));
    assert!(!as_is.adjust_indentation);
    // Every release channel of Zed is Zed.
    for name in ["Zed Preview", "Zed Nightly", "Zed Dev", "zed"] {
        let caps = CompletionCaps::from_params(&json!({ "clientInfo": { "name": name } }));
        assert!(caps.zed, "{name}");
    }
    for name in ["Zedit", "VS Code"] {
        let caps = CompletionCaps::from_params(&json!({ "clientInfo": { "name": name } }));
        assert!(!caps.zed, "{name}");
    }
    assert_eq!(
        CompletionCaps::from_params(&json!({})),
        CompletionCaps::default()
    );
}

#[test]
fn items_in_protocol_form() {
    let (_temp, analyzed, uri) = package();
    let answer = ask(&analyzed, &uri, (4, 10), None, CAPS);
    // No partial word in an expression, with auto-import on: the imports
    // start at the second character, so the list is incomplete (D-1487).
    assert_eq!(answer["isIncomplete"], json!(true));
    // A local after a name that is not ASCII: UTF-16 columns.
    let local = item(&answer, "é");
    assert_eq!(local["kind"], json!(6));
    assert_eq!(local["sortText"], json!("100000é"));
    assert_eq!(
        local["textEdit"],
        json!({
            "range": {
                "start": { "line": 4, "character": 10 },
                "end": { "line": 4, "character": 10 },
            },
            "newText": "é",
        })
    );
    assert_eq!(local["detail"], json!("Int"));
    assert!(local.get("labelDetails").is_none());
    assert_eq!(
        item(&answer, "main")["labelDetails"],
        json!({ "detail": "()" })
    );
    // A module, with no data to resolve.
    let geo = item(&answer, "geo");
    assert_eq!(geo["textEdit"]["newText"], json!("geo::"));
    assert!(geo.get("data").is_none());
    // A snippet.
    let case = item(&answer, "case");
    assert_eq!(case["kind"], json!(15));
    assert_eq!(case["insertTextFormat"], json!(2));
    // Without label details or snippets.
    let plain = ask(
        &analyzed,
        &uri,
        (4, 10),
        None,
        CompletionCaps {
            snippets: false,
            label_details: false,
            ..CAPS
        },
    );
    assert!(item(&plain, "main").get("labelDetails").is_none());
    assert_eq!(item(&plain, "case")["kind"], json!(14));
    assert!(item(&plain, "case").get("insertTextFormat").is_none());
    // A keyword snippet spans lines, so it asks the client to indent them
    // as its first (D-1506).
    assert_eq!(case["insertTextMode"], json!(2));
    // One line needs no mode.
    assert!(local.get("insertTextMode").is_none());
}

#[test]
fn text_on_several_lines_asks_for_adjusted_indentation() {
    // D-1506: the postfix `case` is written relative to its first line,
    // and a client that lists `adjustIndentation` is told to indent its
    // later lines as that one. `asIs` is never sent.
    let main = "fn main() {\n  let o = Some(1)\n  let y = o.\n}\n";
    let (_temp, analyzed, uri) = package_with(main);
    let answer = ask(&analyzed, &uri, (2, 12), Some("."), CAPS);
    let case = item(&answer, "case");
    assert_eq!(case["insertTextMode"], json!(2));
    assert_eq!(
        case["textEdit"]["newText"],
        json!("case o {\n  Some(_) => ${1:todo}\n  None => ${2:todo}\n}$0")
    );
    let items = answer["items"].as_array().unwrap();
    assert!(
        items.iter().all(|i| i["insertTextMode"] != json!(1)),
        "{answer}"
    );
    // A client that does not list `adjustIndentation` gets no mode.
    let answer = ask(
        &analyzed,
        &uri,
        (2, 12),
        Some("."),
        CompletionCaps {
            adjust_indentation: false,
            ..CAPS
        },
    );
    let items = answer["items"].as_array().unwrap();
    assert!(
        items.iter().all(|i| i.get("insertTextMode").is_none()),
        "{answer}"
    );
    // Nor do keyword snippets.
    let (_temp, analyzed, uri) = package();
    let caps = CompletionCaps {
        adjust_indentation: false,
        ..CAPS
    };
    let answer = ask(&analyzed, &uri, (4, 10), None, caps);
    assert!(item(&answer, "case").get("insertTextMode").is_none());
}

#[test]
fn items_carry_their_key_and_deprecation() {
    let (_temp, analyzed, uri) = package_with(&MAIN.replace("let y = ", "let y = geo::"));
    let answer = ask(&analyzed, &uri, (4, 15), Some(":"), CAPS);
    let add = item(&answer, "add");
    assert_eq!(
        add["data"],
        json!({ "module": "app::geo", "ns": "value", "name": "add", "origin": uri })
    );
    assert_eq!(
        add["detail"],
        json!("pub fn add(a: geo::Point, b: geo::Point) -> geo::Point")
    );
    assert_eq!(
        add["labelDetails"],
        json!({ "detail": "(a, b)", "description": "geo" })
    );
    assert_eq!(item(&answer, "plus")["tags"], json!([1]));
    assert!(item(&answer, "plus").get("deprecated").is_none());
    let flag_only = ask(
        &analyzed,
        &uri,
        (4, 15),
        Some(":"),
        CompletionCaps {
            deprecated_tag: false,
            ..CAPS
        },
    );
    assert_eq!(item(&flag_only, "plus")["deprecated"], json!(true));
}

#[test]
fn a_stray_trigger_answers_an_empty_list() {
    let (_temp, analyzed, uri) = package();
    // `let y = ` ends in a space, so neither `::` nor `|>` is completed.
    for trigger in [":", ">"] {
        let answer = ask(&analyzed, &uri, (4, 10), Some(trigger), CAPS);
        assert_eq!(answer, json!({ "isIncomplete": false, "items": [] }));
    }
    let answer = ask(&analyzed, &uri, (4, 10), Some("."), CAPS);
    assert_ne!(answer["items"], json!([]));
    let other = json!({
        "textDocument": { "uri": "file:///nowhere/main.cheby" },
        "position": { "line": 0, "character": 0 },
    });
    let answer = with_cx(|cx| completion(cx, &analyzed, other, CAPS, StdAccess::None));
    assert_eq!(answer.unwrap().value, Value::Null);
    let malformed = with_cx(|cx| completion(cx, &analyzed, json!(1), CAPS, StdAccess::None));
    assert!(malformed.is_err());
}

#[test]
fn resolve_adds_the_docs() {
    let (_temp, analyzed, uri) = package();
    let packages = [&analyzed];
    let run = |item: Value, caps: CompletionCaps| {
        with_cx(|cx| resolve(cx, &packages, item, caps, StdAccess::None))
            .unwrap()
            .value
    };
    let item = json!({
        "label": "add",
        "data": { "module": "app::geo", "ns": "value", "name": "add", "origin": uri },
    });
    let resolved = run(item.clone(), CAPS);
    assert_eq!(
        resolved["documentation"],
        json!({ "kind": "markdown", "value": "Adds two points." })
    );
    let plain = run(
        item,
        CompletionCaps {
            markdown: false,
            ..CAPS
        },
    );
    assert_eq!(plain["documentation"], json!("Adds two points."));
    // An item no package holds now, or with no data, comes back as it was.
    let gone = json!({
        "label": "gone",
        "data": { "module": "app::geo", "ns": "value", "name": "gone", "origin": uri },
    });
    assert_eq!(run(gone.clone(), CAPS), gone);
    let bare = json!({ "label": "let", "kind": 14 });
    assert_eq!(run(bare.clone(), CAPS), bare);
    let malformed = with_cx(|cx| resolve(cx, &packages, json!(1), CAPS, StdAccess::None));
    assert!(malformed.is_err());
}

#[test]
fn bare_fences_are_marked_as_cheby() {
    assert_eq!(
        super::cheby_fences("Text.\n\n```\nf(1)\n```\n\n```text\nx\n```"),
        "Text.\n\n```cheby\nf(1)\n```\n\n```text\nx\n```"
    );
}
