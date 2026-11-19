//! Language server harness: runs `cheby lsp` as an editor would, over
//! stdio, one process per trial (D-1352, docs/plan/testing.md,
//! `tests/lsp/README.md`). The client is `cheby_test_support::lsp`.
//!
//! Ten groups of trials:
//!
//! - `protocol/<name>`: the handshake, `shutdown` and `exit` and their exit
//!   codes, the end of input, a frame that is not JSON-RPC (D-1430), error
//!   answers (`MethodNotFound`, `ServerNotInitialized`, `InvalidRequest`
//!   after `shutdown`), `$/cancelRequest`, malformed params, the negotiated
//!   position encoding (D-1368), the publishing rules (D-1373, D-1442),
//!   that standard output holds frames only (A §3.7), and the applied-edit
//!   check on edits written by hand (D-1676). Two of them compare the
//!   `initialize` answer with a golden:
//!   `tests/lsp/protocol/<name>.expected`.
//! - `format/<name>`: formatting cases a file cannot hold, such as CRLF
//!   line ends, which `.gitattributes` turns into LF.
//! - `symbols/examples/<name>`: the outline of every source file of every
//!   example equals its item list (`features.md` §5.1).
//! - `examples/<name>`: every example's sources are opened, and every
//!   request the server advertises is sent at every identifier and type
//!   name. Each must be answered in time, with no JSON-RPC error and no
//!   exit of the server; `null` is an answer. Completion goes to the end
//!   of every identifier and after every `.` and `|>`, and every item's
//!   edit must sit on one line and hold the position (D-1394); signature
//!   help goes inside every argument list. Both are sampled within a
//!   budget per example, and so is rename, which goes where
//!   `prepareRename` accepted (D-1677). A `RequestFailed` of those two is a
//!   refusal and passes. Formatting must return no edit, since every
//!   example is a `cheby fmt` fixed point. The group tightens as the server
//!   advertises more (D-1379).
//! - `rename_sweep/<name>` and `rename_run/<name>`: the rename sweep
//!   (D-1677). The first renames each symbol of the example once and puts
//!   each edit through the applied-edit check; the second renames 8
//!   symbols whose name cannot reach the output in a copy of the example,
//!   which must pass `cheby check` and print its `expected.stdout`.
//! - `navigation/<name>`: an answer followed into another file. In
//!   `std_round_trip`, definition on `io::println` leads into std's
//!   materialized sources, that file is opened, and hover in it answers
//!   (R-17).
//! - `quality/hover_on_mutants`: the quality gate of plan W7 (`facts.md`
//!   §0.1, §10). Seeded body mutants of the examples go to a warm server,
//!   and hover at each identifier of the edited item is graded against
//!   the clean text: same at or above 90%, key probe at or above 95%. It
//!   prints both numbers and the table by kind.
//! - `parity/examples/<name>` and `parity/ui/<name>`: the published
//!   diagnostics equal `cheby check`'s (D-1348), for an example by running
//!   `cheby check`, for a UI check test by reading its `.stderr` golden.
//!   Only the root module is opened, so the other files are published as
//!   closed files. The published lists are taken after `cheby/idle`, so
//!   the constants child's diagnostics, the back-end codes of
//!   `ui::BACK_END_CODES`, are compared too (D-1349, D-1415). When a file
//!   has a phase-A error, only the files with one and `cheby.toml` are
//!   compared (D-1347). UI tests with `@flags` and run tests are not part
//!   of it, nor goldens with an error that has no code.
//! - `golden/<name>`: the tests under `tests/lsp/`, whose exact answers are
//!   in `<name>.expected`. `CHEBY_BLESS=1` rewrites those goldens, and the
//!   two protocol goldens, except for a test of a later milestone. It
//!   ignores every trial that has no golden. Whatever the golden says, a
//!   `formatting` answer must give `cheby fmt`'s text when applied, a
//!   `rangeFormatting` edit must touch the lines of its range, every
//!   completion item's edit must sit on one line and hold the position
//!   (D-1394), the text after an `apply` marker's item must parse, and
//!   the edit of a `rename` marker or a `willRename` step must pass the
//!   applied-edit check (`editing::check_applied`, D-1676).
//!   Completion, signature help and inlay hints are printed in a readable
//!   form (`editing`). A test's `@steps` run after the opens: waits for
//!   `cheby/idle` and for a child run to start, edits and saves of files
//!   the test holds (`tests/lsp/constants/`), files written with no
//!   notification, closes, and `willRenameFiles` with its moves.
//!
//! Waits (D-1442): a publish comes for each new version of an open
//! document and after each `didOpen` and `didClose`, but not for a run
//! that changes neither the version nor the list. So no trial waits for a
//! publish after `didSave` or a settings change. A request does not wait
//! for such a run either: it waits only for the text it names (D-1367). A
//! trial that needs the server to have caught up sends an edit and waits
//! for that version's publish, since a package's jobs run in order. A
//! trial that needs a save's jobs, the constants child included, to have
//! finished waits for `cheby/idle`, which every client of the harness
//! asks for with `experimental.chebyIdle` (D-1415, D-1443).
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
//! (`UiTest::builds_a_jit`). The server never evaluates constants there
//! (D-1414), so a golden test whose `@steps` wait for the child is ignored
//! too, and so is `rename_run/*`, which runs `cheby check` and `cheby
//! run`. The server process never runs generated code (D-1349), so every
//! other trial runs there.
#![expect(
    clippy::wildcard_enum_match_arm,
    reason = "a test matches only the variants it checks"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::time::Duration;

use cheby_test_support::cache::HarnessCache;
use cheby_test_support::example::{self, Example};
use cheby_test_support::lsp::case::{self, Advertised, Layout, LspTest, Step};
use cheby_test_support::lsp::client::{self, codes};
use cheby_test_support::lsp::editing;
use cheby_test_support::lsp::outline;
use cheby_test_support::lsp::parity::{self, Key};
use cheby_test_support::lsp::quality::{self, Mutant};
use cheby_test_support::lsp::serde_json::{self, Value, json};
use cheby_test_support::lsp::text::{self, Bom, Lines, Position};
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
/// The server's notification that nothing is running or due for a
/// package, the constants child included (D-1415).
const IDLE: &str = "cheby/idle";
/// The token prefix of a child run's `$/progress` (D-1415).
const CHILD_PROGRESS: &str = "cheby/child/";
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
        root,
        cache,
        timeout,
    });
    let mut trials = Vec::new();
    for protocol in PROTOCOL {
        trials.push(protocol_trial(
            "protocol", protocol, &env, current, &verdicts, bless,
        ));
    }
    for format in FORMAT {
        trials.push(protocol_trial(
            "format", format, &env, current, &verdicts, bless,
        ));
    }
    for ex in &examples {
        trials.push(example_trial(ex.clone(), &env, current, &verdicts, bless));
    }
    for ex in &examples {
        trials.push(symbols_trial(ex.clone(), &env, current, &verdicts, bless));
    }
    for ex in &examples {
        trials.push(rename_sweep_trial(
            ex.clone(),
            &env,
            current,
            &verdicts,
            bless,
        ));
    }
    for ex in &examples {
        if let Some(trial) = rename_run_trial(ex.clone(), &env, current, &verdicts, bless) {
            trials.push(trial);
        }
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
    for navigation in NAVIGATION {
        trials.push(protocol_trial(
            "navigation",
            navigation,
            &env,
            current,
            &verdicts,
            bless,
        ));
    }
    trials.push(quality_trial(&examples, &env, current, &verdicts, bless));
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

/// Copies `source` into `dir` under its own name, with no `cheby.toml`:
/// a file outside every package (D-1371).
fn loose_file(dir: &Path, source: &Path) -> Result<(), String> {
    let name = source
        .file_name()
        .ok_or_else(|| format!("{}: no file name", source.display()))?;
    std::fs::copy(source, dir.join(name)).map_err(|e| format!("{}: {e}", source.display()))?;
    Ok(())
}

/// The `.cheby` files directly in `dir`, by name, for a loose test.
fn loose_sources(dir: &Path) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.extension().is_some_and(|e| e == "cheby") {
            files.push(cheby_test_support::display_relative(&path, dir));
        }
    }
    files.sort();
    Ok(files)
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
        name: "malformed_frame_ends_with_1",
        run: malformed_frame_ends_with_1,
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
    Protocol {
        name: "close_and_reopen_publish_again",
        run: close_and_reopen_publish_again,
        golden: false,
    },
    Protocol {
        name: "applied_edit_check",
        run: applied_edit_check,
        golden: false,
    },
    Protocol {
        name: "applied_edit_check_without_idle",
        run: applied_edit_check_without_idle,
        golden: false,
    },
];

/// The `format/*` trials, which need a text no file can hold.
const FORMAT: &[Protocol] = &[Protocol {
    name: "crlf_is_converted",
    run: crlf_is_converted,
    golden: false,
}];

/// The `navigation/*` trials, which follow an answer into another file.
const NAVIGATION: &[Protocol] = &[Protocol {
    name: "std_round_trip",
    run: std_round_trip,
    golden: false,
}];

