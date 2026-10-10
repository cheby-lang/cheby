//! Which items a program needs (D-510).
//!
//! `cheby run` lowers only the items reachable from the entry it runs,
//! like a linker that drops unused code. While the back end is partial,
//! an unused function that it cannot lower yet then never stops a run,
//! and std modules that every program depends on (D-447) cost nothing
//! when unused.
//!
//! From the roots, the walk follows every expression of each reachable
//! body, closures and local functions included, and from them:
//!
//! - [`Callee::Fn`] and [`ExprKind::FnRef`]: the function, and its
//!   dictionaries;
//! - [`ExprKind::ConstRef`]: the constant, whose initializer is walked
//!   like a body;
//! - every [`DictFn`] of every [`Dict`], nested dictionaries included, in
//!   calls, function values, interface calls, operators, interpolation,
//!   `dyn` conversions and the entry's `Show` dictionary;
//! - [`OpImpl::Call`] and the `show` callee of each interpolated value;
//! - with [`reachable_with_hooks`], the `@std_hooks` functions of every
//!   type declared by a module that gets a code unit (D-482): the unit
//!   registers all its types' layouts, and the runtime calls the hooks for
//!   `==`, hashing and debug printing.
//!
//! The walk also records the modules whose constructors the reachable code
//! mentions, so their layouts are registered (D-454), and, for constant
//! evaluation (E6), which items each item refers to: the dependency order
//! of constants ([`Reach::constant_order`]) and the modules an initializer
//! reaches ([`Reach::modules_of`], for its hash seed, D-538). It reads THIR
//! only; the result is made of sets, so lowering order stays `DefIndex`
//! order and the output is deterministic (D-264).
//!
//! [`DictFn`]: cheby_types::thir::DictFn

use std::collections::{BTreeMap, BTreeSet};

use cheby_iface::{DefId, Ice, ModuleId};
use cheby_types::thir::{
    self, Callee, Dict, EntryInfo, ExprKind, InterpPart, OpImpl, Pat, PatKind,
};

/// The checked bodies of every module of a program, by module.
pub type Program<'a> = BTreeMap<ModuleId, &'a thir::Module>;

/// The `@std_hooks(equal, hash, debug)` functions of every type that a
/// module declares (D-482): how [`reachable_with_hooks`] reads interfaces
/// without depending on them.
pub type HooksOf<'h> = dyn Fn(ModuleId) -> Result<Vec<DefId>, Ice> + 'h;

/// The items reachable from some roots.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reach {
    /// Reachable functions and constants.
    items: BTreeSet<DefId>,
    /// Modules with a reachable item or a constructor that reachable code
    /// mentions: the modules that get a code unit.
    modules: BTreeSet<ModuleId>,
    /// The reachable constants.
    consts: BTreeSet<DefId>,
    /// The items each reachable item's body refers to directly.
    edges: BTreeMap<DefId, BTreeSet<DefId>>,
    /// The modules of the constructors each reachable item's body
    /// mentions.
    ctor_modules: BTreeMap<DefId, BTreeSet<ModuleId>>,
}

impl Reach {
    /// Whether `def` is reachable.
    #[must_use]
    pub fn contains(&self, def: DefId) -> bool {
        self.items.contains(&def)
    }

    /// Whether `module` needs a code unit: it has a reachable item, or
    /// reachable code uses one of its constructors.
    #[must_use]
    pub fn needs_module(&self, module: ModuleId) -> bool {
        self.modules.contains(&module)
    }

