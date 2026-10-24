//! `cheby build` (D-924, D-937): the package's `main` into an executable,
//! through the build cache's front records and in-memory code records.
//!
//! The front end runs as `cheby run` runs it (D-955), `main` is chosen by
//! `cheby run`'s rules (D-137, D-464), and what `main` reaches is lowered
//! as `cheby run` lowers it (D-510). Each code unit is then compiled alone
//! into a code record (D-914, D-917), every record is replayed into one
//! object under its module path, in build order (D-915), and the entries
//! unit, compiled alone too, goes into the program object with `cb$image`
//! and the C `main` (D-1111). `cc` links them
//! with the runtime archive (D-929, D-933). Code records are not cached
//! yet: the cache gets them with K4c, and `cheby build` reads them from
//! there with N9 (D-924).
//!
//! Before anything links, in this order:
//!
//! - a program that reaches a constant is a "not available yet" tool error
//!   (D-924, until constants reach executables);
//! - E0053 for every native symbol that a reached `@external` names and
//!   D-943's build resolver cannot find: the runtime's exported names,
//!   then each system library of the archive opened by its recorded file
//!   name ([`cheby_link::native_libs`]);
//! - the runtime archive is found and its runtime ID checked (D-929,
//!   D-930).
//!
//! On Windows `cheby build --target native` is a "not available yet (M6)"
//! tool error (D-935). Those two are what remains of D-924's refusals:
//! constants until K5a, Windows until M6.
//!
//! User layouts and `@std_hooks` reach executables through the module
//! records (D-919): each record lists its unit's types, constructors,
//! layout words, constructor statics and hooks, and the executable's
//! start registers them and writes the layout words and constructor
//! statics before `main` runs (D-920).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use cheby_codegen_clif::record::{CodeRecord, RecordingModule, SymbolLinkage, record_unit};
use cheby_codegen_clif::{UnitCx, compile_unit, isa, mangle};
use cheby_iface::{DefId, Vis};
use cheby_link::program::{PROGRAM_OBJECT_NAME, ProgramSpec};
use cheby_mir::{MirModule, UnitKind};
use cheby_package::{Manifest, ModulePath, Target};
use rayon::prelude::*;

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
    cheby_link::link::link(&cheby_link::link::LinkJob {
        objects: &built.objects,
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
    /// The executable's absolute path.
    pub executable: PathBuf,
}

/// Everything [`build_cached`] does before it looks for the runtime
/// archive and links: the front end, lowering, the code records, the
/// checks before linking, the objects (module documentation).
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
    let mut out = CheckOutput {
        deny_warnings: options.deny_warnings,
        ..CheckOutput::default()
    };
    let front = crate::front_end_for(root, frontend::Mode::Build, None, &mut out, cache)?;
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
    let lowered = backend::lower(&front, &out.sources, &backend::Roots::Main(main), mir)?;
    if let Some(&constant) = lowered.reach.constant_order().first() {
        let name = lowered
            .units
            .iter()
            .find_map(|unit| unit.name(constant).ok())
            .map_or_else(|| "a constant".to_owned(), |n| format!("`{}`", n.path()));
        return Err(Error::new(format!(
            "`cheby build` is not available yet for programs that use constants, such as {name} \
             (planned for a later step of M3)"
        )));
    }
    let compiled = compile_records(&front, &lowered.units, main.def, mir)?;
    let missing = unresolved_symbols(&compiled.records, &compiled.entries);
    if !missing.is_empty() {
        let diagnostics = backend::unresolved(&front, &missing)?;
        crate::report_back_end(&mut out, diagnostics);
        return Ok(Objects::CompileFailed(out));
    }
    let stacks = Stacks {
        main_stack: front.main_stack,
        fiber_stack: front.fiber_stack,
        unwind_edges: mir.unwind_edges,
    };
    let objects = write_objects(&compiled, &mangle::entry_main(&path.to_string()), stacks)?;
    Ok(Objects::Ready {
        output: out,
        built: Built {
            objects,
            executable,
        },
    })
}

/// The code records of one build: each code unit's, in build order, and
/// the entries unit's.
pub(crate) struct Compiled<'a> {
    pub code: Vec<&'a MirModule>,
    pub records: Vec<CodeRecord>,
    pub entries: CodeRecord,
}

