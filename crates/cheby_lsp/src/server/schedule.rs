//! Debounce and jobs (A §3.2).
//!
//! An edit starts a package's debounce. Steady typing moves the deadline,
//! but never past five debounces after the first edit, so a long burst
//! still refreshes. An open, close or save flushes the debounce. When the
//! analysis thread is free, the due package holding the document edited
//! last gets the next job. One job runs at a time, so a package's jobs
//! never overlap, and an edit during a job runs again after it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use cheby_driver::analysis::{Overlay, OverlayFile};

use super::Server;
use crate::analysis_thread::{Changed, Job};

/// Where a package is in its runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunState {
    /// Nothing to do.
    Idle,
    /// A run is due at `deadline`.
    Debouncing {
        deadline: Instant,
        /// The first change of this burst, which caps the deadline.
        first_dirty: Instant,
    },
    /// A job is on the analysis thread.
    Running {
        /// A change came during the job, so another run follows it.
        dirty_again: bool,
    },
}

/// How a change wants its run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Urgency {
    /// After the debounce: an edit.
    Debounce,
    /// At once: open, close, save, settings.
    Flush,
}

impl Server {
    /// Marks the package at `root` as changed, with what changed on disk.
    pub(crate) fn mark_dirty(&mut self, root: &Path, changed: Changed, urgency: Urgency) {
        let debounce = self.settings.debounce();
        let Some(package) = self.packages.get_mut(root) else {
            return;
        };
        package.changed.merge(changed);
        let now = Instant::now();
        package.run = match package.run {
            RunState::Idle => RunState::Debouncing {
                deadline: match urgency {
                    Urgency::Flush => now,
                    Urgency::Debounce => now.checked_add(debounce).unwrap_or(now),
                },
                first_dirty: now,
            },
            RunState::Debouncing { first_dirty, .. } => {
                let deadline = match urgency {
                    Urgency::Flush => now,
                    Urgency::Debounce => {
                        let next = now.checked_add(debounce).unwrap_or(now);
                        let cap = debounce
                            .checked_mul(5)
                            .and_then(|cap| first_dirty.checked_add(cap))
                            .unwrap_or(next);
                        next.min(cap)
                    }
                };
                RunState::Debouncing {
                    deadline,
                    first_dirty,
                }
            }
            RunState::Running { .. } => {
                // The job's D run is wasted work now: a newer job follows.
                package.superseded.store(true, Ordering::Relaxed);
                RunState::Running { dirty_again: true }
            }
        };
    }

    /// Marks every package as changed, to run at once.
    pub(crate) fn mark_all_dirty(&mut self) {
        let roots: Vec<PathBuf> = self.packages.keys().cloned().collect();
        for root in roots {
            self.mark_dirty(&root, Changed::default(), Urgency::Flush);
        }
    }

    /// The next debounce deadline, while the analysis thread is free.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        if self.running.is_some() {
            return None;
        }
        self.packages
            .values()
            .filter_map(|package| match package.run {
                RunState::Debouncing { deadline, .. } => Some(deadline),
                RunState::Idle | RunState::Running { .. } => None,
            })
            .min()
    }

    /// Starts a job if the analysis thread is free and a package is due.
    pub(crate) fn schedule(&mut self) {
        if self.running.is_some() || self.shutting_down {
            return;
        }
        let now = Instant::now();
        let due = self
            .packages
            .values()
            .filter(|package| {
                matches!(package.run, RunState::Debouncing { deadline, .. } if deadline <= now)
            })
            .max_by_key(|package| package.last_edit)
            .map(|package| package.root.clone());
        let Some(root) = due else {
            return;
        };
        let overlay = self.overlay(&root);
        let target = self.settings.analysis.target;
        let Some(package) = self.packages.get_mut(&root) else {
            return;
        };
        let superseded = Arc::new(AtomicBool::new(false));
        package.superseded = Arc::clone(&superseded);
        package.job_versions = overlay
            .files
            .iter()
            .map(|(path, file)| (path.clone(), file.version))
            .collect();
        package.run = RunState::Running { dirty_again: false };
        let job = Job {
            root: root.clone(),
            target,
            overlay,
            changed: std::mem::take(&mut package.changed),
            superseded,
        };
        if self.jobs.send(job).is_err() {
            // The analysis thread is gone; nothing will run again.
            package.run = RunState::Idle;
            self.log_error("the analysis thread has stopped; diagnostics will not refresh");
            return;
        }
        self.running = Some(root);
    }

    /// The package at `root` had the last event of its job.
    pub(crate) fn job_done(&mut self, root: &Path) {
        if self.running.as_deref() == Some(root) {
            self.running = None;
        }
        let Some(package) = self.packages.get_mut(root) else {
            return;
        };
        package.job_versions.clear();
        package.run = match package.run {
            RunState::Running { dirty_again: true } => {
                let now = Instant::now();
                RunState::Debouncing {
                    deadline: now,
                    first_dirty: now,
                }
            }
            RunState::Running { dirty_again: false } | RunState::Idle => RunState::Idle,
            debouncing @ RunState::Debouncing { .. } => debouncing,
        };
    }

    /// The open documents of the package at `root`, as the job's overlay.
    fn overlay(&mut self, root: &Path) -> Overlay {
        let files: BTreeMap<PathBuf, OverlayFile> = self
            .docs
            .iter_mut()
            .filter(|(_, doc)| doc.root.as_deref() == Some(root))
            .map(|(path, doc)| {
                let file = OverlayFile {
                    text: doc.shared(),
                    version: doc.version,
                    stamp: doc.stamp,
                };
                (path.clone(), file)
            })
            .collect();
        Overlay { files }
    }
}
