//! `cheby build` (D-924, D-937): executables of small packages, run and
//! compared with `cheby run` of the same package (D-925, D-926, D-474,
//! D-306, D-1025), user types among them, which the module records carry
//! and the executable's start registers (D-919, D-920), and constants,
//! which the program object carries as images and the start installs
//! (D-979); the runtime archive's tool errors (D-929, D-930); and
//! determinism (D-934).
//!
//! Every test links against the runtime archive next to `cheby`
//! (`target/<profile>/libcheby_rt_exe.a`): `cargo build -p cheby_rt_exe`
//! comes first, and a missing or stale archive fails the tests, never
//! skips them (D-929). Every `cheby` gets a fresh `CHEBY_CACHE` of its own
//! (D-1128), and every build writes with `-o` into a
//! temporary directory, so no test writes into the repository.

#![expect(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    reason = "test helpers: a failed setup step fails the test; offsets of a small file"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::{FreshDir, HarnessCache};
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(120);

/// 001, as `docs/examples` has it.
fn example_001() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/001_hello_world")
}

/// A package `name` in `dir` with `src/main.cheby` holding `main`.
fn package(dir: &Path, name: &str, main: &str) -> PathBuf {
    let root = dir.join(name);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("cheby.toml"),
        format!("name = \"{name}\"\nversion = \"0.1.0\"\n"),
    )
    .unwrap();
    std::fs::write(root.join("src/main.cheby"), main).unwrap();
    root
}

/// Copies the directory `from` to `to`.
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A fresh build cache (D-1128), never a kept one.
fn fresh_cache() -> HarnessCache {
    HarnessCache::create_with("cli-build", None).unwrap()
}

/// `cheby <args>` in `dir` with `cache` and the extra environment `env`.
fn cheby(dir: &Path, args: &[&str], cache: &HarnessCache, env: &[(&str, &str)]) -> RunOutput {
    let mut cmd = Command::new(CHEBY);
    cmd.args(args).current_dir(dir).env("NO_COLOR", "1");
    cache.apply(&mut cmd);
    for (key, value) in env {
        cmd.env(key, value);
    }
    process::run(cmd, None, TIMEOUT).unwrap()
}

/// `cheby build -o <exe>` of the package at `root` with a fresh cache.
fn build(root: &Path, exe: &Path) -> RunOutput {
    let cache = fresh_cache();
    cheby(root, &["build", "-o", exe.to_str().unwrap()], &cache, &[])
}

/// Builds `root` into `<tmp>/exe/<name>` and asserts the build succeeded
/// and printed nothing on standard output (D-937).
fn built(root: &Path, tmp: &Path, name: &str) -> (RunOutput, PathBuf) {
    let exe = tmp.join("exe").join(name);
    let out = build(root, &exe);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    assert_eq!(out.stdout, "");
    assert!(exe.is_file());
    (out, exe)
}

/// Runs `sh -c <script>` with `$0` set to `program`.
fn shell(program: &Path, script: &str, dir: &Path, cache: &HarnessCache) -> RunOutput {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(script)
        .arg(program)
        .current_dir(dir)
        .env("NO_COLOR", "1");
    cache.apply(&mut cmd);
    process::run(cmd, None, TIMEOUT).unwrap()
}

/// The executable's run, after its build's standard error, against `cheby
/// run`'s (D-1025): status, standard output and standard error.
fn assert_same(build: &RunOutput, exe: &RunOutput, run: &RunOutput) {
    assert_eq!(exe.status, run.status, "{}\n{}", exe.stderr, run.stderr);
    assert_eq!(exe.stdout, run.stdout);
    assert_eq!(format!("{}{}", build.stderr, exe.stderr), run.stderr);
}

/// 001 as an executable prints `Hello, world!` and exits 0, as `cheby
/// run` does (D-924's slice).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_001_runs() {
    let tmp = FreshDir::create("cheby-build-001").unwrap();
    let root = tmp.path().join("hello_world");
    copy_dir(&example_001(), &root);
    let (build, exe) = built(&root, tmp.path(), "hello_world");
    let cache = fresh_cache();
    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert_eq!(ran.stdout, "Hello, world!\n");
    assert_eq!(ran.status, Some(0));
    let run = cheby(&root, &["run"], &cache, &[]);
    assert_same(&build, &ran, &run);
    // Only the executable is written, and nothing into the package
    // (D-937).
    assert!(!root.join("build").exists());
}

