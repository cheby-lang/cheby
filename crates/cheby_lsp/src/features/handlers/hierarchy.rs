//! `textDocument/prepareCallHierarchy`, `callHierarchy/incomingCalls` and
//! `callHierarchy/outgoingCalls` (X §2.8), over `ide::hierarchy`.
//!
//! All three are class `Package`: an answer points into other files, so it
//! needs every open document of the package at the client's text (A §3.3,
//! D-1367). `prepareCallHierarchy` names its document by
//! `textDocument.uri`, the other two by `item.data.origin`, the document
//! the item was first prepared in, else by `item.uri`. So a std function's
//! callers come from the package that asked, not from std.
//!
//! An item's `data` is its [`Symbol`] as plain strings ([`CallKey`]), never
//! a handle (X §1.4, D-1365). An item whose key no longer resolves in the
//! snapshot gets `[]`, as a stale item should (X §2.8).
//!
//! [`Locator`] turns a snapshot's file and byte range into a protocol
//! location. `documentLink` and `workspace/symbol` use it too.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use cheby_driver::analysis::{
    DefId, DefKind, FileId, FileLocation, Member, ModulePath, Snapshot, Symbol, SymbolNs,
    SymbolTarget, TextRange, TextSize,
};
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    Location, Position, Range, SymbolKind, SymbolTag, Uri,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::features::handlers::std_file;
use crate::features::ide::hierarchy::{self as ide, CallItem};
use crate::features::ide::is_synthetic;
use crate::features::{Analyzed, HandlerOutput, RequestCx, RequestError, params};
use crate::text::lines::{Bom, LspLines, PositionEncoding};
use crate::text::uri::{normalize, path_to_uri, uri_to_path};

/// `textDocument/prepareCallHierarchy` (class `Package`): the function at
/// the position, as one item, or `null`. `std_dir` is where std's sources
/// are on disk (D-1369), for an item declared in std.
///
/// # Errors
///
/// `InvalidParams` for parameters of another shape, `Cancelled` when the
/// client cancels.
pub(crate) fn prepare(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
    std_dir: Option<&Path>,
) -> Result<HandlerOutput, RequestError> {
    let p: CallHierarchyPrepareParams = params(params_value)?;
    let at = p.text_document_position_params;
    let snapshot = &analyzed.snapshot;
    let mut locator = Locator::new(snapshot, std_dir, cx.encoding);
    locator.origin = Some(at.text_document.uri.as_str().to_owned());
    let Some(file) = locator.open(&at.text_document.uri) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    let Some(offset) = locator.offset(file, at.position) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    cx.cancel.check()?;
    let item = ide::prepare(analyzed.index(), file, offset).and_then(|item| locator.item(&item));
    to_output(item.map(|item| vec![item]))
}

/// `callHierarchy/incomingCalls` (class `Package`): the items that call
/// `params.item`, each with its calls.
///
/// # Errors
///
/// `InvalidParams` for parameters of another shape, `Cancelled` when the
/// client cancels.
pub(crate) fn incoming_calls(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
    std_dir: Option<&Path>,
) -> Result<HandlerOutput, RequestError> {
    let p: CallHierarchyIncomingCallsParams = params(params_value)?;
    let snapshot = &analyzed.snapshot;
    let mut locator = Locator::new(snapshot, std_dir, cx.encoding);
    locator.open(&p.item.uri);
    locator.origin = Some(origin_of(&p.item));
    let Some(def) = def_of(snapshot, &p.item) else {
        return to_output(Vec::<CallHierarchyIncomingCall>::new());
    };
    let calls = ide::incoming(analyzed.index(), def, cx.cancel)?;
    let mut out = Vec::with_capacity(calls.len());
    for call in calls {
        cx.cancel.check()?;
        let Some(from) = locator.item(&call.item) else {
            continue;
        };
        let Some(from_ranges) = locator.ranges(call.item.file, &call.ranges) else {
            continue;
        };
        out.push(CallHierarchyIncomingCall { from, from_ranges });
    }
    to_output(out)
}

