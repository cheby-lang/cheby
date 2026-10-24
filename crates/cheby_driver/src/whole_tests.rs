//! Tests of whole modules (D-966, `whole_*`), whole tests units and their
//! numbering (D-970, `tests_unit_*`), `cheby check`'s kind (D-971, D-978,
//! `kinds_*`) and D-943's rule for unreached missing symbols
//! (`unreached_*`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use cheby_codegen_clif::CodegenOptions;
use cheby_iface::DefId;
use cheby_mir::{FnKey, MirModule, MirOptions, UnitKind};
use cheby_package::ModulePath;

use crate::backend::{self, Roots, Scope, TestRoot};
use crate::frontend::{self, Frontend};
use crate::{CheckOutput, front_end};

/// A package `app` in a fresh temporary directory with `files` under
/// `src/`.
fn package(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cheby_whole_{}_{name}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("cheby.toml"),
        "name = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    for (path, text) in files {
        std::fs::write(dir.join("src").join(path), text).unwrap();
    }
    dir
}

/// The front end of the package at `dir`, which has no error.
fn front(dir: &Path) -> (Frontend, CheckOutput) {
    let mut out = CheckOutput::default();
    let front = front_end(dir, frontend::Mode::Build, &mut out).unwrap();
    assert!(!out.has_errors(), "{}", out.render());
    (front.unwrap().0, out)
}

/// The root module's `main`.
fn main_of(front: &Frontend) -> &cheby_types::thir::FnDef {
    let module = front.local_module(&ModulePath::root("app")).unwrap();
    front.main_of(module).unwrap()
}

/// The keys of the functions of `path`'s code unit.
fn keys_of(units: &[MirModule], path: &str) -> Vec<FnKey> {
    units
        .iter()
        .find(|u| matches!(u.unit, UnitKind::Code(_)) && &*u.path == path)
        .map(|u| u.functions.iter().map(|f| f.key).collect())
        .unwrap_or_default()
}

/// The item named `name` of module `path`.
fn def_named(front: &Frontend, path: &str, name: &str) -> DefId {
    let module = front
        .modules
        .iter()
        .find(|(_, info)| info.path.to_string() == path)
        .map(|(id, _)| id)
        .unwrap();
    let iface = front.set.get(module).unwrap();
    let (index, _) = iface
        .items
        .iter_enumerated()
        .find(|(_, item)| front.globals.names.get(item.name) == name)
        .unwrap();
    DefId::new(module, index)
}

const UTIL: &str = "//! Helpers, one of them unused.

import std::io

