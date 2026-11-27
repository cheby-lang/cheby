//! `cargo xtask doctor`: what in the checkout and on the machine is stale
//! or missing (D-1950). It reports and changes nothing.
//!
//! - The tools of CI's list: installed, and at the version that
//!   `.github/workflows/ci.yml` pins, which is read from the workflow so
//!   that no second list exists.
//! - `target/debug/deps`: how many entries it has, since every rustc lists
//!   the directory first, and whether a `RUSTFLAGS` variable replaces the
//!   flag of `.cargo/config.toml` that keeps object files from piling up.
//! - The tree-sitter grammar: the commit `tests/tree-sitter-cheby.rev`
//!   pins, against the sibling checkouts that should follow it.
//! - Worktrees whose directory is gone, the branches agents' worktrees
//!   left, and a runtime archive older than the runtime's sources.
//!
//! A finding is a line: `ok`, a `note`, or a `problem`, which is something
//! that breaks the tests or makes every build slow. A line that has a fix
//! is followed by its command. The exit status is 1 when a problem was
//! found.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// Past this many entries, `target/debug/deps` slows every compile.
const MAX_DEPS: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    Ok,
    Note,
    Problem,
}

/// One line of the report.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Finding {
    level: Level,
    text: String,
    /// The command that fixes it.
    fix: Option<String>,
}

impl Finding {
    fn new(level: Level, text: impl Into<String>) -> Self {
        Self {
            level,
            text: text.into(),
            fix: None,
        }
    }

    fn fix(mut self, command: impl Into<String>) -> Self {
        self.fix = Some(command.into());
        self
    }
}

pub(crate) fn run(root: &Path) -> Result<(), String> {
    let mut findings = tools(root)?;
    findings.extend(deps(root));
    findings.extend(grammar(root));
    findings.extend(worktrees(root));
    findings.push(archive(root));
    print!("{}", report(&findings));
    let problems = findings
        .iter()
        .filter(|finding| finding.level == Level::Problem)
        .count();
    if problems == 0 {
        Ok(())
    } else {
        Err(format!("doctor found {problems} of these to be problems"))
    }
}

/// The findings as lines, each fix below its line.
fn report(findings: &[Finding]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for finding in findings {
        let label = match finding.level {
            Level::Ok => "ok",
            Level::Note => "note",
            Level::Problem => "problem",
        };
        let _ = writeln!(out, "{label:<8} {}", finding.text);
        if let Some(fix) = &finding.fix {
            let _ = writeln!(out, "         fix: {fix}");
        }
    }
    out
}

/// A tool of CI's list: its name in `ci.yml`, the command that prints its
/// version, how to install it, and whether the tests need it.
struct Tool {
    name: &'static str,
    command: &'static [&'static str],
    install: &'static str,
    required: bool,
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "nextest",
        command: &["cargo", "nextest", "--version"],
        install: "cargo install cargo-nextest --locked",
        required: true,
    },
    Tool {
        name: "typos",
        command: &["typos", "--version"],
        install: "cargo install typos-cli --locked",
        required: false,
    },
    Tool {
        name: "taplo",
        command: &["taplo", "--version"],
        install: "cargo install taplo-cli --locked",
        required: false,
    },
    Tool {
        name: "cargo-deny",
        command: &["cargo", "deny", "--version"],
        install: "cargo install cargo-deny --locked",
        required: false,
    },
    Tool {
        name: "actionlint",
        command: &["actionlint", "-version"],
        install: "see https://github.com/rhysd/actionlint",
        required: false,
    },
    Tool {
        name: "tree-sitter-cli",
        command: &["tree-sitter", "--version"],
        install: "npm install --global tree-sitter-cli",
        required: false,
    },
];

/// The versions `ci.yml` pins, by the names it gives the tools: those of
/// `tool: a@1.2.3,b@4.5.6` lines, of `tree-sitter-cli@…`, and of the
/// actionlint archive's name.
fn pins(workflow: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut add = |name: &str, version: &str| {
        if !version.is_empty() && !out.iter().any(|(known, _)| known == name) {
            out.push((name.to_owned(), version.to_owned()));
        }
    };
    for line in workflow.lines() {
        let line = line.trim();
        if let Some(list) = line.strip_prefix("tool:") {
            for (name, version) in list
                .split(',')
                .filter_map(|tool| tool.trim().split_once('@'))
            {
                add(name, version);
            }
        }
        for word in line.split_whitespace() {
            if let Some(version) = word.strip_prefix("tree-sitter-cli@") {
                add("tree-sitter-cli", version);
            }
            if let Some(rest) = word.split("actionlint_").nth(1) {
                add("actionlint", rest.split('_').next().unwrap_or_default());
            }
        }
    }
    out
}

