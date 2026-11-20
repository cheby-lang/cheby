//! Tests of the rename handlers: the protocol shapes of `prepareRename`
//! and `rename` over one package, with the helpers of `nav_tests`, so
//! they run on Windows too (D-1516, G1 plan §1.10). What a rename writes
//! and refuses is `ide::rename`'s, tested there; these tests pin how the
//! handlers answer it: a range and a placeholder, `null`, `RequestFailed`,
//! a workspace edit in the client's form with its note, and a stale file
//! answered with a message and a sweep (D-1389, D-1391, D-1669, D-1673),
//! and the same for a stale `willRenameFiles` move (D-1711).
#![expect(
    clippy::wildcard_enum_match_arm,
    reason = "a test matches only the variants it checks"
)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use cheby_driver::analysis::{
    Globals, Overlay, OverlayFile, PackageKind, Session, SessionConfig, StdBase,
};
use serde_json::{Value, json};

use super::{prepare_in, rename_in};
use crate::features::handlers::edit::EditCaps;
use crate::features::{Analyzed, CancelToken, HandlerOutput, RequestCx, RequestError};
use crate::server::{ClientCaps, Effect, Settings};
use crate::text::lines::PositionEncoding;
use crate::text::uri::{normalize, path_to_uri};

/// The cursor in a test's text.
const CURSOR: char = '‸';

/// The std directory the tests name, absolute on every platform, so a
/// std file under it has a URI on Windows too. Nothing is read from it.
fn std_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| std::env::temp_dir().join("rename-tests-std"))
}

/// The version the client gave each open document.
const VERSION: i32 = 7;

