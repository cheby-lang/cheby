//! `cargo xtask affected`: the checks of CI that a set of changed files can
//! fail, as commands, and with `--run` their run (D-1948).
//!
//! The changed files are the uncommitted ones, staged, unstaged and
//! untracked; with `--base REF`, every file changed since the merge base
//! with `REF`; or the paths given as arguments. [`rules`] maps them to
//! nextest filters, std modules and the other checks, with the workspace's
//! dependency graph from `cargo metadata` ([`graph`]).
//!
//! The plan is printed with, under each command, the files that ask for it.
//! `--run` runs the commands in order, each with its output in a file of
//! `target/affected/`, goes on after a failure, and fails when one failed.
//! A tool that is not installed is skipped and named.

mod graph;
mod rules;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use self::graph::Workspace;
use self::rules::{Lookup, Plan, Step};

/// How many files a command's line of reasons names before `and N more`.
const SHOWN: usize = 4;

/// How many changed files are listed before `… and N more`.
const LISTED: usize = 12;

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Changed paths, from the repository root; without any, the uncommitted changes
    paths: Vec<String>,
    /// Take every file changed since the merge base with this ref, uncommitted changes included
    #[arg(long, value_name = "REF", conflicts_with = "paths")]
    base: Option<String>,
    /// Run the commands, each with its output in a file under `target/affected/`
    #[arg(long)]
    run: bool,
}

pub(crate) fn run(root: &Path, args: &Args) -> Result<(), String> {
    let (paths, diff_from) = if args.paths.is_empty() {
        let (paths, from) = changed(root, args.base.as_deref())?;
        (paths, Some(from))
    } else {
        let paths: BTreeSet<String> = args.paths.iter().map(|path| normal(root, path)).collect();
        (paths.into_iter().collect(), None)
    };
    if paths.is_empty() {
        println!("no file has changed");
        return Ok(());
    }
    let workspace = Workspace::load(root)?;
    let checkout = Checkout { root, diff_from };
    let plan = rules::plan(&paths, &workspace, &checkout);
    println!("{}:", count(paths.len(), "changed file"));
    for path in paths.iter().take(LISTED) {
        println!("  {path}");
    }
    if paths.len() > LISTED {
        println!("  … and {} more", paths.len().saturating_sub(LISTED));
    }
    if !plan.unmatched.is_empty() {
        println!(
            "\nno rule knows {}, so no check is planned for them:",
            count(plan.unmatched.len(), "of these files").replacen(" of these files", "", 1)
        );
        for path in &plan.unmatched {
            println!("  {path}");
        }
    }
    println!();
    let steps = plan.steps();
    if args.run {
        return run_steps(root, &steps);
    }
    print!("{}", listed(&plan, &steps, paths.len()));
    Ok(())
}

/// `3 changed files`, or `1 changed file`.
fn count(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

/// The plan's commands, each with the files that ask for it, unless all
/// `changed` files do.
fn listed(plan: &Plan, steps: &[Step], changed: usize) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (index, step) in steps.iter().enumerate() {
        let _ = writeln!(out, "{}", step.shown());
        // The commands of one check, such as `test-std` of two modules,
        // share their files, which follow the last of them.
        let next_is_same = steps
            .get(index.saturating_add(1))
            .is_some_and(|next| next.check == step.check);
        if next_is_same {
            continue;
        }
        if let Some(paths) = plan
            .why
            .get(&step.check)
            .filter(|paths| paths.len() < changed)
        {
            let shown: Vec<&str> = paths.iter().take(SHOWN).map(String::as_str).collect();
            let more = paths.len().saturating_sub(shown.len());
            let tail = if more == 0 {
                String::new()
            } else {
                format!(" and {more} more")
            };
            let _ = writeln!(out, "    # {}{tail}", shown.join(", "));
        }
    }
    out
}

