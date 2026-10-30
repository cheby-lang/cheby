//! The front records, their keys and the replayed diagnostics (D-955,
//! D-956, D-964, D-965), and the cache-hit tests of D-973 that read the
//! lookups through `CHEBY_CACHE_LOG`'s file.
//!
//! Every test opens its own store in a fresh directory, so no test reads
//! or fills the developer's cache (D-1128).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use cheby_diag::{Colors, Diagnostic, Span, codes};
use cheby_iface::ModuleId;

use super::*;
use crate::cache::front::{
    External, FrontKey, FrontRecord, ImportKey, ItemRef, MentionerKind, ReferenceSummary,
    UnusedCandidate, UseSummary, decode_hint, encode_hint, front_identity, reads_key,
};
use crate::cache::index::{CommandFamily, PackageIndex};
use crate::cache::replay::{self, SpanCx, StoredSpan};
use crate::cache::{BuildId, CacheLog, Hash128, Store};
use crate::frontend::Frontend;

/// The build ID of every test store.
const BUILD: BuildId = BuildId([7; 16]);

/// A fresh directory under the system's temporary directory, removed on
/// drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cheby_driver_cache_{}_{n}_{name}",
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

/// A package `app` with these files under `src/`, and a fresh cache.
struct Fixture {
    package: TempDir,
    cache: TempDir,
    runs: u32,
}

impl Fixture {
    fn new(name: &str, files: &[(&str, &str)]) -> Self {
        let package = TempDir::new(name);
        std::fs::write(
            package.path().join("cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let fixture = Self {
            package,
            cache: TempDir::new(&format!("{name}_cache")),
            runs: 0,
        };
        for (path, text) in files {
            fixture.write(path, text);
        }
        fixture
    }

    fn dir(&self) -> &Path {
        self.package.path()
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.dir().join("src").join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn store(&self) -> Store {
        Store::open(&self.cache.path().join("cache"), BUILD).unwrap()
    }

    /// A command's cache, logging to a fresh file.
    fn open(&mut self) -> (Cache, PathBuf) {
        self.runs = self.runs.checked_add(1).unwrap();
        let log = self.cache.path().join(format!("log-{}", self.runs));
        let cache = Cache::with_store(self.store(), CacheLog::new(Some(log.clone())));
        (cache, log)
    }

    /// `cheby check` through the cache: the rendered output and the log's
    /// lines.
    fn check(&mut self) -> (String, Vec<String>) {
        let run = self.check_counted();
        (run.output, run.lookups)
    }

    /// `cheby check` through the cache, with what the driver counted and
    /// the store's own counts of entry files opened and written.
    fn check_counted(&mut self) -> Counted {
        let (cache, log) = self.open();
        let out = check_cached(self.dir(), &CheckOptions::default(), &cache).unwrap();
        let opens = cache.store().opens();
        let writes = cache.store().writes();
        let parsed = cache.parsed_modules();
        let checked = cache.checked_modules();
        cache.finish(true).unwrap();
        Counted {
            output: out.render(),
            lookups: lines(&log),
            parses: parses(&log),
            std_lookups: std::fs::read_to_string(&log)
                .unwrap()
                .lines()
                .filter(|line| line.starts_with("front.std "))
                .count(),
            parsed,
            checked,
            opens,
            writes,
        }
    }

    /// The cacheless `cheby check`'s rendered output.
    fn cold_check(&self) -> String {
        check(self.dir()).unwrap().render()
    }

    /// The files under the store's `v1/<kind>/`.
    fn files(&self, kind: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let dir = self.cache.path().join("cache/v1").join(kind);
        for shard in std::fs::read_dir(dir).unwrap() {
            for file in std::fs::read_dir(shard.unwrap().path()).unwrap() {
                out.push(file.unwrap().path());
            }
        }
        out.sort();
        out
    }

    /// The key of the package's index for `family` (D-962).
    fn index_key(&self, family: CommandFamily) -> cache::Key {
        PackageIndex::key(self.dir(), family, BUILD)
    }

    /// The package's index for `family`, if one is stored and decodes.
    fn index(&self, family: CommandFamily) -> Option<PackageIndex> {
        let store = self.store();
        PackageIndex::decode(&store.get_hint(&self.index_key(family)).unwrap()?)
    }

    /// Overwrites the package's index for `family` with bytes that are no
    /// index, so the next command ignores it.
    fn spoil_index(&self, family: CommandFamily, store: &Store) {
        store
            .put_hint(&self.index_key(family), b"not an index")
            .unwrap();
        store.flush();
    }

    /// Every package front record in the store: std's front records (K6,
    /// D-980) and every code record are left out.
    fn records(&self) -> Vec<FrontRecord> {
        let std: Vec<String> = crate::std_lib::STD_MODULES
            .iter()
            .map(|m| {
                crate::std_cache::std_front_key(BUILD, &format!("std::{}", m.path))
                    .hash()
                    .to_hex()
            })
            .collect();
        self.files("rec")
            .iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_none_or(|n| !std.iter().any(|s| s == n))
            })
            .filter_map(|p| FrontRecord::decode(&std::fs::read(p).unwrap()[32..]))
            .collect()
    }
}

/// One `cheby check` through the cache, as [`Fixture::check_counted`]
/// observed it.
#[derive(Debug)]
struct Counted {
    /// The rendered output.
    output: String,
    /// The log's lookup lines.
    lookups: Vec<String>,
    /// The log's `parse` lines' modules.
    parses: Vec<String>,
    /// The driver's count of package modules parsed, sorted.
    parsed: Vec<String>,
    /// The driver's count of package modules checked, sorted.
    checked: Vec<String>,
    /// Entry files the store opened to read.
    opens: usize,
    /// The log's `front.std` lookups (K6, D-980).
    std_lookups: usize,
    /// Entry files the store wrote.
    writes: usize,
}

/// The log's lookup lines (`front <module> hit|miss`).
fn lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .filter(|line| line.starts_with("front "))
        .map(str::to_owned)
        .collect()
}

/// The package modules the log says the command parsed
/// (D-1135), sorted.
fn parses(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("parse "))
        .map(str::to_owned)
        .collect()
}

/// The log of a command in which `hits` hit and every other module of
/// `all` missed, sorted as the log is.
fn expect_log(all: &[&str], hits: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = all
        .iter()
        .map(|m| {
            let verdict = if hits.contains(m) { "hit" } else { "miss" };
            format!("front {m} {verdict}")
        })
        .collect();
    v.sort();
    v
}

/// `c` defines a type and how it shows; `b` imports `c`; the root reads
/// `c`'s interface through a value of `b` (a hidden read, D-955), and has
/// an unused binding (W0002, with a suggested edit); `d` has a
/// package-visible function nothing uses (W0005, `cheby check` only).
const ROOT: &str = "//! Root.\n\nimport app::b\nimport std::io\n\n\
    fn text() -> String {\n  let thing = b::make()\n  let unused = 2\n  \"{thing}\"\n}\n\n\
    fn main() {\n  io::println(text())\n}\n";
const B: &str = "//! B.\n\nimport app::c\n\npub type Wrapper { inner: c::Thing }\n\n\
    pub fn make() -> c::Thing {\n  c::Thing { size: 1 }\n}\n\n\
    pub fn wrap() -> Wrapper {\n  Wrapper { inner: make() }\n}\n";
const C: &str = "//! C.\n\npub type Thing { size: Int }\n\n\
    pub fn show(thing: Thing) -> String {\n  \"thing {thing.size}\"\n}\n";
const D: &str = "//! D.\n\nfn one() -> Int {\n  1\n}\n";
const MODULES: [&str; 4] = ["app", "app::b", "app::c", "app::d"];

fn several(name: &str) -> Fixture {
    Fixture::new(
        name,
        &[
            ("main.cheby", ROOT),
            ("b.cheby", B),
            ("c.cheby", C),
            ("d.cheby", D),
        ],
    )
}

/// The cacheless front end of `dir`, with its sources.
fn front_of(dir: &Path) -> (Frontend, CheckOutput) {
    let mut out = CheckOutput::default();
    let (front, _) = front_end(dir, frontend::Mode::Build, &mut out)
        .unwrap()
        .unwrap();
    (front, out)
}

