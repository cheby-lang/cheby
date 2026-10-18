//! Parser cross-check (D-272, docs/plan/testing.md §5).
//!
//! Parses every example and every UI test with `tree-sitter-cheby` at the
//! commit pinned in `tests/tree-sitter-cheby.rev` and with the compiler
//! parser (`cheby_syntax`). Examples and UI tests must parse, except
//! `parse-fail` UI tests, which must be rejected. Every `.cheby` file of a
//! package UI test, a directory with its own `cheby.toml`, must parse,
//! since a package test cannot be `parse-fail`. Both parsers are held to
//! that intent, so the check fails whenever they disagree on a file. The
//! standard library's sources (`std/**/*.cheby`) and the programs of the
//! runtime benchmarks (`bench/runtime/**/*.cheby`) must parse too.
//!
//! REPL transcripts (`tests/repl/**/*.repl`, plan §8.3, D-546): each input
//! entry (a `> ` line and its `. ` continuation lines; the expression of
//! `:type`; other commands are skipped) is parsed as items, as a module
//! would hold them, and as statements, inside a function body, which
//! covers expressions. The two parsers must agree on whether some form
//! parses. The `.cheby` files of REPL package directories must parse.
//!
//! The code of the stdlib spec (D-662): every ` ```cheby ` block of
//! `docs/stdlib/**/*.md` is parsed as a module's items (declarations,
//! signatures without a body, `test` blocks and example functions), and
//! both parsers must accept it. A failure names the block by the line of
//! its first code line, `docs/stdlib/<module>.md:<line>`. Blocks are read
//! as `CommonMark` reads fences, so a block shown inside a longer fence,
//! such as the ` ````markdown ` sample of `docs/stdlib/README.md`, is that
//! fence's text, not a block of its own.
//!
//! The compiler rejects a file if it is not UTF-8 or if `parse` reports a
//! grammar error. Validation errors (class V) are not consulted: they are
//! rules that tree-sitter does not enforce, tested by `check-fail` UI
//! tests (D-424). `--compiler-only` runs without a tree-sitter CLI, for
//! machines that have none; CI runs both.
//!
//! The grammar is checked out under `target/tree-sitter-cheby`, cloned
//! from a sibling `../tree-sitter-cheby` if there is one and from GitHub
//! otherwise, so a local checkout on another commit is never used by
//! mistake. A pinned commit the checkout lacks is fetched from the
//! sibling's branches first, then from GitHub.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use cheby_diag::{FileId, SourceMap};
use cheby_test_support::{display_relative, example, ui};

const UPSTREAM: &str = "https://github.com/cheby-lang/tree-sitter-cheby.git";

#[derive(Debug, clap::Args)]
pub struct Args {
    /// The `tree-sitter` CLI to use. Defaults to `tree-sitter` on PATH,
    /// then to `node_modules/.bin/tree-sitter` of a sibling checkout.
    #[arg(long)]
    tree_sitter: Option<PathBuf>,
    /// Use this grammar directory as is, instead of the pinned commit
    #[arg(long)]
    grammar: Option<PathBuf>,
    /// Also require every `.cheby` file under this directory to parse,
    /// such as a generated benchmark project
    #[arg(long)]
    also: Vec<PathBuf>,
    /// Append a Markdown summary to this file, such as `$GITHUB_STEP_SUMMARY`
    #[arg(long)]
    summary: Option<PathBuf>,
    /// Check only the compiler parser, without tree-sitter
    #[arg(long)]
    compiler_only: bool,
}

/// What a file is meant to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    Parse,
    Reject,
    /// One form of a REPL entry: judged with the entry's other form.
    ReplForm,
    /// A ` ```cheby ` block of `docs/stdlib`, judged by [`judge_stdlib`].
    StdlibBlock,
}

