//! The link (D-933, D-934, D-936): `cc` from `PATH` turns the module
//! objects, the program object and the runtime archive into an executable
//! for the host.
//!
//! D-1112:
//!
//! - `cc` is looked up in `PATH`, with `CC`, `LD`, `CFLAGS`, `LDFLAGS` and
//!   `MACOSX_DEPLOYMENT_TARGET` removed from its environment, so nothing
//!   but `PATH` reaches the link from the environment (D-264).
//! - Inputs, in this order: the module objects in build order, the program
//!   object, the package's C objects in `sources` order, the runtime
//!   archive, the package's `libraries` as `-l<name>` in manifest order,
//!   then the archive's system libraries ([`crate::native_libs`]): the
//!   package's native code comes where D-1112's line leaves room for it
//!   (D-940, D-1198).
//! - Flags: on Linux `-pie -Wl,--gc-sections -Wl,-z,start-stop-gc
//!   -Wl,--eh-frame-hdr -Wl,-z,relro,-z,now -Wl,--build-id=sha1`
//!   (`-z start-stop-gc` since D-1253, so the program object's
//!   `__start_`/`__stop_` references keep no record entries alive,
//!   `crate::gc`); on macOS `-arch arm64 -mmacosx-version-min=<the
//!   archive's minimum> -Wl,-dead_strip -Wl,-S` (the minimum is 11.0; no
//!   debug map).
//! - The objects are written into a fresh private temporary directory,
//!   each named after its module path (`hello_world.o`, `std::io.o`; the
//!   program object `cb$program.o`), and `cc` runs in that directory with
//!   relative names, so no path of the build reaches the command line but
//!   the archive's and the output's.
//! - The linker writes into a fresh private directory created inside the
//!   output directory, under the output's final basename, since ld64
//!   derives `LC_UUID` and the signature's identifier from it (D-933,
//!   D-934); the file is then renamed into place, on one file system. Both
//!   directories are removed on every path that returns or unwinds; a
//!   signal that takes its default action (Ctrl-C, SIGTERM) during the
//!   link ends the process without removing them, as D-1127 says of
//!   temporary caches.
//! - So, on D-1127's model, right before creating either directory the
//!   link deletes the directories of that pattern whose process is dead
//!   (`sweep_dead`): in the temporary directory every
//!   `cheby-objects-<pid>-<n>`, and in the output directory every
//!   `.cheby-link-<pid>-<n>`, that is a directory (a symbolic link is never
//!   followed or deleted), that the effective user owns, whose `pid` is
//!   not this process's, and whose process no longer exists (`kill(pid,
//!   0)` fails with `ESRCH`). This process's directories, a live
//!   process's, another user's and anything else are never touched, and
//!   every failure is ignored: the sweep only tidies up, and runs on every
//!   link, also one that fails later. A process in another pid namespace
//!   that shares the directory looks dead, and its directories may go
//!   (D-1127). Windows sweeps nothing until M6 (D-1127, D-935).
//! - A `cc` that cannot be started and a failed link are tool errors that
//!   show the linker's output, which is never parsed (D-933).
//!
//! [`check_copies`] is D-918's check over the copies of a link, which
//! `cheby build` runs in debug toolchains before linking.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use cheby_codegen_clif::record::{CodeRecord, RecordContents, SymbolLinkage};

use crate::LinkError;
use crate::native_libs;

/// The environment variables removed from `cc`'s environment (D-933).
pub const REMOVED_VARS: [&str; 5] = ["CC", "LD", "CFLAGS", "LDFLAGS", "MACOSX_DEPLOYMENT_TARGET"];

/// The C compiler that links, looked up in `PATH` (D-933).
pub const CC: &str = "cc";

/// The prefix of the objects' directory in the temporary directory
/// (D-1112).
pub(crate) const OBJECTS_PREFIX: &str = "cheby-objects";

/// The prefix of the staging directory in the output directory (D-1112).
pub(crate) const STAGING_PREFIX: &str = ".cheby-link";

