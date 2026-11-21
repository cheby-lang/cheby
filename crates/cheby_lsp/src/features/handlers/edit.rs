//! The shared edit layer (G1 plan §1.2, X §1.5): every edit that `ide`
//! produces is a [`PlainEdit`] with a byte range in its file's live text,
//! and this module turns a list of them into a protocol `WorkspaceEdit`.
//!
//! Edits are grouped by path and sorted by path, then by start. A client
//! with `workspace.workspaceEdit.documentChanges` gets `TextDocumentEdit`s,
//! versioned for an open document and `null` for a closed one (D-1445);
//! any other client gets a `changes` map (D-1669). With a note, the edits
//! carry the change annotation `cheby.satisfaction` when the client
//! supports annotations, and a `window/showMessage` warning goes with them
//! unless the client honors annotations in rename (D-1391, X §3.4).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use cheby_driver::analysis::PlainEdit;
use lsp_types::{
    AnnotatedTextEdit, ChangeAnnotation, DocumentChanges, MessageType, OneOf,
    OptionalVersionedTextDocumentIdentifier, TextDocumentEdit, TextEdit, Uri, WorkspaceEdit,
};
use serde_json::Value;

use crate::features::handlers::doc::bom;
use crate::features::{Analyzed, RequestCx, RequestError};
use crate::server::Effect;
use crate::text::lines::LspLines;

/// The id of the change annotation that carries a note (D-1391, X §3.4).
const NOTE_ANNOTATION: &str = "cheby.satisfaction";

/// What the client can do with a workspace edit, read from `initialize`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EditCaps {
    /// `workspace.workspaceEdit.documentChanges`: the client takes
    /// versioned `TextDocumentEdit`s.
    pub document_changes: bool,
    /// `workspace.workspaceEdit.changeAnnotationSupport` is present: the
    /// client takes `AnnotatedTextEdit`s.
    pub change_annotations: bool,
    /// `textDocument.rename.honorsChangeAnnotations`: the client asks
    /// before it applies an annotated rename, so the note needs no
    /// `showMessage`.
    pub honors_change_annotations: bool,
}