fn module(front: &Frontend, path: &str) -> ModuleId {
    front
        .modules
        .find(&parse_module_path(path).unwrap())
        .unwrap()
}

/// The span of the item `name` of `module`.
fn item_span(front: &Frontend, module: ModuleId, name: &str) -> Span {
    let iface = front.set.get(module).unwrap();
    iface
        .items
        .iter()
        .find(|i| front.globals.names.get(i.name) == name)
        .and_then(|i| i.span)
        .unwrap()
}

/// `F1` of the package module at `path`, from a cacheless front end.
fn front_key_of(front: &Frontend, path: &str, text: &str) -> cache::Key {
    let id = module(front, path);
    let imports = front.import_targets[usize::try_from(id.as_u32()).unwrap()]
        .iter()
        .map(|t| match t.module() {
            Some(m) => ImportKey::Module {
                path: front.modules.get(m).unwrap().path.to_string(),
                hash: Hash128(front.set.get(m).unwrap().hash.unwrap().0),
            },
            None => ImportKey::Unresolved,
        })
        .collect();
    FrontKey {
        package: "app".to_owned(),
        targets: front.package_targets,
        module: path.to_owned(),
        source: Hash128::of(text.as_bytes()),
        imports,
    }
    .key(BUILD)
}

/// D-973's "second run with no edit": a second `cheby check` and a second
/// `cheby run` hit every front lookup, miss none, and print byte for byte
/// what the first printed and what the cacheless build prints.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn cache_hit_second_run() {
    let mut f = several("second_run");
    let first = f.check_counted();
    let cold = first.output;
    assert_eq!(first.lookups, expect_log(&MODULES, &[]));
    assert_eq!(first.parses, MODULES);
    assert_eq!(first.parsed, MODULES);
    assert!(cold.contains("W0002") && cold.contains("W0005"), "{cold}");
    assert_eq!(cold, f.cold_check());
    // The second `cheby check` parses and checks no package module
    // (D-962, D-967): the log and the driver's count agree.
    let second = f.check_counted();
    assert_eq!(second.lookups, expect_log(&MODULES, &MODULES));
    assert_eq!(second.parses, Vec::<String>::new());
    assert_eq!(second.parsed, Vec::<String>::new());
    assert_eq!(second.checked, Vec::<String>::new());
    assert_eq!(second.output, cold);

    let fresh = match prepare_run(f.dir(), RunOptions::default()).unwrap() {
        Prepared::Ready(program) => program.output().render(),
        Prepared::CompileFailed(out) => panic!("{}", out.render()),
    };
    for _ in 0..2 {
        let (cache, log) = f.open();
        let prepared = prepare_run_cached(f.dir(), RunOptions::default(), &cache).unwrap();
        cache.finish(false).unwrap();
        assert_eq!(lines(&log), expect_log(&MODULES, &MODULES));
        let Prepared::Ready(program) = prepared else {
            panic!("the package does not compile");
        };
        assert_eq!(program.output().render(), fresh);
        assert_eq!(program.run().unwrap(), 0);
    }
}

/// D-973's "cached note into a module whose item moved", in the
/// synthetic form (no warning of the compiler points into another module
/// yet; `.m3-work/wave2/answers.md` Q1): a diagnostic stored in `a`'s
/// front record has a label on `b::target`; an edit moves `target` without
/// changing `b`'s interface hash; `a` hits, and the replayed label lies on
/// the span the fresh front end gives `target` now. `cheby check` prints a
/// hit's replayed diagnostics without checking it (a check from source
/// would not reproduce a synthetic diagnostic); `b`, edited, is checked,
/// and its fresh interface gives the span.
#[test]
fn cache_hit_moved_item() {
    const A: &str = "//! A.\n\nimport app::b\n\npub fn use_it() -> Int {\n  b::target()\n}\n";
    const B0: &str = "//! B.\n\npub fn target() -> Int {\n  1\n}\n";
    const B1: &str = "//! B.\n\n// These lines move `target`\n// down two lines.\n\npub fn target() -> Int {\n  1\n}\n";
    let mut f = Fixture::new(
        "moved_item",
        &[
            (
                "main.cheby",
                "//! Root.\n\nimport app::a\nimport std::io\n\n\
                 fn main() {\n  let n = a::use_it()\n  io::println(\"{n}\")\n}\n",
            ),
            ("a.cheby", A),
            ("b.cheby", B0),
            (
                "bad.cheby",
                "//! Bad.\n\nfn broken() -> Int {\n  \"text\"\n}\n",
            ),
        ],
    );
    let all = ["app", "app::a", "app::b", "app::bad"];
    let (first, log) = f.check();
    assert!(first.contains("error["), "`bad` has an error: {first}");
    assert_eq!(log, expect_log(&all, &[]));

    // The synthetic diagnostic, stored in `a`'s record.
    let (front, _) = front_of(f.dir());
    let a = module(&front, "app::a");
    let b = module(&front, "app::b");
    let old = item_span(&front, b, "target");
    let a_file = front.modules.get(a).unwrap().file.unwrap();
    let synthetic = Diagnostic::new(codes::UNUSED_BINDING, "a synthetic warning")
        .with_primary(item_span(&front, a, "use_it"), "here")
        .with_secondary(old, "the item it names")
        .with_note("a note");
    let span_cx = SpanCx {
        own: a_file,
        own_len: u32::try_from(A.len()).unwrap(),
        modules: &front.modules,
        set: &front.set,
        globals: &front.globals,
    };
    let blob = replay::encode_diagnostics(&[&[], std::slice::from_ref(&synthetic), &[]], &span_cx)
        .unwrap();
    let store = f.store();
    let f1 = front_key_of(&front, "app::a", A);
    // No hidden reads: no hint (D-1137).
    assert_eq!(store.get_hint(&reads_key(f1, BUILD)).unwrap(), None);
    let f2 = front_identity(f1, BUILD, &[]);
    let mut record = FrontRecord::decode(&store.get_record(&f2).unwrap().unwrap()).unwrap();
    record.diagnostics = store.put_blob(&blob).unwrap();
    store.put_record(&f2, &record.encode()).unwrap();
    // The package index holds the record inline and answers first (D-962):
    // a bad index is ignored, so the record file answers.
    f.spoil_index(CommandFamily::Check, &store);
    drop(store);

    // `target` moves; `b`'s interface hash does not change.
    f.write("b.cheby", B1);
    let (moved_front, _) = front_of(f.dir());
    let moved_b = module(&moved_front, "app::b");
    assert_eq!(
        moved_front.set.get(moved_b).unwrap().hash,
        front.set.get(b).unwrap().hash
    );
    let new = item_span(&moved_front, moved_b, "target");
    assert_ne!(new.range, old.range);

    let (cache, log) = f.open();
    let out = check_cached(f.dir(), &CheckOptions::default(), &cache).unwrap();
    cache.finish(false).unwrap();
    assert_eq!(lines(&log), expect_log(&all, &["app", "app::a"]));
    assert!(out.has_errors(), "`bad` has an error");
    let replayed = out
        .diagnostics
        .iter()
        .find(|d| d.message == "a synthetic warning")
        .unwrap();
    let label = replayed.labels.get(1).unwrap();
    assert_eq!(label.span, new, "the note follows the moved item");
    assert_eq!(
        out.sources.file(label.span.file).unwrap().path(),
        "src/b.cheby"
    );
    assert!(out.render().contains("src/b.cheby:6:"), "{}", out.render());
}

/// Keys hash exactly the fields D-955 lists, in a fixed encoding: these
/// constants change only with the encoding.
#[test]
fn front_key_stability() {
    let key = FrontKey {
        package: "app".to_owned(),
        targets: Targets::ALL,
        module: "app::a".to_owned(),
        source: Hash128([1; 16]),
        imports: vec![
            ImportKey::Module {
                path: "app::b".to_owned(),
                hash: Hash128([2; 16]),
            },
            ImportKey::Unresolved,
        ],
    };
    let build = BuildId([3; 16]);
    let f1 = key.key(build);
    let reads = reads_key(f1, build);
    let f2 = front_identity(f1, build, &[("app::c".to_owned(), Hash128([4; 16]))]);
    assert_eq!(f1.hash().to_hex(), "51c4b791c964ad518475fb9a32a77608");
    assert_eq!(reads.hash().to_hex(), "075aea882e28e55e4a80d12353c3fd8a");
    assert_eq!(f2.hash().to_hex(), "5bbe3f2932ffc6818889bbcea60cbf92");
    // Every field counts.
    let mut other = key.clone();
    other.targets = Targets::only(Target::Native);
    assert_ne!(other.key(build), f1);
    let mut other = key.clone();
    other.imports.pop();
    assert_ne!(other.key(build), f1);
    assert_ne!(front_identity(f1, build, &[]), f2);
}

