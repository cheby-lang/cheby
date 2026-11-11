//! In-process tests of single-file mode (D-1371, A §6.4): a document
//! outside every package, or inside one without being one of its modules,
//! gets a session of its own, keyed by its path, that publishes it as a
//! package publishes an open document (D-1373, D-1442), ends at its close
//! with one empty publish, and gives way to a package whose `cheby.toml`
//! appears above it.
//!
//! The temporary directory has no `cheby.toml` above it, so a file there
//! with no package of its own is in strict single-file mode.

use std::path::Path;

use serde_json::{Value, json};

use super::session_tests::{barrier, close, later, rooted_at, save};
use super::tests::{
    B, BAD, Client, MAIN, MANIFEST, Temp, change, codes, has_error, is_empty, open, uri,
};

/// `helper` is used by nothing: W0005 in a package.
const UNUSED: &str =
    "//! A script.\n\nfn helper() -> Int {\n  2\n}\n\nfn main() {\n  let _ = 1\n}\n";

/// An unclosed body: a grammar error.
const GRAMMAR: &str = "//! A script.\n\nfn main() {\n  let _ = 1\n";

/// A root module that uses the package's module `b`.
const USES_B: &str = "//! Root.\n\nimport app::b\n\nfn main() {\n  let _ = b::g()\n}\n";

/// A directory with these files and no `cheby.toml` anywhere above it.
fn loose(name: &str, files: &[(&str, &str)]) -> Temp {
    let dir = Temp::package(name, files);
    std::fs::remove_file(dir.at("cheby.toml")).unwrap();
    assert_eq!(cheby_driver::find_package_root(&dir.0), None);
    dir
}

/// The roots of the server's sessions.
fn sessions(client: &mut Client) -> Vec<String> {
    let answer = client.request("cheby/testSessions", Value::Null).unwrap();
    serde_json::from_value(answer).unwrap()
}

