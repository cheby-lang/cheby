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
//!    printed by the runtime and abandons the entry (D-283); no leak check
//!    runs (D-458).
//! 5. The value of a final expression prints as `<debug text> : <type>`,
//!    nothing for `Nil` (D-546); the values of the entry's `let`s go in
//!    their slots (D-547). Only then is the entry committed: its names
//!    join the session. An entry rejected after its code compiled gives the
//!    slots it repointed back.
//! 6. The definitions that used something the entry redefined with another
//!    signature are checked again from their own text; those that still
//!    check are compiled again and take over their slots, and the others
//!    are reported and keep their old code (D-043, §13.5.2).
//!
//! The session's front end loads every shipped std module (a synthetic
//! package module imports them all), so any std import resolves without
//! loading modules later. Diagnostics and panic reports follow the output
//! written before them: [`Output::stderr`] flushes standard output first
//! (D-548).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use cheby_codegen_clif::SessionName;
use cheby_consteval::{ConstEvalConfig, ConstInfo, ConstMode};
use cheby_diag::{Colors, Diagnostic, FileId, SourceMap, Span, TextRange, codes};
use cheby_iface::{
    DefId, DepView, DisplayCx, Ice, ModuleId, ModuleInfo, ModuleInterface, Name, PackageId,
};
use cheby_jit::{Jit, JitError, JitOptions};
use cheby_mir::reach::{Reach, Root, reachable_with_hooks};
use cheby_mir::{
    Defs, EntryKind, EntryRequest, LowerCx, MirModule, Program, Unsupported, count_and_check,
    lower_entries, lower_items, lower_tests,
};
use cheby_package::{Manifest, ModulePath, Target, Targets};
use cheby_resolve::repl::SessionScope;
use cheby_resolve::{
    Lookup, ModuleLookup, ResolveInput, StdCatalog, closest_name, collect_imports,
    resolve_import_targets,
};
use cheby_runtime::{EntryConfig, EntryMode, EntryOutcome};
use cheby_syntax::SyntaxTree;
use cheby_syntax::ast::{AstNode, SourceFile};
use cheby_types::repl::{CheckedEntry, EntryShape, Redefinition, ReplSession};
use cheby_types::thir;