/// The first word of `text` that is a version: digits and dots, with at
/// least one dot.
fn version_in(text: &str) -> Option<&str> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|word| word.trim_matches('.'))
        .find(|word| word.contains('.') && word.starts_with(|c: char| c.is_ascii_digit()))
}

/// What `command` prints, when it runs and succeeds.
fn output(root: &Path, command: &[&str]) -> Option<String> {
    let (program, args) = command.split_first()?;
    let out = Command::new(program)
        .current_dir(root)
        .args(args)
        .output()
        .ok()?;
    out.status.success().then(|| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// Each tool of CI's list, and a C compiler, which `cheby build` links
/// with (D-1112).
fn tools(root: &Path) -> Result<Vec<Finding>, String> {
    let path = root.join(".github/workflows/ci.yml");
    let workflow =
        std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let pins = pins(&workflow);
    let mut out = Vec::new();
    for tool in TOOLS {
        let pinned = pins
            .iter()
            .find(|(name, _)| name == tool.name)
            .map(|(_, version)| version.as_str());
        let installed = output(root, tool.command)
            // The grammar's checkout may hold the CLI that the cross-check
            // falls back to.
            .or_else(|| {
                (tool.name == "tree-sitter-cli").then(|| {
                    let local = root.join("../tree-sitter-cheby/node_modules/.bin/tree-sitter");
                    output(root, &[local.to_str()?, "--version"])
                })?
            });
        out.push(tool_finding(tool, pinned, installed.as_deref()));
    }
    out.push(match output(root, &["cc", "--version"]) {
        Some(_) => Finding::new(Level::Ok, "cc, which `cheby build` links with"),
        None => Finding::new(
            Level::Note,
            "no `cc` on PATH: `cheby build` and the tests that build executables need one",
        ),
    });
    Ok(out)
}

/// How one tool stands: what its version command printed, against CI's
/// pin.
fn tool_finding(tool: &Tool, pinned: Option<&str>, printed: Option<&str>) -> Finding {
    let Some(printed) = printed else {
        let level = if tool.required {
            Level::Problem
        } else {
            Level::Note
        };
        let consequence = if tool.required {
            "`cargo xtask test` cannot run"
        } else {
            "`cargo xtask affected --run` skips its check"
        };
        return Finding::new(
            level,
            format!("{} is not installed: {consequence}", tool.name),
        )
        .fix(tool.install);
    };
    match (version_in(printed), pinned) {
        (Some(have), Some(pin)) if have == pin => {
            Finding::new(Level::Ok, format!("{} {have}, as in CI", tool.name))
        }
        (Some(have), Some(pin)) => Finding::new(
            Level::Note,
            format!("{} {have}, and CI runs {pin}", tool.name),
        ),
        (Some(have), None) => Finding::new(Level::Ok, format!("{} {have}", tool.name)),
        (None, _) => Finding::new(Level::Ok, tool.name),
    }
}

/// `target/debug/deps`: its number of entries, and whether a variable
/// replaces the flag that keeps macOS's object files out of it.
fn deps(root: &Path) -> Vec<Finding> {
    let dir = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| root.join("target"), PathBuf::from)
        .join("debug/deps");
    let mut out = Vec::new();
    match std::fs::read_dir(&dir) {
        Ok(entries) => out.push(deps_finding(entries.count())),
        Err(_) => out.push(Finding::new(Level::Ok, "no target/debug/deps yet")),
    }
    let set = ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"]
        .into_iter()
        .find(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
    if let Some(name) = set.filter(|_| cfg!(target_os = "macos")) {
        out.push(Finding::new(
            Level::Note,
            format!(
                "{name} is set, which replaces `-Csplit-debuginfo=off` of .cargo/config.toml: \
                 object files will pile up in target/debug/deps"
            ),
        ));
    }
    out
}

fn deps_finding(entries: usize) -> Finding {
    if entries > MAX_DEPS {
        Finding::new(
            Level::Problem,
            format!(
                "target/debug/deps has {entries} entries: every rustc lists it before it compiles"
            ),
        )
        .fix("find target/debug/deps -maxdepth 1 -type f -name '*.rcgu.o' -delete")
    } else {
        Finding::new(
            Level::Ok,
            format!("target/debug/deps has {entries} entries"),
        )
    }
}

/// The grammar's pinned commit against the sibling checkouts (AGENTS.md,
/// "Syntax changes and tree-sitter").
fn grammar(root: &Path) -> Vec<Finding> {
    let Ok(pin) = std::fs::read_to_string(root.join("tests/tree-sitter-cheby.rev")) else {
        return vec![Finding::new(
            Level::Problem,
            "tests/tree-sitter-cheby.rev cannot be read: the cross-check has no grammar to use",
        )];
    };
    let pin = pin.trim();
    let mut out = Vec::new();
    let checkout = root.join("../tree-sitter-cheby");
    if checkout.is_dir() {
        let head = output(&checkout, &["git", "rev-parse", "HEAD"]);
        out.push(pin_finding(
            "../tree-sitter-cheby",
            pin,
            head.as_deref().map(str::trim),
        ));
    }
    let extension = root.join("../zed-cheby/extension.toml");
    if let Ok(text) = std::fs::read_to_string(extension) {
        out.push(pin_finding(
            "../zed-cheby/extension.toml",
            pin,
            extension_rev(&text),
        ));
    }
    if out.is_empty() {
        out.push(Finding::new(
            Level::Ok,
            "no checkout of tree-sitter-cheby or zed-cheby next to this one",
        ));
    }
    out
}

/// The `rev = "…"` of an extension's manifest.
fn extension_rev(text: &str) -> Option<&str> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("rev"))
        .filter_map(|rest| rest.trim_start().strip_prefix('='))
        .map(|value| value.trim().trim_matches('"'))
        .next()
}

