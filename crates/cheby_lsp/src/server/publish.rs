//! Publishing diagnostics (A §8.4, D-1373).
//!
//! Every file of a package is published, open or closed, `cheby.toml`
//! included, as `cheby check` reports them. An open document gets a publish
//! with its version after each run that covers that version, even an empty
//! or unchanged list, which is what a client waits for. A closed file is
//! published only when its list changed. A file that leaves the package
//! gets one empty publish, and is then forgotten. Only the package that
//! owns a path publishes it, so a nested package's list is its own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_driver::analysis::{
    FileReport, Hash128, Live, PlainDiagnostic, Report, Severity, TextOrigin,
};
use lsp_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, Location, MessageType,
    NumberOrString, Position, PublishDiagnosticsParams, Range,
};

use super::Server;
use super::effects::Effect;
use super::state::{Published, package_root};
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
                    && live.snapshot.is_some()
                {
                    package.snapshot.clone_from(&live.snapshot);
                }
                if let Some(message) = &live.failed {
                    self.run_failed(root, message);
                }
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
            self.job_done(root);
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
        self.job_done(root);
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
            if package_root(path).as_deref() != Some(root) {
                continue;
            }
            let open = self.docs.get(path).map(|doc| doc.version);
            let stale = match (open, report.version) {
                (Some(now), Some(ran)) => now > ran,
                // Opened or closed after the job started: the run read the
                // other text, and `didOpen` or `didClose` marked the
                // package for a run that reads this one.
                (Some(_), None) | (None, Some(_)) => true,
                (None, None) => false,
            };
            if stale {
                // A newer run follows and publishes this file.
                continue;
            }
            let diagnostics = self.convert(root, path, report, files);
            let version = open.and(report.version);
            self.publish(root, path, diagnostics, version, open.is_some());
        }
    }

    /// Sends one file's list: always for an open document, else only when
    /// it changed.
    fn publish(
        &mut self,
        root: &Path,
        path: &Path,
        diagnostics: Vec<Diagnostic>,
        version: Option<i32>,
        always: bool,
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
            .is_some_and(|old| old.sent == sent && old.version == version);
        if same && !always {
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
    /// (D-1373).
    fn forget_gone(&mut self, root: &Path, live: &Live) {
        let Some(package) = self.packages.get_mut(root) else {
            return;
        };
        let gone: Vec<PathBuf> = package
            .published
            .keys()
            .filter(|path| !live.files.contains(*path))
            .cloned()
            .collect();
        for path in gone {
            if let Some(package) = self.packages.get_mut(root) {
                package.published.remove(&path);
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

    /// After a job's last event: each open document of the job whose
    /// version is still current and was not published with it gets its
    /// list again with that version, so every version a client waits for
    /// is published (A §8.4). A document the job never published, such as
    /// a module while `cheby.toml` has errors, gets an empty list.
    fn catch_up(&mut self, root: &Path) {
        let Some(package) = self.packages.get(root) else {
            return;
        };
        let due: Vec<(PathBuf, i32)> = package
            .job_versions
            .iter()
            .filter(|(path, version)| {
                self.docs
                    .get(*path)
                    .is_some_and(|doc| doc.version == **version)
                    && package
                        .published
                        .get(*path)
                        .is_none_or(|p| p.version != Some(**version))
            })
            .map(|(path, version)| (path.clone(), *version))
            .collect();
        for (path, version) in due {
            let diagnostics = self
                .packages
                .get(root)
                .and_then(|package| package.published.get(&path))
                .map(|published| published.diagnostics.clone())
                .unwrap_or_default();
            self.publish(root, &path, diagnostics, Some(version), true);
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
        let mut texts: Texts<'a> = BTreeMap::new();
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
            .map(|d| self.diagnostic(root, path, d, files, &mut texts))
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
        let mut related = Vec::new();
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
                    root,
                    &label.path,
                    label_range,
                    &label.message,
                ));
            }
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