impl EditCaps {
    /// Reads the edit capabilities of `params`, the `initialize`
    /// parameters. A capability that is missing or of the wrong shape is
    /// absent.
    pub(crate) fn from_params(params: &Value) -> Self {
        let flag = |pointer: &str| {
            params
                .pointer(pointer)
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        Self {
            document_changes: flag("/capabilities/workspace/workspaceEdit/documentChanges"),
            change_annotations: params
                .pointer("/capabilities/workspace/workspaceEdit/changeAnnotationSupport")
                .is_some_and(Value::is_object),
            honors_change_annotations: flag(
                "/capabilities/textDocument/rename/honorsChangeAnnotations",
            ),
        }
    }
}

/// A workspace edit, and the messages the main loop sends before it.
#[derive(Debug)]
pub(crate) struct EditOut {
    /// The edit, in the form the client takes.
    pub edit: WorkspaceEdit,
    /// The note's `window/showMessage`, when the client cannot show it as
    /// an annotation.
    pub effects: Vec<Effect>,
}

/// `edits`, all from `analyzed`'s texts, as a `WorkspaceEdit` for the
/// client of `cx`, with `note` shown as the module doc says.
///
/// Each file's edits keep their given order among equal starts, so two
/// insertions at one place stay in the order the caller wrote them. A
/// note travels as an annotation only in `documentChanges`, the one form
/// that carries `AnnotatedTextEdit`s, so a client that takes annotations
/// but not `documentChanges` gets the `showMessage` too.
///
/// # Errors
///
/// `Cancelled` when the client cancels, and `Failed` for an edit of a
/// file the snapshot does not hold as a package file, or whose path has
/// no URI.
pub(crate) fn to_workspace_edit(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    edits: &[PlainEdit],
    note: Option<&str>,
) -> Result<EditOut, RequestError> {
    cx.cancel.check()?;
    let caps = cx.caps.edit;
    let mut by_path: BTreeMap<&Path, Vec<&PlainEdit>> = BTreeMap::new();
    for edit in edits {
        by_path.entry(edit.path.as_path()).or_default().push(edit);
    }
    let mut files: Vec<FileEdits> = Vec::with_capacity(by_path.len());
    for (path, mut list) in by_path {
        cx.cancel.check()?;
        // A stable sort: equal starts keep the caller's order.
        list.sort_by_key(|edit| edit.range.start());
        files.push(file_edits(cx, analyzed, path, &list)?);
    }
    let annotation = note.filter(|_| caps.document_changes && caps.change_annotations);
    let edit = if caps.document_changes {
        let documents = files
            .into_iter()
            .map(|file| TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: file.uri,
                    version: file.version,
                },
                edits: file
                    .edits
                    .into_iter()
                    .map(|text_edit| match annotation {
                        Some(_) => OneOf::Right(AnnotatedTextEdit {
                            text_edit,
                            annotation_id: NOTE_ANNOTATION.to_owned(),
                        }),
                        None => OneOf::Left(text_edit),
                    })
                    .collect(),
            })
            .collect();
        WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Edits(documents)),
            change_annotations: annotation.map(|label| {
                HashMap::from([(
                    NOTE_ANNOTATION.to_owned(),
                    ChangeAnnotation {
                        label: label.to_owned(),
                        needs_confirmation: Some(true),
                        description: None,
                    },
                )])
            }),
        }
    } else {
        // No version can go with a `changes` map, so the client's open
        // documents are not guarded between the request and the edit
        // (D-1669).
        #[expect(
            clippy::mutable_key_type,
            reason = "`WorkspaceEdit` keys its changes by `Uri`, which the protocol fixes"
        )]
        let changes: HashMap<Uri, Vec<TextEdit>> = files
            .into_iter()
            .map(|file| (file.uri, file.edits))
            .collect();
        WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }
    };
    // A client that honors annotations asks before it applies them, so
    // the note needs no message of its own.
    let honored = annotation.is_some() && caps.honors_change_annotations;
    let effects = note
        .filter(|_| !honored)
        .map(|note| Effect::show(MessageType::WARNING, note))
        .into_iter()
        .collect();
    Ok(EditOut { edit, effects })
}

/// The edits of one file in protocol form.
struct FileEdits {
    /// The file's URI.
    uri: Uri,
    /// The client's version of an open file, `None` for a closed one
    /// (D-1445: stamps stay internal).
    version: Option<i32>,
    /// Its edits, by start.
    edits: Vec<TextEdit>,
}

