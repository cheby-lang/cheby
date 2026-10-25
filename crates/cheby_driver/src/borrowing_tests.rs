//! Borrowing's records and keys (D-968, D-969, D-955; `borrowing.rs`):
//! per-unit facts whose union gives the build's facts and masks on every
//! example, the facts and masks records in the store, the code keys, and
//! lowering's reads in the code identity.
//!
//! Every test that uses a cache opens its own store in a fresh directory
//! (D-1128), and reads the records' lookups and the code keys through
//! `CHEBY_CACHE_LOG`'s file (D-973).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use cheby_iface::PackageId;
use cheby_mir::MirOptions;
use cheby_mir::borrow::BorrowMask;

use super::*;
use crate::backend::{Roots, TestRoot};
use crate::borrowing::hook;
use crate::cache::code_keys::{CodeKeyInputs, MasksRecord, UnitPart, code_key};
use crate::cache::front::sealed;
use crate::cache::{BuildId, BuildKind, CacheLog, Key, KeyKind, Settings, Store};
use crate::frontend::Frontend;

/// The build ID of every test store.
const BUILD: BuildId = BuildId([9; 16]);

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
            "cheby_driver_borrowing_{}_{n}_{name}",
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

// ---------------------------------------------------------------------
// Per-unit facts on every example (D-969, D-968).
// ---------------------------------------------------------------------

/// The examples' package directories, sorted.
fn examples() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("cheby.toml").is_file())
        .collect();
    out.sort();
    out
}

/// Every test of the package's modules that exists on native.
fn local_tests(front: &Frontend) -> Vec<TestRoot<'_>> {
    front
        .order
        .iter()
        .filter(|&&m| front.modules.get(m).unwrap().package == PackageId::LOCAL)
        .flat_map(|&m| {
            front
                .checked(m)
                .unwrap()
                .thir
                .tests
                .iter()
                .filter(|test| crate::test::on_native(front, test))
                .map(move |test| TestRoot { module: m, test })
        })
        .collect()
}

/// What the hook saw, as a line per inference that failed it, and the
/// functions whose masks the entries unit's facts decide.
#[derive(Default)]
struct Seen {
    inferences: usize,
    borrowing: usize,
    failures: Vec<String>,
    entries_decide: Vec<String>,
}

impl Seen {
    fn add(&mut self, what: &str, checks: Vec<hook::Check>) {
        for check in checks {
            self.inferences = self.inferences.checked_add(1).unwrap();
            self.borrowing = self.borrowing.checked_add(check.borrowing).unwrap();
            if !check.union_equal || !check.masks_equal {
                self.failures.push(format!("{what}: {check:?}"));
            }
            for path in check.without_entries {
                self.entries_decide.push(format!("{what}: {path}"));
            }
        }
    }
}

/// D-969: on every example, built from `main`, from every test on native
/// and from `cheby check`'s constants, and on std's self-test build, the
/// union of each unit's own facts (named by path, rebuilt through the
/// build's interfaces) equals the build's facts, and the masks inferred
/// from it equal the build's masks, which are today's (the build's facts
/// are still `collect_facts` over every unit; `cargo test -p cheby_driver
/// golden` and the gate's CLIF record pin the masks themselves). The
/// functions whose masks only the entries unit's facts decide are listed
/// (GAP(entries-unit-facts)).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unit_facts_every_example() {
    let mut seen = Seen::default();
    let mut lowered_examples = 0_usize;
    for example in examples() {
        let name = example.file_name().unwrap().to_string_lossy().into_owned();
        let mut out = CheckOutput::default();
        let Some((front, _)) = front_end(&example, frontend::Mode::Build, &mut out).unwrap() else {
            continue;
        };
        if out.has_errors() {
            // 031 needs `std::http` until S4a ships it; nothing lowers.
            continue;
        }
        lowered_examples = lowered_examples.checked_add(1).unwrap();
        let root = front
            .order
            .iter()
            .copied()
            .find(|&m| {
                let info = front.modules.get(m).unwrap();
                info.package == PackageId::LOCAL && info.path.is_root()
            })
            .unwrap();
        hook::watch();
        if let Some(main) = front.main_of(root) {
            backend::lower(
                &front,
                &out.sources,
                &Roots::Main(main),
                MirOptions::default(),
            )
            .unwrap();
        }
        seen.add(&format!("{name} main"), hook::take());
        let tests = local_tests(&front);
        if !tests.is_empty() {
            hook::watch();
            backend::lower(
                &front,
                &out.sources,
                &Roots::Tests(tests),
                MirOptions::default(),
            )
            .unwrap();
            seen.add(&format!("{name} tests"), hook::take());
        }
        hook::watch();
        backend::lower(
            &front,
            &out.sources,
            &Roots::Constants,
            MirOptions::default(),
        )
        .unwrap();
        seen.add(&format!("{name} constants"), hook::take());
    }
    assert!(lowered_examples >= 30, "{lowered_examples}");
    std_self_test(&mut seen);
    assert!(seen.inferences >= 100, "{}", seen.inferences);
    assert!(seen.borrowing > 0);
    assert!(seen.failures.is_empty(), "{:#?}", seen.failures);
    println!(
        "unit_facts_every_example: {} inferences, {} masks; masks only the entries unit's facts decide: {:?}",
        seen.inferences, seen.borrowing, seen.entries_decide
    );
}