/// `path` from the repository root with `/`, as git writes paths.
fn normal(root: &Path, path: &str) -> String {
    let given = Path::new(path);
    let relative = if given.is_absolute() {
        given.strip_prefix(root).unwrap_or(given)
    } else {
        given
    };
    let text = cheby_test_support::display_relative(relative, Path::new(""));
    text.trim_start_matches("./").replace('\\', "/")
}

/// The files that differ from `base`'s merge base with `HEAD`, or from
/// `HEAD` without a base, in the index or the working tree, and the
/// untracked files git does not ignore; with the commit they are compared
/// with.
fn changed(root: &Path, base: Option<&str>) -> Result<(Vec<String>, String), String> {
    let from = match base {
        Some(base) => git(root, &["merge-base", base, "HEAD"])?.trim().to_owned(),
        None => "HEAD".to_owned(),
    };
    let split = |text: String| -> Vec<String> {
        text.split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let mut paths: BTreeSet<String> = split(git(
        root,
        &["diff", "--name-only", "--no-renames", "-z", &from],
    )?)
    .into_iter()
    .collect();
    paths.extend(split(git(
        root,
        &["ls-files", "-z", "--others", "--exclude-standard"],
    )?));
    Ok((paths.into_iter().collect(), from))
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The working tree, as the rules ask about it.
struct Checkout<'a> {
    root: &'a Path,
    /// The commit the changes are compared with; `None` for paths given as
    /// arguments, whose changes are not known.
    diff_from: Option<String>,
}

impl Lookup for Checkout<'_> {
    fn exists(&self, path: &str) -> bool {
        self.root.join(path).is_file()
    }

    fn naming(&self, dir: &str, extensions: &[&str], needle: &str) -> Vec<String> {
        let mut found = Vec::new();
        for extension in extensions {
            let files = cheby_test_support::files_with_extension(&self.root.join(dir), extension)
                .unwrap_or_default();
            for file in files {
                let names = std::fs::read(&file)
                    .is_ok_and(|bytes| String::from_utf8_lossy(&bytes).contains(needle));
                if names {
                    found.push(cheby_test_support::display_relative(&file, self.root));
                }
            }
        }
        found.sort();
        found
    }

    fn changed_line_has(&self, path: &str, needle: &str) -> bool {
        let whole_file = || {
            std::fs::read(self.root.join(path))
                .is_ok_and(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
        };
        let Some(from) = &self.diff_from else {
            return whole_file();
        };
        match git(self.root, &["diff", "-U0", from, "--", path]) {
            // An untracked file has no diff: all of it is new.
            Ok(diff) if diff.is_empty() => whole_file(),
            Ok(diff) => diff.lines().any(|line| {
                (line.starts_with('+') || line.starts_with('-'))
                    && !line.starts_with("+++")
                    && !line.starts_with("---")
                    && line.contains(needle)
            }),
            Err(_) => whole_file(),
        }
    }

    fn std_sources(&self) -> BTreeMap<String, String> {
        let dir = self.root.join("std");
        cheby_test_support::files_with_extension(&dir, "cheby")
            .unwrap_or_default()
            .into_iter()
            .filter_map(|file| {
                let relative = cheby_test_support::display_relative(&file, &dir);
                let module = relative.strip_suffix(".cheby")?.replace('/', "::");
                Some((module, std::fs::read_to_string(&file).ok()?))
            })
            .collect()
    }
}

/// Runs `steps` in order, each with its output in a file, and prints a
/// line for each. Fails when a command failed.
fn run_steps(root: &Path, steps: &[Step]) -> Result<(), String> {
    let logs = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| root.join("target"), PathBuf::from)
        .join("affected");
    std::fs::create_dir_all(&logs).map_err(|e| format!("{}: {e}", logs.display()))?;
    let mut failed = 0_usize;
    for (index, step) in steps.iter().enumerate() {
        let name = format!("{:02}-{:?}.log", index.saturating_add(1), step.check).to_lowercase();
        let log = logs.join(name);
        let started = Instant::now();
        let outcome = run_step(root, step, &log)?;
        let seconds = started.elapsed().as_secs_f64();
        match outcome {
            Outcome::Passed => println!("ok    {seconds:>6.1}s  {}", step.shown()),
            Outcome::Failed => {
                failed = failed.saturating_add(1);
                println!("FAIL  {seconds:>6.1}s  {}", step.shown());
                println!("               its output: {}", log.display());
            }
            Outcome::Missing => println!(
                "skip           {}: `{}` is not installed",
                step.shown(),
                step.program
            ),
        }
    }
    if failed == 0 {
        Ok(())
    } else {
        Err(format!(
            "{} failed; the output of each is in {}",
            count(failed, "command"),
            logs.display()
        ))
    }
}

