//! MIR lowering, code generation, constant evaluation and running (G3,
//! E6).
//!
//! A build starts from its roots (D-510, D-534): the `main` being run, the
//! selected tests, or every constant of the package for `cheby check`.
//! Reach walks the modules' reference summaries ([`crate::load_set`],
//! D-967) to the items the roots need, across modules, with the modules
//! whose constructors reachable code uses and the `@std_hooks` functions
//! of their types (D-482, D-585). Every module of the load set (D-922)
//! is then lowered whole (D-966): every function and constant initializer
//! into its code unit, in `DefIndex` order, and every test into its tests
//! unit when one of them is selected (D-970); the entry thunks, of the
//! selected tests and of every reachable constant, form a separate unit
//! (D-453). Reach selects the constants that are evaluated and the native
//! symbols E0053 covers (D-943); a symbol only unreached functions name
//! binds to `cheby_rt_unresolved`. The JIT compiles all units in one
//! batch, then evaluates the reachable constants in that JIT (D-535), so
//! code that runs later reads their values from their slots.
//!
//! The dumps for goldens (`dump_mir`, `dump_clif`) keep D-510's view:
//! they lower only the reachable items, found by `cheby_mir::reach`'s walk
//! over THIR, with their borrow masks inferred over those units alone
//! (D-1175); `CHEBY_DUMP_DIR` dumps what the build
//! compiled.
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
use cheby_consteval::{ConstEvalConfig, ConstImage, ConstInfo, ConstMode, ConstTypes};
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
use crate::cache::code_keys::UnitPart;
use crate::frontend::{Frontend, hidden_reads};
use crate::load_set::{Start, Summaries};

/// The environment variable naming the directory of the determinism dumps
/// (plan §4.5).
const DUMP_DIR_VAR: &str = "CHEBY_DUMP_DIR";

/// What a build lowers (D-966, D-510).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    /// D-510's view: only the items the roots reach, found by walking
    /// THIR, with their masks inferred over those units alone; the dumps
    /// for goldens (D-1175).
    Reached,
    /// Whole modules (D-966): every module of the load set, found by
    /// walking summaries (D-967, D-922), with all its items, and every
    /// test of a module with a selected one (D-970).
    Whole,
}

/// Whether `cheby run`, `cheby test`, `cheby build`, `cheby check` and the
/// REPL's std lower whole modules (D-966, D-971): on since K4a's
/// switch-over; off, they keep D-510's view, as the dumps for goldens do.
const WHOLE_MODULES: bool = true;

/// The scope of a build that is not a dump for goldens.
pub(crate) const fn command_scope() -> Scope {
    if WHOLE_MODULES {
        Scope::Whole
    } else {
        Scope::Reached
    }
}

/// What a build starts from.
pub(crate) enum Roots<'a> {
    /// A `main` to run (`cheby run`, the dumps).
    Main(&'a thir::FnDef),
    /// The tests to run (`cheby test`, plan §8.1).
    Tests(Vec<TestRoot<'a>>),
    /// Every constant of the local package (`cheby check`, plan §7.1,
    /// D-534): its build is the evaluation module (D-978).
    Constants,
    /// The constants a build of `main` or of tests reaches, in their
    /// evaluation order: the evaluation module of `cheby run`, `cheby
    /// test` and `cheby build` (D-978, D-1187), their
    /// module set closed under whole modules (D-593, D-1177), `plain`.
    Evaluate(&'a [DefId]),
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
    // A CLIF dump for goldens keeps D-510's view (D-966).
    let scope = if codegen.capture_clif {
        Scope::Reached
    } else {
        command_scope()
    };
    build_scoped(
        front,
        sources,
        roots,
        codegen,
        mir,
        dump,
        reuse_report,
        scope,
    )
}

/// [`build_dumping`] in `scope`.
#[expect(
    clippy::too_many_arguments,
    reason = "build_dumping's arguments and the scope its tests choose"
)]
#[expect(
    clippy::too_many_lines,
    reason = "one build's steps in order; K5b's lookups of `cheby check`'s constants before it compiles"
)]
pub(crate) fn build_scoped(
    front: &Frontend,
    sources: &SourceMap,
    roots: &Roots<'_>,
    codegen: CodegenOptions,
    mir: MirOptions,
    dump: Option<&Path>,
    reuse_report: bool,
    scope: Scope,
) -> Result<Result<Built, Vec<Diagnostic>>, Ice> {
    let capture_clif = codegen.capture_clif;
    let check = matches!(roots, Roots::Constants);
    // `cheby check`'s build is its evaluation module, whose MIR and CLIF
    // go under `consteval/` as every command's do (D-1187).
    let code_dump: Option<PathBuf> = if check {
        dump.map(|dir| dir.join(CONSTEVAL_DIR))
    } else {
        dump.map(Path::to_path_buf)
    };
    let mir_options = mir;
    let mir = kind_options(roots, mir, scope);
    let libraries = front.native_libraries();
    // K4c: package units come from the code cache where their records hit,
    // and are not lowered. A dump is always of fresh code (D-965), and the
    // code-generation benchmark compiles everything. The reuse report reads
    // records with their cached lines (K4d, D-965, refining D-1223).
    let resolves = |symbol: &str| cheby_jit::resolve_native(symbol, &libraries).is_some();
    let resolves: &(dyn Fn(&str) -> bool + Sync) = &resolves;
    let program = scope == Scope::Whole && !check;
    let reading = Reading {
        code: dump.is_none() && !capture_clif && !matches!(roots, Roots::AllFunctions),
        jit: true,
        resolves: program.then_some(resolves),
        reuse: reuse_report && mir.record_sites,
    };
    let lowered = lower_reading(front, sources, roots, mir, scope, reading)?;
    if let Some(dir) = &code_dump {
        write_mir_dumps(dir, &lowered.units)?;
    }
    let rc_ops = matches!(roots, Roots::AllFunctions).then(|| rc_ops(&lowered.units));
    let reuse_report = if reuse_report {
        Some(reuse_report_of(front, sources, &lowered, mir)?)
    } else {
        None
    };
    // D-943: E0053 for the symbols reached functions name, decided here,
    // before anything runs; the JIT binds the rest, which only unreached
    // functions of whole modules name, to `cheby_rt_unresolved`.
    if scope == Scope::Whole && !check {
        let missing = missing_reached_symbols(front, &lowered);
        if !missing.is_empty() {
            let mut diagnostics = unresolved(front, &missing)?;
            sources.sort(&mut diagnostics);
            return Ok(Err(diagnostics));
        }
    }
    // K5b: `cheby check` looks its constants up first (D-977); when every
    // one hits, it compiles and evaluates nothing, installs nothing and
    // replays the cached failures (D-1187).
    let check_infos = if check {
        const_infos(front, sources, &lowered.reach)?
    } else {
        Vec::new()
    };
    let mut check_plan = if check {
        Some(crate::cache::consts::ConstPlan::new(
            front,
            sources,
            &lowered.reach,
            &check_infos,
            mir_options,
            dump.is_some(),
        )?)
    } else {
        None
    };
    if let Some(plan) =
        check_plan.take_if(|plan| !check_infos.is_empty() && plan.to_evaluate().is_empty())
    {
        return Ok(match plan.finish(front, sources, Ok(Vec::new()))? {
            Ok(_) => Ok(Built {
                jit: Jit::new().map_err(jit_ice)?,
                rc_ops,
                reuse_report,
            }),
            Err(diagnostics) => Err(diagnostics),
        });
    }
    let options = JitOptions {
        stub_unresolved: check || scope == Scope::Whole,
        ..jit_options(capture_clif)
    };
    // The dump captures CLIF too; code generation is the same either way.
    let codegen = CodegenOptions {
        capture_clif: capture_clif || dump.is_some(),
        ..codegen
    };
    // K4c: every unit through its code record, fresh or from the store.
    let mut jit = match crate::code_cache::compile(front, &lowered, codegen, options, &libraries) {
        Ok(jit) => jit,
        Err(JitError::UnresolvedSymbols(symbols)) => {
            let mut diagnostics = unresolved(front, &symbols)?;
            sources.sort(&mut diagnostics);
            return Ok(Err(diagnostics));
        }
        Err(e) => return Err(jit_ice(e)),
    };
    if let Some(dir) = &code_dump
        && !capture_clif
    {
        write_dump(&dir.join("clif.txt"), &jit.take_clif().join("\n"))?;
    }
    match roots {
        Roots::AllFunctions => {
            if let Some(dir) = dump {
                write_dump(&dir.join("consts.txt"), "")?;
            }
        }
        // `cheby check`: this build is the evaluation module; it writes the
        // images and installs nothing (D-979).
        Roots::Constants | Roots::Evaluate(_) => {
            let all: Vec<ConstInfo>;
            let infos: &[ConstInfo] = if check {
                &check_infos
            } else {
                all = const_infos(front, sources, &lowered.reach)?;
                &all
            };
            crate::code_cache::name_consts(front, &mut jit, infos)?;
            let mode = if check {
                ConstMode::Check
            } else {
                ConstMode::Run
            };
            // K5b: only the constants that miss are evaluated, and their
            // images or failures stored (D-977).
            let evaluate = match &check_plan {
                Some(plan) => plan.to_evaluate(),
                None => infos.to_vec(),
            };
            let evaluated = match evaluate_in(front, sources, &mut jit, &evaluate, mode)? {
                Err(diagnostics) => Err(diagnostics),
                Ok(()) => Ok(cheby_consteval::images(&jit, &evaluate)?),
            };
            let evaluated = match check_plan {
                Some(plan) => plan.finish(front, sources, evaluated)?,
                None => evaluated,
            };
            if let Err(diagnostics) = evaluated {
                return Ok(Err(diagnostics));
            }
            if let Some(dir) = dump {
                write_const_dump(front, &jit, dir, &evaluate)?;
            }
            // This JIT is returned with its constants' values, which
            // nothing installs: they stay in place, static (D-539).
            cheby_consteval::freeze(&jit, &evaluate)?;
        }
        // The constants `main` or the tests reach are evaluated in a JIT
        // of their own on `plain` code, then installed here from their
        // images before any entry runs (D-978, D-979).
        Roots::Main(_) | Roots::Tests(_) => {
            let infos = const_infos(front, sources, &lowered.reach)?;
            crate::code_cache::name_consts(front, &mut jit, &infos)?;
            // K5b: the hits come from the store; only the misses are
            // evaluated, and with none no evaluation module is built (D-977).
            let evaluated = crate::cache::consts::images_through_cache(
                front,
                sources,
                &lowered.reach,
                &infos,
                mir_options,
                dump.is_some(),
                |subset| {
                    evaluation_module(front, sources, subset, codegen, mir_options, dump, scope)
                },
            )?;
            match evaluated {
                Err(diagnostics) => return Ok(Err(diagnostics)),
                Ok(images) => cheby_consteval::install(&jit, &images, &infos)?,
            }
        }
    }
    Ok(Ok(Built {
        jit,
        rc_ops,
        reuse_report,
    }))
}

