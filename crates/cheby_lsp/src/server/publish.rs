//! Publishing diagnostics (A §8.4, D-1373, D-1442).
//!
//! Every file of a package is published, open or closed, `cheby.toml`
//! included, as `cheby check` reports them. A publish is skipped when the
//! path's last one had the same version and the same list (D-1442), so a
//! save, a settings change or a sweep that changes nothing sends nothing.
//! `didOpen` and `didClose` forget the path's last publish, so an opened
//! document still gets a publish with its version, even an empty one, and
//! a closed file its publish without one. An open document is published
//! only from a run that read its current text, named by edit stamp, since
//! Zed restarts versions at 0 on reopen (cp-notes, CP1). A file that
//! leaves the package gets one empty publish, and is then forgotten. Only
//! the session that owns a path under `sessions::route` publishes it, so a
//! nested package's list is its own (A §6.5). Std is never published. A
//! single-file session publishes its one document by the same rules
//! (D-1371).
//!
//! A file's list is the front end's, then the constants child's, as
//! `report_back_end` appends them in `cheby check` (A §11.6). A child
//! diagnostic joins while the normalized hash of the published text is
//! the one the child read (D-1412), and its ranges, in normalized text,
//! are mapped onto the published text with `NormMap`. When the child's
//! lists change and no report comes, the latest report's files are
//! published again (`republish_child`).
//!
//! A diagnostic's `message` carries `cheby check`'s text without its
//! snippet (D-1509): the header, then one line each for the primary label
//! when it is not empty, the related labels when the client takes no
//! `relatedInformation`, each note as `note: …` and each help as
//! `help: …`. The first line is the header alone, which is what parity
//! with the UI goldens compares.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_driver::analysis::{
    FileReport, Hash128, Live, NormMap, PackageKind, PlainDiagnostic, Report, Severity, TextOrigin,
    exact_key, normalized_key,
};
use lsp_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, Location, MessageType,
    NumberOrString, Position, PublishDiagnosticsParams, Range,
};

use super::Server;
use super::effects::Effect;
use super::state::Published;
use crate::analysis_thread::RunFailure;
use crate::text::lines::{Bom, LspLines};

impl Server {
    /// The Q run of a job (A §2.3): phase-A lists now, and the whole
    /// package when the run is the check.
    pub(crate) fn on_live(
        &mut self,
        root: &Path,
        result: Result<Box<Live>, RunFailure>,
        done: bool,
    ) {
        match result {
            Ok(live) => {
                if let Some(package) = self.packages.get_mut(root)
                    && let Some(snapshot) = &live.snapshot
                {
                    let new = package
                        .snapshot
                        .as_ref()
                        .is_none_or(|old| !Arc::ptr_eq(old, snapshot));
                    if new {
                        // Counted per root, not by the snapshot's own
                        // generation, which a rebuilt session starts
                        // again (D-1495).
                        let generation = self.generations.entry(root.to_path_buf()).or_default();
                        *generation = generation.saturating_add(1);
                    }
                    package.snapshot = Some(Arc::clone(snapshot));
                }
                if let Some(message) = &live.failed {
                    self.run_failed(root, message);
                }
                self.child_on_live(root, &live);
                self.publish_files(root, &live.phase_a);
                if let Some(report) = &live.report {
                    self.publish_report(root, report);
                }
                self.forget_gone(root, &live);
            }
            Err(failure) => self.run_failed(root, failure.message()),
        }
        if done {
            self.catch_up(root);
            self.finish(root);
        }
    }

    /// The D run of a job, its last event.
    pub(crate) fn on_strict(&mut self, root: &Path, result: Result<Report, RunFailure>) {
        match result {
            Ok(report) => self.publish_report(root, &report),
            // The last published lists stand (A §3.7).
            Err(failure) => self.run_failed(root, failure.message()),
        }
        self.catch_up(root);
        self.finish(root);
    }

    /// The package at `root` had the last event of its job.
    fn finish(&mut self, root: &Path) {
        self.job_done(root);
        if let Some(package) = self.packages.get_mut(root) {
            package.job_stamps.clear();
        }
    }

    /// Publishes a report's files, and shows its tool errors once.
    fn publish_report(&mut self, root: &Path, report: &Report) {
        let files: BTreeMap<PathBuf, Arc<FileReport>> = report
            .files
            .iter()
            .filter(|(path, _)| !report.held.contains(*path))
            .map(|(path, file)| (path.clone(), Arc::clone(file)))
            .collect();
        self.publish_files(root, &files);
        self.child_on_report(root, report);
        let Some(package) = self.packages.get_mut(root) else {
            return;
        };
        if package.shown_tool != report.tool {
            package.shown_tool.clone_from(&report.tool);
            for message in report.tool.clone() {
                self.apply(Effect::show(MessageType::ERROR, message));
            }
        }
    }

