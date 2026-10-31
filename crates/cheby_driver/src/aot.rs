//! `cheby build` (D-924, D-937): the package's `main` into an executable,
//! through the build cache's front records and in-memory code records.
//!
//! The front end runs as `cheby run` runs it (D-955), warm with a cache:
//! it checks only what the build needs (D-1250, N9) over the Build
//! family's own package index (D-962), and starts again with every module
//! checked when the build wants a hit's body (D-1256). `main` is chosen by
//! `cheby run`'s rules (D-137, D-464), and the modules `main` needs are
//! lowered whole as `cheby run` lowers them (D-966), through `cheby run`'s
//! code records (D-914, D-924): a package unit whose `run` record hits is
//! not lowered, std's units come from std's code records (D-1264), and each
//! fresh code unit is compiled alone into the record `cheby run` would
//! compile and store (D-917, D-1220), and stored. Every record is replayed
//! into one object under its module path, in build order (D-915), and the
//! entries unit, compiled alone too and never stored (D-964), goes into
//! the program object with `cb$image` and the C `main` (D-1111). `cc`
//! links them with the runtime archive (D-929, D-933), and the linker
//! drops the functions nothing reaches with their record entries
//! (D-1253, `cheby_link`).
//!
//! Before anything links, in this order:
//!
//! - the package's `[native]` C sources are compiled (cached) and its
//!   shared library loaded by the front end, as for `cheby run` (D-940 to
//!   D-942); the same C objects and the `libraries` go into the link
//!   after the program object (D-1198);
//! - E0053 for every native symbol that a reached `@external` names and
//!   D-943's build resolver cannot find: the runtime's exported names,
//!   then the package's shared library, then each system library of the
//!   archive opened by its recorded file name
//!   ([`cheby_link::native_libs`]); a symbol that it cannot find and
//!   that only unreached functions of whole modules name gets a hidden
//!   stub in the program object, which calls `cheby_rt_unresolved`
//!   (D-943, [`ProgramSpec::stubs`]);
//! - the constants `main` reaches come from their cache entries where they
//!   hit (D-977, D-1261), and the rest are evaluated as under `cheby run`,
//!   in a module of their own on `plain` code (D-978), whose errors stop
//!   the build; their images go into the program object's constants table
//!   ([`ProgramSpec::constants`]), which the executable's start installs
//!   before `main` (D-979, D-924);
//! - the runtime archive is found and its runtime ID checked (D-929,
//!   D-930).
//!
//! On Windows `cheby build --target native` is a "not available yet (M6)"
//! tool error (D-935), what remains of D-924's refusals.
//!
//! User layouts and `@std_hooks` reach executables through the module
//! records (D-919): each record lists its unit's types, constructors,
//! layout words, constructor statics and hooks, and the executable's
//! start registers them and writes the layout words and constructor
//! statics before `main` runs (D-920).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use cheby_codegen_clif::record::{CodeRecord, RecordingModule, SymbolLinkage};
use cheby_codegen_clif::{UnitCx, compile_unit, isa, mangle};
use cheby_iface::{DefId, Vis};
use cheby_link::program::{PROGRAM_OBJECT_NAME, ProgramSpec};
use cheby_mir::{MirModule, UnitKind};
use cheby_package::{Manifest, ModulePath, Target};

use crate::{Cache, CheckOutput, Error, backend, frontend};

/// What `cheby build` builds (D-937).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildOptions {
    /// The module whose `main` is built, such as `my_app::tools::migrate`;
    /// the root module if `None` (D-137, D-464).
    pub module_path: Option<String>,
    /// `-o PATH`, already absolute; `build/native/<name>` under the package
    /// root if `None`.
    pub output: Option<PathBuf>,
    /// `--deny-warnings`: a warning stops the build before anything is
    /// linked (D-549).
    pub deny_warnings: bool,
}

