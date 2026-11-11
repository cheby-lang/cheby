//! The child's JSON document, read and written by both sides (A §11.3 as
//! plan §1.8 amends it, D-1410, D-1412).
//!
//! The child writes exactly one [`Envelope`] to stdout, then exits with
//! status 0 whenever the document is complete, whatever its diagnostics
//! say. The server reads it with [`Envelope::parse`]. Unknown keys are
//! ignored on reading, so sections can grow; a section absent from
//! `--want` is absent here.
//!
//! ```json
//! {
//!   "schema": 1,
//!   "status": "ok",
//!   "manifest": "9f86d081884c7d659a2feaa0c55ad015",
//!   "files": [{ "path": "src/main.cheby", "hash": "2c26b46b68ffc68ff99b453c1d304134" }],
//!   "front": "clean",
//!   "sections": {
//!     "constants": {
//!       "status": "ok",
//!       "diagnostics": [{ "code": "E0075", "path": "src/main.cheby", "range": [120, 131], … }],
//!       "values": [{ "symbol": { … }, "text": "20", "truncated": false }]
//!     }
//!   }
//! }
//! ```
//!
//! - `status`: `ok`, `no-package`, or `tool-error` and `internal-error`
//!   with a `message`. Only `ok` carries the other fields.
//! - Hashes are [`Hash128`] hex over normalized text, and ranges are byte
//!   offsets in that text: a leading BOM removed and `\r\n` read as `\n`
//!   (D-1412). The server maps them onto live text with `NormMap`.
//! - Paths are relative to the package root and use `/`.
//! - A section's `status` is `ok`, `skipped` with a `reason`, or `failed`
//!   with a `message`.
//!
//! Symbols travel as [`SymbolData`], the serde mirror of the driver's
//! `Symbol` (plan §1.3), which has no serde.

use std::path::{Path, PathBuf};

use cheby_driver::analysis::{
    Code, Hash128, Member, ModulePath, PlainDiagnostic, PlainEdit, PlainHelp, PlainLabel, Severity,
    Symbol, SymbolNs, TextRange,
};
use serde::{Deserialize, Serialize};

/// The schema this build writes and reads: `--schema` and the document's
/// `schema` (D-1410). It guards against a stale `current_exe` after an
/// upgrade under a running server.
pub const SCHEMA: u32 = 1;

/// The whole document (A §11.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// [`SCHEMA`] of the writer.
    pub schema: u32,
    /// Whether the job could run at all.
    #[serde(flatten)]
    pub status: Status,
    /// The manifest's hash, for `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<WireHash>,
    /// Every module file read, with its hash, for `ok`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileHash>,
    /// How the front end ended, for `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub front: Option<Front>,
    /// The sections asked for, for `ok`.
    #[serde(default)]
    pub sections: Sections,
}

impl Envelope {
    /// Reads one document. Unknown keys are ignored.
    ///
    /// # Errors
    ///
    /// The parser's message for text that is not one document of this
    /// shape, such as a document cut short when the child was killed.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|e| e.to_string())
    }

    /// The document as one line of JSON.
    ///
    /// # Errors
    ///
    /// The serializer's message; the types here serialize without one.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string(self).map_err(|e| e.to_string())
    }
}

/// The document's `status` (A §11.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum Status {
    /// The job ran: the other fields hold its results.
    Ok,
    /// No `cheby.toml` at `--root`.
    NoPackage,
    /// What `cheby check` treats as a tool error.
    ToolError {
        /// The error's text.
        message: String,
    },
    /// An internal compiler error, or a `--schema` this build does not
    /// write.
    InternalError {
        /// The error's text.
        message: String,
    },
}

/// A [`Hash128`] as 32 lowercase hex digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WireHash(pub Hash128);

impl Serialize for WireHash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_hex())
    }
}

impl<'de> Deserialize<'de> for WireHash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Hash128::from_hex(&text)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom(format!("not a 128-bit hex hash: `{text}`")))
    }
}

