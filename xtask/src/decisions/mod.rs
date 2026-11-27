//! `cargo xtask decisions`: commands for the decision log,
//! `docs/decisions/LOG.md`, which is too long to read whole (D-1940).
//!
//! - `show` prints entries with what they name, what names them and where
//!   the spec, the tests and the code cite them ([`show`]).
//! - `search` lists the entries that have every word.
//! - `next-id` prints the next free id over every branch, remote-tracking
//!   ref and worktree, and `next-adr` the next free ADR number (D-1941,
//!   D-1943; [`refs`]).
//! - `lint` checks the log's form, that every citation names an entry and
//!   that an ADR number has one file (D-1942, D-1943; [`lint`]). CI runs it.

mod cites;
mod lint;
mod log;
mod refs;
mod show;

use std::path::Path;

use self::log::Log;

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, clap::Subcommand)]
enum Cmd {
    /// Print decisions: the entry, what it names, what names it, and where
    /// the spec, the tests and the code cite it
    Show {
        /// An id such as `D-244`, or a range such as `D-1930..D-1939`
        #[arg(required = true, value_name = "ID")]
        ids: Vec<String>,
        /// Print every list whole, and every place as `file:line`
        #[arg(long)]
        cites: bool,
    },
    /// List the decisions whose title or text has every word, titles first
    Search {
        #[arg(required = true, value_name = "WORD")]
        words: Vec<String>,
        /// Look at titles only
        #[arg(long)]
        titles: bool,
        /// How many entries to print
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// Print the next free id, read from the working file and from the log
    /// of every branch, remote-tracking ref and worktree (D-1941)
    NextId,
    /// Print the next free ADR number, read from `docs/adr` of the working
    /// tree and of every branch, remote-tracking ref and worktree (D-1943)
    NextAdr,
    /// Check the log's form and that every `D-NNN` in the repository is an
    /// entry (D-1942)
    Lint,
}

pub(crate) fn run(root: &Path, args: &Args) -> Result<(), String> {
    match &args.command {
        Cmd::Show { ids, cites } => show::show(root, ids, *cites),
        Cmd::Search {
            words,
            titles,
            limit,
        } => show::search(root, words, *titles, *limit),
        Cmd::NextId => refs::run(root, refs::Sequence::Decisions),
        Cmd::NextAdr => refs::run(root, refs::Sequence::Adrs),
        Cmd::Lint => lint::run(root),
    }
}

/// The text of the working file's log.
fn read_text(root: &Path) -> Result<String, String> {
    let path = root.join(log::PATH);
    std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
}

fn read(root: &Path) -> Result<Log, String> {
    read_text(root).map(|text| Log::parse(&text))
}
