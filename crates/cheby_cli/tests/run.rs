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

use cheby_test_support::cache::{CACHE_ENV, HarnessCache};
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
    let cache = fresh_cache();
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never")
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1");
    cache.apply(&mut cmd);
    process::run(cmd, None, TIMEOUT).unwrap()
}

/// A fresh build cache for one `cheby` this file starts, deleted when the
/// value is dropped (D-1128).
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to create the directory fails the test"
)]
fn fresh_cache() -> HarnessCache {
    HarnessCache::create("cli-run").unwrap()
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

/// A slot whose mask is in its name never shares a symbol with another
/// item's slot (D-456, D-917, D-1109). `app::m::f`, package
/// visible and not `pub` (a `pub` function stays owned, D-812), borrows its
/// parameter, and the module `app::m::f` (§7 lets a file and a directory
/// share a name) holds an item `b1`. With D-917's one `$` before the mask,
/// both slots were `cb$slot$app$m$f$b1`, and the JIT stopped with an
/// internal error (wave 2 review, SND-1).
#[test]
fn a_masked_slot_and_a_nested_module_item_both_run() {
    let dir = package(
        "masked_slot",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                "//! Calls both functions.\n\nimport app::m\nimport app::m::f\nimport std::io\n\n\
                 fn main() {\n  let a = m::f(m::Circle(2))\n  let b = f::b1(40)\n  \
                 io::println(\"{a} {b}\")\n}\n",
            ),
            (
                "src/m.cheby",
                "//! Holds `f`, which borrows its parameter.\n\npub type Shape {\n  Circle(Int),\n  \
                 Square(Int),\n}\n\nfn f(shape: Shape) -> Int {\n  case shape {\n    \
                 Circle(r) => r\n    Square(s) => s\n  }\n}\n",
            ),
            (
                "src/m/f.cheby",
                "//! A module whose path and item spell `f`'s masked slot with one `$`.\n\n\
                 pub fn b1(x: Int) -> Int {\n  x + 1\n}\n",
            ),
        ],
    );
    let out = cheby(&dir, &["run"]);
    assert_eq!(out.stderr, "");
    assert_eq!(out.stdout, "2 41\n");
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
                "//! Unsupported.\n\nimport std::io\nimport std::process\n\nfn main() {\n  io::println(\"never\")\n}\n",
            ),
        ],
    );
    let out = cheby(&dir, &["run"]);
    assert_eq!(out.stdout, "");
    // MIR lowering reports no E0038 since `dyn` lowers (D-554); an import
    // of an unshipped std module stays E0038 (D-480), and since `std::http`
    // ships (D-1003) `std::process` is the unshipped one (D-729, D-989).
    assert!(
        out.stderr
            .starts_with("error[E0038]: the module `std::process` is not supported yet"),
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
                "//! Reaches an unshipped module.\n\nimport std::process\n\npub fn pick(text: String) -> String {\n  text\n}\n",
            ),
        ],
    );
    let out = cheby(&dir, &["run"]);
    assert_eq!(out.stdout, "");
    // MIR lowering reports no E0038 since `dyn` lowers (D-554); an import
    // of an unshipped std module in a module `main` reaches stays E0038
    // (D-480), with `std::process` the unshipped one since `std::http`
    // ships (D-1003, D-729, D-989).
    assert!(
        out.stderr.starts_with(
            "error[E0038]: the module `std::process` is not supported yet\n --> src/pick.cheby:3:8\n"
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
/// In a native-only package it is fine. The external binds libc's `labs`,
/// which the process defines: a name the runtime exports would be E0055
/// (D-944).
#[test]
fn native_only_externals_need_a_native_only_package() {
    let source = (
        "src/main.cheby",
        "//! Native only.\n\nimport std::io\n\n@external(native, \"labs\")\nfn magnitude(n: Int) -> Int\n\nfn main() {\n  let m = magnitude(-7)\n  io::println(\"{m}\")\n}\n",
    );
    let dir = package("native_only_both", &[MANIFEST, source]);
    let out = cheby(&dir, &["run"]);
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.starts_with(
            "error[E0067]: `magnitude` exists only on `native`, but `main` also runs on `js`"
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
    assert_eq!(out.stdout, "7\n");
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
    // E0055 for every other name the runtime exports (D-944): a std
    // kernel's symbol, whatever the signature.
    let dir = package(
        "native_runtime_export",
        &[
            NATIVE_MANIFEST,
            (
                "src/main.cheby",
                "//! Reserved too.\n\nimport std::io\n\n@external(native, \"cheby_io_println\")\nfn second(text: String) -> String\n\nfn main() {\n  io::println(\"a\")\n}\n",
            ),
        ],
    );
    let out = cheby(&dir, &["check"]);
    assert!(
        out.stderr.starts_with(
            "error[E0055]: the native symbol `cheby_io_println` is reserved for the toolchain"
        ),
        "{}",
        out.stderr
    );
    assert_eq!(out.status, Some(1));
    // E0054: one symbol, two C signatures, even if neither is called.
    let dir = package(
        "native_conflict",
        &[
            NATIVE_MANIFEST,
            (
                "src/main.cheby",
                "//! Conflict.\n\nimport std::io\n\n@external(native, \"labs\")\npub fn first(n: Int) -> Int\n\n@external(native, \"labs\")\npub fn second(n: I32) -> I32\n\nfn main() {\n  io::println(\"a\")\n}\n",
            ),
        ],
    );
    let out = cheby(&dir, &["check"]);
    assert!(
        out.stderr.starts_with(
            "error[E0054]: the native symbol `labs` is bound with two different C signatures"
        ),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("src/main.cheby"), "{}", out.stderr);
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
    let cache = fresh_cache();
    let mut child = Command::new(CHEBY)
        .args(["--color=never", "run"])
        .current_dir(&dir)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, cache.path())
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

/// Runs `cheby <command>` (`run` or `test`) on a package whose root
/// module is `source`, with standard error a pipe whose reading end is
/// closed before the program starts (so a write to it fails, D-306) and
/// `CHEBY_THREADS=4`. Returns the status (`None` if it had to be killed
/// after 30 s) and standard output.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to spawn or talk to `cheby` fails the test"
)]
fn with_broken_stderr(command: &str, name: &str, source: &str) -> (Option<i32>, String) {
    use std::io::Read as _;
    use std::process::Stdio;

    let dir = package(name, &[MANIFEST, ("src/main.cheby", source)]);
    let cache = fresh_cache();
    let mut child = Command::new(CHEBY)
        .args(["--color=never", command])
        .current_dir(&dir)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, cache.path())
        .env("CHEBY_THREADS", "4")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stderr.take());
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        stdout.read_to_string(&mut text).unwrap();
        text
    });
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        if start.elapsed() > Duration::from_secs(30) {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    (status, reader.join().unwrap())
}