/// `cheby/testDoc` on `path`: the version of the snapshot that answers it.
fn doc_version(client: &mut Client, path: &Path) -> Value {
    let answer = client
        .request(
            "cheby/testDoc",
            json!({ "textDocument": { "uri": uri(path) } }),
        )
        .unwrap();
    answer["version"].clone()
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn loose_clean_file_gets_an_empty_publish_with_its_version() {
    let dir = loose("single_clean", &[]);
    let file = dir.at("script.cheby");
    let (mut client, _) = Client::start(dir.params(&json!({})), Vec::new());
    open(&client, &file, MAIN, 3);
    let published = client.wait_publish(&file, |p| p["version"] == 3);
    assert!(is_empty(&published), "{published}");
    // Its session is keyed by the file itself (D-1371).
    assert_eq!(sessions(&mut client), [file.display().to_string()]);
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn loose_broken_file_gets_its_errors() {
    let dir = loose("single_broken", &[]);
    let file = dir.at("script.cheby");
    let (mut client, _) = Client::start(dir.params(&json!({})), Vec::new());
    // A type error: the whole run's diagnostics.
    open(&client, &file, BAD, 1);
    let typed = client.wait_publish(&file, |p| p["version"] == 1);
    assert_eq!(codes(&typed), ["E0047"], "{typed}");
    // A grammar error: its phase-A list.
    change(&client, &file, GRAMMAR, 2);
    let broken = client.wait_publish(&file, |p| p["version"] == 2);
    assert!(has_error(&broken), "{broken}");
    assert_ne!(codes(&broken), ["E0047"], "{broken}");
    // Fixed: the list empties.
    change(&client, &file, MAIN, 3);
    let fixed = client.wait_publish(&file, |p| p["version"] == 3);
    assert!(is_empty(&fixed), "{fixed}");
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn loose_file_gets_no_w0005() {
    // The same text as a package's root module gets W0005.
    let package = Temp::package("single_w0005_pkg", &[("src/main.cheby", UNUSED)]);
    let main = package.at("src/main.cheby");
    let dir = loose("single_w0005", &[]);
    let file = dir.at("helper.cheby");
    let (mut client, _) = Client::start(dir.params(&json!({})), Vec::new());
    open(&client, &main, UNUSED, 1);
    let in_package = client.wait_publish(&main, |p| p["version"] == 1);
    assert_eq!(codes(&in_package), ["W0005"], "{in_package}");
    open(&client, &file, UNUSED, 1);
    let alone = client.wait_publish(&file, |p| p["version"] == 1);
    assert!(is_empty(&alone), "{alone}");
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn not_a_module_gets_phase_a_only() {
    let package = Temp::package("single_not_module", &[("src/main.cheby", MAIN)]);
    let notes = package.at("notes.cheby");
    let (mut client, _) = Client::start(package.params(&json!({})), Vec::new());
    // A type error, and an import its package's modules cannot give it:
    // neither is reported (D-1371).
    let typed = "//! Notes.\n\nimport app::b\n\npub fn f() -> Int {\n  \"x\"\n}\n";
    open(&client, &notes, typed, 1);
    let published = client.wait_publish(&notes, |p| p["version"] == 1);
    assert!(is_empty(&published), "{published}");
    // A grammar error is.
    change(&client, &notes, GRAMMAR, 2);
    let broken = client.wait_publish(&notes, |p| p["version"] == 2);
    assert!(has_error(&broken), "{broken}");
    // The package's own session does not publish it.
    let mut roots = sessions(&mut client);
    roots.sort();
    let mut want = vec![package.0.display().to_string(), notes.display().to_string()];
    want.sort();
    assert_eq!(roots, want);
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn closing_a_loose_file_sends_one_empty_publish_and_ends_its_session() {
    let dir = loose("single_close", &[]);
    let file = dir.at("script.cheby");
    let (mut client, _) = Client::start(dir.params(&json!({})), Vec::new());
    open(&client, &file, BAD, 1);
    assert!(has_error(
        &client.wait_publish(&file, |p| p["version"] == 1)
    ));
    let seen = client.publishes(&file).len();
    close(&client, &file);
    let cleared = later(&mut client, &file, |_| true);
    assert!(is_empty(&cleared), "{cleared}");
    assert!(cleared["version"].is_null(), "{cleared}");
    barrier(&mut client);
    assert_eq!(sessions(&mut client), Vec::<String>::new());
    assert_eq!(
        client.publishes(&file).len(),
        seen.checked_add(1).unwrap(),
        "{:?}",
        client.publishes(&file)
    );
    // Opened again: a new session, which publishes again.
    open(&client, &file, BAD, 0);
    let reopened = later(&mut client, &file, |p| p["version"] == 0);
    assert!(has_error(&reopened), "{reopened}");
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn new_manifest_moves_a_loose_file_into_its_package_at_save() {
    let dir = loose(
        "single_join",
        &[("src/main.cheby", USES_B), ("src/b.cheby", B)],
    );
    let main = dir.at("src/main.cheby");
    let (mut client, _) = Client::start(dir.params(&json!({})), Vec::new());
    // Alone, `import app::b` names no package it has.
    open(&client, &main, USES_B, 0);
    let alone = client.wait_publish(&main, |p| p["version"] == 0);
    assert!(has_error(&alone), "{alone}");
    // No watcher reports the new `cheby.toml`; the save routes again
    // (A §8.3), and the package's run publishes it clean.
    std::fs::write(dir.at("cheby.toml"), MANIFEST).unwrap();
    save(&client, &main);
    let joined = later(&mut client, &main, |p| p["version"] == 0);
    assert!(is_empty(&joined), "{joined}");
    assert_eq!(sessions(&mut client), [dir.0.display().to_string()]);
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn zed_loose_window_publishes_and_answers() {
    // Zed opens a file outside its worktree in a server of its own, rooted
    // at the file's directory, with no workspace folders (CP1).
    let dir = loose("single_zed", &[("shapes/src/area.cheby", BAD)]);
    let file = dir.at("shapes/src/area.cheby");
    let (mut client, _) = Client::start(rooted_at(&dir.at("shapes/src"), &json!({})), Vec::new());
    open(&client, &file, BAD, 0);
    let first = client.wait_publish(&file, |p| p["version"] == 0);
    assert_eq!(codes(&first), ["E0047"], "{first}");
    // A snapshot request waits for the snapshot of its text, as in a
    // package (D-1367, D-1445).
    change(&client, &file, MAIN, 1);
    assert_eq!(doc_version(&mut client, &file), json!(1));
    let fixed = client.wait_publish(&file, |p| p["version"] == 1);
    assert!(is_empty(&fixed), "{fixed}");
    client.stop();
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn stray_manifest_is_never_published() {
    // A `cheby.toml` with no package of its own is no Cheby source, so no
    // single-file session parses it.
    let dir = loose("single_stray", &[]);
    let stray = dir.at("cheby.toml");
    let file = dir.at("script.cheby");
    let (mut client, _) = Client::start(dir.params(&json!({})), Vec::new());
    open(&client, &stray, MANIFEST, 0);
    open(&client, &file, MAIN, 0);
    client.wait_publish(&file, |p| p["version"] == 0);
    assert_eq!(sessions(&mut client), [file.display().to_string()]);
    assert_eq!(client.publishes(&stray), Vec::<Value>::new());
    client.stop();
}

/// D-1499: a root's generation counter lasts for the server's life. A
/// single-file session ends when its file closes (D-1446), and the next
/// session of the same file, whose snapshots number from 1 again, gets a
/// `cheby/idle` generation above every earlier one (D-1495).
#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn idle_generation_grows_across_a_session_that_ends() {
    let dir = loose("single_generation", &[]);
    let file = dir.at("script.cheby");
    let params = dir.params(&json!({
        "capabilities": { "experimental": { "chebyIdle": true } },
    }));
    let (mut client, _) = Client::start(params, Vec::new());
    let root = uri(&file);
    let generations = |client: &Client| -> Vec<u64> {
        client
            .notifications
            .iter()
            .filter(|n| n.method == "cheby/idle" && n.params["root"] == root)
            .filter_map(|n| n.params["generation"].as_u64())
            .collect()
    };
    open(&client, &file, MAIN, 1);
    client.wait_publish(&file, |p| p["version"] == 1);
    while generations(&client).is_empty() {
        client.next();
    }
    close(&client, &file);
    later(&mut client, &file, |_| true);
    barrier(&mut client);
    assert_eq!(sessions(&mut client), Vec::<String>::new(), "it ended");
    let before = generations(&client);
    let highest = before.iter().copied().max().unwrap();
    assert!(highest >= 1, "the first session made a snapshot");
    // Opened again: a new session, whose first snapshot is its own 1.
    open(&client, &file, MAIN, 0);
    later(&mut client, &file, |p| p["version"] == 0);
    while generations(&client).len() == before.len() {
        client.next();
    }
    let after = generations(&client)[before.len()];
    assert!(after > highest, "{before:?} then {after}");
    let status = client.request("cheby/status", Value::Null).unwrap();
    let shown = status["packages"][0]["generation"].as_u64().unwrap();
    assert!(shown >= after, "{status}");
    client.stop();
}
