//! MIR lowering, code generation, constant evaluation and running (G3,
//! E6).
//!
//! A build lowers only what its roots need (D-510, D-534): the items
//! reachable from the `main` being run, the selected tests, or every
//! constant of the package for `cheby check`, across modules, with the
//! constructor layouts of every module that reachable code uses and the
//! `@std_hooks` functions of their types (D-482). Each such module is
//! lowered to its own MIR unit, in build order with items in `DefIndex`
//! order; the selected tests of a module form its tests unit, and the
//! entry thunks, those of every reachable constant included, a separate
//! unit (D-453). The JIT compiles all units in one batch, then evaluates
//! the reachable constants in that JIT (D-535), so code that runs later
//! reads their values from their slots.
//!
//! A reachable construct that the back end does not implement yet is
//! E0038 at the construct; every reachable item is lowered first, so one
//! build reports all of them, and nothing is compiled.
//!
//! Constants are evaluated under the package's `[const-eval]` limits
//! (D-536), checked against the JS range when they exist on `js` (D-540),
//! and, when the build runs code, their maps and sets are rebuilt with the
//! per-process seed (D-539).
//!
//! **Dumps for determinism checks** (plan §4.5, D-551, D-620): when the
//! environment variable `CHEBY_DUMP_DIR` names a directory, every build
//! writes there, overwriting earlier files:
//!
//! - `mir/<module path>.mir` for each code unit, `mir/<module
//!   path>.tests.mir` for each tests unit and `mir/entries.mir` for the
//!   entry thunks, as `cheby_mir::dump` prints them;
//! - `clif.txt`, the CLIF of every compiled function joined with
//!   newlines, in compile order (only when the caller does not capture
//!   CLIF itself, as `dump_clif` does, so its text is not taken away);
//! - `consts.txt`, one line `<module path>::<name> = <debug text>` per
//!   evaluated constant, sorted by module path and name (empty when no
//!   constant was evaluated).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cheby_consteval::{ConstEvalConfig, ConstInfo, ConstMode, ConstTypes};
use cheby_diag::{Diagnostic, SourceMap, codes};
use cheby_iface::{DefId, DepView, Ice, ModuleId};
use cheby_jit::{Jit, JitError, JitOptions};
use cheby_mir::reach::{Reach, Root, reachable_with_hooks};
use cheby_mir::{
    Defs, EntryKind, EntryRequest, LowerCx, MirModule, Program, UnitKind, Unsupported,
    count_and_check, lower_entries, lower_items, lower_tests,
};
use cheby_package::Target;
use cheby_types::thir;

use crate::frontend::Frontend;

/// The environment variable naming the directory of the determinism dumps
/// (plan §4.5).
const DUMP_DIR_VAR: &str = "CHEBY_DUMP_DIR";

/// What a build starts from.
pub(crate) enum Roots<'a> {
    /// A `main` to run (`cheby run`, the dumps).
    Main(&'a thir::FnDef),
    /// The tests to run (`cheby test`, plan §8.1).
    Tests(Vec<TestRoot<'a>>),
    /// Every constant of the local package (`cheby check`, plan §7.1,
    /// D-534).
    Constants,
    /// Every function of the local package, lowered and compiled, nothing
    /// evaluated or run (the code-generation benchmark of I1, plan §8.5).
    AllFunctions,
}

/// One test to run, with its module.
pub(crate) struct TestRoot<'a> {
    pub module: ModuleId,
    pub test: &'a thir::TestDef,
}

/// A build's result.
pub(crate) struct Built {
    /// The code compiled and the reachable constants evaluated: the entry
    /// thunks of `main` or of the tests are ready to run.
    pub jit: Jit,
}

/// Lowers what the roots reach (D-510), compiles it in one JIT, and
/// evaluates the reachable constants in that JIT (E6). `Err` holds E0038,
/// E0053, E0069 or E0075-E0078 diagnostics, sorted. `Constants` builds
/// with `stub_unresolved` (plan §4.4) and never reports E0053 (D-532);
/// `AllFunctions` evaluates nothing. A CLIF dump (`capture_clif`) compiles
/// with the default options, so dumps do not depend on the environment
/// (D-264). With `CHEBY_DUMP_DIR` set, it writes the determinism dumps
/// (module docs).
#[expect(
    clippy::needless_pass_by_value,
    reason = "the signature of plan §4.5, which callers build in place"
)]
pub(crate) fn build(
    front: &Frontend,
    sources: &SourceMap,
    roots: Roots<'_>,
    capture_clif: bool,
) -> Result<Result<Built, Vec<Diagnostic>>, Ice> {
    build_dumping(front, sources, &roots, capture_clif, dump_dir().as_deref())
}