/// The text of `--reuse-report` for `lowered` (D-717): it reads the
/// reachable functions, as before whole modules (D-966), since a whole
/// module's unreached ones never run; a unit loaded from its record
/// replays the lines cached beside it (K4d, `reuse_replay.rs`).
fn reuse_report_of(
    front: &Frontend,
    sources: &SourceMap,
    lowered: &Lowered,
    mir: MirOptions,
) -> Result<String, Ice> {
    crate::reuse_replay::report(front, sources, lowered, mir)
}

/// The images of the constants a lowered build of `main` reaches (D-979),
/// evaluated in the evaluation module (D-978) with the MIR options `mir`
/// as under `cheby run`: `cheby build`'s path to the program object
/// (D-924). `Err` holds the constants' diagnostics, sorted.
///
/// # Errors
///
/// An internal compiler error.
pub(crate) fn constant_images(
    front: &Frontend,
    sources: &SourceMap,
    lowered: &Lowered,
    mir: MirOptions,
) -> Result<Result<Vec<ConstImage>, Vec<Diagnostic>>, Ice> {
    let infos = const_infos(front, sources, &lowered.reach)?;
    // K5b: the hits by value from the store; the misses evaluated (D-977).
    crate::cache::consts::images_through_cache(
        front,
        sources,
        &lowered.reach,
        &infos,
        mir,
        false,
        |subset| {
            evaluation_module(
                front,
                sources,
                subset,
                codegen_options(mir, false),
                mir,
                None,
                command_scope(),
            )
        },
    )
}

/// The directory of `CHEBY_DUMP_DIR` that holds the evaluation module's
/// MIR and CLIF (D-1187).
const CONSTEVAL_DIR: &str = "consteval";

/// Evaluates `infos` in `jit` (D-535) under the package's `[const-eval]`
/// limits (D-536), with the JS range check of the constants that exist on
/// `js` (D-540): the diagnostics, sorted, if any constant failed.
fn evaluate_in(
    front: &Frontend,
    sources: &SourceMap,
    jit: &mut Jit,
    infos: &[ConstInfo],
    mode: ConstMode,
) -> Result<Result<(), Vec<Diagnostic>>, Ice> {
    let config = ConstEvalConfig {
        mode,
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
    let mut diagnostics = cheby_consteval::evaluate_with(jit, infos, &config, Some(&types))?;
    if diagnostics.is_empty() {
        Ok(Ok(()))
    } else {
        sources.sort(&mut diagnostics);
        Ok(Err(diagnostics))
    }
}

/// The evaluation module of a build of `main` or of tests (D-978,
/// D-1187): the constants `infos` (the build's, in D-535's
/// order, with the build's seeds), lowered from their module set closed
/// under whole modules (D-593, D-1177) in `plain` code, std's units
/// std's one code (K6, D-978, D-980), compiled in a JIT of its own that gets
/// the package's native libraries as the program's does (D-941),
/// evaluated, and written as images (D-979). The JIT is dropped when this
/// returns. With `dump`, its MIR and CLIF go under `consteval/` and the
/// constants' debug texts into `consts.txt` (D-551).
fn evaluation_module(
    front: &Frontend,
    sources: &SourceMap,
    infos: &[ConstInfo],
    codegen: CodegenOptions,
    mir: MirOptions,
    dump: Option<&Path>,
    scope: Scope,
) -> Result<Result<Vec<ConstImage>, Vec<Diagnostic>>, Ice> {
    if infos.is_empty() {
        if let Some(dir) = dump {
            write_dump(&dir.join("consts.txt"), "")?;
        }
        return Ok(Ok(Vec::new()));
    }
    let defs: Vec<DefId> = infos.iter().map(|info| info.def).collect();
    let roots = Roots::Evaluate(&defs);
    let mir = kind_options(&roots, mir, scope);
    // K4c: the constants' modules' `plain` records from the store where
    // they hit; a dump is always of fresh code (D-965).
    let reading = Reading {
        code: dump.is_none() && !codegen.capture_clif,
        jit: true,
        resolves: None,
        reuse: false,
    };
    let lowered = lower_reading(front, sources, &roots, mir, scope, reading)?;
    let dir = dump.map(|dir| dir.join(CONSTEVAL_DIR));
    if let Some(dir) = &dir {
        write_mir_dumps(dir, &lowered.units)?;
    }
    // Unresolved native symbols bind to `cheby_rt_unresolved`: the
    // program's build reported E0053 for every reached one (D-943), and a
    // constant that calls one is E0077 (D-532).
    let options = JitOptions {
        stub_unresolved: true,
        ..jit_options(codegen.capture_clif)
    };
    let codegen = CodegenOptions {
        capture_clif: dir.is_some(),
        ..codegen
    };
    let libraries = front.native_libraries();
    let mut jit = crate::code_cache::compile(front, &lowered, codegen, options, &libraries)
        .map_err(jit_ice)?;
    if let Some(dir) = &dir {
        write_dump(&dir.join("clif.txt"), &jit.take_clif().join("\n"))?;
    }
    crate::code_cache::name_consts(front, &mut jit, infos)?;
    if let Err(diagnostics) = evaluate_in(front, sources, &mut jit, infos, ConstMode::Run)? {
        return Ok(Err(diagnostics));
    }
    let images = cheby_consteval::images(&jit, infos)?;
    if let Some(dir) = dump {
        write_const_dump(front, &jit, dir, infos)?;
    }
    // The results are freed before the JIT goes: the program keeps only
    // the copies its images install (the wave-6 review's R-1).
    cheby_consteval::release(&jit, infos)?;
    Ok(Ok(images))
}

/// The MIR options of a build's kind (D-971, D-978): every evaluation
/// module (`cheby check`'s build, `Roots::Constants`, with whole modules,
/// and the one of every other command, `Roots::Evaluate`) evaluates
/// constants on `plain` code, so it infers no masks; every other build
/// keeps `mir`. Std's units are std's one code in every build, its masks
/// on (K6: `std_options`, D-978, D-980; D-1180's "until std is cached"
/// ends).
pub(crate) const fn kind_options(roots: &Roots<'_>, mir: MirOptions, scope: Scope) -> MirOptions {
    let plain = match roots {
        Roots::Constants => matches!(scope, Scope::Whole),
        Roots::Evaluate(_) => true,
        Roots::Main(_) | Roots::Tests(_) | Roots::AllFunctions => false,
    };
    if plain {
        MirOptions {
            borrow: false,
            ..mir
        }
    } else {
        mir
    }
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
            .with_note(crate::native::E0053_NOTE))
        })
        .collect()
}