/// Records and hints read back what was written, and anything malformed
/// reads as nothing.
#[test]
fn front_record_round_trip() {
    let record = FrontRecord {
        interface: Hash128([1; 16]),
        artifact: Hash128([2; 16]),
        diagnostics: Hash128([3; 16]),
        imports: vec![
            ImportKey::Unresolved,
            ImportKey::Module {
                path: "app::b".to_owned(),
                hash: Hash128([4; 16]),
            },
        ],
        externals: vec![External {
            symbol: "cb_add".to_owned(),
            sig: cheby_mir::ForeignSig {
                params: vec![cheby_mir::CType::I64, cheby_mir::CType::Ptr],
                ret: cheby_mir::CType::Void,
            },
            span: (10, 8),
        }],
        reads: vec!["app::c".to_owned()],
        references: vec![ReferenceSummary {
            item: "limit".to_owned(),
            kind: MentionerKind::Constant,
            mentions: vec![ItemRef {
                module: "app::b".to_owned(),
                item: "Shape::Square".to_owned(),
            }],
        }],
        uses: UseSummary {
            uses: vec![ItemRef {
                module: "app::b".to_owned(),
                item: "area".to_owned(),
            }],
            candidates: vec![UnusedCandidate {
                item: "helper".to_owned(),
                kind: "function".to_owned(),
                span: (3, 6),
            }],
        },
    };
    let bytes = record.encode();
    assert_eq!(FrontRecord::decode(&bytes), Some(record));
    for len in 0..bytes.len() {
        assert_eq!(FrontRecord::decode(&bytes[..len]), None, "{len}");
    }
    let paths = vec!["app::c".to_owned(), "app::d".to_owned()];
    assert_eq!(decode_hint(&encode_hint(&paths)), Some(paths));
    assert_eq!(decode_hint(&[9]), None);
}

/// D-955's two steps: the root reads `c` only through `b` (a hidden
/// read). An interface edit of `c` that leaves `b`'s interface hash alone
/// leaves the root's `F1` unchanged, but its hint names `c`, whose new
/// hash changes `F2`: the root misses.
#[test]
fn front_hidden_read_change_misses() {
    let mut f = several("hidden_read");
    let (cold, _) = f.check();
    let (front, _) = front_of(f.dir());
    let f1 = front_key_of(&front, "app", ROOT);
    let hint = decode_hint(&f.store().get_hint(&reads_key(f1, BUILD)).unwrap().unwrap()).unwrap();
    assert_eq!(hint, ["app::c"]);

    f.write(
        "c.cheby",
        &format!("{C}\npub fn extra() -> Int {{\n  1\n}}\n"),
    );
    let (edited, _) = front_of(f.dir());
    assert_eq!(front_key_of(&edited, "app", ROOT), f1, "the root's F1");
    let b = module(&front, "app::b");
    assert_eq!(
        edited.set.get(module(&edited, "app::b")).unwrap().hash,
        front.set.get(b).unwrap().hash
    );
    let (warm, log) = f.check();
    assert_eq!(log, expect_log(&MODULES, &["app::d"]));
    assert_eq!(warm, f.cold_check());
    assert_eq!(warm, cold);
}

/// An interface edit of `b` misses `b` and its importer, and nothing else.
#[test]
fn front_import_change_misses() {
    let mut f = several("import_change");
    f.check();
    f.write(
        "b.cheby",
        &format!("{B}\npub fn more() -> Int {{\n  2\n}}\n"),
    );
    let (warm, log) = f.check();
    assert_eq!(log, expect_log(&MODULES, &["app::c", "app::d"]));
    assert_eq!(warm, f.cold_check());
}

/// A module with an error that reads a hit prints what a cold build
/// prints: E0043 names `b`'s private `helper` and points at it, and the
/// decoded interface of `b` holds no `priv` item (D-953), so the reader is
/// checked again on fresh interfaces (D-1132), by
/// `cheby check` and by `cheby run`, which stops after the front end.
#[test]
fn front_reader_with_error_prints_cold_diagnostics() {
    let mut f = Fixture::new(
        "reader_error",
        &[
            (
                "main.cheby",
                "//! Root.\n\nimport app::b\n\nfn main() {\n  let _ = b::helper()\n  Nil\n}\n",
            ),
            (
                "b.cheby",
                "//! B.\n\npub fn api() -> Int {\n  helper()\n}\n\npriv fn helper() -> Int {\n  1\n}\n",
            ),
        ],
    );
    let all = ["app", "app::b"];
    let cold = f.cold_check();
    assert!(
        cold.contains("E0043") && cold.contains("src/b.cheby"),
        "{cold}"
    );
    let (first, log) = f.check();
    assert_eq!(log, expect_log(&all, &[]));
    assert_eq!(first, cold);
    let (warm, log) = f.check();
    assert_eq!(log, expect_log(&all, &["app::b"]));
    assert_eq!(warm, cold);
    let fresh = match prepare_run(f.dir(), RunOptions::default()).unwrap() {
        Prepared::CompileFailed(out) => out.render(),
        Prepared::Ready(_) => panic!("the root has an error"),
    };
    let (cache, log) = f.open();
    let prepared = prepare_run_cached(f.dir(), RunOptions::default(), &cache).unwrap();
    cache.finish(false).unwrap();
    assert_eq!(lines(&log), expect_log(&all, &["app::b"]));
    let Prepared::CompileFailed(out) = prepared else {
        panic!("the root has an error");
    };
    assert_eq!(out.render(), fresh);
}

/// Nothing is written for a module whose front end reports an error
/// (D-964): it misses every time, and the others hit.
#[test]
fn front_error_writes_nothing() {
    let mut f = several("error");
    f.write("e.cheby", "//! E.\n\nfn broken() -> Int {\n  \"text\"\n}\n");
    let all = ["app", "app::b", "app::c", "app::d", "app::e"];
    let (cold, log) = f.check();
    assert!(cold.contains("error["), "{cold}");
    assert_eq!(log, expect_log(&all, &[]));
    assert_eq!(f.records().len(), MODULES.len());
    let (warm, log) = f.check();
    assert_eq!(log, expect_log(&all, &MODULES));
    assert_eq!(warm, cold);
    assert_eq!(f.records().len(), MODULES.len());
}

/// A corrupted record is a miss once the package index does not answer for
/// it, and the next command writes it again (D-960, D-962). A corrupted
/// interface artifact is read only when a checked module needs the
/// interface (D-954): a warm check with no edit still hits everything and
/// prints what a cold check prints; after an edit of the root, which needs
/// `b`'s interface, the front end starts again, decodes every hit at its
/// lookup, finds every artifact bad, and checks every module from source
/// (D-1133), printing what a cold check prints.
#[test]
fn front_corrupt_record_or_artifact_misses() {
    let mut f = several("corrupt");
    let (cold, _) = f.check();
    for path in f.files("rec") {
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&path, bytes).unwrap();
    }
    f.spoil_index(CommandFamily::Check, &f.store());
    let (warm, log) = f.check();
    assert_eq!(log, expect_log(&MODULES, &[]));
    assert_eq!(warm, cold);
    let (_, log) = f.check();
    assert_eq!(log, expect_log(&MODULES, &MODULES));

    let artifacts: Vec<String> = f.records().iter().map(|r| r.artifact.to_hex()).collect();
    for path in f.files("blob") {
        if artifacts.contains(&path.file_name().unwrap().to_string_lossy().into_owned()) {
            let mut bytes = std::fs::read(&path).unwrap();
            let at = bytes.len() / 2;
            bytes[at] ^= 0xff;
            std::fs::write(&path, bytes).unwrap();
        }
    }
    let (warm, log) = f.check();
    assert_eq!(log, expect_log(&MODULES, &MODULES));
    assert_eq!(warm, cold);

    f.write(
        "main.cheby",
        &ROOT.replace("let unused = 2", "let unused = 3"),
    );
    let edited_cold = f.cold_check();
    let (warm, log) = f.check();
    assert_eq!(log, expect_log(&MODULES, &[]));
    assert_eq!(warm, edited_cold);
    // The artifacts were written again: the next check hits everything.
    let (warm, log) = f.check();
    assert_eq!(log, expect_log(&MODULES, &MODULES));
    assert_eq!(warm, edited_cold);
}