/// `callHierarchy/outgoingCalls` (class `Package`): the functions that
/// `params.item` calls, each with its calls in `params.item`.
///
/// # Errors
///
/// `InvalidParams` for parameters of another shape, `Cancelled` when the
/// client cancels.
pub(crate) fn outgoing_calls(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
    std_dir: Option<&Path>,
) -> Result<HandlerOutput, RequestError> {
    let p: CallHierarchyOutgoingCallsParams = params(params_value)?;
    let snapshot = &analyzed.snapshot;
    let mut locator = Locator::new(snapshot, std_dir, cx.encoding);
    locator.open(&p.item.uri);
    locator.origin = Some(origin_of(&p.item));
    let Some(caller) = def_of(snapshot, &p.item).and_then(|def| ide::item(snapshot, def)) else {
        return to_output(Vec::<CallHierarchyOutgoingCall>::new());
    };
    let calls = ide::outgoing(analyzed.index(), caller.def, cx.cancel)?;
    let mut out = Vec::with_capacity(calls.len());
    for call in calls {
        cx.cancel.check()?;
        let Some(to) = locator.item(&call.item) else {
            continue;
        };
        // The ranges are in the caller, the item the request names.
        let Some(from_ranges) = locator.ranges(caller.file, &call.ranges) else {
            continue;
        };
        out.push(CallHierarchyOutgoingCall { to, from_ranges });
    }
    to_output(out)
}

/// `value` as the response.
fn to_output<T: Serialize>(value: T) -> Result<HandlerOutput, RequestError> {
    serde_json::to_value(value)
        .map(HandlerOutput::value)
        .map_err(|e| RequestError::Failed(e.to_string()))
}

/// The document `item` was first prepared in: its key's `origin`, else
/// its own URI. Items it leads to carry it on.
fn origin_of(item: &CallHierarchyItem) -> String {
    item.data
        .as_ref()
        .and_then(|data| data.get("origin"))
        .and_then(Value::as_str)
        .map_or_else(|| item.uri.as_str().to_owned(), str::to_owned)
}

/// The item a client sends back, in this snapshot: by its `data`. `None`
/// when the data is missing or odd, or names nothing here now.
fn def_of(snapshot: &Snapshot, item: &CallHierarchyItem) -> Option<DefId> {
    let key: CallKey = serde_json::from_value(item.data.clone()?).ok()?;
    match snapshot.find(&key.symbol()?)? {
        SymbolTarget::Item(def) => Some(def),
        SymbolTarget::Module(_)
        | SymbolTarget::Field { .. }
        | SymbolTarget::TypeParam { .. }
        | SymbolTarget::Local { .. }
        | SymbolTarget::ImportAlias { .. } => None,
    }
}

/// The `data` of a call hierarchy item: its [`Symbol`] as plain strings,
/// so it holds across snapshots (X §1.4, D-1365). Only items, functions,
/// constants and tests, are ever items of the hierarchy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CallKey {
    /// The declaring module's path, such as `app::shapes`.
    pub module: String,
    /// `value`, `type` or `test`.
    pub ns: String,
    /// The item's name: a test's string, an interface function's
    /// interface.
    pub name: String,
    /// An interface function's own name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_fn: Option<String>,
    /// The URI of the document the item was first prepared in. The calls
    /// of the item are routed by it, so the package that asked answers
    /// them, even for an item declared in std or in a dependency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl CallKey {
    /// The key of `symbol`. `None` for a symbol that names no item.
    pub(crate) fn of(symbol: &Symbol) -> Option<Self> {
        let ns = match symbol.ns {
            SymbolNs::Value => "value",
            SymbolNs::Type => "type",
            SymbolNs::Test => "test",
            SymbolNs::Module => return None,
        };
        let interface_fn = match &symbol.member {
            Member::None => None,
            Member::InterfaceFn(name) => Some(name.to_string()),
            Member::Field(_)
            | Member::TypeParam(_)
            | Member::Local { .. }
            | Member::ImportAlias(_) => return None,
        };
        Some(Self {
            module: symbol.module.to_string(),
            ns: ns.to_owned(),
            name: symbol.name.to_string(),
            interface_fn,
            origin: None,
        })
    }

    /// The symbol back. `None` for a key no [`CallKey::of`] makes.
    pub(crate) fn symbol(&self) -> Option<Symbol> {
        let module = module_path(&self.module)?;
        let ns = match self.ns.as_str() {
            "value" => SymbolNs::Value,
            "type" => SymbolNs::Type,
            "test" => SymbolNs::Test,
            _ => return None,
        };
        Some(match &self.interface_fn {
            None => Symbol::item(module, ns, self.name.as_str()),
            Some(func) if ns == SymbolNs::Type => {
                Symbol::interface_fn(module, self.name.as_str(), func.as_str())
            }
            Some(_) => return None,
        })
    }
}