    /// The reachable items, sorted.
    pub fn items(&self) -> impl Iterator<Item = DefId> + '_ {
        self.items.iter().copied()
    }

    /// The reachable constants in the dependency order of their
    /// initializers (D-535): a constant comes after every constant its
    /// initializer reaches, through functions included. Ties go by
    /// [`DefId`], module id then declaration order; the driver passes
    /// module order and names through [`Reach::constant_order_by`]. The
    /// constants of a cycle through function calls come in that tie order
    /// too, and evaluation finds the cycle (E0069).
    #[must_use]
    pub fn constant_order(&self) -> Vec<DefId> {
        self.constant_order_by(|def| def)
    }

    /// [`Reach::constant_order`] with ties broken by `key`, smallest first
    /// (D-535: module order, then name).
    #[must_use]
    pub fn constant_order_by<K: Ord>(&self, key: impl Fn(DefId) -> K) -> Vec<DefId> {
        // The constants each constant's initializer reaches directly or
        // through functions, without going through another initializer:
        // that constant's own dependencies are ordered on their own.
        let deps = self.const_deps();
        let mut waiting: BTreeMap<DefId, usize> = BTreeMap::new();
        let mut users: BTreeMap<DefId, Vec<DefId>> = BTreeMap::new();
        for (&c, ds) in &deps {
            waiting.insert(c, ds.len());
            for &d in ds {
                users.entry(d).or_default().push(c);
            }
        }
        let mut ready: BTreeSet<(K, DefId)> = waiting
            .iter()
            .filter(|&(_, &n)| n == 0)
            .map(|(&c, _)| (key(c), c))
            .collect();
        let mut order = Vec::with_capacity(deps.len());
        let mut placed = BTreeSet::new();
        while order.len() < deps.len() {
            // When only cycles are left, the smallest constant of them comes
            // next; evaluating it reports the cycle.
            let next = ready.pop_first().map(|(_, c)| c).or_else(|| {
                waiting
                    .keys()
                    .filter(|c| !placed.contains(*c))
                    .map(|&c| (key(c), c))
                    .min()
                    .map(|(_, c)| c)
            });
            let Some(next) = next else {
                break;
            };
            if !placed.insert(next) {
                continue;
            }
            order.push(next);
            for &user in users.get(&next).map_or(&[][..], Vec::as_slice) {
                if let Some(n) = waiting.get_mut(&user) {
                    *n = n.saturating_sub(1);
                    if *n == 0 && !placed.contains(&user) {
                        ready.insert((key(user), user));
                    }
                }
            }
        }
        order
    }

    /// For every reachable constant, the constants its initializer reaches
    /// without entering another initializer, itself included if it reaches
    /// itself.
    ///
    /// One pass over the whole graph, not one walk per constant, so a
    /// program with many constants over long call chains is ordered in time
    /// linear in the graph times the constants' bit sets: the functions are
    /// condensed into strongly connected components, and each component's
    /// set of constants is the union of its successors', in reverse
    /// topological order.
    fn const_deps(&self) -> BTreeMap<DefId, BTreeSet<DefId>> {
        let consts: Vec<DefId> = self.consts.iter().copied().collect();
        let const_index: BTreeMap<DefId, usize> =
            consts.iter().enumerate().map(|(n, &c)| (c, n)).collect();
        // The function subgraph: an initializer is entered only from its
        // own constant.
        let fns: Vec<DefId> = self
            .items
            .iter()
            .copied()
            .filter(|d| !self.consts.contains(d))
            .collect();
        let graph = Graph::new(&fns, |def| {
            self.edges_of(def).filter(|d| !self.consts.contains(d))
        });
        let mut reached: Vec<Bits> = Vec::with_capacity(graph.components.len());
        for component in &graph.components {
            let mut bits = Bits::default();
            for &node in component {
                let Some(&def) = fns.get(node) else { continue };
                for succ in self.edges_of(def) {
                    if let Some(&n) = const_index.get(&succ) {
                        bits.insert(n);
                    } else if let Some(other) = graph.component_of(succ)
                        && let Some(done) = reached.get(other)
                    {
                        bits.union_with(done);
                    }
                }
            }
            reached.push(bits);
        }
        let mut out = BTreeMap::new();
        for &c in &consts {
            let mut bits = Bits::default();
            for succ in self.edges_of(c) {
                if let Some(&n) = const_index.get(&succ) {
                    bits.insert(n);
                } else if let Some(done) = graph.component_of(succ).and_then(|k| reached.get(k)) {
                    bits.union_with(done);
                }
            }
            out.insert(
                c,
                bits.iter().filter_map(|n| consts.get(n).copied()).collect(),
            );
        }
        out
    }

    fn edges_of(&self, def: DefId) -> impl Iterator<Item = DefId> + '_ {
        self.edges.get(&def).into_iter().flatten().copied()
    }

    /// [`Reach::modules_of`] for every reachable constant, in one pass over
    /// the item graph (strongly connected components, each component's
    /// modules the union of its own and its successors').
    #[must_use]
    pub fn const_modules(&self) -> BTreeMap<DefId, Vec<ModuleId>> {
        let items: Vec<DefId> = self.items.iter().copied().collect();
        let graph = Graph::new(&items, |def| self.edges_of(def));
        // Dense indices of the modules involved.
        let mut module_ids: Vec<ModuleId> = Vec::new();
        let mut module_index: BTreeMap<ModuleId, usize> = BTreeMap::new();
        let mut index_of = |m: ModuleId| {
            *module_index.entry(m).or_insert_with(|| {
                module_ids.push(m);
                module_ids.len().saturating_sub(1)
            })
        };
        let mut reached: Vec<Bits> = Vec::with_capacity(graph.components.len());
        for (k, component) in graph.components.iter().enumerate() {
            let mut bits = Bits::default();
            for &node in component {
                let Some(&def) = items.get(node) else {
                    continue;
                };
                bits.insert(index_of(def.module));
                for &m in self.ctor_modules.get(&def).into_iter().flatten() {
                    bits.insert(index_of(m));
                }
                for succ in self.edges_of(def) {
                    if let Some(other) = graph.component_of(succ).filter(|&o| o != k)
                        && let Some(done) = reached.get(other)
                    {
                        bits.union_with(done);
                    }
                }
            }
            reached.push(bits);
        }
        self.consts
            .iter()
            .map(|&c| {
                let mut modules: Vec<ModuleId> = graph
                    .component_of(c)
                    .and_then(|k| reached.get(k))
                    .into_iter()
                    .flat_map(Bits::iter)
                    .filter_map(|n| module_ids.get(n).copied())
                    .filter(|&m| m != ModuleId::BUILTIN)
                    .collect();
                modules.sort_unstable();
                (c, modules)
            })
            .collect()
    }

    /// The modules whose functions, constants or constructors the
    /// initializer of `root` reaches, transitively and through the
    /// initializers of the constants it reads, `root`'s own module
    /// included: what its value depends on (D-538, D-593:
    /// constructors' modules and the initializers of read constants count).
    /// Sorted by module id; the hash seed sorts them by module path.
    #[must_use]
    pub fn modules_of(&self, root: DefId) -> Vec<ModuleId> {
        let mut modules = BTreeSet::from([root.module]);
        let mut seen = BTreeSet::new();
        let mut stack = vec![root];
        while let Some(def) = stack.pop() {
            if !seen.insert(def) {
                continue;
            }
            modules.insert(def.module);
            if let Some(ms) = self.ctor_modules.get(&def) {
                modules.extend(ms.iter().copied());
            }
            stack.extend(self.edges_of(def));
        }
        modules.remove(&ModuleId::BUILTIN);
        modules.into_iter().collect()
    }
}

