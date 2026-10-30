//! K5b's tests of constant results in the store (D-977, D-964, D-973,
//! D-978): the key, the entries, and the cache-hit tests 3 and 7 of D-973
//! that read the lookups through `CHEBY_CACHE_LOG`'s file. Every test
//! opens its own store in a fresh directory (D-1128).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use super::consts::{ConstEntry, ConstLimits, const_key, keys_after_check, keys_from_records};
use super::index::{CommandFamily, PackageIndex};
use super::{BuildId, BuildKind, CacheLog, Hash128, Settings, Store};
use crate::{Cache, CheckOptions, Prepared, RunOptions, check_cached, prepare_run_cached};

/// The build ID of every test store.
const BUILD: BuildId = BuildId([5; 16]);

fn settings() -> Settings {
    Settings {
        kind: BuildKind::Plain,
        rc_naive: false,
        unwind_edges: true,
        checked_counts: false,
        stack_check: true,
        target: "aarch64-macos".to_owned(),
    }
}

const LIMITS: ConstLimits = ConstLimits {
    time_limit: Duration::from_secs(10),
    memory_limit: 1 << 30,
    targets: cheby_package::Targets::ALL,
};

#[test]
fn const_cache_key_holds_every_seed_input() {
    let build = BuildId([7; 16]);
    let deps = [("app::a", Hash128([1; 16])), ("app::b", Hash128([2; 16]))];
    let base = const_key(build, &settings(), &LIMITS, "app", "app::a", "x", &deps);
    assert_eq!(
        base,
        const_key(build, &settings(), &LIMITS, "app", "app::a", "x", &deps)
    );
    let other_dep = [("app::a", Hash128([1; 16])), ("app::b", Hash128([3; 16]))];
    let moved = [("app::a", Hash128([1; 16])), ("app::bb", Hash128([2; 16]))];
    let fewer = [("app::a", Hash128([1; 16]))];
    // Equal seeds from other inputs never share an entry: the fields are
    // length-prefixed, so `app::a` + `x` is not `app::` + `ax`.
    let shifted = const_key(build, &settings(), &LIMITS, "app", "app::", "ax", &deps);
    assert_ne!(base, shifted);
    for key in [
        const_key(
            BuildId([8; 16]),
            &settings(),
            &LIMITS,
            "app",
            "app::a",
            "x",
            &deps,
        ),
        const_key(build, &settings(), &LIMITS, "app", "app::a", "y", &deps),
        const_key(build, &settings(), &LIMITS, "app", "app::b", "x", &deps),
        const_key(build, &settings(), &LIMITS, "ap", "app::a", "x", &deps),
        const_key(
            build,
            &settings(),
            &LIMITS,
            "app",
            "app::a",
            "x",
            &other_dep,
        ),
        const_key(build, &settings(), &LIMITS, "app", "app::a", "x", &moved),
        const_key(build, &settings(), &LIMITS, "app", "app::a", "x", &fewer),
        const_key(
            build,
            &Settings {
                rc_naive: true,
                ..settings()
            },
            &LIMITS,
            "app",
            "app::a",
            "x",
            &deps,
        ),
        const_key(
            build,
            &Settings {
                kind: BuildKind::Run,
                ..settings()
            },
            &LIMITS,
            "app",
            "app::a",
            "x",
            &deps,
        ),
        const_key(
            build,
            &settings(),
            &ConstLimits {
                time_limit: Duration::from_secs(11),
                ..LIMITS
            },
            "app",
            "app::a",
            "x",
            &deps,
        ),
        const_key(
            build,
            &settings(),
            &ConstLimits {
                memory_limit: 1 << 20,
                ..LIMITS
            },
            "app",
            "app::a",
            "x",
            &deps,
        ),
        // E0078's range check depends on the checked targets (D-540).
        const_key(
            build,
            &settings(),
            &ConstLimits {
                targets: cheby_package::Targets::only(cheby_package::Target::Native),
                ..LIMITS
            },
            "app",
            "app::a",
            "x",
            &deps,
        ),
    ] {
        assert_ne!(key, base);
    }
}

