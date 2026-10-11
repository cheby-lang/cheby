//! Checking REPL entries against their session (§13.5, D-043, D-098,
//! D-193, D-547), without running anything.
//!
//! An entry is a module of its own (`<repl:N>`, plan §8.3). Its source is
//! either declarations and imports ([`EntryShape::Decls`]), checked as a
//! module, or statements ([`EntryShape::Body`]): the driver parses those
//! inside the wrapper `test "<repl>" {` … `Nil }`, and [`ReplSession`]
//! turns the checked test into the entry's body, which returns the tuple
//! `(shown, b1, …, bk)`: the value of a final expression statement (`Nil`
//! when there is none), then every name the entry's top-level `let` and
//! `let assert` statements bind. Those names become session values: items
//! appended to the entry's interface as constants ([`ReplBinding`]), whose
//! slots the session fills after the entry ran (D-547).
//!
//! The session's names come from [`SessionScope`] (synthetic imports, see
//! `cheby_resolve::repl`), so an entry reads only interfaces (D-246). A
//! name an entry defines again is a redefinition ([`Redefinition`]):
//!
//! - a function or constant with the same signature takes over the old
//!   one's slot, so existing callers and readers reach the new one (D-043);
//! - with another signature it gets a slot of its own, and the definitions
//!   that use the old one are checked again ([`ReplSession::dependents`]);
//! - a type or interface is a different one, and the old one is stale
//!   ([`ReplSession::is_stale`]); an alias is replaced for later entries.
//!
//! Every item of a session carries a generation: how many definitions of
//! its name were compiled before it ([`ItemName`]), which code generation
//! appends to its symbols as `$r<gen>` (D-547).
//!
//! A prompt-level `let` shadows: its session value never shares a slot
//! with what it replaces, and nothing is checked again (D-608,
//! [`RedefinitionKind::Rebinding`]).
//!
//! For interface satisfaction the prompt entries count as one module
//! (D-610): a session type satisfies an interface through the session's
//! current functions, by [`Forwarder`]s appended to the type's entry
//! module ([`ReplSession::satisfaction`]).
//!
//! Stale types print with ` (stale)` (§13.5.5): entries are checked, and
//! values printed, with [`ReplSession::display_table`], which marks the
//! modules of stale types, and [`strip_session_paths`] writes the marks
//! out. `:module` (`ReplSession::enter`) and `:reload`
//! (`ReplSession::replace_module`) change what later entries see.

use std::collections::{BTreeMap, BTreeSet};

use cheby_diag::{Diagnostic, FileId, Span, TextRange, codes};
use cheby_iface::{
    ConstSig, DefId, DefIndex, DefKind, DisplayCx, FnSig, Generics, Ice, IfaceItem, ItemSig,
    ModuleId, ModuleInfo, ModuleInterface, ModuleTable, Name, Namespace, Ty, TyKind, Vis,
};
use cheby_resolve::repl::{SessionBinding, SessionScope, keep_entry_diagnostic};
use cheby_resolve::{ImportRequest, ImportTarget, Res, ResolveInput, ResolvedModule};
use rustc_hash::FxHashMap;

use crate::thir::{self, ExprId, ExprKind, Stmt};
use crate::{Tests, TypesInput, build_interface, check_bodies_with};

/// The test name of an entry's body in the wrapped source.
pub const BODY_TEST_NAME: &str = "<repl>";

/// How an entry was parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryShape {
    /// Declarations and imports, parsed as a module.
    Decls,
    /// Statements, parsed inside the wrapper `test "<repl>" {` … `Nil }`.
    /// The user's text starts at byte `shift` of the parsed file; spans
    /// there move to the same text at offset 0 of file `display`.
    Body { display: FileId, shift: u32 },
    /// A declaration of an earlier entry checked again
    /// ([`ReplSession::dependents`]), parsed from a copy of that entry's
    /// text with everything else blanked: spans move to the entry's file
    /// `display` at the same offsets.
    Again { display: FileId },
}

impl EntryShape {
    /// Moves the spans of `diags`, found in the parsed file `file` (syntax
    /// errors, for example), to the entry's own text.
    pub fn move_diagnostics(self, file: FileId, diags: &mut [Diagnostic]) {
        if let Some(mv) = self.mover(file) {
            mv.diagnostics(diags);
        }
    }

    /// How spans move from the parsed file `file` to the entry's own text.
    const fn mover(self, file: FileId) -> Option<Move> {
        match self {
            Self::Decls => None,
            Self::Body { display, shift } => Some(Move {
                from: file,
                to: display,
                shift,
            }),
            Self::Again { display } => Some(Move {
                from: file,
                to: display,
                shift: 0,
            }),
        }
    }
}

/// What kind of redefinition an entry makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedefinitionKind {
    /// A function or constant with the same signature or type: it takes
    /// over the old one's slot (D-043, D-547, D-607).
    SameSignature,
    /// One with another signature or type, or of another kind: a new slot,
    /// and the old one's users are checked again.
    NewSignature,
    /// A type or an interface: the old one is stale (D-043, D-547).
    Type,
    /// An alias, replaced for later entries only (D-547).
    Alias,
    /// A constructor, replaced for later entries only: a constructor takes
    /// over nothing (D-607), and its users keep the old one, as the users
    /// of a redefined type keep the old type, whose values are stale
    /// (D-609, D-610).
    Constructor,
    /// A session value made by a prompt-level `let`, or replaced by one:
    /// it shadows (D-608). It never shares a slot with what it replaces
    /// (D-607), and nothing is checked again (D-609).
    Rebinding,
}

/// A session name that an entry defines again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Redefinition {
    pub ns: Namespace,
    pub name: Name,
    pub old: DefId,
    pub new: DefId,
    pub kind: RedefinitionKind,
}

/// A name an entry adds to the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Define {
    pub ns: Namespace,
    pub name: Name,
    pub def: DefId,
    pub replaces: Option<Redefinition>,
}

/// How code generation names one item of an entry (D-547): its name in
/// the session (`f`, `T`, `I::f`), its generation, and the earlier
/// definition whose slot it takes over, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemName {
    pub def: DefId,
    pub text: Box<str>,
    pub generation: u32,
    pub reuse: Option<DefId>,
    /// The top-level name the generation counts.
    key: (Namespace, Name),
}

/// A session value bound by a top-level `let` or `let assert`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplBinding {
    pub name: Name,
    /// The constant item appended to the entry's interface.
    pub def: DefId,
    pub ty: Ty,
    pub span: Span,
}

/// The body of a statements entry.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplBody {
    /// The entry's `test` item, whose interface signature is now
    /// `fn() -> (shown, b1, …)` and whose THIR body returns that tuple.
    pub def: DefId,
    /// The type of the value shown (`Nil`: nothing is printed).
    pub shown: Ty,
    pub bindings: Vec<ReplBinding>,
    /// The type of the entry's final expression statement, `Never`
    /// included; `None` when its last statement is not an expression. What
    /// `:type` shows (§13.5.3).
    pub last: Option<Ty>,
}

/// An entry that checked.
#[derive(Clone, Debug)]
pub struct CheckedEntry {
    pub module: ModuleId,
    /// The entry's number in the session's order of committed entries.
    pub stamp: u32,
    pub resolved: ResolvedModule,
    pub iface: ModuleInterface,
    pub thir: thir::Module,
    pub body: Option<ReplBody>,
    pub defines: Vec<Define>,
    pub names: Vec<ItemName>,
    /// The entry's own imports with their targets.
    pub imports: Vec<(ImportRequest, ImportTarget)>,
    /// Where each top-level declaration is written, for checking it again.
    pub sources: Vec<(DefId, Span)>,
    /// Its warnings.
    pub warnings: Vec<Diagnostic>,
}

impl CheckedEntry {
    /// The redefinitions it makes.
    pub fn redefinitions(&self) -> impl Iterator<Item = Redefinition> + '_ {
        self.defines.iter().filter_map(|d| d.replaces)
    }
}

/// A name the session defines, as committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionItem {
    pub def: DefId,
    pub ns: Namespace,
    pub name: Name,
    pub kind: DefKind,
    pub stamp: u32,
    /// Where it is declared (in the entry's own text), for checking it
    /// again; `None` for session values.
    pub source: Option<Span>,
    /// The items its signature and body refer to, and the functions it
    /// used to satisfy interfaces ([`Forwarder`]).
    pub refs: BTreeSet<DefId>,
    /// A constructor's type.
    pub parent: Option<DefId>,
    /// The module entered with `:module` when it was entered: it is
    /// checked again in that module (§13.5.3).
    pub inside: Option<ModuleId>,
}

/// A function of a session type's entry module through which the type
/// satisfies interfaces with a session function of another entry (D-610,
/// D-627): §8.2 looks for a type's functions
/// in the module that declares it, and the prompt entries count as one
/// module there. It has the target's signature and calls the target
/// through its slot, so a redefinition with the same signature reaches it
/// too (D-607).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Forwarder {
    /// The forwarder: an item appended to the type's entry module.
    pub def: DefId,
    /// The session function it calls.
    pub target: DefId,
    /// The session type it is for.
    pub ty: DefId,
    /// The function's name.
    pub name: Name,
    /// Whether the type's module finds it by name: only while its target,
    /// or a redefinition that took over the target's slot, is the
    /// session's function of that name (D-610).
    pub active: bool,
}