/// `CHEBY_DUMP_DIR` bypasses every read and still writes (D-965).
#[test]
fn front_dump_bypasses_reads() {
    let mut f = several("dump");
    f.check();
    let records = f.files("rec");
    let (cache, log) = f.open();
    let cache = cache.without_reads();
    let out = check_cached(f.dir(), &CheckOptions::default(), &cache).unwrap();
    cache.finish(false).unwrap();
    assert_eq!(lines(&log), expect_log(&MODULES, &[]));
    assert_eq!(out.render(), f.cold_check());
    assert_eq!(f.files("rec"), records);
}

/// A diagnostic of each label kind (a primary and a secondary label in
/// the module's own file, a label on another module's item, a help with
/// an edit, a note), stored and replayed, renders as the fresh one does,
/// with and without color and `--deny-warnings` (D-965, D-421, D-549).
#[test]
fn replay_renders_as_fresh() {
    let f = several("replay");
    let (front, out) = front_of(f.dir());
    let root = module(&front, "app");
    let c = module(&front, "app::c");
    let own = item_span(&front, root, "text");
    let other = item_span(&front, c, "Thing");
    let edit = Span::new(own.file, cheby_diag::TextRange::at(own.range.start(), 1));
    let warning = Diagnostic::new(codes::UNUSED_BINDING, "a warning")
        .with_primary(own, "primary")
        .with_secondary(edit, "secondary")
        .with_secondary(other, "in another module")
        .with_note("a note")
        .with_suggestion("a help", edit, "_");
    let error = Diagnostic::new(codes::SYNTAX_ERROR, "an error").with_primary(other, "");
    let fresh = vec![warning.clone(), error.clone()];
    let span_cx = SpanCx {
        own: own.file,
        own_len: u32::try_from(ROOT.len()).unwrap(),
        modules: &front.modules,
        set: &front.set,
        globals: &front.globals,
    };
    assert!(
        matches!(&span_cx.store(other), Some(StoredSpan::Item { module, .. }) if module == "app::c"),
        "{:?}",
        span_cx.store(other)
    );
    let blob = replay::encode_diagnostics(&[&[warning], &[error]], &span_cx).unwrap();
    let lists = replay::decode_diagnostics(&blob, 2, &span_cx).unwrap();
    let replayed: Vec<Diagnostic> = lists.into_iter().flatten().collect();
    assert_eq!(replayed, fresh);
    for colors in [Colors::Off, Colors::On] {
        for deny in [false, true] {
            assert_eq!(
                render_diagnostics(&replayed, &out.sources, colors, deny),
                render_diagnostics(&fresh, &out.sources, colors, deny)
            );
        }
    }
    // A malformed blob, or one whose spans no longer resolve, is nothing.
    assert_eq!(replay::decode_diagnostics(&blob, 1, &span_cx), None);
    assert_eq!(
        replay::decode_diagnostics(&blob[..blob.len() - 1], 2, &span_cx),
        None
    );
    let short = SpanCx {
        own_len: 3,
        ..span_cx
    };
    assert_eq!(replay::decode_diagnostics(&blob, 2, &short), None);
}

/// A span into another module that is not an item's cannot be stored
/// (D-965): the module is then not cached.
#[test]
fn replay_refuses_other_spans_into_other_modules() {
    let f = several("refuse");
    let (front, _) = front_of(f.dir());
    let root = module(&front, "app");
    let c = module(&front, "app::c");
    let thing = item_span(&front, c, "Thing");
    let inside = Span::new(
        thing.file,
        cheby_diag::TextRange::at(thing.range.start(), 1),
    );
    let span_cx = SpanCx {
        own: item_span(&front, root, "text").file,
        own_len: u32::try_from(ROOT.len()).unwrap(),
        modules: &front.modules,
        set: &front.set,
        globals: &front.globals,
    };
    assert_eq!(span_cx.store(inside), None);
    let d = Diagnostic::new(codes::UNUSED_BINDING, "w").with_primary(inside, "");
    assert_eq!(replay::encode_diagnostics(&[&[d]], &span_cx), None);
}

/// A record with summaries of every kind, for the summary tests.
fn summary_record() -> FrontRecord {
    let item = |module: &str, item: &str| ItemRef {
        module: module.to_owned(),
        item: item.to_owned(),
    };
    FrontRecord {
        interface: Hash128([1; 16]),
        artifact: Hash128([2; 16]),
        diagnostics: Hash128([3; 16]),
        imports: vec![ImportKey::Module {
            path: "app::b".to_owned(),
            hash: Hash128([4; 16]),
        }],
        externals: Vec::new(),
        reads: Vec::new(),
        references: vec![
            ReferenceSummary {
                item: "Shape".to_owned(),
                kind: MentionerKind::Hook,
                mentions: vec![item("app::a", "shape_equal")],
            },
            ReferenceSummary {
                item: "area".to_owned(),
                kind: MentionerKind::Function,
                mentions: vec![item("app::b", "Square::Square"), item("std::int", "abs")],
            },
            ReferenceSummary {
                item: "limit".to_owned(),
                kind: MentionerKind::Constant,
                mentions: vec![item("app::a", "area")],
            },
            ReferenceSummary {
                item: "area works".to_owned(),
                kind: MentionerKind::Test,
                mentions: vec![item("app::a", "area")],
            },
        ],
        uses: UseSummary {
            uses: vec![item("app::b", "Square")],
            candidates: vec![UnusedCandidate {
                item: "area".to_owned(),
                kind: "function".to_owned(),
                span: (40, 4),
            }],
        },
    }
}

/// Summaries survive the record's encoding (D-967, D-965), and any damaged
/// byte makes the record a miss (D-960).
#[test]
fn front_summary_round_trip() {
    let record = summary_record();
    assert!(record.holds_constant());
    let bytes = record.encode();
    assert_eq!(FrontRecord::decode(&bytes), Some(record));
    for at in 0..bytes.len() {
        let mut bad = bytes.clone();
        bad[at] ^= 0x01;
        assert_eq!(FrontRecord::decode(&bad), None, "{at}");
    }
}

/// The record's encoding with summaries is fixed: this constant changes
/// only with the encoding (version 2), never with the process (D-264).
#[test]
fn front_summary_stability() {
    let bytes = summary_record().encode();
    assert_eq!(
        Hash128::of(&bytes).to_hex(),
        "44f7e2de65392dc92ed288a925b511fb"
    );
}

/// A body edit that changes what a function mentions changes its
/// reference summary; names are module paths and item names (D-967).
#[test]
fn front_summary_body_edit_changes_mentions() {
    const MAIN: &str = "//! Root.\n\nimport app::b\n\nfn main() {\n  let _ = b::f()\n}\n";
    const B0: &str = "//! B.\n\npub fn f() -> Int {\n  g()\n}\n\npub fn g() -> Int {\n  1\n}\n\npub fn h() -> Int {\n  2\n}\n";
    const B1: &str = "//! B.\n\npub fn f() -> Int {\n  h()\n}\n\npub fn g() -> Int {\n  1\n}\n\npub fn h() -> Int {\n  2\n}\n";
    let mut f = Fixture::new("summary_edit", &[("main.cheby", MAIN), ("b.cheby", B0)]);
    let mentions_of = |f: &Fixture, item: &str| -> Vec<ItemRef> {
        let index = f.index(CommandFamily::Check).unwrap();
        let entry = index.get("app::b").unwrap();
        entry
            .record
            .references
            .iter()
            .find(|s| s.item == item && s.kind == MentionerKind::Function)
            .unwrap()
            .mentions
            .clone()
    };
    f.check();
    let item = |name: &str| ItemRef {
        module: "app::b".to_owned(),
        item: name.to_owned(),
    };
    assert_eq!(mentions_of(&f, "f"), [item("g")]);
    let root = f.index(CommandFamily::Check).unwrap();
    let main = &root.get("app").unwrap().record;
    assert!(
        main.references
            .iter()
            .any(|s| s.item == "main" && s.mentions == [item("f")]),
        "{:?}",
        main.references
    );
    assert!(main.uses.uses.contains(&item("f")), "{:?}", main.uses);
    f.write("b.cheby", B1);
    f.check();
    assert_eq!(mentions_of(&f, "f"), [item("h")]);
}