/// The outcome of [`build_cached`].
#[derive(Debug)]
pub enum BuildOutcome {
    /// Diagnostics with an error (E0053 included), or a denied warning:
    /// nothing was linked.
    CompileFailed(CheckOutput),
    /// The executable is in place; `output` holds the warnings, printed
    /// before the link.
    Linked {
        output: CheckOutput,
        executable: PathBuf,
    },
}

/// The target `cheby build` builds for (D-937): `requested`, which the
/// manifest must allow, with `cheby check`'s tool error otherwise (D-550),
/// or without `--target`, `native` when the manifest allows it and `js`
/// otherwise. A manifest that cannot be read gives `requested`, or
/// `native`: the build itself then reports the manifest's diagnostics.
///
/// # Errors
///
/// The tool error for a target the manifest leaves out.
pub fn build_target(root: &Path, requested: Option<Target>) -> Result<Target, Error> {
    let mut sources = cheby_diag::SourceMap::new();
    let mut diagnostics = Vec::new();
    let manifest = Manifest::load(root, &mut sources, &mut diagnostics);
    let targets = match manifest {
        Ok(Some(manifest)) if !diagnostics.iter().any(cheby_diag::Diagnostic::is_error) => {
            manifest.targets
        }
        _ => return Ok(requested.unwrap_or(Target::Native)),
    };
    match requested {
        Some(target) if !targets.contains(target) => Err(Error::new(format!(
            "the package does not target `{target}`"
        ))),
        Some(target) => Ok(target),
        None if targets.contains(Target::Native) => Ok(Target::Native),
        None => Ok(Target::Js),
    }
}

/// Builds the package at `root` into an executable through the build
/// cache, as `cheby build` runs it (D-924, D-933, D-937).
///
/// # Errors
///
/// A tool error for a malformed or missing module path or `main` (as
/// `prepare_run`), a missing or stale runtime archive (D-929, D-930), a
/// missing `cc` or a failed link (D-933), a program `cheby build` cannot
/// build yet (D-924: one that reaches a constant, until K5a), Windows
/// (D-935, until M6); an [`Error`] for internal failures. User layouts and
/// hooks are no refusal: the module records carry them (D-919, D-920).
pub fn build_cached(
    root: &Path,
    options: &BuildOptions,
    cache: &Cache,
) -> Result<BuildOutcome, Error> {
    build_cached_with(root, options, cache, &mut |_| {})
}

/// [`build_cached`], calling `before_link` with the build's warnings once
/// everything but the link has succeeded, so that `cheby build` prints
/// them before the link, which may still fail (D-937).
///
/// # Errors
///
/// As [`build_cached`].
pub fn build_cached_with(
    root: &Path,
    options: &BuildOptions,
    cache: &Cache,
    before_link: &mut dyn FnMut(&CheckOutput),
) -> Result<BuildOutcome, Error> {
    // The runtime archive first, before any work it would waste (D-929,
    // D-930); Windows has no executables yet (D-935), whatever it holds.
    let archive = if cfg!(windows) {
        None
    } else {
        let override_path = std::env::var_os(cheby_link::archive::ARCHIVE_VAR);
        let exe = std::env::current_exe()
            .map_err(|e| Error::new(format!("cannot find the running `cheby`: {e}")))?;
        let archive = cheby_link::archive::find_archive(override_path.as_deref(), &exe)
            .map_err(link_error)?;
        cheby_link::archive::check_runtime_id(&archive).map_err(link_error)?;
        Some(archive)
    };
    let (output, built) = match build_objects(root, options, Some(cache))? {
        Objects::CompileFailed(output) => return Ok(BuildOutcome::CompileFailed(output)),
        Objects::Ready { output, built } => (output, built),
    };
    let archive = archive
        .ok_or_else(|| Error::from(cheby_iface::Ice::new("a build without a runtime archive")))?;
    if let Some(dir) = built.executable.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::new(format!("cannot create `{}`: {e}", dir.display())))?;
    }
    before_link(&output);
    let _link = crate::phases::span(crate::phases::Phase::Link);
    cheby_link::link::link(&cheby_link::link::LinkJob {
        objects: &built.objects,
        c_objects: &built.c_objects,
        libraries: &built.libraries,
        archive: &archive,
        output: &built.executable,
    })
    .map_err(link_error)?;
    Ok(BuildOutcome::Linked {
        output,
        executable: built.executable,
    })
}

