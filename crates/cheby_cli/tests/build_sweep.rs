//! `cheby build` sweeps the link directories a dead process left
//! (D-1112, on D-1127's model): before it creates
//! `<tmp>/cheby-objects-<pid>-<n>` and `<output dir>/.cheby-link-<pid>-<n>`
//! it deletes the directories of that pattern whose process is dead, and
//! keeps a live process's.
//!
//! The test never plants anything in the shared temporary directory: the
//! child's `TMPDIR` names a fresh directory of its own. It links against
//! the runtime archive next to `cheby`, so `cargo build -p cheby_rt_exe`
//! comes first (D-929), and `cheby` gets a fresh `CHEBY_CACHE` (D-1128).

#![expect(
    clippy::unwrap_used,
    reason = "test helpers: a failed setup step fails the test"
)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::{FreshDir, HarnessCache};
use cheby_test_support::process;

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(120);

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

/// A directory `name` in `parent` with an object file in it, as a link
/// killed by a signal leaves it.
fn plant(parent: &Path, name: &str) -> PathBuf {
    let path = parent.join(name);
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("hello_world.o"), b"object").unwrap();
    path
}

/// The names in `dir`.
fn names(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}

/// A build deletes a dead process's objects' directory in its temporary
/// directory and its staging directory in the output directory, keeps a
/// live process's, removes its own, and the executable runs.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_sweeps_dead_link_dirs() {
    let work = FreshDir::create("cheby-build-sweep").unwrap();
    let tmp = work.path().join("tmp");
    let out = work.path().join("out");
    std::fs::create_dir(&tmp).unwrap();
    std::fs::create_dir(&out).unwrap();
    let root = work.path().join("hello_world");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/001_hello_world"),
        &root,
    );

    let mut child = Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    // This test's process: alive while `cheby` runs, and not `cheby`'s.
    let alive = std::process::id();
    let dead_objects = plant(&tmp, &format!("cheby-objects-{dead}-0"));
    let dead_staging = plant(&out, &format!(".cheby-link-{dead}-0"));
    let live_objects = plant(&tmp, &format!("cheby-objects-{alive}-0"));
    let live_staging = plant(&out, &format!(".cheby-link-{alive}-0"));

    let exe = out.join("hello_world");
    let cache = HarnessCache::create_with("cli-build-sweep", None).unwrap();
    let mut cmd = Command::new(CHEBY);
    cmd.args(["build", "-o", exe.to_str().unwrap()])
        .current_dir(&root)
        .env("NO_COLOR", "1")
        .env("TMPDIR", &tmp);
    cache.apply(&mut cmd);
    let built = process::run(cmd, None, TIMEOUT).unwrap();
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    assert_eq!((built.stdout.as_str(), built.stderr.as_str()), ("", ""));

    assert!(!dead_objects.exists(), "{}", dead_objects.display());
    assert!(!dead_staging.exists(), "{}", dead_staging.display());
    assert!(live_objects.join("hello_world.o").is_file());
    assert!(live_staging.join("hello_world.o").is_file());
    // The build's own directories are gone too (D-1112), and nothing else
    // was written.
    assert_eq!(
        names(&tmp),
        BTreeSet::from([format!("cheby-objects-{alive}-0")])
    );
    assert_eq!(
        names(&out),
        BTreeSet::from([format!(".cheby-link-{alive}-0"), "hello_world".to_owned()])
    );

    let ran = process::run(Command::new(&exe), None, TIMEOUT).unwrap();
    assert_eq!(ran.stdout, "Hello, world!\n");
    assert_eq!(ran.status, Some(0), "{}", ran.stderr);
}