/// A forwarder [`ReplSession::satisfaction`] asks for.
#[derive(Clone, Debug, PartialEq)]
pub struct NewForwarder {
    /// The session type, whose entry module gets the forwarder.
    pub ty: DefId,
    /// The session function it calls.
    pub target: DefId,
    /// The item to append to the type's module: the target's name,
    /// signature, visibility and targets.
    pub item: IfaceItem,
    /// Its name for code generation: `T::f`, with a generation that no
    /// earlier item of that text has (D-611).
    pub text: Box<str>,
    pub generation: u32,
}

/// What the session's interface satisfaction needs after an entry
/// (D-610): forwarders to add, and forwarders that no longer count.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SatisfactionPlan {
    pub add: Vec<NewForwarder>,
    /// Forwarders whose name their type's module no longer finds.
    pub retire: Vec<DefId>,
}

/// The front-end state of a REPL session.
#[derive(Clone, Debug)]
pub struct ReplSession {
    scope: SessionScope,
    items: Vec<SessionItem>,
    /// Every forwarder made so far, in order (D-610).
    forwarders: Vec<Forwarder>,
    /// Modules of the package that `:reload` replaced: their types are
    /// stale (§13.5.3).
    retired: BTreeSet<ModuleId>,
    /// Modules whose diagnostics show another path than their own: a
    /// module `:reload` made shows the path of the module it replaces.
    display_paths: BTreeMap<ModuleId, ModuleInfo>,
    /// Per top-level name, how many of its definitions were compiled.
    generations: FxHashMap<(Namespace, Name), u32>,
    /// Per item text (`Red`, `I::f`), the generation after the last one
    /// compiled: two constructors `Red` of different types share the
    /// symbol path `<repl>::Red`, so the second needs a later generation
    /// (D-611).
    text_generations: FxHashMap<Box<str>, u32>,
    /// Types and interfaces that a redefinition replaced.
    stale: BTreeSet<DefId>,
    /// Session values: the names prompt-level `let`s bound (D-547, D-608).
    values: BTreeSet<DefId>,
    records: Vec<Redefinition>,
    /// The committed entry modules, in order.
    modules: Vec<ModuleId>,
}

impl ReplSession {
    /// A session with no names. `synthetic` and `ast` are as for
    /// [`SessionScope::new`].
    #[must_use]
    pub fn new(scope: SessionScope) -> Self {
        Self {
            scope,
            items: Vec::new(),
            forwarders: Vec::new(),
            retired: BTreeSet::new(),
            display_paths: BTreeMap::new(),
            generations: FxHashMap::default(),
            text_generations: FxHashMap::default(),
            stale: BTreeSet::new(),
            values: BTreeSet::new(),
            records: Vec::new(),
            modules: Vec::new(),
        }
    }

    /// The session's names.
    #[must_use]
    pub const fn scope(&self) -> &SessionScope {
        &self.scope
    }

    /// The committed entry modules, in order: what a later entry may read
    /// besides the package and std.
    #[must_use]
    pub fn modules(&self) -> &[ModuleId] {
        &self.modules
    }

    /// Every redefinition committed so far, in order.
    #[must_use]
    pub fn records(&self) -> &[Redefinition] {
        &self.records
    }

    /// Whether `def`, a type or interface of the session, was replaced by a
    /// redefinition, or is an item of a package module that `:reload`
    /// replaced: its values print with ` (stale)` (§13.5.5, plan §8.3).
    #[must_use]
    pub fn is_stale(&self, def: DefId) -> bool {
        self.stale.contains(&def) || self.retired.contains(&def.module)
    }

