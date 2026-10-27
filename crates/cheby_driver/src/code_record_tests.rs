//! Code records in the store (K4c: D-914, D-1192, D-1193, D-1195, D-965,
//! D-1179): a warm command loads its package units from their records
//! without lowering them, an edit misses only its module, a stale record is
//! a miss and is written again, a stale lowering hint is rewritten, dumps
//! and `--reuse-report` read no code record, and a tests unit loaded from
//! its record serves every selection.
//!
//! Every test opens its own store in a fresh directory (D-1128) and reads
//! the lookups through `CHEBY_CACHE_LOG`'s file (D-973).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use cheby_iface::{ModuleId, PackageId};
use cheby_mir::UnitKind;

use super::*;
use crate::backend::{Placed, Reading, Roots, Scope};
use crate::cache::{BuildId, CacheLog, Store};

/// The build ID of every test store.
const BUILD: BuildId = BuildId([11; 16]);

/// The length of an entry file's header (`cache/store.rs`), before the
/// payload.
const HEADER_LEN: usize = 32;

/// A fresh directory under the system's temporary directory, removed on
/// drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cheby_driver_code_record_{}_{n}_{name}",
            std::process::id()
        ));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A package `app` in a fresh directory, and a fresh cache.
struct Pkg {
    package: TempDir,
    cache: TempDir,
    runs: u32,
}

