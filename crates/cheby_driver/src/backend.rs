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
//! Lowering reports no diagnostics: every construct the front end accepts
//! lowers (D-554), so E0038 remains only for imports of unshipped std
//! modules, which the resolver reports (D-480).
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
//!
//! **MIR and code-generation options** (D-718, D-702): every build reads
//! its switches once, in one place: [`mir_options_from_env`] reads
//! `CHEBY_RC` (`naive` or `perceus`; unset or empty is the default) and
//! `CHEBY_UNWIND_EDGES` (`off` turns the unwind pass off, D-698; unset or
//! empty keeps it), and [`codegen_options`] carries the latter into
//! [`CodegenOptions::unwind_edges`], so the runtime knows code compiled
//! without edges (D-770). A build lowers and checks every unit first,
//! then, unless `CHEBY_RC=naive` (`MirOptions::borrows`), infers the
//! borrow masks across all of them and writes them into each (D-715,
//! D-779, D-787), and only then counts each unit with
//! `cheby_mir::count_lowered_with`. The dumps for goldens use the
//! defaults, so they do not depend on the environment. These are hidden
//! developer switches, honored by release builds too, so `bench-run` can
//! measure every mode (D-702); an invalid value is a driver error (D-746,
//! D-742).
//!
//! **The reuse report** (D-717): [`build_reporting`] with `reuse_report`
//! lowers the package's own modules with [`MirOptions::record_sites`], so
//! their bodies record the source site of each allocation and call in a
//! side table that code generation never reads, and formats the report
//! from the counted units ([`crate::reuse_report`]). Std and dependencies
//! record nothing. The MIR and CLIF are the same with and without it
//! (D-852).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use cheby_codegen_clif::CodegenOptions;
use cheby_consteval::{ConstEvalConfig, ConstInfo, ConstMode, ConstTypes};
use cheby_diag::{Diagnostic, SourceMap, codes};
use cheby_iface::{DefId, DepView, Ice, ModuleId, ReadLog};
use cheby_jit::{Jit, JitError, JitOptions};
use cheby_mir::lower::{lower_items_recording, lower_tests_recording};
use cheby_mir::reach::{Reach, Root, reachable_with_hooks};
use cheby_mir::{
    Defs, EntryKind, EntryRequest, LowerCx, MirModule, MirOptions, Program, RcMode, UnitKind,
    borrow, check_lowered, count_lowered_with, lower_entries,
};
use cheby_package::Target;
use cheby_types::thir;
use rayon::prelude::*;

use crate::Error;
use crate::frontend::{Frontend, hidden_reads};

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
    /// For [`Roots::AllFunctions`], the reference-count operations (`dup`
    /// and `drop` statements) of every MIR unit compiled, for `cargo xtask
    /// bench-compile` (D-718); `None` for the other roots.
    pub rc_ops: Option<u64>,
    /// The text of `--reuse-report` (D-717, [`crate::reuse_report`]),
    /// when the build was asked for it.
    pub reuse_report: Option<String>,
}

/// The MIR options of a build, read from the environment once per build:
/// `CHEBY_RC` (D-746) and `CHEBY_UNWIND_EDGES` (D-742).
///
/// # Errors
///
/// An [`Error`] with the message of an invalid value.
pub(crate) fn mir_options_from_env() -> Result<MirOptions, Error> {
    MirOptions::from_env().map_err(Error::new)
}

/// The code-generation options of a build or a REPL session compiled with
/// the MIR options `mir`: [`CodegenOptions::unwind_edges`] follows
/// `mir.unwind_edges` (`CHEBY_UNWIND_EDGES`, D-742), so the JIT tells the
/// runtime when code has no edges (D-770). A CLIF dump
/// (`capture_clif`) compiles with the default MIR options, so dumps do not
/// depend on the environment (D-264).
pub(crate) fn codegen_options(mir: MirOptions, capture_clif: bool) -> CodegenOptions {
    CodegenOptions {
        capture_clif,
        unwind_edges: mir.unwind_edges,
        ..CodegenOptions::default()
    }
}

/// Lowers what the roots reach (D-510), compiles it in one JIT, and
/// evaluates the reachable constants in that JIT (E6). `Err` holds E0053,
/// E0069 or E0075-E0078 diagnostics, sorted. `Constants` builds
/// with `stub_unresolved` (plan §4.4) and never reports E0053 (D-532);
/// `AllFunctions` evaluates nothing. A CLIF dump (`capture_clif`) compiles
/// with the default options, MIR, code generation and JIT, so dumps do not
/// depend on the environment (D-264); any other build reads the options
/// with [`mir_options_from_env`] and [`codegen_options_from_env`]. With
/// `CHEBY_DUMP_DIR` set, it writes the determinism dumps (module docs).
///
/// # Errors
///
/// An [`Error`] for an invalid option in the environment, or an internal
/// compiler error.
pub(crate) fn build(
    front: &Frontend,
    sources: &SourceMap,
    roots: Roots<'_>,
    capture_clif: bool,
) -> Result<Result<Built, Vec<Diagnostic>>, Error> {
    build_reporting(front, sources, roots, capture_clif, false)
}

