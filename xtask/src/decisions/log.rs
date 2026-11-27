//! The decision log as data: its sessions, its entries and the items of
//! its `Open:` lists, and what an entry names (D-1940).

use std::collections::BTreeMap;

use cheby_test_support::decisions;

/// The log's path from the repository root.
pub(crate) const PATH: &str = "docs/decisions/LOG.md";

/// The parsed log.
#[derive(Debug, Default)]
pub(crate) struct Log {
    pub(crate) sessions: Vec<Session>,
    /// The entries in the file's order.
    pub(crate) entries: Vec<Entry>,
    pub(crate) open_items: Vec<OpenItem>,
    /// The lines that start like an entry and are not one, with what is
    /// wrong.
    pub(crate) malformed: Vec<(usize, &'static str)>,
}

/// A `## YYYY-MM-DD: topic` heading and what follows it.
#[derive(Debug)]
pub(crate) struct Session {
    pub(crate) line: usize,
    /// The heading after `## `.
    pub(crate) heading: String,
    /// Whether the session has a `Goal:` line.
    pub(crate) goal: bool,
    /// The line of its `Open:`.
    pub(crate) open: Option<usize>,
}

/// One `- **D-NNN Title**: text` line.
#[derive(Debug)]
pub(crate) struct Entry {
    pub(crate) id: u32,
    /// The id's digits as written.
    pub(crate) digits: String,
    pub(crate) title: String,
    /// The text after the title, without the marks of a struck entry.
    pub(crate) text: String,
    /// What follows the closing `~~` of a struck entry.
    pub(crate) after_strike: Option<String>,
    pub(crate) line: usize,
    /// The index of its session.
    pub(crate) session: Option<usize>,
    /// Whether it stands below its session's `Open:`.
    pub(crate) below_open: bool,
    /// The indented lines below it, such as a table.
    pub(crate) more: Vec<String>,
}

/// One item of an `Open:` list.
#[derive(Debug)]
pub(crate) struct OpenItem {
    pub(crate) line: usize,
    pub(crate) session: Option<usize>,
    /// For an item that starts struck, what follows its closing `~~`.
    pub(crate) after_strike: Option<String>,
}

/// How an entry speaks of another one, in the words the log uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Relation {
    Supersedes,
    Refines,
    Extends,
    Applies,
    Confirms,
    /// The entry names the other one without one of the words.
    Names,
}

impl Relation {
    /// The relations that have a word, with the word.
    const WORDS: [(Self, &'static str); 5] = [
        (Self::Supersedes, "Supersedes"),
        (Self::Refines, "Refines"),
        (Self::Extends, "Extends"),
        (Self::Applies, "Applies"),
        (Self::Confirms, "Confirms"),
    ];

    /// The label of the decisions an entry names this way.
    pub(crate) const fn forward(self) -> &'static str {
        match self {
            Self::Supersedes => "Supersedes",
            Self::Refines => "Refines",
            Self::Extends => "Extends",
            Self::Applies => "Applies",
            Self::Confirms => "Confirms",
            Self::Names => "Names",
        }
    }

    /// The label of the entries that name a decision this way.
    pub(crate) const fn backward(self) -> &'static str {
        match self {
            Self::Supersedes => "Superseded by",
            Self::Refines => "Refined by",
            Self::Extends => "Extended by",
            Self::Applies => "Applied by",
            Self::Confirms => "Confirmed by",
            Self::Names => "Named by",
        }
    }
}

impl Entry {
    pub(crate) const fn struck(&self) -> bool {
        self.after_strike.is_some()
    }