/// What [`build_objects`] produced.
pub(crate) enum Objects {
    /// Diagnostics with an error, or a denied warning.
    CompileFailed(CheckOutput),
    /// The objects of the link, and the warnings.
    Ready { output: CheckOutput, built: Built },
}

/// The objects of one link and where the executable goes.
pub(crate) struct Built {
    /// `(module path, object)`: each code unit's object in build order,
    /// then the program object under [`PROGRAM_OBJECT_NAME`].
    pub objects: Vec<(String, Vec<u8>)>,
    /// The package's C objects in `sources` order, the same cache entries
    /// `cheby run` loads (D-940, D-942): linked after the program object.
    pub c_objects: Vec<(String, Vec<u8>)>,
    /// The package's `libraries`, linked as `-l<name>` (D-940).
    pub libraries: Vec<String>,
    /// The executable's absolute path.
    pub executable: PathBuf,
    /// Each code unit's module path (std's included) and K8's
    /// `seal::record_hash` of the record the build replayed, in build
    /// order: the same-code test compares them with `cheby run`'s (D-914).
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "read by the same-code test (`build_cache_tests.rs`)"
        )
    )]
    pub record_hashes: Vec<(String, crate::cache::Hash128)>,
}

/// Everything [`build_cached`] does before it looks for the runtime
/// archive and links: the front end, lowering, the code records, the
/// checks before linking, the objects (module documentation).
///
/// With a cache the build is warm (D-1250, N9): its front end checks only
/// what its build needs, and a build that then wants the body of a module
/// it did not check starts again with every module checked, the log back
/// at its mark (D-1256), as `cheby run`'s does.
pub(crate) fn build_objects(
    root: &Path,
    options: &BuildOptions,
    cache: Option<&Cache>,
) -> Result<Objects, Error> {
    if cfg!(windows) {
        return Err(Error::new(
            "`cheby build --target native` is not available yet on Windows (planned for M6)"
                .to_owned(),
        ));
    }
    match build_once(root, options, cache, true) {
        Err(crate::Restart::Again(mark)) => {
            crate::rollback_log(cache, mark);
            build_once(root, options, cache, false).map_err(crate::Restart::into_error)
        }
        other => other.map_err(crate::Restart::into_error),
    }
}