/// The native symbols that the externals of reached functions name
/// (D-943, D-510's set): what E0053 covers. These are the symbols the
/// foreign calls of [`reached_units`] make, exactly what a build lowered
/// before whole modules declared: a reached function's wrapper calls its
/// symbol, while an intrinsic, whose body the compiler generates (D-693),
/// and a function whose only external is for another target call none.
/// The drop functions of reached external types are reached too (D-949,
/// `crate::native::reached_drops`).
pub(crate) fn reached_symbols(lowered: &Lowered) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for unit in reached_units(&lowered.units, lowered) {
        for function in &unit.functions {
            for block in &function.blocks {
                for stmt in &block.stmts {
                    if let cheby_mir::Stmt::Assign {
                        value: cheby_mir::Rvalue::CallForeign { symbol, .. },
                        ..
                    }
                    | cheby_mir::Stmt::Eval(cheby_mir::Rvalue::CallForeign {
                        symbol, ..
                    }) = stmt
                    {
                        out.insert(symbol.to_string());
                    }
                }
            }
        }
    }
    out.extend(crate::native::reached_drops(
        &reached_units(&lowered.units, lowered),
        &lowered.units,
    ));
    out
}

/// The symbols of [`reached_symbols`] that neither the runtime, the
/// package libraries (in build order) nor the process defines, sorted:
/// what `cheby run` and `cheby test` report as E0053 (D-943, D-532), looked
/// up as the JIT looks them up ([`cheby_jit::resolve_native`], D-941).
fn missing_reached_symbols(front: &Frontend, lowered: &Lowered) -> Vec<String> {
    let libraries = front.native_libraries();
    reached_symbols(lowered)
        .into_iter()
        .filter(|symbol| cheby_jit::resolve_native(symbol, &libraries).is_none())
        .collect()
}