/// Std's self-test build into `seen`: each std module's tests, as
/// `test_std` builds them.
fn std_self_test(seen: &mut Seen) {
    let mut sources = SourceMap::new();
    let mut diagnostics = Vec::new();
    let package = crate::test::std_test_package(&mut sources, &mut diagnostics).unwrap();
    let front = frontend::run(
        package,
        frontend::Mode::StdTests,
        &mut sources,
        &mut diagnostics,
    )
    .unwrap();
    let mut std_modules = 0_usize;
    for (module, info) in front.modules.iter() {
        if info.package != PackageId::STD {
            continue;
        }
        let Some(checked) = front.checked(module) else {
            continue;
        };
        let roots: Vec<TestRoot<'_>> = checked
            .thir
            .tests
            .iter()
            .filter(|t| crate::test::on_native(&front, t))
            .map(|test| TestRoot { module, test })
            .collect();
        if roots.is_empty() {
            continue;
        }
        std_modules = std_modules.checked_add(1).unwrap();
        hook::watch();
        backend::lower(
            &front,
            &sources,
            &Roots::Tests(roots),
            MirOptions::default(),
        )
        .unwrap();
        seen.add(&format!("std self-test {}", info.path), hook::take());
    }
    assert!(std_modules >= 10, "{std_modules}");
}