/// What one link takes.
#[derive(Clone, Copy, Debug)]
pub struct LinkJob<'a> {
    /// `(module path, object bytes)`: the module objects in build order,
    /// then the program object under
    /// [`crate::program::PROGRAM_OBJECT_NAME`].
    pub objects: &'a [(String, Vec<u8>)],
    /// The package's C objects, in `sources` order, linked after the
    /// program object (D-940, D-1198): `(source, object
    /// bytes)`, the source only naming nothing but the object's place in
    /// the list.
    pub c_objects: &'a [(String, Vec<u8>)],
    /// The package's `libraries`, as `-l<name>` before the archive's
    /// system libraries (D-940, D-1198).
    pub libraries: &'a [String],
    /// The runtime archive, checked with
    /// [`crate::archive::check_runtime_id`] by the caller.
    pub archive: &'a Path,
    /// The executable to write: an absolute path whose directory exists.
    pub output: &'a Path,
}

/// The kind of host a link line is for (module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkHost {
    /// macOS: ld64 through `cc`.
    MacOs,
    /// Linux: GNU ld (or lld) through `cc`.
    Linux,
}

impl LinkHost {
    /// The host this `cheby` links for.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }
}

/// The flags of the link line for the host, before the output and the
/// inputs (module documentation).
#[must_use]
pub fn flags() -> Vec<String> {
    flags_for(LinkHost::current())
}

/// The flags of the link line for `host` (module documentation). On Linux,
/// `-z start-stop-gc` keeps the program object's `__start_`/`__stop_`
/// references from keeping the record entries' sections, and so every
/// function, alive (D-1253; GNU ld 2.37, lld 13).
#[must_use]
pub fn flags_for(host: LinkHost) -> Vec<String> {
    match host {
        LinkHost::MacOs => {
            let arch = if cfg!(target_arch = "aarch64") {
                "arm64"
            } else {
                std::env::consts::ARCH
            };
            let mut flags = vec!["-arch".to_owned(), arch.to_owned()];
            if let Some(min) = native_libs::macos_min_version() {
                flags.push(format!("-mmacosx-version-min={min}"));
            }
            flags.extend(["-Wl,-dead_strip", "-Wl,-S"].map(str::to_owned));
            flags
        }
        LinkHost::Linux => [
            "-pie",
            "-Wl,--gc-sections",
            START_STOP_GC,
            "-Wl,--eh-frame-hdr",
            "-Wl,-z,relro,-z,now",
            "-Wl,--build-id=sha1",
        ]
        .map(str::to_owned)
        .to_vec(),
    }
}

/// D-1253's flag on D-1112's Linux link line. It applies to the whole
/// line, so the package's own C objects (`[native]` sources) and the
/// archive's members link under it too: a C section registry read only
/// through `__start_`/`__stop_` (entries marked `used` but not `retain`)
/// is dropped by `--gc-sections` under `cheby build` on Linux, where GNU
/// ld's default would keep it, while `cheby run`'s shared library (no GC,
/// `crate::shlib`) keeps it (the review's SOUND-1). D-1288: the flag stays
/// on the whole line, and package C code marks such entries
/// `__attribute__((retain))` (§12.5.4).
pub const START_STOP_GC: &str = "-Wl,-z,start-stop-gc";

/// The command that links `names` (the objects' files in `objects`, in
/// order) with `job`'s archive and libraries into `staged`, for `host`
/// (module documentation): what [`link`] runs.
#[must_use]
pub fn command(
    host: LinkHost,
    job: &LinkJob<'_>,
    objects: &Path,
    staged: &Path,
    names: &[String],
) -> Command {
    let mut cmd = Command::new(CC);
    for var in REMOVED_VARS {
        cmd.env_remove(var);
    }
    cmd.current_dir(objects)
        .args(flags_for(host))
        .arg("-o")
        .arg(staged)
        .args(names)
        .arg(job.archive)
        .args(job.libraries.iter().map(|name| format!("-l{name}")))
        .args(native_libs::link_args());
    cmd
}

/// The file name of an object in the temporary directory: its module
/// path's, which no build path enters.
fn object_file(module: &str) -> String {
    format!("{module}.o")
}

/// The file name of the package's C object at `index` of `sources` in the
/// temporary directory (D-1198).
pub(crate) fn native_object_file(index: usize) -> String {
    format!("native-{index}.o")
}

