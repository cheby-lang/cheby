//! `cargo xtask decisions next-id` and `next-adr`: the logs and the ADR
//! directories of the other branches and worktrees, and the next free
//! number over all of them (D-1941, D-1943).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use cheby_test_support::decisions;

use super::log;

/// The directory of the ADRs, from the repository root.
pub(crate) const ADRS: &str = "docs/adr";

/// The sequence a number is taken from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sequence {
    /// The ids of the decision log's entries, each with its title.
    Decisions,
    /// The numbers of the files of `docs/adr`, each with its file's name.
    Adrs,
}

impl Sequence {
    /// A number as its sequence writes it.
    fn name(self, number: u32) -> String {
        match self {
            Self::Decisions => decisions::format(number),
            Self::Adrs => format!("{number:04}"),
        }
    }

    /// What is read, from the repository root.
    const fn path(self) -> &'static str {
        match self {
            Self::Decisions => log::PATH,
            Self::Adrs => ADRS,
        }
    }

    /// The numbers of the working tree.
    fn here(self, root: &Path) -> Result<BTreeMap<u32, String>, String> {
        match self {
            Self::Decisions => super::read_text(root).map(|text| log::titles(&text)),
            Self::Adrs => adr_files(&root.join(ADRS)).map(|names| adr_numbers(&names)),
        }
    }
}

/// A log or an ADR directory other than the working tree's: what each of
/// its numbers names, an entry's title or an ADR's file.
#[derive(Debug)]
pub(crate) struct Source {
    /// The branches, remote-tracking refs or worktree that have it.
    pub(crate) names: Vec<String>,
    pub(crate) titles: BTreeMap<u32, String>,
}

/// Inclusive ranges of ids.
type Ranges = Vec<(u32, u32)>;

/// What `next-id` says.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Report {
    /// The id after the highest one of every log.
    pub(crate) next: u32,
    /// The highest id of the working file.
    pub(crate) here: Option<u32>,
    /// Where the highest id is, when the working file does not have it.
    pub(crate) highest_in: Vec<String>,
    /// The ids the working file does not have, as ranges, with the logs
    /// that have exactly these.
    pub(crate) elsewhere: Vec<(Ranges, Vec<String>)>,
    /// The ids that are another decision in another log: the id, the title
    /// there and the logs.
    pub(crate) other_titles: Vec<(u32, String, Vec<String>)>,
}

pub(crate) fn run(root: &Path, sequence: Sequence) -> Result<(), String> {
    let here = sequence.here(root)?;
    let others = match sources(root, sequence) {
        Ok(others) => others,
        Err(why) => {
            eprintln!("note: only the working tree was read: {why}");
            Vec::new()
        }
    };
    let report = report(&here, &others);
    println!("{}", sequence.name(report.next));
    eprint!("{}", notes(&report, &here, sequence));
    Ok(())
}

/// The next id and what the other logs have that the working file lacks.
pub(crate) fn report(here: &BTreeMap<u32, String>, others: &[Source]) -> Report {
    let highest_here = here.keys().next_back().copied();
    let highest = others
        .iter()
        .filter_map(|source| source.titles.keys().next_back().copied())
        .chain(highest_here)
        .max();
    let mut highest_in = Vec::new();
    let mut elsewhere: BTreeMap<Ranges, Vec<String>> = BTreeMap::new();
    let mut other_titles: BTreeMap<(u32, &str), Vec<String>> = BTreeMap::new();
    for source in others {
        if highest != highest_here && source.titles.keys().next_back().copied() == highest {
            highest_in.extend(source.names.iter().cloned());
        }
        let missing: Vec<u32> = source
            .titles
            .keys()
            .filter(|id| !here.contains_key(id))
            .copied()
            .collect();
        if !missing.is_empty() {
            elsewhere
                .entry(ranges(&missing))
                .or_default()
                .extend(source.names.iter().cloned());
        }
        for (id, title) in &source.titles {
            if here.get(id).is_some_and(|ours| ours != title) {
                other_titles
                    .entry((*id, title))
                    .or_default()
                    .extend(source.names.iter().cloned());
            }
        }
    }
    Report {
        next: highest.map_or(1, |id| id.saturating_add(1)),
        here: highest_here,
        highest_in,
        elsewhere: elsewhere.into_iter().collect(),
        other_titles: other_titles
            .into_iter()
            .map(|((id, title), names)| (id, title.to_owned(), names))
            .collect(),
    }
}

