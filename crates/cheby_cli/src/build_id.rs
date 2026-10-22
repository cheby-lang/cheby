//! The compiler's build ID (D-957), computed by `build.rs` from the
//! compiler's and std's sources, `Cargo.lock`, the toolchain file, the
//! profile, the target, the cfgs that change behavior and the runtime ID
//! (D-930). It is in every key and every file header of the build cache.

use cheby_driver::cache::BuildId;

/// This `cheby`'s build ID (D-957).
pub(crate) fn build_id() -> BuildId {
    BuildId(include!(concat!(env!("OUT_DIR"), "/build_id.rs")))
}

/// The build script, whose hashing the tests below run as the build does.
#[cfg(test)]
#[path = "../build.rs"]
#[expect(
    dead_code,
    reason = "the script's `main` and its environment are the build's"
)]
mod script;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime};

    use super::script::{Settings, compute, input_paths};

    /// A fresh directory, deleted on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("cheby-build-id-{}-{n}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A small workspace: the inputs in `files`, written in that order.
    fn workspace(dir: &Path, files: &[(&str, &str)]) {
        for (rel, text) in files {
            let path = dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
        }
    }

    const FILES: [(&str, &str); 6] = [
        ("Cargo.lock", "# lock\n"),
        ("rust-toolchain.toml", "[toolchain]\n"),
        ("crates/a/Cargo.toml", "[package]\nname = \"a\"\n"),
        ("crates/a/src/lib.rs", "//! a\n"),
        ("crates/b/src/lib.rs", "//! b\n"),
        ("std/core/list.cheby", "//! list\n"),
    ];

    fn settings() -> Settings {
        Settings {
            profile: "debug".to_owned(),
            target: "aarch64-apple-darwin".to_owned(),
            debug_assertions: true,
            sanitize: String::new(),
            runtime_id: "00112233445566778899aabbccddeeff".to_owned(),
        }
    }

    #[test]
    fn build_id_changes_with_the_runtime_id() {
        let dir = TestDir::new("runtime");
        workspace(&dir.0, &FILES);
        let a = compute(&dir.0, &settings()).unwrap();
        let b = compute(
            &dir.0,
            &Settings {
                runtime_id: "ffeeddccbbaa99887766554433221100".to_owned(),
                ..settings()
            },
        )
        .unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn build_id_is_the_same_for_two_copies_at_two_paths() {
        let one = TestDir::new("one");
        let two = TestDir::new("two-at-another-path");
        workspace(&one.0, &FILES);
        // The other copy is written in reverse order, with other
        // modification times, and holds a Finder file.
        let mut reversed = FILES;
        reversed.reverse();
        workspace(&two.0, &reversed);
        workspace(&two.0, &[("crates/a/.DS_Store", "finder")]);
        let past = SystemTime::now() - Duration::from_hours(48);
        for (rel, _) in FILES {
            fs::File::options()
                .write(true)
                .open(two.0.join(rel))
                .unwrap()
                .set_modified(past)
                .unwrap();
        }
        assert_eq!(input_paths(&one.0).unwrap(), input_paths(&two.0).unwrap());
        assert_eq!(
            compute(&one.0, &settings()).unwrap(),
            compute(&two.0, &settings()).unwrap()
        );
    }

    #[test]
    fn build_id_inputs_are_sorted_relative_paths() {
        let dir = TestDir::new("paths");
        workspace(&dir.0, &FILES);
        let mut expected: Vec<_> = FILES.iter().map(|(p, _)| (*p).to_owned()).collect();
        expected.sort();
        assert_eq!(input_paths(&dir.0).unwrap(), expected);
        // A workspace without the lock file is an error, not another ID.
        fs::remove_file(dir.0.join("Cargo.lock")).unwrap();
        assert!(compute(&dir.0, &settings()).is_err());
    }

    #[test]
    fn build_id_changes_with_each_input_and_setting() {
        let dir = TestDir::new("inputs");
        workspace(&dir.0, &FILES);
        let base = compute(&dir.0, &settings()).unwrap();
        let mut ids = vec![base];
        for s in [
            Settings {
                profile: "release".to_owned(),
                ..settings()
            },
            Settings {
                target: "x86_64-unknown-linux-gnu".to_owned(),
                ..settings()
            },
            Settings {
                debug_assertions: false,
                ..settings()
            },
            Settings {
                sanitize: "address".to_owned(),
                ..settings()
            },
        ] {
            ids.push(compute(&dir.0, &s).unwrap());
        }
        // A byte of a source, a file's name, a new file, and bytes moved
        // from one file to the next.
        let edits: [&[(&str, &str)]; 4] = [
            &[("crates/a/src/lib.rs", "//! A\n")],
            &[
                ("std/core/list.cheby", "//! b\n"),
                ("crates/b/src/lib.rs", "//! list\n"),
            ],
            &[("crates/b/src/new.rs", "")],
            &[
                ("crates/a/src/lib.rs", "//! a\n//"),
                ("crates/b/src/lib.rs", "! b\n"),
            ],
        ];
        for edit in edits {
            let copy = TestDir::new("edit");
            workspace(&copy.0, &FILES);
            workspace(&copy.0, edit);
            ids.push(compute(&copy.0, &settings()).unwrap());
        }
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "every change gives another ID");
        // And nothing else does: the same inputs again.
        assert_eq!(compute(&dir.0, &settings()).unwrap(), base);
    }

    #[test]
    fn build_id_of_the_real_workspace_reads_no_build_output() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let paths = input_paths(&workspace).unwrap();
        assert!(paths.iter().any(|p| p == "Cargo.lock"));
        assert!(paths.iter().any(|p| p == "crates/cheby_cli/build.rs"));
        assert!(paths.iter().any(|p| p.starts_with("std/")));
        assert!(
            paths
                .iter()
                .all(|p| p.starts_with("crates/") || p.starts_with("std/") || !p.contains('/')),
            "only crates/, std/ and the two root files"
        );
        assert!(
            !paths
                .iter()
                .any(|p| p.contains("/target/") || p.contains(".m3-work"))
        );
        assert_ne!(super::build_id().0, [0; 16]);
    }
}
