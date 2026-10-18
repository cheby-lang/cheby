//! Naive reference counting against Perceus (D-718, D-781): each input
//! chooses a program through `cheby_test_support::progen` (its bytes
//! drive the generator's choices), and the program runs three times under
//! the `cheby` that `CHEBY_DIFF_BIN` names: with `CHEBY_RC=naive` and with
//! the default, both with `CHEBY_RT_POISON=1`, then with the default and
//! `CHEBY_RT_POISON=0`, the only run that executes the inline count and
//! reuse paths. Standard output, standard error and the exit status must
//! agree, and the status must be 0, or
//! 101 for a planted final panic, never 102 (a leak, D-458, D-710).
//!
//! An input whose program does not pass `cheby check` is rejected as
//! uninteresting (`Corpus::Reject`), never counted as a pass; the
//! generator is typed by construction, so this should not happen.
//!
//! The target runs a copy of a debug `cheby`, which no rebuild can swap
//! mid-run, and refuses to start without one:
//!
//! ```sh
//! cargo build -p cheby_cli
//! mkdir -p /tmp/rc && cp target/debug/cheby /tmp/rc/cheby
//! cd fuzz
//! CHEBY_DIFF_BIN=/tmp/rc/cheby cargo +nightly-2026-10-01 fuzz run rc_differential -- -max_total_time=1800
//! ```
//!
//! Each execution starts `cheby` four times (check and three runs), so
//! expect a few executions per second.

#![no_main]

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use cheby_test_support::progen::{self, DiffErrorKind, DiffOptions, GenConfig};
use libfuzzer_sys::{Corpus, fuzz_target};

/// The `cheby` under test and this process's package directory.
struct Setup {
    cheby: PathBuf,
    dir: PathBuf,
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
        Setup { cheby, dir }
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
        };
        match progen::differential_with(&setup.cheby, &setup.dir, &options) {
            Ok(_) => Corpus::Keep,
            Err(e) if e.kind == DiffErrorKind::NotChecked => Corpus::Reject,
            Err(e) => {
                let main = program.file("src/main.cheby").unwrap_or("");
                let helpers = program.file("src/helpers.cheby").unwrap_or("");
                panic!("{e}\n--- src/main.cheby\n{main}\n--- src/helpers.cheby\n{helpers}");
            }
        }
    }
);