/// One trial of `group`, `protocol`, `format` or `navigation`.
fn protocol_trial(
    group: &str,
    protocol: &'static Protocol,
    env: &Arc<Env>,
    current: Setting,
    verdicts: &Verdicts,
    bless: bool,
) -> Trial {
    let name = format!("{group}/{}", protocol.name);
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
/// A root module that `cheby fmt` changes.
const MISFORMATTED_MAIN: &str = "//! A package for the protocol trials.\n\nfn main() {\n    let n = [1,2]\n  assert n == [1, 2]\n}\n";
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

/// D-1430: a frame whose body is not a JSON-RPC message ends `cheby lsp`
/// with status 1 and one line on standard error, with the input still
/// open, and the server answers nothing. Each body goes to a fresh server,
/// after `initialize`, and the first one also before it.
fn malformed_frame_ends_with_1(env: &Env, dir: &Path) -> Result<(), String> {
    let not_json: &[u8] = b"{not json";
    let bodies: [(&str, &[u8]); 3] = [
        ("a body that is not JSON", not_json),
        (
            "neither a request, a notification nor an answer",
            br#"{"jsonrpc":"2.0","params":{}}"#,
        ),
        (
            "a batch",
            br#"[{"jsonrpc":"2.0","method":"initialized","params":{}}]"#,
        ),
    ];
    let cases = bodies
        .iter()
        .map(|&(what, body)| (what.to_owned(), body, true))
        .chain(std::iter::once((
            "a body that is not JSON, before `initialize`".to_owned(),
            not_json,
            false,
        )));
    for (what, body, initialized) in cases {
        let mut client = if initialized {
            session(env, dir, ClientKind::Standard, None, None)?.0
        } else {
            start(env, dir)?
        };
        client.send_raw(body)?;
        let ended = client.wait_end().map_err(|e| format!("{what}: {e}"))?;
        ended.expect(1).map_err(|e| format!("{what}: {e}"))?;
        no_unexpected_answers(&ended).map_err(|e| format!("{what}: {e}"))?;
        let lines = ended
            .stderr
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        if lines != 1 {
            return Err(format!(
                "{what}: standard error holds {lines} lines, not one:\n{}",
                ended.stderr
            ));
        }
    }
    Ok(())
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

/// The root module of the applied-edit check's package.
const CHECKED_MAIN: &str = "//! A package for the applied-edit check.\n\nimport protocol::aaaa\n\nfn main() {\n  let total = aaaa::helper()\n  assert total == 1\n}\n";
/// Its other module.
const CHECKED_UTIL: &str = "//! Its helper.\n\npub fn helper() -> Int {\n  1\n}\n";
/// The other module's path. Its name sorts before `main`, so the server
/// publishes it before the root module, whose new error [`check_callers`]
/// then sees only after the wait for `cheby/idle` (D-1676).
const CHECKED_UTIL_PATH: &str = "src/aaaa.cheby";

/// The package of the applied-edit check's trials, written to `dir`, with
/// both modules open in a session of `kind`, once the server has caught up
/// with no error.
fn checked_package(env: &Env, dir: &Path, kind: ClientKind) -> Result<Checked, String> {
    write_package(
        dir,
        &[
            ("cheby.toml", MANIFEST),
            (ROOT_MODULE, CHECKED_MAIN),
            (CHECKED_UTIL_PATH, CHECKED_UTIL),
        ],
    )?;
    let (mut client, _) = session(env, dir, kind, None, None)?;
    open_all(
        &mut client,
        &[ROOT_MODULE.to_owned(), CHECKED_UTIL_PATH.to_owned()],
    )?;
    let texts = BTreeMap::from([
        (ROOT_MODULE.to_owned(), CHECKED_MAIN.to_owned()),
        (CHECKED_UTIL_PATH.to_owned(), CHECKED_UTIL.to_owned()),
    ]);
    let mut settle = editing::Settle::new("", kind != ClientKind::Empty, &client);
    // A package that starts broken would hide the errors the cases add.
    settle.wait(&mut client)?;
    editing::new_errors(&BTreeMap::new(), &editing::published_by_path(&client), &[])
        .map_err(|e| format!("the package does not start clean: {e}"))?;
    Ok(Checked {
        client,
        texts,
        settle,
    })
}

/// One text edit written by hand: a line, a start and an end column on it,
/// and the new text.
type HandEdit<'a> = (u32, u32, u32, &'a str);

/// A `WorkspaceEdit` written by hand: for each open file of `files`, its
/// edits, with the file's version.
fn hand_edit(client: &Client, files: &[(&str, &[HandEdit<'_>])]) -> Result<Value, String> {
    let mut changes = Vec::new();
    for (path, edits) in files {
        let uri = client.uri(Path::new(path))?;
        let edits: Vec<Value> = edits
            .iter()
            .map(|&(line, start, end, text)| {
                json!({
                    "range": {
                        "start": { "line": line, "character": start },
                        "end": { "line": line, "character": end },
                    },
                    "newText": text,
                })
            })
            .collect();
        changes.push(json!({
            "textDocument": { "uri": uri, "version": client.version(&uri) },
            "edits": edits,
        }));
    }
    Ok(json!({ "documentChanges": changes }))
}

/// `result`, an applied-edit check's, failed with a message that holds
/// `wanted`.
fn fails_naming(
    result: Result<editing::Applied, String>,
    wanted: &str,
    what: &str,
) -> Result<(), String> {
    match result {
        Ok(_) => Err(format!("{what}: the applied-edit check passed")),
        Err(e) if e.contains(wanted) => Ok(()),
        Err(e) => Err(format!(
            "{what}: the check failed without naming {wanted}: {e}"
        )),
    }
}

/// D-1676: the applied-edit check through a real server, with a client
/// that takes `cheby/idle`.
fn applied_edit_check(env: &Env, dir: &Path) -> Result<(), String> {
    edit_check_with(env, dir, ClientKind::Standard)
}

/// [`applied_edit_check`] with the `empty` client, which takes no
/// `cheby/idle`, so the check asks `cheby/status` instead.
fn applied_edit_check_without_idle(env: &Env, dir: &Path) -> Result<(), String> {
    edit_check_with(env, dir, ClientKind::Empty)
}

/// The cases of [`applied_edit_check`], each failing one in a session of
/// its own: a local renamed by hand passes and is undone; with rename's
/// narrowing, a line pushed past 100 columns passes (D-1670); a module
/// moved with the edit of its importer passes, and moved back with no edit
/// fails, naming the importer; a line put out of `cheby fmt`'s form fails;
/// an edit of `src/util.cheby` alone that breaks its caller in
/// `src/main.cheby`, a file the edit did not touch, fails naming that file,
/// and passes when the header's codes excuse the error.
/// A session of the applied-edit check's package.
struct Checked {
    client: Client,
    texts: BTreeMap<String, String>,
    settle: editing::Settle,
}

impl Checked {
    /// [`editing::check_applied`] of `edit`, `moves` and `rules`.
    fn check(
        &mut self,
        edit: &Value,
        moves: &[(String, String)],
        rules: editing::EditRules<'_>,
    ) -> Result<editing::Applied, String> {
        editing::check_applied(
            &mut self.client,
            &mut self.texts,
            &mut self.settle,
            edit,
            moves,
            rules,
        )
    }

    /// [`editing::revert`] of `applied`.
    fn revert(&mut self, applied: editing::Applied) -> Result<(), String> {
        editing::revert(&mut self.client, &mut self.texts, &mut self.settle, applied)
    }

    /// A [`hand_edit`] of this session's open files.
    fn edit(&self, files: &[(&str, &[HandEdit<'_>])]) -> Result<Value, String> {
        hand_edit(&self.client, files)
    }

    /// Ends the session.
    fn end(self) -> Result<(), String> {
        self.client.shutdown()?.expect(0)
    }
}

/// The rules of the check with no excused error and `fixed_point`.
const fn rules(fixed_point: editing::FixedPoint) -> editing::EditRules<'static> {
    editing::EditRules {
        new_errors: &[],
        fixed_point,
    }
}

/// The cases of [`applied_edit_check`], each failing one in a session of
/// its own, as [`check_form`], [`check_moves`] and [`check_callers`] list
/// them.
fn edit_check_with(env: &Env, dir: &Path, kind: ClientKind) -> Result<(), String> {
    check_form(env, dir, kind)?;
    check_moves(env, dir, kind)?;
    check_callers(env, dir, kind)
}

/// A local renamed by hand passes and is undone; with rename's narrowing,
/// a line pushed past 100 columns passes (D-1670), and strictly it fails;
/// a line put out of `cheby fmt`'s form fails, in a file that was not
/// open too, which the check opens to ask its form before (D-1712).
fn check_form(env: &Env, dir: &Path, kind: ClientKind) -> Result<(), String> {
    use editing::FixedPoint::{Rename, Strict};
    let mut session = checked_package(env, &dir.join("form"), kind)?;
    let local = session.edit(&[(ROOT_MODULE, &[(5, 6, 11, "sum"), (6, 9, 14, "sum")])])?;
    let applied = session.check(&local, &[], rules(Strict))?;
    session.revert(applied)?;
    if session.texts.get(ROOT_MODULE).map(String::as_str) != Some(CHECKED_MAIN) {
        return Err(format!("the revert left {:?}", session.texts));
    }
    let long = "a_name_long_enough_to_push_its_line_past_the_hundred_columns_that_cheby_fmt_keeps";
    let renames: &[HandEdit<'_>] = &[(5, 6, 11, long), (6, 9, 14, long)];
    let applied = session.check(
        &session.edit(&[(ROOT_MODULE, renames)])?,
        &[],
        rules(Rename),
    )?;
    session.revert(applied)?;
    // The versions moved on with the revert, so the edit is written again.
    let result = session.check(
        &session.edit(&[(ROOT_MODULE, renames)])?,
        &[],
        rules(Strict),
    );
    fails_naming(result, "cheby fmt", "a line past 100 columns, strictly")?;
    session.end()?;
    let mut session = checked_package(env, &dir.join("spaced"), kind)?;
    let spaced = session.edit(&[(ROOT_MODULE, &[(6, 15, 15, "  ")])])?;
    fails_naming(
        session.check(&spaced, &[], rules(Rename)),
        "cheby fmt",
        "an extra space",
    )?;
    session.end()?;
    let mut session = checked_package(env, &dir.join("closed"), kind)?;
    let util = session.client.uri(Path::new(CHECKED_UTIL_PATH))?;
    session.client.close(&util)?;
    session.texts.remove(CHECKED_UTIL_PATH);
    session.settle.after(&mut session.client, &[(util, None)])?;
    let spaced = session.edit(&[(CHECKED_UTIL_PATH, &[(2, 6, 6, " ")])])?;
    fails_naming(
        session.check(&spaced, &[], rules(Rename)),
        "cheby fmt",
        "an extra space in a file that was not open",
    )?;
    session.end()
}

/// A module moved with the edit of its importer passes, and the open
/// documents follow it; moved back with no edit, it fails, naming the
/// importer.
fn check_moves(env: &Env, dir: &Path, kind: ClientKind) -> Result<(), String> {
    let strict = rules(editing::FixedPoint::Strict);
    let mut session = checked_package(env, &dir.join("moves"), kind)?;
    let moves = [(CHECKED_UTIL_PATH.to_owned(), "src/tools.cheby".to_owned())];
    let importer = session.edit(&[(ROOT_MODULE, &[(2, 17, 21, "tools"), (5, 14, 18, "tools")])])?;
    session.check(&importer, &moves, strict)?;
    if !session.texts.contains_key("src/tools.cheby")
        || session.texts.contains_key(CHECKED_UTIL_PATH)
    {
        return Err(format!(
            "after the move, the open documents are {:?}",
            session.texts.keys()
        ));
    }
    let back = [("src/tools.cheby".to_owned(), CHECKED_UTIL_PATH.to_owned())];
    let result = session.check(&json!({}), &back, strict);
    fails_naming(result, "src/main.cheby: E", "the module moved back alone")?;
    session.end()
}

/// An edit of `src/aaaa.cheby` alone that breaks its caller in
/// `src/main.cheby`, a file the edit did not touch, fails naming that file,
/// and passes when the header's codes excuse the error. The edited file's
/// publish comes first, so the check fails here only because it waits for
/// `cheby/idle`, not for the edited version alone (D-1676).
fn check_callers(env: &Env, dir: &Path, kind: ClientKind) -> Result<(), String> {
    let strict = rules(editing::FixedPoint::Strict);
    let renamed: &[HandEdit<'_>] = &[(2, 7, 13, "helpme")];
    let mut session = checked_package(env, &dir.join("caller"), kind)?;
    let result = session.check(&session.edit(&[(CHECKED_UTIL_PATH, renamed)])?, &[], strict);
    fails_naming(result, "src/main.cheby: E", "the helper renamed alone")?;
    // The check waited for the server, so the caller's list is its last.
    let main = session.client.uri(Path::new(ROOT_MODULE))?;
    let codes: Vec<String> = session
        .client
        .published()
        .get(&main)
        .map_or_default(client::diagnostics_of)
        .iter()
        .filter_map(|d| d.get("code").and_then(Value::as_str).map(str::to_owned))
        .collect();
    session.end()?;
    let mut session = checked_package(env, &dir.join("excused"), kind)?;
    let excused = editing::EditRules {
        new_errors: &codes,
        fixed_point: editing::FixedPoint::Strict,
    };
    session.check(
        &session.edit(&[(CHECKED_UTIL_PATH, renamed)])?,
        &[],
        excused,
    )?;
    session.end()
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

/// D-1442, D-1373: `didClose` and `didOpen` forget a path's last publish,
/// so a closed file gets its list again without a version, and a reopened
/// one with its version, though the list is the same. Zed starts versions
/// again at 0 when it reopens a path (CP1).
fn close_and_reopen_publish_again(env: &Env, dir: &Path) -> Result<(), String> {
    write_package(
        dir,
        &[("cheby.toml", MANIFEST), (ROOT_MODULE, BROKEN_TYPE_MAIN)],
    )?;
    let (mut client, _) = session(env, dir, ClientKind::Standard, None, None)?;
    let uri = client.open(Path::new(ROOT_MODULE))?;
    let open = client.diagnostics(&uri, Some(1))?;
    if !has_code(&open, "E0047") {
        return Err(format!("version 1 has no E0047: {open:?}"));
    }
    // The closed file's publish at start, if any, came before version 1's.
    client.take_notifications();
    client.close(&uri)?;
    let closed = client.notification("the publish of the closed src/main.cheby", |m| {
        client::is_publish_of(m, &uri) && !has_version(m)
    })?;
    if !has_code(&client::diagnostics_of(&closed), "E0047") {
        return Err(format!("the closed file's publish has no E0047: {closed}"));
    }
    client.open_version(&uri, BROKEN_TYPE_MAIN, 0)?;
    let reopened = client.diagnostics(&uri, Some(0))?;
    if !has_code(&reopened, "E0047") {
        return Err(format!("the reopened version 0 has no E0047: {reopened:?}"));
    }
    client.shutdown()?.expect(0)
}

/// `features.md` §5.4: CRLF is converted as `cheby fmt` converts it, by the
/// diff. A CRLF text otherwise in `cheby fmt`'s form gets edits, and
/// applying the edits of either text gives `cheby fmt`'s output.
fn crlf_is_converted(env: &Env, dir: &Path) -> Result<(), String> {
    write_package(dir, &[("cheby.toml", MANIFEST), (ROOT_MODULE, CLEAN_MAIN)])?;
    let (mut client, _) = session(env, dir, ClientKind::Standard, None, None)?;
    let uri = client.uri(Path::new(ROOT_MODULE))?;
    for (what, lf) in [
        ("in `cheby fmt`'s form", CLEAN_MAIN),
        ("misformatted", MISFORMATTED_MAIN),
    ] {
        let crlf = lf.replace('\n', "\r\n");
        client.open_text(&uri, &crlf)?;
        let answer = client.request(
            "textDocument/formatting",
            json!({ "textDocument": { "uri": uri }, "options": case::formatting_options() }),
        )?;
        if answer.as_array().is_none_or(Vec::is_empty) {
            return Err(format!(
                "a CRLF text {what} was formatted with no edit: {answer}"
            ));
        }
        check_formatting(env, dir, &crlf, &answer, client.encoding())
            .map_err(|e| format!("a CRLF text {what}: {e}"))?;
        client.close(&uri)?;
    }
    client.shutdown()?.expect(0)
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
        sweep_example(&env, scratch.path(), &trial_name)
    })
}

/// Opens every source and sends every advertised request at every
/// identifier; each must be answered without an error. Completion goes to
/// the end of every identifier and after every `.` and `|>`, signature
/// help inside every argument list, each within its budget per example
/// ([`COMPLETION_BUDGET`], [`SIGNATURE_BUDGET`]).
fn sweep_example(env: &Env, dir: &Path, trial_name: &str) -> Result<(), String> {
    let files = sources(dir)?;
    let (mut client, init) = session(env, dir, ClientKind::Standard, None, None)?;
    let advertised = Advertised::of(init.get("capabilities").unwrap_or(&Value::Null))?;
    let uris = open_all(&mut client, &files)?;
    let encoding = client.encoding();
    let mut texts = Vec::with_capacity(files.len());
    for file in &files {
        let path = dir.join(file);
        texts.push(std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?);
    }
    let points = EditingPoints::new(trial_name, &texts, &advertised, encoding);
    // The queries of `workspace/symbol`: none, then every name once.
    let mut queries = BTreeSet::from([String::new()]);
    // Where a rename may go, by the label of its `prepareRename`, and the
    // answers of those.
    let mut renames = Vec::new();
    let mut prepared_renames = BTreeMap::new();
    for (index, ((file, uri), text)) in files.iter().zip(&uris).zip(&texts).enumerate() {
        let text = text.as_str();
        let lines = Lines::new(text, Bom::Counted);
        let diagnostics = client
            .published()
            .get(uri)
            .map_or_default(client::diagnostics_of);
        let mut requests = Vec::new();
        for (start, end) in case::identifiers(text) {
            if let Some(word) = text.get(start..end) {
                queries.insert(word.to_owned());
            }
            let (a, b) = (
                lines.position(start, encoding),
                lines.position(end, encoding),
            );
            for name in &advertised.position {
                // These two have positions of their own, below.
                if matches!(*name, "completion" | "signatureHelp") {
                    continue;
                }
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
            let label = format!("{file}:{}:{} prepareRename", a.line, a.character);
            renames.push((label, (file.clone(), uri.clone(), a, new_name)));
        }
        let last = lines.position(text.len(), encoding);
        for name in &advertised.document {
            let (method, params) = case::document_params(name, uri, last)?;
            requests.push((format!("{file} {name}"), method, params));
        }
        let completions = points.requests(index, file, uri, text, &mut requests)?;
        let prepared = send_all(&mut client, requests)?;
        send_all(&mut client, hierarchy_calls(&prepared))?;
        sweep_completions(
            &mut client,
            &prepared,
            &completions,
            advertised.completion_resolve,
        )?;
        // Only `prepareRename`'s answers decide which renames are sent.
        prepared_renames.extend(
            prepared
                .iter()
                .filter(|(label, _)| label.ends_with(" prepareRename"))
                .map(|(label, answer)| (label.clone(), answer.clone())),
        );
        if let Some(edits) = prepared.get(&format!("{file} formatting"))
            && edits.as_array().is_some_and(|e| !e.is_empty())
        {
            return Err(format!(
                "formatting {file}, a `cheby fmt` fixed point, gave edits: {edits}"
            ));
        }
    }
    if advertised.rename {
        send_renames(&mut client, trial_name, &renames, &prepared_renames)?;
    }
    if advertised.workspace_symbol {
        let requests = queries
            .into_iter()
            .map(|query| {
                let label = format!("workspace/symbol `{query}`");
                (label, "workspace/symbol", json!({ "query": query }))
            })
            .collect();
        send_all(&mut client, requests)?;
    }
    client.shutdown()?.expect(0)
}

/// One point of the examples group where a rename may go: its file, URI,
/// position and new name.
type RenamePoint = (String, String, Position, &'static str);

/// A rename at each of `renames`, each with the label of its
/// `prepareRename`, where `prepared` holds an accepted answer, at most
/// [`RENAME_BUDGET`] of them by a fixed stride (D-1677, D-1725). It says on
/// standard error how many it dropped.
fn send_renames(
    client: &mut Client,
    trial_name: &str,
    renames: &[(String, RenamePoint)],
    prepared: &BTreeMap<String, Value>,
) -> Result<(), String> {
    let (kept, dropped) = case::accepted_renames(renames, prepared, RENAME_BUDGET);
    if dropped > 0 {
        eprintln!(
            "{trial_name}: rename at {} of {} accepted points, {dropped} dropped by a fixed stride",
            kept.len(),
            kept.len().saturating_add(dropped)
        );
    }
    let mut requests = Vec::new();
    for (file, uri, at, new_name) in kept {
        let (method, params) =
            case::position_params("rename", &uri, at, None, &[new_name.to_owned()], &[])?;
        requests.push((
            format!("{file}:{}:{} rename", at.line, at.character),
            method,
            params,
        ));
    }
    send_all(client, requests)?;
    Ok(())
}

/// Both directions of each item that the `prepareCallHierarchy` answers
/// among `prepared` hold.
fn hierarchy_calls(prepared: &BTreeMap<String, Value>) -> Vec<(String, &'static str, Value)> {
    let mut calls = Vec::new();
    for (label, answer) in prepared {
        if !label.ends_with(" prepareCallHierarchy") {
            continue;
        }
        for item in answer.as_array().into_iter().flatten() {
            for method in ["callHierarchy/incomingCalls", "callHierarchy/outgoingCalls"] {
                calls.push((format!("{label} {method}"), method, json!({ "item": item })));
            }
        }
    }
    calls
}

/// Sends `requests` in batches; each must be answered without an error,
/// except a `RequestFailed` of `prepareRename` and `rename`, their
/// refusal (`case::refusal_allowed`), which gets no answer here. Returns
/// the answers by label.
fn send_all(
    client: &mut Client,
    requests: Vec<(String, &'static str, Value)>,
) -> Result<BTreeMap<String, Value>, String> {
    let mut answers = BTreeMap::new();
    let mut rest = requests.into_iter().peekable();
    while rest.peek().is_some() {
        let chunk: Vec<_> = rest.by_ref().take(BATCH).collect();
        let methods: Vec<&'static str> = chunk.iter().map(|(_, method, _)| *method).collect();
        let (labels, calls): (Vec<String>, Vec<(&str, Value)>) = chunk
            .into_iter()
            .map(|(label, method, params)| (label, (method, params)))
            .unzip();
        let results = client.batch(calls)?;
        for ((label, method), result) in labels.into_iter().zip(methods).zip(results) {
            match result {
                Ok(value) => {
                    answers.insert(label, value);
                }
                Err(e) if case::refusal_allowed(method, e.code) => {}
                Err(e) => return Err(client.fail(&format!("{label} answered {e}"))),
            }
        }
    }
    Ok(answers)
}

/// Renames the examples group sends per example, at most, each a `verify`
/// run in the server (plan §1.9, D-1677, D-1725).
const RENAME_BUDGET: usize = 50;
/// Symbols `rename_sweep/*` renames per example, at most, taken by a
/// fixed stride over the accepted points (D-1677, D-1716, D-1725).
const RENAME_SWEEP_BUDGET: usize = 30;
/// Symbols `rename_run/*` renames and runs per example (D-1677).
const RENAME_RUN_SYMBOLS: usize = 8;
/// Examples `rename_run/*` leaves out: 034 prints the source positions of
/// its log calls, which a longer name moves (D-1677).
const RENAME_RUN_SKIP: &[&str] = &["034_logging"];
/// Examples with no declaration of a safe kind, so `rename_run/*` finds no
/// candidate there and passes with nothing renamed. Anywhere else no
/// candidate fails the trial, since it means the `declaration` modifier or
/// the legend regressed: 001 declares only `fn main`.
const RENAME_RUN_NO_CANDIDATES: &[&str] = &["001_hello_world"];

/// Completion points the examples group sends per example, at most. An
/// answer can hold every name in scope, so each costs far more than a
/// hover; beyond the budget a deterministic stride is kept and the trial
/// says on standard error how many were dropped.
const COMPLETION_BUDGET: usize = 600;
/// Signature help points the examples group sends per example, at most.
const SIGNATURE_BUDGET: usize = 600;

/// Where one example's sweep asks for completion and signature help: byte
/// offsets by file index, sampled within the budgets.
struct EditingPoints {
    completion: Vec<(usize, usize)>,
    signature: Vec<(usize, usize)>,
    /// The advertised trigger characters of completion.
    triggers: Vec<String>,
    encoding: Encoding,
}

impl EditingPoints {
    /// The points of `texts`, the example's sources in order, for the
    /// requests `advertised` holds, each kind sampled within its budget.
    fn new(name: &str, texts: &[String], advertised: &Advertised, encoding: Encoding) -> Self {
        let has = |request: &str| advertised.position.contains(&request);
        let all = |points: fn(&str) -> Vec<usize>| -> Vec<(usize, usize)> {
            texts
                .iter()
                .enumerate()
                .flat_map(|(index, text)| points(text).into_iter().map(move |at| (index, at)))
                .collect()
        };
        let pick = |request: &str, points: fn(&str) -> Vec<usize>, budget: usize| {
            if !has(request) {
                return Vec::new();
            }
            let all = all(points);
            let (kept, dropped) = editing::sample(&all, budget);
            if dropped > 0 {
                eprintln!(
                    "{name}: {request} at {} of {} points, {dropped} dropped by a fixed stride",
                    kept.len(),
                    all.len()
                );
            }
            kept
        };
        Self {
            completion: pick("completion", editing::completion_points, COMPLETION_BUDGET),
            signature: pick("signatureHelp", editing::signature_points, SIGNATURE_BUDGET),
            triggers: advertised.completion_triggers.clone(),
            encoding,
        }
    }

    /// Adds the completion and signature help requests of file `index` to
    /// `requests`, and returns each completion's label and position.
    fn requests(
        &self,
        index: usize,
        file: &str,
        uri: &str,
        text: &str,
        requests: &mut Vec<(String, &'static str, Value)>,
    ) -> Result<Vec<(String, Position)>, String> {
        let (lines, encoding) = (Lines::new(text, Bom::Counted), self.encoding);
        let in_file = |points: &[(usize, usize)]| -> Vec<usize> {
            points
                .iter()
                .filter(|(i, _)| *i == index)
                .map(|&(_, at)| at)
                .collect()
        };
        let mut completions = Vec::new();
        for offset in in_file(&self.completion) {
            let at = lines.position(offset, encoding);
            let label = format!("{file}:{}:{} completion", at.line, at.character);
            let (method, mut params) =
                case::position_params("completion", uri, at, None, &[], &[])?;
            if let Some(map) = params.as_object_mut() {
                map.insert(
                    "context".to_owned(),
                    editing::completion_context(text, offset, &self.triggers),
                );
            }
            requests.push((label.clone(), method, params));
            completions.push((label, at));
        }
        for offset in in_file(&self.signature) {
            let at = lines.position(offset, encoding);
            let (method, params) = case::position_params("signatureHelp", uri, at, None, &[], &[])?;
            requests.push((
                format!("{file}:{}:{} signatureHelp", at.line, at.character),
                method,
                params,
            ));
        }
        Ok(completions)
    }
}

/// Checks every item of the sweep's completion answers (D-1394), then
/// resolves the first item of each, when resolve is advertised, with no
/// error.
fn sweep_completions(
    client: &mut Client,
    answers: &BTreeMap<String, Value>,
    completions: &[(String, Position)],
    resolve: bool,
) -> Result<(), String> {
    let mut resolves = Vec::new();
    for (label, at) in completions {
        let Some(answer) = answers.get(label) else {
            continue;
        };
        let list = editing::completions(answer).map_err(|e| format!("{label}: {e}"))?;
        for item in &list.items {
            editing::check_edit(item, *at).map_err(|e| format!("{label}: {e}"))?;
        }
        if resolve && let Some(first) = list.items.first() {
            resolves.push((
                format!("{label} resolve"),
                "completionItem/resolve",
                first.clone(),
            ));
        }
    }
    send_all(client, resolves)?;
    Ok(())
}

// ---------------------------------------------------------------------
// rename_sweep/* and rename_run/*

fn rename_sweep_trial(
    ex: Example,
    env: &Arc<Env>,
    current: Setting,
    verdicts: &Verdicts,
    bless: bool,
) -> Trial {
    let name = format!("rename_sweep/{}", ex.name);
    let expectation = ex.check_only_expectation(current);
    let milestone = ex.config.milestone;
    let env = Arc::clone(env);
    let trial_name = name.clone();
    // Only 030 is not run, since its `cheby check` fails before any
    // rename (see [`rename_run_trial`]).
    let checks = !ex.config.skip_run;
    trial(name, milestone, expectation, verdicts, bless, move || {
        let scratch = Scratch::new(&trial_name)?;
        copy_dir(&ex.dir, scratch.path())?;
        sweep_renames(&env, scratch.path(), &trial_name, checks)
    })
}

/// Every identifier word of `texts`, so a new name can be fresh.
fn words_of<'a>(texts: impl Iterator<Item = &'a String>) -> BTreeSet<String> {
    let mut words = BTreeSet::new();
    for text in texts {
        for (start, end) in case::identifiers(text) {
            if let Some(word) = text.get(start..end) {
                words.insert(word.to_owned());
            }
        }
    }
    words
}

/// A name no identifier of the package uses, from `n` on, of the case of
/// `word`, so a type stays a type and a value a value.
fn fresh_name(word: &str, taken: &BTreeSet<String>, n: usize) -> String {
    let upper = word.starts_with(char::is_uppercase);
    let mut n = n;
    loop {
        let name = if upper {
            format!("Renamed{n}")
        } else {
            format!("renamed_{n}")
        };
        if !taken.contains(&name) {
            return name;
        }
        n = n.saturating_add(1);
    }
}

/// The places `references` gives for the symbol at `at` of `uri`, by URI and
/// start, with the declaration; none when it fails.
fn references_of(
    client: &mut Client,
    uri: &str,
    at: Position,
) -> Result<Vec<(String, Position)>, String> {
    let (method, params) = case::position_params("references", uri, at, None, &[], &[])?;
    let Ok(answer) = client.call(method, params)? else {
        return Ok(Vec::new());
    };
    Ok(answer
        .as_array()
        .map_or_default(Vec::as_slice)
        .iter()
        .filter_map(|location| {
            let uri = location.get("uri").and_then(Value::as_str)?;
            let start = Position::from_json(location.pointer("/range/start")?).ok()?;
            Some((uri.to_owned(), start))
        })
        .collect())
}

/// A session of the example copied to `dir`, with every source open, when
/// the server advertises rename and `prepareRename`: the client, its
/// capabilities, and each source's path, URI and text, in order. Else the
/// session is ended, and there is none.
type RenameSession = (Client, Value, Vec<(String, String, String)>);

fn rename_session(env: &Env, dir: &Path) -> Result<Option<RenameSession>, String> {
    let files = sources(dir)?;
    let (mut client, init) = session(env, dir, ClientKind::Standard, None, None)?;
    let capabilities = init.get("capabilities").cloned().unwrap_or(Value::Null);
    let advertised = Advertised::of(&capabilities)?;
    if !(advertised.rename && advertised.position.contains(&"prepareRename")) {
        client.shutdown()?.expect(0)?;
        return Ok(None);
    }
    let uris = open_all(&mut client, &files)?;
    let mut opened = Vec::with_capacity(files.len());
    for (file, uri) in files.into_iter().zip(uris) {
        let path = dir.join(&file);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        opened.push((file, uri, text));
    }
    Ok(Some((client, capabilities, opened)))
}

/// `prepareRename` at every identifier of `opened`, the sources of a
/// [`rename_session`]: each point, by the label of its request, with its
/// URI, position and word, and the answers by label.
type Prepared = (
    Vec<(String, (String, Position, String))>,
    BTreeMap<String, Value>,
);

fn prepare_everywhere(
    client: &mut Client,
    opened: &[(String, String, String)],
) -> Result<Prepared, String> {
    let encoding = client.encoding();
    let mut points = Vec::new();
    let mut requests = Vec::new();
    for (file, uri, text) in opened {
        let lines = Lines::new(text, Bom::Counted);
        for (start, end) in case::identifiers(text) {
            let at = lines.position(start, encoding);
            let label = format!("{file}:{}:{} prepareRename", at.line, at.character);
            let (method, params) = case::position_params("prepareRename", uri, at, None, &[], &[])?;
            requests.push((label.clone(), method, params));
            let word = text.get(start..end).unwrap_or_default().to_owned();
            points.push((label, (uri.clone(), at, word)));
        }
    }
    let prepared = send_all(client, requests)?;
    Ok((points, prepared))
}

/// The points of `opened` where `prepareRename` accepts a rename, each
/// with its URI, position and word, in file order.
///
/// # Errors
///
/// Returns a message when it accepts none, as when every request failed:
/// every example declares `main`, so none is never right.
fn accepted_points(
    client: &mut Client,
    opened: &[(String, String, String)],
) -> Result<Vec<(String, Position, String)>, String> {
    let (points, prepared) = prepare_everywhere(client, opened)?;
    let (accepted, _) = case::accepted_renames(&points, &prepared, 0);
    if accepted.is_empty() {
        return Err(client.fail(&format!(
            "`prepareRename` accepted none of {} points",
            points.len()
        )));
    }
    Ok(accepted)
}

/// D-1677's server half: the symbols of the example at `dir` are renamed
/// once each to a fresh name, from the points where `prepareRename`
/// accepts them taken in [`editing::by_stride`]'s order, up to
/// [`RENAME_SWEEP_BUDGET`] renames spread over the example (D-1716), and
/// each edit goes through the applied-edit check (D-1676, D-1670), then is
/// undone. A point that an earlier edit, or the references of an earlier
/// refused rename, covered is the same symbol and is skipped, and so is an
/// edit whose ranges an earlier one had. It says on standard error how
/// many it checked. A refusal fails the trial unless
/// [`case::sweep_refusal_allowed`] takes it, with `checks` false for an
/// example whose `cheby check` fails before any rename; so do a `null`
/// answer, which `prepareRename` accepted, a `prepareRename` that
/// accepted no point, as when every request failed, since every example
/// declares `main`, and a sweep that checked no edit.
fn sweep_renames(env: &Env, dir: &Path, trial_name: &str, checks: bool) -> Result<(), String> {
    let (mut client, _, opened) = rename_session(env, dir)?
        .ok_or("the server does not advertise `rename` and `prepareRename`")?;
    let mut texts: BTreeMap<String, String> = opened
        .iter()
        .map(|(file, _, text)| (file.clone(), text.clone()))
        .collect();
    let words = words_of(texts.values());
    let mut settle = editing::Settle::new("", true, &client);
    let accepted = accepted_points(&mut client, &opened)?;
    let order = editing::by_stride(&accepted, RENAME_SWEEP_BUDGET);
    let rules = editing::EditRules {
        new_errors: &[],
        fixed_point: editing::FixedPoint::Rename,
    };
    let mut covered: BTreeSet<(String, Position)> = BTreeSet::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let (mut sent, mut checked, mut refused) = (0_usize, 0_usize, 0_usize);
    for (uri, at, word) in &order {
        if sent == RENAME_SWEEP_BUDGET {
            break;
        }
        if covered.contains(&(uri.clone(), *at)) {
            continue;
        }
        sent = sent.saturating_add(1);
        let new_name = fresh_name(word, &words, sent);
        let what = format!(
            "{}:{}:{} `{word}` to `{new_name}`",
            client.display(uri),
            at.line.saturating_add(1),
            at.character.saturating_add(1)
        );
        let (method, params) = case::position_params(
            "rename",
            uri,
            *at,
            None,
            std::slice::from_ref(&new_name),
            &[],
        )?;
        let answer = match client.call(method, params)? {
            Ok(Value::Null) => {
                return Err(client.fail(&format!(
                    "renaming {what} answered `null`, where `prepareRename` accepted it"
                )));
            }
            Ok(answer) => case::canonical("rename", answer),
            Err(e)
                if case::refusal_allowed(method, e.code)
                    && case::sweep_refusal_allowed(&e.message, checks) =>
            {
                refused = refused.saturating_add(1);
                covered.insert((uri.clone(), *at));
                covered.extend(references_of(&mut client, uri, *at)?);
                continue;
            }
            Err(e) => {
                return Err(client.fail(&format!(
                    "renaming {what} answered {e}, which a fresh name cannot get \
                     unless the edit missed an occurrence"
                )));
            }
        };
        covered.insert((uri.clone(), *at));
        let files = editing::file_edits(&answer).map_err(|e| format!("renaming {what}: {e}"))?;
        let mut ranges = Vec::new();
        for file in &files {
            for edit in &file.edits {
                if let Some(start) = edit.pointer("/range/start")
                    && let Ok(start) = Position::from_json(start)
                {
                    covered.insert((file.uri.clone(), start));
                }
                ranges.push(json!([file.uri, edit.get("range")]));
            }
        }
        if ranges.is_empty() || !seen.insert(Value::Array(ranges).to_string()) {
            continue;
        }
        let applied =
            editing::check_applied(&mut client, &mut texts, &mut settle, &answer, &[], rules)
                .map_err(|e| format!("renaming {what}: {e}"))?;
        editing::revert(&mut client, &mut texts, &mut settle, applied)?;
        checked = checked.saturating_add(1);
    }
    eprintln!(
        "{trial_name}: {checked} renames applied and checked, {refused} refused, {sent} sent, \
         from {} accepted points (at most {RENAME_SWEEP_BUDGET} sent)",
        accepted.len()
    );
    if checked == 0 {
        return Err(client.fail(&format!(
            "no rename was applied and checked, of {} accepted points",
            accepted.len()
        )));
    }
    client.shutdown()?.expect(0)
}

/// The trial of D-1677's process half for `ex`, or none for an example it
/// leaves out: one that is only checked (030, whose `cheby check` fails
/// before any rename), one that waits for real time (`timing`), and those
/// of [`RENAME_RUN_SKIP`]. Ignored on Windows, where `cheby check` and
/// `cheby run` build a JIT (D-545, D-641).
fn rename_run_trial(
    ex: Example,
    env: &Arc<Env>,
    current: Setting,
    verdicts: &Verdicts,
    bless: bool,
) -> Option<Trial> {
    if ex.config.skip_run || ex.config.timing || RENAME_RUN_SKIP.contains(&ex.name.as_str()) {
        return None;
    }
    let name = format!("rename_run/{}", ex.name);
    let milestone = ex.config.milestone;
    let expectation = current.expectation(milestone);
    let ignored = platform::runs_skipped() || bless;
    let env = Arc::clone(env);
    let trial_name = name.clone();
    Some(trial(
        name,
        milestone,
        expectation,
        verdicts,
        ignored,
        move || run_renamed(&env, &ex, &trial_name),
    ))
}

/// One semantic token: its position, length, type and modifiers, read
/// with the server's legend.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SemanticToken {
    at: Position,
    length: u32,
    kind: String,
    modifiers: BTreeSet<String>,
}

/// The tokens of a `semanticTokens/full` answer, decoded from their
/// relative form with `legend`, the provider's.
fn semantic_tokens(answer: &Value, legend: &Value) -> Vec<SemanticToken> {
    let names = |key: &str| -> Vec<String> {
        legend
            .get(key)
            .and_then(Value::as_array)
            .map_or_default(Vec::as_slice)
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    };
    let (types, modifiers) = (names("tokenTypes"), names("tokenModifiers"));
    let data: Vec<u32> = answer
        .get("data")
        .and_then(Value::as_array)
        .map_or_default(Vec::as_slice)
        .iter()
        .filter_map(|n| n.as_u64().and_then(|n| u32::try_from(n).ok()))
        .collect();
    let mut out = Vec::new();
    let (mut line, mut character) = (0_u32, 0_u32);
    for [delta_line, delta_start, length, kind, bits] in data.as_chunks::<5>().0 {
        line = line.saturating_add(*delta_line);
        character = if *delta_line == 0 {
            character.saturating_add(*delta_start)
        } else {
            *delta_start
        };
        let kind = usize::try_from(*kind)
            .ok()
            .and_then(|k| types.get(k))
            .cloned()
            .unwrap_or_default();
        let modifiers = modifiers
            .iter()
            .enumerate()
            .filter(|(bit, _)| {
                u32::try_from(*bit).is_ok_and(|bit| bit < 32 && bits & (1 << bit) != 0)
            })
            .map(|(_, name)| name.clone())
            .collect();
        out.push(SemanticToken {
            at: Position { line, character },
            length: *length,
            kind,
            modifiers,
        });
    }
    out
}

/// Whether renaming the declaration `word` of semantic type `kind`, on
/// `line`, cannot change what the program prints (D-1677): a parameter, a
/// type parameter, a local binding but not a module's constant, and a
/// private function but not `main` and not a function an operator or an
/// interface of the prelude is satisfied by name (`features.md` §3.4).
fn safe_to_run(kind: &str, line: &str, word: &str) -> bool {
    let head = line.trim_start();
    match kind {
        "parameter" | "typeParameter" => true,
        "variable" => !(head.starts_with("const ") || head.starts_with("pub")),
        "function" => {
            head.starts_with("fn ")
                && !matches!(
                    word,
                    "main" | "show" | "add" | "sub" | "mul" | "div" | "neg" | "compare"
                )
        }
        _ => false,
    }
}

/// The declarations of `opened`, the sources of a [`rename_session`],
/// whose renaming cannot change what the program prints ([`safe_to_run`]),
/// each with its URI, position and name, in order: told apart by
/// `semanticTokens/full` and the `declaration` modifier, read with the
/// legend of `capabilities`.
fn safe_candidates(
    client: &mut Client,
    capabilities: &Value,
    opened: &[(String, String, String)],
) -> Result<Vec<(String, Position, String)>, String> {
    let legend = capabilities
        .pointer("/semanticTokensProvider/legend")
        .ok_or("the safe kinds are told apart by semantic tokens, which the server lacks")?;
    let encoding = client.encoding();
    let mut candidates = Vec::new();
    for (_, uri, text) in opened {
        let answer = client.request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": uri } }),
        )?;
        let lines = Lines::new(text, Bom::Counted);
        for token in semantic_tokens(&answer, legend) {
            if !token.modifiers.contains("declaration") {
                continue;
            }
            let end = Position {
                line: token.at.line,
                character: token.at.character.saturating_add(token.length),
            };
            let word = lines.slice(token.at, end, encoding).unwrap_or_default();
            let line = usize::try_from(token.at.line)
                .ok()
                .and_then(|n| lines.line(n))
                .unwrap_or_default();
            if safe_to_run(&token.kind, line, word) {
                candidates.push((uri.clone(), token.at, word.to_owned()));
            }
        }
    }
    Ok(candidates)
}

/// D-1677's process half: [`RENAME_RUN_SYMBOLS`] declarations of the safe
/// kinds ([`safe_to_run`]), told apart by their semantic tokens and taken
/// by a fixed stride, are each renamed to a fresh name in a copy of the
/// example, and the copy must pass `cheby check` and `cheby run` with the
/// example's `stdin`, `args` and `env`, printing its `expected.stdout`
/// and exiting with its `expected.exit`. `prepareRename` must accept
/// each, a parameter of a function with no body included (D-1717). Its
/// rename must give an edit, or a refusal that
/// [`case::sweep_refusal_allowed`] takes, and at least one picked symbol
/// must run. It says on standard error how many ran.
fn run_renamed(env: &Env, ex: &Example, trial_name: &str) -> Result<(), String> {
    let goldens = ex.goldens()?;
    let expected = goldens
        .stdout
        .clone()
        .ok_or_else(|| format!("{}: missing expected.stdout", ex.name))?;
    let run_args = ex.run_args(&goldens)?;
    let scratch = Scratch::new(trial_name)?;
    copy_dir(&ex.dir, scratch.path())?;
    let (mut client, capabilities, opened) = rename_session(env, scratch.path())?
        .ok_or("the server does not advertise `rename` and `prepareRename`")?;
    let candidates = safe_candidates(&mut client, &capabilities, &opened)?;
    if candidates.is_empty() && !RENAME_RUN_NO_CANDIDATES.contains(&ex.name.as_str()) {
        return Err(client.fail(
            "no declaration of a safe kind: the semantic tokens marked none with `declaration`",
        ));
    }
    let texts: BTreeMap<String, String> = opened
        .into_iter()
        .map(|(_, uri, text)| (uri, text))
        .collect();
    let words = words_of(texts.values());
    let (picked, _) = editing::sample(&candidates, RENAME_RUN_SYMBOLS);
    let mut ran = 0_usize;
    for (n, (uri, at, word)) in picked.iter().enumerate() {
        let new_name = fresh_name(word, &words, n.saturating_add(1));
        let what = format!(
            "{}:{}:{} `{word}` renamed to `{new_name}`",
            client.display(uri),
            at.line.saturating_add(1),
            at.character.saturating_add(1)
        );
        let (method, params) = case::position_params("prepareRename", uri, *at, None, &[], &[])?;
        match client.call(method, params)? {
            Ok(Value::Null) => {
                return Err(client.fail(&format!(
                    "{what}: `prepareRename` answered `null` at a declaration"
                )));
            }
            Ok(_) => {}
            Err(e) => return Err(client.fail(&format!("{what}: prepareRename answered {e}"))),
        }
        let (method, params) = case::position_params(
            "rename",
            uri,
            *at,
            None,
            std::slice::from_ref(&new_name),
            &[],
        )?;
        let edit = match client.call(method, params)? {
            Ok(Value::Null) => {
                return Err(client.fail(&format!(
                    "{what}: rename answered `null`, where `prepareRename` accepted it"
                )));
            }
            Ok(edit) => edit,
            Err(e)
                if case::refusal_allowed(method, e.code)
                    && case::sweep_refusal_allowed(&e.message, true) =>
            {
                eprintln!("{trial_name}: {what}: refused, skipped: {}", e.message);
                continue;
            }
            Err(e) => return Err(client.fail(&format!("{what}: rename answered {e}"))),
        };
        let mut edited = texts.clone();
        text::apply_workspace_edit(&mut edited, &edit, client.encoding())
            .map_err(|e| format!("{what}: {e}"))?;
        let package = Scratch::new(&format!("{trial_name}/{n}"))?;
        copy_dir(&ex.dir, package.path())?;
        for (uri, text) in &edited {
            let file = client
                .paths()
                .relative(uri)
                .ok_or_else(|| format!("{what}: an edit of {uri}, outside the package"))?;
            let path = package.path().join(file);
            std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        check_and_run(env, package.path(), &goldens, &run_args, &expected)
            .map_err(|e| format!("{what}: {e}"))?;
        ran = ran.saturating_add(1);
    }
    eprintln!(
        "{trial_name}: {ran} of {} picked symbols renamed, checked and run, of {} candidates",
        picked.len(),
        candidates.len()
    );
    if ran == 0 && !picked.is_empty() {
        return Err(client.fail(&format!(
            "none of {} picked symbols was renamed and run",
            picked.len()
        )));
    }
    client.shutdown()?.expect(0)
}

/// `cheby` with `args` in the package at `dir`, with the example's `env`
/// and `stdin` and the harness's own environment.
fn run_cheby(
    env: &Env,
    dir: &Path,
    args: &[&str],
    run_args: &example::RunArgs,
    stdin: Option<&str>,
) -> Result<process::RunOutput, String> {
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never")
        .args(args)
        .current_dir(dir)
        .envs(run_args.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("NO_COLOR", "1")
        .env(POISON_ENV, "1");
    env.cache.apply(&mut cmd);
    process::run(cmd, stdin.map(str::as_bytes), env.timeout)
}

/// `cheby check` passes in the package at `dir`, and `cheby run` prints
/// `expected` and exits as `goldens` say.
fn check_and_run(
    env: &Env,
    dir: &Path,
    goldens: &example::Goldens,
    run_args: &example::RunArgs,
    expected: &str,
) -> Result<(), String> {
    let check = run_cheby(env, dir, &["check"], run_args, None)?;
    if check.status != Some(0) {
        return Err(format!(
            "`cheby check` {}:\n{}",
            check.describe_status(),
            check.stderr
        ));
    }
    let mut args = vec!["run"];
    if !run_args.args.is_empty() {
        args.push("--");
        args.extend(run_args.args.iter().map(String::as_str));
    }
    let run = run_cheby(env, dir, &args, run_args, goldens.stdin.as_deref())?;
    golden::compare("the standard output of `cheby run`", expected, &run.stdout)?;
    let exit = goldens.exit.unwrap_or(0);
    if run.status != Some(exit) {
        return Err(format!(
            "`cheby run` {}, expected {exit}:\n{}",
            run.describe_status(),
            run.stderr
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------
// symbols/examples/*

fn symbols_trial(
    ex: Example,
    env: &Arc<Env>,
    current: Setting,
    verdicts: &Verdicts,
    bless: bool,
) -> Trial {
    let name = format!("symbols/examples/{}", ex.name);
    let expectation = ex.check_only_expectation(current);
    let milestone = ex.config.milestone;
    let env = Arc::clone(env);
    let trial_name = name.clone();
    trial(name, milestone, expectation, verdicts, bless, move || {
        let scratch = Scratch::new(&trial_name)?;
        copy_dir(&ex.dir, scratch.path())?;
        outline_is_the_item_list(&env, scratch.path())
    })
}

/// `features.md` §5.1: the outline of each source file is its item list.
/// A text request never waits for analysis (D-1367), so each is sent as
/// soon as its document is open.
fn outline_is_the_item_list(env: &Env, dir: &Path) -> Result<(), String> {
    let files = sources(dir)?;
    let (mut client, init) = session(env, dir, ClientKind::Standard, None, None)?;
    let advertised = Advertised::of(init.get("capabilities").unwrap_or(&Value::Null))?;
    if !advertised.document.contains(&"documentSymbol") {
        return Err("the server does not advertise `documentSymbolProvider`".to_owned());
    }
    for file in &files {
        let path = dir.join(file);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let uri = client.open(Path::new(file))?;
        let answer = client.request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": uri } }),
        )?;
        outline::check(&outline::expected(&text), &answer)
            .map_err(|e| client.fail(&format!("{file}: {e}")))?;
    }
    client.shutdown()?.expect(0)
}

// ---------------------------------------------------------------------
// navigation/*

/// A root module that calls into std.
const STD_CALLER: &str = "//! A package for the std round trip.\n\nimport std::io\n\nfn main() {\n  io::println(\"hi\")\n}\n";

/// R-17: definition on `io::println` leads into std's materialized
/// sources, that file opens as a std document (A §6.3), and hover inside
/// it answers. On the function's own name it shows the function, and the
/// call hierarchy and links answer there. From the package, the callers
/// of `io::println` include `main`, and its references with the
/// declaration hold std's. A local there cannot be renamed (D-1673).
/// Every other identifier of the file must be answered without an error,
/// `null` included, since a std body has no facts (R-17).
fn std_round_trip(env: &Env, dir: &Path) -> Result<(), String> {
    write_package(dir, &[("cheby.toml", MANIFEST), (ROOT_MODULE, STD_CALLER)])?;
    let (mut client, init) = session(env, dir, ClientKind::Standard, None, None)?;
    let advertised = Advertised::of(init.get("capabilities").unwrap_or(&Value::Null))?;
    for needed in ["definition", "hover"] {
        if !advertised.position.contains(&needed) {
            return Err(format!("the server does not advertise `{needed}`"));
        }
    }
    let encoding = client.encoding();
    let uri = client.open(Path::new(ROOT_MODULE))?;
    client.diagnostics(&uri, Some(1))?;
    let at = STD_CALLER
        .find("println")
        .ok_or("no `println` in the caller")?;
    let position = Lines::new(STD_CALLER, Bom::Counted).position(at, encoding);
    let answer = client.request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri }, "position": position.to_json() }),
    )?;
    let (target, name_at) = first_target(&answer)
        .ok_or_else(|| client.fail(&format!("definition of `io::println` gave {answer}")))?;
    let shown = client.display(&target);
    if !(shown.starts_with("<std>/") && shown.ends_with("io.cheby")) {
        return Err(client.fail(&format!(
            "definition of `io::println` is in {shown}, not std's `io.cheby`"
        )));
    }
    let path = cheby_test_support::lsp::uri::uri_to_path(&target)
        .ok_or_else(|| format!("{target} is not a file URI"))?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let lines = Lines::new(&text, Bom::Counted);
    let name_end = Position {
        character: name_at.character.saturating_add(7),
        ..name_at
    };
    if lines.slice(name_at, name_end, encoding) != Some("println") {
        return Err(client.fail(&format!(
            "definition of `io::println` points at {}:{}, which is not its name",
            name_at.line, name_at.character
        )));
    }
    // A std document is never published (A §6.3), so nothing is waited
    // for: the hovers wait for its text themselves (D-1367).
    client.open_text(&target, &text)?;
    let hover = |position: Position| {
        (
            "textDocument/hover",
            json!({ "textDocument": { "uri": target }, "position": position.to_json() }),
        )
    };
    let on_name = client.request("textDocument/hover", hover(name_at).1)?;
    if !quality::shown(&on_name).is_some_and(|s| s.contains("println")) {
        return Err(client.fail(&format!(
            "hover on `println` in std's `io.cheby` gave {on_name}"
        )));
    }
    std_calls_and_links(&mut client, &uri, position, &target, name_at)?;
    std_rename_refused(&mut client, &target, &text)?;
    let requests = case::identifiers(&text)
        .into_iter()
        .map(|(start, _)| {
            let at = lines.position(start, encoding);
            let (method, params) = hover(at);
            (
                format!("{shown}:{}:{} hover", at.line, at.character),
                method,
                params,
            )
        })
        .collect();
    send_all(&mut client, requests)?;
    client.shutdown()?.expect(0)
}

/// D-1673: in the opened std document `target`, whose text is `text`,
/// `prepareRename` refuses a local, here the parameter `outcome` of
/// `io`'s `lift`, with D-1390's message, and `rename` there answers
/// `null` by the route rule for requests that edit, with no reason.
fn std_rename_refused(client: &mut Client, target: &str, text: &str) -> Result<(), String> {
    let at = text
        .find("(outcome:")
        .ok_or("no parameter `outcome` in std's `io.cheby`")?
        .saturating_add(1);
    let position = Lines::new(text, Bom::Counted)
        .position(at, client.encoding())
        .to_json();
    let document = json!({ "uri": target });
    let method = "textDocument/prepareRename";
    let params = json!({ "textDocument": document, "position": position });
    let refusal = "`outcome` is in `std`, which cannot be edited here";
    match client.call(method, params)? {
        Err(e) if case::refusal_allowed(method, e.code) && e.message == refusal => {}
        Err(e) => {
            return Err(client.fail(&format!(
                "prepareRename on `outcome` in std's `io.cheby` answered {e}, not {refusal:?}"
            )));
        }
        Ok(answer) => {
            return Err(client.fail(&format!(
                "prepareRename on `outcome` in std's `io.cheby` answered {answer}, not a refusal"
            )));
        }
    }
    let params = json!({ "textDocument": document, "position": position, "newName": "result" });
    let answer = client.request("textDocument/rename", params)?;
    if !answer.is_null() {
        return Err(client.fail(&format!(
            "rename in std's `io.cheby` answered {answer}, not `null`"
        )));
    }
    Ok(())
}

/// The std round trip's call hierarchy and links: `println`'s own name
/// in the opened std document `target` prepares, and the document links.
/// From the package's `uri`, at `position` on `io::println`, its callers
/// include `main`, and its references with the declaration hold std's
/// name at `name_at`.
fn std_calls_and_links(
    client: &mut Client,
    uri: &str,
    position: Position,
    target: &str,
    name_at: Position,
) -> Result<(), String> {
    // The call hierarchy and links answer in the std document too.
    let in_std = json!({ "textDocument": { "uri": target }, "position": name_at.to_json() });
    let prepared = client.request("textDocument/prepareCallHierarchy", in_std)?;
    if prepared.pointer("/0/name") != Some(&json!("println")) {
        return Err(client.fail(&format!(
            "prepareCallHierarchy on `println` in std's `io.cheby` gave {prepared}"
        )));
    }
    let doc_links = client.request(
        "textDocument/documentLink",
        json!({ "textDocument": { "uri": target } }),
    )?;
    if doc_links.as_array().is_none_or(Vec::is_empty) {
        return Err(client.fail(&format!(
            "documentLink in std's `io.cheby`, which imports, gave {doc_links}"
        )));
    }
    // The package's callers of a std function come from the package that
    // asked, and its references hold std's declaration.
    let in_main = json!({ "textDocument": { "uri": uri }, "position": position.to_json() });
    let prepared = client.request("textDocument/prepareCallHierarchy", in_main)?;
    let item = prepared.get(0).cloned().ok_or_else(|| {
        client.fail(&format!(
            "prepareCallHierarchy on `io::println` gave {prepared}"
        ))
    })?;
    let incoming = client.request("callHierarchy/incomingCalls", json!({ "item": item }))?;
    if incoming.pointer("/0/from/name") != Some(&json!("main")) {
        return Err(client.fail(&format!(
            "incomingCalls of `io::println` gave {incoming}, not `main`"
        )));
    }
    let refs = client.request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": uri },
            "position": position.to_json(),
            "context": { "includeDeclaration": true },
        }),
    )?;
    let has_decl = refs.as_array().is_some_and(|locations| {
        locations.iter().any(|l| {
            l.get("uri").and_then(Value::as_str) == Some(target)
                && l.pointer("/range/start")
                    .and_then(|p| Position::from_json(p).ok())
                    == Some(name_at)
        })
    });
    if !has_decl {
        return Err(client.fail(&format!(
            "references of `io::println` with its declaration gave {refs}"
        )));
    }
    Ok(())
}

/// The URI and the name's position of the first target of a definition
/// answer: a `Location`, a list of them, or a list of `LocationLink`s.
fn first_target(answer: &Value) -> Option<(String, Position)> {
    let first = match answer {
        Value::Array(items) => items.first()?,
        other => other,
    };
    let (uri, range) = match first.get("targetUri") {
        Some(uri) => (uri, first.get("targetSelectionRange")?),
        None => (first.get("uri")?, first.get("range")?),
    };
    let start = Position::from_json(range.get("start")?).ok()?;
    Some((uri.as_str()?.to_owned(), start))
}

// ---------------------------------------------------------------------
// quality/*

/// Edits of each kind the quality gate samples. `facts.md` §0.1 took 100
/// in process; through a debug server 50 of each, about 500 mutants,
/// keep the trial near its budget.
const QUALITY_PER_KIND: usize = 50;

/// Servers the quality gate runs at once, one example each.
const QUALITY_WORKERS: usize = 4;

/// The quality gate of plan W7 (`facts.md` §0.1, §10): `quality::sample` of the
/// body mutants of every example, each sent to a warm server of its
/// example as an edit of its open file, and hover graded at every
/// identifier of the edited item against the clean text. An example whose
/// clean text has an error, such as one that needs a dependency, is left
/// out, since nothing in it is known when clean. Same must reach
/// `quality::SAME_FLOOR` and the key probe `quality::KEY_FLOOR`. Both
/// numbers are printed, with the table of `facts.md` §0.1.
fn quality_trial(
    examples: &[Example],
    env: &Arc<Env>,
    current: Setting,
    verdicts: &Verdicts,
    bless: bool,
) -> Trial {
    let name = "quality/hover_on_mutants".to_owned();
    let expectation = current.expectation(Milestone::M1);
    let env = Arc::clone(env);
    let examples = examples.to_vec();
    trial(
        name,
        Milestone::M1,
        expectation,
        verdicts,
        bless,
        move || quality_gate(&env, &examples),
    )
}

fn quality_gate(env: &Env, examples: &[Example]) -> Result<(), String> {
    let started = std::time::Instant::now();
    // The clean check comes before the sample, so every mutant drawn is
    // graded and each kind gets its full share.
    let indices: Vec<usize> = (0..examples.len()).collect();
    let mut skipped = Vec::new();
    let mut sites = Vec::new();
    for (&index, clean) in on_workers(&indices, |&index| match examples.get(index) {
        Some(ex) => is_clean(env, ex),
        None => Err(format!("no example {index}")),
    })? {
        let Some(ex) = examples.get(index) else {
            continue;
        };
        if !clean? {
            skipped.push(ex.name.as_str());
            continue;
        }
        for file in sources(&ex.dir)? {
            let path = ex.dir.join(&file);
            let text =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            sites.extend(
                quality::mutants(&file, &text)
                    .into_iter()
                    .map(|m| (index, m)),
            );
        }
    }
    let total_sites = sites.len();
    let chosen = quality::sample(sites, QUALITY_PER_KIND, |(_, m)| m.kind);
    let mut drawn: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_example: BTreeMap<usize, Vec<Mutant>> = BTreeMap::new();
    for (index, m) in chosen {
        let count = drawn.entry(m.kind).or_default();
        *count = count.saturating_add(1);
        by_example.entry(index).or_default().push(m);
    }
    let jobs: Vec<(usize, Vec<Mutant>)> = by_example.into_iter().collect();
    let results = on_workers(&jobs, |(index, mutants)| match examples.get(*index) {
        Some(ex) => gate_example(env, ex, mutants),
        None => Err(format!("no example {index}")),
    })?;
    let mut report = quality::Report::default();
    // Each failure once, with the examples that gave it.
    let mut failures: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for ((index, _), outcome) in results {
        let name = examples.get(*index).map_or("?", |ex| ex.name.as_str());
        match outcome {
            Ok(part) => report.merge(&part),
            Err(e) => failures.entry(e).or_default().push(name),
        }
    }
    if !failures.is_empty() {
        let shown: Vec<String> = failures
            .into_iter()
            .map(|(e, names)| format!("{}: {e}", names.join(", ")))
            .collect();
        return Err(shown.join("\n"));
    }
    let total = report.total();
    println!(
        "quality gate: {} of {total_sites} sites, drawn {}, in {:.1} s; left out, not clean: {}\n{}\
         quality gate: same {:.1}% (floor {}%), key probe {:.1}% (floor {}%)",
        total.mutants,
        drawn
            .iter()
            .map(|(kind, n)| format!("{kind} {n}"))
            .collect::<Vec<_>>()
            .join(", "),
        started.elapsed().as_secs_f64(),
        if skipped.is_empty() {
            "none".to_owned()
        } else {
            skipped.join(", ")
        },
        report.table(),
        total.same_pct(),
        quality::SAME_FLOOR,
        total.key_pct(),
        quality::KEY_FLOOR,
    );
    report
        .verdict()
        .map_err(|e| format!("{e}\n{}", report.table()))
}

/// Runs `f` on each of `jobs` on `QUALITY_WORKERS` threads, and gives
/// each job with its result, in the order of `jobs`.
fn on_workers<T: Sync, R: Send>(
    jobs: &[T],
    f: impl Fn(&T) -> R + Sync,
) -> Result<Vec<(&T, R)>, String> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(BTreeMap::new());
    std::thread::scope(|scope| {
        for _ in 0..QUALITY_WORKERS {
            scope.spawn(|| {
                loop {
                    let job = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(item) = jobs.get(job) else {
                        break;
                    };
                    let outcome = f(item);
                    if let Ok(mut results) = results.lock() {
                        results.insert(job, outcome);
                    }
                }
            });
        }
    });
    let results = results
        .into_inner()
        .map_err(|_| "a quality worker panicked".to_owned())?;
    if results.len() != jobs.len() {
        return Err("a quality worker stopped early".to_owned());
    }
    Ok(jobs.iter().zip(results.into_values()).collect())
}

/// An example's copy, opened in a server that advertises hover.
struct Opened {
    _scratch: Scratch,
    dir: PathBuf,
    client: Client,
    files: Vec<String>,
    uris: Vec<String>,
}

fn open_example(env: &Env, ex: &Example) -> Result<Opened, String> {
    let scratch = Scratch::new(&format!("quality/{}", ex.name))?;
    let dir = scratch.path().to_path_buf();
    copy_dir(&ex.dir, &dir)?;
    let files = sources(&dir)?;
    let (mut client, init) = session(env, &dir, ClientKind::Standard, None, None)?;
    let advertised = Advertised::of(init.get("capabilities").unwrap_or(&Value::Null))?;
    if !advertised.position.contains(&"hover") {
        return Err("the server does not advertise `hoverProvider`".to_owned());
    }
    let uris = open_all(&mut client, &files)?;
    Ok(Opened {
        _scratch: scratch,
        dir,
        client,
        files,
        uris,
    })
}

/// Whether the server publishes no error for any open file.
fn published_clean(client: &Client) -> bool {
    client.published().values().all(|params| {
        client::diagnostics_of(params)
            .iter()
            .all(|d| d.get("severity").and_then(Value::as_u64) != Some(1))
    })
}

/// Whether example `ex`'s clean text has no error, as its server sees it.
fn is_clean(env: &Env, ex: &Example) -> Result<bool, String> {
    // `_scratch` keeps the copy until the server has shut down.
    let Opened {
        _scratch, client, ..
    } = open_example(env, ex)?;
    let clean = published_clean(&client);
    client.shutdown()?.expect(0)?;
    Ok(clean)
}

/// Runs `mutants`, all of example `ex`, whose clean text has no error,
/// through one server.
fn gate_example(env: &Env, ex: &Example, mutants: &[Mutant]) -> Result<quality::Report, String> {
    let Opened {
        _scratch,
        dir,
        mut client,
        files,
        uris,
    } = open_example(env, ex)?;
    if !published_clean(&client) {
        client.shutdown()?.expect(0)?;
        return Err("the clean text had an error on the second open".to_owned());
    }
    let mut by_file: BTreeMap<&str, Vec<&Mutant>> = BTreeMap::new();
    for m in mutants {
        by_file.entry(m.file.as_str()).or_default().push(m);
    }
    let mut report = quality::Report::default();
    for (file, mutants) in by_file {
        let uri = files
            .iter()
            .position(|f| f == file)
            .and_then(|i| uris.get(i))
            .ok_or_else(|| format!("{file} is not open"))?
            .clone();
        let path = dir.join(file);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut wanted: BTreeSet<usize> = case::identifiers(&text)
            .into_iter()
            .map(|(start, _)| start)
            .filter(|&start| mutants.iter().any(|m| m.in_item(start)))
            .collect();
        wanted.extend(mutants.iter().filter_map(|m| m.key.map(|k| k.clean)));
        let clean = hover_at(&mut client, &uri, &text, &wanted)?;
        for m in mutants {
            let Some(mutated) = m.apply(&text) else {
                continue;
            };
            let tally = report.kind(m.kind);
            tally.mutants = tally.mutants.saturating_add(1);
            if quality::has_grammar_error(&mutated) {
                tally.grammar = tally.grammar.saturating_add(1);
            }
            let version = client.change(&uri, &mutated)?;
            client.diagnostics(&uri, Some(version))?;
            let graded: Vec<(usize, usize, &str)> = clean
                .iter()
                .filter(|&(&at, _)| m.in_item(at))
                .filter_map(|(&at, shown)| {
                    let shown = shown.as_deref().filter(|s| !quality::has_hole(s))?;
                    Some((at, m.map(at)?, shown))
                })
                .collect();
            // An open call's probe asks only where the clean hover shows
            // the callee's call signature: a generic item or a local.
            let call = m.kind == "open_call";
            let key = m.key.filter(|k| {
                clean.get(&k.clean).is_some_and(|s| {
                    s.as_deref().is_some_and(|s| {
                        !quality::has_hole(s) && (!call || quality::call_ok(Some(s)))
                    })
                })
            });
            let mut asked: BTreeSet<usize> = graded.iter().map(|&(_, at, _)| at).collect();
            asked.extend(key.map(|k| k.mutated));
            let got = hover_at(&mut client, &uri, &mutated, &asked)
                .map_err(|e| format!("{} at {file}:{}: {e}", m.kind, m.start))?;
            let tally = report.kind(m.kind);
            for (at, mapped, shown) in graded {
                let answer = got.get(&mapped).and_then(Option::as_deref);
                tally.grade(quality::grade(shown, answer), at < m.start);
            }
            if let Some(key) = key {
                let reference = clean.get(&key.clean).and_then(Option::as_deref);
                let answer = got.get(&key.mutated).and_then(Option::as_deref);
                tally.key(if call {
                    quality::call_ok(answer)
                } else {
                    quality::key_ok(reference, answer)
                });
            }
        }
        let version = client.change(&uri, &text)?;
        client.diagnostics(&uri, Some(version))?;
    }
    client.shutdown()?.expect(0)?;
    Ok(report)
}

/// What the quality gate grades of hover at each of the byte offsets `at`
/// of `text`, the text of the open `uri` (`quality::graded_text`). An
/// error answer fails.
fn hover_at(
    client: &mut Client,
    uri: &str,
    text: &str,
    at: &BTreeSet<usize>,
) -> Result<BTreeMap<usize, Option<String>>, String> {
    let lines = Lines::new(text, Bom::Counted);
    let encoding = client.encoding();
    let offsets: Vec<usize> = at.iter().copied().collect();
    let mut shown = BTreeMap::new();
    for chunk in offsets.chunks(BATCH) {
        let calls = chunk
            .iter()
            .map(|&offset| {
                let position = lines.position(offset, encoding);
                (
                    "textDocument/hover",
                    json!({ "textDocument": { "uri": uri }, "position": position.to_json() }),
                )
            })
            .collect();
        for (&offset, answer) in chunk.iter().zip(client.batch(calls)?) {
            let answer = answer.map_err(|e| {
                let position = lines.position(offset, encoding);
                client.fail(&format!(
                    "hover at {}:{} answered {e}",
                    position.line, position.character
                ))
            })?;
            shown.insert(offset, quality::graded_text(&answer));
        }
    }
    Ok(shown)
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
    let name = format!("parity/ui/{}", test.name);
    let milestone = test.header.milestone;
    let expectation = current.expectation(milestone);
    // A golden with a back-end code has no front-end error, so on Windows,
    // where the server evaluates no constant (D-1414), it is ignored here.
    let ignored = (platform::runs_skipped() && test.builds_a_jit()) || bless;
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

/// Opens the root module of the package at `dir`, waits for its publish
/// and then for `cheby/idle`, so the constants child's diagnostics are in
/// (D-1415), and compares every published list with `printed`, whose
/// paths `shown` maps to the package's own.
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
    wait_idle(&mut client, "")?;
    // `shutdown` is answered on the main loop after the publishes of the
    // idle package, so every file's list is in.
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
    // D-545, D-1414: the server evaluates no constant on Windows until M6.
    let ignored = platform::runs_skipped()
        && test
            .header
            .as_ref()
            .is_ok_and(|header| header.steps.iter().any(waits_for_child));
    let env = Arc::clone(env);
    let trial_name = name.clone();
    trial(name, milestone, expectation, verdicts, ignored, move || {
        let bless = bless && expectation != Expectation::Fail;
        let header = test.header.clone()?;
        let scratch = Scratch::new(&trial_name)?;
        match (test.layout, header.loose) {
            (Layout::File, false) => {
                single_file_package(scratch.path(), case::PACKAGE_NAME, &test.path)?;
            }
            (Layout::File, true) => loose_file(scratch.path(), &test.path)?,
            (Layout::Package, false) => copy_dir(&test.path, scratch.path())?,
            (Layout::Package, true) => {
                return Err("`@package: none` needs a test that is one file".to_owned());
            }
        }
        if header.bom || header.crlf {
            let files = if header.loose {
                loose_sources(scratch.path())?
            } else {
                sources(scratch.path())?
            };
            for file in files {
                let path = scratch.path().join(file);
                let rewritten = read_rewritten(&path, &header)?;
                std::fs::write(&path, rewritten).map_err(|e| format!("{}: {e}", path.display()))?;
            }
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
    let (client, files, capabilities) = golden_session(env, dir, header)?;
    golden_blocks(env, dir, header, client, &files, &capabilities)
}

/// A golden test's initialized server, its source files, and the
/// server's `capabilities`.
fn golden_session(
    env: &Env,
    dir: &Path,
    header: &case::Header,
) -> Result<(Client, Vec<String>, Value), String> {
    let files = if header.loose {
        loose_sources(dir)?
    } else {
        sources(dir)?
    };
    let mut client = start(env, dir)?;
    if let Some(settings) = &header.settings {
        client.set_configuration(settings.clone());
    }
    let mut params = client.initialize_params(
        &header.client.capabilities(header.encoding),
        header.settings.clone(),
    )?;
    if let Some(map) = params.as_object_mut() {
        if header.loose {
            // As Zed opens a file outside its worktree: rooted at the
            // file's directory, with no workspace folder (D-1371).
            map.insert("workspaceFolders".to_owned(), json!([]));
        }
        if let Some(name) = &header.client_name {
            map.insert("clientInfo".to_owned(), json!({ "name": name }));
        }
    }
    let init = client.initialize(params)?;
    let capabilities = init.get("capabilities").cloned().unwrap_or(Value::Null);
    Ok((client, files, capabilities))
}

/// The position of `marker` in the client's encoding, and the end of its
/// range when it has one.
fn marker_range(
    lines: &Lines<'_>,
    encoding: Encoding,
    marker: &case::Marker,
) -> (Position, Option<Position>) {
    let at = |line_index: usize, column: usize| Position {
        line: u32::try_from(line_index).unwrap_or(u32::MAX),
        character: u32::try_from(
            lines
                .line(line_index)
                .unwrap_or_default()
                .chars()
                .take(column)
                .fold(0_usize, |n, c| n.saturating_add(encoding.units(c))),
        )
        .unwrap_or(u32::MAX),
    };
    let start = at(marker.line, marker.column);
    let end = marker
        .end
        .map(|column| at(marker.end_line.unwrap_or(marker.line), column));
    (start, end)
}

/// The blocks of [`golden_answers`], from a `client` that has not opened
/// `files` yet, of a server that advertised `capabilities`.
fn golden_blocks(
    env: &Env,
    dir: &Path,
    header: &case::Header,
    mut client: Client,
    files: &[String],
    capabilities: &Value,
) -> Result<String, String> {
    let paths = client.paths().clone();
    let mut texts = open_as_editor(&mut client, dir, header, files)?;
    // The `empty` client does not ask for `cheby/idle`.
    let mut settle = editing::Settle::new(
        &idle_root(header, &texts),
        header.client != ClientKind::Empty,
        &client,
    );
    let mut blocks = Vec::new();
    for step in &header.steps {
        if let Some(block) = run_step(&mut client, dir, header, step, &mut texts, &mut settle)? {
            blocks.push(block);
        }
    }
    let encoding = client.encoding();
    // An unknown provider fails the examples group; here only completion's
    // trigger characters and resolve matter.
    let advertised = Advertised::of(capabilities).unwrap_or_default();
    let display = |uri: &str| paths.display(uri);
    // The documents open after the steps, which may have closed or moved
    // some.
    let open: Vec<String> = texts.keys().cloned().collect();
    for file in &open {
        let uri = client.uri(Path::new(file))?;
        let uri = &uri;
        let text = texts.get(file).cloned().unwrap_or_default();
        let lines = Lines::new(&text, Bom::Counted);
        let diagnostics = client
            .published()
            .get(uri)
            .map_or_default(client::diagnostics_of);
        for marker in case::markers(&text).map_err(|e| format!("{file}: {e}"))? {
            let (start, end) = marker_range(&lines, encoding, &marker);
            let (method, mut params) =
                case::position_params(&marker.name, uri, start, end, &marker.args, &diagnostics)?;
            if matches!(marker.name.as_str(), "completion" | "apply")
                && let Some(map) = params.as_object_mut()
            {
                let offset = lines.offset(start, encoding);
                map.insert(
                    "context".to_owned(),
                    editing::completion_context(&text, offset, &advertised.completion_triggers),
                );
            }
            if marker.name == "rename" {
                // Only the messages this request brings belong to its block.
                drop(client.take_where(editing::is_show_message));
            }
            let mut answer = answer_value(&mut client, method, params, &marker.name)?;
            if let Some(follow_up) = case::hierarchy_follow_up(&marker.name) {
                answer = follow_hierarchy(&mut client, follow_up, &answer, &marker.name)?;
            }
            let label = marker.label(file);
            answer = match marker.name.as_str() {
                "completion" => {
                    let words = editing::words(&marker.args)?;
                    let resolve = advertised.completion_resolve;
                    completion_block(&mut client, &answer, &words, start, resolve)
                }
                "apply" => {
                    let words = editing::words(&marker.args)?;
                    let resolve = advertised.completion_resolve;
                    apply_block(&mut client, &answer, &words, &text, start, resolve)
                }
                "signatureHelp" if !is_error(&answer) => editing::signature_help(&answer, encoding),
                "inlayHint" if !is_error(&answer) => {
                    Ok(editing::inlay_hints(&answer, &text, encoding, &display))
                }
                "rename" => rename_block(&mut client, &mut texts, &mut settle, header, &answer),
                _ => Ok(answer),
            }
            .map_err(|e| format!("{label}: {e}"))?;
            if marker.name == "rangeFormatting" {
                check_range_formatting(&answer, &text, marker.line)
                    .map_err(|e| format!("{}: {e}", marker.label(file)))?;
            }
            blocks.push(case::block(&label, &paths.normalize(&answer)));
        }
        let last = lines.position(text.len(), encoding);
        for name in &header.requests {
            let (method, params) = case::document_params(name, uri, last)?;
            let mut answer = answer_value(&mut client, method, params, name)?;
            if name == "inlayHint" && !is_error(&answer) {
                answer = editing::inlay_hints(&answer, &text, encoding, &display);
            }
            if name == "formatting" {
                check_formatting(env, dir, &text, &answer, encoding)
                    .map_err(|e| format!("{file} formatting: {e}"))?;
            }
            blocks.push(case::block(
                &format!("{file} {name}"),
                &paths.normalize(&answer),
            ));
        }
    }
    let ended = client.shutdown()?;
    ended.expect(0)?;
    Ok(golden_text(&ended, &paths, blocks))
}

/// A golden's text: the `diagnostics` block, the last published list of
/// every file with a non-empty one when the session `ended`, then
/// `blocks`.
fn golden_text(ended: &Ended, paths: &Paths, blocks: Vec<String>) -> String {
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
    text
}

/// Whether `step` waits for the constants child, which never runs on
/// Windows until M6 (D-1414).
const fn waits_for_child(step: &Step) -> bool {
    matches!(step, Step::Idle | Step::Started | Step::Quiet)
}

/// Opens each of `files` below `dir` as an editor holds it
/// ([`editor_text`]) and waits for its first publish: their open texts by
/// path.
fn open_as_editor(
    client: &mut Client,
    dir: &Path,
    header: &case::Header,
    files: &[String],
) -> Result<BTreeMap<String, String>, String> {
    let mut texts: BTreeMap<String, String> = BTreeMap::new();
    for file in files {
        let path = dir.join(file);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        texts.insert(file.clone(), editor_text(&text, header));
    }
    if !(header.bom || header.crlf) {
        open_all(client, files)?;
        return Ok(texts);
    }
    // As Zed opens such a file: no BOM and `\n` line ends, while disk
    // keeps the rewritten form, so the child's hash of disk must be
    // normalized to match the open text (D-1412).
    let mut uris = Vec::with_capacity(files.len());
    for file in files {
        let uri = client.uri(Path::new(file))?;
        let text = texts.get(file).map_or("", String::as_str);
        client.open_text(&uri, text)?;
        uris.push(uri);
    }
    for uri in &uris {
        client.diagnostics(uri, Some(1))?;
    }
    Ok(texts)
}

/// `text` as an editor holds a file of a test with `@rewrite`: no leading
/// BOM and `\n` line ends. Any other test's text is unchanged.
fn editor_text(text: &str, header: &case::Header) -> String {
    if header.bom || header.crlf {
        text.strip_prefix('\u{feff}')
            .unwrap_or(text)
            .replace("\r\n", "\n")
    } else {
        text.to_owned()
    }
}

/// The text of `path` in the form the test's `@rewrite` asks for: every
/// line end `\r\n`, a leading BOM, or both.
fn read_rewritten(path: &Path, header: &case::Header) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned();
    if header.crlf {
        text = text.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    if header.bom {
        text.insert(0, '\u{feff}');
    }
    Ok(text)
}

/// Waits for a `cheby/idle` of the session at `root`, a path below the
/// client's root (empty for the package itself, the file's name in
/// single-file mode), sent after the last message read (D-1415). Earlier
/// ones are dropped, with every other notification not yet taken: they
/// tell of a state before the edits and saves the client has sent, which a
/// request does not wait for either (D-1443).
fn wait_idle(client: &mut Client, root: &str) -> Result<(), String> {
    drop(client.take_notifications());
    next_idle(client, root).map(drop)
}

/// Waits for the next `cheby/idle` of the session at `root`, as
/// [`wait_idle`] names it, and checks its payload: `root` and
/// `generation`, the generation of the session's newest snapshot, 0 when
/// the root has had none since the server started, as a package whose
/// manifest gives none (D-1492, D-1499). The generation never goes back,
/// not even when a change of `analysis.target` rebuilds the session
/// (D-1495), so it is at least that of every `cheby/idle` of the session
/// read before it.
/// The notifications read before it are taken and returned, since they
/// tell of a state that ended before it.
fn next_idle(client: &mut Client, root: &str) -> Result<Vec<Value>, String> {
    let paths = client.paths().clone();
    let of_root = |m: &Value| {
        m.get("method").and_then(Value::as_str) == Some(IDLE)
            && client::params(m)
                .get("root")
                .and_then(Value::as_str)
                .and_then(|uri| paths.relative(uri))
                .is_some_and(|relative| relative == root)
    };
    let before = client
        .received()
        .iter()
        .filter(|m| of_root(m))
        .filter_map(|m| client::params(m).get("generation").and_then(Value::as_u64))
        .max();
    let idle = client.notification("`cheby/idle` of the package", of_root)?;
    let params = client::params(&idle);
    let mut keys: Vec<&str> = params
        .as_object()
        .map_or_default(|map| map.keys().map(String::as_str).collect());
    keys.sort_unstable();
    let generation = params.get("generation").and_then(Value::as_u64);
    if keys != ["generation", "root"] || generation.is_none() {
        return Err(format!(
            "`cheby/idle` carries {params}, not `root` and a `generation` (D-1492)"
        ));
    }
    if let (Some(before), Some(now)) = (before, generation)
        && now < before
    {
        return Err(format!(
            "`cheby/idle` went back from generation {before} to {now} (D-1495)"
        ));
    }
    Ok(client.take_notifications())
}

/// Whether `message` is the `$/progress` begin of a child run.
fn child_began(message: &Value) -> bool {
    let params = client::params(message);
    message.get("method").and_then(Value::as_str) == Some("$/progress")
        && params
            .get("token")
            .and_then(Value::as_str)
            .is_some_and(|token| token.starts_with(CHILD_PROGRESS))
        && params.pointer("/value/kind").and_then(Value::as_str) == Some("begin")
}

/// The session whose `cheby/idle` a golden test waits for, below the
/// package: the package itself, or the file in single-file mode (D-1492),
/// the one key of `texts`.
fn idle_root(header: &case::Header, texts: &BTreeMap<String, String>) -> String {
    if header.loose {
        texts.keys().next().cloned().unwrap_or_default()
    } else {
        String::new()
    }
}

/// The block of a `rename` marker: its `answer` and the
/// `window/showMessage`s the server sent before it, such as a note
/// (D-1391). An edit is applied and checked as D-1676 says, with D-1670's
/// narrowing of the fixed point, then undone, so the next marker reads the
/// text it was written for.
fn rename_block(
    client: &mut Client,
    texts: &mut BTreeMap<String, String>,
    settle: &mut editing::Settle,
    header: &case::Header,
    answer: &Value,
) -> Result<Value, String> {
    let messages = editing::shown_messages(&client.take_where(editing::is_show_message));
    if !answer.is_null() && !is_error(answer) {
        let rules = editing::EditRules {
            new_errors: &header.new_errors,
            fixed_point: editing::FixedPoint::Rename,
        };
        let applied = editing::check_applied(client, texts, settle, answer, &[], rules)?;
        editing::revert(client, texts, settle, applied)?;
    }
    Ok(json!({ "answer": answer, "showMessage": messages }))
}

/// A `willRename` step: `workspace/willRenameFiles` for `moves`, paths
/// below the package, and its block, the answer and the
/// `window/showMessage`s the server sent before it (D-1675). An edit is
/// applied and checked with the moves, as D-1676 says. The files move on
/// disk whatever the answer, as a client moves them.
fn will_rename_step(
    client: &mut Client,
    texts: &mut BTreeMap<String, String>,
    settle: &mut editing::Settle,
    header: &case::Header,
    moves: &[(String, String)],
) -> Result<String, String> {
    let mut files = Vec::with_capacity(moves.len());
    for (old, new) in moves {
        files.push(json!({
            "oldUri": client.uri(Path::new(old))?,
            "newUri": client.uri(Path::new(new))?,
        }));
    }
    drop(client.take_where(editing::is_show_message));
    let answer = answer_value(
        client,
        "workspace/willRenameFiles",
        json!({ "files": files }),
        "willRename",
    )?;
    let messages = editing::shown_messages(&client.take_where(editing::is_show_message));
    if answer.is_null() || is_error(&answer) {
        let mut reactions = Vec::new();
        editing::move_files(client, texts, moves, &mut reactions)?;
        settle.after(client, &reactions)?;
    } else {
        let rules = editing::EditRules {
            new_errors: &header.new_errors,
            fixed_point: editing::FixedPoint::Move,
        };
        editing::check_applied(client, texts, settle, &answer, moves, rules)?;
    }
    let mut label = "willRename".to_owned();
    for (old, new) in moves {
        label.push(' ');
        label.push_str(old);
        label.push(' ');
        label.push_str(new);
    }
    let block = json!({ "answer": answer, "showMessage": messages });
    Ok(case::block(&label, &client.normalize(&block)))
}

/// Runs one of a golden test's `@steps` in the package at `dir`, whose
/// open documents' texts `texts` holds, by path below the package, and
/// returns the step's block, if it has one. `settle` follows the inputs
/// the step sends, for the applied-edit check.
fn run_step(
    client: &mut Client,
    dir: &Path,
    header: &case::Header,
    step: &Step,
    texts: &mut BTreeMap<String, String>,
    settle: &mut editing::Settle,
) -> Result<Option<String>, String> {
    let root = idle_root(header, texts);
    let (from, save) = match step {
        Step::Idle => {
            wait_idle(client, &root)?;
            // The `cheby/idle` just read tells of a server that has caught up.
            settle.since = client.received().len().saturating_sub(1);
            return Ok(None);
        }
        Step::Started => {
            client.notification("the `$/progress` begin of a child run", child_began)?;
            return Ok(None);
        }
        Step::Quiet => {
            let mut seen = client.take_notifications();
            seen.extend(next_idle(client, &root)?);
            settle.since = client.received().len().saturating_sub(1);
            return match seen.iter().find(|m| child_began(m)) {
                Some(began) => Err(format!(
                    "`quiet`: a child run began since the last `cheby/idle`: {began}"
                )),
                None => Ok(None),
            };
        }
        Step::Settings(name) => {
            let path = dir.join(format!("{name}.json"));
            let text =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let settings: Value =
                serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            client.set_configuration(settings.clone());
            client.notify(
                "workspace/didChangeConfiguration",
                json!({ "settings": { "cheby": settings } }),
            )?;
            settle.sent(client);
            return Ok(None);
        }
        Step::Disk(from) => {
            write_unseen(dir, header, from)?;
            return Ok(None);
        }
        Step::Close(path) => {
            if texts.remove(path).is_none() {
                return Err(format!("`@steps`: `close {path}`: no such open document"));
            }
            let uri = client.uri(Path::new(path))?;
            drop(client.take_where(|m| client::is_publish_of(m, &uri)));
            client.close(&uri)?;
            settle.after(client, &[(uri, None)])?;
            return Ok(None);
        }
        Step::WillRename(moves) => {
            return will_rename_step(client, texts, settle, header, moves).map(Some);
        }
        Step::Edit(from) => (from, false),
        Step::Save(from) | Step::ZedSave(from) => (from, true),
    };
    let zed = matches!(step, Step::ZedSave(_));
    let base = dir.join(from);
    let found = files_with_extension(&base, "cheby").map_err(|e| format!("{from}: {e}"))?;
    if found.is_empty() {
        return Err(format!("`@steps`: `{from}/` holds no `.cheby` file"));
    }
    let mut sent = Vec::new();
    for path in found {
        let file = cheby_test_support::display_relative(&path, &base);
        if !texts.contains_key(&file) {
            return Err(format!(
                "`@steps`: `{from}/{file}` is not a source file of the test"
            ));
        }
        let disk = read_rewritten(&path, header)?;
        let text = editor_text(&disk, header);
        let uri = client.uri(Path::new(&file))?;
        if save {
            let target = dir.join(&file);
            std::fs::write(&target, &disk).map_err(|e| format!("{}: {e}", target.display()))?;
        }
        let version = client.change(&uri, &text)?;
        if zed {
            // Zed's order (CP2's R3): the watched change of its own write,
            // then `didSave` a few milliseconds later, and an echo of it.
            client.notify(
                "workspace/didChangeWatchedFiles",
                json!({ "changes": [{ "uri": uri, "type": 2 }] }),
            )?;
            std::thread::sleep(Duration::from_millis(5));
            client.save(&uri)?;
            client.save(&uri)?;
        } else if save {
            client.save(&uri)?;
        }
        texts.insert(file, text);
        sent.push((uri, Some(version)));
    }
    settle.after(client, &sent)?;
    Ok(None)
}

/// A `disk DIR` step: each `.cheby` file under the test's `DIR/` is
/// written to the same path below the package at `dir`, in the form the
/// test's `@rewrite` asks for, and the server is told nothing, so a closed
/// file goes stale or a module file appears unseen (D-1669).
fn write_unseen(dir: &Path, header: &case::Header, from: &str) -> Result<(), String> {
    let base = dir.join(from);
    let found = files_with_extension(&base, "cheby").map_err(|e| format!("{from}: {e}"))?;
    if found.is_empty() {
        return Err(format!("`@steps`: `{from}/` holds no `.cheby` file"));
    }
    for path in found {
        let file = cheby_test_support::display_relative(&path, &base);
        let target = dir.join(&file);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let text = read_rewritten(&path, header)?;
        std::fs::write(&target, text).map_err(|e| format!("{}: {e}", target.display()))?;
    }
    Ok(())
}

/// A call hierarchy marker's answer: `follow_up` sent with each item that
/// `prepared`, the answer of its `prepareCallHierarchy`, holds, as one
/// list per item, or `prepared` itself when it holds none or failed.
fn follow_hierarchy(
    client: &mut Client,
    follow_up: &str,
    prepared: &Value,
    name: &str,
) -> Result<Value, String> {
    let Some(items) = prepared.as_array().filter(|items| !items.is_empty()) else {
        return Ok(prepared.clone());
    };
    let mut answers = Vec::with_capacity(items.len());
    for item in items {
        answers.push(answer_value(
            client,
            follow_up,
            json!({ "item": item }),
            name,
        )?);
    }
    Ok(Value::Array(answers))
}

/// Whether `answer` is a failed request as [`answer_value`] shows it.
fn is_error(answer: &Value) -> bool {
    answer
        .as_object()
        .is_some_and(|map| map.contains_key("error"))
}

/// `item` after `completionItem/resolve` when `resolve` is advertised, or
/// the error it answered as [`answer_value`] shows one.
fn resolved(client: &mut Client, item: &Value, resolve: bool) -> Result<Value, String> {
    if !resolve {
        return Ok(item.clone());
    }
    answer_value(client, "completionItem/resolve", item.clone(), "resolve")
}

/// The block of a `completion` marker at `at` (`tests/lsp/README.md`,
/// "Editing features"): every item as one line when `words` is empty,
/// else `count` and the whole of each item a word names, resolved, with
/// the words that name none under `absent`. Every item's edit must sit on
/// one line and hold `at` (D-1394).
fn completion_block(
    client: &mut Client,
    answer: &Value,
    words: &[String],
    at: Position,
    resolve: bool,
) -> Result<Value, String> {
    if is_error(answer) {
        return Ok(answer.clone());
    }
    let list = editing::completions(answer)?;
    for item in &list.items {
        editing::check_edit(item, at)?;
    }
    if words.is_empty() {
        let lines: Vec<String> = list.items.iter().map(editing::summary).collect();
        return Ok(json!({ "isIncomplete": list.incomplete, "items": lines }));
    }
    let mut named = Vec::new();
    for item in list
        .items
        .iter()
        .filter(|item| words.iter().any(|w| editing::names(item, w)))
    {
        let item = resolved(client, item, resolve)?;
        if !is_error(&item) {
            editing::check_edit(&item, at)?;
        }
        named.push(editing::printed(&item));
    }
    let absent: Vec<&String> = words
        .iter()
        .filter(|w| !list.items.iter().any(|item| editing::names(item, w)))
        .collect();
    let mut block = json!({
        "count": list.items.len(),
        "isIncomplete": list.incomplete,
        "items": named,
    });
    if !absent.is_empty()
        && let Some(map) = block.as_object_mut()
    {
        map.insert("absent".to_owned(), json!(absent));
    }
    Ok(block)
}

/// The block of an `apply` marker at `at`: the one item its word names,
/// resolved, and `text` after accepting it, which must parse with no
/// syntax error (plan W8). A word that names no item or several, an error
/// answer and a text that does not parse fail the trial.
fn apply_block(
    client: &mut Client,
    answer: &Value,
    words: &[String],
    text: &str,
    at: Position,
    resolve: bool,
) -> Result<Value, String> {
    let [word] = words else {
        return Err(format!("`apply` takes one label, not {words:?}"));
    };
    if is_error(answer) {
        return Err(format!("completion answered {answer}"));
    }
    let list = editing::completions(answer)?;
    for item in &list.items {
        editing::check_edit(item, at)?;
    }
    let matching: Vec<&Value> = list
        .items
        .iter()
        .filter(|item| editing::names(item, word))
        .collect();
    let item = match matching.as_slice() {
        [item] => *item,
        [] => {
            let offered: Vec<String> = list.items.iter().map(editing::summary).collect();
            return Err(format!(
                "no item is named `{word}`; the items are:\n{}",
                offered.join("\n")
            ));
        }
        several => {
            let offered: Vec<String> = several.iter().map(|i| editing::summary(i)).collect();
            return Err(format!(
                "{} items are named `{word}`; name one as `label@description`:\n{}",
                several.len(),
                offered.join("\n")
            ));
        }
    };
    let item = resolved(client, item, resolve)?;
    if is_error(&item) {
        return Err(format!("completionItem/resolve answered {item}"));
    }
    editing::check_edit(&item, at)?;
    let applied = editing::apply_item(text, &item, at, client.encoding())?;
    let errors = editing::syntax_errors(&applied);
    if !errors.is_empty() {
        return Err(format!(
            "accepting `{word}` gives a text with syntax errors:\n{}\nThe text:\n{applied}",
            errors.join("\n")
        ));
    }
    Ok(json!({ "item": editing::printed(&item), "text": editing::text_lines(&applied) }))
}

/// `cheby fmt -`'s output for `text`, `None` when it refuses the text.
fn run_fmt(env: &Env, dir: &Path, text: &str) -> Result<Option<String>, String> {
    let mut cmd = Command::new(CHEBY);
    cmd.args(["--color=never", "fmt", "-"])
        .current_dir(dir)
        .env("NO_COLOR", "1");
    env.cache.apply(&mut cmd);
    let out = process::run(cmd, Some(text.as_bytes()), env.timeout)?;
    Ok((out.status == Some(0)).then_some(out.stdout))
}

/// `features.md` §5.4: a `formatting` answer applied to `text` gives
/// `cheby fmt`'s output byte for byte, `[]` when that is `text` itself,
/// and `null` when `cheby fmt` refuses the text.
fn check_formatting(
    env: &Env,
    dir: &Path,
    text: &str,
    answer: &Value,
    encoding: Encoding,
) -> Result<(), String> {
    let formatted = run_fmt(env, dir, text)?;
    match (answer, formatted) {
        (Value::Null, None) => Ok(()),
        (Value::Null, Some(_)) => Err("`null`, but `cheby fmt` formats the text".to_owned()),
        (_, None) => Err(format!(
            "`cheby fmt` refuses the text, so the answer must be `null`, not {answer}"
        )),
        (Value::Array(edits), Some(formatted)) => {
            let applied = text::apply_text_edits(text, edits, encoding, Bom::Counted)?;
            if applied != formatted {
                return Err(format!(
                    "applying the edits gives\n{applied}\nbut `cheby fmt` gives\n{formatted}"
                ));
            }
            if formatted == text && !edits.is_empty() {
                return Err(format!(
                    "edits for a text `cheby fmt` leaves alone: {answer}"
                ));
            }
            Ok(())
        }
        (_, Some(_)) => Err(format!("neither a list of edits nor `null`: {answer}")),
    }
}

/// `features.md` §5.4, D-1399: every edit of a `rangeFormatting` answer
/// touches the lines of the range, here the marker's one `line` of `text`.
/// An edit that only removes and inserts `import` lines is exempt: the
/// formatter sorts imports, and the other half of a moved import is kept
/// with the half the range touches.
fn check_range_formatting(answer: &Value, text: &str, line: usize) -> Result<(), String> {
    let Some(edits) = answer.as_array() else {
        return Ok(());
    };
    let line = u32::try_from(line).unwrap_or(u32::MAX);
    let lines: Vec<&str> = text.lines().collect();
    for edit in edits {
        let range = edit.get("range").unwrap_or(&Value::Null);
        let start = Position::from_json(range.get("start").unwrap_or(&Value::Null))?;
        let end = Position::from_json(range.get("end").unwrap_or(&Value::Null))?;
        // A whole-line hunk ends at column 0 of the line after it.
        let last = if end.character == 0 && end.line > start.line {
            end.line.saturating_sub(1)
        } else {
            end.line
        };
        let removed = (start.line..end.line)
            .filter_map(|n| lines.get(usize::try_from(n).ok()?).copied())
            .collect::<Vec<_>>();
        let inserted = edit.get("newText").and_then(Value::as_str).unwrap_or("");
        let imports_only = removed
            .iter()
            .copied()
            .chain(inserted.lines())
            .all(|l| l.trim_start().starts_with("import "));
        if imports_only {
            continue;
        }
        if line < start.line || last < line {
            return Err(format!(
                "an edit of lines {}..={last} does not touch line {line}: {edit}",
                start.line
            ));
        }
    }
    Ok(())
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
