//! Tests `native_cc_*`, `native_shlib_*` and `native_pkg_*` of a
//! package's native code in the driver ([`crate::native`], D-940 to
//! D-944): cached C objects keyed without paths, the dependency check, the
//! shared library and the tool errors. They run `cc`, which Windows has
//! only from M6 (D-935).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use cheby_package::NativeTable;

use crate::Cache;
use crate::cache::{BuildId, CacheLog, Store};
use crate::native::{LIB_DIR, PackageNative, prepare};

const BUILD: BuildId = BuildId([9; 16]);

/// A fresh directory, deleted with everything in it when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cheby-driver-native-{}-{n}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(std::fs::canonicalize(&dir).unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, file: &str, text: &str) {
        let path = self.0.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A package with `src/app.c`, which includes `src/ffi.h` and `cheby.h`,
/// with `factor` as the header's constant.
fn package(name: &str, factor: u32) -> Dir {
    let root = Dir::new(name);
    root.write(
        "src/app.c",
        "#include <stdint.h>\n#include <cheby.h>\n#include \"ffi.h\"\n\
         int64_t app_twice(int64_t n) { return n * APP_FACTOR; }\n",
    );
    root.write("src/ffi.h", &format!("#define APP_FACTOR {factor}\n"));
    root
}

fn table(sources: &[&str], libraries: &[&str]) -> NativeTable {
    NativeTable {
        sources: sources.iter().map(|s| (*s).to_owned()).collect(),
        libraries: libraries.iter().map(|s| (*s).to_owned()).collect(),
    }
}

/// A cache at `dir` whose log records lookups.
fn cache(dir: &Dir) -> Cache {
    Cache::with_store(
        Store::open(&dir.path().join("cache"), BUILD).unwrap(),
        CacheLog::new(Some(dir.path().join("log"))),
    )
}

/// What `app_twice(21)` returns through the package's loaded library.
fn twice(native: &PackageNative) -> i64 {
    let library = native.library.as_ref().unwrap();
    let address = library.lookup("app_twice").unwrap();
    // SAFETY: `app_twice` is the test's `int64_t app_twice(int64_t)`, in a
    // library that is never unloaded.
    let f = unsafe { std::mem::transmute::<*const u8, extern "C" fn(i64) -> i64>(address) };
    f(21)
}

/// Every file under `dir`, by its path relative to `dir`, with its bytes.
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let relative = path
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.insert(relative, std::fs::read(&path).unwrap());
            }
        }
    }
    out
}

/// D-942, D-972: one package at two paths, through two caches, gives the
/// same C-object records and blobs, and the same library file: no package
/// or cache path is in a key or a record.
#[test]
#[cfg_attr(windows, ignore = "C compiles on Windows are M6 (D-935)")]
fn native_cc_two_paths() {
    let a = package("two-a", 2);
    let b = package("two-paths-b", 2);
    let (ca, cb) = (Dir::new("cache-a"), Dir::new("cache-other-b"));
    let (cache_a, cache_b) = (cache(&ca), cache(&cb));
    let table = table(&["src/app.c"], &[]);
    let first = prepare(a.path(), "app", &table, Some(&cache_a)).unwrap();
    let second = prepare(b.path(), "app", &table, Some(&cache_b)).unwrap();
    assert_eq!(twice(&first), 42);
    assert_eq!(first.objects, second.objects);
    cache_a.store.flush();
    cache_b.store.flush();
    for dir in ["v1/rec", "v1/blob", &format!("v1/{LIB_DIR}"), "v1/include"] {
        let (left, right) = (
            files(&ca.path().join("cache").join(dir)),
            files(&cb.path().join("cache").join(dir)),
        );
        assert!(!left.is_empty(), "nothing in {dir}");
        assert_eq!(left, right, "{dir} differs");
        for bytes in left.values() {
            for root in [a.path(), b.path(), ca.path(), cb.path()] {
                let needle = root.to_string_lossy().into_owned();
                assert!(
                    !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                    "{needle} in {dir}"
                );
            }
        }
    }
    assert_eq!(
        cache_a.log.lines(),
        ["cobj src/app.c miss", "nlib app miss"]
    );
}

