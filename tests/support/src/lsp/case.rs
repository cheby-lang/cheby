//! The golden tests under `tests/lsp/` (`tests/lsp/README.md`).
//!
//! A test is one `.cheby` file, copied into a package as `src/main.cheby`,
//! or a directory with its own `cheby.toml`, copied as it is. Its answers
//! are in `<name>.expected` beside the file or the directory.
//!
//! The header, leading `// @key: value` lines of the file or of the
//! package's `src/main.cheby`, takes these keys:
//!
//! - `@decisions: D-NNN, …`: the decisions the test pins, for `cargo xtask
//!   coverage`;
//! - `@milestone: M<n>`: the milestone in which it must pass, M1 by default
//!   (D-1352, D-288);
//! - `@client: empty|standard|full`: the client capabilities
//!   ([`ClientKind`]), `standard` by default;
//! - `@encoding: utf-8|utf-16|utf-32`: the only position encoding the
//!   client offers;
//! - `@settings: {…}`: the `cheby` settings object, sent as
//!   `initializationOptions` and as the answer to `workspace/configuration`
//!   (D-1376);
//! - `@requests: name, …`: document requests sent for each source file,
//!   such as `documentSymbol` or `formatting`.
//!
//! A marker is a comment line that points at the nearest line above it
//! that is not a marker: `//   ^ hover` puts the position at the caret's
//! column, and `^~~~` makes a range of the caret and the tildes. A marker
//! `//> L:C name args` names the position by the compiler's 1-based line
//! and code-point column instead, for a column that a caret cannot reach.
//! Sources stay valid Cheby either way.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::client::ClientKind;
use super::text::{Encoding, Position};
use crate::milestone::Milestone;

/// The name of the package a single-file test is copied into.
pub const PACKAGE_NAME: &str = "lsp_test";

/// The directory of the golden tests, below the repository root.
pub const DIR: &str = "tests/lsp";

/// The extension of a golden.
pub const GOLDEN_EXTENSION: &str = "expected";

/// The directives of a test's header.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    /// The decisions the test pins.
    pub decisions: BTreeSet<u32>,
    /// The milestone in which the test must pass, M1 by default (D-1352).
    pub milestone: Milestone,
    /// The client capabilities.
    pub client: ClientKind,
    /// The only position encoding the client offers, if any.
    pub encoding: Option<Encoding>,
    /// The `cheby` settings object, if any.
    pub settings: Option<Value>,
    /// Document requests sent for each source file.
    pub requests: Vec<String>,
}

impl Default for Header {
    fn default() -> Self {
        Self {
            decisions: BTreeSet::new(),
            milestone: Milestone::M1,
            client: ClientKind::Standard,
            encoding: None,
            settings: None,
            requests: Vec::new(),
        }
    }
}

impl Header {
    /// Parses the leading `// @key: value` lines of `source`.
    ///
    /// # Errors
    ///
    /// Returns a message for a malformed, repeated or unknown directive.
    pub fn of(source: &str) -> Result<Self, String> {
        let mut header = Self::default();
        let mut seen = BTreeSet::new();
        for line in source.lines() {
            let Some(directive) = line.strip_prefix("// @") else {
                break;
            };
            let (key, value) = directive
                .split_once(':')
                .ok_or_else(|| format!("directive `{line}` has no `:`"))?;
            let (key, value) = (key.trim(), value.trim());
            if !seen.insert(key.to_owned()) {
                return Err(format!("`@{key}` is given twice"));
            }
            match key {
                "decisions" => {
                    for d in value.split(',') {
                        header.decisions.insert(crate::decisions::parse(d)?);
                    }
                }
                "milestone" => {
                    header.milestone = value.parse().map_err(|e| format!("@milestone: {e}"))?;
                }
                "client" => {
                    header.client = ClientKind::parse(value).ok_or_else(|| {
                        format!("@client: `{value}` is not empty, standard or full")
                    })?;
                }
                "encoding" => {
                    header.encoding = Some(Encoding::parse(value).ok_or_else(|| {
                        format!("@encoding: `{value}` is not utf-8, utf-16 or utf-32")
                    })?);
                }
                "settings" => {
                    let settings: Value = serde_json::from_str(value)
                        .map_err(|e| format!("@settings: not JSON: {e}"))?;
                    header.settings = Some(settings);
                }
                "requests" => {
                    for name in value.split(',').map(str::trim) {
                        document_method(name)
                            .ok_or_else(|| format!("@requests: unknown request `{name}`"))?;
                        header.requests.push(name.to_owned());
                    }
                }
                other => {
                    return Err(format!(
                        "unknown directive `@{other}`; the keys are decisions, milestone, \
                         client, encoding, settings and requests"
                    ));
                }
            }
        }
        Ok(header)
    }
}

