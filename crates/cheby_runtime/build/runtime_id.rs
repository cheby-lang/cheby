//! The runtime ID (D-930): SipHash-1-3 128, with zero keys, of everything
//! the runtime archive is built from. `build.rs` includes this file, and
//! so do the `runtime_id_*` tests (`src/runtime_id_tests.rs`), through
//! `#[path]`, so both run the same code.
//!
//! The inputs, in this order, each field written as its length (a
//! little-endian `u64`) followed by its bytes:
//!
//! 1. the version line [`ENCODING`];
//! 2. per file of the crate, sorted by its path relative to the crate
//!    (with `/` between components): `file`, the path, the bytes. The
//!    files are every file under `src/` and `build/`, `build.rs` and
//!    `Cargo.toml`. Names that start with `.` (editor and Finder files)
//!    are skipped;
//! 3. `lock` and the workspace's `Cargo.lock`, then `toolchain` and its
//!    `rust-toolchain.toml`;
//! 4. `target` and the target triple;
//! 5. per enabled feature, sorted: `feature` and its name;
//! 6. per cfg of [`CFGS`] that is set, in that order: `cfg`, the name and
//!    its value (empty for a name without one; several values are joined
//!    by `,` as cargo passes them).
//!
//! No absolute path, modification time or directory order enters the
//! hash, so two checkouts of one tree give one ID (D-264). Nothing under
//! `target/` or any other directory is read.

use std::fs;
use std::hash::Hasher as _;
use std::io;
use std::path::{Path, PathBuf};

use siphasher::sip128::{Hasher128 as _, SipHasher13};

/// The first field: names this encoding, so a change of the encoding
/// changes every ID.
pub const ENCODING: &str = "cheby-runtime-id/1";

/// The cfg names the runtime's code reads whose values the target does
/// not fix: `debug_assertions` (the leak check, poisoning), D-875's
/// `cheby_sanitize`, `panic` (D-697 needs `unwind`) and `miri`.
pub const CFGS: [&str; 4] = ["debug_assertions", "cheby_sanitize", "panic", "miri"];

/// The directories of the crate whose files are inputs, relative to it.
pub const CRATE_DIRS: [&str; 2] = ["src", "build"];

/// The single files of the crate that are inputs, relative to it.
pub const CRATE_FILES: [&str; 2] = ["build.rs", "Cargo.toml"];

/// The workspace's files that are inputs, relative to the workspace, with
/// the label that precedes each.
pub const WORKSPACE_FILES: [(&str, &str); 2] =
    [("lock", "Cargo.lock"), ("toolchain", "rust-toolchain.toml")];

/// What the ID hashes besides files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The target triple (`TARGET`).
    pub target: String,
    /// The enabled features, in any order: lowercase names as
    /// `CARGO_FEATURE_*` spells them after the prefix.
    pub features: Vec<String>,
    /// The values of the cfgs of [`CFGS`] that are set, by name.
    pub cfgs: Vec<(String, String)>,
}

/// A computed runtime ID and the files it read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeId {
    /// The ID.
    pub id: [u8; 16],
    /// Every file read, for `cargo::rerun-if-changed`.
    pub inputs: Vec<PathBuf>,
}

impl RuntimeId {
    /// The ID as 32 lowercase hex digits.
    #[must_use]
    pub fn hex(&self) -> String {
        hex(&self.id)
    }
}

/// `id` as 32 lowercase hex digits.
#[must_use]
pub fn hex(id: &[u8; 16]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(32);
    for byte in id {
        for nibble in [byte >> 4, byte & 0xf] {
            let digit = DIGITS.get(usize::from(nibble)).copied().unwrap_or(b'0');
            out.push(char::from(digit));
        }
    }
    out
}

/// Computes the runtime ID of the crate at `crate_dir` in the workspace
/// at `workspace_dir` (D-930).
///
/// # Errors
///
/// A file or directory that cannot be read, a path that is not UTF-8, or
/// a missing workspace file.
pub fn compute(
    crate_dir: &Path,
    workspace_dir: &Path,
    settings: &Settings,
) -> io::Result<RuntimeId> {
    let mut files = Vec::new();
    for dir in CRATE_DIRS {
        let path = crate_dir.join(dir);
        if path.is_dir() {
            walk(&path, dir, &mut files)?;
        }
    }
    for file in CRATE_FILES {
        files.push((file.to_owned(), crate_dir.join(file)));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = SipHasher13::new_with_keys(0, 0);
    field(&mut hasher, ENCODING.as_bytes());
    let mut inputs = Vec::with_capacity(files.len().saturating_add(2));
    for (relative, path) in files {
        let bytes = read(&path)?;
        field(&mut hasher, b"file");
        field(&mut hasher, relative.as_bytes());
        field(&mut hasher, &bytes);
        inputs.push(path);
    }
    for (label, file) in WORKSPACE_FILES {
        let path = workspace_dir.join(file);
        let bytes = read(&path)?;
        field(&mut hasher, label.as_bytes());
        field(&mut hasher, &bytes);
        inputs.push(path);
    }
    field(&mut hasher, b"target");
    field(&mut hasher, settings.target.as_bytes());
    let mut features = settings.features.clone();
    features.sort();
    features.dedup();
    for feature in &features {
        field(&mut hasher, b"feature");
        field(&mut hasher, feature.as_bytes());
    }
    for name in CFGS {
        for (cfg, value) in &settings.cfgs {
            if cfg == name {
                field(&mut hasher, b"cfg");
                field(&mut hasher, cfg.as_bytes());
                field(&mut hasher, value.as_bytes());
            }
        }
    }
    Ok(RuntimeId {
        id: hasher.finish128().as_bytes(),
        inputs,
    })
}

/// Writes one length-prefixed field.
fn field(hasher: &mut SipHasher13, bytes: &[u8]) {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    hasher.write(&len.to_le_bytes());
    hasher.write(bytes);
}

/// Reads a file, naming it in the error.
fn read(path: &Path) -> io::Result<Vec<u8>> {
    fs::read(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("runtime ID: cannot read {}: {error}", path.display()),
        )
    })
}

/// Adds every file under `dir`, whose path relative to the crate is
/// `relative`, to `out` as (relative path, path), skipping names that
/// start with `.`.
fn walk(dir: &Path, relative: &str, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    let entries = fs::read_dir(dir).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("runtime ID: cannot list {}: {error}", dir.display()),
        )
    })?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "runtime ID: {} has a name that is not UTF-8",
                    entry.path().display()
                ),
            ));
        };
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let child = format!("{relative}/{name}");
        // `metadata` follows symbolic links, as reading does.
        if fs::metadata(&path)?.is_dir() {
            walk(&path, &child, out)?;
        } else {
            out.push((child, path));
        }
    }
    Ok(())
}