/// D-942's dependency check: a second run hits; an edit of a header the
/// source includes recompiles, and the library gets a new path (D-712).
#[test]
#[cfg_attr(windows, ignore = "C compiles on Windows are M6 (D-935)")]
fn native_cc_header_edit_recompiles() {
    let root = package("header", 2);
    let dir = Dir::new("header-cache");
    let table = table(&["src/app.c"], &[]);
    let first = prepare(root.path(), "app", &table, Some(&cache(&dir))).unwrap();
    assert_eq!(twice(&first), 42);
    let warm = cache(&dir);
    let again = prepare(root.path(), "app", &table, Some(&warm)).unwrap();
    assert_eq!(warm.log.lines(), ["cobj src/app.c hit", "nlib app hit"]);
    assert_eq!(again.objects, first.objects);
    let old_path = again.library.as_ref().unwrap().path().to_path_buf();
    root.write("src/ffi.h", "#define APP_FACTOR 3\n");
    let edited = cache(&dir);
    let after = prepare(root.path(), "app", &table, Some(&edited)).unwrap();
    assert_eq!(edited.log.lines(), ["cobj src/app.c miss", "nlib app miss"]);
    assert_eq!(twice(&after), 63);
    assert_ne!(after.library.as_ref().unwrap().path(), old_path);
    // The old library stays loaded and keeps its code.
    assert_eq!(twice(&first), 42);
}

/// D-944: a C object that defines a name the runtime exports is a tool
/// error before anything links or loads (plan §7 Q6 (4)'s text).
#[test]
#[cfg_attr(windows, ignore = "C compiles on Windows are M6 (D-935)")]
fn native_cc_runtime_export_is_a_tool_error() {
    let export = cheby_runtime::symbols()
        .iter()
        .map(|(name, _)| *name)
        .find(|name| name.starts_with("cheby_io_"))
        .unwrap();
    let root = Dir::new("export");
    root.write(
        "src/clash.c",
        &format!("void {export}(void) {{}}\nint app_fine(void) {{ return 1; }}\n"),
    );
    let err = prepare(root.path(), "app", &table(&["src/clash.c"], &[]), None).unwrap_err();
    assert_eq!(
        err.message(),
        format!(
            "`src/clash.c` defines `{export}`, a name the Cheby runtime exports; give the C \
             function another name"
        )
    );
    // The wave-6 review's R-3: the names only the runtime archive exports
    // to executables are refused too, or `cheby build` would link the C
    // function in place of the runtime's.
    for export in [
        cheby_link::program::RT_MAIN,
        cheby_runtime::abi::sym::RUNTIME_ID_SYMBOL,
    ] {
        let root = Dir::new("export_archive");
        let definition = if export == cheby_link::program::RT_MAIN {
            "void cheby_rt_main(void) {}".to_owned()
        } else {
            // `$` is not an identifier character in C: the name is given
            // as an assembler label.
            format!(
                "const char cheby_id_data[1] __asm__(\"{}{export}\") = {{0}};",
                label_prefix()
            )
        };
        root.write(
            "src/clash.c",
            &format!(
                "#include <stdint.h>\n{definition}\nint64_t app_value(void) {{ return 21; }}\n"
            ),
        );
        let err = prepare(root.path(), "app", &table(&["src/clash.c"], &[]), None).unwrap_err();
        assert_eq!(
            err.message(),
            format!(
                "`src/clash.c` defines `{export}`, a name the Cheby runtime exports; give the C \
                 function another name"
            )
        );
    }
}

/// The tool error of a C source `src/clash.c` that holds `definition`.
fn clash_error(definition: &str) -> String {
    let root = Dir::new("reserved");
    root.write(
        "src/clash.c",
        &format!("#include <stdint.h>\n{definition}\nint64_t app_value(void) {{ return 21; }}\n"),
    );
    prepare(root.path(), "app", &table(&["src/clash.c"], &[]), None)
        .unwrap_err()
        .message()
        .to_owned()
}

/// D-1201's text for `src/clash.c` defining `symbol`.
fn clash_text(symbol: &str) -> String {
    format!(
        "`src/clash.c` defines `{symbol}`, a name the Cheby runtime exports; give the C \
         function another name"
    )
}