/// One attempt of [`build_objects`]: `warm` as `cheby run`'s first
/// attempt, or every module checked.
#[expect(
    clippy::too_many_lines,
    reason = "one build's steps in order, as the module documentation lists them"
)]
fn build_once(
    root: &Path,
    options: &BuildOptions,
    cache: Option<&Cache>,
    warm: bool,
) -> Result<Objects, crate::Restart> {
    let mark = crate::log_mark(cache);
    let mut out = CheckOutput {
        deny_warnings: options.deny_warnings,
        ..CheckOutput::default()
    };
    let front = crate::front_end_family(
        root,
        frontend::Mode::Build,
        crate::cache::index::CommandFamily::Build,
        None,
        &mut out,
        cache,
        warm.then_some((false, options.module_path.as_deref())),
    )?;
    out.sources.sort(&mut out.diagnostics);
    let Some((front, package)) = front.filter(|_| !out.has_errors()) else {
        return Ok(Objects::CompileFailed(out));
    };
    let path = match &options.module_path {
        Some(text) => crate::parse_module_path(text)
            .ok_or_else(|| Error::new(format!("`{text}` is not a module path")))?,
        None => ModulePath::root(&package),
    };
    let module = front
        .local_module(&path)
        .ok_or_else(|| Error::new(format!("there is no module `{path}` in this package")))?;
    let main = front
        .main_of(module)
        .ok_or_else(|| Error::new(format!("module `{path}` has no `main` function")))?;
    let executable = match &options.output {
        Some(output) => output.clone(),
        None => root.join("build").join("native").join(executable_name(
            &package,
            options.module_path.is_some(),
            &path,
        )),
    };
    let mir = backend::mir_options_from_env()?;
    // A warm build that wants a hit's body starts again (D-1256).
    let again = || warm && front.wanted_body();
    // D-914, D-1250 (N9): the units come through `cheby run`'s code records,
    // under its kind and keys: a package unit whose record hits is not
    // lowered, std's units come from std's code records (D-1264), and every
    // lookup is logged as `cheby run`'s are. A record that imports a symbol
    // D-943's build resolver cannot find is lowered after all, so E0053 is
    // decided on its MIR. GAP(build-from-records).
    let build_resolver = BuildResolver::new(front.native.library.as_deref());
    let found = |symbol: &str| build_resolver.resolves(symbol);
    let reading = backend::Reading {
        code: true,
        jit: true,
        resolves: Some(&found),
        reuse: false,
    };
    let lowered = match backend::lower_reading(
        &front,
        &out.sources,
        &backend::Roots::Main(main),
        mir,
        backend::command_scope(),
        reading,
    ) {
        Err(_) if again() => return Err(crate::Restart::Again(mark)),
        lowered => lowered.map_err(Error::from)?,
    };
    let compiled = compile_lowered(&front, &lowered, main.def, mir)?;
    // D-943: E0053 for the symbols reached functions name and the build
    // resolver cannot find; a stub in the program object for each one
    // only unreached functions of whole modules name.
    let reached = backend::reached_symbols(&lowered);
    let (missing, stubs): (Vec<String>, Vec<String>) = unresolved_symbols(
        &compiled.records,
        &compiled.entries,
        front.native.library.as_deref(),
    )
    .into_iter()
    .partition(|symbol| reached.contains(symbol));
    if !missing.is_empty() {
        let diagnostics = backend::unresolved(&front, &missing).map_err(Error::from)?;
        crate::report_back_end(&mut out, diagnostics);
        return Ok(Objects::CompileFailed(out));
    }
    // As under `cheby run`, the fresh units' records are stored once the
    // program has no E0053, before its constants (D-914): `cheby run` after
    // `cheby build` then misses nothing.
    store_fresh(&front, &compiled);
    // D-978, D-979: the constants `main` reaches go into the program object
    // as images, which the executable's start installs (D-924): K5b's
    // entries where they hit, so a warm build evaluates nothing and builds
    // no evaluation module (D-977, D-1261), and the misses evaluated in a
    // module of their own on `plain` code as under `cheby run`.
    // GAP(build-constants-from-entries).
    let images = match backend::constant_images(&front, &out.sources, &lowered, mir) {
        Err(_) if again() => return Err(crate::Restart::Again(mark)),
        images => images.map_err(Error::from)?,
    };
    let images = match images {
        Ok(images) => images,
        Err(diagnostics) => {
            crate::report_back_end(&mut out, diagnostics);
            return Ok(Objects::CompileFailed(out));
        }
    };
    let constants = cheby_runtime::consteval::image::encode_table(&images);
    let stacks = Stacks {
        main_stack: front.main_stack,
        fiber_stack: front.fiber_stack,
        unwind_edges: mir.unwind_edges,
    };
    let objects = write_objects(
        &compiled,
        &mangle::entry_main(&path.to_string()),
        stacks,
        &stubs,
        &constants,
    )?;
    let record_hashes = compiled
        .code
        .iter()
        .zip(&compiled.records)
        .map(|(unit, record)| {
            (
                unit.path.to_string(),
                crate::cache::seal::record_hash(record),
            )
        })
        .collect();
    Ok(Objects::Ready {
        output: out,
        built: Built {
            objects,
            c_objects: front.native.objects.clone(),
            libraries: front.native.libraries.clone(),
            executable,
            record_hashes,
        },
    })
}

