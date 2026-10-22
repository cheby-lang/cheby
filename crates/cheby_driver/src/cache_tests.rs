//! The front records, their keys and the replayed diagnostics (D-955,
//! D-956, D-964, D-965), and the cache-hit tests of D-973 that read the
//! lookups through `CHEBY_CACHE_LOG`'s file.
//!
//! Every test opens its own store in a fresh directory, so no test reads
//! or fills the developer's cache (GAP(cache-tests-isolated)).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use cheby_diag::{Colors, Diagnostic, Span, codes};
use cheby_iface::ModuleId;

use super::*;
use crate::cache::front::{
    External, FrontKey, FrontRecord, ImportKey, decode_hint, encode_hint, front_identity, reads_key,
};
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
        let (cache, log) = self.open();
        let out = check_cached(self.dir(), &CheckOptions::default(), &cache).unwrap();
        cache.finish(true).unwrap();
        (out.render(), lines(&log))
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

    /// Every front record in the store.
    fn records(&self) -> Vec<FrontRecord> {
        self.files("rec")
            .iter()
            .map(|p| FrontRecord::decode(&std::fs::read(p).unwrap()[32..]).unwrap())
            .collect()
    }
}

fn lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
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
    let (cold, log) = f.check();
    assert_eq!(log, expect_log(&MODULES, &[]));
    assert!(cold.contains("W0002") && cold.contains("W0005"), "{cold}");
    assert_eq!(cold, f.cold_check());
    let (warm, log) = f.check();
    assert_eq!(log, expect_log(&MODULES, &MODULES));
    assert_eq!(warm, cold);

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
/// the span the fresh front end gives `target` now. A module with an error
/// makes `cheby run` stop after the warm pass, which therefore prints the
/// replayed diagnostic without checking `a` again (a check from source
/// would not reproduce a synthetic diagnostic).
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
    let (cache, log) = f.open();
    let Prepared::CompileFailed(_) =
        prepare_run_cached(f.dir(), RunOptions::default(), &cache).unwrap()
    else {
        panic!("`bad` has an error");
    };
    cache.finish(false).unwrap();
    assert_eq!(lines(&log), expect_log(&all, &[]));

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
    let blob =
        replay::encode_diagnostics(&[std::slice::from_ref(&synthetic), &[]], &span_cx).unwrap();
    let store = f.store();
    let f1 = front_key_of(&front, "app::a", A);
    let hint = decode_hint(&store.get_hint(&reads_key(f1, BUILD)).unwrap().unwrap()).unwrap();
    assert!(hint.is_empty(), "{hint:?}");
    let f2 = front_identity(f1, BUILD, &[]);
    let mut record = FrontRecord::decode(&store.get_record(&f2).unwrap().unwrap()).unwrap();
    record.diagnostics = store.put_blob(&blob).unwrap();
    store.put_record(&f2, &record.encode()).unwrap();
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
    let prepared = prepare_run_cached(f.dir(), RunOptions::default(), &cache).unwrap();
    cache.finish(false).unwrap();
    assert_eq!(lines(&log), expect_log(&all, &["app", "app::a"]));
    let Prepared::CompileFailed(out) = prepared else {
        panic!("`bad` has an error");
    };
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
/// checked again on fresh interfaces (GAP(warm-recheck-readers)), by
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

/// A corrupted record, and then a corrupted interface artifact, are
/// misses, and the next command writes them again (D-960).
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
    assert_eq!(log, expect_log(&MODULES, &[]));
    assert_eq!(warm, cold);
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