/// Without `-o`, the executable is `build/native/<package>`, or
/// `build/native/<module path with :: replaced by ->` for a module path
/// (D-937), and nothing else is written into the package.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_default_output() {
    let tmp = FreshDir::create("cheby-build-default").unwrap();
    let root = package(
        tmp.path(),
        "app",
        "//! Prints from the root module.\n\nimport std::io\n\nfn main() {\n  io::println(\"root\")\n}\n",
    );
    std::fs::create_dir_all(root.join("src/tools")).unwrap();
    std::fs::write(
        root.join("src/tools/migrate.cheby"),
        "//! Prints from a tool.\n\nimport std::io\n\nfn main() {\n  io::println(\"tool\")\n}\n",
    )
    .unwrap();
    let cache = fresh_cache();
    let out = cheby(&root, &["build"], &cache, &[]);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    assert_eq!((out.stdout.as_str(), out.stderr.as_str()), ("", ""));
    let exe = root.join("build/native/app");
    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert_eq!(ran.stdout, "root\n");
    let out = cheby(&root, &["build", "app::tools::migrate"], &cache, &[]);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    let exe = root.join("build/native/app-tools-migrate");
    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert_eq!(ran.stdout, "tool\n");
    let mut written: Vec<String> = std::fs::read_dir(root.join("build/native"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    written.sort();
    assert_eq!(written, ["app", "app-tools-migrate"]);
    // A relative `-o` is taken from the current directory.
    let out = cheby(&root.join("src"), &["build", "-o", "../out/x"], &cache, &[]);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    assert!(root.join("out/x").is_file());
}

/// The package of the stream tests: one line on each output stream.
const STREAMS: &str = "//! Writes one line to each output stream.\n\nimport std::io\n\nfn main() {\n  io::println(\"out\")\n  io::eprintln(\"err\")\n}\n";

/// Closed standard streams behave as a null device in an executable as
/// under `cheby run` (D-474, D-925): the program exits with 0.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_closed_streams() {
    let tmp = FreshDir::create("cheby-build-streams").unwrap();
    let root = package(tmp.path(), "streams", STREAMS);
    let (build, exe) = built(&root, tmp.path(), "streams");
    let cache = fresh_cache();
    for redirects in ["<&-", ">&-", "2>&-", "<&- >&- 2>&-"] {
        let ran = shell(&exe, &format!("exec \"$0\" {redirects}"), &root, &cache);
        let run = shell(
            Path::new(CHEBY),
            &format!("exec \"$0\" run {redirects}"),
            &root,
            &cache,
        );
        assert_eq!(ran.status, Some(0), "{redirects}: {}", ran.stderr);
        assert_same(&build, &ran, &run);
    }
}

/// An executable whose output goes to a reader that closes early ends
/// with status 1, as `cheby run` does (D-306, D-925: `SIGPIPE` is
/// ignored, so the write fails with `EPIPE`).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_early_closing_reader() {
    let tmp = FreshDir::create("cheby-build-pipe").unwrap();
    let root = package(
        tmp.path(),
        "lines",
        "//! Prints many lines.\n\nimport std::io\n\nfn lines(n: Int) {\n  case n {\n    0 => Nil\n    _ => {\n      io::println(\"line {n}\")\n      lines(n - 1)\n    }\n  }\n}\n\nfn main() {\n  lines(200000)\n}\n",
    );
    let (build, exe) = built(&root, tmp.path(), "lines");
    let cache = fresh_cache();
    // The writer's status goes to standard error; `head` reads one line.
    let script = |args: &str| format!("{{ \"$0\" {args}; echo \"status=$?\" >&2; }} | head -n 1");
    let ran = shell(&exe, &script(""), &root, &cache);
    let run = shell(Path::new(CHEBY), &script("run"), &root, &cache);
    assert_eq!(ran.stdout, "line 200000\n");
    assert!(ran.stderr.ends_with("status=1\n"), "{}", ran.stderr);
    assert_same(&build, &ran, &run);
}

/// A panic in an executable prints the two lines of §11.6 and exits with
/// 101, as under `cheby run` (D-926).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_panic() {
    let tmp = FreshDir::create("cheby-build-panic").unwrap();
    let root = package(
        tmp.path(),
        "boom",
        "//! Overflows an `I16`.\n\nfn main() {\n  let x: I16 = 32767\n  let _ = x + 1\n}\n",
    );
    let (build, exe) = built(&root, tmp.path(), "boom");
    let cache = fresh_cache();
    let ran = process::run(
        {
            let mut cmd = Command::new(&exe);
            cmd.current_dir(&root);
            cmd
        },
        None,
        TIMEOUT,
    )
    .unwrap();
    assert_eq!(ran.status, Some(101));
    assert_eq!(ran.stderr.lines().count(), 2, "{}", ran.stderr);
    assert!(ran.stderr.starts_with("panicked at "), "{}", ran.stderr);
    let run = cheby(&root, &["run"], &cache, &[]);
    assert_same(&build, &ran, &run);
}

/// A missing runtime archive is a tool error naming the path and the
/// command that builds it (D-929); nothing is written.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_missing_archive() {
    let tmp = FreshDir::create("cheby-build-noarchive").unwrap();
    let root = tmp.path().join("hello_world");
    copy_dir(&example_001(), &root);
    let missing = tmp.path().join("nowhere/libcheby_rt_exe.a");
    let exe = tmp.path().join("hello_world.exe");
    let cache = fresh_cache();
    let out = cheby(
        &root,
        &["build", "-o", exe.to_str().unwrap()],
        &cache,
        &[("CHEBY_RT_ARCHIVE", missing.to_str().unwrap())],
    );
    assert_eq!(out.status, Some(1));
    assert!(out.stderr.starts_with("error: "), "{}", out.stderr);
    assert!(
        out.stderr.contains(missing.to_str().unwrap()),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("cargo build -p cheby_rt_exe"),
        "{}",
        out.stderr
    );
    assert!(!exe.exists());
    // A relative CHEBY_RT_ARCHIVE is a tool error too.
    let out = cheby(
        &root,
        &["build", "-o", exe.to_str().unwrap()],
        &cache,
        &[("CHEBY_RT_ARCHIVE", "libcheby_rt_exe.a")],
    );
    assert_eq!(out.status, Some(1));
    assert!(out.stderr.contains("absolute"), "{}", out.stderr);
}

