//! Language server harness: runs `cheby lsp` as an editor would, over
//! stdio, one process per trial (D-1352, docs/plan/testing.md,
//! `tests/lsp/README.md`). The client is `cheby_test_support::lsp`.
//!
//! Four groups of trials:
//!
//! - `protocol/<name>`: the handshake, `shutdown` and `exit` and their exit
//!   codes, the end of input, error answers (`MethodNotFound`,
//!   `ServerNotInitialized`, `InvalidRequest` after `shutdown`),
//!   `$/cancelRequest`, malformed params, the negotiated position encoding
//!   (D-1368), the publishing rules (D-1373), and that standard output
//!   holds frames only (A §3.7). Two of them compare the `initialize`
//!   answer with a golden: `tests/lsp/protocol/<name>.expected`.
//! - `examples/<name>`: every example's sources are opened, and every
//!   request the server advertises is sent at every identifier and type
//!   name. Each must be answered in time, with no JSON-RPC error and no
//!   exit of the server; `null` is an answer. Formatting must return no
//!   edit, since every example is a `cheby fmt` fixed point. The group
//!   tightens as the server advertises more (D-1379).
//! - `parity/examples/<name>` and `parity/ui/<name>`: the published
//!   diagnostics equal `cheby check`'s (D-1348), for an example by running
//!   `cheby check`, for a UI check test by reading its `.stderr` golden.
//!   Only the root module is opened, so the other files are published as
//!   closed files. When a file has a phase-A error, only the files with
//!   one and `cheby.toml` are compared (D-1347). UI tests with `@flags`
//!   and run tests are not part of it, nor goldens with an error that has
//!   no code. Goldens with a back-end code (`ui::BACK_END_CODES`) are
//!   ignored until the server's constants child lands
//!   ([`PARITY_SKIPS_BACK_END`], D-1349).
//! - `golden/<name>`: the tests under `tests/lsp/`, whose exact answers are
//!   in `<name>.expected`. `CHEBY_BLESS=1` rewrites those goldens, and the
//!   two protocol goldens, except for a test of a later milestone. It
//!   ignores every trial that has no golden.
//!
//! Milestones (D-1352, D-288, D-544): protocol trials and golden tests
//! carry the header default, M1, so they are binding while
//! `tests/milestone` names a later one; example trials carry the example's
//! milestone, with a `skip-run` example of a later milestone tolerated, as
//! the conformance harness's check-only trial is (030); UI parity trials
//! carry the UI test's. `CHEBY_MILESTONE`, `CHEBY_PENDING=require` and
//! `CHEBY_PENDING_ALLOW` make the strict runs of D-544.
//!
//! Every `cheby` gets the fresh `CHEBY_CACHE` of this process (D-959,
//! D-1128), `NO_COLOR=1`, `CHEBY_RT_POISON=1`, and none of `CHEBY_BACKTRACE`
//! (D-1043). `CHEBY_HARNESS_SCALE` multiplies the timeout of each wait
//! (D-722). Packages are copied into a scratch directory of this process
//! under `CARGO_TARGET_TMPDIR`.
//!
//! Windows (D-545, D-641): a trial that runs `cheby check` on a package
//! that may pass the front end is ignored until M6, since `cheby check`
//! then builds a JIT for the package's constants. That is every
//! `parity/examples/*` trial. The UI parity trials whose golden holds no
//! front-end error are ignored too, as their check tests are
//! (`UiTest::builds_a_jit`). The server process never runs generated
//! code (D-1349), so every other trial runs there.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::time::Duration;

use cheby_test_support::cache::HarnessCache;
use cheby_test_support::example::{self, Example};
use cheby_test_support::lsp::case::{self, Advertised, Layout, LspTest};
use cheby_test_support::lsp::client::{self, codes};
use cheby_test_support::lsp::parity::{self, Key};
use cheby_test_support::lsp::serde_json::{self, Value, json};
use cheby_test_support::lsp::text::{Bom, Lines, Position};
use cheby_test_support::lsp::{Client, ClientKind, Encoding, Ended, Paths, StartOptions};
use cheby_test_support::milestone::{Expectation, Milestone, POISON_ENV, Setting, Verdicts};
use cheby_test_support::ui::{self, Mode, UiTest};
use cheby_test_support::{
    bless_enabled, files_with_extension, golden, platform, process, repo_root,
};
use libtest_mimic::{Arguments, Failed, Trial};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
/// The deadline of each wait, before `CHEBY_HARNESS_SCALE` (D-722).
const TIMEOUT: Duration = Duration::from_secs(60);
/// Until the constants child of W9, the server reports no back-end code,
/// so parity trials of goldens that hold one are ignored (D-1348, D-1349).
const PARITY_SKIPS_BACK_END: bool = true;
/// Requests the examples group sends per batch.
const BATCH: usize = 256;
/// The package name a UI single-file test is copied into, as the UI
/// harness does, since messages may name it.
const UI_PACKAGE: &str = "ui_test";
/// The root module, which parity trials open.
const ROOT_MODULE: &str = "src/main.cheby";

/// What every trial needs.
#[derive(Debug)]
struct Env {
    root: PathBuf,
    cache: HarnessCache,
    timeout: Duration,
}