/// The report's lines for standard error.
pub(crate) fn notes(report: &Report, here: &BTreeMap<u32, String>, sequence: Sequence) -> String {
    let (number, numbers, thing) = match sequence {
        Sequence::Decisions => ("id", "ids", "decision"),
        Sequence::Adrs => ("ADR number", "ADR numbers", "file"),
    };
    let mut out = String::new();
    if !report.highest_in.is_empty() {
        let _ = writeln!(
            out,
            "note: the highest {number}, {}, is in {}; the working tree ends at {}",
            sequence.name(report.next.saturating_sub(1)),
            some_of(&report.highest_in),
            report
                .here
                .map_or_else(|| "nothing".to_owned(), |last| sequence.name(last)),
        );
    }
    if !report.elsewhere.is_empty() {
        let _ = writeln!(out, "note: {numbers} that the working tree does not have:");
        for (ranges, names) in &report.elsewhere {
            let ranges: Vec<String> = ranges
                .iter()
                .map(|&(first, last)| {
                    if first == last {
                        sequence.name(first)
                    } else {
                        format!("{}..{}", sequence.name(first), sequence.name(last))
                    }
                })
                .collect();
            let _ = writeln!(out, "  {} in {}", ranges.join(", "), some_of(names));
        }
    }
    if !report.other_titles.is_empty() {
        let _ = writeln!(
            out,
            "warning: {numbers} that are another {thing} elsewhere:"
        );
        for (id, title, names) in &report.other_titles {
            let _ = writeln!(
                out,
                "  {} is \"{title}\" in {}, and \"{}\" in the working tree",
                sequence.name(*id),
                some_of(names),
                here.get(id).map_or("", String::as_str),
            );
        }
    }
    out
}

/// Up to three names, and how many more there are.
fn some_of(names: &[String]) -> String {
    let shown: Vec<&str> = names.iter().take(3).map(String::as_str).collect();
    let more = names.len().saturating_sub(shown.len());
    if more == 0 {
        shown.join(", ")
    } else {
        format!("{} and {more} more", shown.join(", "))
    }
}

/// Sorted ids as inclusive ranges of ids that follow each other.
pub(crate) fn ranges(ids: &[u32]) -> Ranges {
    let mut out = Ranges::new();
    for &id in ids {
        match out.last_mut() {
            Some((_, last)) if last.checked_add(1) == Some(id) => *last = id,
            Some(_) | None => out.push((id, id)),
        }
    }
    out
}

/// What every local branch and remote-tracking ref has at the sequence's
/// path, one source for each distinct object, and what every other worktree
/// has there, which may not be committed. Nothing is fetched.
pub(crate) fn sources(root: &Path, sequence: Sequence) -> Result<Vec<Source>, String> {
    let refs = git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
        ],
        "",
    )?;
    let refs: Vec<&str> = refs
        .lines()
        .filter(|name| !name.ends_with("/HEAD"))
        .collect();
    // One `git cat-file` names each ref's object, and each distinct object
    // is then read once.
    let mut request = String::new();
    for name in &refs {
        let _ = writeln!(request, "{name}:{}", sequence.path());
    }
    let kind = match sequence {
        Sequence::Decisions => "blob",
        Sequence::Adrs => "tree",
    };
    let checked = git(root, &["cat-file", "--batch-check"], &request)?;
    let mut by_object: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (name, line) in refs.iter().zip(checked.lines()) {
        let mut parts = line.split(' ');
        if let (Some(object), Some(found)) = (parts.next(), parts.next())
            && found == kind
        {
            by_object
                .entry(object)
                .or_default()
                .push(short(name).to_owned());
        }
    }
    let mut out = Vec::new();
    match sequence {
        Sequence::Decisions if !by_object.is_empty() => {
            let mut request = String::new();
            for object in by_object.keys() {
                let _ = writeln!(request, "{object}");
            }
            let printed = git_bytes(root, &["cat-file", "--batch"], &request)?;
            for (object, text) in batch(&printed)? {
                if let Some(names) = by_object.remove(object.as_str()) {
                    out.push(Source {
                        names,
                        titles: log::titles(&text),
                    });
                }
            }
        }
        Sequence::Decisions => {}
        Sequence::Adrs => {
            for (object, names) in by_object {
                let files = git(root, &["ls-tree", "--name-only", object], "")?;
                let files: Vec<String> = files.lines().map(str::to_owned).collect();
                out.push(Source {
                    names,
                    titles: adr_numbers(&files),
                });
            }
        }
    }
    let here = root.canonicalize().ok();
    let seen: BTreeSet<_> = here.into_iter().collect();
    let worktrees = git(root, &["worktree", "list", "--porcelain"], "")?;
    for dir in worktrees
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
    {
        // A worktree whose directory is gone has nothing to read.
        let Ok(dir) = Path::new(dir).canonicalize() else {
            continue;
        };
        if seen.contains(&dir) {
            continue;
        }
        if let Ok(titles) = sequence.here(&dir) {
            out.push(Source {
                names: vec![format!("the worktree {}", dir.display())],
                titles,
            });
        }
    }
    Ok(out)
}