/// A fresh directory under the system's temporary directory, removed on
/// drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cheby_driver_consts_{}_{n}_{name}",
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

/// A package `app` with these files under `src/` and this manifest
/// tail, and a cache directory.
struct Fixture {
    package: TempDir,
    cache: TempDir,
    runs: u32,
}

/// What one command printed and looked up.
struct Ran {
    /// The error codes it reported, in order.
    codes: Vec<String>,
    /// The rendered diagnostics.
    rendered: String,
    /// The exit status of `main`, for a run that compiled.
    status: Option<i32>,
    /// The log's lines.
    log: Vec<String>,
}

impl Ran {
    /// The log's lines that start with `prefix` and a space.
    fn lines(&self, prefix: &str) -> Vec<String> {
        let start = format!("{prefix} ");
        self.log
            .iter()
            .filter(|l| l.starts_with(&start))
            .cloned()
            .collect()
    }

    /// The subjects of the log's `prefix` lines that missed.
    fn misses(&self, prefix: &str) -> Vec<String> {
        self.lines(prefix)
            .iter()
            .filter_map(|l| l.strip_suffix(" miss"))
            .filter_map(|l| l.split_once(' ').map(|(_, s)| s.to_owned()))
            .collect()
    }
}

impl Fixture {
    fn new(name: &str, manifest_tail: &str, files: &[(&str, &str)]) -> Self {
        let package = TempDir::new(name);
        std::fs::write(
            package.path().join("cheby.toml"),
            format!("name = \"app\"\nversion = \"0.1.0\"\n{manifest_tail}"),
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

    fn cache_dir(&self, name: &str) -> PathBuf {
        self.cache.path().join(name)
    }

    /// A command's cache over the store `name`, logging to a fresh file.
    fn open(&mut self, name: &str) -> (Cache, PathBuf) {
        self.runs = self.runs.checked_add(1).unwrap();
        let log = self.cache.path().join(format!("log-{}", self.runs));
        let store = Store::open(&self.cache_dir(name), BUILD).unwrap();
        (
            Cache::with_store(store, CacheLog::new(Some(log.clone()))),
            log,
        )
    }

    /// `cheby check` through the store `name`.
    fn check(&mut self, name: &str) -> Ran {
        let (cache, log) = self.open(name);
        let out = check_cached(self.dir(), &CheckOptions::default(), &cache).unwrap();
        cache.finish(true).unwrap();
        Ran {
            codes: codes(&out.diagnostics),
            rendered: out.render(),
            status: None,
            log: read_log(&log),
        }
    }

    /// `cheby run` through the store `name`.
    fn run(&mut self, name: &str) -> Ran {
        let (cache, log) = self.open(name);
        let prepared = prepare_run_cached(self.dir(), RunOptions::default(), &cache).unwrap();
        let (codes, rendered, status) = match prepared {
            Prepared::CompileFailed(out) => (codes(&out.diagnostics), out.render(), None),
            Prepared::Ready(program) => {
                let codes = codes(&program.output().diagnostics);
                let rendered = program.output().render();
                (codes, rendered, Some(program.run().unwrap()))
            }
        };
        cache.finish(false).unwrap();
        Ran {
            codes,
            rendered,
            status,
            log: read_log(&log),
        }
    }

    /// The record files of the store `name`.
    fn records(&self, name: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let dir = self.cache_dir(name).join("v1/rec");
        for shard in std::fs::read_dir(dir).unwrap() {
            for file in std::fs::read_dir(shard.unwrap().path()).unwrap() {
                out.push(file.unwrap().path());
            }
        }
        out.sort();
        out
    }

    /// The constant entries of the store `name` whose image or failure
    /// belongs to `path`, with their files.
    fn entries(&self, name: &str, path: &str) -> Vec<(PathBuf, ConstEntry)> {
        let mut prefix = b"CHBV\x01".to_vec();
        prefix.push(u8::try_from(path.len()).unwrap());
        prefix.extend_from_slice(path.as_bytes());
        self.records(name)
            .into_iter()
            .filter_map(|file| {
                let bytes = std::fs::read(&file).unwrap();
                let entry = ConstEntry::decode(path, bytes.get(32..)?)?;
                match &entry {
                    ConstEntry::Image(image, _) if image.bytes.starts_with(&prefix) => {
                        Some((file, entry))
                    }
                    ConstEntry::Image(..) | ConstEntry::Failed(_) => None,
                }
            })
            .collect()
    }
}

fn codes(diagnostics: &[cheby_diag::Diagnostic]) -> Vec<String> {
    diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.code.to_string())
        .collect()
}

fn read_log(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

const MAIN: &str = "//! Root.

import app::helper
import app::table

fn main() {
  assert table::scaled == helper::factor() * 10
  assert table::fixed == 7
}
";

const TABLE: &str =
    "//! Two constants: one reaches `helper`, the other nothing outside this module.

import app::helper

/// A constant that reaches `helper::factor`.
pub const scaled: Int = helper::factor() * 10

/// A constant that reaches nothing outside this module.
pub const fixed: Int = 7
";

fn helper(factor: u32) -> String {
    format!(
        "//! The function `scaled` reaches.\n\n/// The factor.\npub fn factor() -> Int {{\n  {factor}\n}}\n"
    )
}

/// D-973's third cache-hit test (D-977): an edit to a body a constant
/// reaches re-evaluates only that constant and recompiles no other
/// module. `main` compares `scaled` with `factor()` at run time, so a
/// stale cached image fails its assert; `fixed` hits, and the only
/// `code.run` miss is the edited module's (module code reads constants
/// through slots, D-257). A second warm run evaluates nothing.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn cache_hit_const_body_edit_reevaluates_only_its_constant() {
    let mut f = Fixture::new(
        "body_edit",
        "",
        &[
            ("main.cheby", MAIN),
            ("table.cheby", TABLE),
            ("helper.cheby", &helper(3)),
        ],
    );
    let cold = f.run("c");
    assert_eq!(cold.status, Some(0), "{}", cold.rendered);
    assert_eq!(
        cold.lines("const"),
        [
            "const app::table::fixed miss",
            "const app::table::scaled miss"
        ]
    );
    f.write("helper.cheby", &helper(4));
    let warm = f.run("c");
    assert_eq!(warm.status, Some(0), "{}", warm.rendered);
    assert_eq!(
        warm.lines("const"),
        [
            "const app::table::fixed hit",
            "const app::table::scaled miss"
        ]
    );
    assert_eq!(warm.misses("code.run"), ["app::helper"]);
    let again = f.run("c");
    assert_eq!(again.status, Some(0), "{}", again.rendered);
    assert_eq!(
        again.lines("const"),
        [
            "const app::table::fixed hit",
            "const app::table::scaled hit"
        ]
    );
    // No evaluation module: no `plain` code is looked up at all.
    assert_eq!(again.lines("code.plain"), Vec::<String>::new());
    assert_eq!(again.misses("code.run"), Vec::<String>::new());
}

/// A package whose one constant holds a list of `n` pairs, under a
/// 256 KiB memory limit.
fn near_limit(n: u32) -> String {
    format!(
        "//! A constant near the memory limit.

import std::list

const held: Int = list::length(build({n}, []))

fn build(n: Int, acc: List<(Int, Int)>) -> List<(Int, Int)> {{
  case n {{
    0 => acc
    _ => build(n - 1, [(n, n), ..acc])
  }}
}}

fn main() {{
  assert held == {n}
}}
"
    )
}

const NEAR_LIMIT: &str = "[const-eval]\nmemory-limit = \"256KiB\"\n";

/// Whether a cacheless `cheby check` of [`near_limit`]`(n)` passes.
fn passes(f: &Fixture, n: u32) -> bool {
    f.write("main.cheby", &near_limit(n));
    let out = crate::check(f.dir()).unwrap();
    let codes = codes(&out.diagnostics);
    assert!(codes.is_empty() || codes == ["E0076"], "{}", out.render());
    codes.is_empty()
}

/// The verdict of a command: its error codes, and for a run that
/// compiled, its exit status.
fn verdict(ran: &Ran) -> (Vec<String>, bool) {
    (ran.codes.clone(), ran.status.is_none_or(|s| s == 0))
}

/// D-973's seventh cache-hit test (D-978): a constant whose peak is just
/// under the memory limit, and one just over it, get one verdict with
/// `cheby check` cold then `cheby run` warm, and in the reverse order; the
/// warm command's verdict is the cached one (`const app::held hit`), and
/// the cold verdicts of both orders agree (both evaluate on `plain` code).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn cache_hit_const_memory_limit_one_verdict_either_order() {
    let mut f = Fixture::new("memory", NEAR_LIMIT, &[("main.cheby", &near_limit(1))]);
    // The largest `n` that passes, to a step of 16 pairs.
    let (mut low, mut high) = (16_u32, 16_384_u32);
    assert!(passes(&f, low));
    assert!(!passes(&f, high));
    while high - low > 16 {
        let mid = low + (high - low) / 2;
        if passes(&f, mid) {
            low = mid;
        } else {
            high = mid;
        }
    }
    for (n, expect_pass) in [(low, true), (high, false)] {
        f.write("main.cheby", &near_limit(n));
        let store = format!("check-first-{n}");
        let check_cold = f.check(&store);
        let run_warm = f.run(&store);
        assert_eq!(run_warm.lines("const"), ["const app::held hit"], "n = {n}");
        let store = format!("run-first-{n}");
        let run_cold = f.run(&store);
        let check_warm = f.check(&store);
        assert_eq!(
            check_warm.lines("const"),
            ["const app::held hit"],
            "n = {n}"
        );
        assert_eq!(
            check_cold.lines("const"),
            ["const app::held miss"],
            "n = {n}"
        );
        assert_eq!(run_cold.lines("const"), ["const app::held miss"], "n = {n}");
        let expected = if expect_pass {
            (Vec::new(), true)
        } else {
            (vec!["E0076".to_owned()], true)
        };
        for (what, ran) in [
            ("check cold", &check_cold),
            ("run warm", &run_warm),
            ("run cold", &run_cold),
            ("check warm", &check_warm),
        ] {
            assert_eq!(verdict(ran), expected, "{what}, n = {n}: {}", ran.rendered);
        }
        assert_eq!(check_cold.rendered, check_warm.rendered, "n = {n}");
        assert_eq!(run_cold.rendered, run_warm.rendered, "n = {n}");
    }
}