/// `list`, the edits of the file at `path`, as protocol edits: positions
/// in the session's encoding, with the BOM rule of the file (D-1368).
fn file_edits(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    path: &Path,
    list: &[&PlainEdit],
) -> Result<FileEdits, RequestError> {
    let snapshot = &*analyzed.snapshot;
    let unknown = || {
        RequestError::Failed(format!(
            "an edit names `{}`, which is not a file of the analysis",
            path.display()
        ))
    };
    let file = snapshot.file_of(path).ok_or_else(unknown)?;
    let text = snapshot.source(file).ok_or_else(unknown)?.text();
    let lines = LspLines::new(text, bom(snapshot, file));
    // An open document as the client spells it (A §6.1).
    let uri = cx
        .uris
        .uri_of(path)
        .map_err(|e| RequestError::Failed(e.to_string()))?;
    let edits = list
        .iter()
        .map(|edit| TextEdit {
            range: lines.range(text, edit.range, cx.encoding),
            new_text: edit.replacement.clone(),
        })
        .collect();
    Ok(FileEdits {
        uri,
        version: snapshot.version_of(path),
        edits,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use cheby_driver::analysis::{
        Globals, Overlay, OverlayFile, PackageKind, PlainEdit, Session, SessionConfig, TextRange,
        TextSize,
    };
    use serde_json::{Value, json};

    use super::{EditCaps, EditOut, NOTE_ANNOTATION, to_workspace_edit};
    use crate::features::{Analyzed, CancelToken, RequestCx, RequestError};
    use crate::server::{ClientCaps, Effect, Settings};
    use crate::text::lines::PositionEncoding;
    use crate::text::uri::{UriMap, normalize, path_to_uri};

    /// Each capability is read from its own place, and a missing or
    /// misshapen one is absent.
    #[test]
    fn edit_caps_read_each_capability() {
        assert_eq!(EditCaps::from_params(&json!({})), EditCaps::default());
        let full = json!({ "capabilities": {
            "workspace": { "workspaceEdit": {
                "documentChanges": true,
                "changeAnnotationSupport": { "groupsOnLabel": true },
            } },
            "textDocument": { "rename": { "honorsChangeAnnotations": true } },
        } });
        assert_eq!(
            EditCaps::from_params(&full),
            EditCaps {
                document_changes: true,
                change_annotations: true,
                honors_change_annotations: true,
            }
        );
        let odd = json!({ "capabilities": {
            "workspace": { "workspaceEdit": {
                "documentChanges": "yes",
                "changeAnnotationSupport": true,
            } },
            "textDocument": { "rename": { "honorsChangeAnnotations": false } },
        } });
        assert_eq!(EditCaps::from_params(&odd), EditCaps::default());
        let empty_support = json!({ "capabilities": {
            "workspace": { "workspaceEdit": { "changeAnnotationSupport": {} } },
        } });
        assert_eq!(
            EditCaps::from_params(&empty_support),
            EditCaps {
                change_annotations: true,
                ..EditCaps::default()
            }
        );
    }

    /// The open document: a BOM the client counts, and CRLF line ends.
    const MAIN: &str = "\u{feff}//! Root.\r\n\r\nimport app::shapes\r\n\r\nfn main() {\r\n  let p = shapes::word()\r\n  let _ = p\r\n}\r\n";

    /// A closed file: a BOM that disk hides, and text that is not ASCII.
    const SHAPES: &str =
        "\u{feff}//! Shapes.\n\npub fn word() -> String {\n  \"héllo 😀 wörld\"\n}\n";

    /// The version the client gave the open document.
    const VERSION: i32 = 7;

    /// A package in a fresh temporary directory, removed on drop.
    struct Temp(PathBuf);

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The package of [`MAIN`], open, and [`SHAPES`], closed.
    struct Case {
        temp: Temp,
        analyzed: Analyzed,
    }

    impl Case {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("cheby_lsp_edit_{}_{name}", std::process::id()));
            if dir.exists() {
                std::fs::remove_dir_all(&dir).unwrap();
            }
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(
                dir.join("cheby.toml"),
                "name = \"app\"\nversion = \"0.1.0\"\n",
            )
            .unwrap();
            std::fs::write(dir.join("src/main.cheby"), MAIN).unwrap();
            std::fs::write(dir.join("src/shapes.cheby"), SHAPES).unwrap();
            let temp = Temp(normalize(&std::path::absolute(&dir).unwrap()));
            let mut session = Session::new(SessionConfig {
                root: temp.0.clone(),
                kind: PackageKind::Package,
                target: None,
                globals: Arc::new(Globals::new()),
            });
            let overlay = Overlay {
                files: [(
                    temp.0.join("src/main.cheby"),
                    OverlayFile {
                        text: Arc::from(MAIN),
                        version: VERSION,
                        stamp: 1,
                    },
                )]
                .into_iter()
                .collect(),
            };
            let live = session.analyze_live(&overlay).expect("a run");
            let analyzed = Analyzed::new(live.snapshot.expect("a snapshot"));
            Self { temp, analyzed }
        }

        fn path(&self, file: &str) -> PathBuf {
            self.temp.0.join("src").join(file)
        }

        /// An edit of `file` that replaces the `nth` `needle` of the
        /// snapshot's text from its `skip`th byte to its end.
        fn edit(&self, file: &str, needle: &str, skip: usize, replacement: &str) -> PlainEdit {
            let path = self.path(file);
            let snapshot = &self.analyzed.snapshot;
            let text = snapshot
                .source(snapshot.file_of(&path).unwrap())
                .unwrap()
                .text();
            let at = text.find(needle).unwrap();
            let start = at.checked_add(skip).unwrap();
            let end = at.checked_add(needle.len()).unwrap();
            let range = TextRange::new(
                TextSize::try_from(start).unwrap(),
                TextSize::try_from(end).unwrap(),
            );
            PlainEdit {
                path,
                range,
                replacement: replacement.to_owned(),
            }
        }

        /// An insertion at the start of `needle` in `file`.
        fn insert(&self, file: &str, needle: &str, text: &str) -> PlainEdit {
            let mut edit = self.edit(file, needle, 0, text);
            edit.range = TextRange::empty(edit.range.start());
            edit
        }

        /// The edits of the test, out of order: across files, within a
        /// file, and two insertions at one place.
        fn edits(&self) -> Vec<PlainEdit> {
            vec![
                self.edit("shapes.cheby", "wörld", 0, "world"),
                self.insert("main.cheby", "  let _", "a"),
                self.edit("main.cheby", "let p", 4, "q"),
                self.edit("shapes.cheby", "Shapes", 0, "Forms"),
                self.insert("main.cheby", "  let _", "b"),
                self.edit("main.cheby", "Root", 0, "Main"),
            ]
        }

        /// Runs the conversion for a client with `edit`, in UTF-16.
        fn convert(
            &self,
            edit: EditCaps,
            edits: &[PlainEdit],
            note: Option<&str>,
            cancel: &CancelToken,
        ) -> Result<EditOut, RequestError> {
            self.convert_with(edit, edits, note, cancel, &UriMap::new())
        }

        /// [`Case::convert`] for a client that spelled its open documents
        /// as `uris` records them.
        fn convert_with(
            &self,
            edit: EditCaps,
            edits: &[PlainEdit],
            note: Option<&str>,
            cancel: &CancelToken,
            uris: &UriMap,
        ) -> Result<EditOut, RequestError> {
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
                uris,
                constants: None,
                reports: None,
            };
            to_workspace_edit(&cx, &self.analyzed, edits, note)
        }

        /// [`Case::convert`] of [`Case::edits`], as JSON, with its effects.
        fn run(&self, edit: EditCaps, note: Option<&str>) -> (Value, Vec<Effect>) {
            let out = self
                .convert(edit, &self.edits(), note, &CancelToken::new())
                .expect("an edit");
            (serde_json::to_value(&out.edit).unwrap(), out.effects)
        }

        fn uri(&self, file: &str) -> String {
            path_to_uri(&self.path(file)).unwrap().as_str().to_owned()
        }
    }

    /// An edit as `line:col-line:col text`, with `@id` for an annotation.
    fn show(edit: &Value) -> String {
        let p = |at: &Value| format!("{}:{}", at["line"], at["character"]);
        let annotation = edit
            .get("annotationId")
            .map_or_else(String::new, |id| format!(" @{}", id.as_str().unwrap()));
        format!(
            "{}-{} {}{annotation}",
            p(&edit["range"]["start"]),
            p(&edit["range"]["end"]),
            edit["newText"].as_str().unwrap()
        )
    }

    /// Each file's edits as [`show`] lines.
    fn shown(edits: &Value) -> Vec<String> {
        edits.as_array().unwrap().iter().map(show).collect()
    }

    /// The edits of `main.cheby` and `shapes.cheby`, in the order sent.
    const MAIN_EDITS: [&str; 4] = ["0:5-0:9 Main", "5:6-5:7 q", "6:0-6:0 a", "6:0-6:0 b"];
    const SHAPES_EDITS: [&str; 2] = ["0:4-0:10 Forms", "3:12-3:17 world"];

    /// A §6.1: an open document is named as the client spelled it, here
    /// with a percent-encoded letter, and a closed one by the server's own
    /// URI, in `documentChanges`, whose version then matches the client's
    /// buffer, and in a `changes` map alike.
    #[test]
    fn an_open_document_keeps_the_clients_spelling() {
        let case = Case::new("spelling");
        let spelled: lsp_types::Uri = case
            .uri("main.cheby")
            .replace("/main.cheby", "/%6Dain.cheby")
            .parse()
            .unwrap();
        let mut uris = UriMap::new();
        assert_eq!(uris.insert(&spelled), Some(case.path("main.cheby")));
        let caps = EditCaps {
            document_changes: true,
            ..EditCaps::default()
        };
        let out = case
            .convert_with(caps, &case.edits(), None, &CancelToken::new(), &uris)
            .expect("an edit");
        let value = serde_json::to_value(&out.edit).unwrap();
        let named: Vec<&str> = value["documentChanges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["textDocument"]["uri"].as_str().unwrap())
            .collect();
        assert_eq!(named, [spelled.as_str(), case.uri("shapes.cheby").as_str()]);
        assert_eq!(
            value["documentChanges"][0]["textDocument"]["version"],
            json!(VERSION)
        );
        let out = case
            .convert_with(
                EditCaps::default(),
                &case.edits(),
                None,
                &CancelToken::new(),
                &uris,
            )
            .expect("an edit");
        let value = serde_json::to_value(&out.edit).unwrap();
        let mut keys: Vec<&str> = value["changes"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        let shapes = case.uri("shapes.cheby");
        let mut expected = [spelled.as_str(), shapes.as_str()];
        expected.sort_unstable();
        assert_eq!(keys, expected);
    }

    /// The closed file holds its BOM, so the BOM rule is tested.
    #[test]
    fn the_closed_file_keeps_its_bom() {
        let case = Case::new("bom");
        let snapshot = &case.analyzed.snapshot;
        let shapes = snapshot.file_of(&case.path("shapes.cheby")).unwrap();
        assert!(
            snapshot
                .source(shapes)
                .unwrap()
                .text()
                .starts_with('\u{feff}')
        );
        assert_eq!(snapshot.version_of(&case.path("main.cheby")), Some(VERSION));
        assert_eq!(snapshot.version_of(&case.path("shapes.cheby")), None);
    }

    /// `documentChanges`: one `TextDocumentEdit` per file, by path, the open
    /// file with its version and the closed one with `null`. An open file
    /// counts its BOM and a closed one hides it (D-1368), CRLF ends one
    /// line, and columns count UTF-16 units after `é` and `😀`. Edits go
    /// by start, and two at one place keep their order.
    #[test]
    fn document_changes_carry_versions_and_positions() {
        let case = Case::new("documents");
        let caps = EditCaps {
            document_changes: true,
            ..EditCaps::default()
        };
        let (edit, effects) = case.run(caps, None);
        assert_eq!(effects, []);
        assert!(edit.get("changes").is_none(), "{edit}");
        assert!(edit.get("changeAnnotations").is_none(), "{edit}");
        let documents = edit["documentChanges"].as_array().unwrap();
        assert_eq!(documents.len(), 2, "{edit}");
        assert_eq!(
            documents[0]["textDocument"],
            json!({ "uri": case.uri("main.cheby"), "version": VERSION })
        );
        assert_eq!(shown(&documents[0]["edits"]), MAIN_EDITS);
        assert_eq!(
            documents[1]["textDocument"],
            json!({ "uri": case.uri("shapes.cheby"), "version": null })
        );
        assert_eq!(shown(&documents[1]["edits"]), SHAPES_EDITS);
    }

    /// A client with neither `documentChanges` nor annotations gets a
    /// `changes` map with the same edits and no version (D-1669).
    #[test]
    fn a_plain_client_gets_changes() {
        let case = Case::new("changes");
        let (edit, effects) = case.run(EditCaps::default(), None);
        assert_eq!(effects, []);
        assert!(edit.get("documentChanges").is_none(), "{edit}");
        let changes = edit["changes"].as_object().unwrap();
        assert_eq!(changes.len(), 2, "{edit}");
        assert_eq!(shown(&changes[&case.uri("main.cheby")]), MAIN_EDITS);
        assert_eq!(shown(&changes[&case.uri("shapes.cheby")]), SHAPES_EDITS);
    }

    /// A note: annotated edits and the annotation when the client takes
    /// them, and a `showMessage` warning unless the client honors
    /// annotations in rename (D-1391, X §3.4).
    #[test]
    fn a_note_is_annotated_and_shown_unless_honored() {
        let case = Case::new("note");
        let note = "`Point` will no longer satisfy `Show`";
        let warning = |effects: &[Effect]| match effects {
            [Effect::Notify { method, params }] => {
                assert_eq!(*method, "window/showMessage");
                assert_eq!(params["type"], 2, "a warning");
                assert_eq!(params["message"], note);
                true
            }
            [] => false,
            other => panic!("{other:?}"),
        };
        let annotated = EditCaps {
            document_changes: true,
            change_annotations: true,
            honors_change_annotations: false,
        };
        let (edit, effects) = case.run(annotated, Some(note));
        assert!(warning(&effects));
        assert_eq!(
            edit["changeAnnotations"],
            json!({ "cheby.satisfaction": { "label": note, "needsConfirmation": true } })
        );
        let tagged = |list: &[&str]| -> Vec<String> {
            list.iter()
                .map(|line| format!("{line} @{NOTE_ANNOTATION}"))
                .collect()
        };
        assert_eq!(
            shown(&edit["documentChanges"][0]["edits"]),
            tagged(&MAIN_EDITS)
        );
        assert_eq!(
            shown(&edit["documentChanges"][1]["edits"]),
            tagged(&SHAPES_EDITS)
        );
        // A client that honors annotations asks before it applies them.
        let honors = EditCaps {
            honors_change_annotations: true,
            ..annotated
        };
        let (edit, effects) = case.run(honors, Some(note));
        assert!(!warning(&effects));
        assert!(edit.get("changeAnnotations").is_some(), "{edit}");
        // No annotation support: plain edits, and the warning.
        let plain = EditCaps {
            change_annotations: false,
            ..honors
        };
        let (edit, effects) = case.run(plain, Some(note));
        assert!(warning(&effects));
        assert!(edit.get("changeAnnotations").is_none(), "{edit}");
        assert_eq!(shown(&edit["documentChanges"][0]["edits"]), MAIN_EDITS);
        // Annotations without `documentChanges` have nowhere to go.
        let changes = EditCaps {
            document_changes: false,
            ..honors
        };
        let (edit, effects) = case.run(changes, Some(note));
        assert!(warning(&effects));
        assert!(edit.get("changeAnnotations").is_none(), "{edit}");
        assert_eq!(shown(&edit["changes"][&case.uri("main.cheby")]), MAIN_EDITS);
    }

    /// A cancelled request stops, and an edit of a file the snapshot does
    /// not hold is an error, never a silent drop.
    #[test]
    fn cancelled_and_unknown_files_fail() {
        let case = Case::new("errors");
        let cancel = CancelToken::new();
        cancel.cancel();
        let caps = EditCaps::default();
        let cancelled = case.convert(caps, &case.edits(), None, &cancel);
        assert_eq!(cancelled.unwrap_err(), RequestError::Cancelled);
        let mut stray = case.edit("main.cheby", "Root", 0, "Main");
        stray.path = case.temp.0.join("src/missing.cheby");
        let unknown = case.convert(caps, &[stray], None, &CancelToken::new());
        assert!(
            matches!(&unknown, Err(RequestError::Failed(m)) if m.contains("missing.cheby")),
            "{unknown:?}"
        );
    }
}
