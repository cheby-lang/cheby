//! `cargo xtask decisions lint`: the rules of the log's form and of the
//! citations of decisions in the repository (D-1942).

use std::collections::BTreeMap;
use std::path::Path;

use cheby_test_support::decisions;

use super::cites::{self, Cite};
use super::log::{self, Entry, Log, leading_ids};
use super::refs;

/// The first id whose entry needs a `Why:` and whose session needs an
/// `Open:` list with settled items in the documented form (D-1942). Older
/// entries are exempt, because the log is append-only.
pub(crate) const STRICT_FROM: u32 = 1940;

/// The decisions of branches that are not merged yet, which files of this
/// branch may cite: the id, the branch that has the entry, and who cites it.
/// A row whose id is an entry of the log, or that no file cites, fails the
/// lint, so the list empties as the branches merge (D-1942).
pub(crate) const ON_OTHER_BRANCHES: &[(u32, &str, &str)] = &[(
    1847,
    "property",
    "D-1920 keeps example number 035 free for the branch's example",
)];

/// The line of [`ON_OTHER_BRANCHES`], for a problem with one of its rows.
const ON_OTHER_BRANCHES_LINE: u32 = line!() - 7;

/// One broken rule, at a line of a file.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Problem {
    pub(crate) file: String,
    pub(crate) line: usize,
    pub(crate) message: String,
}

pub(crate) fn run(root: &Path) -> Result<(), String> {
    let log = super::read(root)?;
    let cites = cites::all(root)?;
    let adr = root.join("docs/adr");
    let mut problems = in_log(&log, &|name| adr.join(name).is_file());
    problems.extend(in_cites(&log, &cites, ON_OTHER_BRANCHES));
    problems.extend(in_adrs(&refs::adr_files(&adr)?));
    problems.sort();
    for problem in &problems {
        println!("{}:{}: {}", problem.file, problem.line, problem.message);
    }
    if problems.is_empty() {
        println!(
            "decisions lint: {} entries in {} sessions and {} citations, no problems",
            log.entries.len(),
            log.sessions.len(),
            cites.len(),
        );
        Ok(())
    } else {
        Err(format!(
            "the decision log breaks its rules in {} {} (D-1942)",
            problems.len(),
            if problems.len() == 1 {
                "place"
            } else {
                "places"
            }
        ))
    }
}

/// The rules of the log's own form. `adr_exists` says whether `docs/adr`
/// has a file of that name.
pub(crate) fn in_log(log: &Log, adr_exists: &dyn Fn(&str) -> bool) -> Vec<Problem> {
    let mut found = Vec::new();
    for &(line, why) in &log.malformed {
        found.push((
            line,
            format!("an entry is `- **D-NNN Title**: text`: {why}"),
        ));
    }
    for session in &log.sessions {
        if !heading_is_dated(&session.heading) {
            found.push((
                session.line,
                "a session heading is `## YYYY-MM-DD: topic`".to_owned(),
            ));
        }
        if !session.goal {
            found.push((session.line, "the session has no `Goal:` line".to_owned()));
        }
    }
    entries(log, adr_exists, &mut found);
    newer_rules(log, &mut found);
    found
        .into_iter()
        .map(|(line, message)| Problem {
            file: log::PATH.to_owned(),
            line,
            message,
        })
        .collect()
}