/// A package in a fresh temporary directory, removed on drop.
struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Temp {
    /// An empty directory for the test `name`.
    fn dir(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("cheby_lsp_rename_{}_{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        Self(normalize(&std::path::absolute(&dir).unwrap()))
    }

    /// The package `app` with these files under `src/`.
    fn package(name: &str, files: &[(&str, String)]) -> Self {
        let temp = Self::dir(name);
        std::fs::create_dir_all(temp.0.join("src")).unwrap();
        std::fs::write(
            temp.0.join("cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        for (path, text) in files {
            std::fs::write(temp.src(path), text).unwrap();
        }
        temp
    }

    fn src(&self, file: &str) -> PathBuf {
        self.0.join("src").join(file)
    }
}

/// A package, or a single file, analyzed with some files open, and the
/// cursor's place as the client names it.
struct Case {
    temp: Temp,
    analyzed: Analyzed,
    /// The file that holds the cursor.
    path: PathBuf,
    /// The cursor's line and UTF-16 column, from 0.
    at: (u32, u32),
}

/// Each file's text with the cursor taken out, and the file and the
/// UTF-16 position of the one cursor.
fn take_cursor(files: &[(&str, &str)]) -> (Vec<(&'static str, String)>, String, (u32, u32)) {
    let mut cursor = None;
    let mut clean = Vec::new();
    for &(name, text) in files {
        if let Some(at) = text.find(CURSOR) {
            assert!(cursor.is_none(), "one cursor");
            let before = text.get(..at).unwrap();
            let line = before.matches('\n').count();
            let col: usize = before
                .rsplit('\n')
                .next()
                .unwrap()
                .chars()
                .map(char::len_utf16)
                .sum();
            cursor = Some((
                name.to_owned(),
                (u32::try_from(line).unwrap(), u32::try_from(col).unwrap()),
            ));
        }
        let name: &'static str = Box::leak(name.to_owned().into_boxed_str());
        clean.push((name, text.replace(CURSOR, "")));
    }
    let (name, at) = cursor.expect("a cursor");
    (clean, name, at)
}

impl Case {
    /// The package `app` of `files` under `src/`, those named in `open`
    /// open at [`VERSION`] with their text.
    fn new(name: &str, files: &[(&str, &str)], open: &[&str]) -> Self {
        let (clean, cursor, at) = take_cursor(files);
        let temp = Temp::package(name, &clean);
        let overlay = Overlay {
            files: clean
                .iter()
                .filter(|(file, _)| open.contains(file))
                .map(|(file, text)| {
                    let open = OverlayFile {
                        text: Arc::from(text.as_str()),
                        version: VERSION,
                        stamp: 1,
                    };
                    (temp.src(file), open)
                })
                .collect(),
        };
        let mut session = Session::new(SessionConfig {
            root: temp.0.clone(),
            kind: PackageKind::Package,
            target: None,
            globals: Arc::new(Globals::new()),
        });
        let live = session.analyze_live(&overlay).expect("a run");
        let analyzed = Analyzed::new(live.snapshot.expect("a snapshot"));
        let path = temp.src(&cursor);
        Self {
            temp,
            analyzed,
            path,
            at,
        }
    }

    /// One package file, `main.cheby`, open.
    fn main(name: &str, main: &str) -> Self {
        Self::new(name, &[("main.cheby", main)], &["main.cheby"])
    }

    /// `text` open in single-file mode, as `script.cheby` in a directory
    /// with no `cheby.toml` (D-1371, D-1667).
    fn single(name: &str, text: &str) -> Self {
        let (clean, _, at) = take_cursor(&[("script.cheby", text)]);
        let temp = Temp::dir(name);
        let path = temp.0.join("script.cheby");
        let text = &clean[0].1;
        std::fs::write(&path, text).unwrap();
        let overlay = Overlay {
            files: [(
                path.clone(),
                OverlayFile {
                    text: Arc::from(text.as_str()),
                    version: VERSION,
                    stamp: 1,
                },
            )]
            .into(),
        };
        let mut session = Session::new(SessionConfig {
            root: temp.0.clone(),
            kind: PackageKind::SingleFile {
                file: path.clone(),
                strict: true,
            },
            target: None,
            globals: Arc::new(Globals::new()),
        });
        let live = session.analyze_live(&overlay).expect("a run");
        let analyzed = Analyzed::new(live.snapshot.expect("a snapshot"));
        assert!(analyzed.snapshot.is_single_file());
        Self {
            temp,
            analyzed,
            path,
            at,
        }
    }

    /// The position params at the cursor.
    fn position(&self) -> Value {
        position(&self.path, self.at)
    }

    /// `prepareRename` at the cursor.
    fn prepare(&self) -> Result<HandlerOutput, RequestError> {
        run(&self.analyzed, EditCaps::default(), |cx, analyzed| {
            prepare_in(cx, analyzed, self.position(), Some(std_dir()))
        })
    }

    /// `rename` at the cursor to `new`, for a client with `edit`.
    fn rename(&self, new: &str, edit: EditCaps) -> Result<HandlerOutput, RequestError> {
        self.rename_with(new, edit, &CancelToken::new())
    }

    fn rename_with(
        &self,
        new: &str,
        edit: EditCaps,
        cancel: &CancelToken,
    ) -> Result<HandlerOutput, RequestError> {
        let mut params = self.position();
        params["newName"] = json!(new);
        run_with(&self.analyzed, edit, cancel, |cx, analyzed| {
            rename_in(cx, analyzed, params, Some(std_dir()))
        })
    }

    /// The URI of `file` under `src/`.
    fn uri(&self, file: &str) -> String {
        uri(&self.temp.src(file))
    }
}

/// The URI of `path`, as the client spells it.
fn uri(path: &Path) -> String {
    path_to_uri(path).unwrap().as_str().to_owned()
}

/// The params of a position request at `at` of `path`.
fn position(path: &Path, (line, character): (u32, u32)) -> Value {
    json!({
        "textDocument": { "uri": uri(path) },
        "position": { "line": line, "character": character },
    })
}

/// Runs `handler` for a client with `edit`, in UTF-16.
fn run(
    analyzed: &Analyzed,
    edit: EditCaps,
    handler: impl FnOnce(&RequestCx<'_>, &Analyzed) -> Result<HandlerOutput, RequestError>,
) -> Result<HandlerOutput, RequestError> {
    run_with(analyzed, edit, &CancelToken::new(), handler)
}

fn run_with(
    analyzed: &Analyzed,
    edit: EditCaps,
    cancel: &CancelToken,
    handler: impl FnOnce(&RequestCx<'_>, &Analyzed) -> Result<HandlerOutput, RequestError>,
) -> Result<HandlerOutput, RequestError> {
    let settings = Settings::default();
    let caps = ClientCaps {
        edit,
        ..ClientCaps::default()
    };
    let cx = RequestCx {
        settings: &settings,
        caps: &caps,
        encoding: PositionEncoding::Utf16,
        cancel,
        std: &crate::features::stdlib::StdService::none(),
        uris: &crate::text::uri::UriMap::new(),
        constants: None,
        reports: None,
    };
    handler(&cx, analyzed)
}

/// A client that takes `documentChanges`, and annotations when `notes`.
const fn documents(notes: bool) -> EditCaps {
    EditCaps {
        document_changes: true,
        change_annotations: notes,
        honors_change_annotations: false,
    }
}

/// A range as `line:col-line:col`.
fn range(range: &Value) -> String {
    let p = |at: &Value| format!("{}:{}", at["line"], at["character"]);
    format!("{}-{}", p(&range["start"]), p(&range["end"]))
}

/// Each edit of a list as `line:col-line:col text`, with `@id` for an
/// annotated one.
fn shown(edits: &Value) -> Vec<String> {
    edits
        .as_array()
        .unwrap()
        .iter()
        .map(|edit| {
            let annotation = edit
                .get("annotationId")
                .map_or_else(String::new, |id| format!(" @{}", id.as_str().unwrap()));
            format!(
                "{} {}{annotation}",
                range(&edit["range"]),
                edit["newText"].as_str().unwrap()
            )
        })
        .collect()
}

/// The message of `error`, which must be `RequestFailed`.
#[track_caller]
fn failed(error: Result<HandlerOutput, RequestError>) -> String {
    match error {
        Err(RequestError::Failed(message)) => message,
        other => panic!("expected RequestFailed, got {other:?}"),
    }
}

/// The `window/showMessage`s of `effects`, as `type: message`.
fn messages(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Notify { method, params } if *method == "window/showMessage" => Some(format!(
                "{}: {}",
                params["type"],
                params["message"].as_str()?
            )),
            _ => None,
        })
        .collect()
}

/// A line of text before the name, not ASCII, so a column in UTF-16
/// differs from one in bytes.
const MAIN: &str = "fn main() {
  let count = 1
  let pair = (\"😀é\", ‸count)
  assert pair == (\"😀é\", 1)
}
";

/// D-1673's pair to `null`: a name that can be renamed is its token's
/// range, in the client's encoding, and its text as the placeholder.
#[test]
fn prepare_answers_the_range_and_the_placeholder() {
    let case = Case::main("prepare", MAIN);
    let output = case.prepare().expect("an answer");
    assert_eq!(output.effects, []);
    // `  let pair = ("` is 15 units, `😀` two and `é` one, `", ` three.
    assert_eq!(
        output.value,
        json!({
            "range": {
                "start": { "line": 2, "character": 21 },
                "end": { "line": 2, "character": 26 },
            },
            "placeholder": "count",
        })
    );
}

/// X §3.1: where no name is, `prepareRename` is `null`: whitespace, a
/// keyword, a literal and a test's name.
#[test]
fn prepare_is_null_where_there_is_no_name() {
    for text in [
        "fn main() {\n ‸ let x = 1\n  assert x == 1\n}\n",
        "fn main() {\n  ‸let x = 1\n  assert x == 1\n}\n",
        "fn main() {\n  let x = ‸1\n  assert x == 1\n}\n",
        "fn main() {\n  assert 1 == 1\n}\n\ntest \"o‸ne\" {\n  assert 1 == 1\n}\n",
    ] {
        let case = Case::main("prepare_null", text);
        let output = case.prepare().expect("an answer");
        assert_eq!(output.value, Value::Null, "{text}");
        assert!(output.effects.is_empty(), "{text}");
    }
}

/// X §3.1: a name that cannot be renamed is `RequestFailed`, so the user
/// hears why, at `prepareRename` and at `rename` alike.
#[test]
fn a_refusal_is_request_failed() {
    let text = "import std::io\n\nfn main() {\n  ‸io::println(\"hi\")\n}\n";
    let case = Case::main("refusal", text);
    let reason = "a module's name is its file's name: rename the file";
    assert_eq!(failed(case.prepare()), reason);
    assert_eq!(failed(case.rename("out", documents(true))), reason);
    // The new name is checked by `rename` only (X §3.2 step 3).
    let case = Case::main("bad_name", MAIN);
    assert!(case.prepare().is_ok());
    assert_eq!(
        failed(case.rename("x-y", documents(true))),
        "`x-y` is not a valid name"
    );
}

/// D-1673: in a std document, `prepareRename` refuses every name, a
/// local's too, with D-1390's message.
#[test]
fn prepare_refuses_every_name_of_a_std_document() {
    static BASE: OnceLock<Arc<StdBase>> = OnceLock::new();
    let base = BASE.get_or_init(|| StdBase::build(Arc::new(Globals::new())).expect("std checks"));
    let analyzed = Analyzed::new(Arc::clone(base.snapshot()));
    let source = cheby_driver::std_lib::STD_MODULES
        .iter()
        .find(|m| m.path == "option")
        .expect("std::option")
        .source;
    // The first parameter of the first function: a local.
    let (before, _) = source.split_once("pub fn ").expect("a function");
    let line = before.matches('\n').count();
    let rest = source.get(before.len()..).unwrap();
    let open = rest.find('(').expect("a parameter list");
    let col = open.checked_add(1).unwrap();
    let path = std_dir().join("std").join("option.cheby");
    let at = (u32::try_from(line).unwrap(), u32::try_from(col).unwrap());
    let error = run(&analyzed, EditCaps::default(), |cx, analyzed| {
        prepare_in(cx, analyzed, position(&path, at), Some(std_dir()))
    });
    let message = failed(error);
    assert!(
        message.ends_with("is in `std`, which cannot be edited here"),
        "{message}"
    );
}

/// The util module, closed in the tests, whose `helper` main calls.
const UTIL: &str = "//! Util.\n\npub fn helper() -> Int {\n  1\n}\n";

/// Main, open, calling `util::helper` with the cursor on it.
const CALLER: &str = "import app::util\n\nfn main() {\n  assert util::‸helper() == 1\n}\n";

/// G1 plan §1.2: with `documentChanges`, one `TextDocumentEdit` per
/// file, by path, the open one with its version and the closed one with
/// `null` (D-1445).
#[test]
fn rename_answers_document_changes_with_versions() {
    let case = Case::new(
        "documents",
        &[("main.cheby", CALLER), ("util.cheby", UTIL)],
        &["main.cheby"],
    );
    let output = case.rename("assist", documents(false)).expect("an edit");
    assert_eq!(output.effects, []);
    let edit = &output.value;
    assert!(edit.get("changes").is_none(), "{edit}");
    let docs = edit["documentChanges"].as_array().unwrap();
    assert_eq!(docs.len(), 2, "{edit}");
    assert_eq!(
        docs[0]["textDocument"],
        json!({ "uri": case.uri("main.cheby"), "version": VERSION })
    );
    assert_eq!(shown(&docs[0]["edits"]), ["3:15-3:21 assist"]);
    assert_eq!(
        docs[1]["textDocument"],
        json!({ "uri": case.uri("util.cheby"), "version": null })
    );
    assert_eq!(shown(&docs[1]["edits"]), ["2:7-2:13 assist"]);
}

/// D-1669: a client with neither `documentChanges` nor annotations gets
/// a `changes` map, with no version.
#[test]
fn a_plain_client_gets_changes() {
    let case = Case::new(
        "changes",
        &[("main.cheby", CALLER), ("util.cheby", UTIL)],
        &["main.cheby"],
    );
    let output = case.rename("assist", EditCaps::default()).expect("an edit");
    let edit = &output.value;
    assert!(edit.get("documentChanges").is_none(), "{edit}");
    let changes = edit["changes"].as_object().unwrap();
    assert_eq!(changes.len(), 2, "{edit}");
    assert_eq!(
        shown(&changes[&case.uri("main.cheby")]),
        ["3:15-3:21 assist"]
    );
    assert_eq!(
        shown(&changes[&case.uri("util.cheby")]),
        ["2:7-2:13 assist"]
    );
}

/// A function whose satisfaction nothing uses.
const AREA: &str = "interface Area {
  fn area(Self) -> Int
}

type Square { Square(Int) }

fn ‸area(s: Square) -> Int {
  let Square(n) = s
  n * n
}

fn main() {
  assert area(Square(2)) == 4
}
";

/// D-1391: the note goes as an annotation needing confirmation when the
/// client takes annotations, and as a warning beside the edit unless the
/// client honors them in rename.
#[test]
fn a_note_is_annotated_or_shown() {
    let case = Case::main("note", AREA);
    let note = "`Square` will no longer satisfy `Area`";
    let output = case.rename("size", documents(true)).expect("an edit");
    assert_eq!(messages(&output.effects), [format!("2: {note}")]);
    let edit = &output.value;
    assert_eq!(
        edit["changeAnnotations"],
        json!({ "cheby.satisfaction": { "label": note, "needsConfirmation": true } })
    );
    assert_eq!(
        shown(&edit["documentChanges"][0]["edits"]),
        [
            "6:3-6:7 size @cheby.satisfaction",
            "12:9-12:13 size @cheby.satisfaction",
        ]
    );
    let honors = EditCaps {
        honors_change_annotations: true,
        ..documents(true)
    };
    let output = case.rename("size", honors).expect("an edit");
    assert_eq!(messages(&output.effects), Vec::<String>::new());
    assert!(output.value.get("changeAnnotations").is_some());
    // No annotations: plain edits, and the warning.
    let output = case.rename("size", EditCaps::default()).expect("an edit");
    assert_eq!(messages(&output.effects), [format!("2: {note}")]);
    assert_eq!(
        shown(&output.value["changes"][&case.uri("main.cheby")]),
        ["6:3-6:7 size", "12:9-12:13 size"]
    );
}

/// D-1389, D-1669, D-1711: a closed file changed on disk since the
/// snapshot is answered `null` with a warning that says so, and the
/// package is swept before the answer, so the next try reads the file
/// again (G1 plan §3 E).
#[test]
fn a_stale_file_is_refused_with_a_sweep() {
    let case = Case::new(
        "stale",
        &[("main.cheby", CALLER), ("util.cheby", UTIL)],
        &["main.cheby"],
    );
    std::fs::write(
        case.temp.src("util.cheby"),
        format!("{UTIL}\npub fn twice() -> Int {{\n  helper() + helper()\n}}\n"),
    )
    .unwrap();
    let output = case.rename("assist", documents(true)).expect("an answer");
    assert_eq!(output.value, Value::Null);
    assert_eq!(
        output.effects,
        [
            Effect::Sweep {
                root: case.temp.0.clone(),
            },
            Effect::show(
                lsp_types::MessageType::WARNING,
                "`src/util.cheby` changed on disk, try again"
            ),
        ]
    );
}

/// D-1675, D-1711: a move that finds a closed file changed on disk is
/// answered `null` too, with the move's warning first, since the client
/// moves the files whatever the answer, and then the sweep.
#[test]
fn a_stale_move_is_refused_with_a_sweep() {
    let case = Case::new(
        "stale_move",
        &[("main.cheby", CALLER), ("util.cheby", UTIL)],
        &["main.cheby"],
    );
    std::fs::write(
        case.temp.src("util.cheby"),
        format!("{UTIL}\npub fn twice() -> Int {{\n  helper() + helper()\n}}\n"),
    )
    .unwrap();
    let params = json!({
        "files": [{ "oldUri": case.uri("util.cheby"), "newUri": case.uri("tools.cheby") }],
    });
    let output = run(&case.analyzed, documents(true), |cx, analyzed| {
        crate::features::handlers::file_ops::will_rename_files(cx, analyzed, params)
    })
    .expect("an answer");
    assert_eq!(output.value, Value::Null);
    assert_eq!(
        output.effects,
        [
            Effect::show(
                lsp_types::MessageType::WARNING,
                "`src/util.cheby` changed on disk, so imports were not updated"
            ),
            Effect::Sweep {
                root: case.temp.0.clone(),
            },
        ]
    );
}

/// A new module file on disk that the snapshot lacks is stale too
/// (D-1669).
#[test]
fn a_new_module_file_is_stale() {
    let case = Case::new(
        "new_module",
        &[("main.cheby", CALLER), ("util.cheby", UTIL)],
        &["main.cheby"],
    );
    std::fs::write(
        case.temp.src("extra.cheby"),
        "//! Extra.\n\nimport app::util\n\npub fn more() -> Int {\n  util::helper()\n}\n",
    )
    .unwrap();
    let output = case.rename("assist", documents(true)).expect("an answer");
    assert_eq!(output.value, Value::Null);
    assert_eq!(
        output.effects,
        [
            Effect::Sweep {
                root: case.temp.0.clone(),
            },
            Effect::show(
                lsp_types::MessageType::WARNING,
                "`src/extra.cheby` changed on disk, try again"
            ),
        ]
    );
}

/// X §3.2: renaming a name to itself is an empty edit, not `null`.
#[test]
fn the_same_name_is_an_empty_edit() {
    let case = Case::main("same", MAIN);
    let output = case.rename("count", documents(true)).expect("an edit");
    assert_eq!(output.effects, []);
    assert_eq!(output.value, json!({ "documentChanges": [] }));
    let output = case.rename("count", EditCaps::default()).expect("an edit");
    assert_eq!(output.value, json!({ "changes": {} }));
}

/// No name at the cursor, and a document the snapshot does not hold, are
/// `null`.
#[test]
fn nothing_to_rename_is_null() {
    let case = Case::main("nothing", "fn main() {\n ‸ let x = 1\n  assert x == 1\n}\n");
    let output = case.rename("y", documents(true)).expect("an answer");
    assert_eq!(output.value, Value::Null);
    let elsewhere = case.temp.src("missing.cheby");
    let mut params = position(&elsewhere, (0, 0));
    params["newName"] = json!("y");
    let output = run(&case.analyzed, documents(true), |cx, analyzed| {
        rename_in(cx, analyzed, params, None)
    })
    .expect("an answer");
    assert_eq!(output.value, Value::Null);
}

/// A cancelled request stops, and parameters of another shape are
/// `InvalidParams`.
#[test]
fn cancelled_and_malformed_requests_fail() {
    let case = Case::main("cancel", MAIN);
    let cancel = CancelToken::new();
    cancel.cancel();
    assert_eq!(
        case.rename_with("total", documents(true), &cancel)
            .unwrap_err(),
        RequestError::Cancelled
    );
    let no_name = case.position();
    let error = run(&case.analyzed, documents(true), |cx, analyzed| {
        rename_in(cx, analyzed, no_name, None)
    });
    assert!(
        matches!(error, Err(RequestError::InvalidParams(_))),
        "{error:?}"
    );
    let error = run(&case.analyzed, documents(true), |cx, analyzed| {
        prepare_in(cx, analyzed, json!({ "position": 1 }), None)
    });
    assert!(
        matches!(error, Err(RequestError::InvalidParams(_))),
        "{error:?}"
    );
}

/// D-1667: a single-file document renames as a package's does, checked
/// the same way.
#[test]
fn a_single_file_document_renames() {
    let case = Case::single("single", MAIN);
    let prepared = case.prepare().expect("an answer");
    assert_eq!(prepared.value["placeholder"], "count");
    let output = case.rename("total", documents(true)).expect("an edit");
    let edit = &output.value;
    let docs = edit["documentChanges"].as_array().unwrap();
    assert_eq!(docs.len(), 1, "{edit}");
    assert_eq!(
        docs[0]["textDocument"],
        json!({ "uri": uri(&case.path), "version": VERSION })
    );
    assert_eq!(
        shown(&docs[0]["edits"]),
        ["1:6-1:11 total", "2:21-2:26 total"]
    );
    // A new error is refused there as in a package.
    assert!(
        failed(case.rename("main", documents(true))).starts_with("Renaming would cause: "),
        "the check runs in single-file mode"
    );
}

/// D-1706: in a std checkout's session, `prepareRename` refuses std's API
/// with `RequestFailed` and its message, `rename` writes a package-visible
/// helper across two std modules as a workspace edit with each open
/// document's version, and `willRenameFiles` answers `null` with the
/// message that says where std's modules are listed.
#[test]
fn a_std_checkout_renames_and_refuses() {
    use crate::features::ide::checkout_tests::Checkout;

    let checkout = Checkout::new("handler_rename");
    let a = checkout.read("std/probe_a.cheby");
    let b = checkout.read("std/probe_b.cheby");
    let analyzed = checkout.analyze(&[("std/probe_a.cheby", &a), ("std/probe_b.cheby", &b)]);
    let at = |display: &str, text: &str, needle: &str| {
        let plain = needle.replace(CURSOR, "");
        assert_eq!(text.matches(&plain).count(), 1, "`{plain}` occurs once");
        let marked = text.replacen(&plain, needle, 1);
        let (_, _, cursor) = take_cursor(&[(display, &marked)]);
        position(&checkout.at(display), cursor)
    };
    let refused = run(&analyzed, EditCaps::default(), |cx, analyzed| {
        prepare_in(
            cx,
            analyzed,
            at("std/probe_b.cheby", &b, "list::‸map(doubled"),
            Some(std_dir()),
        )
    });
    assert_eq!(
        failed(refused),
        "`list::map` is `pub`: std's API also lives in `docs/stdlib/`, the examples and the tests"
    );
    let mut params = at("std/probe_a.cheby", &a, "fn ‸helper(");
    params["newName"] = json!("doubler");
    let output = run(&analyzed, documents(false), |cx, analyzed| {
        rename_in(cx, analyzed, params, Some(std_dir()))
    })
    .expect("an edit");
    assert_eq!(output.effects, []);
    let changes = output.value["documentChanges"].as_array().expect("changes");
    let files: Vec<(String, Value, Vec<String>)> = changes
        .iter()
        .map(|change| {
            (
                change["textDocument"]["uri"].as_str().unwrap().to_owned(),
                change["textDocument"]["version"].clone(),
                shown(&change["edits"]),
            )
        })
        .collect();
    assert_eq!(
        files,
        [
            (
                uri(&checkout.at("std/probe_a.cheby")),
                json!(1),
                vec![
                    "5:3-5:9 doubler".to_owned(),
                    "14:9-14:15 doubler".to_owned()
                ]
            ),
            (
                uri(&checkout.at("std/probe_b.cheby")),
                json!(1),
                vec!["8:25-8:31 doubler".to_owned()]
            ),
        ]
    );
    let moved = json!({
        "files": [{
            "oldUri": uri(&checkout.at("std/probe_a.cheby")),
            "newUri": uri(&checkout.at("std/probe_z.cheby")),
        }],
    });
    let output = run(&analyzed, documents(true), |cx, analyzed| {
        crate::features::handlers::file_ops::will_rename_files(cx, analyzed, moved)
    })
    .expect("an answer");
    assert_eq!(output.value, Value::Null);
    assert_eq!(
        output.effects,
        [Effect::show(
            lsp_types::MessageType::WARNING,
            "The imports were not updated: std's modules are listed in \
             `crates/cheby_driver/src/std_lib.rs` and specified in `docs/stdlib/`"
        )]
    );
}