/// [`build`], and with `reuse_report` also the text of `--reuse-report`
/// in [`Built::reuse_report`] (D-717): lowering then records the sites of
/// the package's own modules ([`MirOptions::record_sites`]), which changes
/// no code (D-852). Under `CHEBY_RC=naive` nothing is
/// recorded and the report is its one naive line.
///
/// # Errors
///
/// As [`build`].
#[expect(
    clippy::needless_pass_by_value,
    reason = "the signature of plan §4.5, which callers build in place"
)]
pub(crate) fn build_reporting(
    front: &Frontend,
    sources: &SourceMap,
    roots: Roots<'_>,
    capture_clif: bool,
    reuse_report: bool,
) -> Result<Result<Built, Vec<Diagnostic>>, Error> {
    let mut options = if capture_clif {
        MirOptions::default()
    } else {
        mir_options_from_env()?
    };
    options.record_sites = reuse_report && options.rc == RcMode::Perceus;
    let codegen = codegen_options(options, capture_clif);
    Ok(build_dumping(
        front,
        sources,
        &roots,
        codegen,
        options,
        dump_dir().as_deref(),
        reuse_report,
    )?)
}

/// [`build`] with the MIR options `mir` and the code-generation options
/// `codegen`, writing the determinism dumps to `dump` if it is given, and
/// the reuse report if `reuse_report` asks for it.
fn build_dumping(
    front: &Frontend,
    sources: &SourceMap,
    roots: &Roots<'_>,
    codegen: CodegenOptions,
    mir: MirOptions,
    dump: Option<&Path>,
    reuse_report: bool,
) -> Result<Result<Built, Vec<Diagnostic>>, Ice> {
    let capture_clif = codegen.capture_clif;
    let lowered = lower(front, sources, roots, mir)?;
    if let Some(dir) = dump {
        write_mir_dumps(dir, &lowered.units)?;
    }
    let rc_ops = matches!(roots, Roots::AllFunctions).then(|| rc_ops(&lowered.units));
    let reuse_report = if reuse_report {
        let local: BTreeSet<ModuleId> = local_modules(front).into_iter().collect();
        Some(crate::reuse_report::report(
            &lowered.units,
            &local,
            sources,
            &front.globals.names,
            &crate::reuse_report::Types {
                table: &front.globals.types,
                interfaces: &front.set,
            },
            mir.rc == RcMode::Naive,
        )?)
    } else {
        None
    };
    let check = matches!(roots, Roots::Constants);
    let options = JitOptions {
        stub_unresolved: check,
        ..jit_options(capture_clif)
    };
    // The dump captures CLIF too; code generation is the same either way.
    let codegen = CodegenOptions {
        capture_clif: capture_clif || dump.is_some(),
        ..codegen
    };
    let mut jit = match compile_with(&lowered.units, codegen, options) {
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
        // D-1123: constant evaluation reads the
        // types of the constants it evaluates directly, outside any view;
        // constants are cached under their own keys (D-964, `cache.md` §6).
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
    Ok(Ok(Built {
        jit,
        rc_ops,
        reuse_report,
    }))
}

/// D-759's RC operations of every function of `units`: `dup`, `drop` and
/// `drop_reuse` outside unwind pads ([`cheby_mir::stats::rc_ops`]).
/// D-843: until checkpoint 4 this counted the pads' drops
/// too (`.m2-work/slice4/P2/rc-column.md`).
fn rc_ops(units: &[MirModule]) -> u64 {
    let count: usize = units
        .iter()
        .flat_map(|unit| &unit.functions)
        .map(cheby_mir::stats::rc_ops)
        .sum();
    u64::try_from(count).unwrap_or(u64::MAX)
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

/// One E0053 at the `@external` that names each unresolved symbol (D-532),
/// for `cheby run` and, with D-943's build resolver, `cheby build`
/// (`crate::aot`).
pub(crate) fn unresolved(front: &Frontend, symbols: &[String]) -> Result<Vec<Diagnostic>, Ice> {
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
pub(crate) struct Lowered {
    pub units: Vec<MirModule>,
    pub reach: Reach,
    /// Per module outside std that this build lowered: the hidden reads
    /// of its units in lowering and borrow inference (D-955). The cache's
    /// code and tests identities read them from wave 2 on (K3a).
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "D-955's lowering read sets; the cache keys read them from K3a on, tests today"
        )
    )]
    pub reads: BTreeMap<ModuleId, UnitReads>,
}

/// The hidden reads of one module's units in lowering and borrow
/// inference (D-955, `cache.md` §3.3's `Rc` and `Rt`): the modules their
/// recording views handed out that are neither the module's direct
/// imports, nor its front end's hidden reads ([`Checked::front_reads`]),
/// nor std modules, nor the builtin module; sorted by id. `None` for a
/// unit the build did not lower.
///
/// [`Checked::front_reads`]: crate::frontend::Checked::front_reads
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct UnitReads {
    /// The code unit's: lowering and borrow inference.
    pub code: Option<Vec<ModuleId>>,
    /// The tests unit's: lowering (tests units get no masks of their
    /// own).
    pub tests: Option<Vec<ModuleId>>,
}

/// The logs of one module's recording views while it is lowered.
#[derive(Default)]
struct UnitLogs {
    code: ReadLog,
    tests: ReadLog,
}

