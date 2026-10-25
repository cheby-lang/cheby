//! A package's native code (D-940 to D-944) through the `cheby` binary: a
//! package with one C function runs the same under `cheby run`, `cheby
//! test`, the REPL and as an executable; two builds from two package paths
//! under one basename are identical (D-934, D-942); `cheby check` compiles
//! no C (D-532, D-941); the tool errors of a failed compile and of a
//! missing `cc` (plan §7 Q6 (4)).
//!
//! The tests that build executables link against the runtime archive next
//! to `cheby` (`cargo build -p cheby_rt_exe` first, D-929). Every `cheby`
//! gets a fresh `CHEBY_CACHE` of its own (D-1128), and every build writes
//! with `-o` into a temporary directory.

#![expect(
    clippy::unwrap_used,
    reason = "test helpers: a failed setup step fails the test"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::{FreshDir, HarnessCache};
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(120);

/// The package's manifest: native only, one C source.
const MANIFEST: &str = "name = \"app\"\nversion = \"0.1.0\"\ntargets = [\"native\"]\n\n\
                        [native]\nsources = [\"src/twice.c\"]\n";

/// A C function that uses no `cheby.h` function.
const TWICE_C: &str = "#include <stdint.h>\n#include <cheby.h>\n\n\
                       int64_t app_twice(int64_t n) { return n * 2; }\n";

/// The module that binds the C function.
const DOUBLING: &str = "//! Doubling in C.\n\n\
                        /// Twice `n`, computed by the package's C code.\n\
                        @external(native, \"app_twice\")\npub fn twice(n: Int) -> Int\n";

/// The package's root module: `main` and a test call the C function.
const MAIN: &str = "//! Doubles a number in C.\n\nimport app::doubling\nimport std::io\n\n\
                    fn main() {\n  let n = doubling::twice(21)\n  io::println(\"twice: {n}\")\n}\n\n\
                    test \"twice doubles\" {\n  assert doubling::twice(21) == 42\n}\n";

/// Writes the package `files` into `dir/name`.
fn package(dir: &Path, name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = dir.join(name);
    for (path, text) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    root
}

/// The doubling package in `dir/name`.
fn twice_package(dir: &Path, name: &str) -> PathBuf {
    package(
        dir,
        name,
        &[
            ("cheby.toml", MANIFEST),
            ("src/twice.c", TWICE_C),
            ("src/doubling.cheby", DOUBLING),
            ("src/main.cheby", MAIN),
        ],
    )
}

/// A fresh build cache (D-1128).
fn fresh_cache() -> HarnessCache {
    HarnessCache::create_with("cli-native", None).unwrap()
}

/// `cheby <args>` in `dir` with a fresh cache, the extra environment `env`
/// and standard input `stdin`.
fn cheby(dir: &Path, args: &[&str], env: &[(&str, &str)], stdin: Option<&[u8]>) -> RunOutput {
    let cache = fresh_cache();
    let mut cmd = Command::new(CHEBY);
    cmd.args(args).current_dir(dir).env("NO_COLOR", "1");
    cache.apply(&mut cmd);
    for (key, value) in env {
        cmd.env(key, value);
    }
    process::run(cmd, stdin, TIMEOUT).unwrap()
}

/// D-941: the C function runs under `cheby run`, `cheby test` and the REPL,
/// each loading the package's shared library.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn native_pkg_runs_under_run_test_and_the_repl() {
    let tmp = FreshDir::create("cheby-native-jit").unwrap();
    let root = twice_package(tmp.path(), "app");
    let run = cheby(&root, &["run"], &[], None);
    assert_eq!(run.stderr, "");
    assert_eq!(run.stdout, "twice: 42\n");
    assert_eq!(run.status, Some(0));
    let test = cheby(&root, &["test"], &[], None);
    assert_eq!(test.status, Some(0), "{}{}", test.stdout, test.stderr);
    assert!(test.stdout.contains("twice doubles"), "{}", test.stdout);
    let repl = cheby(
        &root,
        &["--color=never", "repl"],
        &[],
        Some(b"import app::doubling\ndoubling::twice(21)\n"),
    );
    assert_eq!(repl.status, Some(0), "{}{}", repl.stdout, repl.stderr);
    assert!(repl.stdout.contains("42 : Int"), "{}", repl.stdout);
    // Nothing is written into the package.
    assert!(!root.join("build").exists());
}

/// D-940, D-942, D-934: the executable links the same C object and prints
/// what `cheby run` prints; two builds from two package paths under one
/// basename are byte for byte identical.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn native_pkg_executable() {
    let tmp = FreshDir::create("cheby-native-exe").unwrap();
    let first = twice_package(&tmp.path().join("one"), "app");
    let second = twice_package(&tmp.path().join("two/longer"), "app");
    let mut exes = Vec::new();
    for (root, dir) in [(&first, "exe-a"), (&second, "exe-b")] {
        let exe = tmp.path().join(dir).join("app");
        let build = cheby(root, &["build", "-o", exe.to_str().unwrap()], &[], None);
        assert_eq!(build.status, Some(0), "{}", build.stderr);
        assert_eq!(build.stdout, "");
        let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
        assert_eq!(ran.stdout, "twice: 42\n");
        assert_eq!(ran.status, Some(0));
        exes.push(std::fs::read(&exe).unwrap());
    }
    assert!(exes[0] == exes[1], "the two executables differ");
}