/// A set of small numbers, one bit each.
#[derive(Clone, Debug, Default)]
struct Bits(Vec<u64>);

impl Bits {
    fn insert(&mut self, n: usize) {
        let word = n / 64;
        if self.0.len() <= word {
            self.0.resize(word.saturating_add(1), 0);
        }
        if let Some(w) = self.0.get_mut(word) {
            *w |= 1_u64 << (n % 64);
        }
    }

    fn union_with(&mut self, other: &Self) {
        if self.0.len() < other.0.len() {
            self.0.resize(other.0.len(), 0);
        }
        for (w, &o) in self.0.iter_mut().zip(&other.0) {
            *w |= o;
        }
    }

    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().enumerate().flat_map(|(k, &w)| {
            (0..64_usize)
                .filter(move |b| w & (1_u64 << b) != 0)
                .map(move |b| k.saturating_mul(64).saturating_add(b))
        })
    }
}

/// The search state of one node in [`Graph::new`].
#[derive(Clone, Copy, Debug, Default)]
struct Visit {
    /// The node's discovery number, once found.
    order: Option<usize>,
    /// The smallest discovery number it reaches within its component.
    low: usize,
    on_stack: bool,
}

/// A graph over some items, condensed into strongly connected components
/// with Tarjan's algorithm on explicit stacks (no recursion, plan §0).
struct Graph {
    index: BTreeMap<DefId, usize>,
    /// Each node's component.
    component: Vec<usize>,
    /// The components' nodes, in reverse topological order: a component
    /// comes after every component it reaches.
    components: Vec<Vec<usize>>,
}

