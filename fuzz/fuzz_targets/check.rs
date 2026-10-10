//! The whole front end never fails internally: `cheby check` on any
//! one-module package ends with diagnostics, never an internal compiler
//! error, a panic or a stack overflow (docs/plan/testing.md §8, D-464).
//!
//! Each input becomes `src/main.cheby` of a package in a temporary
//! directory, checked against the real std. The check runs on a thread
//! with a smaller stack than the command line's main thread, so that
//! unbounded recursion shows up early (D-441).
//!
//! Seed the corpus with the examples and the UI tests:
//!
//! ```sh
//! mkdir -p corpus/check
//! cp ../docs/examples/*/src/main.cheby corpus/check/ 2>/dev/null
//! find ../tests/ui -name '*.cheby' -exec sh -c 'cp "$1" corpus/check/$(echo "$1" | tr / _)' _ {} \;
//! cargo +nightly fuzz run check
//! ```

#![no_main]

use std::path::PathBuf;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;

/// The stack of the checking thread: a quarter of the usual main thread.
const STACK: usize = 2 << 20;

/// The package directory of this process, with its manifest written.
fn package() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("cheby_fuzz_check_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).expect("create the package directory");
        std::fs::write(
            dir.join("cheby.toml"),
            "name = \"fuzz\"\nversion = \"0.1.0\"\n",
        )
        .expect("write the manifest");
        dir
    })
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let dir = package();
    std::fs::write(dir.join("src/main.cheby"), text).expect("write the module");
    let checked = std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || cheby_driver::check(dir).map(|out| out.render()))
        .expect("spawn the checking thread")
        .join();
    match checked {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => assert!(
            !e.message().starts_with("internal compiler error"),
            "{}",
            e.message()
        ),
        Err(panic) => std::panic::resume_unwind(panic),
    }
});