pub fn used() -> Nil {
  io::println(\"used\")
}

pub fn unused(n: Int) -> Int {
  n * 2
}
";

const MAIN: &str = "//! Calls one helper.

import app::util

fn main() {
  util::used()
}
";

/// D-966: a module that holds a reached item is lowered and compiled
/// whole, so its unreached function's code exists; D-510's view lowers
/// only the reached one.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn whole_module_compiles_its_unreached_function() {
    let dir = package("unreached", &[("main.cheby", MAIN), ("util.cheby", UTIL)]);
    let (front, out) = front(&dir);
    let main = main_of(&front);
    let unused = def_named(&front, "app::util", "unused");
    let used = def_named(&front, "app::util", "used");
    let roots = Roots::Main(main);
    let whole = backend::lower_scoped(
        &front,
        &out.sources,
        &roots,
        MirOptions::default(),
        Scope::Whole,
    )
    .unwrap();
    let keys = keys_of(&whole.units, "app::util");
    assert!(keys.contains(&FnKey::Item(unused)), "{keys:?}");
    assert!(keys.contains(&FnKey::Item(used)), "{keys:?}");
    assert!(!whole.reach.contains(unused));
    let reached = backend::lower_scoped(
        &front,
        &out.sources,
        &roots,
        MirOptions::default(),
        Scope::Reached,
    )
    .unwrap();
    assert!(!keys_of(&reached.units, "app::util").contains(&FnKey::Item(unused)));
    let codegen = CodegenOptions {
        capture_clif: true,
        ..CodegenOptions::default()
    };
    let Ok(mut built) = backend::build_scoped(
        &front,
        &out.sources,
        &roots,
        codegen,
        MirOptions::default(),
        None,
        false,
        Scope::Whole,
    )
    .unwrap() else {
        panic!("the package did not build");
    };
    let clif = built.jit.take_clif().join("\n");
    assert!(clif.contains("cb$fn$app$util$unused"), "{clif}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// D-966 on every example: the whole-module build of `main` compiles
/// (every function of every module of the load set lowers and compiles,
/// D-554) and evaluates the same constants as D-510's build, with the same
/// outcome.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn whole_every_example_builds() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|dir| dir.join("cheby.toml").is_file())
        .collect();
    dirs.sort();
    let mut built = 0;
    for dir in dirs {
        let mut out = CheckOutput::default();
        let Some((front, package)) = front_end(&dir, frontend::Mode::Build, &mut out)
            .unwrap()
            .filter(|_| !out.has_errors())
        else {
            continue;
        };
        let Some(main) = front
            .local_module(&ModulePath::root(&package))
            .and_then(|m| front.main_of(m))
        else {
            continue;
        };
        let outcome = |scope: Scope| -> Result<usize, Vec<String>> {
            match backend::build_scoped(
                &front,
                &out.sources,
                &Roots::Main(main),
                CodegenOptions::default(),
                MirOptions::default(),
                None,
                false,
                scope,
            )
            .unwrap_or_else(|ice| panic!("{}: {ice}", dir.display()))
            {
                Ok(_) => Ok(0),
                Err(diagnostics) => Err(diagnostics.iter().map(|d| d.message.clone()).collect()),
            }
        };
        assert_eq!(
            outcome(Scope::Whole),
            outcome(Scope::Reached),
            "{}",
            dir.display()
        );
        built += 1;
    }
    assert!(built >= 29, "{built}");
}

/// The example `001_hello_world`.
fn hello_world() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/001_hello_world")
}

/// D-1175: the dumps of `001_hello_world` keep D-510's
/// view, whatever the commands lower: `dump_mir` prints the units of
/// [`Scope::Reached`], and whole modules compile more of `std::io`.
#[test]
fn whole_dumps_of_001_keep_the_reached_view() {
    let dir = hello_world();
    let (front, out) = {
        let mut out = CheckOutput::default();
        let (front, _) = front_end(&dir, frontend::Mode::Build, &mut out)
            .unwrap()
            .unwrap();
        (front, out)
    };
    let module = front
        .local_module(&ModulePath::root("hello_world"))
        .unwrap();
    let main = front.main_of(module).unwrap();
    let roots = Roots::Main(main);
    let dump = |units: &[MirModule]| -> Vec<String> {
        units.iter().map(|u| cheby_mir::dump(u).unwrap()).collect()
    };
    let program = backend::lower_program(&front, &out.sources, main).unwrap();
    let reached = backend::lower_scoped(
        &front,
        &out.sources,
        &roots,
        MirOptions::default(),
        Scope::Reached,
    )
    .unwrap();
    assert_eq!(dump(&program), dump(&reached.units));
    let (_, text) = crate::dump_mir(&dir).unwrap();
    assert_eq!(text, dump(&program).join("\n"));
    let whole = backend::lower_scoped(
        &front,
        &out.sources,
        &roots,
        MirOptions::default(),
        Scope::Whole,
    )
    .unwrap();
    let io = |units: &[MirModule]| keys_of(units, "std::io").len();
    assert!(
        io(&whole.units) > io(&program),
        "{} {}",
        io(&whole.units),
        io(&program)
    );
    // Reach is the same either way: the dumps and the build agree on what
    // `main` reaches.
    assert_eq!(whole.reach, reached.reach);
}