fn main() -> ExitCode {
    let args = Arguments::from_args();
    let root = repo_root();
    let setup = Setting::read(&root).and_then(|m| {
        Ok((
            m,
            example::all(&root)?,
            ui::all(&root)?,
            case::all(&root)?,
            Verdicts::from_env()?,
            process::scaled_timeout(TIMEOUT)?,
            // D-959: one fresh cache for every child of this process.
            HarnessCache::create("lsp")?,
        ))
    });
    let (current, examples, ui_tests, golden_tests, verdicts, timeout, cache) = match setup {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("LSP harness: {message}");
            return ExitCode::from(2);
        }
    };
    let bless = bless_enabled();
    eprintln!("LSP harness: {}", cache.describe());
    if let Some(reason) = platform::run_skip_reason() {
        eprintln!(
            "LSP harness: trials that run `cheby check` on a package that may pass the front end \
             are ignored: {reason}"
        );
    }
    let env = Arc::new(Env {
        root: root.clone(),
        cache,
        timeout,
    });
    let mut trials = Vec::new();
    for protocol in PROTOCOL {
        trials.push(protocol_trial(protocol, &env, current, &verdicts, bless));
    }
    for ex in &examples {
        trials.push(example_trial(ex.clone(), &env, current, &verdicts, bless));
    }
    for ex in &examples {
        trials.push(example_parity_trial(
            ex.clone(),
            &env,
            current,
            &verdicts,
            bless,
        ));
    }
    for test in ui_tests {
        if let Some(trial) = ui_parity_trial(test, &env, current, &verdicts, bless) {
            trials.push(trial);
        }
    }
    for test in golden_tests {
        trials.push(golden_trial(test, &env, current, &verdicts, bless));
    }
    let conclusion = libtest_mimic::run(&args, trials);
    let _ = std::fs::remove_dir(scratch_root());
    verdicts.report("LSP trials");
    conclusion.exit_code()
}

