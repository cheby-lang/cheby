//! Tests of the frame of code actions (G1 plan §4.3 B): `list` and
//! `resolve_with` with providers and resolvers of the tests' own, over
//! real snapshots and reports. Each runs only the front end, so it runs on
//! Windows too (D-1516).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cheby_driver::analysis::codes;
use cheby_driver::analysis::{
    FileReport, Globals, Live, Overlay, OverlayFile, PackageKind, PlainDiagnostic, PlainEdit,
    Session, SessionConfig, TextRange, TextSize, exact_key,
};
use lsp_types::CodeActionParams;
use serde_json::{Value, json};

use super::{ActionData, list, resolve_with};
use crate::features::ide::Refusal;
use crate::features::ide::actions::{
    Action, ActionCommand, ActionId, ActionKind, DiagRef, FixCx, Provide, Resolve, formatted,
    one_replacement,
};
use crate::features::stdlib::StdService;
use crate::features::{
    Analyzed, CancelToken, FileReports, PublishedReport, RequestCx, RequestError,
};
use crate::server::{ClientCaps, Effect, Settings};
use crate::text::lines::{Bom, LspLines, PositionEncoding};
use crate::text::uri::{UriMap, normalize, path_to_uri};

/// A package in a fresh temporary directory, removed on drop.
struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Temp {
    /// The package `app` with these files under `src/`.
    fn package(name: &str, files: &[(&str, &str)]) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cheby_lsp_actions_{}_{name}_{n}",
            std::process::id()
        ));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        for (path, text) in files {
            std::fs::write(dir.join("src").join(path), text).unwrap();
        }
        Self(normalize(&std::path::absolute(dir).unwrap()))
    }

    fn src(&self, file: &str) -> PathBuf {
        self.0.join("src").join(file)
    }

    fn uri(&self, file: &str) -> lsp_types::Uri {
        path_to_uri(&self.src(file)).unwrap()
    }

    fn session(&self) -> Session {
        Session::new(SessionConfig {
            root: self.0.clone(),
            kind: PackageKind::Package,
            target: None,
            globals: Arc::new(Globals::new()),
        })
    }

    /// An overlay of these open documents, each at version 1.
    fn open(&self, files: &[(&str, &str)]) -> Overlay {
        Overlay {
            files: files
                .iter()
                .enumerate()
                .map(|(n, (file, text))| {
                    let stamp = u64::try_from(n).unwrap().checked_add(1).unwrap();
                    let open = OverlayFile {
                        text: Arc::from(*text),
                        version: 1,
                        stamp,
                    };
                    (self.src(file), open)
                })
                .collect(),
        }
    }
}

/// The reports of `files`, as the server keeps the published ones.
fn reports(files: &BTreeMap<PathBuf, Arc<FileReport>>, substituted: bool) -> FileReports {
    FileReports {
        files: files
            .iter()
            .map(|(path, report)| {
                let published = PublishedReport {
                    report: Arc::clone(report),
                    substituted,
                };
                (path.clone(), published)
            })
            .collect(),
    }
}

/// The snapshot and the check of one run that is the check.
fn checked(live: Live) -> (Analyzed, FileReports) {
    let report = live.report.expect("the run is the check");
    let files = reports(&report.files, report.substituted);
    (Analyzed::new(live.snapshot.expect("a snapshot")), files)
}

/// What a request reads besides its snapshot.
struct Env {
    settings: Settings,
    caps: ClientCaps,
    cancel: CancelToken,
    std: StdService,
    uris: UriMap,
}

impl Env {
    /// A client like Zed 1.27: literals of every kind, `edit` resolved,
    /// `data` kept, `isPreferred` read, `documentChanges`, and with
    /// `disabled` shown when `disabled`.
    fn zed(disabled: bool) -> Self {
        Self::with(&json!({
            "codeActionLiteralSupport": { "codeActionKind": { "valueSet": [
                "quickfix", "refactor", "refactor.rewrite", "source", "source.organizeImports",
            ] } },
            "resolveSupport": { "properties": ["edit"] },
            "dataSupport": true,
            "isPreferredSupport": true,
            "disabledSupport": disabled,
        }))
    }

    /// A client whose `textDocument.codeAction` is `actions`.
    fn with(actions: &Value) -> Self {
        let caps = ClientCaps::from_params(&json!({ "capabilities": {
            "textDocument": { "codeAction": actions },
            "workspace": { "workspaceEdit": { "documentChanges": true } },
        } }));
        Self {
            settings: Settings::default(),
            caps,
            cancel: CancelToken::new(),
            std: StdService::none(),
            uris: UriMap::new(),
        }
    }

    fn cx(&self, reports: Option<FileReports>) -> RequestCx<'_> {
        RequestCx {
            settings: &self.settings,
            caps: &self.caps,
            encoding: PositionEncoding::Utf16,
            cancel: &self.cancel,
            std: &self.std,
            constants: None,
            uris: &self.uris,
            reports: reports.map(Arc::new),
        }
    }
}

/// The text of `path` in `analyzed`.
fn text_of<'a>(analyzed: &'a Analyzed, path: &Path) -> &'a str {
    let snapshot = &analyzed.snapshot;
    snapshot
        .source(snapshot.file_of(path).unwrap())
        .unwrap()
        .text()
}

/// `textDocument/codeAction` params for the whole of `needle` in `text`,
/// with `diagnostics` as the client's context and `only`.
fn params(
    uri: &lsp_types::Uri,
    text: &str,
    needle: &str,
    diagnostics: &Value,
    only: Option<&[&str]>,
) -> CodeActionParams {
    let lines = LspLines::new(text, Bom::Counted);
    let at = text
        .find(needle)
        .unwrap_or_else(|| panic!("`{needle}` in the text"));
    let range = TextRange::new(
        TextSize::try_from(at).unwrap(),
        TextSize::try_from(at.checked_add(needle.len()).unwrap()).unwrap(),
    );
    let range = lines.range(text, range, PositionEncoding::Utf16);
    let mut context = json!({ "diagnostics": diagnostics });
    if let Some(only) = only {
        context["only"] = json!(only);
    }
    serde_json::from_value(json!({
        "textDocument": { "uri": uri.as_str() },
        "range": range,
        "context": context,
    }))
    .unwrap()
}

/// The answer of a listing, as JSON.
fn listed(
    env: &Env,
    analyzed: &Analyzed,
    files: Option<FileReports>,
    p: &CodeActionParams,
    providers: &[(&str, Provide)],
) -> Vec<Value> {
    let cx = env.cx(files);
    let out = list(&cx, analyzed, p, providers, no_resolver).expect("an answer");
    out.value.as_array().unwrap().clone()
}

/// The titles of a listing, in order.
fn titles(actions: &[Value]) -> Vec<&str> {
    actions
        .iter()
        .map(|a| a["title"].as_str().unwrap())
        .collect()
}

/// An action with nothing set but its title and kind.
fn action(title: &str, kind: ActionKind) -> Action {
    Action {
        title: title.to_owned(),
        kind,
        diagnostic: None,
        preferred: false,
        removes: false,
        edit: None,
        data: None,
        disabled: None,
        command: None,
    }
}