    /// Every stale type and interface of the session, sorted.
    pub fn stale(&self) -> impl Iterator<Item = DefId> + '_ {
        self.stale.iter().copied()
    }

    /// Enters a module (`:module`), or leaves the one entered with `None`
    /// (§13.5.3, D-098).
    pub fn enter(&mut self, entered: Option<cheby_resolve::repl::Entered>) {
        self.scope.enter(entered);
    }

    /// Records that `:reload` replaced the package module `old` with `new`,
    /// which diagnostics show with the path of `shown`, the module `old`
    /// stood for: earlier imports of `old` now import `new`, and the types
    /// of `old` are stale (§13.5.3).
    pub fn replace_module(&mut self, old: ModuleId, new: ModuleId, shown: ModuleInfo) {
        self.scope.retarget(old, new);
        self.retired.insert(old);
        self.display_paths.insert(new, shown);
    }

    /// Makes diagnostics show module `module` with the path of `shown`: a
    /// module `:reload` made, while it is checked.
    pub fn show_as(&mut self, module: ModuleId, shown: ModuleInfo) {
        self.display_paths.insert(module, shown);
    }

    /// Undoes [`ReplSession::show_as`] for `modules`, the versions of a
    /// `:reload` that was rejected: they show their own marked paths, so
    /// the live versions they would have replaced are not stale.
    pub fn forget_shown(&mut self, modules: &[ModuleId]) {
        for module in modules {
            self.display_paths.remove(module);
        }
    }

    /// Whether `:reload` replaced module `module`.
    #[must_use]
    pub fn is_retired(&self, module: ModuleId) -> bool {
        self.retired.contains(&module)
    }

    /// The module path diagnostics show for `module`: that of the module it
    /// replaced if `:reload` made it.
    #[must_use]
    pub fn shown_path<'a>(&'a self, module: ModuleId, real: &'a ModuleInfo) -> &'a ModuleInfo {
        self.display_paths.get(&module).unwrap_or(real)
    }

    /// A copy of `real` for checking entries and printing types: a module
    /// with stale types gets a marked last segment, which
    /// [`strip_session_paths`] turns into ` (stale)` after the type, and a
    /// module `:reload` made shows the path of the module it replaced
    /// (D-463). Module ids do not change, and nothing but text uses paths
    /// while an entry is checked, so the copy only changes what
    /// diagnostics say.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if two modules end up with one path.
    pub fn display_table(&self, real: &ModuleTable) -> Result<ModuleTable, Ice> {
        let mut stale: BTreeSet<ModuleId> = self.retired.clone();
        stale.extend(self.stale.iter().map(|def| def.module));
        // While `:reload` checks a new version, the version it replaces is
        // shown as stale already, so no two modules share a path: of the
        // modules shown with one path, only the newest is not stale.
        let mut newest: FxHashMap<String, ModuleId> = FxHashMap::default();
        for (id, info) in real.iter() {
            let shown = self.shown_path(id, info).path.to_string();
            if let Some(older) = newest.insert(shown, id) {
                stale.insert(older);
            }
        }
        let mut table = ModuleTable::new();
        for (id, info) in real.iter() {
            if id == ModuleId::BUILTIN {
                continue;
            }
            let shown = self.shown_path(id, info);
            let shown = ModuleInfo {
                path: shown.path.clone(),
                package: info.package,
                file: info.file,
            };
            let added = table.add(if stale.contains(&id) {
                stale_path(&shown, id)
            } else {
                shown
            })?;
            if added != id {
                return Err(Ice::new("the REPL's display table moved a module"));
            }
        }
        Ok(table)
    }

    /// Every forwarder made so far (D-610).
    #[must_use]
    pub fn forwarders(&self) -> &[Forwarder] {
        &self.forwarders
    }

    /// What interface satisfaction across entries needs after `entry` was
    /// committed (D-610): a forwarder in a session type's entry module for
    /// each current session function whose parameters name the type, and
    /// the retirement of forwarders whose function is no longer the
    /// session's function of that name. `read` reads the session's
    /// interface items. A forwarder stays when its function was redefined
    /// with the same signature, because it calls through the slot the new
    /// definition took over (D-607).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a session item cannot be read.
    pub fn satisfaction<'a>(
        &self,
        entry: &CheckedEntry,
        read: &dyn Fn(DefId) -> Option<&'a IfaceItem>,
        globals: &cheby_iface::Globals,
    ) -> Result<SatisfactionPlan, Ice> {
        let mut plan = SatisfactionPlan::default();
        let types = &globals.types;
        let session_types: Vec<DefId> = self
            .items
            .iter()
            .filter(|item| item.kind == DefKind::Type)
            .map(|item| item.def)
            .collect();
        let mut text_generations = self.text_generations.clone();
        for define in &entry.defines {
            if define.ns != Namespace::Value {
                continue;
            }
            // The functions of this name that the session's name no
            // longer means: every forwarder of the name, unless the new
            // definition took over its target's slot.
            let same_slot = define
                .replaces
                .is_some_and(|r| r.kind == RedefinitionKind::SameSignature);
            let Some(item) = read(define.def) else {
                return Err(Ice::new("a session function is not readable"));
            };
            // A `priv` item never becomes a session name (D-606): the name
            // keeps its function.
            if item.vis == Vis::Priv {
                continue;
            }
            if !same_slot {
                for forwarder in &self.forwarders {
                    if forwarder.active && forwarder.name == define.name {
                        plan.retire.push(forwarder.def);
                    }
                }
            }
            if item.kind != DefKind::Fn || item.parent.is_some() {
                continue;
            }
            let ItemSig::Fn(sig) = &item.sig else {
                continue;
            };
            for &ty in &session_types {
                if !sig.params.iter().any(|p| mentions_def(types, p.ty, ty)) {
                    continue;
                }
                let kept = same_slot
                    && self
                        .forwarders
                        .iter()
                        .any(|f| f.active && f.ty == ty && f.name == define.name);
                if kept {
                    continue;
                }
                let ty_item = read(ty).ok_or_else(|| Ice::new("a session type is unreadable"))?;
                let text: Box<str> = format!(
                    "{}::{}",
                    globals.names.get(ty_item.name),
                    globals.names.get(item.name)
                )
                .into();
                let generation = text_generations.get(&text).copied().unwrap_or(0);
                text_generations.insert(text.clone(), generation.saturating_add(1));
                plan.add.push(NewForwarder {
                    ty,
                    target: define.def,
                    item: IfaceItem {
                        name: item.name,
                        kind: DefKind::Fn,
                        vis: item.vis,
                        parent: None,
                        span: item.span,
                        targets: item.targets,
                        deprecated: None,
                        sig: item.sig.clone(),
                    },
                    text,
                    generation,
                });
            }
        }
        Ok(plan)
    }

    /// Records the forwarders of `plan` (D-610): `added` are their items,
    /// in the order of `plan.add`.
    pub fn apply_satisfaction(&mut self, plan: &SatisfactionPlan, added: &[DefId]) {
        for forwarder in &mut self.forwarders {
            if plan.retire.contains(&forwarder.def) {
                forwarder.active = false;
            }
        }
        for (new, &def) in plan.add.iter().zip(added) {
            let next = new.generation.saturating_add(1);
            let slot = self.text_generations.entry(new.text.clone()).or_insert(0);
            *slot = (*slot).max(next);
            self.forwarders.push(Forwarder {
                def,
                target: new.target,
                ty: new.ty,
                name: new.item.name,
                active: true,
            });
        }
    }

    /// The item `name` currently means in `ns`, if it is a session item.
    #[must_use]
    pub fn current(&self, ns: Namespace, name: Name) -> Option<DefId> {
        match self.scope.get(ns, name)? {
            SessionBinding::Item(def) => Some(def),
            SessionBinding::Import(_) => None,
        }
    }

    /// The committed session items, in order.
    #[must_use]
    pub fn items(&self) -> &[SessionItem] {
        &self.items
    }

    /// Checks one entry: `input` holds its tree, its own imports and their
    /// targets, and its dependencies (the package, std, and
    /// [`ReplSession::modules`]). `Ok(Err(..))` holds its diagnostics when
    /// one is an error, with spans moved to the entry's own text.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if an invariant is broken.
    pub fn check_entry(
        &self,
        input: &ResolveInput<'_>,
        shape: EntryShape,
    ) -> Result<Result<CheckedEntry, Vec<Diagnostic>>, Ice> {
        let (resolved, mut diags) = self.scope.resolve_entry(input)?;
        let types = TypesInput {
            module: input.module,
            package: input.package,
            tree: input.tree,
            resolved: &resolved,
            deps: input.deps,
            modules: input.modules,
            globals: input.globals,
        };
        let (mut iface, found) = build_interface(&types)?;
        diags.extend(found);
        let (mut thir, found) = check_bodies_with(&types, &iface, Tests::All)?;
        diags.extend(found);
        let file = input.tree.file();
        diags.retain(|d| keep_entry_diagnostic(d, file));
        let mut body = None;
        if let EntryShape::Body { .. } = shape
            && !diags.iter().any(Diagnostic::is_error)
        {
            let made = make_body(&types, &mut iface, &mut thir)?;
            // The bindings are used by later entries, and the final
            // expression is shown, not discarded.
            diags.retain(|d| !made.quiet(d));
            body = Some(made.body);
        } else if let EntryShape::Body { .. } = shape {
            // An entry with an error binds nothing: its unused bindings are
            // noise next to the error.
            diags.retain(|d| d.code != codes::UNUSED_BINDING);
        }
        let mover = shape.mover(file);
        if let Some(mv) = mover {
            mv.diagnostics(&mut diags);
            mv.thir(&mut thir);
            mv.iface(&mut iface)?;
            if let Some(body) = &mut body {
                for binding in &mut body.bindings {
                    binding.span = mv.span(binding.span);
                }
            }
        }
        for diag in &mut diags {
            strip_diagnostic(diag);
        }
        if diags.iter().any(Diagnostic::is_error) {
            return Ok(Err(diags));
        }
        let stamp = u32::try_from(self.modules.len())
            .map_err(|_| Ice::new("too many REPL entries"))?
            .saturating_add(1);
        let values: Vec<DefId> = body
            .iter()
            .flat_map(|b| b.bindings.iter().map(|binding| binding.def))
            .collect();
        let (defines, names) = self.defines(&types, &iface, &values)?;
        let sources = if matches!(shape, EntryShape::Body { .. }) {
            Vec::new()
        } else {
            declaration_sources(input, &resolved)
                .into_iter()
                .map(|(def, span)| (def, mover.map_or(span, |mv| mv.span(span))))
                .collect()
        };
        let imports = input
            .imports
            .iter()
            .cloned()
            .zip(input.targets.iter().copied())
            .collect();
        Ok(Ok(CheckedEntry {
            module: input.module,
            stamp,
            resolved,
            iface,
            thir,
            body,
            defines,
            names,
            imports,
            sources,
            warnings: diags,
        }))
    }

    /// The names an entry adds, with what they replace, and the names of
    /// its items for code generation. `values` are the entry's session
    /// values: a rebinding by or of one shadows (D-608).
    fn defines(
        &self,
        input: &TypesInput<'_>,
        iface: &ModuleInterface,
        values: &[DefId],
    ) -> Result<(Vec<Define>, Vec<ItemName>), Ice> {
        let mut defines = Vec::new();
        let mut names = Vec::new();
        for (index, item) in iface.items.iter_enumerated() {
            if item.kind == DefKind::Test {
                continue;
            }
            let def = DefId::new(iface.module, index);
            let top = top_level(iface, index)?;
            let key = (
                top.kind
                    .namespace()
                    .ok_or_else(|| Ice::new("a top-level item without a namespace"))?,
                top.name,
            );
            let text = item_text(input, iface, item)?;
            let generation = self.generations.get(&key).copied().unwrap_or(0).max(
                self.text_generations
                    .get(text.as_str())
                    .copied()
                    .unwrap_or(0),
            );
            let mut reuse = None;
            if let Some(ns) = item.kind.namespace() {
                let replaces = match self.current(ns, item.name) {
                    Some(old) => Some(Redefinition {
                        ns,
                        name: item.name,
                        old,
                        new: def,
                        kind: if values.contains(&def) || self.values.contains(&old) {
                            RedefinitionKind::Rebinding
                        } else {
                            redefinition_kind(input, item, old)?
                        },
                    }),
                    None => None,
                };
                if let Some(r) = replaces
                    && r.kind == RedefinitionKind::SameSignature
                {
                    reuse = Some(r.old);
                }
                defines.push(Define {
                    ns,
                    name: item.name,
                    def,
                    replaces,
                });
            }
            names.push(ItemName {
                def,
                text: text.into(),
                generation,
                reuse,
                key,
            });
        }
        Ok((defines, names))
    }

    /// Records that `entry`'s code was compiled: later definitions of its
    /// names get the next generation, whether or not the entry is
    /// committed, so no symbol is defined twice (D-547).
    pub fn note_compiled(&mut self, entry: &CheckedEntry) {
        for name in &entry.names {
            let next = name.generation.saturating_add(1);
            let slot = self.generations.entry(name.key).or_insert(0);
            *slot = (*slot).max(next);
            let slot = self.text_generations.entry(name.text.clone()).or_insert(0);
            *slot = (*slot).max(next);
        }
    }

    /// Makes `entry` part of the session: its names now mean its items and
    /// its imports, replaced types become stale, and its redefinitions are
    /// recorded.
    pub fn commit(&mut self, entry: &CheckedEntry, names: &cheby_iface::Interner) {
        for (request, target) in &entry.imports {
            self.scope.add_import(request, *target, entry.stamp, names);
        }
        if let Some(body) = &entry.body {
            self.values
                .extend(body.bindings.iter().map(|binding| binding.def));
        }
        let inside = self.scope.entered().map(|e| e.module);
        for define in &entry.defines {
            if let Some(r) = define.replaces {
                if r.kind == RedefinitionKind::Type {
                    self.stale.insert(r.old);
                    // The old type's constructors leave the session with
                    // it, unless the new type has them too.
                    let ctors: Vec<(Namespace, Name, DefId)> = self
                        .items
                        .iter()
                        .filter(|item| item.parent == Some(r.old))
                        .map(|item| (item.ns, item.name, item.def))
                        .collect();
                    for (ns, name, def) in ctors {
                        self.scope.forget(ns, name, def);
                    }
                }
                self.records.push(r);
            }
            // A `priv` item stays in its entry (§13.5 has no rule for it,
            // and an import cannot reach it).
            if entry
                .iface
                .item(define.def.index)
                .is_some_and(|item| item.vis == Vis::Priv)
            {
                continue;
            }
            self.scope
                .define(define.ns, define.name, define.def, entry.stamp);
            let kind = entry
                .iface
                .item(define.def.index)
                .map_or(DefKind::Const, |item| item.kind);
            let source = entry
                .sources
                .iter()
                .find(|(def, _)| *def == define.def)
                .map(|&(_, span)| span);
            let mut refs = refs_of(&entry.resolved, define.def.index);
            refs.extend(thir_refs(&entry.thir, define.def));
            self.items.push(SessionItem {
                def: define.def,
                ns: define.ns,
                name: define.name,
                kind,
                stamp: entry.stamp,
                source,
                refs,
                parent: entry
                    .iface
                    .item(define.def.index)
                    .and_then(|item| item.parent)
                    .map(|p| DefId::new(entry.module, p)),
                inside,
            });
        }
        self.modules.push(entry.module);
    }

    /// The current session items that use a definition that `changed`
    /// replaced with another signature, by name or through a forwarder for
    /// interface satisfaction: they are checked again (§13.5.2, D-043,
    /// D-547, D-609). Session values are never checked again, since their
    /// value is already computed, and a `let` rebinding changes nothing
    /// (D-608).
    #[must_use]
    pub fn dependents(&self, changed: &[Redefinition]) -> Vec<&SessionItem> {
        let mut olds: Vec<DefId> = changed
            .iter()
            .filter(|r| r.kind == RedefinitionKind::NewSignature)
            .map(|r| r.old)
            .collect();
        // Every earlier definition of a changed name, not only the one just
        // replaced: a user that an earlier signature change broke still
        // runs code compiled against an older one, and the new definition
        // may fit it again (§13.5.2, D-043, D-547).
        for item in &self.items {
            if !olds.contains(&item.def)
                && changed.iter().any(|r| {
                    r.kind == RedefinitionKind::NewSignature
                        && r.ns == item.ns
                        && r.name == item.name
                        && r.new != item.def
                })
            {
                olds.push(item.def);
            }
        }
        // A user of an earlier definition with the same signature reached
        // the replaced one through the slot they shared.
        loop {
            let before = olds.len();
            for r in &self.records {
                if r.kind == RedefinitionKind::SameSignature
                    && olds.contains(&r.new)
                    && !olds.contains(&r.old)
                {
                    olds.push(r.old);
                }
            }
            if olds.len() == before {
                break;
            }
        }
        // A user of a forwarder relied on its function for interface
        // satisfaction (D-609, D-610).
        let forwarded: Vec<DefId> = self
            .forwarders
            .iter()
            .filter(|f| olds.contains(&f.target))
            .map(|f| f.def)
            .collect();
        olds.extend(forwarded);
        let news: Vec<DefId> = changed.iter().map(|r| r.new).collect();
        self.items
            .iter()
            .filter(|item| {
                item.source.is_some()
                    && !news.contains(&item.def)
                    && self.current(item.ns, item.name) == Some(item.def)
                    && item.refs.iter().any(|r| olds.contains(r))
            })
            .collect()
    }

    /// The current session definitions that use an item of a module that
    /// `:reload` replaced: they are checked again (§13.5.3), in the order
    /// they entered the session.
    #[must_use]
    pub fn reload_dependents(&self, replaced: &[ModuleId]) -> Vec<&SessionItem> {
        self.items
            .iter()
            .filter(|item| {
                item.source.is_some()
                    && self.current(item.ns, item.name) == Some(item.def)
                    && item.refs.iter().any(|r| replaced.contains(&r.module))
            })
            .collect()
    }

    /// A type as diagnostics show it (D-463), with session types by their
    /// bare names, as written at the prompt.
    #[must_use]
    pub fn show_ty(&self, ty: Ty, cx: &DisplayCx<'_>) -> String {
        strip_session_paths(&cheby_iface::display(ty, cx))
    }
}