/// Copies of `units` with only the functions of reached items and of the
/// selected tests, their closures and continuations included, and every
/// other generated function: what a build lowered before whole modules
/// (D-510), for `--reuse-report` (D-717).
fn reached_units(units: &[MirModule], lowered: &Lowered) -> Vec<MirModule> {
    let keep = |def: DefId| lowered.reach.contains(def) || lowered.selected.contains(&def);
    units
        .iter()
        .map(|unit| {
            let mut copy = unit.clone();
            copy.functions.retain(|f| match f.key {
                cheby_mir::FnKey::Item(def)
                | cheby_mir::FnKey::ValueShim(def)
                | cheby_mir::FnKey::ConstInit(def)
                | cheby_mir::FnKey::Test(def)
                | cheby_mir::FnKey::Closure { owner: def, .. }
                | cheby_mir::FnKey::Continuation { owner: def, .. } => keep(def),
                _ => true,
            });
            copy
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
    /// The selected tests, whose entries run (D-970).
    pub selected: BTreeSet<DefId>,
    /// The code key of each package unit (D-1193), under which K4c stores
    /// its code record.
    pub keys: crate::borrowing::UnitKeys,
    /// Every unit of the build in build order, the entries unit last: one
    /// of [`Lowered::units`], or a package unit's record from the store,
    /// which was not lowered (K4c).
    pub order: Vec<Placed>,
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
/// environment says (D-264), and keeps D-510's view: only the reachable
/// items, with masks inferred over those units (D-1175).
pub(crate) fn lower_program(
    front: &Frontend,
    sources: &SourceMap,
    main: &thir::FnDef,
) -> Result<Vec<MirModule>, Ice> {
    Ok(lower_scoped(
        front,
        sources,
        &Roots::Main(main),
        MirOptions::default(),
        Scope::Reached,
    )?
    .units)
}

/// Lowers what a command lowers from `roots` ([`command_scope`]), counting
/// and checking each unit with the MIR options `mir`: [`lower_scoped`].
pub(crate) fn lower(
    front: &Frontend,
    sources: &SourceMap,
    roots: &Roots<'_>,
    mir: MirOptions,
) -> Result<Lowered, Ice> {
    lower_scoped(front, sources, roots, mir, command_scope())
}

/// Lowers what `roots` need in `scope`, counting and checking each unit
/// with the MIR options `mir`: under [`Scope::Reached`] the reachable
/// items of the modules that need a code unit and the selected tests;
/// under [`Scope::Whole`] every item of every module of the load set and
/// every test of a module with a selected one. Each unit of a module
/// outside std reads other modules through a recording view, and
/// [`Lowered::reads`] holds what it read beyond its imports and its front
/// end's reads (D-955).
pub(crate) fn lower_scoped(
    front: &Frontend,
    sources: &SourceMap,
    roots: &Roots<'_>,
    mir: MirOptions,
    scope: Scope,
) -> Result<Lowered, Ice> {
    lower_reading(front, sources, roots, mir, scope, Reading::default())
}

/// [`lower_scoped`], taking package units from the build cache as
/// `reading` allows (K4c, [`lower_through_cache`]): a unit whose code
/// record hits is not lowered, and [`Lowered::order`] holds its record.
pub(crate) fn lower_reading(
    front: &Frontend,
    sources: &SourceMap,
    roots: &Roots<'_>,
    mir: MirOptions,
    scope: Scope,
    reading: Reading<'_>,
) -> Result<Lowered, Ice> {
    let local = local_modules(front);
    let mut starts: Vec<Start<'_>> = Vec::new();
    let mut walk_roots: Vec<Root<'_>> = Vec::new();
    let mut entries: Vec<EntryRequest<'_>> = Vec::new();
    // The selected tests of each module, by module.
    let mut tests: BTreeMap<ModuleId, Vec<&thir::TestDef>> = BTreeMap::new();
    match roots {
        Roots::Main(main) => {
            walk_roots.push(Root::Fn(main));
            starts.push(Start::Main(main));
            entries.push(EntryRequest {
                kind: EntryKind::Main(main.def),
                info: main.entry.as_ref(),
            });
        }
        Roots::Tests(selected) => {
            for t in selected {
                walk_roots.push(Root::Test(t.test));
                starts.push(Start::Test(t.test));
                entries.push(EntryRequest {
                    kind: EntryKind::Test(t.test.def),
                    info: Some(&t.test.entry),
                });
                tests.entry(t.module).or_default().push(t.test);
            }
        }
        Roots::Constants => {
            for &module in &local {
                let consts = &checked(front, module)?.thir.consts;
                walk_roots.extend(consts.iter().map(Root::Const));
                starts.extend(consts.iter().map(|c| Start::Const(c.def)));
            }
        }
        Roots::AllFunctions => {
            for &module in &local {
                let fns = &checked(front, module)?.thir.fns;
                walk_roots.extend(fns.iter().map(Root::Fn));
                starts.extend(fns.iter().map(|f| Start::Fn(f.def)));
            }
        }
        Roots::Evaluate(consts) => {
            for &def in *consts {
                // Only D-510's walk reads the body: a std hit has none (K6).
                if scope == Scope::Reached {
                    let mut consts = checked(front, def.module)?.thir.consts.iter();
                    let c = consts.find(|c| c.def == def);
                    walk_roots.push(Root::Const(c.ok_or_else(|| Ice::new("a constant's body"))?));
                }
                starts.push(Start::Const(def));
            }
        }
    }
    let selected: BTreeSet<DefId> = tests.values().flatten().map(|t| t.def).collect();
    let (reach, code_modules) = reach_in(front, &walk_roots, &starts, &mut tests, scope)?;
    // The constants' entry thunks: in an evaluation module, and in the
    // dumps for goldens, which keep D-1175's view. A command's own JIT
    // runs no constant entry: its constants come from images (D-979).
    let const_entries = match roots {
        Roots::Constants | Roots::Evaluate(_) => true,
        Roots::Main(_) | Roots::Tests(_) => scope == Scope::Reached,
        Roots::AllFunctions => false,
    };
    if const_entries {
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
        .filter(|&(module, selected)| code_modules.contains(&module) || selected.is_some())
        .collect();
    let logs = read_logs(front, &jobs);
    // D-510 lowers the reachable items of a module, D-966 all of them.
    let wanted = |def: DefId| scope == Scope::Whole || reach.contains(def);
    let cx = Lowering {
        front,
        sources,
        wanted: &wanted,
        local: &local,
        jobs: &jobs,
        code_modules: &code_modules,
        logs: &logs,
        record_sites: mir.record_sites,
        whole: scope == Scope::Whole,
    };
    let entries_unit = if entries.is_empty() {
        None
    } else {
        Some(lower_entry_unit(front, sources, &entries)?)
    };
    let done = lower_through_cache(&cx, entries_unit, roots, mir, reading)?;
    Ok(Lowered {
        units: done.units,
        order: done.order,
        reach,
        reads: done.reads,
        selected,
        keys: done.keys,
    })
}

/// One lowering's modules and how to lower them ([`lower_scoped`]).
struct Lowering<'a> {
    front: &'a Frontend,
    sources: &'a SourceMap,
    /// The items lowered (D-510's reachable ones, or all, D-966).
    wanted: &'a (dyn Fn(DefId) -> bool + Sync),
    /// The local package's modules.
    local: &'a [ModuleId],
    /// The modules lowered, in build order, each with its selected tests.
    jobs: &'a [(ModuleId, Option<&'a Vec<&'a thir::TestDef>>)],
    /// The modules that get a code unit.
    code_modules: &'a BTreeSet<ModuleId>,
    logs: &'a BTreeMap<ModuleId, UnitLogs>,
    /// `--reuse-report`'s sites (D-717), recorded only by the package's own
    /// modules.
    record_sites: bool,
    /// Whether the build lowers whole modules ([`Scope::Whole`]): std's
    /// units then get std's one code, its masks inferred over every std
    /// module (D-968, `std_cache.rs`), unless the build is std's self-test.
    whole: bool,
}

/// A unit's place in a build: its module's job (build order) and part.
type At = (usize, UnitPart);

impl Lowering<'_> {
    /// Every unit the build needs: a code part for each module that gets a
    /// code unit, a tests part for each with selected tests, in build
    /// order.
    fn parts(&self) -> Vec<At> {
        let mut out = Vec::new();
        for (job, &(module, selected)) in self.jobs.iter().enumerate() {
            if self.code_modules.contains(&module) {
                out.push((job, UnitPart::Code));
            }
            if selected.is_some() {
                out.push((job, UnitPart::Tests));
            }
        }
        out
    }

    /// The module of the unit `at`.
    fn module(&self, at: At) -> Result<ModuleId, Ice> {
        self.jobs
            .get(at.0)
            .map(|&(module, _)| module)
            .ok_or_else(|| Ice::new("a unit of a module the build does not lower"))
    }

    /// Lowers the units `parts`, each checked as lowered, in parallel on
    /// rayon's global pool (G1, D-263), taken in build order so the units
    /// and the error reported do not depend on the thread count (D-264).
    /// Lowering a module reads only its THIR and the interfaces (D-246).
    fn lower(&self, parts: &BTreeSet<At>) -> Result<Vec<(At, MirModule)>, Ice> {
        let jobs: BTreeSet<usize> = parts.iter().map(|&(job, _)| job).collect();
        let jobs: Vec<usize> = jobs.into_iter().collect();
        let lowered: Vec<Result<Vec<(At, MirModule)>, Ice>> = jobs
            .par_iter()
            .map(|&job| {
                let &(module, selected) = self
                    .jobs
                    .get(job)
                    .ok_or_else(|| Ice::new("a unit of a module the build does not lower"))?;
                let code = parts.contains(&(job, UnitPart::Code));
                let tests = if parts.contains(&(job, UnitPart::Tests)) {
                    selected
                } else {
                    None
                };
                // Only the package's own modules record their sites for the
                // reuse report (D-717); std and dependencies never do.
                let record_sites = self.record_sites && self.local.contains(&module);
                let [code, tests] = lower_module(
                    self.front,
                    self.sources,
                    self.wanted,
                    module,
                    code,
                    tests,
                    record_sites,
                    self.logs.get(&module),
                )?;
                let mut out = Vec::new();
                if let Some(unit) = code {
                    out.push(((job, UnitPart::Code), unit?));
                }
                if let Some(unit) = tests {
                    out.push(((job, UnitPart::Tests), unit?));
                }
                Ok(out)
            })
            .collect();
        let mut out = Vec::new();
        for result in lowered {
            out.extend(result?);
        }
        Ok(out)
    }

    /// The masks [`unit_masks`] gives a code unit, recording its module's
    /// reads (D-955). A std code unit of a build with std's one code gets
    /// none here: its masks are std's whole ones (D-968), which the build
    /// passes as stored masks.
    fn masks_for(
        &self,
        unit: &MirModule,
        facts: &borrow::BuildFacts,
        std_whole: bool,
    ) -> Result<borrow::Masks, Ice> {
        let log = match unit.unit {
            UnitKind::Code(module) => {
                if std_whole && is_std(self.front, module) {
                    return Ok(borrow::Masks::new());
                }
                self.logs.get(&module).map(|l| &l.code)
            }
            _ => None,
        };
        unit_masks(self.front, unit, facts, log)
    }
}

/// Whether `module` is a std module of `front`.
fn is_std(front: &Frontend, module: ModuleId) -> bool {
    front
        .modules
        .get(module)
        .is_some_and(|info| info.package == cheby_iface::PackageId::STD)
}

/// Whether `roots` are std's own tests: `cargo xtask test-std`'s builds,
/// under `BuildKind::StdSelfTest` (D-980, D-479), where std is the
/// package and its masks are inferred over the build's units as a test
/// build's are (D-779), so they never reach std's one code
/// (GAP(std-self-test-kind)).
fn is_self_test(front: &Frontend, roots: &Roots<'_>) -> bool {
    matches!(roots, Roots::Tests(tests) if tests.iter().any(|t| is_std(front, t.module)))
}

/// How a lowering gets its std units (K6, D-968, D-980).
struct StdUnits {
    /// Whether std's units are std's one code: a build of whole modules
    /// that is not std's self-test.
    whole: bool,
    /// Std's MIR options in this build ([`crate::std_cache::std_mir`]).
    mir: MirOptions,
    /// The std units whose code records came from the store or from std's
    /// first use, which are not lowered: in a JIT host with a cache that
    /// reads.
    cached: BTreeMap<At, cheby_codegen_clif::record::CodeRecord>,
    /// The whole masks of the std units this build lowers itself, by def
    /// of the build's front end; empty without borrowing.
    masks: Option<borrow::Masks>,
}

impl StdUnits {
    /// The MIR options `unit` is counted with: std's in a build with std's
    /// one code, `mir` otherwise.
    fn mir_of(&self, front: &Frontend, unit: &MirModule, mir: MirOptions) -> MirOptions {
        match unit.unit {
            UnitKind::Code(module) if self.whole && is_std(front, module) => self.mir,
            _ => mir,
        }
    }

    /// Writes std's whole masks into the std units among `units` (D-968),
    /// which the build lowered itself; a std unit's calls name only std's
    /// functions and the package's units call only std's `pub` ones, which
    /// never borrow (D-787), so no other unit needs them.
    fn apply_masks(&self, front: &Frontend, units: &mut [MirModule]) -> Result<(), Ice> {
        let Some(masks) = &self.masks else {
            return Ok(());
        };
        let applied: Vec<Result<(), Ice>> = units
            .par_iter_mut()
            .filter(|unit| matches!(unit.unit, UnitKind::Code(m) if is_std(front, m)))
            .map(|unit| borrow::apply(unit, masks))
            .collect();
        applied.into_iter().collect()
    }
}

/// Std's MIR options in a build of `roots` lowered with `mir` (after
/// [`kind_options`]): an evaluation module's `plain` kind turns borrowing
/// off for the package only, and std keeps its one code, which borrows
/// (D-978, [`crate::std_cache::std_mir`]); any other build lowers std with
/// its own options, so a build without borrowing (`CHEBY_RC=naive`, the
/// oracle of D-718 and D-808) borrows nowhere. `plain` turns borrowing
/// off alone, and the oracle's options turn reuse off with it
/// ([`MirOptions::perceus_passes_off`]), so an evaluation module under the
/// oracle keeps std without borrowing too.
const fn std_options(roots: &Roots<'_>, mir: MirOptions) -> MirOptions {
    match roots {
        Roots::Constants | Roots::Evaluate(_) if mir.reuse => crate::std_cache::std_mir(mir),
        _ => MirOptions {
            record_sites: false,
            ..mir
        },
    }
}

/// The std units of `parts` and how this lowering gets them (K6): in a
/// build of whole modules that is not std's self-test, a JIT host whose
/// command reads a cache takes every std unit from its code record under
/// std's settings, logging `code.std <module> hit|miss`, and a miss is
/// std's first use (`std_cache.rs`); any other such build lowers its std
/// units itself and gives them std's whole masks. A self-test, the
/// code-generation benchmark (`own_scope`) and a dump for goldens
/// (D-1175) infer std's masks over the build's units, as before.
fn std_units(
    cx: &Lowering<'_>,
    parts: &[At],
    own_scope: bool,
    std_mir: MirOptions,
    reading: Reading<'_>,
) -> Result<StdUnits, Ice> {
    let front = cx.front;
    let whole = cx.whole && !own_scope;
    let mut out = StdUnits {
        whole,
        mir: std_mir,
        cached: BTreeMap::new(),
        masks: None,
    };
    if !whole {
        return Ok(out);
    }
    let mut std_parts: Vec<(At, String)> = Vec::new();
    for &at in parts {
        let module = cx.module(at)?;
        if at.1 == UnitPart::Code && is_std(front, module) {
            std_parts.push((at, front.modules.try_get(module)?.path.to_string()));
        }
    }
    if std_parts.is_empty() {
        return Ok(out);
    }
    let store = crate::borrowing::sink(front)
        .filter(|_| reading.jit)
        .and_then(crate::borrowing::RecordSink::std_store);
    if let Some(store) = store {
        let settings = crate::std_cache::std_settings(&crate::borrowing::settings(
            crate::cache::BuildKind::Std,
            std_mir,
        ));
        let paths: Vec<String> = std_parts.iter().map(|(_, path)| path.clone()).collect();
        let mut found = crate::std_cache::std_records(&store, &paths, &settings, std_mir)?;
        for (at, path) in std_parts {
            let record = found
                .remove(&path)
                .ok_or_else(|| Ice::new(format!("no std record of `{path}`")))?;
            out.cached.insert(at, record);
        }
    } else if std_mir.borrows() {
        out.masks = Some(crate::std_cache::std_masks_in(front)?);
    }
    Ok(out)
}

/// What a lowering may take from the build cache (K4c).
#[derive(Clone, Copy, Default)]
pub(crate) struct Reading<'a> {
    /// Whether code and tests records are read (D-914): a unit whose record
    /// hits is not lowered, inferred or compiled, and loads from its
    /// record. `CHEBY_DUMP_DIR` reads nothing at all (D-965).
    pub code: bool,
    /// Whether the build's JIT loads the units' code records, so each
    /// package unit's lookup is logged (`code.<kind>`, `tests.<kind>`), as
    /// a miss when `code` reads none: every JIT host's build, not `cheby
    /// build`'s, which compiles its units itself (`aot.rs`).
    pub jit: bool,
    /// For a program's build, whether a native symbol resolves (D-943): a
    /// record that imports one that does not is lowered after all, so
    /// E0053 is decided on its MIR, for the symbols reached functions name.
    pub resolves: Option<&'a (dyn Fn(&str) -> bool + Sync)>,
    /// `--reuse-report` (K4d, D-965): a record is read only with the reuse
    /// lines cached beside it, which it replays; one without them is a
    /// miss, lowered and written with its lines (`reuse_replay.rs`).
    pub reuse: bool,
}

/// What [`lower_through_cache`] gives [`lower_scoped`].
struct CachedLowering {
    units: Vec<MirModule>,
    order: Vec<Placed>,
    reads: BTreeMap<ModuleId, UnitReads>,
    keys: crate::borrowing::UnitKeys,
}

/// A unit of a build, in build order ([`Lowered::order`]).
#[derive(Debug)]
pub(crate) enum Placed {
    /// The unit lowered, at this index of [`Lowered::units`].
    Fresh(usize),
    /// A package unit whose code record came from the store (D-914): it was
    /// not lowered.
    Cached {
        /// The unit's kind.
        unit: UnitKind,
        /// Its module path.
        path: Box<str>,
        /// Its record, decoded with `CodeRecord::decode`'s checks.
        record: cheby_codegen_clif::record::CodeRecord,
    },
}

/// Whether every runtime and foreign symbol `record` imports satisfies
/// `resolves`.
fn record_resolves(
    record: &cheby_codegen_clif::record::CodeRecord,
    resolves: &(dyn Fn(&str) -> bool + Sync),
) -> bool {
    record.symbols.iter().all(|symbol| {
        symbol.linkage != cheby_codegen_clif::record::SymbolLinkage::Import
            || symbol.name.starts_with(cheby_codegen_clif::mangle::PREFIX)
            || resolves(&symbol.name)
    })
}

/// K4c's keys before lowering (D-914, D-1192's "read to skip work",
/// D-1193, D-1195; plan §2.2): the units of `cx` and the entries unit
/// `entries`, each package unit taken from the cache where it can be.
///
/// 1. Each package unit's identity comes from its lowering hint, and its
///    facts record is read ([`crate::borrowing::plan`]).
/// 2. Every unit whose facts did not hit is lowered, std's and the entries
///    unit included; the build's facts are those units' facts and the
///    stored ones (D-1191).
/// 3. In a build with borrowing, each code unit not lowered reads its masks
///    record under the digest of the facts about its module; one that
///    misses is lowered. The lowered code units are inferred, and every
///    lowered unit gets those masks and the stored ones (D-779).
/// 4. Every package unit gets its code key ([`crate::borrowing::unit_keys`])
///    and its record is looked up: a unit not lowered whose record hits is
///    loaded from it, so its lowering, inference and code generation are
///    skipped; a lowered one whose record hits is compiled no more; one not
///    lowered whose record misses is lowered now, inferred again (a debug
///    toolchain compares its records), and looked up once more if its fresh
///    identity differs from its hint's (D-1195: its hint is then written
///    again).
/// 5. The units left are counted. The records of the lowered units are
///    written behind the command (D-1138), and the log gets one `facts`,
///    `masks`, `code.<kind>` or `tests.<kind>` line per lookup and one
///    `key` line per unit (D-973).
///
/// Without a cache, or with nothing read, every unit is lowered as before.
#[expect(
    clippy::too_many_lines,
    reason = "the five steps of the module docs, in their order, over one build's units"
)]
fn lower_through_cache(
    cx: &Lowering<'_>,
    entries: Option<MirModule>,
    roots: &Roots<'_>,
    mir: MirOptions,
    reading: Reading<'_>,
) -> Result<CachedLowering, Ice> {
    use crate::borrowing::{self as records, Planned, StoredMasks};
    let front = cx.front;
    let self_test = is_self_test(front, roots);
    let kind = if self_test {
        crate::cache::BuildKind::StdSelfTest
    } else {
        records::build_kind(roots, mir)
    };
    let borrows = mir.borrows();
    let parts = cx.parts();
    // K6: std's units are std's one code (D-968, D-980), from its records
    // in a JIT host with a cache, or lowered here with std's whole masks.
    // The code-generation benchmark (I1) compiles its own build as before,
    // so its numbers stay comparable (it runs nothing, D-975).
    let own_scope = self_test || matches!(roots, Roots::AllFunctions);
    let mut std = std_units(cx, &parts, own_scope, std_options(roots, mir), reading)?;
    let mut keyed = Vec::with_capacity(parts.len());
    for &at in &parts {
        keyed.push((at.1, cx.module(at)?));
    }
    let plan = records::plan(front, &keyed, reading.code);
    let planned = |at: At| -> Option<&Planned> { plan.get(&(at.1, cx.module(at).ok()?)) };
    // 1-2: every unit whose facts the cache does not hold is lowered.
    let mut skipped: BTreeSet<At> = parts
        .iter()
        .copied()
        .filter(|&at| planned(at).is_some_and(|p| p.facts.is_some()))
        .collect();
    let first: BTreeSet<At> = parts
        .iter()
        .copied()
        .filter(|at| !skipped.contains(at) && !std.cached.contains_key(at))
        .collect();
    let mut tags: Vec<Option<At>> = Vec::new();
    let mut units: Vec<MirModule> = Vec::new();
    for (at, unit) in cx.lower(&first)? {
        tags.push(Some(at));
        units.push(unit);
    }
    if let Some(unit) = entries {
        tags.push(None);
        units.push(unit);
    }
    // 3: the build's facts and masks; every unit is lowered before any is
    // counted, since the masks come from the whole build (D-779).
    let mut stored: BTreeMap<At, StoredMasks> = BTreeMap::new();
    let mut inferred = if borrows {
        let stored_facts: Vec<&cheby_mir::borrow::UnitFacts> = skipped
            .iter()
            .filter_map(|&at| planned(at)?.facts.as_ref())
            .collect();
        let facts = records::build_facts(front, &units, &stored_facts)?;
        let mut second = BTreeSet::new();
        records::with_build_defs(front, |defs| -> Result<(), Ice> {
            for &at in skipped.iter().filter(|at| at.1 == UnitPart::Code) {
                let Some(p) = planned(at) else { continue };
                match records::stored_masks(front, p, cx.module(at)?, kind, &facts, defs)? {
                    Some(masks) => {
                        stored.insert(at, masks);
                    }
                    None => {
                        second.insert(at);
                    }
                }
            }
            Ok(())
        })?;
        for (at, unit) in cx.lower(&second)? {
            skipped.remove(&at);
            tags.push(Some(at));
            units.push(unit);
        }
        let by_def: borrow::Masks = stored
            .values()
            .flat_map(|s| s.by_def.iter().map(|(&def, &mask)| (def, mask)))
            .collect();
        records::infer_with(front, &mut units, facts, &by_def, &|unit, facts| {
            cx.masks_for(unit, facts, std.whole)
        })?
    } else {
        records::Inferred::plain(front, &units)?
    };
    // The std units lowered here get std's whole masks (D-968).
    std.apply_masks(front, &mut units)?;
    // 4: the keys, the records of the lowered units, and the lookups.
    let lowered: BTreeSet<At> = tags.iter().flatten().copied().collect();
    let mut reads = unit_reads(front, cx.jobs, &lowered, cx.logs)?;
    let refs: Vec<&MirModule> = units.iter().collect();
    let mut fresh = records::write_fresh(
        front,
        &refs,
        &reads,
        &inferred,
        kind,
        borrows,
        &plan,
        reading.code,
    )?;
    let mut keys = keys_of(
        cx, &parts, &plan, &skipped, &stored, &fresh, &inferred, kind, mir,
    )?;
    let mut cached: BTreeMap<At, cheby_codegen_clif::record::CodeRecord> = BTreeMap::new();
    let mut third = BTreeSet::new();
    let sink = records::sink(front);
    let usable = |record: &cheby_codegen_clif::record::CodeRecord| {
        reading
            .resolves
            .is_none_or(|ok| record_resolves(record, ok))
    };
    // K4d: `--reuse-report` reads a record only with its reuse lines.
    let readable = |key: &crate::cache::Key| {
        reading.code && (!reading.reuse || crate::reuse_replay::has_lines(front, key))
    };
    if let Some(sink) = sink {
        for &at in &parts {
            let Some(key) = keys.keys.get(&(at.1, cx.module(at)?)) else {
                continue;
            };
            match sink.code_find(key, readable(key)).filter(|r| usable(r)) {
                Some(record) => {
                    cached.insert(at, record);
                }
                None if skipped.contains(&at) => {
                    third.insert(at);
                }
                None => {}
            }
        }
    }
    if !third.is_empty() {
        let mut more: Vec<MirModule> = Vec::new();
        let mut more_tags: Vec<At> = Vec::new();
        for (at, unit) in cx.lower(&third)? {
            skipped.remove(&at);
            more_tags.push(at);
            more.push(unit);
        }
        // Inferred again from the build's facts, which a debug toolchain's
        // record comparison checks against the stored masks (D-1192).
        let masks = if borrows {
            records::masks_of(&more, inferred.facts(), &|unit, facts| {
                cx.masks_for(unit, facts, std.whole)
            })?
        } else {
            borrow::Masks::new()
        };
        let mut all = inferred.masks.clone();
        all.extend(masks.iter().map(|(&def, &mask)| (def, mask)));
        if borrows {
            let applied: Vec<Result<(), Ice>> = more
                .par_iter_mut()
                .map(|unit| borrow::apply(unit, &all))
                .collect();
            applied.into_iter().collect::<Result<(), Ice>>()?;
        }
        inferred.extend(records::Inferred::plain(front, &more)?, &masks);
        let done: BTreeSet<At> = more_tags.iter().copied().collect();
        // Merged per part: the other unit of a module may have been
        // lowered before.
        for (module, late) in unit_reads(front, cx.jobs, &done, cx.logs)? {
            let entry = reads.entry(module).or_default();
            if late.code.is_some() {
                entry.code = late.code;
            }
            if late.tests.is_some() {
                entry.tests = late.tests;
            }
        }
        let refs: Vec<&MirModule> = more.iter().collect();
        fresh.extend(records::write_fresh(
            front,
            &refs,
            &reads,
            &inferred,
            kind,
            borrows,
            &plan,
            reading.code,
        )?);
        // A key that moved (the hint named other reads) is looked up once
        // more under the fresh identity.
        let again = keys_of(
            cx, &parts, &plan, &skipped, &stored, &fresh, &inferred, kind, mir,
        )?;
        for &at in &more_tags {
            let at_key = (at.1, cx.module(at)?);
            let (Some(old), Some(new)) = (keys.keys.get(&at_key), again.keys.get(&at_key)) else {
                continue;
            };
            if old != new
                && let Some(record) = sink
                    .and_then(|s| s.code_find(new, readable(new)))
                    .filter(|r| usable(r))
            {
                cached.insert(at, record);
            }
        }
        keys = again;
        for (at, unit) in more_tags.into_iter().zip(more) {
            tags.push(Some(at));
            units.push(unit);
        }
    }
    // 5: the units left, counted, in build order; then the log. Std's
    // records join the package's (K6).
    cached.append(&mut std.cached);
    let mut placed: Vec<(Option<At>, MirModule)> = tags
        .into_iter()
        .zip(units)
        .filter(|(at, _)| at.is_none_or(|at| !cached.contains_key(&at)))
        .collect();
    placed.sort_by_key(|(at, _)| at.unwrap_or((usize::MAX, UnitPart::Tests)));
    let (tags, mut units): (Vec<Option<At>>, Vec<MirModule>) = placed.into_iter().unzip();
    let counted: Vec<Result<(), Ice>> = units
        .par_iter_mut()
        .map(|unit| count_lowered_with(unit, &std.mir_of(front, unit, mir)))
        .collect();
    for result in counted {
        result?;
    }
    let mut order = Vec::with_capacity(parts.len().saturating_add(1));
    let fresh_at: BTreeMap<At, usize> = tags
        .iter()
        .enumerate()
        .filter_map(|(i, at)| at.map(|at| (at, i)))
        .collect();
    for &at in &parts {
        if let Some(record) = cached.remove(&at) {
            let module = cx.module(at)?;
            order.push(Placed::Cached {
                unit: match at.1 {
                    UnitPart::Code => UnitKind::Code(module),
                    UnitPart::Tests => UnitKind::Tests(module),
                },
                path: front.modules.try_get(module)?.path.to_string().into(),
                record,
            });
        } else if let Some(&i) = fresh_at.get(&at) {
            order.push(Placed::Fresh(i));
        } else {
            return Err(Ice::new(
                "a unit of the build was neither lowered nor cached",
            ));
        }
    }
    if let Some(i) = tags.iter().position(Option::is_none) {
        order.push(Placed::Fresh(i));
    }
    if let Some(sink) = sink {
        if reading.jit {
            log_lookups(cx, sink, &parts, &keys, &order, &skipped, kind, borrows)?;
        }
        records::log_keys(front, kind, &keys)?;
    }
    Ok(CachedLowering {
        units,
        order,
        reads,
        keys,
    })
}