/// How a test is laid out on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// One `.cheby` file, copied into a package as `src/main.cheby`.
    File,
    /// A directory with `cheby.toml` and `src/**`, copied as it is.
    Package,
}

/// One golden test.
#[derive(Debug, Clone)]
pub struct LspTest {
    /// The path relative to `tests/lsp`, with `/` separators and no
    /// extension.
    pub name: String,
    /// The `.cheby` file or the package directory.
    pub path: PathBuf,
    /// How it is laid out.
    pub layout: Layout,
    /// Its header, or why it does not parse, which fails the trial and
    /// not the whole harness.
    pub header: Result<Header, String>,
}

impl LspTest {
    /// The golden beside the file or the directory.
    #[must_use]
    pub fn golden(&self) -> PathBuf {
        match self.layout {
            Layout::File => self.path.with_extension(GOLDEN_EXTENSION),
            Layout::Package => {
                let mut name = self.path.file_name().unwrap_or_default().to_os_string();
                name.push(".");
                name.push(GOLDEN_EXTENSION);
                self.path.with_file_name(name)
            }
        }
    }

    /// The file that holds the header.
    #[must_use]
    pub fn header_file(&self) -> PathBuf {
        match self.layout {
            Layout::File => self.path.clone(),
            Layout::Package => self.path.join("src/main.cheby"),
        }
    }
}

/// Every golden test, sorted by name, each with its header or why it
/// does not parse.
///
/// # Errors
///
/// Returns a message if a directory cannot be read.
pub fn all(root: &Path) -> Result<Vec<LspTest>, String> {
    let dir = root.join(DIR);
    let mut found = Vec::new();
    if dir.exists() {
        discover(&dir, &mut found).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let mut tests = Vec::with_capacity(found.len());
    for (path, layout) in found {
        let name = path
            .strip_prefix(&dir)
            .unwrap_or(&path)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let mut test = LspTest {
            name,
            path,
            layout,
            header: Ok(Header::default()),
        };
        let file = test.header_file();
        test.header = crate::golden::read(&file)
            .and_then(|text| text.ok_or_else(|| "the file is missing".to_owned()))
            .and_then(|text| Header::of(&text))
            .map_err(|e| format!("{}: {e}", file.display()));
        tests.push(test);
    }
    Ok(tests)
}

fn discover(dir: &Path, out: &mut Vec<(PathBuf, Layout)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if entry.file_type()?.is_dir() {
            if path.join("cheby.toml").is_file() {
                out.push((path, Layout::Package));
            } else {
                discover(&path, out)?;
            }
        } else if path.extension().is_some_and(|e| e == "cheby") {
            out.push((path, Layout::File));
        }
    }
    Ok(())
}

/// One marker of a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// The 0-based line it points at.
    pub line: usize,
    /// The 0-based code-point column of the position.
    pub column: usize,
    /// The end column of a range marker, `^~~`, exclusive.
    pub end: Option<usize>,
    /// The request's short name, such as `hover`.
    pub name: String,
    /// The words after the name, such as a rename's new name.
    pub args: Vec<String>,
}

impl Marker {
    /// The label of the marker's block in a golden: the compiler's 1-based
    /// line and code-point column and the request.
    #[must_use]
    pub fn label(&self, file: &str) -> String {
        let mut label = format!(
            "{file}:{}:{} {}",
            self.line.saturating_add(1),
            self.column.saturating_add(1),
            self.name
        );
        for arg in &self.args {
            label.push(' ');
            label.push_str(arg);
        }
        label
    }
}