/// The rules of each entry: how its id is written, that it is used once
/// and rises within the session, where the entry stands, and its
/// supersession and ADR links.
fn entries(log: &Log, adr_exists: &dyn Fn(&str) -> bool, found: &mut Vec<(usize, String)>) {
    let by_id = log.by_id();
    let mut first_line: BTreeMap<u32, usize> = BTreeMap::new();
    // The session and the id of the entry before.
    let mut last: Option<(Option<usize>, u32)> = None;
    for entry in &log.entries {
        let name = decisions::format(entry.id);
        let mut problem = |message: String| found.push((entry.line, message));
        if decisions::parse(&format!("D-{}", entry.digits)).is_err() {
            problem(format!(
                "`D-{}` is written `{name}`: three digits below 1000 and no leading \
                 zero from 1000 on",
                entry.digits
            ));
        }
        match first_line.get(&entry.id) {
            Some(line) => problem(format!("{name} is already the entry of line {line}")),
            None => {
                first_line.insert(entry.id, entry.line);
            }
        }
        if entry.session.is_none() {
            problem(format!("{name} stands above the first session"));
        }
        if entry.below_open {
            problem(format!("{name} stands below its session's `Open:` list"));
        }
        if let Some((session, previous)) = last
            && session == entry.session
            && previous >= entry.id
        {
            problem(format!(
                "{name} follows {} in one session: ids rise within a session",
                decisions::format(previous)
            ));
        }
        last = Some((entry.session, entry.id));
        supersession(entry, &by_id)
            .into_iter()
            .chain(adr_links(entry, adr_exists))
            .for_each(&mut problem);
        if entry.id >= STRICT_FROM && !entry.text.contains("Why:") {
            problem(format!("{name} has no `Why:`"));
        }
    }
}

/// The rules of the sessions that hold an entry from [`STRICT_FROM`] on:
/// the session has an `Open:` list, and a struck item of it says how it
/// was settled.
fn newer_rules(log: &Log, found: &mut Vec<(usize, String)>) {
    let newer = |session: Option<usize>| {
        session.is_some()
            && log
                .entries
                .iter()
                .any(|entry| entry.session == session && entry.id >= STRICT_FROM)
    };
    for (index, session) in log.sessions.iter().enumerate() {
        if session.open.is_none() && newer(Some(index)) {
            found.push((session.line, "the session has no `Open:` list".to_owned()));
        }
    }
    for item in &log.open_items {
        let Some(after) = &item.after_strike else {
            continue;
        };
        let after = after.trim_start();
        let settled = after
            .strip_prefix("→ ")
            .is_some_and(|rest| !leading_ids(rest).is_empty())
            || after.starts_with("Done:");
        if !settled && newer(item.session) {
            found.push((
                item.line,
                "a struck item of `Open:` is followed by `→ D-NNN` or `Done: …`".to_owned(),
            ));
        }
    }
}

/// Every citation names an entry of the log, or a decision that
/// `elsewhere` lists for another branch; and every row of `elsewhere` is
/// still needed.
pub(crate) fn in_cites(log: &Log, cites: &[Cite], elsewhere: &[(u32, &str, &str)]) -> Vec<Problem> {
    let by_id = log.by_id();
    let listed = |id: u32| elsewhere.iter().any(|&(other, _, _)| other == id);
    let mut out: Vec<Problem> = cites
        .iter()
        .filter(|cite| !by_id.contains_key(&cite.id) && !listed(cite.id))
        .map(|cite| Problem {
            file: cite.file.clone(),
            line: cite.line,
            message: format!(
                "{} is not an entry of the decision log",
                decisions::format(cite.id)
            ),
        })
        .collect();
    for &(id, branch, _) in elsewhere {
        let name = decisions::format(id);
        let why = if by_id.contains_key(&id) {
            format!("{name} is an entry of the log now")
        } else if cites.iter().any(|cite| cite.id == id) {
            continue;
        } else {
            format!("no file cites {name} any more")
        };
        out.push(Problem {
            file: file!().replace('\\', "/"),
            line: usize::try_from(ON_OTHER_BRANCHES_LINE).unwrap_or(0),
            message: format!("{why}: remove its row of `{branch}` from `ON_OTHER_BRANCHES`"),
        });
    }
    out
}

/// No two files of `docs/adr` have one number (D-1943). `files` is sorted.
pub(crate) fn in_adrs(files: &[String]) -> Vec<Problem> {
    let mut first: BTreeMap<u32, &str> = BTreeMap::new();
    let mut out = Vec::new();
    for file in files {
        let Some(number) = refs::adr_number(file) else {
            continue;
        };
        match first.get(&number) {
            Some(other) => out.push(Problem {
                file: format!("{}/{file}", refs::ADRS),
                line: 1,
                message: format!("`{other}` has the ADR number {number:04} already"),
            }),
            None => {
                first.insert(number, file);
            }
        }
    }
    out
}

