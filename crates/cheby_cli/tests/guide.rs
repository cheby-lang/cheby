//! The guide for coding agents, end to end (D-1952): `cheby guide` prints
//! `docs/guide/agents.md`, `cheby new` writes it as `AGENTS.md`, and every
//! `cheby` code block of the guide is a module that checks without a
//! diagnostic, is in `cheby fmt`'s form and passes its own tests, as a
//! single file (D-1949). So a renamed std function or a changed rule fails
//! here before the guide teaches it.
#![expect(
    clippy::unwrap_used,
    reason = "test helpers: a failed setup step fails the test"
)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::{CACHE_ENV, FreshDir};
use cheby_test_support::process::{self, RunOutput};
use cheby_test_support::repo_root;

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(300);

fn guide() -> String {
    std::fs::read_to_string(repo_root().join("docs/guide/agents.md"))
        .unwrap()
        .replace("\r\n", "\n")
}

fn cheby(dir: &Path, cache: &Path, args: &[&str]) -> RunOutput {
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never")
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, cache);
    process::run(cmd, None, TIMEOUT).unwrap()
}

/// The `cheby` code blocks of the guide, in order.
fn blocks(guide: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    for line in guide.lines() {
        match &mut current {
            Some(block) if line == "```" => {
                out.push(std::mem::take(block));
                current = None;
            }
            Some(block) => {
                block.push_str(line);
                block.push('\n');
            }
            None if line == "```cheby" => current = Some(String::new()),
            None => {}
        }
    }
    out
}

#[test]
fn the_command_prints_the_guide() {
    let dir = FreshDir::create("cli-guide").unwrap();
    let cache = FreshDir::create("cli-guide-cache").unwrap();
    let out = cheby(dir.path(), cache.path(), &["guide"]);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    assert_eq!(out.stderr, "");
    assert_eq!(out.stdout, guide());
}

/// `cheby new` writes the guide as `AGENTS.md`, and `--no-guide` leaves it
/// out.
#[test]
fn a_new_package_has_the_guide() {
    let dir = FreshDir::create("cli-guide-new").unwrap();
    let cache = FreshDir::create("cli-guide-cache").unwrap();
    let with = cheby(dir.path(), cache.path(), &["new", "my_app"]);
    assert_eq!(
        (with.status, with.stdout.as_str(), with.stderr.as_str()),
        (Some(0), "", "")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("my_app/AGENTS.md")).unwrap(),
        guide()
    );
    let without = cheby(dir.path(), cache.path(), &["new", "bare", "--no-guide"]);
    assert_eq!(without.status, Some(0), "{}", without.stderr);
    assert!(!dir.path().join("bare/AGENTS.md").exists());
    assert!(dir.path().join("bare/src/main.cheby").is_file());
}

/// Every code block is a whole module: no diagnostic, no formatting left
/// to do, and tests that pass.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT code, which Windows cannot until M6 (D-545)"
)]
fn every_code_block_checks_formats_and_passes() {
    let blocks = blocks(&guide());
    assert!(blocks.len() >= 2, "{}", blocks.len());
    let dir = FreshDir::create("cli-guide-blocks").unwrap();
    let cache = FreshDir::create("cli-guide-cache").unwrap();
    let mut tests = 0;
    for (index, block) in blocks.iter().enumerate() {
        let file = format!("block{index}.cheby");
        std::fs::write(dir.path().join(&file), block).unwrap();
        let check = cheby(dir.path(), cache.path(), &["check", &file]);
        assert_eq!(
            (check.status, check.stderr.as_str()),
            (Some(0), ""),
            "block {index}:\n{block}"
        );
        let fmt = cheby(dir.path(), cache.path(), &["fmt", "--check", &file]);
        assert_eq!(
            fmt.status,
            Some(0),
            "block {index} is not formatted:\n{block}"
        );
        if block.contains("\ntest \"") {
            let test = cheby(dir.path(), cache.path(), &["test", &file]);
            assert_eq!(test.status, Some(0), "block {index}:\n{}", test.stdout);
            tests += 1;
        }
    }
    assert!(tests >= 2, "{tests} blocks have tests");
}