/// An archive without this `cheby`'s runtime ID's symbol is the stale
/// archive's tool error, before anything links (D-930).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_stale_archive() {
    let tmp = FreshDir::create("cheby-build-stale").unwrap();
    let root = tmp.path().join("hello_world");
    copy_dir(&example_001(), &root);
    let stale = tmp.path().join("libstale.a");
    std::fs::write(&stale, b"!<arch>\n").unwrap();
    let exe = tmp.path().join("hello_world.exe");
    let cache = fresh_cache();
    let out = cheby(
        &root,
        &["build", "-o", exe.to_str().unwrap()],
        &cache,
        &[("CHEBY_RT_ARCHIVE", stale.to_str().unwrap())],
    );
    assert_eq!(out.status, Some(1));
    assert!(
        out.stderr.contains(stale.to_str().unwrap()),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("other runtime sources"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("cheby_rt_id$"), "{}", out.stderr);
    assert!(
        out.stderr.contains("cargo build -p cheby_rt_exe"),
        "{}",
        out.stderr
    );
    assert!(!exe.exists());
}

/// A program that reaches a constant builds (D-924's refusal of constants
/// ended when constants reached executables as images, D-979): the
/// executable prints the constant as `cheby run` does (D-1025).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_constants_reach_executables() {
    let tmp = FreshDir::create("cheby-build-const").unwrap();
    let root = package(
        tmp.path(),
        "limits",
        "//! Prints a constant.\n\nimport std::io\n\nconst limit: Int = 3\n\nfn main() {\n  io::println(\"{limit}\")\n}\n",
    );
    let (build, exe) = built(&root, tmp.path(), "limits");
    let cache = fresh_cache();
    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert_eq!(ran.stdout, "3\n");
    assert_eq!(ran.status, Some(0), "{}", ran.stderr);
    let run = cheby(&root, &["run"], &cache, &[]);
    assert_same(&build, &ran, &run);
}

/// D-539, D-979: two constants that share a map share one collection in
/// an executable: the later constant's image refers to the earlier one's
/// map by path and index, so the program object stores the map once, and
/// installation gives both the one collection. The executable prints what
/// `cheby run` prints.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_constants_shared_map_is_one_collection() {
    let tmp = FreshDir::create("cheby-build-const-shared").unwrap();
    let root = package(
        tmp.path(),
        "shared",
        r#"//! Two constants that share a map.

import std::io
import std::map

const base: map::Map<String, Int> = map::from_list([("a", 1), ("b", 2)])

const pair: (map::Map<String, Int>, Int) = (base, 3)

fn main() {
  let (table, n) = pair
  let same = table == base
  let size = map::size(table)
  let b = map::get(table, "b")
  io::println("{same} {size} {n} {b:?}")
}
"#,
    );
    let (build, exe) = built(&root, tmp.path(), "shared");
    let bytes = std::fs::read(&exe).unwrap();
    let count = |needle: &[u8]| bytes.windows(needle.len()).filter(|w| *w == needle).count();
    // One image each, in D-535's order; the pair's refers to the base's
    // map (an earlier image's object: tag 3, the path, the index).
    assert_eq!(count(b"CHBV\x01\x0cshared::base"), 1);
    assert_eq!(count(b"CHBV\x01\x0cshared::pair"), 1);
    assert_eq!(count(b"\x03\x0cshared::base"), 1);
    let cache = fresh_cache();
    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert_eq!(ran.stdout, "True 2 3 Some(2)\n");
    assert_eq!(ran.status, Some(0), "{}", ran.stderr);
    let run = cheby(&root, &["run"], &cache, &[]);
    assert_same(&build, &ran, &run);
}

/// A constant of a user record that holds a std type with `@std_hooks`
/// (`sorted_map::SortedMap`; user code cannot write `@std_hooks`, §4.8)
/// prints, compares and hashes in an executable as under `cheby run`
/// (D-979, D-1025): installation registers the record's layout and
/// `SortedMap`'s hooks first, and builds the set of records with the hooks'
/// hashes (D-196).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_constants_user_record_with_std_hooks() {
    let tmp = FreshDir::create("cheby-build-const-hooks").unwrap();
    let root = package(
        tmp.path(),
        "tables",
        r#"//! A constant record that holds a sorted map, in a set.

import std::io
import std::set
import std::sorted_map

/// A named table.
type Table { Table { name: String, rows: sorted_map::SortedMap<Int, String> } }

const table: Table = Table { name: "t", rows: sorted_map::from_list([(2, "b"), (1, "a")]) }

const tables: set::Set<Table> = set::from_list([table, Table { name: "u", rows: sorted_map::new() }])

fn main() {
  io::println("{table:?}")
  let probe = Table { name: "t", rows: sorted_map::from_list([(1, "a"), (2, "b")]) }
  let same = table == probe
  let found = set::contains(tables, probe)
  let size = set::size(tables)
  io::println("{same} {found} {size}")
}
"#,
    );
    let (build, exe) = built(&root, tmp.path(), "tables");
    let cache = fresh_cache();
    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert_eq!(ran.status, Some(0), "{}", ran.stderr);
    assert!(ran.stdout.ends_with("True True 2\n"), "{}", ran.stdout);
    let run = cheby(&root, &["run"], &cache, &[]);
    assert_same(&build, &ran, &run);
}