/// A constant that panics is cached with its diagnostics (D-977) and
/// replayed: a warm `cheby check` and a warm `cheby run` print what the
/// cold ones do, and evaluate nothing.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn const_cache_failure_is_replayed() {
    const BROKEN: &str = "//! A constant that panics.

const broken: Int = 1 / zero()

fn zero() -> Int {
  0
}

fn main() {
  assert broken == 0
}
";
    let mut f = Fixture::new("broken", "", &[("main.cheby", BROKEN)]);
    let cold = f.check("c");
    assert_eq!(cold.codes, ["E0075"], "{}", cold.rendered);
    assert_eq!(cold.lines("const"), ["const app::broken miss"]);
    let warm = f.check("c");
    assert_eq!(warm.lines("const"), ["const app::broken hit"]);
    assert_eq!(warm.rendered, cold.rendered);
    let run = f.run("c");
    assert_eq!(run.lines("const"), ["const app::broken hit"]);
    assert_eq!(run.codes, ["E0075"], "{}", run.rendered);
    assert_eq!(run.lines("code.plain"), Vec::<String>::new());
    assert_eq!(f.entries("c", "app::broken").len(), 0);
}

/// D-964: a constant that runs out of time is never cached: the machine's
/// speed is outside the key, so the next command evaluates it again.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn const_cache_time_limit_failure_is_not_stored() {
    const ENDLESS: &str = "//! An endless constant.

