//! `textDocument/completion` and `completionItem/resolve` (X §4.1, §4.6).
//!
//! - [`completion`] is class `Doc`: it reads the document's snapshot at the
//!   exact text the request saw (D-1367). It reads the settings
//!   `completion.autoImport` and `completion.typeDriven` (D-1376), and the
//!   client capabilities of [`CompletionCaps`]. A std file of the std base
//!   is found by its display path, as signature help finds it. A file that
//!   is no module of the run, such as one with no package module behind
//!   it, is parsed here and gets the L0 items of its context.
//! - [`resolve`] is class `Workspace`: the item carries no document, only
//!   its `data`, so it reads the newest snapshot of every package and
//!   picks the one holding the document the item came from. It adds the
//!   item's docs, by the hover's path (D-1385), and a note when the item
//!   exists on one target only. An item it cannot place comes back
//!   unchanged (X §4.1).
//!
//! An item's `data` is its symbol as plain strings, with the document's
//! URI as `origin` (X §1.4, D-1365): the call hierarchy's key.

use std::path::Path;
use std::str::FromStr;

use cheby_driver::analysis::{Snapshot, Symbol, SymbolTarget, Targets, item_docs};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, CompletionItemTag,
    CompletionList, CompletionParams, CompletionTextEdit, Documentation, InsertTextFormat,
    InsertTextMode, MarkupContent, MarkupKind, TextEdit, Uri,
};
use serde_json::Value;

use super::hierarchy::CallKey;
use crate::features::ide::completion::items::EntryKind;
use crate::features::ide::completion::{Completions, Entry, Options, StdAccess, complete};
use crate::features::ide::hover::source;
use crate::features::ide::signature::file_at;
use crate::features::{Analyzed, DocText, HandlerOutput, RequestCx, RequestError, params};
use crate::text::lines::{LspLines, PositionEncoding};
use crate::text::uri::{normalize, uri_to_path};

/// What the client can take in a completion, read once from `initialize`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one switch per client capability"
)]
pub(crate) struct CompletionCaps {
    /// `completionItem.snippetSupport`.
    pub snippets: bool,
    /// `completionItem.labelDetailsSupport`.
    pub label_details: bool,
    /// `completionItem.tagSupport` lists `Deprecated`.
    pub deprecated_tag: bool,
    /// `completionItem.deprecatedSupport`, the older flag.
    pub deprecated_flag: bool,
    /// `completionItem.documentationFormat` lists `markdown`.
    pub markdown: bool,
    /// `clientInfo.name` is Zed's, which filters on `filterText` against
    /// the word (X §4.2).
    pub zed: bool,
    /// `completionItem.insertTextModeSupport` lists `asIs`, so an item
    /// that writes its own indentation can say so.
    pub as_is: bool,
}

