//! REPL sessions (§13.5, G5, plan §8.3): the driver side of `cheby repl`.
//!
//! A [`Session`] holds the package's front end, the session's front-end
//! state ([`ReplSession`]), one [`Jit`] and one [`Runtime`] for the whole
//! session (D-712, D-690), and what it compiled and evaluated so far.
//! [`Session::eval`] takes one entry:
//!
//! 1. Its text becomes the source file `<repl:N>`. Declarations and
//!    imports are parsed as a module; statements inside the wrapper
//!    `test "<repl>" {` … `Nil }` (`cheby_types::repl`), whose spans move
//!    back to the entry's own text.
//! 2. It is checked as the module `<repl:N>` against the package, std and
//!    the session's names, reading only interfaces (D-246).
//! 3. What it reaches and nothing compiled yet is lowered and compiled in
//!    one JIT batch, all or nothing: its items, the items of the package
//!    and std it reaches for the first time, its body and its thunk
//!    `cb$entry$repl$<N>`, and the thunks of new constants. Its items carry
//!    their session names and generations (D-547, E4): a redefinition with
//!    the same signature takes over the old slot, another gets a new one.
//! 4. New constants are evaluated (E6); the body runs as a root fiber of
//!    the session's runtime, on the package's `main-stack` (§13.5.4, D-660,
//!    `EntryMode::Repl`), in the session's current group of detached
//!    fibers (D-799), which keep running after the entry (D-194). A panic
//!    is printed by the runtime, and the entry unwinds by the return flag:
//!    its pads release what its frames held (D-712, D-733). No leak check
//!    runs (D-458). Where fibers are not supported (Windows until M6,
//!    D-714), the body runs isolated on its own thread instead.
//! 5. The value of a final expression prints as `<debug text> : <type>`,
//!    nothing for `Nil` (D-546); what the entry returned is marked shared
//!    (D-705, D-712), and the values of the entry's `let`s go in their
//!    slots with release stores (D-547). Only then is the entry committed:
//!    its names join the session. An entry rejected after its code
//!    compiled gives the slots it repointed back.
//! 6. The definitions that used something the entry redefined with another
//!    signature are checked again from their own text; those that still
//!    check are compiled again and take over their slots, and the others
//!    are reported and keep their old code (D-043, §13.5.2, D-609).
//!
//! Besides that:
//!
//! - Interface satisfaction counts the prompt entries as one module
//!   (D-610): after each commit, forwarders are added to session types'
//!   entry modules ([`cheby_types::repl::ReplSession::satisfaction`]).
//! - Two live bindings of one native symbol with different C signatures
//!   are E0054 (D-475); replaced definitions and failed entries do not
//!   count.
//! - Values and diagnostics write a type that a redefinition or `:reload`
//!   replaced with ` (stale)` after it (§13.5.5).
//! - The commands (§13.5.3): [`Session::type_of`] (`:type`),
//!   [`Session::enter_module`] (`:module`) and [`Session::reload`]
//!   (`:reload`), which first tears the session's detached fibers down.
//! - The session's end ([`Session::end`], or dropping it) tears down the
//!   detached fibers of the current group and of every group a teardown
//!   left stuck, then shuts the runtime down (§13.5.4, D-712). A
//!   termination (D-709) between entries ends the process before the next
//!   prompt ([`Session::exit_if_terminated`]).
//!
//! The session's front end loads every shipped std module (a synthetic
//! package module imports them all), so any std import resolves without
//! loading modules later. Diagnostics and panic reports follow the output
//! written before them: [`Output::stderr`] flushes standard output first
//! (D-548).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_codegen_clif::SessionName;
use cheby_consteval::{ConstEvalConfig, ConstInfo, ConstMode};
use cheby_diag::{Colors, Diagnostic, FileId, SourceMap, Span, TextRange, codes};
use cheby_iface::{
    DefId, DefIndex, DepView, DisplayCx, Ice, InterfaceSet, ItemSig, ModuleId, ModuleInfo,
    ModuleInterface, Name, Namespace, PackageId, Vis,
};
use cheby_jit::{Jit, JitError, JitOptions, SlotHandle};
use cheby_mir::reach::{Reach, Root, reachable_with_hooks};
use cheby_mir::repr::foreign_sig_with_context;
use cheby_mir::{
    Defs, EntryKind, EntryRequest, LowerCx, MirModule, MirOptions, Program, count_and_check_with,
    lower_entries, lower_items, lower_tests,
};
use cheby_package::{Manifest, ModulePath, Target, Targets};
use cheby_resolve::repl::{Entered, SessionScope};
use cheby_resolve::{
    Lookup, ModuleLookup, ResolveInput, ResolvedModule, StdCatalog, closest_name, collect_imports,
    cycle_diagnostic, resolve_import_targets,
};
use cheby_runtime::{
    DetachedGroup, EntryConfig, EntryMode, EntryOutcome, Runtime, RuntimeConfig, RuntimeError,
};
use cheby_syntax::SyntaxTree;
use cheby_syntax::ast::{AstNode, SourceFile};
use cheby_types::repl::{
    CheckedEntry, EntryShape, Redefinition, ReplSession, SatisfactionPlan, forwarder_fn,
};
use cheby_types::thir;

use crate::frontend::{self, Frontend, NativeBinding};
use crate::{
    Error, Package, ParsedFile, find_package_root, parse_file, render_diagnostics, std_lib,
};

/// The wrapper's text before a statements entry (`cheby_types::repl`).
const BODY_PREFIX: &str = "test \"<repl>\" {\n";

/// The wrapper's text after a statements entry.
const BODY_SUFFIX: &str = "\nNil\n}\n";

/// The wrapper's text after a statements entry with grammar errors.
const BODY_CLOSE: &str = "\n}\n";

/// The display path of the session's own source: the module that imports
/// every shipped std module, and the target of synthetic imports.
const SESSION_FILE: &str = "<repl>";

/// How a session runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionOptions {
    /// `--deny-warnings`: an entry with a warning is refused (D-549).
    pub deny_warnings: bool,
    /// Whether diagnostics are colored (D-467).
    pub colors: Colors,
    /// The number of worker threads of the session's runtime; `None`
    /// reads `CHEBY_THREADS` (D-658), as `cheby repl` does. In-process
    /// hosts that start several sessions at once, such as tests, bound it
    /// (architecture §3.7).
    pub threads: Option<usize>,
}

/// Where an entry's output goes.
pub trait Output {
    /// Writes to standard output: a value line.
    fn stdout(&mut self, text: &str);
    /// Writes to standard error: diagnostics. Standard output is flushed
    /// first, so the report follows what the entry printed (D-548).
    fn stderr(&mut self, text: &str);
}

/// The outcome of [`Session::start`].
pub enum Start {
    /// The session is ready for entries; the text holds the package's
    /// warnings, rendered.
    Ready(Box<Session>, String),
    /// The package has errors, rendered.
    Failed(String),
}

impl std::fmt::Debug for Start {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready(..) => f.write_str("Ready"),
            Self::Failed(text) => f.debug_tuple("Failed").field(text).finish(),
        }
    }
}

/// A REPL session.
pub struct Session {
    options: SessionOptions,
    /// The MIR options every entry and package module is lowered with,
    /// read once when the session starts (`CHEBY_RC`, plan §2.3), with
    /// borrowing off (D-808, D-971); std's units borrow
    /// ([`Session::lower_std`]).
    mir: MirOptions,
    sources: SourceMap,
    front: Frontend,
    repl: ReplSession,
    jit: Jit,
    /// The checked bodies of committed entry modules.
    entries: BTreeMap<ModuleId, thir::Module>,
    /// Items lowered and compiled into the JIT.
    compiled: BTreeSet<DefId>,
    /// Modules whose code unit, which registers their constructor layouts,
    /// was compiled.
    units: BTreeSet<ModuleId>,
    /// Constants evaluated.
    evaluated: BTreeSet<DefId>,
    /// Constants whose entry thunk is compiled into the JIT. It outlives a
    /// failed evaluation: a constant that an entry's failed evaluation
    /// reached (a standard-library constant it forced, say) is not
    /// evaluated, so the next entry that reaches it evaluates it again with
    /// the thunk it already has.
    const_thunks: BTreeSet<DefId>,
    /// Entries read so far: `N` of the next `<repl:N>`.
    count: u32,
    /// Definitions checked again so far, to name their modules.
    again: u32,
    /// What `std::…` paths an import may name (D-451).
    known: Vec<&'static str>,
    /// Every word an entry returned: its bindings live in session slots,
    /// so it is never freed (D-458: no leak check in the REPL). Each is
    /// marked shared before its bindings are bound (D-705, D-712). Since
    /// session values are never dropped, D-712's teardown "before any
    /// session value is dropped" holds at every teardown.
    kept: Vec<u64>,
    /// The session's runtime (D-690, D-712): every entry runs as a root
    /// fiber of it, and the detached fibers an entry starts keep running
    /// across entries (D-194). It holds the JIT's code token, so the code
    /// stays alive while its fibers may run it (D-708). `None` where fibers
    /// are not supported (Windows until M6, D-714): entries then run
    /// isolated on the thread backend; and after a termination, once the
    /// halt has completed.
    runtime: Option<Runtime>,
    /// The group of the detached fibers that entries start now (D-799).
    /// Each teardown (`:reload`) opens a fresh one, so detached fibers
    /// started later never join a group that a teardown left closing
    /// (D-800, D-827).
    group: Arc<DetachedGroup>,
    /// Groups whose teardown skipped members blocked in a system call
    /// (D-786, D-804): they stay closing, and the session's end tears them
    /// down again.
    stuck: Vec<Arc<DetachedGroup>>,
    /// While a definition is checked again: the note its errors get.
    again_note: Option<String>,
    /// The package's root directory; `None` outside a package.
    root: Option<PathBuf>,
    /// The native symbols that session items bind (D-475).
    natives: Vec<SessionNative>,
    /// Modules `:reload` replaced, with the module that replaced each.
    replaced: BTreeMap<ModuleId, ModuleId>,
    /// How many times `:reload` changed something: the generation of its
    /// modules (D-547).
    reloads: u32,
    /// The generation of the package's native code that code compiled now
    /// calls: 0 at the start, then the JIT's number for the library each
    /// `:reload` that changed a C source loaded (D-712, D-941,
    /// [`cheby_jit::generation_symbol`]).
    native_generation: u32,
    /// The build cache std's code is loaded from (K6, D-971, D-980): std's
    /// records, which std's first use writes; nothing of the session is
    /// stored (D-964). `None` compiles std in the session, as before.
    std_store: Option<crate::Cache>,
}

/// A std module's code record a session loads (K6): its module, path and
/// record.
type StdRecord = (ModuleId, String, cheby_codegen_clif::record::CodeRecord);

/// A native symbol that an `@external(native, …)` of a session item
/// binds: live while the item is the session's definition of its name
/// (D-475).
struct SessionNative {
    def: DefId,
    name: Name,
    symbol: Box<str>,
    binding: NativeBinding,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
}

impl Drop for Session {
    /// Ends the session ([`Session::end`]) if its host did not: its
    /// detached fibers are torn down and its runtime stops before the JIT
    /// goes.
    fn drop(&mut self) {
        let _ = self.end();
    }
}

/// The runtime of a session in the package `front` (D-690, D-712), with
/// `threads` workers (`None`: `CHEBY_THREADS`), the package's name as the
/// program name and its `fiber-stack` for spawned fibers (D-660); `None`
/// where fibers are not supported (Windows until M6, D-714), so entries
/// run isolated. The manifest's `[runtime]` table is read once, when the
/// session starts: `:reload` compares module files only (D-625) and
/// neither applies nor mentions a changed table
/// (D-830).
///
/// # Errors
///
/// An [`Error`] if the runtime's threads cannot start.
fn session_runtime(front: &Frontend, threads: Option<usize>) -> Result<Option<Runtime>, Error> {
    let config = RuntimeConfig {
        threads,
        args: Vec::new(),
        program_name: front.package_name.clone(),
        fiber_stack: crate::stack_bytes(front.fiber_stack),
        signals: false,
    };
    match Runtime::new(&config) {
        Ok(runtime) => Ok(Some(runtime)),
        Err(RuntimeError::Unsupported) => Ok(None),
        Err(error) => Err(Error::new(error.to_string())),
    }
}

/// An entry read and parsed, before it is checked.
struct ReadEntry {
    /// `<repl:N>`.
    name: String,
    number: u32,
    parsed: ParsedFile,
    shape: EntryShape,
    /// Its grammar and validation diagnostics.
    diagnostics: Vec<Diagnostic>,
}

/// What checking an entry gave.
struct Entry {
    checked: CheckedEntry,
    /// The entry's number, `N` of `<repl:N>`.
    number: u32,
}

impl Session {
    /// Starts a session in `dir`: inside a package (`dir` or an ancestor
    /// holds `cheby.toml`), its modules are loaded and can be imported
    /// (D-098); elsewhere the session has no package. Every shipped std
    /// module is loaded.
    ///
    /// # Errors
    ///
    /// An [`Error`] for failures that are not diagnostics, among them an
    /// invalid setting in the environment: `CHEBY_THREADS` (D-658), checked
    /// first even when `options.threads` sets the count, and the
    /// compiler's `CHEBY_RC` and `CHEBY_UNWIND_EDGES`; or if the session's
    /// runtime cannot start its threads.
    pub fn start(dir: &Path, options: SessionOptions) -> Result<Start, Error> {
        Self::start_cached(dir, options, None)
    }

    /// [`Session::start`], loading std's code from `cache`'s records (K6,
    /// D-971, D-980): the first batch that needs std loads every std
    /// module's record, which std's first use writes when one is missing;
    /// nothing of the session is stored (D-964). [`Session::end`] finishes
    /// the cache (its `CHEBY_CACHE_LOG` and its writes).
    ///
    /// # Errors
    ///
    /// As [`Session::start`].
    pub fn start_cached(
        dir: &Path,
        options: SessionOptions,
        cache: Option<crate::Cache>,
    ) -> Result<Start, Error> {
        cheby_runtime::sched::threads_from_env().map_err(|e| Error::new(e.to_string()))?;
        // No borrowing in any package unit a session lowers (D-715, D-808):
        // a later batch could tail-call a function compiled with a mask
        // its slot could not change. Each such unit is counted on its own,
        // which never infers; the switch says so where the options are
        // made. Std runs its normal code, its masks inferred once over
        // all of std (D-971, `Session::lower_std`).
        let mir = MirOptions {
            borrow: false,
            ..crate::backend::mir_options_from_env()?
        };
        let codegen = crate::backend::codegen_options(mir, false);
        let mut sources = SourceMap::new();
        let mut diagnostics = Vec::new();
        let root = find_package_root(dir);
        let package = match &root {
            Some(root) => crate::load_package(root, &mut sources, &mut diagnostics)?,
            None => Some(no_package(dir, &mut sources, &mut diagnostics)?),
        };
        let render = |diagnostics: &mut Vec<Diagnostic>, sources: &SourceMap| {
            sources.sort(diagnostics);
            render_diagnostics(diagnostics, sources, options.colors, false)
        };
        let Some(mut package) = package.filter(|_| !diagnostics.iter().any(Diagnostic::is_error))
        else {
            return Ok(Start::Failed(render(&mut diagnostics, &sources)));
        };
        let (std_module, synthetic) = std_module(&package, &mut sources, &mut diagnostics)?;
        let ast = synthetic_import(&std_module)?;
        package.modules.push(std_module);
        let front = frontend::run(
            package,
            frontend::Mode::Build,
            &mut sources,
            &mut diagnostics,
        )?;
        let synthetic_span = Span::new(synthetic, TextRange::empty(0));
        diagnostics.retain(|d| d.primary_span().is_none_or(|s| s.file != synthetic));
        if diagnostics.iter().any(Diagnostic::is_error) {
            return Ok(Start::Failed(render(&mut diagnostics, &sources)));
        }
        let warnings = render(&mut diagnostics, &sources);
        // `env::program_name()` is the package's name, as under `cheby
        // run`, and `env::args()` is empty (D-578, D-612): for the
        // runtime, which keeps its own (D-690), and for isolated entries
        // (constant evaluation, and every entry on the thread backend).
        cheby_runtime::entry::set_program(Vec::new(), front.package_name.clone());
        let mut jit = Jit::configured(codegen, JitOptions::from_env())?;
        // The package's native code, loaded by the front end (D-941).
        for library in front.native_libraries() {
            jit.add_library(library)?;
        }
        let runtime = session_runtime(&front, options.threads)?;
        if let Some(runtime) = &runtime {
            runtime.attach(jit.code_token());
        }
        let session = Self {
            options,
            mir,
            sources,
            repl: ReplSession::new(SessionScope::new(synthetic_span, ast)),
            front,
            jit,
            entries: BTreeMap::new(),
            compiled: BTreeSet::new(),
            units: BTreeSet::new(),
            evaluated: BTreeSet::new(),
            const_thunks: BTreeSet::new(),
            count: 0,
            again: 0,
            known: std_lib::known(),
            kept: Vec::new(),
            again_note: None,
            root,
            natives: Vec::new(),
            replaced: BTreeMap::new(),
            reloads: 0,
            native_generation: 0,
            runtime,
            group: DetachedGroup::new(),
            stuck: Vec::new(),
            std_store: cache,
        };
        Ok(Start::Ready(Box::new(session), warnings))
    }

