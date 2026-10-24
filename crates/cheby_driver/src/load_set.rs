//! Reach over reference summaries, and the load set (D-967, D-922,
//! D-966).
//!
//! A build no longer walks THIR to find what it needs: it walks each
//! module's reference summary, what each function, constant, type's hooks
//! and test mentions, by module path and name ([`ReferenceSummary`], the
//! front record's type, K3b). The walk gives the same [`Reach`] as
//! [`cheby_mir::reach`]'s walk over THIR, which stays for the dumps for
//! goldens (D-510's view) and the REPL: the items the roots reach, which
//! decide E0053 (D-943), the constants the command evaluates in their
//! order (D-535) and each constant's module set (D-538's seeds).
//!
//! The load set is what a command loads and compiles in one batch, in
//! build order, std included (D-922): every module that holds a reached
//! item or a constructor reached code mentions, compiled whole (D-966),
//! and, since a whole module's unreached items are compiled too, every
//! module that any item of a loaded module mentions, and so on until
//! nothing is added (GAP(load-set-closure): their code names those
//! modules' slots and layouts). A module that only lends constructors is loaded and
//! compiled whole like any other (GAP(load-set-constructor-modules)).
//! A module's tests unit is whole too (D-970), so the tests of a module
//! with a selected test count as its items.
//!
//! GAP(reach-summaries-source): until `cheby run`, `cheby test` and `cheby
//! build` skip parsing unchanged modules, every module they load is
//! checked in the command, and its summary is made here from its THIR, in
//! the front record's form with two differences that keep the walk equal
//! to THIR's: each list holds what reach's walk meets, in the order it
//! first meets it ([`cheby_mir::reach::Mentions`]), since the item a
//! module's hooks are first reached from decides an edge that constants'
//! module sets read; and a function with a native external, whose body is
//! for other targets (D-510, D-207), and a function's entry dictionary,
//! which reach reads only for a root, mention nothing. The front record's
//! summaries (sorted, every body and entry) are read the same way once a
//! command walks them without parsing.

use std::collections::{BTreeMap, BTreeSet};

use cheby_iface::{DefId, DefIndex, DefKind, Ice, ItemSig, ModuleId};
use cheby_mir::reach::{Mention, Reach, body_mentions, entry_mentions};
use cheby_types::thir;
use rustc_hash::FxHashMap;

use crate::cache::front::{ItemRef, MentionerKind, ReferenceSummary};
use crate::frontend::Frontend;

/// Where a walk starts.
#[derive(Clone, Copy)]
pub(crate) enum Start<'a> {
    /// A `main` to run, whose entry's `Show` dictionary is reached too
    /// (D-182).
    Main(&'a thir::FnDef),
    /// A selected test of its module.
    Test(&'a thir::TestDef),
    /// A constant to evaluate (`cheby check`, D-534).
    Const(DefId),
    /// A function compiled for its own sake (the code-generation
    /// benchmark).
    Fn(DefId),
}

/// One module's summary, with the lookups the walk needs.
struct ModuleSummary {
    /// Function, constant, hook and test summaries in item order (hooks
    /// in their types' order), as [`summarize`] makes them.
    entries: Vec<ReferenceSummary>,
    /// Each entry's position by kind and name.
    by_name: BTreeMap<(MentionerKind, String), usize>,
}

/// The reference summaries of a build's modules, and the names they use
/// resolved back to ids.
pub(crate) struct Summaries<'f> {
    front: &'f Frontend,
    modules: BTreeMap<ModuleId, ModuleSummary>,
    /// Module paths to ids.
    paths: FxHashMap<String, ModuleId>,
    /// Each module's function, constant and constructor names to their
    /// indices.
    names: BTreeMap<ModuleId, FxHashMap<String, DefIndex>>,
}

impl<'f> Summaries<'f> {
    /// The summaries of every module `front` checked, in build order.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a module's THIR and interfaces disagree.
    pub(crate) fn new(front: &'f Frontend) -> Result<Self, Ice> {
        let mut modules = BTreeMap::new();
        for &module in &front.order {
            let entries = summarize(front, module)?;
            let by_name = entries
                .iter()
                .enumerate()
                .map(|(n, s)| ((s.kind, s.item.clone()), n))
                .collect();
            modules.insert(module, ModuleSummary { entries, by_name });
        }
        let mut paths = FxHashMap::default();
        let mut names = BTreeMap::new();
        for (id, info) in front.modules.iter() {
            paths.insert(info.path.to_string(), id);
            let mut by_name = FxHashMap::default();
            if let Some(iface) = front.set.get(id) {
                for (index, it) in iface.items.iter_enumerated() {
                    if matches!(it.kind, DefKind::Fn | DefKind::Const | DefKind::Ctor) {
                        by_name.insert(item_name(front, iface, it), index);
                    }
                }
            }
            names.insert(id, by_name);
        }
        Ok(Self {
            front,
            modules,
            paths,
            names,
        })
    }