/// A half-open byte range `[start, end]` in normalized text (D-1412).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WireRange(pub u32, pub u32);

impl From<TextRange> for WireRange {
    fn from(range: TextRange) -> Self {
        Self(range.start(), range.end())
    }
}

impl From<WireRange> for TextRange {
    fn from(range: WireRange) -> Self {
        Self::new(range.0, range.1)
    }
}

/// One file the child read (D-1412).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHash {
    /// Relative to the package root, with `/`.
    pub path: String,
    /// The hash of its normalized text.
    pub hash: WireHash,
}

/// How the child's front end ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Front {
    /// No error: the sections ran.
    Clean,
    /// The saved files do not check, so every section is `skipped`
    /// (plan §1.8).
    Errors,
}

/// The sections asked for by `--want`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sections {
    /// `--want constants`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constants: Option<Section<ConstantsSection>>,
    /// `--want ownership`: O §7.2's `files`. The W15 wave gives it its
    /// type; until then it is read and written as raw JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership: Option<serde_json::Value>,
}

/// One section's outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum Section<T> {
    /// The section ran.
    Ok(T),
    /// The section did not run.
    Skipped {
        /// Why, such as "front-end errors" or "Windows" (D-1414).
        reason: String,
    },
    /// The section failed inside.
    Failed {
        /// The failure's text.
        message: String,
    },
}

/// The constants section (D-1413).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConstantsSection {
    /// E0069 and E0075 to E0078, in `cheby check`'s order.
    #[serde(default)]
    pub diagnostics: Vec<WireDiagnostic>,
    /// Every evaluated constant's value; empty when evaluation failed.
    #[serde(default)]
    pub values: Vec<WireValue>,
}

/// One constant's value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireValue {
    /// The constant.
    pub symbol: SymbolData,
    /// Its debug text, at most `--max-text` characters (D-1413).
    pub text: String,
    /// The text was cut.
    #[serde(default)]
    pub truncated: bool,
}

/// One diagnostic of the child, as `PlainDiagnostic` with paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireDiagnostic {
    /// `E0075` and the like.
    pub code: String,
    /// `error` or `warning`.
    pub severity: WireSeverity,
    /// The header message.
    pub message: String,
    /// The file of the primary range; `cheby.toml` for none.
    pub path: String,
    /// The primary range, `None` when the diagnostic has no span.
    #[serde(default)]
    pub range: Option<WireRange>,
    /// The primary label's message, empty for none.
    #[serde(default)]
    pub primary_message: String,
    /// Every other label.
    #[serde(default)]
    pub related: Vec<WireLabel>,
    /// The `= note:` lines.
    #[serde(default)]
    pub notes: Vec<String>,
    /// The `= help:` lines.
    #[serde(default)]
    pub helps: Vec<WireHelp>,
    /// The constant that failed, so the server marks it and keeps the
    /// others (D-1413). `None` when no constant holds the range.
    #[serde(default)]
    pub symbol: Option<SymbolData>,
}

/// A diagnostic's severity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WireSeverity {
    /// An error.
    Error,
    /// A warning.
    Warning,
}

/// A label other than the first primary one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireLabel {
    /// Its file, relative to the root.
    pub path: String,
    /// Its range in that file's normalized text.
    pub range: WireRange,
    /// Its message, empty for none.
    #[serde(default)]
    pub message: String,
}

/// A `help` line with its edits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireHelp {
    /// The help's message.
    pub message: String,
    /// Its machine-applicable edits.
    #[serde(default)]
    pub edits: Vec<WireEdit>,
}

/// One replacement of a [`WireHelp`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireEdit {
    /// Its file, relative to the root.
    pub path: String,
    /// The range replaced, in normalized text.
    pub range: WireRange,
    /// The new text.
    pub replacement: String,
}

