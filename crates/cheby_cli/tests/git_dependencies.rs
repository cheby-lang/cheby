//! Git dependencies end to end (§13.4, D-1876 to D-1884): the built binary
//! and real repositories on disk. Git's `insteadOf` maps the host
//! `packages.example` to a directory, so no test uses the network, and
//! removing the mapping takes a test offline.
//!
//! Programs do not run on Windows until M6, and git dependencies wait for it
//! too, so this file is not built there (D-545).
#![cfg(not(windows))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::{CACHE_ENV, FreshDir};
use cheby_test_support::git_fixture;
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
const TIMEOUT: Duration = Duration::from_secs(120);

struct World {
    dir: PathBuf,
    /// A cache of its own, never the one a run shares (D-1556): the tests
    /// change the store in it.
    cache: FreshDir,
}

impl World {
    #[expect(clippy::unwrap_used, reason = "a test helper")]
    fn new(name: &str) -> Self {
        let dir = Path::new(TMP).join("git_dependencies").join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("repos")).unwrap();
        let world = Self {
            dir,
            cache: FreshDir::create("cli-git-dependencies").unwrap(),
        };
        world.online();
        world
    }

    fn online(&self) {
        self.write(
            "gitconfig",
            &format!(
                "[url \"file://{}/repos/\"]\n\tinsteadOf = https://packages.example/\n[user]\n\tname = T\n\temail = t@packages.example\n[init]\n\tdefaultBranch = main\n",
                self.dir.display()
            ),
        );
    }

    /// Every later fetch fails.
    fn offline(&self) {
        self.write(
            "gitconfig",
            "[url \"file:///nowhere/\"]\n\tinsteadOf = https://packages.example/\n",
        );
    }

    #[expect(clippy::unwrap_used, reason = "a test helper")]
    fn write(&self, path: &str, text: &str) {
        let path = self.dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[expect(clippy::unwrap_used, reason = "a test helper")]
    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.dir.join(path)).unwrap()
    }

    fn env(&self, cmd: &mut Command) {
        cmd.env("NO_COLOR", "1");
        git_fixture::isolate(cmd, &self.dir.join("gitconfig"));
    }

    #[expect(clippy::unwrap_used, reason = "a test helper")]
    fn git(&self, repo: &str, args: &[&str]) {
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(self.dir.join("repos").join(repo));
        self.env(&mut cmd);
        let out = process::run(cmd, None, TIMEOUT).unwrap();
        assert_eq!(out.status, Some(0), "git {args:?}: {}", out.stderr);
    }

    /// Commits the files to `repo`, made when it is new, and tags them.
    #[expect(clippy::unwrap_used, reason = "a test helper")]
    fn commit(&self, repo: &str, tag: &str, files: &[(&str, &str)]) {
        let dir = self.dir.join("repos").join(repo);
        if !dir.exists() {
            std::fs::create_dir_all(&dir).unwrap();
            self.git(repo, &["init", "--quiet"]);
        }
        for (path, text) in files {
            self.write(&format!("repos/{repo}/{path}"), text);
        }
        self.git(repo, &["add", "--all"]);
        self.git(repo, &["commit", "--quiet", "--allow-empty", "-m", tag]);
        self.git(repo, &["tag", "--force", tag]);
    }

    #[expect(clippy::unwrap_used, reason = "a test helper")]
    fn cheby_in(&self, package: &str, args: &[&str]) -> RunOutput {
        let mut cmd = Command::new(CHEBY);
        cmd.arg("--color=never")
            .args(args)
            .current_dir(self.dir.join(package));
        cmd.env(CACHE_ENV, self.cache.path());
        self.env(&mut cmd);
        process::run(cmd, None, TIMEOUT).unwrap()
    }

    fn cheby(&self, args: &[&str]) -> RunOutput {
        self.cheby_in("app", args)
    }

    fn prints(&self, args: &[&str], stdout: &str) {
        let out = self.cheby(args);
        assert_eq!(
            (out.stdout.as_str(), out.stderr.as_str(), out.status),
            (stdout, "", Some(0)),
            "{args:?}"
        );
    }

    fn fails(&self, args: &[&str], expected: &[&str]) {
        let out = self.cheby(args);
        assert_eq!(out.status, Some(1), "{args:?}: {}", out.stderr);
        for text in expected {
            assert!(out.stderr.contains(text), "{args:?}: {}", out.stderr);
        }
    }

    /// A program `app` whose manifest ends with `rest`.
    fn app(&self, rest: &str, main: &str) {
        self.write(
            "app/cheby.toml",
            &format!("name = \"app\"\nversion = \"0.1.0\"\n{rest}"),
        );
        self.write("app/src/main.cheby", main);
    }

    /// The directory of the fetched sources in the store.
    #[expect(clippy::unwrap_used, reason = "a test helper")]
    fn sources(&self) -> Vec<PathBuf> {
        let src = self.cache.path().join("pkg/v1/src");
        let mut out = Vec::new();
        for url in std::fs::read_dir(src).unwrap() {
            for commit in std::fs::read_dir(url.unwrap().path()).unwrap() {
                out.push(commit.unwrap().path());
            }
        }
        out
    }
}