impl CompletionCaps {
    /// Reads the capabilities of `params`, the `initialize` parameters.
    pub(crate) fn from_params(params: &Value) -> Self {
        let item = params.pointer("/capabilities/textDocument/completion/completionItem");
        let flag = |name: &str| {
            item.and_then(|i| i.get(name))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        let lists = |pointer: &str, wanted: &Value| {
            item.and_then(|i| i.pointer(pointer))
                .and_then(Value::as_array)
                .is_some_and(|values| values.contains(wanted))
        };
        // Zed names itself by its release channel: `Zed`, `Zed Preview`,
        // `Zed Nightly`, `Zed Dev`.
        let zed = params
            .pointer("/clientInfo/name")
            .and_then(Value::as_str)
            .is_some_and(|name| {
                let name = name.to_ascii_lowercase();
                name == "zed" || name.starts_with("zed ")
            });
        Self {
            snippets: flag("snippetSupport"),
            label_details: flag("labelDetailsSupport"),
            deprecated_tag: lists("/tagSupport/valueSet", &Value::from(1)),
            deprecated_flag: flag("deprecatedSupport"),
            markdown: lists("/documentationFormat", &Value::from("markdown")),
            zed,
            as_is: lists("/insertTextModeSupport/valueSet", &Value::from(1)),
        }
    }
}

/// `textDocument/completion`, class `Doc`. `caps` comes from
/// `initialize`, and `std` gives std's index for modules the run did not
/// load. A `:` or `>` that completes neither `::` nor `|>` gets an empty
/// list at once (X §4.1).
///
/// # Errors
///
/// `InvalidParams` when the parameters do not have the method's shape,
/// and `Cancelled` when the client cancelled the request.
pub(crate) fn completion(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
    caps: CompletionCaps,
    std: StdAccess<'_>,
) -> Result<HandlerOutput, RequestError> {
    let p: CompletionParams = params(params_value)?;
    let at = p.text_document_position;
    let Some(path) = uri_to_path(&at.text_document.uri) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    let path = normalize(&path);
    let snapshot = &*analyzed.snapshot;
    let Some(doc) = source(snapshot, &path) else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    cx.cancel.check()?;
    let lines = LspLines::new(doc.text, doc.bom);
    let offset = lines.offset(doc.text, at.position, cx.encoding);
    let trigger = p
        .context
        .and_then(|c| c.trigger_character)
        .unwrap_or_default();
    let before = usize::try_from(offset)
        .ok()
        .and_then(|end| doc.text.get(..end))
        .unwrap_or("");
    let stray = match trigger.as_str() {
        ":" => !before.ends_with("::"),
        ">" => !before.ends_with("|>"),
        _ => false,
    };
    let options = Options {
        snippets: caps.snippets,
        type_driven: cx.settings.completion.type_driven,
        auto_import: cx.settings.completion.auto_import,
        zed: caps.zed,
    };
    let found = if stray {
        Completions::default()
    } else {
        // A package file, or a materialized std file of the std base, as
        // signature help and inlay hints find it.
        let view = file_at(snapshot, &path).and_then(|f| snapshot.module_at(f));
        if let Some((view, tree)) = view.and_then(|v| Some((v, v.tree()?))) {
            complete(snapshot, Some(view), tree, doc.text, offset, options, std)
        } else {
            let Some(own) = DocText::new(path.clone(), None, doc.text.into()) else {
                return Ok(HandlerOutput::value(Value::Null));
            };
            complete(snapshot, None, &own.tree, doc.text, offset, options, std)
        }
    };
    cx.cancel.check()?;
    let conv = Conv {
        text: doc.text,
        lines: &lines,
        enc: cx.encoding,
        caps,
        origin: at.text_document.uri.as_str(),
    };
    let list = CompletionList {
        is_incomplete: found.incomplete,
        items: found.items.iter().map(|e| conv.item(e)).collect(),
    };
    serde_json::to_value(list)
        .map(HandlerOutput::value)
        .map_err(|e| RequestError::Failed(e.to_string()))
}

/// What turns entries into protocol items.
struct Conv<'a> {
    text: &'a str,
    lines: &'a LspLines,
    enc: PositionEncoding,
    caps: CompletionCaps,
    origin: &'a str,
}

impl Conv<'_> {
    /// `entry` as a protocol item.
    fn item(&self, entry: &Entry) -> CompletionItem {
        let range = self.lines.range(self.text, entry.insert.range, self.enc);
        let additional: Vec<TextEdit> = entry
            .additional
            .iter()
            .map(|(range, text)| TextEdit {
                range: self.lines.range(self.text, *range, self.enc),
                new_text: text.clone(),
            })
            .collect();
        let label_details = (self.caps.label_details
            && (entry.label_detail.is_some() || entry.description.is_some()))
        .then(|| CompletionItemLabelDetails {
            detail: entry.label_detail.clone(),
            description: entry.description.clone(),
        });
        let data = entry.key.as_ref().and_then(CallKey::of).map(|mut key| {
            key.origin = Some(self.origin.to_owned());
            key
        });
        CompletionItem {
            label: entry.label.clone(),
            label_details,
            kind: Some(kind(entry.kind)),
            detail: entry.detail.clone(),
            deprecated: (entry.deprecated
                && !self.caps.deprecated_tag
                && self.caps.deprecated_flag)
                .then_some(true),
            tags: (entry.deprecated && self.caps.deprecated_tag)
                .then(|| vec![CompletionItemTag::DEPRECATED]),
            sort_text: Some(entry.sort_text()),
            filter_text: entry.filter_text.clone(),
            insert_text_format: entry.insert.snippet.then_some(InsertTextFormat::SNIPPET),
            // An edit that writes its own indentation is taken as it is,
            // not indented again by a client whose default adjusts it.
            insert_text_mode: (entry.indented && self.caps.as_is).then_some(InsertTextMode::AS_IS),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range,
                new_text: entry.insert.text.clone(),
            })),
            additional_text_edits: (!additional.is_empty()).then_some(additional),
            data: data.and_then(|key| serde_json::to_value(key).ok()),
            ..CompletionItem::default()
        }
    }
}

