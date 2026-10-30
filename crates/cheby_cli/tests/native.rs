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

/// C code that builds and reads Cheby values through `cheby.h` (D-945,
/// D-187): strings (invalid UTF-8 replaced, D-311), lists, options,
/// results, and one write through the runtime's stream (D-951).
const VALUES_C: &str = r#"#include <cheby.h>

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

cheby_value *app_greet(cheby_value *name) {
    size_t len = 0;
    const char *bytes = cheby_ffi_string_bytes(name, &len);
    char *text = malloc(len + 7);
    if (text == NULL) {
        return NULL;
    }
    memcpy(text, "hello, ", 7);
    memcpy(text + 7, bytes, len);
    cheby_value *out = cheby_ffi_string_new(text, len + 7);
    free(text);
    return out;
}

cheby_value *app_numbers(int64_t n) {
    cheby_value *items[16];
    size_t count = n < 0 ? 0 : n > 16 ? 16 : (size_t)n;
    for (size_t i = 0; i < count; i++) {
        items[i] = cheby_ffi_int_new((int64_t)i + 1);
    }
    return cheby_ffi_list_new(items, count);
}

cheby_value *app_first(cheby_value *items) {
    if (cheby_ffi_list_length(items) == 0) {
        return cheby_ffi_none();
    }
    return cheby_ffi_some(cheby_ffi_dup(cheby_ffi_list_get(items, 0)));
}

cheby_value *app_parse(cheby_value *text) {
    size_t len = 0;
    const char *bytes = cheby_ffi_string_bytes(text, &len);
    int64_t n = 0;
    for (size_t i = 0; i < len; i++) {
        if (bytes[i] < '0' || bytes[i] > '9') {
            static const char message[] = "not a number";
            return cheby_ffi_err(cheby_ffi_string_new(message, sizeof message - 1));
        }
        n = n * 10 + (bytes[i] - '0');
    }
    return cheby_ffi_ok(cheby_ffi_int_new(n));
}

cheby_value *app_invalid(void) {
    return cheby_ffi_string_new("a\xff" "b", 3);
}

void app_say(cheby_value *line) {
    size_t len = 0;
    const char *bytes = cheby_ffi_string_bytes(line, &len);
    cheby_ffi_write_stdout(bytes, len);
}
"#;

/// The module that binds [`VALUES_C`]: `main` prints what C builds, and C's
/// output comes between two `io::println` lines, also in a test.
const VALUES_MAIN: &str = r#"//! Cheby values built and read by C code.

import std::io

@external(native, "app_greet")
fn greet(name: String) -> String

@external(native, "app_numbers")
fn numbers(n: Int) -> List<Int>

@external(native, "app_first")
fn first(items: List<String>) -> Option<String>

@external(native, "app_parse")
fn parse(text: String) -> Result<Int, String>

@external(native, "app_invalid")
fn invalid() -> String

@external(native, "app_say")
fn say(line: String)

fn main() {
  let hello = greet("C")
  io::println(hello)
  let listed = numbers(3)
  io::println("{listed:?}")
  let some = first(["a", "b"])
  let none = first([])
  io::println("{some:?} {none:?}")
  let good = parse("42")
  let bad = parse("4x")
  io::println("{good:?} {bad:?}")
  let replaced = invalid()
  io::println(replaced)
  io::println("one")
  say("two\n")
  io::println("three")
}

test "C output is captured in order" {
  io::println("one")
  say("two\n")
  io::println("three")
  assert False
}
"#;

/// What [`VALUES_MAIN`]'s `main` prints.
const VALUES_OUT: &str = "hello, C\n[1, 2, 3]\nSome(\"a\") None\nOk(42) Err(\"not a number\")\n\
                          a\u{fffd}b\none\ntwo\nthree\n";

/// The values package in `dir/app`.
fn values_package(dir: &Path) -> PathBuf {
    package(
        dir,
        "app",
        &[
            (
                "cheby.toml",
                "name = \"app\"\nversion = \"0.1.0\"\ntargets = [\"native\"]\n\n\
                 [native]\nsources = [\"src/values.c\"]\n",
            ),
            ("src/values.c", VALUES_C),
            ("src/main.cheby", VALUES_MAIN),
        ],
    )
}