    /// The decisions the entry's text names, each with the word that stands
    /// directly before it or before the list it is in, as in
    /// `Refines D-159 and D-244`. Its own id is left out, and so is what
    /// follows the strike of a struck entry.
    fn names(&self) -> BTreeMap<u32, Relation> {
        let mut out = BTreeMap::new();
        let texts = std::iter::once(&self.text).chain(&self.more);
        for text in texts {
            for id in decisions::mentioned_in(text) {
                out.entry(id).or_insert(Relation::Names);
            }
            // The strongest word is written last, so it stays.
            for (relation, word) in Relation::WORDS.into_iter().rev() {
                for (at, _) in text.match_indices(word) {
                    let rest = text
                        .get(at.saturating_add(word.len())..)
                        .and_then(|rest| rest.strip_prefix(' '));
                    for id in rest.map_or_default(leading_ids) {
                        out.insert(id, relation);
                    }
                }
            }
        }
        out.remove(&self.id);
        out
    }

    /// The decision a struck entry says it is superseded by.
    pub(crate) fn superseded_by(&self) -> Option<u32> {
        let after = self.after_strike.as_deref()?;
        let rest = after.trim_start().strip_prefix("Superseded by ")?;
        id_at(rest).map(|(id, _)| id)
    }
}

impl Log {
    pub(crate) fn parse(text: &str) -> Self {
        let mut log = Self::default();
        // Whether the lines read now are below the session's `Open:`.
        let mut in_open = false;
        // Whether an indented line still belongs to the last entry.
        let mut in_entry = false;
        for (index, line) in text.lines().enumerate() {
            let number = index.saturating_add(1);
            let session = log.sessions.len().checked_sub(1);
            if let Some(heading) = line.strip_prefix("## ") {
                log.sessions.push(Session {
                    line: number,
                    heading: heading.to_owned(),
                    goal: false,
                    open: None,
                });
                in_open = false;
                in_entry = false;
                continue;
            }
            match entry_line(line) {
                Line::Entry(raw) => {
                    log.entries.push(Entry {
                        id: raw.id,
                        digits: raw.digits.to_owned(),
                        title: raw.title.to_owned(),
                        text: raw.text.to_owned(),
                        after_strike: raw.after_strike.map(str::to_owned),
                        line: number,
                        session,
                        below_open: in_open,
                        more: Vec::new(),
                    });
                    in_entry = true;
                    continue;
                }
                Line::Malformed(why) => {
                    log.malformed.push((number, why));
                    in_entry = false;
                    continue;
                }
                Line::Other => {}
            }
            if line.trim().is_empty() {
                continue;
            }
            if in_entry && line.starts_with("  ") {
                if let Some(entry) = log.entries.last_mut() {
                    entry.more.push(line.to_owned());
                }
                continue;
            }
            in_entry = false;
            if line.starts_with("Goal:") {
                if let Some(session) = log.sessions.last_mut() {
                    session.goal = true;
                }
            } else if line.trim_end() == "Open:" {
                if let Some(session) = log.sessions.last_mut() {
                    session.open = Some(number);
                }
                in_open = true;
            } else if in_open && let Some(item) = line.strip_prefix("- ") {
                log.open_items.push(OpenItem {
                    line: number,
                    session,
                    after_strike: item.strip_prefix("~~").map(|rest| {
                        rest.rsplit_once("~~")
                            .map_or("", |(_, after)| after)
                            .to_owned()
                    }),
                });
            }
        }
        log
    }

    /// The entries by id; the first one wins when an id is used twice.
    pub(crate) fn by_id(&self) -> BTreeMap<u32, &Entry> {
        let mut out = BTreeMap::new();
        for entry in &self.entries {
            out.entry(entry.id).or_insert(entry);
        }
        out
    }

    /// The heading of an entry's session.
    pub(crate) fn heading_of(&self, entry: &Entry) -> &str {
        entry
            .session
            .and_then(|index| self.sessions.get(index))
            .map_or("", |session| session.heading.as_str())
    }
}

/// The decisions `entry` names, with the word it uses (see
/// [`Entry::names`]). A struck entry that says it is superseded by `entry`
/// counts as superseded however `entry` words it, as in
/// `Supersedes the pipe part of D-050 and D-221`.
pub(crate) fn names_of(entry: &Entry, by_id: &BTreeMap<u32, &Entry>) -> BTreeMap<u32, Relation> {
    let mut out = entry.names();
    for (id, relation) in &mut out {
        if by_id
            .get(id)
            .is_some_and(|other| other.superseded_by() == Some(entry.id))
        {
            *relation = Relation::Supersedes;
        }
    }
    out
}