/// D-306 and D-709: a detached fiber whose write to standard error fails
/// while `main` still runs ends the program at once with status 1, and
/// nothing more is printed: `main`, which waits in `channel::receive` for
/// good, never prints its last line, and no leak check runs.
#[test]
fn a_detached_write_failure_while_main_runs_exits_with_one() {
    let (status, stdout) = with_broken_stderr(
        "run",
        "detached_write_failure",
        "//! A detached fiber writes to a broken standard error while `main` waits.\n\n\
         import std::channel\nimport std::fiber\nimport std::io\n\n\
         fn main() {\n  let (keep, never) = channel::new::<Int>(1)\n  \
         io::println(\"main waits\")\n  \
         fiber::spawn_detached(fn() { io::eprintln(\"this line is lost\") })\n  \
         let got = channel::receive(never)\n  \
         io::println(\"never printed: {got:?}\")\n  \
         let _ = channel::send(keep, 0)\n}\n",
    );
    assert_eq!(stdout, "main waits\n");
    assert_eq!(status, Some(1));
}

/// D-785: a write that fails in a detached fiber after `main` has
/// returned, once standard output is flushed and the exit status fixed,
/// leaves that status (0 here) and prints nothing more.
///
/// The fiber writes a 4 MiB line to standard error, a pipe this test does
/// not read, so the write blocks inside the output call once the pipe is
/// full, where no safepoint or cancellation reaches it. `main` sleeps
/// 100 ms on a timer, prints its line to standard output and returns, and
/// the host flushes standard output and fixes the status 0. The exit
/// then waits for the fiber: a teardown (D-710) or a halt (D-073, D-708)
/// stops a fiber only at its next safepoint. The test checks that the
/// process is still waiting 300 ms after `main`'s line, then closes the
/// pipe, so the blocked write fails after the status was fixed.
#[test]
fn a_detached_write_failure_after_main_returned_keeps_the_status() {
    use std::io::{BufRead as _, BufReader, Read as _};
    use std::process::Stdio;

    let dir = package(
        "late_write_failure",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                "//! A detached fiber's write to standard error fails after `main` returned.\n\n\
                 import std::duration\nimport std::fiber\nimport std::io\nimport std::selector\n\
                 import std::string\n\n\
                 fn main() {\n  let line = string::repeat(\"x\", 4 * 1024 * 1024)\n  \
                 fiber::spawn_detached(fn() { io::eprintln(line) })\n  \
                 selector::new()\n  |> selector::after(_, duration::milliseconds(100), fn() { Nil })\n  \
                 |> selector::select\n  \
                 io::println(\"main returns\")\n}\n",
            ),
        ],
    );
    let cache = fresh_cache();
    let mut child = Command::new(CHEBY)
        .args(["--color=never", "run"])
        .current_dir(&dir)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, cache.path())
        .env("CHEBY_THREADS", "4")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let blocked = child.stderr.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    assert_eq!(first, "main returns\n");
    std::thread::sleep(Duration::from_millis(300));
    let waiting = child.try_wait().unwrap().is_none();
    drop(blocked);
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        if start.elapsed() > Duration::from_secs(30) {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    assert!(
        waiting,
        "the process exited before its blocked write failed: {status:?}"
    );
    assert_eq!(rest, "");
    assert_eq!(status, Some(0));
}

/// D-783: a write that fails during `cheby test` stops the run at once.
/// Nothing more is printed, not the failing test's line, not the next
/// test's, not the summary, and `cheby test` exits with 1.
#[test]
fn a_write_failure_ends_cheby_test_with_one() {
    let (status, stdout) = with_broken_stderr(
        "test",
        "test_write_failure",
        "//! The second test writes to a broken standard error.\n\n\
         import std::io\n\n\
         fn main() {\n  Nil\n}\n\n\
         test \"first\" {\n  assert 1 + 1 == 2\n}\n\n\
         test \"writes\" {\n  io::eprintln(\"this line is lost\")\n}\n\n\
         test \"third\" {\n  io::println(\"never printed\")\n}\n",
    );
    assert_eq!(stdout, "test app \"first\" ... ok\n");
    assert_eq!(status, Some(1));
}

/// D-711 and D-784: a debug build names the leaked runtime kinds on a line
/// after D-458's. A `Receiver` whose last reference sits in its own
/// channel's buffer is a cycle a program can build: the `Sender` goes and
/// closes the channel, but the buffer keeps `Mail(rx)` until the last
/// `Receiver` goes (D-670), which never happens. The leak is one
/// `<Receiver>` and one `Mail`. The bless guard keeps leak lines out of UI
/// goldens (D-445), so this test pins the line.
#[cfg(debug_assertions)]
#[test]
fn a_leaked_receiver_is_named_after_the_leak_line() {
    let dir = package(
        "leaked_kinds",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                "//! Leaks a receiver that its own channel's buffer holds.\n\nimport std::channel\n\ntype Mail {\n  Mail(channel::Receiver<Mail>)\n}\n\nfn main() {\n  let (tx, rx) = channel::new(1)\n  let _ = channel::send(tx, Mail(rx))\n  Nil\n}\n",
            ),
        ],
    );
    let out = cheby(&dir, &["run"]);
    assert_eq!(out.stdout, "");
    assert_eq!(
        out.stderr,
        "error: leak check failed: 2 objects are still live at exit\n\
         note: leaked 1 <Receiver>, and 1 object of other kinds\n"
    );
    assert_eq!(out.status, Some(102));
}