/// D-1176: a module that only lends a
/// constructor to reached code is in the load set and compiled whole.
#[test]
fn whole_module_that_lends_a_constructor_is_compiled_whole() {
    let shapes = "//! Shapes.

pub type Shape {
  Circle(Int),
  Square(Int),
}

pub fn area(shape: Shape) -> Int {
  case shape {
    Circle(r) => 3 * r * r
    Square(s) => s * s
  }
}
";
    let main = "//! Builds a shape and compares it.

import app::shapes

fn main() {
  assert shapes::Circle(2) != shapes::Square(2)
}
";
    let dir = package("lender", &[("main.cheby", main), ("shapes.cheby", shapes)]);
    let (front, out) = front(&dir);
    let area = def_named(&front, "app::shapes", "area");
    let roots = Roots::Main(main_of(&front));
    let whole = backend::lower_scoped(
        &front,
        &out.sources,
        &roots,
        MirOptions::default(),
        Scope::Whole,
    )
    .unwrap();
    assert!(keys_of(&whole.units, "app::shapes").contains(&FnKey::Item(area)));
    assert!(!whole.reach.contains(area));
    let reached = backend::lower_scoped(
        &front,
        &out.sources,
        &roots,
        MirOptions::default(),
        Scope::Reached,
    )
    .unwrap();
    assert!(
        reached
            .units
            .iter()
            .any(|u| &*u.path == "app::shapes" && matches!(u.unit, UnitKind::Code(_)))
    );
    assert!(!keys_of(&reached.units, "app::shapes").contains(&FnKey::Item(area)));
    std::fs::remove_dir_all(&dir).unwrap();
}

const THREE_TESTS: &str = "//! Three tests.

import std::list

fn main() {
  Nil
}

test \"first\" {
  assert list::length([1]) == 1
}

test \"second\" {
  let double = fn(n) { n * 2 }
  assert double(2) == 4
}

test \"third\" {
  let triple = fn(n) { n * 3 }
  assert triple(1) == 3
}
";

/// D-970, D-1179: a whole tests unit holds every test of its
/// module, so in the JIT the test symbols and their closures number all
/// tests in source order, whatever the filter selects; only the selected
/// test gets an entry thunk. D-510's view numbered the selected ones.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn tests_unit_numbering_ignores_the_filter() {
    let dir = package("numbering", &[("main.cheby", THREE_TESTS)]);
    let (front, out) = front(&dir);
    let module = front.local_module(&ModulePath::root("app")).unwrap();
    let tests = &front.checked(module).unwrap().thir.tests;
    let third = tests.iter().find(|t| &*t.name == "third").unwrap();
    let clif_of = |scope: Scope| -> String {
        let roots = Roots::Tests(vec![TestRoot {
            module,
            test: third,
        }]);
        let codegen = CodegenOptions {
            capture_clif: true,
            ..CodegenOptions::default()
        };
        let Ok(mut built) = backend::build_scoped(
            &front,
            &out.sources,
            &roots,
            codegen,
            MirOptions::default(),
            None,
            false,
            scope,
        )
        .unwrap() else {
            panic!("the tests did not build");
        };
        built.jit.take_clif().join("\n")
    };
    let whole = clif_of(Scope::Whole);
    for symbol in [
        "; cb$test$app$0\n",
        "; cb$test$app$1\n",
        "; cb$test$app$2\n",
        "cb$clo$test$app$1$0",
        "cb$clo$test$app$2$0",
        "cb$entry$test$app$2",
    ] {
        assert!(whole.contains(symbol), "{symbol}");
    }
    assert!(!whole.contains("cb$entry$test$app$0"));
    assert!(!whole.contains("cb$entry$test$app$1"));
    // Before whole tests units, the selected test was the module's only
    // one: number 0.
    let reached = clif_of(Scope::Reached);
    assert!(reached.contains("cb$entry$test$app$0"));
    assert!(reached.contains("cb$clo$test$app$0$0"));
    assert!(!reached.contains("cb$test$app$1"));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// D-970: `cheby test` with a filter runs only the selected test and