/// The character that marks the last segment of a module with stale types
/// in [`ReplSession::display_table`]. It never occurs in Cheby source
/// outside strings, and diagnostics never quote a control character.
const STALE_MARK: char = '\u{1f}';

/// `info`'s path with a last segment added that marks it as holding stale
/// types: `\u{1f}<id>\u{1f}<last segment>`, unique per module. Only the
/// last segment shows in types (D-463).
fn stale_path(info: &ModuleInfo, id: ModuleId) -> ModuleInfo {
    let last = info.path.segments().last().map_or("", String::as_str);
    ModuleInfo {
        path: info
            .path
            .child(&format!("{STALE_MARK}{}{STALE_MARK}{last}", id.as_u32())),
        package: info.package,
        file: info.file,
    }
}

/// `text` with every marked type of [`stale_path`] written with ` (stale)`
/// after it (its type arguments included), as in `Shape (stale)` or
/// `prices::Item<Int> (stale)` (§13.5.5, D-626), and the
/// marks removed elsewhere.
fn mark_stale(text: &str) -> String {
    const MARK: usize = STALE_MARK.len_utf8();
    // The byte ranges of the marks to drop, and the positions after which
    // ` (stale)` goes; found in one pass, so nested marks need no
    // recursion.
    let mut drops: Vec<(usize, usize)> = Vec::new();
    let mut stale_ends: Vec<usize> = Vec::new();
    let mut from = 0_usize;
    while let Some(found) = text.get(from..).and_then(|t| t.find(STALE_MARK)) {
        let start = from.saturating_add(found);
        let after = start.saturating_add(MARK);
        let Some(close) = text.get(after..).and_then(|t| t.find(STALE_MARK)) else {
            drops.push((start, after));
            break;
        };
        let body_start = after.saturating_add(close).saturating_add(MARK);
        drops.push((start, body_start));
        let body = text.get(body_start..).unwrap_or("");
        // The segment: `<repl:N>`, or an identifier.
        let segment_len = if body.starts_with("<repl:") {
            body.find('>')
                .map_or(body.len(), |end| end.saturating_add(1))
        } else {
            ident_len(body)
        };
        let mut at = body_start.saturating_add(segment_len);
        from = at;
        let Some(named) = text.get(at..).and_then(|t| t.strip_prefix("::")) else {
            continue;
        };
        let name_len = ident_len(named);
        if name_len == 0 {
            continue;
        }
        at = at.saturating_add(2).saturating_add(name_len);
        let tail = text.get(at..).unwrap_or("");
        if tail.starts_with('<') {
            let mut depth = 0_usize;
            let mut previous = ' ';
            for (i, c) in tail.char_indices() {
                match c {
                    '<' => depth = depth.saturating_add(1),
                    // `->` of a function type is not a bracket.
                    '>' if previous != '-' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            at = at.saturating_add(i).saturating_add(1);
                            break;
                        }
                    }
                    _ => {}
                }
                previous = c;
            }
        }
        stale_ends.push(at);
    }
    if drops.is_empty() {
        return text.to_owned();
    }
    stale_ends.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut ends = stale_ends.iter().peekable();
    let mut dropped = drops.iter().peekable();
    for (i, c) in text.char_indices() {
        while ends.next_if(|&&end| end <= i).is_some() {
            out.push_str(" (stale)");
        }
        while dropped.next_if(|&&(_, end)| end <= i).is_some() {}
        if dropped.peek().is_some_and(|&&(start, _)| start <= i) {
            continue;
        }
        out.push(c);
    }
    for _ in ends {
        out.push_str(" (stale)");
    }
    out
}

/// The length of the identifier at the start of `text`.
fn ident_len(text: &str) -> usize {
    text.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(text.len())
}

/// `<repl:N>::` removed from `text`: the qualifier of session types and
/// items, which the prompt writes without one. A type of a module that
/// [`ReplSession::display_table`] marked as stale gets ` (stale)` after
/// it.
#[must_use]
pub fn strip_session_paths(text: &str) -> String {
    const OPEN: &str = "<repl:";
    // A statements entry is checked as the body of a wrapper test; the
    // diagnostics call it what it is (D-629).
    let wrapper = format!("the test \"{BODY_TEST_NAME}\"");
    let marked = mark_stale(text).replace(&wrapper, "the entry");
    let text = marked.as_str();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(OPEN) {
        let (before, from) = rest.split_at(at);
        out.push_str(before);
        // An entry module named on its own, as in "`<repl:9>` has no
        // function": the entries count as one module, the session (D-610,
        // D-629).
        if let Some(close) = from.find('>')
            && out.ends_with('`')
            && from
                .get(close.saturating_add(1)..)
                .is_some_and(|s| s.starts_with('`'))
            && !from
                .get(..close)
                .is_some_and(|s| s.contains(char::is_whitespace))
        {
            out.pop();
            if out.ends_with("module ") {
                out.truncate(out.len().saturating_sub("module ".len()));
            }
            out.push_str("the session");
            rest = from.get(close.saturating_add(2)..).unwrap_or("");
            continue;
        }
        match from.find(">::") {
            Some(end)
                if !from
                    .get(..end)
                    .is_some_and(|s| s.contains(char::is_whitespace)) =>
            {
                rest = from.get(end.saturating_add(3)..).unwrap_or("");
            }
            _ => {
                out.push_str(OPEN);
                rest = from.get(OPEN.len()..).unwrap_or("");
            }
        }
    }
    out.push_str(rest);
    out
}

/// [`strip_session_paths`] over every text of a diagnostic.
fn strip_diagnostic(diag: &mut Diagnostic) {
    diag.message = strip_session_paths(&diag.message);
    for label in &mut diag.labels {
        label.message = strip_session_paths(&label.message);
    }
    for note in &mut diag.notes {
        *note = strip_session_paths(note);
    }
    for help in &mut diag.helps {
        help.message = strip_session_paths(&help.message);
    }
}

/// The top-level item of item `index`: itself, or the type or interface it
/// belongs to.
fn top_level(iface: &ModuleInterface, index: DefIndex) -> Result<&IfaceItem, Ice> {
    let mut at = index;
    loop {
        let item = iface
            .item(at)
            .ok_or_else(|| Ice::new("an entry item is missing from its interface"))?;
        match item.parent {
            Some(parent) => at = parent,
            None => return Ok(item),
        }
    }
}

/// The item's name as symbols use it: `f`, `T`, or `I::f` for an
/// interface function (as `cheby_mir::Defs::name` writes it).
fn item_text(
    input: &TypesInput<'_>,
    iface: &ModuleInterface,
    item: &IfaceItem,
) -> Result<String, Ice> {
    let names = &input.globals.names;
    if item.kind == DefKind::InterfaceFn {
        let parent = item
            .parent
            .and_then(|p| iface.item(p))
            .ok_or_else(|| Ice::new("an interface function without its interface"))?;
        return Ok(format!(
            "{}::{}",
            names.get(parent.name),
            names.get(item.name)
        ));
    }
    Ok(names.get(item.name).to_owned())
}