    /// The exit status that a termination fixed (D-709), such as a
    /// detached fiber's failed write to standard error (D-306), or `None`
    /// while none happened. The REPL reads it before each prompt and after
    /// each line it reads (D-828).
    #[must_use]
    pub fn exit_status(&self) -> Option<i32> {
        self.runtime.as_ref().and_then(Runtime::exit_status)
    }

    /// Ends the process if a termination happened while no entry ran: the
    /// session ends before the next prompt with the status the termination
    /// fixed and prints nothing more, after completing the halt (D-709,
    /// D-797, D-801, D-828).
    pub fn exit_if_terminated(&mut self) {
        if let Some(status) = self.exit_status() {
            self.exit_after_termination(status);
        }
    }

    /// Ends the process with `status` after a termination (D-709, D-797):
    /// completes the runtime's halt, which joins every thread except one
    /// blocked in a system call (D-786), then exits, dropping nothing:
    /// no session value, and not the JIT, whose code a thread still blocked
    /// may return into. On the thread backend, the entry's isolated thread
    /// is parked for good, so the code it runs is kept (leaked).
    fn exit_after_termination(&mut self, status: i32) -> ! {
        match self.runtime.take() {
            Some(runtime) => runtime.halt(),
            None => std::mem::forget(self.jit.code_token()),
        }
        std::process::exit(status)
    }

    /// Tears the session's detached fibers down (D-712, D-194): cancels
    /// and unwinds every member of the current group and waits until it is
    /// empty, running no user code (ADR-0029). Members blocked in a system
    /// call, on the blocking pool or inline, cannot be stopped: they are
    /// skipped and counted (D-786, D-804), and their group stays closing,
    /// so it goes to the stuck groups that the session's end tears down
    /// again. Detached fibers started later join a fresh group either way
    /// (D-827). A skipped fiber parked on a standard
    /// input read on the pool still takes the next line when its read
    /// returns, and drops it as it unwinds: the REPL's prompt and a
    /// session's detached readers share standard input in the order they
    /// ask (D-548, D-829). A teardown that meets a
    /// termination's halt ends the process with its status.
    fn tear_down_detached(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let report = runtime.teardown(&self.group);
        if report.halted {
            let status = runtime
                .exit_status()
                .unwrap_or(cheby_runtime::abi::status::ERR);
            self.exit_after_termination(status);
        }
        let old = std::mem::replace(&mut self.group, DetachedGroup::new());
        if old.live() > 0 {
            self.stuck.push(old);
        }
    }

    /// Ends the session (§13.5.4, D-712): tears down the detached fibers of
    /// the current group and of every stuck group, then shuts the runtime
    /// down, which joins its threads and releases the JIT's code token
    /// (D-708). Session values are never dropped (D-458), so they all
    /// outlive the teardown. `Some(status)` when a termination happened,
    /// before or during the teardown: the halt is then completed instead
    /// (D-797), and the host ends with that status, printing nothing more.
    /// Later calls do nothing; dropping the session calls it.
    pub fn end(&mut self) -> Option<i32> {
        // K6: std's records are written and the lookup log with them; a
        // failure only costs a later command a miss (D-1129).
        if let Some(cache) = self.std_store.take() {
            let _ = cache.finish(false);
        }
        let runtime = self.runtime.take()?;
        for group in std::iter::once(&self.group).chain(&self.stuck) {
            if runtime.teardown(group).halted {
                break;
            }
        }
        if let Some(status) = runtime.exit_status() {
            runtime.halt();
            return Some(status);
        }
        runtime.shutdown();
        None
    }

    /// The session's front-end state: its names, redefinitions and stale
    /// types (for the commands of W5-A).
    #[must_use]
    pub const fn repl(&self) -> &ReplSession {
        &self.repl
    }

    /// Checks, compiles and runs one entry, writing its value, warnings and
    /// errors to `out`. An entry with an error, or one that panics, changes
    /// nothing in the session. Text with nothing but blanks and comments
    /// is not an entry.
    ///
    /// # Errors
    ///
    /// An [`Error`] for failures that are not diagnostics (an internal
    /// compiler error).
    pub fn eval(&mut self, text: &str, out: &mut dyn Output) -> Result<(), Error> {
        let Some(decls) = entry_kind(text) else {
            return Ok(());
        };
        let read = self.read_entry(text, decls)?;
        let entry = self.check(
            &read.name,
            read.number,
            &read.parsed,
            read.shape,
            read.diagnostics,
            out,
        )?;
        let Some(entry) = entry else {
            return Ok(());
        };
        let changed: Vec<Redefinition> = entry.checked.redefinitions().collect();
        if !self.build_and_run(&entry, out)? {
            return Ok(());
        }
        self.commit(entry)?;
        self.check_again(&changed, out)
    }

    /// `:type expr` (§13.5.3, D-546): checks `expr` as the next entry,
    /// without running or keeping anything, and prints `<expr> : <type>`,
    /// the type of its final expression, as diagnostics write types. It
    /// takes an entry number, so its diagnostics have a place of their
    /// own (D-624). Only errors are shown.
    ///
    /// # Errors
    ///
    /// An [`Error`] for an internal compiler error.
    pub fn type_of(&mut self, expr: &str, out: &mut dyn Output) -> Result<(), Error> {
        let expr = expr.trim();
        if entry_kind(expr) != Some(false) {
            out.stderr(&self.tool_error("`:type` needs an expression, as in `:type 1 + 2`"));
            return Ok(());
        }
        let read = self.read_entry(expr, false)?;
        let module = self.add_entry_module(&read)?;
        let mut diagnostics = read.diagnostics;
        let mut checked = None;
        read.shape
            .move_diagnostics(read.parsed.file, &mut diagnostics);
        if let (Some(tree), false) = (&read.parsed.syntax, read.parsed.has_syntax_errors)
            && !diagnostics.iter().any(Diagnostic::is_error)
        {
            match self.check_tree(module, tree, read.shape)? {
                Ok(entry) => checked = Some(entry),
                Err(found) => diagnostics.extend(found),
            }
        }
        diagnostics.retain(Diagnostic::is_error);
        if !diagnostics.is_empty() {
            self.report(&mut diagnostics, out);
            return Ok(());
        }
        let Some(entry) = checked else {
            return Ok(());
        };
        match entry.body.as_ref().and_then(|body| body.last) {
            Some(ty) => {
                let shown = self.show_ty(ty, &entry.iface)?;
                out.stdout(&format!("{expr} : {shown}\n"));
            }
            None => {
                out.stderr(&self.tool_error(
                    "`:type` needs an expression, and the last statement here is not one",
                ));
            }
        }
        Ok(())
    }

    /// `:module path` (§13.5.3, D-098): later entries are checked as if they
    /// were written at the end of that module of the package or the
    /// standard library, so its `priv` items and its imports are in scope,
    /// and its package's package-visible items are usable. `:module` alone
    /// leaves it. Prints nothing when it succeeds (D-624).
    ///
    /// # Errors
    ///
    /// An [`Error`] for an internal compiler error.
    pub fn enter_module(&mut self, path: &str, out: &mut dyn Output) -> Result<(), Error> {
        let path = path.trim();
        if path.is_empty() {
            self.repl.enter(None);
            return Ok(());
        }
        let Some(module) = self.module_at(path)? else {
            out.stderr(&self.tool_error(&format!("there is no module `{path}`")));
            return Ok(());
        };
        let entered = self.entered(module)?;
        self.repl.enter(Some(entered));
        Ok(())
    }