/// D-945, D-187, D-311: C code builds and reads strings, lists, options
/// and results through `cheby.h`'s functions, the same under `cheby run`
/// (through the forwarder's table, D-941) and in an executable (the
/// runtime archive's definitions), and nothing leaks (the debug leak
/// check, D-710).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn native_ffi_values_round_trip() {
    let tmp = FreshDir::create("cheby-native-ffi-values").unwrap();
    let root = values_package(tmp.path());
    let run = cheby(&root, &["run"], &[], None);
    assert_eq!(run.stderr, "");
    assert_eq!(run.stdout, VALUES_OUT);
    assert_eq!(run.status, Some(0));
    let exe = tmp.path().join("exe").join("app");
    let build = cheby(&root, &["build", "-o", exe.to_str().unwrap()], &[], None);
    assert_eq!(build.status, Some(0), "{}", build.stderr);
    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert_eq!(ran.stderr, "");
    assert_eq!(ran.stdout, VALUES_OUT);
    assert_eq!(ran.status, Some(0));
}

/// D-951, D-684: `cheby_ffi_write_stdout` writes once through the
/// runtime's stream, in order with `io::println` under `cheby run`, in an
/// executable, and inside `cheby test`'s capture of the test that wrote it.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn native_ffi_output_order() {
    let tmp = FreshDir::create("cheby-native-ffi-order").unwrap();
    let root = values_package(tmp.path());
    let run = cheby(&root, &["run"], &[], None);
    assert!(run.stdout.ends_with("one\ntwo\nthree\n"), "{}", run.stdout);
    let exe = tmp.path().join("exe").join("app");
    let build = cheby(&root, &["build", "-o", exe.to_str().unwrap()], &[], None);
    assert_eq!(build.status, Some(0), "{}", build.stderr);
    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert!(ran.stdout.ends_with("one\ntwo\nthree\n"), "{}", ran.stdout);
    let test = cheby(&root, &["test"], &[], None);
    assert_eq!(test.status, Some(1), "{}{}", test.stdout, test.stderr);
    let all = format!("{}{}", test.stdout, test.stderr);
    assert!(all.contains("one\ntwo\nthree\n"), "{all}");
}

/// `@blocking` C functions of a package (D-950, §12.7): one builds a
/// `String` with `cheby_ffi_string_new` on a pool thread, and one makes a
/// value of an external type there, whose drop function writes a line.
const BLOCKING_C: &str = r#"#include <cheby.h>

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

cheby_value *app_greet(cheby_value *name) {
    static const char hello[] = "hello from the pool, ";
    size_t len = 0;
    const char *bytes = cheby_ffi_string_bytes(name, &len);
    char *text = malloc(sizeof hello - 1 + len);
    if (text == NULL) {
        return NULL;
    }
    memcpy(text, hello, sizeof hello - 1);
    memcpy(text + sizeof hello - 1, bytes, len);
    cheby_value *out = cheby_ffi_string_new(text, sizeof hello - 1 + len);
    free(text);
    return out;
}

typedef struct {
    int64_t n;
} counter;

void *app_counter_open(int64_t n) {
    counter *c = malloc(sizeof *c);
    if (c != NULL) {
        c->n = n;
    }
    return c;
}

int64_t app_counter_get(void *c) {
    return ((counter *)c)->n;
}

void app_counter_close(void *c) {
    free(c);
    cheby_ffi_write_stdout("closed\n", 7);
}
"#;

/// The module that binds [`BLOCKING_C`].
const BLOCKING_POOL: &str = r#"//! Foreign functions that run on the blocking pool.

/// A counter kept by C code.
@external(native, "app_counter_close")
pub type Counter

/// Greets `name` with a `String` built on a pool thread.
@external(native, "app_greet")
@blocking
pub fn greet(name: String) -> String