/// One ` ```cheby ` block of `docs/stdlib`, written out as a module.
struct StdlibBlock {
    /// `docs/stdlib/<module>.md:<line>`, the line of the block's first
    /// code line.
    shown: String,
    /// The line of the page the block's first line is on.
    first_line: usize,
    /// The block's code, written out under `target/cross-check-stdlib`.
    path: PathBuf,
}

/// One input entry of a REPL transcript, written out in both forms.
struct ReplEntry {
    /// `tests/repl/<name>.repl:<line>`.
    shown: String,
    /// The entry as items, and as statements in a function body.
    forms: [PathBuf; 2],
}

pub fn run(root: &Path, args: &Args) -> Result<(), String> {
    let rev = std::fs::read_to_string(root.join("tests/tree-sitter-cheby.rev"))
        .map_err(|e| format!("tests/tree-sitter-cheby.rev: {e}"))?
        .trim()
        .to_owned();

    let mut files = collect(root)?;
    let entries = repl_entries(root)?;
    for entry in &entries {
        for form in &entry.forms {
            files.insert(form.clone(), Intent::ReplForm);
        }
    }
    let blocks = stdlib_blocks(root)?;
    for block in &blocks {
        files.insert(block.path.clone(), Intent::StdlibBlock);
    }
    add_also(&args.also, &mut files)?;

    let tree_sitter = if args.compiler_only {
        None
    } else {
        let grammar = match &args.grammar {
            Some(dir) => dir.clone(),
            None => checkout(root, &rev)?,
        };
        let cli = find_cli(root, args.tree_sitter.as_deref())?;
        Some(parse(&cli, &grammar, root, &files)?)
    };

    let mut mismatches = Vec::new();
    let (ts_ok, compiler_ok) = judge_files(root, &files, tree_sitter.as_ref(), &mut mismatches)?;
    let repl_ok = judge_repl(&entries, tree_sitter.as_ref(), &mut mismatches)?;
    let stdlib_ok = judge_stdlib(&blocks, tree_sitter.as_ref(), &mut mismatches)?;

    let short_rev = rev.get(..7).unwrap_or(&rev);
    let ts_part = if tree_sitter.is_some() {
        format!("tree-sitter-cheby {short_rev}: {ts_ok} as expected")
    } else {
        "tree-sitter-cheby: skipped (--compiler-only)".to_owned()
    };
    let line = format!(
        "parser cross-check: {} files and {} REPL inputs; {ts_part}; compiler parser: \
         {compiler_ok} as expected; REPL inputs: {repl_ok} as expected; {} mismatches",
        files
            .values()
            .filter(|&&intent| matches!(intent, Intent::Parse | Intent::Reject))
            .count(),
        entries.len(),
        mismatches.len()
    );
    let parsers = if tree_sitter.is_some() {
        "both parsers"
    } else {
        "the compiler parser"
    };
    let stdlib_line = format!(
        "docs/stdlib: {} blocks; {stdlib_ok} accepted by {parsers}",
        blocks.len()
    );
    println!("{line}");
    println!("{stdlib_line}");
    for m in &mismatches {
        println!("  {m}");
    }
    if let Some(summary) = &args.summary {
        let mut text = format!("- {line}\n- {stdlib_line}\n");
        for m in &mismatches {
            let _ = writeln!(text, "  - {m}");
        }
        super::append(summary, &text)?;
    }
    if mismatches.is_empty() {
        Ok(())
    } else {
        Err("parser cross-check failed".to_owned())
    }
}