/// How `item` (new) replaces `old`.
fn redefinition_kind(
    input: &TypesInput<'_>,
    item: &IfaceItem,
    old: DefId,
) -> Result<RedefinitionKind, Ice> {
    let old_item = input
        .deps
        .item(old)
        .ok_or_else(|| Ice::new(format!("the session item {old:?} is not readable")))?;
    Ok(match item.kind {
        DefKind::Type | DefKind::Interface => RedefinitionKind::Type,
        DefKind::Alias => RedefinitionKind::Alias,
        // A constructor is neither a function nor a constant: replacing
        // one, most often by the constructor of a redefined type of the
        // same name, checks nothing again (D-607, D-609).
        _ if old_item.kind == DefKind::Ctor => RedefinitionKind::Constructor,
        _ if same_signature(&old_item.sig, &item.sig)
            && old_item.kind.namespace() == item.kind.namespace() =>
        {
            RedefinitionKind::SameSignature
        }
        _ => RedefinitionKind::NewSignature,
    })
}

/// Whether two signatures are the same for callers: function parameters
/// and results (not parameter names), or a constant's type. A constructor
/// is never the same as anything: its type changed with it.
fn same_signature(old: &ItemSig, new: &ItemSig) -> bool {
    match (old, new) {
        (ItemSig::Fn(a), ItemSig::Fn(b)) => same_fn(a, b),
        (ItemSig::Const(a), ItemSig::Const(b)) => a.ty == b.ty,
        _ => false,
    }
}

fn same_fn(a: &FnSig, b: &FnSig) -> bool {
    a.generics == b.generics
        && a.ret == b.ret
        && a.clamped == b.clamped
        && a.params.len() == b.params.len()
        && a.params.iter().zip(&b.params).all(|(x, y)| x.ty == y.ty)
}

/// Every item that item `index`'s signature or body names.
fn refs_of(resolved: &ResolvedModule, index: DefIndex) -> BTreeSet<DefId> {
    let mut out = BTreeSet::new();
    if let Some(body) = resolved.bodies.get(&index) {
        for res in body.paths.values().chain(body.names.values()) {
            if let Res::Def(def) = res {
                out.insert(*def);
            }
        }
    }
    // A signature's paths are not grouped by item: every one of the
    // entry's, which over-approximates for an entry with several items.
    for res in resolved.sig_paths.values() {
        if let Res::Def(def) = res {
            out.insert(*def);
        }
    }
    out
}

/// Every function that the body of item `def` of `thir` calls or names
/// through a callee or a dictionary: with [`refs_of`], the functions it
/// used to satisfy interfaces, forwarders included (D-609, D-610).
fn thir_refs(thir: &thir::Module, def: DefId) -> BTreeSet<DefId> {
    let mut out = BTreeSet::new();
    let body = thir
        .fns
        .iter()
        .find(|f| f.def == def)
        .and_then(|f| f.body.as_ref())
        .or_else(|| thir.consts.iter().find(|c| c.def == def).map(|c| &c.body));
    let Some(body) = body else {
        return out;
    };
    let mut dicts: Vec<&thir::Dict> = Vec::new();
    let callee = |callee: &thir::Callee, out: &mut BTreeSet<DefId>| {
        if let thir::Callee::Fn { def, .. } = callee {
            out.insert(*def);
        }
    };
    for expr in &body.exprs {
        match &expr.kind {
            ExprKind::FnRef { def, dicts: d, .. } => {
                out.insert(*def);
                dicts.extend(d.iter());
            }
            ExprKind::Call { callee: c, .. }
            | ExprKind::Binary {
                imp: thir::OpImpl::Call(c),
                ..
            }
            | ExprKind::Neg {
                imp: thir::OpImpl::Call(c),
                ..
            } => {
                callee(c, &mut out);
                collect_callee_dicts(c, &mut dicts);
            }
            ExprKind::Interpolate { parts } => {
                for part in parts {
                    if let thir::InterpPart::Value { show: Some(c), .. } = part {
                        callee(c, &mut out);
                        collect_callee_dicts(c, &mut dicts);
                    }
                }
            }
            ExprKind::DynCoerce { dict, .. } => dicts.push(dict),
            _ => {}
        }
    }
    // Dictionaries nest: a worklist, never recursion over user data.
    while let Some(dict) = dicts.pop() {
        match dict {
            thir::Dict::Impl { fns, .. } => {
                for f in fns {
                    out.insert(f.def);
                    dicts.extend(f.dicts.iter());
                }
            }
            thir::Dict::Embedded { of, .. } => dicts.push(of),
            thir::Dict::Tuple { elems, .. } => dicts.extend(elems.iter()),
            thir::Dict::Param { .. } | thir::Dict::Dyn { .. } => {}
        }
    }
    out
}

/// The dictionaries a callee holds.
fn collect_callee_dicts<'a>(callee: &'a thir::Callee, dicts: &mut Vec<&'a thir::Dict>) {
    match callee {
        thir::Callee::Fn { dicts: d, .. } => dicts.extend(d.iter()),
        thir::Callee::Iface { dict, .. } => dicts.push(dict),
        _ => {}
    }
}

/// Whether `ty` names the nominal type `def` anywhere.
fn mentions_def(types: &cheby_iface::TyInterner, ty: Ty, def: DefId) -> bool {
    let mut stack = vec![ty];
    while let Some(ty) = stack.pop() {
        match types.kind(ty) {
            TyKind::Named { def: d, args } => {
                if *d == def {
                    return true;
                }
                stack.extend(args.iter().copied());
            }
            TyKind::Dyn { args, .. } => stack.extend(args.iter().copied()),
            TyKind::Tuple(elems) => stack.extend(elems.iter().copied()),
            TyKind::Fn { params, ret } => {
                stack.extend(params.iter().copied());
                stack.push(*ret);
            }
            TyKind::Param(_) | TyKind::Error | TyKind::Never => {}
        }
    }
    false
}

/// The body of forwarder `def` (D-610): it takes the parameters of `sig`,
/// the signature of the session function `target`, and calls `target` with
/// them, passing its own type parameters and dictionaries on.
///
/// # Errors
///
/// An [`Ice`] if a body grows past its id space.
pub fn forwarder_fn(
    def: DefId,
    target: DefId,
    sig: &FnSig,
    span: Span,
    globals: &cheby_iface::Globals,
) -> Result<thir::FnDef, Ice> {
    let types = &globals.types;
    let mut locals = cheby_iface::IndexVec::new();
    let mut exprs = cheby_iface::IndexVec::new();
    let mut params = Vec::with_capacity(sig.params.len());
    let mut args = Vec::with_capacity(sig.params.len());
    for param in &sig.params {
        let local = locals.push(thir::Local {
            name: param.name,
            ty: param.ty,
            span,
            synthetic: true,
        })?;
        params.push(local);
        args.push(exprs.push(thir::Expr {
            kind: ExprKind::Local(local),
            ty: param.ty,
            span,
        })?);
    }
    let mut ty_args = Vec::with_capacity(sig.generics.len());
    let mut dicts = Vec::new();
    for (param, generic) in sig.generics.params.iter_enumerated() {
        ty_args.push(types.intern(TyKind::Param(param)));
        for bound in 0..generic.bounds.len() {
            dicts.push(thir::Dict::Param {
                param,
                bound: u32::try_from(bound).map_err(|_| Ice::new("too many bounds"))?,
            });
        }
    }
    let call = exprs.push(thir::Expr {
        kind: ExprKind::Call {
            callee: thir::Callee::Fn {
                def: target,
                ty_args: ty_args.into(),
                dicts: dicts.into(),
            },
            args: args.into(),
        },
        ty: sig.ret,
        span,
    })?;
    let block = exprs.push(thir::Expr {
        kind: ExprKind::Block {
            stmts: Box::new([]),
            tail: Some(call),
        },
        ty: sig.ret,
        span,
    })?;
    Ok(thir::FnDef {
        def,
        span,
        native_external: None,
        body: Some(thir::Body {
            params,
            locals,
            exprs,
            value: block,
            ret: sig.ret,
        }),
        entry: None,
    })
}

/// Where each top-level declaration of a declarations entry is written.
fn declaration_sources(input: &ResolveInput<'_>, resolved: &ResolvedModule) -> Vec<(DefId, Span)> {
    resolved
        .items
        .iter_enumerated()
        .filter(|(_, item)| item.parent.is_none())
        .filter_map(|(index, item)| {
            let node = input.tree.node(item.node)?;
            Some((resolved.def_id(index), node.span()))
        })
        .collect()
}

/// What [`make_body`] made, with what its warnings must not report.
struct MadeBody {
    body: ReplBody,
    /// The spans of the bindings (W0002) and of the shown expression
    /// (W0006).
    bindings: Vec<Span>,
    shown: Option<Span>,
}

impl MadeBody {
    /// Whether `diag` is a warning the REPL's reading of the entry makes
    /// wrong: an unused session value, or a shown `Result`.
    fn quiet(&self, diag: &Diagnostic) -> bool {
        let Some(span) = diag.primary_span() else {
            return false;
        };
        (diag.code == codes::UNUSED_BINDING && self.bindings.contains(&span))
            || (diag.code == codes::DISCARDED_RESULT && self.shown == Some(span))
    }
}