/// The code records of one build: each code unit's, in build order, and
/// the entries unit's.
pub(crate) struct Compiled<'a> {
    pub code: Vec<CodeUnit<'a>>,
    pub records: Vec<CodeRecord>,
    pub entries: CodeRecord,
}

/// A code unit of [`Compiled`].
pub(crate) struct CodeUnit<'a> {
    /// Its module path.
    pub path: &'a str,
    /// For a package unit compiled fresh, the code key its record is
    /// stored under (`cheby run`'s, D-914); `None` for a unit from the
    /// store, a std unit or a build without a cache.
    store: Option<&'a crate::cache::Key>,
}

/// Compiles every code unit of `units` alone into its record as a JIT
/// host does ([`fresh_records`]), in parallel and taken in build order
/// (D-917, D-264), and the entries unit alone into its own (D-1110): a
/// build without the cache's lowering, for tests of the records.
#[cfg(test)]
pub(crate) fn compile_records<'a>(
    front: &frontend::Frontend,
    units: &'a [MirModule],
    main: DefId,
    mir: cheby_mir::MirOptions,
) -> Result<Compiled<'a>, Error> {
    let codegen = build_codegen(mir);
    let code: Vec<&MirModule> = units
        .iter()
        .filter(|unit| matches!(unit.unit, UnitKind::Code(_)))
        .collect();
    let entries = units
        .iter()
        .find(|unit| unit.unit == UnitKind::Entries)
        .ok_or_else(|| Error::from(cheby_iface::Ice::new("the build lowered no entries unit")))?;
    let records = fresh_records(&code, &codegen)?;
    let entries = entries_record(entries, &entries_cx(front, units, main), &codegen)?;
    Ok(Compiled {
        code: code
            .iter()
            .map(|unit| CodeUnit {
                path: &unit.path,
                store: None,
            })
            .collect(),
        records,
        entries,
    })
}

/// The records of a lowering through the cache ([`backend::lower_reading`]),
/// in build order ([`backend::Lowered::order`]): each unit from the store
/// as it is, each fresh code unit compiled alone as a JIT host compiles it
/// ([`fresh_records`]), so an executable's code is the code `cheby run`
/// loads (D-914), and the entries unit compiled alone under D-1110's rule
/// (it is never stored, D-964).
fn compile_lowered<'a>(
    front: &frontend::Frontend,
    lowered: &'a backend::Lowered,
    main: DefId,
    mir: cheby_mir::MirOptions,
) -> Result<Compiled<'a>, Error> {
    let codegen = build_codegen(mir);
    let mut fresh: Vec<&MirModule> = Vec::new();
    let mut entries = None;
    for placed in &lowered.order {
        if let backend::Placed::Fresh(i) = placed {
            let unit = lowered
                .units
                .get(*i)
                .ok_or_else(|| Error::from(cheby_iface::Ice::new("a lowered unit went missing")))?;
            match unit.unit {
                UnitKind::Code(_) => fresh.push(unit),
                UnitKind::Entries => entries = Some(unit),
                UnitKind::Tests(_) => {
                    return Err(cheby_iface::Ice::new("a build lowered a tests unit").into());
                }
            }
        }
    }
    let entries = entries
        .ok_or_else(|| Error::from(cheby_iface::Ice::new("the build lowered no entries unit")))?;
    let mut compiled = fresh_records(&fresh, &codegen)?.into_iter();
    let mut code = Vec::with_capacity(lowered.order.len());
    let mut records = Vec::with_capacity(lowered.order.len());
    for placed in &lowered.order {
        match placed {
            backend::Placed::Cached { unit, path, record } => {
                if !matches!(unit, UnitKind::Code(_)) {
                    return Err(cheby_iface::Ice::new("a build loaded a tests unit").into());
                }
                code.push(CodeUnit { path, store: None });
                records.push(record.clone());
            }
            backend::Placed::Fresh(i) => {
                let Some(unit) = lowered.units.get(*i) else {
                    continue;
                };
                if unit.unit == UnitKind::Entries {
                    continue;
                }
                let record = compiled.next().ok_or_else(|| {
                    Error::from(cheby_iface::Ice::new("a fresh unit was not compiled"))
                })?;
                code.push(CodeUnit {
                    path: &unit.path,
                    store: crate::code_cache::unit_key(lowered, unit).map(|(_, key)| key),
                });
                records.push(record);
            }
        }
    }
    let entries = entries_record(entries, &entries_cx(front, &lowered.units, main), &codegen)?;
    Ok(Compiled {
        code,
        records,
        entries,
    })
}