/// Judges the files that must parse or must be rejected (not the REPL
/// forms and stdlib blocks, which have their own judges) and returns how
/// many tree-sitter and the compiler parser treat as expected.
fn judge_files(
    root: &Path,
    files: &BTreeMap<PathBuf, Intent>,
    tree_sitter: Option<&BTreeMap<PathBuf, bool>>,
    mismatches: &mut Vec<String>,
) -> Result<(usize, usize), String> {
    let mut ts_ok: usize = 0;
    let mut compiler_ok: usize = 0;
    for (path, intent) in files {
        if matches!(intent, Intent::ReplForm | Intent::StdlibBlock) {
            continue;
        }
        let shown = display_relative(path, root);
        if let Some(verdicts) = tree_sitter {
            let parsed = *verdicts
                .get(path)
                .ok_or_else(|| format!("tree-sitter reported nothing for {shown}"))?;
            match (intent, parsed) {
                (Intent::Parse, true) | (Intent::Reject, false) => {
                    ts_ok = ts_ok.saturating_add(1);
                }
                (Intent::Parse, false) => {
                    mismatches.push(format!("{shown}: must parse, but tree-sitter rejects it"));
                }
                (Intent::Reject, true) => mismatches.push(format!(
                    "{shown}: is `parse-fail`, but tree-sitter accepts it"
                )),
                (Intent::ReplForm | Intent::StdlibBlock, _) => {}
            }
        }
        match (intent, compiler_verdict(path, 0)?) {
            (Intent::Parse, None) | (Intent::Reject, Some(_)) => {
                compiler_ok = compiler_ok.saturating_add(1);
            }
            (Intent::Parse, Some(error)) => mismatches.push(format!(
                "{shown}: must parse, but the compiler parser rejects it: {error}"
            )),
            (Intent::Reject, None) => mismatches.push(format!(
                "{shown}: is `parse-fail`, but the compiler parser accepts it"
            )),
            (Intent::ReplForm | Intent::StdlibBlock, _) => {}
        }
    }
    Ok((ts_ok, compiler_ok))
}

/// The compiler parser's verdict on a file: `None` if it parses, or the
/// first grammar error as `line:col: message`, with `line_offset` added to
/// the line (for a block taken from a longer file).
fn compiler_verdict(path: &Path, line_offset: usize) -> Result<Option<String>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(Some("the file is not valid UTF-8".to_owned()));
    };
    let mut sources = SourceMap::new();
    let file = sources
        .add_file("", text.as_str())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    debug_assert_eq!(file, FileId::new(0));
    let parse = cheby_syntax::parse(file, text.into());
    Ok(parse.errors().first().map(|error| {
        let at = error
            .primary_span()
            .and_then(|span| sources.location(span))
            .map(|loc| {
                let line = usize::try_from(loc.line)
                    .unwrap_or(usize::MAX)
                    .saturating_add(line_offset);
                format!("{line}:{}: ", loc.col)
            })
            .unwrap_or_default();
        format!("{at}error[{}]: {}", error.code, error.message)
    }))
}

/// Every file to check, with what it is meant to do, sorted by path.
fn collect(root: &Path) -> Result<BTreeMap<PathBuf, Intent>, String> {
    let mut files = BTreeMap::new();
    for ex in example::all(root)? {
        for source in ex.sources()? {
            files.insert(source, Intent::Parse);
        }
    }
    // The standard library's sources, the `.cheby` files of REPL package
    // directories and the runtime benchmarks' programs must parse.
    for dir in [
        root.join("std"),
        root.join("tests/repl"),
        root.join("bench/runtime"),
    ] {
        if dir.exists() {
            for source in cheby_test_support::files_with_extension(&dir, "cheby")
                .map_err(|e| format!("{}: {e}", dir.display()))?
            {
                files.insert(source, Intent::Parse);
            }
        }
    }
    for test in ui::all(root)? {
        // `ui::all` refuses a `parse-fail` package test, so only a
        // single-file test can be meant as a syntax error.
        let intent = if test.header.mode.is_syntax_error() {
            Intent::Reject
        } else {
            Intent::Parse
        };
        for source in test.sources()? {
            files.insert(source, intent);
        }
    }
    Ok(files)
}