/// Compiles every code unit of `units` alone into its record, in
/// parallel and taken in build order (D-917, D-264), and the entries unit
/// alone into its own.
pub(crate) fn compile_records<'a>(
    front: &frontend::Frontend,
    units: &'a [MirModule],
    main: DefId,
    mir: cheby_mir::MirOptions,
) -> Result<Compiled<'a>, Error> {
    let named = named_outside(units, main);
    let ucx = UnitCx {
        named_outside: &|def: DefId| named.contains(&def) || front_named(front, def),
    };
    let codegen = backend::codegen_options(mir, false);
    let code: Vec<&MirModule> = units
        .iter()
        .filter(|unit| matches!(unit.unit, UnitKind::Code(_)))
        .collect();
    let entries = units
        .iter()
        .find(|unit| unit.unit == UnitKind::Entries)
        .ok_or_else(|| Error::from(cheby_iface::Ice::new("the build lowered no entries unit")))?;
    let compiled: Vec<Result<CodeRecord, Error>> = code
        .par_iter()
        .map(|unit| {
            let isa = isa::host_isa().map_err(|e| Error::new(e.to_string()))?;
            record_unit(isa, unit, &ucx, &codegen).map_err(|e| Error::new(e.to_string()))
        })
        .collect();
    let records = compiled.into_iter().collect::<Result<Vec<_>, _>>()?;
    let entries = entries_record(entries, &ucx, &codegen)?;
    Ok(Compiled {
        code,
        records,
        entries,
    })
}

/// What the program image takes from the package and the build.
#[derive(Clone, Copy)]
struct Stacks {
    main_stack: u64,
    fiber_stack: u64,
    unwind_edges: bool,
}

/// The objects of the link: each record replayed under its module path,
/// in build order, then the program object (D-915). Debug toolchains first
/// run D-918's check over the local copies.
fn write_objects(
    compiled: &Compiled<'_>,
    entry: &str,
    stacks: Stacks,
) -> Result<Vec<(String, Vec<u8>)>, Error> {
    if cfg!(debug_assertions) {
        let mut copies: Vec<(&str, &CodeRecord)> = compiled
            .code
            .iter()
            .zip(&compiled.records)
            .map(|(unit, record)| (&*unit.path, record))
            .collect();
        copies.push((PROGRAM_OBJECT_NAME, &compiled.entries));
        cheby_link::link::check_copies(&copies).map_err(link_error)?;
    }
    let mut objects = Vec::with_capacity(compiled.records.len().saturating_add(1));
    let mut module_records = Vec::with_capacity(compiled.records.len());
    for (unit, record) in compiled.code.iter().zip(&compiled.records) {
        let isa = isa::host_isa().map_err(|e| Error::new(e.to_string()))?;
        let object = cheby_link::object::replay(record, isa, &unit.path).map_err(link_error)?;
        objects.push((unit.path.to_string(), object));
        module_records.push(mangle::module_record(&unit.path));
    }
    let spec = ProgramSpec {
        module_records: &module_records,
        entry,
        main_stack: stacks.main_stack,
        fiber_stack: stacks.fiber_stack,
        unwind_edges: stacks.unwind_edges,
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
    ucx: &UnitCx<'_>,
    codegen: &cheby_codegen_clif::CodegenOptions,
) -> Result<CodeRecord, Error> {
    let isa = isa::host_isa().map_err(|e| Error::new(e.to_string()))?;
    let mut module = RecordingModule::new(isa);
    compile_unit(&mut module, unit, ucx, codegen).map_err(|e| Error::new(e.to_string()))?;
    module.finish().map_err(|e| Error::new(e.to_string()))
}

/// The native symbols the records import that D-943's build resolver
/// cannot find, sorted: every imported name outside the generated ones
/// (`cb$…`) is a runtime or foreign symbol, named only by an import word
/// (D-917). The runtime's exported names come first, then the archive's
/// system libraries.
fn unresolved_symbols(records: &[CodeRecord], entries: &CodeRecord) -> Vec<String> {
    let runtime = cheby_runtime::symbols();
    let mut foreign: BTreeSet<&str> = BTreeSet::new();
    for record in records.iter().chain(std::iter::once(entries)) {
        for symbol in &record.symbols {
            if symbol.linkage == SymbolLinkage::Import
                && !symbol.name.starts_with(mangle::PREFIX)
                && !runtime.iter().any(|(name, _)| *name == symbol.name)
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
