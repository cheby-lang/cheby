//! `cheby build` from cached records (N9: D-914, D-924, D-1250, D-1256,
//! D-1262, D-1264): a warm front end over the Build family's own package
//! index (D-962) that checks only what misses, `cheby run`'s code records
//! (its kind, keys and store entries), std's code records, and the
//! constants' entries (D-977, D-1261).
//!
//! Every test opens its own store in a fresh directory, so no test reads
//! or fills the developer's cache (D-1128), and reads its lookups through
//! `CHEBY_CACHE_LOG`'s file (D-973, D-1135, D-1263).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::aot::{BuildOptions, Objects, build_objects};
use crate::cache::{BuildId, CacheLog, Hash128, Store};

/// The build ID of every test store.
const BUILD: BuildId = BuildId([11; 16]);

/// A fresh directory under the system's temporary directory, removed on
/// drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cheby_driver_build_cache_{}_{n}_{name}",
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

/// What one command left in its log.
struct Logged {
    lines: Vec<String>,
}

impl Logged {
    /// The lines that start with `prefix` and a space, sorted as logged.
    fn of(&self, prefix: &str) -> Vec<&str> {
        self.lines
            .iter()
            .map(String::as_str)
            .filter(|line| {
                line.strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with(' '))
            })
            .collect()
    }

    /// The lines of `prefix` that end in `miss`.
    fn misses(&self, prefix: &str) -> Vec<&str> {
        self.of(prefix)
            .into_iter()
            .filter(|line| line.ends_with(" miss"))
            .collect()
    }
}

/// What a build gave: its objects and its records' hashes.
struct Build {
    objects: Vec<(String, Vec<u8>)>,
    hashes: Vec<(String, Hash128)>,
    log: Logged,
}

/// A package `app` with these files under `src/`, and a cache of its own.
struct Fixture {
    package: TempDir,
    cache: TempDir,
    commands: u32,
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
            commands: 0,
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

    /// A command's cache over a fresh store of `dir` (the fixture's own
    /// unless given), logging to a fresh file.
    fn open_in(&mut self, dir: &Path) -> (Cache, PathBuf) {
        self.commands = self.commands.checked_add(1).unwrap();
        let log = self.cache.path().join(format!("log-{}", self.commands));
        let store = Store::open(&dir.join("cache"), BUILD).unwrap();
        (
            Cache::with_store(store, CacheLog::new(Some(log.clone()))),
            log,
        )
    }

    fn logged(log: &Path) -> Logged {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        Logged {
            lines: text.lines().map(str::to_owned).collect(),
        }
    }

    /// `cheby build` of the root module through the fixture's cache.
    fn build(&mut self) -> Build {
        let dir = self.cache.path().to_path_buf();
        self.build_in(&dir)
    }

    /// `cheby build` of the root module through the cache in `dir`.
    fn build_in(&mut self, dir: &Path) -> Build {
        let (cache, log) = self.open_in(dir);
        let options = BuildOptions {
            output: Some(self.cache.path().join("app-exe")),
            ..BuildOptions::default()
        };
        let built = match build_objects(self.dir(), &options, Some(&cache)).unwrap() {
            Objects::Ready { output, built } => {
                assert!(!output.has_errors(), "{}", output.render());
                built
            }
            Objects::CompileFailed(output) => panic!("the build failed: {}", output.render()),
        };
        cache.finish(false).unwrap();
        Build {
            objects: built.objects,
            hashes: built.record_hashes,
            log: Self::logged(&log),
        }
    }

    /// `cheby run`'s build (nothing runs) through the fixture's cache.
    fn run(&mut self) -> Logged {
        let dir = self.cache.path().to_path_buf();
        self.run_in(&dir)
    }

    /// `cheby run`'s build through the cache in `dir`.
    fn run_in(&mut self, dir: &Path) -> Logged {
        let (cache, log) = self.open_in(dir);
        match prepare_run_cached(self.dir(), RunOptions::default(), &cache).unwrap() {
            Prepared::Ready(_) => {}
            Prepared::CompileFailed(out) => panic!("{}", out.render()),
        }
        cache.finish(false).unwrap();
        Self::logged(&log)
    }
}