/// Adds every `.cheby` file under the `--also` directories, which must
/// parse.
fn add_also(dirs: &[PathBuf], files: &mut BTreeMap<PathBuf, Intent>) -> Result<(), String> {
    for dir in dirs {
        let extra = cheby_test_support::files_with_extension(dir, "cheby")
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        if extra.is_empty() {
            return Err(format!("{} holds no .cheby files", dir.display()));
        }
        for path in extra {
            files.insert(
                std::path::absolute(&path).map_err(|e| e.to_string())?,
                Intent::Parse,
            );
        }
    }
    Ok(())
}

/// Compares the parsers on each REPL entry and returns how many agree. An
/// input both parsers reject is a syntax error on purpose, whose
/// diagnostic the transcript shows; only a disagreement is a mismatch.
fn judge_repl(
    entries: &[ReplEntry],
    tree_sitter: Option<&BTreeMap<PathBuf, bool>>,
    mismatches: &mut Vec<String>,
) -> Result<usize, String> {
    let mut agree: usize = 0;
    for entry in entries {
        let mut compiler = false;
        let mut ts = false;
        for form in &entry.forms {
            compiler |= compiler_verdict(form, 0)?.is_none();
            if let Some(verdicts) = tree_sitter {
                ts |= verdicts.get(form).copied().ok_or_else(|| {
                    format!("tree-sitter reported nothing for {}", form.display())
                })?;
            }
        }
        if tree_sitter.is_none() || compiler == ts {
            agree = agree.saturating_add(1);
        } else if compiler {
            mismatches.push(format!(
                "{}: the compiler parser accepts the input, but tree-sitter rejects it",
                entry.shown
            ));
        } else {
            mismatches.push(format!(
                "{}: tree-sitter accepts the input, but the compiler parser rejects it",
                entry.shown
            ));
        }
    }
    Ok(agree)
}

/// Checks that both parsers accept each block of `docs/stdlib` (D-662)
/// and returns how many they do.
fn judge_stdlib(
    blocks: &[StdlibBlock],
    tree_sitter: Option<&BTreeMap<PathBuf, bool>>,
    mismatches: &mut Vec<String>,
) -> Result<usize, String> {
    let mut accepted: usize = 0;
    for block in blocks {
        let mut ok = true;
        if let Some(verdicts) = tree_sitter {
            let parsed = verdicts.get(&block.path).copied().ok_or_else(|| {
                format!("tree-sitter reported nothing for {}", block.path.display())
            })?;
            if !parsed {
                ok = false;
                mismatches.push(format!(
                    "{}: tree-sitter rejects the ```cheby block",
                    block.shown
                ));
            }
        }
        // The page's line of the block's first line is 1 in the block.
        let offset = block.first_line.saturating_sub(1);
        if let Some(error) = compiler_verdict(&block.path, offset)? {
            ok = false;
            mismatches.push(format!(
                "{}: the compiler parser rejects the ```cheby block: {error}",
                block.shown
            ));
        }
        if ok {
            accepted = accepted.saturating_add(1);
        }
    }
    Ok(accepted)
}