/// The names of the files of an ADR directory, sorted.
pub(crate) fn adr_files(dir: &Path) -> Result<Vec<String>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        out.push(entry.file_name().to_string_lossy().into_owned());
    }
    out.sort();
    Ok(out)
}

/// The number an ADR's file name starts with, as `0042-explicit-pipe-holes.md`
/// has 42.
pub(crate) fn adr_number(file: &str) -> Option<u32> {
    let (digits, rest) = file.split_at_checked(4)?;
    let markdown = Path::new(file).extension().is_some_and(|ext| ext == "md");
    (rest.starts_with('-') && markdown && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

/// The ADR files by number; the first name wins when a number has two.
fn adr_numbers(files: &[String]) -> BTreeMap<u32, String> {
    let mut out = BTreeMap::new();
    for file in files {
        if let Some(number) = adr_number(file) {
            out.entry(number).or_insert_with(|| file.clone());
        }
    }
    out
}

/// A ref's name without `refs/heads/` or `refs/remotes/`.
fn short(name: &str) -> &str {
    name.strip_prefix("refs/heads/")
        .or_else(|| name.strip_prefix("refs/remotes/"))
        .unwrap_or(name)
}

/// The objects of `git cat-file --batch`'s output, each as its name and its
/// text: a line `<name> <type> <size>`, then that many bytes and a line
/// break.
pub(crate) fn batch(mut bytes: &[u8]) -> Result<Vec<(String, String)>, String> {
    let broken = || "git cat-file printed something else than objects".to_owned();
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let end = bytes.iter().position(|&b| b == b'\n').ok_or_else(broken)?;
        let (header, rest) = bytes.split_at_checked(end).ok_or_else(broken)?;
        let header = String::from_utf8_lossy(header);
        let mut parts = header.split(' ');
        let name = parts.next().ok_or_else(broken)?;
        let size: usize = parts
            .nth(1)
            .and_then(|size| size.parse().ok())
            .ok_or_else(broken)?;
        let rest = rest.get(1..).ok_or_else(broken)?;
        let (content, rest) = rest.split_at_checked(size).ok_or_else(broken)?;
        out.push((
            name.to_owned(),
            String::from_utf8_lossy(content).into_owned(),
        ));
        bytes = rest.strip_prefix(b"\n").ok_or_else(broken)?;
    }
    Ok(out)
}

fn git(root: &Path, args: &[&str], input: &str) -> Result<String, String> {
    git_bytes(root, args, input).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Runs git in `root` with `input` on its standard input and returns its
/// standard output.
fn git_bytes(root: &Path, args: &[&str], input: &str) -> Result<Vec<u8>, String> {
    let mut child = Command::new("git")
        .current_dir(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run git: {e}"))?;
    let stdin = child.stdin.take();
    // git may fill its output pipe before it has read every request, so the
    // requests are written from a second thread.
    let out = std::thread::scope(|scope| {
        scope.spawn(move || {
            if let Some(mut stdin) = stdin {
                // git may exit before it reads everything; its status says so.
                let _ = stdin.write_all(input.as_bytes());
            }
        });
        child.wait_with_output()
    })
    .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(entries: &[(u32, &str)]) -> BTreeMap<u32, String> {
        entries
            .iter()
            .map(|&(id, title)| (id, title.to_owned()))
            .collect()
    }

    fn source(name: &str, entries: &[(u32, &str)]) -> Source {
        Source {
            names: vec![name.to_owned()],
            titles: titles(entries),
        }
    }

    #[test]
    fn the_working_file_alone() {
        let here = titles(&[(1, "One"), (2, "Two")]);
        let found = report(&here, &[]);
        assert_eq!(found.next, 3);
        assert_eq!(notes(&found, &here, Sequence::Decisions), "");
        assert_eq!(report(&BTreeMap::new(), &[]).next, 1);
    }

    /// Another branch that is further raises the id, and what it holds is
    /// listed; a log that has less than the working file says nothing.
    #[test]
    fn other_logs_raise_the_id() {
        let here = titles(&[(1, "One"), (2, "Two"), (6, "Six")]);
        let others = [
            source("main", &[(1, "One"), (2, "Two")]),
            source(
                "property",
                &[(1, "One"), (3, "Three"), (4, "Four"), (8, "Eight")],
            ),
            source(
                "origin/property",
                &[(1, "One"), (3, "Three"), (4, "Four"), (8, "Eight")],
            ),
            source("old", &[(1, "One"), (2, "Another two")]),
        ];
        let found = report(&here, &others);
        assert_eq!(found.next, 9);
        assert_eq!(found.here, Some(6));
        assert_eq!(found.highest_in, ["property", "origin/property"]);
        assert_eq!(
            notes(&found, &here, Sequence::Decisions),
            "note: the highest id, D-008, is in property, origin/property; the working tree ends \
             at D-006\n\
             note: ids that the working tree does not have:\n\
             \x20 D-003..D-004, D-008 in property, origin/property\n\
             warning: ids that are another decision elsewhere:\n\
             \x20 D-002 is \"Another two\" in old, and \"Two\" in the working tree\n"
        );
    }

    /// ADR numbers are read from file names, and a number that names
    /// another file on another branch is reported.
    #[test]
    fn adr_numbers_and_their_files() {
        assert_eq!(adr_number("0042-explicit-pipe-holes.md"), Some(42));
        for other in [
            "README.md",
            "0042.md",
            "042-short.md",
            "0042-notes.txt",
            "é042-x.md",
        ] {
            assert_eq!(adr_number(other), None, "{other}");
        }
        let files =
            |names: &[&str]| -> Vec<String> { names.iter().map(|&n| n.to_owned()).collect() };
        let here = adr_numbers(&files(&[
            "0001-one.md",
            "0002-two.md",
            "0005-five.md",
            "README.md",
        ]));
        let others = [Source {
            names: vec!["property".to_owned()],
            titles: adr_numbers(&files(&["0001-one.md", "0002-other.md", "0003-three.md"])),
        }];
        let found = report(&here, &others);
        assert_eq!(found.next, 6);
        assert_eq!(
            notes(&found, &here, Sequence::Adrs),
            "note: ADR numbers that the working tree does not have:\n\
             \x20 0003 in property\n\
             warning: ADR numbers that are another file elsewhere:\n\
             \x20 0002 is \"0002-other.md\" in property, and \"0002-two.md\" in the working tree\n"
        );
    }

    #[test]
    fn ids_as_ranges() {
        assert_eq!(ranges(&[1, 2, 3, 7, 9, 10]), [(1, 3), (7, 7), (9, 10)]);
        assert_eq!(ranges(&[]), Ranges::new());
        let names: Vec<String> = ["a", "b", "c", "d", "e"].map(str::to_owned).to_vec();
        assert_eq!(some_of(&names), "a, b, c and 2 more");
    }

    #[test]
    fn reads_the_objects_of_a_batch() {
        let printed = b"abc blob 5\nfirst\ndef blob 12\nsecond\nlines\n";
        assert_eq!(
            batch(printed).unwrap(),
            [
                ("abc".to_owned(), "first".to_owned()),
                ("def".to_owned(), "second\nlines".to_owned()),
            ]
        );
        assert!(batch(b"abc missing\n").is_err());
        assert!(batch(b"abc blob 9\nshort\n").is_err());
    }
}
