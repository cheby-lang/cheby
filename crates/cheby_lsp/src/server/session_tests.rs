//! In-process tests of routing, sessions, watchers, sweeps and the publish
//! rules that come with them (A §6.3 to §6.5, §8, D-1373, D-1374, D-1375,
//! D-1442), through `run_on` with the client of `tests`.
//!
//! A publish that changes nothing is skipped (D-1442), so a test that must
//! know a job finished waits for a publish the job changes, such as the
//! next version's after an edit, since a package's jobs run in order.
//! `barrier` sends `cheby/testSettings`, which the main loop answers after
//! every notification of the messages before it, and which waits for no
//! job.
//!
//! The tests of a std checkout (D-1699 to D-1707) run over a copy of this
//! repository's `std/` beside an empty `crates/cheby_driver/src/std_lib.rs`
//! ([`Checkout`]). They are front end only, so they run on Windows too.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::sessions::STD_DIFFERS;
use super::tests::{
    B, BAD, Client, MAIN, MAIN_BAD, MANIFEST, Temp, WAIT, change, has_error, is_empty, open, uri,
};
use crate::features::ide::file_ops::STD_MOVE;

/// A root module that uses the package's module `b`.
const MAIN_USES_B: &str = "//! Root.\n\nimport app::b\n\nfn main() {\n  let _ = b::g()\n}\n";

/// The first publish of `path` that `accept` takes among those still to
/// come.
pub(super) fn later(client: &mut Client, path: &Path, accept: impl Fn(&Value) -> bool) -> Value {
    let seen = client.publishes(path).len();
    after(client, path, seen, accept)
}

/// The first publish of `path` that `accept` takes after the first `seen`.
pub(super) fn after(
    client: &mut Client,
    path: &Path,
    seen: usize,
    accept: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now().checked_add(WAIT).unwrap();
    loop {
        let found = client.publishes(path).into_iter().skip(seen).find(&accept);
        if let Some(found) = found {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "no publish of {}",
            path.display()
        );
        client.next();
    }
}

/// A client that registers file watchers dynamically, with relative
/// patterns.
pub(super) fn watching() -> Value {
    json!({
        "capabilities": {
            "workspace": {
                "didChangeWatchedFiles": {
                    "dynamicRegistration": true,
                    "relativePatternSupport": true,
                },
            },
        },
    })
}

/// `initialize` parameters for no workspace folder at all, as Zed sends
/// for a file outside its worktree, rooted at `root`.
pub(super) fn rooted_at(root: &Path, extra: &Value) -> Value {
    let mut params = json!({
        "capabilities": {},
        "rootUri": uri(root),
        "workspaceFolders": [],
        "initializationOptions": { "analysis": { "debounceMs": 0 } },
    });
    if let (Some(params), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            params.insert(key.clone(), value.clone());
        }
    }
    params
}

pub(super) fn close(client: &Client, path: &Path) {
    client.notify(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": uri(path) } }),
    );
}

pub(super) fn save(client: &Client, path: &Path) {
    client.notify(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": uri(path) } }),
    );
}

/// `workspace/didChangeWatchedFiles` for one path, with the change type:
/// 1 created, 2 changed, 3 deleted.
pub(super) fn watched(client: &Client, path: &Path, kind: u8) {
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": uri(path), "type": kind }] }),
    );
}

/// The watchers of a `client/registerCapability` for files, if it is one.
fn file_watchers(params: &Value) -> Option<&Value> {
    let registration = params["registrations"].as_array()?.first()?;
    (registration["method"] == "workspace/didChangeWatchedFiles")
        .then(|| &registration["registerOptions"]["watchers"])
}

