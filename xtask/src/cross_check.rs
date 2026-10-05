//! Parser cross-check (D-272, docs/plan/testing.md §5).
//!
//! Parses every example and every UI test with `tree-sitter-cheby` at the
//! commit pinned in `tests/tree-sitter-cheby.rev`. Examples and UI tests
//! must parse, except `parse-fail` UI tests, which must be rejected. Once
//! the compiler parser exists (M1), its verdict is compared too.
//!
//! The grammar is checked out under `target/tree-sitter-cheby`, cloned
//! from a sibling `../tree-sitter-cheby` if there is one and from GitHub
//! otherwise, so a local checkout on another commit is never used by
//! mistake.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

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
}

/// What a file is meant to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    Parse,
    Reject,
}

pub fn run(root: &Path, args: &Args) -> Result<(), String> {
    let rev = std::fs::read_to_string(root.join("tests/tree-sitter-cheby.rev"))
        .map_err(|e| format!("tests/tree-sitter-cheby.rev: {e}"))?
        .trim()
        .to_owned();
    let grammar = match &args.grammar {
        Some(dir) => dir.clone(),
        None => checkout(root, &rev)?,
    };
    let cli = find_cli(root, args.tree_sitter.as_deref())?;

    let mut files = collect(root)?;
    for dir in &args.also {
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
    let verdicts = parse(&cli, &grammar, root, &files)?;

    let mut mismatches = Vec::new();
    for (path, intent) in &files {
        let parsed = *verdicts.get(path).ok_or_else(|| {
            format!(
                "tree-sitter reported nothing for {}",
                display_relative(path, root)
            )
        })?;
        let ok = match intent {
            Intent::Parse => parsed,
            Intent::Reject => !parsed,
        };
        if !ok {
            let what = match intent {
                Intent::Parse => "must parse, but tree-sitter rejects it",
                Intent::Reject => "is `parse-fail`, but tree-sitter accepts it",
            };
            mismatches.push(format!("{}: {what}", display_relative(path, root)));
        }
    }

    let short_rev = rev.get(..7).unwrap_or(&rev);
    let line = format!(
        "tree-sitter-cheby {short_rev}: {} files, {} as expected, {} mismatches \
         (compiler parser: not before M1)",
        files.len(),
        files.len().saturating_sub(mismatches.len()),
        mismatches.len()
    );
    println!("{line}");
    for m in &mismatches {
        println!("  {m}");
    }
    if let Some(summary) = &args.summary {
        let mut text = format!("- {line}\n");
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

/// Every file to check, with what it is meant to do, sorted by path.
fn collect(root: &Path) -> Result<BTreeMap<PathBuf, Intent>, String> {
    let mut files = BTreeMap::new();
    for ex in example::all(root)? {
        for source in ex.sources()? {
            files.insert(source, Intent::Parse);
        }
    }
    for test in ui::all(root)? {
        let intent = if test.header.mode.is_syntax_error() {
            Intent::Reject
        } else {
            Intent::Parse
        };
        files.insert(test.path, intent);
    }
    Ok(files)
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
    if !dir.exists() {
        let sibling = root.join("../tree-sitter-cheby");
        let source = if sibling.join(".git").exists() {
            sibling.to_string_lossy().into_owned()
        } else {
            UPSTREAM.to_owned()
        };
        println!("cloning tree-sitter-cheby from {source}");
        git(root, &["clone", "--quiet", &source, &dir.to_string_lossy()])?;
    }
    let has_rev = git(&dir, &["cat-file", "-e", &format!("{rev}^{{commit}}")]).is_ok();
    if !has_rev {
        // The clone may come from a sibling that lacks the commit.
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
