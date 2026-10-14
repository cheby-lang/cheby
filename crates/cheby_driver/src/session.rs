//! REPL sessions (§13.5, G5, plan §8.3): the driver side of `cheby repl`.
//!
//! A [`Session`] holds the package's front end, the session's front-end
//! state ([`ReplSession`]), one [`Jit`] for the whole session, and what it
//! compiled and evaluated so far. [`Session::eval`] takes one entry:
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
//! 4. New constants are evaluated (E6); the body runs on its own stack as
//!    the root of its entry (§13.5.4, D-457, `EntryMode::Repl`). A panic is
//!    printed by the runtime, and the entry unwinds by the return flag: its
//!    pads release what its frames held and its thread ends (D-712,
//!    D-733). No leak check runs (D-458).
//! 5. The value of a final expression prints as `<debug text> : <type>`,
//!    nothing for `Nil` (D-546); the values of the entry's `let`s go in
//!    their slots (D-547). Only then is the entry committed: its names
//!    join the session. An entry rejected after its code compiled gives the
//!    slots it repointed back.
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
//!   (`:reload`).
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
use cheby_jit::{Jit, JitError, JitOptions};
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
use cheby_runtime::{EntryConfig, EntryMode, EntryOutcome};
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
    /// The MIR options every entry is lowered with, read once when the
    /// session starts (`CHEBY_RC`, plan §2.3), with borrowing off
    /// (GAP(borrow-off-in-repl)).
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
    /// so it is never freed (D-458: no leak check in the REPL).
    kept: Vec<u64>,
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
}

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
    /// invalid setting in the environment: `CHEBY_THREADS` (D-658), which
    /// the REPL checks although its entries run isolated (D-713), and the
    /// compiler's `CHEBY_RC` and `CHEBY_UNWIND_EDGES`.
    pub fn start(dir: &Path, options: SessionOptions) -> Result<Start, Error> {
        cheby_runtime::sched::threads_from_env().map_err(|e| Error::new(e.to_string()))?;
        // No borrowing in any unit a session lowers, std and package
        // modules included (D-715, GAP(borrow-off-in-repl)): a later batch
        // could tail-call a function compiled with a mask its slot could
        // not change. Each unit is counted on its own anyway, which never
        // infers; the switch says so where the options are made.
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
        // run`, and `env::args()` is empty (D-578, D-612).
        cheby_runtime::entry::set_program(Vec::new(), front.package_name.clone());
        let jit = Jit::configured(codegen, JitOptions::from_env())?;
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
        };
        Ok(Start::Ready(Box::new(session), warnings))
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
        match self.jit.compile(&built.units) {
            Ok(()) => {}
            Err(JitError::UnresolvedSymbols(symbols)) => {
                let mut diagnostics = self.unresolved(&entry.checked, &symbols);
                self.report(&mut diagnostics, out);
                return Ok(false);
            }
            Err(e) => return Err(e.into()),
        }
        self.repl.note_compiled(checked);
        self.compiled.extend(built.items);
        self.units.extend(built.modules);
        self.const_thunks.extend(built.constants.iter().copied());
        if !built.constants.is_empty() {
            let mut diagnostics = self.evaluate(&built.constants, &built.reach, checked)?;
            if !diagnostics.is_empty() {
                self.jit.revert_repoints()?;
                self.report(&mut diagnostics, out);
                return Ok(false);
            }
            self.evaluated.extend(built.constants.iter().copied());
        }
        let Some(body) = &checked.body else {
            return Ok(true);
        };
        // The entry carries whether the session's code has unwind edges
        // (D-742): under `CHEBY_UNWIND_EDGES=off` a raise ends the process
        // with its report (D-770).
        let config = EntryConfig {
            stack_size: cheby_runtime::entry::DEFAULT_ROOT_STACK_SIZE,
            unwind_edges: self.jit.unwind_edges(),
            ..EntryConfig::new(EntryMode::Repl)
        };
        let outcome = self
            .jit
            .entry(EntryKind::Repl(entry.number))?
            .run(&config)?;
        let word = match outcome {
            EntryOutcome::Returned(word) => word,
            // The runtime printed the report after flushing standard output;
            // the entry unwound by the return flag, its pads released what
            // its frames held, and its thread ended (D-712, D-733).
            EntryOutcome::Panicked(_) => {
                self.jit.revert_repoints()?;
                return Ok(false);
            }
            // A write failed (D-306, D-709): the entry's isolated thread is
            // parked for good, so the code it runs is kept (leaked) and no
            // value it may reach is dropped. The session ends with the
            // status and prints nothing more (spec §13.5): the REPL is the
            // host here, and nothing else of the process may run.
            EntryOutcome::Exited(status) => {
                std::mem::forget(self.jit.code_token());
                std::process::exit(status)
            }
        };
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

    /// E0054 for each native symbol `entry` binds that a live binding binds
    /// with another C signature (D-475): a binding of the package or the
    /// standard library, or of a session item that is still the session's
    /// definition of its name and that the entry does not replace.
    /// Replaced definitions and entries that failed do not count.
    fn native_conflicts(&self, entry: &CheckedEntry) -> Vec<Diagnostic> {
        let replaced: Vec<DefId> = entry.redefinitions().map(|r| r.old).collect();
        let mut out = Vec::new();
        for (_, symbol, binding) in
            entry_natives(&entry.resolved, &entry.iface, &self.front.globals)
        {
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
            .map(|symbol| {
                let own = entry
                    .resolved
                    .items
                    .iter()
                    .find_map(|item| item.attrs.native_symbol().filter(|(s, _)| s == symbol));
                let span = own
                    .map(|(_, span)| span)
                    .or_else(|| self.front.natives.get(symbol.as_str()).map(|b| b.span));
                let diag = Diagnostic::new(
                    codes::UNRESOLVED_NATIVE_SYMBOL,
                    format!("the native symbol `{symbol}` is not defined"),
                )
                .with_note(
                    "the symbol must be defined by the Cheby runtime or the running process",
                );
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

    /// `:reload` (§13.5.3, D-098, D-547, D-625): reads the
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
            let bindings = self
                .front
                .module_natives
                .get(usize::try_from(id.as_u32()).unwrap_or(usize::MAX))
                .cloned()
                .unwrap_or_default();
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
    /// The items it lowered.
    items: Vec<DefId>,
    /// The modules whose code units it compiles.
    modules: Vec<ModuleId>,
    /// The constants to evaluate, in order.
    constants: Vec<DefId>,
    reach: Reach,
}

impl Session {
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
        let wanted = |def: DefId| reach.contains(def) && !self.compiled.contains(&def);
        for module in modules {
            if !reach.needs_module(module) {
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
                let hash = module_info
                    .file
                    .and_then(|file| self.sources.file(file))
                    .map_or(0, |f| {
                        cheby_runtime::consteval::source_hash(f.text().as_bytes())
                    });
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
        Ok(cheby_consteval::evaluate(&mut self.jit, &infos, &config)?)
    }
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
    let source = parse_file(sources, SESSION_FILE, text.into_bytes(), diagnostics)?;
    let file = source.file;
    Ok((
        crate::Module {
            path: ModulePath::root(&package.manifest.name).child(SESSION_FILE),
            source,
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

    /// Runs `entries` in a session outside any package, on a thread with
    /// the command's stack (D-601), and returns what each printed.
    fn session(entries: &[&str]) -> Vec<String> {
        session_with(SessionOptions::default(), entries)
    }

    /// [`session`] with `options`.
    fn session_with(options: SessionOptions, entries: &[&str]) -> Vec<String> {
        let entries: Vec<String> = entries.iter().map(|e| (*e).to_owned()).collect();
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(move || {
                let dir = std::env::temp_dir();
                let Start::Ready(mut session, warnings) = Session::start(&dir, options).unwrap()
                else {
                    panic!("the session did not start");
                };
                assert_eq!(warnings, "");
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

    /// No unit a session lowers borrows (D-715, GAP(borrow-off-in-repl)):
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
                let dir = std::env::temp_dir();
                let Start::Ready(mut session, _) =
                    Session::start(&dir, SessionOptions::default()).unwrap()
                else {
                    panic!("the session did not start");
                };
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

    /// `--deny-warnings` refuses an entry with a warning (D-549).
    #[test]
    #[cfg_attr(
        windows,
        ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
    )]
    fn denied_warnings_refuse_the_entry() {
        let options = SessionOptions {
            deny_warnings: true,
            ..SessionOptions::default()
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
                let dir = std::env::temp_dir();
                let Start::Ready(mut session, _) =
                    Session::start(&dir, SessionOptions::default()).unwrap()
                else {
                    panic!("the session did not start");
                };
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