/// The root: two other modules, one with a constant.
const MAIN: &str = "//! The root: what the two other modules give.

import app::count
import app::label
import std::io

fn main() {
  let n = count::total()
  let limit = count::limit
  io::println(label::name())
  io::println(\"total: {n}\")
  io::println(\"limit: {limit}\")
}
";

/// A module with a function and a constant.
const COUNT: &str = "//! Counting.

/// The largest count.
pub const limit: Int = steps(10, 0)

/// The total.
pub fn total() -> Int {
  steps(4, 0) + 1
}

fn steps(n: Int, acc: Int) -> Int {
  case n {
    0 => acc
    _ => steps(n - 1, acc + n)
  }
}
";

/// `COUNT` with one body edited: no interface or mask changes.
const COUNT_EDITED: &str = "//! Counting.

/// The largest count.
pub const limit: Int = steps(10, 0)

/// The total.
pub fn total() -> Int {
  steps(4, 0) + 2
}

fn steps(n: Int, acc: Int) -> Int {
  case n {
    0 => acc
    _ => steps(n - 1, acc + n)
  }
}
";

/// A module with a string.
const LABEL: &str = "//! A label.

/// The program's name.
pub fn name() -> String {
  \"counter\"
}
";

/// The three-module package.
fn three(name: &str) -> Fixture {
    Fixture::new(
        name,
        &[
            ("main.cheby", MAIN),
            ("count.cheby", COUNT),
            ("label.cheby", LABEL),
        ],
    )
}

/// The package modules of [`three`].
const MODULES: [&str; 3] = ["app", "app::count", "app::label"];

/// N9's done-when: after a one-body edit in a three-module package, a
/// second `cheby build` parses only the edited module, misses only its
/// code record, and neither parses std nor misses a std record (D-1250,
/// D-1262, D-1264); with no edit, a build parses nothing and misses
/// nothing.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_cache_one_body_edit_parses_and_compiles_one_module() {
    let mut f = three("one_body");
    let cold = f.build();
    assert_eq!(
        cold.log.misses("code.run").len(),
        MODULES.len(),
        "{:?}",
        cold.log.lines
    );
    let again = f.build();
    assert_eq!(
        again.log.of("parse"),
        Vec::<&str>::new(),
        "{:?}",
        again.log.lines
    );
    assert_eq!(again.log.misses("code.run"), Vec::<&str>::new());
    assert_eq!(again.log.of("parse.std"), Vec::<&str>::new());
    assert_eq!(again.log.misses("code.std"), Vec::<&str>::new());
    assert!(
        again.objects == cold.objects,
        "a warm build's objects differ"
    );
    f.write("count.cheby", COUNT_EDITED);
    let edited = f.build();
    assert_eq!(
        edited.log.of("parse"),
        ["parse app::count"],
        "{:?}",
        edited.log.lines
    );
    assert_eq!(edited.log.misses("code.run"), ["code.run app::count miss"]);
    assert_eq!(edited.log.of("code.run").len(), MODULES.len());
    assert_eq!(edited.log.of("parse.std"), Vec::<&str>::new());
    assert_eq!(edited.log.misses("code.std"), Vec::<&str>::new());
    assert!(
        !edited.log.of("code.std").is_empty(),
        "{:?}",
        edited.log.lines
    );
}

/// `cheby build` after `cheby run` misses no code record, std's
/// included, and `cheby run` after `cheby build` misses none either: both
/// read and write one set of entries (D-914, D-924).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_cache_shares_runs_records() {
    let mut f = three("shares");
    let run = f.run();
    assert_eq!(
        run.misses("code.run").len(),
        MODULES.len(),
        "{:?}",
        run.lines
    );
    let built = f.build();
    assert_eq!(
        built.log.misses("code.run"),
        Vec::<&str>::new(),
        "{:?}",
        built.log.lines
    );
    assert_eq!(built.log.of("code.run").len(), MODULES.len());
    assert_eq!(built.log.misses("code.std"), Vec::<&str>::new());
    assert_eq!(built.log.misses("const"), Vec::<&str>::new());
    let mut g = three("shares_back");
    let _ = g.build();
    let run = g.run();
    assert_eq!(
        run.misses("code.run"),
        Vec::<&str>::new(),
        "{:?}",
        run.lines
    );
    assert_eq!(run.of("code.run").len(), MODULES.len());
    assert_eq!(run.misses("code.std"), Vec::<&str>::new());
    assert_eq!(run.misses("const"), Vec::<&str>::new());
}