/// [`build`], writing the determinism dumps to `dump` if it is given.
fn build_dumping(
    front: &Frontend,
    sources: &SourceMap,
    roots: &Roots<'_>,
    capture_clif: bool,
    dump: Option<&Path>,
) -> Result<Result<Built, Vec<Diagnostic>>, Ice> {
    let lowered = match lower(front, sources, roots)? {
        Ok(lowered) => lowered,
        Err(mut diagnostics) => {
            sources.sort(&mut diagnostics);
            return Ok(Err(diagnostics));
        }
    };
    if let Some(dir) = dump {
        write_mir_dumps(dir, &lowered.units)?;
    }
    let check = matches!(roots, Roots::Constants);
    let options = JitOptions {
        stub_unresolved: check,
        ..jit_options(capture_clif)
    };
    // The dump captures CLIF too; code generation is the same either way.
    let capture = capture_clif || dump.is_some();
    let mut jit = match compile_with(&lowered.units, capture, options) {
        Ok(jit) => jit,
        Err(JitError::UnresolvedSymbols(symbols)) => {
            let mut diagnostics = unresolved(front, &symbols)?;
            sources.sort(&mut diagnostics);
            return Ok(Err(diagnostics));
        }
        Err(e) => return Err(jit_ice(e)),
    };
    if let Some(dir) = dump
        && !capture_clif
    {
        write_dump(&dir.join("clif.txt"), &jit.take_clif().join("\n"))?;
    }
    let mut evaluated = Vec::new();
    if !matches!(roots, Roots::AllFunctions) {
        let infos = const_infos(front, sources, &lowered.reach)?;
        let config = ConstEvalConfig {
            mode: if check {
                ConstMode::Check
            } else {
                ConstMode::Run
            },
            // The manifest's `[const-eval]` limits (D-536).
            time_limit: front.const_eval_time_limit,
            memory_limit: front.const_eval_memory_limit,
            targets: front.package_targets,
        };
        let item = |def: DefId| {
            front
                .set
                .get(def.module)
                .and_then(|iface| iface.item(def.index))
        };
        let types = ConstTypes {
            types: &front.globals.types,
            item: &item,
        };
        let mut diagnostics =
            cheby_consteval::evaluate_with(&mut jit, &infos, &config, Some(&types))?;
        if !diagnostics.is_empty() {
            sources.sort(&mut diagnostics);
            return Ok(Err(diagnostics));
        }
        evaluated = infos;
    }
    if let Some(dir) = dump {
        write_const_dump(front, &jit, dir, &evaluated)?;
    }
    Ok(Ok(Built { jit }))
}

