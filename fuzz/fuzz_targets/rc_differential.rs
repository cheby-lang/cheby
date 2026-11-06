//! Naive reference counting against Perceus and the reference evaluator
//! (D-718, D-781, D-1028): each input chooses a program through
//! `cheby_test_support::progen` (its bytes drive the generator's choices),
//! and the program runs three times under the `cheby` that
//! `CHEBY_DIFF_BIN` names: with `CHEBY_RC=naive` and with the default,
//! both with `CHEBY_RT_POISON=1`, then with the default and
//! `CHEBY_RT_POISON=0`, the only run that executes the inline count and
//! reuse paths. Standard output, standard error and the exit status must
//! agree, and the status must be 0, or 101 for a planted final panic,
//! never 102 (a leak, D-458, D-710). The run they agree on must then
//! match the reference evaluator (`progen::evaluate`, D-719), which
//! computes the program's output from the spec in process: standard
//! output and the exit status, and standard error wherever the evaluator
//! predicts it exactly (D-1145).
//!
//! An input whose program does not pass `cheby check`, or that the
//! evaluator does not support (outside its subset, past its step budget),
//! is rejected as uninteresting (`Corpus::Reject`), never counted as a
//! pass; the generator is typed by construction and the evaluator covers
//! everything it emits, so neither should happen.
//!
//! Every `cheby` gets `CHEBY_CACHE` set to one fresh cache of this
//! process (`progen::shared_cache`, D-1128), so a run
//! never reads or fills the developer's cache.
//!
//! The target runs a copy of a debug `cheby`, which no rebuild can swap
//! mid-run, and refuses to start without one:
//!
//! ```sh
//! cargo build -p cheby_cli
//! mkdir -p /tmp/rc && cp target/debug/cheby /tmp/rc/cheby
//! cd fuzz
//! RUSTFLAGS='--cfg cheby_sanitize="address"' CHEBY_DIFF_BIN=/tmp/rc/cheby \
//!   cargo +nightly-2026-10-01 fuzz run rc_differential -- -max_total_time=1800
//! ```
//!
//! Each execution starts `cheby` four times (check and three runs), so
//! expect a few executions per second.

#![no_main]

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use cheby_test_support::cache::HarnessCache;
use cheby_test_support::progen::{self, DiffErrorKind, DiffOptions, GenConfig};
use libfuzzer_sys::{Corpus, fuzz_target};

/// The `cheby` under test, this process's package directory and its
/// cache.
struct Setup {
    cheby: PathBuf,
    dir: PathBuf,
    /// Held for the life of the process.
    cache: Arc<HarnessCache>,
}

fn setup() -> &'static Setup {
    static SETUP: OnceLock<Setup> = OnceLock::new();
    SETUP.get_or_init(|| {
        let Some(cheby) = std::env::var_os("CHEBY_DIFF_BIN").map(PathBuf::from) else {
            eprintln!(
                "rc_differential: set CHEBY_DIFF_BIN to a copy of a debug `cheby` \
                 (cargo build -p cheby_cli, then copy target/debug/cheby)"
            );
            std::process::exit(2);
        };
        if !cheby.is_file() {
            eprintln!(
                "rc_differential: CHEBY_DIFF_BIN={} is not a file",
                cheby.display()
            );
            std::process::exit(2);
        }
        let dir = std::env::temp_dir().join(format!("cheby_fuzz_rc_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the package directory");
        let cache = progen::shared_cache().expect("create the process's CHEBY_CACHE");
        eprintln!("rc_differential: {}", cache.describe());
        Setup { cheby, dir, cache }
    })
}

fuzz_target!(
    init: {
        let _ = setup();
    },
    |data: &[u8]| -> Corpus {
        let setup = setup();
        let program = progen::generate_from_bytes(data, &GenConfig::default());
        progen::write_package(&program, &setup.dir).expect("write the package");
        let options = DiffOptions {
            timeout: Duration::from_secs(120),
            dump_dir: None,
            cache: Some(setup.cache.path().to_path_buf()),
        };
        match progen::differential_with(&setup.cheby, &setup.dir, &options) {
            Ok(_) => Corpus::Keep,
            Err(e) if matches!(e.kind, DiffErrorKind::NotChecked | DiffErrorKind::NotEvaluated) => {
                Corpus::Reject
            }
            Err(e) => {
                let main = program.file("src/main.cheby").unwrap_or("");
                let helpers = program.file("src/helpers.cheby").unwrap_or("");
                panic!("{e}\n--- src/main.cheby\n{main}\n--- src/helpers.cheby\n{helpers}");
            }
        }
    }
);