// ---------------------------------------------------------------------
// Commands through a cache: records, keys and the lookup log.
// ---------------------------------------------------------------------

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

    /// `cheby run`'s compile through the cache: the log's lines, or the
    /// error; a package that does not compile is an error with its output.
    fn run(&mut self) -> Result<Vec<String>, Error> {
        let (cache, log) = self.open();
        let prepared = prepare_run_cached(self.dir(), RunOptions::default(), &cache);
        cache.finish(false).unwrap();
        match prepared? {
            Prepared::Ready(_) => Ok(read_log(&log)),
            Prepared::CompileFailed(out) => Err(Error::new(out.render())),
        }
    }

    /// `cheby test`'s compile through the cache: the log's lines.
    fn test(&mut self) -> Vec<String> {
        let (cache, log) = self.open();
        let prepared = prepare_test_cached(self.dir(), &TestOptions::default(), &cache).unwrap();
        cache.finish(false).unwrap();
        assert!(matches!(prepared, PreparedTests::Ready(_)));
        read_log(&log)
    }

    /// The files under `v1/<kind>/`, by name, with their bytes, sorted.
    fn files(&self, kind: &str) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let dir = self.root().join("v1").join(kind);
        for shard in std::fs::read_dir(dir).unwrap() {
            for file in std::fs::read_dir(shard.unwrap().path()).unwrap() {
                let path = file.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                out.push((name, std::fs::read(&path).unwrap()));
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

/// The code keys of `artifact` (`code.run`, `tests.test`, …) by module.
fn keys(lines: &[String], artifact: &str) -> BTreeMap<String, String> {
    let prefix = format!("key {artifact} ");
    lines
        .iter()
        .filter_map(|l| l.strip_prefix(&prefix))
        .map(|rest| {
            let (module, key) = rest.split_once(' ').unwrap();
            (module.to_owned(), key.to_owned())
        })
        .collect()
}

/// The modules whose keys differ between `a` and `b` (both must name the
/// same modules).
fn changed(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>) -> Vec<String> {
    assert_eq!(
        a.keys().collect::<Vec<_>>(),
        b.keys().collect::<Vec<_>>(),
        "{a:?} {b:?}"
    );
    a.iter()
        .filter(|&(m, k)| b.get(m) != Some(k))
        .map(|(m, _)| m.clone())
        .collect()
}

const MAIN: &str = "//! Root.\n\nimport app::other\nimport app::util\nimport std::io\n\n\
    fn main() {\n  let a = util::check(\"a\")\n  let n = other::twice(2)\n  io::println(\"{a} {n}\")\n}\n";
/// `check` is package-visible and only compares its parameter: borrowed.
const UTIL: &str = "//! Util.\n\nfn check(s: String) -> Bool {\n  s == \"x\"\n}\n";
/// A body edit that changes no mask.
const UTIL_BODY: &str = "//! Util.\n\nfn check(s: String) -> Bool {\n  s == \"y\"\n}\n";
/// A body edit that makes `check` store its parameter: owned.
const UTIL_OWNED: &str = "//! Util.\n\nfn check(s: String) -> Bool {\n  [s] == [\"x\"]\n}\n";
const OTHER: &str = "//! Other.\n\npub fn twice(n: Int) -> Int {\n  n * 2\n}\n";
const TESTS: &str = "\ntest \"checks\" {\n  assert check(\"x\")\n}\n";

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

const MODULES: [&str; 3] = ["app", "app::other", "app::util"];

/// A cold run writes a facts and a masks record per module, logged as
/// misses; the next run finds them all.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn masks_record_written_then_hit() {
    let mut p = three("written");
    let cold = p.run().unwrap();
    let all = |verdict: &str, artifact: &str| -> Vec<String> {
        MODULES
            .iter()
            .map(|m| format!("{artifact} {m} {verdict}"))
            .collect()
    };
    assert_eq!(lookups(&cold, "facts"), all("miss", "facts"), "{cold:?}");
    assert_eq!(lookups(&cold, "masks"), all("miss", "masks"), "{cold:?}");
    let records = p.files("rec").len();
    let warm = p.run().unwrap();
    assert_eq!(lookups(&warm, "facts"), all("hit", "facts"), "{warm:?}");
    assert_eq!(lookups(&warm, "masks"), all("hit", "masks"), "{warm:?}");
    assert_eq!(p.files("rec").len(), records, "a warm run writes no record");
    // The masks record of `app::util` holds `check`'s mask.
    let masks: Vec<MasksRecord> = p
        .files("rec")
        .iter()
        .filter_map(|(_, bytes)| MasksRecord::decode(&bytes[HEADER_LEN..]))
        .collect();
    assert!(
        masks
            .iter()
            .any(|r| r.masks == [("check".to_owned(), BorrowMask(1))]),
        "{masks:?}"
    );
    assert_eq!(keys(&cold, "code.run"), keys(&warm, "code.run"));
    assert_eq!(
        keys(&cold, "code.run").keys().collect::<Vec<_>>(),
        MODULES.iter().collect::<Vec<_>>()
    );
}

/// D-1131's pattern: a masks record that hits but says other masks than
/// the fresh inference is an internal error naming the module.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn masks_record_difference_is_an_internal_error() {
    let mut p = three("difference");
    p.run().unwrap();
    let dir = p.root().join("v1/rec");
    let mut spoiled = 0;
    for shard in std::fs::read_dir(&dir).unwrap() {
        for file in std::fs::read_dir(shard.unwrap().path()).unwrap() {
            let path = file.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            let Some(record) = MasksRecord::decode(&bytes[HEADER_LEN..]) else {
                continue;
            };
            if record.masks.is_empty() {
                continue;
            }
            // Another mask, sealed again: a payload of the same length, so
            // the header stays valid.
            let other = MasksRecord {
                masks: vec![("check".to_owned(), BorrowMask(0b11))],
            };
            let mut file = bytes[..HEADER_LEN].to_vec();
            file.extend_from_slice(&other.encode());
            assert_eq!(file.len(), bytes.len());
            std::fs::write(&path, file).unwrap();
            spoiled += 1;
        }
    }
    assert_eq!(spoiled, 1);
    let error = p.run().unwrap_err();
    assert!(
        error.message().contains("masks record of `app::util`"),
        "{error}"
    );
}