/// The directory of the determinism dumps, if `CHEBY_DUMP_DIR` names one.
fn dump_dir() -> Option<PathBuf> {
    std::env::var_os(DUMP_DIR_VAR)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

/// Writes `text` to `path`, creating its directory.
fn write_dump(path: &Path, text: &str) -> Result<(), Ice> {
    let fail = |e: std::io::Error| {
        Ice::new(format!(
            "{DUMP_DIR_VAR}: cannot write {}: {e}",
            path.display()
        ))
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(fail)?;
    }
    std::fs::write(path, text).map_err(fail)
}

/// The MIR dump of every unit, one file per unit under `mir/`.
fn write_mir_dumps(dir: &Path, units: &[MirModule]) -> Result<(), Ice> {
    for unit in units {
        let name = match unit.unit {
            UnitKind::Code(_) => format!("{}.mir", unit.path),
            UnitKind::Tests(_) => format!("{}.tests.mir", unit.path),
            UnitKind::Entries => "entries.mir".to_owned(),
        };
        write_dump(&dir.join("mir").join(name), &cheby_mir::dump(unit)?)?;
    }
    Ok(())
}

/// `consts.txt`: the debug text of every evaluated constant, sorted by
/// module path and name.
fn write_const_dump(
    front: &Frontend,
    jit: &Jit,
    dir: &Path,
    infos: &[ConstInfo],
) -> Result<(), Ice> {
    let defs: Vec<DefId> = infos.iter().map(|info| info.def).collect();
    let texts = cheby_consteval::debug_texts(jit, &defs)?;
    let mut lines = Vec::with_capacity(infos.len());
    for (info, text) in infos.iter().zip(texts) {
        let Some(text) = text else { continue };
        let module = front.modules.try_get(info.def.module)?.path.to_string();
        lines.push((module, info.name.to_string(), text));
    }
    lines.sort();
    let mut out = String::new();
    for (module, name, text) in lines {
        for part in [
            module.as_str(),
            "::",
            name.as_str(),
            " = ",
            text.as_str(),
            "\n",
        ] {
            out.push_str(part);
        }
    }
    write_dump(&dir.join("consts.txt"), &out)
}

/// One E0053 at the `@external` that names each unresolved symbol (D-532).
fn unresolved(front: &Frontend, symbols: &[String]) -> Result<Vec<Diagnostic>, Ice> {
    symbols
        .iter()
        .map(|symbol| {
            let span = front
                .natives
                .get(symbol.as_str())
                .map(|binding| binding.span)
                .ok_or_else(|| {
                    Ice::new(format!("the native symbol `{symbol}` has no `@external`"))
                })?;
            Ok(Diagnostic::new(
                codes::UNRESOLVED_NATIVE_SYMBOL,
                format!("the native symbol `{symbol}` is not defined"),
            )
            .with_primary(span, "nothing defines this symbol")
            .with_note("the symbol must be defined by the Cheby runtime or the running process"))
        })
        .collect()
}

/// A JIT failure that is not about the program.
fn jit_ice(e: JitError) -> Ice {
    match e {
        JitError::Codegen(cheby_codegen_clif::CodegenError::Ice(ice)) => ice,
        other => Ice::new(other.to_string()),
    }
}

/// What [`lower`] produced.
struct Lowered {
    units: Vec<MirModule>,
    reach: Reach,
}

/// The MIR units that running `main` needs, after reference counting and
/// validation: one per needed module in build order, then the entry unit,
/// which also holds the thunks of the constants `main` reaches. `Err`
/// holds the E0038 diagnostics of the constructs the back end cannot
/// lower yet, unsorted.
pub(crate) fn lower_program(
    front: &Frontend,
    sources: &SourceMap,
    main: &thir::FnDef,
) -> Result<Result<Vec<MirModule>, Vec<Diagnostic>>, Ice> {
    Ok(lower(front, sources, &Roots::Main(main))?.map(|lowered| lowered.units))
}

/// Lowers what `roots` reach.
fn lower(
    front: &Frontend,
    sources: &SourceMap,
    roots: &Roots<'_>,
) -> Result<Result<Lowered, Vec<Diagnostic>>, Ice> {
    let mut program = Program::new();
    for &module in &front.order {
        program.insert(module, &checked(front, module)?.thir);
    }
    let local = local_modules(front);
    let mut walk_roots: Vec<Root<'_>> = Vec::new();
    let mut entries: Vec<EntryRequest<'_>> = Vec::new();
    // The selected tests of each module, by module.
    let mut tests: BTreeMap<ModuleId, Vec<&thir::TestDef>> = BTreeMap::new();
    match roots {
        Roots::Main(main) => {
            walk_roots.push(Root::Fn(main));
            entries.push(EntryRequest {
                kind: EntryKind::Main(main.def),
                info: main.entry.as_ref(),
            });
        }
        Roots::Tests(selected) => {
            for t in selected {
                walk_roots.push(Root::Test(t.test));
                entries.push(EntryRequest {
                    kind: EntryKind::Test(t.test.def),
                    info: Some(&t.test.entry),
                });
                tests.entry(t.module).or_default().push(t.test);
            }
        }
        Roots::Constants => {
            for &module in &local {
                walk_roots.extend(checked(front, module)?.thir.consts.iter().map(Root::Const));
            }
        }
        Roots::AllFunctions => {
            for &module in &local {
                walk_roots.extend(checked(front, module)?.thir.fns.iter().map(Root::Fn));
            }
        }
    }
    let hooks = |module: ModuleId| hooks_of(front, module);
    let reach = reachable_with_hooks(&program, &walk_roots, &hooks)?;
    if !matches!(roots, Roots::AllFunctions) {
        for def in reach.constant_order() {
            entries.push(EntryRequest {
                kind: EntryKind::Const(def),
                info: None,
            });
        }
    }
    let mut units = Vec::new();
    let mut unsupported = Unsupported::default();
    for &module in &front.order {
        let selected = tests.get(&module);
        if !reach.needs_module(module) && selected.is_none() {
            continue;
        }
        let checked = checked(front, module)?;
        let own = front
            .set
            .get(module)
            .ok_or_else(|| Ice::new(format!("module {module:?} has no interface")))?;
        let deps = DepView::new(&front.set, &checked.deps);
        let cx = LowerCx {
            defs: Defs {
                own: Some(own),
                deps: &deps,
                modules: &front.modules,
                globals: &front.globals,
            },
            sources,
        };
        if reach.needs_module(module) {
            let lowered = lower_items(&checked.thir, &|def| reach.contains(def), &cx);
            if let Some(mut unit) = unsupported.take(lowered)? {
                count_and_check(&mut unit)?;
                units.push(unit);
            }
        }
        if let Some(selected) = selected {
            // Only the selected tests: the others may reach items this
            // build does not lower.
            let only = thir::Module {
                module,
                fns: Vec::new(),
                consts: Vec::new(),
                tests: selected.iter().map(|&t| t.clone()).collect(),
            };
            if let Some(mut unit) = unsupported.take(lower_tests(&only, &cx))? {
                count_and_check(&mut unit)?;
                units.push(unit);
            }
        }
    }
    if !entries.is_empty()
        && let Some(entry) = unsupported.take(lower_entry_unit(front, sources, &entries))?
    {
        units.push(entry);
    }
    if unsupported.is_empty() {
        Ok(Ok(Lowered { units, reach }))
    } else {
        Ok(Err(unsupported.into_diagnostics()))
    }
}

/// The modules of the local package, in build order.
fn local_modules(front: &Frontend) -> Vec<ModuleId> {
    front
        .order
        .iter()
        .copied()
        .filter(|&m| {
            front
                .modules
                .get(m)
                .is_some_and(|info| info.package == cheby_iface::PackageId::LOCAL)
        })
        .collect()
}

fn checked(front: &Frontend, module: ModuleId) -> Result<&crate::frontend::Checked, Ice> {
    front
        .checked(module)
        .ok_or_else(|| Ice::new(format!("module {module:?} was not checked")))
}

/// What reads `module`'s interface alone.
fn own_defs<'a>(
    front: &'a Frontend,
    module: ModuleId,
    deps: &'a DepView<'a>,
) -> Result<Defs<'a>, Ice> {
    let own = front
        .set
        .get(module)
        .ok_or_else(|| Ice::new(format!("module {module:?} has no interface")))?;
    Ok(Defs {
        own: Some(own),
        deps,
        modules: &front.modules,
        globals: &front.globals,
    })
}