/// The code-generation options of every unit a build compiles, the
/// entries unit's included: a JIT host's (`cheby_jit`'s `Jit::configured`),
/// whose `checked_rc` follows the runtime's poisoning mode (`CHEBY_RT_POISON=1`
/// in a debug toolchain, D-565). The code keys hold that mode
/// (`checked_counts`), so a build under poisoning reads and writes the
/// checked records `cheby run` does, and its program object's local copies
/// of the trampolines equal the std records' (D-918). GAP(build-from-records).
fn build_codegen(mir: cheby_mir::MirOptions) -> cheby_codegen_clif::CodegenOptions {
    cheby_codegen_clif::CodegenOptions {
        checked_rc: cheby_jit::JitOptions::from_env().checked_rc,
        ..backend::codegen_options(mir, false)
    }
}

/// D-1110's answer for the entries unit of `units` with `main`: the
/// interfaces' non-`priv` items and every def another unit names.
fn entries_cx<'a>(
    front: &'a frontend::Frontend,
    units: &[MirModule],
    main: DefId,
) -> NamedOutside<'a> {
    NamedOutside {
        front,
        named: named_outside(units, main),
    }
}

/// [`entries_cx`]'s answer.
struct NamedOutside<'a> {
    front: &'a frontend::Frontend,
    named: BTreeSet<DefId>,
}

impl NamedOutside<'_> {
    fn contains(&self, def: DefId) -> bool {
        self.named.contains(&def) || front_named(self.front, def)
    }
}

/// `units`, code units, each compiled alone into its record as a JIT host
/// compiles a unit of its first batch (`cheby_jit`'s `compile_units`):
/// every item global hidden (D-1220) and an empty seed, in parallel on
/// rayon's global pool and taken in the order given (D-923, D-264). The
/// record is then the one `cheby run` stores under the unit's code key
/// (D-914), whichever command compiled it. GAP(build-from-records): this
/// refines D-1220's "`cheby build` keeps D-1110's rule" for code units.
fn fresh_records(
    units: &[&MirModule],
    codegen: &cheby_codegen_clif::CodegenOptions,
) -> Result<Vec<CodeRecord>, Error> {
    if units.is_empty() {
        return Ok(Vec::new());
    }
    let isa = isa::host_isa().map_err(|e| Error::new(e.to_string()))?;
    let seed = cheby_codegen_clif::jit_unit::Seed::default();
    let jobs: Vec<(&MirModule, Option<&cheby_codegen_clif::jit_unit::Seed>)> =
        units.iter().map(|&unit| (unit, Some(&seed))).collect();
    let ucx = UnitCx {
        named_outside: &|_| true,
    };
    cheby_codegen_clif::jit_unit::records_for_jit(&isa, &jobs, &ucx, codegen)
        .into_iter()
        .map(|unit| {
            unit.map(|unit| unit.record)
                .map_err(|e| Error::new(e.to_string()))
        })
        .collect()
}