/// Whether `place` is at the commit that `tests/tree-sitter-cheby.rev`
/// pins.
fn pin_finding(place: &str, pin: &str, found: Option<&str>) -> Finding {
    let short = |rev: &str| rev.get(..7).unwrap_or(rev).to_owned();
    match found {
        Some(rev) if rev == pin => Finding::new(
            Level::Ok,
            format!("{place} is at the pinned grammar commit {}", short(pin)),
        ),
        Some(rev) => Finding::new(
            Level::Note,
            format!(
                "{place} is at {}, and tests/tree-sitter-cheby.rev pins {}",
                short(rev),
                short(pin)
            ),
        ),
        None => Finding::new(Level::Note, format!("{place} names no commit")),
    }
}

/// Worktrees whose directory is gone, and the branches that agents'
/// worktrees left.
fn worktrees(root: &Path) -> Vec<Finding> {
    let mut out = Vec::new();
    let listed = output(root, &["git", "worktree", "list", "--porcelain"]).unwrap_or_default();
    let gone = prunable(&listed);
    if gone > 0 {
        out.push(
            Finding::new(
                Level::Note,
                format!("{gone} worktrees have no directory any more"),
            )
            .fix("git worktree prune"),
        );
    }
    let branches = output(
        root,
        &[
            "git",
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/worktree-agent-*",
        ],
    )
    .unwrap_or_default();
    let left = branches.lines().filter(|line| !line.is_empty()).count();
    if left > 0 {
        out.push(Finding::new(
            Level::Note,
            format!(
                "{left} branches are named `worktree-agent-*`, left by agents' worktrees; `git \
                 branch -d` deletes one that is merged"
            ),
        ));
    }
    if out.is_empty() {
        out.push(Finding::new(Level::Ok, "no stale worktree or agent branch"));
    }
    out
}

/// How many worktrees of `git worktree list --porcelain` are prunable.
fn prunable(listed: &str) -> usize {
    listed
        .lines()
        .filter(|line| line.split_whitespace().next() == Some("prunable"))
        .count()
}