fn library(name: &str, version: &str, rest: &str) -> String {
    format!("name = \"{name}\"\nversion = \"{version}\"\nkind = \"library\"\n{rest}")
}

/// A module with a type, a function that returns `n` and one that panics.
fn shapes(n: u32) -> String {
    format!(
        "//! Shapes.\n\n/// A point.\npub exposed type Point {{\n  Point {{ x: Int, y: Int }},\n}}\n\n/// A number.\npub fn number() -> Int {{\n  {n}\n}}\n\n/// No points.\npub fn none() -> List<Point> {{\n  []\n}}\n\n/// Fails.\npub fn boom() -> Int {{\n  panic as \"boom\"\n}}\n"
    )
}

/// The repository `shapes` with the releases 1.0.0, 1.1.0, 2.0.0 and a
/// pre-release.
fn with_shapes(name: &str) -> World {
    let world = World::new(name);
    for (version, n) in [
        ("1.0.0", 100),
        ("1.1.0", 110),
        ("2.0.0", 200),
        ("2.1.0-rc.1", 211),
    ] {
        world.commit(
            "shapes",
            &format!("v{version}"),
            &[
                ("cheby.toml", &library("shapes", version, "")),
                ("src/point.cheby", &shapes(n)),
            ],
        );
    }
    world
}

const MAIN: &str = "//! App.\n\nimport shapes::point\nimport std::io\n\nfn main() {\n  let n = point::number()\n  io::println(\"{n}\")\n}\n";

/// `[dependencies]` with `shapes` at a tag.
fn at(tag: &str) -> String {
    format!(
        "\n[dependencies]\nshapes = {{ git = \"https://packages.example/shapes\", tag = \"{tag}\" }}\n"
    )
}

/// `cheby add` pins the newest release, and then every command works with no
/// network (D-1881, D-1882).
#[test]
fn a_package_is_added_and_used_offline() {
    let world = with_shapes("add");
    world.app("", MAIN);
    world.prints(
        &["add", "https://packages.example/shapes/"],
        "added packages.example/shapes@2.0.0\n",
    );
    assert_eq!(
        world.read("app/cheby.toml"),
        "name = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nshapes = { git = \"https://packages.example/shapes/\", tag = \"v2.0.0\" }\n"
    );
    let lock = world.read("app/cheby.lock");
    assert!(
        lock.contains("url = \"packages.example/shapes\"\nmajor = \"2\"\nversion = \"2.0.0\"\n"),
        "{lock}"
    );
    assert!(lock.contains("required = [\"tag:v2.0.0\"]\n"), "{lock}");
    world.offline();
    world.prints(&["check", "--deny-warnings"], "");
    for args in [["run"].as_slice(), &["run"], &["run", "--release"]] {
        world.prints(args, "200\n");
    }
    for args in [["build"].as_slice(), &["build", "--release"]] {
        world.prints(args, "");
        let exe = Command::new(world.dir.join("app/build/native/app"));
        assert_eq!(process::run(exe, None, TIMEOUT).unwrap().stdout, "200\n");
    }
    // Adding it again at another version changes the entry.
    world.online();
    world.prints(
        &["add", "https://packages.example/shapes", "--tag", "v1.0.0"],
        "added packages.example/shapes@1.0.0\nremoved packages.example/shapes@2.0.0\n",
    );
    world.prints(&["run"], "100\n");
    world.app("", MAIN);
    std::fs::remove_file(world.dir.join("app/cheby.lock")).unwrap();
    let out = world.cheby(&["add", "https://packages.example/shapes", "--rev", "HEAD"]);
    assert_eq!(out.status, Some(1));
    assert_eq!(
        out.stderr,
        "error: `HEAD` is not a commit hash of at least seven digits\n"
    );
}