/// A new counter at `n`, made on a pool thread.
@external(native, "app_counter_open")
@blocking
pub fn open_counter(n: Int) -> Counter

/// The counter's value.
@external(native, "app_counter_get")
pub fn count(counter: Counter) -> Int
"#;

/// The root module: prints what the pool built.
const BLOCKING_MAIN: &str = r#"//! Prints values that C code built on the blocking pool.

import app::pool
import std::io

fn main() {
  io::println(pool::greet("Ada"))
  let counter = pool::open_counter(41)
  let n = pool::count(counter) + 1
  io::println("count: {n}")
}
"#;

/// What [`BLOCKING_MAIN`] prints: the drop function runs on the fiber when
/// the counter's last use is over (D-171), before the count is printed.
const BLOCKING_OUT: &str = "hello from the pool, Ada\nclosed\ncount: 42\n";

/// The blocking package in `dir/app`.
fn blocking_package(dir: &Path) -> PathBuf {
    package(
        dir,
        "app",
        &[
            (
                "cheby.toml",
                "name = \"app\"\nversion = \"0.1.0\"\ntargets = [\"native\"]\n\n\
                 [native]\nsources = [\"src/pool.c\"]\n",
            ),
            ("src/pool.c", BLOCKING_C),
            ("src/pool.cheby", BLOCKING_POOL),
            ("src/main.cheby", BLOCKING_MAIN),
        ],
    )
}

/// D-950, D-683, D-818: a package's `@blocking` C function runs on the
/// pool through its `cb$blk$` shim and builds a `String` there, charged to
/// the calling fiber's domain (the debug leak check, D-710, passes), and
/// a value of an external type made on the pool is wrapped and dropped on
/// the fiber; the same under `cheby run` and in an executable.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn native_blocking_builds_values_on_the_pool() {
    let tmp = FreshDir::create("cheby-native-blocking").unwrap();
    let root = blocking_package(tmp.path());
    let run = cheby(&root, &["run"], &[], None);
    assert_eq!(run.stderr, "");
    assert_eq!(run.stdout, BLOCKING_OUT);
    assert_eq!(run.status, Some(0));
    let exe = tmp.path().join("exe").join("app");
    let build = cheby(&root, &["build", "-o", exe.to_str().unwrap()], &[], None);
    assert_eq!(build.status, Some(0), "{}", build.stderr);
    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert_eq!(ran.stderr, "");
    assert_eq!(ran.stdout, BLOCKING_OUT);
    assert_eq!(ran.status, Some(0));
}

/// D-948, D-964: E0079 is a front-end error, so its module is never
/// cached: a second `cheby check` on the same cache reports it again from
/// source.
#[test]
fn native_ext_result_is_reported_again_on_a_warm_cache() {
    let tmp = FreshDir::create("cheby-native-e0079").unwrap();
    let root = package(
        tmp.path(),
        "app",
        &[
            (
                "cheby.toml",
                "name = \"app\"\nversion = \"0.1.0\"\ntargets = [\"native\"]\n",
            ),
            (
                "src/main.cheby",
                "//! A foreign result that holds an external type.\n\n\
                 @external(native, \"db_close\")\npub type Db\n\n\
                 @external(native, \"db_open\")\npub fn open(path: String) -> Option<Db>\n\n\
                 fn main() {}\n",
            ),
        ],
    );
    let cache = fresh_cache();
    let check = || {
        let mut cmd = Command::new(CHEBY);
        cmd.args(["check"]).current_dir(&root).env("NO_COLOR", "1");
        cache.apply(&mut cmd);
        process::run(cmd, None, TIMEOUT).unwrap()
    };
    for round in ["cold", "warm"] {
        let out = check();
        assert_eq!(out.status, Some(1), "{round}: {}", out.stderr);
        assert!(
            out.stderr.starts_with(
                "error[E0079]: the native foreign result type `Option<Db>` holds the external \
                 type `Db` below its top level\n"
            ),
            "{round}: {}",
            out.stderr
        );
    }
}