    /// Publishes each of `files` under the rules of the module doc.
    fn publish_files(&mut self, root: &Path, files: &BTreeMap<PathBuf, Arc<FileReport>>) {
        for (path, report) in files {
            if !self.owns(root, path) {
                continue;
            }
            let version = match self.current(root, path) {
                Current::Open(version) => Some(version),
                Current::Closed => None,
                // A newer run follows and publishes this file.
                Current::Stale => continue,
            };
            let diagnostics = self.convert(root, path, report, files);
            self.publish(root, path, diagnostics, version);
        }
    }

    /// Publishes `files`, the latest report's, again after the constants
    /// child's lists changed and no report came. An open document is
    /// published only while the client holds the report's text, with its
    /// current version; otherwise a newer run publishes it.
    pub(crate) fn republish_child(
        &mut self,
        root: &Path,
        files: &BTreeMap<PathBuf, Arc<FileReport>>,
    ) {
        for (path, report) in files {
            if !self.owns(root, path) {
                continue;
            }
            let version = match self.docs.get(path) {
                Some(doc) if exact_key(&doc.text) == report.hash => Some(doc.version),
                Some(_) => continue,
                None => None,
            };
            let diagnostics = self.convert(root, path, report, files);
            self.publish(root, path, diagnostics, version);
        }
    }

    /// How the job in flight of `root` read `path`, against the text the
    /// client holds now. Texts are compared by edit stamp: a document
    /// closed and opened again starts at version 0, which an older run may
    /// have read too.
    fn current(&self, root: &Path, path: &Path) -> Current {
        let Some(package) = self.packages.get(root) else {
            return Current::Stale;
        };
        match (self.docs.get(path), package.job_stamps.get(path)) {
            (Some(doc), Some(&stamp)) if doc.stamp == stamp => Current::Open(
                package
                    .job_versions
                    .get(path)
                    .copied()
                    .unwrap_or(doc.version),
            ),
            (Some(_), _) | (None, Some(_)) => Current::Stale,
            (None, None) => Current::Closed,
        }
    }

    /// Sends one file's list, unless it equals the path's last publish in
    /// version and diagnostics and `didOpen` or `didClose` did not forget
    /// that one (D-1442).
    fn publish(
        &mut self,
        root: &Path,
        path: &Path,
        diagnostics: Vec<Diagnostic>,
        version: Option<i32>,
    ) {
        let sent = Hash128::of(
            serde_json::to_string(&diagnostics)
                .unwrap_or_default()
                .as_bytes(),
        );
        let Some(package) = self.packages.get_mut(root) else {
            return;
        };
        let same = package
            .published
            .get(path)
            .is_some_and(|old| !old.forgotten && old.sent == sent && old.version == version);
        if same {
            return;
        }
        let Ok(uri) = self.uris.uri_of(path) else {
            return;
        };
        package.published.insert(
            path.to_path_buf(),
            Published {
                version,
                sent,
                diagnostics: diagnostics.clone(),
                forgotten: false,
            },
        );
        let params = PublishDiagnosticsParams {
            uri,
            diagnostics,
            version,
        };
        self.apply(Effect::Notify {
            method: "textDocument/publishDiagnostics",
            params: serde_json::to_value(params).unwrap_or_default(),
        });
    }

    /// One empty publish for each published file that left the package
    /// (D-1373), or that the package no longer owns, such as a file of a
    /// nested package whose `cheby.toml` appeared (A §6.5). A file that a
    /// live session owns now gets nothing here, since its owner publishes
    /// it, maybe already.
    fn forget_gone(&mut self, root: &Path, live: &Live) {
        let Some(package) = self.packages.get(root) else {
            return;
        };
        let gone: Vec<PathBuf> = package
            .published
            .keys()
            .filter(|path| !live.files.contains(*path) || !self.owns(root, path))
            .cloned()
            .collect();
        for path in gone {
            if let Some(package) = self.packages.get_mut(root) {
                package.published.remove(&path);
            }
            let elsewhere = self
                .route_of(&path)
                .root()
                .is_some_and(|owner| owner != root && self.packages.contains_key(owner));
            if elsewhere {
                continue;
            }
            let Ok(uri) = self.uris.uri_of(&path) else {
                continue;
            };
            let version = self.docs.get(&path).map(|doc| doc.version);
            let params = PublishDiagnosticsParams {
                uri,
                diagnostics: Vec::new(),
                version,
            };
            self.apply(Effect::Notify {
                method: "textDocument/publishDiagnostics",
                params: serde_json::to_value(params).unwrap_or_default(),
            });
        }
    }

