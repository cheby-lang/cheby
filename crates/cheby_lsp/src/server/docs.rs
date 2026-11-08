//! `didOpen`, `didChange`, `didClose` and `didSave` (A §3.2, §5).
//!
//! The server keeps each open document's text, edited in place by
//! incremental changes, and routes the document to its package by the
//! nearest `cheby.toml` (D-1375). A package gets a session at its first
//! open document. With no file watcher yet, an open, close or save also
//! sweeps the package's files on disk (A §8.3), so a file deleted or
//! changed outside the editor shows at the next of them.

use std::path::{Path, PathBuf};

use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, MessageType, Uri,
};
use serde_json::Value;

use super::Server;
use super::schedule::Urgency;
use super::state::{Doc, Package, package_root};
use crate::analysis_thread::Changed;
use crate::text::lines::apply_change;
use crate::text::uri::normalize;

/// A sweep of disk, for open, close and save.
const SWEEP: Changed = Changed {
    sweep: true,
    paths: Vec::new(),
};

impl Server {
    /// `textDocument/didOpen`.
    pub(crate) fn did_open(&mut self, params: Value) {
        let Some(params) = self.params::<DidOpenTextDocumentParams>("didOpen", params) else {
            return;
        };
        let item = params.text_document;
        let Some(path) = self.uris.insert(&item.uri) else {
            self.log(
                MessageType::LOG,
                format!("not a file: {}", item.uri.as_str()),
            );
            return;
        };
        let root = package_root(&path);
        if let Some(root) = &root {
            self.ensure_package(root);
        }
        let stamp = self.next_stamp();
        self.docs.insert(
            path,
            Doc {
                version: item.version,
                text: item.text,
                shared: None,
                stamp,
                root: root.clone(),
            },
        );
        if let Some(root) = root {
            self.touch(&root, stamp);
            self.mark_dirty(&root, SWEEP, Urgency::Flush);
        }
    }

    /// `textDocument/didChange`.
    pub(crate) fn did_change(&mut self, params: Value) {
        let Some(params) = self.params::<DidChangeTextDocumentParams>("didChange", params) else {
            return;
        };
        let Some(path) = self.open_path(&params.text_document.uri) else {
            return;
        };
        let stamp = self.next_stamp();
        let encoding = self.encoding;
        let Some(doc) = self.docs.get_mut(&path) else {
            return;
        };
        let mut lost = false;
        for change in params.content_changes {
            lost |= !apply_change(&mut doc.text, change.range, &change.text, encoding);
        }
        doc.version = params.text_document.version;
        doc.shared = None;
        doc.stamp = stamp;
        let root = doc.root.clone();
        if lost {
            self.log(
                MessageType::WARNING,
                format!(
                    "a change to {} did not fall on character boundaries and was skipped",
                    path.display()
                ),
            );
        }
        if let Some(root) = root {
            self.touch(&root, stamp);
            self.mark_dirty(&root, Changed::default(), Urgency::Debounce);
        }
    }

    /// `textDocument/didClose`: the file's text comes from disk again.
    pub(crate) fn did_close(&mut self, params: Value) {
        let Some(params) = self.params::<DidCloseTextDocumentParams>("didClose", params) else {
            return;
        };
        let Some(path) = self.open_path(&params.text_document.uri) else {
            return;
        };
        let doc = self.docs.remove(&path);
        self.uris.remove(&path);
        if let Some(root) = doc.and_then(|doc| doc.root) {
            let stamp = self.next_stamp();
            self.touch(&root, stamp);
            self.mark_dirty(&root, SWEEP, Urgency::Flush);
        }
    }

    /// `textDocument/didSave`: disk may have changed, here and elsewhere.
    pub(crate) fn did_save(&mut self, params: Value) {
        let Some(params) = self.params::<DidSaveTextDocumentParams>("didSave", params) else {
            return;
        };
        let Some(path) = self.open_path(&params.text_document.uri) else {
            return;
        };
        let root = self.docs.get(&path).and_then(|doc| doc.root.clone());
        if let Some(root) = root {
            let changed = Changed {
                sweep: true,
                paths: vec![path],
            };
            self.mark_dirty(&root, changed, Urgency::Flush);
        }
    }

    /// The path of an open document, by any spelling of its URI.
    fn open_path(&self, uri: &Uri) -> Option<PathBuf> {
        let path = normalize(&crate::text::uri::uri_to_path(uri)?);
        self.docs.contains_key(&path).then_some(path)
    }

    /// A session for the package at `root`, made if it has none.
    pub(crate) fn ensure_package(&mut self, root: &Path) {
        if !self.packages.contains_key(root) {
            self.packages
                .insert(root.to_path_buf(), Package::new(root.to_path_buf()));
        }
    }

    /// The order of the next edit.
    fn next_stamp(&mut self) -> u64 {
        self.stamp = self.stamp.saturating_add(1);
        self.stamp
    }

    /// Records `stamp` as the package's newest edit.
    fn touch(&mut self, root: &Path, stamp: u64) {
        if let Some(package) = self.packages.get_mut(root) {
            package.last_edit = stamp;
        }
    }
}