/// A location names the package by its URL and version (D-468), and a
/// repository over two URLs is one package.
#[test]
fn a_panic_names_the_url() {
    let world = with_shapes("panic");
    world.app(&at("v1.0.0"), &MAIN.replace("number()", "boom()"));
    world.prints(&["update"], "added packages.example/shapes@1.0.0\n");
    let out = world.cheby(&["run"]);
    assert_eq!(out.status, Some(101));
    assert!(
        out.stderr
            .contains("panicked at packages.example/shapes@1.0.0/src/point.cheby:20:3:\nboom\n"),
        "{}",
        out.stderr
    );
}

/// The highest version of a major that a manifest requires is chosen, through
/// other packages too, and two majors are two packages whose values never
/// compare equal (D-192, D-238, D-1313).
#[test]
fn versions_and_majors() {
    let world = with_shapes("versions");
    world.commit(
        "draw",
        "v0.1.0",
        &[
            (
                "cheby.toml",
                &library(
                    "draw",
                    "0.1.0",
                    "\n[dependencies]\ns = { git = \"https://packages.example/shapes/\", tag = \"v1.1.0\" }\n",
                ),
            ),
            (
                "src/main.cheby",
                "//! Draw.\n\nimport s::point\n\n/// The origin.\npub fn origin() -> point::Point {\n  point::Point { x: 1, y: 2 }\n}\n",
            ),
        ],
    );
    world.app(
        "\n[dependencies]\ndraw = { git = \"https://packages.example/draw\", tag = \"v0.1.0\" }\none = { git = \"https://packages.example/shapes\", tag = \"v1.0.0\" }\ntwo = { git = \"https://packages.example/shapes\", tag = \"v2.0.0\" }\n",
        "//! App.\n\nimport draw\nimport one::point as one_point\nimport std::io\nimport two::point as two_point\n\n/// Any value.\ninterface Any\n\nfn main() {\n  let n = one_point::number()\n  let m = two_point::number()\n  let p: one_point::Point = draw::origin()\n  let a: dyn Any = p\n  let b: dyn Any = one_point::Point { x: 1, y: 2 }\n  let c: dyn Any = two_point::Point { x: 1, y: 2 }\n  let ab = a == b\n  let bc = b == c\n  let d: dyn Any = one_point::none()\n  let e: dyn Any = two_point::none()\n  let de = d == e\n  io::println(\"{n} {m} {ab} {bc} {de}\")\n}\n",
    );
    world.prints(
        &["update"],
        "added packages.example/draw@0.1.0\nadded packages.example/shapes@1.1.0\nadded packages.example/shapes@2.0.0\n",
    );
    for args in [["run"].as_slice(), &["run"], &["run", "--release"]] {
        world.prints(args, "110 200 True False False\n");
    }
    world.prints(&["build"], "");
    let exe = Command::new(world.dir.join("app/build/native/app"));
    assert_eq!(
        process::run(exe, None, TIMEOUT).unwrap().stdout,
        "110 200 True False False\n"
    );
    assert!(
        world
            .read("app/cheby.lock")
            .contains("required = [\"tag:v1.0.0\", \"tag:v1.1.0\"]")
    );
    world.prints(&["update"], "");
}

/// A build downloads nothing and changes no lockfile: it names the command
/// that does (D-1881).
#[test]
fn a_build_says_what_to_run() {
    let world = with_shapes("says");
    world.app(&at("v1.0.0"), MAIN);
    let not_locked = [
        "error[E0036]: dependency `shapes` is not in `cheby.lock` as the manifest requires it\n --> cheby.toml:5:1\n",
        "help: run `cheby update`",
    ];
    world.fails(&["check"], &not_locked);
    assert!(!world.dir.join("app/cheby.lock").exists());
    world.prints(&["update"], "added packages.example/shapes@1.0.0\n");
    world.app(&at("v1.1.0"), MAIN);
    world.fails(&["run"], &not_locked);
    world.prints(
        &["update"],
        "changed packages.example/shapes@1.1.0 from 1.0.0\n",
    );
    world.prints(&["run"], "110\n");
    // A machine that has the lockfile and not the sources.
    std::fs::remove_dir_all(world.cache.path().join("pkg")).unwrap();
    world.fails(
        &["check"],
        &[
            "error[E0036]: `packages.example/shapes@1.1.0` is not downloaded\n",
            "help: run `cheby fetch`",
        ],
    );
    let lock = world.read("app/cheby.lock");
    world.prints(&["fetch"], "fetched packages.example/shapes@1.1.0\n");
    world.prints(&["fetch"], "");
    assert_eq!(world.read("app/cheby.lock"), lock);
    world.prints(&["run"], "110\n");
    world.write("app/cheby.lock", "<<<<<<< HEAD\n");
    world.fails(
        &["check"],
        &["error: `cheby.lock` is not valid: ", "cheby update"],
    );
}