/// The module path that `text`, such as `app::shapes`, writes.
pub(crate) fn module_path(text: &str) -> Option<ModulePath> {
    let mut segments = text.split("::");
    let first = segments.next().filter(|s| !s.is_empty())?;
    segments.try_fold(ModulePath::root(first), |path, segment| {
        (!segment.is_empty()).then(|| path.child(segment))
    })
}

/// Turns a snapshot's files and byte ranges into protocol locations, in
/// the session's encoding (D-1368). A package file is its path, a std file
/// its materialized copy under `std_dir` (D-1369), and a file with no
/// place on disk has no location. The request's own document keeps the
/// URI the client sent, so the client sees its own spelling.
pub(crate) struct Locator<'a> {
    snapshot: &'a Snapshot,
    std_dir: Option<&'a Path>,
    enc: PositionEncoding,
    /// The documents the request named, by path, with the URI it used.
    named: Vec<(PathBuf, Uri)>,
    /// The `origin` of the call hierarchy items it makes ([`CallKey`]).
    pub(crate) origin: Option<String>,
    lines: HashMap<FileId, LspLines>,
}

impl<'a> Locator<'a> {
    /// A locator over `snapshot`.
    pub(crate) fn new(
        snapshot: &'a Snapshot,
        std_dir: Option<&'a Path>,
        enc: PositionEncoding,
    ) -> Self {
        Self {
            snapshot,
            std_dir,
            enc,
            named: Vec::new(),
            origin: None,
            lines: HashMap::new(),
        }
    }

    /// Notes that the request names `uri`, and gives its file in the
    /// snapshot: a package file, or a materialized std file of the std
    /// base. `None` for a URI that names no file of the snapshot.
    pub(crate) fn open(&mut self, uri: &Uri) -> Option<FileId> {
        let path = normalize(&uri_to_path(uri)?);
        let file = self
            .snapshot
            .file_of(&path)
            .or_else(|| std_file(self.snapshot, &path, self.std_dir?));
        if !self.named.iter().any(|(p, _)| *p == path) {
            self.named.push((path, uri.clone()));
        }
        file
    }

    /// Where `file` is on disk.
    pub(crate) fn path(&self, file: FileId) -> Option<PathBuf> {
        match self.snapshot.location(file)? {
            FileLocation::Package(path) => Some(path.clone()),
            FileLocation::Std(display) => Some(self.std_dir?.join(display)),
            FileLocation::Synthetic => None,
        }
    }

    /// The URI of `path`: the one the request used for it, else its own.
    pub(crate) fn uri_of_path(&self, path: &Path) -> Option<Uri> {
        if let Some((_, uri)) = self.named.iter().find(|(p, _)| p == path) {
            return Some(uri.clone());
        }
        path_to_uri(path).ok()
    }

    /// The URI of `file`.
    pub(crate) fn uri(&self, file: FileId) -> Option<Uri> {
        self.uri_of_path(&self.path(file)?)
    }

