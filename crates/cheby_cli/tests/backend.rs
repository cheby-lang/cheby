//! The language server's hidden child command, `cheby __backend` (plan
//! §1.8, D-1349, D-1410): one JSON document on stdout, status 0 whenever
//! the document is whole, and an exit as soon as stdin closes, so a child
//! whose server died does not linger.
//!
//! Each test keeps the child's stdin open until it has read the answer,
//! since the child's watchdog exits when stdin ends, and kills the child on
//! the way out, so no child outlives its test.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cheby_lsp::child::wire::{Envelope, Front, Section, Status};
use cheby_test_support::cache::HarnessCache;

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");

/// How long a child may take before the test kills it and fails.
const PATIENCE: Duration = Duration::from_secs(120);

/// The child's status when its watchdog saw stdin end (`WATCHDOG_EXIT`).
const WATCHDOG_EXIT: i32 = 3;

/// A package `name` in this test binary's scratch directory, with `main`
/// as its root module and `extra` appended to its manifest.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to write the package fails the test"
)]
fn package(name: &str, main: &str, extra: &str) -> PathBuf {
    let root = Path::new(TMP).join("backend").join(name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("cheby.toml"),
        format!("name = \"{name}\"\nversion = \"0.1.0\"\n{extra}"),
    )
    .unwrap();
    std::fs::write(root.join("src/main.cheby"), main).unwrap();
    root
}

/// A running `cheby __backend`, killed when dropped.
struct Backend {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<JoinHandle<Vec<u8>>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
}

/// What a child left: its status and its two streams.
struct Ended {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl Backend {
    /// Starts `cheby __backend <args>` with stdin open.
    #[expect(
        clippy::unwrap_used,
        reason = "a test helper: failing to start the child fails the test"
    )]
    fn start(args: &[&str], cache: &HarnessCache) -> Self {
        let mut cmd = Command::new(CHEBY);
        cmd.arg("-j")
            .arg("1")
            .arg("__backend")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("NO_COLOR", "1");
        cache.apply(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().map(|mut out| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = out.read_to_end(&mut bytes);
                bytes
            })
        });
        let stderr = child.stderr.take().map(|mut err| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = err.read_to_end(&mut bytes);
                bytes
            })
        });
        Self {
            child,
            stdin,
            stdout,
            stderr,
        }
    }

    /// Closes the child's stdin, as a server that died does.
    fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    /// The status, if the child has exited.
    #[expect(clippy::unwrap_used, reason = "a test helper")]
    fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().unwrap()
    }

    /// Waits up to `limit` for the child to exit, with stdin as it is.
    #[expect(
        clippy::unwrap_used,
        reason = "a test helper: a child that does not end fails the test"
    )]
    fn wait(mut self, limit: Duration) -> Ended {
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.exited() {
                break status;
            }
            assert!(
                start.elapsed() <= limit,
                "`cheby __backend` still runs after {limit:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let stdout = self.stdout.take().map(|t| t.join().unwrap()).unwrap();
        let stderr = self.stderr.take().map(|t| t.join().unwrap()).unwrap();
        Ended {
            status,
            stdout: String::from_utf8(stdout).unwrap(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Runs `cheby __backend <args>` to its end with stdin held open, and
/// reads its one document.
#[expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper: a missing or broken document fails the test"
)]
fn answer(args: &[&str]) -> Envelope {
    let cache = HarnessCache::create("cli-backend").unwrap();
    let ended = Backend::start(args, &cache).wait(PATIENCE);
    assert_eq!(ended.status.code(), Some(0), "stderr: {}", ended.stderr);
    assert!(ended.stdout.ends_with("}\n"), "{:?}", ended.stdout);
    Envelope::parse(ended.stdout.as_bytes())
        .unwrap_or_else(|e| panic!("not one document ({e}): {:?}", ended.stdout))
}

/// D-1410: a `--schema` this build does not write is an `internal-error`
/// document, with status 0 and nothing else on stdout. It reads no package.
#[test]
fn schema_mismatch_is_an_internal_error_document() {
    let document = answer(&["--schema", "99", "--root", TMP, "--want", "constants"]);
    assert_eq!(document.schema, 1);
    assert!(
        matches!(&document.status, Status::InternalError { message } if message.contains("schema 99")),
        "{document:?}"
    );
    assert_eq!(document.front, None, "{document:?}");
}

/// Plan §1.8: a saved file that does not parse gives `front: errors`, and
/// the constants section is skipped; the child still answers `ok` and 0.
#[test]
#[cfg_attr(
    windows,
    ignore = "the child evaluates no constant on Windows until M6 (D-545, D-1414)"
)]
fn grammar_error_gives_front_errors() {
    let root = package(
        "broken",
        "//! Does not parse.\n\nconst limit: Int = 20\n\nfn main( {\n}\n",
        "",
    );
    let root = root.to_str().expect("a UTF-8 path");
    let document = answer(&["--schema", "1", "--root", root, "--want", "constants"]);
    assert_eq!(document.status, Status::Ok, "{document:?}");
    assert_eq!(document.front, Some(Front::Errors), "{document:?}");
    assert!(
        matches!(document.sections.constants, Some(Section::Skipped { .. })),
        "{document:?}"
    );
    assert!(
        document
            .files
            .iter()
            .any(|file| file.path == "src/main.cheby"),
        "{document:?}"
    );
}

