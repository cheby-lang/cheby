//! Tests of the runtime ID (D-930): the hashing of `build/runtime_id.rs`,
//! which `build.rs` runs, on temporary copies of its inputs, and the
//! generated constants.

#[path = "../build/runtime_id.rs"]
mod hashing;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use hashing::{RuntimeId, Settings, compute, hex};

use crate::abi;

/// A temporary copy of the runtime ID's inputs laid out as the workspace
/// is, with a file of another crate and a file under `target/`; deleted
/// on drop.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    /// A fresh copy of the real inputs.
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("cheby-runtime-id-{}-{n}", std::process::id()));
        if root.exists() {
            std::fs::remove_dir_all(&root).unwrap();
        }
        let source = Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace = source.join("../..");
        let runtime = root.join("crates/cheby_runtime");
        for dir in ["src", "build"] {
            copy_dir(&source.join(dir), &runtime.join(dir));
        }
        for file in ["build.rs", "Cargo.toml"] {
            copy_file(&source.join(file), &runtime.join(file));
        }
        for file in ["Cargo.lock", "rust-toolchain.toml"] {
            copy_file(&workspace.join(file), &root.join(file));
        }
        copy_file(
            &workspace.join("crates/cheby_codegen_clif/src/lib.rs"),
            &root.join("crates/cheby_codegen_clif/src/lib.rs"),
        );
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("target/debug/stale.txt"), "old").unwrap();
        Self { root }
    }

    /// The runtime crate of the copy.
    fn runtime(&self) -> PathBuf {
        self.root.join("crates/cheby_runtime")
    }

    /// The copy's runtime ID under `settings`.
    fn id(&self, settings: &Settings) -> RuntimeId {
        compute(&self.runtime(), &self.root, settings).unwrap()
    }

    /// Appends `text` to the file at `relative` (to the copy's root),
    /// creating it when it does not exist.
    fn append(&self, relative: &str, text: &str) {
        let path = self.root.join(relative);
        let mut bytes = std::fs::read(&path).unwrap_or_default();
        bytes.extend_from_slice(text.as_bytes());
        std::fs::write(&path, bytes).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn copy_file(from: &Path, to: &Path) {
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::copy(from, to).unwrap();
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let target = to.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

/// The settings `build.rs` saw for this build, which it passes on as
/// `CHEBY_RUNTIME_ID_*` variables.
fn build_settings() -> Settings {
    let split = |text: &'static str| -> Vec<&'static str> {
        text.split(';').filter(|part| !part.is_empty()).collect()
    };
    Settings {
        target: env!("CHEBY_RUNTIME_ID_TARGET").to_owned(),
        features: split(env!("CHEBY_RUNTIME_ID_FEATURES"))
            .into_iter()
            .map(str::to_owned)
            .collect(),
        cfgs: split(env!("CHEBY_RUNTIME_ID_CFGS"))
            .into_iter()
            .map(|cfg| {
                let (name, value) = cfg.split_once('=').unwrap();
                (name.to_owned(), value.to_owned())
            })
            .collect(),
    }
}

/// Plain settings for the copies.
fn settings(target: &str) -> Settings {
    Settings {
        target: target.to_owned(),
        features: Vec::new(),
        cfgs: vec![
            ("debug_assertions".to_owned(), String::new()),
            ("panic".to_owned(), "unwind".to_owned()),
        ],
    }
}

/// The constants `build.rs` generated are the ID of the crate's own
/// inputs, under the settings it saw, and the symbol carries its hex.
#[test]
fn runtime_id_matches_the_build() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let id = compute(crate_dir, &crate_dir.join("../.."), &build_settings()).unwrap();
    assert_eq!(id.id, abi::RUNTIME_ID);
    assert_eq!(id.hex(), abi::RUNTIME_ID_HEX);
    assert_eq!(hex(&abi::RUNTIME_ID), abi::RUNTIME_ID_HEX);
    assert_eq!(abi::RUNTIME_ID_HEX.len(), 32);
    assert!(
        abi::RUNTIME_ID_HEX
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    assert_eq!(
        abi::sym::RUNTIME_ID_SYMBOL,
        format!("cheby_rt_id${}", abi::RUNTIME_ID_HEX)
    );
    // Every input is a file of the crate or one of the two workspace
    // files, never anything under `target/`.
    for input in &id.inputs {
        let text = input.to_string_lossy().replace('\\', "/");
        assert!(!text.contains("/target/"), "{text}");
        assert!(!text.contains("/.m3-work/"), "{text}");
        assert!(
            text.contains("/cheby_runtime/")
                || text.ends_with("/Cargo.lock")
                || text.ends_with("/rust-toolchain.toml"),
            "{text}"
        );
    }
}

#[test]
fn runtime_id_hex_is_lowercase_and_ordered() {
    let mut id = [0_u8; 16];
    id[0] = 0xab;
    id[15] = 0x0f;
    assert_eq!(hex(&id), "ab00000000000000000000000000000f");
}

/// A file of another crate, or anything under `target/`, leaves the ID
/// as it is.
#[test]
fn runtime_id_ignores_other_crates_and_target() {
    let fixture = Fixture::new();
    let plain = settings("aarch64-apple-darwin");
    let before = fixture.id(&plain);
    fixture.append("crates/cheby_codegen_clif/src/lib.rs", "\n// an edit\n");
    fixture.append("target/debug/stale.txt", "new");
    assert_eq!(fixture.id(&plain).id, before.id);
}

/// An edit to a runtime source, a new source file, the build script or
/// the manifest changes the ID.
#[test]
fn runtime_id_changes_with_runtime_sources() {
    let fixture = Fixture::new();
    let plain = settings("aarch64-apple-darwin");
    let mut seen = vec![fixture.id(&plain).id];
    for (file, text) in [
        ("crates/cheby_runtime/src/abi.rs", "\n// an edit\n"),
        ("crates/cheby_runtime/src/new_module.rs", "// a new file\n"),
        ("crates/cheby_runtime/build.rs", "\n// an edit\n"),
        ("crates/cheby_runtime/build/runtime_id.rs", "\n// an edit\n"),
        ("crates/cheby_runtime/Cargo.toml", "\n# an edit\n"),
    ] {
        fixture.append(file, text);
        let id = fixture.id(&plain).id;
        assert!(!seen.contains(&id), "{file}: the ID did not change");
        seen.push(id);
    }
}

/// `Cargo.lock`, the toolchain file, the target, the features and the
/// cfg values each change the ID.
#[test]
fn runtime_id_changes_with_the_lock_target_and_settings() {
    let fixture = Fixture::new();
    let plain = settings("aarch64-apple-darwin");
    let before = fixture.id(&plain).id;
    let mut seen = vec![before];
    let mut differs = |id: [u8; 16], what: &str| {
        assert!(!seen.contains(&id), "{what}: the ID did not change");
        seen.push(id);
    };
    differs(
        fixture.id(&settings("x86_64-unknown-linux-gnu")).id,
        "target",
    );
    let mut featured = plain.clone();
    featured.features.push("extra".to_owned());
    differs(fixture.id(&featured).id, "a feature");
    let mut release = plain.clone();
    release.cfgs.retain(|(name, _)| name != "debug_assertions");
    differs(fixture.id(&release).id, "debug_assertions");
    let mut sanitized = plain.clone();
    sanitized
        .cfgs
        .push(("cheby_sanitize".to_owned(), "address".to_owned()));
    differs(fixture.id(&sanitized).id, "cheby_sanitize");
    // A cfg the runtime does not read is not an input.
    let mut other = plain.clone();
    other.cfgs.push(("loom".to_owned(), String::new()));
    assert_eq!(fixture.id(&other).id, before);
    fixture.append("Cargo.lock", "\n# an edit\n");
    differs(fixture.id(&plain).id, "Cargo.lock");
    fixture.append("rust-toolchain.toml", "\n# an edit\n");
    differs(fixture.id(&plain).id, "rust-toolchain.toml");
}

/// Two copies at two absolute paths give one ID (D-264), and features
/// count as a set.
#[test]
fn runtime_id_is_independent_of_the_absolute_path() {
    let first = Fixture::new();
    let second = Fixture::new();
    assert_ne!(first.root, second.root);
    let plain = settings("aarch64-apple-darwin");
    assert_eq!(first.id(&plain).id, second.id(&plain).id);
    let mut a = plain.clone();
    a.features = vec!["b".to_owned(), "a".to_owned()];
    let mut b = plain;
    b.features = vec!["a".to_owned(), "b".to_owned(), "a".to_owned()];
    assert_eq!(first.id(&a).id, second.id(&b).id);
}