/// D-972: two copies of a package at two paths, each with its own cache,
/// hold the same records byte for byte, and log the same keys.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn masks_record_identical_from_two_paths() {
    let mut a = three("path_a");
    let mut b = three("path_b_longer_name");
    let tested = format!("{UTIL}{TESTS}");
    a.write("util.cheby", &tested);
    b.write("util.cheby", &tested);
    let run_a = a.run().unwrap();
    let run_b = b.run().unwrap();
    let test_a = a.test();
    let test_b = b.test();
    assert_eq!(a.files("rec"), b.files("rec"));
    assert_eq!(a.files("blob"), b.files("blob"));
    assert_eq!(keys(&run_a, "code.run"), keys(&run_b, "code.run"));
    assert!(!keys(&test_a, "tests.test").is_empty(), "{test_a:?}");
    assert_eq!(keys(&test_a, "code.test"), keys(&test_b, "code.test"));
    assert_eq!(keys(&test_a, "tests.test"), keys(&test_b, "tests.test"));
}

/// D-969, D-812: a body edit that changes no mask changes only its
/// module's code key; an edit that changes a callee's mask changes its
/// callers' keys too; keys are the same from two package paths.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_key_follows_bodies_and_callees_masks() {
    let mut p = three("edits");
    let first = keys(&p.run().unwrap(), "code.run");
    p.write("util.cheby", UTIL_BODY);
    let body = keys(&p.run().unwrap(), "code.run");
    assert_eq!(changed(&first, &body), ["app::util"]);
    p.write("util.cheby", UTIL_OWNED);
    let owned_lines = p.run().unwrap();
    let owned = keys(&owned_lines, "code.run");
    assert_eq!(changed(&body, &owned), ["app", "app::util"]);
    // The root's front record still hits: its key moved with the mask.
    assert!(
        owned_lines.contains(&"front app hit".to_owned()),
        "{owned_lines:?}"
    );
    let mut copy = three("edits_copy");
    copy.write("util.cheby", UTIL_OWNED);
    assert_eq!(keys(&copy.run().unwrap(), "code.run"), owned);
}

/// A test build keys code under the `test` kind: each module it compiles
/// gets another key than the run build gives it, and its tests unit gets
/// a key of its own.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_key_kind_changes_every_key() {
    let mut p = three("kinds");
    p.write("util.cheby", &format!("{UTIL}{TESTS}"));
    let run = keys(&p.run().unwrap(), "code.run");
    let test_lines = p.test();
    let test = keys(&test_lines, "code.test");
    assert!(test.contains_key("app::util"), "{test_lines:?}");
    assert!(
        test.iter().all(|(m, k)| run.get(m).is_some_and(|r| r != k)),
        "{run:?} {test:?}"
    );
    assert_eq!(
        keys(&test_lines, "tests.test").keys().collect::<Vec<_>>(),
        ["app::util"]
    );
    assert!(
        lookups(&test_lines, "facts").contains(&"facts app::util#tests miss".to_owned()),
        "{test_lines:?}"
    );
}

/// Every field of a code key changes it: the kind and each setting of
/// D-958, the positions switch (D-1173), the identity, the own masks and
/// the callees' slot symbols (D-1109), and the unit part.
#[test]
fn code_key_settings_change_every_key() {
    let settings = Settings {
        kind: BuildKind::Run,
        rc_naive: false,
        unwind_edges: true,
        checked_counts: false,
        stack_check: true,
        target: "aarch64-macos".to_owned(),
    };
    let identity = Key::builder(KeyKind::Code, BUILD).str("c2").finish();
    let own = vec![("f".to_owned(), BorrowMask(1))];
    let callees = vec!["cb$slot$app$util$check$$b1".to_owned()];
    let base = CodeKeyInputs {
        build: BUILD,
        part: UnitPart::Code,
        settings: &settings,
        positions: true,
        identity,
        own: &own,
        callees: &callees,
    };
    let key = code_key(&base);
    assert_eq!(key, code_key(&base.clone()));
    let mut variants: Vec<Settings> = Vec::new();
    for kind in [
        BuildKind::Test,
        BuildKind::Plain,
        BuildKind::Std,
        BuildKind::StdSelfTest,
    ] {
        variants.push(Settings {
            kind,
            ..settings.clone()
        });
    }
    variants.push(Settings {
        rc_naive: true,
        ..settings.clone()
    });
    variants.push(Settings {
        unwind_edges: false,
        ..settings.clone()
    });
    variants.push(Settings {
        checked_counts: true,
        ..settings.clone()
    });
    variants.push(Settings {
        stack_check: false,
        ..settings.clone()
    });
    variants.push(Settings {
        target: "x86_64-linux".to_owned(),
        ..settings.clone()
    });
    for s in &variants {
        assert_ne!(
            code_key(&CodeKeyInputs {
                settings: s,
                ..base.clone()
            }),
            key,
            "{s:?}"
        );
    }
    let no_own: Vec<(String, BorrowMask)> = Vec::new();
    let other_mask = vec!["cb$slot$app$util$check".to_owned()];
    let other_identity = Key::builder(KeyKind::Code, BUILD).str("t2").finish();
    for changed in [
        CodeKeyInputs {
            positions: false,
            ..base.clone()
        },
        CodeKeyInputs {
            identity: other_identity,
            ..base.clone()
        },
        CodeKeyInputs {
            own: &no_own,
            ..base.clone()
        },
        CodeKeyInputs {
            callees: &other_mask,
            ..base.clone()
        },
        CodeKeyInputs {
            part: UnitPart::Tests,
            ..base.clone()
        },
        CodeKeyInputs {
            build: BuildId([1; 16]),
            ..base.clone()
        },
    ] {
        assert_ne!(code_key(&changed), key, "{changed:?}");
    }
}