/// The MIR units that running `main` needs, after reference counting and
/// validation: one per needed module in build order, then the entry unit,
/// which also holds the thunks of the constants `main` reaches. A dump for
/// goldens, so it lowers with the default MIR options, whatever the
/// environment says (D-264).
pub(crate) fn lower_program(
    front: &Frontend,
    sources: &SourceMap,
    main: &thir::FnDef,
) -> Result<Vec<MirModule>, Ice> {
    Ok(lower(front, sources, &Roots::Main(main), MirOptions::default())?.units)
}

/// Lowers what `roots` reach, counting and checking each unit with the
/// MIR options `mir`. Each unit of a module outside std reads other
/// modules through a recording view, and [`Lowered::reads`] holds what
/// it read beyond its imports and its front end's reads (D-955).
pub(crate) fn lower(
    front: &Frontend,
    sources: &SourceMap,
    roots: &Roots<'_>,
    mir: MirOptions,
) -> Result<Lowered, Ice> {
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
    // Lowering a module reads only its THIR and the interfaces (D-246), so
    // the modules are lowered and checked in parallel on rayon's global
    // pool (G1, D-263). The results are taken in build order, so the units
    // and the error reported do not depend on the thread count (D-264).
    let jobs: Vec<(ModuleId, Option<&Vec<&thir::TestDef>>)> = front
        .order
        .iter()
        .map(|&module| (module, tests.get(&module)))
        .filter(|&(module, selected)| reach.needs_module(module) || selected.is_some())
        .collect();
    let logs = read_logs(front, &jobs);
    // Only the package's own modules record their sites for the reuse
    // report (D-717); std and dependencies never do.
    let lowered: Vec<Result<ModuleUnits, Ice>> = jobs
        .par_iter()
        .map(|&(module, selected)| {
            let record_sites = mir.record_sites && local.contains(&module);
            lower_module(
                front,
                sources,
                &reach,
                module,
                selected,
                record_sites,
                logs.get(&module),
            )
        })
        .collect();
    let mut units = Vec::new();
    for result in lowered {
        for unit in result?.into_iter().flatten() {
            units.push(unit?);
        }
    }
    if !entries.is_empty() {
        units.push(lower_entry_unit(front, sources, &entries)?);
    }
    // Every unit is lowered before any is counted: the borrow masks come
    // from the whole build (D-779).
    if mir.borrows() {
        infer_borrowing(front, &mut units, &logs)?;
    }
    let counted: Vec<Result<(), Ice>> = units
        .par_iter_mut()
        .map(|unit| count_lowered_with(unit, &mir))
        .collect();
    for result in counted {
        result?;
    }
    let reads = unit_reads(front, &jobs, &reach, &logs)?;
    Ok(Lowered {
        units,
        reach,
        reads,
    })
}

/// What `main` reaches over the program [`lower`] walks, without D-482's
/// hook roots: `cheby build` reads it to tell the code that only a type's
/// `@std_hooks` bring into a build (GAP(aot-layouts-not-yet)).
///
/// # Errors
///
/// An [`Ice`] if a module of the build order was not checked, or a body
/// refers to an expression it does not have.
pub(crate) fn reach_without_hooks(front: &Frontend, main: &thir::FnDef) -> Result<Reach, Ice> {
    let mut program = Program::new();
    for &module in &front.order {
        program.insert(module, &checked(front, module)?.thir);
    }
    cheby_mir::reach::reachable(&program, &[Root::Fn(main)])
}

/// An empty read log per module of `jobs` outside std (D-955); std
/// records nothing, since the build ID fixes it.
fn read_logs(
    front: &Frontend,
    jobs: &[(ModuleId, Option<&Vec<&thir::TestDef>>)],
) -> BTreeMap<ModuleId, UnitLogs> {
    jobs.iter()
        .filter(|&&(module, _)| {
            front
                .modules
                .get(module)
                .is_some_and(|info| info.package != cheby_iface::PackageId::STD)
        })
        .map(|&(module, _)| (module, UnitLogs::default()))
        .collect()
}

/// The hidden reads of each lowered unit of the modules in `logs`
/// (D-955): what its views handed out beyond the module's imports and
/// its front end's hidden reads.
fn unit_reads(
    front: &Frontend,
    jobs: &[(ModuleId, Option<&Vec<&thir::TestDef>>)],
    reach: &Reach,
    logs: &BTreeMap<ModuleId, UnitLogs>,
) -> Result<BTreeMap<ModuleId, UnitReads>, Ice> {
    let mut reads = BTreeMap::new();
    for &(module, selected) in jobs {
        let Some(log) = logs.get(&module) else {
            continue;
        };
        let checked = checked(front, module)?;
        let imports = front
            .import_targets
            .get(usize::try_from(module.as_u32()).map_err(|_| Ice::new("module id overflow"))?)
            .map_or(&[][..], Vec::as_slice);
        let hidden = |log: &ReadLog| {
            hidden_reads(
                &front.modules,
                module,
                log.modules(),
                imports,
                &checked.front_reads,
            )
        };
        reads.insert(
            module,
            UnitReads {
                code: reach.needs_module(module).then(|| hidden(&log.code)),
                tests: selected.map(|_| hidden(&log.tests)),
            },
        );
    }
    Ok(reads)
}