    /// The id an [`ItemRef`] names: a function, constant or constructor.
    fn resolve(&self, item: &ItemRef) -> Result<DefId, Ice> {
        let module = *self
            .paths
            .get(&item.module)
            .ok_or_else(|| Ice::new(format!("no module `{}`", item.module)))?;
        let index = self
            .names
            .get(&module)
            .and_then(|names| names.get(&item.item))
            .copied()
            .ok_or_else(|| Ice::new(format!("no item `{}::{}`", item.module, item.item)))?;
        Ok(DefId::new(module, index))
    }

    /// Whether `def` is a constructor.
    fn is_ctor(&self, def: DefId) -> bool {
        self.front
            .set
            .get(def.module)
            .and_then(|iface| iface.item(def.index))
            .is_some_and(|item| item.kind == DefKind::Ctor)
    }

    /// The summary of the function or constant `def`, and whether it is a
    /// constant; `None` for an item without one (an interface function, a
    /// builtin), whose body no build lowers.
    fn item_summary(&self, def: DefId) -> Option<(&ReferenceSummary, bool)> {
        let summary = self.modules.get(&def.module)?;
        let iface = self.front.set.get(def.module)?;
        let item = iface.item(def.index)?;
        let name = item_name(self.front, iface, item);
        let kind = match item.kind {
            DefKind::Fn => MentionerKind::Function,
            DefKind::Const => MentionerKind::Constant,
            _ => return None,
        };
        let &at = summary.by_name.get(&(kind, name))?;
        Some((summary.entries.get(at)?, kind == MentionerKind::Constant))
    }

    /// The mentions of `module`'s hook summaries, in its types' order.
    fn hook_mentions(&self, module: ModuleId) -> Vec<ItemRef> {
        self.modules
            .get(&module)
            .into_iter()
            .flat_map(|s| &s.entries)
            .filter(|s| s.kind == MentionerKind::Hook)
            .flat_map(|s| s.mentions.iter().cloned())
            .collect()
    }

    /// The mentions of `test`'s summary in its module.
    fn test_mentions(&self, test: &thir::TestDef) -> Vec<ItemRef> {
        self.modules
            .get(&test.def.module)
            .and_then(|s| {
                let &at = s
                    .by_name
                    .get(&(MentionerKind::Test, test.name.to_string()))?;
                s.entries.get(at)
            })
            .map(|s| s.mentions.clone())
            .unwrap_or_default()
    }