/// D-1232, D-944, D-1201: a C object that defines
/// a name with the prefix `cheby_rt_` or `cb$` is the same tool error as
/// one that defines a runtime export, whether or not the runtime exports
/// that name today (`cb$` through an assembler label, as C identifiers
/// hold no `$`).
#[test]
#[cfg_attr(windows, ignore = "C compiles on Windows are M6 (D-935)")]
fn native_cc_reserved_prefix_is_a_tool_error() {
    assert_eq!(
        clash_error("void cheby_rt_mine(void) {}"),
        clash_text("cheby_rt_mine")
    );
    assert_eq!(
        clash_error(&format!(
            "const char cb_data[1] __asm__(\"{}cb$mine\") = {{0}};",
            label_prefix()
        )),
        clash_text("cb$mine")
    );
}

/// D-944, D-1204: `cheby.h`'s functions are runtime exports, so a C object
/// that defines one is the tool error too.
#[test]
#[cfg_attr(windows, ignore = "C compiles on Windows are M6 (D-935)")]
fn native_cc_reserved_ffi_name_is_a_tool_error() {
    assert_eq!(
        clash_error("void *cheby_ffi_dup(void *v) { return v; }"),
        clash_text("cheby_ffi_dup")
    );
}

/// The prefix the platform's C compiler puts before a global symbol's
/// name: Mach-O's `_`.
fn label_prefix() -> &'static str {
    if cfg!(target_vendor = "apple") {
        "_"
    } else {
        ""
    }
}

/// D-942: a failed compile is a tool error that names the source and shows
/// the compiler's output; a failed link names the package (plan §7 Q6
/// (4)).
#[test]
#[cfg_attr(windows, ignore = "C compiles on Windows are M6 (D-935)")]
fn native_cc_failures_are_tool_errors() {
    let root = Dir::new("failures");
    root.write("src/bad.c", "int broken( {\n");
    root.write(
        "src/undef.c",
        "int app_nowhere(int);\nint app_call(int n) { return app_nowhere(n); }\n",
    );
    let err = prepare(root.path(), "app", &table(&["src/bad.c"], &[]), None).unwrap_err();
    assert!(
        err.message()
            .starts_with("compiling `src/bad.c` with `cc` failed (")
            && err.message().contains("bad.c:"),
        "{}",
        err.message()
    );
    let err = prepare(root.path(), "app", &table(&["src/undef.c"], &[]), None).unwrap_err();
    assert!(
        err.message()
            .starts_with("linking the native code of package `app` with `cc` failed (")
            && err.message().contains("app_nowhere"),
        "{}",
        err.message()
    );
}

/// The rendered diagnostics of `cheby check` of `root`, through `cache`.
fn check_text(root: &Path, cache: &Cache) -> String {
    let out = crate::check_cached(root, &crate::CheckOptions::default(), cache).unwrap();
    crate::render_diagnostics(
        &out.diagnostics,
        &out.sources,
        cheby_diag::Colors::default(),
        false,
    )
}

/// D-944: a package that binds a name the runtime exports gets E0055 from
/// the driver's check after resolution, once per binding, also when the
/// module's front record is a cache hit; a reserved prefix is reported
/// once, by resolution, and a free `cheby_` name is fine.
#[test]
fn e0055_runtime_exports_outside_std() {
    let root = Dir::new("e0055");
    root.write(
        "cheby.toml",
        "name = \"app\"\nversion = \"0.1.0\"\ntargets = [\"native\"]\n",
    );
    root.write(
        "src/main.cheby",
        "//! Reserved names.\n\n@external(native, \"cheby_io_println\")\npub fn say(text: String)\n\n\
         @external(native, \"cheby_app_free\")\npub fn free_name() -> Int\n\nfn main() {\n  Nil\n}\n",
    );
    let dir = Dir::new("e0055-cache");
    for round in ["cold", "warm"] {
        let cache = cache(&dir);
        let text = check_text(root.path(), &cache);
        if round == "warm" {
            // The module has no error of its own, so its record is a hit,
            // and E0055 comes from the driver's check over its externals.
            let lines = cache.log.lines();
            assert!(
                lines
                    .iter()
                    .any(|l| l.starts_with("front app") && l.ends_with(" hit")),
                "{lines:?}"
            );
        }
        assert_eq!(text.matches("error[E0055]").count(), 1, "{round}: {text}");
        assert!(
            text.contains(
                "error[E0055]: the native symbol `cheby_io_println` is reserved for the toolchain"
            ),
            "{round}: {text}"
        );
        assert!(
            text.contains(
                "= note: the Cheby runtime exports this symbol, so only the standard library may \
                 bind it"
            ),
            "{round}: {text}"
        );
        // Never E0054 against std's binding of the same symbol.
        assert!(!text.contains("E0054"), "{round}: {text}");
        assert!(!text.contains("cheby_app_free"), "{round}: {text}");
    }
    // A reserved prefix is resolution's E0055, reported once.
    root.write(
        "src/main.cheby",
        "//! A prefix.\n\n@external(native, \"cheby_rt_alloc\")\npub fn alloc(size: Int) -> Int\n\n\
         fn main() {\n  Nil\n}\n",
    );
    let text = check_text(root.path(), &cache(&dir));
    assert_eq!(text.matches("error[E0055]").count(), 1, "{text}");
    assert!(text.contains("symbols starting with `cheby_rt_`"), "{text}");
}

