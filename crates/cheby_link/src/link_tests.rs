//! Tests `link_sweep_*` of the link directories' sweep (D-1112, on
//! D-1127's model): `sweep_dead` deletes a dead process's
//! `<prefix>-<pid>-<n>` directories and nothing else. Each test works in a
//! fresh directory of its own, never in the shared temporary directory
//! itself. Unix only: Windows sweeps nothing until M6 (D-935).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::link::{OBJECTS_PREFIX, STAGING_PREFIX, sweep_dead};

/// A fresh directory, deleted with everything in it when dropped.
struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cheby-link-sweep-test-{}-{n}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The pid of a process that has exited and been waited for, as
/// `cache_location_fallback_sweeps_dead_temporaries` takes one.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// A directory `name` under `parent` with a file and a subdirectory in it,
/// as a link leaves its objects behind.
fn plant(parent: &Path, name: &str) -> PathBuf {
    let path = parent.join(name);
    std::fs::create_dir_all(path.join("sub")).unwrap();
    std::fs::write(path.join("hello_world.o"), b"object").unwrap();
    std::fs::write(path.join("sub/file"), b"x").unwrap();
    path
}

/// A dead process's objects' directory and staging directory are deleted
/// by the sweep of their own prefix.
#[test]
fn link_sweep_removes_a_dead_processs_directories() {
    let dir = TestDir::new("dead");
    let dead = dead_pid();
    let objects = plant(dir.path(), &format!("{OBJECTS_PREFIX}-{dead}-0"));
    let staging = plant(dir.path(), &format!("{STAGING_PREFIX}-{dead}-3"));
    sweep_dead(dir.path(), OBJECTS_PREFIX);
    assert!(!objects.exists(), "{}", objects.display());
    assert!(staging.exists(), "another prefix's sweep keeps it");
    sweep_dead(dir.path(), STAGING_PREFIX);
    assert!(!staging.exists(), "{}", staging.display());
}

/// The sweep keeps this process's directory, a live process's, every
/// other name, a regular file, a symbolic link and its target, and the
/// other prefix's directory; a dead process's directory beside them goes,
/// so the sweep ran.
#[test]
fn link_sweep_keeps_everything_else() {
    let dir = TestDir::new("keep");
    let swept = dir.path().join("tmp");
    std::fs::create_dir(&swept).unwrap();
    let dead = dead_pid();
    let own = std::process::id();
    let alive = std::os::unix::process::parent_id();
    let gone = plant(&swept, &format!("{OBJECTS_PREFIX}-{dead}-7"));
    let mut kept = vec![
        plant(&swept, &format!("{OBJECTS_PREFIX}-{own}-0")),
        plant(&swept, &format!("{OBJECTS_PREFIX}-{alive}-0")),
        plant(&swept, &format!("{OBJECTS_PREFIX}-{dead}")),
        plant(&swept, &format!("{OBJECTS_PREFIX}-{dead}-1-2")),
        plant(&swept, &format!("{OBJECTS_PREFIX}-x-1")),
        plant(&swept, &format!("{OBJECTS_PREFIX}--1")),
        plant(&swept, &format!("{OBJECTS_PREFIX}-{dead}-")),
        plant(&swept, &format!("{OBJECTS_PREFIX}x-{dead}-0")),
        plant(&swept, &format!("{OBJECTS_PREFIX}-99999999999-0")),
        plant(&swept, &format!("{STAGING_PREFIX}-{dead}-0")),
    ];
    let file = swept.join(format!("{OBJECTS_PREFIX}-{dead}-5"));
    std::fs::write(&file, b"not a directory").unwrap();
    kept.push(file);
    let target = plant(dir.path(), "target");
    let link = swept.join(format!("{OBJECTS_PREFIX}-{dead}-6"));
    std::os::unix::fs::symlink(&target, &link).unwrap();

    sweep_dead(&swept, OBJECTS_PREFIX);

    assert!(!gone.exists(), "a dead process's directory is swept");
    for path in &kept {
        assert!(path.exists(), "{}", path.display());
    }
    assert!(
        std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()),
        "the symbolic link is kept"
    );
    assert!(target.join("hello_world.o").is_file(), "its target is kept");
    assert!(target.join("sub/file").is_file(), "its target is kept");
}

/// A `parent` that is missing or cannot be read is ignored: the sweep only
/// tidies up.
#[test]
fn link_sweep_ignores_an_unreadable_or_missing_parent() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TestDir::new("unreadable");
    sweep_dead(&dir.path().join("missing"), OBJECTS_PREFIX);
    sweep_dead(&dir.path().join("missing"), STAGING_PREFIX);
    let closed = dir.path().join("closed");
    std::fs::create_dir(&closed).unwrap();
    let dead = dead_pid();
    let leftover = plant(&closed, &format!("{OBJECTS_PREFIX}-{dead}-0"));
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
    sweep_dead(&closed, OBJECTS_PREFIX);
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700)).unwrap();
    // SAFETY: `geteuid` takes no arguments, cannot fail and touches no
    // memory of ours.
    let root = unsafe { libc::geteuid() } == 0;
    // As root the directory can be read anyway and the leftover goes;
    // otherwise nothing could be read and it stays. Either way, no panic.
    assert!(root || leftover.exists(), "{}", leftover.display());
    let file = dir.path().join("file");
    std::fs::write(&file, b"not a directory").unwrap();
    sweep_dead(&file, OBJECTS_PREFIX);
    assert!(file.is_file());
}