/// A struck entry ends with `Superseded by D-NNN.` and that entry says
/// `Supersedes` with its id in the same sentence; the entries that directly
/// follow the word `Supersedes` are struck.
fn supersession(entry: &Entry, by_id: &BTreeMap<u32, &Entry>) -> Vec<String> {
    let mut out = Vec::new();
    let name = decisions::format(entry.id);
    if entry.struck() {
        match entry.superseded_by() {
            None => out.push(format!(
                "{name} is struck, so `Superseded by D-NNN.` follows its closing `~~`"
            )),
            Some(by) => {
                let by_name = decisions::format(by);
                match by_id.get(&by) {
                    None => out.push(format!(
                        "{name} is superseded by {by_name}, which is not an entry"
                    )),
                    Some(other) if !says_supersedes(other, entry.id) => out.push(format!(
                        "{name} is superseded by {by_name}, which does not say `Supersedes \
                         {name}`"
                    )),
                    Some(_) => {}
                }
            }
        }
    }
    for (at, word) in entry.text.match_indices("Supersedes ") {
        let rest = entry
            .text
            .get(at.saturating_add(word.len())..)
            .unwrap_or("");
        for id in leading_ids(rest) {
            if by_id.get(&id).is_some_and(|other| !other.struck()) {
                out.push(format!(
                    "{name} supersedes {}, which is not struck",
                    decisions::format(id)
                ));
            }
        }
    }
    out
}

/// Whether a sentence of `entry` that has the word `Supersedes` names `id`.
fn says_supersedes(entry: &Entry, id: u32) -> bool {
    entry.text.match_indices("Supersedes").any(|(at, _)| {
        let rest = entry.text.get(at..).unwrap_or("");
        let sentence = rest.split_once(". ").map_or(rest, |(sentence, _)| sentence);
        decisions::mentioned_in(sentence).contains(&id)
    })
}

/// A link into `docs/adr` names a file there, and a label `ADR-NNNN` has the
/// file's number.
fn adr_links(entry: &Entry, adr_exists: &dyn Fn(&str) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut parts = entry.text.split("](../adr/");
    let Some(mut before) = parts.next() else {
        return out;
    };
    for part in parts {
        let file = part.split_once(')').map_or(part, |(file, _)| file);
        let label = before.rsplit_once('[').map_or("", |(_, label)| label);
        if !adr_exists(file) {
            out.push(format!("`docs/adr/{file}` does not exist"));
        } else if let Some(number) = label.strip_prefix("ADR-")
            && !file.starts_with(&format!("{number}-"))
        {
            out.push(format!("the link `{label}` names `docs/adr/{file}`"));
        }
        before = part;
    }
    out
}

