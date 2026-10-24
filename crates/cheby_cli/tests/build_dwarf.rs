//! Line tables in executables (D-938): on Linux, `cheby build` of a copy
//! of 001 gives an executable whose DWARF holds rows of `src/main.cheby`,
//! whose Cheby compile unit names no absolute path and has no
//! `DW_AT_comp_dir`, in which no path of the package, its copies, the cache
//! or a temporary directory appears anywhere (every compile unit's
//! `DW_AT_name` and `DW_AT_comp_dir` and every line-table header's
//! directories and files, Rust's and C's included, D-934), and that is
//! identical when built from two paths under one basename. On macOS the
//! executable is linked with `-Wl,-S` and carries no debug map until M6
//! (D-938, D-1112): no `__DWARF` segment.
//!
//! Every test links against the runtime archive next to `cheby`
//! (`cargo build -p cheby_rt_exe` first, D-929); every `cheby` gets a
//! fresh `CHEBY_CACHE` (D-1128) and writes with `-o` into a temporary
//! directory. Windows builds no executable until M6 (D-935), so the file
//! holds no test there.

#![cfg(any(target_os = "linux", target_os = "macos"))]
#![expect(
    clippy::unwrap_used,
    reason = "test helpers: a failed setup step fails the test"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::{FreshDir, HarnessCache};
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TIMEOUT: Duration = Duration::from_secs(120);

/// 001, as `docs/examples` has it.
fn example_001() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/001_hello_world")
}

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

/// `cheby build -o <exe>` in the package `root` with the fresh cache
/// `cache` and the extra environment `env`.
fn build(root: &Path, exe: &Path, cache: &HarnessCache, env: &[(&str, &str)]) -> RunOutput {
    let mut cmd = Command::new(CHEBY);
    cmd.args(["build", "-o", exe.to_str().unwrap()])
        .current_dir(root)
        .env("NO_COLOR", "1");
    cache.apply(&mut cmd);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = process::run(cmd, None, TIMEOUT).unwrap();
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    out
}

/// What an executable's DWARF names, read with `gimli`.
#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct Dwarf {
    /// Each compile unit: its `DW_AT_name`, its `DW_AT_comp_dir`, and its
    /// line table's directories and files, each a path as written.
    units: Vec<Unit>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct Unit {
    name: String,
    comp_dir: Option<String>,
    dirs: Vec<String>,
    files: Vec<String>,
    /// The rows of the line table: (file name, line).
    rows: Vec<(String, u64)>,
}

#[cfg(target_os = "linux")]
impl Dwarf {
    fn read(bytes: &[u8]) -> Self {
        use object::{Object as _, ObjectSection as _};
        let file = object::File::parse(bytes).unwrap();
        let load = |id: gimli::SectionId| -> Result<std::borrow::Cow<'_, [u8]>, gimli::Error> {
            Ok(file
                .section_by_name(id.name())
                .and_then(|s| s.uncompressed_data().ok())
                .unwrap_or(std::borrow::Cow::Borrowed(&[][..])))
        };
        let owned = gimli::DwarfSections::load(load).unwrap();
        let dwarf = owned.borrow(|s| gimli::EndianSlice::new(s, gimli::LittleEndian));
        let mut out = Self::default();
        let mut headers = dwarf.units();
        while let Some(header) = headers.next().unwrap() {
            let unit = dwarf.unit(header).unwrap();
            let text = |value| {
                dwarf
                    .attr_string(&unit, value)
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            let mut entries = unit.entries();
            let Some(root) = entries.next_dfs().unwrap() else {
                continue;
            };
            let mut read = Unit {
                name: root
                    .attr_value(gimli::DW_AT_name)
                    .map(&text)
                    .unwrap_or_default(),
                comp_dir: root.attr_value(gimli::DW_AT_comp_dir).map(&text),
                ..Unit::default()
            };
            if let Some(program) = unit.line_program.clone() {
                let header = program.header().clone();
                for dir in header.include_directories() {
                    read.dirs.push(text(dir.clone()));
                }
                for file in header.file_names() {
                    read.files.push(text(file.path_name()));
                }
                let mut rows = program.rows();
                while let Some((header, row)) = rows.next_row().unwrap() {
                    if row.end_sequence() {
                        continue;
                    }
                    let name = row
                        .file(header)
                        .map(|f| text(f.path_name()))
                        .unwrap_or_default();
                    read.rows.push((name, row.line().map_or(0, |l| l.get())));
                }
            }
            out.units.push(read);
        }
        out
    }