/// The `@std_hooks` functions of every type `module` declares (D-482),
/// read from its interface: a module's code unit registers the layouts of
/// all its types, so their hooks must be compiled too.
fn hooks_of(front: &Frontend, module: ModuleId) -> Result<Vec<DefId>, Ice> {
    let none = DepView::new(&front.set, &[]);
    let defs = own_defs(front, module, &none)?;
    let Some(own) = defs.own else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (index, item) in own.items.iter_enumerated() {
        if let cheby_iface::ItemSig::Type(t) = &item.sig
            && t.hooks.is_some()
            && let Some(fns) = cheby_mir::layout::type_hooks(DefId::new(module, index), &defs)?
        {
            out.extend(fns);
        }
    }
    Ok(out)
}

/// The entry unit with the thunks of `entries` (D-137, D-453).
fn lower_entry_unit(
    front: &Frontend,
    sources: &SourceMap,
    entries: &[EntryRequest<'_>],
) -> Result<MirModule, cheby_mir::LowerError> {
    let mut all: Vec<ModuleId> = front.order.clone();
    all.sort_unstable();
    let deps = DepView::new(&front.set, &all);
    let cx = LowerCx {
        defs: Defs {
            own: None,
            deps: &deps,
            modules: &front.modules,
            globals: &front.globals,
        },
        sources,
    };
    let mut unit = lower_entries(entries, &cx)?;
    count_and_check(&mut unit)?;
    Ok(unit)
}

/// The reachable constants in evaluation order (D-535: dependencies
/// first, ties by module order, then name), each with its name's span, its
/// seed (D-538) and whether it exists on `js` (D-540).
fn const_infos(
    front: &Frontend,
    sources: &SourceMap,
    reach: &Reach,
) -> Result<Vec<ConstInfo>, Ice> {
    let position: BTreeMap<ModuleId, usize> = front
        .order
        .iter()
        .enumerate()
        .map(|(n, &m)| (m, n))
        .collect();
    let none = DepView::new(&front.set, &[]);
    let mut names: BTreeMap<DefId, String> = BTreeMap::new();
    let mut spans = BTreeMap::new();
    // Each module's constants by id, built once per module.
    let mut by_module: BTreeMap<ModuleId, BTreeMap<DefId, &thir::ConstDef>> = BTreeMap::new();
    for def in reach.items() {
        if let std::collections::btree_map::Entry::Vacant(slot) = by_module.entry(def.module) {
            slot.insert(
                checked(front, def.module)?
                    .thir
                    .consts
                    .iter()
                    .map(|c| (c.def, c))
                    .collect(),
            );
        }
        let Some(c) = by_module.get(&def.module).and_then(|m| m.get(&def)) else {
            continue;
        };
        let defs = own_defs(front, def.module, &none)?;
        names.insert(def, defs.name(def)?.item.into());
        spans.insert(def, c.span);
    }
    let const_modules = reach.const_modules();
    let order = reach.constant_order_by(|def| {
        (
            position.get(&def.module).copied().unwrap_or(usize::MAX),
            names.get(&def).cloned().unwrap_or_default(),
        )
    });
    let mut source_hashes: BTreeMap<ModuleId, u64> = BTreeMap::new();
    let mut infos = Vec::with_capacity(order.len());
    for def in order {
        let name = names
            .get(&def)
            .ok_or_else(|| Ice::new(format!("the constant {def:?} has no name")))?;
        let name_span = *spans
            .get(&def)
            .ok_or_else(|| Ice::new(format!("the constant {def:?} has no span")))?;
        let info = front.modules.try_get(def.module)?;
        let module_path = info.path.to_string();
        let mut deps: Vec<(String, u64)> = Vec::new();
        for &module in const_modules.get(&def).map_or(&[][..], Vec::as_slice) {
            let hash = if let Some(&hash) = source_hashes.get(&module) {
                hash
            } else {
                let hash = module_source_hash(front, sources, module)?;
                source_hashes.insert(module, hash);
                hash
            };
            deps.push((front.modules.try_get(module)?.path.to_string(), hash));
        }
        deps.sort();
        let deps: Vec<(&str, u64)> = deps.iter().map(|(p, h)| (p.as_str(), *h)).collect();
        // The package name is the module path's first segment, `std` for
        // std modules (D-538).
        let seed =
            cheby_runtime::consteval::const_seed(info.path.package(), &module_path, name, &deps);
        let on_js = own_defs(front, def.module, &none)?
            .item(def)?
            .targets
            .contains(Target::Js);
        infos.push(ConstInfo {
            def,
            name: name.as_str().into(),
            name_span,
            seed,
            on_js,
        });
    }
    Ok(infos)
}

/// The `SipHash` of `module`'s source bytes (D-538); 0 for a module without
/// a source file.
fn module_source_hash(front: &Frontend, sources: &SourceMap, module: ModuleId) -> Result<u64, Ice> {
    let info = front.modules.try_get(module)?;
    let Some(file) = info.file else {
        return Ok(0);
    };
    let text = sources
        .file(file)
        .ok_or_else(|| Ice::new(format!("module `{}` has no source", info.path)))?
        .text();
    Ok(cheby_runtime::consteval::source_hash(text.as_bytes()))
}

/// The JIT options of a build: from the environment (checked counting
/// under `CHEBY_RT_POISON=1`, plan §4.3.4), or the defaults for a CLIF
/// dump, so dumps do not depend on the environment (D-264).
fn jit_options(capture_clif: bool) -> JitOptions {
    if capture_clif {
        JitOptions::default()
    } else {
        JitOptions::from_env()
    }
}

fn compile_with(
    units: &[MirModule],
    capture_clif: bool,
    options: JitOptions,
) -> Result<Jit, JitError> {
    let mut jit = Jit::configured(
        cheby_codegen_clif::CodegenOptions {
            capture_clif,
            ..cheby_codegen_clif::CodegenOptions::default()
        },
        options,
    )?;
    jit.compile(units)?;
    Ok(jit)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use cheby_package::{Manifest, Targets};

    use super::*;
    use crate::{CheckOutput, RunOptions, RunResult, frontend};

    /// A package `app` in a fresh temporary directory, with its manifest
    /// and `src/main.cheby`.
    fn package(name: &str, manifest_extra: &str, main: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cheby_backend_{}_{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("cheby.toml"),
            format!("name = \"app\"\nversion = \"0.1.0\"\n{manifest_extra}"),
        )
        .unwrap();
        std::fs::write(dir.join("src/main.cheby"), main).unwrap();
        dir
    }

    fn front(dir: &Path) -> (Frontend, CheckOutput) {
        let mut out = CheckOutput::default();
        let (front, _) = crate::front_end(dir, frontend::Mode::Build, &mut out)
            .unwrap()
            .unwrap();
        assert!(!out.has_errors(), "{}", out.render());
        (front, out)
    }

    fn codes(out: &CheckOutput) -> Vec<String> {
        out.diagnostics
            .iter()
            .filter(|d| d.is_error())
            .map(|d| d.code.to_string())
            .collect()
    }

    const FORCING: &str = "//! Constants that read a map.

import std::list
import std::map

const table: map::Map<Int, Int> = numbers(40, map::new())

const keys: List<Int> = map::keys(table)

const size: Int = map::size(table) + list::length(keys)

fn numbers(n: Int, acc: map::Map<Int, Int>) -> map::Map<Int, Int> {
  case n {
    0 => acc
    _ => numbers(n - 1, map::insert(acc, n * 7919, n))
  }
}

fn main() {
  assert size == 80
}
";

    /// Plan §7.8: whichever constant forces `table` first, `table` is
    /// built under its own seed (D-535, D-538), so its key order, which
    /// `keys` records, is the same.
    #[test]
    fn a_constant_has_one_value_whichever_constant_forces_it() {
        let dir = package("forcing", "", FORCING);
        let (front, out) = front(&dir);
        let module = front
            .local_module(&cheby_package::ModulePath::root("app"))
            .unwrap();
        let main = front.main_of(module).unwrap();
        let lowered = lower(&front, &out.sources, &Roots::Main(main))
            .unwrap()
            .unwrap();
        let infos = const_infos(&front, &out.sources, &lowered.reach).unwrap();
        let names: Vec<&str> = infos.iter().map(|i| &*i.name).collect();
        assert_eq!(names, ["table", "keys", "size"]);
        let by_name = |name: &str| infos.iter().find(|i| &*i.name == name).unwrap().clone();
        let config = ConstEvalConfig {
            mode: ConstMode::Check,
            time_limit: Manifest::DEFAULT_TIME_LIMIT,
            memory_limit: Manifest::DEFAULT_MEMORY_LIMIT,
            targets: Targets::ALL,
        };
        let mut texts = Vec::new();
        for order in [
            ["table", "keys", "size"],
            ["keys", "table", "size"],
            ["size", "keys", "table"],
        ] {
            let mut jit = compile_with(&lowered.units, false, JitOptions::from_env()).unwrap();
            let order: Vec<ConstInfo> = order.iter().map(|n| by_name(n)).collect();
            let diagnostics = cheby_consteval::evaluate(&mut jit, &order, &config).unwrap();
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            let read = |name: &str| {
                // SAFETY: an evaluated constant's slot holds its value.
                unsafe { *jit.const_slot(by_name(name).def).unwrap() }
            };
            // SAFETY: frozen values that live as long as `jit`.
            unsafe {
                assert_eq!(
                    cheby_runtime::hamt::seed_of(read("table")),
                    Some(by_name("table").seed)
                );
                texts.push(cheby_runtime::generic::debug_string(read("keys")));
            }
            // Frozen results left the build's domain.
            assert_eq!(jit.domain().live_objects(), 0);
        }
        assert_eq!(texts[0], texts[1]);
        assert_eq!(texts[0], texts[2]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The error codes of `cheby check` on a package `app` with
    /// `manifest_extra` in its manifest and `main` as its root module.
    fn check_codes(name: &str, manifest_extra: &str, main: &str) -> Vec<String> {
        let dir = package(name, manifest_extra, main);
        let checked = crate::check(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        codes(&checked)
    }

    /// D-536: a constant that runs past the manifest's tiny time limit is
    /// E0076, whether its loop is a self tail call or tail calls between
    /// two functions; the deadline interrupts it at the next function
    /// entry, and `cheby check` returns soon after.
    #[test]
    fn a_tiny_time_limit_stops_endless_constants() {
        for (name, body) in [
            ("self", "fn spin(n: Int) -> Int {\n  spin(n + 1)\n}\n"),
            (
                "mutual",
                "fn spin(n: Int) -> Int {\n  spun(n + 1)\n}\n\nfn spun(n: Int) -> Int {\n  spin(n - 1)\n}\n",
            ),
        ] {
            let started = std::time::Instant::now();
            let codes = check_codes(
                &format!("time_{name}"),
                "[const-eval]\ntime-limit = \"30ms\"\n",
                &format!(
                    "//! An endless constant.\n\nconst forever: Int = spin(0)\n\n{body}\nfn main() {{\n  assert forever == 0\n}}\n"
                ),
            );
            assert_eq!(codes, ["E0076"], "{name}");
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "{name}"
            );
        }
    }

    /// D-536: the memory limit counts the build's live bytes, what kernels
    /// allocate included; a constant that only churns through temporaries
    /// stays under it.
    #[test]
    fn a_tiny_memory_limit_stops_constants_that_hold_too_much() {
        let limit = "[const-eval]\nmemory-limit = \"256KiB\"\n";
        for (name, init) in [
            // One kernel allocation of 1 MB.
            ("kernel", "string::length(string::repeat(\"ab\", 500_000))"),
            // Many small allocations by generated code, all live.
            ("code", "list::length(build(100_000, []))"),
        ] {
            let codes = check_codes(
                &format!("memory_{name}"),
                limit,
                &format!(
                    "//! A constant that holds too much.\n\nimport std::list\nimport std::string\n\n\
                     const held: Int = {init}\n\n\
                     fn build(n: Int, acc: List<(Int, Int)>) -> List<(Int, Int)> {{\n  case n {{\n    0 => acc\n    _ => build(n - 1, [(n, n), ..acc])\n  }}\n}}\n\n\
                     fn main() {{\n  assert held > 0\n}}\n"
                ),
            );
            assert_eq!(codes, ["E0076"], "{name}");
        }
        let codes = check_codes(
            "memory_churn",
            limit,
            "//! A constant that drops what it builds.\n\nimport std::list\n\n\
             const total: Int = churn(40, 0)\n\n\
             fn churn(n: Int, acc: Int) -> Int {\n  case n {\n    0 => acc\n    _ => churn(n - 1, acc + list::length(list::range(0, 5_000)))\n  }\n}\n\n\
             fn main() {\n  assert total == 200_000\n}\n",
        );
        assert_eq!(codes, Vec::<String>::new());
    }

    const KEYS: &str = "//! A constant that depends on a map's key order.

import std::list
import std::map
import std::set

const table: map::Map<Int, set::Set<Int>> = numbers(30, map::new())

const keys: List<Int> = map::keys(table)

fn numbers(n: Int, acc: map::Map<Int, set::Set<Int>>) -> map::Map<Int, set::Set<Int>> {
  case n {
    0 => acc
    _ => numbers(n - 1, map::insert(acc, n * 7919, set::from_list([n, n + 1])))
  }
}

fn main() {
  assert list::length(keys) == 30
}
";

    /// D-538, D-539 (plan §7.8): a constant that depends on `map::keys`
    /// order has the same value under `cheby check` (`Roots::Constants`, no
    /// rebuild) and `cheby run` (`Roots::Main`, rebuilt with the process
    /// seed afterwards); only the maps' own seeds differ.
    #[test]
    fn check_and_run_give_constants_the_same_value() {
        let dir = package("keys_order", "", KEYS);
        let (front, out) = front(&dir);
        let module = front
            .local_module(&cheby_package::ModulePath::root("app"))
            .unwrap();
        let main = front.main_of(module).unwrap();
        let consts: Vec<DefId> = checked(&front, module)
            .unwrap()
            .thir
            .consts
            .iter()
            .map(|c| c.def)
            .collect();
        let mut texts = Vec::new();
        let mut seeds = Vec::new();
        for roots in [Roots::Constants, Roots::Main(main)] {
            let Ok(built) = build_dumping(&front, &out.sources, &roots, false, None).unwrap()
            else {
                panic!("the package did not build");
            };
            texts.push(cheby_consteval::debug_texts(&built.jit, &consts).unwrap());
            let table = built.jit.const_slot(consts[0]).unwrap();
            // SAFETY: the frozen value of an evaluated constant.
            seeds.push(unsafe { cheby_runtime::hamt::seed_of(*table) });
            assert_eq!(built.jit.domain().live_objects(), 0);
        }
        assert!(texts[0].iter().all(Option::is_some));
        assert_eq!(texts[0], texts[1]);
        assert_ne!(seeds[0], seeds[1]);
        assert_eq!(seeds[1], Some(cheby_runtime::generic::process_seed()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Plan §4.5: the determinism dumps hold the MIR of every unit, the
    /// CLIF and the debug text of every evaluated constant, sorted.
    #[test]
    fn the_dump_hook_writes_mir_clif_and_constants() {
        let dir = package("dumps", "", KEYS);
        let dumps = dir.join("dumps");
        let (front, out) = front(&dir);
        let Ok(_) =
            build_dumping(&front, &out.sources, &Roots::Constants, false, Some(&dumps)).unwrap()
        else {
            panic!("the package did not build");
        };
        let consts = std::fs::read_to_string(dumps.join("consts.txt")).unwrap();
        let lines: Vec<&str> = consts.lines().collect();
        assert_eq!(lines.len(), 2, "{consts}");
        assert!(lines[0].starts_with("app::keys = ["), "{consts}");
        assert!(
            lines[1].starts_with("app::table = map::from_list(["),
            "{consts}"
        );
        let app = std::fs::read_to_string(dumps.join("mir/app.mir")).unwrap();
        assert!(app.contains("numbers"), "{app}");
        assert!(dumps.join("mir/std::map.mir").is_file());
        assert!(dumps.join("mir/entries.mir").is_file());
        let clif = std::fs::read_to_string(dumps.join("clif.txt")).unwrap();
        assert!(clif.contains("function"), "{clif}");
        // A second build overwrites the files: the same text.
        let Ok(_) =
            build_dumping(&front, &out.sources, &Roots::Constants, false, Some(&dumps)).unwrap()
        else {
            panic!("the package did not build");
        };
        assert_eq!(
            std::fs::read_to_string(dumps.join("consts.txt")).unwrap(),
            consts
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The gate relies on it (plan §2.1, gate 4): 007's constants evaluate
    /// under the default limits on the debug toolchain.
    #[test]
    fn example_007_constants_evaluate_under_the_default_limits() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/007_mandelbrot");
        let mut out = CheckOutput::default();
        let (front, _) = crate::front_end(&dir, frontend::Mode::Build, &mut out)
            .unwrap()
            .unwrap();
        assert!(!out.has_errors(), "{}", out.render());
        assert_eq!(front.const_eval_time_limit, Manifest::DEFAULT_TIME_LIMIT);
        assert_eq!(
            front.const_eval_memory_limit,
            Manifest::DEFAULT_MEMORY_LIMIT
        );
        let started = std::time::Instant::now();
        let Ok(built) =
            build_dumping(&front, &out.sources, &Roots::Constants, false, None).unwrap()
        else {
            panic!("007's constants did not evaluate");
        };
        let elapsed = started.elapsed();
        assert!(elapsed < Manifest::DEFAULT_TIME_LIMIT, "{elapsed:?}");
        assert_eq!(built.jit.domain().live_objects(), 0);
    }

    const MISSING: &str = "//! A constant that reaches a native function nothing defines.

import std::io

@external(native, \"app_missing_clock\")
fn clock() -> Int

const start: Int = pick(CALL)

fn pick(now: Bool) -> Int {
  case now {
    True => clock()
    False => 0
  }
}

fn main() {
  io::println(\"{start}\")
}
";

    /// D-532: `cheby check` binds
    /// unresolved native symbols to a stub, so it reports nothing for a
    /// symbol constants only reach, and E0077 for one they call;
    /// `cheby run` reports E0053 either way.
    #[test]
    fn check_stubs_missing_native_symbols() {
        for (call, check_codes) in [("False", Vec::<&str>::new()), ("True", vec!["E0077"])] {
            let dir = package(
                &format!("missing_{call}"),
                "targets = [\"native\"]\n",
                &MISSING.replace("CALL", call),
            );
            let checked = crate::check(&dir).unwrap();
            assert_eq!(codes(&checked), check_codes, "{}", checked.render());
            let RunResult::CompileFailed(out) = crate::run(&dir, RunOptions::default()).unwrap()
            else {
                panic!("a program with a missing native symbol ran");
            };
            assert_eq!(codes(&out), ["E0053"], "{}", out.render());
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// `Roots::AllFunctions` compiles every function and evaluates nothing,
    /// so a constant that would panic is not reported.
    #[test]
    fn all_functions_compile_without_evaluating() {
        let dir = package(
            "all_functions",
            "",
            "//! Every function.\n\nconst broken: Int = 1 / zero()\n\nfn zero() -> Int {\n  0\n}\n\nfn unused(n: Int) -> Int {\n  n + broken\n}\n\nfn main() {\n  Nil\n}\n",
        );
        let (front, out) = front(&dir);
        let Ok(built) = build(&front, &out.sources, Roots::AllFunctions, false).unwrap() else {
            panic!("the functions did not build");
        };
        let local = local_modules(&front);
        let unused = checked(&front, local[0])
            .unwrap()
            .thir
            .fns
            .iter()
            .find(|f| {
                own_defs(&front, f.def.module, &DepView::new(&front.set, &[]))
                    .unwrap()
                    .name(f.def)
                    .unwrap()
                    .item
                    .as_ref()
                    == "unused"
            })
            .map(|f| f.def)
            .unwrap();
        assert!(built.jit.entry(EntryKind::Main(unused)).is_err());
        let broken = checked(&front, local[0]).unwrap().thir.consts[0].def;
        let slot = built.jit.const_slot(broken).unwrap();
        // SAFETY: the slot of a compiled constant.
        assert_eq!(unsafe { *slot }, 0);
        // `cheby check` evaluates it.
        let checked = crate::check(&dir).unwrap();
        assert_eq!(codes(&checked), ["E0075"], "{}", checked.render());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
