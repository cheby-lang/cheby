//! `cheby new` end to end (D-1032): the built binary in fresh directories
//! under `CARGO_TARGET_TMPDIR`. The skeleton passes `cheby check`,
//! `cheby run` and `cheby fmt --check`; an invalid name and an existing
//! path are tool errors that write nothing (D-464).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::{FreshDir, HarnessCache};
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
const TIMEOUT: Duration = Duration::from_secs(120);

/// The answer's error text for an invalid name (§7 Q6 (1)).
const INVALID: &str = "is not a valid package name: package names are lower-case identifiers, \
                       such as `my_app`";

/// Runs `cheby --color=never <args>` in `dir`, with a fresh `CHEBY_CACHE`
/// of its own, deleted afterwards (D-1128).
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to spawn `cheby` fails the test"
)]
fn cheby(dir: &Path, args: &[&str]) -> RunOutput {
    let cache = HarnessCache::create("new").unwrap();
    let mut cmd = Command::new(CHEBY);
    cmd.arg("--color=never")
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1");
    cache.apply(&mut cmd);
    process::run(cmd, None, TIMEOUT).unwrap()
}

/// A fresh, empty working directory, deleted when dropped.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to create the directory fails the test"
)]
fn fresh() -> FreshDir {
    let parent = Path::new(TMP).join("new");
    std::fs::create_dir_all(&parent).unwrap();
    FreshDir::create_in(&parent, "work").unwrap()
}

/// The sorted names of `dir`'s entries.
#[expect(clippy::unwrap_used, reason = "a test helper")]
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// `cheby new app` in a fresh directory: status 0, silent, the package.
fn new_app(work: &FreshDir) -> PathBuf {
    let out = cheby(work.path(), &["new", "app"]);
    assert_eq!(
        (out.stdout.as_str(), out.stderr.as_str(), out.status),
        ("", "", Some(0)),
        "{}",
        out.describe_status()
    );
    work.path().join("app")
}

/// Exactly the three files of the answer, with their texts; no `git init`;
/// the package checks and is in `cheby fmt`'s form.
#[test]
fn cheby_new_package_checks_and_is_formatted() {
    let work = fresh();
    let app = new_app(&work);
    assert_eq!(names_in(work.path()), ["app"]);
    assert_eq!(
        names_in(&app),
        [".gitignore", "AGENTS.md", "cheby.toml", "src"]
    );
    assert_eq!(names_in(&app.join("src")), ["main.cheby"]);
    let read = |rel: &str| std::fs::read_to_string(app.join(rel)).unwrap();
    assert_eq!(read("cheby.toml"), "name = \"app\"\nversion = \"0.1.0\"\n");
    assert_eq!(
        read("src/main.cheby"),
        "//! The app program.\n\nimport std::io\n\nfn main() {\n  io::println(\"Hello, world!\")\n}\n"
    );
    assert_eq!(read(".gitignore"), "build/\n");

    let out = cheby(&app, &["check"]);
    assert_eq!(
        (out.stdout.as_str(), out.stderr.as_str(), out.status),
        ("", "", Some(0))
    );
    let out = cheby(&app, &["fmt", "--check"]);
    assert_eq!(
        (out.stdout.as_str(), out.stderr.as_str(), out.status),
        ("", "", Some(0))
    );
}

/// A library skeleton checks, is formatted and has no default program
/// (D-1855, D-1872).
#[test]
fn cheby_new_library_checks_and_is_formatted() {
    let work = fresh();
    let out = cheby(work.path(), &["new", "--library", "geometry"]);
    assert_eq!(
        (out.stdout.as_str(), out.stderr.as_str(), out.status),
        ("", "", Some(0))
    );
    let dir = work.path().join("geometry");
    for args in [["check"].as_slice(), &["fmt", "--check"]] {
        let out = cheby(&dir, args);
        assert_eq!(
            (out.stdout.as_str(), out.stderr.as_str(), out.status),
            ("", "", Some(0))
        );
    }
    let out = cheby(&dir, &["run"]);
    assert_eq!(
        (out.stdout.as_str(), out.stderr.as_str(), out.status),
        (
            "",
            "error: package `geometry` is a library, so `cheby run` needs the path of a module \
             with `main`\n",
            Some(1)
        )
    );
}