/// D-784 in `cheby test`: a passing test that leaks a runtime-kind object
/// fails with D-458's line followed by the kinds line in its failure
/// block, counted from the test's own delta (D-711).
#[cfg(debug_assertions)]
#[test]
fn a_test_that_leaks_a_receiver_names_it_in_its_block() {
    let dir = package(
        "leaked_kinds_test",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                "//! A test leaks a receiver that its own channel's buffer holds.\n\n\
                 import std::channel\n\n\
                 type Mail {\n  Mail(channel::Receiver<Mail>)\n}\n\n\
                 fn main() {\n  Nil\n}\n\n\
                 test \"leaks a receiver\" {\n  let (tx, rx) = channel::new(1)\n  \
                 assert channel::send(tx, Mail(rx)) == Ok(Nil)\n}\n",
            ),
        ],
    );
    let out = cheby(&dir, &["test"]);
    assert_eq!(
        out.stdout,
        "test app \"leaks a receiver\" ... FAILED\n\n\
         ---- app \"leaks a receiver\" ----\n\
         error: leak check failed: 2 objects are still live at exit\n\
         note: leaked 1 <Receiver>, and 1 object of other kinds\n\n\
         test result: FAILED. 0 passed; 1 failed; 0 filtered out\n"
    );
    assert_eq!(out.status, Some(1));
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
    let cache = fresh_cache();
    let mut cmd = Command::new(CHEBY);
    cmd.args(["--color=never", "repl"])
        .current_dir(&dir)
        .env("NO_COLOR", "1");
    cache.apply(&mut cmd);
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

/// Runs `cheby` in `dir` with `env` added and `stdin` piped in.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to spawn `cheby` fails the test"
)]
fn cheby_env(dir: &Path, args: &[&str], env: &[(&str, &str)], stdin: Option<&[u8]>) -> RunOutput {
    let cache = fresh_cache();
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never")
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .envs(env.iter().copied());
    cache.apply(&mut cmd);
    process::run(cmd, stdin, TIMEOUT).unwrap()
}

/// The `name: N` line of a `CHEBY_RT_STATS=1` run's standard error.
fn stat(stderr: &str, name: &str) -> Option<u64> {
    stderr.lines().find_map(|line| {
        line.trim()
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix(':'))
            .and_then(|n| n.trim().parse().ok())
    })
}

/// Reuse (D-716) in `std::sorted_map`'s `insert`, which rebuilds the
/// matched node in nested arms that never use the matched map (gate 6's
/// ROB6-1): with Perceus a loop of inserts and of replacing inserts counts
/// reuse hits and allocates far less than naive counting, and both print
/// the same. Run with poisoning off, so the inline reuse paths run.
#[test]
fn sorted_map_inserts_reuse_their_nodes() {
    let dir = package(
        "sorted_map_reuse",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                "//! Inserts into a sorted map, then replaces every value.\n\
                 \n\
                 import std::io\n\
                 import std::list\n\
                 import std::sorted_map\n\
                 \n\
                 fn main() {\n  \
                   let keys = list::range(0, 2000)\n  \
                   let grown = list::fold(keys, sorted_map::new(), fn(m, k) {\n    \
                     sorted_map::insert(m, k * 7919 % 2000, k)\n  \
                   })\n  \
                   let replaced = list::fold(keys, grown, fn(m, k) { sorted_map::insert(m, k, k + 1) })\n  \
                   let n = sorted_map::size(replaced)\n  \
                   let total = sorted_map::fold(replaced, 0, fn(acc, _, v) { acc + v })\n  \
                   io::println(\"{n} {total}\")\n\
                 }\n",
            ),
        ],
    );
    let run = |rc: Option<&str>| {
        let cache = fresh_cache();
        let mut cmd = Command::new(CHEBY);
        cmd.args(["--color=never", "run"])
            .current_dir(&dir)
            .env("NO_COLOR", "1")
            .env("CHEBY_RT_STATS", "1")
            .env("CHEBY_RT_POISON", "0")
            .env_remove("CHEBY_DUMP_DIR");
        cache.apply(&mut cmd);
        match rc {
            Some(mode) => cmd.env("CHEBY_RC", mode),
            None => cmd.env_remove("CHEBY_RC"),
        };
        let out = process::run(cmd, None, TIMEOUT).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(out.status, Some(0), "{}", out.stderr);
        assert_eq!(out.stdout, "2000 2001000\n");
        let allocations = stat(&out.stderr, "allocations");
        let hits = stat(&out.stderr, "reuse hits");
        (allocations.zip(hits), out.stderr)
    };
    let (Some((perceus, hits)), stderr) = run(None) else {
        panic!("no stats lines")
    };
    let (Some((naive, naive_hits)), _) = run(Some("naive")) else {
        panic!("no stats lines")
    };
    assert_eq!(naive_hits, 0, "naive counting never reuses");
    assert!(hits > 0, "no reuse hits:\n{stderr}");
    assert!(
        perceus.saturating_mul(4) < naive,
        "{perceus} allocations against naive's {naive}:\n{stderr}"
    );
}

/// Reuse (D-838, D-839) in `std::json`'s parser and writer, which keep the
/// open arrays and objects in a chain of cells and write each step's cell
/// over the old one (D-906). Nothing else would notice if an edit to
/// `std/json/tree.cheby` or `std/json.cheby` moved a rebuild out of the
/// block that takes its cell apart: the results stay the same, and only
/// the count of reuse hits drops. The document is an array of 1000
/// objects, each with three members, one of them an array of three
/// numbers. The parser rewrites the innermost cell once per value inside
/// a container (7000) and once more per member for its key (3000); the
/// writer rewrites it once per element or member after the first of each
/// array and object (999 + 2000 + 2000). Run with poisoning off, so the
/// inline reuse paths run.
#[test]
fn json_containers_reuse_their_cells() {
    // The parser's steps, then the writer's.
    const EXPECTED: u64 = 7000 + 3000 + 999 + 2000 + 2000;
    let dir = package(
        "json_reuse",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                r#"//! Reads a JSON document of 1000 objects and writes it back.

import std::io
import std::json
import std::list
import std::string

fn main() {
  let records = list::map(list::range(0, 1000), fn(n) {
    "\{\"id\": {n}, \"tags\": [1, 2, 3], \"ok\": true}"
  })
  let text = "[" + string::join(records, ", ") + "]"
  let assert Ok(value) = json::parse_value(text)
  let written = json::to_string(value)
  let size = string::length(written)
  io::println("{size}")
}
"#,
            ),
        ],
    );
    let cache = fresh_cache();
    let mut cmd = Command::new(CHEBY);
    cmd.args(["--color=never", "run"])
        .current_dir(&dir)
        .env("NO_COLOR", "1")
        .env("CHEBY_RT_STATS", "1")
        .env("CHEBY_RT_POISON", "0")
        .env_remove("CHEBY_RC")
        .env_remove("CHEBY_DUMP_DIR");
    cache.apply(&mut cmd);
    let out = process::run(cmd, None, TIMEOUT).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    assert_eq!(out.stdout, "35891\n");
    let (Some(hits), Some(misses)) = (
        stat(&out.stderr, "reuse hits"),
        stat(&out.stderr, "reuse misses"),
    ) else {
        panic!("no reuse lines:\n{}", out.stderr)
    };
    assert!(
        hits >= EXPECTED,
        "{hits} reuse hits, expected at least {EXPECTED}:\n{}",
        out.stderr
    );
    assert_eq!(misses, 0, "{}", out.stderr);
}

