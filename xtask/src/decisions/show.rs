//! `cargo xtask decisions show` and `search` (D-1940).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use cheby_test_support::decisions;

use super::cites::{self, Cite};
use super::log::{self, Entry, Log, Relation, names_of};

/// How many lines a list of `show` has before it is cut, without
/// `--cites`.
const SHOWN: usize = 10;

/// The relations in the order `show` prints them.
const RELATIONS: [Relation; 6] = [
    Relation::Supersedes,
    Relation::Refines,
    Relation::Extends,
    Relation::Applies,
    Relation::Confirms,
    Relation::Names,
];

/// The headings above the lines of the spec that cite a decision, by file
/// and line.
type Headings = BTreeMap<(String, usize), String>;

pub(crate) fn show(root: &Path, args: &[String], whole: bool) -> Result<(), String> {
    let log = super::read(root)?;
    let by_id = log.by_id();
    let mut entries = Vec::new();
    for arg in args {
        entries.extend(asked(arg, &by_id)?);
    }
    let cites = cites::all(root)?;
    let named_by = named_by(&log, &by_id);
    let headings = headings(root, &entries, &cites)?;
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let view = View {
            log: &log,
            by_id: &by_id,
            named_by: &named_by,
            cites: &cites,
            headings: &headings,
            whole,
        };
        print!("{}", view.render(entry));
    }
    Ok(())
}

/// The entries one argument asks for: `D-244`, `244`, or a range
/// `D-1930..D-1939`, which gives the entries the log has in it.
fn asked<'a>(arg: &str, by_id: &BTreeMap<u32, &'a Entry>) -> Result<Vec<&'a Entry>, String> {
    let id = |text: &str| {
        let digits = text
            .strip_prefix("D-")
            .or_else(|| text.strip_prefix("d-"))
            .unwrap_or(text);
        digits
            .parse::<u32>()
            .map_err(|_| format!("`{text}` is not a decision's id, such as `D-244`"))
    };
    if let Some((first, last)) = arg.split_once("..") {
        let found: Vec<&Entry> = by_id
            .range(id(first)?..=id(last)?)
            .map(|(_, entry)| *entry)
            .collect();
        if found.is_empty() {
            return Err(format!("the log has no entry in `{arg}`"));
        }
        return Ok(found);
    }
    let id = id(arg)?;
    by_id.get(&id).map(|entry| vec![*entry]).ok_or_else(|| {
        format!(
            "{} is not an entry of the decision log",
            decisions::format(id)
        )
    })
}

/// The entries that name each decision, with the word they use.
fn named_by(log: &Log, by_id: &BTreeMap<u32, &Entry>) -> BTreeMap<u32, Vec<(Relation, u32)>> {
    let mut out: BTreeMap<u32, Vec<(Relation, u32)>> = BTreeMap::new();
    for entry in &log.entries {
        for (id, relation) in names_of(entry, by_id) {
            out.entry(id).or_default().push((relation, entry.id));
        }
    }
    out
}

fn in_spec(file: &str) -> bool {
    file.starts_with("docs/spec/") || file.starts_with("docs/stdlib/")
}

/// The heading above each line of `docs/spec` and `docs/stdlib` that cites
/// one of `entries`.
fn headings(root: &Path, entries: &[&Entry], cites: &[Cite]) -> Result<Headings, String> {
    let mut texts: BTreeMap<&str, String> = BTreeMap::new();
    let mut out = Headings::new();
    for cite in cites {
        if !in_spec(&cite.file) || !entries.iter().any(|entry| entry.id == cite.id) {
            continue;
        }
        if !texts.contains_key(cite.file.as_str()) {
            let path = root.join(&cite.file);
            let text =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            texts.insert(&cite.file, text);
        }
        let heading = texts
            .get(cite.file.as_str())
            .and_then(|text| heading_above(text, cite.line));
        if let Some(heading) = heading {
            out.insert((cite.file.clone(), cite.line), heading);
        }
    }
    Ok(out)
}

