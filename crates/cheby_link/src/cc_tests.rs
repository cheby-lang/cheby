//! Tests `native_cc_*` of the C compile (D-942) and `native_shlib_*` of a
//! package's shared library (D-941). They run `cc`, which Windows has
//! only from M6 (D-935).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::cc::{
    CcError, CompileJob, Dependency, HEADER, HEADER_NAME, classify, compile, parse_depfile,
};
#[cfg(unix)]
use crate::shlib::FFI_TABLE;
use crate::shlib::{SharedJob, ShlibError, defined_symbols, link_shared};

/// A fresh directory, deleted with everything in it when dropped.
struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cheby-native-cc-test-{}-{n}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The compile's paths are compared as given, so resolve the
        // temporary directory's links (`/var` is `/private/var` on macOS).
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

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A package `root` with `src/app.c`, which includes a header of its own,
/// `cheby.h` and a system header, and a header directory with `cheby.h`.
fn package(name: &str) -> (TestDir, TestDir) {
    let root = TestDir::new(name);
    root.write(
        "src/app.c",
        "#include <stdint.h>\n#include <cheby.h>\n#include \"ffi.h\"\n\
         int64_t app_twice(int64_t n) { return n * APP_FACTOR; }\n\
         const char *app_file(void) { return __FILE__; }\n",
    );
    root.write("src/ffi.h", "#define APP_FACTOR 2\n");
    let headers = TestDir::new(&format!("{name}-include"));
    headers.write(HEADER_NAME, HEADER);
    (root, headers)
}

fn job<'a>(root: &'a TestDir, headers: &'a TestDir, source: &'a str) -> CompileJob<'a> {
    CompileJob {
        root: root.path(),
        source,
        header_dir: headers.path(),
    }
}

#[test]
fn native_cc_header_is_the_guard_and_the_typedef() {
    assert_eq!(
        HEADER,
        "#ifndef CHEBY_H\n#define CHEBY_H\n\ntypedef struct cheby_value cheby_value;\n\n#endif\n"
    );
}

#[test]
#[cfg_attr(windows, ignore = "C compiles on Windows are M6 (D-935)")]
fn native_cc_compiles_and_records_what_it_read() {
    let (root, headers) = package("deps");
    let compiled = compile(&job(&root, &headers, "src/app.c")).unwrap();
    assert!(object::File::parse(compiled.object.as_slice()).is_ok());
    let deps = &compiled.dependencies;
    for wanted in [
        Dependency::Package("src/app.c".to_owned()),
        Dependency::Package("src/ffi.h".to_owned()),
        Dependency::Header,
    ] {
        assert!(deps.contains(&wanted), "{wanted:?} not in {deps:?}");
    }
    // `stdint.h` is a system header, recorded by its absolute path.
    assert!(
        deps.iter()
            .any(|d| matches!(d, Dependency::Other(p) if p.is_absolute())),
        "{deps:?}"
    );
    // No path of the package or the header directory is recorded.
    for dep in deps {
        if let Dependency::Other(path) = dep {
            assert!(!path.starts_with(root.path()), "{path:?}");
            assert!(!path.starts_with(headers.path()), "{path:?}");
        }
    }
    let symbols = defined_symbols(&compiled.object).unwrap();
    assert!(symbols.contains(&"app_twice".to_owned()), "{symbols:?}");
    assert!(symbols.contains(&"app_file".to_owned()), "{symbols:?}");
}

/// D-942, D-264: one source compiled in two package roots with two header
/// directories gives the same object bytes, and no root reaches them.
#[test]
#[cfg_attr(windows, ignore = "C compiles on Windows are M6 (D-935)")]
fn native_cc_two_paths_give_one_object() {
    let (a, a_headers) = package("path-a");
    let (b, b_headers) = package("path-b-longer");
    let first = compile(&job(&a, &a_headers, "src/app.c")).unwrap();
    let second = compile(&job(&b, &b_headers, "src/app.c")).unwrap();
    assert_eq!(first.object, second.object);
    // The recorded dependencies are the same too.
    assert_eq!(first.dependencies, second.dependencies);
    for root in [a.path(), b.path(), a_headers.path(), b_headers.path()] {
        let needle = root.to_string_lossy().into_owned();
        assert!(
            !first
                .object
                .windows(needle.len())
                .any(|w| w == needle.as_bytes()),
            "{needle} in the object"
        );
    }
}

#[test]
#[cfg_attr(windows, ignore = "C compiles on Windows are M6 (D-935)")]
fn native_cc_failure_names_the_source() {
    let root = TestDir::new("bad");
    root.write("src/bad.c", "int broken( { return 0; }\n");
    let headers = TestDir::new("bad-include");
    headers.write(HEADER_NAME, HEADER);
    let err = compile(&job(&root, &headers, "src/bad.c")).unwrap_err();
    let CcError::Failed { source, output, .. } = &err else {
        panic!("not a failed compile: {err:?}");
    };
    assert_eq!(source, "src/bad.c");
    assert!(output.contains("bad.c"), "{output}");
    let text = err.to_string();
    assert!(
        text.starts_with("compiling `src/bad.c` with `cc` failed (") && text.contains("):\n"),
        "{text}"
    );
}