/// A path relative to the package root as the wire spells it: its
/// components joined by `/` on every platform.
#[must_use]
pub fn path_text(path: &Path) -> String {
    let parts: Vec<_> = path
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect();
    parts.join("/")
}

/// The relative path a wire path names, in this platform's spelling.
#[must_use]
pub fn path_of(text: &str) -> PathBuf {
    text.split('/').filter(|part| !part.is_empty()).collect()
}

impl WireDiagnostic {
    /// `diagnostic`, whose primary range lies in `path`, as the wire
    /// carries it. Every path is relative to the root and every range is in
    /// normalized text already (D-1412); this only changes spelling.
    #[must_use]
    pub fn from_plain(path: &Path, diagnostic: &PlainDiagnostic, symbol: Option<&Symbol>) -> Self {
        Self {
            code: diagnostic.code.to_string(),
            severity: match diagnostic.severity {
                Severity::Error => WireSeverity::Error,
                Severity::Warning => WireSeverity::Warning,
            },
            message: diagnostic.message.clone(),
            path: path_text(path),
            range: diagnostic.range.map(WireRange::from),
            primary_message: diagnostic.primary_message.clone(),
            related: diagnostic
                .related
                .iter()
                .map(|label| WireLabel {
                    path: path_text(&label.path),
                    range: label.range.into(),
                    message: label.message.clone(),
                })
                .collect(),
            notes: diagnostic.notes.clone(),
            helps: diagnostic
                .helps
                .iter()
                .map(|help| WireHelp {
                    message: help.message.clone(),
                    edits: help
                        .edits
                        .iter()
                        .map(|edit| WireEdit {
                            path: path_text(&edit.path),
                            range: edit.range.into(),
                            replacement: edit.replacement.clone(),
                        })
                        .collect(),
                })
                .collect(),
            symbol: symbol.map(SymbolData::from),
        }
    }

    /// The diagnostic back as plain data, with paths relative to the root
    /// and ranges still in normalized text: the server maps them onto live
    /// text with `NormMap` (D-1412). `None` for a code this build cannot
    /// read, such as one a newer writer added.
    #[must_use]
    pub fn to_plain(&self) -> Option<PlainDiagnostic> {
        Some(PlainDiagnostic {
            code: Code::parse(&self.code)?,
            severity: match self.severity {
                WireSeverity::Error => Severity::Error,
                WireSeverity::Warning => Severity::Warning,
            },
            message: self.message.clone(),
            range: self.range.map(TextRange::from),
            primary_message: self.primary_message.clone(),
            related: self
                .related
                .iter()
                .map(|label| PlainLabel {
                    path: path_of(&label.path),
                    range: label.range.into(),
                    message: label.message.clone(),
                })
                .collect(),
            notes: self.notes.clone(),
            helps: self
                .helps
                .iter()
                .map(|help| PlainHelp {
                    message: help.message.clone(),
                    edits: help
                        .edits
                        .iter()
                        .map(|edit| PlainEdit {
                            path: path_of(&edit.path),
                            range: edit.range.into(),
                            replacement: edit.replacement.clone(),
                        })
                        .collect(),
                })
                .collect(),
        })
    }
}

/// The serde mirror of the driver's `Symbol` (plan §1.3, D-1365), for
/// child output and `data` payloads.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SymbolData {
    /// The module's segments, the package name first.
    pub module: Vec<String>,
    /// The namespace.
    pub ns: NsData,
    /// The item's name; empty for a module or an import alias.
    #[serde(default)]
    pub name: String,
    /// What inside the item.
    #[serde(default)]
    pub member: MemberData,
}

/// The mirror of `SymbolNs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NsData {
    /// The module itself.
    Module,
    /// Types, aliases and interfaces.
    Type,
    /// Functions, constants and constructors.
    Value,
    /// A `test` item.
    Test,
}

