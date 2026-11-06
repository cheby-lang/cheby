//! Computes the build ID (D-957) and writes `$OUT_DIR/build_id.rs`, which
//! `src/build_id.rs` includes.
//!
//! The ID is SipHash-1-3 128 with zero keys over a canonical encoding: a
//! sequence of fields, each its length in bytes (a little-endian `u64`)
//! and its bytes. The fields are a version tag; the cargo profile, the
//! target, the `debug_assertions` and `cheby_sanitize` cfgs and the runtime
//! ID (D-930, from `cheby_runtime`'s build script as
//! `DEP_CHEBY_RUNTIME_RUNTIME_ID`), each after its name; the number of
//! files; and every input file's workspace-relative path (with `/`) and
//! bytes, sorted by path. The inputs are every file under `crates/` and
//! `std/`, `Cargo.lock` and `rust-toolchain.toml`. No absolute path,
//! modification time or directory order enters it (D-264).
//!
//! D-1116: files and directories whose names start with `.`
//! (editor and Finder droppings such as `.DS_Store`) are not inputs.
//!
//! `src/build_id.rs`'s tests include this file through `#[path]`, so the
//! hashing is tested as the build runs it. The script reads nothing under
//! `target/` or `.m*-work/`.

use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use siphasher::sip128::{Hasher128 as _, SipHasher13};

/// The directories whose every file is an input, relative to the
/// workspace.
const INPUT_DIRS: [&str; 2] = ["crates", "std"];
/// The single files that are inputs, relative to the workspace.
const INPUT_FILES: [&str; 2] = ["Cargo.lock", "rust-toolchain.toml"];
/// The encoding's version: a change of the encoding changes every ID.
const TAG: &str = "cheby-build-id-v1";

/// The build's settings that enter the ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    /// Cargo's `PROFILE`.
    pub(crate) profile: String,
    /// Cargo's `TARGET`.
    pub(crate) target: String,
    /// Whether `debug_assertions` is set.
    pub(crate) debug_assertions: bool,
    /// The `cheby_sanitize` cfg's value, empty when unset.
    pub(crate) sanitize: String,
    /// The runtime ID's 32 hex digits (D-930).
    pub(crate) runtime_id: String,
}

/// The workspace-relative paths of every input under `workspace`, with
/// `/` as the separator, sorted.
///
/// # Errors
///
/// When a directory cannot be read or an input file is missing.
pub(crate) fn input_paths(workspace: &Path) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    for dir in INPUT_DIRS {
        walk(&workspace.join(dir), dir, &mut out)?;
    }
    for file in INPUT_FILES {
        if !workspace.join(file).is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{}: missing", workspace.join(file).display()),
            ));
        }
        out.push(file.to_owned());
    }
    out.sort();
    Ok(out)
}

/// Adds the files under `dir`, whose relative path is `rel`, to `out`.
fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let child = format!("{rel}/{name}");
        // `metadata` follows a link, so a linked file or directory counts
        // as what it points at.
        let meta = fs::metadata(entry.path())?;
        if meta.is_dir() {
            walk(&entry.path(), &child, out)?;
        } else if meta.is_file() {
            out.push(child);
        }
    }
    Ok(())
}

/// The build ID of the inputs under `workspace` with `settings`.
///
/// # Errors
///
/// When an input cannot be read.
pub(crate) fn compute(workspace: &Path, settings: &Settings) -> io::Result<[u8; 16]> {
    let mut h = SipHasher13::new_with_keys(0, 0);
    field(&mut h, TAG.as_bytes());
    for (name, value) in [
        ("profile", settings.profile.as_str()),
        ("target", settings.target.as_str()),
        (
            "debug_assertions",
            if settings.debug_assertions { "1" } else { "0" },
        ),
        ("cheby_sanitize", settings.sanitize.as_str()),
        ("runtime_id", settings.runtime_id.as_str()),
    ] {
        field(&mut h, name.as_bytes());
        field(&mut h, value.as_bytes());
    }
    let paths = input_paths(workspace)?;
    let count = u64::try_from(paths.len()).unwrap_or(u64::MAX);
    field(&mut h, &count.to_le_bytes());
    for rel in &paths {
        field(&mut h, rel.as_bytes());
        let bytes = fs::read(workspace.join(rel))?;
        field(&mut h, &bytes);
    }
    Ok(h.finish128().as_bytes())
}

/// Appends one length-prefixed field.
fn field(h: &mut SipHasher13, bytes: &[u8]) {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    std::hash::Hasher::write(h, &len.to_le_bytes());
    std::hash::Hasher::write(h, bytes);
}

/// A Rust array expression of `id`'s bytes.
fn array_literal(id: &[u8; 16]) -> String {
    let bytes: Vec<String> = id.iter().map(|b| format!("0x{b:02x}")).collect();
    format!("[{}]\n", bytes.join(", "))
}

fn main() -> Result<(), Box<dyn Error>> {
    let crate_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("no CARGO_MANIFEST_DIR")?);
    let workspace = crate_dir.join("..").join("..");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("no OUT_DIR")?);
    let settings = Settings {
        profile: env::var("PROFILE")?,
        target: env::var("TARGET")?,
        debug_assertions: env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some(),
        sanitize: env::var("CARGO_CFG_CHEBY_SANITIZE").unwrap_or_default(),
        runtime_id: env::var("DEP_CHEBY_RUNTIME_RUNTIME_ID").map_err(|e| {
            format!("DEP_CHEBY_RUNTIME_RUNTIME_ID (from cheby_runtime's build script): {e}")
        })?,
    };
    for dir in INPUT_DIRS.iter().chain(&INPUT_FILES) {
        println!("cargo::rerun-if-changed=../../{dir}");
    }
    let id = compute(&workspace, &settings)?;
    fs::write(out_dir.join("build_id.rs"), array_literal(&id))?;
    Ok(())
}
