//! `cheby run` end to end: the built binary on example 001
//! and on small packages. Binding whatever the milestone: these must pass.
//! In a debug build of the toolchain, status 0 also means the leak check
//! found no live object at exit (it would give 102).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cheby_test_support::process::{self, RunOutput};
use cheby_test_support::repo_root;

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
const TIMEOUT: Duration = Duration::from_secs(60);

#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to spawn `cheby` fails the test"
)]
fn cheby(dir: &Path, args: &[&str]) -> RunOutput {
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never")
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1");
    process::run(cmd, None, TIMEOUT).unwrap()
}

#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to write the package fails the test"
)]
fn package(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = Path::new(TMP).join("run").join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    for (path, text) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    dir
}

const MANIFEST: (&str, &str) = ("cheby.toml", "name = \"app\"\nversion = \"0.1.0\"\n");

#[test]
fn hello_world_runs() {
    let out = cheby(&repo_root().join("docs/examples/001_hello_world"), &["run"]);
    assert_eq!(out.stderr, "");
    assert_eq!(out.stdout, "Hello, world!\n");
    assert_eq!(out.status, Some(0));
}

/// Program arguments after `--` are not taken for a module path.
#[test]
fn arguments_follow_the_separator() {
    let out = cheby(
        &repo_root().join("docs/examples/001_hello_world"),
        &["run", "--", "a", "--b"],
    );
    assert_eq!(out.stdout, "Hello, world!\n");
    assert_eq!(out.status, Some(0));
}

/// `let`, dups, drops, and tail calls within and across modules, with
/// output on both streams.
#[test]
fn lets_and_tail_calls_run() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../cheby_driver/tests/packages/lets");
    let out = cheby(&dir, &["run"]);
    assert_eq!(out.stdout, "hi\ngreet: hi\nhi!\n");
    assert_eq!(out.stderr, "hi\n");
    assert_eq!(out.status, Some(0));
}

#[test]
fn a_named_module_runs() {
    let dir = package(
        "named",
        &[
            MANIFEST,
            ("src/main.cheby", "//! Root.\n\nfn main() {\n  Nil\n}\n"),
            (
                "src/tools/migrate.cheby",
                "//! A tool.\n\nimport std::io\n\nfn main() {\n  io::println(\"migrating\")\n}\n",
            ),
        ],
    );
    let out = cheby(&dir, &["run", "app::tools::migrate"]);
    assert_eq!(out.stderr, "");
    assert_eq!(out.stdout, "migrating\n");
    assert_eq!(out.status, Some(0));
}

#[test]
fn tool_errors_exit_with_one() {
    let dir = package(
        "tool_errors",
        &[
            MANIFEST,
            ("src/main.cheby", "//! Root.\n\nfn main() {\n  Nil\n}\n"),
            ("src/lib.cheby", "//! No main.\n\nfn f() {\n  Nil\n}\n"),
        ],
    );
    let out = cheby(&dir, &["run", "app::nope"]);
    assert_eq!(
        out.stderr,
        "error: there is no module `app::nope` in this package\n"
    );
    assert_eq!(out.status, Some(1));
    let out = cheby(&dir, &["run", "app::lib"]);
    assert_eq!(
        out.stderr,
        "error: module `app::lib` has no `main` function\n"
    );
    assert_eq!(out.status, Some(1));
}

/// Compile errors are printed and nothing runs.
#[test]
fn compile_errors_exit_with_one() {
    let dir = package(
        "compile_error",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                "//! Unsupported.\n\nimport std::io\n\nfn main() {\n  io::println(\"never\")\n  let _ = 1 + 2\n}\n",
            ),
        ],
    );
    let out = cheby(&dir, &["run"]);
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr
            .starts_with("error[E0038]: operators are not supported yet"),
        "{}",
        out.stderr
    );
    assert_eq!(out.status, Some(1));
}

/// A construct that the compiler cannot run yet, in code that `main`
/// reaches, is a compile error at the construct with status 1, never an
/// internal error, whichever phase finds it (D-451, D-464, plan §8).
#[test]
fn unsupported_constructs_reached_from_main_exit_with_one() {
    let dir = package(
        "unsupported_reached",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                "//! Back-end gate.\n\nimport std::io\n\nfn main() {\n  io::println(\"never\")\n  let _ = pick(\"a\")\n}\n\nfn pick(text: String) -> String {\n  case text {\n    \"a\" => \"first\"\n    _ => text\n  }\n}\n",
            ),
        ],
    );
    let out = cheby(&dir, &["run"]);
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.starts_with(
            "error[E0038]: `case` expressions are not supported yet\n  --> src/main.cheby:11:3"
        ),
        "{}",
        out.stderr
    );
    assert!(
        !out.stderr.contains("internal compiler error"),
        "{}",
        out.stderr
    );
    assert_eq!(out.status, Some(1));
}