/// A changed file in the store, an added one, and a lockfile whose checksum is
/// another's all fail (D-1878).
#[test]
fn checksums_are_verified() {
    use std::os::unix::fs::PermissionsExt as _;
    let world = with_shapes("checksum");
    world.app(&at("v1.0.0"), MAIN);
    world.prints(&["update"], "added packages.example/shapes@1.0.0\n");
    let sources = world.sources();
    let [tree] = sources.as_slice() else {
        panic!("{sources:?}");
    };
    let mismatch = "the files of `packages.example/shapes@1.0.0` do not match the checksum";
    let file = tree.join("src/point.cheby");
    assert!(std::fs::write(&file, "").is_err(), "the store is read-only");
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&file, text.replace("100", "666")).unwrap();
    for command in ["check", "run", "test", "build", "fetch"] {
        world.fails(&[command], &[mismatch, "cheby fetch"]);
    }
    std::fs::write(&file, &text).unwrap();
    world.prints(&["run"], "100\n");
    std::fs::write(tree.join("src/more.cheby"), "//! More.\n").unwrap();
    world.fails(&["run"], &[mismatch]);
    std::fs::remove_dir_all(tree).unwrap();
    world.prints(&["fetch"], "fetched packages.example/shapes@1.0.0\n");
    let lock = world.read("app/cheby.lock");
    let (before, after) = lock.split_once("sha256:").unwrap();
    let rest = after.split_once('"').unwrap().1;
    let wrong = format!("{before}sha256:{}\"{rest}", "0".repeat(64));
    world.write("app/cheby.lock", &wrong);
    world.fails(&["run"], &[mismatch]);
    std::fs::remove_dir_all(tree).unwrap();
    world.fails(&["fetch"], &[mismatch]);
    assert!(!tree.exists());
}

/// A tag that moved changes nothing until `cheby update`, which says so.
#[test]
fn a_moved_tag_needs_an_update() {
    let world = with_shapes("moved");
    world.app(&at("v1.0.0"), MAIN);
    world.prints(&["update"], "added packages.example/shapes@1.0.0\n");
    world.commit(
        "shapes",
        "v1.0.0",
        &[
            ("cheby.toml", &library("shapes", "1.0.0", "")),
            ("src/point.cheby", &shapes(666)),
        ],
    );
    world.prints(&["fetch"], "");
    world.prints(&["run"], "100\n");
    let out = world.cheby(&["update"]);
    assert!(
        out.stdout
            .starts_with("changed packages.example/shapes@1.0.0, whose commit moved from "),
        "{}",
        out.stdout
    );
    world.prints(&["run"], "666\n");
}

/// A commit that no branch and no tag holds any more is fetched by its hash,
/// when the server gives it (D-1887).
#[test]
fn a_commit_of_a_moved_tag_is_fetched() {
    let world = with_shapes("unreachable");
    world.app(&at("v2.0.0"), MAIN);
    world.prints(&["update"], "added packages.example/shapes@2.0.0\n");
    world.git("shapes", &["checkout", "--quiet", "-B", "main", "v1.0.0"]);
    world.git("shapes", &["tag", "--delete", "v2.0.0", "v2.1.0-rc.1"]);
    world.git("shapes", &["tag", "v2.0.0", "v1.1.0"]);
    std::fs::remove_dir_all(world.cache.path().join("pkg")).unwrap();
    world.prints(&["fetch"], "fetched packages.example/shapes@2.0.0\n");
    world.prints(&["run"], "200\n");
    // A server that gives only what its branches and tags hold.
    std::fs::remove_dir_all(world.cache.path().join("pkg")).unwrap();
    let config = world.read("gitconfig");
    world.write("gitconfig", &format!("{config}[protocol]\n\tversion = 0\n"));
    world.fails(
        &["fetch"],
        &[
            "error: `packages.example/shapes` no longer has the commit `",
            "help: its tag may have moved; `cheby update` locks what the tags name now",
        ],
    );
}

