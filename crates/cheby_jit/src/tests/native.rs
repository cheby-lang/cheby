//! Tests `native_pkg_*` of package libraries in the JIT (D-941): a
//! library added with [`Jit::add_library`] serves the native symbols of
//! later batches, and [`resolve_native`] looks in the runtime, then the
//! libraries in order, then the process (D-455). Each test builds its
//! library with `cc`, as the driver's shared library is built.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;

/// Compiles `source` into a shared library in a fresh directory and opens
/// it. The directory is left behind: a library is never unloaded, so its
/// file is kept too (D-941).
fn library(name: &str, source: &str) -> Arc<NativeLibrary> {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "cheby-jit-native-{}-{n}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("lib.c"), source).unwrap();
    let file = if cfg!(target_os = "macos") {
        "libtest.dylib"
    } else {
        "libtest.so"
    };
    let kind = if cfg!(target_os = "macos") {
        "-dynamiclib"
    } else {
        "-shared"
    };
    let status = std::process::Command::new("cc")
        .current_dir(&dir)
        .args([kind, "-fPIC", "-O2", "-o", file, "lib.c"])
        .status()
        .unwrap();
    assert!(status.success(), "cc failed for {name}");
    Arc::new(NativeLibrary::open(&dir.join(file)).unwrap())
}

/// The `int` the library's global `name` holds.
#[expect(
    clippy::cast_ptr_alignment,
    reason = "the C compiler aligns an `int` global to 4 bytes"
)]
fn global(library: &NativeLibrary, name: &str) -> i32 {
    let address = library.lookup(name).unwrap();
    // SAFETY: the test's library defines `name` as an `int`, which lives
    // as long as the library, which is never unloaded.
    unsafe { address.cast::<i32>().read_volatile() }
}

/// A symbol only the package's library defines does not resolve without
/// it; once it is added, the batch compiles and its call runs the
/// library's function.
#[test]
fn native_pkg_library_serves_later_batches() {
    let lib = library(
        "serves",
        "int app_n8a_count = 0;\nvoid app_n8a_hit(void) { app_n8a_count += 1; }\n",
    );
    let mut jit = Jit::new().unwrap();
    let err = jit
        .compile(&calls_natives(&["app_n8a_hit"]).units())
        .unwrap_err();
    assert!(
        matches!(&err, JitError::UnresolvedSymbols(symbols) if symbols == &["app_n8a_hit"]),
        "{err}"
    );
    jit.add_library(Arc::clone(&lib)).unwrap();
    // Adding it twice keeps one entry.
    jit.add_library(Arc::clone(&lib)).unwrap();
    assert_eq!(jit.libraries().len(), 1);
    let mut p = calls_natives(&["app_n8a_hit"]);
    let target = p.call(def(1, 0), Vec::new());
    jit.compile(&p.units()).unwrap();
    assert_eq!(global(&lib, "app_n8a_count"), 0);
    assert_eq!(
        run_const(&jit, target),
        EntryOutcome::Returned(static_nil())
    );
    assert_eq!(global(&lib, "app_n8a_count"), 1);
}

/// D-455, D-941: the runtime's symbols win over a library's, a library
/// wins over the process, and an earlier library over a later one.
#[test]
fn native_pkg_lookup_order() {
    let first = library(
        "first",
        "void cheby_rt_unresolved(void) {}\nlong labs(long n) { return n; }\n\
         int app_n8a_both(void) { return 1; }\n",
    );
    let second = library("second", "int app_n8a_both(void) { return 2; }\n");
    let libraries = [Arc::clone(&first), Arc::clone(&second)];
    let runtime = cheby_runtime::symbols()
        .iter()
        .find(|(name, _)| *name == sym::UNRESOLVED)
        .map(|&(_, address)| address)
        .unwrap();
    assert_eq!(resolve_native(sym::UNRESOLVED, &libraries), Some(runtime));
    assert_eq!(resolve_native("labs", &libraries), first.lookup("labs"));
    assert_ne!(
        resolve_native("labs", &libraries),
        resolve_native("labs", &[])
    );
    assert_eq!(
        resolve_native("app_n8a_both", &libraries),
        first.lookup("app_n8a_both")
    );
    assert_eq!(
        resolve_native("app_n8a_both", &[Arc::clone(&second), first]),
        second.lookup("app_n8a_both")
    );
    // The process still answers last, and nothing answers for a name no
    // one defines.
    assert!(resolve_native("getpid", &libraries).is_some());
    assert_eq!(resolve_native("app_n8a_nowhere", &libraries), None);
}

/// D-712, D-941: after a new library generation, code that names a symbol
/// under that generation calls the new library, while code compiled
/// before keeps calling the old one, under the same plain name.
#[test]
fn native_pkg_library_generations() {
    let old = library(
        "old",
        "int app_n8a_old = 0;\nvoid app_n8a_first(void) { app_n8a_old += 1; }\n",
    );
    let new = library(
        "new",
        "int app_n8a_new = 0;\nvoid app_n8a_first(void) { app_n8a_new += 10; }\n",
    );
    let mut jit = Jit::new().unwrap();
    jit.add_library(Arc::clone(&old)).unwrap();
    let mut p = calls_natives(&["app_n8a_first"]);
    let first = p.call(def(1, 0), Vec::new());
    jit.compile(&p.units()).unwrap();
    let generation = jit.add_library_generation(vec![Arc::clone(&new)]);
    assert_eq!(generation, 1);
    assert_eq!(jit.libraries().len(), 1);
    let renamed = generation_symbol("app_n8a_first", generation);
    assert_eq!(split_generation(&renamed), ("app_n8a_first", 1));
    assert_eq!(split_generation("app_n8a_first"), ("app_n8a_first", 0));
    assert_eq!(generation_symbol("app_n8a_first", 0), "app_n8a_first");
    let mut q = Prog {
        thunk_base: 100,
        ..Prog::default()
    };
    let call = Stmt::Eval(Rvalue::CallForeign {
        symbol: renamed.as_str().into(),
        sig: cheby_mir::ForeignSig {
            params: Vec::new(),
            ret: cheby_mir::CType::Void,
        },
        args: Vec::new(),
        blocking: false,
    });
    let g = q.item(
        1,
        "g",
        &[],
        &[],
        vec![block(
            vec![call],
            Terminator::Return(Operand::Const(Const::Nil)),
        )],
        &[],
    );
    let second = q.call(g, Vec::new());
    jit.compile(&q.units()).unwrap();
    run_const(&jit, first);
    run_const(&jit, second);
    assert_eq!(global(&old, "app_n8a_old"), 1);
    assert_eq!(global(&new, "app_n8a_new"), 10);
}
