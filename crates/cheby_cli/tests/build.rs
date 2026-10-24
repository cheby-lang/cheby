//! `cheby build` (D-924, D-937): executables of small packages that reach
//! no constant and register no user type, run and compared with `cheby
//! run` of the same package (D-925, D-926, D-474, D-306); the runtime
//! archive's tool errors (D-929, D-930); the "not available yet" errors
//! (D-924, GAP(aot-layouts-not-yet)); and determinism (D-934).
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

/// A program that reaches a constant is refused before anything links,
/// until constants reach executables (D-924).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_not_yet_constants() {
    let tmp = FreshDir::create("cheby-build-const").unwrap();
    let root = package(
        tmp.path(),
        "limits",
        "//! Prints a constant.\n\nimport std::io\n\nconst limit: Int = 3\n\nfn main() {\n  io::println(\"{limit}\")\n}\n",
    );
    let exe = tmp.path().join("exe/limits");
    let out = build(&root, &exe);
    assert_eq!(out.status, Some(1));
    assert_eq!(
        out.stderr,
        "error: `cheby build` is not available yet for programs that use constants, such as \
         `limits::limit` (planned for a later step of M3)\n"
    );
    assert!(!exe.exists());
}

/// A program whose code uses a user type is refused before anything
/// links, until module records carry layouts (GAP(aot-layouts-not-yet)).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_not_yet_user_types() {
    let tmp = FreshDir::create("cheby-build-types").unwrap();
    let root = package(
        tmp.path(),
        "shapes",
        "//! Prints the area of a square.\n\nimport std::io\n\ntype Shape {\n  Square(Int)\n}\n\nfn area(shape: Shape) -> Int {\n  case shape {\n    Square(side) => side * side\n  }\n}\n\nfn main() {\n  let size = area(Square(3))\n  io::println(\"{size}\")\n}\n",
    );
    let exe = tmp.path().join("exe/shapes");
    let out = build(&root, &exe);
    assert_eq!(out.status, Some(1), "{}", out.stderr);
    assert!(
        out.stderr.starts_with(
            "error: `cheby build` is not available yet for programs that use user-defined types"
        ),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.ends_with("(planned for a later step of M3)\n"),
        "{}",
        out.stderr
    );
    assert!(!exe.exists());
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
