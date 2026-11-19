//! Tests of the `textDocument/inlayHint` handler: the protocol shape, the
//! range, and the settings. Each test runs only the front end, so it runs
//! on Windows too (D-1516).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use cheby_driver::analysis::{ModulePath, Symbol, SymbolNs, normalized_key};
use serde_json::{Value, json};

use super::inlay_hint;
use crate::features::CancelToken;
use crate::features::RequestCx;
use crate::features::ide::hover::tests::{Temp, analyze};
use crate::server::{ClientCaps, ConstValues, Settings};
use crate::text::lines::PositionEncoding;
use crate::text::uri::path_to_uri;

const TEXT: &str = "fn main() {\n  let a = [1]\n  let b = [a]\n  assert b != []\n}\n";

/// The answer to `textDocument/inlayHint` over `range` of [`TEXT`], with
/// `settings`.
fn answer(settings: &Settings, range: &Value) -> Value {
    answer_on(TEXT, settings, range)
}

/// The answer to `textDocument/inlayHint` over `range` of `text`, with
/// `settings`.
fn answer_on(text: &str, settings: &Settings, range: &Value) -> Value {
    answer_with(text, settings, range, |_| None)
}

/// [`answer_on`] with the constant values that `values` makes from the
/// absolute path of `src/main.cheby`.
fn answer_with(
    text: &str,
    settings: &Settings,
    range: &Value,
    values: impl FnOnce(&Path) -> Option<ConstValues>,
) -> Value {
    let temp = Temp::package("inlay", &[("main.cheby", text)]);
    let path = temp.src("main.cheby");
    let analyzed = analyze(&temp.0, &[(path.clone(), text)]);
    let constants = values(&path).map(Arc::new);
    let uri = path_to_uri(&path).unwrap();
    let caps = ClientCaps::default();
    let cancel = CancelToken::new();
    let cx = RequestCx {
        settings,
        caps: &caps,
        encoding: PositionEncoding::Utf16,
        cancel: &cancel,
        std: &crate::features::stdlib::StdService::none(),
        uris: &crate::text::uri::UriMap::new(),
        constants,
    };
    let params = json!({ "textDocument": { "uri": uri.as_str() }, "range": range });
    inlay_hint(&cx, &analyzed, params).unwrap().value
}

/// The range of lines `from` to `to`, from column 0 to column 0.
fn lines(from: u32, to: u32) -> Value {
    json!({
        "start": { "line": from, "character": 0 },
        "end": { "line": to, "character": 0 },
    })
}

#[test]
fn type_hints_with_an_edit_in_the_range() {
    let all = answer(&Settings::default(), &lines(0, 5));
    assert_eq!(
        all,
        json!([
            {
                "position": { "line": 1, "character": 7 },
                "label": ": List<Int>",
                "kind": 1,
                "textEdits": [{
                    "range": {
                        "start": { "line": 1, "character": 7 },
                        "end": { "line": 1, "character": 7 },
                    },
                    "newText": ": List<Int>",
                }],
            },
            {
                "position": { "line": 2, "character": 7 },
                "label": ": List<List<Int>>",
                "kind": 1,
                "textEdits": [{
                    "range": {
                        "start": { "line": 2, "character": 7 },
                        "end": { "line": 2, "character": 7 },
                    },
                    "newText": ": List<List<Int>>",
                }],
            },
        ])
    );
    let second = answer(&Settings::default(), &lines(2, 3));
    assert_eq!(second.as_array().map(Vec::len), Some(1));
    assert_eq!(second.pointer("/0/position/line"), Some(&json!(2)));
    assert_eq!(answer(&Settings::default(), &lines(3, 5)), json!([]));
}

#[test]
fn settings_turn_hints_off() {
    let mut settings = Settings::default();
    settings.inlay_hints.enable = false;
    assert_eq!(answer(&settings, &lines(0, 5)), Value::Null);
    let mut settings = Settings::default();
    settings.inlay_hints.let_types = false;
    assert_eq!(answer(&settings, &lines(0, 5)), json!([]));
}