enum Outcome {
    Passed,
    Failed,
    /// The program is not installed.
    Missing,
}

/// Runs one command with its standard output and standard error in `log`.
/// A `cargo xtask` command runs this binary, so cargo is not asked to
/// build the one that is running.
fn run_step(root: &Path, step: &Step, log: &Path) -> Result<Outcome, String> {
    let file = std::fs::File::create(log).map_err(|e| format!("{}: {e}", log.display()))?;
    let errors = file
        .try_clone()
        .map_err(|e| format!("{}: {e}", log.display()))?;
    let own = std::env::current_exe()
        .ok()
        .filter(|_| step.program == "cargo" && step.args.first().is_some_and(|arg| arg == "xtask"));
    let mut command = if let Some(xtask) = &own {
        let mut command = Command::new(xtask);
        command.args(step.args.iter().skip(1));
        command
    } else {
        let mut command = Command::new(step.program);
        command.args(&step.args);
        command
    };
    command
        .current_dir(root)
        .envs(step.env.iter().copied())
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(errors);
    match command.status() {
        Ok(status) if status.success() => Ok(Outcome::Passed),
        Ok(_) => Ok(Outcome::Failed),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Outcome::Missing),
        Err(e) => Err(format!("cannot run {}: {e}", step.program)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::affected::graph::tests::workspace;

    struct Nothing;

    impl Lookup for Nothing {
        fn exists(&self, _path: &str) -> bool {
            false
        }

        fn naming(&self, _dir: &str, _extensions: &[&str], _needle: &str) -> Vec<String> {
            Vec::new()
        }

        fn changed_line_has(&self, _path: &str, _needle: &str) -> bool {
            false
        }

        fn std_sources(&self) -> BTreeMap<String, String> {
            BTreeMap::new()
        }
    }

    /// Each check's commands are followed by the files that ask for it, cut
    /// after a few.
    #[test]
    fn commands_are_listed_with_their_files() {
        let paths: Vec<String> = [
            "std/list.cheby",
            "std/map.cheby",
            "docs/a.md",
            "docs/b.md",
            "docs/c.md",
        ]
        .map(str::to_owned)
        .to_vec();
        let plan = rules::plan(&paths, &workspace(), &Nothing);
        let text = listed(&plan, &plan.steps(), paths.len());
        assert!(
            text.contains(
                "cargo xtask test-std 'std::list \"'\ncargo xtask test-std 'std::map \"'\n    \
                 # std/list.cheby, std/map.cheby\n"
            ),
            "{text}"
        );
        // Every file asks for the spelling check, so it needs no reason.
        assert!(
            text.ends_with("cargo xtask decisions lint\ntypos\n"),
            "{text}"
        );
    }

    #[test]
    fn paths_are_written_from_the_root() {
        let root = Path::new("/repo");
        assert_eq!(normal(root, "./std/list.cheby"), "std/list.cheby");
        assert_eq!(normal(root, "/repo/std/list.cheby"), "std/list.cheby");
        assert_eq!(
            normal(root, "docs/spec/01-overview.md"),
            "docs/spec/01-overview.md"
        );
        assert_eq!(count(1, "changed file"), "1 changed file");
        assert_eq!(count(3, "command"), "3 commands");
    }
}