/// The runtime archive against the runtime's sources. `cargo xtask test`
/// builds it; this is for `cheby build` run by hand, which refuses an
/// archive of other sources (D-929).
fn archive(root: &Path) -> Finding {
    let target =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let file = target.join("debug").join(cheby_link::archive::ARCHIVE_FILE);
    let command = "cargo build -p cheby_rt_exe";
    let Some(built) = modified(&file) else {
        return Finding::new(
            Level::Note,
            "no runtime archive in target/debug: `cheby build` run by hand needs one",
        )
        .fix(command);
    };
    let newest = ["crates/cheby_runtime", "crates/cheby_rt_exe"]
        .iter()
        .flat_map(|dir| {
            cheby_test_support::files_with_extension(&root.join(dir), "rs").unwrap_or_default()
        })
        .filter_map(|source| modified(&source))
        .max();
    if newest.is_some_and(|source| source > built) {
        Finding::new(
            Level::Note,
            "the runtime archive is older than the runtime's sources: `cheby build` run by hand \
             refuses it",
        )
        .fix(command)
    } else {
        Finding::new(
            Level::Ok,
            "the runtime archive is newer than the runtime's sources",
        )
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The workflow pins every tool of the list, where the parser finds
    /// it: a tool CI stops pinning, or pins in another form, fails here.
    #[test]
    fn the_workflow_pins_every_tool() {
        let root = cheby_test_support::repo_root();
        let workflow = std::fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap();
        let pins = pins(&workflow);
        for tool in TOOLS {
            let pinned = pins.iter().find(|(name, _)| name == tool.name);
            assert!(
                pinned.is_some_and(|(_, version)| version_in(version) == Some(version.as_str())),
                "{}: {pins:?}",
                tool.name
            );
        }
    }

    #[test]
    fn pins_are_read_from_each_form() {
        let workflow = "        with:\n          tool: nextest@0.9.151\n          tool: \
                        typos@1.52.2,taplo@0.10.0\n      - run: npm install --global \
                        tree-sitter-cli@0.27.0\n          \
                        archive=actionlint_1.7.12_linux_amd64.tar.gz\n          tool: nextest@9.9.9\n";
        assert_eq!(
            pins(workflow),
            [
                ("nextest", "0.9.151"),
                ("typos", "1.52.2"),
                ("taplo", "0.10.0"),
                ("tree-sitter-cli", "0.27.0"),
                ("actionlint", "1.7.12"),
            ]
            .map(|(name, version)| (name.to_owned(), version.to_owned()))
        );
    }

    #[test]
    fn versions_in_what_tools_print() {
        assert_eq!(
            version_in("cargo-nextest 0.9.151 (abc 2026-01-01)"),
            Some("0.9.151")
        );
        assert_eq!(version_in("typos-cli 1.52.2"), Some("1.52.2"));
        assert_eq!(
            version_in("1.7.12\ninstalled by downloading"),
            Some("1.7.12")
        );
        assert_eq!(
            version_in("tree-sitter 0.27.0 (deadbeef 2026)"),
            Some("0.27.0")
        );
        assert_eq!(version_in("no version here 12"), None);
    }

    #[test]
    fn a_tool_against_its_pin() {
        let nextest = &TOOLS[0];
        let typos = &TOOLS[1];
        let same = tool_finding(nextest, Some("0.9.151"), Some("cargo-nextest 0.9.151"));
        assert_eq!(
            (same.level, same.text.as_str()),
            (Level::Ok, "nextest 0.9.151, as in CI")
        );
        let other = tool_finding(typos, Some("1.52.2"), Some("typos-cli 1.40.0"));
        assert_eq!(
            (other.level, other.text.as_str()),
            (Level::Note, "typos 1.40.0, and CI runs 1.52.2")
        );
        // Only what the tests cannot run without is a problem.
        assert_eq!(tool_finding(nextest, None, None).level, Level::Problem);
        let missing = tool_finding(typos, None, None);
        assert_eq!(missing.level, Level::Note);
        assert_eq!(
            missing.fix.as_deref(),
            Some("cargo install typos-cli --locked")
        );
    }

    #[test]
    fn deps_grammar_and_worktrees() {
        assert_eq!(deps_finding(9000).level, Level::Ok);
        let full = deps_finding(MAX_DEPS + 1);
        assert_eq!(full.level, Level::Problem);
        assert!(full.fix.is_some());
        assert_eq!(
            extension_rev("[grammars.cheby]\nrepository = \"x\"\nrev = \"abc1234def\"\n"),
            Some("abc1234def")
        );
        let pin = "b490f82ac6fbd71d";
        assert_eq!(pin_finding("../zed", pin, Some(pin)).level, Level::Ok);
        let behind = pin_finding("../zed", pin, Some("0123456789ab"));
        assert_eq!(
            behind.text,
            "../zed is at 0123456, and tests/tree-sitter-cheby.rev pins b490f82"
        );
        assert_eq!(
            prunable(
                "worktree /a\nHEAD x\n\nworktree /b\nHEAD y\nprunable gitdir file points to non-existent location\n"
            ),
            1
        );
    }

    #[test]
    fn the_report_puts_a_fix_below_its_line() {
        let findings = [
            Finding::new(Level::Ok, "fine"),
            Finding::new(Level::Problem, "broken").fix("mend it"),
            Finding::new(Level::Note, "odd"),
        ];
        assert_eq!(
            report(&findings),
            "ok       fine\nproblem  broken\n         fix: mend it\nnote     odd\n"
        );
    }
}