/// D-1480: `localFnTypes` and `patternBinders` reach their rows, and no
/// other.
#[test]
fn local_fn_and_pattern_binder_settings() {
    let text =
        "fn main() {\n  let (a, b) = (1, 2)\n  fn id(n) {\n    n\n  }\n  assert id(a) == b\n}\n";
    let labels = |settings: &Settings| -> Vec<(u64, u64, String)> {
        let found = answer_on(text, settings, &lines(0, 7));
        found
            .as_array()
            .into_iter()
            .flatten()
            .map(|hint| {
                (
                    hint["position"]["line"].as_u64().unwrap(),
                    hint["position"]["character"].as_u64().unwrap(),
                    hint["label"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let binders = vec![(1, 8, ": Int".to_owned()), (1, 11, ": Int".to_owned())];
    let local_fn = vec![(2, 9, ": Int".to_owned()), (2, 10, " -> Int".to_owned())];
    let all = labels(&Settings::default());
    assert_eq!(all, [binders.clone(), local_fn.clone()].concat());
    let mut settings = Settings::default();
    settings.inlay_hints.local_fn_types = false;
    assert_eq!(labels(&settings), binders);
    let mut settings = Settings::default();
    settings.inlay_hints.pattern_binders = false;
    assert_eq!(labels(&settings), local_fn);
    let mut settings = Settings::default();
    settings.inlay_hints.let_types = false;
    settings.inlay_hints.closure_types = false;
    assert_eq!(labels(&settings), all);
}

#[test]
fn malformed_parameters_and_unknown_documents() {
    let temp = Temp::package("inlay_bad", &[("main.cheby", TEXT)]);
    let path = temp.src("main.cheby");
    let analyzed = analyze(&temp.0, &[(path, TEXT)]);
    let settings = Settings::default();
    let caps = ClientCaps::default();
    let cancel = CancelToken::new();
    let cx = RequestCx {
        settings: &settings,
        caps: &caps,
        encoding: PositionEncoding::Utf16,
        cancel: &cancel,
        std: &crate::features::stdlib::StdService::none(),
        uris: &crate::text::uri::UriMap::new(),
        constants: None,
    };
    assert!(inlay_hint(&cx, &analyzed, json!(1)).is_err());
    let elsewhere = json!({
        "textDocument": { "uri": "file:///nowhere/main.cheby" },
        "range": lines(0, 1),
    });
    assert_eq!(
        inlay_hint(&cx, &analyzed, elsewhere).unwrap().value,
        Value::Null
    );
}

/// D-1515, D-1376: `parameterNames` is off by default, and on it sends
/// each name as a hint of kind `Parameter` whose label carries its own
/// space, so it sets no padding, as the type hints set none.
#[test]
fn parameter_names_are_off_by_default_and_of_kind_parameter() {
    let text =
        "fn add(a: Int, b: Int) -> Int {\n  a + b\n}\n\nfn main() {\n  assert add(1, 2) == 3\n}\n";
    assert_eq!(
        answer_on(text, &Settings::default(), &lines(0, 7)),
        json!([])
    );
    let mut settings = Settings::default();
    settings.inlay_hints.parameter_names = true;
    assert_eq!(
        answer_on(text, &settings, &lines(0, 7)),
        json!([
            {
                "position": { "line": 5, "character": 13 },
                "label": "a: ",
                "kind": 2,
            },
            {
                "position": { "line": 5, "character": 16 },
                "label": "b: ",
                "kind": 2,
            },
        ])
    );
}

/// D-1679, X §7.1: `constants`, off by default, sends a current value as
/// a hint with no kind, after the initializer, whose label carries its
/// own space; the value comes from `RequestCx::constants`.
#[test]
fn constant_values_are_off_by_default_and_have_no_kind() {
    let text = "const limit: Int = 20\n\nconst twice: Int = limit * 2\n\nfn main() {\n  assert twice == 40\n}\n";
    let values = |main: &Path| {
        let mut values = ConstValues::default();
        values.insert_value(
            Symbol::item(ModulePath::root("app"), SymbolNs::Value, "twice"),
            "40",
            false,
            BTreeMap::from([(main.to_path_buf(), normalized_key(text))]),
        );
        Some(values)
    };
    assert_eq!(
        answer_with(text, &Settings::default(), &lines(0, 7), values),
        json!([])
    );
    let mut settings = Settings::default();
    settings.inlay_hints.constants = true;
    assert_eq!(
        answer_with(text, &settings, &lines(0, 7), values),
        json!([{ "position": { "line": 2, "character": 28 }, "label": " = 40" }])
    );
    assert_eq!(
        answer_with(text, &settings, &lines(0, 7), |_| None),
        json!([])
    );
}