/// The mirror of `Member`.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum MemberData {
    /// The item itself.
    #[default]
    None,
    /// A field of the type.
    Field {
        /// The field's name.
        name: String,
    },
    /// A function of the interface.
    InterfaceFn {
        /// The function's name.
        name: String,
    },
    /// A type parameter of the item.
    TypeParam {
        /// The parameter's name.
        name: String,
    },
    /// A local of the item.
    Local {
        /// The local's name.
        name: String,
        /// Which local of that name, in source order.
        ordinal: u32,
    },
    /// An import's `as` name in the module.
    ImportAlias {
        /// The alias.
        name: String,
    },
}

impl From<&Symbol> for SymbolData {
    fn from(symbol: &Symbol) -> Self {
        let ns = match symbol.ns {
            SymbolNs::Module => NsData::Module,
            SymbolNs::Type => NsData::Type,
            SymbolNs::Value => NsData::Value,
            SymbolNs::Test => NsData::Test,
        };
        let member = match &symbol.member {
            Member::None => MemberData::None,
            Member::Field(name) => MemberData::Field {
                name: name.to_string(),
            },
            Member::InterfaceFn(name) => MemberData::InterfaceFn {
                name: name.to_string(),
            },
            Member::TypeParam(name) => MemberData::TypeParam {
                name: name.to_string(),
            },
            Member::Local { name, ordinal } => MemberData::Local {
                name: name.to_string(),
                ordinal: *ordinal,
            },
            Member::ImportAlias(name) => MemberData::ImportAlias {
                name: name.to_string(),
            },
        };
        Self {
            module: symbol.module.segments().to_vec(),
            ns,
            name: symbol.name.to_string(),
            member,
        }
    }
}