/// W0005 over use summaries (D-516, D-965): a use added or removed in one
/// module changes the warning of an item of another, which is not
/// checked again, and a warm check prints what a cold one prints.
#[test]
fn w0005_warm_use_removed_and_added() {
    const USES: &str =
        "//! Root.\n\nimport app::util\n\nfn main() {\n  let _ = util::helper()\n}\n";
    const NO_USE: &str =
        "//! Root.\n\nimport app::util\n\nfn main() {\n  let _ = util::other()\n}\n";
    const UTIL: &str =
        "//! Util.\n\nfn helper() -> Int {\n  1\n}\n\npub fn other() -> Int {\n  2\n}\n";
    let mut f = Fixture::new("w0005", &[("main.cheby", USES), ("util.cheby", UTIL)]);
    let (cold, _) = f.check();
    assert!(!cold.contains("W0005"), "{cold}");
    f.write("main.cheby", NO_USE);
    let removed = f.check_counted();
    assert_eq!(removed.parsed, ["app"], "util is not parsed");
    assert!(removed.output.contains("W0005"), "{}", removed.output);
    assert!(removed.output.contains("helper"), "{}", removed.output);
    assert_eq!(removed.output, f.cold_check());
    f.write("main.cheby", USES);
    let added = f.check_counted();
    assert_eq!(added.parsed, ["app"], "util is not parsed");
    assert!(!added.output.contains("W0005"), "{}", added.output);
    assert_eq!(added.output, f.cold_check());
}

/// A package without warnings or hidden reads.
const CLEAN_MAIN: &str =
    "//! Root.\n\nimport app::util\n\nfn main() {\n  let _ = util::double(2)\n}\n";
const CLEAN_UTIL: &str = "//! Util.\n\npub fn double(n: Int) -> Int {\n  n * 2\n}\n";

/// D-962: a warm check with no edit reads its package index and nothing
/// else (no record, hint or blob file is opened), and writes no file: an
/// unchanged index is not rewritten (D-1133).
#[test]
fn index_warm_check_opens_and_writes_nothing() {
    let mut f = Fixture::new(
        "index_quiet",
        &[("main.cheby", CLEAN_MAIN), ("util.cheby", CLEAN_UTIL)],
    );
    let cold = f.check_counted();
    assert!(cold.writes > 0);
    let warm = f.check_counted();
    assert_eq!(
        warm.lookups,
        expect_log(&["app", "app::util"], &["app", "app::util"])
    );
    // K6 (D-980): std's front records and interface artifacts are read too,
    // three opens per std module (`std_cache_tests.rs`); no package file.
    assert_eq!(
        warm.opens,
        1 + 3 * warm.std_lookups,
        "the index and std's records only"
    );
    assert_eq!(warm.writes, 0);
    assert_eq!(warm.output, cold.output);
}

/// A corrupt index and a stale one are ignored, never an error (D-962):
/// the modules' own files answer, and output equals a cold check's.
#[test]
fn index_stale_or_corrupt_is_ignored() {
    let mut f = several("index_stale");
    let (cold, _) = f.check();
    let store = f.store();
    let stale = store
        .get_hint(&f.index_key(CommandFamily::Check))
        .unwrap()
        .unwrap();
    f.spoil_index(CommandFamily::Check, &store);
    drop(store);
    let (warm, log) = f.check();
    assert_eq!(log, expect_log(&MODULES, &MODULES));
    assert_eq!(warm, cold);
    // An interface edit of `c`, then the index from before it.
    f.write(
        "c.cheby",
        &format!("{C}\npub fn extra() -> Int {{\n  1\n}}\n"),
    );
    f.check();
    let store = f.store();
    store
        .put_hint(&f.index_key(CommandFamily::Check), &stale)
        .unwrap();
    drop(store);
    let (warm, log) = f.check();
    assert_eq!(log, expect_log(&MODULES, &MODULES));
    assert_eq!(warm, f.cold_check());
}

/// An edit that closes an import cycle through modules the index let go
/// unparsed: the front end starts again with every module parsed, and
/// prints E0040 as a cold check does (D-1133).
#[test]
fn index_cycle_after_edit_starts_again() {
    let mut f = several("index_cycle");
    f.check();
    f.write(
        "c.cheby",
        &C.replacen("//! C.\n\n", "//! C.\n\nimport app::b\n\n", 1),
    );
    let warm = f.check_counted();
    assert!(warm.output.contains("E0040"), "{}", warm.output);
    assert_eq!(warm.output, f.cold_check());
    assert_eq!(
        warm.parsed, MODULES,
        "every module parsed once, after the restart"
    );
}

/// Two package roots on one cache keep two indexes (D-962), and each warm
/// check hits every module of its package.
#[test]
fn index_two_roots_keep_two_indexes() {
    let mut a = several("index_root_a");
    let other = Fixture::new(
        "index_root_b",
        &[("main.cheby", CLEAN_MAIN), ("util.cheby", CLEAN_UTIL)],
    );
    let check_other = |a: &mut Fixture| {
        let (cache, log) = a.open();
        let out = check_cached(other.dir(), &CheckOptions::default(), &cache).unwrap();
        cache.finish(false).unwrap();
        (out.render(), lines(&log))
    };
    a.check();
    let (other_cold, _) = check_other(&mut a);
    let store = a.store();
    let mine = store
        .get_hint(&PackageIndex::key(a.dir(), CommandFamily::Check, BUILD))
        .unwrap();
    let theirs = store
        .get_hint(&PackageIndex::key(other.dir(), CommandFamily::Check, BUILD))
        .unwrap();
    drop(store);
    let (mine, theirs) = (
        PackageIndex::decode(&mine.unwrap()).unwrap(),
        PackageIndex::decode(&theirs.unwrap()).unwrap(),
    );
    assert_eq!(mine.modules.len(), MODULES.len());
    assert_eq!(theirs.modules.len(), 2);
    let (_, log) = a.check();
    assert_eq!(log, expect_log(&MODULES, &MODULES));
    let (other_warm, log) = check_other(&mut a);
    assert_eq!(
        log,
        expect_log(&["app", "app::util"], &["app", "app::util"])
    );
    assert_eq!(other_warm, other_cold);
}

/// `Cache::finish` flushes the store's queued writes before it trims
/// (plan §2.2): an index put just before `finish` still keeps the old blob
/// it names, which the trim would otherwise delete (D-961).
#[test]
fn index_trim_waits_for_queued_writes() {
    let dir = TempDir::new("index_trim");
    let root = dir.path().join("cache");
    let store = Store::open(&root, BUILD).unwrap();
    let old = store.put_blob(b"an old artifact").unwrap();
    let unnamed = store.put_blob(b"an old blob no index names").unwrap();
    store.flush();
    let week_ago = std::time::SystemTime::now() - std::time::Duration::from_hours(7 * 24);
    for name in [old, unnamed] {
        let path = root
            .join("v1/blob")
            .join(name.to_hex().get(..2).unwrap())
            .join(name.to_hex());
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(week_ago)
            .unwrap();
    }
    let mut record = summary_record();
    record.artifact = old;
    let index = PackageIndex {
        modules: vec![crate::cache::index::IndexEntry {
            path: "app".to_owned(),
            source: Hash128([9; 16]),
            written: Vec::new(),
            f1: Hash128([10; 16]),
            f2: Hash128([11; 16]),
            record,
        }],
    };
    let key = PackageIndex::key(dir.path(), CommandFamily::Check, BUILD);
    // Queued, with more writes behind it.
    store.put_hint(&key, &index.encode()).unwrap();
    for n in 0..64_u32 {
        store.put_blob(format!("filler {n}").as_bytes()).unwrap();
    }
    Cache::with_store(store, CacheLog::new(None))
        .finish(true)
        .unwrap();
    let store = Store::open(&root, BUILD).unwrap();
    assert!(
        store.get_blob(old).unwrap().is_some(),
        "the indexed blob is kept"
    );
    assert_eq!(
        store.get_blob(unnamed).unwrap(),
        None,
        "the other is trimmed"
    );
    assert_eq!(
        PackageIndex::decode(&store.get_hint(&key).unwrap().unwrap()),
        Some(index)
    );
}