/// Pushes onto a record's list field whose other fields are read after
/// the push, the micro-benchmark of D-911: the reads move before the
/// push, so the record ends there and each push gets a unique list
/// and writes in place. Before, the record held the list across the push
/// and every push but the first copied it. Both RC modes see the moved
/// reads, so naive counting pushes in place too, without reuse. Field
/// accesses (`step`) and a record update (`update`) each push 2000 times.
/// Run with poisoning off, so the inline paths run.
#[test]
fn record_field_pushes_write_in_place() {
    let dir = package(
        "record_field_pushes",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                "//! Pushes onto a record's list field, reading its other fields after each push.\n\
                 \n\
                 import std::io\n\
                 import std::list\n\
                 \n\
                 type State { items: List<Int>, count: Int, sum: Int }\n\
                 \n\
                 fn step(s: State, n: Int) -> State {\n  \
                   case n {\n    \
                     0 => s\n    \
                     _ => {\n      \
                       let next = State { items: list::push(s.items, n), count: s.count + 1, sum: s.sum + n }\n      \
                       step(next, n - 1)\n    \
                     }\n  \
                   }\n\
                 }\n\
                 \n\
                 fn update(s: State, n: Int) -> State {\n  \
                   case n {\n    \
                     0 => s\n    \
                     _ => update(State { ..s, items: list::push(s.items, n), count: s.count + 1 }, n - 1)\n  \
                   }\n\
                 }\n\
                 \n\
                 fn main() {\n  \
                   let a = step(State { items: [], count: 0, sum: 0 }, 2000)\n  \
                   let b = update(State { items: [], count: 0, sum: 0 }, 2000)\n  \
                   let lengths = list::length(a.items) + list::length(b.items)\n  \
                   io::println(\"{lengths} {a.count} {a.sum} {b.count} {b.sum}\")\n\
                 }\n",
            ),
        ],
    );
    for rc in [None, Some("naive")] {
        let cache = fresh_cache();
        let mut cmd = Command::new(CHEBY);
        cmd.args(["--color=never", "run"])
            .current_dir(&dir)
            .env("NO_COLOR", "1")
            .env("CHEBY_RT_STATS", "1")
            .env("CHEBY_RT_POISON", "0")
            .env_remove("CHEBY_DUMP_DIR");
        cache.apply(&mut cmd);
        match rc {
            Some(mode) => cmd.env("CHEBY_RC", mode),
            None => cmd.env_remove("CHEBY_RC"),
        };
        let out = process::run(cmd, None, TIMEOUT).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(out.status, Some(0), "{rc:?}: {}", out.stderr);
        assert_eq!(out.stdout, "4000 2000 2001000 2000 0\n");
        assert_eq!(
            stat(&out.stderr, "list push copied"),
            Some(0),
            "{rc:?}: {}",
            out.stderr
        );
    }
}

/// A field every variant holds at the same place (D-071) is read without a
/// test of the variant, so the read proves no layout to reuse (the fix
/// review's TOK-1): a `Circle` rebuilt as a larger `Rect` in later arms,
/// and a `Rect` rebuilt as a smaller `Circle` in the read's block. With
/// poisoning on, a size mismatch would be a fatal error; with it off, the
/// inline reuse paths run. Both print what naive counting prints.
#[test]
fn a_shared_field_read_reuses_no_other_variant() {
    let widen = package(
        "shared_field_widen",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                "//! A small variant read through a shared field, rebuilt as a large one.\n\
                 \n\
                 import std::io\n\
                 import std::list\n\
                 \n\
                 type Shape {\n  \
                   Rect { name: String, width: Int, height: Int, depth: Int },\n  \
                   Circle { name: String, radius: Int },\n\
                 }\n\
                 \n\
                 fn widen(s: Shape, k: Int) -> Shape {\n  \
                   let n = s.name\n  \
                   case k % 3 {\n    \
                     0 => Rect { name: n, width: k, height: k + 1, depth: k + 2 }\n    \
                     1 => Rect { name: n, width: 0, height: 0, depth: k }\n    \
                     _ => Circle { name: n, radius: k }\n  \
                   }\n\
                 }\n\
                 \n\
                 fn total(shapes: List<Shape>) -> Int {\n  \
                   list::fold(shapes, 0, fn(acc, s) {\n    \
                     case s {\n      \
                       Circle { radius, .. } => acc + radius\n      \
                       Rect { width, height, depth, .. } => acc + width + height + depth\n    \
                     }\n  \
                   })\n\
                 }\n\
                 \n\
                 fn main() {\n  \
                   let shapes = list::map(list::range(0, 2000), fn(i) { widen(Circle { name: \"c\", radius: i }, i) })\n  \
                   let t = total(shapes)\n  \
                   io::println(\"{t}\")\n\
                 }\n",
            ),
        ],
    );
    let rename = package(
        "shared_field_rename",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                "//! A large variant read through a shared field, rebuilt as a small one.\n\
                 \n\
                 import std::io\n\
                 import std::list\n\
                 \n\
                 type Shape {\n  \
                   Circle { name: String, radius: Int },\n  \
                   Rect { name: String, width: Int, height: Int, depth: Int },\n\
                 }\n\
                 \n\
                 fn rename(s: Shape) -> Shape {\n  \
                   let n = s.name\n  \
                   Circle { name: n, radius: 1 }\n\
                 }\n\
                 \n\
                 fn main() {\n  \
                   let shapes = list::map(list::range(0, 1000), fn(i) {\n    \
                     case i % 2 {\n      \
                       0 => Rect { name: \"r\", width: i, height: i, depth: i }\n      \
                       _ => Circle { name: \"c\", radius: i }\n    \
                     }\n  \
                   })\n  \
                   let renamed = list::map(shapes, rename)\n  \
                   let total = list::fold(renamed, 0, fn(acc, s) {\n    \
                     case s {\n      \
                       Circle { radius, .. } => acc + radius\n      \
                       Rect { width, .. } => acc + width\n    \
                     }\n  \
                   })\n  \
                   io::println(\"{total}\")\n\
                 }\n",
            ),
        ],
    );
    for (dir, expected) in [(&widen, "3333667\n"), (&rename, "1000\n")] {
        for (rc, poison) in [("perceus", "1"), ("perceus", "0"), ("naive", "1")] {
            let env = [("CHEBY_RC", rc), ("CHEBY_RT_POISON", poison)];
            let out = cheby_env(dir, &["run"], &env, None);
            assert_eq!(
                (out.status, out.stdout.as_str()),
                (Some(0), expected),
                "{} with {rc}, poison {poison}:\n{}",
                dir.display(),
                out.stderr
            );
        }
    }
}