/// Whether a heading is `YYYY-MM-DD: topic` with a month and a day that
/// exist.
fn heading_is_dated(heading: &str) -> bool {
    let Some((date, topic)) = heading.split_once(": ") else {
        return false;
    };
    let parts: Vec<&str> = date.split('-').collect();
    let [year, month, day] = parts.as_slice() else {
        return false;
    };
    let number = |text: &str, digits: usize| {
        (text.len() == digits && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse::<u32>().ok())
            .flatten()
    };
    number(year, 4).is_some()
        && number(month, 2).is_some_and(|month| (1..=12).contains(&month))
        && number(day, 2).is_some_and(|day| (1..=31).contains(&day))
        && !topic.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An id that the repository's log does not have. It is built here, so
    /// that this file names no such id for the lint to find.
    fn missing() -> String {
        decisions::format(9000)
    }

    fn messages(text: &str) -> Vec<String> {
        in_log(&Log::parse(text), &|name| name == "0001-one.md")
            .into_iter()
            .map(|problem| format!("{}: {}", problem.line, problem.message))
            .collect()
    }

    fn session(entries: &str) -> String {
        format!("## 2026-09-28: Topic\n\nGoal: a goal.\n\n{entries}")
    }

    #[test]
    fn a_log_in_form_has_no_problems() {
        let text = session(
            "- **D-001 One**: text. → [ADR-0001](../adr/0001-one.md)\n\
             - ~~**D-002 Two**: text.~~ Superseded by D-004.\n\
             - **D-004 Four**: text. Supersedes D-002. Refines D-001.\n",
        );
        assert_eq!(messages(&text), Vec::<String>::new());
    }

    #[test]
    fn the_form_of_entries_and_sessions() {
        let text = "- **D-001 One**: above the sessions.\n\
                    ## Topic without a date\n\n\
                    - **D-002 Two** no colon.\n\
                    - **D-0003 Three**: a leading zero.\n\
                    - **D-04 Four**: two digits.\n\n\
                    ## 2026-13-01: A month that does not exist\n\n\
                    Goal: a goal.\n\n\
                    Open:\n\n\
                    - **D-005 Five**: below the list.\n";
        let found = messages(text);
        let has = |line: usize, part: &str| {
            assert!(
                found
                    .iter()
                    .any(|m| m.starts_with(&format!("{line}: ")) && m.contains(part)),
                "{line} {part}: {found:#?}"
            );
        };
        has(1, "above the first session");
        has(2, "YYYY-MM-DD");
        has(2, "no `Goal:`");
        has(4, "does not end with");
        has(5, "written `D-003`");
        has(6, "written `D-004`");
        has(8, "YYYY-MM-DD");
        has(14, "below its session's");
    }

    #[test]
    fn ids_are_unique_and_rise_within_a_session() {
        let text = format!(
            "{}\n{}",
            session(
                "- **D-005 Five**: text.\n- **D-003 Three**: text.\n- **D-003 Again**: text.\n"
            ),
            // A later session may start below the last id: branches merge
            // out of order.
            session("- **D-001 One**: text.\n- **D-004 Four**: text.\n"),
        );
        assert_eq!(
            messages(&text),
            [
                "6: D-003 follows D-005 in one session: ids rise within a session",
                "7: D-003 is already the entry of line 6",
                "7: D-003 follows D-003 in one session: ids rise within a session",
            ]
        );
    }

    #[test]
    fn a_struck_entry_and_its_successor_name_each_other() {
        let text = session(
            "- ~~**D-001 One**: text.~~\n\
             - ~~**D-002 Two**: text.~~ Superseded by D-005.\n\
             - ~~**D-003 Three**: text.~~ Superseded by D-006.\n\
             - **D-004 Four**: text.\n\
             - **D-005 Five**: text. Refines D-002.\n\
             - **D-006 Six**: text. Supersedes D-003 and D-004. Supersedes part of D-005.\n",
        );
        let found = messages(&text);
        assert_eq!(found.len(), 3, "{found:#?}");
        assert!(found[0].starts_with("5: D-001 is struck, so"));
        assert!(found[1].contains("D-002 is superseded by D-005, which does not say"));
        assert!(found[2].contains("D-006 supersedes D-004, which is not struck"));
        let unknown = session(&format!(
            "- ~~**D-001 One**: text.~~ Superseded by {}.\n",
            missing()
        ));
        assert!(messages(&unknown)[0].contains("which is not an entry"));
    }

    #[test]
    fn adr_links_name_files_with_their_number() {
        let text = session(
            "- **D-001 One**: text. → [ADR-0002](../adr/0002-two.md)\n\
             - **D-002 Two**: text. → [ADR-0002](../adr/0001-one.md) and [the first](../adr/0001-one.md)\n",
        );
        assert_eq!(
            messages(&text),
            [
                "5: `docs/adr/0002-two.md` does not exist",
                "6: the link `ADR-0002` names `docs/adr/0001-one.md`",
            ]
        );
    }

    /// From `STRICT_FROM` on, an entry has a `Why:`, its session an `Open:`
    /// list, and a struck item says how it was settled. Older sessions are
    /// exempt.
    #[test]
    fn the_newer_rules_start_at_the_cutoff() {
        let old =
            session("- **D-001 One**: no reason.\n\nOpen:\n\n- ~~An item.~~ Settled by D-001.\n");
        assert_eq!(messages(&old), Vec::<String>::new());
        let first = decisions::format(STRICT_FROM);
        let next = decisions::format(STRICT_FROM.saturating_add(1));
        let bare = session(&format!("- **{first} New**: no reason.\n"));
        assert_eq!(
            messages(&bare),
            [
                format!("5: {first} has no `Why:`"),
                "1: the session has no `Open:` list".to_owned(),
            ]
        );
        let full = session(&format!(
            "- **{first} New**: a rule. Why: a reason.\n\
             - **{next} Newer**: a rule. Why: a reason.\n\n\
             Open:\n\n\
             - An item.\n\
             - ~~An item.~~ → {next}\n\
             - ~~An item.~~ Done: written.\n\
             - ~~An item.~~ Settled.\n\
             - ~~An item.~~ → later\n"
        ));
        let found = messages(&full);
        assert_eq!(found.len(), 2, "{found:#?}");
        assert!(found[0].starts_with("13: a struck item"));
        assert!(found[1].starts_with("14: a struck item"));
    }

    /// A citation names an entry, or a decision that the list gives to
    /// another branch. A row of the list fails when its id is an entry or
    /// when nothing cites it.
    #[test]
    fn a_citation_names_an_entry() {
        let log = Log::parse(&session("- **D-001 One**: text.\n"));
        let cite = |id| Cite {
            file: "src/lib.rs".to_owned(),
            line: 3,
            id,
            directive: false,
        };
        let messages = |cites: &[Cite], elsewhere: &[(u32, &str, &str)]| -> Vec<String> {
            in_cites(&log, cites, elsewhere)
                .into_iter()
                .map(|problem| problem.message)
                .collect()
        };
        assert_eq!(
            messages(&[cite(1), cite(9000)], &[]),
            [format!("{} is not an entry of the decision log", missing())]
        );
        let row = [(9000, "other", "a reason")];
        assert_eq!(messages(&[cite(1), cite(9000)], &row), Vec::<String>::new());
        assert_eq!(
            messages(&[cite(1)], &row),
            [format!(
                "no file cites {} any more: remove its row of `other` from `ON_OTHER_BRANCHES`",
                missing()
            )]
        );
        assert_eq!(
            messages(&[cite(1)], &[(1, "other", "a reason")]),
            [
                "D-001 is an entry of the log now: remove its row of `other` from `ON_OTHER_BRANCHES`"
            ]
        );
    }

    #[test]
    fn an_adr_number_has_one_file() {
        let files: Vec<String> = ["0001-one.md", "0002-other.md", "0002-two.md", "README.md"]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(
            in_adrs(&files),
            [Problem {
                file: "docs/adr/0002-two.md".to_owned(),
                line: 1,
                message: "`0002-other.md` has the ADR number 0002 already".to_owned(),
            }]
        );
    }

    /// The line the list's problems point at is the list's.
    #[test]
    fn the_list_is_where_its_line_says() {
        let text = std::fs::read_to_string(
            cheby_test_support::repo_root().join("xtask/src/decisions/lint.rs"),
        )
        .unwrap();
        let line = usize::try_from(ON_OTHER_BRANCHES_LINE).unwrap();
        assert!(
            text.lines()
                .nth(line - 1)
                .unwrap()
                .contains("const ON_OTHER_BRANCHES:"),
            "line {line}"
        );
    }

    /// The repository's log follows the rules of its form. The citations
    /// are checked by `cargo xtask decisions lint`, which CI runs.
    #[test]
    fn the_log_is_in_form() {
        let root = cheby_test_support::repo_root();
        let log = super::super::read(&root).unwrap();
        let adr = root.join("docs/adr");
        let found = in_log(&log, &|name| adr.join(name).is_file());
        assert_eq!(found, Vec::<Problem>::new());
        assert!(log.entries.len() > 1800);
    }
}