/// The title of every entry of a log's text, by id, for a log that is only
/// compared with another one.
pub(crate) fn titles(text: &str) -> BTreeMap<u32, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if let Line::Entry(raw) = entry_line(line) {
            out.entry(raw.id).or_insert_with(|| raw.title.to_owned());
        }
    }
    out
}

/// The ids that `text` starts with: `D-1, D-2 and D-3` gives three, and a
/// text that starts with another word gives none.
pub(crate) fn leading_ids(text: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some((id, after)) = id_at(rest) {
        out.push(id);
        let Some(next) = [", and ", " and ", ", "]
            .iter()
            .find_map(|separator| after.strip_prefix(separator))
        else {
            break;
        };
        rest = next;
    }
    out
}

/// The id `text` starts with, in the form the log writes it, and the rest.
fn id_at(text: &str) -> Option<(u32, &str)> {
    let digits = text.strip_prefix("D-")?;
    let length = digits.bytes().take_while(u8::is_ascii_digit).count();
    let (digits, rest) = digits.split_at_checked(length)?;
    let id = decisions::parse(&format!("D-{digits}")).ok()?;
    Some((id, rest))
}

struct Raw<'a> {
    id: u32,
    digits: &'a str,
    title: &'a str,
    text: &'a str,
    after_strike: Option<&'a str>,
}