    /// After a job's last event: each open document of the job whose text
    /// is still the one the job read, and that was not published with its
    /// version, gets its list again with that version, so every version a
    /// client waits for is published (A §8.4). A document the job never
    /// published, such as a module while `cheby.toml` has errors, gets an
    /// empty list.
    fn catch_up(&mut self, root: &Path) {
        let Some(package) = self.packages.get(root) else {
            return;
        };
        let due: Vec<(PathBuf, i32)> = package
            .job_stamps
            .iter()
            .filter_map(|(path, stamp)| {
                let doc = self.docs.get(path).filter(|doc| doc.stamp == *stamp)?;
                let published = package
                    .published
                    .get(path)
                    .is_some_and(|p| !p.forgotten && p.version == Some(doc.version));
                (!published && self.owns(root, path)).then(|| (path.clone(), doc.version))
            })
            .collect();
        for (path, version) in due {
            let diagnostics = self
                .packages
                .get(root)
                .and_then(|package| package.published.get(&path))
                .map(|published| published.diagnostics.clone())
                .unwrap_or_default();
            self.publish(root, &path, diagnostics, Some(version));
        }
    }

    /// A run that failed: always logged, shown once per package (A §3.7).
    fn run_failed(&mut self, root: &Path, message: &str) {
        self.log(
            MessageType::ERROR,
            format!("analysis of {} failed: {message}", root.display()),
        );
        let Some(package) = self.packages.get_mut(root) else {
            return;
        };
        if !package.shown_failure {
            package.shown_failure = true;
            self.apply(Effect::show(
                MessageType::ERROR,
                format!("cheby: analysis failed: {message}"),
            ));
        }
    }

    /// `report`'s diagnostics in protocol form. A related label is placed
    /// in the text of its file from `files`, else from an open document,
    /// and dropped when neither is known. Each file's line index is built
    /// once per call, however many labels point into it.
    fn convert<'a>(
        &'a self,
        root: &Path,
        path: &'a Path,
        report: &'a FileReport,
        files: &'a BTreeMap<PathBuf, Arc<FileReport>>,
    ) -> Vec<Diagnostic> {
        let child = self.child_list(root, path, report, files);
        let mut texts: Texts<'_> = BTreeMap::new();
        texts.insert(
            path,
            Some((
                &*report.text,
                LspLines::new(&report.text, bom(report.origin)),
            )),
        );
        report
            .diagnostics
            .iter()
            .chain(&child)
            .map(|d| self.diagnostic(root, path, d, files, &mut texts))
            .collect()
    }

    /// The constants child's diagnostics of `path`, while `report`'s text
    /// is the one the child read (D-1412), with their ranges mapped from
    /// normalized text onto the texts they are published against. A
    /// related label whose file's text is not known, or whose range no
    /// longer fits it, is dropped.
    fn child_list(
        &self,
        root: &Path,
        path: &Path,
        report: &FileReport,
        files: &BTreeMap<PathBuf, Arc<FileReport>>,
    ) -> Vec<PlainDiagnostic> {
        let list = self
            .children
            .diagnostics(root, path, normalized_key(&report.text));
        if list.is_empty() {
            return Vec::new();
        }
        let own = NormMap::new(&report.text);
        let mut maps: BTreeMap<PathBuf, Option<NormMap>> = BTreeMap::new();
        list.iter()
            .map(|d| {
                let mut d = d.clone();
                d.range = d.range.and_then(|range| own.to_live_range(range));
                d.related.retain_mut(|label| {
                    let map = if label.path == path {
                        Some(&own)
                    } else {
                        maps.entry(label.path.clone())
                            .or_insert_with(|| {
                                files
                                    .get(&label.path)
                                    .map(|file| &*file.text)
                                    .or_else(|| {
                                        self.docs.get(&label.path).map(|doc| doc.text.as_str())
                                    })
                                    .map(NormMap::new)
                            })
                            .as_ref()
                    };
                    match map.and_then(|map| map.to_live_range(label.range)) {
                        Some(range) => {
                            label.range = range;
                            true
                        }
                        None => false,
                    }
                });
                d
            })
            .collect()
    }

    /// The text of `path` and its line index, from `files`, else from an
    /// open document, made once and kept in `texts`.
    fn placed<'a, 't>(
        &'a self,
        path: &'a Path,
        files: &'a BTreeMap<PathBuf, Arc<FileReport>>,
        texts: &'t mut Texts<'a>,
    ) -> Option<&'t (&'a str, LspLines)> {
        texts
            .entry(path)
            .or_insert_with(|| {
                files
                    .get(path)
                    .map(|file| (&*file.text, bom(file.origin)))
                    .or_else(|| {
                        self.docs
                            .get(path)
                            .map(|doc| (doc.text.as_str(), Bom::Counted))
                    })
                    .map(|(text, counted)| (text, LspLines::new(text, counted)))
            })
            .as_ref()
    }

    /// One diagnostic of the file at `path` in protocol form.
    fn diagnostic<'a>(
        &'a self,
        root: &Path,
        path: &'a Path,
        d: &'a PlainDiagnostic,
        files: &'a BTreeMap<PathBuf, Arc<FileReport>>,
        texts: &mut Texts<'a>,
    ) -> Diagnostic {
        let enc = self.encoding;
        let range = match (d.range, self.placed(path, files, texts)) {
            (Some(range), Some((text, lines))) => {
                lines.range(text, lines.widen_empty(text, range), enc)
            }
            _ => Range::new(Position::new(0, 0), Position::new(0, 0)),
        };
        let mut message = d.message.clone();
        if !d.primary_message.is_empty() {
            message.push('\n');
            message.push_str(&d.primary_message);
        }
        let mut related = Vec::new();
        // A single-file session is keyed by its file: paths show relative
        // to the file's directory.
        let shown_root = match self.packages.get(root).map(|package| &package.kind) {
            Some(PackageKind::SingleFile { .. }) => root.parent().unwrap_or(root),
            Some(PackageKind::Package) | None => root,
        };
        for label in &d.related {
            let Some((other, other_lines)) = self.placed(&label.path, files, texts) else {
                continue;
            };
            let label_range = other_lines.range(other, label.range, enc);
            if self.caps.related_information {
                let Ok(uri) = self.uris.uri_of(&label.path) else {
                    continue;
                };
                related.push(DiagnosticRelatedInformation {
                    location: Location::new(uri, label_range),
                    message: label.message.clone(),
                });
            } else {
                message.push_str(&related_line(
                    shown_root,
                    &label.path,
                    label_range,
                    &label.message,
                ));
            }
        }
        for note in &d.notes {
            message.push_str(&child_line("note", note));
        }
        for help in &d.helps {
            message.push_str(&child_line("help", &help.message));
        }
        Diagnostic {
            range,
            severity: Some(match d.severity {
                Severity::Error => DiagnosticSeverity::ERROR,
                Severity::Warning => DiagnosticSeverity::WARNING,
            }),
            code: Some(NumberOrString::String(d.code.to_string())),
            code_description: None,
            source: Some("cheby".to_owned()),
            message,
            related_information: (!related.is_empty()).then_some(related),
            tags: None,
            data: None,
        }
    }
}

