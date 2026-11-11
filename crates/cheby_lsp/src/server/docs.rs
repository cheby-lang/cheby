//! `didOpen`, `didChange`, `didClose` and `didSave` (A §3.2, §5).
//!
//! The server keeps each open document's text, edited in place by
//! incremental changes, and routes the document by its own path
//! (`sessions`, A §6.4). A package gets a session at its first open
//! document. An open, close or save also sweeps the package's files on
//! disk (A §8.3, D-1374), so a file deleted or changed outside the editor
//! shows at the next of them even when no watcher reports it.
//!
//! Zed restarts a document's version at 0 after every close and open, and
//! reports a rename of an open file as `didClose` of the old URI then
//! `didOpen` of the new one (cp-notes, CP1). Neither needs anything here:
//! a text is named by its edit stamp, which rises at every open and edit
//! and never restarts, not by its version (D-1445, `schedule`), and the
//! old path leaves the package at the close's sweep, which publishes its
//! one empty list (D-1373). `didOpen` and `didClose` forget the path's
//! last publish, so the next one is sent even when unchanged (D-1442).
//!
//! A document outside every package, or inside one without being one of
//! its modules, gets a single-file session of its own at its open, which
//! ends at its close with one empty publish (D-1371, `sessions`).

use std::path::PathBuf;

use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, MessageType, Uri,
};
use serde_json::Value;

use super::Server;
use super::schedule::Urgency;
use super::sessions::DocRoute;
use super::state::Doc;
use crate::analysis_thread::Changed;
use crate::text::lines::apply_change;
use crate::text::uri::normalize;

/// A sweep of disk, for open, close and save.
pub(crate) const SWEEP: Changed = Changed {
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
        let route = self.route_of(&path);
        let root = route.root().map(std::path::Path::to_path_buf);
        self.ensure_route(&route);
        self.forget_publish(&path);
        // The text's identity for requests and publishes (D-1445).
        let stamp = self.next_stamp();
        self.docs.insert(
            path,
            Doc {
                version: item.version,
                text: item.text,
                shared: None,
                stamp,
                root: root.clone(),
                route,
                diverged: false,
                dirty: false,
            },
        );
        if let Some(root) = root {
            self.touch(&root, stamp);
            self.mark_dirty(&root, SWEEP, Urgency::Flush);
            self.settle(&root);
        }
        // With no watcher, an open may be the first news of a `cheby.toml`
        // that came or went above another open document.
        self.reroute();
    }

    /// `textDocument/didChange`.
    pub(crate) fn did_change(&mut self, params: Value) {
        let Some(params) = self.params::<DidChangeTextDocumentParams>("didChange", params) else {
            return;
        };
        let Some(path) = self.open_path(&params.text_document.uri) else {
            return;
        };
        // A new text: a request names it by this stamp (D-1445).
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
        doc.dirty = true;
        // An edited std file no longer matches std's snapshot, so its
        // snapshot requests get their fallback from now on (A §6.3).
        let diverged = doc.route == DocRoute::Std && !doc.diverged;
        doc.diverged |= doc.route == DocRoute::Std;
        let root = doc.root.clone();
        if diverged {
            self.log(
                MessageType::INFO,
                format!(
                    "{} is a std source; edits to it are not analyzed",
                    path.display()
                ),
            );
        }
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
        self.forget_publish(&path);
        match doc.map(|doc| (doc.root, doc.route)) {
            // Nothing refreshes a closed file outside every package, so its
            // session ends now, with one empty publish (D-1371, D-1373).
            Some((Some(root), DocRoute::SingleFile { .. })) => self.end_session(&root),
            Some((Some(root), _)) => {
                let stamp = self.next_stamp();
                self.touch(&root, stamp);
                self.mark_dirty(&root, SWEEP, Urgency::Flush);
                self.settle(&root);
            }
            Some((None, _)) | None => {}
        }
        self.uris.remove(&path);
        self.reroute();
    }

    /// `textDocument/didSave`: disk may have changed, here and elsewhere.
    pub(crate) fn did_save(&mut self, params: Value) {
        let Some(params) = self.params::<DidSaveTextDocumentParams>("didSave", params) else {
            return;
        };
        let Some(path) = self.open_path(&params.text_document.uri) else {
            return;
        };
        let root = self.docs.get_mut(&path).and_then(|doc| {
            doc.dirty = false;
            doc.root.clone()
        });
        if let Some(root) = root {
            let changed = Changed {
                sweep: true,
                paths: vec![path.clone()],
            };
            self.mark_dirty(&root, changed, Urgency::Flush);
            // The constants child reads disk: a save that changed it kills
            // the child and makes a run due (D-1411, D-1494).
            self.child_on_save(&root, &[path]);
        }
        // A document outside every package rejoins one whose `cheby.toml`
        // appeared, even when no watcher reported it (A §8.2, §8.3).
        self.reroute();
    }

    /// The path of an open document, by any spelling of its URI.
    fn open_path(&self, uri: &Uri) -> Option<PathBuf> {
        let path = normalize(&crate::text::uri::uri_to_path(uri)?);
        self.docs.contains_key(&path).then_some(path)
    }
}