const forever: Int = spin(0)

fn spin(n: Int) -> Int {
  spin(n + 1)
}

fn main() {
  assert forever == 0
}
";
    let mut f = Fixture::new(
        "endless",
        "[const-eval]\ntime-limit = \"50ms\"\n",
        &[("main.cheby", ENDLESS)],
    );
    for _ in 0..2 {
        let ran = f.check("c");
        assert_eq!(ran.codes, ["E0076"], "{}", ran.rendered);
        assert_eq!(ran.lines("const"), ["const app::forever miss"]);
    }
}

/// D-1188's tag 3: `pair` shares `base`'s list, so its image references
/// `base`'s, and its entry lists `base`'s image with the hash of its
/// bytes (D-1261). When `base`'s entry is gone,
/// `pair` hits but is evaluated again with `base`, so the two images
/// installed together come from one evaluation; the run's asserts pass.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn const_cache_shared_image_is_evaluated_with_what_it_references() {
    const SHARED: &str = "//! Two constants that share a list.

import std::list

const base: List<Int> = list::range(0, 5)

const pair: (List<Int>, Int) = (base, list::length(base))

fn main() {
  let (items, n) = pair
  assert items == base
  assert n == 5
}
";
    let mut f = Fixture::new("shared", "", &[("main.cheby", SHARED)]);
    let cold = f.run("c");
    assert_eq!(cold.status, Some(0), "{}", cold.rendered);
    let pair = f.entries("c", "app::pair");
    let base = f.entries("c", "app::base");
    let [(_, ConstEntry::Image(_, refs))] = pair.as_slice() else {
        panic!("no image of `pair`: {}", pair.len());
    };
    let [(base_file, ConstEntry::Image(base_image, _))] = base.as_slice() else {
        panic!("no image of `base`: {}", base.len());
    };
    assert_eq!(
        refs.as_slice(),
        [("app::base".into(), Hash128::of(&base_image.bytes))]
    );
    let warm = f.run("c");
    assert_eq!(warm.status, Some(0), "{}", warm.rendered);
    assert_eq!(
        warm.lines("const"),
        ["const app::base hit", "const app::pair hit"]
    );
    std::fs::remove_file(base_file).unwrap();
    let again = f.run("c");
    assert_eq!(again.status, Some(0), "{}", again.rendered);
    assert_eq!(
        again.lines("const"),
        ["const app::base miss", "const app::pair hit"]
    );
    // `pair` was evaluated again beside `base`: its entry was written again
    // with the same bytes.
    assert_eq!(f.entries("c", "app::pair").len(), 1);
    assert_eq!(f.entries("c", "app::base").len(), 1);
}