/// The last Markdown heading at or above `line`, without its `#` marks. A
/// `#` line inside a code fence is not a heading.
fn heading_above(text: &str, line: usize) -> Option<String> {
    let mut found = None;
    let mut fenced = false;
    for text in text.lines().take(line) {
        if text.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if !fenced && text.starts_with('#') {
            found = Some(text.trim_start_matches('#').trim().to_owned());
        }
    }
    found
}

/// What `show` prints an entry from.
struct View<'a> {
    log: &'a Log,
    by_id: &'a BTreeMap<u32, &'a Entry>,
    named_by: &'a BTreeMap<u32, Vec<(Relation, u32)>>,
    cites: &'a [Cite],
    headings: &'a Headings,
    /// Whether lists are printed whole, with every place as `file:line`.
    whole: bool,
}

impl View<'_> {
    fn render(&self, entry: &Entry) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "{} {}", decisions::format(entry.id), entry.title);
        let _ = writeln!(out, "  {}", self.log.heading_of(entry));
        let _ = writeln!(out, "  {}:{}", log::PATH, entry.line);
        let _ = writeln!(out, "  {}", self.status(entry));
        let _ = writeln!(out);
        let _ = writeln!(out, "  {}", entry.text);
        for line in &entry.more {
            let _ = writeln!(out, "{line}");
        }
        let _ = writeln!(out);
        let names = names_of(entry, self.by_id);
        for relation in RELATIONS {
            let lines: Vec<String> = names
                .iter()
                .filter(|&(_, found)| *found == relation)
                .map(|(id, _)| self.titled(*id))
                .collect();
            self.list(&mut out, relation.forward(), &lines);
        }
        let named_by = self.named_by.get(&entry.id).map_or(&[][..], Vec::as_slice);
        for relation in RELATIONS {
            let lines: Vec<String> = named_by
                .iter()
                .filter(|&&(found, _)| found == relation)
                .map(|&(_, id)| self.titled(id))
                .collect();
            self.list(&mut out, relation.backward(), &lines);
        }
        let adrs: Vec<String> = entry
            .text
            .split("](../adr/")
            .skip(1)
            .map(|part| {
                format!(
                    "docs/adr/{}",
                    part.split_once(')').map_or(part, |(file, _)| file)
                )
            })
            .collect();
        self.list(&mut out, "ADR", &adrs);
        self.places(&mut out, entry.id);
        out
    }

    /// Whether the entry is in force, or the chain of entries that
    /// superseded it.
    fn status(&self, entry: &Entry) -> String {
        if !entry.struck() {
            return "in force".to_owned();
        }
        let mut out = "struck".to_owned();
        let mut current = entry;
        // An entry is superseded by a later one, so the chain ends; the
        // count guards against a log that says otherwise.
        for _ in 0..self.by_id.len() {
            let Some(by) = current.superseded_by() else {
                break;
            };
            let _ = write!(out, ", superseded by {}", self.titled(by));
            match self.by_id.get(&by) {
                Some(next) if next.struck() && next.id != current.id => current = next,
                Some(_) | None => break,
            }
        }
        out
    }

    /// `D-NNN Title`, and that the entry is struck.
    fn titled(&self, id: u32) -> String {
        let name = decisions::format(id);
        match self.by_id.get(&id) {
            Some(entry) if entry.struck() => format!("{name} {} (struck)", entry.title),
            Some(entry) => format!("{name} {}", entry.title),
            None => format!("{name} (not in the log)"),
        }
    }

    /// The spec sections, the tests and the other places that cite `id`.
    fn places(&self, out: &mut String, id: u32) {
        let mut spec = Vec::new();
        let mut tests = Vec::new();
        let mut elsewhere = Vec::new();
        let mut per_directory: BTreeMap<String, usize> = BTreeMap::new();
        for cite in self.cites {
            if cite.id != id || cite.file == log::PATH {
                continue;
            }
            let place = if self.whole {
                format!("{}:{}", cite.file, cite.line)
            } else {
                cite.file.clone()
            };
            if in_spec(&cite.file) {
                let heading = self.headings.get(&(cite.file.clone(), cite.line));
                spec.push(heading.map_or_else(|| place.clone(), |h| format!("{place}, {h}")));
            } else if cite.directive {
                tests.push(place);
            } else {
                let count = per_directory.entry(directory(&cite.file)).or_default();
                *count = count.saturating_add(1);
                elsewhere.push(place);
            }
        }
        spec.dedup();
        tests.dedup();
        if !self.whole {
            let mut counted: Vec<(String, usize)> = per_directory.into_iter().collect();
            counted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            elsewhere = counted
                .into_iter()
                .map(|(directory, count)| format!("{directory} {count}"))
                .collect();
        }
        self.list(out, "Spec", &spec);
        self.list(out, "Tests", &tests);
        self.list(out, "Elsewhere", &elsewhere);
    }

    /// A label and its lines, cut after [`SHOWN`] unless the lists are
    /// printed whole; nothing for no lines.
    fn list(&self, out: &mut String, label: &str, lines: &[String]) {
        if lines.is_empty() {
            return;
        }
        let _ = writeln!(out, "  {label}:");
        let shown = if self.whole { lines.len() } else { SHOWN };
        for line in lines.iter().take(shown) {
            let _ = writeln!(out, "    {line}");
        }
        let more = lines.len().saturating_sub(shown);
        if more > 0 {
            let _ = writeln!(out, "    … and {more} more (--cites prints all)");
        }
    }
}