impl Pkg {
    fn new(name: &str, files: &[(&str, &str)]) -> Self {
        let package = TempDir::new(name);
        std::fs::write(
            package.path().join("cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let pkg = Self {
            package,
            cache: TempDir::new(&format!("{name}_cache")),
            runs: 0,
        };
        for (path, text) in files {
            pkg.write(path, text);
        }
        pkg
    }

    fn dir(&self) -> &Path {
        self.package.path()
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.dir().join("src").join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn root(&self) -> PathBuf {
        self.cache.path().join("cache")
    }

    fn open(&mut self) -> (Cache, PathBuf) {
        self.runs = self.runs.checked_add(1).unwrap();
        let log = self.cache.path().join(format!("log-{}", self.runs));
        let store = Store::open(&self.root(), BUILD).unwrap();
        (
            Cache::with_store(store, CacheLog::new(Some(log.clone()))),
            log,
        )
    }

    /// `cheby run`'s compile through the cache, with `options`: the log's
    /// lines.
    fn run_with(&mut self, options: RunOptions) -> Vec<String> {
        let (cache, log) = self.open();
        let prepared = prepare_run_cached(self.dir(), options, &cache).unwrap();
        cache.finish(false).unwrap();
        assert!(matches!(prepared, Prepared::Ready(_)));
        read_log(&log)
    }

    fn run(&mut self) -> Vec<String> {
        self.run_with(RunOptions::default())
    }

    /// `cheby test`'s compile through the cache, then its run: the log's
    /// lines and the report.
    fn test(&mut self, filter: Option<&str>) -> (Vec<String>, String) {
        let (cache, log) = self.open();
        let options = TestOptions {
            filter: filter.map(str::to_owned),
            ..TestOptions::default()
        };
        let prepared = prepare_test_cached(self.dir(), &options, &cache).unwrap();
        cache.finish(false).unwrap();
        let PreparedTests::Ready(program) = prepared else {
            panic!("the tests did not compile");
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let report = program.run(&mut out, &mut err).unwrap();
        (read_log(&log), report.render())
    }

    /// The warm lowering of `main` through the cache, as `cheby run` makes
    /// it (D-914): which units it lowered and which it loads.
    fn lower_main(&mut self) -> (Vec<UnitKind>, Vec<(UnitKind, bool)>, Vec<ModuleId>) {
        let (cache, _) = self.open();
        let mut out = CheckOutput::default();
        let (front, package) = front_end_family(
            self.dir(),
            frontend::Mode::Build,
            cache::index::CommandFamily::Run,
            None,
            &mut out,
            Some(&cache),
        )
        .unwrap()
        .unwrap();
        let module = front.local_module(&ModulePath::root(&package)).unwrap();
        let main = front.main_of(module).unwrap();
        let roots = Roots::Main(main);
        let mir = crate::backend::kind_options(
            &roots,
            crate::backend::mir_options_from_env().unwrap(),
            Scope::Whole,
        );
        let reading = Reading {
            code: true,
            jit: true,
            resolves: None,
        };
        let lowered =
            crate::backend::lower_reading(&front, &out.sources, &roots, mir, Scope::Whole, reading)
                .unwrap();
        let fresh: Vec<UnitKind> = lowered.units.iter().map(|u| u.unit).collect();
        let order: Vec<(UnitKind, bool)> = lowered
            .order
            .iter()
            .map(|placed| match placed {
                Placed::Fresh(i) => (lowered.units[*i].unit, false),
                Placed::Cached { unit, .. } => (*unit, true),
            })
            .collect();
        let local: Vec<ModuleId> = front
            .order
            .iter()
            .copied()
            .filter(|&m| front.modules.get(m).unwrap().package == PackageId::LOCAL)
            .collect();
        drop(cache);
        (fresh, order, local)
    }

    /// The files under `v1/<kind>/`, by path, with their bytes, sorted.
    fn files(&self, kind: &str) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let dir = self.root().join("v1").join(kind);
        let Ok(shards) = std::fs::read_dir(dir) else {
            return out;
        };
        for shard in shards {
            for file in std::fs::read_dir(shard.unwrap().path()).unwrap() {
                let path = file.unwrap().path();
                let bytes = std::fs::read(&path).unwrap();
                out.push((path, bytes));
            }
        }
        out.sort();
        out
    }
}

fn read_log(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The `<artifact> <subject> hit|miss` lines of `artifact`.
fn lookups(lines: &[String], artifact: &str) -> Vec<String> {
    let prefix = format!("{artifact} ");
    lines
        .iter()
        .filter(|l| l.starts_with(&prefix))
        .cloned()
        .collect()
}

/// `<artifact> <module> <verdict>` for each module of `modules`.
fn all(artifact: &str, modules: &[&str], verdict: &str) -> Vec<String> {
    modules
        .iter()
        .map(|m| format!("{artifact} {m} {verdict}"))
        .collect()
}

const MAIN: &str = "//! Root.\n\nimport app::other\nimport app::util\nimport std::io\n\n\
    fn main() {\n  let a = util::check(\"a\")\n  let n = other::twice(2)\n  io::println(\"{a} {n}\")\n}\n";
/// `check` is package-visible and only compares its parameter: borrowed.
const UTIL: &str = "//! Util.\n\nfn check(s: String) -> Bool {\n  s == \"x\"\n}\n";
/// A body edit that changes no mask.
const UTIL_BODY: &str = "//! Util.\n\nfn check(s: String) -> Bool {\n  s == \"y\"\n}\n";
const OTHER: &str = "//! Other.\n\npub fn twice(n: Int) -> Int {\n  n * 2\n}\n";
const MODULES: [&str; 3] = ["app", "app::other", "app::util"];

fn three(name: &str) -> Pkg {
    Pkg::new(
        name,
        &[
            ("main.cheby", MAIN),
            ("util.cheby", UTIL),
            ("other.cheby", OTHER),
        ],
    )
}

/// D-914, D-1192's "read to skip work": a cold run misses every package
/// unit's code record and writes it; a warm run finds every one, so it
/// lowers none of the package's units (std's and the entries unit are
/// lowered, uncached until K6, D-964) and loads them from their records in
/// build order, writing no record.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_record_warm_run_lowers_no_package_unit() {
    let mut p = three("warm");
    let cold = p.run();
    assert_eq!(
        lookups(&cold, "code.run"),
        all("code.run", &MODULES, "miss")
    );
    let records = p.files("rec").len();
    let warm = p.run();
    assert_eq!(lookups(&warm, "code.run"), all("code.run", &MODULES, "hit"));
    assert_eq!(lookups(&warm, "facts"), all("facts", &MODULES, "hit"));
    assert_eq!(lookups(&warm, "masks"), all("masks", &MODULES, "hit"));
    assert_eq!(p.files("rec").len(), records, "a warm run writes no record");
    let (fresh, order, local) = p.lower_main();
    for unit in &fresh {
        if let UnitKind::Code(m) | UnitKind::Tests(m) = unit {
            assert!(!local.contains(m), "a package unit was lowered: {fresh:?}");
        }
    }
    let cached: Vec<UnitKind> = order
        .iter()
        .filter(|(_, cached)| *cached)
        .map(|(unit, _)| *unit)
        .collect();
    assert_eq!(cached.len(), MODULES.len(), "{order:?}");
    assert_eq!(order.last(), Some(&(UnitKind::Entries, false)));
}

/// D-812's no-mask case: a body edit that changes no mask misses only the
/// edited module's code record.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_record_body_edit_misses_only_its_module() {
    let mut p = three("body");
    p.run();
    p.write("util.cheby", UTIL_BODY);
    let edited = p.run();
    assert_eq!(
        lookups(&edited, "code.run"),
        [
            "code.run app hit",
            "code.run app::other hit",
            "code.run app::util miss"
        ]
    );
}

/// GAP(code-record-entries): a record that does not decode (here of another
/// `RECORD_VERSION`) is a miss, compiled again and written again, and the
/// next run finds it.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_record_other_version_is_a_miss_and_written_again() {
    let mut p = three("version");
    p.run();
    // A code record starts with `cbcr` and its version, a `u32`
    // (`CodeRecord::encode`).
    let magic = b"cbcr";
    let mut spoiled = 0;
    for (path, bytes) in p.files("rec") {
        let payload = &bytes[HEADER_LEN..];
        if !payload.starts_with(magic) {
            continue;
        }
        // Another version, same length.
        let at = HEADER_LEN + magic.len();
        let mut changed = bytes.clone();
        changed[at] = changed[at].wrapping_add(1);
        std::fs::write(&path, changed).unwrap();
        spoiled += 1;
    }
    assert_eq!(spoiled, MODULES.len());
    let again = p.run();
    assert_eq!(
        lookups(&again, "code.run"),
        all("code.run", &MODULES, "miss")
    );
    let warm = p.run();
    assert_eq!(lookups(&warm, "code.run"), all("code.run", &MODULES, "hit"));
}

