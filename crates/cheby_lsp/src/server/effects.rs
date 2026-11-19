//! Effects: messages the main loop sends on behalf of a step, applied in
//! order (plan §1.5). Only the main loop writes to the connection, so the
//! request queue stays consistent.

use std::path::{Path, PathBuf};

use lsp_server::{Message, Notification};
use lsp_types::MessageType;
use serde_json::{Value, json};

use super::docs::SWEEP;
use super::schedule::Urgency;
use super::{Outgoing, Server};

/// One message to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// A notification to the client.
    Notify { method: &'static str, params: Value },
    /// A request to the client whose answer is not used.
    Request { method: &'static str, params: Value },
    /// The package at `root`, a session key, reads disk again now: a
    /// request found a file on disk that its snapshot does not hold, such
    /// as a stale closed file or a new module (D-1389, D-1669), before any
    /// watcher or sweep reported it (D-1443).
    Sweep { root: PathBuf },
}

impl Effect {
    /// A `window/logMessage`.
    pub(crate) fn log(level: MessageType, message: impl Into<String>) -> Self {
        Self::Notify {
            method: "window/logMessage",
            params: json!({ "type": level, "message": message.into() }),
        }
    }

    /// A `window/showMessage`.
    pub(crate) fn show(level: MessageType, message: impl Into<String>) -> Self {
        Self::Notify {
            method: "window/showMessage",
            params: json!({ "type": level, "message": message.into() }),
        }
    }
}

impl Server {
    /// Sends `effect`. A connection that is gone ends the main loop.
    pub(crate) fn apply(&mut self, effect: Effect) {
        match effect {
            Effect::Notify { method, params } => {
                self.send(Notification::new(method.to_owned(), params).into());
            }
            Effect::Request { method, params } => {
                self.request(method, params, Outgoing::Ignore);
            }
            Effect::Sweep { root } => self.sweep_now(&root),
        }
    }

    /// Makes the package at `root` stat every file it read, walk `src/`
    /// again and run at once, as an open, close or save does, so a file
    /// whose stamp changed is read again. The constants child hears of it
    /// as of a sweep's finding, since this stands in for a watcher that
    /// has not reported yet (D-1494). An unknown root does nothing.
    fn sweep_now(&mut self, root: &Path) {
        if !self.packages.contains_key(root) {
            return;
        }
        self.mark_dirty(root, SWEEP, Urgency::Flush);
        self.child_on_save(root, &[]);
    }

    /// Sends a request to the client, remembering what its answer is for.
    pub(crate) fn request(&mut self, method: &str, params: Value, purpose: Outgoing) {
        let request = self
            .queue
            .outgoing
            .register(method.to_owned(), params, purpose);
        self.send(Message::Request(request));
    }

    /// Sends one message. When the client has stopped reading, the loop
    /// ends at its next turn, as at the end of input (A §3.7).
    pub(crate) fn send(&mut self, message: Message) {
        if self.connection.sender.send(message).is_err() {
            self.closed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use cheby_driver::analysis::{Globals, Overlay, OverlayFile, PackageKind, Snapshot};
    use crossbeam_channel::{Receiver, Sender};

    use super::SWEEP;
    use crate::analysis_thread::{self, Changed, Event, Job, Work};
    use crate::text::uri::normalize;

    const MAIN: &str = "//! Root.\n\nfn main() {\n  let _ = 1\n}\n";
    const B: &str = "//! B.\n\npub fn g() -> Int {\n  2\n}\n";
    /// Longer than [`B`], so its stamp changes whatever the clock's grain.
    const B_LATER: &str = "//! B, later.\n\npub fn g() -> Int {\n  3\n}\n";

    /// A package in a fresh temporary directory, removed on drop.
    struct Temp(PathBuf);

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl Temp {
        fn package() -> Self {
            let dir =
                std::env::temp_dir().join(format!("cheby_lsp_effects_{}", std::process::id()));
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
            std::fs::write(dir.join("src/b.cheby"), B).unwrap();
            Self(normalize(&std::path::absolute(&dir).unwrap()))
        }
    }

    /// Runs one job of the package at `root`, with `main` open, and
    /// returns its snapshot after the job's last event.
    fn run(
        jobs: &Sender<Work>,
        events: &Receiver<Event>,
        root: &Path,
        changed: Changed,
    ) -> Arc<Snapshot> {
        let main = root.join("src/main.cheby");
        let overlay = Overlay {
            files: [(
                main,
                OverlayFile {
                    text: Arc::from(MAIN),
                    version: 1,
                    stamp: 1,
                },
            )]
            .into_iter()
            .collect(),
        };
        let job = Job {
            root: root.to_path_buf(),
            kind: PackageKind::Package,
            target: None,
            overlay,
            changed,
            superseded: Arc::new(AtomicBool::new(false)),
            panic: false,
        };
        jobs.send(Work::Run(job)).unwrap();
        let Event::Live {
            result,
            strict_follows,
            ..
        } = events.recv().unwrap()
        else {
            panic!("a Q run first");
        };
        let snapshot = result.unwrap().snapshot.expect("a snapshot");
        if strict_follows {
            assert!(matches!(events.recv().unwrap(), Event::Strict { .. }));
        }
        snapshot
    }

    /// The text of `b.cheby` in `snapshot`.
    fn b_text(snapshot: &Snapshot, root: &Path) -> String {
        let file = snapshot.file_of(&root.join("src/b.cheby")).unwrap();
        snapshot.source(file).unwrap().text().to_owned()
    }

    /// What `Effect::Sweep` marks, a sweep, makes the session read a closed
    /// file that changed on disk with no watcher's report. Without it the
    /// session keeps the text it read.
    #[test]
    fn sweep_re_reads_a_changed_closed_file() {
        let temp = Temp::package();
        let root = &temp.0;
        let (jobs, events) = analysis_thread::spawn(Arc::new(Globals::new())).unwrap();
        let first = run(&jobs, &events, root, Changed::default());
        assert_eq!(b_text(&first, root), B);
        std::fs::write(root.join("src/b.cheby"), B_LATER).unwrap();
        let unswept = run(&jobs, &events, root, Changed::default());
        assert_eq!(b_text(&unswept, root), B, "nothing told the session");
        let swept = run(&jobs, &events, root, SWEEP);
        assert_eq!(b_text(&swept, root), B_LATER);
    }
}
