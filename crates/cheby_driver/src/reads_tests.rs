//! Hidden reads recorded in every phase (D-955, `cache.md` §3.3): the
//! front end's set per module and the lowering sets per unit.

use std::path::{Path, PathBuf};

use cheby_iface::{ModuleId, PackageId};
use cheby_mir::MirOptions;

use super::*;
use crate::backend::{Roots, TestRoot, UnitReads};
use crate::frontend::Frontend;

const MANIFEST: (&str, &[u8]) = ("cheby.toml", b"name = \"app\"\nversion = \"0.1.0\"\n");

/// A package in a fresh temporary directory with these files.
fn package(name: &str, files: &[(&str, &[u8])]) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("cheby_driver_reads_{}_{name}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    for (path, contents) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    dir
}

/// `c` defines a type and how it shows, `b` imports `c` and names the
/// type in its signatures and in the field of its own type `Wrapper`.
const C: (&str, &[u8]) = (
    "src/c.cheby",
    b"//! C.\n\npub type Thing { size: Int }\n\n\
      pub fn show(thing: Thing) -> String {\n  \"thing {thing.size}\"\n}\n",
);
const B: (&str, &[u8]) = (
    "src/b.cheby",
    b"//! B.\n\nimport app::c\n\npub type Wrapper { inner: c::Thing }\n\n\
      pub fn make() -> c::Thing {\n  c::Thing { size: 1 }\n}\n\n\
      pub fn wrap() -> Wrapper {\n  Wrapper { inner: make() }\n}\n",
);

/// The front end of the package at `dir` (no errors), with its sources.
fn front_of(dir: &Path) -> (Frontend, CheckOutput) {
    let mut out = CheckOutput::default();
    let (front, _) = front_end(dir, frontend::Mode::Build, &mut out)
        .unwrap()
        .unwrap();
    assert!(!out.has_errors(), "{}", out.render());
    (front, out)
}

fn module(front: &Frontend, path: &str) -> ModuleId {
    let mut segments = path.split("::");
    let first = segments.next().unwrap();
    let path = segments.fold(ModulePath::root(first), |p, s| p.child(s));
    front.modules.find(&path).unwrap()
}

fn paths(front: &Frontend, ids: &[ModuleId]) -> Vec<String> {
    ids.iter()
        .map(|&m| front.modules.get(m).unwrap().path.to_string())
        .collect()
}

/// Every module's front-end reads, by path.
fn front_reads(front: &Frontend) -> Vec<(String, Vec<String>)> {
    front
        .order
        .iter()
        .map(|&m| {
            let checked = front.checked(m).unwrap();
            (
                front.modules.get(m).unwrap().path.to_string(),
                paths(front, &checked.front_reads),
            )
        })
        .collect()
}

/// The lowering reads of a build of `main` and of every local test.
fn lowering_reads(front: &Frontend, out: &CheckOutput) -> Vec<(String, UnitReads)> {
    let root = front
        .order
        .iter()
        .copied()
        .find(|&m| {
            let info = front.modules.get(m).unwrap();
            info.package == PackageId::LOCAL && info.path.is_root()
        })
        .unwrap();
    let mut all = Vec::new();
    let main_reads = front.main_of(root).map_or_else(Default::default, |main| {
        backend::lower(
            front,
            &out.sources,
            &Roots::Main(main),
            MirOptions::default(),
        )
        .unwrap()
        .reads
    });
    let tests: Vec<TestRoot<'_>> = front
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
        .collect();
    let test_reads = backend::lower(
        front,
        &out.sources,
        &Roots::Tests(tests),
        MirOptions::default(),
    )
    .unwrap()
    .reads;
    for (reads, kind) in [(main_reads, "main"), (test_reads, "tests")] {
        for (m, unit) in reads {
            let path = front.modules.get(m).unwrap().path.to_string();
            all.push((format!("{kind}:{path}"), unit));
        }
    }
    all
}

/// No recorded set holds a direct import, a std module or the builtin
/// module, and the lowering sets hold nothing the front end recorded.
fn assert_never_imports_or_std(front: &Frontend, out: &CheckOutput) {
    let lowered = lowering_reads(front, out);
    for &m in &front.order {
        let checked = front.checked(m).unwrap();
        let imports: Vec<ModuleId> = front
            .import_targets
            .get(usize::try_from(m.as_u32()).unwrap())
            .unwrap()
            .iter()
            .filter_map(|t| t.module())
            .collect();
        let path = front.modules.get(m).unwrap().path.to_string();
        let units: Vec<&UnitReads> = lowered
            .iter()
            .filter(|(name, _)| name.split_once(':').is_some_and(|(_, p)| p == path))
            .map(|(_, unit)| unit)
            .collect();
        let mut sets: Vec<&[ModuleId]> = vec![&checked.front_reads];
        for unit in &units {
            sets.extend(unit.code.as_deref());
            sets.extend(unit.tests.as_deref());
            for set in [unit.code.as_deref(), unit.tests.as_deref()]
                .into_iter()
                .flatten()
            {
                assert!(
                    set.iter().all(|r| !checked.front_reads.contains(r)),
                    "{path}: a lowering read repeats a front-end read"
                );
            }
        }
        for set in sets {
            for &read in set {
                assert_ne!(read, ModuleId::BUILTIN, "{path}");
                assert_ne!(read, m, "{path}");
                assert!(!imports.contains(&read), "{path} records its import");
                assert_ne!(
                    front.modules.get(read).unwrap().package,
                    PackageId::STD,
                    "{path} records a std module"
                );
            }
        }
    }
}