/// Every package unit's code key ([`crate::borrowing::unit_keys`]): a
/// lowered unit's from its fresh identity, facts and masks, one not
/// lowered from its planned identity, stored facts and stored masks.
#[expect(
    clippy::too_many_arguments,
    reason = "what lower_through_cache holds about each unit"
)]
fn keys_of(
    cx: &Lowering<'_>,
    parts: &[At],
    plan: &crate::borrowing::Plan,
    skipped: &BTreeSet<At>,
    stored: &BTreeMap<At, crate::borrowing::StoredMasks>,
    fresh: &BTreeMap<(UnitPart, ModuleId), crate::borrowing::FreshUnit>,
    inferred: &crate::borrowing::Inferred,
    kind: crate::cache::BuildKind,
    mir: MirOptions,
) -> Result<crate::borrowing::UnitKeys, Ice> {
    let none: Vec<(String, borrow::BorrowMask)> = Vec::new();
    // A code unit's own masks, by module: fresh, or stored.
    let code_own = |job: usize, module: ModuleId| -> &[(String, borrow::BorrowMask)] {
        fresh.get(&(UnitPart::Code, module)).map_or_else(
            || {
                stored
                    .get(&(job, UnitPart::Code))
                    .map_or(none.as_slice(), |s| s.record.masks.as_slice())
            },
            |f| f.own.as_slice(),
        )
    };
    let mut units = Vec::with_capacity(parts.len());
    for &at in parts {
        let module = cx.module(at)?;
        let (identity, facts) = if let Some(f) = fresh.get(&(at.1, module)) {
            let Some(facts) = inferred.part(at.1, module) else {
                continue;
            };
            (f.identity, facts)
        } else if skipped.contains(&at)
            && let Some(p) = plan.get(&(at.1, module))
            && let (Some(identity), Some(facts)) = (p.identity, p.facts.as_ref())
        {
            (identity, facts)
        } else {
            continue;
        };
        units.push(crate::borrowing::KeyUnit {
            part: at.1,
            module,
            identity,
            own: code_own(at.0, module),
            facts,
        });
    }
    crate::borrowing::unit_keys(cx.front, kind, mir, &units, &inferred.masks)
}