/// Turns the checked wrapper test of a statements entry into its body:
/// the block's final expression statement becomes the first element of
/// the tuple it returns, followed by the names its top-level `let`s bind,
/// which are appended to the interface as session values.
fn make_body(
    input: &TypesInput<'_>,
    iface: &mut ModuleInterface,
    thir: &mut thir::Module,
) -> Result<MadeBody, Ice> {
    let test = thir
        .tests
        .first_mut()
        .ok_or_else(|| Ice::new("a REPL entry without its body"))?;
    let def = test.def;
    let body = &mut test.body;
    let block = body.value;
    let (stmts, tail, span) = match body.expr(block) {
        Some(thir::Expr {
            kind: ExprKind::Block { stmts, tail },
            span,
            ..
        }) => (stmts.to_vec(), *tail, *span),
        _ => return Err(Ice::new("a REPL entry's body is not a block")),
    };
    let nil = tail.ok_or_else(|| Ice::new("a REPL entry's body without its `Nil`"))?;
    let mut stmts = stmts;
    let mut shown = None;
    let last_ty = match stmts.last() {
        Some(&Stmt::Expr(last)) => Some(expr_ty(body, last)?),
        _ => None,
    };
    if let Some(&Stmt::Expr(last)) = stmts.last()
        && expr_ty(body, last)? != Ty::NEVER
    {
        shown = Some(last);
        stmts.pop();
    }
    let shown_id = shown.unwrap_or(nil);
    let shown_ty = expr_ty(body, shown_id)?;
    let mut elems: Vec<ExprId> = vec![shown_id];
    let bindings = bind_values(input, iface, body, &stmts, &mut elems)?;
    if elems.len() < 2 {
        elems.push(nil);
    }
    let tys = elems
        .iter()
        .map(|&e| expr_ty(body, e))
        .collect::<Result<Box<[Ty]>, Ice>>()?;
    let tuple_ty = input.globals.types.intern(TyKind::Tuple(tys));
    let tuple = body.exprs.push(thir::Expr {
        kind: ExprKind::Tuple(elems.into()),
        ty: tuple_ty,
        span,
    })?;
    let block_expr = body
        .exprs
        .get_mut(block)
        .ok_or_else(|| Ice::new("a REPL entry's body is not a block"))?;
    block_expr.kind = ExprKind::Block {
        stmts: stmts.into(),
        tail: Some(tuple),
    };
    block_expr.ty = tuple_ty;
    body.ret = tuple_ty;
    let shown_span = match shown {
        Some(id) => Some(
            body.expr(id)
                .ok_or_else(|| Ice::new("a missing expression"))?
                .span,
        ),
        None => None,
    };
    iface.set_sig(
        def.index,
        ItemSig::Fn(FnSig {
            generics: Generics::empty(),
            params: Box::new([]),
            ret: tuple_ty,
            clamped: Box::new([]),
        }),
    )?;
    Ok(MadeBody {
        bindings: bindings.iter().map(|b| b.span).collect(),
        shown: shown_span,
        body: ReplBody {
            def,
            shown: shown_ty,
            bindings,
            last: last_ty,
        },
    })
}

/// Appends a read of every name the top-level `let` and `let assert`
/// statements of `stmts` bind to `elems`, in order, and makes each a
/// session value: a constant item of the entry's interface.
fn bind_values(
    input: &TypesInput<'_>,
    iface: &mut ModuleInterface,
    body: &mut thir::Body,
    stmts: &[Stmt],
    elems: &mut Vec<ExprId>,
) -> Result<Vec<ReplBinding>, Ice> {
    let mut locals = Vec::new();
    for stmt in stmts {
        if let Stmt::Let { pat, .. } | Stmt::LetAssert { pat, .. } = stmt {
            pat.walk(&mut |p| {
                if let thir::PatKind::Bind(l) | thir::PatKind::As { local: l, .. } = &p.kind
                    && !locals.contains(l)
                {
                    locals.push(*l);
                }
            });
        }
    }
    let mut bindings = Vec::new();
    for local in locals {
        let decl = body
            .local(local)
            .ok_or_else(|| Ice::new("a REPL binding without its local"))?
            .clone();
        let read = body.exprs.push(thir::Expr {
            kind: ExprKind::Local(local),
            ty: decl.ty,
            span: decl.span,
        })?;
        elems.push(read);
        let index = iface.push(IfaceItem {
            name: decl.name,
            kind: DefKind::Const,
            vis: Vis::Package,
            parent: None,
            span: Some(decl.span),
            targets: input.resolved.targets,
            deprecated: None,
            sig: ItemSig::Const(ConstSig { ty: decl.ty }),
        })?;
        bindings.push(ReplBinding {
            name: decl.name,
            def: DefId::new(iface.module, index),
            ty: decl.ty,
            span: decl.span,
        });
    }
    Ok(bindings)
}

fn expr_ty(body: &thir::Body, id: ExprId) -> Result<Ty, Ice> {
    body.expr(id)
        .map(|e| e.ty)
        .ok_or_else(|| Ice::new("a missing expression"))
}

/// Moves spans of the wrapped file to the entry's own text.
#[derive(Clone, Copy, Debug)]
struct Move {
    from: FileId,
    to: FileId,
    shift: u32,
}

impl Move {
    fn span(self, span: Span) -> Span {
        if span.file != self.from || span.range.start() < self.shift {
            return span;
        }
        let start = span.range.start().saturating_sub(self.shift);
        let end = span.range.end().saturating_sub(self.shift);
        Span::new(self.to, TextRange::new(start, end))
    }

    fn diagnostics(self, diags: &mut [Diagnostic]) {
        for diag in diags {
            for label in &mut diag.labels {
                label.span = self.span(label.span);
            }
            for help in &mut diag.helps {
                for edit in &mut help.edits {
                    edit.span = self.span(edit.span);
                }
            }
        }
    }

    fn thir(self, module: &mut thir::Module) {
        for test in &mut module.tests {
            test.span = self.span(test.span);
            self.body(&mut test.body);
        }
        for f in &mut module.fns {
            f.span = self.span(f.span);
            if let Some(body) = &mut f.body {
                self.body(body);
            }
        }
        for c in &mut module.consts {
            c.span = self.span(c.span);
            self.body(&mut c.body);
        }
    }

    fn body(self, body: &mut thir::Body) {
        for local in &mut body.locals {
            local.span = self.span(local.span);
        }
        body.for_each_pat_mut(&mut |p| p.span = self.span(p.span));
        for expr in &mut body.exprs {
            expr.span = self.span(expr.span);
            match &mut expr.kind {
                ExprKind::Case { arms, .. } => {
                    for arm in arms.iter_mut() {
                        arm.span = self.span(arm.span);
                    }
                }
                ExprKind::Closure { params, .. } | ExprKind::LocalFn { params, .. } => {
                    for param in params.iter_mut() {
                        param.span = self.span(param.span);
                    }
                }
                _ => {}
            }
        }
    }

    fn iface(self, iface: &mut ModuleInterface) -> Result<(), Ice> {
        let spans: Vec<(DefIndex, Option<Span>)> = iface
            .items
            .iter_enumerated()
            .map(|(index, item)| (index, item.span.map(|s| self.span(s))))
            .collect();
        for (index, span) in spans {
            iface.set_span(index, span)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cheby_diag::{FileId, Span, TextRange};
    use cheby_iface::{DefId, DepView, ModuleId, ModuleInfo, Namespace, PackageId, Ty, TyKind};
    use cheby_package::{ModulePath, Targets};
    use cheby_resolve::repl::SessionScope;
    use cheby_resolve::{
        Lookup, ModuleLookup, ResolveInput, StdCatalog, collect_imports, resolve_import_targets,
    };
    use cheby_syntax::ast::{AstNode, SourceFile};

    use super::*;
    use crate::test_support::{Build, build};

    /// A package build with a REPL session on top, as the driver runs one
    /// but without code: each entry is parsed, registered as a module,
    /// checked, and committed when it has no error.
    struct Session {
        b: Build,
        repl: ReplSession,
        entries: u32,
        /// The label texts of the last failed entry's diagnostics.
        labels: Vec<String>,
    }

    struct Lookups<'a> {
        modules: &'a cheby_iface::ModuleTable,
    }

    impl ModuleLookup for Lookups<'_> {
        fn lookup(&mut self, _: PackageId, path: &[&str]) -> Result<Lookup, Ice> {
            let (first, rest) = path.split_first().unwrap();
            let path = rest.iter().fold(ModulePath::root(first), |p, s| p.child(s));
            Ok(match self.modules.find(&path) {
                Some(id) => Lookup::Found(id),
                None => Lookup::NoPackage,
            })
        }
    }

    const PREFIX: &str = "test \"<repl>\" {\n";

    impl Session {
        fn new(package: &[(&str, &str)]) -> Self {
            let mut b = build(package);
            assert!(b.codes().is_empty(), "{}", b.rendered());
            let synthetic = b.sources.add_file("<repl>".to_owned(), "").unwrap();
            let (tree, _) =
                cheby_syntax::parse(synthetic, Arc::from("import std::io\n")).into_parts();
            let ast = SourceFile::cast(tree.root())
                .unwrap()
                .imports()
                .next()
                .unwrap()
                .id();
            let scope = SessionScope::new(Span::new(synthetic, TextRange::empty(0)), ast);
            Self {
                b,
                repl: ReplSession::new(scope),
                entries: 0,
                labels: Vec::new(),
            }
        }

        /// The satisfaction plan after `entry` was committed (D-610).
        fn plan(&self, entry: &CheckedEntry) -> SatisfactionPlan {
            let set = &self.b.set;
            let read = |def: DefId| set.get(def.module).and_then(|i| i.item(def.index));
            self.repl
                .satisfaction(entry, &read, &self.b.globals)
                .unwrap()
        }