/// D-941: a package that names only `libraries` gets a library with the
/// forwarder alone, linked with them: libm's `cbrt` under `cheby run` and
/// in an executable.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn native_pkg_libraries_only() {
    let tmp = FreshDir::create("cheby-native-libs").unwrap();
    let root = package(
        tmp.path(),
        "cube",
        &[
            (
                "cheby.toml",
                "name = \"cube\"\nversion = \"0.1.0\"\ntargets = [\"native\"]\n\n\
                 [native]\nlibraries = [\"m\"]\n",
            ),
            (
                "src/main.cheby",
                "//! A cube root from libm.\n\nimport std::io\n\n\
                 @external(native, \"cbrt\")\nfn cube_root(x: Float) -> Float\n\n\
                 fn main() {\n  let root = cube_root(27.0)\n  \
                 let close = root > 2.999 && root < 3.001\n  \
                 io::println(\"close: {close}\")\n}\n",
            ),
        ],
    );
    let run = cheby(&root, &["run"], &[], None);
    assert_eq!(run.stderr, "");
    assert_eq!(run.stdout, "close: True\n");
    let exe = tmp.path().join("exe").join("cube");
    let build = cheby(&root, &["build", "-o", exe.to_str().unwrap()], &[], None);
    assert_eq!(build.status, Some(0), "{}", build.stderr);
    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert_eq!(ran.stdout, "close: True\n");
}

/// D-943: one package with a reached and an unreached missing symbol gives
/// byte-identical output under `cheby run` and `cheby build`: E0053 for the
/// reached one only, decided before anything runs or links, the symbol its
/// C code defines found in its shared library.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn native_pkg_missing_symbols_same_under_run_and_build() {
    let tmp = FreshDir::create("cheby-native-missing").unwrap();
    let root = package(
        tmp.path(),
        "app",
        &[
            (
                "cheby.toml",
                "name = \"app\"\nversion = \"0.1.0\"\ntargets = [\"native\"]\n\n\
                 [native]\nsources = [\"src/present.c\"]\n",
            ),
            (
                "src/present.c",
                "#include <stdint.h>\n\nint64_t app_present(void) { return 7; }\n",
            ),
            (
                "src/main.cheby",
                "//! Missing symbols.\n\nimport std::io\n\n\
                 @external(native, \"app_present\")\nfn present() -> Int\n\n\
                 @external(native, \"app_missing_reached\")\nfn missing() -> Int\n\n\
                 @external(native, \"app_missing_unreached\")\npub fn unused() -> Int\n\n\
                 fn main() {\n  let a = present()\n  let b = missing()\n  \
                 io::println(\"{a} {b}\")\n}\n",
            ),
        ],
    );
    let run = cheby(&root, &["run"], &[], None);
    let exe = tmp.path().join("exe").join("app");
    let build = cheby(&root, &["build", "-o", exe.to_str().unwrap()], &[], None);
    assert!(
        run.stderr
            .starts_with("error[E0053]: the native symbol `app_missing_reached` is not defined"),
        "{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("app_missing_unreached"),
        "{}",
        run.stderr
    );
    assert_eq!(build.stderr, run.stderr);
    assert_eq!((build.stdout.as_str(), run.stdout.as_str()), ("", ""));
    assert_eq!((build.status, run.status), (Some(1), Some(1)));
    assert!(!exe.exists());
}

/// D-532, D-941: `cheby check` validates the table and compiles nothing,
/// so a C source that does not compile passes it; `cheby run` compiles it
/// and reports the failure as a tool error that names the source, with the
/// compiler's output (D-942, plan §7 Q6 (4)).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn native_cc_check_compiles_nothing() {
    let tmp = FreshDir::create("cheby-native-check").unwrap();
    let root = package(
        tmp.path(),
        "app",
        &[
            ("cheby.toml", MANIFEST),
            ("src/twice.c", "int64_t app_twice( {\n"),
            ("src/doubling.cheby", DOUBLING),
            ("src/main.cheby", MAIN),
        ],
    );
    let check = cheby(&root, &["check"], &[], None);
    assert_eq!(check.stderr, "");
    assert_eq!(check.status, Some(0));
    let run = cheby(&root, &["run"], &[], None);
    assert_eq!(run.stdout, "");
    assert!(
        run.stderr
            .starts_with("error: compiling `src/twice.c` with `cc` failed (")
            && run.stderr.contains("twice.c"),
        "{}",
        run.stderr
    );
    assert_eq!(run.status, Some(1));
}

/// Plan §7 Q6 (4): without `cc` in `PATH`, `cheby run` stops with a tool
/// error before anything runs.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn native_cc_missing_cc_is_a_tool_error() {
    let tmp = FreshDir::create("cheby-native-no-cc").unwrap();
    let root = twice_package(tmp.path(), "app");
    let empty = tmp.path().join("empty-path");
    std::fs::create_dir_all(&empty).unwrap();
    let run = cheby(&root, &["run"], &[("PATH", empty.to_str().unwrap())], None);
    assert_eq!(run.stdout, "");
    assert!(
        run.stderr.starts_with(
            "error: `[native]` needs a C compiler, but `cc` could not be run from PATH: "
        ),
        "{}",
        run.stderr
    );
    assert_eq!(run.status, Some(1));
}