/// A program that holds a list of two million integers while it waits
/// for a line: `big` is a constant when `constant`, else built in `main`.
/// It writes `ready` to standard error once the list exists.
fn waiting_program(constant: bool) -> String {
    let (top, local) = if constant {
        ("const big: List<Int> = list::range(0, 2000000)\n\n", "")
    } else {
        ("", "  let big = list::range(0, 2000000)\n")
    };
    format!(
        "//! Holds a large list while it waits for a line.\n\nimport std::io\nimport std::list\n\n\
         {top}fn main() {{\n{local}  let n = list::length(big)\n  io::eprintln(\"ready\")\n  \
         let _ = io::read_line()\n  let m = list::length(big)\n  io::println(\"{{n}} {{m}}\")\n}}\n"
    )
}

/// The resident memory of process `pid` in KiB: `VmRSS` on Linux, `ps`
/// elsewhere.
fn resident_kib(pid: u32) -> u64 {
    if cfg!(target_os = "linux") {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let line = status.lines().find(|l| l.starts_with("VmRSS:")).unwrap();
        line.split_whitespace().nth(1).unwrap().parse().unwrap()
    } else {
        let out = Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
}

/// Runs the executable `exe`, waits for its `ready`, and returns the
/// memory it holds then; then lets it finish and checks its output.
fn held_kib(exe: &Path) -> u64 {
    use std::io::{BufRead as _, Write as _};
    let mut child = Command::new(exe)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::BufReader::new(stderr).read_line(&mut line);
        let _ = sender.send(line);
    });
    let ready = receiver.recv_timeout(process::scaled_timeout(TIMEOUT).unwrap());
    let got_ready = ready.as_deref() == Ok("ready\n");
    if !got_ready {
        let _ = child.kill();
    }
    assert!(got_ready, "{} did not get ready: {ready:?}", exe.display());
    let held = resident_kib(child.id());
    child.stdin.take().unwrap().write_all(b"\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "2000000 2000000\n");
    held
}

/// The wave-6 review's R-1: an executable whose constant is a large list
/// holds about what the same list built at run time holds (D-979): the
/// installer builds the list straight from the program object's image,
/// with no decoded copy of every element and no copy of the table, so the
/// image's bytes, mapped from the file, are its only extra (about 1.7
/// times the run-time list's resident memory on macOS). Before the fix it
/// held 6 times as much; the bound of 3 leaves room for the platforms'
/// resident-memory accounting.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_constants_cost_about_their_value() {
    let tmp = FreshDir::create("cheby-build-const-memory").unwrap();
    let constant = package(tmp.path(), "constant", &waiting_program(true));
    let runtime = package(tmp.path(), "runtime", &waiting_program(false));
    let (_, constant) = built(&constant, tmp.path(), "constant");
    let (_, runtime) = built(&runtime, tmp.path(), "runtime");
    let (constant, runtime) = (held_kib(&constant), held_kib(&runtime));
    assert!(
        constant <= 3 * runtime,
        "the constant's program holds {constant} KiB, the run-time list's {runtime} KiB"
    );
}

/// The package of the user-type tests: a type with one constructor.
const SHAPES: &str = "//! Prints the area of a square.\n\nimport std::io\n\ntype Shape { Square(Int) }\n\nfn area(shape: Shape) -> Int {\n  case shape {\n    Square(side) => side * side\n  }\n}\n\nfn main() {\n  let size = area(Square(3))\n  io::println(\"{size}\")\n}\n";

/// A program whose code uses a user type builds (D-919, D-920): its
/// executable prints `9` and exits 0, as `cheby run` does (D-1025).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_user_types_run() {
    let tmp = FreshDir::create("cheby-build-types").unwrap();
    let root = package(tmp.path(), "shapes", SHAPES);
    let (build, exe) = built(&root, tmp.path(), "shapes");
    let cache = fresh_cache();
    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert_eq!(ran.stdout, "9\n");
    assert_eq!(ran.status, Some(0), "{}", ran.stderr);
    let run = cheby(&root, &["run"], &cache, &[]);
    assert_same(&build, &ran, &run);
}

/// D-934 with user types: the shapes package built from two copies at two
/// absolute paths, with two fresh caches, under one basename gives
/// byte-identical executables; the module records hold no layout id
/// (D-915, D-920).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_user_types_twice_identical() {
    let tmp = FreshDir::create("cheby-build-types-twice").unwrap();
    let a = package(&tmp.path().join("a"), "shapes", SHAPES);
    let b = package(&tmp.path().join("b/nested/deeper"), "shapes", SHAPES);
    std::fs::create_dir_all(tmp.path().join("out-a")).unwrap();
    std::fs::create_dir_all(tmp.path().join("out-b/x")).unwrap();
    let (_, exe_a) = built(&a, &tmp.path().join("out-a"), "shapes");
    let (_, exe_b) = built(&b, &tmp.path().join("out-b/x"), "shapes");
    let (x, y) = (std::fs::read(exe_a).unwrap(), std::fs::read(exe_b).unwrap());
    assert!(
        x == y,
        "the executables differ ({} and {} bytes)",
        x.len(),
        y.len()
    );
}