/// D-973's interface edit: `c`'s interface changes; its importer `b` and
/// its recorded hidden reader, the root, miss; `d` hits; only the misses
/// are parsed, and the output equals a cold check's.
#[test]
fn cache_hit_interface_edit() {
    let mut f = several("interface_edit");
    f.check();
    f.write(
        "c.cheby",
        &format!("{C}\npub fn extra() -> Int {{\n  1\n}}\n"),
    );
    let warm = f.check_counted();
    assert_eq!(warm.lookups, expect_log(&MODULES, &["app::d"]));
    assert_eq!(warm.parsed, ["app", "app::b", "app::c"]);
    assert_eq!(warm.checked, ["app", "app::b", "app::c"]);
    assert_eq!(warm.output, f.cold_check());
}

/// Deletes the blob `name` from the fixture's store.
fn delete_blob(f: &Fixture, name: Hash128) {
    let hex = name.to_hex();
    let path = f
        .files("blob")
        .into_iter()
        .find(|p| p.file_name().unwrap().to_string_lossy() == hex)
        .unwrap();
    std::fs::remove_file(path).unwrap();
}

/// The front record of `module` in the check family's package index.
fn indexed_record(f: &Fixture, module: &str) -> FrontRecord {
    f.index(CommandFamily::Check)
        .unwrap()
        .get(module)
        .unwrap()
        .record
        .clone()
}

/// A hit whose diagnostics blob is gone is a miss (D-960), also in the
/// attempt after a restart, which decodes each hit's interface at its
/// lookup: that hit publishes nothing before it turns into a miss, so its
/// check from source publishes its interface once, and the warm check
/// prints what a cold one prints. Here `x`'s import of `g` no longer
/// resolves, which restarts the front end.
#[test]
fn cache_hit_lost_diagnostics_after_restart() {
    let mut f = Fixture::new(
        "lost_diagnostics_restart",
        &[
            (
                "main.cheby",
                "//! App.\n\nimport std::io\nimport app::a\nimport app::x\n\n\
                 fn main() {\n  io::println(a::twice(\"x\"))\n  io::println(x::name())\n}\n",
            ),
            (
                "a.cheby",
                "//! A.\n\n/// Twice.\npub fn twice(s: String) -> String {\n  \
                 let unused = 1\n  \"{s}{s}\"\n}\n",
            ),
            (
                "x.cheby",
                "//! X.\n\nimport app::g\n\n/// Name.\npub fn name() -> String {\n  g::name()\n}\n",
            ),
            (
                "g.cheby",
                "//! G.\n\n/// Name.\npub fn name() -> String {\n  \"g\"\n}\n",
            ),
        ],
    );
    f.check();
    let a = indexed_record(&f, "app::a");
    assert_ne!(a.diagnostics, Hash128::of(&replay::empty_blob(3)));
    delete_blob(&f, a.diagnostics);
    std::fs::remove_file(f.dir().join("src/g.cheby")).unwrap();
    let cold = f.cold_check();
    assert!(cold.contains("W0002"), "{cold}");
    let (warm, _) = f.check();
    assert_eq!(warm, cold);
}

/// `a`'s diagnostics blob and `b`'s interface artifact are gone, with no
/// edit: `a`'s replay fails, so `a` is checked from source; that needs
/// `b`'s interface, which cannot be decoded, so the front end starts
/// again. In that attempt `a`'s interface decodes, but `a` is a miss
/// again: it must not have been published before (D-960, D-954).
#[test]
fn cache_hit_lost_diagnostics_and_artifact() {
    let mut f = Fixture::new(
        "lost_diagnostics_artifact",
        &[
            (
                "main.cheby",
                "//! App.\n\nimport std::io\nimport app::a\n\n\
                 fn main() {\n  io::println(a::twice(\"x\"))\n}\n",
            ),
            (
                "a.cheby",
                "//! A.\n\nimport app::b\n\n/// Twice.\npub fn twice(s: String) -> String {\n  \
                 let unused = 1\n  b::join(s, s)\n}\n",
            ),
            (
                "b.cheby",
                "//! B.\n\n/// Join.\npub fn join(x: String, y: String) -> String {\n  \
                 let unused = 2\n  \"{x}{y}\"\n}\n",
            ),
        ],
    );
    let (first, _) = f.check();
    delete_blob(&f, indexed_record(&f, "app::a").diagnostics);
    delete_blob(&f, indexed_record(&f, "app::b").artifact);
    let cold = f.cold_check();
    assert_eq!(first, cold);
    for _ in 0..2 {
        let (warm, _) = f.check();
        assert_eq!(warm, cold);
    }
}

/// A phase-A error stops the front end before it runs (D-452), and with it
/// the replay of the phase-A diagnostics of the modules the package index
/// let go unparsed: the warm check starts again and parses every module,
/// so it prints `b`'s W0001 as a cold check does (D-268, D-965).
#[test]
fn cache_hit_phase_a_error_keeps_unparsed_warnings() {
    let main = "//! Root.\n\nimport std::io\nimport app::b\n\n\
                fn main() {\n  let n = b::g()\n  io::println(\"{n}\")\n}\n";
    let b = "//! B.\n\npub fn g() -> Int {\n  let t: (Int, Int, Int, Int) = (1, 2, 3, 4)\n  \
             let (a, _, _, _) = t\n  a\n}\n";
    let mut f = Fixture::new("phase_a_error", &[("main.cheby", main), ("b.cheby", b)]);
    let (first, _) = f.check();
    assert!(first.contains("W0001"), "{first}");
    // An error in phase A of the root, `b` unchanged.
    f.write(
        "main.cheby",
        "//! Root.\n\nimport std::io\nimport app::b\n\nfn main() {\n  io::println(\"{b::g()}\")\n}\n",
    );
    let cold = f.cold_check();
    assert!(cold.contains("W0001") && cold.contains("E0012"), "{cold}");
    let (warm, _) = f.check();
    assert_eq!(warm, cold);
    // A module file with an invalid name, the root restored.
    f.write("main.cheby", main);
    f.check();
    f.write("Bad-Name.cheby", "//! Bad.\n");
    let cold = f.cold_check();
    assert!(cold.contains("W0001"), "{cold}");
    let (warm, _) = f.check();
    assert_eq!(warm, cold);
}

/// A 10k-line `wide` project from `cheby_bench_gen`: the second
/// `cheby check` parses no package module and hits every one (D-962).
#[test]
fn cache_hit_wide_project_parses_nothing() {
    let project = cheby_bench_gen::generate(&cheby_bench_gen::Config {
        lines: 10_000,
        shape: cheby_bench_gen::Shape::Wide,
        seed: 1,
    });
    let package = TempDir::new("wide");
    project.write(package.path()).unwrap();
    let cache_dir = TempDir::new("wide_cache");
    let run = |n: u32| {
        let log = cache_dir.path().join(format!("log-{n}"));
        let store = Store::open(&cache_dir.path().join("cache"), BUILD).unwrap();
        let cache = Cache::with_store(store, CacheLog::new(Some(log.clone())));
        let out = check_cached(package.path(), &CheckOptions::default(), &cache).unwrap();
        let parsed = cache.parsed_modules();
        cache.finish(false).unwrap();
        (out.render(), lines(&log), parses(&log), parsed)
    };
    let (cold, cold_log, cold_parses, _) = run(1);
    assert_eq!(cold_log.len(), project.modules());
    assert_eq!(cold_parses.len(), project.modules());
    let (warm, warm_log, warm_parses, parsed) = run(2);
    assert_eq!(warm, cold);
    assert_eq!(warm_log.len(), project.modules());
    assert!(warm_log.iter().all(|l| l.ends_with(" hit")), "{warm_log:?}");
    assert_eq!(warm_parses, Vec::<String>::new());
    assert_eq!(parsed, Vec::<String>::new());
}