/// The first two parts of a path, as `crates/cheby_types`, or the file's
/// name for a file of the root.
fn directory(file: &str) -> String {
    let mut parts = file.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(first), Some(second), Some(_)) => format!("{first}/{second}"),
        (Some(first), Some(_), None) => first.to_owned(),
        _ => file.to_owned(),
    }
}

pub(crate) fn search(
    root: &Path,
    words: &[String],
    titles: bool,
    limit: usize,
) -> Result<(), String> {
    let log = super::read(root)?;
    let found = matches(&log, words, titles);
    for entry in found.iter().take(limit) {
        let date = log
            .heading_of(entry)
            .split_once(": ")
            .map_or("", |(date, _)| date);
        let struck = entry.superseded_by().map_or_else(String::new, |by| {
            format!(", superseded by {}", decisions::format(by))
        });
        println!(
            "{} {} ({date}{struck})",
            decisions::format(entry.id),
            entry.title
        );
        if !titles && let Some(snippet) = words.iter().find_map(|word| snippet(&entry.text, word)) {
            println!("    {snippet}");
        }
    }
    let entries = |count: usize| if count == 1 { "entry" } else { "entries" };
    if found.len() > limit {
        println!(
            "{limit} of {} {}; --limit {} prints all",
            found.len(),
            entries(found.len()),
            found.len()
        );
    } else {
        println!("{} {}", found.len(), entries(found.len()));
    }
    Ok(())
}

/// The entries whose title or text has every word, ignoring the case of
/// ASCII letters: those with every word in the title first, each group in
/// the log's order. With `titles`, only the first group. A word matches
/// where a word of the text starts with it ([`word_at`]).
fn matches<'a>(log: &'a Log, words: &[String], titles: bool) -> Vec<&'a Entry> {
    let words: Vec<String> = words.iter().map(|word| word.to_ascii_lowercase()).collect();
    let mut in_title = Vec::new();
    let mut in_text = Vec::new();
    for entry in &log.entries {
        let title = entry.title.to_ascii_lowercase();
        if words.iter().all(|word| word_at(&title, word).is_some()) {
            in_title.push(entry);
        } else if !titles {
            let text = format!("{} {}", entry.text, entry.more.join(" ")).to_ascii_lowercase();
            if words.iter().all(|word| {
                word_at(&title, word)
                    .or_else(|| word_at(&text, word))
                    .is_some()
            }) {
                in_text.push(entry);
            }
        }
    }
    in_title.extend(in_text);
    in_title
}