/// Every ` ```cheby ` block of `docs/stdlib/**/*.md`, in path order, each
/// written to `target/cross-check-stdlib` as a module (D-662).
fn stdlib_blocks(root: &Path) -> Result<Vec<StdlibBlock>, String> {
    let dir = root.join("docs/stdlib");
    let pages = cheby_test_support::files_with_extension(&dir, "md")
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let out = root.join("target/cross-check-stdlib");
    if out.exists() {
        std::fs::remove_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut blocks = Vec::new();
    for page in &pages {
        let text = std::fs::read_to_string(page).map_err(|e| format!("{}: {e}", page.display()))?;
        let shown = display_relative(page, root);
        for (first_line, code) in cheby_blocks(&text) {
            let path = out.join(format!("{}.cheby", blocks.len()));
            std::fs::write(&path, code).map_err(|e| format!("{}: {e}", path.display()))?;
            blocks.push(StdlibBlock {
                shown: format!("{shown}:{first_line}"),
                first_line,
                path,
            });
        }
    }
    Ok(blocks)
}

/// The fenced code blocks of a Markdown text whose info string is `cheby`,
/// each with the line of its first code line. Fences follow `CommonMark`: a
/// line indented at most three spaces that starts with three or more
/// backticks or tildes opens a block, and a line of the same character, at
/// least as long, with nothing after it, closes it, so a shorter fence
/// inside a longer one is text. An unclosed block runs to the end of the
/// text. The opening fence's indentation is removed from the code lines,
/// up to its width.
fn cheby_blocks(text: &str) -> Vec<(usize, String)> {
    /// A fence: its indentation, character and length, and the rest of the
    /// line after it.
    fn fence(line: &str) -> Option<(usize, char, usize, &str)> {
        let rest = line.trim_start_matches(' ');
        let indent = line.len().saturating_sub(rest.len());
        let c = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
        let after = rest.trim_start_matches(c);
        let len = rest.len().saturating_sub(after.len());
        (indent <= 3 && len >= 3).then_some((indent, c, len, after))
    }
    struct Open {
        indent: usize,
        c: char,
        len: usize,
        cheby: bool,
        first_line: usize,
        code: String,
    }
    let mut blocks = Vec::new();
    let mut open: Option<Open> = None;
    for (n, line) in text.lines().enumerate() {
        let number = n.saturating_add(1);
        match (&mut open, fence(line)) {
            (None, Some((indent, c, len, info))) => {
                // A backtick fence's info string has no backtick.
                if c == '`' && info.contains('`') {
                    continue;
                }
                let cheby = info.split_whitespace().next() == Some("cheby");
                open = Some(Open {
                    indent,
                    c,
                    len,
                    cheby,
                    first_line: number.saturating_add(1),
                    code: String::new(),
                });
            }
            (Some(block), Some((_, c, len, after)))
                if c == block.c && len >= block.len && after.trim().is_empty() =>
            {
                if block.cheby {
                    blocks.push((block.first_line, std::mem::take(&mut block.code)));
                }
                open = None;
            }
            (Some(block), _) => {
                let spaces = line
                    .len()
                    .saturating_sub(line.trim_start_matches(' ').len());
                let strip = spaces.min(block.indent);
                block.code.push_str(line.get(strip..).unwrap_or(line));
                block.code.push('\n');
            }
            (None, None) => {}
        }
    }
    if let Some(block) = open
        && block.cheby
    {
        blocks.push((block.first_line, block.code));
    }
    blocks
}

/// The input entries of every REPL transcript under `tests/repl`, each
/// written to `target/cross-check-repl` as items and as statements, and
/// the `.cheby` files of REPL package directories added to `files`.
fn repl_entries(root: &Path) -> Result<Vec<ReplEntry>, String> {
    let dir = root.join("tests/repl");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let out = root.join("target/cross-check-repl");
    if out.exists() {
        std::fs::remove_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    let transcripts = cheby_test_support::files_with_extension(&dir, "repl")
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut entries = Vec::new();
    for (n, transcript) in transcripts.iter().enumerate() {
        let text = std::fs::read_to_string(transcript)
            .map_err(|e| format!("{}: {e}", transcript.display()))?;
        let shown = display_relative(transcript, root);
        for (k, (line, input)) in transcript_inputs(&text).into_iter().enumerate() {
            let base = out.join(format!("{n}-{k}"));
            let forms = [
                base.with_extension("item.cheby"),
                base.with_extension("stmt.cheby"),
            ];
            let [item, stmt] = &forms;
            std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
            std::fs::write(item, format!("{input}\n"))
                .map_err(|e| format!("{}: {e}", item.display()))?;
            std::fs::write(stmt, format!("fn repl_entry() {{\n{input}\n}}\n"))
                .map_err(|e| format!("{}: {e}", stmt.display()))?;
            entries.push(ReplEntry {
                shown: format!("{shown}:{line}"),
                forms,
            });
        }
    }
    Ok(entries)
}

/// The Cheby inputs of a transcript with the line each starts on: a `> `
/// line and the `. ` lines after it. Of the REPL commands only `:type`
/// holds Cheby, its expression.
fn transcript_inputs(text: &str) -> Vec<(usize, String)> {
    let mut inputs: Vec<(usize, String)> = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (n, line) in text.lines().enumerate() {
        let first = line.strip_prefix("> ").or((line == ">").then_some(""));
        let more = line.strip_prefix(". ").or((line == ".").then_some(""));
        if let Some(rest) = first {
            inputs.extend(current.take());
            current = Some((n.saturating_add(1), rest.to_owned()));
        } else if let (Some(rest), Some((_, entry))) = (more, current.as_mut()) {
            entry.push('\n');
            entry.push_str(rest);
        } else {
            inputs.extend(current.take());
        }
    }
    inputs.extend(current);
    inputs
        .into_iter()
        .filter_map(|(line, input)| {
            let trimmed = input.trim_start();
            if let Some(command) = trimmed.strip_prefix(':') {
                let expr = command.strip_prefix("type")?.trim_start();
                return (!expr.is_empty()).then(|| (line, expr.to_owned()));
            }
            (!trimmed.is_empty()).then_some((line, input))
        })
        .collect()
}

/// Clones the grammar at `rev` into `target/tree-sitter-cheby`, or reuses
/// the checkout if it is already there.
///
/// Every git command runs with `GIT_CEILING_DIRECTORIES` set to `target/`,
/// so git never looks for a repository above the checkout. Without it, a
/// checkout whose `.git` is incomplete, as a CI cache can restore it, makes
/// git fall back to the `cheby` repository itself, and checking out the
/// grammar's commit there replaces this repository's files.
fn checkout(root: &Path, rev: &str) -> Result<PathBuf, String> {
    let ceiling = root.join("target");
    let dir = ceiling.join("tree-sitter-cheby");
    let git = |cwd: &Path, args: &[&str]| git(cwd, &ceiling, args);
    if dir.exists() && git(&dir, &["rev-parse", "--git-dir"]).is_err() {
        println!("removing a broken checkout in {}", dir.display());
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let sibling = root.join("../tree-sitter-cheby");
    let sibling = sibling
        .join(".git")
        .exists()
        .then(|| sibling.to_string_lossy().into_owned());
    if !dir.exists() {
        let source = sibling.as_deref().unwrap_or(UPSTREAM);
        println!("cloning tree-sitter-cheby from {source}");
        git(root, &["clone", "--quiet", source, &dir.to_string_lossy()])?;
    }
    let has_rev = |dir: &Path| git(dir, &["cat-file", "-e", &format!("{rev}^{{commit}}")]).is_ok();
    // An earlier checkout, or a clone of a sibling, may lack the commit:
    // fetch every branch of the sibling, where a pinned commit is made
    // first, then the commit itself from GitHub.
    if !has_rev(&dir)
        && let Some(sibling) = &sibling
    {
        git(
            &dir,
            &[
                "fetch",
                "--quiet",
                sibling,
                "+refs/heads/*:refs/remotes/sibling/*",
            ],
        )?;
    }
    if !has_rev(&dir) {
        git(&dir, &["fetch", "--quiet", UPSTREAM, rev])?;
    }
    git(&dir, &["checkout", "--quiet", "--detach", rev])?;
    Ok(dir)
}

fn git(dir: &Path, ceiling: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CEILING_DIRECTORIES", ceiling)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn find_cli(root: &Path, explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    let on_path = Command::new("tree-sitter").arg("--version").output();
    if on_path.is_ok_and(|o| o.status.success()) {
        return Ok(PathBuf::from("tree-sitter"));
    }
    let bin = if cfg!(windows) {
        "tree-sitter.cmd"
    } else {
        "tree-sitter"
    };
    let sibling = root
        .join("../tree-sitter-cheby/node_modules/.bin")
        .join(bin);
    if sibling.exists() {
        return Ok(sibling);
    }
    Err(
        "no tree-sitter CLI found; install it with `npm install -g tree-sitter-cli` \
         or pass --tree-sitter"
            .to_owned(),
    )
}

/// Runs `tree-sitter parse` on every file and returns which ones parsed.
fn parse(
    cli: &Path,
    grammar: &Path,
    root: &Path,
    files: &BTreeMap<PathBuf, Intent>,
) -> Result<BTreeMap<PathBuf, bool>, String> {
    let list = root.join("target/cross-check-paths.txt");
    let paths: Vec<String> = files
        .keys()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    std::fs::write(&list, paths.join("\n") + "\n")
        .map_err(|e| format!("{}: {e}", list.display()))?;

    let out = Command::new(cli)
        .args(["parse", "--quiet", "--json-summary", "--grammar-path"])
        .arg(grammar)
        .arg("--paths")
        .arg(&list)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", cli.display()))?;
    // tree-sitter exits non-zero when any file fails to parse, and prints
    // one line per failure before the JSON summary.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout
        .find('{')
        .and_then(|i| stdout.get(i..))
        .ok_or_else(|| {
            format!(
                "tree-sitter printed no summary ({})\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            )
        })?;
    let summary: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("cannot read tree-sitter's summary: {e}"))?;
    let entries = summary
        .get("parse_summaries")
        .and_then(serde_json::Value::as_array)
        .ok_or("tree-sitter's summary has no `parse_summaries`")?;
    entries
        .iter()
        .map(|entry| {
            let file = entry
                .get("file")
                .and_then(serde_json::Value::as_str)
                .ok_or("summary entry without `file`")?;
            let ok = entry
                .get("successful")
                .and_then(serde_json::Value::as_bool)
                .ok_or("summary entry without `successful`")?;
            Ok((PathBuf::from(file), ok))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{cheby_blocks, transcript_inputs};

    #[test]
    fn cheby_blocks_follow_commonmark_fences() {
        let text = "# `std::list`\n\
                    \n\
                    ```cheby\n\
                    pub fn get<T>(xs: List<T>, index: Int) -> Option<T>\n\
                    ```\n\
                    \n\
                    ```text\n\
                    not cheby\n\
                    ```\n\
                    ````markdown\n\
                    ```cheby\n\
                    shown inside a longer fence\n\
                    ```\n\
                    ````\n\
                    \x20 ```cheby title\n\
                    \x20 test \"a\" {\n\
                    \x20   assert True\n\
                    \x20 }\n\
                    \x20 ```\n\
                    ~~~cheby\n\
                    fn tilde() {}\n\
                    ~~~~\n\
                    ```cheby\n\
                    fn unclosed() {}\n";
        assert_eq!(
            cheby_blocks(text),
            [
                (
                    4,
                    "pub fn get<T>(xs: List<T>, index: Int) -> Option<T>\n".to_owned()
                ),
                (16, "test \"a\" {\n  assert True\n}\n".to_owned()),
                (21, "fn tilde() {}\n".to_owned()),
                (24, "fn unclosed() {}\n".to_owned()),
            ]
        );
    }

    #[test]
    fn transcript_inputs_are_entries_with_continuations() {
        let text = "> let x = 1\n\
                    > fn double(n: Int) -> Int {\n\
                    .   n * 2\n\
                    . }\n\
                    > double(x)\n\
                    2 : Int\n\
                    > :type double\n\
                    double : fn(Int) -> Int\n\
                    > :help\n\
                    commands: ...\n\
                    >\n\
                    > io::read_line()\n\
                    a line read by the entry\n";
        let inputs = transcript_inputs(text);
        assert_eq!(
            inputs,
            [
                (1, "let x = 1".to_owned()),
                (2, "fn double(n: Int) -> Int {\n  n * 2\n}".to_owned()),
                (5, "double(x)".to_owned()),
                (7, "double".to_owned()),
                (12, "io::read_line()".to_owned()),
            ]
        );
    }
}