/// Module `a` holds a constant whose initializer calls a function of `b`;
/// `c` is unrelated.
fn constants_fixture(name: &str) -> Fixture {
    Fixture::new(
        name,
        &[
            (
                "main.cheby",
                "//! Root.\n\nimport app::a\nimport app::c\nimport std::io\n\n\
                 fn main() {\n  let n = a::limit + c::other()\n  io::println(\"{n}\")\n}\n",
            ),
            (
                "a.cheby",
                "//! A.\n\nimport app::b\n\npub const limit: Int = b::compute(3)\n",
            ),
            (
                "b.cheby",
                "//! B.\n\npub fn compute(n: Int) -> Int {\n  n * 2\n}\n",
            ),
            ("c.cheby", "//! C.\n\npub fn other() -> Int {\n  7\n}\n"),
        ],
    )
}

/// D-1134: a warm check parses and checks
/// exactly the constant's module and what it imports, on fresh
/// interfaces, so constant evaluation runs, and prints what a cold check
/// prints.
#[test]
fn cache_hit_constants_parse_only_their_modules() {
    let mut f = constants_fixture("constants");
    let (cold, _) = f.check();
    assert_eq!(cold, f.cold_check());
    let warm = f.check_counted();
    assert_eq!(
        warm.lookups,
        expect_log(
            &["app", "app::a", "app::b", "app::c"],
            &["app", "app::a", "app::b", "app::c"]
        )
    );
    assert_eq!(warm.parsed, ["app::a", "app::b"]);
    assert_eq!(warm.checked, ["app::a", "app::b"]);
    assert_eq!(warm.output, cold);
}

/// An edit of `b`'s body that makes the constant's evaluation fail: the
/// warm check prints the diagnostic a cold check of the edited package
/// prints.
#[test]
fn cache_hit_constants_body_edit_fails_evaluation() {
    let mut f = constants_fixture("constants_edit");
    f.check();
    f.write(
        "b.cheby",
        "//! B.\n\npub fn compute(n: Int) -> Int {\n  n / (n - 3)\n}\n",
    );
    let cold = f.cold_check();
    assert!(cold.contains("error["), "{cold}");
    let warm = f.check_counted();
    assert_eq!(warm.output, cold);
    assert_eq!(warm.checked, ["app::a", "app::b"]);
}

/// No module is checked twice (D-1132): a warm
/// `cheby check` checks nothing; a first `cheby run` and `cheby test` check
/// each package module once and a warm one checks none (D-1250, K4d); the
/// build-mode front end of `cheby build` checks each package module once.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn cache_hit_no_module_checked_twice() {
    let mut f = several("checked_once");
    f.check();
    let warm = f.check_counted();
    assert_eq!(warm.checked, Vec::<String>::new());
    let once = |checked: Vec<String>| {
        assert_eq!(checked, MODULES, "each module once");
    };
    for first in [true, false] {
        let (cache, _) = f.open();
        let prepared = prepare_run_cached(f.dir(), RunOptions::default(), &cache).unwrap();
        assert!(matches!(prepared, Prepared::Ready(_)));
        if first {
            once(cache.checked_modules());
        } else {
            assert_eq!(cache.checked_modules(), Vec::<String>::new());
        }
        cache.finish(false).unwrap();
    }
    for first in [true, false] {
        let (cache, _) = f.open();
        let prepared = prepare_test_cached(f.dir(), &TestOptions::default(), &cache).unwrap();
        assert!(matches!(prepared, PreparedTests::Ready(_)));
        if first {
            once(cache.checked_modules());
        } else {
            assert_eq!(cache.checked_modules(), Vec::<String>::new());
        }
        cache.finish(false).unwrap();
    }
    for _ in 0..2 {
        let (cache, _) = f.open();
        let mut out = CheckOutput::default();
        let front =
            front_end_for(f.dir(), frontend::Mode::Build, None, &mut out, Some(&cache)).unwrap();
        assert!(front.is_some());
        once(cache.checked_modules());
        cache.finish(false).unwrap();
    }
    // After an interface edit too.
    f.write(
        "c.cheby",
        &format!("{C}\npub fn extra() -> Int {{\n  1\n}}\n"),
    );
    let edited = f.check_counted();
    assert_eq!(edited.checked, ["app", "app::b", "app::c"]);
}

/// D-1137: a package without hidden reads writes no
/// read-set hint; the store's only hint file is the package index.
#[test]
fn hint_empty_none_written() {
    let mut f = Fixture::new(
        "hint_none",
        &[("main.cheby", CLEAN_MAIN), ("util.cheby", CLEAN_UTIL)],
    );
    f.check();
    let index = f.index_key(CommandFamily::Check).hash().to_hex();
    let hints: Vec<String> = f
        .files("hint")
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(hints, [index]);
}

/// A module with a hidden read still gets its hint, and a warm check
/// still hits it, from the index and from the files.
#[test]
fn hint_empty_hidden_read_kept() {
    let mut f = several("hint_kept");
    f.check();
    let (front, _) = front_of(f.dir());
    let f1 = front_key_of(&front, "app", ROOT);
    let hint = decode_hint(&f.store().get_hint(&reads_key(f1, BUILD)).unwrap().unwrap()).unwrap();
    assert_eq!(hint, ["app::c"]);
    assert_eq!(f.files("hint").len(), 2, "the root's hint and the index");
    let (_, log) = f.check();
    assert_eq!(log, expect_log(&MODULES, &MODULES));
    f.spoil_index(CommandFamily::Check, &f.store());
    let (_, log) = f.check();
    assert_eq!(log, expect_log(&MODULES, &MODULES));
}

/// A lost non-empty hint reads as the empty set, so the module misses
/// (never a wrong hit), the output equals a cold check's, and the hint is
/// written again.
#[test]
fn hint_empty_lost_hint_misses() {
    let mut f = several("hint_lost");
    let (cold, _) = f.check();
    let (front, _) = front_of(f.dir());
    let f1 = front_key_of(&front, "app", ROOT);
    let hint_file = f
        .files("hint")
        .into_iter()
        .find(|p| p.file_name().unwrap().to_string_lossy() == reads_key(f1, BUILD).hash().to_hex())
        .unwrap();
    std::fs::remove_file(hint_file).unwrap();
    f.spoil_index(CommandFamily::Check, &f.store());
    let (warm, log) = f.check();
    assert_eq!(log, expect_log(&MODULES, &["app::b", "app::c", "app::d"]));
    assert_eq!(warm, cold);
    let (warm, log) = f.check();
    assert_eq!(log, expect_log(&MODULES, &MODULES));
    assert_eq!(warm, cold);
}

// D-973's cache-hit tests 1, 2 and 6 (K4d; K8 gathers the eight): each
// drives `cheby run`'s command function with a cache over a fresh store
// and reads `CHEBY_CACHE_LOG`'s `code.run` lines (D-1224), never a timing.

/// The root of the code cache-hit tests: it calls `util::check` and
/// `other::twice`, never anything of `lone`.
const HIT_MAIN: &str = "//! Root.\n\nimport app::other\nimport app::util\nimport std::io\n\n\
    fn main() {\n  let ok = util::check(\"x\")\n  let n = other::twice(21)\n  io::println(\"{ok} {n}\")\n}\n";
/// `check` is package-visible (a `pub` function never borrows, D-787) and
/// only compares its parameter: it borrows it.
const HIT_UTIL: &str =
    "//! Util.\n\n/// Whether `s` is `x`.\nfn check(s: String) -> Bool {\n  s == \"x\"\n}\n";
/// A body edit of `check` that changes no mask.
const HIT_UTIL_BODY: &str =
    "//! Util.\n\n/// Whether `s` is `x`.\nfn check(s: String) -> Bool {\n  s == \"y\"\n}\n";
/// A body edit of `check` that puts its parameter in a list: it owns it
/// now, so its mask changes (D-812).
const HIT_UTIL_MASK: &str =
    "//! Util.\n\n/// Whether `s` is `x`.\nfn check(s: String) -> Bool {\n  [s] == [\"x\"]\n}\n";