    /// The line index of `file`, made once. A leading BOM counts in an
    /// open document's text, not in disk text (D-1368).
    fn lines(&mut self, file: FileId) -> Option<(&'a str, &LspLines)> {
        let snapshot = self.snapshot;
        let text = snapshot.source(file)?.text();
        let lines = self.lines.entry(file).or_insert_with(|| {
            let open = match snapshot.location(file) {
                Some(FileLocation::Package(path)) => snapshot.version_of(path).is_some(),
                Some(FileLocation::Std(_) | FileLocation::Synthetic) | None => false,
            };
            LspLines::new(text, if open { Bom::Counted } else { Bom::Hidden })
        });
        Some((text, lines))
    }

    /// `range` of `file` in the session's encoding.
    pub(crate) fn range(&mut self, file: FileId, range: TextRange) -> Option<Range> {
        let enc = self.enc;
        let (text, lines) = self.lines(file)?;
        Some(lines.range(text, range, enc))
    }

    /// Every range of `ranges`, all in `file`.
    pub(crate) fn ranges(&mut self, file: FileId, ranges: &[TextRange]) -> Option<Vec<Range>> {
        ranges.iter().map(|&r| self.range(file, r)).collect()
    }

    /// The byte offset of `position` in `file`.
    pub(crate) fn offset(&mut self, file: FileId, position: Position) -> Option<TextSize> {
        let enc = self.enc;
        let (text, lines) = self.lines(file)?;
        Some(lines.offset(text, position, enc))
    }

    /// `range` of `file` as a location.
    pub(crate) fn location(&mut self, file: FileId, range: TextRange) -> Option<Location> {
        let uri = self.uri(file)?;
        Some(Location::new(uri, self.range(file, range)?))
    }

