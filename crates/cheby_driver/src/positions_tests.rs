//! Source positions through the driver (D-938): the switch
//! `CHEBY_POSITIONS` (GAP(positions-switch)), tested in a child process
//! with the variable on its `Command`, since no test sets the environment;
//! records with positions identical from two package paths; and the
//! records' sizes with positions on and off for CP2 (gate item 25, an
//! ignored test).

use std::path::{Path, PathBuf};
use std::process::Command;

use cheby_codegen_clif::record::CodeRecord;
use cheby_mir::MirOptions;

use super::*;
use crate::aot::compile_records;

/// Set in the child process of the switch's tests.
const CHILD: &str = "CHEBY_POSITIONS_TEST_CHILD";

/// The examples `cheby build` must build (D-1164), as `cargo xtask
/// check-determinism` lists them.
const MUST_BUILD: [&str; 8] = [
    "001_hello_world",
    "002_greet",
    "003_fizzbuzz",
    "004_tictactoe",
    "008_json",
    "024_two_sum",
    "025_linked_list",
    "032_aoc_2024_day_1",
];

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/examples")
        .join(name)
}

/// The code records of the program at `root`, by module path, compiled
/// with `mir` as `cheby build` compiles them (without a cache).
fn records(root: &Path, mir: MirOptions) -> Vec<(String, CodeRecord)> {
    let mut out = CheckOutput::default();
    let (front, package) = front_end(root, frontend::Mode::Build, &mut out)
        .unwrap()
        .unwrap_or_else(|| panic!("{}", out.render()));
    assert!(!out.has_errors(), "{}", out.render());
    let module = front.local_module(&ModulePath::root(&package)).unwrap();
    let main = front.main_of(module).unwrap();
    let lowered = backend::lower(&front, &out.sources, &backend::Roots::Main(main), mir).unwrap();
    let compiled = compile_records(&front, &lowered.units, main.def, mir).unwrap();
    compiled
        .code
        .iter()
        .zip(compiled.records)
        .map(|(unit, record)| (unit.path.to_string(), record))
        .collect()
}

/// The SipHash-1-3 (zero key) of `bytes`.
fn digest(bytes: &[u8]) -> u64 {
    use std::hash::Hasher as _;
    let mut h = siphasher::sip::SipHasher13::new_with_keys(0, 0);
    h.write(bytes);
    h.finish()
}

/// `record` without its positions: what it holds besides them.
fn without_positions(record: &CodeRecord) -> CodeRecord {
    let mut plain = record.clone();
    plain.source = None;
    for f in &mut plain.functions {
        f.positions.clear();
    }
    plain
}

/// What the child process found: the line it prints.
fn summary(mir: MirOptions) -> String {
    let records = records(&example("001_hello_world"), mir);
    let rows: usize = records
        .iter()
        .flat_map(|(_, r)| &r.functions)
        .map(|f| f.positions.len())
        .sum();
    let sources: Vec<&str> = records
        .iter()
        .filter_map(|(_, r)| r.source.as_deref())
        .collect();
    let mut code = Vec::new();
    for (_, record) in &records {
        code.extend(without_positions(record).encode());
    }
    format!(
        "SUMMARY lowering={} positions={} rows={rows} sources={} code={:#018x}",
        MirOptions::lowering_records_positions(),
        mir.positions,
        sources.join(","),
        digest(&code)
    )
}

/// The child: with `CHEBY_POSITIONS_TEST_CHILD` set, reads the MIR
/// options as every command does (`backend::mir_options_from_env`) and
/// prints what 001's records hold, or the error; without it, nothing.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn positions_switch_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    match backend::mir_options_from_env() {
        Ok(mir) => println!("{}", summary(mir)),
        Err(e) => println!("ERROR {e}"),
    }
}

/// The child's line with `CHEBY_POSITIONS` set to `value`, or unset.
fn child(value: Option<&str>) -> String {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "positions_tests::positions_switch_child",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .env_remove(MirOptions::POSITIONS_ENV)
        .env_remove(MirOptions::RC_ENV)
        .env_remove(MirOptions::UNWIND_EDGES_ENV);
    if let Some(value) = value {
        command.env(MirOptions::POSITIONS_ENV, value);
    }
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The test harness prints the test's name on the same line.
    stdout
        .lines()
        .find_map(|l| {
            l.find("SUMMARY ")
                .or_else(|| l.find("ERROR "))
                .and_then(|at| l.get(at..))
        })
        .unwrap_or_else(|| panic!("no summary in:\n{stdout}"))
        .to_owned()
}

/// The field `name=` of a summary line.
fn field<'a>(line: &'a str, name: &str) -> &'a str {
    line.split(' ')
        .find_map(|part| part.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name} in {line}"))
}