const HIT_OTHER: &str = "//! Other.\n\nimport app::lone\n\n/// Twice `n`.\npub fn twice(n: Int) -> Int {\n  lone::add(n, n)\n}\n";
const HIT_LONE: &str =
    "//! Lone.\n\n/// The sum.\npub fn add(a: Int, b: Int) -> Int {\n  a + b\n}\n";

impl Fixture {
    /// `cheby run`'s build through the cache: the log's `code.run` lines
    /// and its `front` lines.
    fn run_build(&mut self) -> (Vec<String>, Vec<String>) {
        let (cache, log) = self.open();
        match prepare_run_cached(self.dir(), RunOptions::default(), &cache).unwrap() {
            Prepared::Ready(_) => {}
            Prepared::CompileFailed(out) => panic!("{}", out.render()),
        }
        cache.finish(false).unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        let of = |prefix: &str| -> Vec<String> {
            text.lines()
                .filter(|l| l.starts_with(prefix))
                .map(str::to_owned)
                .collect()
        };
        (of("code.run "), lines(&log))
    }
}

/// `code.run <module> hit|miss` for each module, missing those of
/// `misses`.
fn code_run_lines(all: &[&str], misses: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = all
        .iter()
        .map(|m| {
            let verdict = if misses.contains(m) { "miss" } else { "hit" };
            format!("code.run {m} {verdict}")
        })
        .collect();
    v.sort();
    v
}

/// The modules of the code cache-hit tests' package.
const HIT_MODULES: [&str; 4] = ["app", "app::lone", "app::other", "app::util"];

/// Whether the environment's MIR options infer masks (`CHEBY_RC=naive`
/// infers none, so no edit changes one).
fn infers_masks() -> bool {
    crate::backend::mir_options_from_env().is_ok_and(|mir| mir.borrow)
}

/// Cache-hit test 1 (D-973, D-1193): a body edit that changes no mask
/// misses only its module's code record; every other module's hits.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn cache_hit_body_edit_misses_only_its_module() {
    let mut f = Fixture::new(
        "hit_body",
        &[
            ("main.cheby", HIT_MAIN),
            ("util.cheby", HIT_UTIL),
            ("other.cheby", HIT_OTHER),
            ("lone.cheby", HIT_LONE),
        ],
    );
    let (cold, _) = f.run_build();
    assert_eq!(cold, code_run_lines(&HIT_MODULES, &HIT_MODULES));
    let (warm, _) = f.run_build();
    assert_eq!(warm, code_run_lines(&HIT_MODULES, &[]));
    f.write("util.cheby", HIT_UTIL_BODY);
    let (edited, _) = f.run_build();
    assert_eq!(edited, code_run_lines(&HIT_MODULES, &["app::util"]));
}

/// Cache-hit test 2 (D-973, D-812): an edit that changes the borrow mask of
/// a package-visible function misses the code of its module and of every
/// module of the package that calls it (`app`), and nothing else: `other`
/// and `lone` call nothing of `util`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn cache_hit_mask_edit_misses_the_callers() {
    if !infers_masks() {
        return;
    }
    let mut f = Fixture::new(
        "hit_mask",
        &[
            ("main.cheby", HIT_MAIN),
            ("util.cheby", HIT_UTIL),
            ("other.cheby", HIT_OTHER),
            ("lone.cheby", HIT_LONE),
        ],
    );
    let (cold, _) = f.run_build();
    assert_eq!(cold, code_run_lines(&HIT_MODULES, &HIT_MODULES));
    f.write("util.cheby", HIT_UTIL_MASK);
    let (edited, _) = f.run_build();
    assert_eq!(edited, code_run_lines(&HIT_MODULES, &["app", "app::util"]));
    let (again, _) = f.run_build();
    assert_eq!(again, code_run_lines(&HIT_MODULES, &[]));
}

/// Cache-hit test 6 (D-973, D-955): `app` passes `b::Wrapper` values along
/// and never reads `c`'s interface in its check, but its borrow inference
/// walks `Wrapper`'s field into `c::Thing` (a hidden read of its lowering,
/// `reads_tests.rs`). An interface edit of `c` that adds a field to `Thing`
/// leaves `app`'s front record a hit and misses its code: its code identity
/// holds `c`'s interface hash. `lone` reads nothing of `c` and hits.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn cache_hit_reader_lowering_edit_misses_the_reader() {
    const MAIN: &str = "//! Root.\n\nimport app::b\nimport app::lone\n\n\
        fn keep(w: b::Wrapper) -> b::Wrapper {\n  w\n}\n\n\
        fn main() {\n  let _ = keep(b::wrap())\n  let _ = lone::add(1, 2)\n  Nil\n}\n";
    const B: &str = "//! B.\n\nimport app::c\n\npub type Wrapper { inner: c::Thing }\n\n\
        pub fn make() -> c::Thing {\n  c::Thing { size: 1 }\n}\n\n\
        pub fn wrap() -> Wrapper {\n  Wrapper { inner: make() }\n}\n";
    const C0: &str = "//! C.\n\npub type Thing { size: Int }\n";
    const C1: &str = "//! C.\n\npub type Thing { size: Int, label: Int }\n";
    const B1: &str = "//! B.\n\nimport app::c\n\npub type Wrapper { inner: c::Thing }\n\n\
        pub fn make() -> c::Thing {\n  c::Thing { size: 1, label: 2 }\n}\n\n\
        pub fn wrap() -> Wrapper {\n  Wrapper { inner: make() }\n}\n";
    let mut f = Fixture::new(
        "hit_lowering",
        &[
            ("main.cheby", MAIN),
            ("b.cheby", B),
            ("c.cheby", C0),
            ("lone.cheby", HIT_LONE),
        ],
    );
    let all = ["app", "app::b", "app::c", "app::lone"];
    let (cold, _) = f.run_build();
    assert_eq!(cold, code_run_lines(&all, &all));
    f.write("c.cheby", C1);
    f.write("b.cheby", B1);
    let (edited, front) = f.run_build();
    // `app`'s front record hits: its check reads nothing of `c`.
    assert!(front.contains(&"front app hit".to_owned()), "{front:?}");
    assert_eq!(edited, code_run_lines(&all, &["app", "app::b", "app::c"]));
}

/// D-1250 (K4d, GAP(warm-check-before-keys)): the entry of every
/// example's `main` and of every test it holds, `Result` mains with their
/// `Show` dictionaries included, round-trips through the warm facts'
/// encoding by names (`warm_keys::Naming`), so a warm hit's entries unit
/// gets what a check would give it.
#[test]
fn warm_run_entries_round_trip_on_every_example() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
    let mut examples: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("cheby.toml").is_file())
        .collect();
    examples.sort();
    let (mut mains, mut results, mut tests) = (0_usize, 0_usize, 0_usize);
    for example in examples {
        let mut out = CheckOutput::default();
        let Some((front, _)) = front_end(&example, frontend::Mode::Build, &mut out).unwrap() else {
            continue;
        };
        let naming = crate::warm_keys::Naming {
            modules: &front.modules,
            set: &front.set,
            globals: &front.globals,
        };
        for &m in &front.order {
            if front.modules.get(m).unwrap().package != cheby_iface::PackageId::LOCAL {
                continue;
            }
            let checked = front.checked_quiet(m).unwrap();
            let entries = checked
                .thir
                .fns
                .iter()
                .filter_map(|f| f.entry.as_ref())
                .chain(checked.thir.tests.iter().map(|t| &t.entry));
            for entry in entries {
                let bytes = naming
                    .encode_entry(entry)
                    .unwrap_or_else(|| panic!("{}: {entry:?}", example.display()));
                assert_eq!(naming.decode_entry(&bytes).as_ref(), Some(entry));
                if matches!(entry, cheby_types::thir::EntryInfo::Result { .. }) {
                    results += 1;
                }
            }
            mains += checked
                .thir
                .fns
                .iter()
                .filter(|f| f.entry.is_some())
                .count();
            tests += checked.thir.tests.len();
        }
    }
    assert!(
        mains >= 30 && results >= 15 && tests >= 50,
        "{mains} {results} {tests}"
    );
}