/// A resolver for a test that resolves nothing.
fn no_resolver(_id: &ActionId) -> Resolve {
    refuse
}

fn refuse(_cx: &FixCx<'_>, _range: TextRange, _id: &ActionId) -> Result<Vec<PlainEdit>, Refusal> {
    Err(Refusal("nothing to resolve".to_owned()))
}

/// An edit of the request's file that replaces `needle` by `with`.
fn edit_of(cx: &FixCx<'_>, needle: &str, with: &str) -> Vec<PlainEdit> {
    let at = cx.text.find(needle).unwrap();
    vec![PlainEdit {
        path: cx.path().unwrap().to_path_buf(),
        range: TextRange::new(
            TextSize::try_from(at).unwrap(),
            TextSize::try_from(at.checked_add(needle.len()).unwrap()).unwrap(),
        ),
        replacement: with.to_owned(),
    }]
}

const MAIN: &str = "//! Root.\n\nfn main() {\n  let total = 1\n  let other = 2\n}\n";

/// One of every kind, out of order, each with an edit of its own.
fn every_kind(cx: &FixCx<'_>, _: TextRange, diags: &[(usize, &PlainDiagnostic)]) -> Vec<Action> {
    let first = diags.first().map(|&(i, _)| DiagRef::Report(i));
    let last = diags.last().map(|&(i, _)| DiagRef::Report(i));
    vec![
        Action {
            edit: Some(edit_of(cx, "//! Root.", "//! Root!")),
            ..action("Source", ActionKind::Source)
        },
        Action {
            edit: Some(edit_of(cx, "let other", "let _other")),
            ..action("Rewrite late", ActionKind::RefactorRewrite)
        },
        Action {
            edit: Some(edit_of(cx, "let total", "let _total")),
            ..action("Rewrite early", ActionKind::RefactorRewrite)
        },
        Action {
            command: Some(ActionCommand::Explain(codes::UNUSED_BINDING)),
            diagnostic: first,
            ..action("Explain W0002", ActionKind::QuickFix)
        },
        Action {
            edit: Some(edit_of(cx, "other", "_other")),
            diagnostic: last,
            preferred: true,
            ..action("Fix the second", ActionKind::QuickFix)
        },
        Action {
            edit: Some(edit_of(cx, "total", "_total")),
            diagnostic: first,
            preferred: true,
            ..action("Fix the first", ActionKind::QuickFix)
        },
        Action {
            edit: Some(Vec::new()),
            ..action("Organize imports", ActionKind::SourceOrganizeImports)
        },
    ]
}

/// The package of [`MAIN`], open, with its check's reports.
fn main_case(name: &str) -> (Temp, Analyzed, FileReports) {
    let temp = Temp::package(name, &[("main.cheby", MAIN)]);
    let live = temp
        .session()
        .analyze_live(&temp.open(&[("main.cheby", MAIN)]))
        .unwrap();
    let (analyzed, files) = checked(live);
    (temp, analyzed, files)
}

/// W11-Q2: a client without code-action literals gets `[]`, even where an
/// action would be offered (D-1734).
#[test]
fn a_client_without_literals_gets_nothing() {
    let (temp, analyzed, files) = main_case("no_literals");
    let p = params(
        &temp.uri("main.cheby"),
        MAIN,
        "total = 1\n  let other",
        &json!([]),
        None,
    );
    let literal = listed(
        &Env::zed(false),
        &analyzed,
        Some(reports_clone(&files)),
        &p,
        &[("all", every_kind)],
    );
    assert_ne!(literal, Vec::<Value>::new());
    let plain = Env::with(&json!({ "resolveSupport": { "properties": ["edit"] } }));
    assert_eq!(
        listed(&plain, &analyzed, Some(files), &p, &[("all", every_kind)]),
        Vec::<Value>::new()
    );
}

/// A copy of `files`, sharing the reports.
fn reports_clone(files: &FileReports) -> FileReports {
    FileReports {
        files: files
            .files
            .iter()
            .map(|(path, p)| {
                let published = PublishedReport {
                    report: Arc::clone(&p.report),
                    substituted: p.substituted,
                };
                (path.clone(), published)
            })
            .collect(),
    }
}

/// X §6.1's order: fixes by their diagnostic's start, then rewrites by
/// source order, then source actions, then Explain (X §8.3); a fix is preferred only
/// as its diagnostic's one fix with an edit; organize imports with nothing
/// to do is left out with no `only` (D-1691). Every action carries `data`
/// with the URI, the hash and the range, and an action with its edit in
/// hand no id (D-1733).
#[test]
fn listing_orders_and_prefers() {
    let (temp, analyzed, files) = main_case("order");
    let uri = temp.uri("main.cheby");
    let p = params(&uri, MAIN, "total = 1\n  let other", &json!([]), None);
    let actions = listed(
        &Env::zed(false),
        &analyzed,
        Some(files),
        &p,
        &[("all", every_kind)],
    );
    assert_eq!(
        titles(&actions),
        [
            "Fix the first",
            "Fix the second",
            "Rewrite early",
            "Rewrite late",
            "Source",
            "Explain W0002",
        ]
    );
    let preferred: Vec<bool> = actions
        .iter()
        .map(|a| a.get("isPreferred") == Some(&json!(true)))
        .collect();
    assert_eq!(preferred, [true, true, false, false, false, false]);
    let explain = &actions[5];
    assert_eq!(explain["command"]["command"], "cheby.explain");
    assert_eq!(explain["command"]["arguments"], json!(["W0002"]));
    assert_eq!(explain["diagnostics"][0]["code"], "W0002");
    let data: ActionData = serde_json::from_value(actions[0]["data"].clone()).unwrap();
    assert_eq!(data.uri, uri.as_str());
    assert_eq!(data.hash, exact_key(MAIN).to_hex());
    let start = u32::try_from(MAIN.find("total = 1").unwrap()).unwrap();
    assert_eq!(data.range[0], start);
    assert_eq!(data.id, None);
    assert!(actions[0]["edit"]["documentChanges"].is_array());
}

/// Two fixes with edits on one diagnostic: neither is preferred.
#[test]
fn two_fixes_share_no_preference() {
    fn two(cx: &FixCx<'_>, _: TextRange, diags: &[(usize, &PlainDiagnostic)]) -> Vec<Action> {
        let on = diags.first().map(|&(i, _)| DiagRef::Report(i));
        ["_total", "total_"]
            .into_iter()
            .map(|name| Action {
                edit: Some(edit_of(cx, "total", name)),
                diagnostic: on,
                preferred: true,
                ..action(name, ActionKind::QuickFix)
            })
            .collect()
    }
    let (temp, analyzed, files) = main_case("two_fixes");
    let p = params(&temp.uri("main.cheby"), MAIN, "total", &json!([]), None);
    let actions = listed(
        &Env::zed(false),
        &analyzed,
        Some(files),
        &p,
        &[("two", two)],
    );
    assert_eq!(titles(&actions), ["_total", "total_"]);
    assert!(
        actions.iter().all(|a| a.get("isPreferred").is_none()),
        "{actions:?}"
    );
}

