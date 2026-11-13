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

use std::path::Path;
use std::time::Instant;

use serde_json::{Value, json};

use super::tests::{
    B, BAD, Client, MAIN, MAIN_BAD, MANIFEST, Temp, WAIT, change, has_error, is_empty, open, uri,
};

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