/// `d` defines a type that `c`'s type holds.
const LD: &str = "//! D.\n\npub type Num { n: Int }\n";
/// `c`'s type holds a `d::Num`.
const LC: &str = "//! C.\n\nimport app::d\n\npub type Thing { size: d::Num }\n\n\
    pub fn thing() -> Thing {\n  Thing { size: d::Num { n: 1 } }\n}\n";
/// `c`'s type without `d`.
const LC_ALONE: &str = "//! C.\n\npub type Thing { size: Int }\n\n\
    pub fn thing() -> Thing {\n  Thing { size: 1 }\n}\n";
/// `b` imports `c` and wraps its type; its interface does not change with
/// `c`'s field.
const LB: &str = "//! B.\n\nimport app::c\n\npub type Wrapper { inner: c::Thing }\n\n\
    pub fn wrap() -> Wrapper {\n  Wrapper { inner: c::thing() }\n}\n";
/// The root's lowering reads `c` (and, through `c`'s field, `d`) through
/// `keep`'s parameter (`borrowing_tests::lowering_reads_enter_the_code_identity`).
const LMAIN: &str = "//! Root.\n\nimport app::b\n\n\
    fn keep(w: b::Wrapper) -> b::Wrapper {\n  w\n}\n\n\
    fn main() {\n  let _ = keep(b::wrap())\n  Nil\n}\n";

/// The lowering hints in the cache, decoded.
fn hints(p: &Pkg) -> Vec<Vec<String>> {
    p.files("hint")
        .iter()
        .filter_map(|(_, bytes)| crate::cache::front::decode_hint(&bytes[HEADER_LEN..]))
        .collect()
}

/// D-955, D-1195: a warm run computes each key from the lowering hint
/// before it lowers, and loads the root without lowering it; when an
/// interface only the root's lowering reads changes so that it reads less,
/// the root's front identity stays, its planned key misses, and its stale
/// hint is rewritten on that lookup, so the run after it hits.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_record_stale_lowering_hint_is_rewritten() {
    let mut p = Pkg::new(
        "hint",
        &[
            ("main.cheby", LMAIN),
            ("b.cheby", LB),
            ("c.cheby", LC),
            ("d.cheby", LD),
        ],
    );
    p.run();
    let both = vec!["app::c".to_owned(), "app::d".to_owned()];
    assert!(hints(&p).contains(&both), "{:?}", hints(&p));
    let modules = ["app", "app::b", "app::c", "app::d"];
    let warm = p.run();
    assert_eq!(lookups(&warm, "code.run"), all("code.run", &modules, "hit"));
    p.write("c.cheby", LC_ALONE);
    let edited = p.run();
    assert!(edited.contains(&"front app hit".to_owned()), "{edited:?}");
    assert!(
        lookups(&edited, "code.run").contains(&"code.run app miss".to_owned()),
        "{edited:?}"
    );
    let after = hints(&p);
    assert!(after.contains(&vec!["app::c".to_owned()]), "{after:?}");
    assert!(!after.contains(&both), "the stale hint stayed: {after:?}");
    let warm = p.run();
    assert!(
        lookups(&warm, "code.run").contains(&"code.run app hit".to_owned()),
        "{warm:?}"
    );
}