/// The protocol kind of `kind` (X §4.1).
const fn kind(kind: EntryKind) -> CompletionItemKind {
    match kind {
        EntryKind::Function => CompletionItemKind::FUNCTION,
        EntryKind::Method => CompletionItemKind::METHOD,
        EntryKind::Field => CompletionItemKind::FIELD,
        EntryKind::Variable => CompletionItemKind::VARIABLE,
        EntryKind::Constant => CompletionItemKind::CONSTANT,
        EntryKind::Struct => CompletionItemKind::STRUCT,
        EntryKind::Enum => CompletionItemKind::ENUM,
        EntryKind::EnumMember => CompletionItemKind::ENUM_MEMBER,
        EntryKind::Interface => CompletionItemKind::INTERFACE,
        EntryKind::Module => CompletionItemKind::MODULE,
        EntryKind::TypeParameter => CompletionItemKind::TYPE_PARAMETER,
        EntryKind::Keyword => CompletionItemKind::KEYWORD,
        EntryKind::Snippet => CompletionItemKind::SNIPPET,
        EntryKind::Value => CompletionItemKind::VALUE,
    }
}

/// `completionItem/resolve`, class `Workspace`: `packages` are the newest
/// snapshot of each package. It adds `documentation`, and nothing else:
/// edits are never computed here (X §4.1).
///
/// # Errors
///
/// `InvalidParams` when the parameters are not a completion item, and
/// `Cancelled` when the client cancelled the request.
pub(crate) fn resolve(
    cx: &RequestCx<'_>,
    packages: &[&Analyzed],
    params_value: Value,
    caps: CompletionCaps,
    std: StdAccess<'_>,
) -> Result<HandlerOutput, RequestError> {
    let mut item: CompletionItem = params(params_value)?;
    cx.cancel.check()?;
    let key = item
        .data
        .clone()
        .and_then(|data| serde_json::from_value::<CallKey>(data).ok());
    if let Some(key) = key
        && let Some(symbol) = key.symbol()
    {
        let origin = key
            .origin
            .as_deref()
            .and_then(|uri| Uri::from_str(uri).ok())
            .and_then(|uri| uri_to_path(&uri))
            .map(|path| normalize(&path));
        let snapshot = origin
            .as_deref()
            .and_then(|path| holding(packages, path))
            .or_else(|| {
                packages
                    .iter()
                    .map(|a| &*a.snapshot)
                    .find(|s| s.find(&symbol).is_some())
            });
        if let Some(text) = docs(snapshot, &symbol, std) {
            item.documentation = Some(if caps.markdown {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: text,
                })
            } else {
                Documentation::String(text)
            });
        }
    }
    serde_json::to_value(item)
        .map(HandlerOutput::value)
        .map_err(|e| RequestError::Failed(e.to_string()))
}

/// The snapshot among `packages` that holds the document at `path`.
fn holding<'a>(packages: &[&'a Analyzed], path: &Path) -> Option<&'a Snapshot> {
    packages
        .iter()
        .map(|a| &*a.snapshot)
        .find(|s| s.file_of(path).is_some())
}

/// The docs of `symbol`: from its module's tree in `snapshot`, else from
/// std's index (D-1385), with a note for an item of one target only.
fn docs(snapshot: Option<&Snapshot>, symbol: &Symbol, std: StdAccess<'_>) -> Option<String> {
    let def = snapshot.and_then(|s| match s.find(symbol)? {
        SymbolTarget::Item(def) => Some((s, def)),
        _ => None,
    });
    let local = def.and_then(|(s, def)| {
        let view = s.module(def.module)?;
        item_docs(view.tree()?, view.resolved()?.item(def.index)?)
    });
    let std_item = std.get().and_then(|index| index.get(symbol));
    let text = local.or_else(|| std_item.and_then(|i| i.doc.clone()));
    let targets = def
        .and_then(|(s, def)| s.interfaces().get(def.module)?.item(def.index))
        .map(|item| item.targets)
        .or_else(|| std_item.map(|i| i.targets));
    let note = targets.filter(|&t| t != Targets::ALL).map(|t| {
        let on: Vec<String> = t.iter().map(|t| format!("`{t}`")).collect();
        if on.is_empty() {
            "On no target.".to_owned()
        } else {
            format!("Only on {}.", on.join(" and "))
        }
    });
    let parts: Vec<String> = [note, text.map(|t| cheby_fences(&t))]
        .into_iter()
        .flatten()
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// `docs` with each bare opening fence marked as Cheby, since doc
/// comments are Cheby by default (§13.10). Hover's rule, for the same
/// text.
fn cheby_fences(docs: &str) -> String {
    let mut open = false;
    let lines: Vec<String> = docs
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("```") {
                return line.to_owned();
            }
            let was_open = open;
            open = !open;
            if !was_open && trimmed.trim_end() == "```" {
                let indent = line.len().saturating_sub(trimmed.len());
                format!("{}```cheby", line.get(..indent).unwrap_or(""))
            } else {
                line.to_owned()
            }
        })
        .collect();
    lines.join("\n")
}

#[cfg(test)]
mod tests;