/// Where `word` first stands at the start of a word of `text`: `hole` is
/// found in `holes` and in `pipe_hole`, and not in `whole`.
fn word_at(text: &str, word: &str) -> Option<usize> {
    text.match_indices(word).map(|(at, _)| at).find(|&at| {
        !text
            .get(..at)
            .and_then(|before| before.bytes().next_back())
            .is_some_and(|b| b.is_ascii_alphanumeric())
    })
}

/// About 160 characters of `text` around the first `word`, with `…` where
/// it is cut.
fn snippet(text: &str, word: &str) -> Option<String> {
    // ASCII lower case keeps every byte's place.
    let at = word_at(&text.to_ascii_lowercase(), &word.to_ascii_lowercase())?;
    let start = text.floor_char_boundary(at.saturating_sub(60));
    let end = text.ceil_char_boundary(at.saturating_add(100).min(text.len()));
    let part = text.get(start..end)?;
    Some(format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        part.trim(),
        if end < text.len() { "…" } else { "" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "\
## 2026-09-28: First

Goal: start.

- **D-001 Pipes**: the first rule. Why: a reason. → [ADR-0001](../adr/0001-one.md)
- ~~**D-002 Holes**: the second rule.~~ Superseded by D-003.
- ~~**D-003 Holes again**: the third rule. Supersedes D-002.~~ Superseded by D-004.
- **D-004 Explicit holes**: a pipe needs a hole. Supersedes D-003. Refines D-001, as D-002 did.
  | a table |
";

    fn cite(file: &str, line: usize, id: u32, directive: bool) -> Cite {
        Cite {
            file: file.to_owned(),
            line,
            id,
            directive,
        }
    }

    fn rendered(id: u32, cites: &[Cite], whole: bool) -> String {
        let log = Log::parse(LOG);
        let by_id = log.by_id();
        let mut headings = Headings::new();
        headings.insert(
            ("docs/spec/05-expressions.md".to_owned(), 12),
            "5.9 Pipes".to_owned(),
        );
        let view = View {
            log: &log,
            by_id: &by_id,
            named_by: &named_by(&log, &by_id),
            cites,
            headings: &headings,
            whole,
        };
        view.render(by_id[&id])
    }

    #[test]
    fn an_entry_with_its_relations_and_places() {
        let cites = [
            cite(log::PATH, 5, 1, false),
            cite("docs/spec/05-expressions.md", 12, 1, false),
            cite("docs/spec/05-expressions.md", 12, 1, false),
            cite("tests/ui/05-expressions/pipe.cheby", 2, 1, true),
            cite("tests/ui/05-expressions/pipe.cheby", 9, 1, false),
            cite("crates/cheby_types/src/a.rs", 3, 1, false),
            cite("crates/cheby_types/src/b.rs", 4, 1, false),
            cite("AGENTS.md", 7, 1, false),
            cite("crates/cheby_types/src/a.rs", 8, 4, false),
        ];
        assert_eq!(
            rendered(1, &cites, false),
            "D-001 Pipes
  2026-09-28: First
  docs/decisions/LOG.md:5
  in force

  the first rule. Why: a reason. → [ADR-0001](../adr/0001-one.md)

  Refined by:
    D-004 Explicit holes
  ADR:
    docs/adr/0001-one.md
  Spec:
    docs/spec/05-expressions.md, 5.9 Pipes
  Tests:
    tests/ui/05-expressions/pipe.cheby
  Elsewhere:
    crates/cheby_types 2
    AGENTS.md 1
    tests/ui 1
"
        );
        let whole = rendered(1, &cites, true);
        assert!(whole.contains("    docs/spec/05-expressions.md:12, 5.9 Pipes\n"));
        assert!(whole.contains("    tests/ui/05-expressions/pipe.cheby:2\n"));
        assert!(whole.contains("    crates/cheby_types/src/b.rs:4\n"));
    }

    #[test]
    fn a_struck_entry_names_its_chain() {
        let text = rendered(2, &[], false);
        assert!(
            text.contains(
                "  struck, superseded by D-003 Holes again (struck), superseded by D-004 \
                 Explicit holes\n"
            ),
            "{text}"
        );
        assert!(text.contains("  Superseded by:\n    D-003 Holes again (struck)\n"));
        assert!(text.contains("  Named by:\n    D-004 Explicit holes\n"));
        let four = rendered(4, &[], false);
        assert!(four.contains("  | a table |\n\n  Supersedes:\n    D-003 Holes again (struck)\n"));
        assert!(four.contains("  Refines:\n    D-001 Pipes\n  Names:\n    D-002 Holes (struck)\n"));
    }

    #[test]
    fn long_lists_are_cut_without_cites() {
        let cites: Vec<Cite> = (1..=12)
            .map(|n| cite(&format!("tests/ui/t{n:02}.cheby"), 1, 1, true))
            .collect();
        let cut = rendered(1, &cites, false);
        assert!(cut.contains("    tests/ui/t10.cheby\n    … and 2 more (--cites prints all)\n"));
        assert!(!cut.contains("t11"));
        assert!(rendered(1, &cites, true).contains("    tests/ui/t12.cheby:1\n"));
    }

    #[test]
    fn ids_and_ranges() {
        let log = Log::parse(LOG);
        let by_id = log.by_id();
        let ids = |arg: &str| -> Result<Vec<u32>, String> {
            asked(arg, &by_id).map(|found| found.iter().map(|e| e.id).collect())
        };
        assert_eq!(ids("D-002"), Ok(vec![2]));
        assert_eq!(ids("4"), Ok(vec![4]));
        assert_eq!(ids("D-002..D-900"), Ok(vec![2, 3, 4]));
        assert!(ids("D-005").unwrap_err().contains("D-005 is not an entry"));
        assert!(ids("D-005..D-009").unwrap_err().contains("no entry in"));
        assert!(ids("pipes").unwrap_err().contains("not a decision's id"));
    }

    #[test]
    fn headings_skip_code_fences() {
        let text = "# 5. Expressions\n\n## 5.9 Pipes\n\n```toml\n# a comment\n```\n\ntext\n\n## 5.10 Use\n";
        assert_eq!(heading_above(text, 9).as_deref(), Some("5.9 Pipes"));
        assert_eq!(heading_above(text, 11).as_deref(), Some("5.10 Use"));
        assert_eq!(heading_above("text\n", 1), None);
    }

    #[test]
    fn search_puts_titles_first() {
        let log = Log::parse(LOG);
        let ids = |words: &[&str], titles: bool| -> Vec<u32> {
            let words: Vec<String> = words.iter().map(|&w| w.to_owned()).collect();
            matches(&log, &words, titles).iter().map(|e| e.id).collect()
        };
        // D-004 has the word in its text, the others in their titles.
        assert_eq!(ids(&["HOLE"], false), [2, 3, 4]);
        assert_eq!(ids(&["pipe"], false), [1, 4]);
        assert_eq!(ids(&["pipe"], true), [1]);
        assert_eq!(ids(&["pipe", "explicit"], false), [4]);
        assert_eq!(ids(&["nothing"], false), [0_u32; 0]);
        // A word of the text starts with the word that is asked for.
        assert_eq!(ids(&["ole"], false), [0_u32; 0]);
        assert_eq!(word_at("the whole pipe_hole", "hole"), Some(15));
        assert_eq!(word_at("a whole", "hole"), None);
    }

    #[test]
    fn a_snippet_is_cut_at_characters() {
        let text = format!("{}the word → here{}", "é".repeat(100), "ü".repeat(100));
        let found = snippet(&text, "WORD").unwrap();
        assert!(found.starts_with('…') && found.ends_with('…'), "{found}");
        assert!(found.contains("the word → here"));
        assert_eq!(snippet("short text", "text").as_deref(), Some("short text"));
        assert_eq!(snippet("short text", "long"), None);
    }
}