/// A library whose one function returns `n`, at version 1.0.0.
fn numbered(world: &World, name: &str, version: &str, n: u32, rest: &str) {
    world.commit(
        name,
        &format!("v{version}"),
        &[
            ("cheby.toml", &library(name, version, rest)),
            (
                "src/main.cheby",
                &format!("//! {name}.\n\n/// A number.\npub fn number() -> Int {{\n  {n}\n}}\n"),
            ),
        ],
    );
}

/// `cheby add` looks up only the package it adds. What is locked keeps its
/// commit when its tag moved, and needs no server (D-1895).
#[test]
fn add_keeps_what_is_locked() {
    let world = World::new("pins");
    for (name, n) in [("old", 1), ("other", 2), ("fresh", 3)] {
        numbered(&world, name, "1.0.0", n, "");
    }
    world.app(
        "",
        "//! App.\n\nimport old\nimport std::io\n\nfn main() {\n  let n = old::number()\n  io::println(\"{n}\")\n}\n",
    );
    world.prints(
        &["add", "https://packages.example/old"],
        "added packages.example/old@1.0.0\n",
    );
    let locked = world.read("app/cheby.lock");
    let (_, old) = locked.split_once("[[package]]").unwrap();
    // The tag moves, and then its server is gone.
    numbered(&world, "old", "1.0.0", 100, "");
    world.prints(
        &["add", "https://packages.example/other"],
        "added packages.example/other@1.0.0\n",
    );
    assert!(world.read("app/cheby.lock").contains(old.trim_end()));
    world.prints(&["run"], "1\n");
    let (repo, gone) = (world.dir.join("repos/old"), world.dir.join("repos/gone"));
    std::fs::rename(&repo, &gone).unwrap();
    world.prints(
        &["add", "https://packages.example/fresh"],
        "added packages.example/fresh@1.0.0\n",
    );
    world.prints(&["run"], "1\n");
    std::fs::rename(&gone, &repo).unwrap();
    // A package that is locked and not downloaded is, as it is locked.
    std::fs::remove_dir_all(world.cache.path().join("pkg")).unwrap();
    world.prints(
        &["add", "https://packages.example/fresh", "--name", "again"],
        "",
    );
    world.prints(&["run"], "1\n");
    // The added package itself is looked up, and `update` looks up every one.
    let out = world.cheby(&["add", "https://packages.example/old"]);
    assert!(
        out.stdout
            .starts_with("changed packages.example/old@1.0.0, whose commit moved from "),
        "{}{}",
        out.stdout,
        out.stderr
    );
    world.prints(&["run"], "100\n");
    numbered(&world, "other", "1.0.0", 200, "");
    let out = world.cheby(&["update"]);
    assert!(
        out.stdout
            .starts_with("changed packages.example/other@1.0.0, "),
        "{}",
        out.stdout
    );
    // A lockfile that cannot be read is not replaced.
    world.write("app/cheby.lock", "<<<<<<< HEAD\n");
    world.fails(
        &["add", "https://packages.example/fresh"],
        &["`cheby.lock` is not valid", "cheby update"],
    );
    assert_eq!(world.read("app/cheby.lock"), "<<<<<<< HEAD\n");
}