enum Line<'a> {
    Entry(Raw<'a>),
    /// It starts like an entry, `- **D-` or `- ~~**D-`, and is not one.
    Malformed(&'static str),
    Other,
}

fn entry_line(line: &str) -> Line<'_> {
    let Some(rest) = line.strip_prefix("- ") else {
        return Line::Other;
    };
    let (struck, rest) = rest
        .strip_prefix("~~")
        .map_or((false, rest), |rest| (true, rest));
    let Some(rest) = rest.strip_prefix("**D-") else {
        return Line::Other;
    };
    let length = rest.bytes().take_while(u8::is_ascii_digit).count();
    let Some((digits, rest)) = rest.split_at_checked(length) else {
        return Line::Malformed("no number after `D-`");
    };
    let Ok(id) = digits.parse() else {
        return Line::Malformed("no number after `D-`");
    };
    let Some(rest) = rest.strip_prefix(' ') else {
        return Line::Malformed("no title after the id");
    };
    let Some((title, body)) = rest.split_once("**: ") else {
        return Line::Malformed("the title does not end with `**: ` and a text");
    };
    if title.trim().is_empty() || body.trim().is_empty() {
        return Line::Malformed("the title or the text is empty");
    }
    let (text, after_strike) = if struck {
        let Some((text, after)) = body.rsplit_once("~~") else {
            return Line::Malformed("a struck entry has no closing `~~`");
        };
        (text, Some(after))
    } else {
        (body, None)
    };
    Line::Entry(Raw {
        id,
        digits,
        title,
        text,
        after_strike,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "\
# Decision log

## 2026-09-28: First

Goal: start.

- **D-001 One**: the first rule. Why: a reason.
- ~~**D-002 Two**: the second rule. Why: a reason.~~ Superseded by D-003.
- **D-003 Three**: the third rule, with a table.
  | a | b |
  | - | - |

  and a line after a blank one. Why: a reason. Supersedes D-002. Refines D-001 and D-002.

A paragraph between entries.

Open:

- An open item.
- ~~A settled item.~~ → D-003
- ~~Another one.~~ Done: written.
";

    #[test]
    fn reads_sessions_entries_and_open_items() {
        let log = Log::parse(LOG);
        assert_eq!(log.sessions.len(), 1);
        let session = &log.sessions[0];
        assert_eq!(session.heading, "2026-09-28: First");
        assert!(session.goal);
        assert_eq!(session.open, Some(17));
        let ids: Vec<u32> = log.entries.iter().map(|e| e.id).collect();
        assert_eq!(ids, [1, 2, 3]);
        assert_eq!(log.entries[0].title, "One");
        assert_eq!(log.entries[0].text, "the first rule. Why: a reason.");
        assert_eq!(log.entries[0].line, 7);
        assert!(!log.entries[0].struck());
        assert_eq!(log.malformed, Vec::<(usize, &str)>::new());
        let after: Vec<Option<&str>> = log
            .open_items
            .iter()
            .map(|item| item.after_strike.as_deref())
            .collect();
        assert_eq!(after, [None, Some(" → D-003"), Some(" Done: written.")]);
    }

    #[test]
    fn a_struck_entry_keeps_its_text_and_what_follows() {
        let log = Log::parse(LOG);
        let two = &log.entries[1];
        assert!(two.struck());
        assert_eq!(two.text, "the second rule. Why: a reason.");
        assert_eq!(two.superseded_by(), Some(3));
        assert_eq!(log.entries[0].superseded_by(), None);
    }

    /// Indented lines belong to the entry above, across a blank line; a
    /// paragraph at the margin does not.
    #[test]
    fn indented_lines_belong_to_the_entry() {
        let log = Log::parse(LOG);
        let three = &log.entries[2];
        assert_eq!(three.more.len(), 3);
        assert!(three.more[2].contains("Supersedes D-002"));
    }

    #[test]
    fn names_carry_the_word_before_them() {
        let log = Log::parse(LOG);
        let names = names_of(&log.entries[2], &log.by_id());
        // `Supersedes` wins over `Refines` for D-002, and the entry's own id is
        // left out.
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            [(1, Relation::Refines), (2, Relation::Supersedes)]
        );
        assert_eq!(leading_ids("D-001, D-002, and D-003."), [1, 2, 3]);
        assert_eq!(leading_ids("the pipe part of D-050"), [0_u32; 0]);
        assert_eq!(leading_ids("D-001 to D-003"), [1]);
        // `D-1` is not how the log writes an id.
        assert_eq!(leading_ids("D-1"), [0_u32; 0]);
    }

    /// The struck entry's `Superseded by` makes the relation, whatever the
    /// words of the entry that supersedes it.
    #[test]
    fn a_part_of_an_entry_is_superseded() {
        let log = Log::parse(
            "- **D-001 One**: text.\n\
             - ~~**D-002 Two**: text.~~ Superseded by D-003.\n\
             - **D-003 Three**: text. Supersedes the first part of D-001 and D-002.\n",
        );
        let by_id = log.by_id();
        let names = names_of(&log.entries[2], &by_id);
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            [(1, Relation::Names), (2, Relation::Supersedes)]
        );
        // What follows a strike is the status, not a name.
        assert_eq!(names_of(&log.entries[1], &by_id).len(), 0);
    }

    #[test]
    fn lines_that_start_like_an_entry_are_malformed() {
        for (line, why) in [
            ("- **D-x Title**: text", "number"),
            ("- **D-004: Title**: text", "no title"),
            ("- **D-004 Title** text", "does not end"),
            ("- **D-004 Title**: ", "is empty"),
            ("- **D-004 Title**:", "does not end"),
            ("- ~~**D-004 Title**: text", "closing"),
        ] {
            let log = Log::parse(line);
            assert_eq!(log.entries.len(), 0, "{line}");
            assert_eq!(log.malformed.len(), 1, "{line}");
            assert!(
                log.malformed[0].1.contains(why),
                "{line}: {}",
                log.malformed[0].1
            );
        }
        // An item of an `Open:` list may start with an id.
        let log = Log::parse("- D-004 is not implemented yet.");
        assert_eq!((log.entries.len(), log.malformed.len()), (0, 0));
    }

    #[test]
    fn titles_by_id() {
        let titles = titles(LOG);
        assert_eq!(titles.len(), 3);
        assert_eq!(titles[&2], "Two");
    }
}