/// What a session prints, both streams in order.
#[derive(Default)]
struct Printed(String);

impl crate::session::Output for Printed {
    fn stdout(&mut self, text: &str) {
        self.0.push_str(text);
    }

    fn stderr(&mut self, text: &str) {
        self.0.push_str(text);
    }
}

/// D-712, D-941: in the REPL the package's C function runs; after a header
/// its C source includes changes, `:reload` loads a new library from a new
/// path and gives the module that binds it a new version, which calls the
/// new code; a function compiled before reaches the new version through
/// its slot, as after any `:reload` (D-626), while the old version's code
/// keeps the old library, which is never unloaded. A second `:reload` with
/// nothing changed loads nothing.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn native_pkg_repl_reload_loads_the_new_library() {
    use crate::session::{Session, SessionOptions, Start};
    let root = package("repl-reload", 2);
    root.write(
        "cheby.toml",
        "name = \"app\"\nversion = \"0.1.0\"\ntargets = [\"native\"]\n\n\
         [native]\nsources = [\"src/app.c\"]\n",
    );
    root.write(
        "src/doubling.cheby",
        "//! Doubling in C.\n\n/// Twice `n`, in C.\n@external(native, \"app_twice\")\n\
         pub fn twice(n: Int) -> Int\n",
    );
    root.write(
        "src/main.cheby",
        "//! The app.\n\nimport app::doubling\n\nfn main() {\n  let _ = doubling::twice(1)\n}\n",
    );
    let path = root.path().to_path_buf();
    let printed = std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || {
            let options = SessionOptions {
                threads: Some(2),
                ..SessionOptions::default()
            };
            let Start::Ready(mut session, _) = Session::start(&path, options).unwrap() else {
                panic!("the session did not start");
            };
            let mut out = Printed::default();
            for entry in [
                "import app::doubling",
                "fn before() -> Int {\n  doubling::twice(21)\n}",
                "before()",
            ] {
                session.eval(entry, &mut out).unwrap();
            }
            std::fs::write(path.join("src/ffi.h"), "#define APP_FACTOR 3\n").unwrap();
            session.reload(&mut out).unwrap();
            for entry in ["doubling::twice(21)", "before()"] {
                session.eval(entry, &mut out).unwrap();
            }
            session.reload(&mut out).unwrap();
            out.0
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(
        printed,
        "42 : Int\nreloaded `app::doubling`\nreloaded `app`\n63 : Int\n63 : Int\n\
         no module changed\n"
    );
}

/// D-941: without a cache the library is still built and loaded, from a
/// temporary directory that is gone once it is open; a table that names
/// nothing loads nothing; a package that names only `libraries` gets the
/// forwarder alone.
#[test]
#[cfg_attr(windows, ignore = "C compiles on Windows are M6 (D-935)")]
fn native_shlib_without_a_cache() {
    let root = package("no-cache", 2);
    let native = prepare(root.path(), "app", &table(&["src/app.c"], &[]), None).unwrap();
    assert_eq!(twice(&native), 42);
    assert!(!native.library.as_ref().unwrap().path().exists());
    let empty = prepare(root.path(), "app", &table(&[], &[]), None).unwrap();
    assert!(empty.library.is_none() && empty.objects.is_empty());
    let libs = prepare(root.path(), "app", &table(&[], &["m"]), None).unwrap();
    let library = libs.library.as_ref().unwrap();
    assert!(library.lookup(cheby_link::shlib::FFI_TABLE).is_some());
    assert!(library.lookup("cbrt").is_some());
    assert_eq!(libs.libraries, ["m"]);
}