/// The markers of `text`, in order.
///
/// # Errors
///
/// Returns a message for a marker with no line above it, no request
/// name, an unknown request, or a bad `//>` position.
pub fn markers(text: &str) -> Result<Vec<Marker>, String> {
    let mut out = Vec::new();
    let mut target = None;
    for (index, line) in text.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let number = index.saturating_add(1);
        let trimmed = line.trim_start();
        let Some(comment) = trimmed.strip_prefix("//") else {
            target = Some(index);
            continue;
        };
        let (position, rest) = if let Some(explicit) = comment.strip_prefix("> ") {
            let (at, rest) = explicit
                .trim_start()
                .split_once(' ')
                .unwrap_or((explicit, ""));
            let (l, c) = at
                .split_once(':')
                .ok_or_else(|| format!("line {number}: `{at}` is not L:C"))?;
            let parse = |s: &str| {
                s.parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .ok_or_else(|| format!("line {number}: `{at}` is not a 1-based L:C"))
            };
            ((parse(l)?, parse(c)?, None), rest.to_owned())
        } else if comment.trim_start().starts_with('^') {
            let Some(line_above) = target else {
                return Err(format!("line {number}: a marker with no line above it"));
            };
            let caret = line
                .chars()
                .position(|c| c == '^')
                .ok_or_else(|| format!("line {number}: no caret"))?;
            let after: String = line.chars().skip(caret.saturating_add(1)).collect();
            let tildes = after.chars().take_while(|&c| c == '~').count();
            let end = (tildes > 0).then(|| caret.saturating_add(1).saturating_add(tildes));
            let rest: String = after.chars().skip(tildes).collect();
            ((line_above, caret, end), rest)
        } else {
            // An ordinary comment: a marker below it still points past it.
            continue;
        };
        let mut words = rest.split_whitespace();
        let name = words
            .next()
            .ok_or_else(|| format!("line {number}: a marker with no request"))?;
        position_method(name).ok_or_else(|| format!("line {number}: unknown request `{name}`"))?;
        let (line, column, end) = position;
        out.push(Marker {
            line,
            column,
            end,
            name: name.to_owned(),
            args: words.map(str::to_owned).collect(),
        });
    }
    Ok(out)
}

/// The method of a request that takes a position or a range, by short
/// name.
#[must_use]
pub fn position_method(name: &str) -> Option<&'static str> {
    Some(match name {
        "hover" => "textDocument/hover",
        "definition" => "textDocument/definition",
        "declaration" => "textDocument/declaration",
        "typeDefinition" => "textDocument/typeDefinition",
        "implementation" => "textDocument/implementation",
        "references" => "textDocument/references",
        "documentHighlight" => "textDocument/documentHighlight",
        "completion" => "textDocument/completion",
        "signatureHelp" => "textDocument/signatureHelp",
        "prepareRename" => "textDocument/prepareRename",
        "rename" => "textDocument/rename",
        "codeAction" => "textDocument/codeAction",
        "prepareCallHierarchy" => "textDocument/prepareCallHierarchy",
        "selectionRange" => "textDocument/selectionRange",
        "rangeFormatting" => "textDocument/rangeFormatting",
        _ => return None,
    })
}

/// The method of a request about a whole document, by short name.
#[must_use]
pub fn document_method(name: &str) -> Option<&'static str> {
    Some(match name {
        "documentSymbol" => "textDocument/documentSymbol",
        "formatting" => "textDocument/formatting",
        "foldingRange" => "textDocument/foldingRange",
        "documentLink" => "textDocument/documentLink",
        "codeLens" => "textDocument/codeLens",
        "semanticTokens" => "textDocument/semanticTokens/full",
        "inlayHint" => "textDocument/inlayHint",
        _ => return None,
    })
}

/// The byte ranges of every identifier and type name token of `text`,
/// in order: the positions the examples group sends its requests at.
#[must_use]
pub fn identifiers(text: &str) -> Vec<(usize, usize)> {
    use cheby_syntax::SyntaxKind;
    let lexed = cheby_syntax::lexer::lex(cheby_diag::FileId::new(0), text);
    (0..lexed.len())
        .filter(|&i| {
            matches!(
                lexed.kind(i),
                SyntaxKind::LOWER_IDENT | SyntaxKind::UPPER_IDENT
            )
        })
        .filter_map(|i| {
            let range = lexed.range(i);
            let start = usize::try_from(range.start()).ok()?;
            let end = usize::try_from(range.end()).ok()?;
            Some((start, end))
        })
        .collect()
}