        /// Records `plan` with made-up forwarder items.
        fn apply(&mut self, plan: &SatisfactionPlan) {
            let added: Vec<DefId> = plan
                .add
                .iter()
                .enumerate()
                .map(|(n, f)| {
                    DefId::new(
                        f.ty.module,
                        DefIndex::from_u32(u32::try_from(n).unwrap().checked_add(100).unwrap()),
                    )
                })
                .collect();
            self.repl.apply_satisfaction(plan, &added);
        }

        /// Checks `text` as the next entry; commits it when it checks.
        /// The entry, or its diagnostics as `CODE message at path:line:col`.
        fn entry(&mut self, text: &str) -> (Option<CheckedEntry>, Vec<String>) {
            self.entries = self.entries.checked_add(1).unwrap();
            let n = self.entries;
            let name = format!("<repl:{n}>");
            let decls = [
                "fn ",
                "pub ",
                "priv ",
                "type ",
                "interface ",
                "const ",
                "import ",
            ]
            .iter()
            .any(|k| text.starts_with(k));
            let display = self.b.sources.add_file(name.clone(), text).unwrap();
            let (file, parsed, shape) = if decls {
                (display, text.to_owned(), EntryShape::Decls)
            } else {
                let wrapped = format!("{PREFIX}{text}\nNil\n}}\n");
                let file = self
                    .b
                    .sources
                    .add_file(name.clone(), wrapped.as_str())
                    .unwrap();
                let shift = u32::try_from(PREFIX.len()).unwrap();
                (file, wrapped, EntryShape::Body { display, shift })
            };
            let (tree, errors) = cheby_syntax::parse(file, Arc::from(parsed.as_str())).into_parts();
            assert!(errors.is_empty(), "{errors:?}");
            let module = self
                .b
                .modules
                .add(ModuleInfo {
                    path: ModulePath::root(&name),
                    package: PackageId::LOCAL,
                    file: Some(display),
                })
                .unwrap();
            let imports = collect_imports(&tree, &self.b.globals.names);
            let mut lookup = Lookups {
                modules: &self.b.modules,
            };
            let (targets, _) = resolve_import_targets(
                &imports,
                PackageId::LOCAL,
                &mut lookup,
                &self.b.globals.names,
            )
            .unwrap();
            let mut deps: Vec<ModuleId> = self.b.thir.keys().copied().collect();
            deps.extend(self.repl.modules());
            deps.sort_unstable();
            let display = self.repl.display_table(&self.b.modules).unwrap();
            let input = ResolveInput {
                module,
                package: PackageId::LOCAL,
                tree: &tree,
                imports: &imports,
                targets: &targets,
                deps: DepView::new(&self.b.set, &deps),
                modules: &display,
                globals: &self.b.globals,
                std: StdCatalog { known: &[] },
                package_targets: Targets::ALL,
            };
            match self.repl.check_entry(&input, shape).unwrap() {
                Ok(entry) => {
                    self.repl.note_compiled(&entry);
                    self.repl.commit(&entry, &self.b.globals.names);
                    self.b.set.insert(Arc::new(entry.iface.clone())).unwrap();
                    (Some(entry), Vec::new())
                }
                Err(diags) => (
                    None,
                    {
                        self.labels = diags
                            .iter()
                            .flat_map(|d| d.labels.iter().map(|l| l.message.clone()))
                            .collect();
                        diags
                    }
                    .iter()
                    .map(|d| {
                        let at = d
                            .primary_span()
                            .and_then(|s| self.b.sources.location(s))
                            .map(|l| format!("{}:{}:{}", l.path, l.line, l.col))
                            .unwrap_or_default();
                        format!("{} {} at {at}", d.code, d.message)
                    })
                    .collect(),
                ),
            }
        }

        fn fails(&mut self, text: &str) -> Vec<String> {
            let (entry, errors) = self.entry(text);
            assert!(entry.is_none(), "entry `{text}` checked");
            errors
        }

        fn ok(&mut self, text: &str) -> CheckedEntry {
            let (entry, errors) = self.entry(text);
            entry.unwrap_or_else(|| panic!("entry `{text}` failed: {errors:?}"))
        }

        fn show(&mut self, text: &str) -> String {
            let entry = self.ok(text);
            self.shown(&entry)
        }

        fn name(&self, text: &str) -> Name {
            self.b.globals.names.intern(text)
        }

        fn value(&self, text: &str) -> DefId {
            self.repl
                .current(Namespace::Value, self.name(text))
                .unwrap()
        }

        fn shown(&self, entry: &CheckedEntry) -> String {
            let deps: Vec<ModuleId> = Vec::new();
            let view = DepView::new(&self.b.set, &deps);
            let display = self.repl.display_table(&self.b.modules).unwrap();
            let cx = DisplayCx {
                globals: &self.b.globals,
                modules: &display,
                own: Some(&entry.iface),
                deps: &view,
                generics: None,
            };
            let ty = entry.body.as_ref().unwrap().shown;
            // Named types of earlier entries need their interfaces.
            let all: Vec<ModuleId> = (0..u32::try_from(self.b.modules.len()).unwrap())
                .map(ModuleId::from_u32)
                .collect();
            let view = DepView::new(&self.b.set, &all);
            let cx = DisplayCx { deps: &view, ..cx };
            self.repl.show_ty(ty, &cx)
        }
    }