    /// Every path the DWARF names: each unit's name and compilation
    /// directory, and its line table's directories and files.
    fn paths(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for unit in &self.units {
            out.push(unit.name.as_str());
            out.extend(unit.comp_dir.as_deref());
            out.extend(unit.dirs.iter().map(String::as_str));
            out.extend(unit.files.iter().map(String::as_str));
        }
        out
    }
}

/// D-938's done-when on Linux: rows of `src/main.cheby`; the Cheby compile
/// units without an absolute path or `DW_AT_comp_dir`; no path of the
/// package, its copies, the cache or a temporary directory anywhere in
/// the DWARF; two builds from two paths under one basename identical; and
/// `CHEBY_POSITIONS=off` gives an executable without Cheby's line table.
#[test]
#[cfg(target_os = "linux")]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_dwarf_linux_line_tables() {
    let tmp = FreshDir::create("cheby-build-dwarf").unwrap();
    let (a, b) = (
        tmp.path().join("a/hello_world"),
        tmp.path().join("b/c/hello_world"),
    );
    copy_dir(&example_001(), &a);
    copy_dir(&example_001(), &b);
    let (cache_a, cache_b) = (
        HarnessCache::create_with("cli-build-dwarf", None).unwrap(),
        HarnessCache::create_with("cli-build-dwarf", None).unwrap(),
    );
    let (exe_a, exe_b) = (
        tmp.path().join("x/hello_world"),
        tmp.path().join("y/z/hello_world"),
    );
    build(&a, &exe_a, &cache_a, &[]);
    build(&b, &exe_b, &cache_b, &[]);
    let bytes = std::fs::read(&exe_a).unwrap();
    assert!(bytes == std::fs::read(&exe_b).unwrap(), "two builds differ");
    let dwarf = Dwarf::read(&bytes);
    let cheby: Vec<&Unit> = dwarf
        .units
        .iter()
        .filter(|u| u.name.ends_with(".cheby"))
        .collect();
    let main = cheby
        .iter()
        .find(|u| u.name == "src/main.cheby")
        .unwrap_or_else(|| panic!("no unit of src/main.cheby: {:#?}", dwarf.units.len()));
    assert!(
        main.rows
            .iter()
            .any(|(f, line)| f.ends_with("main.cheby") && *line > 0),
        "{:?}",
        main.rows
    );
    for unit in &cheby {
        assert!(unit.comp_dir.is_none(), "{unit:?}");
        for path in [&unit.name]
            .into_iter()
            .chain(&unit.dirs)
            .chain(&unit.files)
        {
            assert!(!path.starts_with('/'), "{unit:?}");
        }
    }
    let forbidden = [
        tmp.path().to_string_lossy().into_owned(),
        cache_a.path().to_string_lossy().into_owned(),
        cache_b.path().to_string_lossy().into_owned(),
        std::env::temp_dir().to_string_lossy().into_owned(),
        "cheby-objects-".to_owned(),
        ".cheby-link-".to_owned(),
    ];
    for path in dwarf.paths() {
        for bad in &forbidden {
            assert!(!path.contains(bad.as_str()), "`{path}` names `{bad}`");
        }
    }
    // With positions off, no Cheby line table.
    let exe_off = tmp.path().join("off/hello_world");
    let cache_off = HarnessCache::create_with("cli-build-dwarf", None).unwrap();
    build(&a, &exe_off, &cache_off, &[("CHEBY_POSITIONS", "off")]);
    let off = Dwarf::read(&std::fs::read(&exe_off).unwrap());
    assert!(
        off.units.iter().all(|u| !u.name.ends_with(".cheby")),
        "{:?}",
        off.units.iter().map(|u| &u.name).collect::<Vec<_>>()
    );
}

/// D-938 and D-1112 on macOS: `-Wl,-S` keeps the objects' DWARF out of
/// the executable, which has no debug map until M6: no `__DWARF` segment
/// and no `__debug_*` section.
#[test]
#[cfg(target_os = "macos")]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_dwarf_macos_no_debug_map() {
    use object::{Object as _, ObjectSection as _, ObjectSegment as _};
    let tmp = FreshDir::create("cheby-build-dwarf").unwrap();
    let root = tmp.path().join("hello_world");
    copy_dir(&example_001(), &root);
    let cache = HarnessCache::create_with("cli-build-dwarf", None).unwrap();
    let exe = tmp.path().join("x/hello_world");
    build(&root, &exe, &cache, &[]);
    let bytes = std::fs::read(&exe).unwrap();
    let file = object::File::parse(&*bytes).unwrap();
    assert!(
        file.segments()
            .all(|s| s.name().ok().flatten() != Some("__DWARF")),
        "a __DWARF segment"
    );
    assert!(
        file.sections()
            .all(|s| !s.name().unwrap_or("").starts_with("__debug_")),
        "a __debug_ section"
    );
}