/// `CHEBY_POSITIONS=off` turns positions off for the whole build:
/// lowering records none, the records hold no rows and name no source;
/// `on` and unset keep them; the code is the same either way
/// (GAP(positions-switch), D-938).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn positions_switch_off_keeps_the_code() {
    let off = child(Some("off"));
    let on = child(Some("on"));
    let unset = child(None);
    assert_eq!(field(&off, "lowering"), "false", "{off}");
    assert_eq!(field(&off, "positions"), "false", "{off}");
    assert_eq!(field(&off, "rows"), "0", "{off}");
    assert_eq!(field(&off, "sources"), "", "{off}");
    for line in [&on, &unset] {
        assert_eq!(field(line, "lowering"), "true", "{line}");
        assert_eq!(field(line, "positions"), "true", "{line}");
        assert_ne!(field(line, "rows"), "0", "{line}");
        let sources = field(line, "sources");
        assert!(sources.split(',').any(|s| s == "src/main.cheby"), "{line}");
        assert!(sources.split(',').any(|s| s == "std/io.cheby"), "{line}");
    }
    assert_eq!(on, unset);
    assert_eq!(field(&off, "code"), field(&on, "code"));
}

/// Any other value is an error at the start of the build, as for
/// `CHEBY_RC` (D-742, D-746).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn positions_switch_other_values_are_errors() {
    for value in ["yes", "OFF", "0"] {
        assert_eq!(
            child(Some(value)),
            format!("ERROR CHEBY_POSITIONS must be `on` or `off`, not `{value}`")
        );
    }
    // Empty means unset, as for the other switches.
    assert!(child(Some("")).starts_with("SUMMARY lowering=true positions=true"));
}

/// Copies the directory `from` into `to`.
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            copy_dir(&path, &to.join(entry.file_name()));
        } else {
            std::fs::copy(&path, to.join(entry.file_name())).unwrap();
        }
    }
}

/// A unit compiled from two package paths gives one record (D-264,
/// D-915): its positions name the diagnostic path (D-421), never where
/// the package lies, and no absolute path reaches it.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn record_positions_two_paths_identical() {
    let base = std::env::temp_dir().join(format!("cheby-n7a-positions-{}", std::process::id()));
    let a = base.join("a").join("hello_world");
    let b = base.join("b").join("deeper").join("hello_world");
    copy_dir(&example("001_hello_world"), &a);
    copy_dir(&example("001_hello_world"), &b);
    let from_a = records(&a, MirOptions::default());
    let from_b = records(&b, MirOptions::default());
    let _ = std::fs::remove_dir_all(&base);
    assert_eq!(from_a.len(), from_b.len());
    for ((path, ra), (_, rb)) in from_a.iter().zip(&from_b) {
        let (ea, eb) = (ra.encode(), rb.encode());
        assert_eq!(ea, eb, "{path}");
        let base_text = base.to_string_lossy();
        let text = String::from_utf8_lossy(&ea);
        assert!(!text.contains(&*base_text), "{path}");
        let root = env!("CARGO_MANIFEST_DIR");
        assert!(!text.contains(root), "{path}");
        assert!(
            ra.source.as_deref().is_none_or(|s| !s.starts_with('/')),
            "{path}"
        );
    }
    let hello = &from_a.iter().find(|(p, _)| p == "hello_world").unwrap().1;
    assert_eq!(hello.source.as_deref(), Some("src/main.cheby"));
    assert!(hello.functions.iter().any(|f| !f.positions.is_empty()));
}

/// Gate item 25: the encoded code records with positions on and off,
/// 001's `hello_world` and `std::io`, and the total over every unit of the
/// eight `MUST_BUILD` examples (D-1164). Informative, for CP2's choice
/// (D-938); `--ignored --nocapture` prints the table.
#[test]
#[ignore = "informative sizes for gate item 25; run with --ignored --nocapture"]
fn positions_record_sizes() {
    let off = MirOptions {
        positions: false,
        ..MirOptions::default()
    };
    let size = |records: &[(String, CodeRecord)], path: &str| {
        records
            .iter()
            .find(|(p, _)| p == path)
            .map_or(0, |(_, r)| r.encode().len())
    };
    let hello_on = records(&example("001_hello_world"), MirOptions::default());
    let hello_off = records(&example("001_hello_world"), off);
    for path in ["hello_world", "std::io"] {
        println!(
            "RECORD-SIZE 001 {path}: {} bytes with positions, {} without",
            size(&hello_on, path),
            size(&hello_off, path)
        );
    }
    let (mut total_on, mut total_off, mut units) = (0_usize, 0_usize, 0_usize);
    for name in MUST_BUILD {
        let on = records(&example(name), MirOptions::default());
        let off = records(&example(name), off);
        let on_bytes: usize = on.iter().map(|(_, r)| r.encode().len()).sum();
        let off_bytes: usize = off.iter().map(|(_, r)| r.encode().len()).sum();
        println!(
            "RECORD-SIZE {name}: {} units, {on_bytes} bytes with positions, {off_bytes} without",
            on.len()
        );
        total_on += on_bytes;
        total_off += off_bytes;
        units += on.len();
    }
    let permille = (total_on - total_off) * 1000 / total_off.max(1);
    println!(
        "RECORD-SIZE total over MUST_BUILD: {units} units, {total_on} bytes with positions, \
         {total_off} without ({}.{}% more)",
        permille / 10,
        permille % 10
    );
}