/// A warm build of a package with constants evaluates nothing: every
/// constant comes from its entry (`const … hit`, D-977), so no evaluation
/// module is built, and the program object, whose table holds the
/// entries' images (D-979, D-1261), is the cold build's byte for byte.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_cache_constants_from_entries() {
    let mut f = three("consts");
    let cold = f.build();
    assert_eq!(
        cold.log.misses("const"),
        ["const app::count::limit miss"],
        "{:?}",
        cold.log.lines
    );
    let warm = f.build();
    assert_eq!(
        warm.log.of("const"),
        ["const app::count::limit hit"],
        "{:?}",
        warm.log.lines
    );
    assert_eq!(warm.log.misses("code.plain"), Vec::<&str>::new());
    assert_eq!(warm.log.of("code.plain"), Vec::<&str>::new());
    let program = |b: &Build| b.objects.last().cloned().unwrap();
    assert!(
        program(&warm) == program(&cold),
        "the program objects differ"
    );
}

/// N9's same-code test (D-914): the code of an executable is the code
/// `cheby run` loads. A cold `cheby run` compiles and stores every unit's
/// record (the JIT's), and a `cheby build` on that cache replays them all
/// (every lookup a hit), so its record hashes (`seal::record_hash`) are
/// those of the records `cheby run` loads; a cold `cheby build` on a fresh
/// cache compiles its own and finds the same hashes, std's included; and
/// the objects of the three builds are the same.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_cache_executable_code_is_runs_records() {
    let mut f = three("same_code");
    let run = f.run();
    assert!(!run.of("code.std").is_empty(), "{:?}", run.lines);
    let after_run = f.build();
    assert_eq!(after_run.log.misses("code.run"), Vec::<&str>::new());
    assert_eq!(after_run.log.misses("code.std"), Vec::<&str>::new());
    let fresh = TempDir::new("same_code_fresh");
    let cold = f.build_in(fresh.path());
    assert_eq!(cold.log.misses("code.run").len(), MODULES.len());
    let by_path = |b: &Build| -> BTreeMap<String, Hash128> { b.hashes.iter().cloned().collect() };
    assert_eq!(after_run.hashes.len(), cold.hashes.len());
    assert!(
        after_run
            .hashes
            .iter()
            .any(|(path, _)| path.starts_with("std::"))
    );
    for module in MODULES {
        assert!(by_path(&cold).contains_key(module), "{module}");
    }
    assert_eq!(by_path(&after_run), by_path(&cold));
    assert_eq!(after_run.hashes, cold.hashes, "build order");
    // The records a later `cheby run` loads: the ones `cheby build` read.
    let warm_run = f.run();
    assert_eq!(warm_run.misses("code.run"), Vec::<&str>::new());
    assert!(after_run.objects == cold.objects, "the objects differ");
}

/// A build with no cache compiles the records a cache would hold: the
/// same objects as a cold build through a cache.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn build_cache_without_a_cache_builds_the_same_objects() {
    let mut f = three("no_cache");
    let cached = f.build();
    let options = BuildOptions {
        output: Some(f.cache.path().join("app-exe")),
        ..BuildOptions::default()
    };
    let Objects::Ready { built, .. } = build_objects(f.dir(), &options, None).unwrap() else {
        panic!("the build failed");
    };
    assert_eq!(built.record_hashes, cached.hashes);
    assert!(built.objects == cached.objects, "the objects differ");
}