/// A type with a record constructor, a positional one and a nullary one
/// (its `cb$ctor$` static, D-920), debug printed (D-463), compared with
/// `==` and used as the key of a `std::map` (hashing by type key, D-559):
/// the executable prints what `cheby run` prints (D-1025).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_user_types_print_and_compare() {
    let tmp = FreshDir::create("cheby-build-types-print").unwrap();
    let root = package(
        tmp.path(),
        "places",
        r#"//! Prints, compares and looks up user types.

import std::io
import std::map

/// A place on a grid.
type Place { Point { x: Int, y: Int }, Cell(Int, String), Nowhere }

/// What `place` is called.
fn describe(place: Place) -> String {
  case place {
    Point { x, y } => "point {x},{y}"
    Cell(n, name) => "cell {n} {name}"
    Nowhere => "nowhere"
  }
}

fn main() {
  let a = Point { x: 1, y: 2 }
  let b = Cell(3, "c")
  let c = Nowhere
  io::println("{a:?} {b:?} {c:?}")
  io::println(describe(a))
  let same = a == Point { x: 1, y: 2 }
  let mixed = a == b
  let empty = c == Nowhere
  io::println("{same} {mixed} {empty}")
  let places = map::insert(map::insert(map::insert(map::new(), a, "a"), b, "b"), c, "c")
  let found = map::get(places, Point { x: 1, y: 2 })
  let none = map::get(places, Nowhere)
  let missing = map::get(places, Cell(4, "c"))
  let size = map::size(places)
  io::println("{found:?} {none:?} {missing:?} {size}")
}
"#,
    );
    let (build, exe) = built(&root, tmp.path(), "places");
    let cache = fresh_cache();
    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert_eq!(
        ran.stdout,
        "Point { x: 1, y: 2 } Cell(3, \"c\") Nowhere\npoint 1,2\nTrue False True\nSome(\"a\") Some(\"c\") None 3\n"
    );
    assert_eq!(ran.status, Some(0), "{}", ran.stderr);
    let run = cheby(&root, &["run"], &cache, &[]);
    assert_same(&build, &ran, &run);
}

/// D-934: 001 built from two copies at two absolute paths, with two fresh
/// caches, under one basename gives byte-identical executables.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_twice_identical() {
    let tmp = FreshDir::create("cheby-build-twice").unwrap();
    let a = tmp.path().join("a/hello_world");
    let b = tmp.path().join("b/nested/deeper/hello_world");
    copy_dir(&example_001(), &a);
    copy_dir(&example_001(), &b);
    std::fs::create_dir_all(tmp.path().join("out-a")).unwrap();
    std::fs::create_dir_all(tmp.path().join("out-b/x")).unwrap();
    let (_, exe_a) = built(&a, &tmp.path().join("out-a"), "hello_world");
    let (_, exe_b) = built(&b, &tmp.path().join("out-b/x"), "hello_world");
    let (x, y) = (std::fs::read(exe_a).unwrap(), std::fs::read(exe_b).unwrap());
    assert!(
        x == y,
        "the executables differ ({} and {} bytes)",
        x.len(),
        y.len()
    );
}

/// D-934 on a warm cache: 001 built, then built again on the same cache,
/// whose front records hit, gives the same executable.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_warm_identical() {
    let tmp = FreshDir::create("cheby-build-warm").unwrap();
    let root = tmp.path().join("hello_world");
    copy_dir(&example_001(), &root);
    let cache = fresh_cache();
    let cold = tmp.path().join("cold/hello_world");
    let warm = tmp.path().join("warm/hello_world");
    let log = tmp.path().join("warm.log");
    for exe in [&cold, &warm] {
        let out = cheby(
            &root,
            &["build", "-o", exe.to_str().unwrap()],
            &cache,
            &[("CHEBY_CACHE_LOG", log.to_str().unwrap())],
        );
        assert_eq!(out.status, Some(0), "{}", out.stderr);
    }
    // The second build's lookup log: its package module hits.
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.lines().any(|l| l == "front hello_world hit"), "{text}");
    let (x, y) = (std::fs::read(cold).unwrap(), std::fs::read(warm).unwrap());
    assert!(
        x == y,
        "the executables differ ({} and {} bytes)",
        x.len(),
        y.len()
    );
}

/// D-934's basename: a build under another basename differs from the
/// first only in `LC_UUID` and the code signature on macOS (ld64 derives
/// both from the output's name); on Linux the bytes are the same. The
/// signature holds the name, so a longer name gives a longer signature:
/// its size, in `LC_CODE_SIGNATURE` and in `__LINKEDIT`'s segment
/// command, may differ too, and everything before the signature is
/// compared.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_other_basename() {
    let tmp = FreshDir::create("cheby-build-basename").unwrap();
    let root = tmp.path().join("hello_world");
    copy_dir(&example_001(), &root);
    let (_, first) = built(&root, tmp.path(), "hello_world");
    let (_, other) = built(&root, tmp.path(), "greeting_program");
    let (x, y) = (std::fs::read(first).unwrap(), std::fs::read(other).unwrap());
    if cfg!(target_os = "macos") {
        let (allowed, signature) = macho_name_dependent(&x);
        assert_eq!(
            (allowed.clone(), signature),
            macho_name_dependent(&y),
            "the ranges moved"
        );
        assert!(x != y, "the basename changed nothing");
        for at in (0..signature).filter(|&i| x[i] != y[i]) {
            assert!(
                allowed.iter().any(|r| r.contains(&at)),
                "byte {at} differs outside LC_UUID and the code signature"
            );
        }
    } else {
        assert!(
            x == y,
            "the executables differ ({} and {} bytes)",
            x.len(),
            y.len()
        );
    }
}