/// A library's skeleton passes its test, and `cheby build` compiles it and
/// writes nothing (D-1852).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn cheby_new_library_tests_and_builds() {
    let work = fresh();
    cheby(work.path(), &["new", "--library", "geometry"]);
    let dir = work.path().join("geometry");
    let out = cheby(&dir, &["test"]);
    assert_eq!((out.stderr.as_str(), out.status), ("", Some(0)));
    assert!(out.stdout.contains("1 passed"), "{}", out.stdout);
    for args in [["build"].as_slice(), &["build", "--release"]] {
        let out = cheby(&dir, args);
        assert_eq!(
            (out.stdout.as_str(), out.stderr.as_str(), out.status),
            ("", "", Some(0))
        );
        assert_eq!(
            names_in(&dir),
            [".gitignore", "AGENTS.md", "cheby.toml", "src"]
        );
    }
    // D-1875: no file is written, so `-o` is refused.
    let out = cheby(&dir, &["build", "-o", "geo"]);
    assert_eq!(
        (out.stdout.as_str(), out.stderr.as_str(), out.status),
        (
            "",
            "error: package `geometry` is a library, so `cheby build` writes a file for `-o` only \
             with the path of a module with `main`\n",
            Some(1)
        )
    );
    assert_eq!(
        names_in(&dir),
        [".gitignore", "AGENTS.md", "cheby.toml", "src"]
    );
}

/// The skeleton runs and prints its greeting.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn cheby_new_package_runs() {
    let work = fresh();
    let app = new_app(&work);
    let out = cheby(&app, &["run"]);
    assert_eq!(
        (out.stdout.as_str(), out.stderr.as_str(), out.status),
        ("Hello, world!\n", "", Some(0))
    );
}

/// An invalid name is a tool error, with a `help:` line when a valid name
/// is close, and writes nothing.
#[test]
fn cheby_new_refuses_an_invalid_name() {
    let work = fresh();
    let cases = [
        (
            "MyApp",
            format!("error: `MyApp` {INVALID}\nhelp: try `my_app`\n"),
        ),
        (
            "web-server",
            format!("error: `web-server` {INVALID}\nhelp: try `web_server`\n"),
        ),
        (
            "type",
            format!("error: `type` {INVALID}\nhelp: try `type_`\n"),
        ),
        ("...", format!("error: `...` {INVALID}\n")),
        (
            "std",
            "error: `std` is not a valid package name: it is reserved for the standard library\n"
                .to_owned(),
        ),
    ];
    for (name, stderr) in cases {
        let out = cheby(work.path(), &["new", name]);
        assert_eq!(
            (out.stdout.as_str(), out.stderr.as_str(), out.status),
            ("", stderr.as_str(), Some(1)),
            "{name}"
        );
    }
    assert_eq!(names_in(work.path()), Vec::<String>::new());
}

/// A path that exists, a file, a directory empty or not, or a link, is a
/// tool error and is left as it was.
#[test]
fn cheby_new_refuses_an_existing_path() {
    let work = fresh();
    let dir = work.path();
    std::fs::write(dir.join("file"), "kept\n").unwrap();
    std::fs::create_dir(dir.join("empty")).unwrap();
    std::fs::create_dir(dir.join("full")).unwrap();
    std::fs::write(dir.join("full/cheby.toml"), "kept\n").unwrap();
    // A dangling link exists too.
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.join("missing"), dir.join("link")).unwrap();
    let names = ["file", "empty", "full"]
        .into_iter()
        .chain(cfg!(unix).then_some("link"));
    let before = names_in(dir);
    for name in names {
        let out = cheby(dir, &["new", name]);
        assert_eq!(
            (out.stdout.as_str(), out.stderr.as_str(), out.status),
            (
                "",
                format!("error: `{name}` already exists\n").as_str(),
                Some(1)
            ),
            "{name}"
        );
    }
    assert_eq!(names_in(dir), before);
    assert_eq!(std::fs::read_to_string(dir.join("file")).unwrap(), "kept\n");
    assert_eq!(names_in(&dir.join("empty")), Vec::<String>::new());
    assert_eq!(names_in(&dir.join("full")), ["cheby.toml"]);
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_link(dir.join("link")).unwrap(),
        dir.join("missing")
    );
}
