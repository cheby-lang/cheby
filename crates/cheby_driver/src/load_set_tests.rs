//! Tests `load_set_*` (D-967, D-922): the walk over reference summaries
//! reaches what the walk over THIR reaches, on every example and on every
//! std module's tests, and the load set holds every module a loaded
//! module's items mention.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use cheby_iface::{ModuleId, PackageId};
use cheby_mir::Program;
use cheby_mir::reach::{Reach, Root, reachable_with_hooks};
use cheby_types::thir;

use crate::frontend::{self, Frontend};
use crate::load_set::{Start, Summaries};
use crate::{CheckOutput, backend};

/// The example packages under `docs/examples`, sorted.
fn example_dirs() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|dir| dir.join("cheby.toml").is_file())
        .collect();
    dirs.sort();
    dirs
}

/// The THIR walk of `roots` over every module `front` checked, with the
/// hooks a build reaches (D-585).
fn thir_reach(front: &Frontend, roots: &[Root<'_>]) -> Reach {
    let mut program = Program::new();
    for &module in &front.order {
        program.insert(module, &front.checked(module).unwrap().thir);
    }
    let hooks = |module: ModuleId| backend::hooks_of(front, module);
    reachable_with_hooks(&program, roots, &hooks).unwrap()
}

/// Both walks from the same roots give the same [`Reach`], and so the
/// same constants in the same order and the same module sets.
fn assert_same(front: &Frontend, what: &str, roots: &[Root<'_>], starts: &[Start<'_>]) -> Reach {
    let thir = thir_reach(front, roots);
    let summaries = Summaries::new(front).unwrap();
    let walked = summaries.reach(starts).unwrap();
    assert_eq!(walked, thir, "{what}");
    assert_eq!(walked.constant_order(), thir.constant_order(), "{what}");
    assert_eq!(walked.const_modules(), thir.const_modules(), "{what}");
    walked
}

/// The local package's modules of `front`, in build order.
fn local(front: &Frontend) -> Vec<ModuleId> {
    front
        .order
        .iter()
        .copied()
        .filter(|&m| front.modules.get(m).unwrap().package == PackageId::LOCAL)
        .collect()
}

/// On every example: from `main`, from every test on native, and from
/// every constant (`cheby check`'s roots), the summaries reach what THIR
/// reaches; and the load set holds the reached modules, in build order,
/// closed under what its modules' items mention.
#[test]
fn load_set_equals_the_thir_walk_on_every_example() {
    let mut checked = 0;
    for dir in example_dirs() {
        let what = dir.display().to_string();
        let mut out = CheckOutput::default();
        let front = crate::front_end(&dir, frontend::Mode::Build, &mut out).unwrap();
        let Some((front, package)) = front.filter(|_| !out.has_errors()) else {
            // 030 needs its git dependency (M5), 031 stops at E0038
            // (`std::http`).
            continue;
        };
        let main = front
            .local_module(&cheby_package::ModulePath::root(&package))
            .and_then(|m| front.main_of(m));
        if let Some(main) = main {
            let reach = assert_same(
                &front,
                &format!("{what}: main"),
                &[Root::Fn(main)],
                &[Start::Main(main)],
            );
            let summaries = Summaries::new(&front).unwrap();
            let loaded = summaries.load_set(&reach, &BTreeSet::new()).unwrap();
            assert_load_set(&front, &summaries, &reach, &loaded, &what);
        }
        let tests: Vec<&thir::TestDef> = local(&front)
            .into_iter()
            .flat_map(|m| &front.checked(m).unwrap().thir.tests)
            .filter(|t| crate::test::on_native(&front, t))
            .collect();
        if !tests.is_empty() {
            let roots: Vec<Root<'_>> = tests.iter().map(|t| Root::Test(t)).collect();
            let starts: Vec<Start<'_>> = tests.iter().map(|t| Start::Test(t)).collect();
            assert_same(&front, &format!("{what}: tests"), &roots, &starts);
        }
        let consts: Vec<&thir::ConstDef> = local(&front)
            .into_iter()
            .flat_map(|m| &front.checked(m).unwrap().thir.consts)
            .collect();
        let roots: Vec<Root<'_>> = consts.iter().map(|c| Root::Const(c)).collect();
        let starts: Vec<Start<'_>> = consts.iter().map(|c| Start::Const(c.def)).collect();
        assert_same(&front, &format!("{what}: constants"), &roots, &starts);
        checked += 1;
    }
    // 32 examples, 030 and 031 aside.
    assert!(checked >= 30, "{checked}");
}

/// The load set is in build order, holds every module the reach needs,
/// and every module a loaded module's items mention (whole modules,
/// D-966).
fn assert_load_set(
    front: &Frontend,
    summaries: &Summaries<'_>,
    reach: &Reach,
    loaded: &[ModuleId],
    what: &str,
) {
    let position = |m: ModuleId| front.order.iter().position(|&o| o == m).unwrap();
    assert!(
        loaded.windows(2).all(|w| position(w[0]) < position(w[1])),
        "{what}"
    );
    let set: BTreeSet<ModuleId> = loaded.iter().copied().collect();
    for module in reach.modules().filter(|&m| m != ModuleId::BUILTIN) {
        assert!(set.contains(&module), "{what}: {module:?}");
    }
    for &module in loaded {
        for target in summaries.mentioned_modules(module).unwrap() {
            assert!(
                target == ModuleId::BUILTIN || set.contains(&target),
                "{what}: {module:?} mentions {target:?}"
            );
        }
    }
}

/// On every std module's tests, as `cargo xtask test-std` builds them one
/// module at a time (D-542), the summaries reach what THIR reaches.
#[test]
fn load_set_equals_the_thir_walk_on_std_tests() {
    let mut sources = cheby_diag::SourceMap::new();
    let mut diagnostics = Vec::new();
    let package = crate::test::std_test_package(&mut sources, &mut diagnostics).unwrap();
    let front = frontend::run(
        package,
        frontend::Mode::StdTests,
        &mut sources,
        &mut diagnostics,
    )
    .unwrap();
    let mut modules = 0;
    for &module in &front.order {
        if front.modules.get(module).unwrap().package != PackageId::STD {
            continue;
        }
        let tests: Vec<&thir::TestDef> = front
            .checked(module)
            .unwrap()
            .thir
            .tests
            .iter()
            .filter(|t| crate::test::on_native(&front, t))
            .collect();
        if tests.is_empty() {
            continue;
        }
        let roots: Vec<Root<'_>> = tests.iter().map(|t| Root::Test(t)).collect();
        let starts: Vec<Start<'_>> = tests.iter().map(|t| Start::Test(t)).collect();
        let path = front.modules.get(module).unwrap().path.to_string();
        assert_same(&front, &path, &roots, &starts);
        modules += 1;
    }
    assert!(modules >= 20, "{modules}");
}