/// The bytes of a Mach-O executable that ld64 derives from the output's
/// name, read through the load commands with `object`: the byte ranges of
/// `LC_UUID`'s 16 bytes, `LC_CODE_SIGNATURE`'s `datasize` and
/// `__LINKEDIT`'s `vmsize` and `filesize` (the signature's size), and
/// where the signature's data starts (it runs to the end of the file).
fn macho_name_dependent(bytes: &[u8]) -> (Vec<std::ops::Range<usize>>, usize) {
    use object::macho::{self, MachHeader64};
    use object::read::macho::MachHeader as _;
    let endian = object::Endianness::Little;
    let header = MachHeader64::<object::Endianness>::parse(bytes, 0).unwrap();
    let mut commands = header.load_commands(endian, bytes, 0).unwrap();
    let mut ranges = Vec::new();
    let mut signature = None;
    let mut offset = std::mem::size_of::<macho::MachHeader64<object::Endianness>>();
    while let Some(command) = commands.next().unwrap() {
        match command.cmd() {
            macho::LC_UUID => ranges.push(offset + 8..offset + 24),
            macho::LC_CODE_SIGNATURE => {
                let data = command.data::<macho::LinkeditDataCommand<object::Endianness>>();
                let data = data.unwrap();
                signature = Some(usize::try_from(data.dataoff.get(endian)).unwrap());
                ranges.push(offset + 12..offset + 16);
            }
            macho::LC_SEGMENT_64 => {
                let segment = command.data::<macho::SegmentCommand64<object::Endianness>>();
                if segment.unwrap().segname.starts_with(b"__LINKEDIT\0") {
                    // `vmsize` and `filesize` of `segment_command_64`.
                    ranges.push(offset + 32..offset + 40);
                    ranges.push(offset + 48..offset + 56);
                }
            }
            _ => {}
        }
        offset += usize::try_from(command.cmdsize()).unwrap();
    }
    assert_eq!(ranges.len(), 4, "LC_UUID, LC_CODE_SIGNATURE and __LINKEDIT");
    (ranges, signature.unwrap())
}

/// The lines of a `CHEBY_CACHE_LOG` file that start with `prefix` and a
/// space.
fn log_lines(log: &Path, prefix: &str) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.strip_prefix(prefix).is_some_and(|r| r.starts_with(' ')))
        .map(str::to_owned)
        .collect()
}

/// `cheby <args>` in `root` on `cache`, logging to a fresh `log`.
fn logged(root: &Path, args: &[&str], cache: &HarnessCache, log: &Path) -> RunOutput {
    let _ = std::fs::remove_file(log);
    cheby(
        root,
        args,
        cache,
        &[("CHEBY_CACHE_LOG", log.to_str().unwrap())],
    )
}

/// A package `app` with these files under `src/`.
fn package_files(dir: &Path, manifest_extra: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = dir.join("app");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("cheby.toml"),
        format!("name = \"app\"\nversion = \"0.1.0\"\n{manifest_extra}"),
    )
    .unwrap();
    for (path, text) in files {
        std::fs::write(root.join("src").join(path), text).unwrap();
    }
    root
}

/// The root of [`three_modules`]: two other modules, one with a constant.
const THREE_MAIN: &str = "//! The root: what the two other modules give.

import app::count
import app::label
import std::io