impl SymbolData {
    /// The driver's `Symbol`. `None` for a module path with no segment.
    #[must_use]
    pub fn to_symbol(&self) -> Option<Symbol> {
        let (package, rest) = self.module.split_first()?;
        let module = rest
            .iter()
            .fold(ModulePath::root(package), |path, segment| {
                path.child(segment)
            });
        let ns = match self.ns {
            NsData::Module => SymbolNs::Module,
            NsData::Type => SymbolNs::Type,
            NsData::Value => SymbolNs::Value,
            NsData::Test => SymbolNs::Test,
        };
        let member = match &self.member {
            MemberData::None => Member::None,
            MemberData::Field { name } => Member::Field(name.as_str().into()),
            MemberData::InterfaceFn { name } => Member::InterfaceFn(name.as_str().into()),
            MemberData::TypeParam { name } => Member::TypeParam(name.as_str().into()),
            MemberData::Local { name, ordinal } => Member::Local {
                name: name.as_str().into(),
                ordinal: *ordinal,
            },
            MemberData::ImportAlias { name } => Member::ImportAlias(name.as_str().into()),
        };
        Some(Symbol {
            module,
            ns,
            name: self.name.as_str().into(),
            member,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> WireHash {
        WireHash(Hash128([byte; 16]))
    }

    fn constant(name: &str) -> SymbolData {
        SymbolData::from(&Symbol::item(
            ModulePath::root("app").child("limits"),
            SymbolNs::Value,
            name,
        ))
    }

    fn full() -> Envelope {
        Envelope {
            schema: SCHEMA,
            status: Status::Ok,
            manifest: Some(hash(0x9f)),
            files: vec![FileHash {
                path: "src/main.cheby".to_owned(),
                hash: hash(0x2c),
            }],
            front: Some(Front::Clean),
            sections: Sections {
                constants: Some(Section::Ok(ConstantsSection {
                    diagnostics: vec![WireDiagnostic {
                        code: "E0075".to_owned(),
                        severity: WireSeverity::Error,
                        message: "constant `boom` panicked".to_owned(),
                        path: "src/main.cheby".to_owned(),
                        range: Some(WireRange(120, 131)),
                        primary_message: String::new(),
                        related: vec![WireLabel {
                            path: "src/limits.cheby".to_owned(),
                            range: WireRange(40, 46),
                            message: "here".to_owned(),
                        }],
                        notes: vec!["a note".to_owned()],
                        helps: vec![WireHelp {
                            message: "a help".to_owned(),
                            edits: vec![WireEdit {
                                path: "src/main.cheby".to_owned(),
                                range: WireRange(0, 0),
                                replacement: "x".to_owned(),
                            }],
                        }],
                        symbol: Some(constant("boom")),
                    }],
                    values: vec![WireValue {
                        symbol: constant("limit"),
                        text: "20".to_owned(),
                        truncated: false,
                    }],
                })),
                ownership: None,
            },
        }
    }

    fn round_trip(envelope: &Envelope) -> Envelope {
        let json = envelope.to_json().expect("serializes");
        Envelope::parse(json.as_bytes()).expect("parses")
    }

    #[test]
    fn full_document_round_trips() {
        let envelope = full();
        assert_eq!(round_trip(&envelope), envelope);
    }

    #[test]
    fn every_status_round_trips() {
        for status in [
            Status::Ok,
            Status::NoPackage,
            Status::ToolError {
                message: "cannot read `src`".to_owned(),
            },
            Status::InternalError {
                message: "schema 2".to_owned(),
            },
        ] {
            let envelope = Envelope {
                schema: SCHEMA,
                status,
                manifest: None,
                files: Vec::new(),
                front: None,
                sections: Sections::default(),
            };
            assert_eq!(round_trip(&envelope), envelope);
        }
    }

    #[test]
    fn skipped_and_failed_sections_round_trip() {
        for section in [
            Section::Skipped {
                reason: "front-end errors".to_owned(),
            },
            Section::Failed {
                message: "internal compiler error".to_owned(),
            },
        ] {
            let mut envelope = full();
            envelope.front = Some(Front::Errors);
            envelope.sections.constants = Some(section);
            assert_eq!(round_trip(&envelope), envelope);
        }
    }

    #[test]
    fn written_shape_is_a_11_3() {
        let value: serde_json::Value =
            serde_json::from_str(&full().to_json().expect("serializes")).expect("is JSON");
        assert_eq!(value["schema"], 1);
        assert_eq!(value["status"], "ok");
        assert_eq!(value["manifest"], "9f".repeat(16));
        assert_eq!(value["front"], "clean");
        let constants = &value["sections"]["constants"];
        assert_eq!(constants["status"], "ok");
        assert_eq!(
            constants["diagnostics"][0]["range"],
            serde_json::json!([120, 131])
        );
        assert_eq!(constants["diagnostics"][0]["severity"], "error");
        assert_eq!(constants["values"][0]["symbol"]["ns"], "value");
        assert_eq!(constants["values"][0]["symbol"]["member"]["kind"], "none");
        assert!(value["sections"].get("ownership").is_none());
    }

    #[test]
    fn reader_ignores_unknown_keys_and_takes_ownership_as_json() {
        let text = r#"{"schema":1,"status":"ok","front":"clean","later":true,
            "sections":{"ownership":{"files":[]},"reuse":{"status":"skipped","reason":"x"}}}"#;
        let envelope = Envelope::parse(text.as_bytes()).expect("parses");
        assert_eq!(envelope.front, Some(Front::Clean));
        assert_eq!(
            envelope.sections.ownership,
            Some(serde_json::json!({ "files": [] }))
        );
    }

    #[test]
    fn truncated_document_is_an_error() {
        let json = full().to_json().expect("serializes");
        let cut = json.get(..json.len() / 2).expect("a prefix");
        assert!(Envelope::parse(cut.as_bytes()).is_err());
    }

    #[test]
    fn bad_hash_is_an_error() {
        let text = r#"{"schema":1,"status":"ok","manifest":"xyz"}"#;
        assert!(Envelope::parse(text.as_bytes()).is_err());
    }

    fn plain() -> PlainDiagnostic {
        PlainDiagnostic {
            code: Code::parse("E0075").expect("a code"),
            severity: Severity::Error,
            message: "constant `boom` panicked".to_owned(),
            range: Some(TextRange::new(120, 131)),
            primary_message: "here".to_owned(),
            related: vec![PlainLabel {
                path: path_of("src/limits.cheby"),
                range: TextRange::new(40, 46),
                message: "read here".to_owned(),
            }],
            notes: vec!["panicked at src/main.cheby:3:1: boom".to_owned()],
            helps: vec![PlainHelp {
                message: "a help".to_owned(),
                edits: vec![PlainEdit {
                    path: path_of("src/main.cheby"),
                    range: TextRange::new(0, 0),
                    replacement: "x".to_owned(),
                }],
            }],
        }
    }

    #[test]
    fn plain_diagnostics_round_trip_through_the_wire() {
        let diagnostic = plain();
        let symbol = Symbol::item(ModulePath::root("app"), SymbolNs::Value, "boom");
        let wire =
            WireDiagnostic::from_plain(&path_of("src/main.cheby"), &diagnostic, Some(&symbol));
        assert_eq!(wire.path, "src/main.cheby");
        assert_eq!(
            wire.related.first().map(|l| l.path.as_str()),
            Some("src/limits.cheby")
        );
        let json = serde_json::to_string(&wire).expect("serializes");
        let back: WireDiagnostic = serde_json::from_str(&json).expect("parses");
        assert_eq!(back, wire);
        assert_eq!(back.to_plain(), Some(diagnostic));
        assert_eq!(back.symbol.and_then(|s| s.to_symbol()), Some(symbol));
    }

    #[test]
    fn warnings_and_spanless_diagnostics_round_trip() {
        let diagnostic = PlainDiagnostic {
            code: Code::parse("W0003").expect("a code"),
            severity: Severity::Warning,
            range: None,
            related: Vec::new(),
            helps: Vec::new(),
            ..plain()
        };
        let wire = WireDiagnostic::from_plain(Path::new("cheby.toml"), &diagnostic, None);
        assert_eq!(wire.severity, WireSeverity::Warning);
        assert_eq!(wire.range, None);
        assert_eq!(wire.to_plain(), Some(diagnostic));
    }

    #[test]
    fn an_unknown_code_has_no_plain_form() {
        let mut wire = WireDiagnostic::from_plain(Path::new("cheby.toml"), &plain(), None);
        wire.code = "X9999".to_owned();
        assert_eq!(wire.to_plain(), None);
    }

    #[test]
    fn wire_paths_use_slashes() {
        let path: PathBuf = ["src", "util", "text.cheby"].iter().collect();
        assert_eq!(path_text(&path), "src/util/text.cheby");
        assert_eq!(path_of("src/util/text.cheby"), path);
        assert_eq!(path_text(Path::new("cheby.toml")), "cheby.toml");
    }

    #[test]
    fn symbols_round_trip_through_data() {
        let module = ModulePath::root("app").child("shapes");
        for symbol in [
            Symbol::of_module(module.clone()),
            Symbol::item(module.clone(), SymbolNs::Value, "LIMIT"),
            Symbol::item(module.clone(), SymbolNs::Test, "adds two"),
            Symbol::field(module.clone(), "Shape", "radius"),
            Symbol::interface_fn(module.clone(), "Area", "area"),
            Symbol::type_param(module.clone(), SymbolNs::Type, "Box", "T"),
            Symbol::local(module.clone(), SymbolNs::Value, "main", "x", 2),
            Symbol::import_alias(module.clone(), SymbolNs::Value, "l"),
        ] {
            let data = SymbolData::from(&symbol);
            let json = serde_json::to_string(&data).expect("serializes");
            let back: SymbolData = serde_json::from_str(&json).expect("parses");
            assert_eq!(back.to_symbol(), Some(symbol));
        }
    }
}