/// The requests a server advertises in its `capabilities`, by short name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Advertised {
    /// Requests at a position or a range ([`position_method`]), in a fixed
    /// order. `rename` is not among them: it follows a `prepareRename`.
    pub position: Vec<&'static str>,
    /// Requests about a whole document ([`document_method`]).
    pub document: Vec<&'static str>,
    /// Whether `rename` is advertised.
    pub rename: bool,
}

impl Advertised {
    /// Reads `capabilities`. A provider is advertised when its key is
    /// present and neither `false` nor `null`.
    ///
    /// # Errors
    ///
    /// Returns a message for an advertised `…Provider` key this table does
    /// not know, so the examples group never skips a request in silence:
    /// a wave that advertises one adds it here.
    pub fn of(capabilities: &Value) -> Result<Self, String> {
        let has = |key: &str| {
            capabilities
                .get(key)
                .is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
        };
        let position_table = [
            ("hoverProvider", "hover"),
            ("definitionProvider", "definition"),
            ("declarationProvider", "declaration"),
            ("typeDefinitionProvider", "typeDefinition"),
            ("implementationProvider", "implementation"),
            ("referencesProvider", "references"),
            ("documentHighlightProvider", "documentHighlight"),
            ("completionProvider", "completion"),
            ("signatureHelpProvider", "signatureHelp"),
            ("codeActionProvider", "codeAction"),
            ("callHierarchyProvider", "prepareCallHierarchy"),
            ("selectionRangeProvider", "selectionRange"),
            ("documentRangeFormattingProvider", "rangeFormatting"),
        ];
        let document_table = [
            ("documentSymbolProvider", "documentSymbol"),
            ("documentFormattingProvider", "formatting"),
            ("foldingRangeProvider", "foldingRange"),
            ("documentLinkProvider", "documentLink"),
            ("codeLensProvider", "codeLens"),
            ("semanticTokensProvider", "semanticTokens"),
            ("inlayHintProvider", "inlayHint"),
        ];
        let unknown: Vec<&str> = capabilities
            .as_object()
            .into_iter()
            .flat_map(|map| map.keys())
            .map(String::as_str)
            .filter(|key| key.ends_with("Provider") && has(key))
            .filter(|key| {
                *key != "renameProvider"
                    && !position_table.iter().any(|(k, _)| k == key)
                    && !document_table.iter().any(|(k, _)| k == key)
            })
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "the server advertises {}, which `Advertised::of` does not know",
                unknown.join(", ")
            ));
        }
        let mut position: Vec<&'static str> = position_table
            .iter()
            .filter(|(key, _)| has(key))
            .map(|&(_, name)| name)
            .collect();
        let rename = has("renameProvider");
        let prepare = capabilities
            .get("renameProvider")
            .and_then(|r| r.get("prepareProvider"))
            .and_then(Value::as_bool)
            == Some(true);
        if prepare {
            position.push("prepareRename");
        }
        let document = document_table
            .iter()
            .filter(|(key, _)| has(key))
            .map(|&(_, name)| name)
            .collect();
        Ok(Self {
            position,
            document,
            rename,
        })
    }
}

/// Formatting options every formatting request sends.
#[must_use]
pub fn formatting_options() -> Value {
    json!({ "tabSize": 2, "insertSpaces": true })
}

/// The params of position request `name` at `start`, or at the range
/// `start..end`, of `uri`. `diagnostics` are those published for the
/// file, of which a code action gets the ones its range touches.
///
/// # Errors
///
/// Returns a message for an unknown request or missing arguments.
pub fn position_params(
    name: &str,
    uri: &str,
    start: Position,
    end: Option<Position>,
    args: &[String],
    diagnostics: &[Value],
) -> Result<(&'static str, Value), String> {
    let method = position_method(name).ok_or_else(|| format!("unknown request `{name}`"))?;
    let document = json!({ "uri": uri });
    let range = json!({ "start": start.to_json(), "end": end.unwrap_or(start).to_json() });
    let params = match name {
        "references" => json!({
            "textDocument": document,
            "position": start.to_json(),
            "context": { "includeDeclaration": !args.iter().any(|a| a == "noDeclaration") },
        }),
        "rename" => {
            let new_name = args.first().ok_or("`rename` needs the new name after it")?;
            json!({ "textDocument": document, "position": start.to_json(), "newName": new_name })
        }
        "codeAction" => {
            let touching: Vec<Value> = diagnostics
                .iter()
                .filter(|d| touches(d, start, end.unwrap_or(start)))
                .cloned()
                .collect();
            let mut context = json!({ "diagnostics": touching });
            if let (Some(only), Some(map)) = (args.first(), context.as_object_mut()) {
                map.insert("only".to_owned(), json!([only]));
            }
            json!({ "textDocument": document, "range": range, "context": context })
        }
        "selectionRange" => json!({ "textDocument": document, "positions": [start.to_json()] }),
        "rangeFormatting" => json!({
            "textDocument": document,
            "range": range,
            "options": formatting_options(),
        }),
        "completion" => json!({
            "textDocument": document,
            "position": start.to_json(),
            "context": { "triggerKind": 1 },
        }),
        _ => json!({ "textDocument": document, "position": start.to_json() }),
    };
    Ok((method, params))
}