use crate::frontend::{self, Frontend};
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
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
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
    /// An [`Error`] for failures that are not diagnostics.
    pub fn start(dir: &Path, options: SessionOptions) -> Result<Start, Error> {
        let mut sources = SourceMap::new();
        let mut diagnostics = Vec::new();
        let package = match find_package_root(dir) {
            Some(root) => crate::load_package(&root, &mut sources, &mut diagnostics)?,
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
        // run`, and `env::args()` is empty (D-578, GAP(repl-program)).
        cheby_runtime::entry::set_program(Vec::new(), front.package_name.clone());
        let jit = Jit::configured(
            cheby_codegen_clif::CodegenOptions::default(),
            JitOptions::from_env(),
        )?;
        let session = Self {
            options,
            sources,
            repl: ReplSession::new(SessionScope::new(synthetic_span, ast)),
            front,
            jit,
            entries: BTreeMap::new(),
            compiled: BTreeSet::new(),
            units: BTreeSet::new(),
            evaluated: BTreeSet::new(),
            count: 0,
            again: 0,
            known: std_lib::known(),
            kept: Vec::new(),
            again_note: None,
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
        let entry = self.check(&name, number, &parsed, shape, diagnostics, out)?;
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
                    checked = Some(entry);
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
        };
        let (targets, mut import_diags) =
            resolve_import_targets(&imports, PackageId::LOCAL, &mut lookup, names)?;
        let deps = self.all_modules();
        let input = ResolveInput {
            module,
            package: PackageId::LOCAL,
            tree,
            imports: &imports,
            targets: &targets,
            deps: DepView::new(&self.front.set, &deps),
            modules: &self.front.modules,
            globals: &self.front.globals,
            std: StdCatalog { known: &self.known },
            // The session runs natively (§13.5, the JIT): an entry is
            // checked for `native` only, so a native-only item of the
            // package is usable (GAP(repl-targets)).
            package_targets: self
                .front
                .package_targets
                .intersection(Targets::only(Target::Native)),
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
        let built = match self.lower(entry)? {
            Ok(built) => built,
            Err(mut diagnostics) => {
                self.report(&mut diagnostics, out);
                return Ok(false);
            }
        };
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
        let config = EntryConfig {
            stack_size: cheby_runtime::entry::DEFAULT_ROOT_STACK_SIZE,
            ..EntryConfig::new(EntryMode::Repl)
        };
        let outcome = self
            .jit
            .entry(EntryKind::Repl(entry.number))?
            .run(&config)?;
        let word = match outcome {
            EntryOutcome::Returned(word) => word,
            // The runtime printed the report after flushing standard output;
            // the entry's objects are abandoned with its stack (D-283).
            EntryOutcome::Panicked(_) => {
                self.jit.revert_repoints()?;
                return Ok(false);
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
            let ty = self.show_ty(body.shown, &checked.iface);
            out.stdout(&format!("{debug} : {ty}\n"));
        }
        Ok(true)
    }

    /// A type as the REPL prints it (D-463, D-546).
    fn show_ty(&self, ty: cheby_iface::Ty, own: &ModuleInterface) -> String {
        let all = self.all_modules();
        let deps = DepView::new(&self.front.set, &all);
        let cx = DisplayCx {
            globals: &self.front.globals,
            modules: &self.front.modules,
            own: Some(own),
            deps: &deps,
            generics: None,
        };
        self.repl.show_ty(ty, &cx)
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
        self.repl.commit(&checked, &self.front.globals.names);
        self.front.set.insert(Arc::new(checked.iface))?;
        self.entries.insert(checked.module, checked.thir);
        Ok(())
    }

    /// Checks again, from their own text, the definitions that used a
    /// definition `changed` replaced with another signature (§13.5.2,
    /// D-043): one that still checks is compiled again and replaces the
    /// old one (taking over its slot), and one that does not is reported
    /// and keeps its old code.
    fn check_again(&mut self, changed: &[Redefinition], out: &mut dyn Output) -> Result<(), Error> {
        let dependents: Vec<(DefId, Name, Span)> = self
            .repl
            .dependents(changed)
            .iter()
            .filter_map(|item| Some((item.def, item.name, item.source?)))
            .collect();
        for (_, name, source) in dependents {
            let Some(file) = self.sources.file(source.file) else {
                continue;
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
                "`{text}` was checked again because a definition it uses changed its \
                 signature; it keeps its old definition"
            ));
            let entry = self.check(&module_name, 0, &parsed, shape, diagnostics, out);
            let result = match entry {
                Ok(Some(entry)) => match self.build_and_run(&entry, out) {
                    Ok(true) => self.commit(entry),
                    Ok(false) => Ok(()),
                    Err(e) => Err(e),
                },
                Ok(None) => Ok(()),
                Err(e) => Err(e),
            };
            self.again_note = None;
            result?;
        }
        Ok(())
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
    /// Lowers what `entry` reaches that is not compiled yet. `Err` holds
    /// E0038 diagnostics.
    fn lower(&self, entry: &Entry) -> Result<Result<Built, Vec<Diagnostic>>, Error> {
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
        let mut unsupported = Unsupported::default();
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
            if let Some(mut unit) = unsupported.take(lower_items(thir, &wanted, &cx))? {
                count_and_check(&mut unit)?;
                units.push(unit);
            }
            items.extend(new);
            lowered_modules.push(module);
        }
        let constants: Vec<DefId> = reach
            .constant_order()
            .into_iter()
            .filter(|def| !self.evaluated.contains(def))
            .collect();
        units.extend(self.lower_entry_units(entry, &constants, &all, &mut unsupported)?);
        if !unsupported.is_empty() {
            return Ok(Err(unsupported.into_diagnostics()));
        }
        Ok(Ok(Built {
            units,
            items,
            modules: lowered_modules,
            constants,
            reach,
        }))
    }

    /// The entry's tests unit (its body) and its entries unit: the thunk of
    /// its body and those of `constants`.
    fn lower_entry_units(
        &self,
        entry: &Entry,
        constants: &[DefId],
        all: &[ModuleId],
        unsupported: &mut Unsupported,
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
            if let Some(mut unit) = unsupported.take(lower_tests(&tests, &cx))? {
                count_and_check(&mut unit)?;
                units.push(unit);
            }
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
        if !requests.is_empty()
            && let Some(mut unit) = unsupported.take(lower_entries(&requests, &cx))?
        {
            count_and_check(&mut unit)?;
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
                return Ok(Lookup::Found(id));
            }
            let text = module_path.to_string();
            let candidates: Vec<String> = self
                .front
                .modules
                .iter()
                .filter(|(_, info)| {
                    info.package == PackageId::LOCAL
                        && info.path.package() == self.package
                        && info
                            .path
                            .segments()
                            .last()
                            .is_none_or(|s| s != SESSION_FILE)
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

    /// `--deny-warnings` refuses an entry with a warning (D-549).
    #[test]
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