#[test]
fn native_cc_dependency_files() {
    let text = "/tmp/x/source.o: src/app.c /usr/include/stdint.h \\\n  \
                /cache/include/cheby.h src/with\\ space.h \\\n  src/dollar$$.h\n\
                src/ffi.h:\n";
    assert_eq!(
        parse_depfile(text),
        [
            "src/app.c",
            "/usr/include/stdint.h",
            "/cache/include/cheby.h",
            "src/with space.h",
            "src/dollar$.h",
        ]
    );
    assert_eq!(parse_depfile(""), Vec::<String>::new());
}

#[test]
fn native_cc_classify() {
    let root = Path::new("/pkg/app");
    let headers = Path::new("/cache/include");
    assert_eq!(
        classify(Path::new("src/./a.c"), root, headers),
        Dependency::Package("src/a.c".to_owned())
    );
    assert_eq!(
        classify(Path::new("/pkg/app/src/b.h"), root, headers),
        Dependency::Package("src/b.h".to_owned())
    );
    assert_eq!(
        classify(Path::new("/cache/include/cheby.h"), root, headers),
        Dependency::Header
    );
    // A package file named like the header is still the package's.
    assert_eq!(
        classify(Path::new("cheby.h"), root, headers),
        Dependency::Package("cheby.h".to_owned())
    );
    assert_eq!(
        classify(Path::new("src/../../other/c.h"), root, headers),
        Dependency::Other(PathBuf::from("/pkg/other/c.h"))
    );
}

/// The package's C objects of `root`'s sources.
fn objects(root: &TestDir, headers: &TestDir, sources: &[&str]) -> Vec<(String, Vec<u8>)> {
    sources
        .iter()
        .map(|source| {
            let compiled = compile(&job(root, headers, source)).unwrap();
            ((*source).to_owned(), compiled.object)
        })
        .collect()
}

/// Opens `bytes` as a shared library from a file in `dir` and tells which
/// of `names` it defines.
#[cfg(unix)]
fn defines(dir: &TestDir, bytes: &[u8], names: &[&str]) -> Vec<bool> {
    use std::ffi::CString;
    let path = dir.path().join(crate::shlib::file_name("app"));
    std::fs::write(&path, bytes).unwrap();
    let file = CString::new(path.to_str().unwrap()).unwrap();
    // SAFETY: a valid NUL-terminated path; the library is the test's own,
    // whose initializers are the C compiler's defaults.
    let handle = unsafe { libc::dlopen(file.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    assert!(!handle.is_null(), "dlopen failed");
    names
        .iter()
        .map(|name| {
            let name = CString::new(*name).unwrap();
            // SAFETY: a live handle and a valid NUL-terminated name.
            !unsafe { libc::dlsym(handle, name.as_ptr()) }.is_null()
        })
        .collect()
}

#[test]
#[cfg(unix)]
fn native_shlib_links_the_objects_and_the_forwarder() {
    let (root, headers) = package("shlib");
    let objects = objects(&root, &headers, &["src/app.c"]);
    let bytes = link_shared(&SharedJob {
        package: "app",
        objects: &objects,
        libraries: &[],
    })
    .unwrap();
    let out = TestDir::new("shlib-out");
    assert_eq!(
        defines(&out, &bytes, &["app_twice", FFI_TABLE, "app_missing"]),
        [true, true, false]
    );
    // D-264: the same objects give the same library.
    let again = link_shared(&SharedJob {
        package: "app",
        objects: &objects,
        libraries: &[],
    })
    .unwrap();
    assert_eq!(bytes, again);
}

/// D-941: a package that names only `libraries` gets the forwarder alone,
/// linked with them.
#[test]
#[cfg(unix)]
fn native_shlib_libraries_only() {
    let bytes = link_shared(&SharedJob {
        package: "app",
        objects: &[],
        libraries: &["m".to_owned()],
    })
    .unwrap();
    let out = TestDir::new("shlib-libs");
    // `dlsym` on the library also searches what it links: libm's `cbrt`.
    assert_eq!(defines(&out, &bytes, &[FFI_TABLE, "cbrt"]), [true, true]);
}

/// D-941: an undefined name in the package's C code fails the link at
/// once (`-z defs`, ld64's default), as a tool error naming the package.
#[test]
#[cfg_attr(windows, ignore = "C compiles on Windows are M6 (D-935)")]
fn native_shlib_undefined_name_fails_the_link() {
    let root = TestDir::new("undef");
    root.write(
        "src/app.c",
        "int app_nowhere(int);\nint app_call(int n) { return app_nowhere(n); }\n",
    );
    let headers = TestDir::new("undef-include");
    headers.write(HEADER_NAME, HEADER);
    let objects = objects(&root, &headers, &["src/app.c"]);
    let err = link_shared(&SharedJob {
        package: "app",
        objects: &objects,
        libraries: &[],
    })
    .unwrap_err();
    assert!(matches!(err, ShlibError::Failed { .. }), "{err:?}");
    let text = err.to_string();
    assert!(
        text.starts_with("linking the native code of package `app` with `cc` failed (")
            && text.contains("app_nowhere"),
        "{text}"
    );
}