/// The lookup lines of a lowering through the cache (D-973;
/// D-1224): `code.<kind> <module> hit|miss` or
/// `tests.<kind> <module>#tests hit|miss` for each package unit with a
/// key, and, for each unit loaded without being lowered, the `facts` hit
/// and, in a build with borrowing, the `masks` hit its key read (a lowered
/// unit's lines come with its records).
#[expect(
    clippy::too_many_arguments,
    reason = "what lower_through_cache holds about each unit"
)]
fn log_lookups(
    cx: &Lowering<'_>,
    sink: &crate::borrowing::RecordSink,
    parts: &[At],
    keys: &crate::borrowing::UnitKeys,
    order: &[Placed],
    skipped: &BTreeSet<At>,
    kind: crate::cache::BuildKind,
    borrows: bool,
) -> Result<(), Ice> {
    for (&at, placed) in parts.iter().zip(order) {
        let module = cx.module(at)?;
        if !keys.keys.contains_key(&(at.1, module)) {
            continue;
        }
        let path = cx.front.modules.try_get(module)?.path.to_string();
        let hit = matches!(placed, Placed::Cached { .. });
        let subject = match at.1 {
            UnitPart::Code => path.clone(),
            UnitPart::Tests => format!("{path}#tests"),
        };
        sink.log_lookup(&format!("{}.{}", at.1.tag(), kind.tag()), &subject, hit);
        if skipped.contains(&at) {
            sink.log_lookup("facts", &subject, true);
            if borrows && at.1 == UnitPart::Code {
                sink.log_lookup("masks", &subject, true);
            }
        }
    }
    Ok(())
}