/// Borrowing inference across a build's lowered units (D-715, D-779,
/// D-787): the facts of every unit, the masks of each code unit's module
/// from its interfaces and THIR (in parallel, each module on its own), and
/// then the whole build's masks written into every unit, its tests units
/// and the entries unit included ([`cheby_mir::borrow::apply`]). A
/// `cheby test` build infers over its tests units too, so its code units
/// may get other masks than a `cheby run` build's; `cheby run`'s code
/// never depends on tests (D-779).
fn infer_borrowing(
    front: &Frontend,
    units: &mut [MirModule],
    logs: &BTreeMap<ModuleId, UnitLogs>,
) -> Result<(), Ice> {
    let facts = {
        let refs: Vec<&MirModule> = units.iter().collect();
        borrow::collect_facts(&refs)
    };
    let found: Vec<Result<borrow::Masks, Ice>> = units
        .par_iter()
        .map(|unit| {
            let log = match unit.unit {
                UnitKind::Code(module) => logs.get(&module).map(|l| &l.code),
                _ => None,
            };
            unit_masks(front, unit, &facts, log)
        })
        .collect();
    let mut masks = borrow::Masks::new();
    for result in found {
        masks.extend(result?);
    }
    let applied: Vec<Result<(), Ice>> = units
        .par_iter_mut()
        .map(|unit| borrow::apply(unit, &masks))
        .collect();
    applied.into_iter().collect()
}

/// The masks [`borrow::infer`] gives `unit` if it is a code unit, read
/// with its module's interfaces (D-246) and THIR, through a view that
/// records into `log` when one is given (D-955).
fn unit_masks(
    front: &Frontend,
    unit: &MirModule,
    facts: &borrow::BuildFacts,
    log: Option<&ReadLog>,
) -> Result<borrow::Masks, Ice> {
    let UnitKind::Code(module) = unit.unit else {
        return Ok(borrow::Masks::new());
    };
    let checked = checked(front, module)?;
    let readable = view_of(front, &checked.deps, log);
    let eligibility =
        borrow::IfaceEligibility::new(own_defs(front, module, &readable)?, &checked.thir);
    Ok(borrow::infer(unit, facts, &eligibility))
}

/// The view of `deps` in `front`'s interfaces, recording into `log` if
/// one is given (D-955).
fn view_of<'a>(front: &'a Frontend, deps: &'a [ModuleId], log: Option<&'a ReadLog>) -> DepView<'a> {
    match log {
        Some(log) => DepView::recording(&front.set, deps, log),
        None => DepView::new(&front.set, deps),
    }
}

/// The code unit and the tests unit of one module, each lowered and
/// checked; `None` where the build needs no such unit.
type ModuleUnits = [Option<Result<MirModule, Ice>>; 2];

/// Lowers `module`: its items that `reach` contains into its code unit,
/// and its `selected` tests into its tests unit, each checked as lowered
/// ([`check_lowered`]); counting waits for the whole build (D-779). With
/// `record_sites`, both units record the sites of `--reuse-report`
/// (D-717). With `logs`, each unit reads other modules through a view
/// that records into its own log (D-955).
fn lower_module(
    front: &Frontend,
    sources: &SourceMap,
    reach: &Reach,
    module: ModuleId,
    selected: Option<&Vec<&thir::TestDef>>,
    record_sites: bool,
    logs: Option<&UnitLogs>,
) -> Result<ModuleUnits, Ice> {
    let checked = checked(front, module)?;
    let own = front
        .set
        .get(module)
        .ok_or_else(|| Ice::new(format!("module {module:?} has no interface")))?;
    let code_deps = view_of(front, &checked.deps, logs.map(|l| &l.code));
    let tests_deps = view_of(front, &checked.deps, logs.map(|l| &l.tests));
    let cx_with = |deps| LowerCx {
        defs: Defs {
            own: Some(own),
            deps,
            modules: &front.modules,
            globals: &front.globals,
        },
        sources,
    };
    let cx = cx_with(&code_deps);
    let tests_cx = cx_with(&tests_deps);
    let counted = |lowered: Result<MirModule, Ice>| {
        lowered.and_then(|unit| {
            check_lowered(&unit)?;
            Ok(unit)
        })
    };
    let code = reach.needs_module(module).then(|| {
        counted(lower_items_recording(
            &checked.thir,
            &|def| reach.contains(def),
            &cx,
            record_sites,
        ))
    });
    let tests = selected.map(|selected| {
        // Only the selected tests: the others may reach items this build
        // does not lower.
        let only = thir::Module {
            module,
            fns: Vec::new(),
            consts: Vec::new(),
            tests: selected.iter().map(|&t| t.clone()).collect(),
        };
        counted(lower_tests_recording(&only, &tests_cx, record_sites))
    });
    Ok([code, tests])
}

/// Every unit a build of the local package can lower, each counted and
/// checked: what `main` reaches (if given), every local test that exists
/// on native, and every local constant (D-510). Lowering reports no
/// diagnostics (D-554), so the only failure is an internal error.
#[cfg(test)]
pub(crate) fn lower_every_root(
    front: &Frontend,
    sources: &SourceMap,
    main: Option<&thir::FnDef>,
) -> Result<Vec<MirModule>, Ice> {
    lower_every_root_with(front, sources, main, MirOptions::default())
}