/// The params of document request `name` of `uri`, whose last line is
/// `last` (for ranged requests over the whole text).
///
/// # Errors
///
/// Returns a message for an unknown request.
pub fn document_params(
    name: &str,
    uri: &str,
    last: Position,
) -> Result<(&'static str, Value), String> {
    let method = document_method(name).ok_or_else(|| format!("unknown request `{name}`"))?;
    let document = json!({ "uri": uri });
    let params = match name {
        "formatting" => json!({ "textDocument": document, "options": formatting_options() }),
        "inlayHint" => json!({
            "textDocument": document,
            "range": { "start": { "line": 0, "character": 0 }, "end": last.to_json() },
        }),
        _ => json!({ "textDocument": document }),
    };
    Ok((method, params))
}

/// Whether the range of diagnostic `d` touches `start..end`.
fn touches(d: &Value, start: Position, end: Position) -> bool {
    let range = d.get("range").unwrap_or(&Value::Null);
    let (Ok(a), Ok(b)) = (
        Position::from_json(range.get("start").unwrap_or(&Value::Null)),
        Position::from_json(range.get("end").unwrap_or(&Value::Null)),
    ) else {
        return false;
    };
    a <= end && start <= b
}

/// Sorts what a request answers where the order means nothing, so goldens
/// do not depend on it: locations of `references` and the edits of a
/// `rename`, by path, line and column.
#[must_use]
pub fn canonical(name: &str, answer: Value) -> Value {
    fn key(v: &Value) -> (String, u64, u64) {
        let uri = v
            .get("uri")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let start = v
            .get("range")
            .and_then(|r| r.get("start"))
            .unwrap_or(&Value::Null);
        let n = |f: &str| start.get(f).and_then(Value::as_u64).unwrap_or(0);
        (uri, n("line"), n("character"))
    }
    match (name, answer) {
        ("references", Value::Array(mut items)) => {
            items.sort_by_key(key);
            Value::Array(items)
        }
        ("rename", Value::Object(mut edit)) => {
            if let Some(Value::Object(changes)) = edit.get_mut("changes") {
                for edits in changes.values_mut() {
                    if let Value::Array(items) = edits {
                        items.sort_by_key(key);
                    }
                }
            }
            Value::Object(edit)
        }
        (_, answer) => answer,
    }
}