/// D-1510: each value names, as `/` paths, the package's module files its
/// initializer reaches: its own and, through a function, `util`'s, but
/// not a module it does not reach.
#[test]
#[cfg_attr(
    windows,
    ignore = "the child evaluates no constant on Windows until M6 (D-545, D-1414)"
)]
fn values_name_the_modules_they_reach() {
    let root = package(
        "reaches",
        "//! Constants that reach `util` or not.\n\nimport reaches::util\nimport reaches::other\n\n\
         const plain: Int = 20\n\nconst tripled: Int = util::triple(plain)\n\n\
         fn main() {\n  assert tripled > other::zero()\n}\n",
        "",
    );
    std::fs::write(
        root.join("src/util.cheby"),
        "//! Arithmetic.\n\npub fn triple(n: Int) -> Int {\n  n * 3\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/other.cheby"),
        "//! Read by `main` only.\n\npub fn zero() -> Int {\n  0\n}\n",
    )
    .unwrap();
    let root = root.to_str().expect("a UTF-8 path");
    let document = answer(&["--schema", "1", "--root", root, "--want", "constants"]);
    let Some(Section::Ok(constants)) = document.sections.constants else {
        panic!("an ok section: {document:?}");
    };
    let modules = |name: &str| {
        constants
            .values
            .iter()
            .find(|value| value.symbol.name == name)
            .and_then(|value| value.modules.clone())
    };
    assert_eq!(modules("plain"), Some(vec!["src/main.cheby".to_owned()]));
    assert_eq!(
        modules("tripled"),
        Some(vec![
            "src/main.cheby".to_owned(),
            "src/util.cheby".to_owned()
        ])
    );
}

/// D-1410, A §11.5: a child busy in a constant exits soon after its stdin
/// closes, with the watchdog's status and nothing on stdout, so a child
/// whose server died does not linger.
#[test]
#[cfg_attr(
    windows,
    ignore = "the child evaluates no constant on Windows until M6 (D-545, D-1414)"
)]
fn child_exits_when_stdin_closes() {
    // The constant spins for the whole of a 10-minute limit.
    let root = package(
        "slow",
        "//! A constant that outlasts any test.\n\
         \n\
         import std::io\n\
         \n\
         const forever: Int = spin(0)\n\
         \n\
         fn spin(n: Int) -> Int {\n  spin(n + 1)\n}\n\
         \n\
         fn main() {\n  io::println(\"{forever}\")\n}\n",
        "\n[const-eval]\ntime-limit = \"600s\"\n",
    );
    let cache = HarnessCache::create("cli-backend").unwrap();
    let root = root.to_str().expect("a UTF-8 path");
    let mut child = Backend::start(
        &["--schema", "1", "--root", root, "--want", "constants"],
        &cache,
    );
    // Give it time to reach the constant; it must still be running.
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        child.exited().is_none(),
        "`cheby __backend` ended before its stdin closed"
    );
    let closed = Instant::now();
    child.close_stdin();
    let ended = child.wait(Duration::from_secs(10));
    assert!(
        closed.elapsed() < Duration::from_secs(5),
        "took {:?}",
        closed.elapsed()
    );
    assert_eq!(
        ended.status.code(),
        Some(WATCHDOG_EXIT),
        "stderr: {}",
        ended.stderr
    );
    assert_eq!(ended.stdout, "");
}