const KEYS_MAIN: &str = "//! Root: constants over several modules.

import app::shapes
import app::sizes
import std::list

/// Reaches `sizes` through a function and `shapes` through a constructor.
const area: Int = sizes::double(shapes::area(shapes::Square { side: 3 }))

/// Reads `area`, and std only.
const areas: List<Int> = list::map(list::range(0, 3), fn(n) { n * area })

/// Reaches nothing outside this module.
const plain: Int = 4

fn main() {
  assert area == 18
  assert list::length(areas) == 3
  assert plain == 4
  assert sizes::unit == 1
}
";

const KEYS_SHAPES: &str = "//! Shapes.

/// A shape.
pub type Shape { Square { side: Int }, Circle { radius: Int } }

/// Its area, roughly.
pub fn area(shape: Shape) -> Int {
  case shape {
    Square { side } => side * side
    Circle { radius } => 3 * radius * radius
  }
}
";

const KEYS_SIZES: &str = "//! Sizes.

/// One unit.
pub const unit: Int = 1

/// Twice `n`.
pub fn double(n: Int) -> Int {
  n * 2 * unit
}
";

/// The package constants' keys after the check of `front`: the reach of
/// `cheby check`'s roots, every constant of the package, over reference
/// summaries (D-967), as its build walks it, then `const_infos` and
/// [`keys_after_check`]; sorted by path.
fn keys_after(
    front: &crate::frontend::Frontend,
    sources: &cheby_diag::SourceMap,
    settings: &Settings,
    limits: &ConstLimits,
) -> Vec<(String, super::Key)> {
    let mut starts = Vec::new();
    for (id, info) in front.modules.iter() {
        if info.package == cheby_iface::PackageId::LOCAL
            && let Some(checked) = front.checked(id)
        {
            starts.extend(
                checked
                    .thir
                    .consts
                    .iter()
                    .map(|c| crate::load_set::Start::Const(c.def)),
            );
        }
    }
    let reach = crate::load_set::Summaries::new(front)
        .unwrap()
        .reach(&starts)
        .unwrap();
    let infos = crate::backend::const_infos(front, sources, &reach).unwrap();
    let keys = keys_after_check(front, sources, &reach, &infos, BUILD, settings, limits).unwrap();
    let mut after: Vec<(String, super::Key)> = infos
        .iter()
        .map(|info| info.path.to_string())
        .zip(keys)
        .filter(|(path, _)| path.starts_with("app"))
        .collect();
    after.sort_by(|a, b| a.0.cmp(&b.0));
    after
}