/// How a job read a file, against the text the client holds now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Current {
    /// An open document whose current text the job read, at this version.
    Open(i32),
    /// A file closed all along, read from disk.
    Closed,
    /// The job read other text: an edit, an open or a close came since it
    /// started, and each of them marked the package for a run that reads
    /// the new text.
    Stale,
}

/// The texts of one [`Server::convert`] call and their line indexes, by
/// path; `None` for a path whose text is not known.
type Texts<'a> = BTreeMap<&'a Path, Option<(&'a str, LspLines)>>;

/// How a report's text counts a BOM: as the client sent it, or as disk
/// holds it.
const fn bom(origin: TextOrigin) -> Bom {
    match origin {
        TextOrigin::Editor => Bom::Counted,
        TextOrigin::Disk => Bom::Hidden,
    }
}

/// A related label joined to the message, for a client that has no
/// `relatedInformation`: `\npath:line:column: message`, the path relative
/// to the package root, line and column from 1 in the client's units.
fn related_line(root: &Path, path: &Path, range: Range, message: &str) -> String {
    let shown = path.strip_prefix(root).unwrap_or(path);
    let line = range.start.line.saturating_add(1);
    let column = range.start.character.saturating_add(1);
    if message.is_empty() {
        format!("\n{}:{line}:{column}", shown.display())
    } else {
        format!("\n{}:{line}:{column}: {message}", shown.display())
    }
}

/// A note or a help joined to the message as `cheby check` prints it after
/// its `= ` (D-1509): `\nnote: text`, and each later line of the text
/// indented under the first, with trailing spaces removed.
fn child_line(kind: &str, text: &str) -> String {
    let mut lines = text.split('\n');
    let first = lines.next().unwrap_or_default();
    let mut out = format!("\n{kind}: {first}");
    let indent = " ".repeat(kind.len().saturating_add(2));
    for line in lines {
        out.push('\n');
        out.push_str(format!("{indent}{line}").trim_end());
    }
    out
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod tests;