impl Graph {
    /// The graph of `nodes`, with the successors `succ` gives; successors
    /// outside `nodes` are ignored.
    fn new<I: Iterator<Item = DefId>>(nodes: &[DefId], succ: impl Fn(DefId) -> I) -> Self {
        let index: BTreeMap<DefId, usize> =
            nodes.iter().enumerate().map(|(n, &d)| (d, n)).collect();
        let adj: Vec<Vec<usize>> = nodes
            .iter()
            .map(|&d| succ(d).filter_map(|s| index.get(&s).copied()).collect())
            .collect();
        let mut visits = vec![Visit::default(); nodes.len()];
        let mut component = vec![usize::MAX; nodes.len()];
        let mut components: Vec<Vec<usize>> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        let mut frames: Vec<(usize, usize)> = Vec::new();
        let mut counter = 0_usize;
        for root in 0..nodes.len() {
            if visits.get(root).is_none_or(|v| v.order.is_some()) {
                continue;
            }
            open(&mut visits, &mut stack, &mut counter, root);
            frames.push((root, 0));
            while let Some(frame) = frames.last_mut() {
                let (node, next) = *frame;
                if let Some(&s) = adj.get(node).and_then(|a| a.get(next)) {
                    frame.1 = next.saturating_add(1);
                    match visits.get(s).copied() {
                        Some(Visit { order: None, .. }) => {
                            open(&mut visits, &mut stack, &mut counter, s);
                            frames.push((s, 0));
                        }
                        Some(Visit {
                            order: Some(order),
                            on_stack: true,
                            ..
                        }) => {
                            if let Some(v) = visits.get_mut(node) {
                                v.low = v.low.min(order);
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                frames.pop();
                let Some(&Visit {
                    order: Some(order),
                    low,
                    ..
                }) = visits.get(node)
                else {
                    continue;
                };
                if low == order {
                    let k = components.len();
                    let mut members = Vec::new();
                    while let Some(m) = stack.pop() {
                        if let Some(v) = visits.get_mut(m) {
                            v.on_stack = false;
                        }
                        if let Some(c) = component.get_mut(m) {
                            *c = k;
                        }
                        members.push(m);
                        if m == node {
                            break;
                        }
                    }
                    components.push(members);
                }
                if let Some(&(parent, _)) = frames.last()
                    && let Some(v) = visits.get_mut(parent)
                {
                    v.low = v.low.min(low);
                }
            }
        }
        Self {
            index,
            component,
            components,
        }
    }

    /// The component of `def`, if it is a node.
    fn component_of(&self, def: DefId) -> Option<usize> {
        self.index
            .get(&def)
            .and_then(|&n| self.component.get(n))
            .copied()
    }
}

/// Gives `node` its discovery number and puts it on the component stack.
fn open(visits: &mut [Visit], stack: &mut Vec<usize>, counter: &mut usize, node: usize) {
    if let Some(v) = visits.get_mut(node) {
        *v = Visit {
            order: Some(*counter),
            low: *counter,
            on_stack: true,
        };
    }
    *counter = counter.saturating_add(1);
    stack.push(node);
}

/// What the walk starts from.
#[derive(Clone, Copy, Debug)]
pub enum Root<'a> {
    /// A function, such as the `main` being run, with its entry
    /// information, whose `Show` dictionary is reachable too (D-182).
    Fn(&'a thir::FnDef),
    /// A `test` block (G4).
    Test(&'a thir::TestDef),
    /// A constant, whose initializer is evaluated (E6, D-534).
    Const(&'a thir::ConstDef),
}

/// One item of the program, found by its id.
#[derive(Clone, Copy)]
enum ItemRef<'a> {
    Fn(&'a thir::FnDef),
    Const(&'a thir::ConstDef),
}

struct Walk<'a, 'h> {
    index: BTreeMap<DefId, ItemRef<'a>>,
    reach: Reach,
    queue: Vec<DefId>,
    /// The item whose body is being walked; `None` for a test's body.
    owner: Option<DefId>,
    hooks: Option<&'h HooksOf<'h>>,
    /// The first failure of `hooks`.
    error: Option<Ice>,
}

/// The items of `program` reachable from `roots`.
///
/// # Errors
///
/// An [`Ice`] if a body refers to an expression it does not have.
pub fn reachable(program: &Program<'_>, roots: &[Root<'_>]) -> Result<Reach, Ice> {
    walk(program, roots, None)
}

/// [`reachable`], also reaching the `@std_hooks` functions of every type
/// declared by a module that needs a code unit (D-482), which `hooks`
/// gives for a module.
///
/// # Errors
///
/// An [`Ice`] if a body refers to an expression it does not have, or
/// `hooks` fails.
pub fn reachable_with_hooks(
    program: &Program<'_>,
    roots: &[Root<'_>],
    hooks: &HooksOf<'_>,
) -> Result<Reach, Ice> {
    walk(program, roots, Some(hooks))
}

fn walk<'h>(
    program: &Program<'_>,
    roots: &[Root<'_>],
    hooks: Option<&'h HooksOf<'h>>,
) -> Result<Reach, Ice> {
    let mut index = BTreeMap::new();
    for module in program.values() {
        for f in &module.fns {
            index.insert(f.def, ItemRef::Fn(f));
        }
        for c in &module.consts {
            index.insert(c.def, ItemRef::Const(c));
        }
    }
    let mut walk = Walk {
        index,
        reach: Reach::default(),
        queue: Vec::new(),
        owner: None,
        hooks,
        error: None,
    };
    for root in roots {
        walk.owner = None;
        match *root {
            Root::Fn(f) => {
                if let Some(entry) = &f.entry {
                    walk.entry(entry);
                }
                walk.item(f.def);
            }
            Root::Test(t) => {
                walk.module(t.def.module);
                walk.entry(&t.entry);
                walk.body(&t.body)?;
            }
            Root::Const(c) => walk.item(c.def),
        }
    }
    while let Some(def) = walk.queue.pop() {
        walk.owner = Some(def);
        match walk.index.get(&def).copied() {
            Some(ItemRef::Fn(f)) => {
                // On native, a function with a native external runs the
                // external and never its body (D-207).
                if let (None, Some(body)) = (&f.native_external, &f.body) {
                    walk.body(body)?;
                }
            }
            Some(ItemRef::Const(c)) => {
                walk.reach.consts.insert(def);
                walk.body(&c.body)?;
            }
            // Not a checked item, such as an interface function: lowering
            // never sees it.
            None => {}
        }
    }
    match walk.error {
        Some(ice) => Err(ice),
        None => Ok(walk.reach),
    }
}

impl Walk<'_, '_> {
    /// Marks `def` reachable; its body is walked once.
    fn item(&mut self, def: DefId) {
        if let Some(owner) = self.owner {
            self.reach.edges.entry(owner).or_default().insert(def);
        }
        if self.reach.items.insert(def) {
            self.module(def.module);
            self.queue.push(def);
        }
    }

    /// `module` gets a code unit, which registers the layouts of every type
    /// it declares, so those types' hooks are reachable
    /// (D-585).
    fn module(&mut self, module: ModuleId) {
        if !self.reach.modules.insert(module) {
            return;
        }
        let Some(hooks) = self.hooks else {
            return;
        };
        match hooks(module) {
            Ok(fns) => {
                for f in fns {
                    self.item(f);
                }
            }
            Err(ice) => {
                self.error.get_or_insert(ice);
            }
        }
    }

    /// A constructor's module needs its layouts registered.
    fn ctor(&mut self, ctor: DefId) {
        if ctor.module == ModuleId::BUILTIN {
            return;
        }
        self.module(ctor.module);
        if let Some(owner) = self.owner {
            self.reach
                .ctor_modules
                .entry(owner)
                .or_default()
                .insert(ctor.module);
        }
    }

    fn entry(&mut self, entry: &EntryInfo) {
        if let EntryInfo::Result {
            show: Some(dict), ..
        } = entry
        {
            self.dict(dict);
        }
    }

    fn dict(&mut self, dict: &Dict) {
        match dict {
            Dict::Impl { fns, .. } => {
                for f in fns {
                    self.item(f.def);
                    for d in &f.dicts {
                        self.dict(d);
                    }
                }
            }
            Dict::Embedded { of, .. } => self.dict(of),
            Dict::Tuple { elems, .. } => {
                for d in elems {
                    self.dict(d);
                }
            }
            // An enclosing item's own dictionary, or the one a `dyn`
            // value carries: what it holds was reached where it was built.
            // A kind added to THIR later is lowered as E0038.
            _ => {}
        }
    }

    fn callee(&mut self, callee: &Callee) {
        match callee {
            Callee::Fn { def, dicts, .. } => {
                self.item(*def);
                for d in dicts {
                    self.dict(d);
                }
            }
            Callee::Iface { dict, .. } => self.dict(dict),
            Callee::Ctor { ctor, .. } => self.ctor(*ctor),
            // A `dyn` call uses the receiver's dictionary; a value callee
            // is an expression, walked as one.
            _ => {}
        }
    }

    fn pat(&mut self, pat: &Pat) {
        let mut ctors = Vec::new();
        pat.walk(&mut |p| {
            if let PatKind::Ctor { ctor, .. } = &p.kind {
                ctors.push(*ctor);
            }
        });
        for ctor in ctors {
            self.ctor(ctor);
        }
    }

    /// Every expression of `body` reachable from its value, closures and
    /// local functions included.
    fn body(&mut self, body: &thir::Body) -> Result<(), Ice> {
        let mut stack = vec![body.value];
        let mut seen = BTreeSet::new();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let expr = body
                .expr(id)
                .ok_or_else(|| Ice::new(format!("no THIR expression {id:?}")))?;
            self.expr(&expr.kind);
            for (_, pat) in expr.kind.pats() {
                self.pat(pat);
            }
            stack.extend(expr.kind.children());
        }
        Ok(())
    }

    fn expr(&mut self, kind: &ExprKind) {
        match kind {
            ExprKind::Call { callee, .. } => self.callee(callee),
            ExprKind::FnRef { def, dicts, .. } => {
                self.item(*def);
                for d in dicts {
                    self.dict(d);
                }
            }
            ExprKind::ConstRef { def } => self.item(*def),
            ExprKind::Ctor { ctor, .. } | ExprKind::Record { ctor, .. } => self.ctor(*ctor),
            ExprKind::Field { index_per_ctor, .. } => {
                for &(ctor, _) in index_per_ctor {
                    self.ctor(ctor);
                }
            }
            ExprKind::RecordUpdate { fields, .. } => {
                for field in fields {
                    for &(ctor, _) in &field.index_per_ctor {
                        self.ctor(ctor);
                    }
                }
            }
            ExprKind::Binary { imp, .. } | ExprKind::Neg { imp, .. } => {
                if let OpImpl::Call(callee) = imp {
                    self.callee(callee);
                }
            }
            ExprKind::Interpolate { parts } => {
                for part in parts {
                    if let InterpPart::Value {
                        show: Some(callee), ..
                    } = part
                    {
                        self.callee(callee);
                    }
                }
            }
            ExprKind::DynCoerce { dict, .. } => self.dict(dict),
            // The rest refer to no item; their sub-expressions are walked
            // as children. A variant added to THIR later is lowered as
            // E0038 until the back end supports it.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(module: u32, index: u32) -> DefId {
        DefId::new(
            ModuleId::from_u32(module),
            cheby_iface::DefIndex::from_u32(index),
        )
    }

    fn reach(consts: &[DefId], edges: &[(DefId, DefId)]) -> Reach {
        let mut r = Reach::default();
        for &c in consts {
            r.consts.insert(c);
            r.items.insert(c);
        }
        for &(a, b) in edges {
            r.items.insert(a);
            r.items.insert(b);
            r.edges.entry(a).or_default().insert(b);
        }
        r
    }

    #[test]
    fn constants_follow_their_dependencies_through_functions() {
        let (ca, cb, cc, func) = (d(1, 0), d(1, 1), d(2, 0), d(2, 5));
        // ca reads func, func reads cc; cb reads ca.
        let r = reach(&[ca, cb, cc], &[(ca, func), (func, cc), (cb, ca)]);
        assert_eq!(r.constant_order(), [cc, ca, cb]);
        // Ties by key: cc and an unrelated ce.
        let ce = d(0, 9);
        let r = reach(&[ca, ce, cc], &[(ca, cc)]);
        assert_eq!(r.constant_order(), [ce, cc, ca]);
        assert_eq!(r.constant_order_by(std::cmp::Reverse), [cc, ca, ce]);
    }

    #[test]
    fn cycles_still_place_every_constant() {
        let [ca, cb, f1, f2, cz] = [0, 1, 2, 3, 4].map(|i| d(1, i));
        // ca -> f1 -> cb -> f2 -> ca, and cz depends on ca.
        let r = reach(
            &[ca, cb, cz],
            &[(ca, f1), (f1, cb), (cb, f2), (f2, ca), (cz, ca)],
        );
        assert_eq!(r.constant_order(), [ca, cb, cz]);
    }

    #[test]
    fn modules_of_follow_items_and_constructors() {
        let (a, f, x) = (d(3, 0), d(2, 1), d(5, 0));
        let mut r = reach(&[a, x], &[(a, f), (f, x)]);
        r.ctor_modules
            .entry(f)
            .or_default()
            .insert(ModuleId::from_u32(7));
        assert_eq!(
            r.modules_of(a),
            [2, 3, 5, 7].map(ModuleId::from_u32).to_vec()
        );
        assert_eq!(r.modules_of(x), [ModuleId::from_u32(5)]);
    }

    /// The constants `c` reaches without entering another initializer,
    /// one walk per constant: the definition [`Reach::const_deps`] must
    /// match.
    fn naive_deps(r: &Reach, c: DefId) -> BTreeSet<DefId> {
        let mut out = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut stack: Vec<DefId> = r.edges_of(c).collect();
        while let Some(def) = stack.pop() {
            if !seen.insert(def) {
                continue;
            }
            if r.consts.contains(&def) {
                out.insert(def);
            } else {
                stack.extend(r.edges_of(def));
            }
        }
        out
    }

    #[test]
    fn whole_graph_passes_match_one_walk_per_constant() {
        // Pseudo-random graphs with cycles through functions and through
        // constants, and constructor modules.
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = |bound: u32| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            u32::try_from(state % u64::from(bound)).unwrap()
        };
        for round in 0..40 {
            let n = 5 + round;
            let nodes: Vec<DefId> = (0..n).map(|i| d(next(6) + 1, i)).collect();
            let consts: Vec<DefId> = nodes.iter().copied().filter(|_| next(3) == 0).collect();
            let mut edges = Vec::new();
            for _ in 0..n * 2 {
                let a = nodes[usize::try_from(next(n)).unwrap()];
                let b = nodes[usize::try_from(next(n)).unwrap()];
                edges.push((a, b));
            }
            let mut r = reach(&consts, &edges);
            for &node in &nodes {
                r.items.insert(node);
                if next(4) == 0 {
                    r.ctor_modules
                        .entry(node)
                        .or_default()
                        .insert(ModuleId::from_u32(next(9) + 1));
                }
            }
            let deps = r.const_deps();
            let modules = r.const_modules();
            for &c in &r.consts {
                assert_eq!(deps[&c], naive_deps(&r, c), "round {round}, {c:?}");
                assert_eq!(modules[&c], r.modules_of(c), "round {round}, {c:?}");
            }
        }
    }

    #[test]
    fn many_constants_over_a_long_chain_are_ordered_quickly() {
        // 5,000 functions calling the next, 5,000 constants calling the
        // first, and a chain of 5,000 constants each reading the previous:
        // one walk per constant took minutes.
        let n = 5_000;
        let fns: Vec<DefId> = (0..n).map(|i| d(1, i)).collect();
        let calls: Vec<DefId> = (0..n).map(|i| d(2, i)).collect();
        let chain: Vec<DefId> = (0..n).map(|i| d(3, i)).collect();
        let mut edges: Vec<(DefId, DefId)> = fns.windows(2).map(|w| (w[0], w[1])).collect();
        edges.extend(calls.iter().map(|&c| (c, fns[0])));
        edges.extend(chain.windows(2).map(|w| (w[1], w[0])));
        let consts: Vec<DefId> = calls.iter().chain(&chain).copied().collect();
        let r = reach(&consts, &edges);
        let start = std::time::Instant::now();
        let order = r.constant_order();
        let modules = r.const_modules();
        assert!(start.elapsed() < std::time::Duration::from_secs(20));
        assert_eq!(order.len(), consts.len());
        let position: BTreeMap<DefId, usize> =
            order.iter().enumerate().map(|(i, &c)| (c, i)).collect();
        assert!(chain.windows(2).all(|w| position[&w[0]] < position[&w[1]]));
        assert_eq!(modules[&calls[0]], [1, 2].map(ModuleId::from_u32).to_vec());
        assert_eq!(modules[chain.last().unwrap()], [ModuleId::from_u32(3)]);
    }
}
