//! Which items a program needs (GAP(backend-reachability)).
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
//! - [`OpImpl::Call`] and the `show` callee of each interpolated value.
//!
//! The walk also records the modules whose constructors the reachable code
//! mentions, so their layouts are registered (D-454). It reads THIR only;
//! the result is a set, so lowering order stays `DefIndex` order and the
//! output is deterministic (D-264).

use std::collections::{BTreeMap, BTreeSet};

use cheby_iface::{DefId, Ice, ModuleId};
use cheby_types::thir::{
    self, Callee, Dict, EntryInfo, ExprKind, InterpPart, OpImpl, Pat, PatKind,
};

/// The checked bodies of every module of a program, by module.
pub type Program<'a> = BTreeMap<ModuleId, &'a thir::Module>;

/// The items reachable from some roots.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reach {
    /// Reachable functions and constants.
    items: BTreeSet<DefId>,
    /// Modules with a reachable item or a constructor that reachable code
    /// mentions: the modules that get a code unit.
    modules: BTreeSet<ModuleId>,
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
}

/// What the walk starts from.
#[derive(Clone, Copy, Debug)]
pub enum Root<'a> {
    /// A function, such as the `main` being run, with its entry
    /// information, whose `Show` dictionary is reachable too (D-182).
    Fn(&'a thir::FnDef),
    /// A `test` block (G4).
    Test(&'a thir::TestDef),
}

/// One item of the program, found by its id.
#[derive(Clone, Copy)]
enum ItemRef<'a> {
    Fn(&'a thir::FnDef),
    Const(&'a thir::ConstDef),
}

struct Walk<'a> {
    index: BTreeMap<DefId, ItemRef<'a>>,
    reach: Reach,
    queue: Vec<DefId>,
}

/// The items of `program` reachable from `roots`.
///
/// # Errors
///
/// An [`Ice`] if a body refers to an expression it does not have.
pub fn reachable(program: &Program<'_>, roots: &[Root<'_>]) -> Result<Reach, Ice> {
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
    };
    for root in roots {
        match *root {
            Root::Fn(f) => {
                if let Some(entry) = &f.entry {
                    walk.entry(entry);
                }
                walk.item(f.def);
            }
            Root::Test(t) => {
                walk.reach.modules.insert(t.def.module);
                walk.entry(&t.entry);
                walk.body(&t.body)?;
            }
        }
    }
    while let Some(def) = walk.queue.pop() {
        match walk.index.get(&def).copied() {
            Some(ItemRef::Fn(f)) => {
                if let Some(body) = &f.body {
                    walk.body(body)?;
                }
            }
            Some(ItemRef::Const(c)) => walk.body(&c.body)?,
            // Not a checked item, such as an interface function: lowering
            // never sees it.
            None => {}
        }
    }
    Ok(walk.reach)
}

impl Walk<'_> {
    /// Marks `def` reachable; its body is walked once.
    fn item(&mut self, def: DefId) {
        if self.reach.items.insert(def) {
            self.reach.modules.insert(def.module);
            self.queue.push(def);
        }
    }

    /// A constructor's module needs its layouts registered.
    fn ctor(&mut self, ctor: DefId) {
        if ctor.module != ModuleId::BUILTIN {
            self.reach.modules.insert(ctor.module);
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
        pat.walk(&mut |p| {
            if let PatKind::Ctor { ctor, .. } = &p.kind
                && ctor.module != ModuleId::BUILTIN
            {
                self.reach.modules.insert(ctor.module);
            }
        });
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