/// The masks record's bytes round-trip, and damage reads as a miss.
#[test]
fn masks_record_round_trip() {
    let record = MasksRecord {
        masks: vec![
            ("a".to_owned(), BorrowMask(1)),
            ("b::c".to_owned(), BorrowMask(0b101)),
        ],
    };
    let bytes = record.encode();
    assert_eq!(MasksRecord::decode(&bytes), Some(record.clone()));
    let mut damaged = bytes.clone();
    damaged[2] ^= 1;
    assert_eq!(MasksRecord::decode(&damaged), None);
    assert_eq!(MasksRecord::decode(&sealed(vec![9])), None);
    assert_eq!(
        MasksRecord::decode(&MasksRecord::default().encode()),
        Some(MasksRecord::default())
    );
}

// ---------------------------------------------------------------------
// Lowering's reads in the code identity (D-955).
// ---------------------------------------------------------------------

/// `c` defines a type, `b` imports `c` and wraps the type.
const LC: &str = "//! C.\n\npub type Thing { size: Int }\n\n\
    pub fn show(thing: Thing) -> String {\n  \"thing {thing.size}\"\n}\n";
const LB: &str = "//! B.\n\nimport app::c\n\npub type Wrapper { inner: c::Thing }\n\n\
    pub fn make() -> c::Thing {\n  c::Thing { size: 1 }\n}\n\n\
    pub fn wrap() -> Wrapper {\n  Wrapper { inner: make() }\n}\n";
/// The root passes `b::Wrapper` values along, so its front end never reads
/// `c`; borrow inference asks whether `keep`'s parameter is handle-free and
/// walks `Wrapper`'s field into `c::Thing`, so only its lowering reads `c`
/// (as `reads_tests::hidden_reads_lowering_records_what_only_it_reads`
/// shows).
const LMAIN: &str = "//! Root.\n\nimport app::b\n\n\
    fn keep(w: b::Wrapper) -> b::Wrapper {\n  w\n}\n\n\
    fn main() {\n  let _ = keep(b::wrap())\n  Nil\n}\n";

/// A module whose lowering reads an interface its front end did not read
/// gets that interface's hash in its code identity: an edit of `c`'s
/// interface leaves the root's front record a hit (its imports' interfaces
/// did not change) and changes the root's code key.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn lowering_reads_enter_the_code_identity() {
    let mut p = Pkg::new(
        "lowering",
        &[("main.cheby", LMAIN), ("b.cheby", LB), ("c.cheby", LC)],
    );
    let cold = p.run().unwrap();
    let before = keys(&cold, "code.run");
    // The root's lowering hint names `c` (D-955's second step under `F2`).
    let hints = p.files("hint");
    assert!(
        hints.iter().any(|(_, bytes)| {
            crate::cache::front::decode_hint(&bytes[HEADER_LEN..])
                .is_some_and(|paths| paths == ["app::c"])
        }),
        "no lowering hint names app::c"
    );
    p.write(
        "c.cheby",
        &format!("{LC}\npub fn extra() -> Int {{\n  1\n}}\n"),
    );
    let after_lines = p.run().unwrap();
    let after = keys(&after_lines, "code.run");
    // `b` imports `c`, so its front record misses and its key moves; the
    // root imports only `b`, whose interface did not change, and it calls
    // only `pub` functions of `b`, whose masks are owned: the root's key
    // moves because its lowering read `c`.
    assert!(
        after_lines.contains(&"front app hit".to_owned()),
        "{after_lines:?}"
    );
    assert_eq!(changed(&before, &after), ["app", "app::b", "app::c"]);
}