fn main() {
  let n = count::total()
  let limit = count::limit
  io::println(label::name())
  io::println(\"total: {n}\")
  io::println(\"limit: {limit}\")
}
";

/// A module with a function and a constant; `{step}` is the body edit.
fn three_count(step: u32) -> String {
    format!(
        "//! Counting.

/// The largest count.
pub const limit: Int = steps(10, 0)

/// The total.
pub fn total() -> Int {{
  steps(4, 0) + {step}
}}

fn steps(n: Int, acc: Int) -> Int {{
  case n {{
    0 => acc
    _ => steps(n - 1, acc + n)
  }}
}}
"
    )
}

/// A module with a string; `{name}` is the body edit.
fn three_label(name: &str) -> String {
    format!(
        "//! A label.

/// The program's name.
pub fn name() -> String {{
  \"{name}\"
}}
"
    )
}

/// A three-module package with a constant.
fn three_modules(dir: &Path) -> PathBuf {
    package_files(
        dir,
        "",
        &[
            ("main.cheby", THREE_MAIN),
            ("count.cheby", &three_count(1)),
            ("label.cheby", &three_label("counter")),
        ],
    )
}

/// N9 (D-1250, D-914, D-1262, D-1264): after a one-body edit in a
/// three-module package, a second `cheby build` parses only the edited
/// module and misses only its `run` code record; it neither parses std nor
/// misses a std record, evaluates nothing (`const … hit`: the constant's
/// module set holds `count` only, D-1259, and `label` is edited), and its
/// executable prints the edit.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_cache_one_body_edit_rebuilds_one_module() {
    let tmp = FreshDir::create("cheby-build-cache-edit").unwrap();
    let root = three_modules(tmp.path());
    let cache = fresh_cache();
    let exe = tmp.path().join("exe/app");
    let out = exe.to_str().unwrap();
    let log = tmp.path().join("build.log");
    let cold = logged(&root, &["build", "-o", out], &cache, &log);
    assert_eq!(cold.status, Some(0), "{}", cold.stderr);
    std::fs::write(root.join("src/label.cheby"), three_label("counted")).unwrap();
    let warm = logged(&root, &["build", "-o", out], &cache, &log);
    assert_eq!(warm.status, Some(0), "{}", warm.stderr);
    assert_eq!(log_lines(&log, "parse"), ["parse app::label"]);
    let code: Vec<String> = log_lines(&log, "code.run");
    assert_eq!(code.len(), 3, "{code:?}");
    let misses: Vec<&String> = code.iter().filter(|l| l.ends_with(" miss")).collect();
    assert_eq!(misses, ["code.run app::label miss"]);
    assert_eq!(log_lines(&log, "parse.std"), Vec::<String>::new());
    let std = log_lines(&log, "code.std");
    assert_ne!(std, Vec::<String>::new());
    assert!(std.iter().all(|l| l.ends_with(" hit")), "{std:?}");
    assert_eq!(log_lines(&log, "const"), ["const app::count::limit hit"]);
    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert_eq!(ran.stdout, "counted\ntotal: 11\nlimit: 55\n");
}

/// `cheby build` after `cheby run` misses no code record, std's included,
/// and evaluates nothing; its executable equals, byte for byte under one
/// basename, a cold build's on a fresh cache (D-914, D-934); and `cheby
/// run` after `cheby build` misses nothing either.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_cache_after_run_reads_runs_records() {
    let tmp = FreshDir::create("cheby-build-cache-run").unwrap();
    let root = three_modules(tmp.path());
    let cache = fresh_cache();
    let log = tmp.path().join("cache.log");
    let run = logged(&root, &["run"], &cache, &log);
    assert_eq!(run.status, Some(0), "{}", run.stderr);
    let warm_exe = tmp.path().join("warm/app");
    let warm = logged(
        &root,
        &["build", "-o", warm_exe.to_str().unwrap()],
        &cache,
        &log,
    );
    assert_eq!(warm.status, Some(0), "{}", warm.stderr);
    for prefix in ["code.run", "code.std", "const"] {
        let lines = log_lines(&log, prefix);
        assert!(!lines.is_empty(), "{prefix}");
        assert!(lines.iter().all(|l| l.ends_with(" hit")), "{lines:?}");
    }
    assert_eq!(log_lines(&log, "code.plain"), Vec::<String>::new());
    let cold_exe = tmp.path().join("cold/app");
    let cold = build(&root, &cold_exe);
    assert_eq!(cold.status, Some(0), "{}", cold.stderr);
    let (x, y) = (
        std::fs::read(&warm_exe).unwrap(),
        std::fs::read(&cold_exe).unwrap(),
    );
    assert!(
        x == y,
        "the executables differ ({} and {} bytes)",
        x.len(),
        y.len()
    );
    let ran = process::run(Command::new(&warm_exe), None, TIMEOUT).unwrap();
    assert_same(&warm, &ran, &run);
    let back = fresh_cache();
    let built_first = build_on(&root, &tmp.path().join("first/app"), &back);
    assert_eq!(built_first.status, Some(0), "{}", built_first.stderr);
    let again = logged(&root, &["run"], &back, &log);
    assert_eq!(again.status, Some(0), "{}", again.stderr);
    for prefix in ["code.run", "code.std", "const"] {
        let lines = log_lines(&log, prefix);
        assert!(!lines.is_empty(), "{prefix}");
        assert!(lines.iter().all(|l| l.ends_with(" hit")), "{lines:?}");
    }
}

/// `cheby build -o <exe>` of `root` on `cache`.
fn build_on(root: &Path, exe: &Path, cache: &HarnessCache) -> RunOutput {
    cheby(root, &["build", "-o", exe.to_str().unwrap()], cache, &[])
}

/// Under the runtime's poisoning mode (`CHEBY_RT_POISON=1`, as every child
/// of the harnesses runs; a debug toolchain's checked count operations,
/// D-565), `cheby build` compiles as `cheby run`'s JIT does: a cold build
/// links (its program object's trampolines equal the std records', D-918),
/// a build after `cheby run` reads its checked records and misses none, and
/// both executables print what `cheby run` prints.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_cache_poisoned_build_reads_runs_records() {
    let tmp = FreshDir::create("cheby-build-cache-poison").unwrap();
    let root = copy_of_001(tmp.path());
    let poison = [("CHEBY_RT_POISON", "1")];
    let cold_cache = fresh_cache();
    let cold_exe = tmp.path().join("cold/hello_world");
    let cold = cheby(
        &root,
        &["build", "-o", cold_exe.to_str().unwrap()],
        &cold_cache,
        &poison,
    );
    assert_eq!(cold.status, Some(0), "{}", cold.stderr);
    let cache = fresh_cache();
    let log = tmp.path().join("poison.log");
    let run = cheby(&root, &["run"], &cache, &poison);
    assert_eq!(run.status, Some(0), "{}", run.stderr);
    let warm_exe = tmp.path().join("warm/hello_world");
    let warm = cheby(
        &root,
        &["build", "-o", warm_exe.to_str().unwrap()],
        &cache,
        &[
            ("CHEBY_RT_POISON", "1"),
            ("CHEBY_CACHE_LOG", log.to_str().unwrap()),
        ],
    );
    assert_eq!(warm.status, Some(0), "{}", warm.stderr);
    let code = log_lines(&log, "code.run");
    assert!(code.iter().all(|l| l.ends_with(" hit")), "{code:?}");
    for exe in [&cold_exe, &warm_exe] {
        let ran = process::run(Command::new(exe), None, TIMEOUT).unwrap();
        assert_same(&warm, &ran, &run);
    }
}