    /// What `starts` reach, walking summaries (module documentation): the
    /// same [`Reach`] as [`cheby_mir::reach::reachable_with_hooks`] over
    /// THIR from the same roots.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a summary names an item that does not exist.
    pub(crate) fn reach(&self, starts: &[Start<'_>]) -> Result<Reach, Ice> {
        let mut walk = Walk {
            reach: Reach::default(),
            queue: Vec::new(),
        };
        for start in starts {
            match *start {
                Start::Main(f) => {
                    if let Some(entry) = &f.entry {
                        for mention in entry_mentions(entry) {
                            let Mention::Item(def) = mention else {
                                continue;
                            };
                            walk.item(self, def, None)?;
                        }
                    }
                    walk.item(self, f.def, None)?;
                }
                Start::Test(t) => {
                    walk.module(self, t.def.module, None)?;
                    for item in self.test_mentions(t) {
                        walk.mention(self, &item, None)?;
                    }
                }
                Start::Const(def) | Start::Fn(def) => walk.item(self, def, None)?,
            }
        }
        while let Some(def) = walk.queue.pop() {
            let Some((summary, is_const)) = self.item_summary(def) else {
                continue;
            };
            let mentions = summary.mentions.clone();
            if is_const {
                walk.reach.insert_item(def, true);
            }
            for item in &mentions {
                walk.mention(self, item, Some(def))?;
            }
        }
        Ok(walk.reach)
    }

    /// The load set of a build whose roots reach `reach` and whose tests
    /// units are those of `tests_of` (module documentation), in build
    /// order.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a summary names an item that does not exist.
    pub(crate) fn load_set(
        &self,
        reach: &Reach,
        tests_of: &BTreeSet<ModuleId>,
    ) -> Result<Vec<ModuleId>, Ice> {
        let mut loaded: BTreeSet<ModuleId> = reach
            .modules()
            .chain(tests_of.iter().copied())
            .filter(|&m| m != ModuleId::BUILTIN)
            .collect();
        let mut work: Vec<ModuleId> = loaded.iter().copied().collect();
        while let Some(module) = work.pop() {
            for target in self.mentioned(module, tests_of.contains(&module))? {
                if target != ModuleId::BUILTIN && loaded.insert(target) {
                    work.push(target);
                }
            }
        }
        Ok(self
            .front
            .order
            .iter()
            .copied()
            .filter(|m| loaded.contains(m))
            .collect())
    }

    /// The modules whose items or constructors the functions, constants
    /// and hooks of `module` mention, sorted.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `module` has no summary or a summary names an item
    /// that does not exist.
    #[cfg(test)]
    pub(crate) fn mentioned_modules(&self, module: ModuleId) -> Result<BTreeSet<ModuleId>, Ice> {
        self.mentioned(module, false)
    }

    /// The modules `module`'s summaries mention, its tests' too if
    /// `tests`.
    fn mentioned(&self, module: ModuleId, tests: bool) -> Result<BTreeSet<ModuleId>, Ice> {
        let summary = self
            .modules
            .get(&module)
            .ok_or_else(|| Ice::new(format!("module {module:?} has no summary")))?;
        let mut out = BTreeSet::new();
        for entry in &summary.entries {
            if entry.kind == MentionerKind::Test && !tests {
                continue;
            }
            for item in &entry.mentions {
                out.insert(self.resolve(item)?.module);
            }
        }
        Ok(out)
    }
}

/// The state of [`Summaries::reach`]: [`cheby_mir::reach`]'s walk, step
/// for step, over summaries.
struct Walk {
    reach: Reach,
    queue: Vec<DefId>,
}

impl Walk {
    /// One mention of `owner`'s summary (`None` for a root's).
    fn mention(
        &mut self,
        summaries: &Summaries<'_>,
        item: &ItemRef,
        owner: Option<DefId>,
    ) -> Result<(), Ice> {
        let def = summaries.resolve(item)?;
        if summaries.is_ctor(def) {
            self.ctor(summaries, def, owner)
        } else {
            self.item(summaries, def, owner)
        }
    }

    /// Marks `def` reachable; its summary is walked once.
    fn item(
        &mut self,
        summaries: &Summaries<'_>,
        def: DefId,
        owner: Option<DefId>,
    ) -> Result<(), Ice> {
        if let Some(owner) = owner {
            self.reach.insert_edge(owner, def);
        }
        if self.reach.insert_item(def, false) {
            self.module(summaries, def.module, owner)?;
            self.queue.push(def);
        }
        Ok(())
    }

    /// `module` gets a code unit, so the hooks of its types are reached
    /// (D-585).
    fn module(
        &mut self,
        summaries: &Summaries<'_>,
        module: ModuleId,
        owner: Option<DefId>,
    ) -> Result<(), Ice> {
        if !self.reach.insert_module(module) {
            return Ok(());
        }
        for item in summaries.hook_mentions(module) {
            let def = summaries.resolve(&item)?;
            self.item(summaries, def, owner)?;
        }
        Ok(())
    }