/// `only` filters by kind prefix; organize imports with nothing to do is
/// kept only when `only` names `source` or `source.organizeImports`
/// (D-1691), and with an edit whenever its kind passes.
#[test]
fn only_filters_and_organize_imports_follows_d1691() {
    fn organize(cx: &FixCx<'_>, _: TextRange, _: &[(usize, &PlainDiagnostic)]) -> Vec<Action> {
        let with = cx.text.contains("let total");
        vec![Action {
            edit: Some(if with {
                Vec::new()
            } else {
                edit_of(cx, "//! Root.", "//! Root!")
            }),
            ..action("Organize imports", ActionKind::SourceOrganizeImports)
        }]
    }
    let (temp, analyzed, files) = main_case("only");
    let uri = temp.uri("main.cheby");
    let env = Env::zed(false);
    let ask = |only: Option<&[&str]>, providers: &[(&str, Provide)]| {
        let p = params(&uri, MAIN, "total = 1\n  let other", &json!([]), only);
        titles(&listed(
            &env,
            &analyzed,
            Some(reports_clone(&files)),
            &p,
            providers,
        ))
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>()
    };
    let all: &[(&str, Provide)] = &[("all", every_kind)];
    assert_eq!(
        ask(Some(&["quickfix"]), all),
        ["Fix the first", "Fix the second", "Explain W0002"]
    );
    assert_eq!(
        ask(Some(&["refactor"]), all),
        ["Rewrite early", "Rewrite late"]
    );
    assert_eq!(
        ask(Some(&["refactor.rewrite"]), all),
        ["Rewrite early", "Rewrite late"]
    );
    assert_eq!(ask(Some(&["source"]), all), ["Source", "Organize imports"]);
    assert_eq!(
        ask(Some(&["source.organizeImports"]), all),
        ["Organize imports"]
    );
    assert_eq!(ask(Some(&["refactor.extract"]), all), Vec::<String>::new());
    // With nothing to do, organize imports needs an `only` that names it.
    let empty: &[(&str, Provide)] = &[("organize", organize)];
    assert_eq!(ask(None, empty), Vec::<String>::new());
    assert_eq!(ask(Some(&["quickfix"]), empty), Vec::<String>::new());
    assert_eq!(ask(Some(&["source"]), empty), ["Organize imports"]);
    let p = params(&uri, MAIN, "total", &json!([]), Some(&["source"]));
    let answer = listed(&env, &analyzed, Some(reports_clone(&files)), &p, empty);
    assert!(
        answer[0].get("edit").is_none(),
        "nothing to do goes with no edit: {answer:?}"
    );
    assert!(answer[0]["data"]["id"].is_null(), "and nothing to resolve");
}

/// A disabled action is listed with its reason for a client that shows
/// it, and left out for one that does not (D-1690).
#[test]
fn a_disabled_action_needs_disabled_support() {
    fn disabled(cx: &FixCx<'_>, _: TextRange, _: &[(usize, &PlainDiagnostic)]) -> Vec<Action> {
        vec![
            Action {
                disabled: Some("the rewrite would drop a comment".to_owned()),
                edit: Some(edit_of(cx, "total", "sum")),
                data: Some(ActionId::Habit { index: 0 }),
                ..action("Disabled", ActionKind::RefactorRewrite)
            },
            action("Enabled", ActionKind::RefactorRewrite),
        ]
    }
    let (temp, analyzed, files) = main_case("disabled");
    let p = params(&temp.uri("main.cheby"), MAIN, "total", &json!([]), None);
    let shown = listed(
        &Env::zed(true),
        &analyzed,
        Some(reports_clone(&files)),
        &p,
        &[("d", disabled)],
    );
    assert_eq!(titles(&shown), ["Disabled", "Enabled"]);
    assert_eq!(
        shown[0]["disabled"]["reason"],
        "the rewrite would drop a comment"
    );
    // A client that ignores `disabled` gets nothing it could apply (C-7).
    assert!(
        shown[0].get("edit").is_none_or(Value::is_null),
        "{}",
        shown[0]
    );
    assert!(shown[0]["data"]["id"].is_null(), "{}", shown[0]);
    let hidden = listed(
        &Env::zed(false),
        &analyzed,
        Some(files),
        &p,
        &[("d", disabled)],
    );
    assert_eq!(titles(&hidden), ["Enabled"]);
}

const TWO_MAIN: &str =
    "//! Root.\n\nimport app::util\n\nfn main() {\n  let _ = util::helper()\n}\n";
const TWO_UTIL: &str = "//! Util.\n\npub fn helper() -> Int {\n  1\n}\n";

/// A resolver that edits `util.cheby`, which the test resolver of each
/// action id names: `MakeVisible` renames `helper` there alone, which
/// breaks its use (`Introduces`), and `Habit` changes its body.
fn cross_file(id: &ActionId) -> Resolve {
    match id {
        ActionId::MakeVisible { .. } => rename_helper,
        ActionId::Fix { .. }
        | ActionId::AddImport { .. }
        | ActionId::RemoveImport
        | ActionId::OrganizeImports
        | ActionId::FillArms
        | ActionId::Stub { .. }
        | ActionId::Habit { .. }
        | ActionId::Rename { .. } => change_body,
    }
}

/// The edit that replaces `needle` in `util.cheby` as the snapshot holds
/// it.
fn util_edit(cx: &FixCx<'_>, needle: &str, with: &str) -> Vec<PlainEdit> {
    let root = cx.analyzed.snapshot.root().to_path_buf();
    let path = root.join("src").join("util.cheby");
    let text = text_of(cx.analyzed, &path);
    let at = text.find(needle).unwrap();
    vec![PlainEdit {
        path,
        range: TextRange::new(
            TextSize::try_from(at).unwrap(),
            TextSize::try_from(at.checked_add(needle.len()).unwrap()).unwrap(),
        ),
        replacement: with.to_owned(),
    }]
}

#[expect(clippy::unnecessary_wraps, reason = "a resolver returns a `Result`")]
fn rename_helper(cx: &FixCx<'_>, _: TextRange, _: &ActionId) -> Result<Vec<PlainEdit>, Refusal> {
    Ok(util_edit(cx, "helper", "assist"))
}

#[expect(clippy::unnecessary_wraps, reason = "a resolver returns a `Result`")]
fn change_body(cx: &FixCx<'_>, _: TextRange, _: &ActionId) -> Result<Vec<PlainEdit>, Refusal> {
    let at = text_of(
        cx.analyzed,
        &cx.analyzed.snapshot.root().join("src/util.cheby"),
    );
    let body = if at.contains("  2\n") {
        "  2\n"
    } else {
        "  1\n"
    };
    Ok(util_edit(cx, body, "  3\n"))
}

/// The `codeAction/resolve` params of an action listed for `uri`, whose
/// text is `text`, with `id`.
fn resolve_params(
    uri: &lsp_types::Uri,
    text: &str,
    id: Option<&ActionId>,
) -> lsp_types::CodeAction {
    serde_json::from_value(json!({
        "title": "A fix",
        "kind": "quickfix",
        "data": {
            "uri": uri.as_str(),
            "hash": exact_key(text).to_hex(),
            "range": [0, 0],
            "id": id,
        },
    }))
    .unwrap()
}

