//! `cheby explain` end to end (D-420, G8): for every registered code the
//! built binary prints the code's file in `docs/diagnostics/` as it is on
//! disk, and an unknown code is a tool error.

use std::process::Command;
use std::time::Duration;

use cheby_diag::codes::ALL;
use cheby_test_support::cache::HarnessCache;
use cheby_test_support::process::{self, RunOutput};
use cheby_test_support::repo_root;

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(60);

#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to spawn `cheby` fails the test"
)]
fn explain(code: &str) -> RunOutput {
    // A fresh build cache of this test (D-1128).
    let cache = HarnessCache::create("cli-explain").unwrap();
    let mut cmd = Command::new(CHEBY);
    cmd.args(["--color=never", "explain", code])
        .current_dir(repo_root())
        .env("NO_COLOR", "1");
    cache.apply(&mut cmd);
    process::run(cmd, None, TIMEOUT).unwrap()
}

#[test]
fn every_code_prints_its_file() {
    let dir = repo_root().join("docs/diagnostics");
    assert!(ALL.len() > 90, "{} codes", ALL.len());
    for info in ALL {
        let code = info.code.to_string();
        let file = std::fs::read_to_string(dir.join(format!("{code}.md"))).unwrap();
        let mut expected = file.replace("\r\n", "\n");
        if !expected.ends_with('\n') {
            expected.push('\n');
        }
        let out = explain(&code);
        assert_eq!(out.status, Some(0), "{code}: {}", out.stderr);
        assert_eq!(out.stderr, "", "{code}");
        assert_eq!(out.stdout, expected, "{code}");
        assert!(
            out.stdout.starts_with(&format!("# {code}: ")),
            "{code}: {}",
            out.stdout
        );
    }
}

/// The codes this plan added for constant evaluation (plan §3) are
/// registered and explained.
#[test]
fn constant_evaluation_codes_are_explained() {
    for code in ["E0075", "E0076", "E0077", "E0078"] {
        let out = explain(code);
        assert_eq!(out.status, Some(0), "{code}: {}", out.stderr);
        assert!(
            repo_root()
                .join(format!("docs/diagnostics/{code}.md"))
                .is_file()
        );
        assert!(
            out.stdout.starts_with(&format!("# {code}: constant ")),
            "{}",
            out.stdout
        );
    }
}

#[test]
fn an_unknown_code_is_a_tool_error() {
    for code in ["E9999", "X0001", "e00"] {
        let out = explain(code);
        assert_eq!(out.status, Some(1), "{code}");
        assert_eq!(out.stdout, "", "{code}");
        assert!(
            out.stderr
                .contains(&format!("no diagnostic has the code `{code}`")),
            "{code}: {}",
            out.stderr
        );
    }
}