/// An added package raises what it needs a higher version of, and nothing
/// else. The added package itself may go down (D-1895).
#[test]
fn add_raises_only_what_it_needs() {
    let world = World::new("raises");
    numbered(&world, "base", "1.0.0", 10, "");
    numbered(&world, "base", "1.1.0", 11, "");
    numbered(&world, "base", "1.2.0", 12, "");
    let needs = |version: &str| {
        format!(
            "\n[dependencies]\nbase = {{ git = \"https://packages.example/base\", tag = \"v{version}\" }}\n"
        )
    };
    numbered(&world, "low", "1.0.0", 1, &needs("1.0.0"));
    numbered(&world, "high", "1.0.0", 2, &needs("1.2.0"));
    world.app(
        "",
        "//! App.\n\nimport base\nimport std::io\n\nfn main() {\n  let n = base::number()\n  io::println(\"{n}\")\n}\n",
    );
    world.prints(
        &["add", "https://packages.example/base", "--tag", "v1.1.0"],
        "added packages.example/base@1.1.0\n",
    );
    world.prints(
        &["add", "https://packages.example/low"],
        "added packages.example/low@1.0.0\n",
    );
    world.prints(&["run"], "11\n");
    world.prints(
        &["add", "https://packages.example/high"],
        "changed packages.example/base@1.2.0 from 1.1.0\nadded packages.example/high@1.0.0\n",
    );
    world.prints(&["run"], "12\n");
    // `high` still needs 1.2.0. Without it, 1.0.0 is chosen again.
    world.prints(
        &["add", "https://packages.example/base", "--tag", "v1.0.0"],
        "",
    );
    world.prints(&["run"], "12\n");
    let manifest = world.read("app/cheby.toml");
    let without: Vec<&str> = manifest
        .split_inclusive('\n')
        .filter(|l| !l.starts_with("high"))
        .collect();
    world.write("app/cheby.toml", &without.concat());
    world.prints(
        &["add", "https://packages.example/base", "--tag", "v1.0.0"],
        "changed packages.example/base@1.0.0 from 1.2.0\nremoved packages.example/high@1.0.0\n",
    );
    world.prints(&["run"], "10\n");
}

/// What a package from git may not hold (D-1859, D-1879). A refused package
/// changes neither the manifest nor the lockfile.
#[test]
fn refused_packages() {
    let world = World::new("refused");
    let good = library("bad", "1.0.0", "");
    let refused = |repo: &str, expected: &str| {
        world.app("", "//! App.\n\nfn main() {\n  Nil\n}\n");
        let before = world.read("app/cheby.toml");
        world.fails(
            &["add", &format!("https://packages.example/{repo}")],
            &[expected],
        );
        assert_eq!(world.read("app/cheby.toml"), before);
        assert!(!world.dir.join("app/cheby.lock").exists());
    };
    world.commit("link", "v1.0.0", &[("cheby.toml", &good)]);
    std::os::unix::fs::symlink("/etc/passwd", world.dir.join("repos/link/secret")).unwrap();
    world.commit("link", "v1.0.1", &[]);
    refused(
        "link",
        "`packages.example/link` has a symbolic link, `secret`",
    );
    world.commit("sub", "v1.0.0", &[("cheby.toml", &good)]);
    world.git(
        "sub",
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},lib", "a".repeat(40)),
        ],
    );
    world.git("sub", &["commit", "--quiet", "-m", "sub"]);
    world.git("sub", &["tag", "v1.0.1"]);
    refused("sub", "`packages.example/sub` has a submodule, `lib`");
    world.commit(
        "lfs",
        "v1.0.0",
        &[
            ("cheby.toml", &good),
            (
                "data.bin",
                "version https://git-lfs.github.com/spec/v1\noid sha256:0\nsize 1\n",
            ),
        ],
    );
    refused("lfs", "keeps `data.bin` in Git LFS");
    world.commit("none", "v1.0.0", &[("README", "none\n")]);
    refused("none", "has no `cheby.toml` at its root");
    world.commit(
        "program",
        "v1.0.0",
        &[("cheby.toml", "name = \"p\"\nversion = \"1.0.0\"\n")],
    );
    refused("program", "is a program, not a library");
    world.commit(
        "path",
        "v1.0.0",
        &[(
            "cheby.toml",
            &library("p", "1.0.0", "\n[dependencies]\na = { path = \"../a\" }\n"),
        )],
    );
    refused(
        "path",
        "has a path dependency, which a package from git may not have",
    );
    world.commit("untagged", "trunk", &[("cheby.toml", &good)]);
    refused(
        "untagged",
        "`packages.example/untagged` has no tag of a release",
    );
    refused("absent", "cannot fetch `packages.example/absent`");
}