    /// A constructor's module needs its layouts registered.
    fn ctor(
        &mut self,
        summaries: &Summaries<'_>,
        ctor: DefId,
        owner: Option<DefId>,
    ) -> Result<(), Ice> {
        if ctor.module == ModuleId::BUILTIN {
            return Ok(());
        }
        self.module(summaries, ctor.module, owner)?;
        if let Some(owner) = owner {
            self.reach.insert_ctor_module(owner, ctor.module);
        }
        Ok(())
    }
}

/// An interface item's name in summaries: its name, or `Parent::name` for
/// a constructor or an interface function.
fn item_name(
    front: &Frontend,
    iface: &cheby_iface::ModuleInterface,
    item: &cheby_iface::IfaceItem,
) -> String {
    let name = front.globals.names.get(item.name);
    match item.parent.and_then(|p| iface.item(p)) {
        Some(parent) => format!("{}::{name}", front.globals.names.get(parent.name)),
        None => name.to_owned(),
    }
}

/// `def` as summaries name it.
fn item_ref(front: &Frontend, def: DefId) -> Result<ItemRef, Ice> {
    let module = front.modules.try_get(def.module)?;
    let iface = front
        .set
        .get(def.module)
        .ok_or_else(|| Ice::new(format!("module `{}` has no interface", module.path)))?;
    let item = iface
        .item(def.index)
        .ok_or_else(|| Ice::new(format!("{def:?} is not an item of `{}`", module.path)))?;
    Ok(ItemRef {
        module: module.path.to_string(),
        item: item_name(front, iface, item),
    })
}

/// The summary entries of `module` (module documentation): its functions
/// and constants in item order, the hooks of its types in their order,
/// then its tests, each mention once in the order reach's walk meets it.
///
/// # Errors
///
/// An [`Ice`] if the module was not checked, or its THIR names an item
/// its interfaces do not have.
pub(crate) fn summarize(front: &Frontend, module: ModuleId) -> Result<Vec<ReferenceSummary>, Ice> {
    let checked = front
        .checked(module)
        .ok_or_else(|| Ice::new(format!("module {module:?} was not checked")))?;
    let thir = &checked.thir;
    let iface = &checked.interface;
    let refs = |mentions: Vec<Mention>| -> Result<Vec<ItemRef>, Ice> {
        mentions
            .into_iter()
            .map(|m| match m {
                Mention::Item(def) | Mention::Ctor(def) => item_ref(front, def),
            })
            .collect()
    };
    let name = |def: DefId| -> Result<String, Ice> {
        iface
            .item(def.index)
            .map(|item| item_name(front, iface, item))
            .ok_or_else(|| Ice::new(format!("{def:?} has no interface item")))
    };
    let mut items: Vec<(DefIndex, ReferenceSummary)> = Vec::new();
    for f in &thir.fns {
        // On native, a native external replaces the body (D-207, D-510).
        let mentions = match (&f.native_external, &f.body) {
            (None, Some(body)) => refs(body_mentions(body)?)?,
            _ => Vec::new(),
        };
        items.push((
            f.def.index,
            ReferenceSummary {
                item: name(f.def)?,
                kind: MentionerKind::Function,
                mentions,
            },
        ));
    }
    for c in &thir.consts {
        items.push((
            c.def.index,
            ReferenceSummary {
                item: name(c.def)?,
                kind: MentionerKind::Constant,
                mentions: refs(body_mentions(&c.body)?)?,
            },
        ));
    }
    items.sort_by_key(|(index, _)| *index);
    let mut out: Vec<ReferenceSummary> = items.into_iter().map(|(_, s)| s).collect();
    let none = cheby_iface::DepView::new(&front.set, &[]);
    let defs = cheby_mir::Defs {
        own: Some(iface),
        deps: &none,
        modules: &front.modules,
        globals: &front.globals,
    };
    for (index, item) in iface.items.iter_enumerated() {
        if let ItemSig::Type(t) = &item.sig
            && t.hooks.is_some()
            && let Some(fns) = cheby_mir::layout::type_hooks(DefId::new(module, index), &defs)?
        {
            out.push(ReferenceSummary {
                item: item_name(front, iface, item),
                kind: MentionerKind::Hook,
                mentions: fns
                    .iter()
                    .map(|&f| item_ref(front, f))
                    .collect::<Result<_, _>>()?,
            });
        }
    }
    for t in &thir.tests {
        // A test is always a root: its entry, then its body (D-182).
        let mut mentions = entry_mentions(&t.entry);
        for m in body_mentions(&t.body)? {
            if !mentions.contains(&m) {
                mentions.push(m);
            }
        }
        let mentions = refs(mentions)?;
        match out
            .iter_mut()
            .find(|s| s.kind == MentionerKind::Test && *s.item == *t.name)
        {
            // Two tests of one name share a summary, as in front records.
            Some(same) => {
                for m in mentions {
                    if !same.mentions.contains(&m) {
                        same.mentions.push(m);
                    }
                }
            }
            None => out.push(ReferenceSummary {
                item: t.name.to_string(),
                kind: MentionerKind::Test,
                mentions,
            }),
        }
    }
    Ok(out)
}
