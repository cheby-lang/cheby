//! `cheby run` end to end: the built binary on example 001
//! and on small packages. Binding whatever the milestone: these must pass.
//! In a debug build of the toolchain, status 0 also means the leak check
//! found no live object at exit (it would give 102).
//!
//! Programs do not run on Windows until M6, so this file is not built
//! there (D-545).
#![cfg(not(windows))]

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
                "//! Unsupported.\n\nimport std::fiber\nimport std::io\n\nfn main() {\n  io::println(\"never\")\n}\n",
            ),
        ],
    );
    let out = cheby(&dir, &["run"]);
    assert_eq!(out.stdout, "");
    // MIR lowering reports no E0038 since `dyn` lowers (D-554); an import
    // of an unshipped std module stays E0038 (D-480).
    assert!(
        out.stderr
            .starts_with("error[E0038]: the module `std::fiber` is not supported yet"),
        "{}",
        out.stderr
    );
    assert_eq!(out.status, Some(1));
}

/// A construct that the compiler cannot run yet, in code that `main`
/// reaches, is a compile error at the construct with status 1, never an
/// internal error, whichever phase finds it (D-464, D-480, D-510).
#[test]
fn unsupported_constructs_reached_from_main_exit_with_one() {
    let dir = package(
        "unsupported_reached",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                "//! Back-end gate.\n\nimport app::pick\nimport std::io\n\nfn main() {\n  io::println(\"never\")\n  let _ = pick::pick(\"a\")\n}\n",
            ),
            (
                "src/pick.cheby",
                "//! Reaches an unshipped module.\n\nimport std::fiber\n\npub fn pick(text: String) -> String {\n  text\n}\n",
            ),
        ],
    );
    let out = cheby(&dir, &["run"]);
    assert_eq!(out.stdout, "");
    // MIR lowering reports no E0038 since `dyn` lowers (D-554); an import
    // of an unshipped std module in a module `main` reaches stays E0038
    // (D-480).
    assert!(
        out.stderr.starts_with(
            "error[E0038]: the module `std::fiber` is not supported yet\n --> src/pick.cheby:3:8\n"
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
/// (§12.5). In a package built for both targets the cross-target check
/// (§12.6, C12) rejects its use from `main` with E0067, and nothing runs.
/// In a native-only package it is fine.
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
            "error[E0067]: `say` exists only on `native`, but `main` also runs on `js`"
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

/// Runs `main_body` (after a `read_line`, so the reader of standard output
/// is gone before the program writes) with standard output closed, and
/// returns the status and standard error.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to spawn or talk to `cheby` fails the test"
)]
fn run_with_closed_stdout(name: &str, main_body: &str) -> (Option<i32>, String) {
    use std::io::Write as _;
    use std::process::Stdio;

    let source = format!(
        "//! Writes to a closed standard output.\n\nimport std::io\n\nfn until_err(n: Int) -> Int {{\n  case io::write_stdout(\"line\\n\") {{\n    Ok(_) => until_err(n + 1)\n    Err(_) => n\n  }}\n}}\n\nfn main() {{\n  let _ = io::read_line()\n  let n = until_err(0)\n  io::eprintln(\"failed after {{n}} writes\")\n{main_body}  io::eprintln(\"main returns\")\n}}\n"
    );
    let dir = package(name, &[MANIFEST, ("src/main.cheby", &source)]);
    let mut child = Command::new(CHEBY)
        .args(["--color=never", "run"])
        .current_dir(&dir)
        .env("NO_COLOR", "1")
        .env("CHEBY_RT_POISON", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Close the reading end first, then let the program go on.
    drop(child.stdout.take());
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"go\n").unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Once `io::write_stdout` has returned `Err`, the text it reported may
/// stay unwritten without ending the program (D-574).
#[test]
fn a_reported_stdout_failure_keeps_the_exit_status() {
    let (status, stderr) = run_with_closed_stdout("stdout_reported", "");
    assert!(stderr.contains("failed after 0 writes"), "{stderr}");
    assert!(stderr.contains("main returns"), "{stderr}");
    assert_eq!(status, Some(0), "{stderr}");
}

/// Text written after the reported failure, without a `Result`, that
/// cannot be written still ends the program with status 1 (D-306, D-346):
/// the excuse covers only the text the `Err` reported.
#[test]
fn later_unreported_stdout_failures_exit_with_one() {
    let (status, stderr) = run_with_closed_stdout("stdout_unreported", "  io::print(\"lost\")\n");
    assert!(stderr.contains("failed after 0 writes"), "{stderr}");
    assert_eq!(status, Some(1), "{stderr}");
}

/// A `case` with ten list patterns of about 2,000 elements each, longest
/// first, compiles and runs: the decision tree recurses per tested
/// element, and the compiler runs on a thread whose stack is large enough
/// for it (D-601). The source is generated, one line
/// per element as the formatter writes long lists.
#[test]
fn long_list_patterns_do_not_overflow_the_compiler() {
    let mut main = String::from(
        "//! Ten long list patterns.\n\nimport std::io\nimport std::list\n\n\
         fn f(xs: List<Int>) -> Int {\n  case xs {\n",
    );
    for arm in 0..10_usize {
        main.push_str("    [\n");
        for _ in 0..2_000 - arm {
            main.push_str("      0,\n");
        }
        main.push_str("    ] => ");
        main.push_str(&arm.to_string());
        main.push('\n');
    }
    main.push_str(
        "    _ => 99\n  }\n}\n\nfn main() {\n  let n = f(list::repeat(0, 1_997))\n  \
         io::println(\"{n}\")\n}\n",
    );
    let dir = package("long_list_patterns", &[MANIFEST, ("src/main.cheby", &main)]);
    let out = cheby(&dir, &["run"]);
    assert_eq!(out.stderr, "");
    assert_eq!(out.stdout, "3\n");
    assert_eq!(out.status, Some(0));
}

/// An entry that the end of piped input leaves open is checked, so its
/// syntax error is reported instead of the entry vanishing (D-546).
#[test]
fn an_unfinished_repl_entry_is_reported_at_the_end_of_input() {
    let dir = Path::new(TMP).join(format!("repl-unfinished-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cmd = Command::new(CHEBY);
    cmd.args(["--color=never", "repl"])
        .current_dir(&dir)
        .env("NO_COLOR", "1");
    let out = process::run(cmd, Some(b"1 + 1\nfn f() -> Int {\n"), TIMEOUT).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        out.stdout, "> 1 + 1\n2 : Int\n> fn f() -> Int {\n. \n",
        "{}",
        out.stderr
    );
    assert!(
        out.stderr
            .starts_with("error[E0015]: unclosed `{`\n --> <repl:2>:1:16\n"),
        "{}",
        out.stderr
    );
    assert_eq!(out.status, Some(0));
}