/// No URL, tag or commit reaches git as anything else (D-1879).
#[test]
fn refused_urls() {
    let world = World::new("urls");
    for url in [
        "ext::sh -c id",
        "file:///etc",
        "http://packages.example/a",
        "../shapes",
        "https://me:secret@packages.example/a",
    ] {
        world.app("", "//! App.\n\nfn main() {\n  Nil\n}\n");
        let out = world.cheby(&["add", url]);
        assert_eq!(out.status, Some(1), "{url}");
        assert!(
            out.stderr.starts_with("error: the git URL "),
            "{}",
            out.stderr
        );
        assert!(!out.stderr.contains("secret"), "{}", out.stderr);
        world.app(
            &format!("\n[dependencies]\na = {{ git = \"{url}\", tag = \"v1\" }}\n"),
            "//! App.\n\nfn main() {\n  Nil\n}\n",
        );
        world.fails(&["update"], &["E0036"]);
        world.fails(&["check"], &["error[E0036]: the git URL "]);
    }
    for entry in [
        "tag = \"--upload-pack=id\"",
        "rev = \"HEAD\"",
        "tag = \"a..b\"",
    ] {
        world.app(
            &format!("\n[dependencies]\na = {{ git = \"https://packages.example/a\", {entry} }}\n"),
            "//! App.\n\nfn main() {\n  Nil\n}\n",
        );
        world.fails(&["check"], &["error[E0036]: ", "is not a valid"]);
    }
}

/// A package from git has C sources, compiled where the store keeps them,
/// which changes none of its files (D-940, D-1878, D-1888).
#[test]
fn a_package_from_git_has_native_code() {
    let world = World::new("native");
    let manifest = library(
        "half",
        "1.0.0",
        "targets = [\"native\"]\n\n[native]\nsources = [\"src/half.c\"]\nlibraries = [\"m\"]\n",
    );
    world.commit(
        "half",
        "v1.0.0",
        &[
            ("cheby.toml", &manifest),
            ("src/half.h", "#define HALF 0.5\n"),
            ("src/half.c", "#include <math.h>\n#include \"half.h\"\ndouble half_of(double x) { return floor(x * HALF); }\n"),
            ("src/main.cheby", "//! Half.\n\n/// Half of a number, rounded down.\n@external(native, \"half_of\")\npub fn half(x: Float) -> Float\n"),
        ],
    );
    world.app(
        "targets = [\"native\"]\n",
        "//! App.\n\nimport half\nimport std::io\n\nfn main() {\n  let n = half::half(7.0)\n  io::println(\"{n}\")\n}\n\ntest \"half of 9\" {\n  assert half::half(9.0) == 4.0\n}\n",
    );
    world.prints(
        &["add", "https://packages.example/half"],
        "added packages.example/half@1.0.0\n",
    );
    for args in [["run"].as_slice(), &["run"], &["run", "--release"]] {
        world.prints(args, "3.0\n");
    }
    world.prints(&["build"], "");
    let exe = Command::new(world.dir.join("app/build/native/app"));
    assert_eq!(process::run(exe, None, TIMEOUT).unwrap().stdout, "3.0\n");
    assert_eq!(world.cheby(&["test"]).status, Some(0));
}

/// Every name that a git tag may have can be required, one with a quotation
/// mark too (D-1879, D-1880).
#[test]
fn a_tag_with_a_quotation_mark() {
    let world = with_shapes("quotation_mark");
    world.git("shapes", &["tag", "v1\"x", "v1.0.0"]);
    world.app("", MAIN);
    world.prints(
        &["add", "https://packages.example/shapes", "--tag", "v1\"x"],
        "added packages.example/shapes@1.0.0\n",
    );
    world.prints(&["update"], "");
    world.prints(&["run"], "100\n");
    world.app(
        "\n[dependencies]\nshapes = { git = \"https://packages.example/shapes\", tag = 'v1\"x' }\n",
        MAIN,
    );
    world.prints(&["update"], "");
    world.prints(&["run"], "100\n");
}