/// reports the others as filtered out, over a whole tests unit.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn tests_unit_filter_selects_only_entries() {
    let dir = package("filter", &[("main.cheby", THREE_TESTS)]);
    let options = crate::TestOptions {
        filter: Some("third".to_owned()),
        ..crate::TestOptions::default()
    };
    let crate::PreparedTests::Ready(program) = crate::prepare_test(&dir, &options).unwrap() else {
        panic!("the tests did not build");
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    let report = program.run(&mut out, &mut err).unwrap();
    assert!(report.success());
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("third"), "{text}");
    assert!(!text.contains("first"), "{text}");
    assert!(text.contains("2 filtered out"), "{text}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// D-971, D-978: `cheby check` evaluates constants on `plain` code, so its
/// whole-module build infers no masks; `cheby run`'s build borrows where
/// it may (D-787).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn kinds_check_evaluates_constants_on_plain_code() {
    let main = "//! A constant and a function a build may borrow in.

const answer: Bool = check(\"x\") && True

fn check(s: String) -> Bool {
  s == \"x\"
}

fn main() {
  assert answer && check(\"x\")
}
";
    let dir = package("kinds", &[("main.cheby", main)]);
    let (front, out) = front(&dir);
    let mir = MirOptions::default();
    assert!(!backend::kind_options(&Roots::Constants, mir, Scope::Whole).borrows());
    assert!(backend::kind_options(&Roots::Constants, mir, Scope::Reached).borrows());
    let main_def = main_of(&front);
    assert!(backend::kind_options(&Roots::Main(main_def), mir, Scope::Whole).borrows());
    let clif_of = |roots: &Roots<'_>| -> String {
        let codegen = CodegenOptions {
            capture_clif: true,
            ..CodegenOptions::default()
        };
        let Ok(mut built) = backend::build_scoped(
            &front,
            &out.sources,
            roots,
            codegen,
            mir,
            None,
            false,
            Scope::Whole,
        )
        .unwrap() else {
            panic!("the package did not build");
        };
        built.jit.take_clif().join("\n")
    };
    let run = clif_of(&Roots::Main(main_def));
    // D-917: a borrowing slot carries its mask, `$$b<mask>`.
    assert!(run.contains("cb$slot$app$check$$b1"), "{run}");
    let check = clif_of(&Roots::Constants);
    assert!(check.contains("cb$slot$app$check"), "{check}");
    assert!(!check.contains("cb$slot$app$check$$b"), "{check}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The `E0053` symbols of a build of the package at `dir` in `scope`, or
/// `None` when the package does not check.
fn e0053(dir: &Path, scope: Scope) -> Option<BTreeSet<String>> {
    let mut out = CheckOutput::default();
    let (front, package) = front_end(dir, frontend::Mode::Build, &mut out).ok()??;
    if out.has_errors() {
        return None;
    }
    let main = front
        .local_module(&ModulePath::root(&package))
        .and_then(|m| front.main_of(m))?;
    let built = backend::build_scoped(
        &front,
        &out.sources,
        &Roots::Main(main),
        CodegenOptions::default(),
        MirOptions::default(),
        None,
        false,
        scope,
    )
    .unwrap();
    Some(match built {
        Ok(_) => BTreeSet::new(),
        Err(diagnostics) => diagnostics
            .iter()
            .filter(|d| d.code.to_string() == "E0053")
            .map(|d| d.message.clone())
            .collect(),
    })
}

/// D-943: on every package of `tests/ui/12-targets-and-ffi`, E0053 covers
/// the same symbols with whole modules as D-510's build reported
/// (b7160e4's rule: the externals of reached functions), and
/// `unreached_native_symbol` reports only its reached one.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unreached_e0053_set_is_d510s_on_the_ffi_packages() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/ui/12-targets-and-ffi");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    let mut compared = 0;
    let mut reported = 0;
    for path in entries {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let (dir, temporary) = if path.join("cheby.toml").is_file() {
            (path.clone(), false)
        } else if path.extension().is_some_and(|e| e == "cheby") {
            let text = std::fs::read_to_string(&path).unwrap();
            (
                package(&format!("ffi_{name}"), &[("main.cheby", &text)]),
                true,
            )
        } else {
            continue;
        };
        let reached = e0053(&dir, Scope::Reached);
        let whole = e0053(&dir, Scope::Whole);
        assert_eq!(whole, reached, "{name}");
        if let Some(symbols) = &whole {
            compared += 1;
            reported += usize::from(!symbols.is_empty());
        }
        if name == "unreached_native_symbol" {
            let symbols: Vec<String> = whole.unwrap().into_iter().collect();
            assert_eq!(
                symbols,
                ["the native symbol `app_reached_missing` is not defined"]
            );
        }
        if temporary {
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
    // Seven packages build a `main` at b7160e4, five of them with E0053.
    assert!(compared >= 7 && reported >= 5, "{compared} {reported}");
}

/// D-943 in an executable: a package whose only missing native symbol an
/// unreached function names builds; the program object defines the stub,
/// global and hidden, and the module object imports it through its import
/// word; the executable links against the runtime archive beside this
/// test's target directory (`cargo build -p cheby_rt_exe` first, D-929),
/// prints what `cheby run` prints, and exports no stub.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn unreached_build_links_and_hides_the_stub() {
    use ::object::read::{Object as _, ObjectSymbol as _};

    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/ui/13-tooling/build/unreached_missing_symbol.cheby"),
    )
    .unwrap();
    let dir = package("build_stub", &[("main.cheby", &text)]);
    let exe = dir.join("app-exe");
    let options = crate::BuildOptions {
        output: Some(exe.clone()),
        ..crate::BuildOptions::default()
    };
    let crate::aot::Objects::Ready { output, built } =
        crate::aot::build_objects(&dir, &options, None).unwrap()
    else {
        panic!("the package did not build");
    };
    assert!(!output.has_errors(), "{}", output.render());
    let stub = "app_build_unreached_missing";
    let plain = |name: &str| -> String {
        if cfg!(target_os = "macos") {
            name.strip_prefix('_').unwrap_or(name).to_owned()
        } else {
            name.to_owned()
        }
    };
    let (_, program) = built.objects.last().unwrap();
    let file = ::object::File::parse(&**program).unwrap();
    let symbol = file
        .symbols()
        .find(|s| s.name().is_ok_and(|n| plain(n) == stub))
        .expect("the program object defines the stub");
    assert!(symbol.is_definition() && symbol.is_global());
    assert_eq!(symbol.scope(), ::object::SymbolScope::Linkage);
    let (_, module) = built
        .objects
        .iter()
        .find(|(name, _)| name == "app")
        .unwrap();
    let file = ::object::File::parse(&**module).unwrap();
    assert!(
        file.symbols()
            .any(|s| s.is_undefined() && s.name().is_ok_and(|n| plain(n) == stub))
    );
    // The archive beside the target directory's `cheby`.
    let target = std::env::current_exe()
        .unwrap()
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("cheby");
    let archive = cheby_link::archive::find_archive(None, &target).unwrap();
    cheby_link::archive::check_runtime_id(&archive).unwrap();
    cheby_link::link::link(&cheby_link::link::LinkJob {
        objects: &built.objects,
        archive: &archive,
        output: &exe,
    })
    .unwrap();
    let run = std::process::Command::new(&exe).output().unwrap();
    assert_eq!(run.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(run.stdout).unwrap(),
        "linked with an unreached missing symbol\n"
    );
    let bytes = std::fs::read(&exe).unwrap();
    let file = ::object::File::parse(&*bytes).unwrap();
    let exported: Vec<String> = file
        .exports()
        .unwrap()
        .iter()
        .map(|e| plain(&String::from_utf8_lossy(e.name())))
        .chain(
            file.dynamic_symbols()
                .filter(::object::ObjectSymbol::is_definition)
                .filter_map(|s| s.name().ok().map(plain)),
        )
        .collect();
    assert!(!exported.iter().any(|n| n == stub), "{exported:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}
