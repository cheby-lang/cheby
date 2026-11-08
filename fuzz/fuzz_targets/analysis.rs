//! The language server's analysis never fails internally: a session's live
//! run on any text of `src/main.cheby`, and the strict run it plans, end
//! with diagnostics, never an error, a failed run, a panic or a stack
//! overflow, and every diagnostic they publish lies in its own file
//! (plan W4, A §2.3, A §2.8).
//!
//! Each input is the open document `src/main.cheby` of a package in a
//! temporary directory whose `src/` holds no file on disk, so the module
//! exists through the overlay alone (A §2.6). A fresh session per input
//! keeps a crash reproducible from its input alone: with no last good
//! text, a broken input is a stub in the strict run (D-1357). The runs go
//! on a thread with a smaller stack than the server's analysis thread, so
//! that unbounded recursion shows up early (D-441).
//!
//! Seed the corpus as `check`'s, or share it:
//!
//! ```sh
//! mkdir -p corpus/analysis
//! cp corpus/check/* corpus/analysis/
//! RUSTFLAGS='--cfg cheby_sanitize="address"' ASAN_OPTIONS=detect_leaks=0 \
//!   cargo +nightly-2026-10-01 fuzz run analysis -- -rss_limit_mb=6144
//! ```
//!
//! LeakSanitizer stays off for the reason `check` gives (D-898): the
//! interners a session builds are kept.

#![no_main]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use cheby_driver::analysis::{FileReport, Globals, Overlay, OverlayFile, Session, SessionConfig};
use libfuzzer_sys::fuzz_target;

/// The stack of the analysis thread: a quarter of the usual main thread.
const STACK: usize = 2 << 20;

/// The package directory of this process: a manifest and an empty `src/`.
fn package() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("cheby_fuzz_analysis_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).expect("create the package directory");
        std::fs::write(
            dir.join("cheby.toml"),
            "name = \"fuzz\"\nversion = \"0.1.0\"\n",
        )
        .expect("write the manifest");
        std::path::absolute(dir).expect("an absolute package directory")
    })
}

/// Panics unless every diagnostic of `files` lies in its own file: its
/// range in the report's text, which is the live text of its path, and its
/// labels and edits in a package file's live text.
fn check_published(
    root: &Path,
    main: &Path,
    text: &str,
    files: &BTreeMap<PathBuf, Arc<FileReport>>,
) {
    let live = |path: &Path| -> Option<String> {
        if path == main {
            Some(text.to_owned())
        } else {
            std::fs::read_to_string(path).ok()
        }
    };
    for (path, report) in files {
        assert!(path.starts_with(root), "a report for {}", path.display());
        assert_eq!(
            live(path).as_deref(),
            Some(&*report.text),
            "{}",
            path.display()
        );
        let is_manifest = path.file_name().is_some_and(|n| n == "cheby.toml");
        for diag in &report.diagnostics {
            match diag.range {
                Some(range) => assert!(
                    range.slice(&report.text).is_some(),
                    "{} {}: {range:?} out of {}",
                    diag.code,
                    diag.message,
                    path.display()
                ),
                None => assert!(is_manifest, "{} {}: no range", diag.code, diag.message),
            }
            let placed = diag
                .related
                .iter()
                .map(|l| (l.path.as_path(), l.range))
                .chain(
                    diag.helps
                        .iter()
                        .flat_map(|h| &h.edits)
                        .map(|e| (e.path.as_path(), e.range)),
                );
            for (at, range) in placed {
                let inside =
                    at.starts_with(root) && live(at).is_some_and(|t| range.slice(&t).is_some());
                assert!(
                    inside,
                    "{} {}: {range:?} out of {}",
                    diag.code,
                    diag.message,
                    at.display()
                );
            }
        }
    }
}

/// The live run on `text`, then the D run it plans.
fn analyze(root: &Path, text: &str) {
    let main = root.join("src/main.cheby");
    let mut session = Session::new(SessionConfig {
        root: root.to_path_buf(),
        target: None,
        globals: Arc::new(Globals::new()),
    });
    let overlay = Overlay {
        files: BTreeMap::from([(
            main.clone(),
            OverlayFile {
                text: Arc::from(text),
                version: 1,
                stamp: 1,
            },
        )]),
    };
    let live = match session.analyze_live(&overlay) {
        Ok(live) => live,
        Err(e) => panic!("analyze_live: {}", e.message()),
    };
    assert!(
        live.failed.is_none(),
        "the live run failed: {:?}",
        live.failed
    );
    check_published(root, &main, text, &live.phase_a);
    if let Some(report) = &live.report {
        check_published(root, &main, text, &report.files);
    }
    if let Some(plan) = live.plan {
        match session.analyze_strict(plan) {
            Ok(report) => check_published(root, &main, text, &report.files),
            Err(e) => panic!("analyze_strict: {}", e.message()),
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let text = text.to_owned();
    let root = package();
    let ran = std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || analyze(root, &text))
        .expect("spawn the analysis thread")
        .join();
    if let Err(panic) = ran {
        std::panic::resume_unwind(panic);
    }
});