/// Links `job` into `job.output` (module documentation).
///
/// # Errors
///
/// [`LinkError::NoCc`] when `cc` cannot be started,
/// [`LinkError::LinkFailed`] with the linker's output when it fails, and
/// [`LinkError::Io`] when a directory or file cannot be written or the
/// output cannot be renamed into place.
pub fn link(job: &LinkJob<'_>) -> Result<(), LinkError> {
    let io = |what: &str, path: &Path, e: std::io::Error| {
        LinkError::Io(format!("cannot {what} `{}`: {e}", path.display()))
    };
    let out_dir = job
        .output
        .parent()
        .ok_or_else(|| LinkError::Io(format!("`{}` has no directory", job.output.display())))?;
    let name = job
        .output
        .file_name()
        .ok_or_else(|| LinkError::Io(format!("`{}` has no file name", job.output.display())))?;
    let tmp = std::env::temp_dir();
    sweep_dead(&tmp, OBJECTS_PREFIX);
    let objects = PrivateDir::create(&tmp, OBJECTS_PREFIX)?;
    let mut names = Vec::with_capacity(job.objects.len());
    for (module, bytes) in job.objects {
        let file = object_file(module);
        let path = objects.path.join(&file);
        std::fs::write(&path, bytes).map_err(|e| io("write", &path, e))?;
        names.push(file);
    }
    // The package's C objects follow the program object, each under its
    // place in `sources`: no module path holds `-`, so no name collides
    // (D-1198).
    for (index, (_, bytes)) in job.c_objects.iter().enumerate() {
        let file = native_object_file(index);
        let path = objects.path.join(&file);
        std::fs::write(&path, bytes).map_err(|e| io("write", &path, e))?;
        names.push(file);
    }
    sweep_dead(out_dir, STAGING_PREFIX);
    let staging = PrivateDir::create(out_dir, STAGING_PREFIX)?;
    let staged = staging.path.join(name);
    let mut cmd = command(LinkHost::current(), job, &objects.path, &staged, &names);
    let output = cmd.output().map_err(|e| LinkError::NoCc(e.to_string()))?;
    if !output.status.success() {
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        return Err(LinkError::LinkFailed {
            status: output.status.to_string(),
            output: text,
        });
    }
    std::fs::rename(&staged, job.output).map_err(|e| io("write", job.output, e))?;
    Ok(())
}

/// A fresh directory only its owner can read, removed with everything in
/// it when dropped.
pub(crate) struct PrivateDir {
    path: PathBuf,
}

impl PrivateDir {
    /// The directory.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Creates `<parent>/<prefix>-<pid>-<n>` for the first `n` whose
    /// directory does not exist yet.
    pub(crate) fn create(parent: &Path, prefix: &str) -> Result<Self, LinkError> {
        let pid = std::process::id();
        for n in 0_u32..10_000 {
            let mut name = OsString::from(prefix);
            name.push(format!("-{pid}-{n}"));
            let path = parent.join(name);
            match create_private(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(LinkError::Io(format!(
                        "cannot create `{}`: {e}",
                        path.display()
                    )));
                }
            }
        }
        Err(LinkError::Io(format!(
            "cannot create a fresh directory in `{}`",
            parent.display()
        )))
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        // Nothing to report if it fails: the directory is the command's own.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

/// D-1112, on D-1127's model: deletes every `<prefix>-<pid>-<n>`
/// directory directly under `parent` that the effective user owns and
/// whose process is dead, ignoring every failure (module documentation).
#[cfg(unix)]
pub(crate) fn sweep_dead(parent: &Path, prefix: &str) {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    // SAFETY: `geteuid` takes no arguments, cannot fail and touches no
    // memory of ours.
    let euid = unsafe { libc::geteuid() };
    let own = std::process::id();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| leftover_pid(name, prefix))
        else {
            continue;
        };
        if pid == own {
            continue;
        }
        let path = entry.path();
        // `symlink_metadata` never follows a symbolic link, so a link is
        // not a directory here and is never deleted.
        let owned_dir =
            std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir() && m.uid() == euid);
        if !owned_dir || process_alive(pid) {
            continue;
        }
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// Windows: nothing is swept until M6 (D-1127, D-935).
#[cfg(not(unix))]
pub(crate) fn sweep_dead(_parent: &Path, _prefix: &str) {}