/// A `rev` is the start of a commit's hash, whatever branches and tags are
/// named like it.
#[test]
fn a_rev_is_no_branch_and_no_tag() {
    let world = with_shapes("rev");
    world.app(&at("v1.0.0"), MAIN);
    assert_eq!(world.cheby(&["update"]).status, Some(0));
    let lock = world.read("app/cheby.lock");
    let (_, commit) = lock.split_once("commit = \"").unwrap();
    let full = commit.get(..40).unwrap();
    for length in [7, 12, 40] {
        let rev = full.get(..length).unwrap();
        if length < 40 {
            world.git("shapes", &["branch", rev, "v2.0.0"]);
            world.git("shapes", &["tag", rev, "v2.0.0"]);
        }
        for rev in [rev, &rev.to_ascii_uppercase()] {
            world.app(
                &format!("\n[dependencies]\nshapes = {{ git = \"https://packages.example/shapes\", rev = \"{rev}\" }}\n"),
                MAIN,
            );
            world.prints(&["update"], "");
            world.prints(&["run"], "100\n");
        }
    }
    for rev in ["0000000", &"0".repeat(40), &"0".repeat(64)] {
        world.app(
            &format!("\n[dependencies]\nshapes = {{ git = \"https://packages.example/shapes\", rev = \"{rev}\" }}\n"),
            MAIN,
        );
        world.fails(
            &["update"],
            &[&format!(
                "error: `packages.example/shapes` has no commit `{rev}`\n"
            )],
        );
    }
}

/// A package that cannot be read is named.
#[test]
fn a_missing_manifest_is_named() {
    let world = with_shapes("missing_manifest");
    let overridden = format!(
        "{}\n[overrides]\n\"https://packages.example/shapes\" = {{ path = \"../local\" }}\n",
        at("v1.0.0")
    );
    world.app(&overridden, MAIN);
    std::fs::create_dir_all(world.dir.join("local")).unwrap();
    let out = world.cheby(&["update"]);
    assert_eq!(out.status, Some(1));
    assert!(
        out.stderr
            .contains("missing_manifest/local/cheby.toml: No such file or directory"),
        "{}",
        out.stderr
    );
}

/// An override gives a directory the identity of a git package, so its types
/// are the types another package uses (D-1860, D-1865, D-1866).
#[test]
fn an_override_replaces_a_package() {
    let world = with_shapes("override");
    world.commit(
        "draw",
        "v0.1.0",
        &[
            ("cheby.toml", &library("draw", "0.1.0", &at("v1.0.0"))),
            (
                "src/main.cheby",
                "//! Draw.\n\nimport shapes::point\n\n/// A point.\npub fn origin() -> point::Point {\n  point::Point { x: point::number(), y: 2 }\n}\n",
            ),
        ],
    );
    let dependencies = format!(
        "{}draw = {{ git = \"https://packages.example/draw\", tag = \"v0.1.0\" }}\n",
        at("v1.0.0")
    );
    let main = "//! App.\n\nimport draw\nimport shapes::point\nimport std::io\n\nfn main() {\n  let p: point::Point = draw::origin()\n  let n = p.x\n  io::println(\"{n}\")\n}\n";
    world.app(&dependencies, main);
    let out = world.cheby(&["update"]);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    world.prints(&["run"], "100\n");
    let lock = world.read("app/cheby.lock");
    let with_override = format!(
        "{dependencies}\n[overrides]\n\"git@packages.example:shapes.git\" = {{ path = \"../local\" }}\n"
    );
    world.write(
        "local/cheby.toml",
        &library(
            "local_shapes",
            "1.5.0",
            "\n[dependencies]\nhelp = { path = \"../help\" }\n",
        ),
    );
    world.write(
        "local/src/point.cheby",
        &shapes(0)
            .replace("//! Shapes.\n", "//! Shapes.\n\nimport help\n")
            .replace("  0\n", "  help::number()\n"),
    );
    world.write("help/cheby.toml", &library("help", "0.1.0", ""));
    world.write(
        "help/src/main.cheby",
        "//! Help.\n\n/// A number.\npub fn number() -> Int {\n  7\n}\n",
    );
    world.app(&with_override, main);
    world.offline();
    for args in [["run"].as_slice(), &["run"], &["run", "--release"]] {
        world.prints(args, "7\n");
    }
    assert_eq!(world.read("app/cheby.lock"), lock);
    world.write(
        "app/src/main.cheby",
        &main.replace("let n = p.x", "let n = point::boom()"),
    );
    let out = world.cheby(&["run"]);
    assert!(
        out.stderr
            .contains("panicked at packages.example/shapes@1.5.0/src/point.cheby:"),
        "{}",
        out.stderr
    );
    world.write("local/cheby.toml", &library("local_shapes", "3.0.0", ""));
    world.fails(
        &["check"],
        &["error[E0036]: the override of `packages.example/shapes` has version 3.0.0, and the build uses major 1\n"],
    );
    world.app(&dependencies, main);
    world.prints(&["run"], "100\n");
}