/// D-1250 with D-1134: the keys before the check, from the package
/// index's vouched front records alone (their reference summaries and
/// source hashes), equal the keys after it, from the reach and
/// `const_infos` (D-1259).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn const_cache_keys_before_check_equal_keys_after() {
    let mut f = Fixture::new(
        "keys",
        "",
        &[
            ("main.cheby", KEYS_MAIN),
            ("shapes.cheby", KEYS_SHAPES),
            ("sizes.cheby", KEYS_SIZES),
        ],
    );
    let ran = f.check("c");
    assert_eq!(ran.codes, Vec::<String>::new(), "{}", ran.rendered);
    // The front end of a cold check on a fresh store, and its keys after
    // the check.
    let (cache, _) = f.open("fresh");
    let mut out = crate::CheckOutput::default();
    let (front, _) = crate::front_end_for(
        f.dir(),
        crate::frontend::Mode::Check,
        None,
        &mut out,
        Some(&cache),
    )
    .unwrap()
    .unwrap();
    let limits = ConstLimits::of(&front);
    let settings = super::consts::settings(cheby_mir::MirOptions::default());
    let after = keys_after(&front, &out.sources, &settings, &limits);
    // The keys before the check, from the first check's index.
    let store = Store::open(&f.cache_dir("c"), BUILD).unwrap();
    let index = PackageIndex::decode(
        &store
            .get_hint(&PackageIndex::key(f.dir(), CommandFamily::Check, BUILD))
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let vouched: Vec<Option<&super::index::IndexEntry>> = front
        .modules
        .iter()
        .map(|(_, info)| index.get(&info.path.to_string()))
        .collect();
    let mut before: Vec<(String, super::Key)> =
        keys_from_records(&front.modules, &vouched, BUILD, &settings, &limits)
            .into_iter()
            .map(|(path, key)| (path, key.unwrap()))
            .collect();
    before.sort_by(|a, b| a.0.cmp(&b.0));
    let paths: Vec<&str> = before.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        ["app::area", "app::areas", "app::plain", "app::sizes::unit"]
    );
    assert_eq!(before, after);
    // `area`'s set holds `shapes` and `sizes` beside its own module;
    // `plain`'s only its own (std left out, module docs).
    let source = |path: &str| index.get(path).unwrap().source;
    let alone = const_key(
        BUILD,
        &settings,
        &limits,
        "app",
        "app",
        "area",
        &[("app", source("app"))],
    );
    let whole = const_key(
        BUILD,
        &settings,
        &limits,
        "app",
        "app",
        "area",
        &[
            ("app", source("app")),
            ("app::shapes", source("app::shapes")),
            ("app::sizes", source("app::sizes")),
        ],
    );
    let plain = const_key(
        BUILD,
        &settings,
        &limits,
        "app",
        "app",
        "plain",
        &[("app", source("app"))],
    );
    assert_eq!(
        before.iter().map(|(_, k)| *k).take(3).collect::<Vec<_>>(),
        [whole, before[1].1, plain]
    );
    assert_ne!(whole, alone);
    drop(cache);
}