/// The process id in a link directory's name `<prefix>-<pid>-<n>`, two
/// non-empty runs of ASCII digits, or `None` for any other name.
#[cfg(unix)]
fn leftover_pid(name: &str, prefix: &str) -> Option<u32> {
    let (pid, n) = name
        .strip_prefix(prefix)?
        .strip_prefix('-')?
        .split_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if digits(pid) && digits(n) {
        pid.parse().ok()
    } else {
        None
    }
}

/// Whether the process `pid` exists: `kill(pid, 0)` succeeds or fails
/// with anything but `ESRCH`, such as `EPERM` for another user's process.
/// A pid that `pid_t` cannot hold, or 0, counts as alive, so it is never
/// swept.
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    if pid <= 0 {
        return true;
    }
    // SAFETY: signal 0 sends nothing: `kill` only checks that the process
    // exists and may be signalled, and touches no memory of ours. `pid` is
    // positive, so it names one process, never a process group.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// D-918's check over the copies of a link: every name that several
/// records define `Local` has the same kind, contents, alignment and
/// relocations (by target name) in each. `records` pairs each record with
/// the module path of its object. Objects are replays of the records
/// (D-915), so equal records give equal copies.
///
/// # Errors
///
/// [`LinkError::Copies`] naming the first symbol whose copies differ.
pub fn check_copies(records: &[(&str, &CodeRecord)]) -> Result<(), LinkError> {
    let mut seen: BTreeMap<&str, (&str, Copy<'_>)> = BTreeMap::new();
    for &(module, record) in records {
        for (index, symbol) in (0_u32..).zip(&record.symbols) {
            if symbol.linkage != SymbolLinkage::Local {
                continue;
            }
            let copy = Copy::of(record, index);
            match seen.get(symbol.name.as_str()) {
                Some((first, theirs)) if *theirs != copy => {
                    return Err(LinkError::Copies(format!(
                        "the local copies of `{}` in `{first}` and `{module}` differ (D-918)",
                        symbol.name
                    )));
                }
                Some(_) => {}
                None => {
                    seen.insert(&symbol.name, (module, copy));
                }
            }
        }
    }
    Ok(())
}

/// What a local definition's copies must agree on.
#[derive(PartialEq, Eq)]
struct Copy<'a> {
    kind: cheby_codegen_clif::record::SymbolKind,
    align: Option<u64>,
    bytes: Option<&'a [u8]>,
    zeros: Option<u64>,
    /// `(offset, kind, target name, addend)`; the kind is `None` for data.
    relocs: Vec<(u32, Option<String>, Option<&'a str>, i64)>,
}

impl<'a> Copy<'a> {
    fn of(record: &'a CodeRecord, index: u32) -> Self {
        let name = |symbol: u32| record.symbol_at(symbol).map(|s| s.name.as_str());
        let kind = record
            .symbol_at(index)
            .map_or(cheby_codegen_clif::record::SymbolKind::Function, |s| s.kind);
        if let Some(function) = record.functions.iter().find(|f| f.symbol == index) {
            return Self {
                kind,
                align: Some(function.align),
                bytes: Some(&function.bytes),
                zeros: None,
                relocs: function
                    .relocs
                    .iter()
                    .map(|r| {
                        (
                            r.offset,
                            Some(format!("{:?}", r.kind)),
                            name(r.symbol),
                            r.addend,
                        )
                    })
                    .collect(),
            };
        }
        if let Some(data) = record.data.iter().find(|d| d.symbol == index) {
            let (bytes, zeros) = match &data.contents {
                RecordContents::Bytes(bytes) => (Some(bytes.as_slice()), None),
                RecordContents::Zeros(size) => (None, Some(*size)),
            };
            return Self {
                kind,
                align: data.align,
                bytes,
                zeros,
                relocs: data
                    .relocs
                    .iter()
                    .map(|r| (r.offset, None, name(r.symbol), r.addend))
                    .collect(),
            };
        }
        Self {
            kind,
            align: None,
            bytes: None,
            zeros: None,
            relocs: Vec::new(),
        }
    }
}
