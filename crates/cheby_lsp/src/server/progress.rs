//! What the user and the harness can see of jobs (D-1415, A §12.4).
//!
//! - `$/progress`: a child run shows as work-done progress when the client
//!   has `window.workDoneProgress`. The server creates each token with
//!   `window/workDoneProgress/create`, then sends `begin` and `end`.
//! - One `window/showMessage` per session for a child run cut short by
//!   its time limit or its stdout cap, so constants never go missing with
//!   no sign (D-1415). The detail goes to `window/logMessage`. `child`
//!   sends both.
//! - `cheby/idle`, only for a client with `experimental.chebyIdle`: sent
//!   for a package when nothing is running or due for it, the analysis
//!   job, parked requests and its children included. It is the harness's
//!   barrier after a save (D-1443), with [`IdleParams`]. A package owes
//!   one once the loop saw it busy at the end of a turn, or once a save
//!   reached it, and pays it at the end of the first turn that finds it
//!   idle. So a save that starts nothing still gets its `cheby/idle`, and
//!   no sleep is needed to wait for one.
//! - `cheby/status`, a request answered on the main loop, for debugging
//!   and the harness: per package, its kind, newest generation (the
//!   count of `cheby/idle`, D-1495), last run
//!   state, parked requests and each child job's state and last outcome,
//!   and why constants are not evaluated, such as on Windows (A §9, §11.7,
//!   D-1414).
//!
//! The `Server` methods that send these are in `child_loop`.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::json;

use super::child::Children;
use super::effects::Effect;

/// One `$/progress` token, `cheby/child/<n>`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ProgressToken(pub String);

/// The tokens the server has made.
#[derive(Debug, Default)]
pub(crate) struct Progress {
    /// The number of the next token.
    pub next: u64,
}

impl Progress {
    /// A new token and the effects that create it and begin it, titled
    /// `title`, such as "Evaluating constants". No token and no effect
    /// when the client does not take progress.
    pub(crate) fn begin(
        &mut self,
        enabled: bool,
        title: &str,
    ) -> (Option<ProgressToken>, Vec<Effect>) {
        if !enabled {
            return (None, Vec::new());
        }
        let token = ProgressToken(format!("cheby/child/{}", self.next));
        self.next = self.next.saturating_add(1);
        let effects = vec![
            Effect::Request {
                method: "window/workDoneProgress/create",
                params: json!({ "token": token.0 }),
            },
            Effect::Notify {
                method: "$/progress",
                params: json!({
                    "token": token.0,
                    "value": { "kind": "begin", "title": title, "cancellable": false },
                }),
            },
        ];
        (Some(token), effects)
    }

    /// The effect that ends `token`, with a closing message such as "cut
    /// short after 60 s".
    #[expect(
        clippy::unused_self,
        clippy::needless_pass_by_value,
        reason = "a token is ended once, through the `Progress` that made it"
    )]
    pub(crate) fn end(&mut self, token: ProgressToken, message: &str) -> Vec<Effect> {
        vec![Effect::Notify {
            method: "$/progress",
            params: json!({
                "token": token.0,
                "value": { "kind": "end", "message": message },
            }),
        }]
    }
}

/// The parameters of `cheby/idle` (A §12.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct IdleParams {
    /// The package root, or a single-file session's file, as a `file:`
    /// URI.
    pub root: String,
    /// The root's generation: a count per root of its snapshots, which
    /// grows with each new one and is kept for the server's life, across
    /// rebuilt and restarted sessions (D-1492, D-1495, D-1499). 0 means
    /// only that the root has had no snapshot since the server started.
    pub generation: u64,
}

impl Children {
    /// One turn's end for `cheby/idle`: `packages` holds each package's
    /// root and whether something other than a child is running or due for
    /// it. A busy package comes to owe a notice, and each package that
    /// owes one and is idle now is returned, in root order, and owes none.
    pub(crate) fn idle_turn(
        &mut self,
        packages: impl IntoIterator<Item = (PathBuf, bool)>,
    ) -> Vec<PathBuf> {
        let mut idle = Vec::new();
        let mut live = std::collections::BTreeSet::new();
        for (root, busy_elsewhere) in packages {
            if busy_elsewhere || self.busy(&root) {
                self.idle_owed.insert(root.clone());
            } else if self.idle_owed.remove(&root) {
                idle.push(root.clone());
            }
            live.insert(root);
        }
        // A session that ended owes nothing.
        self.idle_owed.retain(|root| live.contains(root));
        idle.sort();
        idle
    }

    /// `root` owes a `cheby/idle`, such as after a save that may start
    /// nothing (D-1443).
    pub(crate) fn owe_idle(&mut self, root: &Path) {
        self.idle_owed.insert(root.to_path_buf());
    }
}