/// The package of [`TWO_MAIN`], open, and [`TWO_UTIL`], closed.
fn two_files(name: &str) -> (Temp, Analyzed) {
    let temp = Temp::package(name, &[("main.cheby", TWO_MAIN), ("util.cheby", TWO_UTIL)]);
    let live = temp
        .session()
        .analyze_live(&temp.open(&[("main.cheby", TWO_MAIN)]))
        .unwrap();
    let analyzed = Analyzed::new(live.snapshot.unwrap());
    (temp, analyzed)
}

const VISIBLE: ActionId = ActionId::RemoveImport;

fn make_visible() -> ActionId {
    ActionId::MakeVisible {
        module: "app::util".to_owned(),
        item: "helper".to_owned(),
    }
}

/// A resolve whose hash is not the snapshot's text's answers
/// `ContentModified` (D-1683); one whose action has nothing to compute
/// answers it as sent (D-1733); a malformed `data` is `InvalidParams`.
#[test]
fn resolve_checks_the_hash_and_answers_what_has_nothing_to_compute() {
    let (temp, analyzed) = two_files("resolve_hash");
    let env = Env::zed(false);
    let cx = env.cx(None);
    let uri = temp.uri("main.cheby");
    let stale = resolve_params(&uri, "//! Another text.\n", Some(&VISIBLE));
    assert_eq!(
        resolve_with(&cx, &analyzed, stale, cross_file).unwrap_err(),
        RequestError::ContentModified
    );
    let explain = resolve_params(&uri, TWO_MAIN, None);
    let sent = serde_json::to_value(&explain).unwrap();
    let out = resolve_with(&cx, &analyzed, explain, cross_file).unwrap();
    assert_eq!(out.value, sent, "answered as it was sent");
    assert_eq!(out.effects, Vec::<Effect>::new());
    let mut bad = resolve_params(&uri, TWO_MAIN, None);
    bad.data = Some(json!({ "uri": uri.as_str() }));
    assert!(matches!(
        resolve_with(&cx, &analyzed, bad, cross_file),
        Err(RequestError::InvalidParams(_))
    ));
}