    /// The module of the package or std at `path`, written as in an
    /// import, in its newest version (`:reload`).
    fn module_at(&self, path: &str) -> Result<Option<ModuleId>, Error> {
        let segments: Vec<&str> = path.split("::").map(str::trim).collect();
        if segments
            .iter()
            .any(|s| s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        {
            return Ok(None);
        }
        let mut lookup = SessionLookup {
            package: &self.front.package_name,
            front: &self.front,
            replaced: &self.replaced,
        };
        let module = match lookup.lookup(PackageId::LOCAL, &segments)? {
            Lookup::Found(id) => Some(id),
            Lookup::RootByMainSegment => self
                .front
                .local_module(&ModulePath::root(&self.front.package_name))
                .map(|id| newest(&self.replaced, id)),
            _ => None,
        };
        Ok(module)
    }

    /// What a session sees of module `module` entered with `:module`: its
    /// items and its imports.
    fn entered(&self, module: ModuleId) -> Result<Entered, Error> {
        let iface = self
            .front
            .set
            .get(module)
            .ok_or_else(|| Ice::new(format!("module {module:?} has no interface")))?;
        let mut items: Vec<Name> = Vec::new();
        for item in &iface.items {
            if item.kind.namespace().is_some() && !items.contains(&item.name) {
                items.push(item.name);
            }
        }
        let at = usize::try_from(module.as_u32()).map_err(|_| Ice::new("a module id"))?;
        let requests = self.front.imports.get(at).cloned().unwrap_or_default();
        let targets = self
            .front
            .import_targets
            .get(at)
            .cloned()
            .unwrap_or_default();
        let imports = requests
            .into_iter()
            .zip(targets)
            .map(|(request, target)| {
                let target = match target.module() {
                    Some(m) => cheby_resolve::ImportTarget::Module(newest(&self.replaced, m)),
                    None => target,
                };
                (request, target)
            })
            .collect();
        Ok(Entered {
            module,
            items,
            imports,
            stamp: u32::try_from(self.repl.modules().len())
                .map_err(|_| Ice::new("too many entries"))?,
        })
    }

    /// A tool error (D-464), rendered as the session renders diagnostics.
    fn tool_error(&self, message: &str) -> String {
        cheby_diag::render_tool_error(message, self.options.colors)
    }

    /// How the session renders diagnostics.
    #[must_use]
    pub const fn colors(&self) -> Colors {
        self.options.colors
    }

    /// The parts of an entry before it is checked.
    fn read_entry(&mut self, text: &str, decls: bool) -> Result<ReadEntry, Error> {
        self.count = self.count.saturating_add(1);
        let number = self.count;
        let name = format!("<repl:{number}>");
        let display = self
            .sources
            .add_file(name.clone(), text)
            .map_err(|e| Error::new(e.to_string()))?;
        let (parsed_text, shape) = if decls {
            (text.to_owned(), None)
        } else {
            // The wrapper's `Nil` must not complete what the entry left
            // unfinished (`1 +`): an entry with grammar errors is parsed
            // without it, so they are reported as they are.
            let closed = format!("{BODY_PREFIX}{text}{BODY_CLOSE}");
            let probe = cheby_syntax::parse(FileId::new(0), Arc::from(closed.as_str()));
            if probe.errors().is_empty() {
                (format!("{BODY_PREFIX}{text}{BODY_SUFFIX}"), Some(display))
            } else {
                (closed, Some(display))
            }
        };
        let shift = u32::try_from(BODY_PREFIX.len()).map_err(|_| Ice::new("a long prefix"))?;
        let mut diagnostics = Vec::new();
        let parsed = parse_file(
            &mut self.sources,
            &name,
            parsed_text.into_bytes(),
            &mut diagnostics,
        )?;
        let shape = match shape {
            None => EntryShape::Decls,
            Some(display) => {
                // A grammar error at the wrapper's closing brace is at the
                // end of what the user wrote.
                let end = u32::try_from(text.len())
                    .map_err(|_| Ice::new("a long entry"))?
                    .saturating_add(shift);
                for diagnostic in &mut diagnostics {
                    if diagnostic
                        .primary_span()
                        .is_some_and(|span| span.range.start() > end)
                    {
                        diagnostic.message = diagnostic
                            .message
                            .replace("found `}`", "found the end of the entry");
                    }
                }
                EntryShape::Body { display, shift }
            }
        };
        Ok(ReadEntry {
            name,
            number,
            parsed,
            shape,
            diagnostics,
        })
    }

    /// Registers the module of a read entry.
    fn add_entry_module(&mut self, read: &ReadEntry) -> Result<ModuleId, Error> {
        Ok(self.front.modules.add(ModuleInfo {
            path: ModulePath::root(&read.name),
            package: PackageId::LOCAL,
            file: Some(display_file(read.shape, read.parsed.file)),
        })?)
    }

    /// Parses and checks an entry; prints its diagnostics. `None` when it
    /// has an error (or a denied warning).
    fn check(
        &mut self,
        name: &str,
        number: u32,
        parsed: &ParsedFile,
        shape: EntryShape,
        mut diagnostics: Vec<Diagnostic>,
        out: &mut dyn Output,
    ) -> Result<Option<Entry>, Error> {
        let module = self.front.modules.add(ModuleInfo {
            path: ModulePath::root(name),
            package: PackageId::LOCAL,
            file: Some(display_file(shape, parsed.file)),
        })?;
        // Grammar and validation errors stop the entry before resolution
        // (D-452).
        shape.move_diagnostics(parsed.file, &mut diagnostics);
        let mut checked = None;
        if let (Some(tree), false) = (&parsed.syntax, parsed.has_syntax_errors)
            && !diagnostics.iter().any(Diagnostic::is_error)
        {
            match self.check_tree(module, tree, shape)? {
                Ok(entry) => {
                    diagnostics.extend(entry.warnings.iter().cloned());
                    // Two live bindings of one native symbol with
                    // different C signatures (D-475).
                    let conflicts = self.native_conflicts(&entry);
                    if conflicts.is_empty() {
                        checked = Some(entry);
                    } else {
                        diagnostics.extend(conflicts);
                    }
                }
                Err(found) => diagnostics.extend(found),
            }
        }
        if matches!(shape, EntryShape::Again { .. }) {
            // Its warnings were shown when it was entered.
            diagnostics.retain(Diagnostic::is_error);
        }
        let refused = checked.is_none()
            || diagnostics.iter().any(Diagnostic::is_error)
            || (self.options.deny_warnings && !diagnostics.is_empty());
        if !diagnostics.is_empty() {
            self.report(&mut diagnostics, out);
        }
        Ok(match checked {
            Some(checked) if !refused => Some(Entry { checked, number }),
            _ => None,
        })
    }

    /// Resolves the imports of `tree` and checks it as module `module`.
    fn check_tree(
        &self,
        module: ModuleId,
        tree: &SyntaxTree,
        shape: EntryShape,
    ) -> Result<Result<CheckedEntry, Vec<Diagnostic>>, Error> {
        let names = &self.front.globals.names;
        let imports = collect_imports(tree, names);
        let mut lookup = SessionLookup {
            package: &self.front.package_name,
            front: &self.front,
            replaced: &self.replaced,
        };
        let (targets, mut import_diags) =
            resolve_import_targets(&imports, PackageId::LOCAL, &mut lookup, names)?;
        let deps = self.all_modules();
        // Diagnostics name stale types with ` (stale)` and reloaded
        // modules by their own paths; a module entered with `:module`
        // opens its private items (§13.5.3).
        let display = self.repl.display_table(&self.front.modules)?;
        let opened = self.opened_set()?;
        let set = opened.as_ref().unwrap_or(&self.front.set);
        let input = ResolveInput {
            module,
            package: PackageId::LOCAL,
            tree,
            imports: &imports,
            targets: &targets,
            deps: DepView::new(set, &deps),
            modules: &display,
            globals: &self.front.globals,
            std: StdCatalog { known: &self.known },
            // The session runs natively (§13.5, the JIT): an entry is
            // checked for `native` only, so a native-only item of the
            // package is usable (D-606). That is also so in a package
            // whose targets lack `native`: its items that exist only on
            // `js` are E0067 at the prompt, while its portable items, std
            // and the session's definitions are usable (D-628).
            package_targets: Targets::only(Target::Native),
        };
        let checked = self.repl.check_entry(&input, shape)?;
        shape.move_diagnostics(tree.file(), &mut import_diags);
        Ok(match checked {
            Ok(entry) if !import_diags.iter().any(Diagnostic::is_error) => {
                let mut entry = entry;
                entry.warnings.extend(import_diags);
                Ok(entry)
            }
            Ok(_) => Err(import_diags),
            Err(mut diags) => {
                diags.extend(import_diags);
                Err(diags)
            }
        })
    }

    /// Writes diagnostics, sorted, to standard error.
    fn report(&self, diagnostics: &mut [Diagnostic], out: &mut dyn Output) {
        if let Some(note) = &self.again_note {
            for diagnostic in diagnostics.iter_mut().filter(|d| d.is_error()) {
                diagnostic.notes.push(note.clone());
            }
        }
        self.sources.sort(diagnostics);
        out.stderr(&render_diagnostics(
            diagnostics,
            &self.sources,
            self.options.colors,
            self.options.deny_warnings,
        ));
    }

    /// The modules an entry may read: every module of the front end and
    /// every committed entry, sorted.
    fn all_modules(&self) -> Vec<ModuleId> {
        let mut all: Vec<ModuleId> = self.front.order.clone();
        all.extend(self.repl.modules());
        all.sort_unstable();
        all.dedup();
        all
    }

    /// Compiles what `entry` needs, evaluates its new constants and runs
    /// its body; prints its value. `false` when it is rejected (an error is
    /// printed, or it panicked).
    fn build_and_run(&mut self, entry: &Entry, out: &mut dyn Output) -> Result<bool, Error> {
        let checked = &entry.checked;
        let module_path: Box<str> = format!("<repl:{}>", entry.number).into();
        let module_path = self
            .front
            .modules
            .get(checked.module)
            .map_or(module_path, |info| info.path.to_string().into());
        self.jit.set_session_names(checked.names.iter().map(|n| {
            (
                n.def,
                SessionName {
                    module: module_path.clone(),
                    item: n.text.clone(),
                    generation: n.generation,
                    reuse: n.reuse,
                },
            )
        }));
        let built = self.lower(entry)?;
        self.load_std_records(&built.std_records)?;
        // The slots the entry's redefinitions take over keep their old code
        // until the constants the new code reads are filled
        // (`evaluate_and_publish`).
        match self.jit.compile_unpublished(&built.units) {
            Ok(()) => {}
            Err(JitError::UnresolvedSymbols(symbols)) => {
                let mut diagnostics = self.unresolved(&entry.checked, &symbols);
                self.report(&mut diagnostics, out);
                return Ok(false);
            }
            Err(e) => return Err(e.into()),
        }
        self.repl.note_compiled(checked);
        self.compiled.extend(built.items.iter().copied());
        self.units.extend(built.modules.iter().copied());
        self.const_thunks.extend(built.constants.iter().copied());
        let mut diagnostics = self.evaluate_and_publish(&built, checked)?;
        if !diagnostics.is_empty() {
            self.jit.revert_repoints()?;
            self.report(&mut diagnostics, out);
            return Ok(false);
        }
        self.evaluated.extend(built.constants.iter().copied());
        let Some(body) = &checked.body else {
            return Ok(true);
        };
        // The entry runs on the package's `main-stack`, 64 MiB unless the
        // manifest sets another size (D-660, D-613), read when the session
        // started (D-830). It carries whether
        // the session's code has unwind edges (D-742): under
        // `CHEBY_UNWIND_EDGES=off` a raise ends the process with its
        // report (D-770).
        let config = EntryConfig {
            stack_size: crate::stack_bytes(self.front.main_stack),
            unwind_edges: self.jit.unwind_edges(),
            ..EntryConfig::new(EntryMode::Repl)
        };
        let thunk = self.jit.entry(EntryKind::Repl(entry.number))?;
        // A root fiber of the session's runtime in its current group
        // (D-712, D-799), with its own root scope (§13.5.4); the detached
        // fibers it starts outlive it (D-194). Without a runtime (D-714),
        // an isolated entry on the thread backend.
        let outcome = match &self.runtime {
            Some(runtime) => thunk.run_root_in(runtime, &config, &self.group)?,
            None => thunk.run(&config)?,
        };
        let word = match outcome {
            EntryOutcome::Returned(word) => word,
            // The runtime printed the report after flushing standard output;
            // the entry unwound by the return flag and its pads released
            // what its frames held (D-712, D-733).
            EntryOutcome::Panicked(_) => {
                self.jit.revert_repoints()?;
                return Ok(false);
            }
            // The session's domain deadlocked (D-857): the runtime cancelled
            // only the entry's root scope, so the entry's fibers unwound,
            // while the session's detached fibers keep waiting until
            // `:reload` or the session's end (D-712). The report goes to
            // standard error after standard output is flushed (D-548), and
            // the entry is reverted as a panicked one (D-859).
            EntryOutcome::Deadlocked(report) => {
                out.stderr(&report);
                self.jit.revert_repoints()?;
                return Ok(false);
            }
            // A write failed (D-306, D-709), in the entry or in any fiber
            // while it ran: the session ends with the status and prints
            // nothing more (spec §13.5), after the halt (D-797).
            EntryOutcome::Exited(status) => self.exit_after_termination(status),
        };
        // Everything the entry returned is marked shared before any of it
        // is bound (D-705, D-712): its bindings become session values that
        // later entries and detached fibers read, and shared values are
        // never updated or reused in place (D-716). The marking runs in
        // the session's domain, the one the entry charged.
        cheby_runtime::alloc::with_domain(self.jit.domain(), || {
            // SAFETY: `word` is the thunk's live pair, kept forever. The
            // entry's root has ended, and its outcome reached this thread
            // through the runtime's channel (or the isolated thread's), so
            // every write it made happens before this walk; an object of
            // it that another fiber can reach was marked shared when it
            // reached that fiber (D-705), so every object with a positive
            // count is confined to this thread.
            unsafe { cheby_runtime::rc::mark_shared(word) };
        });
        self.kept.push(word);
        // SAFETY: the thunk returns the owned pair `(debug text, the body's
        // tuple)` (cheby_mir's REPL thunk), a live object, kept forever.
        let &[text, tuple] = unsafe { cheby_runtime::values::read_tuple(word) }.as_slice() else {
            return Err(Ice::new("a REPL thunk returned something else than a pair").into());
        };
        // SAFETY: as above; the body's tuple holds the shown value, then the
        // bindings in order.
        let values = unsafe { cheby_runtime::values::read_tuple(tuple) };
        let bound: Vec<(DefId, u64)> = body
            .bindings
            .iter()
            .zip(values.iter().skip(1))
            .map(|(binding, &word)| (binding.def, word))
            .collect();
        if bound.len() != body.bindings.len() {
            return Err(Ice::new("a REPL entry returned fewer values than it binds").into());
        }
        self.jit.bind_values(&bound)?;
        if body.shown != cheby_iface::Ty::NIL {
            // SAFETY: the first element of the pair is a `String` object.
            let debug = unsafe { cheby_runtime::values::str_of(text) }.to_owned();
            let ty = self.show_ty(body.shown, &checked.iface)?;
            out.stdout(&format!("{debug} : {ty}\n"));
        }
        Ok(true)
    }

    /// A type as the REPL prints it (D-463, D-546), with ` (stale)` after a
    /// type a redefinition or `:reload` replaced (§13.5.5).
    fn show_ty(&self, ty: cheby_iface::Ty, own: &ModuleInterface) -> Result<String, Error> {
        let all = self.all_modules();
        let deps = DepView::new(&self.front.set, &all);
        let display = self.repl.display_table(&self.front.modules)?;
        let cx = DisplayCx {
            globals: &self.front.globals,
            modules: &display,
            own: Some(own),
            deps: &deps,
            generics: None,
        };
        Ok(self.repl.show_ty(ty, &cx))
    }

    /// The interfaces an entry is checked against while a module is
    /// entered with `:module` (§13.5.3, D-624): that module's
    /// `priv` items are visible to the entry, which is a module of the
    /// local package (D-606). For a module of another package, the
    /// standard library, its package's package-visible items become
    /// visible too, and the structure of its types usable, since the
    /// package boundary is that module's package (D-098). `None` when no
    /// module is entered. Only checking reads these; code is generated
    /// from the real interfaces.
    fn opened_set(&self) -> Result<Option<InterfaceSet>, Error> {
        let Some(entered) = self.repl.scope().entered() else {
            return Ok(None);
        };
        let module = entered.module;
        let package = self.front.modules.try_get(module)?.package;
        let mut opened = Vec::new();
        for (id, info) in self.front.modules.iter() {
            let own = id == module;
            if !own && (package == PackageId::LOCAL || info.package != package) {
                continue;
            }
            let Some(iface) = self.front.set.get(id) else {
                continue;
            };
            let mut items = iface.items.clone();
            for item in &mut items {
                item.vis = match (item.vis, package == PackageId::LOCAL) {
                    (Vis::Priv, true) if own => Vis::Package,
                    (Vis::Priv, false) if own => Vis::Pub,
                    (Vis::Package, false) => Vis::Pub,
                    (vis, _) => vis,
                };
                if package != PackageId::LOCAL
                    && let ItemSig::Type(def) = &mut item.sig
                {
                    def.exposed = true;
                }
            }
            opened.push(Arc::new(ModuleInterface::new(id, iface.package, items)?));
        }
        Ok(Some(replace_interfaces(
            &self.front.set,
            self.front.modules.len(),
            opened,
        )?))
    }

    /// E0055 for each native symbol `entry` binds that the runtime exports
    /// (D-944), and E0054 for each other one that a live binding binds
    /// with another C signature (D-475): a binding of the package or the
    /// standard library, or of a session item that is still the session's
    /// definition of its name and that the entry does not replace.
    /// Replaced definitions and entries that failed do not count.
    fn native_conflicts(&self, entry: &CheckedEntry) -> Vec<Diagnostic> {
        let replaced: Vec<DefId> = entry.redefinitions().map(|r| r.old).collect();
        let mut out = Vec::new();
        let runtime = cheby_runtime::symbols();
        let exported = |symbol: &str| runtime.iter().any(|(name, _)| *name == symbol);
        for (_, symbol, binding) in
            entry_natives(&entry.resolved, &entry.iface, &self.front.globals)
        {
            // D-944: E0055 for a name the runtime exports, which only std
            // may bind; it is then no binding for E0054.
            if let Some(reserved) =
                cheby_resolve::runtime_export_reserved(&symbol, binding.span, &exported)
            {
                out.push(reserved);
                continue;
            }
            let live = self.front.natives.get(&symbol).or_else(|| {
                self.natives
                    .iter()
                    .filter(|n| {
                        n.symbol == symbol
                            && !replaced.contains(&n.def)
                            && self.repl.current(Namespace::Value, n.name) == Some(n.def)
                    })
                    .map(|n| &n.binding)
                    .find(|b| b.sig != binding.sig)
            });
            if let Some(first) = live
                && first.sig != binding.sig
            {
                out.push(
                    Diagnostic::new(
                        codes::CONFLICTING_NATIVE_SYMBOL,
                        format!(
                            "the native symbol `{symbol}` is bound with two different C signatures"
                        ),
                    )
                    .with_primary(binding.span, "bound here with another signature")
                    .with_secondary(first.span, "first bound here")
                    .with_note(
                        "every `@external` of one symbol must give it the same C signature; \
                         in the REPL, only the session's current definitions count",
                    ),
                );
            }
        }
        out
    }

    /// One E0053 per native symbol nothing defines, at the entry's
    /// `@external` that names it, else at the package's first binding.
    fn unresolved(&self, entry: &CheckedEntry, symbols: &[String]) -> Vec<Diagnostic> {
        symbols
            .iter()
            .map(|name| {
                // The symbol itself, not the name of its library
                // generation (`name_native_generation`).
                let symbol = cheby_jit::split_generation(name).0;
                let own = entry
                    .resolved
                    .items
                    .iter()
                    .find_map(|item| item.attrs.native_symbol().filter(|(s, _)| *s == symbol));
                let span = own
                    .map(|(_, span)| span)
                    .or_else(|| self.front.natives.get(symbol).map(|b| b.span));
                let diag = Diagnostic::new(
                    codes::UNRESOLVED_NATIVE_SYMBOL,
                    format!("the native symbol `{symbol}` is not defined"),
                )
                .with_note(crate::native::E0053_NOTE);
                match span {
                    Some(span) => diag.with_primary(span, "nothing defines this symbol"),
                    None => diag,
                }
            })
            .collect()
    }

    /// Makes `entry` part of the session: its names, its interface for
    /// later entries, and its bodies for later reachability.
    fn commit(&mut self, entry: Entry) -> Result<(), Error> {
        let Entry { checked, .. } = entry;
        for (index, symbol, binding) in
            entry_natives(&checked.resolved, &checked.iface, &self.front.globals)
        {
            if let Some(item) = checked.iface.item(index) {
                self.natives.push(SessionNative {
                    def: DefId::new(checked.module, index),
                    name: item.name,
                    symbol,
                    binding,
                });
            }
        }
        self.repl.commit(&checked, &self.front.globals.names);
        self.front.set.insert(Arc::new(checked.iface.clone()))?;
        let set = &self.front.set;
        let read = |def: DefId| set.get(def.module).and_then(|iface| iface.item(def.index));
        let plan = self
            .repl
            .satisfaction(&checked, &read, &self.front.globals)?;
        self.entries.insert(checked.module, checked.thir);
        self.apply_satisfaction(&plan)
    }

    /// Adds the forwarders of `plan` to their types' entry modules, with
    /// their bodies and symbol names, and retires the ones it retires
    /// (D-610).
    fn apply_satisfaction(&mut self, plan: &SatisfactionPlan) -> Result<(), Error> {
        if plan.add.is_empty() && plan.retire.is_empty() {
            return Ok(());
        }
        let mut changed: BTreeMap<ModuleId, ModuleInterface> = BTreeMap::new();
        let mut added = Vec::with_capacity(plan.add.len());
        let mut names = Vec::with_capacity(plan.add.len());
        for new in &plan.add {
            let module = new.ty.module;
            let iface = match changed.entry(module) {
                std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::btree_map::Entry::Vacant(e) => e.insert(
                    self.front
                        .set
                        .get(module)
                        .ok_or_else(|| Ice::new("a session type's module has no interface"))?
                        .clone(),
                ),
            };
            let index = iface.push(new.item.clone())?;
            let def = DefId::new(module, index);
            let (ItemSig::Fn(sig), Some(span)) = (&new.item.sig, new.item.span) else {
                return Err(Ice::new("a forwarder without a function signature").into());
            };
            let body = forwarder_fn(def, new.target, sig, span, &self.front.globals)?;
            self.entries
                .get_mut(&module)
                .ok_or_else(|| Ice::new("a session type's module has no bodies"))?
                .fns
                .push(body);
            let path = self.front.modules.try_get(module)?.path.to_string();
            names.push((
                def,
                SessionName {
                    module: path.into(),
                    item: new.text.clone(),
                    generation: new.generation,
                    reuse: None,
                },
            ));
            added.push(def);
        }
        for &def in &plan.retire {
            let iface = match changed.entry(def.module) {
                std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::btree_map::Entry::Vacant(e) => e.insert(
                    self.front
                        .set
                        .get(def.module)
                        .ok_or_else(|| Ice::new("a forwarder's module has no interface"))?
                        .clone(),
                ),
            };
            if let Some(name) = iface.item(def.index).map(|item| item.name) {
                iface.unbind(Namespace::Value, name, def.index);
            }
        }
        self.front.set = replace_interfaces(
            &self.front.set,
            self.front.modules.len(),
            changed.into_values().map(Arc::new).collect(),
        )?;
        self.jit.set_session_names(names);
        self.repl.apply_satisfaction(plan, &added);
        Ok(())
    }

    /// Checks again, from their own text, the definitions that used a
    /// definition `changed` replaced with another signature (§13.5.2,
    /// D-043): one that still checks is compiled again and replaces the
    /// old one (taking over its slot), and one that does not is reported
    /// and keeps its old code.
    fn check_again(&mut self, changed: &[Redefinition], out: &mut dyn Output) -> Result<(), Error> {
        let dependents: Vec<Again> = self
            .repl
            .dependents(changed)
            .iter()
            .filter_map(|item| {
                Some(Again {
                    name: item.name,
                    source: item.source?,
                    inside: item.inside,
                })
            })
            .collect();
        self.recheck(
            dependents,
            "a definition it uses changed its signature",
            out,
        )
    }

    /// Checks `items` again from their own text, in order, each in the
    /// module it was entered in: one that checks is compiled again and
    /// replaces itself, and the errors of one that does not get the note
    /// "`f` was checked again because {why}; it keeps its old definition".
    fn recheck(&mut self, items: Vec<Again>, why: &str, out: &mut dyn Output) -> Result<(), Error> {
        let saved = self.repl.scope().entered().cloned();
        for item in items {
            let inside = match item.inside {
                Some(module) => Some(self.entered(newest(&self.replaced, module))?),
                None => None,
            };
            self.repl.enter(inside);
            let result = self.recheck_one(item.name, item.source, why, out);
            self.again_note = None;
            self.repl.enter(saved.clone());
            result?;
        }
        Ok(())
    }

    fn recheck_one(
        &mut self,
        name: Name,
        source: Span,
        why: &str,
        out: &mut dyn Output,
    ) -> Result<(), Error> {
        let Some(file) = self.sources.file(source.file) else {
            return Ok(());
        };
        let path = file.path().to_owned();
        let masked = mask(file.text(), source.range);
        self.again = self.again.saturating_add(1);
        let module_name = format!("{}:{}>", path.trim_end_matches('>'), self.again);
        let mut diagnostics = Vec::new();
        let parsed = parse_file(
            &mut self.sources,
            &path,
            masked.into_bytes(),
            &mut diagnostics,
        )?;
        let shape = EntryShape::Again {
            display: source.file,
        };
        let text = self.front.globals.names.get(name);
        self.again_note = Some(format!(
            "`{text}` was checked again because {why}; it keeps its old definition"
        ));
        let entry = self.check(&module_name, 0, &parsed, shape, diagnostics, out)?;
        if let Some(entry) = entry
            && self.build_and_run(&entry, out)?
        {
            self.commit(entry)?;
        }
        Ok(())
    }

    /// `:reload` (§13.5.3, D-098, D-547, D-625): first tears the session's
    /// detached fibers down, whatever follows (§13.5.4, D-712,
    /// D-827); then reads the
    /// package's module files again. Each module whose text changed, each
    /// new module, and each module of the package that imports one of them
    /// is checked again as a new module, all or nothing; its items get new
    /// symbols, because its module path carries the reload's generation.
    /// If every one checks, later entries and imports see the new versions,
    /// values of their old types are stale, each new version is listed as
    /// "reloaded `path`", and the session's definitions that used an old
    /// version are checked again, also those that an earlier `:reload`
    /// broke and that still run an older version: those that still check
    /// are compiled again, and the others are reported and keep their old
    /// code. A reload that fails changes nothing, not even how values
    /// print. What package discovery reports is shown even when no module
    /// changed, and an error there refuses the reload. A module whose file
    /// is gone stays loaded, and `cheby.toml` is not read again.
    ///
    /// # Errors
    ///
    /// An [`Error`] for an internal compiler error.
    pub fn reload(&mut self, out: &mut dyn Output) -> Result<(), Error> {
        // Every `:reload` tears the session's detached fibers down first,
        // also outside a package, when no module changed and when the
        // reload is rejected; later detached fibers join a fresh group
        // (D-712, D-194, D-799, D-827). The JIT and the
        // runtime stay.
        self.tear_down_detached();
        let Some(root) = self.root.clone() else {
            out.stderr(&self.tool_error(
                "`:reload` reloads the modules of the package, and this session has no package",
            ));
            return Ok(());
        };
        let mut diagnostics = Vec::new();
        let files = match cheby_package::discover(&root, &self.front.package_name, &mut diagnostics)
        {
            Ok(files) => files,
            Err(e) => {
                out.stderr(&self.tool_error(&e.to_string()));
                return Ok(());
            }
        };
        // The modules whose text changed, and the new ones.
        let mut reloaded: Vec<Reloaded> = Vec::new();
        for file in files {
            let bytes = match std::fs::read(root.join(&file.file)) {
                Ok(bytes) => bytes,
                Err(e) => {
                    out.stderr(&self.tool_error(&format!("{}: {e}", file.display)));
                    return Ok(());
                }
            };
            let old = self
                .front
                .local_module(&file.path)
                .map(|id| newest(&self.replaced, id));
            let same = old
                .and_then(|id| self.front.modules.get(id))
                .and_then(|info| info.file)
                .and_then(|f| self.sources.file(f))
                .is_some_and(|f| f.text().as_bytes() == bytes.as_slice());
            if !same {
                reloaded.push(Reloaded {
                    path: file.path,
                    display: file.display,
                    bytes,
                    old,
                });
            }
        }
        // D-712, D-941: a changed C source loads the package's native code
        // again beside the old, and the modules that bind native symbols
        // get new versions that call it; old code keeps the old library
        // (D-1197).
        match self.reload_native() {
            Ok(true) => self.add_native_modules(&mut reloaded)?,
            Ok(false) => {}
            Err(e) => {
                out.stderr(&self.tool_error(e.message()));
                return Ok(());
            }
        }
        self.add_importers(&mut reloaded)?;
        if reloaded.is_empty() {
            // What discovery found is reported even when no text changed,
            // as `cheby check` reports it; an error refuses the reload
            // (review R5-ROB-4).
            let refused = diagnostics.iter().any(Diagnostic::is_error)
                || (self.options.deny_warnings && !diagnostics.is_empty());
            if !diagnostics.is_empty() {
                self.report(&mut diagnostics, out);
            }
            if !refused {
                out.stdout("no module changed\n");
            }
            return Ok(());
        }
        self.reloads = self.reloads.saturating_add(1);
        match self.check_reloaded(reloaded, diagnostics, out)? {
            Some(done) => self.finish_reload(&done, out),
            None => Ok(()),
        }
    }

    /// Prepares the package's native code again (D-941): `true` when its
    /// C code changed, after the new library joined the JIT as a new
    /// generation that code compiled from now on calls (D-712); `false`
    /// when nothing changed or the package has no native code.
    ///
    /// # Errors
    ///
    /// The tool error of a failed compile or link (D-942).
    fn reload_native(&mut self) -> Result<bool, Error> {
        let Some((root, package, table)) = self.front.native.source.clone() else {
            return Ok(false);
        };
        let known = self.front.native.inputs;
        let Some(native) = crate::native::prepare_unless(&root, &package, &table, None, known)?
        else {
            return Ok(false);
        };
        self.native_generation = self.jit.add_library_generation(native.libraries());
        self.front.native = native;
        Ok(true)
    }

    /// Adds to `reloaded` every module of the package that binds a native
    /// symbol, with the text it has, so its new version calls the package's
    /// new native code (D-712, D-941).
    fn add_native_modules(&self, reloaded: &mut Vec<Reloaded>) -> Result<(), Error> {
        for &id in &self.front.order {
            let Ok(info) = self.front.modules.try_get(id) else {
                continue;
            };
            let at = usize::try_from(id.as_u32()).unwrap_or(usize::MAX);
            let binds = self
                .front
                .module_natives
                .get(at)
                .is_some_and(|natives| !natives.is_empty());
            if info.package != PackageId::LOCAL || self.replaced.contains_key(&id) || !binds {
                continue;
            }
            let path = self.original_path(id)?;
            if reloaded.iter().any(|r| r.path == path) {
                continue;
            }
            let Some(file) = info.file.and_then(|f| self.sources.file(f)) else {
                continue;
            };
            reloaded.push(Reloaded {
                path,
                display: file.path().to_owned(),
                bytes: file.text().as_bytes().to_vec(),
                old: Some(id),
            });
        }
        Ok(())
    }

    /// Adds to `reloaded` the modules of the package that import a
    /// reloaded one, again with the text they have.
    fn add_importers(&self, reloaded: &mut Vec<Reloaded>) -> Result<(), Error> {
        loop {
            let ids: Vec<ModuleId> = reloaded.iter().filter_map(|r| r.old).collect();
            let paths: Vec<&ModulePath> = reloaded.iter().map(|r| &r.path).collect();
            let mut more = Vec::new();
            for &id in &self.front.order {
                let Ok(info) = self.front.modules.try_get(id) else {
                    continue;
                };
                if info.package != PackageId::LOCAL
                    || self.replaced.contains_key(&id)
                    || paths.contains(&&self.original_path(id)?)
                    || info
                        .path
                        .segments()
                        .last()
                        .is_some_and(|s| s == SESSION_FILE)
                {
                    continue;
                }
                let imports_one = self
                    .front
                    .import_targets
                    .get(usize::try_from(id.as_u32()).unwrap_or(usize::MAX))
                    .is_some_and(|targets| {
                        targets.iter().filter_map(|t| t.module()).any(|m| {
                            let m = newest(&self.replaced, m);
                            ids.contains(&m)
                                || self.original_path(m).is_ok_and(|p| paths.contains(&&p))
                        })
                    });
                if !imports_one {
                    continue;
                }
                let Some(file) = info.file.and_then(|f| self.sources.file(f)) else {
                    continue;
                };
                more.push(Reloaded {
                    path: self.original_path(id)?,
                    display: file.path().to_owned(),
                    bytes: file.text().as_bytes().to_vec(),
                    old: Some(id),
                });
            }
            if more.is_empty() {
                return Ok(());
            }
            reloaded.extend(more);
        }
    }

    /// The path a module of the package is imported by: its own, or that
    /// of the module a `:reload` version stands for.
    fn original_path(&self, module: ModuleId) -> Result<ModulePath, Error> {
        let info = self.front.modules.try_get(module)?;
        Ok(self.repl.shown_path(module, info).path.clone())
    }

    /// Parses and checks the modules of a `:reload`, in import order;
    /// `None` after reporting their errors.
    fn check_reloaded(
        &mut self,
        reloaded: Vec<Reloaded>,
        mut diagnostics: Vec<Diagnostic>,
        out: &mut dyn Output,
    ) -> Result<Option<Vec<(Reloaded, ModuleId)>>, Error> {
        let pending = self.parse_reloaded(reloaded, &mut diagnostics)?;
        let ids: Vec<ModuleId> = pending.iter().map(|p| p.id).collect();
        let done = self.check_parsed(pending, diagnostics, out)?;
        if done.is_none() {
            // A rejected reload changes nothing: its versions leave the
            // build and stop showing the paths of the live modules, which
            // would otherwise print as stale (review R5-ROB-1).
            self.front.forget_reloaded(&ids);
            self.repl.forget_shown(&ids);
        }
        Ok(done)
    }

    /// Checks the parsed modules of a `:reload`, in import order; `None`
    /// after reporting their errors.
    fn check_parsed(
        &mut self,
        pending: Vec<PendingModule>,
        mut diagnostics: Vec<Diagnostic>,
        out: &mut dyn Output,
    ) -> Result<Option<Vec<(Reloaded, ModuleId)>>, Error> {
        if diagnostics.iter().any(Diagnostic::is_error)
            || pending.iter().any(|p| p.source.has_syntax_errors)
        {
            self.report(&mut diagnostics, out);
            return Ok(None);
        }
        let Some(order) = self.reload_order(&pending, &mut diagnostics)? else {
            self.report(&mut diagnostics, out);
            return Ok(None);
        };
        let mut slots: Vec<Option<PendingModule>> = pending.into_iter().map(Some).collect();
        let mut done = Vec::new();
        // A module that imports one that failed is not checked: its errors
        // would only repeat.
        let mut failed: Vec<ModuleId> = Vec::new();
        for node in order {
            let Some(pending) = slots
                .get_mut(usize::try_from(node).map_err(|_| Ice::new("a node"))?)
                .and_then(Option::take)
            else {
                continue;
            };
            let id = pending.id;
            if pending
                .targets
                .iter()
                .any(|t| t.module().is_some_and(|m| failed.contains(&m)))
            {
                failed.push(id);
                continue;
            }
            let tree = pending
                .source
                .syntax
                .as_ref()
                .ok_or_else(|| Ice::new("a reloaded module without its tree"))?;
            let display = self.repl.display_table(&self.front.modules)?;
            let found = frontend::check_reloaded(
                &mut self.front,
                &display,
                frontend::ReloadedModule {
                    id,
                    tree,
                    imports: pending.requests,
                    targets: pending.targets,
                },
            )?;
            if found.iter().any(Diagnostic::is_error) {
                failed.push(id);
            }
            diagnostics.extend(found);
            done.push((pending.module, id));
        }
        let olds: Vec<ModuleId> = done.iter().filter_map(|(m, _)| m.old).collect();
        let natives = self.reload_natives(&olds, &mut diagnostics);
        // With `--deny-warnings`, a warning refuses the reload as it refuses
        // an entry (D-549).
        let rejected = !failed.is_empty()
            || diagnostics.iter().any(Diagnostic::is_error)
            || (self.options.deny_warnings && !diagnostics.is_empty());
        if !diagnostics.is_empty() {
            self.report(&mut diagnostics, out);
        }
        if rejected {
            return Ok(None);
        }
        self.front.natives = natives;
        Ok(Some(done))
    }

    /// Parses the modules of a `:reload`, gives each a new module id,
    /// whose path carries the reload's generation while diagnostics show
    /// the module's own, and maps its imports to modules, the new versions
    /// of the others included.
    fn parse_reloaded(
        &mut self,
        reloaded: Vec<Reloaded>,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<Vec<PendingModule>, Error> {
        let mut parsed = Vec::with_capacity(reloaded.len());
        let mut overrides: BTreeMap<ModulePath, ModuleId> = BTreeMap::new();
        for module in reloaded {
            let source = parse_file(
                &mut self.sources,
                &module.display,
                module.bytes.clone(),
                diagnostics,
            )?;
            let id = self.front.modules.add(ModuleInfo {
                path: reload_path(&module.path, self.reloads),
                package: PackageId::LOCAL,
                file: Some(source.file),
            })?;
            self.repl.show_as(
                id,
                ModuleInfo {
                    path: module.path.clone(),
                    package: PackageId::LOCAL,
                    file: Some(source.file),
                },
            );
            overrides.insert(module.path.clone(), id);
            parsed.push((module, id, source));
        }
        let mut pending = Vec::with_capacity(parsed.len());
        for (module, id, source) in parsed {
            let (requests, targets) = match &source.syntax {
                Some(tree) => {
                    let requests = collect_imports(tree, &self.front.globals.names);
                    let mut lookup = SessionLookup {
                        package: &self.front.package_name,
                        front: &self.front,
                        replaced: &self.replaced,
                    };
                    let mut lookup = ReloadLookup {
                        inner: &mut lookup,
                        package: &self.front.package_name,
                        overrides: &overrides,
                    };
                    let (targets, found) = resolve_import_targets(
                        &requests,
                        PackageId::LOCAL,
                        &mut lookup,
                        &self.front.globals.names,
                    )?;
                    diagnostics.extend(found);
                    (requests, targets)
                }
                None => (Vec::new(), Vec::new()),
            };
            pending.push(PendingModule {
                module,
                id,
                source,
                requests,
                targets,
            });
        }
        Ok(pending)
    }

    /// The order in which the modules of a `:reload` are checked: each
    /// after the reloaded modules it imports. `None` after pushing E0040
    /// for each import cycle among them.
    fn reload_order(
        &self,
        pending: &[PendingModule],
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<Option<Vec<u32>>, Error> {
        let node = |n: usize| u32::try_from(n).map_err(|_| Ice::new("too many modules"));
        let ids: Vec<ModuleId> = pending.iter().map(|p| p.id).collect();
        let mut graph = cheby_package::graph::Graph::new(pending.len());
        for (from, p) in pending.iter().enumerate() {
            for to in p.targets.iter().filter_map(|t| t.module()) {
                if let Some(to) = ids.iter().position(|&id| id == to) {
                    graph.add_edge(node(from)?, node(to)?);
                }
            }
        }
        let cycles = match cheby_package::graph::topological_order(&graph) {
            Ok(order) => return Ok(Some(order)),
            Err(cycles) => cycles,
        };
        let display = self.repl.display_table(&self.front.modules)?;
        for cycle in &cycles {
            let mut with_spans = Vec::new();
            for (n, &at) in cycle.nodes.iter().enumerate() {
                let next = cycle
                    .nodes
                    .get(n.saturating_add(1))
                    .or_else(|| cycle.nodes.first())
                    .copied()
                    .ok_or_else(|| Ice::new("an empty cycle"))?;
                let index = |n: u32| usize::try_from(n).map_err(|_| Ice::new("a node"));
                let to = ids.get(index(next)?).copied();
                let from = pending.get(index(at)?).ok_or_else(|| Ice::new("a node"))?;
                let span = from
                    .targets
                    .iter()
                    .position(|t| t.module() == to)
                    .and_then(|k| from.requests.get(k))
                    .map(|r| r.span)
                    .ok_or_else(|| Ice::new("a cycle edge without its import"))?;
                with_spans.push((from.id, span));
            }
            diagnostics.push(cycle_diagnostic(&with_spans, &display)?);
        }
        Ok(None)
    }

    /// The first binding of each native symbol across the package's and
    /// std's current modules once `olds` are replaced, pushing E0054 for a
    /// symbol bound with two C signatures (§12.5.1).
    fn reload_natives(
        &self,
        olds: &[ModuleId],
        diagnostics: &mut Vec<Diagnostic>,
    ) -> BTreeMap<Box<str>, NativeBinding> {
        let mut natives = BTreeMap::new();
        for &id in &self.front.order {
            if self.replaced.contains_key(&id) || olds.contains(&id) {
                continue;
            }
            let mut bindings = self
                .front
                .module_natives
                .get(usize::try_from(id.as_u32()).unwrap_or(usize::MAX))
                .cloned()
                .unwrap_or_default();
            // D-944: a reloaded version may bind a name the runtime
            // exports; the session's other modules never do, or it would
            // not have started.
            if !self.is_std(id) {
                bindings = frontend::reserve_exports(bindings, diagnostics);
            }
            frontend::bind_natives(bindings, &mut natives, diagnostics);
        }
        natives
    }

    /// Makes the modules of a `:reload` the session's, lists them, and
    /// checks again the session's definitions that used the old versions.
    fn finish_reload(
        &mut self,
        done: &[(Reloaded, ModuleId)],
        out: &mut dyn Output,
    ) -> Result<(), Error> {
        let mut olds = Vec::new();
        for (module, new) in done {
            if let Some(old) = module.old {
                let shown = self.front.modules.try_get(old)?;
                let shown = ModuleInfo {
                    path: self.repl.shown_path(old, shown).path.clone(),
                    package: PackageId::LOCAL,
                    file: shown.file,
                };
                self.replaced.insert(old, *new);
                self.repl.replace_module(old, *new, shown);
                olds.push(old);
            }
            out.stdout(&format!("reloaded `{}`\n", module.path));
        }
        // A module entered with `:module` is entered in its new version.
        if let Some(entered) = self.repl.scope().entered()
            && self.replaced.contains_key(&entered.module)
        {
            let module = newest(&self.replaced, entered.module);
            let entered = self.entered(module)?;
            self.repl.enter(Some(entered));
        }
        // Every earlier version of a reloaded module, not only the one just
        // replaced: a definition that an earlier `:reload` broke still runs
        // code compiled against an older version, and the new one may fit
        // it again, as with a function redefined twice (D-609, review
        // R5-ROB-3).
        let news: Vec<ModuleId> = done.iter().map(|&(_, new)| new).collect();
        for &old in self.replaced.keys() {
            if !olds.contains(&old) && news.contains(&newest(&self.replaced, old)) {
                olds.push(old);
            }
        }
        let dependents: Vec<Again> = self
            .repl
            .reload_dependents(&olds)
            .iter()
            .filter_map(|item| {
                Some(Again {
                    name: item.name,
                    source: item.source?,
                    inside: item.inside,
                })
            })
            .collect();
        self.recheck(dependents, "`:reload` changed a module it uses", out)
    }
}

/// A module that `:reload` checks again.
struct Reloaded {
    /// The module's path, as imports write it.
    path: ModulePath,
    /// Its file as diagnostics show it.
    display: String,
    /// Its text.
    bytes: Vec<u8>,
    /// The version it replaces, if it was loaded.
    old: Option<ModuleId>,
}

/// A module of a `:reload`, parsed, with its new id and its imports.
struct PendingModule {
    module: Reloaded,
    id: ModuleId,
    source: ParsedFile,
    requests: Vec<cheby_resolve::ImportRequest>,
    targets: Vec<cheby_resolve::ImportTarget>,
}

/// A definition checked again.
struct Again {
    name: Name,
    source: Span,
    inside: Option<ModuleId>,
}

/// Names every native symbol `unit`'s foreign calls make after the
/// package's native code of `generation` (D-712, D-941,
/// [`cheby_jit::generation_symbol`]): the JIT resolves a name once, so
/// code compiled after a `:reload` that changed a C source reaches the new
/// library only under names of its own (D-1197). Front
/// end and diagnostics keep the symbol itself; generation 0 changes
/// nothing.
fn name_native_generation(unit: &mut MirModule, generation: u32) {
    if generation == 0 {
        return;
    }
    for function in &mut unit.functions {
        for block in &mut function.blocks {
            for stmt in &mut block.stmts {
                if let cheby_mir::Stmt::Assign {
                    value: cheby_mir::Rvalue::CallForeign { symbol, .. },
                    ..
                }
                | cheby_mir::Stmt::Eval(cheby_mir::Rvalue::CallForeign { symbol, .. }) = stmt
                {
                    *symbol = cheby_jit::generation_symbol(symbol, generation).into();
                }
            }
        }
    }
}

/// The path of the new version of a module that `:reload` makes:
/// `app::prices` becomes `app::prices<r1>`, which no import can name, so
/// its symbols never collide with the old version's (D-547,
/// D-625).
fn reload_path(path: &ModulePath, generation: u32) -> ModulePath {
    let segments = path.segments();
    let mark = |segment: &str| format!("{segment}<r{generation}>");
    match segments.split_last() {
        None => ModulePath::root(&mark("")),
        Some((last, [])) => ModulePath::root(&mark(last)),
        Some((last, [first, middle @ ..])) => middle
            .iter()
            .fold(ModulePath::root(first), |p, s| p.child(s))
            .child(&mark(last)),
    }
}

/// The lookup of `:reload`: a reloaded module's imports find the new
/// versions of the modules reloaded with it.
struct ReloadLookup<'a, 'b> {
    inner: &'a mut SessionLookup<'b>,
    package: &'a str,
    overrides: &'a BTreeMap<ModulePath, ModuleId>,
}

impl ModuleLookup for ReloadLookup<'_, '_> {
    fn lookup(&mut self, from: PackageId, path: &[&str]) -> Result<Lookup, Ice> {
        if let Some((&first, rest)) = path.split_first()
            && first == self.package
        {
            let module_path = rest
                .iter()
                .fold(ModulePath::root(first), |p, segment| p.child(segment));
            if let Some(&id) = self.overrides.get(&module_path) {
                return Ok(Lookup::Found(id));
            }
        }
        self.inner.lookup(from, path)
    }
}

/// What [`Session::lower`] made.
struct Built {
    units: Vec<MirModule>,
    /// Std's records, loaded before `units` when this batch is the first
    /// to need std in a session with a cache (K6).
    std_records: Vec<StdRecord>,
    /// The items it lowered.
    items: Vec<DefId>,
    /// The modules whose code units it compiles.
    modules: Vec<ModuleId>,
    /// The constants to evaluate, in order.
    constants: Vec<DefId>,
    reach: Reach,
}

impl Session {
    /// D-971: the REPL runs std's normal code. The first batch that needs
    /// a std module lowers every std module whole (D-966), infers borrowing
    /// over std's units alone, the scope D-968 gives std, and counts them
    /// with borrowing on; no later batch lowers a std item again, so no
    /// entry can change a std mask. Package modules stay `plain` (D-808):
    /// an entry calls only std's `pub` functions, which are never borrowed
    /// (D-787). Under `CHEBY_RC=naive` nothing borrows (D-746).
    fn lower_std(&self) -> Result<Vec<MirModule>, Error> {
        let std_mir = MirOptions {
            borrow: true,
            ..self.mir
        };
        let mut units = Vec::new();
        for &module in &self.front.order {
            if self
                .front
                .modules
                .get(module)
                .is_none_or(|info| info.package != PackageId::STD)
            {
                continue;
            }
            let c = self
                .front
                .checked(module)
                .ok_or_else(|| Ice::new(format!("module {module:?} was not checked")))?;
            let own = self
                .front
                .set
                .get(module)
                .ok_or_else(|| Ice::new(format!("module {module:?} has no interface")))?;
            let view = DepView::new(&self.front.set, &c.deps);
            let cx = LowerCx {
                defs: Defs {
                    own: Some(own),
                    deps: &view,
                    modules: &self.front.modules,
                    globals: &self.front.globals,
                },
                sources: &self.sources,
            };
            let unit = lower_items(&c.thir, &|_| true, &cx)?;
            cheby_mir::check_lowered(&unit)?;
            units.push(unit);
        }
        if std_mir.borrows() {
            crate::backend::infer_units(&self.front, &mut units)?;
        }
        for unit in &mut units {
            cheby_mir::count_lowered_with(unit, &std_mir)?;
        }
        Ok(units)
    }

    /// Whether `module` is a std module.
    fn is_std(&self, module: ModuleId) -> bool {
        self.front
            .modules
            .get(module)
            .is_some_and(|info| info.package == PackageId::STD)
    }

    /// When a batch first needs std (`reach` needs one of `needed`'s std
    /// modules and no std module is compiled yet) under whole modules:
    /// [`Session::lower_std`]'s units, with every item and module they
    /// compile added to `items` and `lowered`. `None` otherwise.
    fn std_batch(
        &self,
        reach: &Reach,
        needed: &[ModuleId],
        items: &mut Vec<DefId>,
        lowered: &mut Vec<ModuleId>,
        records: &mut Vec<StdRecord>,
    ) -> Result<Option<Vec<MirModule>>, Error> {
        let first = crate::backend::command_scope() == crate::backend::Scope::Whole
            && needed
                .iter()
                .any(|&m| self.is_std(m) && reach.needs_module(m))
            && !self.units.iter().any(|&m| self.is_std(m));
        if !first {
            return Ok(None);
        }
        // K6 (D-971, D-980): with a cache, std's normal code is every std
        // module's record, which std's first use writes when one misses.
        if let Some(cache) = self.std_store.as_ref().filter(|c| c.reads) {
            let std_mir = crate::std_cache::std_mir(self.mir);
            let settings = crate::std_cache::std_settings(&crate::borrowing::settings(
                crate::cache::BuildKind::Std,
                std_mir,
            ));
            let modules: Vec<ModuleId> = self
                .front
                .order
                .iter()
                .copied()
                .filter(|&m| m != ModuleId::BUILTIN && self.is_std(m))
                .collect();
            let mut paths = Vec::with_capacity(modules.len());
            for &module in &modules {
                paths.push(self.front.modules.try_get(module)?.path.to_string());
            }
            let at = crate::std_cache::StdStore::of_cache(cache);
            let mut found = crate::std_cache::std_records(&at, &paths, &settings, std_mir)?;
            for (module, path) in modules.into_iter().zip(paths) {
                let record = found
                    .remove(&path)
                    .ok_or_else(|| Ice::new(format!("no std record of `{path}`")))?;
                self.std_compiled(module, items, lowered)?;
                records.push((module, path, record));
            }
            return Ok(Some(Vec::new()));
        }
        let units = self.lower_std()?;
        for unit in &units {
            if let Some(module) = unit.module() {
                self.std_compiled(module, items, lowered)?;
            }
        }
        Ok(Some(units))
    }

    /// K6: loads std's `records` into the session's JIT, in a batch of
    /// their own before the entry's; std redefines no slot, so nothing
    /// waits to be published (D-971, D-1220).
    fn load_std_records(&mut self, records: &[StdRecord]) -> Result<(), Error> {
        if records.is_empty() {
            return Ok(());
        }
        let batch: Vec<cheby_jit::BatchUnit<'_>> = records
            .iter()
            .map(|(module, path, record)| cheby_jit::BatchUnit::Record {
                record,
                unit: cheby_mir::UnitKind::Code(*module),
                path,
            })
            .collect();
        self.jit
            .compile_units(&batch, &cheby_jit::BatchCx { test_ordinals: &[] })?;
        // A record carries no index of its slots: std's constants are named
        // by symbol, as `code_cache::name_consts` names a command's (K4c).
        let mut names = Vec::new();
        for (module, path, _) in records {
            let (Some(c), Some(iface)) = (self.front.checked(*module), self.front.set.get(*module))
            else {
                continue;
            };
            for constant in &c.thir.consts {
                let item = iface
                    .item(constant.def.index)
                    .ok_or_else(|| Ice::new("a std constant without its item"))?;
                let name = cheby_mir::DefName {
                    def: constant.def,
                    module: path.as_str().into(),
                    item: self.front.globals.names.get(item.name).into(),
                };
                let symbol = cheby_codegen_clif::mangle::item(
                    cheby_codegen_clif::mangle::Kind::Const,
                    &name,
                );
                names.push((constant.def, symbol));
            }
        }
        self.jit.name_consts(names);
        Ok(())
    }

    /// Adds std module `module` to what a batch compiles: the module, its
    /// functions and its constants.
    fn std_compiled(
        &self,
        module: ModuleId,
        items: &mut Vec<DefId>,
        lowered: &mut Vec<ModuleId>,
    ) -> Result<(), Error> {
        lowered.push(module);
        let c = self
            .front
            .checked(module)
            .ok_or_else(|| Ice::new(format!("module {module:?} was not checked")))?;
        items.extend(c.thir.fns.iter().map(|f| f.def));
        items.extend(c.thir.consts.iter().map(|c| c.def));
        Ok(())
    }

    /// Lowers what `entry` reaches that is not compiled yet. MIR lowering
    /// reports no diagnostics (D-554), so every error is an `Ice`.
    fn lower(&self, entry: &Entry) -> Result<Built, Error> {
        let checked = &entry.checked;
        let mut program = Program::new();
        for &module in &self.front.order {
            if let Some(c) = self.front.checked(module) {
                program.insert(module, &c.thir);
            }
        }
        for (&module, thir) in &self.entries {
            program.insert(module, thir);
        }
        program.insert(checked.module, &checked.thir);
        let mut roots: Vec<Root<'_>> = Vec::new();
        roots.extend(checked.thir.fns.iter().map(Root::Fn));
        roots.extend(checked.thir.consts.iter().map(Root::Const));
        roots.extend(checked.thir.tests.iter().map(Root::Test));
        let hooks = |module: ModuleId| self.hooks_of(module, checked);
        let reach = reachable_with_hooks(&program, &roots, &hooks)?;
        let all = self.all_modules();
        let mut modules: Vec<ModuleId> = self.front.order.clone();
        modules.extend(self.entries.keys().copied());
        modules.push(checked.module);
        let mut units = Vec::new();
        let mut items = Vec::new();
        let mut lowered_modules = Vec::new();
        let (mut std_records, lowered) = (Vec::new(), &mut lowered_modules);
        let std_first = self.std_batch(&reach, &modules, &mut items, lowered, &mut std_records)?;
        let std_first = std_first.map(|std| units.extend(std)).is_some();
        let wanted = |def: DefId| reach.contains(def) && !self.compiled.contains(&def);
        for module in modules {
            if !reach.needs_module(module) || (std_first && self.is_std(module)) {
                continue;
            }
            let new: Vec<DefId> = reach
                .items()
                .filter(|d| d.module == module && wanted(*d))
                .collect();
            if new.is_empty() && self.units.contains(&module) {
                continue;
            }
            let (thir, own, deps) =
                if module == checked.module {
                    (&checked.thir, &checked.iface, all.clone())
                } else if let Some(c) = self.front.checked(module) {
                    let own =
                        self.front.set.get(module).ok_or_else(|| {
                            Ice::new(format!("module {module:?} has no interface"))
                        })?;
                    (&c.thir, own, c.deps.clone())
                } else {
                    let thir = self
                        .entries
                        .get(&module)
                        .ok_or_else(|| Ice::new(format!("module {module:?} was not checked")))?;
                    let own =
                        self.front.set.get(module).ok_or_else(|| {
                            Ice::new(format!("module {module:?} has no interface"))
                        })?;
                    (thir, own, all.clone())
                };
            let view = DepView::new(&self.front.set, &deps);
            let cx = LowerCx {
                defs: Defs {
                    own: Some(own),
                    deps: &view,
                    modules: &self.front.modules,
                    globals: &self.front.globals,
                },
                sources: &self.sources,
            };
            let mut unit = lower_items(thir, &wanted, &cx)?;
            if !self.is_std(module) {
                name_native_generation(&mut unit, self.native_generation);
                crate::native::attach_drops(&self.front, module, &mut unit, self.native_generation);
            }
            count_and_check_with(&mut unit, &self.mir)?;
            units.push(unit);
            items.extend(new);
            lowered_modules.push(module);
        }
        let constants: Vec<DefId> = reach
            .constant_order()
            .into_iter()
            .filter(|def| !self.evaluated.contains(def))
            .collect();
        let thunks: Vec<DefId> = constants
            .iter()
            .copied()
            .filter(|def| !self.const_thunks.contains(def))
            .collect();
        units.extend(self.lower_entry_units(entry, &thunks, &all)?);
        Ok(Built {
            units,
            std_records,
            items,
            modules: lowered_modules,
            constants,
            reach,
        })
    }

    /// The entry's tests unit (its body) and its entries unit: the thunk of
    /// its body and those of `constants`.
    fn lower_entry_units(
        &self,
        entry: &Entry,
        constants: &[DefId],
        all: &[ModuleId],
    ) -> Result<Vec<MirModule>, Error> {
        let checked = &entry.checked;
        let mut units = Vec::new();
        let view = DepView::new(&self.front.set, all);
        let cx = LowerCx {
            defs: Defs {
                own: Some(&checked.iface),
                deps: &view,
                modules: &self.front.modules,
                globals: &self.front.globals,
            },
            sources: &self.sources,
        };
        if !checked.thir.tests.is_empty() {
            let tests = thir::Module {
                module: checked.module,
                fns: Vec::new(),
                consts: Vec::new(),
                tests: checked.thir.tests.clone(),
            };
            let mut unit = lower_tests(&tests, &cx)?;
            count_and_check_with(&mut unit, &self.mir)?;
            units.push(unit);
        }
        let mut requests: Vec<EntryRequest<'_>> = Vec::new();
        if checked.body.is_some() {
            requests.push(EntryRequest {
                kind: EntryKind::Repl(entry.number),
                info: None,
            });
        }
        requests.extend(constants.iter().map(|&def| EntryRequest {
            kind: EntryKind::Const(def),
            info: None,
        }));
        if !requests.is_empty() {
            let mut unit = lower_entries(&requests, &cx)?;
            count_and_check_with(&mut unit, &self.mir)?;
            units.push(unit);
        }
        Ok(units)
    }

    /// The `@std_hooks` functions of every type `module` declares (D-482).
    fn hooks_of(&self, module: ModuleId, entry: &CheckedEntry) -> Result<Vec<DefId>, Ice> {
        let own = if module == entry.module {
            Some(&entry.iface)
        } else {
            self.front.set.get(module)
        };
        let Some(own) = own else {
            return Ok(Vec::new());
        };
        let none = DepView::new(&self.front.set, &[]);
        let defs = Defs {
            own: Some(own),
            deps: &none,
            modules: &self.front.modules,
            globals: &self.front.globals,
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

    /// Evaluates the batch's new constants and points the slots its
    /// redefinitions take over at their new code, constants first
    /// (D-832, which refines D-712, D-547, D-194).
    ///
    /// The session's detached fibers may call through a slot at any time
    /// (D-194), and a constant read by code a detached fiber runs must be
    /// filled, or the read is a fatal error outside constant evaluation.
    /// So a slot gets its new code only once every constant that code
    /// reads is evaluated, and a constant is evaluated only once every
    /// slot its initializer reaches holds the new code, so that it sees
    /// this entry's redefinitions as it did before. The two rules are
    /// applied in rounds: the constants that are ready, then the slots
    /// that are ready. A cycle between them (a constant whose initializer
    /// reaches a redefinition whose new code reads it, statically) is left
    /// to the old order: every slot first, then the constants. The first
    /// round whose constants fail ends it with their diagnostics; the
    /// caller then reverts the slots published so far.
    fn evaluate_and_publish(
        &mut self,
        built: &Built,
        entry: &CheckedEntry,
    ) -> Result<Vec<Diagnostic>, Error> {
        let mut waiting: Vec<DefId> = built.constants.clone();
        let mut unpublished = self.jit.pending_repoints();
        if !unpublished.is_empty() && !waiting.is_empty() {
            let slots: Vec<SlotHandle> = unpublished.iter().map(|&(_, slot)| slot).collect();
            // The slots each waiting constant's initializer reaches, and the
            // waiting constants each redefinition's new code reads.
            let calls: BTreeMap<DefId, Vec<SlotHandle>> = waiting
                .iter()
                .map(|&c| (c, self.slots_reached(&built.reach, c, &waiting, &slots)))
                .collect();
            let reads: BTreeMap<DefId, Vec<DefId>> = unpublished
                .iter()
                .map(|&(def, _)| (def, constants_read(&built.reach, def, &waiting)))
                .collect();
            loop {
                let evaluable: Vec<DefId> = waiting
                    .iter()
                    .copied()
                    .filter(|c| {
                        calls.get(c).is_none_or(|reached| {
                            reached
                                .iter()
                                .all(|slot| !unpublished.iter().any(|&(_, s)| s == *slot))
                        })
                    })
                    .collect();
                if !evaluable.is_empty() {
                    let diagnostics = self.evaluate(&evaluable, &built.reach, entry)?;
                    if !diagnostics.is_empty() {
                        return Ok(diagnostics);
                    }
                    waiting.retain(|c| !evaluable.contains(c));
                }
                let publish: Vec<SlotHandle> = unpublished
                    .iter()
                    .filter(|&&(def, _)| {
                        reads
                            .get(&def)
                            .is_none_or(|read| read.iter().all(|c| !waiting.contains(c)))
                    })
                    .map(|&(_, slot)| slot)
                    .collect();
                if !publish.is_empty() {
                    self.jit.publish_repoints(&publish)?;
                    unpublished.retain(|(_, slot)| !publish.contains(slot));
                }
                if (evaluable.is_empty() && publish.is_empty()) || waiting.is_empty() {
                    break;
                }
            }
        }
        self.jit.publish_all_repoints()?;
        if waiting.is_empty() {
            return Ok(Vec::new());
        }
        self.evaluate(&waiting, &built.reach, entry)
    }

    /// The slots among `slots` that the initializer of the constant `root`
    /// may call through: every function its initializer reaches, through
    /// functions and the initializers of the constants among `waiting`
    /// that it forces (D-535), and the slot that function's callers load.
    fn slots_reached(
        &self,
        reach: &Reach,
        root: DefId,
        waiting: &[DefId],
        slots: &[SlotHandle],
    ) -> Vec<SlotHandle> {
        let mut found = Vec::new();
        let mut seen = BTreeSet::from([root]);
        let mut work: Vec<DefId> = reach.refers_to(root).collect();
        while let Some(def) = work.pop() {
            if !seen.insert(def) {
                continue;
            }
            if reach.is_const(def) {
                // An evaluated constant's initializer never runs again.
                if !waiting.contains(&def) {
                    continue;
                }
            } else if let Some(slot) = self.jit.slot_of(def)
                && slots.contains(&slot)
                && !found.contains(&slot)
            {
                found.push(slot);
            }
            work.extend(reach.refers_to(def));
        }
        found
    }

    /// Evaluates `constants` (E6, D-535) with their seeds (D-538).
    fn evaluate(
        &mut self,
        constants: &[DefId],
        reach: &Reach,
        entry: &CheckedEntry,
    ) -> Result<Vec<Diagnostic>, Error> {
        let const_modules = reach.const_modules();
        let mut infos = Vec::with_capacity(constants.len());
        for &def in constants {
            let (own, thir) =
                if def.module == entry.module {
                    (&entry.iface, &entry.thir)
                } else {
                    let own = self.front.set.get(def.module).ok_or_else(|| {
                        Ice::new(format!("the constant {def:?} has no interface"))
                    })?;
                    let thir = match self.front.checked(def.module) {
                        Some(c) => &c.thir,
                        None => self.entries.get(&def.module).ok_or_else(|| {
                            Ice::new(format!("the constant {def:?} was not checked"))
                        })?,
                    };
                    (own, thir)
                };
            let item = own
                .item(def.index)
                .ok_or_else(|| Ice::new(format!("the constant {def:?} has no item")))?;
            let span = thir
                .consts
                .iter()
                .find(|c| c.def == def)
                .map(|c| c.span)
                .ok_or_else(|| Ice::new(format!("the constant {def:?} has no body")))?;
            let name = self.front.globals.names.get(item.name);
            let info = self.front.modules.try_get(def.module)?;
            let module_path = info.path.to_string();
            let mut deps: Vec<(String, u64)> = Vec::new();
            for &module in const_modules.get(&def).map_or(&[][..], Vec::as_slice) {
                let module_info = self.front.modules.try_get(module)?;
                // The hash computed when the file was read (D-963); a
                // module the session added is hashed here.
                let hash = match self.front.source_hashes(module) {
                    Some(hashes) => hashes.sip64,
                    None => module_info
                        .file
                        .and_then(|file| self.sources.file(file))
                        .map_or(0, |f| {
                            cheby_runtime::consteval::source_hash(f.text().as_bytes())
                        }),
                };
                deps.push((module_info.path.to_string(), hash));
            }
            deps.sort();
            let deps: Vec<(&str, u64)> = deps.iter().map(|(p, h)| (p.as_str(), *h)).collect();
            let seed = cheby_runtime::consteval::const_seed(
                info.path.package(),
                &module_path,
                name,
                &deps,
            );
            infos.push(ConstInfo {
                def,
                name: name.into(),
                path: format!("{module_path}::{name}").into(),
                name_span: span,
                seed,
                on_js: item.targets.contains(Target::Js),
            });
        }
        let config = ConstEvalConfig {
            mode: ConstMode::Run,
            // The manifest's `[const-eval]` limits, as for `cheby run`
            // (D-536).
            time_limit: self.front.const_eval_time_limit,
            memory_limit: self.front.const_eval_memory_limit,
            targets: self.front.package_targets,
        };
        // K5a, D-1190: the entry's constants evaluate in
        // the session JIT (its package code is `plain`, D-971; std keeps
        // its masks, D-1180), then reach their slots from images, as in
        // every other host (D-979): a new generation gets a new image.
        // The images stay in the process, so an earlier entry's installed
        // constant is shared by address, never copied (D-539, the wave-6
        // review's R-2), and the evaluated results are freed (R-1).
        let diagnostics = cheby_consteval::evaluate(&mut self.jit, &infos, &config)?;
        if diagnostics.is_empty() {
            cheby_consteval::replace_with_images(&self.jit, &infos)?;
        }
        Ok(diagnostics)
    }
}

/// The constants among `waiting` that the code of the function `root` may
/// read (D-832): those its body and the
/// functions it reaches refer to. A read loads the constant's slot and
/// never runs its initializer, so the walk does not enter initializers.
fn constants_read(reach: &Reach, root: DefId, waiting: &[DefId]) -> Vec<DefId> {
    let mut found = Vec::new();
    let mut seen = BTreeSet::from([root]);
    let mut work: Vec<DefId> = reach.refers_to(root).collect();
    while let Some(def) = work.pop() {
        if !seen.insert(def) {
            continue;
        }
        if reach.is_const(def) {
            if waiting.contains(&def) {
                found.push(def);
            }
            continue;
        }
        work.extend(reach.refers_to(def));
    }
    found
}

/// The module source the REPL parses for the entry `text`: declarations
/// and imports as they are, statements inside the wrapper
/// `test "<repl>" {` … `Nil }`; `None` for text that is not an entry
/// (blanks and comments). For tools that parse REPL input the way the REPL
/// does, such as the parser cross-check of `tests/repl/` transcripts
/// (D-272).
#[must_use]
pub fn entry_source(text: &str) -> Option<String> {
    Some(if entry_kind(text)? {
        text.to_owned()
    } else {
        format!("{BODY_PREFIX}{text}{BODY_SUFFIX}")
    })
}

/// The entries of a transcript of `tests/repl/` (plan §8.3): each starts
/// at a line `> …` and goes on with the lines `. …` after it; the prompts
/// are removed. Standard input lines (`< …`) and output lines are not
/// part of any entry.
#[must_use]
pub fn transcript_entries(transcript: &str) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for line in transcript.lines() {
        if let Some(rest) = line.strip_prefix("> ") {
            entries.push(rest.to_owned());
        } else if let Some(rest) = line.strip_prefix(". ")
            && let Some(entry) = entries.last_mut()
        {
            entry.push('\n');
            entry.push_str(rest);
        }
    }
    entries
}

/// Whether `text` is declarations and imports (`Some(true)`), statements
/// (`Some(false)`), or not an entry at all (`None`): blanks and comments.
/// Declarations start with an item keyword, an attribute or a doc comment.
fn entry_kind(text: &str) -> Option<bool> {
    for line in text.lines() {
        let line = line.trim_start();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("///") || line.starts_with("//!") {
            return Some(true);
        }
        if line.starts_with("//") {
            continue;
        }
        if line.starts_with('@') {
            return Some(true);
        }
        let word: String = line
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        // `fn(x) { … }` is an anonymous function, an expression.
        let anonymous = word == "fn"
            && line
                .get(2..)
                .is_some_and(|rest| rest.trim_start().starts_with('('));
        return Some(
            !anonymous
                && matches!(
                    word.as_str(),
                    "fn" | "pub" | "priv" | "type" | "interface" | "const" | "import" | "test"
                ),
        );
    }
    None
}

/// The newest version of module `module`, following the replacements of
/// `:reload`.
fn newest(replaced: &BTreeMap<ModuleId, ModuleId>, module: ModuleId) -> ModuleId {
    let mut at = module;
    // Each replacement makes a module with a higher id, so this ends.
    while let Some(&next) = replaced.get(&at) {
        if next <= at {
            break;
        }
        at = next;
    }
    at
}

/// A copy of `set` (whose modules have ids below `len`) in which each
/// interface of `replace` takes the place of its module's.
fn replace_interfaces(
    set: &InterfaceSet,
    len: usize,
    replace: Vec<Arc<ModuleInterface>>,
) -> Result<InterfaceSet, Ice> {
    let builtin = set
        .get(ModuleId::BUILTIN)
        .ok_or_else(|| Ice::new("the builtin module has no interface"))?
        .clone();
    let mut out = InterfaceSet::new(builtin);
    out.reserve_modules(len)?;
    let mut replace: BTreeMap<ModuleId, Arc<ModuleInterface>> =
        replace.into_iter().map(|i| (i.module, i)).collect();
    for n in 1..len {
        let id = ModuleId::from_u32(u32::try_from(n).map_err(|_| Ice::new("too many modules"))?);
        if let Some(iface) = replace.remove(&id).or_else(|| set.get_arc(id).cloned()) {
            out.publish(iface)?;
        }
    }
    Ok(out)
}

/// The native symbols the functions of a session entry bind, with their
/// items, in item order (§12.5.1, D-475).
fn entry_natives(
    resolved: &ResolvedModule,
    iface: &ModuleInterface,
    globals: &cheby_iface::Globals,
) -> Vec<(DefIndex, Box<str>, NativeBinding)> {
    let mut out = Vec::new();
    for (index, item) in resolved.items.iter_enumerated() {
        let Some((symbol, span)) = item.attrs.native_symbol() else {
            continue;
        };
        let Some(ItemSig::Fn(sig)) = iface.item(index).map(|i| &i.sig) else {
            continue;
        };
        out.push((
            index,
            symbol.into(),
            NativeBinding {
                sig: foreign_sig_with_context(sig, globals, item.attrs.std_context),
                span,
            },
        ));
    }
    out
}

/// The file whose text an entry's spans end up in.
const fn display_file(shape: EntryShape, parsed: FileId) -> FileId {
    match shape {
        EntryShape::Decls => parsed,
        EntryShape::Body { display, .. } | EntryShape::Again { display } => display,
    }
}

/// `text` with everything outside `keep` blanked, line ends kept, so
/// positions in it are positions in `text`.
fn mask(text: &str, keep: TextRange) -> String {
    let start = usize::try_from(keep.start()).unwrap_or(usize::MAX);
    let end = usize::try_from(keep.end()).unwrap_or(usize::MAX);
    text.char_indices()
        .flat_map(|(at, c)| {
            let kept = (start..end).contains(&at) || c == '\n';
            let replacement = if kept { c } else { ' ' };
            // A blanked character keeps its byte length.
            let width = if kept { 1 } else { c.len_utf8() };
            std::iter::repeat_n(replacement, width)
        })
        .collect()
}

/// The package of a session started outside any package: no modules, the
/// name `repl`.
fn no_package(
    dir: &Path,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Package, Error> {
    const MANIFEST: &str = "name = \"repl\"\nversion = \"0.0.0\"\n";
    let file = sources
        .add_file("<repl>/cheby.toml".to_owned(), MANIFEST)
        .map_err(|e| Error::new(e.to_string()))?;
    let manifest = Manifest::parse(file, MANIFEST, diagnostics)
        .ok_or_else(|| Error::new("internal compiler error: the REPL's manifest".to_owned()))?;
    Ok(Package {
        root: dir.to_path_buf(),
        manifest,
        modules: Vec::new(),
    })
}

/// The synthetic module of the session's package that imports every
/// shipped std module, so every std import of an entry resolves; its file
/// is also where synthetic imports point.
fn std_module(
    package: &Package,
    sources: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(crate::Module, FileId), Error> {
    let mut text = String::new();
    for module in std_lib::STD_MODULES {
        text.push_str("import std::");
        text.push_str(module.path);
        text.push('\n');
    }
    let hashes = crate::SourceHashes::of(text.as_bytes());
    let source = parse_file(sources, SESSION_FILE, text.into_bytes(), diagnostics)?;
    let file = source.file;
    Ok((
        crate::Module {
            path: ModulePath::root(&package.manifest.name).child(SESSION_FILE),
            source,
            hashes,
            parsed: true,
            // The REPL uses no cache (D-964), the only reader of these.
            syntax_diagnostics: Vec::new(),
        },
        file,
    ))
}

/// The id of the first `import` of the session's synthetic module.
fn synthetic_import(
    module: &crate::Module,
) -> Result<cheby_syntax::ast::AstId<cheby_syntax::ast::ImportDecl<'static>>, Error> {
    module
        .source
        .syntax
        .as_ref()
        .and_then(|tree| SourceFile::cast(tree.root()))
        .and_then(|file| file.imports().next())
        .map(cheby_syntax::ast::AstNode::id)
        .ok_or_else(|| Ice::new("the REPL's std module has no import").into())
}

/// Maps the import paths of entries to modules the session's front end
/// loaded: the package's and every shipped std module.
struct SessionLookup<'a> {
    package: &'a str,
    front: &'a Frontend,
    /// The modules `:reload` replaced: an import finds the newest version.
    replaced: &'a BTreeMap<ModuleId, ModuleId>,
}

impl ModuleLookup for SessionLookup<'_> {
    fn lookup(&mut self, _from: PackageId, path: &[&str]) -> Result<Lookup, Ice> {
        let Some((&first, rest)) = path.split_first() else {
            return Err(Ice::new("an import with an empty path"));
        };
        let module_path = rest
            .iter()
            .fold(ModulePath::root(first), |p, segment| p.child(segment));
        if first == self.package {
            if rest == ["main"] {
                return Ok(Lookup::RootByMainSegment);
            }
            if let Some(id) = self.front.local_module(&module_path)
                && rest.last() != Some(&SESSION_FILE)
            {
                return Ok(Lookup::Found(newest(self.replaced, id)));
            }
            let text = module_path.to_string();
            // Modules a user can import: not the session's own, and not
            // the new versions `:reload` made, whose paths are marked.
            let candidates: Vec<String> = self
                .front
                .modules
                .iter()
                .filter(|(_, info)| {
                    info.package == PackageId::LOCAL
                        && info.path.package() == self.package
                        && info.path.segments().iter().all(|s| !s.contains('<'))
                })
                .map(|(_, info)| info.path.to_string())
                .collect();
            let closest =
                closest_name(&text, candidates.iter().map(String::as_str)).map(str::to_owned);
            return Ok(Lookup::NoModule {
                package: PackageId::LOCAL,
                closest,
            });
        }
        if first != "std" {
            return Ok(Lookup::NoPackage);
        }
        if let Some(id) = self.front.modules.find(&module_path) {
            return Ok(Lookup::Found(id));
        }
        let under_std = rest.join("::");
        if std_lib::is_planned(&under_std) {
            return Ok(Lookup::Planned);
        }
        let closest = closest_name(&under_std, std_lib::known()).map(|p| format!("std::{p}"));
        Ok(Lookup::NoModule {
            package: PackageId::STD,
            closest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both streams in one string, in the order of writes.
    #[derive(Default)]
    struct Buffer {
        text: String,
    }

    impl Output for Buffer {
        fn stdout(&mut self, text: &str) {
            self.text.push_str(text);
        }

        fn stderr(&mut self, text: &str) {
            self.text.push_str(text);
        }
    }

    /// The options of an in-process test session: two workers, since the
    /// tests run in parallel and each session starts a runtime (plan L2).
    fn options() -> SessionOptions {
        SessionOptions {
            threads: Some(2),
            ..SessionOptions::default()
        }
    }

    /// Starts a session outside any package with `options`.
    fn start(options: SessionOptions) -> Box<Session> {
        let dir = std::env::temp_dir();
        let session = Session::start(&dir, options).unwrap();
        let (session, warnings) = match session {
            Start::Ready(session, warnings) => (session, warnings),
            Start::Failed(why) => panic!("the session did not start:\n{why}"),
        };
        assert_eq!(warnings, "");
        session
    }

    /// Runs `entries` in a session outside any package, on a thread with
    /// the command's stack (D-601), and returns what each printed.
    fn session(entries: &[&str]) -> Vec<String> {
        session_with(options(), entries)
    }

    /// [`session`] with `options`.
    fn session_with(options: SessionOptions, entries: &[&str]) -> Vec<String> {
        let entries: Vec<String> = entries.iter().map(|e| (*e).to_owned()).collect();
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(move || {
                let mut session = start(options);
                entries
                    .iter()
                    .map(|entry| {
                        let mut out = Buffer::default();
                        session.eval(entry, &mut out).unwrap();
                        out.text
                    })
                    .collect()
            })
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn a_session_runs_entries_and_redefinitions() {
        let printed = session(&[
            "let x = 20",
            "fn f(n: Int) -> Int {\n  n + x\n}",
            "fn g() -> Int {\n  f(1)\n}",
            "g()",
            "fn f(n: Int) -> Int {\n  n * 100\n}",
            "g()",
            "let x = 1 / 0",
            "x",
            "nope",
        ]);
        assert_eq!(printed[0], "");
        assert_eq!(printed[3], "21 : Int\n");
        // The same signature: `g` reaches the new `f` through its slot.
        assert_eq!(printed[5], "100 : Int\n");
        // A panic (reported by the runtime) binds nothing.
        assert_eq!(printed[6], "");
        assert_eq!(printed[7], "20 : Int\n");
        assert!(printed[8].starts_with("error[E0042]"), "{}", printed[8]);
        assert!(printed[8].contains("<repl:9>:1:1"), "{}", printed[8]);
    }

    /// D-1190, the wave-6 review's R-2: a constant that
    /// holds an earlier entry's constant shares its installed value, as
    /// D-539 asks, instead of installing a copy of it, so entries that
    /// name a large constant cost a word each. Every installed value is
    /// static, and nothing of the evaluations stays in the session's
    /// domain (R-1).
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn repl_constants_share_earlier_installed_constants() {
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(|| {
                let mut session = start(options());
                let live = session.jit.domain().live_objects();
                for entry in [
                    "import std::list",
                    "const big: List<Int> = list::range(0, 1000)",
                    "const c1: (List<Int>, Int) = (big, 1)",
                    "const c2: (List<Int>, List<Int>) = (big, big)",
                    "const c3: (List<Int>, Int) = c1",
                ] {
                    let mut out = Buffer::default();
                    session.eval(entry, &mut out).unwrap();
                    assert_eq!(out.text, "", "{entry}");
                }
                assert_eq!(session.jit.domain().live_objects(), live);
                let slot = |name: &str| -> u64 {
                    let def = session
                        .evaluated
                        .iter()
                        .copied()
                        .find(|def| {
                            session
                                .front
                                .set
                                .get(def.module)
                                .and_then(|iface| iface.item(def.index))
                                .is_some_and(|item| {
                                    session.front.globals.names.get(item.name) == name
                                })
                        })
                        .unwrap_or_else(|| panic!("no constant `{name}`"));
                    // SAFETY: an installed constant's slot.
                    unsafe { *session.jit.const_slot(def).unwrap() }
                };
                let big = slot("big");
                let c1 = slot("c1");
                // SAFETY: installed values, static for the session.
                unsafe {
                    for word in [big, c1, slot("c2"), slot("c3")] {
                        assert!(cheby_runtime::consteval::image::is_static(word));
                    }
                    assert_eq!(cheby_runtime::values::read_tuple(c1)[0], big);
                    assert_eq!(cheby_runtime::values::read_tuple(slot("c2")), [big, big]);
                }
                // A constant that is an earlier one is that value.
                assert_eq!(slot("c3"), c1);
                for (entry, printed) in [
                    ("let (l, m) = c3", ""),
                    ("(list::length(l), m)", "(1000, 1) : (Int, Int)\n"),
                ] {
                    let mut out = Buffer::default();
                    session.eval(entry, &mut out).unwrap();
                    assert_eq!(out.text, printed, "{entry}");
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// No package unit a session lowers borrows (D-715, D-808, D-971):
    /// the session's options turn borrowing off whatever `CHEBY_RC` says,
    /// and a function that a build would borrow in (a package-visible
    /// function that only reads its `String`) runs, then is redefined with
    /// the same signature, through its all-owned slot. Counting a unit that
    /// borrows under these options would be an internal error.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn borrow_off_in_every_unit_a_session_lowers() {
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(|| {
                let mut session = start(options());
                assert!(!session.mir.borrow && !session.mir.borrows());
                let mut printed = Vec::new();
                for entry in [
                    "fn check(s: String) -> Bool {\n  s == \"x\"\n}",
                    "check(\"x\")",
                    "fn check(s: String) -> Bool {\n  s == \"y\"\n}",
                    "check(\"x\")",
                ] {
                    let mut out = Buffer::default();
                    session.eval(entry, &mut out).unwrap();
                    printed.push(out.text);
                }
                assert_eq!(printed[1], "True : Bool\n");
                assert_eq!(printed[3], "False : Bool\n");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// D-971: the REPL runs std's normal code. Std's units are every std
    /// module, whole, with masks inferred over std (some of its functions
    /// borrow); the first entry that needs std compiles all of them, and
    /// the session's package code stays `plain`.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn kinds_repl_runs_std_borrowed_and_package_plain() {
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(|| {
                let mut session = start(options());
                let units = session.lower_std().unwrap();
                let std: BTreeSet<ModuleId> = session
                    .front
                    .order
                    .iter()
                    .copied()
                    .filter(|&m| {
                        session.front.modules.get(m).map(|i| i.package) == Some(PackageId::STD)
                    })
                    .collect();
                let lowered: BTreeSet<ModuleId> =
                    units.iter().filter_map(MirModule::module).collect();
                assert_eq!(lowered, std);
                assert!(
                    units
                        .iter()
                        .flat_map(|u| &u.functions)
                        .any(|f| !f.borrow.is_owned()),
                    "std borrows somewhere"
                );
                assert!(!session.mir.borrows());
                let mut out = Buffer::default();
                session.eval("import std::list", &mut out).unwrap();
                session.eval("list::length([1, 2, 3])", &mut out).unwrap();
                assert_eq!(out.text, "3 : Int\n");
                if crate::backend::command_scope() == crate::backend::Scope::Whole {
                    assert!(std.iter().all(|m| session.units.contains(m)));
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// `--deny-warnings` refuses an entry with a warning (D-549).
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn denied_warnings_refuse_the_entry() {
        let options = SessionOptions {
            deny_warnings: true,
            ..options()
        };
        let printed = session_with(
            options,
            &[
                "fn f() -> Int {\n  let unused = 1\n  2\n}",
                "f()",
                "let x = 1",
                "x",
            ],
        );
        assert!(printed[0].starts_with("error[W0002]"), "{}", printed[0]);
        assert!(printed[0].contains(crate::DENIED_NOTE), "{}", printed[0]);
        assert!(printed[1].starts_with("error[E0042]"), "{}", printed[1]);
        assert_eq!(printed[3], "1 : Int\n");
    }

    /// The `index`th value that the last committed entry bound: its
    /// thunk's pair holds the debug text and the body's tuple, which holds
    /// the shown value, then the bindings in order.
    fn last_binding(session: &Session, index: usize) -> u64 {
        let pair = *session.kept.last().unwrap();
        // SAFETY: a kept pair, live for the session.
        let tuple = unsafe { cheby_runtime::values::read_tuple(pair) }[1];
        // SAFETY: as above.
        let values = unsafe { cheby_runtime::values::read_tuple(tuple) };
        values[1..][index]
    }

    /// Evaluates `entry` and returns what it printed.
    fn eval(session: &mut Session, entry: &str) -> String {
        let mut out = Buffer::default();
        session.eval(entry, &mut out).unwrap();
        out.text
    }

    /// D-705, D-712: a value is marked shared, with everything it reaches,
    /// before it is bound as a session value, so its count is negative and
    /// it is never updated or reused in place (D-716).
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn session_values_are_marked_shared_before_they_are_bound() {
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(|| {
                let mut session = start(options());
                assert_eq!(eval(&mut session, "let xs = [[1, 2], [3]]"), "");
                let xs = last_binding(&session, 0);
                // SAFETY: a bound session value, kept for the session.
                let count = unsafe { cheby_runtime::rc::count_of(xs) }.unwrap();
                assert!(count < 0, "{count}");
                assert_eq!(
                    eval(&mut session, "xs"),
                    "[[1, 2], [3]] : List<List<Int>>\n"
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// D-712, D-194, D-827 and D-831:
    /// a detached fiber parked in a capacity-0 `channel::send`, on a
    /// channel whose `Receiver` is a session value and whose `Sender` only
    /// the fiber holds, is torn down by `:reload`, here outside a package
    /// (a tool error, which still tears down): its wait node is unlinked
    /// from the channel, which the runtime's reader shows, its `Sender`
    /// goes as it unwinds, and a later `receive` finds the channel closed.
    /// Later detached fibers join a fresh group and run.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn session_reload_tears_down_a_parked_sender_and_unlinks_its_node() {
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(|| {
                let mut session = start(options());
                eval(&mut session, "import std::channel");
                eval(&mut session, "import std::fiber");
                let printed = eval(
                    &mut session,
                    "let rx = {\n  let (tx, receiver) = channel::new::<Int>(0)\n  \
                     fiber::spawn_detached(fn() { channel::send(tx, 1) })\n  receiver\n}",
                );
                assert_eq!(printed, "");
                let rx = last_binding(&session, 0);
                let first = session.group.clone();
                // The fiber parks in its send, linking its node; this waits
                // for that, with a hang detector.
                let start = std::time::Instant::now();
                // SAFETY: `rx` is a live `Receiver`, a session value.
                while !unsafe { Runtime::channel_has_waiters(rx) } {
                    assert!(start.elapsed() < std::time::Duration::from_secs(60));
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                assert_eq!(first.live(), 1);
                let mut out = Buffer::default();
                session.reload(&mut out).unwrap();
                assert!(
                    out.text.contains("this session has no package"),
                    "{}",
                    out.text
                );
                // SAFETY: as above.
                assert!(!unsafe { Runtime::channel_has_waiters(rx) });
                assert_eq!(first.live(), 0);
                assert!(!Arc::ptr_eq(&first, &session.group));
                assert!(session.stuck.is_empty());
                assert_eq!(
                    eval(&mut session, "channel::receive(rx)"),
                    "Err(Nil) : Result<Int, Nil>\n"
                );
                eval(&mut session, "let (tx2, rx2) = channel::new::<Int>(1)");
                eval(
                    &mut session,
                    "fiber::spawn_detached(fn() { channel::send(tx2, 2) })",
                );
                assert_eq!(
                    eval(&mut session, "channel::receive(rx2)"),
                    "Ok(2) : Result<Int, Nil>\n"
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// D-857's stuck groups (the user's answer to UQ-12 (b)), D-859 and
    /// D-827, at one and at two workers: an entry's detached fiber blocks
    /// in a read of a FIFO on the blocking pool, which the test holds open
    /// without writing, so `:reload` cannot stop it and keeps its group as
    /// a stuck one. An entry that deadlocks later is still reported while
    /// that read is pending, with no line for the stuck fiber; the entry is
    /// reverted, so its binding does not exist, and the next entry runs.
    /// Once the test writes to the FIFO, the read returns, and the
    /// session's end stops the fiber and ends cleanly.
    #[cfg(unix)]
    #[test]
    fn deadlock_after_a_stuck_group() {
        for threads in [1, 2] {
            deadlock_after_a_stuck_group_at(threads);
        }
    }

    /// [`deadlock_after_a_stuck_group`] on `threads` workers.
    #[cfg(unix)]
    fn deadlock_after_a_stuck_group_at(threads: usize) {
        use std::io::Write as _;

        let dir = std::env::temp_dir().join(format!(
            "cheby_session_stuck_{}_{threads}",
            std::process::id()
        ));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success(), "mkfifo failed");
        // Opening the FIFO to write waits until the fiber's read has opened
        // it; the test then holds it open without writing, so the read
        // waits on the blocking pool.
        let opener = {
            let fifo = fifo.clone();
            std::thread::spawn(move || std::fs::OpenOptions::new().write(true).open(&fifo).unwrap())
        };
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(move || {
                let mut session = start(SessionOptions {
                    threads: Some(threads),
                    ..SessionOptions::default()
                });
                eval(&mut session, "import std::channel");
                eval(&mut session, "import std::fiber");
                eval(&mut session, "import std::io");
                let spawn = format!(
                    "fiber::spawn_detached(fn() {{ io::read_file(\"{}\") }})",
                    fifo.display()
                );
                assert_eq!(eval(&mut session, &spawn), "");
                let mut writer = opener.join().unwrap();
                let first = Arc::clone(&session.group);
                let mut out = Buffer::default();
                session.reload(&mut out).unwrap();
                assert!(
                    out.text.contains("this session has no package"),
                    "{}",
                    out.text
                );
                // The fiber waits on the pool, so the teardown skipped it
                // and its group is a stuck one (D-827).
                assert_eq!(first.live(), 1);
                assert_eq!(session.stuck.len(), 1);
                assert_eq!(
                    eval(&mut session, "let (tx, rx) = channel::new::<Int>(0)"),
                    ""
                );
                assert_eq!(
                    eval(&mut session, "let got = channel::receive(rx)"),
                    "fatal error: all fibers are blocked\n  \
                     blocked in channel::receive at <repl:6>:1:11\n",
                    "CHEBY_THREADS={threads}"
                );
                // The read is still pending.
                assert_eq!(first.live(), 1);
                let reverted = eval(&mut session, "got");
                assert!(reverted.starts_with("error[E0042]"), "{reverted}");
                assert_eq!(eval(&mut session, "1 + 2"), "3 : Int\n");
                writer.write_all(b"x").unwrap();
                drop(writer);
                assert_eq!(session.end(), None);
            })
            .unwrap()
            .join()
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The entries of the redefinition test (plan L2; the CLI test
    /// `repl_runtime_redefinition_under_stress` runs the same ones): a
    /// detached fiber calls `f` through its slot in a loop while 200
    /// entries redefine `f` with the same signature. It sends the number of
    /// changes it saw once `f` returns 200, or `-1` if it ever saw an older
    /// definition after a newer one.
    fn redefinitions() -> Vec<String> {
        let mut entries: Vec<String> = [
            "import std::channel",
            "import std::fiber",
            "fn f() -> Int {\n  0\n}",
            "fn watch(last: Int, changes: Int, done: channel::Sender<Int>) -> Int {\n  \
             let now = f()\n  case {\n    now < last => {\n      \
             let _ = channel::send(done, -1)\n      changes\n    }\n    \
             now == 200 => {\n      let _ = channel::send(done, changes + 1)\n      \
             changes\n    }\n    now > last => watch(now, changes + 1, done)\n    \
             _ => watch(last, changes, done)\n  }\n}",
            "let (done_tx, done_rx) = channel::new::<Int>(1)",
            "fiber::spawn_detached(fn() { watch(0, 0, done_tx) })",
        ]
        .map(str::to_owned)
        .to_vec();
        entries.extend((1..=200).map(|n| format!("fn f() -> Int {{\n  {n}\n}}")));
        entries
    }

    /// D-712: slots are repointed with release stores while a detached
    /// fiber runs the code that loads them. The fiber sees only whole
    /// words, never an older `f` after a newer one, and the last `f`.
    fn redefinitions_under_a_detached_caller(threads: usize) {
        for _ in 0..3 {
            let printed = std::thread::Builder::new()
                .stack_size(256 << 20)
                .spawn(move || {
                    let mut session = start(SessionOptions {
                        threads: Some(threads),
                        ..SessionOptions::default()
                    });
                    for entry in redefinitions() {
                        assert_eq!(eval(&mut session, &entry), "", "{entry}");
                    }
                    eval(&mut session, "channel::receive(done_rx)")
                })
                .unwrap()
                .join()
                .unwrap();
            let changes: i64 = printed
                .strip_prefix("Ok(")
                .and_then(|rest| rest.strip_suffix(") : Result<Int, Nil>\n"))
                .and_then(|n| n.parse().ok())
                .unwrap_or_else(|| panic!("{printed}"));
            assert!((1..=200).contains(&changes), "{printed}");
        }
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn session_redefinitions_under_a_detached_caller_at_two_threads() {
        redefinitions_under_a_detached_caller(2);
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn session_redefinitions_under_a_detached_caller_at_four_threads() {
        redefinitions_under_a_detached_caller(4);
    }

    #[test]
    fn entries_are_declarations_or_statements() {
        assert_eq!(entry_kind("fn f() {\n  1\n}"), Some(true));
        assert_eq!(
            entry_kind("  // a comment\npub type T {\n  T,\n}"),
            Some(true)
        );
        assert_eq!(entry_kind("/// Docs.\nfn f() {\n  1\n}"), Some(true));
        assert_eq!(entry_kind("@external(native, \"f\")\nfn f()"), Some(true));
        assert_eq!(entry_kind("import std::list"), Some(true));
        assert_eq!(entry_kind("let x = 1"), Some(false));
        assert_eq!(entry_kind("fnord(1)"), Some(false));
        assert_eq!(entry_kind("fn(x: Int) { x + 1 }"), Some(false));
        assert_eq!(entry_kind("fn (x: Int) { x }(1)"), Some(false));
        assert_eq!(entry_kind("1 + 2"), Some(false));
        assert_eq!(entry_kind("   \n// only a comment\n"), None);
    }

    #[test]
    fn transcripts_split_into_entries() {
        let transcript =
            "> 1 + 2\n3 : Int\n> fn f() -> Int {\n.   1\n. }\n> io::read_line()\n< a line\n";
        assert_eq!(
            transcript_entries(transcript),
            vec!["1 + 2", "fn f() -> Int {\n  1\n}", "io::read_line()"]
        );
        assert_eq!(
            entry_source("1 + 2").unwrap(),
            "test \"<repl>\" {\n1 + 2\nNil\n}\n"
        );
        assert_eq!(entry_source("import std::io").unwrap(), "import std::io");
        assert_eq!(entry_source("// nothing"), None);
    }

    /// `:type` checks without running or keeping anything, and E0054
    /// counts only live bindings (§13.5.3, D-475).
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn types_and_live_native_bindings() {
        let printed = std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(|| {
                let mut session = start(options());
                let mut out = Buffer::default();
                session.type_of("1 / 0", &mut out).unwrap();
                session.type_of("let x = 1", &mut out).unwrap();
                for entry in [
                    "x",
                    "@external(native, \"abs\")\nfn m(n: I32) -> I32",
                    "@external(native, \"abs\")\nfn w(n: Int) -> Int",
                    "@external(native, \"abs\")\nfn m(n: Int) -> Int",
                    "@external(native, \"abs\")\nfn w(n: Int) -> Int",
                ] {
                    session.eval(entry, &mut out).unwrap();
                }
                session.enter_module("nowhere", &mut out).unwrap();
                session.reload(&mut out).unwrap();
                out.text
            })
            .unwrap()
            .join()
            .unwrap();
        assert!(printed.starts_with("1 / 0 : Int\n"), "{printed}");
        assert!(printed.contains("`:type` needs an expression"), "{printed}");
        // `:type let x = 1` bound nothing.
        assert!(printed.contains("error[E0042]"), "{printed}");
        // `w` conflicts with the live `m`; after `m` is redefined with the
        // same C signature as `w`, `w` is accepted.
        assert_eq!(printed.matches("error[E0054]").count(), 1, "{printed}");
        assert!(
            printed.contains("there is no module `nowhere`"),
            "{printed}"
        );
        assert!(printed.contains("this session has no package"), "{printed}");
    }

    #[test]
    fn reloaded_modules_get_marked_paths() {
        let path = ModulePath::root("store").child("prices");
        assert_eq!(reload_path(&path, 2).to_string(), "store::prices<r2>");
        assert_eq!(
            reload_path(&ModulePath::root("store"), 1).to_string(),
            "store<r1>"
        );
        let mut replaced = BTreeMap::new();
        replaced.insert(ModuleId::from_u32(3), ModuleId::from_u32(7));
        replaced.insert(ModuleId::from_u32(7), ModuleId::from_u32(9));
        assert_eq!(
            newest(&replaced, ModuleId::from_u32(3)),
            ModuleId::from_u32(9)
        );
        assert_eq!(
            newest(&replaced, ModuleId::from_u32(4)),
            ModuleId::from_u32(4)
        );
    }

    #[test]
    fn masking_keeps_positions() {
        let text = "fn a() {\n  1\n}\nfn é() {\n  2\n}\n";
        let start = u32::try_from(text.find("fn é").unwrap()).unwrap();
        let end = u32::try_from(text.len()).unwrap();
        let masked = mask(text, TextRange::new(start, end));
        assert_eq!(masked.len(), text.len());
        assert_eq!(masked.lines().count(), text.lines().count());
        assert!(masked.ends_with("fn é() {\n  2\n}\n"));
        assert!(masked.starts_with("        \n"));
    }
}