    #[test]
    fn entries_see_the_package_and_earlier_entries() {
        let mut s = Session::new(&[("", "pub fn double(x: Int) -> Int {\n  x * 2\n}\n")]);
        s.ok("import app");
        let shown = s.ok("app::double(2)");
        assert_eq!(s.shown(&shown), "Int");
        let quad = s.ok("fn quad(x: Int) -> Int {\n  app::double(app::double(x))\n}\n");
        assert!(quad.body.is_none());
        let f = s.value("quad");
        assert_eq!(f.module, quad.module);
        let call = s.ok("quad(1) + 1");
        assert_eq!(s.shown(&call), "Int");
        // The entry's body returns `(shown, Nil)` and its signature says so.
        let body = call.body.as_ref().unwrap();
        let ItemSig::Fn(sig) = &call.iface.item(body.def.index).unwrap().sig else {
            panic!("the body has no function signature");
        };
        assert!(matches!(
            s.b.globals.types.kind(sig.ret),
            TyKind::Tuple(elems) if elems[..] == [Ty::INT, Ty::NIL]
        ));
        // An unknown name is an error at the entry's own position.
        let errors = s.fails("1 + nope");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].starts_with("E0042"), "{errors:?}");
        assert!(errors[0].ends_with("at <repl:5>:1:5"), "{errors:?}");
        // A failed entry changes nothing.
        assert_eq!(s.repl.modules().len(), 4);
    }

    #[test]
    fn redefinitions_are_recorded_with_their_kind() {
        let mut s = Session::new(&[("", "")]);
        s.ok("fn f(x: Int) -> Int {\n  x\n}\n");
        let first = s.value("f");
        s.ok("fn g() -> Int {\n  f(1)\n}\n");
        let g = s.value("g");
        let same = s.ok("fn f(y: Int) -> Int {\n  y + 1\n}\n");
        let r = same.redefinitions().next().unwrap();
        assert_eq!(r.kind, RedefinitionKind::SameSignature);
        assert_eq!(r.old, first);
        let name = same.names.iter().find(|n| n.def == r.new).unwrap();
        assert_eq!((name.generation, name.reuse), (1, Some(first)));
        let other = s.ok("fn f(x: String) -> String {\n  x\n}\n");
        let r = other.redefinitions().next().unwrap();
        assert_eq!(r.kind, RedefinitionKind::NewSignature);
        let name = other.names.iter().find(|n| n.def == r.new).unwrap();
        assert_eq!((name.generation, name.reuse), (2, None));
        // `g` used the `f` that changed its signature.
        let dependents: Vec<DefId> = s.repl.dependents(&[r]).iter().map(|i| i.def).collect();
        assert_eq!(dependents, vec![g]);
        assert_eq!(s.repl.records().len(), 2);
        // Redefining `g` itself with no use of `f` leaves nothing to check.
        s.ok("fn g() -> Int {\n  2\n}\n");
        assert_eq!(s.repl.dependents(&[r]).len(), 0);
    }

    #[test]
    fn redefined_types_are_stale() {
        let mut s = Session::new(&[("", "")]);
        s.ok("type Shape {\n  Circle(Float),\n}\n");
        let old = s.repl.current(Namespace::Type, s.name("Shape")).unwrap();
        let value = s.ok("let c = Circle(1.0)");
        let c = value.body.as_ref().unwrap().bindings[0];
        assert_eq!(s.show("c"), "Shape");
        let entry = s.ok("type Shape {\n  Square(Float),\n}\n");
        let r = entry.redefinitions().next().unwrap();
        assert_eq!((r.kind, r.old), (RedefinitionKind::Type, old));
        assert!(s.repl.is_stale(old));
        assert!(!s.repl.is_stale(r.new));
        assert_eq!(s.repl.stale().collect::<Vec<_>>(), vec![old]);
        // The old value keeps the old type.
        assert!(matches!(
            s.b.globals.types.kind(c.ty),
            TyKind::Named { def, .. } if *def == old
        ));
        // The old constructor is gone from the session, the new one is in.
        s.fails("Circle(2.0)");
        assert_eq!(s.show("Square(2.0)"), "Shape");
        // Mixing the old and the new type is a type error.
        s.fails("[c, Square(1.0)]");
    }

    #[test]
    fn a_redefined_type_checks_none_of_its_constructors_users_again() {
        // Review SEM5-1, R5-ROB-2: the new type's constructor of the same
        // name replaces the old constructor for later entries only
        // (D-607, D-609).
        let mut s = Session::new(&[("", "")]);
        s.ok("type Cat {\n  Cat(Int),\n}\n");
        s.ok("fn f(c: Cat) -> Int {\n  let Cat(n) = c\n  n\n}\n");
        let f = s.repl.current(Namespace::Value, s.name("f")).unwrap();
        for text in [
            "type Cat {\n  Cat(Int),\n}\n",
            "type Cat {\n  Cat(String),\n}\n",
            "type Cat {\n  Cat(Int),\n  Kitten,\n}\n",
        ] {
            let old_ctor = s.repl.current(Namespace::Value, s.name("Cat")).unwrap();
            let entry = s.ok(text);
            let changed: Vec<Redefinition> = entry.redefinitions().collect();
            assert!(
                changed
                    .iter()
                    .any(|r| r.old == old_ctor && r.kind == RedefinitionKind::Constructor),
                "{changed:?}"
            );
            assert!(
                changed
                    .iter()
                    .all(|r| r.kind != RedefinitionKind::NewSignature),
                "{changed:?}"
            );
            assert_eq!(s.repl.dependents(&changed).len(), 0);
            assert_eq!(s.repl.current(Namespace::Value, s.name("f")), Some(f));
        }
    }

    #[test]
    fn prompt_lets_are_session_values_and_may_be_rebound() {
        let mut s = Session::new(&[("", "")]);
        let first = s.ok("let x = 1");
        let body = first.body.as_ref().unwrap();
        assert_eq!(body.shown, Ty::NIL);
        assert_eq!(body.bindings.len(), 1);
        assert_eq!(body.bindings[0].ty, Ty::INT);
        let x1 = s.value("x");
        assert_eq!(x1, body.bindings[0].def);
        // Rebinding reads the old value; no E0044 across entries (D-193).
        // It shadows: a value of its own, never the old slot (D-607,
        // D-608).
        let second = s.ok("let x = x + 1");
        let x2 = s.value("x");
        assert_ne!(x1, x2);
        let r = second.redefinitions().next().unwrap();
        assert_eq!((r.kind, r.old), (RedefinitionKind::Rebinding, x1));
        let name = second.names.iter().find(|n| n.def == x2).unwrap();
        assert_eq!((name.generation, name.reuse), (1, None));
        // A reader checked before keeps the binding it was checked
        // against, and nothing is checked again, whatever the new type.
        s.ok("fn read_x() -> Int {\n  x\n}\n");
        let third = s.ok("let x = \"text\"");
        let r = third.redefinitions().next().unwrap();
        assert_eq!(r.kind, RedefinitionKind::Rebinding);
        assert_eq!(s.repl.dependents(&[r]).len(), 0);
        assert_eq!(s.show("x"), "String");
        // A function that replaces a session value does not take its
        // slot either.
        let f = s.ok("fn x() -> Int {\n  1\n}\n");
        assert_eq!(
            f.redefinitions().next().unwrap().kind,
            RedefinitionKind::Rebinding
        );
        // Several names, `let assert`, and no shadowing inside one entry.
        let pair = s.ok("let (a, b) = (1, \"b\")");
        assert_eq!(pair.body.as_ref().unwrap().bindings.len(), 2);
        s.ok("let assert Some(c) = Some(a)");
        assert_eq!(s.show("c"), "Int");
        let errors = s.fails("let z = 1\nlet z = 2");
        assert!(errors[0].starts_with("E0044"), "{errors:?}");
        assert!(errors[0].ends_with("at <repl:10>:2:5"), "{errors:?}");
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    /// Stale types print with ` (stale)` after them, their type arguments
    /// included, in values and in diagnostics (§13.5.5).
    #[test]
    fn stale_types_are_marked() {
        let mut s = Session::new(&[("", "")]);
        s.ok("type Shape {\n  Circle(Float),\n}\n");
        s.ok("let c = Circle(1.0)");
        s.ok("type Box<T> {\n  Box(T),\n}\n");
        s.ok("let b = Box(c)");
        s.ok("type Shape {\n  Square(Float),\n}\n");
        s.ok("type Box<T> {\n  Box(T),\n}\n");
        let table = s.repl.display_table(&s.b.modules).unwrap();
        assert_eq!(table.len(), s.b.modules.len());
        assert_eq!(s.show("c"), "Shape (stale)");
        assert_eq!(s.show("b"), "Box<Shape (stale)> (stale)");
        assert_eq!(s.show("Square(1.0)"), "Shape");
        // Mixing the old and the new type is an error that says which is
        // which.
        s.fails("[c, Square(1.0)]");
        assert!(
            s.labels.iter().any(|l| l.contains("`Shape (stale)`")),
            "{:?}",
            s.labels
        );
        assert_eq!(
            strip_session_paths("found `\u{1f}7\u{1f}<repl:3>::Box<Int>`, x"),
            "found `Box<Int> (stale)`, x"
        );
        assert_eq!(
            strip_session_paths("`\u{1f}12\u{1f}prices::Item`"),
            "`prices::Item (stale)`"
        );
        assert_eq!(
            strip_session_paths("module `\u{1f}12\u{1f}prices`"),
            "module `prices`"
        );
    }

    /// A session type satisfies interfaces through the session's current
    /// functions (D-610): forwarders appear for the functions whose
    /// parameters name it, and retire when the name means another function.
    #[test]
    fn session_types_satisfy_through_forwarders() {
        let mut s = Session::new(&[("", "")]);
        s.ok("type Cat {\n  Cat(Int),\n}\n");
        s.ok("type Dog {\n  Dog(Int),\n}\n");
        let cat = s.repl.current(Namespace::Type, s.name("Cat")).unwrap();
        let dog = s.repl.current(Namespace::Type, s.name("Dog")).unwrap();
        let show = s.ok("fn show(c: Cat) -> String {\n  \"cat\"\n}\n");
        let plan = s.plan(&show);
        assert_eq!(plan.add.len(), 1);
        assert_eq!(plan.add[0].ty, cat);
        assert_eq!(&*plan.add[0].text, "Cat::show");
        assert_eq!(plan.retire.len(), 0);
        s.apply(&plan);
        // The same signature keeps the forwarder: it calls through the slot.
        let same = s.ok("fn show(c: Cat) -> String {\n  \"a cat\"\n}\n");
        let plan = s.plan(&same);
        assert!(plan.add.is_empty() && plan.retire.is_empty(), "{plan:?}");
        // Another type: the name now means `Dog`'s function.
        let other = s.ok("fn show(d: Dog) -> String {\n  \"dog\"\n}\n");
        let plan = s.plan(&other);
        assert_eq!(plan.add.len(), 1);
        assert_eq!(plan.add[0].ty, dog);
        assert_eq!(plan.retire.len(), 1);
        s.apply(&plan);
        assert_eq!(
            s.repl
                .forwarders()
                .iter()
                .filter(|f| f.active)
                .map(|f| f.ty)
                .collect::<Vec<_>>(),
            vec![dog]
        );
        // A `priv` function never becomes a session name, so it changes
        // nothing.
        let private = s.ok("priv fn show(c: Cat) -> String {\n  \"c\"\n}\n");
        assert_eq!(s.plan(&private), SatisfactionPlan::default());
        // A forwarder's body calls its function with its parameters.
        let sig = FnSig {
            generics: Generics::empty(),
            params: Box::new([cheby_iface::ParamSig {
                name: s.name("c"),
                ty: s.b.globals.types.named(cat, []),
            }]),
            ret: Ty::STRING,
            clamped: Box::new([]),
        };
        let span = Span::new(FileId::new(1), TextRange::empty(0));
        let target = show.defines[0].def;
        let f = forwarder_fn(cat, target, &sig, span, &s.b.globals).unwrap();
        let body = f.body.unwrap();
        assert_eq!(body.params.len(), 1);
        assert!(body.exprs.iter().any(|e| matches!(
            &e.kind,
            ExprKind::Call { callee: thir::Callee::Fn { def, .. }, args }
                if *def == target && args.len() == 1
        )));
    }

    #[test]
    fn session_paths_are_stripped() {
        assert_eq!(strip_session_paths("<repl:3>::Point"), "Point");
        assert_eq!(
            strip_session_paths("List<<repl:12>::T> and <repl:1:r1>::U"),
            "List<T> and U"
        );
        assert_eq!(strip_session_paths("at <repl:3>:1:2"), "at <repl:3>:1:2");
        assert_eq!(strip_session_paths("<repl:"), "<repl:");
        assert_eq!(
            strip_session_paths("and `<repl:9>` has no function `show`"),
            "and the session has no function `show`"
        );
        assert_eq!(
            strip_session_paths("`x` is private to module `<repl:4:2>`"),
            "`x` is private to the session"
        );
        assert_eq!(
            strip_session_paths("but the test \"<repl>\" runs on `native`"),
            "but the entry runs on `native`"
        );
    }

    #[test]
    fn spans_move_to_the_entry_text() {
        let mv = Move {
            from: FileId::new(1),
            to: FileId::new(2),
            shift: 10,
        };
        let at = |file, start, end| Span::new(FileId::new(file), TextRange::new(start, end));
        assert_eq!(mv.span(at(1, 12, 15)), at(2, 2, 5));
        assert_eq!(mv.span(at(1, 3, 4)), at(1, 3, 4));
        assert_eq!(mv.span(at(3, 12, 15)), at(3, 12, 15));
    }
}