/// Stores the record of each package unit `compiled` compiled fresh under
/// its code key, behind the command (D-1138), as `cheby run`'s JIT does
/// (`code_cache.rs`).
fn store_fresh(front: &frontend::Frontend, compiled: &Compiled<'_>) {
    let Some(sink) = crate::borrowing::sink(front) else {
        return;
    };
    for (unit, record) in compiled.code.iter().zip(&compiled.records) {
        if let Some(key) = unit.store {
            sink.code_store(key, record);
        }
    }
}

/// D-943's build resolver over the program's records: the runtime's
/// exported names, then the package's shared library, then the archive's
/// system libraries ([`cheby_link::native_libs::undefined`]), each answer
/// remembered.
struct BuildResolver<'a> {
    runtime: BTreeSet<&'static str>,
    package: Option<&'a cheby_jit::NativeLibrary>,
    known: std::sync::Mutex<std::collections::BTreeMap<String, bool>>,
}

impl<'a> BuildResolver<'a> {
    fn new(package: Option<&'a cheby_jit::NativeLibrary>) -> Self {
        Self {
            runtime: cheby_runtime::symbols()
                .iter()
                .map(|(name, _)| *name)
                .collect(),
            package,
            known: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Whether the link would find `symbol`.
    fn resolves(&self, symbol: &str) -> bool {
        if self.runtime.contains(symbol)
            || self
                .package
                .is_some_and(|library| library.lookup(symbol).is_some())
        {
            return true;
        }
        let Ok(mut known) = self.known.lock() else {
            return cheby_link::native_libs::undefined(&[symbol]).is_empty();
        };
        if let Some(&found) = known.get(symbol) {
            return found;
        }
        let found = cheby_link::native_libs::undefined(&[symbol]).is_empty();
        known.insert(symbol.to_owned(), found);
        found
    }
}

/// What the program image takes from the package and the build.
#[derive(Clone, Copy)]
struct Stacks {
    main_stack: u64,
    fiber_stack: u64,
    unwind_edges: bool,
}

/// The objects of the link: each record replayed under its module path,
/// in build order, then the program object (D-915) with D-943's `stubs`
/// (sorted) and the constants table `constants` (D-979). Debug toolchains
/// first run D-918's check over the local copies.
fn write_objects(
    compiled: &Compiled<'_>,
    entry: &str,
    stacks: Stacks,
    stubs: &[String],
    constants: &[u8],
) -> Result<Vec<(String, Vec<u8>)>, Error> {
    if cfg!(debug_assertions) {
        let mut copies: Vec<(&str, &CodeRecord)> = compiled
            .code
            .iter()
            .zip(&compiled.records)
            .map(|(unit, record)| (unit.path, record))
            .collect();
        copies.push((PROGRAM_OBJECT_NAME, &compiled.entries));
        cheby_link::link::check_copies(&copies).map_err(link_error)?;
    }
    let mut objects = Vec::with_capacity(compiled.records.len().saturating_add(1));
    let mut module_records = Vec::with_capacity(compiled.records.len());
    for (unit, record) in compiled.code.iter().zip(&compiled.records) {
        let isa = isa::host_isa().map_err(|e| Error::new(e.to_string()))?;
        let object = cheby_link::object::replay(record, isa, unit.path).map_err(link_error)?;
        objects.push((unit.path.to_string(), object));
        module_records.push(mangle::module_record(unit.path));
    }
    let spec = ProgramSpec {
        module_records: &module_records,
        entry,
        main_stack: stacks.main_stack,
        fiber_stack: stacks.fiber_stack,
        unwind_edges: stacks.unwind_edges,
        stubs,
        constants,
    };
    let isa = isa::host_isa().map_err(|e| Error::new(e.to_string()))?;
    let program =
        cheby_link::program::program_object(isa, &spec, &compiled.entries).map_err(link_error)?;
    objects.push((PROGRAM_OBJECT_NAME.to_owned(), program));
    Ok(objects)
}

/// The executable's file name under `build/native/` (D-937): the
/// package's name for the root module, else the module path with `::`
/// replaced by `-`, which no path segment holds.
fn executable_name(package: &str, module_given: bool, path: &ModulePath) -> String {
    if module_given {
        path.to_string().replace("::", "-")
    } else {
        package.to_owned()
    }
}

/// The front end's answer of D-1110: an item that is
/// not `priv` may be named by another unit; so may a def without an
/// interface item (a test).
fn front_named(front: &frontend::Frontend, def: DefId) -> bool {
    front
        .set
        .get(def.module)
        .and_then(|iface| iface.item(def.index))
        .is_none_or(|item| item.vis != Vis::Priv)
}

/// The defs other units name besides [`front_named`]'s
/// (D-1110): `main`, which the entries unit calls,
/// the constants the program reaches, whose initializers it calls, and
/// every item of a module that the module's own tests unit names (a
/// build has no tests unit today, D-924).
fn named_outside(units: &[MirModule], main: DefId) -> BTreeSet<DefId> {
    let mut named = BTreeSet::from([main]);
    for unit in units {
        match unit.unit {
            UnitKind::Entries => named.extend(unit.names.iter().map(|n| n.def)),
            UnitKind::Tests(module) => named.extend(
                unit.names
                    .iter()
                    .map(|n| n.def)
                    .filter(|def| def.module == module),
            ),
            UnitKind::Code(_) => {}
        }
    }
    named
}

/// The entries unit compiled alone into a record that is never cached
/// (D-964): its thunks and trampolines are local.
fn entries_record(
    unit: &MirModule,
    named: &NamedOutside<'_>,
    codegen: &cheby_codegen_clif::CodegenOptions,
) -> Result<CodeRecord, Error> {
    let ucx = UnitCx {
        named_outside: &|def: DefId| named.contains(def),
    };
    let isa = isa::host_isa().map_err(|e| Error::new(e.to_string()))?;
    let mut module = RecordingModule::new(isa);
    compile_unit(&mut module, unit, &ucx, codegen).map_err(|e| Error::new(e.to_string()))?;
    module.finish().map_err(|e| Error::new(e.to_string()))
}

/// The native symbols the records import that D-943's build resolver
/// cannot find, sorted: every imported name outside the generated ones
/// (`cb$…`) is a runtime or foreign symbol, named only by an import word
/// (D-917). The runtime's exported names come first, then the package's
/// shared library (its C code and its `libraries`, through its `dlopen`
/// handle, D-941), then the archive's system libraries (D-943, D-1113);
/// never the `cheby` process.
fn unresolved_symbols(
    records: &[CodeRecord],
    entries: &CodeRecord,
    package: Option<&cheby_jit::NativeLibrary>,
) -> Vec<String> {
    let runtime = cheby_runtime::symbols();
    let mut foreign: BTreeSet<&str> = BTreeSet::new();
    for record in records.iter().chain(std::iter::once(entries)) {
        for symbol in &record.symbols {
            if symbol.linkage == SymbolLinkage::Import
                && !symbol.name.starts_with(mangle::PREFIX)
                && !runtime.iter().any(|(name, _)| *name == symbol.name)
                && package.is_none_or(|library| library.lookup(&symbol.name).is_none())
            {
                foreign.insert(&symbol.name);
            }
        }
    }
    let foreign: Vec<&str> = foreign.into_iter().collect();
    cheby_link::native_libs::undefined(&foreign)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// A failure of `cheby_link`: a tool error's text as it is, an internal
/// failure as an internal error.
#[expect(
    clippy::needless_pass_by_value,
    reason = "passed to `map_err`, which hands over the error"
)]
fn link_error(error: cheby_link::LinkError) -> Error {
    if error.is_tool_error() {
        Error::new(error.to_string())
    } else {
        Error::from(cheby_iface::Ice::new(error.to_string()))
    }
}