/// One block of a golden: `== <label>` and pretty JSON.
#[must_use]
pub fn block(label: &str, value: &Value) -> String {
    let body = serde_json::to_string_pretty(value).unwrap_or_else(|e| format!("<{e}>"));
    format!("== {label}\n{body}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers() {
        let header = Header::of(
            "// @decisions: D-1352\n// @client: empty\n// @encoding: utf-32\n\
             // @settings: {\"analysis\": {\"debounceMs\": 0}}\n\
             // @requests: documentSymbol, formatting\nfn main() {}\n",
        )
        .unwrap();
        assert_eq!(header.decisions, BTreeSet::from([1352]));
        assert_eq!(header.milestone, Milestone::M1);
        assert_eq!(header.client, ClientKind::Empty);
        assert_eq!(header.encoding, Some(Encoding::Utf32));
        assert_eq!(
            header.settings,
            Some(json!({"analysis": {"debounceMs": 0}}))
        );
        assert_eq!(header.requests, ["documentSymbol", "formatting"]);
        assert_eq!(Header::of("fn main() {}\n").unwrap(), Header::default());
        for bad in [
            "// @mode: check-pass\n",
            "// @client: odd\n",
            "// @encoding: utf-7\n",
            "// @settings: {\n",
            "// @requests: hover\n",
            "// @milestone: M2\n// @milestone: M3\n",
            "// @decisions D-1\n",
        ] {
            assert!(Header::of(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn caret_markers() {
        let text = "fn main() {\n  let total = add(1, 2)\n  //          ^ hover\n  // a note\n  //    ^~~~ codeAction quickfix\n}\n";
        let found = markers(text).unwrap();
        assert_eq!(
            found,
            [
                Marker {
                    line: 1,
                    column: 14,
                    end: None,
                    name: "hover".to_owned(),
                    args: vec![],
                },
                Marker {
                    line: 1,
                    column: 8,
                    end: Some(12),
                    name: "codeAction".to_owned(),
                    args: vec!["quickfix".to_owned()],
                },
            ]
        );
        assert_eq!(
            found[0].label("src/main.cheby"),
            "src/main.cheby:2:15 hover"
        );
    }

    #[test]
    fn explicit_markers() {
        let found = markers("fn main() {}\n//> 1:1 rename start\n").unwrap();
        assert_eq!(found[0].line, 0);
        assert_eq!(found[0].column, 0);
        assert_eq!(found[0].args, ["start"]);
        assert!(markers("//> 0:1 hover\n").is_err());
        assert!(markers("//> 1 hover\n").is_err());
    }

    #[test]
    fn bad_markers() {
        assert!(markers("// ^ hover\n").is_err());
        assert!(markers("x\n// ^\n").is_err());
        assert!(markers("x\n// ^ frobnicate\n").is_err());
    }

    #[test]
    fn identifier_tokens() {
        let text = "fn main() {\n  let total = io::x(1)\n}\n";
        let words: Vec<&str> = identifiers(text)
            .into_iter()
            .map(|(a, b)| text.get(a..b).unwrap())
            .collect();
        assert_eq!(words, ["main", "total", "io", "x"]);
    }

    #[test]
    fn advertised_requests() {
        let caps = json!({
            "hoverProvider": true,
            "definitionProvider": false,
            "renameProvider": {"prepareProvider": true},
            "documentFormattingProvider": {},
            "textDocumentSync": 2,
        });
        let found = Advertised::of(&caps).unwrap();
        assert_eq!(found.position, ["hover", "prepareRename"]);
        assert_eq!(found.document, ["formatting"]);
        assert!(found.rename);
        assert_eq!(Advertised::of(&json!({})), Ok(Advertised::default()));
        let ranged = Advertised::of(&json!({ "documentRangeFormattingProvider": true })).unwrap();
        assert_eq!(ranged.position, ["rangeFormatting"]);
        // An unknown provider fails, unless it is not advertised.
        assert!(Advertised::of(&json!({ "workspaceSymbolProvider": true })).is_err());
        assert!(Advertised::of(&json!({ "workspaceSymbolProvider": false })).is_ok());
    }

    #[test]
    fn params_and_sorting() {
        let at = Position {
            line: 1,
            character: 2,
        };
        let (method, params) =
            position_params("rename", "file:///a", at, None, &["b".to_owned()], &[]).unwrap();
        assert_eq!(method, "textDocument/rename");
        assert_eq!(params["newName"], json!("b"));
        assert!(position_params("rename", "file:///a", at, None, &[], &[]).is_err());
        let d = json!({"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 5}}});
        let far = json!({"range": {"start": {"line": 9, "character": 0}, "end": {"line": 9, "character": 1}}});
        let (_, params) =
            position_params("codeAction", "file:///a", at, None, &[], &[d, far]).unwrap();
        assert_eq!(
            params["context"]["diagnostics"].as_array().map(Vec::len),
            Some(1)
        );
        let refs = json!([
            {"uri": "file:///b", "range": {"start": {"line": 0, "character": 0}}},
            {"uri": "file:///a", "range": {"start": {"line": 3, "character": 1}}},
            {"uri": "file:///a", "range": {"start": {"line": 1, "character": 9}}},
        ]);
        let sorted = canonical("references", refs);
        assert_eq!(sorted[0]["range"]["start"]["line"], json!(1));
        assert_eq!(sorted[2]["uri"], json!("file:///b"));
    }
}