/// Waits until the main loop has handled everything sent before.
pub(super) fn barrier(client: &mut Client) {
    client.settings();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn no_watcher_client_sees_disk_change_after_sweep() {
    let package = Temp::package("sweep", &[("src/main.cheby", MAIN), ("src/b.cheby", B)]);
    let main = package.at("src/main.cheby");
    let b = package.at("src/b.cheby");
    let (mut client, _) = Client::start(package.params(&json!({})), Vec::new());
    open(&client, &main, MAIN, 1);
    client.wait_publish(&main, |p| p["version"] == 1);
    assert!(!client.publishes(&b).iter().any(has_error));
    // Nothing tells the server: no watcher, no save. The 2 s sweep finds
    // the longer text (D-1374).
    std::fs::write(&b, BAD).unwrap();
    let swept = client.wait_publish(&b, has_error);
    assert!(swept.get("version").is_none_or(Value::is_null), "{swept}");
    assert!(
        !client
            .requests
            .iter()
            .any(|r| file_watchers(&r.params).is_some()),
        "a client without the capability got a watcher"
    );
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn external_edit_of_closed_file_republishes() {
    let package = Temp::package("external", &[("src/main.cheby", MAIN), ("src/b.cheby", B)]);
    let b = package.at("src/b.cheby");
    let (mut client, _) = Client::start(package.params(&watching()), Vec::new());
    let registration =
        client.wait_request_that("client/registerCapability", |p| file_watchers(p).is_some());
    assert_eq!(
        file_watchers(&registration.params),
        Some(&json!([
            { "globPattern": "**/*.cheby", "kind": 7 },
            { "globPattern": "**/cheby.toml", "kind": 7 },
        ]))
    );
    let first = client.wait_publish(&b, |_| true);
    assert!(is_empty(&first), "{first}");
    // The watcher reports the change; no sweep is due for 2 s, and the
    // package has no open document to sweep for anyway.
    std::fs::write(&b, BAD).unwrap();
    watched(&client, &b, 2);
    let edited = client.wait_publish(&b, has_error);
    assert!(edited.get("version").is_none_or(Value::is_null), "{edited}");
    // Deleted, and reported: one empty publish (D-1373).
    std::fs::remove_file(&b).unwrap();
    watched(&client, &b, 3);
    later(&mut client, &b, is_empty);
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn manifest_edit_on_disk_reruns() {
    let package = Temp::package("manifest_disk", &[("src/main.cheby", MAIN)]);
    let manifest = package.at("cheby.toml");
    let (mut client, _) = Client::start(package.params(&watching()), Vec::new());
    let first = client.wait_publish(&manifest, |_| true);
    assert!(is_empty(&first), "{first}");
    std::fs::write(&manifest, "name = \"app\"\nversion = 1\n").unwrap();
    watched(&client, &manifest, 2);
    let broken = client.wait_publish(&manifest, has_error);
    assert!(broken.get("version").is_none_or(Value::is_null), "{broken}");
    std::fs::write(&manifest, MANIFEST).unwrap();
    watched(&client, &manifest, 2);
    later(&mut client, &manifest, is_empty);
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn package_outside_folders_gets_relative_watchers() {
    let folder = Temp::package("rel_folder", &[("src/main.cheby", MAIN)]);
    let other = Temp::package("rel_other", &[("src/main.cheby", MAIN), ("src/b.cheby", B)]);
    let (mut client, _) = Client::start(folder.params(&watching()), Vec::new());
    client.wait_request_that("client/registerCapability", |p| file_watchers(p).is_some());
    let main = other.at("src/main.cheby");
    open(&client, &main, MAIN, 1);
    let own = client.wait_request_that("client/registerCapability", |p| {
        file_watchers(p).is_some_and(|w| w[0]["globPattern"].is_object())
    });
    let base = uri(&other.0);
    assert_eq!(
        file_watchers(&own.params),
        Some(&json!([
            { "globPattern": { "baseUri": base, "pattern": "**/*.cheby" }, "kind": 7 },
            { "globPattern": { "baseUri": base, "pattern": "**/cheby.toml" }, "kind": 7 },
        ]))
    );
    client.wait_publish(&main, |p| p["version"] == 1);
    // The watcher it registered serves its files.
    let b = other.at("src/b.cheby");
    std::fs::write(&b, BAD).unwrap();
    watched(&client, &b, 2);
    client.wait_publish(&b, has_error);
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn multi_root_serves_each_folder() {
    let a = Temp::package(
        "multi_a",
        &[("src/main.cheby", MAIN), ("src/bad.cheby", BAD)],
    );
    let b = Temp::package(
        "multi_b",
        &[("src/main.cheby", MAIN), ("src/bad.cheby", BAD)],
    );
    let params = json!({
        "capabilities": {},
        "workspaceFolders": [
            { "uri": uri(&a.0), "name": "a" },
            { "uri": uri(&b.0), "name": "b" },
        ],
        "initializationOptions": { "analysis": { "debounceMs": 0 } },
    });
    let (mut client, _) = Client::start(params, Vec::new());
    // Both folders are package roots: each session publishes its closed
    // files before anything opens (D-1375).
    for package in [&a, &b] {
        let bad = client.wait_publish(&package.at("src/bad.cheby"), has_error);
        assert!(bad.get("version").is_none_or(Value::is_null), "{bad}");
    }
    // Each document goes to its own package's session.
    for package in [&a, &b] {
        let main = package.at("src/main.cheby");
        open(&client, &main, MAIN_BAD, 1);
        assert!(has_error(
            &client.wait_publish(&main, |p| p["version"] == 1)
        ));
    }
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn nested_package_publishes_once() {
    // As a module of the outer package, `inner/src/main.cheby` imports a
    // module the outer package does not have.
    let inner_main =
        "//! Inner root.\n\nimport inner::util\n\nfn main() {\n  let _ = util::two()\n}\n";
    let util = "//! Util.\n\npub fn two() -> Int {\n  2\n}\n";
    let outer = Temp::package(
        "nested",
        &[
            ("src/main.cheby", MAIN),
            (
                "src/inner/cheby.toml",
                "name = \"inner\"\nversion = \"0.1.0\"\n",
            ),
            ("src/inner/src/main.cheby", inner_main),
            ("src/inner/src/util.cheby", util),
        ],
    );
    let main = outer.at("src/inner/src/main.cheby");
    let (mut client, _) = Client::start(outer.params(&json!({})), Vec::new());
    client.wait_publish(&outer.at("src/main.cheby"), |_| true);
    barrier(&mut client);
    // The outer session does not publish the inner package's files.
    assert_eq!(client.publishes(&main), Vec::<Value>::new());
    open(&client, &main, inner_main, 1);
    let published = client.wait_publish(&main, |p| p["version"] == 1);
    assert!(is_empty(&published), "{published}");
    // A later run of the outer package publishes nothing for it.
    let outer_main = outer.at("src/main.cheby");
    open(&client, &outer_main, MAIN_BAD, 1);
    client.wait_publish(&outer_main, |p| p["version"] == 1);
    barrier(&mut client);
    assert_eq!(client.publishes(&main), [published]);
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn file_routes_by_its_own_path() {
    // Zed's server for a file outside its worktree: rooted at the file's
    // directory, which is not the package root, with no folders (CP1).
    let package = Temp::package(
        "own_path",
        &[("src/main.cheby", MAIN), ("src/bad.cheby", BAD)],
    );
    let main = package.at("src/main.cheby");
    let (mut client, _) = Client::start(rooted_at(&package.at("src"), &json!({})), Vec::new());
    open(&client, &main, MAIN, 0);
    let published = client.wait_publish(&main, |p| p["version"] == 0);
    assert!(is_empty(&published), "{published}");
    // The session is the package's, so its closed files are published.
    client.wait_publish(&package.at("src/bad.cheby"), has_error);
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn versions_restart_at_zero_after_close_and_open() {
    let package = Temp::package("restart", &[("src/main.cheby", MAIN)]);
    let main = package.at("src/main.cheby");
    let (mut client, _) = Client::start(package.params(&json!({})), Vec::new());
    open(&client, &main, MAIN, 0);
    client.wait_publish(&main, |p| p["version"] == 0);
    change(&client, &main, MAIN_BAD, 5);
    assert!(has_error(
        &client.wait_publish(&main, |p| p["version"] == 5)
    ));
    // Closed: the text on disk is clean, and the publish has no version.
    close(&client, &main);
    later(&mut client, &main, |p| {
        p["version"].is_null() && is_empty(p)
    });
    // Opened again at version 0 with the broken text, as Zed does.
    open(&client, &main, MAIN_BAD, 0);
    let reopened = later(&mut client, &main, |p| p["version"] == 0);
    assert!(has_error(&reopened), "{reopened}");
    change(&client, &main, MAIN, 1);
    let fixed = later(&mut client, &main, |p| p["version"] == 1);
    assert!(is_empty(&fixed), "{fixed}");
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn rename_as_close_and_open() {
    let package = Temp::package("rename", &[("src/main.cheby", MAIN), ("src/b.cheby", BAD)]);
    let b = package.at("src/b.cheby");
    let c = package.at("src/c.cheby");
    let (mut client, _) = Client::start(package.params(&json!({})), Vec::new());
    open(&client, &b, BAD, 3);
    assert!(has_error(&client.wait_publish(&b, |p| p["version"] == 3)));
    // Zed renames an open file as `didClose` of the old URI and `didOpen`
    // of the new one at version 0, with no watcher event (CP1).
    std::fs::rename(&b, &c).unwrap();
    close(&client, &b);
    open(&client, &c, BAD, 0);
    assert!(has_error(&client.wait_publish(&c, |p| p["version"] == 0)));
    // The old path left the package: one empty publish.
    client.wait_publish(&b, is_empty);
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn save_with_no_change_sends_no_publish() {
    let package = Temp::package("save", &[("src/main.cheby", MAIN), ("src/b.cheby", B)]);
    let main = package.at("src/main.cheby");
    let b = package.at("src/b.cheby");
    let (mut client, _) = Client::start(package.params(&json!({})), Vec::new());
    open(&client, &main, MAIN, 1);
    client.wait_publish(&main, |p| p["version"] == 1);
    client.wait_publish(&b, |_| true);
    // The save's sweep finds `b` changed, so the save's job publishes it;
    // `main` is unchanged in version and list, so it gets nothing
    // (D-1442).
    std::fs::write(&b, BAD).unwrap();
    save(&client, &main);
    client.wait_publish(&b, has_error);
    barrier(&mut client);
    assert_eq!(
        client.publishes(&main).len(),
        1,
        "{:?}",
        client.publishes(&main)
    );
    // A settings push whose job changes no list publishes nothing either.
    // A new target runs every package again, and the edit after it runs
    // next, so its publish comes after that job's.
    let b_seen = client.publishes(&b).len();
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "cheby": { "analysis": { "debounceMs": 0, "target": "native" } } } }),
    );
    change(&client, &main, MAIN, 2);
    client.wait_publish(&main, |p| p["version"] == 2);
    assert_eq!(
        client.publishes(&b).len(),
        b_seen,
        "{:?}",
        client.publishes(&b)
    );
    // Closing forgets the last publish, so the closed file gets one.
    close(&client, &main);
    later(&mut client, &main, |p| p["version"].is_null());
    client.stop();
}

#[test]
fn push_with_the_cheby_key_is_not_pulled() {
    let (mut client, _) = Client::start(
        json!({ "capabilities": { "workspace": { "configuration": true } } }),
        vec![json!({ "analysis": { "debounceMs": 9 } })],
    );
    client.wait_request("workspace/configuration");
    assert_eq!(client.settings()["analysis"]["debounceMs"], 9);
    // Zed pushes `settings.cheby` to every server (CP1): it is the new
    // layer, and nothing is asked (D-1376).
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "cheby": { "analysis": { "debounceMs": 13 } } } }),
    );
    assert_eq!(client.settings()["analysis"]["debounceMs"], 13);
    let pulls = |client: &Client| {
        client
            .requests
            .iter()
            .filter(|r| r.method == "workspace/configuration")
            .count()
    };
    assert_eq!(pulls(&client), 1);
    // A push without the key asks.
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": {} }),
    );
    client.wait_request("workspace/configuration");
    assert_eq!(pulls(&client), 2);
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn session_ends_after_its_last_document_closes() {
    let package = Temp::package(
        "linger",
        &[("src/main.cheby", MAIN), ("src/bad.cheby", BAD)],
    );
    let main = package.at("src/main.cheby");
    let bad = package.at("src/bad.cheby");
    // No workspace folder holds the package, so its session lives only
    // while it has an open document, and a while after (D-1375).
    let (mut client, _) = Client::start(rooted_at(&package.at("src"), &json!({})), Vec::new());
    open(&client, &main, MAIN, 1);
    client.wait_publish(&main, |p| p["version"] == 1);
    client.wait_publish(&bad, has_error);
    close(&client, &main);
    // Its end clears what it published, since nothing refreshes it now.
    let cleared = client.wait_publish(&bad, is_empty);
    assert!(
        cleared.get("version").is_none_or(Value::is_null),
        "{cleared}"
    );
    // Opening a document again starts a new session.
    let seen = client.publishes(&bad).len();
    open(&client, &main, MAIN, 0);
    later(&mut client, &main, |p| p["version"] == 0);
    after(&mut client, &bad, seen, has_error);
    client.stop();
}

#[test]
fn std_document_is_never_published() {
    // The temporary fallback of the std directory for the tests' build.
    let dir = std::env::temp_dir().join(format!(
        "cheby-std-{}",
        cheby_driver::cache::BuildId([0; 16]).to_hex()
    ));
    let io = crate::text::uri::normalize(&std::path::absolute(dir.join("std/io.cheby")).unwrap());
    let (mut client, _) = Client::start(json!({ "capabilities": {} }), Vec::new());
    open(
        &client,
        &io,
        "//! Std.\n\npub fn f() -> Int {\n  \"x\"\n}\n",
        0,
    );
    change(&client, &io, "//! Std.\n", 1);
    barrier(&mut client);
    assert_eq!(client.publishes(&io), Vec::<Value>::new());
    // It routed to std: an edit there is noted once (A §6.3).
    change(&client, &io, "//! Std 2.\n", 2);
    barrier(&mut client);
    let noted = client
        .logs()
        .iter()
        .filter(|l| l.contains("is a std source"))
        .count();
    assert_eq!(noted, 1, "{:?}", client.logs());
    client.stop();
}

/// A std source is read-only, so formatting it answers `null` without
/// running the formatter, while the same text elsewhere gets its edits
/// (A §6.3, X §5.4).
#[test]
fn std_document_formats_to_null() {
    let dir = std::env::temp_dir().join(format!(
        "cheby-std-{}",
        cheby_driver::cache::BuildId([0; 16]).to_hex()
    ));
    let io = crate::text::uri::normalize(&std::path::absolute(dir.join("std/io.cheby")).unwrap());
    let loose = crate::text::uri::normalize(
        &std::path::absolute(std::env::temp_dir().join("cheby-loose/messy.cheby")).unwrap(),
    );
    let messy = "pub fn  f( ) -> Int { 1 }\n";
    let (mut client, _) = Client::start(json!({ "capabilities": {} }), Vec::new());
    for path in [&io, &loose] {
        open(&client, path, messy, 0);
    }
    for method in ["textDocument/formatting", "textDocument/rangeFormatting"] {
        let params = |path: &Path| {
            json!({
                "textDocument": { "uri": uri(path) },
                "options": { "tabSize": 2, "insertSpaces": true },
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 3 } },
            })
        };
        assert_eq!(
            client.request(method, params(&io)),
            Ok(Value::Null),
            "{method}"
        );
        let edits = client.request(method, params(&loose)).unwrap();
        assert!(
            edits.as_array().is_some_and(|e| !e.is_empty()),
            "{method}: {edits}"
        );
    }
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn new_manifest_routes_an_open_document_again() {
    let package = Temp::package(
        "reroute",
        &[("src/main.cheby", MAIN_USES_B), ("src/b.cheby", B)],
    );
    let manifest = package.at("cheby.toml");
    std::fs::remove_file(&manifest).unwrap();
    let main = package.at("src/main.cheby");
    let (mut client, _) = Client::start(package.params(&watching()), Vec::new());
    // No package yet: single-file mode, where `import app::b` names no
    // package the file has (D-1371).
    open(&client, &main, MAIN_USES_B, 0);
    let alone = client.wait_publish(&main, |p| p["version"] == 0);
    assert!(has_error(&alone), "{alone}");
    // A `cheby.toml` appears above it: the document joins its package
    // (A §8.2).
    std::fs::write(&manifest, MANIFEST).unwrap();
    watched(&client, &manifest, 1);
    let joined = later(&mut client, &main, |p| p["version"] == 0);
    assert!(is_empty(&joined), "{joined}");
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn new_manifest_joins_at_save_without_a_watcher() {
    let package = Temp::package(
        "rejoin",
        &[("src/main.cheby", MAIN_USES_B), ("src/b.cheby", B)],
    );
    let manifest = package.at("cheby.toml");
    std::fs::remove_file(&manifest).unwrap();
    let main = package.at("src/main.cheby");
    let (mut client, _) = Client::start(package.params(&json!({})), Vec::new());
    open(&client, &main, MAIN_USES_B, 0);
    let alone = client.wait_publish(&main, |p| p["version"] == 0);
    assert!(has_error(&alone), "{alone}");
    // No watcher reports the new `cheby.toml`; the save routes again
    // (A §8.3).
    std::fs::write(&manifest, MANIFEST).unwrap();
    save(&client, &main);
    let joined = later(&mut client, &main, |p| p["version"] == 0);
    assert!(is_empty(&joined), "{joined}");
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn deleted_manifest_ends_the_session() {
    let package = Temp::package(
        "unroot",
        &[("src/main.cheby", MAIN), ("src/bad.cheby", BAD)],
    );
    let manifest = package.at("cheby.toml");
    let main = package.at("src/main.cheby");
    let bad = package.at("src/bad.cheby");
    let (mut client, _) = Client::start(package.params(&watching()), Vec::new());
    open(&client, &main, MAIN_BAD, 1);
    assert!(has_error(
        &client.wait_publish(&main, |p| p["version"] == 1)
    ));
    client.wait_publish(&bad, has_error);
    // No package is left, so its closed file's list is cleared at once
    // (A §2.6), and the open document gets single-file mode's list, which
    // has its own errors (D-1371).
    std::fs::remove_file(&manifest).unwrap();
    watched(&client, &manifest, 3);
    later(&mut client, &bad, is_empty);
    later(&mut client, &main, |p| p["version"] == 1 && has_error(p));
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn recreated_manifest_restarts_the_folder_session() {
    let package = Temp::package(
        "refold",
        &[("src/main.cheby", MAIN), ("src/bad.cheby", BAD)],
    );
    let manifest = package.at("cheby.toml");
    let bad = package.at("src/bad.cheby");
    let (mut client, _) = Client::start(package.params(&watching()), Vec::new());
    // The folder's session publishes its closed files (D-1375).
    client.wait_publish(&bad, has_error);
    std::fs::remove_file(&manifest).unwrap();
    watched(&client, &manifest, 3);
    later(&mut client, &bad, is_empty);
    // Back, as after a `git checkout` across it: the folder is a package
    // root again, and its closed files are published again.
    std::fs::write(&manifest, MANIFEST).unwrap();
    watched(&client, &manifest, 1);
    later(&mut client, &bad, has_error);
    client.stop();
}

/// A checkout of this repository in a fresh temporary directory: a copy of
/// its `std/`, an empty `crates/cheby_driver/src/std_lib.rs` and no
/// `cheby.toml`, removed on drop (D-1699).
struct Checkout(PathBuf);

impl Drop for Checkout {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Checkout {
    /// A checkout named `name` in the temporary directory.
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("cheby_lsp_co_{}_{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        Self::fill(&dir);
        Self(crate::text::uri::normalize(
            &std::path::absolute(dir).unwrap(),
        ))
    }

    /// Writes a checkout at `dir`: `std/` copied from this repository, and
    /// the marker file.
    fn fill(dir: &Path) {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
        copy_tree(&repository.join("std"), &dir.join("std"));
        let marker = ["crates", "cheby_driver", "src", "std_lib.rs"]
            .iter()
            .fold(dir.to_path_buf(), |path, part| path.join(part));
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, "").unwrap();
    }

    /// The path of `file`, `/`-separated, under the checkout, built from
    /// components.
    fn at(&self, file: &str) -> PathBuf {
        file.split('/')
            .fold(self.0.clone(), |path, part| path.join(part))
    }

    /// Writes `text` at `file` under the checkout.
    fn write(&self, file: &str, text: &str) -> PathBuf {
        let path = self.at(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    /// `initialize` parameters with the checkout as the one workspace
    /// folder.
    fn folder(&self, extra: &Value) -> Value {
        let mut params = json!({
            "capabilities": {},
            "rootUri": uri(&self.0),
            "workspaceFolders": [{ "uri": uri(&self.0), "name": "cheby" }],
            "initializationOptions": { "analysis": { "debounceMs": 0 } },
        });
        if let (Some(params), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                params.insert(key.clone(), value.clone());
            }
        }
        params
    }
}

/// Copies the files under `from` to `to`.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_tree(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

/// Opens `path` with its text on disk.
fn open_from_disk(client: &Client, path: &Path, version: i32) {
    open(
        client,
        path,
        &std::fs::read_to_string(path).unwrap(),
        version,
    );
}

/// The root of every session, sorted.
fn sessions(client: &mut Client) -> Vec<String> {
    let roots = client.request("cheby/testSessions", Value::Null).unwrap();
    serde_json::from_value(roots).unwrap()
}

/// A session's root as `cheby/testSessions` names it.
fn root_name(path: &Path) -> String {
    path.display().to_string()
}

/// The `cheby/status` entry of the session at `root`.
fn status_of(client: &mut Client, root: &Path) -> Value {
    let status = client.request("cheby/status", Value::Null).unwrap();
    status["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["root"] == uri(root))
        .cloned()
        .unwrap_or_else(|| panic!("no session at {}: {status}", root.display()))
}

/// Waits until `accept` takes the session roots.
fn wait_sessions(client: &mut Client, accept: impl Fn(&[String]) -> bool) {
    let deadline = Instant::now().checked_add(WAIT).unwrap();
    loop {
        let roots = sessions(client);
        if accept(&roots) {
            return;
        }
        assert!(Instant::now() < deadline, "sessions stayed {roots:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits until no session has a run running or due.
fn wait_runs_idle(client: &mut Client) {
    let deadline = Instant::now().checked_add(WAIT).unwrap();
    loop {
        let status = client.request("cheby/status", Value::Null).unwrap();
        let packages = status["packages"].as_array().unwrap();
        if packages.iter().all(|entry| entry["run"] == "idle") {
            return;
        }
        assert!(Instant::now() < deadline, "runs never ended: {status}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Holds the analysis thread, or lets it go.
fn hold(client: &mut Client, on: bool) {
    client
        .request("cheby/testHold", json!({ "hold": on }))
        .unwrap();
}

/// How many times D-1705's warning was shown.
fn differs_shown(client: &Client) -> usize {
    client
        .notifications
        .iter()
        .filter(|n| n.method == "window/showMessage")
        .filter(|n| n.params["type"] == 2 && n.params["message"] == STD_DIFFERS)
        .count()
}

/// The watchers of every `client/registerCapability` with relative
/// patterns.
fn relative_watchers(client: &Client) -> Vec<Value> {
    client
        .requests
        .iter()
        .filter_map(|r| file_watchers(&r.params))
        .filter(|w| w[0]["globPattern"].is_object())
        .cloned()
        .collect()
}

/// A probe module of a checkout whose test body has a type error.
const PROBE_TESTS: &str = "//! Probe.\n\npub fn one() -> Int {\n  1\n}\n\ntest \"one is one\" {\n  let x: Int = \"no\"\n  assert one() == x\n}\n";

/// A probe module of a checkout, and one that calls it.
const PROBE_LIB: &str = "//! Probe.\n\npub fn f() -> Int {\n  1\n}\n";
const PROBE_LIB_CHANGED: &str = "//! Probe.\n\npub fn f(x: Int) -> Int {\n  x\n}\n";
const PROBE_DEP: &str =
    "//! Probe dependent.\n\nimport std::probe_lib\n\npub fn g() -> Int {\n  probe_lib::f()\n}\n";

/// A checkout's session starts at its first open document, never at
/// `initialized` for a workspace folder that is a checkout, publishes every
/// std file, open or closed, and registers no watcher of its own inside a
/// folder (D-1704). A copy equal to the binary's std shows no warning and
/// has no `stdDiffers` (D-1705).
#[test]
fn checkout_session_starts_at_open() {
    let co = Checkout::new("start");
    let (mut client, _) = Client::start(co.folder(&watching()), Vec::new());
    barrier(&mut client);
    assert_eq!(sessions(&mut client), Vec::<String>::new());
    let list = co.at("std/list.cheby");
    open_from_disk(&client, &list, 1);
    assert_eq!(sessions(&mut client), [root_name(&co.0)]);
    let entry = status_of(&mut client, &co.0);
    assert_eq!(entry["kind"], "stdCheckout", "{entry}");
    assert!(entry.get("stdDiffers").is_none(), "{entry}");
    assert_eq!(
        entry["children"]["notEvaluated"], "a std checkout evaluates no constants (D-1491, D-1700)",
        "{entry}"
    );
    let published = client.wait_publish(&list, |p| p["version"] == 1);
    assert!(is_empty(&published), "{published}");
    // A closed module is published too, with no version.
    let closed = client.wait_publish(&co.at("std/json/tree.cheby"), |_| true);
    assert!(is_empty(&closed), "{closed}");
    assert!(closed.get("version").is_none_or(Value::is_null), "{closed}");
    assert_eq!(differs_shown(&client), 0);
    assert_eq!(relative_watchers(&client), Vec::<Value>::new());
    client.stop();
}

/// Outside every workspace folder, a checkout's session registers
/// `std/**/*.cheby` under the checkout, and the manifests whose coming
/// ends it, `std/**/cheby.toml` and the root `cheby.toml`, and nothing
/// else (D-1704, D-1729). A root `cheby.toml` reported through that
/// watcher then routes the open document again at once. No Windows
/// ignore, for the reason `checkout_ends_when_no_longer_a_checkout` gives.
#[test]
fn checkout_outside_folders_watches_std_and_manifests() {
    let co = Checkout::new("watch_own");
    let list = co.at("std/list.cheby");
    let (mut client, _) = Client::start(rooted_at(&co.at("std"), &watching()), Vec::new());
    open_from_disk(&client, &list, 1);
    let own = client.wait_request_that("client/registerCapability", |p| {
        file_watchers(p).is_some_and(|w| w[0]["globPattern"].is_object())
    });
    assert_eq!(
        file_watchers(&own.params),
        Some(&json!([
            { "globPattern": { "baseUri": uri(&co.0), "pattern": "std/**/*.cheby" }, "kind": 7 },
            { "globPattern": { "baseUri": uri(&co.0), "pattern": "std/**/cheby.toml" }, "kind": 7 },
            { "globPattern": { "baseUri": uri(&co.0), "pattern": "cheby.toml" }, "kind": 7 },
        ]))
    );
    assert_eq!(sessions(&mut client), [root_name(&co.0)]);
    let manifest = co.write("cheby.toml", MANIFEST);
    watched(&client, &manifest, 1);
    assert_eq!(sessions(&mut client), [root_name(&list)]);
    client.stop();
}

/// A checkout's session is rooted at the whole repository, but a change
/// outside its `std/` starts no run of it, while one inside does (D-1704).
#[test]
fn checkout_holds_only_its_std() {
    let co = Checkout::new("holds");
    let example = co.write("docs/examples/x/src/main.cheby", MAIN);
    let (mut client, _) = Client::start(co.folder(&watching()), Vec::new());
    open_from_disk(&client, &co.at("std/list.cheby"), 1);
    wait_runs_idle(&mut client);
    hold(&mut client, true);
    std::fs::write(&example, MAIN_BAD).unwrap();
    watched(&client, &example, 2);
    barrier(&mut client);
    let entry = status_of(&mut client, &co.0);
    assert_eq!(entry["run"], "idle", "{entry}");
    let bool_file = co.at("std/bool.cheby");
    watched(&client, &bool_file, 2);
    barrier(&mut client);
    let entry = status_of(&mut client, &co.0);
    assert_eq!(entry["run"], "debouncing", "{entry}");
    hold(&mut client, false);
    client.stop();
}

/// A checkout nested in another, such as a worktree under `.claude/`, has
/// a session of its own, and a change to its `std/` reaches that session
/// only (D-1704).
#[test]
fn nested_checkout_reaches_only_its_session() {
    let outer = Checkout::new("nest_outer");
    let nested_dir = outer.at(".claude/worktrees/agent/x");
    Checkout::fill(&nested_dir);
    let nested = |file: &str| {
        file.split('/')
            .fold(nested_dir.clone(), |path, part| path.join(part))
    };
    let (mut client, _) = Client::start(outer.folder(&watching()), Vec::new());
    open_from_disk(&client, &outer.at("std/list.cheby"), 1);
    open_from_disk(&client, &nested("std/list.cheby"), 1);
    let mut roots = vec![root_name(&outer.0), root_name(&nested_dir)];
    roots.sort();
    assert_eq!(sessions(&mut client), roots);
    wait_runs_idle(&mut client);
    hold(&mut client, true);
    watched(&client, &nested("std/bool.cheby"), 2);
    barrier(&mut client);
    assert_eq!(status_of(&mut client, &nested_dir)["run"], "debouncing");
    assert_eq!(status_of(&mut client, &outer.0)["run"], "idle");
    watched(&client, &outer.at("std/bool.cheby"), 2);
    barrier(&mut client);
    assert_eq!(status_of(&mut client, &outer.0)["run"], "debouncing");
    hold(&mut client, false);
    client.stop();
}

/// With no watcher, the checkout's sweep stats its `std/` and sees a
/// closed module changed on disk (D-1374, D-1704).
#[test]
fn checkout_sweep_sees_a_changed_std_file() {
    let co = Checkout::new("sweep");
    let list = co.at("std/list.cheby");
    let (mut client, _) = Client::start(rooted_at(&co.at("std"), &json!({})), Vec::new());
    open_from_disk(&client, &list, 1);
    client.wait_publish(&list, |p| p["version"] == 1);
    let bool_file = co.at("std/bool.cheby");
    let mut text = std::fs::read_to_string(&bool_file).unwrap();
    text.push_str("\npub fn probe_broken() -> Int {\n  \"x\"\n}\n");
    std::fs::write(&bool_file, text).unwrap();
    let swept = client.wait_publish(&bool_file, has_error);
    assert!(swept.get("version").is_none_or(Value::is_null), "{swept}");
    client.stop();
}

/// With no watcher, the checkout's sweep stats its root `cheby.toml` too,
/// so a manifest created there ends the checkout at the next sweep, and
/// the document leaves for single-file mode (D-1699, D-1729). No Windows
/// ignore, for the reason `checkout_ends_when_no_longer_a_checkout` gives.
#[test]
fn checkout_sweep_sees_a_root_manifest() {
    let co = Checkout::new("sweep_manifest");
    let list = co.at("std/list.cheby");
    let (mut client, _) = Client::start(rooted_at(&co.at("std"), &json!({})), Vec::new());
    open_from_disk(&client, &list, 1);
    assert_eq!(sessions(&mut client), [root_name(&co.0)]);
    co.write("cheby.toml", MANIFEST);
    let alone = [root_name(&list)];
    wait_sessions(&mut client, |roots| roots == alone);
    client.stop();
}

/// An edit of an open module that breaks a closed one publishes the closed
/// one's error (D-1704).
#[test]
fn checkout_publishes_a_closed_module_an_edit_broke() {
    let co = Checkout::new("dependent");
    let lib = co.write("std/probe_lib.cheby", PROBE_LIB);
    let dep = co.write("std/probe_dep.cheby", PROBE_DEP);
    let (mut client, _) = Client::start(co.folder(&watching()), Vec::new());
    open(&client, &lib, PROBE_LIB, 1);
    client.wait_publish(&lib, |p| p["version"] == 1);
    let clean = client.wait_publish(&dep, |_| true);
    assert!(is_empty(&clean), "{clean}");
    change(&client, &lib, PROBE_LIB_CHANGED, 2);
    let broken = later(&mut client, &dep, has_error);
    assert!(broken.get("version").is_none_or(Value::is_null), "{broken}");
    client.stop();
}

/// The test bodies of an open module are checked and those of a closed one
/// are not, so an error in one is published while its module is open and
/// cleared by its close, which runs the session though no text changed
/// (D-1701).
#[test]
fn checkout_test_body_errors_follow_open() {
    let co = Checkout::new("tests");
    let probe = co.write("std/probe_tests.cheby", PROBE_TESTS);
    let list = co.at("std/list.cheby");
    let (mut client, _) = Client::start(co.folder(&watching()), Vec::new());
    open_from_disk(&client, &list, 1);
    client.wait_publish(&list, |p| p["version"] == 1);
    let closed = client.wait_publish(&probe, |_| true);
    assert!(is_empty(&closed), "{closed}");
    open(&client, &probe, PROBE_TESTS, 1);
    let opened = later(&mut client, &probe, |p| p["version"] == 1);
    assert!(has_error(&opened), "{opened}");
    close(&client, &probe);
    let cleared = later(&mut client, &probe, |p| p["version"].is_null());
    assert!(is_empty(&cleared), "{cleared}");
    // The close ran the session, which `list` keeps open.
    assert_eq!(sessions(&mut client), [root_name(&co.0)]);
    client.stop();
}

/// A checkout's session outlives its last document's close, as a
/// package's does, and then ends (D-1375, D-1704).
#[test]
fn checkout_session_lingers_then_ends() {
    let co = Checkout::new("linger");
    let list = co.at("std/list.cheby");
    let (mut client, _) = Client::start(rooted_at(&co.at("std"), &json!({})), Vec::new());
    open_from_disk(&client, &list, 1);
    assert_eq!(sessions(&mut client), [root_name(&co.0)]);
    close(&client, &list);
    // The close runs the session (D-1701), so it cannot end before that
    // run, and its 120 s start after it.
    assert_eq!(sessions(&mut client), [root_name(&co.0)]);
    wait_sessions(&mut client, <[String]>::is_empty);
    client.stop();
}

/// A `cheby.toml` that appears above the checkout's `std/` ends its session
/// and routes its documents again, and so does a marker file that goes
/// (D-1699, D-1707).
///
/// It runs a package session too, yet carries no Windows ignore, unlike
/// the manifest tests above: `Client::start` gives the server no
/// constants child, and the front end that the session runs builds no JIT
/// code, which only the child's work does (`backend_query`, D-641,
/// D-1516).
#[test]
fn checkout_ends_when_no_longer_a_checkout() {
    let co = Checkout::new("unroot");
    let list = co.at("std/list.cheby");
    let (mut client, _) = Client::start(co.folder(&watching()), Vec::new());
    open_from_disk(&client, &list, 1);
    assert_eq!(sessions(&mut client), [root_name(&co.0)]);
    // A package's rules now: `std/list.cheby` is no module of it, and the
    // folder is a package root with its eager session (D-1375).
    let manifest = co.write("cheby.toml", MANIFEST);
    watched(&client, &manifest, 1);
    let mut both = vec![root_name(&co.0), root_name(&list)];
    both.sort();
    assert_eq!(sessions(&mut client), both);
    assert_eq!(status_of(&mut client, &co.0)["kind"], "package");
    std::fs::remove_file(&manifest).unwrap();
    watched(&client, &manifest, 3);
    assert_eq!(sessions(&mut client), [root_name(&co.0)]);
    assert_eq!(status_of(&mut client, &co.0)["kind"], "stdCheckout");
    // At `std/` itself.
    let inner = co.write("std/cheby.toml", MANIFEST);
    watched(&client, &inner, 1);
    assert_eq!(sessions(&mut client), [root_name(&list)]);
    std::fs::remove_file(&inner).unwrap();
    watched(&client, &inner, 3);
    assert_eq!(sessions(&mut client), [root_name(&co.0)]);
    // No watcher reports a marker that goes; the next route sees it.
    std::fs::remove_file(co.at("crates/cheby_driver/src/std_lib.rs")).unwrap();
    save(&client, &list);
    assert_eq!(sessions(&mut client), [root_name(&list)]);
    client.stop();
}

/// D-1705: a checkout whose `std/` on disk is the binary's shows nothing;
/// one with a changed file, an added module or a removed one shows the
/// warning once at its session's start and has `stdDiffers: true`, and a
/// session that ends and starts again shows it again.
#[test]
fn checkout_std_differs_warns_at_each_start() {
    let same = Checkout::new("differs_same");
    let changed = Checkout::new("differs_changed");
    let added = Checkout::new("differs_added");
    let removed = Checkout::new("differs_removed");
    let list_text = std::fs::read_to_string(changed.at("std/list.cheby")).unwrap();
    changed.write("std/list.cheby", &format!("{list_text}\n"));
    added.write("std/probe_extra.cheby", "//! Extra.\n");
    std::fs::remove_file(removed.at("std/bool.cheby")).unwrap();
    let (mut client, _) = Client::start(rooted_at(&same.0, &json!({})), Vec::new());
    open_from_disk(&client, &same.at("std/list.cheby"), 1);
    barrier(&mut client);
    assert_eq!(differs_shown(&client), 0);
    assert!(status_of(&mut client, &same.0).get("stdDiffers").is_none());
    for (shown, co) in [(1, &changed), (2, &added), (3, &removed)] {
        open_from_disk(&client, &co.at("std/list.cheby"), 1);
        barrier(&mut client);
        assert_eq!(differs_shown(&client), shown, "{}", co.0.display());
        let entry = status_of(&mut client, &co.0);
        assert_eq!(entry["stdDiffers"], true, "{entry}");
        assert_eq!(entry["kind"], "stdCheckout", "{entry}");
    }
    // The session of `changed` ends after its close, and its next start
    // warns again.
    close(&client, &changed.at("std/list.cheby"));
    let gone = root_name(&changed.0);
    wait_sessions(&mut client, |roots| !roots.contains(&gone));
    open_from_disk(&client, &changed.at("std/list.cheby"), 2);
    barrier(&mut client);
    assert_eq!(differs_shown(&client), 4);
    client.stop();
}

/// D-1706: a move of a checkout's std file, of a folder under its `std/`
/// or of that `std/` itself answers `null` with the move's warning from
/// routing alone: with no std document open, so with no session (D-1704),
/// and with the session alive for a folder, which routes to no session. A
/// move of a folder outside `std/` is not refused so. A request with
/// files in a package and in the checkout is refused whole (D-1730).
#[test]
fn checkout_moves_are_refused_from_routing() {
    let co = Checkout::new("moves");
    let (mut client, _) = Client::start(co.folder(&json!({})), Vec::new());
    barrier(&mut client);
    let refused = format!("The imports were not updated: {STD_MOVE}");
    let shown = |client: &Client| {
        client
            .notifications
            .iter()
            .filter(|n| n.method == "window/showMessage")
            .filter(|n| n.params["type"] == 2 && n.params["message"] == refused.as_str())
            .count()
    };
    let moves = |client: &mut Client, old: &str, new: &str| {
        let files =
            json!({ "files": [{ "oldUri": uri(&co.at(old)), "newUri": uri(&co.at(new)) }] });
        let answer = client.request("workspace/willRenameFiles", files).unwrap();
        assert_eq!(answer, Value::Null, "{old}");
        barrier(client);
    };
    let pairs = [
        ("std/list.cheby", "std/lists.cheby"),
        ("std/json", "std/js"),
        ("std", "stdlib"),
    ];
    for (count, (old, new)) in (1..).zip(pairs) {
        moves(&mut client, old, new);
        assert_eq!(shown(&client), count, "{old} with no session");
    }
    assert_eq!(sessions(&mut client), Vec::<String>::new());
    moves(&mut client, "crates", "crates_moved");
    assert_eq!(shown(&client), 3, "a folder outside std/");
    open_from_disk(&client, &co.at("std/list.cheby"), 1);
    assert_eq!(sessions(&mut client), [root_name(&co.0)]);
    for (count, (old, new)) in (4..).zip(pairs) {
        moves(&mut client, old, new);
        assert_eq!(shown(&client), count, "{old} with the session");
    }
    // A request whose files lie in a package and in the checkout is refused
    // whole, whichever comes first (D-1730).
    co.write("docs/examples/x/cheby.toml", MANIFEST);
    co.write("docs/examples/x/src/main.cheby", MAIN);
    co.write("docs/examples/x/src/a.cheby", "//! A.\n");
    let package_move = json!({
        "oldUri": uri(&co.at("docs/examples/x/src/a.cheby")),
        "newUri": uri(&co.at("docs/examples/x/src/b.cheby")),
    });
    let std_move = json!({
        "oldUri": uri(&co.at("std/list.cheby")),
        "newUri": uri(&co.at("std/lists.cheby")),
    });
    for (count, files) in (7..).zip([
        json!([package_move, std_move]),
        json!([std_move, package_move]),
    ]) {
        let answer = client
            .request("workspace/willRenameFiles", json!({ "files": files }))
            .unwrap();
        assert_eq!(answer, Value::Null, "{files}");
        barrier(&mut client);
        assert_eq!(shown(&client), count, "{files}");
    }
    client.stop();
}

/// A job of a checkout's session still on the analysis thread when a
/// `cheby.toml` makes the checkout a package reaches no session: the
/// package's session made at the same root counts only its own run, and
/// never takes the checkout's snapshot (D-1699). A checkout's run takes
/// far longer than the main loop takes for the two notifications, so the
/// job is still running at the flip. No Windows ignore, for the reason
/// `checkout_ends_when_no_longer_a_checkout` gives.
#[test]
fn checkout_job_in_flight_reaches_no_new_session() {
    let co = Checkout::new("orphan");
    let list = co.at("std/list.cheby");
    let (mut client, _) = Client::start(co.folder(&watching()), Vec::new());
    open_from_disk(&client, &list, 1);
    client.wait_publish(&list, |p| p["version"] == 1);
    wait_runs_idle(&mut client);
    let before = status_of(&mut client, &co.0)["generation"]
        .as_u64()
        .unwrap();
    let text = std::fs::read_to_string(&list).unwrap();
    change(&client, &list, &format!("{text}\n"), 2);
    let manifest = co.write("cheby.toml", MANIFEST);
    watched(&client, &manifest, 1);
    barrier(&mut client);
    assert_eq!(status_of(&mut client, &co.0)["kind"], "package");
    wait_runs_idle(&mut client);
    let entry = status_of(&mut client, &co.0);
    assert_eq!(entry["kind"], "package", "{entry}");
    assert_eq!(
        entry["generation"].as_u64().unwrap(),
        before + 1,
        "the package's own run only: {entry}"
    );
    client.stop();
}