/// A std constant whose entry misses while std's front records hit (the
/// package's limits changed, so every constant's key did): it is evaluated
/// again, and the run prints what the first one did.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn const_cache_std_constant_misses_beside_std_hits() {
    const PI: &str = "//! Reads a std constant.

import std::float

const twice: Float = float::pi * 2.0

fn main() {
  assert twice > 6.0
}
";
    let mut f = Fixture::new("std_pi", "", &[("main.cheby", PI)]);
    let cold = f.run("c");
    assert_eq!(cold.status, Some(0), "{}", cold.rendered);
    assert!(
        cold.lines("const")
            .contains(&"const std::float::pi miss".to_owned())
    );
    std::fs::write(
        f.dir().join("cheby.toml"),
        "name = \"app\"\nversion = \"0.1.0\"\n[const-eval]\nmemory-limit = \"512MiB\"\n",
    )
    .unwrap();
    let warm = f.run("c");
    assert_eq!(warm.status, Some(0), "{}", warm.rendered);
    assert_eq!(
        warm.lines("const"),
        ["const app::twice miss", "const std::float::pi miss"]
    );
}

/// A module's constant that only its tests read (032's `puzzle::sample`):
/// `cheby run` never looks it up, so the walk from `main` over the vouched
/// summaries does not count it, while it counts what `main` reaches,
/// through functions and other constants' initializers.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn const_cache_reached_from_main_leaves_test_only_constants_out() {
    const ROOT: &str = "//! Root.

import app::table

fn main() {
  assert table::doubled() == 4
}
";
    const TABLE: &str = "//! Constants.

const base: Int = 2

const twice: Int = base * 2

/// Twice `base`.
pub fn doubled() -> Int {
  twice
}

/// Read only by the test below.
priv const sample: String = \"1 2\"

test \"sample\" {
  assert sample == \"1 2\"
}
";
    let mut f = Fixture::new(
        "reached",
        "",
        &[("main.cheby", ROOT), ("table.cheby", TABLE)],
    );
    let ran = f.run("c");
    assert_eq!(ran.status, Some(0), "{}", ran.rendered);
    assert_eq!(
        ran.lines("const"),
        [
            "const app::table::base miss",
            "const app::table::twice miss"
        ]
    );
    let store = Store::open(&f.cache_dir("c"), BUILD).unwrap();
    let index = PackageIndex::decode(
        &store
            .get_hint(&PackageIndex::key(f.dir(), CommandFamily::Run, BUILD))
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let (cache, _) = f.open("fresh");
    let mut out = crate::CheckOutput::default();
    let (front, _) = crate::front_end_for(
        f.dir(),
        crate::frontend::Mode::Check,
        None,
        &mut out,
        Some(&cache),
    )
    .unwrap()
    .unwrap();
    let vouched: Vec<Option<&super::index::IndexEntry>> = front
        .modules
        .iter()
        .map(|(_, info)| index.get(&info.path.to_string()))
        .collect();
    let reached = super::consts::reached_from_main(&front.modules, &vouched, "app").unwrap();
    let reached: Vec<&str> = reached.iter().map(String::as_str).collect();
    assert_eq!(reached, ["app::table::base", "app::table::twice"]);
    drop(cache);
}