/// D-658: an invalid `CHEBY_THREADS` stops `cheby run` before `main`, and
/// `cheby test` and the REPL before they start, with the same line and
/// status 1.
#[test]
fn an_invalid_thread_count_stops_run_test_and_repl() {
    let line = "error: CHEBY_THREADS must be a whole number of at least 1, not \"four\"\n";
    let env = [("CHEBY_THREADS", "four")];
    let hello = repo_root().join("docs/examples/001_hello_world");
    let out = cheby_env(&hello, &["run"], &env, None);
    assert_eq!(out.stderr, line);
    assert_eq!(out.stdout, "");
    assert_eq!(out.status, Some(1));
    let dir = package(
        "threads_test",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                "//! One test.\n\nfn main() {\n  Nil\n}\n\ntest \"runs\" {\n  assert 1 + 1 == 2\n}\n",
            ),
        ],
    );
    let out = cheby_env(&dir, &["test"], &env, None);
    assert_eq!(out.stderr, line);
    assert_eq!(out.stdout, "");
    assert_eq!(out.status, Some(1));
    let out = cheby_env(&dir, &["repl"], &env, Some(b""));
    assert_eq!(out.stderr, line);
    assert_eq!(out.status, Some(1));
    // A valid count runs, including one above the cap.
    for threads in ["1", "2", "9999"] {
        let out = cheby_env(&hello, &["run"], &[("CHEBY_THREADS", threads)], None);
        assert_eq!(out.stdout, "Hello, world!\n", "{threads}");
        assert_eq!(out.status, Some(0), "{threads}: {}", out.stderr);
        let out = cheby_env(&dir, &["test"], &[("CHEBY_THREADS", threads)], None);
        assert_eq!(out.status, Some(0), "{threads}: {}", out.stdout);
    }
}

/// D-742: the REPL compiles with the unwind edges `CHEBY_UNWIND_EDGES`
/// selects, so an invalid value stops it when the session starts, with
/// the MIR options' error.
#[test]
fn an_invalid_unwind_edges_switch_stops_the_repl() {
    let dir = Path::new(TMP).join(format!("repl-unwind-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = cheby_env(
        &dir,
        &["repl"],
        &[("CHEBY_UNWIND_EDGES", "bogus")],
        Some(b"1 + 1\n"),
    );
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        out.stderr,
        "error: CHEBY_UNWIND_EDGES must be off or empty, not \"bogus\"\n"
    );
    assert_eq!(out.stdout, "");
    assert_eq!(out.status, Some(1));
}

/// The fatal line of a raise in code compiled without unwind edges
/// (D-770).
const EDGELESS: &str = "fatal runtime error: an unwinding in code compiled without unwind edges (CHEBY_UNWIND_EDGES=off)\n";

/// D-742: `CHEBY_UNWIND_EDGES=off` compiles without the unwind pass, and a
/// program that does not panic runs as it does with it.
#[test]
fn hello_world_runs_without_unwind_edges() {
    let out = cheby_env(
        &repo_root().join("docs/examples/001_hello_world"),
        &["run"],
        &[("CHEBY_UNWIND_EDGES", "off")],
        None,
    );
    assert_eq!(out.stderr, "");
    assert_eq!(out.stdout, "Hello, world!\n");
    assert_eq!(out.status, Some(0));
}

/// D-770: a panic in code compiled with
/// `CHEBY_UNWIND_EDGES=off` flushes standard output, prints its report,
/// then the fatal line, and ends the process with a nonzero status, never
/// by a signal: the code has no edges to unwind by.
#[test]
fn a_panic_without_unwind_edges_is_fatal_with_its_report() {
    let dir = package(
        "edgeless_panic",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                "//! Prints, then panics.\n\nimport std::io\n\nfn main() {\n  io::println(\"before\")\n  panic as \"stop here\"\n}\n",
            ),
        ],
    );
    let out = cheby_env(&dir, &["run"], &[("CHEBY_UNWIND_EDGES", "off")], None);
    assert_eq!(out.stdout, "before\n", "{}", out.stderr);
    assert!(
        out.stderr
            .starts_with("panicked at src/main.cheby:7:3:\nstop here\n"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.ends_with(EDGELESS), "{}", out.stderr);
    assert!(
        matches!(out.status, Some(code) if code != 0),
        "{}: {}",
        out.describe_status(),
        out.stderr
    );
    // With the edges, the same program ends with the panic's status.
    let out = cheby(&dir, &["run"]);
    assert_eq!(out.stdout, "before\n");
    assert!(!out.stderr.contains("fatal"), "{}", out.stderr);
    assert_eq!(out.status, Some(101), "{}", out.stderr);
}

