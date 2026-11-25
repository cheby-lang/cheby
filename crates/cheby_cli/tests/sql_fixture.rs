//! `tests/fixtures/cheby-sql`, the git dependency of example 030 (D-1889):
//! its own tests, its form and its warnings. It needs SQLite's header and
//! library.
//!
//! Programs do not run on Windows until M6, so this file is not built there
//! (D-545).
#![cfg(not(windows))]

use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::HarnessCache;
use cheby_test_support::{process, repo_root};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(120);

#[test]
fn the_sql_fixture_passes_its_tests() {
    let cache = HarnessCache::create("cli-sql-fixture").unwrap();
    for args in [
        ["fmt", "--check"].as_slice(),
        &["check", "--deny-warnings"],
        &["test"],
        &["test", "--release"],
        &["build"],
    ] {
        let mut cmd = Command::new(CHEBY);
        cmd.arg("--color=never")
            .args(args)
            .current_dir(repo_root().join("tests/fixtures/cheby-sql"))
            .env("NO_COLOR", "1");
        cache.apply(&mut cmd);
        let out = process::run(cmd, None, TIMEOUT).unwrap();
        assert_eq!(
            (out.stderr.as_str(), out.status),
            ("", Some(0)),
            "{args:?}\n{}",
            out.stdout
        );
    }
}