/// What the roots reach in `scope`, and the modules that get a code unit:
/// D-510's reachable modules over THIR (`walk_roots`), or the load set
/// over summaries (`starts`, D-967, D-922), where every module with a
/// selected test gets its whole tests unit: every test of it that exists
/// on native replaces the selection in `tests` (D-970); the selection
/// decides only which entries run. In the JIT, where a build's units share
/// one `Decls`, `cb$test$<module>$<n>` then numbers all of a module's
/// tests in source order, whatever the filter (D-1179); a
/// unit compiled alone still ranks the tests it imports among those it
/// names, which K4c makes agree when it caches tests units.
fn reach_in<'a>(
    front: &'a Frontend,
    walk_roots: &[Root<'_>],
    starts: &[Start<'_>],
    tests: &mut BTreeMap<ModuleId, Vec<&'a thir::TestDef>>,
    scope: Scope,
) -> Result<(Reach, BTreeSet<ModuleId>), Ice> {
    match scope {
        Scope::Reached => {
            let mut program = Program::new();
            for &module in &front.order {
                program.insert(module, &checked(front, module)?.thir);
            }
            let hooks = |module: ModuleId| hooks_of(front, module);
            let reach = reachable_with_hooks(&program, walk_roots, &hooks)?;
            let modules: BTreeSet<ModuleId> = reach.modules().collect();
            Ok((reach, modules))
        }
        Scope::Whole => {
            let summaries = Summaries::new(front)?;
            let reach = summaries.reach(starts)?;
            let with_tests: BTreeSet<ModuleId> = tests.keys().copied().collect();
            let modules = summaries.load_set(&reach, &with_tests)?;
            for (&module, chosen) in tests.iter_mut() {
                // K4d: a warm hit's tests come from its warm facts (D-1250).
                *chosen = front
                    .tests_of(module)
                    .ok_or_else(|| Ice::new(format!("module {module:?} was not checked")))?
                    .iter()
                    .filter(|t| crate::test::on_native(front, t))
                    .collect();
            }
            Ok((reach, modules.into_iter().collect()))
        }
    }
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

/// The hidden reads of each unit of `lowered` (by job and part) of the
/// modules in `logs` (D-955): what its views handed out beyond the
/// module's imports and its front end's hidden reads. A unit the build
/// loaded from the cache was not lowered and reads nothing (K4c).
fn unit_reads(
    front: &Frontend,
    jobs: &[(ModuleId, Option<&Vec<&thir::TestDef>>)],
    lowered: &BTreeSet<At>,
    logs: &BTreeMap<ModuleId, UnitLogs>,
) -> Result<BTreeMap<ModuleId, UnitReads>, Ice> {
    let mut reads = BTreeMap::new();
    for (job, &(module, _)) in jobs.iter().enumerate() {
        let code = lowered.contains(&(job, UnitPart::Code));
        let tests = lowered.contains(&(job, UnitPart::Tests));
        if !code && !tests {
            continue;
        }
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
                code: code.then(|| hidden(&log.code)),
                tests: tests.then(|| hidden(&log.tests)),
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
/// never depends on tests (D-779). [`crate::borrowing::infer`] runs the
/// steps and keeps what the build's records need (D-968, D-969).
fn infer_borrowing(
    front: &Frontend,
    units: &mut [MirModule],
    logs: &BTreeMap<ModuleId, UnitLogs>,
) -> Result<crate::borrowing::Inferred, Ice> {
    crate::borrowing::infer(front, units, &|unit, facts| {
        let log = match unit.unit {
            UnitKind::Code(module) => logs.get(&module).map(|l| &l.code),
            _ => None,
        };
        unit_masks(front, unit, facts, log)
    })
}

/// [`infer_borrowing`] over `units` alone, recording no reads: the REPL's
/// std units (D-971).
///
/// # Errors
///
/// An [`Ice`] if a unit's module was not checked.
pub(crate) fn infer_units(front: &Frontend, units: &mut [MirModule]) -> Result<(), Ice> {
    infer_borrowing(front, units, &BTreeMap::new()).map(|_| ())
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

/// [`unit_masks`] recording no reads: std's whole inference (D-968,
/// `std_cache.rs`).
pub(crate) fn unit_masks_of(
    front: &Frontend,
    unit: &MirModule,
    facts: &borrow::BuildFacts,
) -> Result<borrow::Masks, Ice> {
    unit_masks(front, unit, facts, None)
}

/// The code units of `modules`, each lowered whole (D-966) and checked as
/// lowered, in parallel, in the order given: std's whole inference scope
/// (D-968, `std_cache.rs`).
///
/// # Errors
///
/// An [`Ice`] if a module was not checked or does not lower.
pub(crate) fn lower_whole(
    front: &Frontend,
    sources: &SourceMap,
    modules: &[ModuleId],
) -> Result<Vec<MirModule>, Ice> {
    let all = |_: DefId| true;
    let lowered: Vec<Result<Option<MirModule>, Ice>> = modules
        .par_iter()
        .map(|&module| {
            let [code, _] = lower_module(front, sources, &all, module, true, None, false, None)?;
            code.transpose()
        })
        .collect();
    let mut out = Vec::with_capacity(modules.len());
    for unit in lowered {
        out.extend(unit?);
    }
    Ok(out)
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

/// Lowers `module`: if `code`, its items that `wanted` selects into its
/// code unit, and its `selected` tests into its tests unit, each checked
/// as lowered ([`check_lowered`]); counting waits for the whole build
/// (D-779). With `record_sites`, both units record the sites of
/// `--reuse-report` (D-717). With `logs`, each unit reads other modules
/// through a view that records into its own log (D-955).
#[expect(
    clippy::too_many_arguments,
    reason = "one module's job of `lower_scoped`, called in one place"
)]
fn lower_module(
    front: &Frontend,
    sources: &SourceMap,
    wanted: &(dyn Fn(DefId) -> bool + Sync),
    module: ModuleId,
    code: bool,
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
    let code = code.then(|| {
        counted(lower_items_recording(
            &checked.thir,
            wanted,
            &cx,
            record_sites,
        ))
        .map(|mut unit| {
            // D-949: the external types' drop functions, which the module
            // record's external entries point to (D-1168).
            crate::native::attach_drops(front, module, &mut unit, 0);
            unit
        })
    });
    let tests = selected.map(|selected| {
        // Only the tests given: the selected ones under D-510, whose
        // others may reach items this build does not lower, or every test
        // on native of a whole tests unit (D-970).
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
pub(crate) fn hooks_of(front: &Frontend, module: ModuleId) -> Result<Vec<DefId>, Ice> {
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
pub(crate) fn const_infos(
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
        // K5b: a module a warm command did not check (each of its
        // constants hit, D-1250) names its constants through its decoded
        // interface, whose spans are the names' (D-965).
        if front.checked_quiet(def.module).is_none() {
            let defs = own_defs(front, def.module, &none)?;
            if let Some(item) = defs.own.and_then(|own| own.item(def.index))
                && item.kind == cheby_iface::DefKind::Const
                && let Some(span) = item.span
            {
                names.insert(def, defs.name(def)?.item.into());
                spans.insert(def, span);
            }
            continue;
        }
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
            path: format!("{module_path}::{name}").into(),
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

/// [`compile_with_libraries`] without package libraries, for the tests:
/// every command's JIT goes through [`compile_with_libraries`].
#[cfg(test)]
fn compile_with(
    units: &[MirModule],
    codegen: CodegenOptions,
    options: JitOptions,
) -> Result<Jit, JitError> {
    compile_with_libraries(units, codegen, options, &[])
}

/// A JIT with `codegen` and `options` that also resolves native symbols in
/// the package libraries `libraries` (D-941, build order), with `units`
/// compiled, for the tests: every command's JIT goes through
/// [`crate::code_cache::compile`], which loads the same records.
#[cfg(test)]
fn compile_with_libraries(
    units: &[MirModule],
    codegen: CodegenOptions,
    options: JitOptions,
    libraries: &[std::sync::Arc<cheby_jit::NativeLibrary>],
) -> Result<Jit, JitError> {
    let mut jit = Jit::configured(codegen, options)?;
    for library in libraries {
        jit.add_library(std::sync::Arc::clone(library))?;
    }
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
        // The evaluation module of these constants (D-978).
        let defs: Vec<DefId> = infos.iter().map(|i| i.def).collect();
        let roots = Roots::Evaluate(&defs);
        let evaluation = lower(
            &front,
            &out.sources,
            &roots,
            kind_options(&roots, MirOptions::default(), Scope::Whole),
        )
        .unwrap();
        let mut texts = Vec::new();
        for order in [
            ["table", "keys", "size"],
            ["keys", "table", "size"],
            ["size", "keys", "table"],
        ] {
            let mut jit = compile_with(
                &evaluation.units,
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
            // SAFETY: values their slots own while `jit` lives.
            unsafe {
                assert_eq!(
                    cheby_runtime::hamt::seed_of(read("table")),
                    Some(by_name("table").seed)
                );
                texts.push(cheby_runtime::generic::debug_string(read("keys")));
                // The wave-6 review's R-1: the results are not frozen, so
                // releasing them frees them.
                assert!(!cheby_runtime::consteval::image::is_static(read("table")));
            }
            assert!(jit.domain().live_objects() > 0);
            cheby_consteval::release(&jit, &order).unwrap();
            // Released results leave nothing in the build's domain, and
            // their slots are empty.
            assert_eq!(jit.domain().live_objects(), 0);
            assert_eq!(read("table"), 0);
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
        // `cheby check`'s build is its evaluation module, dumped under
        // `consteval/` (D-1187).
        let module = dumps.join(CONSTEVAL_DIR);
        let app = std::fs::read_to_string(module.join("mir/app.mir")).unwrap();
        assert!(app.contains("numbers"), "{app}");
        assert!(module.join("mir/std::map.mir").is_file());
        assert!(module.join("mir/entries.mir").is_file());
        let clif = std::fs::read_to_string(module.join("clif.txt")).unwrap();
        assert!(clif.contains("function"), "{clif}");
        assert!(!dumps.join("clif.txt").exists());
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

    /// The root module's `main` of a package checked by [`front`].
    fn main_of(front: &Frontend) -> &thir::FnDef {
        let module = front
            .local_module(&cheby_package::ModulePath::root("app"))
            .unwrap();
        front.main_of(module).unwrap()
    }

    /// D-978, D-979: `cheby run` evaluates its constants in a module of its
    /// own, so the program's JIT compiles no constant entry thunk, and the
    /// values it reads are installed from images: static, with maps built
    /// with the process seed (D-196).
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn const_plain_run_runs_no_thunk_in_the_program_jit() {
        let dir = package("plainrun", "", FORCING);
        let (front, out) = front(&dir);
        let main = main_of(&front);
        let Ok(built) = build(&front, &out.sources, Roots::Main(main), false).unwrap() else {
            panic!("the package did not build");
        };
        let lowered = lower(
            &front,
            &out.sources,
            &Roots::Main(main),
            MirOptions::default(),
        )
        .unwrap();
        let infos = const_infos(&front, &out.sources, &lowered.reach).unwrap();
        assert_eq!(infos.len(), 3);
        for info in &infos {
            assert!(
                built.jit.entry(EntryKind::Const(info.def)).is_err(),
                "{}",
                info.name
            );
            let slot = built.jit.const_slot(info.def).unwrap();
            // SAFETY: an installed constant's slot, readable.
            let word = unsafe { slot.read() };
            assert_ne!(word, 0, "{}", info.name);
            // SAFETY: an installed value.
            let installed = cheby_runtime::abi::is_tagged_int(word)
                || unsafe { cheby_runtime::consteval::image::is_static(word) };
            assert!(installed, "{}", info.name);
        }
        let table = infos.iter().find(|i| &*i.name == "table").unwrap();
        // SAFETY: the installed map.
        let seed = unsafe {
            cheby_runtime::hamt::seed_of(built.jit.const_slot(table.def).unwrap().read())
        };
        assert_eq!(seed, Some(cheby_runtime::generic::process_seed()));
        assert!(built.jit.entry(EntryKind::Main(main.def)).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// D-1187: the evaluation module of a constant in a
    /// module that imports `std::list` holds `std::list`'s whole code unit
    /// (D-966, D-1177), as the program's whole-module build has it and more
    /// than D-510's reached view, and no unit of it carries a mask: the
    /// package's and std's code are `plain` (D-978, D-1180).
    #[test]
    fn const_plain_evaluation_module_holds_std_list_whole_without_masks() {
        let dir = package("plainlist", "", FORCING);
        let (front, out) = front(&dir);
        let main = main_of(&front);
        let program = lower(
            &front,
            &out.sources,
            &Roots::Main(main),
            MirOptions::default(),
        )
        .unwrap();
        let infos = const_infos(&front, &out.sources, &program.reach).unwrap();
        let defs: Vec<DefId> = infos.iter().map(|i| i.def).collect();
        let roots = Roots::Evaluate(&defs);
        let mir = kind_options(&roots, MirOptions::default(), Scope::Whole);
        assert!(!mir.borrow);
        let evaluation = lower(&front, &out.sources, &roots, mir).unwrap();
        let reached = lower_scoped(
            &front,
            &out.sources,
            &Roots::Main(main),
            MirOptions::default(),
            Scope::Reached,
        )
        .unwrap();
        let items = |units: &[MirModule]| -> BTreeSet<cheby_mir::FnKey> {
            units
                .iter()
                .filter(|u| matches!(u.unit, UnitKind::Code(_)) && &*u.path == "std::list")
                .flat_map(|u| u.functions.iter().map(|f| f.key))
                .collect()
        };
        let whole = items(&evaluation.units);
        assert!(!whole.is_empty(), "no std::list unit");
        assert_eq!(whole, items(&program.units));
        assert!(whole.len() > items(&reached.units).len());
        for unit in &evaluation.units {
            for function in &unit.functions {
                assert_eq!(
                    function.borrow,
                    borrow::BorrowMask::OWNED,
                    "{}: {:?}",
                    unit.path,
                    function.key
                );
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// D-978: `cheby check` and `cheby run` evaluate the same functions:
    /// the evaluation modules' MIR and CLIF dumps are the same files with
    /// the same text when `main` reaches every constant.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn const_plain_check_and_run_evaluate_the_same_functions() {
        let dir = package("plainsame", "", FORCING);
        let (front, out) = front(&dir);
        let main = main_of(&front);
        let mut texts = Vec::new();
        for (name, roots) in [("check", Roots::Constants), ("run", Roots::Main(main))] {
            let dumps = dir.join(name);
            let Ok(_) = build_dumping(
                &front,
                &out.sources,
                &roots,
                CodegenOptions::default(),
                MirOptions::default(),
                Some(&dumps),
                false,
            )
            .unwrap() else {
                panic!("the package did not build");
            };
            let module = dumps.join(CONSTEVAL_DIR);
            let mut mir: Vec<(String, String)> = std::fs::read_dir(module.join("mir"))
                .unwrap()
                .map(|e| {
                    let path = e.unwrap().path();
                    let name = path.file_name().unwrap().to_string_lossy().into_owned();
                    (name, std::fs::read_to_string(&path).unwrap())
                })
                .collect();
            mir.sort();
            let clif = std::fs::read_to_string(module.join("clif.txt")).unwrap();
            assert!(clif.contains("function"), "{clif}");
            let consts = std::fs::read_to_string(dumps.join("consts.txt")).unwrap();
            texts.push((mir, clif, consts));
        }
        assert_eq!(texts[0], texts[1]);
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