/// `cheby check` runs every implemented phase now, not only syntax.
#[test]
fn check_reports_resolution_errors() {
    let dir = package(
        "check",
        &[
            MANIFEST,
            ("src/main.cheby", "//! Root.\n\nfn main() {\n  nope()\n}\n"),
        ],
    );
    let out = cheby(&dir, &["check"]);
    assert!(out.stderr.contains("error[E0042]"), "{}", out.stderr);
    assert_eq!(out.status, Some(1));
}

/// A bodiless function with only a native external exists only on native
/// (§12.5), and the cross-target check (§12.6, C12) does not exist yet:
/// in a package built for both targets it is rejected, never run. In a
/// native-only package it is fine.
#[test]
fn native_only_externals_need_a_native_only_package() {
    let source = (
        "src/main.cheby",
        "//! Native only.\n\n@external(native, \"cheby_io_println\")\nfn say(text: String)\n\nfn main() {\n  say(\"native only\")\n}\n",
    );
    let dir = package("native_only_both", &[MANIFEST, source]);
    let out = cheby(&dir, &["run"]);
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.starts_with(
            "error[E0038]: functions that exist only on some of the package's targets are not supported yet"
        ),
        "{}",
        out.stderr
    );
    assert_eq!(out.status, Some(1));
    let native = (
        "cheby.toml",
        "name = \"app\"\nversion = \"0.1.0\"\ntargets = [\"native\"]\n",
    );
    let dir = package("native_only_native", &[native, source]);
    let out = cheby(&dir, &["run"]);
    assert_eq!(out.stderr, "");
    assert_eq!(out.stdout, "native only\n");
    assert_eq!(out.status, Some(0));
}

const NATIVE_MANIFEST: (&str, &str) = (
    "cheby.toml",
    "name = \"app\"\nversion = \"0.1.0\"\ntargets = [\"native\"]\n",
);

/// Problems with native symbols are coded program errors (D-419, D-442,
/// D-455), never internal compiler errors.
#[test]
fn native_symbol_errors_are_diagnostics() {
    // E0055: the runtime's and generated code's symbols are reserved.
    let dir = package(
        "native_reserved",
        &[
            NATIVE_MANIFEST,
            (
                "src/main.cheby",
                "//! Reserved.\n\n@external(native, \"cheby_rt_drop\")\nfn evil(text: String) -> String\n\nfn main() {\n  Nil\n}\n",
            ),
        ],
    );
    let out = cheby(&dir, &["check"]);
    assert!(
        out.stderr.starts_with(
            "error[E0055]: the native symbol `cheby_rt_drop` is reserved for the toolchain"
        ),
        "{}",
        out.stderr
    );
    assert_eq!(out.status, Some(1));
    // E0054: one symbol, two C signatures, even if neither is called; the
    // std binding of `cheby_io_println` takes part too.
    let dir = package(
        "native_conflict",
        &[
            NATIVE_MANIFEST,
            (
                "src/main.cheby",
                "//! Conflict.\n\nimport std::io\n\n@external(native, \"cheby_io_println\")\nfn second(text: String) -> String\n\nfn main() {\n  io::println(\"a\")\n}\n",
            ),
        ],
    );
    let out = cheby(&dir, &["check"]);
    assert!(
        out.stderr.starts_with(
            "error[E0054]: the native symbol `cheby_io_println` is bound with two different C signatures"
        ),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("std/io.cheby"), "{}", out.stderr);
    assert_eq!(out.status, Some(1));
    // E0053: `cheby check` cannot know, `cheby run` reports it before the
    // program starts, at the `@external`.
    let dir = package(
        "native_missing",
        &[
            NATIVE_MANIFEST,
            (
                "src/main.cheby",
                "//! Missing.\n\n@external(native, \"no_such_symbol_xyz\")\nfn say(text: String)\n\nfn main() {\n  say(\"direct\")\n}\n",
            ),
        ],
    );
    let out = cheby(&dir, &["check"]);
    assert_eq!(out.stderr, "");
    assert_eq!(out.status, Some(0));
    let out = cheby(&dir, &["run"]);
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr
            .starts_with("error[E0053]: the native symbol `no_such_symbol_xyz` is not defined\n --> src/main.cheby:3:19"),
        "{}",
        out.stderr
    );
    assert_eq!(out.status, Some(1));
}