/// D-770: constant evaluation carries the setting like
/// `cheby run`, so under `CHEBY_UNWIND_EDGES=off` `cheby check` of a
/// package whose constant panics ends with the fatal line and a nonzero
/// status, never by a signal.
#[test]
fn a_panicking_constant_without_unwind_edges_is_fatal() {
    let dir = package(
        "edgeless_const",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                "//! A constant that panics.\n\nimport std::io\n\nconst ratio: Int = 100 / parts()\n\nfn parts() -> Int {\n  0\n}\n\nfn main() {\n  io::println(\"main ran: {ratio}\")\n}\n",
            ),
        ],
    );
    let out = cheby_env(&dir, &["check"], &[("CHEBY_UNWIND_EDGES", "off")], None);
    assert!(out.stderr.contains(EDGELESS), "{}", out.stderr);
    assert!(
        matches!(out.status, Some(code) if code != 0),
        "{}: {}",
        out.describe_status(),
        out.stderr
    );
    // With the edges, the panic is E0075 at the constant (D-533).
    let out = cheby(&dir, &["check"]);
    assert!(
        out.stderr
            .starts_with("error[E0075]: evaluating the constant `ratio` panicked\n"),
        "{}",
        out.stderr
    );
    assert_eq!(out.status, Some(1), "{}", out.stderr);
}

/// Runs `cheby <command>` (`run` or `test`) on a package whose root
/// module is `source`, at `CHEBY_THREADS=threads`, with standard input a
/// pipe that stays open and never gets data. Returns the status (`None` if
/// it had to be killed after 30 s) and standard output.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to spawn or talk to `cheby` fails the test"
)]
fn with_open_stdin(
    command: &str,
    name: &str,
    source: &str,
    threads: &str,
) -> (Option<i32>, String) {
    use std::io::Read as _;
    use std::process::Stdio;

    let dir = package(name, &[MANIFEST, ("src/main.cheby", source)]);
    let cache = fresh_cache();
    let mut child = Command::new(CHEBY)
        .args(["--color=never", command])
        .current_dir(&dir)
        .env("NO_COLOR", "1")
        .env(CACHE_ENV, cache.path())
        .env("CHEBY_THREADS", threads)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take();
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        stdout.read_to_string(&mut text).unwrap();
        text
    });
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        if start.elapsed() > Duration::from_secs(30) {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    drop(stdin);
    (status, reader.join().unwrap())
}

/// A detached fiber that waits in a scope's close for a scoped child
/// blocked in `io::read_line`, in `main` and in a test.
const SCOPED_READER: &str = "//! A detached fiber waits for a scoped child blocked in `io::read_line`.\n\n\
    import std::channel\nimport std::duration\nimport std::fiber\nimport std::io\n\n\
    fn leave_a_reader() -> Nil {\n  fiber::spawn_detached(fn() {\n    let _ = {\n      \
    use owner <- fiber::scope()\n      let _ = fiber::spawn(owner, fn() { io::read_line() })\n      \
    Nil\n    }\n    Nil\n  })\n  let (keep, never) = channel::new::<Nil>(1)\n  \
    let _ = fiber::timeout(duration::milliseconds(200), fn() { channel::receive(never) })\n  \
    let _ = channel::send(keep, Nil)\n  Nil\n}\n\n\
    fn main() {\n  leave_a_reader()\n  io::println(\"main done\")\n}\n\n\
    test \"a test leaves a reader\" {\n  leave_a_reader()\n  assert True\n}\n\n\
    test \"the next test runs\" {\n  assert 1 + 1 == 2\n}\n";

/// D-786 (review finding CONC-W2-1 of gate 2): the teardown skips a
/// detached fiber that waits for a scoped child blocked in
/// `io::read_line`, so `cheby run` exits with 0 while standard input stays
/// open. Before the fix it waited for the read. At every thread count,
/// one included: the child waits on the blocking pool, which holds no
/// worker (D-683, D-814).
#[test]
fn a_detached_fiber_waiting_for_a_blocked_reader_does_not_hold_up_run() {
    for threads in ["1", "2", "4"] {
        let (status, stdout) = with_open_stdin("run", "scoped_reader_run", SCOPED_READER, threads);
        assert_eq!(status, Some(0), "CHEBY_THREADS={threads}: {stdout}");
        assert_eq!(stdout, "main done\n", "CHEBY_THREADS={threads}");
    }
}

/// D-786, D-782: the same in `cheby test`: the first test's teardown
/// skips the detached fiber, the next test runs, and `cheby test` exits
/// with 0 (at every thread count, as above).
#[test]
fn a_detached_fiber_waiting_for_a_blocked_reader_does_not_hold_up_test() {
    for threads in ["1", "2", "4"] {
        let (status, stdout) =
            with_open_stdin("test", "scoped_reader_test", SCOPED_READER, threads);
        assert_eq!(status, Some(0), "CHEBY_THREADS={threads}: {stdout}");
        assert!(
            stdout.contains("\"the next test runs\" ... ok"),
            "CHEBY_THREADS={threads}: {stdout}"
        );
    }
}

/// A detached reader that wakes `main`, then blocks in `io::read_line`.
const SIGNALLING_READER: &str = "//! A detached reader signals `main`, then blocks in `io::read_line`.\n\n\
    import std::channel\nimport std::fiber\nimport std::io\n\n\
    fn reader(ready: channel::Sender<Nil>) -> Nil {\n  \
    let assert Ok(Nil) = channel::send(ready, Nil)\n  let _ = io::read_line()\n  Nil\n}\n\n\
    fn main() {\n  let (ready_tx, ready_rx) = channel::new::<Nil>(1)\n  \
    fiber::spawn_detached(fn() { reader(ready_tx) })\n  \
    let assert Ok(Nil) = channel::receive(ready_rx)\n  io::println(\"main returns\")\n}\n\n\
    test \"a test leaves a reader\" {\n  let (ready_tx, ready_rx) = channel::new::<Nil>(1)\n  \
    fiber::spawn_detached(fn() { reader(ready_tx) })\n  \
    assert channel::receive(ready_rx) == Ok(Nil)\n}\n\n\
    test \"the next test runs\" {\n  assert 1 + 1 == 2\n}\n";