/// D-1731: a cross-file fix runs `verify` (D-1684), and each refusal
/// answers as a refused rename: `RequestFailed` with "Applying the fix
/// would cause" for a new error, with the reason for a blocked package,
/// and with the text for an edit `verify` cannot check; a file that
/// changed on disk answers the action with no edit, a warning and a sweep.
/// A fix that `verify` accepts comes with its edit.
#[test]
fn a_cross_file_fix_is_verified_and_refused_as_a_rename() {
    let (temp, analyzed) = two_files("resolve_verify");
    let env = Env::zed(false);
    let cx = env.cx(None);
    let uri = temp.uri("main.cheby");
    // Accepted: the body of `helper` changes, nothing breaks.
    let habit = ActionId::Habit { index: 0 };
    let out = resolve_with(
        &cx,
        &analyzed,
        resolve_params(&uri, TWO_MAIN, Some(&habit)),
        cross_file,
    )
    .unwrap();
    let changes = &out.value["edit"]["documentChanges"];
    assert_eq!(changes.as_array().unwrap().len(), 1, "{}", out.value);
    assert!(
        changes[0]["textDocument"]["uri"]
            .as_str()
            .unwrap()
            .ends_with("util.cheby")
    );
    assert_eq!(
        changes[0]["textDocument"]["version"],
        Value::Null,
        "a closed file"
    );
    // Introduces: `helper` renamed in its module alone breaks its use.
    let introduces = resolve_with(
        &cx,
        &analyzed,
        resolve_params(&uri, TWO_MAIN, Some(&make_visible())),
        cross_file,
    );
    match introduces {
        Err(RequestError::Failed(message)) => {
            assert!(
                message.starts_with("Applying the fix would cause: "),
                "{message}"
            );
            assert!(message.contains("E0042"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    // Stale: the closed file changed on disk since the snapshot.
    std::fs::write(temp.src("util.cheby"), TWO_UTIL.replace("  1\n", "  2\n")).unwrap();
    let out = resolve_with(
        &cx,
        &analyzed,
        resolve_params(&uri, TWO_MAIN, Some(&habit)),
        cross_file,
    )
    .unwrap();
    assert!(
        out.value.get("edit").is_none_or(Value::is_null),
        "{}",
        out.value
    );
    match out.effects.as_slice() {
        [Effect::Sweep { root }, Effect::Notify { method, params }] => {
            assert_eq!(root, &temp.0);
            assert_eq!(*method, "window/showMessage");
            assert_eq!(params["type"], 2);
            let text = params["message"].as_str().unwrap();
            assert!(
                text.contains("util.cheby") && text.ends_with("changed on disk, try again"),
                "{text}"
            );
        }
        other => panic!("{other:?}"),
    }
}

/// D-1741, D-1731: E0006's fix of a used name resolves through rename in
/// the frame, and its refusals answer as a refused rename's: a new error
/// is `RequestFailed` with rename's text, and a closed file changed on
/// disk is the action with no edit, a warning and a sweep (W11's review,
/// A7).
#[test]
fn a_used_name_s_fix_is_refused_as_a_rename() {
    const CLASH: &str = "//! Root.\n\nimport app::util\n\nfn main() {\n  let my_value = 1\n  let myValue = util::helper()\n  assert myValue == my_value\n}\n";
    let temp = Temp::package(
        "resolve_e6_refused",
        &[("main.cheby", CLASH), ("util.cheby", TWO_UTIL)],
    );
    let live = temp
        .session()
        .analyze_live(&temp.open(&[("main.cheby", CLASH)]))
        .unwrap();
    let analyzed = Analyzed::new(live.snapshot.unwrap());
    let env = Env::zed(false);
    let cx = env.cx(None);
    let uri = temp.uri("main.cheby");
    let id = ActionId::Rename {
        at: TextSize::try_from(CLASH.find("myValue =").unwrap()).unwrap(),
        name: "my_value".to_owned(),
    };
    match resolve_with(
        &cx,
        &analyzed,
        resolve_params(&uri, CLASH, Some(&id)),
        cross_file,
    ) {
        Err(RequestError::Failed(message)) => {
            assert!(
                message.starts_with("Renaming would cause: error[E0044]"),
                "{message}"
            );
        }
        other => panic!("{other:?}"),
    }
    let free = ActionId::Rename {
        at: TextSize::try_from(CLASH.find("myValue =").unwrap()).unwrap(),
        name: "my_other".to_owned(),
    };
    std::fs::write(temp.src("util.cheby"), TWO_UTIL.replace("  1\n", "  2\n")).unwrap();
    let out = resolve_with(
        &cx,
        &analyzed,
        resolve_params(&uri, CLASH, Some(&free)),
        cross_file,
    )
    .unwrap();
    assert!(
        out.value.get("edit").is_none_or(Value::is_null),
        "{}",
        out.value
    );
    match out.effects.as_slice() {
        [Effect::Sweep { root }, Effect::Notify { method, params }] => {
            assert_eq!(root, &temp.0);
            assert_eq!(*method, "window/showMessage");
            let text = params["message"].as_str().unwrap();
            assert!(text.ends_with("changed on disk, try again"), "{text}");
        }
        other => panic!("{other:?}"),
    }
}

/// An edit `verify` cannot check, of a file the snapshot does not hold,
/// answers `RequestFailed` with the text that says so (D-1731), and a
/// package `verify` will not check, here one whose file is not UTF-8,
/// answers its reason.
#[test]
fn a_fix_verify_cannot_check_or_refuses_whole_fails() {
    fn elsewhere(_: &ActionId) -> Resolve {
        missing_file
    }
    #[expect(clippy::unnecessary_wraps, reason = "a resolver returns a `Result`")]
    fn missing_file(cx: &FixCx<'_>, _: TextRange, _: &ActionId) -> Result<Vec<PlainEdit>, Refusal> {
        let path = cx.analyzed.snapshot.root().join("src/missing.cheby");
        Ok(vec![PlainEdit {
            path,
            range: TextRange::empty(0),
            replacement: "x".to_owned(),
        }])
    }
    let (temp, analyzed) = two_files("resolve_unchecked");
    let env = Env::zed(false);
    let cx = env.cx(None);
    let uri = temp.uri("main.cheby");
    match resolve_with(
        &cx,
        &analyzed,
        resolve_params(&uri, TWO_MAIN, Some(&VISIBLE)),
        elsewhere,
    ) {
        Err(RequestError::Failed(message)) => {
            assert!(
                message.starts_with("the edit could not be checked"),
                "{message}"
            );
        }
        other => panic!("{other:?}"),
    }
    let latin = Temp::package("resolve_blocked", &[("main.cheby", TWO_MAIN)]);
    std::fs::write(
        latin.src("util.cheby"),
        b"//! Util.\n\npub fn helper() -> Int {\n  1\n}\n// \xe9\n",
    )
    .unwrap();
    let live = latin
        .session()
        .analyze_live(&latin.open(&[("main.cheby", TWO_MAIN)]))
        .unwrap();
    let blocked = Analyzed::new(live.snapshot.unwrap());
    let uri = latin.uri("main.cheby");
    match resolve_with(
        &cx,
        &blocked,
        resolve_params(&uri, TWO_MAIN, Some(&VISIBLE)),
        cross_file,
    ) {
        Err(RequestError::Failed(message)) => {
            assert!(message.contains("not valid UTF-8"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

/// P-6: the snapshot a resolve reads holds every open document's newest
/// text, so a fix in another open document is written against its text
/// after an edit, with its version (D-1683).
#[test]
fn resolve_reads_the_other_open_document() {
    let edited = TWO_UTIL
        .replace("pub fn helper", "/// Helps.\npub fn helper")
        .replace("  1\n", "  2\n");
    let temp = Temp::package(
        "resolve_other",
        &[("main.cheby", TWO_MAIN), ("util.cheby", TWO_UTIL)],
    );
    let live = temp
        .session()
        .analyze_live(&temp.open(&[("main.cheby", TWO_MAIN), ("util.cheby", &edited)]))
        .unwrap();
    let analyzed = Analyzed::new(live.snapshot.unwrap());
    let env = Env::zed(false);
    let cx = env.cx(None);
    let habit = ActionId::Habit { index: 0 };
    let out = resolve_with(
        &cx,
        &analyzed,
        resolve_params(&temp.uri("main.cheby"), TWO_MAIN, Some(&habit)),
        cross_file,
    )
    .unwrap();
    let change = &out.value["edit"]["documentChanges"][0];
    assert_eq!(
        change["textDocument"]["version"], 1,
        "the open document's version"
    );
    // `  2` is on line 4 of the edited text, `  1` on line 3 of the old one.
    assert_eq!(change["edits"][0]["range"]["start"]["line"], 4, "{change}");
}

/// A client that cannot resolve `edit` gets each same-file edit in the
/// listing, with no id left to resolve, and no fix whose edit touches
/// another file (D-1684).
#[test]
fn a_client_without_resolve_gets_edits_at_listing() {
    fn resolvable(_: &FixCx<'_>, _: TextRange, _: &[(usize, &PlainDiagnostic)]) -> Vec<Action> {
        vec![
            Action {
                data: Some(ActionId::Habit { index: 0 }),
                ..action("Here", ActionKind::QuickFix)
            },
            Action {
                data: Some(make_visible()),
                ..action("Elsewhere", ActionKind::QuickFix)
            },
        ]
    }
    fn resolvers(id: &ActionId) -> Resolve {
        match id {
            ActionId::Habit { .. } => here,
            ActionId::Fix { .. }
            | ActionId::AddImport { .. }
            | ActionId::RemoveImport
            | ActionId::OrganizeImports
            | ActionId::FillArms
            | ActionId::Stub { .. }
            | ActionId::MakeVisible { .. }
            | ActionId::Rename { .. } => rename_helper,
        }
    }
    #[expect(clippy::unnecessary_wraps, reason = "a resolver returns a `Result`")]
    fn here(cx: &FixCx<'_>, _: TextRange, _: &ActionId) -> Result<Vec<PlainEdit>, Refusal> {
        Ok(edit_of(cx, "let _", "let _ignored"))
    }
    let (temp, analyzed) = two_files("no_resolve");
    let uri = temp.uri("main.cheby");
    let p = params(&uri, TWO_MAIN, "helper", &json!([]), None);
    let resolving = Env::zed(false);
    let cx = resolving.cx(None);
    let out = list(&cx, &analyzed, &p, &[("r", resolvable)], resolvers).unwrap();
    let actions = out.value.as_array().unwrap();
    assert_eq!(titles(actions), ["Here", "Elsewhere"]);
    assert!(
        actions
            .iter()
            .all(|a| a.get("edit").is_none() && !a["data"]["id"].is_null())
    );
    let plain = Env::with(&json!({
        "codeActionLiteralSupport": { "codeActionKind": { "valueSet": ["quickfix"] } },
        "dataSupport": true,
    }));
    let cx = plain.cx(None);
    let out = list(&cx, &analyzed, &p, &[("r", resolvable)], resolvers).unwrap();
    let actions = out.value.as_array().unwrap();
    assert_eq!(titles(actions), ["Here"]);
    assert!(
        actions[0]["edit"]["documentChanges"].is_array(),
        "{}",
        actions[0]
    );
    assert!(actions[0]["data"]["id"].is_null(), "{}", actions[0]);
}

/// The diagnostics of `file`'s report, by code.
fn codes_of(files: &FileReports, path: &Path) -> Vec<String> {
    files.files[path]
        .report
        .diagnostics
        .iter()
        .map(|d| d.code.to_string())
        .collect()
}

/// A removal of each W0003, W0004 and W0005 the provider gets, and a
/// "Make `pub`" of each W0005, as `fixes` and `stubs` will offer them.
fn removals(_: &FixCx<'_>, _: TextRange, diags: &[(usize, &PlainDiagnostic)]) -> Vec<Action> {
    let mut out = Vec::new();
    for &(i, d) in diags {
        let on = Some(DiagRef::Report(i));
        if matches!(
            d.code,
            codes::UNUSED_IMPORT | codes::UNUSED_PRIVATE_ITEM | codes::UNUSED_PACKAGE_ITEM
        ) {
            out.push(Action {
                removes: true,
                diagnostic: on,
                data: Some(ActionId::Fix {
                    code: d.code,
                    index: 0,
                }),
                ..action(&format!("Remove for {}", d.code), ActionKind::QuickFix)
            });
        }
        if d.code == codes::UNUSED_PACKAGE_ITEM {
            out.push(Action {
                diagnostic: on,
                data: Some(make_visible()),
                ..action("Make `lonely` `pub`", ActionKind::QuickFix)
            });
        }
    }
    out.push(Action {
        removes: true,
        data: Some(ActionId::OrganizeImports),
        ..action("Organize imports", ActionKind::SourceOrganizeImports)
    });
    out
}

const G: &str = "//! G.\n\nimport std::list\n\npub fn used() -> Int {\n  1\n}\n\nfn lonely() -> Int {\n  2\n}\n\npriv fn hidden() -> Int {\n  3\n}\n";
const B_GOOD: &str = "//! B.\n\nimport app::g\n\npub fn b() -> Int {\n  g::used()\n}\n";
const B_BROKEN: &str = "//! B.\n\nimport app::g\n\npub fn b() -> Int {\n  g::used(\n";
const S_MAIN: &str = "//! Root.\n\nimport app::b\n\nfn main() {\n  let _ = b::b()\n}\n";

/// The titles listed over the whole of `G` for the reports `files`.
fn removal_titles(analyzed: &Analyzed, files: FileReports, uri: &lsp_types::Uri) -> Vec<String> {
    removal_titles_in(analyzed, files, uri, G)
}

/// The titles listed over the whole of `text` for the reports `files`.
fn removal_titles_in(
    analyzed: &Analyzed,
    files: FileReports,
    uri: &lsp_types::Uri,
    text: &str,
) -> Vec<String> {
    let p = params(uri, text, text, &json!([]), None);
    titles(&listed(
        &Env::zed(false),
        analyzed,
        Some(files),
        &p,
        &[("r", removals)],
    ))
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// S-11, D-1360, a run with a last good text: `b` parsed, then broke
/// (D-1347), so the D run reads its last good text. W0004 and W0005 are
/// still reported, the report is substituted, and no removal is offered,
/// organize imports' included, while "Make `pub`" stays. The live run's
/// report, which read every text, offers them all.
#[test]
fn no_removal_while_substituted() {
    let temp = Temp::package(
        "substituted",
        &[("main.cheby", S_MAIN), ("g.cheby", G), ("b.cheby", B_GOOD)],
    );
    let g = temp.src("g.cheby");
    let uri = temp.uri("g.cheby");
    let mut session = temp.session();
    let first = session.analyze_live(&temp.open(&[("g.cheby", G)])).unwrap();
    let (analyzed, files) = checked(first);
    assert_eq!(codes_of(&files, &g), ["W0003", "W0005", "W0004"]);
    let clean = reports_clone(&files);
    let all = removal_titles(&analyzed, files, &uri);
    assert_eq!(
        all,
        [
            "Remove for W0003",
            "Remove for W0005",
            "Make `lonely` `pub`",
            "Remove for W0004",
            "Organize imports",
        ]
    );
    // Only `b` breaks, and its last good text is the clean run's: `g`
    // keeps the clean report, whose flag is not set, though its warnings
    // are now read against `b`'s last good text. The snapshot's broken
    // file marks it (C-2).
    let only_b = session
        .analyze_live(&temp.open(&[("g.cheby", G), ("b.cheby", B_BROKEN)]))
        .unwrap();
    let broken_now = Analyzed::new(only_b.snapshot.unwrap());
    assert!(!broken_now.snapshot.status().broken.is_empty());
    assert_eq!(
        removal_titles(&broken_now, clean, &uri),
        ["Make `lonely` `pub`"]
    );
    // `g` changes too, so the D run has inputs not run before.
    let g_2 = G.replace("//! G.", "//! G, again.");
    let broken = session
        .analyze_live(&temp.open(&[("g.cheby", &g_2), ("b.cheby", B_BROKEN)]))
        .unwrap();
    let analyzed = Analyzed::new(broken.snapshot.clone().unwrap());
    let plan = broken.plan.expect("a D run on b's last good text");
    let report = session.analyze_strict(plan).unwrap();
    assert!(report.substituted);
    let files = reports(&report.files, report.substituted);
    assert_eq!(codes_of(&files, &g), ["W0003", "W0005", "W0004"]);
    assert_eq!(
        removal_titles_in(&analyzed, files, &uri, &g_2),
        ["Make `lonely` `pub`"]
    );
}

/// D-1739: a cut import counts as a substitute too. `c2` comes to import
/// `c1`, which imports it, so the cycle is cut, and `g`'s clean report,
/// whose hash still matches, offers no removal while the cut lasts.
#[test]
fn no_removal_while_an_import_is_cut() {
    const C1: &str = "//! C1.\n\nimport app::c2\n\npub fn one() -> Int {\n  c2::two()\n}\n";
    const C2: &str = "//! C2.\n\npub fn two() -> Int {\n  1\n}\n";
    let temp = Temp::package(
        "cut",
        &[
            ("main.cheby", S_MAIN),
            ("g.cheby", G),
            ("b.cheby", B_GOOD),
            ("c1.cheby", C1),
            ("c2.cheby", C2),
        ],
    );
    let uri = temp.uri("g.cheby");
    let mut session = temp.session();
    let first = session.analyze_live(&temp.open(&[("g.cheby", G)])).unwrap();
    let (analyzed, files) = checked(first);
    let clean = reports_clone(&files);
    assert!(
        removal_titles(&analyzed, files, &uri).contains(&"Remove for W0004".to_owned()),
        "the clean run offers removals"
    );
    let cycle = C2.replace("//! C2.\n\n", "//! C2.\n\nimport app::c1\n\n");
    let cut = session
        .analyze_live(&temp.open(&[("g.cheby", G), ("c2.cheby", &cycle)]))
        .unwrap();
    let cut = Analyzed::new(cut.snapshot.unwrap());
    assert_ne!(cut.snapshot.status().cut_imports.len(), 0);
    assert!(cut.snapshot.status().broken.is_empty());
    assert_eq!(removal_titles(&cut, clean, &uri), ["Make `lonely` `pub`"]);
}

/// S-11, D-1357, a run with a module that never parsed: `b` enters as
/// empty text, so W0005 is dropped and W0003, still reported, offers no
/// removal, nor does organize imports.
#[test]
fn no_removal_after_a_stub() {
    let temp = Temp::package(
        "stubbed",
        &[
            ("main.cheby", S_MAIN),
            ("g.cheby", G),
            ("b.cheby", B_BROKEN),
        ],
    );
    let g = temp.src("g.cheby");
    let mut session = temp.session();
    let live = session.analyze_live(&temp.open(&[("g.cheby", G)])).unwrap();
    let analyzed = Analyzed::new(live.snapshot.clone().unwrap());
    let report = session.analyze_strict(live.plan.expect("a D run")).unwrap();
    assert!(report.substituted);
    let files = reports(&report.files, report.substituted);
    let found = codes_of(&files, &g);
    assert!(found.contains(&"W0003".to_owned()), "{found:?}");
    assert!(
        !found.contains(&"W0005".to_owned()),
        "D-1357 drops W0005: {found:?}"
    );
    assert_eq!(
        removal_titles(&analyzed, files, &temp.uri("g.cheby")),
        Vec::<String>::new()
    );
}

/// S-7, D-1393: a report of an older text, as a held file keeps (D-1357),
/// gives the providers no diagnostic, so no fix rides on it, while what
/// reads no report, here a rewrite, is still offered (D-1685).
#[test]
fn fix_dropped_for_old_report() {
    fn by_report(cx: &FixCx<'_>, _: TextRange, diags: &[(usize, &PlainDiagnostic)]) -> Vec<Action> {
        let mut out: Vec<Action> = diags
            .iter()
            .map(|&(i, d)| Action {
                diagnostic: Some(DiagRef::Report(i)),
                edit: Some(edit_of(cx, "total", "_total")),
                ..action(&format!("Fix {}", d.code), ActionKind::QuickFix)
            })
            .collect();
        assert!(
            cx.report.is_none() && diags.is_empty(),
            "the report is of an older text"
        );
        out.push(action("Rewrite", ActionKind::RefactorRewrite));
        out
    }
    let temp = Temp::package("old_report", &[("main.cheby", MAIN)]);
    let mut session = temp.session();
    let (_, old) = checked(
        session
            .analyze_live(&temp.open(&[("main.cheby", MAIN)]))
            .unwrap(),
    );
    let newer = MAIN.replace("let other = 2", "let other = 3");
    let (analyzed, _) = checked(
        session
            .analyze_live(&temp.open(&[("main.cheby", &newer)]))
            .unwrap(),
    );
    let uri = temp.uri("main.cheby");
    let p = params(&uri, &newer, "total", &json!([]), None);
    let titles_now = titles(&listed(
        &Env::zed(false),
        &analyzed,
        Some(old),
        &p,
        &[("r", by_report)],
    ))
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    assert_eq!(titles_now, ["Rewrite"]);
}

/// G1 plan §1.6: the frame passes the request's own diagnostics to the
/// providers as registered codes and ranges, so Explain is offered for a
/// code that no report holds, such as a child's, and names the diagnostic
/// as the client sent it. An unregistered code is left out.
#[test]
fn explain_is_offered_from_context_alone() {
    fn explain(cx: &FixCx<'_>, _: TextRange, diags: &[(usize, &PlainDiagnostic)]) -> Vec<Action> {
        assert!(diags.is_empty(), "no report");
        cx.context
            .iter()
            .enumerate()
            .map(|(i, &(code, _))| Action {
                command: Some(ActionCommand::Explain(code)),
                diagnostic: Some(DiagRef::Context(i)),
                ..action(&format!("Explain {code}"), ActionKind::QuickFix)
            })
            .collect()
    }
    let (temp, analyzed, _) = main_case("context");
    let uri = temp.uri("main.cheby");
    let child = json!({
        "range": { "start": { "line": 3, "character": 6 }, "end": { "line": 3, "character": 11 } },
        "severity": 1,
        "code": "E0075",
        "source": "cheby",
        "message": "division by zero",
    });
    let unknown = json!({
        "range": { "start": { "line": 3, "character": 6 }, "end": { "line": 3, "character": 11 } },
        "code": "E9999",
        "message": "no such code",
    });
    let p = params(&uri, MAIN, "total", &json!([child, unknown]), None);
    let actions = listed(&Env::zed(false), &analyzed, None, &p, &[("e", explain)]);
    assert_eq!(titles(&actions), ["Explain E0075"]);
    assert_eq!(actions[0]["diagnostics"], json!([child]));
}

/// D-1682: in a file of the package that is not a module, no action is
/// offered, "Use a field path" included.
#[test]
fn nothing_outside_a_module() {
    const UPDATE: &str = "//! Root.\n\ntype P {\n  x: Int,\n}\n\ntype S {\n  p: P,\n}\n\nfn f(s: S) -> S {\n  S { ..s, p: P { ..s.p, x: 1 } }\n}\n\nfn main() {\n  let _ = f\n}\n";
    let temp = Temp::package("not_a_module", &[("main.cheby", UPDATE)]);
    let stray = temp.0.join("notes.cheby");
    std::fs::write(&stray, UPDATE).unwrap();
    let live = temp
        .session()
        .analyze_live(&temp.open(&[("main.cheby", UPDATE)]))
        .unwrap();
    let (analyzed, files) = checked(live);
    let env = Env::zed(false);
    let ask = |uri: &lsp_types::Uri| {
        let p = params(uri, UPDATE, "P { ..s.p", &json!([]), None);
        let cx = env.cx(Some(reports_clone(&files)));
        let out = super::code_action(&cx, &analyzed, serde_json::to_value(&p).unwrap()).unwrap();
        out.value
    };
    let in_module = ask(&temp.uri("main.cheby"));
    assert_eq!(in_module[0]["title"], "Use a field path", "{in_module}");
    assert_eq!(ask(&path_to_uri(&stray).unwrap()), json!([]));
}

/// D-1735: a fix within a file comes in `cheby fmt`'s form on the lines it
/// touches: here a line at 100 columns that the fix lengthens, which the
/// formatter breaks. E0044's fix, which renames, is left as written, and
/// so is every fix in a file with a grammar error.
#[test]
fn a_fix_that_lengthens_a_full_line_is_formatted() {
    let call = format!("  let total = say(\"{}\", 1)\n", "x".repeat(76));
    let text = format!(
        "fn say(s: String, n: Int) -> Int {{\n  n\n}}\n\nfn main() {{\n{call}  assert total > 0\n}}\n"
    );
    assert_eq!(call.trim_end().chars().count(), 100);
    let path = PathBuf::from("/p/src/main.cheby");
    let cancel = CancelToken::new();
    let at = text.find("1)").unwrap();
    let fix = vec![PlainEdit {
        path: path.clone(),
        range: TextRange::new(
            TextSize::try_from(at).unwrap(),
            TextSize::try_from(at.checked_add(1).unwrap()).unwrap(),
        ),
        replacement: "12".to_owned(),
    }];
    let apply = |edits: &[PlainEdit]| {
        let mut out = text.clone();
        for edit in edits.iter().rev() {
            let r = usize::try_from(edit.range.start()).unwrap()
                ..usize::try_from(edit.range.end()).unwrap();
            out.replace_range(r, &edit.replacement);
        }
        out
    };
    let done = formatted(&text, &path, fix.clone(), false, &cancel);
    let after = apply(&done);
    assert_eq!(
        Some(after.clone()),
        crate::features::ide::edits::formatted(&after),
        "{after}"
    );
    assert!(
        after.lines().all(|line| line.chars().count() <= 100),
        "{after}"
    );
    assert_ne!(done, fix);
    // A fix that renames, E0044's or E0006's, is rename's writer, never
    // formatted (D-1668, D-1735, D-1741).
    assert_eq!(formatted(&text, &path, fix.clone(), true, &cancel), fix);
    // A file with a grammar error keeps the raw edits.
    let broken = format!("{text}fn broken() {{\n  let = 1\n}}\n");
    assert_eq!(formatted(&broken, &path, fix.clone(), false, &cancel), fix);
    // An edit already in form stays as it is.
    let short = vec![PlainEdit {
        path: path.clone(),
        range: TextRange::new(
            TextSize::try_from(text.find("total >").unwrap()).unwrap(),
            TextSize::try_from(text.find("total >").unwrap().checked_add(5).unwrap()).unwrap(),
        ),
        replacement: "_total".to_owned(),
    }];
    let kept = formatted(&text, &path, short.clone(), false, &cancel);
    assert_eq!(kept, short);
    // D-1738: past the cap of the widening, the fix keeps its raw edits.
    // In this text each `let n` hunk deletes a line that the next `let m`
    // hunk writes back, so a kept set closes only with the whole chain,
    // which takes three rounds a link; a short chain closes within the cap.
    let chain = |links: usize| {
        use std::fmt::Write as _;
        let mut body = String::new();
        for n in 0..links {
            writeln!(body, "  let n{n} = add(1, // one\n    2)").unwrap();
            writeln!(body, "  let m{n} = add(\n    1,\n    2\n  )").unwrap();
        }
        format!(
            "fn add(a: Int, b: Int) -> Int {{\n  a + b\n}}\n\nfn main() {{\n{body}  assert True\n}}\n"
        )
    };
    let raw_at = |text: &str| {
        let at = text.find("add(1, // one").unwrap().checked_add(4).unwrap();
        vec![PlainEdit {
            path: path.clone(),
            range: TextRange::new(
                TextSize::try_from(at).unwrap(),
                TextSize::try_from(at.checked_add(1).unwrap()).unwrap(),
            ),
            replacement: "3".to_owned(),
        }]
    };
    let long = chain(
        crate::features::ide::edits::MAX_ROUNDS
            .checked_mul(3)
            .unwrap(),
    );
    assert_eq!(
        formatted(&long, &path, raw_at(&long), false, &cancel),
        raw_at(&long)
    );
    let short = chain(2);
    assert_ne!(
        formatted(&short, &path, raw_at(&short), false, &cancel),
        raw_at(&short)
    );
}

/// The one replacement between two texts is cut on character boundaries.
/// G1 plan §4.7's listing budget, kept runnable (T-5): a listing with
/// every provider of the registry and no `only` at the start of every
/// name of `docs/examples/029_csv_report/src/main.cheby`, over the check's
/// report, timed one by one; it prints the median. A measurement, so it is
/// ignored: `cargo xtask test -E 'test(/measure_listing_on_029/)'
/// --run-ignored only --no-capture`.
#[test]
#[ignore = "a measurement (G1 plan §4.7), run with `--run-ignored only`"]
fn measure_listing_on_029() {
    use crate::features::ide::actions::registry;

    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("docs/examples/029_csv_report/src");
    let mut sources: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read_to_string(&path).unwrap())
        })
        .collect();
    sources.sort();
    let files: Vec<(&str, &str)> = sources
        .iter()
        .map(|(name, text)| (name.as_str(), text.as_str()))
        .collect();
    let temp = Temp::package("measure_029", &files);
    let main = files
        .iter()
        .find(|(name, _)| *name == "main.cheby")
        .map(|(_, text)| *text)
        .unwrap();
    let mut session = temp.session();
    let live = session
        .analyze_live(&temp.open(&[("main.cheby", main)]))
        .unwrap();
    let (analyzed, files) = checked(live);
    let env = Env::zed(false);
    let uri = temp.uri("main.cheby");
    let lines = LspLines::new(main, Bom::Counted);
    let starts: Vec<usize> = main
        .char_indices()
        .filter(|&(at, c)| {
            (c.is_ascii_alphabetic() || c == '_')
                && main
                    .get(..at)
                    .and_then(|head| head.chars().next_back())
                    .is_none_or(|before| !(before.is_ascii_alphanumeric() || before == '_'))
        })
        .map(|(at, _)| at)
        .collect();
    let mut times = Vec::with_capacity(starts.len());
    let mut actions = 0_usize;
    for at in starts {
        let range = TextRange::empty(TextSize::try_from(at).unwrap());
        let p: CodeActionParams = serde_json::from_value(json!({
            "textDocument": { "uri": uri.as_str() },
            "range": lines.range(main, range, PositionEncoding::Utf16),
            "context": { "diagnostics": [] },
        }))
        .unwrap();
        let reports = Some(reports_clone(&files));
        let started = std::time::Instant::now();
        let listed = listed(&env, &analyzed, reports, &p, registry::PROVIDERS);
        times.push(started.elapsed());
        actions = actions.saturating_add(listed.len());
    }
    times.sort();
    let median = times.get(times.len() / 2).copied().unwrap_or_default();
    eprintln!(
        "029_csv_report/src/main.cheby: points={} actions={actions} median={median:?}",
        times.len()
    );
}

/// A fix's edit that writes LF into a CRLF file writes CRLF there, where
/// the line it starts on ends in CRLF, and an LF file or another file's
/// edit keeps what it wrote (C-6, D-1444).
#[test]
fn edits_keep_a_crlf_file_s_line_breaks() {
    let path = PathBuf::from("/p/src/main.cheby");
    let other = PathBuf::from("/p/src/other.cheby");
    let edit = |path: &Path, replacement: &str| PlainEdit {
        path: path.to_path_buf(),
        range: TextRange::new(11, 12),
        replacement: replacement.to_owned(),
    };
    let crlf = "fn f() {\r\n  1\r\n}\r\n";
    let out = super::line_breaks(
        crlf,
        &path,
        vec![edit(&path, "case {\n    x\r\n  }"), edit(&other, "a\nb")],
    );
    assert_eq!(out[0].replacement, "case {\r\n    x\r\n  }");
    assert_eq!(out[1].replacement, "a\nb");
    let lf = "fn f() {\n  1\n}\n";
    let out = super::line_breaks(lf, &path, vec![edit(&path, "case {\n  }")]);
    assert_eq!(out[0].replacement, "case {\n  }");
    // The last line, with no line break, follows the line before it.
    assert!(super::crlf_at("a\r\nb", 4));
    assert!(!super::crlf_at("a\nb", 3));
}

#[test]
fn one_replacement_keeps_characters_whole() {
    let path = Path::new("/p/a.cheby");
    assert_eq!(one_replacement("abc", "abc", path), None);
    let edit = one_replacement("let é = 1", "let è = 1", path).unwrap();
    assert_eq!(edit.range, TextRange::new(4, 6));
    assert_eq!(edit.replacement, "è");
    let edit = one_replacement("aaa", "aaaa", path).unwrap();
    assert_eq!(
        (edit.range, edit.replacement.as_str()),
        (TextRange::new(3, 3), "a")
    );
}