/// Wraps `body` as a trial judged against the milestone.
fn trial(
    name: String,
    milestone: Milestone,
    expectation: Expectation,
    verdicts: &Verdicts,
    ignored: bool,
    body: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Trial {
    let verdicts = verdicts.clone();
    let judged = name.clone();
    Trial::test(name, move || {
        verdicts
            .judge(&judged, milestone, expectation, body())
            .map_err(Failed::from)
    })
    .with_kind(expectation.kind())
    .with_ignored_flag(ignored)
}

// ---------------------------------------------------------------------
// Scratch packages

/// This process's scratch directory, so concurrent runs never share one.
fn scratch_root() -> PathBuf {
    Path::new(TMP).join(format!("lsp-{}", std::process::id()))
}

/// A fresh, empty directory for one trial, removed when dropped.
#[derive(Debug)]
struct Scratch(PathBuf);

impl Scratch {
    fn new(trial: &str) -> Result<Self, String> {
        let dir = scratch_root().join(trial.replace('/', "__"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        // Canonical, so the URIs the client writes match what the server
        // may report after resolving symbolic links. Not on Windows, where
        // `canonicalize` gives a verbatim path, `\\?\C:\…`, that reads as
        // a UNC host in a URI.
        let dir = if cfg!(windows) {
            dir
        } else {
            dir.canonicalize().unwrap_or(dir)
        };
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Copies `from` into `to`, skipping hidden entries and `build/`.
fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let entries = std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", from.display()))?;
        let name = entry.file_name();
        let shown = name.to_string_lossy();
        if shown.starts_with('.') || shown == "build" {
            continue;
        }
        let (source, target) = (entry.path(), to.join(&name));
        if source.is_dir() {
            copy_dir(&source, &target)?;
        } else {
            std::fs::copy(&source, &target).map_err(|e| format!("{}: {e}", source.display()))?;
        }
    }
    Ok(())
}

/// Writes a package of one module, `src/main.cheby`, from `source`.
fn single_file_package(dir: &Path, name: &str, source: &Path) -> Result<(), String> {
    let src = dir.join("src");
    std::fs::create_dir_all(&src).map_err(|e| format!("{}: {e}", src.display()))?;
    std::fs::write(
        dir.join("cheby.toml"),
        format!("name = \"{name}\"\nversion = \"0.0.0\"\n"),
    )
    .map_err(|e| e.to_string())?;
    std::fs::copy(source, src.join("main.cheby"))
        .map_err(|e| format!("{}: {e}", source.display()))?;
    Ok(())
}

/// Writes `files`, relative paths and texts, into `dir`.
fn write_package(dir: &Path, files: &[(&str, &str)]) -> Result<(), String> {
    for (path, text) in files {
        let path = dir.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// The `.cheby` files of the package at `dir`, relative to it with `/`.
fn sources(dir: &Path) -> Result<Vec<String>, String> {
    let files = files_with_extension(&dir.join("src"), "cheby")
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(files
        .iter()
        .map(|f| cheby_test_support::display_relative(f, dir))
        .collect())
}

// ---------------------------------------------------------------------
// Sessions

/// Starts a server in `dir` and initializes it with `dir` as the only
/// workspace folder.
fn session(
    env: &Env,
    dir: &Path,
    kind: ClientKind,
    encoding: Option<Encoding>,
    settings: Option<Value>,
) -> Result<(Client, Value), String> {
    let mut client = start(env, dir)?;
    if let Some(settings) = &settings {
        client.set_configuration(settings.clone());
    }
    let params = client.initialize_params(&kind.capabilities(encoding), settings)?;
    let init = client.initialize(params)?;
    Ok((client, init))
}

fn start(env: &Env, dir: &Path) -> Result<Client, String> {
    start_with(env, dir, &[])
}

/// `cheby lsp` with `args` after `lsp`.
fn start_with(env: &Env, dir: &Path, args: &[&str]) -> Result<Client, String> {
    Client::start(StartOptions {
        cheby: Path::new(CHEBY),
        root: dir,
        cache: &env.cache,
        timeout: env.timeout,
        args,
    })
}

/// Opens each of `files` and waits for its first publish, version 1.
fn open_all(client: &mut Client, files: &[String]) -> Result<Vec<String>, String> {
    let mut uris = Vec::with_capacity(files.len());
    for file in files {
        uris.push(client.open(Path::new(file))?);
    }
    for uri in &uris {
        client.diagnostics(uri, Some(1))?;
    }
    Ok(uris)
}

// ---------------------------------------------------------------------
// protocol/*

/// One protocol trial.
struct Protocol {
    name: &'static str,
    run: fn(&Env, &Path) -> Result<(), String>,
    /// Whether it has a golden under `tests/lsp/protocol/`.
    golden: bool,
}

const PROTOCOL: &[Protocol] = &[
    Protocol {
        name: "initialize_answers_capabilities",
        run: initialize_answers_capabilities,
        golden: false,
    },
    Protocol {
        name: "position_encoding_negotiation",
        run: position_encoding_negotiation,
        golden: false,
    },
    Protocol {
        name: "capabilities_with_empty_client",
        run: capabilities_with_empty_client,
        golden: true,
    },
    Protocol {
        name: "capabilities_with_full_client",
        run: capabilities_with_full_client,
        golden: true,
    },
    Protocol {
        name: "shutdown_then_exit_gives_0",
        run: shutdown_then_exit_gives_0,
        golden: false,
    },
    Protocol {
        name: "exit_without_shutdown_gives_1",
        run: exit_without_shutdown_gives_1,
        golden: false,
    },
    Protocol {
        name: "end_of_input_gives_1",
        run: end_of_input_gives_1,
        golden: false,
    },
    Protocol {
        name: "request_before_initialize_is_server_not_initialized",
        run: request_before_initialize,
        golden: false,
    },
    Protocol {
        name: "request_after_shutdown_is_invalid_request",
        run: request_after_shutdown,
        golden: false,
    },
    Protocol {
        name: "unknown_method_is_method_not_found",
        run: unknown_method,
        golden: false,
    },
    Protocol {
        name: "unknown_notification_is_ignored",
        run: unknown_notification,
        golden: false,
    },
    Protocol {
        name: "cancel_request_does_no_harm",
        run: cancel_request_does_no_harm,
        golden: false,
    },
    Protocol {
        name: "malformed_params_keep_serving",
        run: malformed_params_keep_serving,
        golden: false,
    },
    Protocol {
        name: "second_initialize_is_invalid_request",
        run: second_initialize,
        golden: false,
    },
    Protocol {
        name: "stdio_flag_is_accepted",
        run: stdio_flag_is_accepted,
        golden: false,
    },
    Protocol {
        name: "stdout_holds_frames_only",
        run: stdout_holds_frames_only,
        golden: false,
    },
    Protocol {
        name: "closed_file_published_at_start",
        run: closed_file_published_at_start,
        golden: false,
    },
    Protocol {
        name: "typing_republishes_with_new_version",
        run: typing_republishes_with_new_version,
        golden: false,
    },
    Protocol {
        name: "deleted_file_gets_one_empty_publish",
        run: deleted_file_gets_one_empty_publish,
        golden: false,
    },
];

fn protocol_trial(
    protocol: &'static Protocol,
    env: &Arc<Env>,
    current: Setting,
    verdicts: &Verdicts,
    bless: bool,
) -> Trial {
    let name = format!("protocol/{}", protocol.name);
    let expectation = current.expectation(Milestone::M1);
    let ignored = bless && !protocol.golden;
    let env = Arc::clone(env);
    let trial_name = name.clone();
    trial(
        name,
        Milestone::M1,
        expectation,
        verdicts,
        ignored,
        move || {
            let scratch = Scratch::new(&trial_name)?;
            (protocol.run)(&env, scratch.path())
        },
    )
}

/// A clean package.
const CLEAN_MAIN: &str =
    "//! A package for the protocol trials.\n\nfn main() {\n  let n = 1\n  assert n == 1\n}\n";
/// A root module with a type error (E0047).
const BROKEN_TYPE_MAIN: &str = "//! A package with a type error.\n\nfn answer() -> Int {\n  \"forty-two\"\n}\n\nfn main() {\n  assert answer() == 42\n}\n";
/// The same module with the error fixed.
const FIXED_MAIN: &str = "//! A package with a type error.\n\nfn answer() -> Int {\n  42\n}\n\nfn main() {\n  assert answer() == 42\n}\n";
/// A module with a type error.
const BROKEN_UTIL: &str =
    "//! A module with a type error.\n\npub fn answer() -> Int {\n  \"forty-two\"\n}\n";
const MANIFEST: &str = "name = \"protocol\"\nversion = \"0.0.0\"\n";

fn initialize_answers_capabilities(env: &Env, dir: &Path) -> Result<(), String> {
    let (client, init) = session(env, dir, ClientKind::Standard, None, None)?;
    let caps = init
        .get("capabilities")
        .ok_or("`initialize` has no capabilities")?;
    let encoding = caps.get("positionEncoding").and_then(Value::as_str);
    if !matches!(encoding, None | Some("utf-16")) {
        return Err(format!(
            "a client offering no encoding got {encoding:?}, not UTF-16"
        ));
    }
    let sync = caps.get("textDocumentSync").ok_or("no textDocumentSync")?;
    let want = [("openClose", json!(true)), ("change", json!(2))];
    for (key, value) in want {
        if sync.get(key) != Some(&value) {
            return Err(format!("textDocumentSync.{key} is not {value}: {sync}"));
        }
    }
    if sync
        .get("save")
        .is_none_or(|s| s == &Value::Bool(false) || s.is_null())
    {
        return Err(format!("textDocumentSync does not ask for didSave: {sync}"));
    }
    if caps.get("diagnosticProvider").is_some() {
        return Err("pull diagnostics are advertised; the server pushes only (D-1379)".to_owned());
    }
    client.shutdown()?.expect(0)
}

/// D-1368: `utf-8`, else `utf-32`, else `utf-16`.
fn position_encoding_negotiation(env: &Env, dir: &Path) -> Result<(), String> {
    let table: [(&[&str], &str); 4] = [
        (&["utf-16"], "utf-16"),
        (&["utf-32", "utf-16"], "utf-32"),
        (&["utf-16", "utf-32", "utf-8"], "utf-8"),
        (&["utf-7"], "utf-16"),
    ];
    for (offered, chosen) in table {
        let mut client = start(env, dir)?;
        let caps = json!({ "general": { "positionEncodings": offered } });
        let params = client.initialize_params(&caps, None)?;
        let init = client.initialize(params)?;
        let got = init
            .get("capabilities")
            .and_then(|c| c.get("positionEncoding"))
            .and_then(Value::as_str)
            .unwrap_or("utf-16");
        if got != chosen {
            return Err(format!(
                "offered {offered:?}, the server chose {got}, not {chosen}"
            ));
        }
        client.shutdown()?.expect(0)?;
    }
    Ok(())
}

fn capabilities_with_empty_client(env: &Env, dir: &Path) -> Result<(), String> {
    capabilities_golden(
        env,
        dir,
        ClientKind::Empty,
        "capabilities_with_empty_client",
    )
}

fn capabilities_with_full_client(env: &Env, dir: &Path) -> Result<(), String> {
    capabilities_golden(env, dir, ClientKind::Full, "capabilities_with_full_client")
}

/// The `initialize` answer for `kind`, compared with its golden. The
/// server's version is left out, so a release does not move it.
fn capabilities_golden(env: &Env, dir: &Path, kind: ClientKind, name: &str) -> Result<(), String> {
    let (client, mut init) = session(env, dir, kind, None, None)?;
    if let Some(info) = init.get_mut("serverInfo").and_then(Value::as_object_mut)
        && info.contains_key("version")
    {
        info.insert("version".to_owned(), json!("<version>"));
    }
    let text = case::block("initialize", &client.normalize(&init));
    client.shutdown()?.expect(0)?;
    let path = env
        .root
        .join("tests/lsp/protocol")
        .join(format!("{name}.expected"));
    if bless_enabled() {
        return golden::write(&path, &text, false);
    }
    check_golden(&path, &text)
}

fn shutdown_then_exit_gives_0(env: &Env, dir: &Path) -> Result<(), String> {
    let (client, _) = session(env, dir, ClientKind::Standard, None, None)?;
    client.shutdown()?.expect(0)
}

fn exit_without_shutdown_gives_1(env: &Env, dir: &Path) -> Result<(), String> {
    let (client, _) = session(env, dir, ClientKind::Standard, None, None)?;
    client.exit()?.expect(1)
}

fn end_of_input_gives_1(env: &Env, dir: &Path) -> Result<(), String> {
    let (client, _) = session(env, dir, ClientKind::Standard, None, None)?;
    client.end_input()?.expect(1)
}

fn request_before_initialize(env: &Env, dir: &Path) -> Result<(), String> {
    let mut client = start(env, dir)?;
    let answer = client.call("cheby/status", json!({}))?;
    expect_error(
        &answer,
        codes::SERVER_NOT_INITIALIZED,
        "a request before `initialize`",
    )?;
    let params = client.initialize_params(&ClientKind::Standard.capabilities(None), None)?;
    client.initialize(params)?;
    client.shutdown()?.expect(0)
}

fn request_after_shutdown(env: &Env, dir: &Path) -> Result<(), String> {
    let (mut client, _) = session(env, dir, ClientKind::Standard, None, None)?;
    let answer = client.request("shutdown", Value::Null)?;
    if !answer.is_null() {
        return Err(format!("`shutdown` answered {answer}"));
    }
    let after = client.call("cheby/status", json!({}))?;
    expect_error(&after, codes::INVALID_REQUEST, "a request after `shutdown`")?;
    client.exit()?.expect(0)
}

fn unknown_method(env: &Env, dir: &Path) -> Result<(), String> {
    let (mut client, _) = session(env, dir, ClientKind::Standard, None, None)?;
    let answer = client.call("cheby/noSuchRequest", json!({}))?;
    expect_error(&answer, codes::METHOD_NOT_FOUND, "an unknown request")?;
    let answer = client.call("textDocument/noSuchRequest", json!({}))?;
    expect_error(&answer, codes::METHOD_NOT_FOUND, "an unknown request")?;
    client.shutdown()?.expect(0)
}

fn second_initialize(env: &Env, dir: &Path) -> Result<(), String> {
    let (mut client, _) = session(env, dir, ClientKind::Standard, None, None)?;
    let params = client.initialize_params(&ClientKind::Standard.capabilities(None), None)?;
    let answer = client.call("initialize", params)?;
    expect_error(&answer, codes::INVALID_REQUEST, "a second `initialize`")?;
    client.shutdown()?.expect(0)
}

/// `cheby lsp --stdio`, as clients that name the transport start it
/// (A §12.5).
fn stdio_flag_is_accepted(env: &Env, dir: &Path) -> Result<(), String> {
    let mut client = start_with(env, dir, &["--stdio"])?;
    let params = client.initialize_params(&ClientKind::Standard.capabilities(None), None)?;
    client.initialize(params)?;
    client.shutdown()?.expect(0)
}

fn unknown_notification(env: &Env, dir: &Path) -> Result<(), String> {
    let (mut client, _) = session(env, dir, ClientKind::Standard, None, None)?;
    client.notify("cheby/noSuchNotification", json!({}))?;
    client.notify("$/noSuchNotification", json!({ "value": 1 }))?;
    client.notify("$/setTrace", json!({ "value": "off" }))?;
    let ended = client.shutdown()?;
    ended.expect(0)?;
    no_unexpected_answers(&ended)
}

fn cancel_request_does_no_harm(env: &Env, dir: &Path) -> Result<(), String> {
    let (mut client, _) = session(env, dir, ClientKind::Standard, None, None)?;
    // Unknown, then already answered.
    client.notify("$/cancelRequest", json!({ "id": 999_999 }))?;
    let id = client.send_request("cheby/noSuchRequest", json!({}))?;
    let answer = client.response(id)?;
    expect_error(&answer, codes::METHOD_NOT_FOUND, "an unknown request")?;
    client.notify("$/cancelRequest", json!({ "id": id }))?;
    client.notify("$/cancelRequest", json!({ "id": "a string id" }))?;
    let ended = client.shutdown()?;
    ended.expect(0)?;
    no_unexpected_answers(&ended)
}

/// A malformed notification is dropped, and a malformed request gets an
/// error answer, never a panic.
fn malformed_params_keep_serving(env: &Env, dir: &Path) -> Result<(), String> {
    let (mut client, _) = session(env, dir, ClientKind::Standard, None, None)?;
    client.notify("textDocument/didOpen", json!({ "textDocument": 5 }))?;
    client.notify("textDocument/didChange", json!({ "bogus": true }))?;
    client.notify("textDocument/didClose", json!("not an object"))?;
    client.notify("workspace/didChangeConfiguration", json!(17))?;
    let answer = client.call("textDocument/hover", json!("not an object"))?;
    match answer {
        Err(e) if e.code == codes::INVALID_PARAMS || e.code == codes::METHOD_NOT_FOUND => {}
        other => {
            return Err(format!(
                "a malformed `textDocument/hover` answered {other:?}, not InvalidParams or \
                 MethodNotFound"
            ));
        }
    }
    client.shutdown()?.expect(0)
}

/// A session that publishes, edits and saves, after which standard output
/// must have held frames of JSON-RPC 2.0 only (A §3.7).
fn stdout_holds_frames_only(env: &Env, dir: &Path) -> Result<(), String> {
    write_package(
        dir,
        &[("cheby.toml", MANIFEST), (ROOT_MODULE, BROKEN_TYPE_MAIN)],
    )?;
    let (mut client, _) = session(env, dir, ClientKind::Standard, None, None)?;
    let uri = client.open(Path::new(ROOT_MODULE))?;
    client.diagnostics(&uri, Some(1))?;
    let version = client.change(&uri, "fn main( {\n")?;
    client.diagnostics(&uri, Some(version))?;
    let version = client.change(&uri, FIXED_MAIN)?;
    client.diagnostics(&uri, Some(version))?;
    client.save(&uri)?;
    client.close(&uri)?;
    let ended = client.shutdown()?;
    ended.expect(0)?;
    if ended
        .received
        .iter()
        .any(|m| m.get("jsonrpc").and_then(Value::as_str) != Some("2.0"))
    {
        return Err("a message that is not JSON-RPC 2.0".to_owned());
    }
    Ok(())
}

/// D-1373, A §6.5: a folder that is a package gets its diagnostics at
/// start, a closed file's without a version.
fn closed_file_published_at_start(env: &Env, dir: &Path) -> Result<(), String> {
    write_package(
        dir,
        &[
            ("cheby.toml", MANIFEST),
            (ROOT_MODULE, CLEAN_MAIN),
            ("src/util.cheby", BROKEN_UTIL),
        ],
    )?;
    let (mut client, _) = session(env, dir, ClientKind::Standard, None, None)?;
    let uri = client.uri(Path::new("src/util.cheby"))?;
    let found = client.notification("a publish of the closed src/util.cheby", |m| {
        client::is_publish_of(m, &uri) && !client::diagnostics_of(m).is_empty()
    })?;
    let version = client::params(&found).get("version");
    if version.is_some_and(|v| !v.is_null()) {
        return Err(format!(
            "a closed file was published with version {version:?}"
        ));
    }
    if !has_code(&client::diagnostics_of(&found), "E0047") {
        return Err(format!(
            "the publish of src/util.cheby has no E0047: {found}"
        ));
    }
    client.shutdown()?.expect(0)
}

/// D-1373: each version of an open document gets its own publish, an
/// empty one included.
fn typing_republishes_with_new_version(env: &Env, dir: &Path) -> Result<(), String> {
    write_package(
        dir,
        &[("cheby.toml", MANIFEST), (ROOT_MODULE, BROKEN_TYPE_MAIN)],
    )?;
    let (mut client, _) = session(env, dir, ClientKind::Standard, None, None)?;
    let uri = client.open(Path::new(ROOT_MODULE))?;
    let first = client.diagnostics(&uri, Some(1))?;
    if !has_code(&first, "E0047") {
        return Err(format!("version 1 has no E0047: {first:?}"));
    }
    let version = client.change(&uri, FIXED_MAIN)?;
    let fixed = client.diagnostics(&uri, Some(version))?;
    if !fixed.is_empty() {
        return Err(format!(
            "version {version} is clean but published {fixed:?}"
        ));
    }
    let version = client.change(&uri, BROKEN_TYPE_MAIN)?;
    let again = client.diagnostics(&uri, Some(version))?;
    if !has_code(&again, "E0047") {
        return Err(format!("version {version} has no E0047: {again:?}"));
    }
    // The same text again still gets a publish with the new version.
    let version = client.change(&uri, BROKEN_TYPE_MAIN)?;
    let same = client.diagnostics(&uri, Some(version))?;
    if same != again {
        return Err(format!(
            "an unchanged text published {same:?}, not {again:?}"
        ));
    }
    client.shutdown()?.expect(0)
}

/// D-1373, A §2.6: a file that leaves the package gets one empty publish.
fn deleted_file_gets_one_empty_publish(env: &Env, dir: &Path) -> Result<(), String> {
    write_package(
        dir,
        &[
            ("cheby.toml", MANIFEST),
            (ROOT_MODULE, CLEAN_MAIN),
            ("src/util.cheby", BROKEN_UTIL),
        ],
    )?;
    let (mut client, _) = session(env, dir, ClientKind::Standard, None, None)?;
    let util = client.open(Path::new("src/util.cheby"))?;
    let first = client.diagnostics(&util, Some(1))?;
    if first.is_empty() {
        return Err("src/util.cheby has a type error but published nothing".to_owned());
    }
    let path = dir.join("src/util.cheby");
    std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": util, "type": 3 }] }),
    )?;
    client.close(&util)?;
    client.notification("the empty publish of the deleted src/util.cheby", |m| {
        client::is_publish_of(m, &util)
            && client::diagnostics_of(m).is_empty()
            && client::params(m).get("version").is_none_or(Value::is_null)
    })?;
    // Another run, after which the deleted file must not be published again.
    let main = client.open(Path::new(ROOT_MODULE))?;
    client.diagnostics(&main, Some(1))?;
    let ended = client.shutdown()?;
    ended.expect(0)?;
    let publishes: Vec<&Value> = ended
        .received
        .iter()
        .filter(|m| client::is_publish_of(m, &util))
        .collect();
    let after_empty = publishes
        .iter()
        .skip_while(|m| !client::diagnostics_of(m).is_empty() || has_version(m))
        .count();
    if after_empty != 1 {
        return Err(format!(
            "src/util.cheby was published {after_empty} times from its empty publish on, not once: \
             {publishes:?}"
        ));
    }
    Ok(())
}

fn has_version(message: &Value) -> bool {
    client::params(message)
        .get("version")
        .is_some_and(|v| !v.is_null())
}

fn has_code(diagnostics: &[Value], code: &str) -> bool {
    diagnostics
        .iter()
        .any(|d| d.get("code").and_then(Value::as_str) == Some(code))
}

fn expect_error(
    answer: &Result<Value, client::RpcError>,
    code: i64,
    what: &str,
) -> Result<(), String> {
    match answer {
        Err(e) if e.code == code => Ok(()),
        other => Err(format!("{what} answered {other:?}, not error {code}")),
    }
}

/// Exactly one answer to each request the client sent, and no other
/// answer: none to a notification, which has no id, none to an id named
/// only by `$/cancelRequest`, and no second one to an answered request.
fn no_unexpected_answers(ended: &Ended) -> Result<(), String> {
    let mut answered = BTreeSet::new();
    let stray: Vec<&Value> = ended
        .received
        .iter()
        .filter(|m| m.get("method").is_none())
        .filter(|m| {
            let id = m
                .get("id")
                .map_or_else(|| "null".to_owned(), Value::to_string);
            !ended.sent.contains(&id) || !answered.insert(id)
        })
        .collect();
    if stray.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "answers to no request, or second answers: {stray:?}"
        ))
    }
}

// ---------------------------------------------------------------------
// examples/*

fn example_trial(
    ex: Example,
    env: &Arc<Env>,
    current: Setting,
    verdicts: &Verdicts,
    bless: bool,
) -> Trial {
    let name = format!("examples/{}", ex.name);
    let expectation = ex.check_only_expectation(current);
    let milestone = ex.config.milestone;
    let env = Arc::clone(env);
    let trial_name = name.clone();
    trial(name, milestone, expectation, verdicts, bless, move || {
        let scratch = Scratch::new(&trial_name)?;
        copy_dir(&ex.dir, scratch.path())?;
        sweep_example(&env, scratch.path())
    })
}

/// Opens every source and sends every advertised request at every
/// identifier; each must be answered without an error.
fn sweep_example(env: &Env, dir: &Path) -> Result<(), String> {
    let files = sources(dir)?;
    let (mut client, init) = session(env, dir, ClientKind::Standard, None, None)?;
    let advertised = Advertised::of(init.get("capabilities").unwrap_or(&Value::Null))?;
    let uris = open_all(&mut client, &files)?;
    let encoding = client.encoding();
    for (file, uri) in files.iter().zip(&uris) {
        let path = dir.join(file);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let lines = Lines::new(&text, Bom::Counted);
        let diagnostics = client
            .published()
            .get(uri)
            .map(client::diagnostics_of)
            .unwrap_or_default();
        let mut requests = Vec::new();
        let mut renames = Vec::new();
        for (start, end) in case::identifiers(&text) {
            let (a, b) = (
                lines.position(start, encoding),
                lines.position(end, encoding),
            );
            for name in &advertised.position {
                let end = matches!(*name, "codeAction" | "rangeFormatting").then_some(b);
                let (method, params) = case::position_params(name, uri, a, end, &[], &diagnostics)?;
                requests.push((
                    format!("{file}:{}:{} {name}", a.line, a.character),
                    method,
                    params,
                ));
            }
            let word = text.get(start..end).unwrap_or_default();
            let new_name = if word.starts_with(char::is_uppercase) {
                "Renamed"
            } else {
                "renamed"
            };
            renames.push((a, new_name));
        }
        let last = lines.position(text.len(), encoding);
        for name in &advertised.document {
            let (method, params) = case::document_params(name, uri, last)?;
            requests.push((format!("{file} {name}"), method, params));
        }
        let prepared = send_all(&mut client, requests)?;
        // A rename where `prepareRename` accepted the position.
        if advertised.rename {
            let mut rename_requests = Vec::new();
            for (at, new_name) in renames {
                let label = format!("{file}:{}:{} prepareRename", at.line, at.character);
                let accepted = prepared.get(&label).is_some_and(|v| !v.is_null());
                if accepted {
                    let (method, params) = case::position_params(
                        "rename",
                        uri,
                        at,
                        None,
                        &[new_name.to_owned()],
                        &[],
                    )?;
                    rename_requests.push((
                        format!("{file}:{}:{} rename", at.line, at.character),
                        method,
                        params,
                    ));
                }
            }
            send_all(&mut client, rename_requests)?;
        }
        if let Some(edits) = prepared.get(&format!("{file} formatting"))
            && edits.as_array().is_some_and(|e| !e.is_empty())
        {
            return Err(format!(
                "formatting {file}, a `cheby fmt` fixed point, gave edits: {edits}"
            ));
        }
    }
    client.shutdown()?.expect(0)
}

/// Sends `requests` in batches; each must be answered without an error.
/// Returns the answers by label.
fn send_all(
    client: &mut Client,
    requests: Vec<(String, &'static str, Value)>,
) -> Result<BTreeMap<String, Value>, String> {
    let mut answers = BTreeMap::new();
    let mut rest = requests.into_iter().peekable();
    while rest.peek().is_some() {
        let chunk: Vec<_> = rest.by_ref().take(BATCH).collect();
        let (labels, calls): (Vec<String>, Vec<(&str, Value)>) = chunk
            .into_iter()
            .map(|(label, method, params)| (label, (method, params)))
            .unzip();
        let results = client.batch(calls)?;
        for (label, result) in labels.into_iter().zip(results) {
            match result {
                Ok(value) => {
                    answers.insert(label, value);
                }
                Err(e) => return Err(client.fail(&format!("{label} answered {e}"))),
            }
        }
    }
    Ok(answers)
}

// ---------------------------------------------------------------------
// parity/*

fn example_parity_trial(
    ex: Example,
    env: &Arc<Env>,
    current: Setting,
    verdicts: &Verdicts,
    bless: bool,
) -> Trial {
    let name = format!("parity/examples/{}", ex.name);
    let expectation = ex.check_only_expectation(current);
    let milestone = ex.config.milestone;
    let env = Arc::clone(env);
    let trial_name = name.clone();
    let ignored = platform::runs_skipped() || bless;
    trial(name, milestone, expectation, verdicts, ignored, move || {
        let scratch = Scratch::new(&trial_name)?;
        copy_dir(&ex.dir, scratch.path())?;
        let printed = run_check(&env, scratch.path())?;
        let package = scratch.path().to_string_lossy().replace('\\', "/");
        let shown = |file: &str| {
            let file = file.replace('\\', "/");
            file.strip_prefix(&format!("{package}/"))
                .map_or_else(|| file.clone(), str::to_owned)
        };
        compare_parity(&env, scratch.path(), &printed, &shown)
    })
}

/// `cheby check`'s diagnostics in `dir`.
fn run_check(env: &Env, dir: &Path) -> Result<Vec<parity::Printed>, String> {
    let mut cmd = Command::new(CHEBY);
    cmd.args(["--color=never", "check"])
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env(POISON_ENV, "1");
    env.cache.apply(&mut cmd);
    let out = process::run(cmd, None, env.timeout)?;
    Ok(parity::parse_check_output(&out.stderr))
}

fn ui_parity_trial(
    test: UiTest,
    env: &Arc<Env>,
    current: Setting,
    verdicts: &Verdicts,
    bless: bool,
) -> Option<Trial> {
    let checks = matches!(
        test.header.mode,
        Mode::CheckPass | Mode::CheckFail | Mode::ParseFail
    );
    if !checks || !test.header.flags.is_empty() {
        return None;
    }
    let golden = golden::read(&test.golden("stderr"))
        .ok()
        .flatten()
        .unwrap_or_default();
    let printed = parity::parse_check_output(&golden);
    // A tool error with no code reaches the user as a message, not as a
    // diagnostic, so it has no published counterpart.
    if printed.iter().any(|p| p.code.is_none()) {
        return None;
    }
    let back_end = printed.iter().any(|p| {
        p.code
            .as_deref()
            .is_some_and(|c| ui::BACK_END_CODES.contains(&c))
    });
    let name = format!("parity/ui/{}", test.name);
    let milestone = test.header.milestone;
    let expectation = current.expectation(milestone);
    let ignored = (platform::runs_skipped() && test.builds_a_jit())
        || bless
        || (PARITY_SKIPS_BACK_END && back_end);
    let env = Arc::clone(env);
    let trial_name = name.clone();
    Some(trial(
        name,
        milestone,
        expectation,
        verdicts,
        ignored,
        move || {
            let scratch = Scratch::new(&trial_name)?;
            let prefix = match test.layout {
                ui::Layout::File => {
                    single_file_package(scratch.path(), UI_PACKAGE, &test.path)?;
                    None
                }
                ui::Layout::Package => {
                    copy_dir(&test.path, scratch.path())?;
                    Some(format!("tests/ui/{}/", test.name))
                }
            };
            let file_golden = format!("tests/ui/{}.cheby", test.name);
            let shown = |file: &str| match &prefix {
                None if file == file_golden => ROOT_MODULE.to_owned(),
                None => file.to_owned(),
                Some(prefix) => file
                    .strip_prefix(prefix.as_str())
                    .unwrap_or(file)
                    .to_owned(),
            };
            compare_parity(&env, scratch.path(), &printed, &shown)
        },
    ))
}

/// Opens the root module of the package at `dir`, waits for its publish,
/// and compares every published list with `printed`, whose paths `shown`
/// maps to the package's own.
fn compare_parity(
    env: &Env,
    dir: &Path,
    printed: &[parity::Printed],
    shown: &dyn Fn(&str) -> String,
) -> Result<(), String> {
    let (mut client, _) = session(env, dir, ClientKind::Standard, None, None)?;
    let encoding = client.encoding();
    let paths: Paths = client.paths().clone();
    let main = client.open(Path::new(ROOT_MODULE))?;
    client.diagnostics(&main, Some(1))?;
    // `shutdown` is answered on the main loop after the publishes of the
    // run that published the root module, so every file of that run is in.
    let ended = client.shutdown()?;
    ended.expect(0)?;

    let mut texts: BTreeMap<String, String> = BTreeMap::new();
    let mut read = |file: &str| -> Option<String> {
        if let Some(text) = texts.get(file) {
            return Some(text.clone());
        }
        let text = std::fs::read_to_string(dir.join(file)).ok()?;
        texts.insert(file.to_owned(), text.clone());
        Some(text)
    };
    let expected: Vec<Key> = printed
        .iter()
        .map(|p| {
            let file = p.at.as_ref().map(|(file, _, _)| shown(file));
            let text = file.as_deref().and_then(&mut read);
            let located = match (&file, &text) {
                (Some(file), Some(text)) => {
                    let bom = if file == ROOT_MODULE {
                        Bom::Counted
                    } else {
                        Bom::Hidden
                    };
                    Some((file.as_str(), text.as_str(), bom))
                }
                _ => None,
            };
            parity::printed_key(p, located, encoding)
        })
        .collect();
    let mut actual = Vec::new();
    for (uri, params) in &ended.published {
        let Some(file) = paths.relative(uri) else {
            return Err(format!("a publish of {uri}, outside the package"));
        };
        for d in client::diagnostics_of(params) {
            actual.push(parity::published_key(&file, &d));
        }
    }
    // D-1347, D-1348: with a phase-A error, the files that have one and the
    // manifest show exactly their phase-A diagnostics, and the others are
    // checked against last good text.
    let mut broken: BTreeSet<String> = BTreeSet::new();
    for file in sources(dir)? {
        let bytes = std::fs::read(dir.join(&file)).map_err(|e| format!("{file}: {e}"))?;
        if parity::is_broken(&bytes) {
            broken.insert(file);
        }
    }
    let manifest_errors = expected.iter().any(|k| k.file == parity::MANIFEST);
    if !broken.is_empty() || manifest_errors {
        broken.insert(parity::MANIFEST.to_owned());
        let keep = |k: &Key| broken.contains(&k.file);
        let expected: Vec<Key> = expected.into_iter().filter(keep).collect();
        let actual: Vec<Key> = actual.into_iter().filter(keep).collect();
        return parity::compare(&expected, &actual);
    }
    parity::compare(&expected, &actual)
}

// ---------------------------------------------------------------------
// golden/*

fn golden_trial(
    test: LspTest,
    env: &Arc<Env>,
    current: Setting,
    verdicts: &Verdicts,
    bless: bool,
) -> Trial {
    let name = format!("golden/{}", test.name);
    // A header that does not parse fails the test as one that must pass.
    let (milestone, expectation) = match &test.header {
        Ok(header) => (header.milestone, current.expectation(header.milestone)),
        Err(_) => (Milestone::M1, Expectation::Pass),
    };
    let env = Arc::clone(env);
    let trial_name = name.clone();
    trial(name, milestone, expectation, verdicts, false, move || {
        let bless = bless && expectation != Expectation::Fail;
        let header = test.header.clone()?;
        let scratch = Scratch::new(&trial_name)?;
        match test.layout {
            Layout::File => {
                single_file_package(scratch.path(), case::PACKAGE_NAME, &test.path)?;
            }
            Layout::Package => copy_dir(&test.path, scratch.path())?,
        }
        let answers = golden_answers(&env, scratch.path(), &header)?;
        if bless {
            return golden::write(&test.golden(), &answers, false);
        }
        check_golden(&test.golden(), &answers)
    })
}

/// The golden text of a test: its diagnostics, then one block per marker
/// and per document request, in file and line order.
fn golden_answers(env: &Env, dir: &Path, header: &case::Header) -> Result<String, String> {
    let files = sources(dir)?;
    let (mut client, _) = session(
        env,
        dir,
        header.client,
        header.encoding,
        header.settings.clone(),
    )?;
    let paths = client.paths().clone();
    let uris = open_all(&mut client, &files)?;
    let encoding = client.encoding();
    let mut blocks = Vec::new();
    for (file, uri) in files.iter().zip(&uris) {
        let path = dir.join(file);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let lines = Lines::new(&text, Bom::Counted);
        let diagnostics = client
            .published()
            .get(uri)
            .map(client::diagnostics_of)
            .unwrap_or_default();
        for marker in case::markers(&text).map_err(|e| format!("{file}: {e}"))? {
            let line = lines.line(marker.line).unwrap_or_default();
            let at = |column: usize| Position {
                line: u32::try_from(marker.line).unwrap_or(u32::MAX),
                character: u32::try_from(
                    line.chars()
                        .take(column)
                        .fold(0_usize, |n, c| n.saturating_add(encoding.units(c))),
                )
                .unwrap_or(u32::MAX),
            };
            let (method, params) = case::position_params(
                &marker.name,
                uri,
                at(marker.column),
                marker.end.map(at),
                &marker.args,
                &diagnostics,
            )?;
            let answer = answer_value(&mut client, method, params, &marker.name)?;
            blocks.push(case::block(&marker.label(file), &paths.normalize(&answer)));
        }
        let last = lines.position(text.len(), encoding);
        for name in &header.requests {
            let (method, params) = case::document_params(name, uri, last)?;
            let answer = answer_value(&mut client, method, params, name)?;
            blocks.push(case::block(
                &format!("{file} {name}"),
                &paths.normalize(&answer),
            ));
        }
    }
    let ended = client.shutdown()?;
    ended.expect(0)?;
    let mut published = serde_json::Map::new();
    for (uri, params) in &ended.published {
        let list = client::diagnostics_of(params);
        if !list.is_empty() {
            published.insert(paths.display(uri), paths.normalize(&Value::Array(list)));
        }
    }
    let mut text = case::block("diagnostics", &Value::Object(published));
    for block in blocks {
        text.push('\n');
        text.push_str(&block);
    }
    Ok(text)
}

/// The answer of a request, or its error as `{"error": {code, message}}`.
fn answer_value(
    client: &mut Client,
    method: &str,
    params: Value,
    name: &str,
) -> Result<Value, String> {
    Ok(match client.call(method, params)? {
        Ok(value) => case::canonical(name, value),
        Err(e) => json!({ "error": { "code": e.code, "message": e.message } }),
    })
}

/// Compares `actual` with the golden at `path`. A missing golden fails
/// with the whole text, so a reviewer can write it by hand or bless it.
fn check_golden(path: &Path, actual: &str) -> Result<(), String> {
    match golden::read(path)? {
        Some(expected) => golden::compare("the answer", &expected, actual)
            .map_err(|e| format!("{}: {e}\nthe whole answer:\n{actual}", path.display())),
        None => Err(format!(
            "{} is missing; bless it with `cargo xtask bless`, or write it from:\n{actual}",
            path.display()
        )),
    }
}