/// D-965 and GAP(reuse-report-bypasses-code-reads): `--reuse-report` reads
/// no code record (its lines are made while lowering), logging every lookup
/// as a miss, and still writes; the report is the same warm as cold.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_record_reuse_report_reads_no_record() {
    let mut p = three("reuse");
    let report = |p: &mut Pkg| {
        let (cache, _) = p.open();
        let options = RunOptions {
            reuse_report: true,
            ..RunOptions::default()
        };
        let prepared = prepare_run_cached(p.dir(), options, &cache).unwrap();
        cache.finish(false).unwrap();
        let Prepared::Ready(program) = prepared else {
            panic!("did not compile");
        };
        program.reuse_report().to_owned()
    };
    let cold = report(&mut p);
    p.run();
    let options = RunOptions {
        reuse_report: true,
        ..RunOptions::default()
    };
    let lines = p.run_with(options);
    assert_eq!(
        lookups(&lines, "code.run"),
        all("code.run", &MODULES, "miss")
    );
    assert_eq!(report(&mut p), cold);
}

/// D-965: a run with `CHEBY_DUMP_DIR` (a cache without reads) reads no code
/// record, so its dump is of fresh code, and still writes its records: the
/// next run on the same cache, with reads, finds them all.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_record_dump_reads_none_and_writes() {
    let mut p = three("dump");
    let (cache, log) = p.open();
    let cache = cache.without_reads();
    let prepared = prepare_run_cached(p.dir(), RunOptions::default(), &cache).unwrap();
    cache.finish(false).unwrap();
    assert!(matches!(prepared, Prepared::Ready(_)));
    let dumped = read_log(&log);
    assert_eq!(
        lookups(&dumped, "code.run"),
        all("code.run", &MODULES, "miss")
    );
    let warm = p.run();
    assert_eq!(lookups(&warm, "code.run"), all("code.run", &MODULES, "hit"));
    // And a dump run after it still reads nothing.
    let (cache, log) = p.open();
    let cache = cache.without_reads();
    let _ = prepare_run_cached(p.dir(), RunOptions::default(), &cache).unwrap();
    cache.finish(false).unwrap();
    assert_eq!(
        lookups(&read_log(&log), "code.run"),
        all("code.run", &MODULES, "miss")
    );
}

const TESTED: &str = "//! Util.\n\nfn check(s: String) -> Bool {\n  s == \"x\"\n}\n\n\
    test \"first\" {\n  assert check(\"x\")\n}\n\n\
    test \"second\" {\n  assert !check(\"y\")\n}\n\n\
    test \"third\" {\n  assert check(\"x\")\n}\n";

/// D-1179, D-970: a tests unit is whole, so its record serves every
/// selection: a warm `cheby test` with a filter loads the tests record,
/// runs the selected test under its ordinal among all of the module's
/// tests, and names and counts the same tests as a cold run with that
/// filter.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_record_tests_unit_serves_every_selection() {
    let mut cold = three("tests_cold");
    cold.write("util.cheby", TESTED);
    let (_, cold_report) = cold.test(Some("second"));
    let mut warm = three("tests_warm");
    warm.write("util.cheby", TESTED);
    let (first, all_report) = warm.test(None);
    assert!(
        lookups(&first, "tests.test").contains(&"tests.test app::util#tests miss".to_owned()),
        "{first:?}"
    );
    assert!(all_report.contains("3 passed"), "{all_report}");
    let (lines, report) = warm.test(Some("second"));
    assert!(
        lookups(&lines, "tests.test").contains(&"tests.test app::util#tests hit".to_owned()),
        "{lines:?}"
    );
    assert_eq!(report, cold_report);
    assert!(report.contains("1 passed"), "{report}");
}