    /// `item` in protocol form, with its key as `data`.
    pub(crate) fn item(&mut self, item: &CallItem) -> Option<CallHierarchyItem> {
        let uri = self.uri(item.file)?;
        let key = CallKey {
            origin: self.origin.clone(),
            ..CallKey::of(&item.symbol)?
        };
        let data = serde_json::to_value(key).ok()?;
        Some(CallHierarchyItem {
            name: item.name.clone(),
            kind: match item.kind {
                DefKind::Const => SymbolKind::CONSTANT,
                DefKind::Fn
                | DefKind::InterfaceFn
                | DefKind::Test
                | DefKind::Type
                | DefKind::Alias
                | DefKind::Interface
                | DefKind::Ctor => SymbolKind::FUNCTION,
            },
            tags: item.deprecated.then(|| vec![SymbolTag::DEPRECATED]),
            // A single-file document names no package (D-1477).
            detail: (!is_synthetic(self.snapshot, item.def.module))
                .then(|| item.module.to_string()),
            uri,
            range: self.range(item.file, item.range)?,
            selection_range: self.range(item.file, item.selection)?,
            data: Some(data),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use cheby_driver::analysis::{Symbol, SymbolNs};
    use serde_json::{Value, json};

    use super::{CallKey, incoming_calls, module_path, outgoing_calls, prepare};
    use crate::features::Analyzed;
    use crate::features::handlers::symbols::tests::with_cx;
    use crate::features::ide::hierarchy::tests::{Temp, analyze};
    use crate::text::uri::path_to_uri;

    const MAIN: &str = "import app::other\n\nfn f(n: Int) -> Int {\n  other::g(n) + 1\n}\n\nfn main() {\n  assert f(1) == 3\n}\n";
    const OTHER: &str = "pub fn g(n: Int) -> Int {\n  n + 1\n}\n";

    fn uri(path: &Path) -> String {
        path_to_uri(path).unwrap().as_str().to_owned()
    }

    fn prepared(analyzed: &Analyzed, file: &Path, line: u32, character: u32) -> Value {
        let p = json!({
            "textDocument": { "uri": uri(file) },
            "position": { "line": line, "character": character },
        });
        with_cx(|cx| prepare(cx, analyzed, p, None)).unwrap().value
    }

    #[test]
    fn keys_round_trip() {
        let module = module_path("app::shapes").unwrap();
        let symbols = [
            Symbol::item(module.clone(), SymbolNs::Value, "area"),
            Symbol::item(module.clone(), SymbolNs::Test, "areas add up"),
            Symbol::interface_fn(module.clone(), "Show", "show"),
        ];
        for symbol in symbols {
            let key = CallKey::of(&symbol).unwrap();
            let json = serde_json::to_value(&key).unwrap();
            let back: CallKey = serde_json::from_value(json).unwrap();
            assert_eq!(back.symbol(), Some(symbol));
        }
        assert_eq!(CallKey::of(&Symbol::of_module(module)), None);
        assert_eq!(module_path(""), None);
        assert_eq!(module_path("app::"), None);
        assert_eq!(
            module_path("std").map(|p| p.to_string()),
            Some("std".to_owned())
        );
    }

    #[test]
    #[cfg_attr(windows, ignore = "builds a JIT until M6 (D-545, D-641)")]
    fn prepare_then_incoming_and_outgoing() {
        let temp = Temp::package("handler", &[("main.cheby", MAIN), ("other.cheby", OTHER)]);
        let main = temp.src("main.cheby");
        let other = temp.src("other.cheby");
        let analyzed = analyze(&temp.0, &[(main.clone(), MAIN)]);
        let items = prepared(&analyzed, &main, 3, 10);
        let item = items.get(0).cloned().expect("one item");
        assert_eq!(item["name"], json!("g"));
        assert_eq!(item["kind"], json!(12));
        assert_eq!(item["detail"], json!("app::other"));
        assert_eq!(item["uri"], json!(uri(&other)));
        assert_eq!(
            item["selectionRange"],
            json!({ "start": { "line": 0, "character": 7 }, "end": { "line": 0, "character": 8 } })
        );
        assert_eq!(
            item["data"],
            json!({ "module": "app::other", "ns": "value", "name": "g", "origin": uri(&main) })
        );
        let got = with_cx(|cx| incoming_calls(cx, &analyzed, json!({ "item": item }), None))
            .unwrap()
            .value;
        assert_eq!(got.as_array().map(Vec::len), Some(1));
        assert_eq!(got[0]["from"]["name"], json!("f"));
        assert_eq!(got[0]["from"]["uri"], json!(uri(&main)));
        assert_eq!(
            got[0]["fromRanges"],
            json!([{ "start": { "line": 3, "character": 9 }, "end": { "line": 3, "character": 10 } }])
        );
        let f = got[0]["from"].clone();
        let out = with_cx(|cx| outgoing_calls(cx, &analyzed, json!({ "item": f }), None))
            .unwrap()
            .value;
        assert_eq!(out.as_array().map(Vec::len), Some(1));
        assert_eq!(out[0]["to"]["name"], json!("g"));
        assert_eq!(out[0]["fromRanges"], got[0]["fromRanges"]);
        // Nothing to prepare on a keyword, and a stale item gets `[]`.
        assert_eq!(prepared(&analyzed, &main, 7, 3), Value::Null);
        let mut stale = item;
        stale["data"]["name"] = json!("gone");
        let got = with_cx(|cx| incoming_calls(cx, &analyzed, json!({ "item": stale }), None))
            .unwrap()
            .value;
        assert_eq!(got, json!([]));
    }

    #[test]
    #[cfg_attr(windows, ignore = "builds a JIT until M6 (D-545, D-641)")]
    fn bad_params_are_invalid_params() {
        let temp = Temp::package("bad_params", &[("main.cheby", "fn main() {}\n")]);
        let analyzed = analyze(&temp.0, &[]);
        let got = with_cx(|cx| prepare(cx, &analyzed, json!(3), None));
        assert!(matches!(
            got,
            Err(crate::features::RequestError::InvalidParams(_))
        ));
    }
}