/// `a` (the root) imports `b`, `b` imports `c`, and `a` shows the value a
/// function of `b` returns, whose type is `c`'s: checking `a` reads `c`'s
/// interface to find how the type shows (D-260), and the front end records
/// `c` for `a`. `b` imports `c`, so `b` records nothing; std is never
/// recorded.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn hidden_reads_front_end_records_a_type_of_an_unimported_module() {
    let dir = package(
        "front",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                b"//! Root.\n\nimport app::b\nimport std::io\n\n\
                  fn text() -> String {\n  let thing = b::make()\n  \"{thing}\"\n}\n\n\
                  fn main() {\n  io::println(text())\n}\n",
            ),
            B,
            C,
        ],
    );
    let (front, out) = front_of(&dir);
    let reads = front_reads(&front);
    let of = |path: &str| {
        reads
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, r)| r.clone())
            .unwrap()
    };
    assert_eq!(of("app"), ["app::c"], "{reads:?}");
    assert_eq!(of("app::b"), Vec::<String>::new());
    assert_eq!(of("app::c"), Vec::<String>::new());
    assert!(
        reads
            .iter()
            .filter(|(p, _)| p.starts_with("std::"))
            .all(|(_, r)| r.is_empty()),
        "{reads:?}"
    );
    assert_never_imports_or_std(&front, &out);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `a` only passes `b::Wrapper` values along, so checking it never reads
/// `c`; borrow inference asks whether the parameter of the package-visible
/// `keep` has a handle-free type (D-715) and walks `Wrapper`'s field into
/// `c::Thing`, so the code
/// unit of `a` records `c` and the front end does not (D-955: a case where
/// only lowering reads a module exists, so no debug assertion claims
/// otherwise).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn hidden_reads_lowering_records_what_only_it_reads() {
    let dir = package(
        "lowering",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                b"//! Root.\n\nimport app::b\n\n\
                  fn keep(w: b::Wrapper) -> b::Wrapper {\n  w\n}\n\n\
                  fn main() {\n  let _ = keep(b::wrap())\n  Nil\n}\n\n\
                  test \"keeps a wrapper\" {\n  let _ = keep(b::wrap())\n  assert True\n}\n",
            ),
            B,
            C,
        ],
    );
    let (front, out) = front_of(&dir);
    let root = module(&front, "app");
    let c = module(&front, "app::c");
    assert_eq!(
        front.checked(root).unwrap().front_reads,
        Vec::<ModuleId>::new(),
        "{:?}",
        front_reads(&front)
    );
    let lowered = lowering_reads(&front, &out);
    let main_unit = lowered
        .iter()
        .find(|(name, _)| name == "main:app")
        .map(|(_, unit)| unit.clone())
        .unwrap();
    assert_eq!(main_unit.code, Some(vec![c]), "{lowered:?}");
    assert_eq!(main_unit.tests, None);
    // The tests build lowers the root's code (the test calls `keep`) and
    // its tests unit, each with its own set.
    let tests_unit = lowered
        .iter()
        .find(|(name, _)| name == "tests:app")
        .map(|(_, unit)| unit.clone())
        .unwrap();
    assert_eq!(tests_unit.code, Some(vec![c]), "{lowered:?}");
    assert!(tests_unit.tests.is_some(), "{lowered:?}");
    assert_never_imports_or_std(&front, &out);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A direct import and std modules are never recorded, in any phase, for
/// any example that builds.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn hidden_reads_never_record_imports_or_std() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
    let mut examples: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("cheby.toml").is_file())
        .filter(|p| !p.ends_with("030_shop_service") && !p.ends_with("031_web_scraper"))
        .collect();
    examples.sort();
    assert!(examples.len() >= 30);
    for example in examples {
        let (front, out) = front_of(&example);
        assert_never_imports_or_std(&front, &out);
    }
}

/// Every set is the same with one worker thread and with the default.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn hidden_reads_are_the_same_with_one_job_and_the_default() {
    let dir = package(
        "jobs",
        &[
            MANIFEST,
            (
                "src/main.cheby",
                b"//! Root.\n\nimport app::b\nimport app::keeper\nimport std::io\n\n\
                  fn text() -> String {\n  let thing = b::make()\n  \"{thing}\"\n}\n\n\
                  fn main() {\n  let _ = keeper::keep(b::wrap())\n  io::println(text())\n}\n",
            ),
            (
                "src/keeper.cheby",
                b"//! Keeps wrappers.\n\nimport app::b\n\n\
                  fn keep(w: b::Wrapper) -> b::Wrapper {\n  w\n}\n\n\
                  test \"keeps a wrapper\" {\n  let _ = keep(b::wrap())\n  assert True\n}\n",
            ),
            B,
            C,
        ],
    );
    let sets = |threads: Option<usize>| {
        let run = || {
            let (front, out) = front_of(&dir);
            (front_reads(&front), lowering_reads(&front, &out))
        };
        match threads {
            Some(threads) => rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .stack_size(256 << 20)
                .build()
                .unwrap()
                .install(run),
            None => run(),
        }
    };
    let one = sets(Some(1));
    let default = sets(None);
    assert_eq!(one, default);
    assert!(one.0.iter().any(|(_, r)| !r.is_empty()), "{one:?}");
    assert!(
        one.1
            .iter()
            .any(|(_, u)| u.code.as_ref().is_some_and(|c| !c.is_empty())),
        "{one:?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