/// [`lower_every_root`] with the MIR options `mir`.
#[cfg(test)]
pub(crate) fn lower_every_root_with(
    front: &Frontend,
    sources: &SourceMap,
    main: Option<&thir::FnDef>,
    mir: MirOptions,
) -> Result<Vec<MirModule>, Ice> {
    let mut units = Vec::new();
    if let Some(main) = main {
        units.extend(lower(front, sources, &Roots::Main(main), mir)?.units);
    }
    let mut tests = Vec::new();
    for module in local_modules(front) {
        for test in &checked(front, module)?.thir.tests {
            if crate::test::on_native(front, test) {
                tests.push(TestRoot { module, test });
            }
        }
    }
    if !tests.is_empty() {
        units.extend(lower(front, sources, &Roots::Tests(tests), mir)?.units);
    }
    units.extend(lower(front, sources, &Roots::Constants, mir)?.units);
    Ok(units)
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

/// The entry unit with the thunks of `entries` (D-137, D-453), checked as
/// lowered; counting waits for the whole build (D-779).
fn lower_entry_unit(
    front: &Frontend,
    sources: &SourceMap,
    entries: &[EntryRequest<'_>],
) -> Result<MirModule, Ice> {
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
    let unit = lower_entries(entries, &cx)?;
    check_lowered(&unit)?;
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
/// a source file. The hash computed when the file was read, or when
/// `cheby` was built for std (D-963), is used when there is one.
fn module_source_hash(front: &Frontend, sources: &SourceMap, module: ModuleId) -> Result<u64, Ice> {
    if let Some(hashes) = front.source_hashes(module) {
        return Ok(hashes.sip64);
    }
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
    codegen: CodegenOptions,
    options: JitOptions,
) -> Result<Jit, JitError> {
    let mut jit = Jit::configured(codegen, options)?;
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

    /// D-1130: constant seeds read the 64-bit hashes
    /// computed when the files were read (package modules) or when `cheby`
    /// was built (std), and each equals the seed computed as before from
    /// the source map's texts (D-538), for a constant that reaches a
    /// package module and std modules.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn front_source_hash_seed_unchanged() {
        let dir = package(
            "seed",
            "",
            "//! Root.\n\nimport app::util\n\nfn main() {\n  assert util::size == 2\n}\n",
        );
        std::fs::write(
            dir.join("src/util.cheby"),
            "//! Util.\n\nimport std::map\n\n\
             pub const table: map::Map<Int, Int> = map::insert(map::insert(map::new(), 1, 2), 3, 4)\n\n\
             pub const size: Int = map::size(table)\n",
        )
        .unwrap();
        let (front, out) = front(&dir);
        let module = front
            .local_module(&cheby_package::ModulePath::root("app"))
            .unwrap();
        let main = front.main_of(module).unwrap();
        let lowered = lower(
            &front,
            &out.sources,
            &Roots::Main(main),
            MirOptions::default(),
        )
        .unwrap();
        let infos = const_infos(&front, &out.sources, &lowered.reach).unwrap();
        assert_eq!(infos.len(), 2);
        let const_modules = lowered.reach.const_modules();
        for info in &infos {
            let modules = const_modules.get(&info.def).unwrap();
            let mut deps: Vec<(String, u64)> = modules
                .iter()
                .map(|&m| {
                    let module = front.modules.get(m).unwrap();
                    let text = out.sources.file(module.file.unwrap()).unwrap().text();
                    (
                        module.path.to_string(),
                        cheby_runtime::consteval::source_hash(text.as_bytes()),
                    )
                })
                .collect();
            deps.sort();
            assert!(deps.iter().any(|(p, _)| p == "app::util"), "{deps:?}");
            assert!(deps.iter().any(|(p, _)| p.starts_with("std::")), "{deps:?}");
            let deps: Vec<(&str, u64)> = deps.iter().map(|(p, h)| (p.as_str(), *h)).collect();
            let seed = cheby_runtime::consteval::const_seed("app", "app::util", &info.name, &deps);
            assert_eq!(info.seed, seed, "{}", info.name);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Plan §7.8: whichever constant forces `table` first, `table` is
    /// built under its own seed (D-535, D-538), so its key order, which
    /// `keys` records, is the same.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn a_constant_has_one_value_whichever_constant_forces_it() {
        let dir = package("forcing", "", FORCING);
        let (front, out) = front(&dir);
        let module = front
            .local_module(&cheby_package::ModulePath::root("app"))
            .unwrap();
        let main = front.main_of(module).unwrap();
        let lowered = lower(
            &front,
            &out.sources,
            &Roots::Main(main),
            MirOptions::default(),
        )
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
            let mut jit = compile_with(
                &lowered.units,
                CodegenOptions::default(),
                JitOptions::from_env(),
            )
            .unwrap();
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
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
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
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
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
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
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
            let Ok(built) = build_dumping(
                &front,
                &out.sources,
                &roots,
                CodegenOptions::default(),
                MirOptions::default(),
                None,
                false,
            )
            .unwrap() else {
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
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn the_dump_hook_writes_mir_clif_and_constants() {
        let dir = package("dumps", "", KEYS);
        let dumps = dir.join("dumps");
        let (front, out) = front(&dir);
        let Ok(_) = build_dumping(
            &front,
            &out.sources,
            &Roots::Constants,
            CodegenOptions::default(),
            MirOptions::default(),
            Some(&dumps),
            false,
        )
        .unwrap() else {
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
        let Ok(_) = build_dumping(
            &front,
            &out.sources,
            &Roots::Constants,
            CodegenOptions::default(),
            MirOptions::default(),
            Some(&dumps),
            false,
        )
        .unwrap() else {
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
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
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
        let Ok(built) = build_dumping(
            &front,
            &out.sources,
            &Roots::Constants,
            CodegenOptions::default(),
            MirOptions::default(),
            None,
            false,
        )
        .unwrap() else {
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
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
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
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
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

    /// `bench_build` reports the size of what it built (D-718): the bytes
    /// of machine code the JIT defined and the `dup` and `drop` statements
    /// of the MIR, the same in two builds.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn bench_build_reports_code_bytes_and_rc_ops() {
        let dir = package("bench_size", "", KEYS);
        let first = crate::bench_build(&dir).unwrap();
        let second = crate::bench_build(&dir).unwrap();
        assert!(first.code_bytes > 0, "{first:?}");
        assert!(first.rc_ops > 0, "{first:?}");
        assert_eq!(
            (first.code_bytes, first.rc_ops),
            (second.code_bytes, second.rc_ops)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The MIR units of every root of the package at `dir` (its `main`,
    /// its tests on native, its constants) under `mir`, or the sorted
    /// codes of the front end's errors. An internal error fails the test.
    #[expect(
        clippy::panic_in_result_fn,
        reason = "`Err` is a rejection by the front end; an internal error fails the test"
    )]
    fn every_root_units(dir: &Path, mir: MirOptions) -> Result<Vec<MirModule>, String> {
        let mut out = CheckOutput::default();
        let front = crate::front_end(dir, frontend::Mode::Build, &mut out).unwrap();
        let Some((front, package)) = front.filter(|_| !out.has_errors()) else {
            let mut found = codes(&out);
            found.sort();
            found.dedup();
            return Err(found.join(", "));
        };
        let main = front
            .local_module(&cheby_package::ModulePath::root(&package))
            .and_then(|m| front.main_of(m));
        let units = lower_every_root_with(&front, &out.sources, main, mir)
            .unwrap_or_else(|ice| panic!("{}: {ice}", dir.display()));
        assert!(!units.is_empty(), "{}", dir.display());
        Ok(units)
    }

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

    /// The MIR dump of every unit of `units`, and their CLIF compiled with
    /// the default options, native symbols nothing defines stubbed (020's
    /// C library, plan §4.4).
    fn dumps(units: &[MirModule]) -> (Vec<String>, Vec<String>) {
        let mir = units.iter().map(|u| cheby_mir::dump(u).unwrap()).collect();
        let codegen = CodegenOptions {
            capture_clif: true,
            ..CodegenOptions::default()
        };
        let options = JitOptions {
            stub_unresolved: true,
            ..JitOptions::default()
        };
        let mut jit = compile_with(units, codegen, options).unwrap();
        (mir, jit.take_clif())
    }

    /// D-717, D-852: `--reuse-report` changes no code.
    /// For every example the front end accepts, the build of its `main`
    /// and the build of its tests on native lower to byte-identical MIR
    /// dumps and compile to byte-identical CLIF with and without
    /// [`MirOptions::record_sites`]. With it, the package's own units
    /// record sites and std's never do; without it, no unit records any.
    /// The report of every build with sites is made without an error, and
    /// some example reports a reuse and a kernel line.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn reuse_report_sites_change_no_dump() {
        let recording = MirOptions {
            record_sites: true,
            ..MirOptions::default()
        };
        let (mut builds, mut reports, mut skipped) = (0_usize, String::new(), Vec::new());
        let mut with_sites = 0_usize;
        for dir in example_dirs() {
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            let mut out = CheckOutput::default();
            let front = crate::front_end(&dir, frontend::Mode::Build, &mut out).unwrap();
            let Some((front, package)) = front.filter(|_| !out.has_errors()) else {
                skipped.push(name);
                continue;
            };
            let local: BTreeSet<ModuleId> = local_modules(&front).into_iter().collect();
            let main = front
                .local_module(&cheby_package::ModulePath::root(&package))
                .and_then(|m| front.main_of(m));
            let mut tests = Vec::new();
            for &module in &local {
                for test in &checked(&front, module).unwrap().thir.tests {
                    if crate::test::on_native(&front, test) {
                        tests.push(TestRoot { module, test });
                    }
                }
            }
            let mut roots = Vec::new();
            if let Some(main) = main {
                roots.push(Roots::Main(main));
            }
            if !tests.is_empty() {
                roots.push(Roots::Tests(tests));
            }
            for roots in &roots {
                let plain = lower(&front, &out.sources, roots, MirOptions::default())
                    .unwrap()
                    .units;
                let sited = lower(&front, &out.sources, roots, recording).unwrap().units;
                assert!(
                    plain
                        .iter()
                        .flat_map(|u| &u.functions)
                        .all(|f| f.sites.is_empty()),
                    "{name}: sites recorded without the flag"
                );
                for unit in &sited {
                    let own = unit.module().is_some_and(|m| local.contains(&m));
                    assert!(
                        own || unit.functions.iter().all(|f| f.sites.is_empty()),
                        "{name}: {} records sites",
                        unit.path
                    );
                }
                if sited
                    .iter()
                    .flat_map(|u| &u.functions)
                    .any(|f| !f.sites.is_empty())
                {
                    with_sites += 1;
                }
                let (plain_mir, plain_clif) = dumps(&plain);
                let (sited_mir, sited_clif) = dumps(&sited);
                assert!(!plain_clif.is_empty(), "{name}: no CLIF captured");
                assert!(plain_mir == sited_mir, "{name}: the MIR dumps differ");
                assert!(plain_clif == sited_clif, "{name}: the CLIF differs");
                let report = crate::reuse_report::report(
                    &sited,
                    &local,
                    &out.sources,
                    &front.globals.names,
                    &crate::reuse_report::Types {
                        table: &front.globals.types,
                        interfaces: &front.set,
                    },
                    false,
                )
                .unwrap();
                assert!(
                    report.lines().all(|l| l.starts_with("reuse: src/")),
                    "{name}: {report}"
                );
                reports.push_str(&report);
                builds += 1;
            }
        }
        println!(
            "reuse report: {builds} builds compared, {with_sites} with sites, skipped: {}",
            skipped.join(", ")
        );
        assert!(builds >= 40, "only {builds} builds compared");
        assert!(
            with_sites >= builds.saturating_sub(5),
            "only {with_sites} of {builds} builds record a site"
        );
        assert!(reports.contains("when it is unique\n"), "no reuse reported");
        assert!(
            reports.contains("at its last use: in place"),
            "no kernel line reported"
        );
    }

    /// Under naive counting nothing is reused, so the report is its one
    /// line, and no site is recorded (D-746, D-852).
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn reuse_report_under_naive_counting_is_one_line() {
        let dir = package("reuse_naive", "", KEYS);
        let (front, out) = front(&dir);
        let main = front
            .local_module(&cheby_package::ModulePath::root("app"))
            .and_then(|m| front.main_of(m))
            .unwrap();
        let naive = MirOptions {
            rc: RcMode::Naive,
            ..MirOptions::default()
        };
        let perceus = MirOptions {
            record_sites: true,
            ..MirOptions::default()
        };
        for (mir, expected) in [
            (naive, Some(crate::reuse_report::NAIVE_LINE)),
            (perceus, None),
        ] {
            let Ok(built) = build_dumping(
                &front,
                &out.sources,
                &Roots::Main(main),
                CodegenOptions::default(),
                mir,
                None,
                true,
            )
            .unwrap() else {
                panic!("the package did not build");
            };
            let report = built.reuse_report.unwrap();
            match expected {
                Some(line) => assert_eq!(report, line),
                None => assert!(
                    report.starts_with("reuse: src/main.cheby:") && !report.contains("is off"),
                    "{report}"
                ),
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// D-698, plan §2.4 and gate 2. Rules checked: with the unwind pass on
    /// (the default), every unit of every example whose front end passes
    /// (its `main`, its tests on native and its constants) passes
    /// `Stage::Unwound`: every instruction that may unwind has an edge to
    /// the pad of exactly what it leaves held, nothing else has one, pads
    /// end in `resume` with nothing held, and every Cheby-ABI function has
    /// its entry pad. With `CHEBY_UNWIND_EDGES=off` the pass does not run:
    /// no function that is not `precounted` has an edge, an entry pad or a
    /// `resume` (a precounted intrinsic body keeps the edges it was
    /// generated with, which code generation under `off` ignores). The
    /// examples the front end rejects are computed, skipped and printed.
    #[test]
    fn unwound_examples() {
        let dirs = example_dirs();
        let off = MirOptions {
            unwind_edges: false,
            ..MirOptions::default()
        };
        let mut checked = 0_usize;
        let mut edges = 0_usize;
        let mut skipped = Vec::new();
        for dir in &dirs {
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            let units = match every_root_units(dir, MirOptions::default()) {
                Ok(units) => units,
                Err(codes) => {
                    skipped.push(format!("{name} (the front end reports {codes})"));
                    continue;
                }
            };
            for unit in &units {
                if let Err(errors) = cheby_mir::validate(unit, cheby_mir::Stage::Unwound) {
                    panic!("{name}: {}: {}", unit.path, errors.join("\n  "));
                }
                edges += unit
                    .functions
                    .iter()
                    .flat_map(|f| f.blocks.iter())
                    .map(|b| b.unwind.len())
                    .sum::<usize>();
            }
            let Ok(units) = every_root_units(dir, off) else {
                panic!("{name}: the front end rejects it the second time");
            };
            for unit in &units {
                for f in unit.functions.iter().filter(|f| !f.precounted) {
                    let text = cheby_mir::dump_function(unit, f).unwrap();
                    // The string table may quote any text.
                    let code = text
                        .lines()
                        .filter(|line| !line.trim_start().starts_with("str#"));
                    let mut found = code.filter(|l| l.contains("unwind") || l.contains("resume"));
                    assert!(
                        found.next().is_none()
                            && f.entry_unwind.is_none()
                            && f.blocks.iter().all(|b| b.unwind.is_empty()),
                        "{name}: edges with CHEBY_UNWIND_EDGES=off:\n{text}"
                    );
                }
            }
            checked += 1;
        }
        eprintln!(
            "unwound_examples: {checked} examples checked ({edges} unwind edges), {} skipped:",
            skipped.len()
        );
        for line in &skipped {
            eprintln!("  {line}");
        }
        assert!(edges > 0);
        assert!(
            checked * 2 > dirs.len(),
            "only {checked} of {} examples checked",
            dirs.len()
        );
    }

    /// `CHEBY_RC=naive` keeps M1's naive counting selectable (D-718) as the
    /// oracle of borrowing (D-715) and reuse (D-716): for every example
    /// that passes the front end, its `main`, its tests and its constants
    /// lower in both modes to MIR that validates after the unwind pass,
    /// naive MIR is the MIR before borrowing and reuse (Perceus with its
    /// passes off, [`MirOptions::perceus_passes_off`]; D-808,
    /// D-838), and no naive unit borrows or reuses
    /// anything. Perceus borrows in some example and reuses in some
    /// example. The examples the front end rejects today are skipped and
    /// printed with their codes.
    #[test]
    fn rc_modes_validate_and_naive_mir_is_the_mir_before_borrowing() {
        let dirs = example_dirs();
        let naive = MirOptions {
            rc: cheby_mir::RcMode::Naive,
            ..MirOptions::default()
        };
        let before_borrowing = MirOptions::default().perceus_passes_off();
        let mut compared = 0_usize;
        let mut borrowing = Vec::new();
        let mut reusing = Vec::new();
        let mut skipped = Vec::new();
        let check = |name: &str, units: &[MirModule]| {
            for unit in units {
                if let Err(errors) = cheby_mir::validate(unit, cheby_mir::Stage::Unwound) {
                    panic!("{name}: {}: {}", unit.path, errors.join("\n  "));
                }
            }
        };
        for dir in &dirs {
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            let naive_units = match every_root_units(dir, naive) {
                Ok(units) => units,
                Err(codes) => {
                    skipped.push(format!("{name} (the front end reports {codes})"));
                    continue;
                }
            };
            let Ok(perceus_units) = every_root_units(dir, MirOptions::default()) else {
                panic!("{name}: the front end rejects it the second time");
            };
            let Ok(owned_units) = every_root_units(dir, before_borrowing) else {
                panic!("{name}: the front end rejects it the third time");
            };
            check(&name, &naive_units);
            check(&name, &perceus_units);
            assert!(
                !naive_units.iter().any(cheby_mir::borrow::borrows_anything),
                "{name}: naive MIR borrows"
            );
            let dump = |units: &[MirModule]| -> Vec<String> {
                units.iter().map(|u| cheby_mir::dump(u).unwrap()).collect()
            };
            let naive_dump = dump(&naive_units);
            assert!(
                naive_dump == dump(&owned_units),
                "{name}: naive MIR differs from the MIR before borrowing and reuse"
            );
            assert!(
                !naive_dump.iter().any(|text| text.contains("drop_reuse")),
                "{name}: naive MIR reuses"
            );
            if perceus_units
                .iter()
                .any(cheby_mir::borrow::borrows_anything)
            {
                borrowing.push(name.clone());
            }
            if dump(&perceus_units)
                .iter()
                .any(|text| text.contains("drop_reuse"))
            {
                reusing.push(name);
            }
            compared += 1;
        }
        eprintln!(
            "rc_modes: {compared} examples compared, {} with borrowing, {} with reuse, {} skipped:",
            borrowing.len(),
            reusing.len(),
            skipped.len()
        );
        for line in &skipped {
            eprintln!("  {line}");
        }
        assert!(
            compared * 2 > dirs.len(),
            "only {compared} of {} examples compared",
            dirs.len()
        );
        assert!(!borrowing.is_empty(), "no example borrows anything");
        assert!(!reusing.is_empty(), "no example reuses anything");
    }

    /// D-742: `CHEBY_UNWIND_EDGES` is the one unwinding switch, and code
    /// generation carries the MIR's setting into
    /// [`CodegenOptions::unwind_edges`] (D-770); a CLIF dump
    /// compiles with the defaults.
    #[test]
    fn unwind_edges_switch_reaches_the_codegen_options() {
        // The two crates name the edges switch alike; the MIR crate does
        // not depend on the runtime.
        assert_eq!(
            MirOptions::UNWIND_EDGES_ENV,
            cheby_runtime::abi::UNWIND_EDGES_ENV
        );
        assert_eq!(
            codegen_options(MirOptions::default(), true),
            CodegenOptions {
                capture_clif: true,
                ..CodegenOptions::default()
            }
        );
        assert!(codegen_options(MirOptions::default(), false).unwind_edges);
        let off = MirOptions {
            unwind_edges: false,
            ..MirOptions::default()
        };
        assert_eq!(
            codegen_options(off, false),
            CodegenOptions {
                unwind_edges: false,
                ..CodegenOptions::default()
            }
        );
    }
}