/// D-786 (review finding ROB2-1 of gate 2): a fiber that wakes `main` and
/// then blocks in `io::read_line` leaves `main` running, so `main` prints
/// and the program exits with 0 while standard input stays open. Before
/// the fix `main` waited in the blocked worker's runnext slot until the
/// read returned. The reader now parks on the blocking pool and its worker
/// runs `main`, at every thread count, one included (D-683); a reader
/// blocked inline, after a refused pool thread, still hands `main` to
/// another worker first (D-817).
#[test]
fn a_reader_that_wakes_main_then_blocks_hands_main_off() {
    for threads in ["1", "2", "4"] {
        let (status, stdout) =
            with_open_stdin("run", "signalling_reader_run", SIGNALLING_READER, threads);
        assert_eq!(status, Some(0), "CHEBY_THREADS={threads}: {stdout}");
        assert_eq!(stdout, "main returns\n", "CHEBY_THREADS={threads}");
        let (status, stdout) =
            with_open_stdin("test", "signalling_reader_test", SIGNALLING_READER, threads);
        assert_eq!(status, Some(0), "CHEBY_THREADS={threads}: {stdout}");
        assert!(
            stdout.contains("\"the next test runs\" ... ok"),
            "CHEBY_THREADS={threads}: {stdout}"
        );
    }
}

/// ROB2-1: in a reader pipeline, a line is echoed as soon as it arrives,
/// not when the next one does: the reader sends the line over a
/// capacity-0 channel, which wakes `main` into its worker's runnext slot,
/// and then waits in `io::read_line` for the next line, parked on the
/// blocking pool (D-683), at every thread count.
#[test]
fn a_reader_pipeline_echoes_each_line_before_the_next_arrives() {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::process::Stdio;

    let dir = package(
        "reader_pipeline",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                "//! A reader fiber sends each line to `main`, which echoes it.\n\n\
                 import std::channel\nimport std::fiber\nimport std::io\n\n\
                 fn read_all(lines: channel::Sender<String>) -> Nil {\n  case io::read_line() {\n    \
                 Ok(Some(line)) => {\n      let assert Ok(Nil) = channel::send(lines, line)\n      \
                 read_all(lines)\n    }\n    _ => Nil\n  }\n}\n\n\
                 fn echo(lines: channel::Receiver<String>) -> Nil {\n  case channel::receive(lines) {\n    \
                 Ok(line) => {\n      io::println(\"echo {line}\")\n      echo(lines)\n    }\n    \
                 Err(_) => Nil\n  }\n}\n\n\
                 fn main() {\n  let (lines_tx, lines_rx) = channel::new::<String>(0)\n  let _ = {\n    \
                 use owner <- fiber::scope()\n    let _ = fiber::spawn(owner, fn() { read_all(lines_tx) })\n    \
                 echo(lines_rx)\n  }\n  Nil\n}\n",
            ),
        ],
    );
    for threads in ["1", "2", "4"] {
        let cache = fresh_cache();
        let mut child = Command::new(CHEBY)
            .args(["--color=never", "run"])
            .current_dir(&dir)
            .env("NO_COLOR", "1")
            .env(CACHE_ENV, cache.path())
            .env("CHEBY_THREADS", threads)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (lines, received) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in stdout.lines() {
                let Ok(line) = line else { break };
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        let mut echoed = Vec::new();
        for word in ["one", "two"] {
            stdin.write_all(format!("{word}\n").as_bytes()).unwrap();
            stdin.flush().unwrap();
            // The next line is not sent before this one is echoed.
            let line = received.recv_timeout(Duration::from_secs(30));
            echoed.push(line.clone());
            if line.as_deref() != Ok(&format!("echo {word}")) {
                child.kill().ok();
                child.wait().ok();
                panic!("CHEBY_THREADS={threads}: {word} was not echoed: {echoed:?}");
            }
        }
        drop(stdin);
        let status = child.wait().unwrap();
        reader.join().unwrap();
        assert_eq!(status.code(), Some(0), "CHEBY_THREADS={threads}");
    }
}

/// §10.4's example, verbatim: the root fiber still holds the `Sender`, so
/// its receive can never complete.
const DEADLOCK_EXAMPLE: &str = "import std::channel\n\n\
     fn main() {\n  let (tx, rx) = channel::new::<Int>(0)\n  \
     let assert Ok(n) = channel::receive(rx)\n  let _ = channel::send(tx, n)\n}\n";

/// The report §10.4 gives for [`DEADLOCK_EXAMPLE`] (D-856, D-858).
const DEADLOCK_EXAMPLE_REPORT: &str =
    "fatal error: all fibers are blocked\n  blocked in channel::receive at src/main.cheby:5:22\n";

/// Two scoped fibers each wait to receive from the other, and `main` waits
/// at the end of their scope: both fibers wait at the same call.
const DEADLOCK_TWO_FIBERS: &str = "//! Two fibers wait on each other.\n\n\
     import std::channel\nimport std::fiber\n\n\
     fn main() {\n  let (ping_tx, ping_rx) = channel::new::<Int>(0)\n  \
     let (pong_tx, pong_rx) = channel::new::<Int>(0)\n  let outcome = {\n    \
     use owner <- fiber::scope()\n    fiber::spawn(owner, fn() { relay(ping_rx, pong_tx) })\n    \
     fiber::spawn(owner, fn() { relay(pong_rx, ping_tx) })\n    Nil\n  }\n  let _ = outcome\n}\n\n\
     fn relay(from: channel::Receiver<Int>, to: channel::Sender<Int>) -> Result<Nil, Int> {\n  \
     let assert Ok(n) = channel::receive(from)\n  channel::send(to, n + 1)\n}\n";

/// The report of [`DEADLOCK_TWO_FIBERS`]: sorted by location, then by
/// operation, with equal lines kept (D-858).
const DEADLOCK_TWO_FIBERS_REPORT: &str = "fatal error: all fibers are blocked\n  \
     blocked in fiber::scope at src/main.cheby:10:5\n  \
     blocked in channel::receive at src/main.cheby:19:22\n  \
     blocked in channel::receive at src/main.cheby:19:22\n";

/// The environments every deadlock test runs its program in: the thread
/// count inherited, 1, 2 and 4 workers, and 4 workers under D-687's
/// migration stress. The report is the same in each (D-722, D-858).
const DEADLOCK_ENVS: [&[(&str, &str)]; 5] = [
    &[],
    &[("CHEBY_THREADS", "1")],
    &[("CHEBY_THREADS", "2")],
    &[("CHEBY_THREADS", "4")],
    &[("CHEBY_THREADS", "4"), ("CHEBY_RT_STRESS_MIGRATE", "1")],
];