/// A copy of 001 under `dir`.
fn copy_of_001(dir: &Path) -> PathBuf {
    let root = dir.join("hello_world");
    copy_dir(&example_001(), &root);
    root
}

/// D-942 and D-943 on a warm cache: a `[native]` package whose one
/// unreached function names a symbol nothing defines. After `cheby run`,
/// a `cheby build` takes the C object from the cache (`cobj … hit`) and
/// std's code from its records, lowers the module that names the missing
/// symbol again as `cheby run` does, the hidden stub still binds the
/// unreached symbol, and the executable links and prints what `cheby run`
/// prints.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_cache_native_objects_from_the_cache() {
    let tmp = FreshDir::create("cheby-build-cache-native").unwrap();
    let root = package_files(
        tmp.path(),
        "targets = [\"native\"]\n\n[native]\nsources = [\"src/twice.c\"]\n",
        &[
            (
                "twice.c",
                "#include <stdint.h>\n\nint64_t app_twice(int64_t n) { return n * 2; }\n",
            ),
            (
                "main.cheby",
                "//! Doubling in C, and a binding nothing calls.

import std::io

@external(native, \"app_twice\")
fn twice(n: Int) -> Int

/// A binding nothing defines and nothing calls (D-943).
@external(native, \"app_missing_unreached\")
pub fn missing() -> Int

fn main() {
  let n = twice(21)
  io::println(\"{n}\")
}
",
            ),
        ],
    );
    let cache = fresh_cache();
    let log = tmp.path().join("cache.log");
    let run = logged(&root, &["run"], &cache, &log);
    assert_eq!(run.status, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "42\n");
    let exe = tmp.path().join("exe/app");
    let build = logged(&root, &["build", "-o", exe.to_str().unwrap()], &cache, &log);
    assert_eq!(build.status, Some(0), "{}", build.stderr);
    let cobj = log_lines(&log, "cobj");
    assert!(!cobj.is_empty(), "no C object lookup");
    assert!(cobj.iter().all(|l| l.ends_with(" hit")), "{cobj:?}");
    // `app`'s record imports the symbol nothing defines, so both commands
    // lower `app` again to decide E0053 on its MIR (D-943): its lookup is
    // a miss under `cheby run` and `cheby build` alike, and std's hit.
    assert_eq!(log_lines(&log, "code.run"), ["code.run app miss"]);
    let std = log_lines(&log, "code.std");
    assert!(std.iter().all(|l| l.ends_with(" hit")), "{std:?}");
    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert_same(&build, &ran, &run);
}

/// The names of the generated functions an executable keeps (`cb$…` text
/// symbols, without Mach-O's underscore).
fn kept_functions(exe: &Path) -> Vec<String> {
    use object::read::{Object as _, ObjectSymbol as _};
    let bytes = std::fs::read(exe).unwrap();
    let file = object::File::parse(&*bytes).unwrap();
    let mut out: Vec<String> = file
        .symbols()
        .filter(|s| s.kind() == object::SymbolKind::Text)
        .filter_map(|s| s.name().ok())
        .map(|n| n.strip_prefix('_').unwrap_or(n).to_owned())
        .filter(|n| n.starts_with("cb$"))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The module with a kept function between two that nothing calls.
const GC_UTIL: &str = "//! Checks, and two functions nothing calls.

/// Nothing calls this.
pub fn before(n: Int) -> Int {
  n + 1
}

/// Fails when `n` is not positive.
pub fn check(n: Int) -> Int {
  assert n > 0
  n
}

/// Nothing calls this either.
pub fn after(n: Int) -> Int {
  n * 3
}
";

/// D-1253: the linker drops the functions of a whole module that nothing
/// reaches, with their record entries: the executable keeps `check` and
/// drops `before` and `after`, while `cheby run` loads all three; the
/// call-site table of the kept function still locates its frame, so a
/// failed assertion in it reports its location as `cheby run` does.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn link_gc_drops_unreached_functions_and_keeps_locations() {
    let tmp = FreshDir::create("cheby-build-link-gc").unwrap();
    let root = package_files(
        tmp.path(),
        "",
        &[
            (
                "main.cheby",
                "//! Calls one function of `util`.

import app::util
import std::io

fn main() {
  io::println(\"start\")
  let _ = util::check(0)
  Nil
}
",
            ),
            ("util.cheby", GC_UTIL),
        ],
    );
    let (build, exe) = built(&root, tmp.path(), "app");
    let kept = kept_functions(&exe);
    let has = |item: &str| kept.iter().any(|n| n.ends_with(&format!("$util${item}")));
    assert!(has("check"), "{kept:?}");
    assert!(!has("before"), "{kept:?}");
    assert!(!has("after"), "{kept:?}");
    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert_eq!(ran.status, Some(101), "{}", ran.stderr);
    assert!(ran.stderr.contains("src/util.cheby:"), "{}", ran.stderr);
    let cache = fresh_cache();
    let run = cheby(&root, &["run"], &cache, &[]);
    assert_same(&build, &ran, &run);
}