/// D-724, D-856, D-858, D-859: §10.4's example, run as a package, writes
/// exactly the report the spec gives, at `src/main.cheby:5:22`, prints
/// nothing to standard output and exits with status 103, in every build
/// (a `--release` run of this test drives a release `cheby`).
#[test]
fn deadlock_the_spec_example_exits_with_103() {
    let dir = package(
        "deadlock_example",
        &[MANIFEST, ("src/main.cheby", DEADLOCK_EXAMPLE)],
    );
    let out = cheby(&dir, &["run"]);
    assert_eq!(out.stderr, DEADLOCK_EXAMPLE_REPORT);
    assert_eq!(out.stdout, "");
    assert_eq!(out.status, Some(103), "{}", out.describe_status());
}

/// D-722, D-858: the report is the same at every thread count and under
/// migration stress.
#[test]
fn deadlock_the_spec_example_reports_the_same_at_every_thread_count() {
    let dir = package(
        "deadlock_example_threads",
        &[MANIFEST, ("src/main.cheby", DEADLOCK_EXAMPLE)],
    );
    for env in DEADLOCK_ENVS {
        let out = cheby_env(&dir, &["run"], env, None);
        assert_eq!(out.stderr, DEADLOCK_EXAMPLE_REPORT, "{env:?}");
        assert_eq!(out.stdout, "", "{env:?}");
        assert_eq!(out.status, Some(103), "{env:?}: {}", out.describe_status());
    }
}

/// D-858: two fibers blocked at the same call give two equal lines, after
/// `main`'s at its scope, whichever fiber parked first, at every thread
/// count and under migration stress.
#[test]
fn deadlock_of_two_fibers_is_reported_in_order() {
    let dir = package(
        "deadlock_two_fibers",
        &[MANIFEST, ("src/main.cheby", DEADLOCK_TWO_FIBERS)],
    );
    for env in DEADLOCK_ENVS {
        let out = cheby_env(&dir, &["run"], env, None);
        assert_eq!(out.stderr, DEADLOCK_TWO_FIBERS_REPORT, "{env:?}");
        assert_eq!(out.status, Some(103), "{env:?}: {}", out.describe_status());
    }
}

/// D-859: `cheby run` halts after a deadlock with nothing unwinding, so no
/// leak check runs (a debug build would otherwise find the channel and
/// its handles live, D-458) and no `CHEBY_RT_STATS` report follows, as
/// after a termination: standard error holds the report and nothing else.
#[test]
fn deadlock_is_followed_by_no_leak_line_and_no_stats_report() {
    let dir = package(
        "deadlock_no_stats",
        &[MANIFEST, ("src/main.cheby", DEADLOCK_EXAMPLE)],
    );
    for threads in ["1", "2"] {
        let env = [("CHEBY_RT_STATS", "1"), ("CHEBY_THREADS", threads)];
        let out = cheby_env(&dir, &["run"], &env, None);
        assert_eq!(out.stderr, DEADLOCK_EXAMPLE_REPORT, "{env:?}");
        assert_eq!(out.status, Some(103), "{env:?}: {}", out.describe_status());
    }
}

/// The `[const-eval]` memory limit of the verdict test.
const VERDICT_LIMIT: &str = "256KiB";

/// A list length whose constant's peak is a little under
/// [`VERDICT_LIMIT`] on `plain` code, and one a little over it (D-617).
/// At K5a the verdict turned at 28 945 elements (28 944 passed), on
/// aarch64 macOS's debug toolchain: about 3% on each side.
const VERDICT_UNDER: u32 = 28_000;
const VERDICT_OVER: u32 = 30_000;

/// The E0076 report in `stderr`: its lines from `error[E0076]` up to the
/// first empty line, or `None` without one.
fn e0076(stderr: &str) -> Option<String> {
    let (_, from) = stderr.split_once("error[E0076]")?;
    let block = from.split("\n\n").next().unwrap_or(from);
    Some(format!("error[E0076]{block}"))
}

/// D-978 (wave 5's Open item): every command evaluates constants on the
/// same `plain` code, so a constant near `[const-eval] memory-limit` gets
/// one verdict from `cheby check` then `cheby run`, from `cheby run` then
/// `cheby check`, and from `cheby test` and `cheby build`: none just under
/// the limit, and the same E0076 text just over it. Each command gets a
/// fresh cache (D-1128); until K5b caches results, the two orders are the
/// two command orders.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn const_verdict_near_the_memory_limit_is_one_across_commands() {
    for (n, over) in [(VERDICT_UNDER, false), (VERDICT_OVER, true)] {
        let main = format!(
            "//! A constant whose peak is near the memory limit.\n\nimport std::list\n\n\
             const held: Int = list::length(list::range(0, {n}))\n\n\
             fn main() {{\n  assert held == {n}\n}}\n\n\
             test \"held\" {{\n  assert held == {n}\n}}\n"
        );
        let manifest = format!(
            "name = \"app\"\nversion = \"0.1.0\"\n\n[const-eval]\nmemory-limit = \"{VERDICT_LIMIT}\"\n"
        );
        let dir = package(
            &format!("const_verdict_{n}"),
            &[("cheby.toml", &manifest), ("src/main.cheby", &main)],
        );
        let exe = dir.join("out").join("app");
        let exe = exe.to_str().unwrap();
        let mut verdicts = Vec::new();
        for order in [
            &[&["check"][..], &["run"][..]][..],
            &[&["run"][..], &["check"][..]],
            &[&["test"][..]],
            &[&["build", "-o", exe][..]],
        ] {
            for args in order {
                let out = cheby(&dir, args);
                let verdict = e0076(&out.stderr);
                let status = i32::from(verdict.is_some());
                assert_eq!(out.status, Some(status), "{args:?} {n}: {}", out.stderr);
                verdicts.push((args.first().copied().unwrap_or(""), verdict));
            }
        }
        let first = verdicts.first().map(|(_, v)| v.clone()).unwrap();
        assert_eq!(first.is_some(), over, "{n}: {first:?}");
        for (command, verdict) in &verdicts {
            assert_eq!(verdict, &first, "{command} {n}");
        }
        if let Some(text) = &first {
            assert!(
                text.contains(&format!("the memory limit of {VERDICT_LIMIT}")),
                "{text}"
            );
        }
    }
}
