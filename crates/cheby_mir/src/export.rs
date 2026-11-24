//! Bodies that another unit may inline (design T8, T9): a function of
//! one unit with what its body names that only its unit defines, so the
//! release inliner can copy it into another unit ([`crate::inline`]).
//!
//! # What crosses ([`exports`])
//!
//! A unit exports a function when the inliner may copy it inside the unit
//! ([`crate::inline`]'s callee rule: a top-level function in no cycle of
//! the unit's call graph, not `precounted`, naming no continuation and no
//! fiber context, not an `@external` wrapper's shape, not a wait that
//! keeps its caller's frame, D-861), when it costs at most the inliner's
//! budget, and when nothing it names, after the closures and statics it
//! brings along, is `priv`: the units of other modules have no def of a
//! `priv` item when they read the module's decoded interface (D-953), so
//! the rule holds for every body, whoever reads it.
//!
//! With the `spec` lever a unit also exports, for clones only
//! ([`crate::specialize`]), a function above the inliner's budget, up to
//! the clone budget, and one in a cycle; [`Exported::inline`] tells the
//! two apart. Such an export may bring the `priv` functions of its module
//! it calls, directly or through each other, as helpers
//! ([`Exported::helpers`]), each keyed as a clone and called by key: no
//! other module can name them. A function whose calls got clones is
//! exported as it was before, so another unit's copy names no clone.
//!
//! An export is lowered MIR after its own unit's release passes, so the
//! inliner copies it flat and never searches it again. It is a bundle:
//!
//! - the function itself;
//! - the closures it names by key, transitively, which the importer
//!   renumbers as closures of the host (no two different bodies then
//!   share a symbol, D-918);
//! - the functions that depend on nothing but their key, which it or its
//!   statics name (value shims, tuple functions, `dyn` forwarders): the
//!   importer brings in the ones the host's unit lacks, and every unit's
//!   copy is the same (D-918);
//! - the static objects it names, transitively, numbered from 0 in the
//!   order of the unit's statics: each keeps its module and name, so its
//!   symbol is the exporter's, and the importer merges equal ones as
//!   lowering does (D-523).
//!
//! # Across front ends ([`PortableBody`])
//!
//! Std's bodies come from std's own front end ([`crate::borrow`]'s
//! names): a [`PortableBody`] names every def of its bundle by module
//! path and item, and [`PortableBody::import`] finds each one in the
//! reading build's interfaces, so the body names that build's defs. The
//! locals lose their source names and types, which belong to the other
//! front end and which nothing after lowering needs (`hoist` ran, and
//! borrowing reads only the parameters' types, D-715's rule 5), and
//! their spans name the other front end's files. The form holds nothing
//! of a front end besides those defs, so the build cache stores std's in
//! its export record ([`encode_bodies`], design T15).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use cheby_iface::{DefId, DefKind, Ice, ModuleId, Vis};
use rustc_hash::FxHashMap;

use crate::defs::Defs;
use crate::inline::InlineBudget;
use crate::ir::{
    Callee, Const, EntryKind, FnKey, Function, LayoutRef, MirModule, Operand, Rvalue, StaticId,
    StaticObject, Stmt, TagType, Terminator,
};

/// A function another unit may inline, with what it brings along (module
/// docs).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Exported {
    /// The function's item.
    pub def: DefId,
    /// The function, `FnKey::Item(def)`.
    pub function: Function,
    /// The closures it names, transitively, in the unit's order.
    pub closures: Vec<Function>,
    /// The value shims, tuple functions and `dyn` forwarders that it, its
    /// closures or its statics name, in the unit's order.
    pub shared: Vec<Function>,
    /// The statics it names, transitively: static `n` has id `n`, and a
    /// [`Const::Static`] of the bundle indexes this list.
    pub statics: Vec<StaticObject>,
    /// The source file of its unit ([`MirModule::source`]): a copy into a
    /// unit of the same file keeps its positions.
    pub source: Option<Box<str>>,
    /// Whether the inliner may copy it. When not, it is a function that
    /// only a clone copies ([`crate::specialize`]): one in a cycle of its
    /// unit's call graph, one above the inliner's budget, or one with
    /// helpers.
    pub inline: bool,
    /// The `priv` functions of its module that it calls, directly or
    /// through each other, which the importer copies with the clone, since
    /// no other module can name them (module docs): each keyed as a clone
    /// of the function ([`FnKey::Specialized`], numbered from 0), and
    /// called by key.
    pub helpers: Vec<Function>,
}

/// The functions of `unit` that other units may inline (module docs), in
/// the unit's order, each with its bundle; `defs` must see every def the
/// unit names.
///
/// # Errors
///
/// An [`Ice`] for malformed MIR.
pub fn exports(
    unit: &MirModule,
    defs: &Defs<'_>,
    budget: &InlineBudget,
    plain: &[Function],
    clones: bool,
) -> Result<Vec<Exported>, Ice> {
    let index = unit.function_index();
    let in_cycle = crate::inline::cycles(unit, &index);
    let plain: FxHashMap<FnKey, &Function> = plain.iter().map(|f| (f.key, f)).collect();
    let mut out = Vec::new();
    for (n, f) in unit.functions.iter().enumerate() {
        let FnKey::Item(def) = f.key else { continue };
        let f = plain.get(&f.key).copied().unwrap_or(f);
        let size = crate::inline::cost(f);
        let inline = in_cycle.get(n) == Some(&false) && size <= budget.inline;
        let clone = clones && size <= budget.clone;
        if !(inline || clone) || !crate::inline::is_callee(unit, f) {
            continue;
        }
        let Some(mut body) = bundle(unit, def, f, &index, &plain)? else {
            continue;
        };
        body.inline = inline;
        if names_hidden(&body, defs) {
            // A clone may bring the `priv` functions it calls along.
            if !clone || !add_helpers(unit, &mut body, defs, budget, &index, &plain)? {
                continue;
            }
            body.inline = false;
            if names_hidden(&body, defs) {
                continue;
            }
        }
        out.push(body);
    }
    Ok(out)
}

/// The bundle of `f`, the function of `def` in `unit`; `None` when it
/// names a function its unit does not define or that cannot travel.
fn bundle(
    unit: &MirModule,
    def: DefId,
    f: &Function,
    index: &FxHashMap<FnKey, usize>,
    plain: &FxHashMap<FnKey, &Function>,
) -> Result<Option<Exported>, Ice> {
    // The functions of the bundle, by index into the unit, found from the
    // function itself.
    let mut taken: Vec<usize> = Vec::new();
    let mut statics: Vec<StaticId> = Vec::new();
    let mut queue: Vec<FnKey> = Vec::new();
    let mut static_queue: Vec<StaticId> = Vec::new();
    names_of(f, &mut queue, &mut static_queue);
    loop {
        if let Some(id) = static_queue.pop() {
            if statics.contains(&id) {
                continue;
            }
            statics.push(id);
            let object = unit
                .static_object(id)
                .ok_or_else(|| Ice::new(format!("export: no static#{}", id.as_u32())))?;
            for field in &object.fields {
                const_names(field, &mut queue, &mut static_queue);
            }
            continue;
        }
        let Some(key) = queue.pop() else { break };
        match key {
            FnKey::Closure { .. }
            | FnKey::ValueShim(_)
            | FnKey::TupleDictFn { .. }
            | FnKey::DynForward { .. } => {}
            FnKey::Item(_) => continue,
            FnKey::Continuation { .. }
            | FnKey::Specialized { .. }
            | FnKey::ConstInit(_)
            | FnKey::Test(_)
            | FnKey::Entry(_)
            | FnKey::Trampoline(_) => return Ok(None),
        }
        let Some(&at) = index.get(&key) else {
            return Ok(None);
        };
        if taken.contains(&at) {
            continue;
        }
        taken.push(at);
        let g = unit
            .functions
            .get(at)
            .ok_or_else(|| Ice::new("export: a function index"))?;
        let g = plain.get(&g.key).copied().unwrap_or(g);
        names_of(g, &mut queue, &mut static_queue);
    }
    taken.sort_unstable();
    statics.sort_unstable();
    let renumber: FxHashMap<StaticId, StaticId> = statics
        .iter()
        .enumerate()
        .map(|(n, &id)| Ok((id, static_id(n)?)))
        .collect::<Result<_, Ice>>()?;
    let map_static = |id: StaticId| {
        renumber
            .get(&id)
            .copied()
            .ok_or_else(|| Ice::new("export: a static outside the bundle"))
    };
    let mut closures = Vec::new();
    let mut shared = Vec::new();
    for at in taken {
        let g = unit
            .functions
            .get(at)
            .ok_or_else(|| Ice::new("export: a function index"))?;
        let mut g = plain.get(&g.key).copied().unwrap_or(g).clone();
        map_function(&mut g, &mut |d| Ok(d), &mut |id| map_static(id))?;
        if matches!(g.key, FnKey::Closure { .. }) {
            closures.push(g);
        } else {
            shared.push(g);
        }
    }
    let mut function = f.clone();
    map_function(&mut function, &mut |d| Ok(d), &mut |id| map_static(id))?;
    let mut objects = Vec::with_capacity(statics.len());
    for (n, &id) in statics.iter().enumerate() {
        let mut object = unit
            .static_object(id)
            .ok_or_else(|| Ice::new("export: a static"))?
            .clone();
        object.id = static_id(n)?;
        map_static_object(&mut object, &mut |d| Ok(d), &mut |id| map_static(id))?;
        objects.push(object);
    }
    Ok(Some(Exported {
        def,
        function,
        closures,
        shared,
        statics: objects,
        source: unit.source.clone(),
        inline: true,
        helpers: Vec::new(),
    }))
}

fn static_id(n: usize) -> Result<StaticId, Ice> {
    u32::try_from(n)
        .map(StaticId::from_u32)
        .map_err(|_| Ice::new("export: too many statics"))
}

/// Adds the function keys and statics `f` names to the queues: its code
/// words, `Callee::Code` targets, function values (whose value shims
/// their static closures hold, D-453) and static objects.
fn names_of(f: &Function, keys: &mut Vec<FnKey>, statics: &mut Vec<StaticId>) {
    for b in &f.blocks {
        for s in &b.stmts {
            let (Stmt::Assign { value, .. } | Stmt::Eval(value)) = s else {
                continue;
            };
            if let Rvalue::Call {
                callee: Callee::Code(key),
                ..
            } = value
            {
                keys.push(*key);
            }
            for op in value.operands() {
                if let Operand::Const(c) = op {
                    const_names(&c, keys, statics);
                }
            }
        }
        if let Terminator::TailCall {
            callee: Callee::Code(key),
            ..
        } = &b.term
        {
            keys.push(*key);
        }
        for op in term_operands(&b.term) {
            if let Operand::Const(c) = op {
                const_names(c, keys, statics);
            }
        }
    }
}

fn const_names(c: &Const, keys: &mut Vec<FnKey>, statics: &mut Vec<StaticId>) {
    match c {
        Const::Code(key) => keys.push(*key),
        Const::FnValue(def) => keys.push(FnKey::ValueShim(*def)),
        Const::Static(id) => statics.push(*id),
        Const::Int(_)
        | Const::Float(_)
        | Const::Bool(_)
        | Const::Sized { .. }
        | Const::Str(_)
        | Const::Nil
        | Const::StaticCtor(_)
        | Const::EmptyList
        | Const::BuiltinDesc(_)
        | Const::Ctx
        | Const::ErrNil => {}
    }
}

/// The operands a terminator reads.
fn term_operands(term: &Terminator) -> Vec<&Operand> {
    match term {
        Terminator::Return(op)
        | Terminator::Branch { cond: op, .. }
        | Terminator::Switch { on: op, .. } => vec![op],
        Terminator::TailCall { args, .. } | Terminator::Goto { args, .. } => args.iter().collect(),
        Terminator::Panic { message, .. } => message.iter().collect(),
        Terminator::Unreachable | Terminator::Resume => Vec::new(),
    }
}

/// Every def `body` names, sorted, without duplicates.
fn defs_of(body: &Exported) -> Result<Vec<DefId>, Ice> {
    let mut out: Vec<DefId> = vec![body.def];
    let mut record = |d: DefId| {
        out.push(d);
        Ok(d)
    };
    let mut same = |id: StaticId| Ok(id);
    let mut probe = body.clone();
    for f in std::iter::once(&mut probe.function)
        .chain(&mut probe.closures)
        .chain(&mut probe.shared)
        .chain(&mut probe.helpers)
    {
        map_function(f, &mut record, &mut same)?;
    }
    for s in &mut probe.statics {
        map_static_object(s, &mut record, &mut same)?;
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// How many times the clone budget a body with its helpers may cost: the
/// helpers are copied as they are and specialize nothing, so they count
/// for the unit's code size, not for the clone budget.
const HELPERS_SHARE: usize = 4;

/// Gives `body`, a clone's export, the `priv` functions of `unit` it calls,
/// directly or through each other, as helpers ([`Exported::helpers`]):
/// each a callee the inliner could copy, cycles aside, that names no
/// closure, no static and no shared function, together within
/// [`HELPERS_SHARE`] times the clone budget. Their calls become calls by key. Returns whether every
/// such function could come along.
fn add_helpers(
    unit: &MirModule,
    body: &mut Exported,
    defs: &Defs<'_>,
    budget: &InlineBudget,
    index: &FxHashMap<FnKey, usize>,
    plain: &FxHashMap<FnKey, &Function>,
) -> Result<bool, Ice> {
    let mut helpers: Vec<(DefId, Function)> = Vec::new();
    let mut total = crate::inline::cost(&body.function);
    loop {
        let mut wanted: Vec<DefId> = Vec::new();
        for f in std::iter::once(&body.function)
            .chain(&body.closures)
            .chain(helpers.iter().map(|(_, h)| h))
        {
            for d in direct_callees(f) {
                if !helpers.iter().any(|(h, _)| *h == d) && is_hidden(d, defs) {
                    wanted.push(d);
                }
            }
        }
        wanted.sort_unstable();
        wanted.dedup();
        if wanted.is_empty() {
            break;
        }
        for d in wanted {
            let Some(&at) = index.get(&FnKey::Item(d)) else {
                return Ok(false);
            };
            let g = unit
                .functions
                .get(at)
                .ok_or_else(|| Ice::new("export: a function index"))?;
            let g = plain.get(&g.key).copied().unwrap_or(g);
            let mut keys = Vec::new();
            let mut statics = Vec::new();
            names_of(g, &mut keys, &mut statics);
            if !keys.is_empty() || !statics.is_empty() || !crate::inline::is_callee(unit, g) {
                return Ok(false);
            }
            total = total.saturating_add(crate::inline::cost(g));
            if total > budget.clone.saturating_mul(HELPERS_SHARE) {
                return Ok(false);
            }
            helpers.push((d, g.clone()));
        }
    }
    let key_of = |d: DefId| -> Option<FnKey> {
        let n = helpers.iter().position(|(h, _)| *h == d)?;
        Some(FnKey::Specialized {
            owner: body.def,
            index: u32::try_from(n).ok()?,
            origin: body.def,
        })
    };
    let mut out = Vec::with_capacity(helpers.len());
    for (d, mut h) in helpers.iter().cloned() {
        h.key = key_of(d).ok_or_else(|| Ice::new("export: a helper"))?;
        h.sites.clear();
        out.push(h);
    }
    for f in std::iter::once(&mut body.function)
        .chain(&mut body.closures)
        .chain(&mut out)
    {
        call_by_key(f, &key_of);
    }
    body.helpers = out;
    Ok(true)
}

/// The direct callees of `f`, in order, with duplicates.
fn direct_callees(f: &Function) -> Vec<DefId> {
    let mut out = Vec::new();
    for b in &f.blocks {
        for s in &b.stmts {
            if let Stmt::Assign {
                value:
                    Rvalue::Call {
                        callee: Callee::Direct(d) | Callee::Borrowing(d, _),
                        ..
                    },
                ..
            } = s
            {
                out.push(*d);
            }
        }
        if let Terminator::TailCall {
            callee: Callee::Direct(d) | Callee::Borrowing(d, _),
            ..
        } = &b.term
        {
            out.push(*d);
        }
    }
    out
}

/// Each direct call of `f` of a function `key_of` keys becomes a call of
/// that key.
fn call_by_key(f: &mut Function, key_of: &dyn Fn(DefId) -> Option<FnKey>) {
    let retarget = |c: &mut Callee| {
        if let Callee::Direct(d) | Callee::Borrowing(d, _) = c
            && let Some(key) = key_of(*d)
        {
            *c = Callee::Code(key);
        }
    };
    for b in &mut f.blocks {
        for s in &mut b.stmts {
            if let Stmt::Assign {
                value: Rvalue::Call { callee, .. },
                ..
            } = s
            {
                retarget(callee);
            }
        }
        if let Terminator::TailCall { callee, .. } = &mut b.term {
            retarget(callee);
        }
    }
}

/// Whether `body` names a def that a unit of another module cannot see:
/// a `priv` item, or a constructor or interface function of a `priv`
/// parent (module docs); or one `defs` cannot read.
fn names_hidden(body: &Exported, defs: &Defs<'_>) -> bool {
    let Ok(all) = defs_of(body) else {
        return true;
    };
    all.into_iter().any(|def| is_hidden(def, defs))
}

/// Whether a unit of another module cannot see `def`: a `priv` item, a
/// test, or a constructor or interface function of a `priv` parent; or
/// one `defs` cannot read.
fn is_hidden(def: DefId, defs: &Defs<'_>) -> bool {
    if def.module == ModuleId::BUILTIN {
        return false;
    }
    let Ok(item) = defs.item(def) else {
        return true;
    };
    let parent_hidden = item
        .parent
        .is_some_and(|p| match defs.item(DefId::new(def.module, p)) {
            Ok(parent) => parent.vis == Vis::Priv,
            Err(_) => true,
        });
    item.vis == Vis::Priv || item.kind == DefKind::Test || parent_hidden
}

/// An [`Exported`] whose defs are named by module path and item, so a
/// build with other defs can read it ([`PortableBody::import`], module
/// docs).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PortableBody {
    /// The body in the defs of the front end that exported it.
    body: Exported,
    /// Every def the body names, sorted, with its module path (empty for
    /// the builtin module) and item, as [`crate::borrow`] names
    /// functions (D-969).
    names: Vec<(DefId, Box<str>, Box<str>)>,
}

impl PortableBody {
    /// `body` with its defs named through `defs`, the interfaces of the
    /// front end it was lowered in, in the form its bytes keep: its locals
    /// lose their names and types, its functions their spans and sites
    /// (module docs).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `defs` cannot name a def the body names.
    pub fn of(mut body: Exported, defs: &Defs<'_>) -> Result<Self, Ice> {
        bytes::portable_form(&mut body);
        let mut names = Vec::new();
        for def in defs_of(&body)? {
            let (module, item) = crate::borrow::def_text(def, defs)?;
            names.push((def, module, item));
        }
        Ok(Self { body, names })
    }

    /// The module path and item of the body's function.
    #[must_use]
    pub fn name(&self) -> Option<(&str, &str)> {
        self.names
            .binary_search_by_key(&self.body.def, |&(d, _, _)| d)
            .ok()
            .and_then(|at| self.names.get(at))
            .map(|(_, module, item)| (&**module, &**item))
    }

    /// The body, in the defs of the front end that exported it.
    #[must_use]
    pub const fn body(&self) -> &Exported {
        &self.body
    }

    /// The module path and item of `def`, a def the body names.
    #[must_use]
    pub fn name_of(&self, def: DefId) -> Option<(&str, &str)> {
        self.names
            .binary_search_by_key(&def, |&(d, _, _)| d)
            .ok()
            .and_then(|at| self.names.get(at))
            .map(|(_, module, item)| (&**module, &**item))
    }

    /// The body's cost ([`crate::inline::cost`]).
    #[must_use]
    pub fn cost(&self) -> usize {
        crate::inline::cost(&self.body.function)
    }

    /// The body in the defs of the build whose interfaces `defs` are:
    /// each name found there ([`crate::borrow::find_defs`]); `None` when
    /// one names nothing the build sees. The builtin module's defs are the
    /// same in every build. The locals lose their names and types (module
    /// docs).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if an interface's names cannot be read.
    pub fn import(&self, defs: &Defs<'_>) -> Result<Option<Exported>, Ice> {
        let wanted: Vec<(&str, &str)> = self
            .names
            .iter()
            .filter(|(d, _, _)| d.module != ModuleId::BUILTIN)
            .map(|(_, m, i)| (&**m, &**i))
            .collect();
        let found = crate::borrow::find_defs(&wanted, defs)?;
        let mut map: FxHashMap<DefId, DefId> = FxHashMap::default();
        let mut found = found.into_iter();
        for (d, _, _) in &self.names {
            if d.module == ModuleId::BUILTIN {
                map.insert(*d, *d);
                continue;
            }
            match found.next().flatten() {
                Some(theirs) => {
                    map.insert(*d, theirs);
                }
                None => return Ok(None),
            }
        }
        let mut lookup = |d: DefId| {
            map.get(&d)
                .copied()
                .ok_or_else(|| Ice::new(format!("a portable body does not name {d:?}")))
        };
        let mut same = |id: StaticId| Ok(id);
        let mut body = self.body.clone();
        body.def = lookup(body.def)?;
        for f in std::iter::once(&mut body.function)
            .chain(&mut body.closures)
            .chain(&mut body.shared)
            .chain(&mut body.helpers)
        {
            map_function(f, &mut lookup, &mut same)?;
            for decl in &mut f.locals {
                decl.name = None;
                decl.ty = None;
            }
            f.sites.clear();
        }
        for s in &mut body.statics {
            map_static_object(s, &mut lookup, &mut same)?;
        }
        Ok(Some(body))
    }
}

/// Portable bodies by the module path and item of their function
/// ([`PortableBody::name`]): std's exports, as a release build reads
/// them.
pub type PortableBodies = BTreeMap<(Box<str>, Box<str>), PortableBody>;

/// Where a build's inliner finds portable bodies by the module path and
/// item of their function ([`BuildBodies`]): in memory
/// ([`PortableBodies`]) or in their bytes, each decoded when it is asked
/// for ([`EncodedBodies`], design T15). Both give the same bodies.
pub trait PortableSource: Sync {
    /// Whether the source holds no body.
    fn is_empty(&self) -> bool;

    /// The body of `name`'s function imported through `defs`
    /// ([`PortableBody::import`]); `None` when there is none or it does
    /// not import.
    fn import(&self, name: &(Box<str>, Box<str>), defs: &Defs<'_>) -> Option<Exported>;
}

impl PortableSource for PortableBodies {
    fn is_empty(&self) -> bool {
        Self::is_empty(self)
    }

    fn import(&self, name: &(Box<str>, Box<str>), defs: &Defs<'_>) -> Option<Exported> {
        self.get(name)?.import(defs).ok().flatten()
    }
}

/// The exports of `units` ([`exports`]) as portable bodies, named
/// through `defs`, the interfaces of the front end that lowered them.
///
/// # Errors
///
/// An [`Ice`] for malformed MIR or a def `defs` cannot name.
pub fn portable_exports(
    units: &[MirModule],
    defs: &Defs<'_>,
    budget: &InlineBudget,
    clones: bool,
) -> Result<PortableBodies, Ice> {
    let mut all = Vec::new();
    for unit in units {
        all.extend(exports(unit, defs, budget, &[], clones)?);
    }
    portable_of(all, defs)
}

/// `exports` as portable bodies by name, named through `defs`, the
/// interfaces of the front end that lowered them.
///
/// # Errors
///
/// An [`Ice`] for a def `defs` cannot name.
pub fn portable_of(exports: Vec<Exported>, defs: &Defs<'_>) -> Result<PortableBodies, Ice> {
    let mut out = PortableBodies::new();
    for body in exports {
        let portable = PortableBody::of(body, defs)?;
        if let Some((module, item)) = portable.name() {
            let key = (Box::from(module), Box::from(item));
            out.insert(key, portable);
        }
    }
    Ok(out)
}

/// The bodies one build's inliner reads ([`crate::inline::Bodies`]): the
/// exports of its own units that are optimized already, by def, and
/// portable bodies (std's) by name, each imported through the build's
/// interfaces `defs` the first time a unit asks for it. A body that does
/// not import is not inlined.
pub struct BuildBodies<'d, 'a> {
    own: BTreeMap<DefId, Arc<Exported>>,
    portable: &'d dyn PortableSource,
    defs: &'d Defs<'a>,
    /// The constants whose reads the clean-up folds
    /// ([`crate::inline::Bodies::consts`]).
    consts: &'d crate::fold_consts::ConstValues,
    /// The portable bodies imported so far, by def: an import is a function
    /// of the body and the interfaces, so the order of the requests cannot
    /// change a result.
    imported: Mutex<BTreeMap<DefId, Option<Arc<Exported>>>>,
}

impl std::fmt::Debug for BuildBodies<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildBodies")
            .field("own", &self.own.len())
            .field("portable", &!self.portable.is_empty())
            .finish_non_exhaustive()
    }
}

impl<'d, 'a> BuildBodies<'d, 'a> {
    /// No exports of the build yet, `portable`'s bodies, and the values
    /// `consts` of the constants the build folds.
    #[must_use]
    pub fn new(
        portable: &'d dyn PortableSource,
        defs: &'d Defs<'a>,
        consts: &'d crate::fold_consts::ConstValues,
    ) -> Self {
        Self {
            own: BTreeMap::new(),
            portable,
            defs,
            consts,
            imported: Mutex::new(BTreeMap::new()),
        }
    }

    /// Adds exports of the build's units.
    pub fn add(&mut self, exports: Vec<Exported>) {
        for body in exports {
            self.own.insert(body.def, Arc::new(body));
        }
    }
}

impl crate::inline::Bodies for BuildBodies<'_, '_> {
    fn body(&self, def: DefId) -> Option<Arc<Exported>> {
        if let Some(body) = self.own.get(&def) {
            return Some(Arc::clone(body));
        }
        if self.portable.is_empty() {
            return None;
        }
        let mut imported = self.imported.lock().ok()?;
        if let Some(found) = imported.get(&def) {
            return found.clone();
        }
        let found = crate::borrow::def_text(def, self.defs)
            .ok()
            .and_then(|key| self.portable.import(&key, self.defs))
            .map(Arc::new);
        imported.insert(def, found.clone());
        found
    }

    fn consts(&self) -> &crate::fold_consts::ConstValues {
        self.consts
    }
}

/// One layer's work for [`optimize_build`]: each unit of the layer
/// optimized, giving its exports.
pub type LayerStep<'s> = dyn Fn(&mut MirModule) -> Result<Vec<Exported>, Ice> + Sync + 's;

/// How [`optimize_build`] runs a [`LayerStep`] over a layer's units: the
/// results in the order of the units.
pub type LayerRun<'r> =
    dyn Fn(&mut [&mut MirModule], &LayerStep<'_>) -> Vec<Result<Vec<Exported>, Ice>> + 'r;

/// The release inliner over the units of one build that `selected`
/// picks, which inline each other's bodies and `portable`'s (design T8,
/// T9): layer by layer ([`crate::inline::layers`]), each unit inlined
/// ([`crate::inline::optimize_across`]), checked as lowered again and
/// exported ([`exports`]), so a later layer reads its bodies. The
/// clean-up folds the reads of the constants `consts` gives
/// ([`crate::fold_consts`]). `each` runs the step over a layer's units,
/// in any order or in parallel, and gives the results in the layer's
/// order; the first error in that order is returned, so the result does
/// not depend on threads (D-264).
///
/// # Errors
///
/// An [`Ice`] for malformed MIR, a def `defs` cannot see, or a unit that
/// is no longer valid lowered MIR.
#[expect(
    clippy::too_many_arguments,
    reason = "the build's units, its bodies, its constants and how to run a layer"
)]
pub fn optimize_build(
    units: &mut [MirModule],
    selected: &dyn Fn(&MirModule) -> bool,
    portable: &dyn PortableSource,
    defs: &Defs<'_>,
    budget: &InlineBudget,
    passes: crate::inline::Passes,
    consts: &crate::fold_consts::ConstValues,
    each: &LayerRun<'_>,
) -> Result<Vec<Exported>, Ice> {
    let layers = crate::inline::layers(units);
    let mut bodies = BuildBodies::new(portable, defs, consts);
    let mut all = Vec::new();
    for layer in layers {
        let mut refs: Vec<&mut MirModule> = units
            .iter_mut()
            .enumerate()
            .filter(|(n, unit)| layer.contains(n) && selected(unit))
            .map(|(_, unit)| unit)
            .collect();
        if refs.is_empty() {
            continue;
        }
        let shared = &bodies;
        let step = |unit: &mut MirModule| -> Result<Vec<Exported>, Ice> {
            let plain = crate::inline::optimize_across(unit, budget, passes, shared, defs)?;
            crate::check_lowered(unit)?;
            exports(unit, defs, budget, &plain, passes.spec)
        };
        let done = each(&mut refs, &step);
        let mut found = Vec::new();
        for result in done {
            found.extend(result?);
        }
        all.extend(found.iter().cloned());
        bodies.add(found);
    }
    Ok(all)
}

/// A rewrite of the defs a function or static names.
type MapDef<'m> = dyn FnMut(DefId) -> Result<DefId, Ice> + 'm;
/// A rewrite of the statics a function or static names.
type MapStatic<'m> = dyn FnMut(StaticId) -> Result<StaticId, Ice> + 'm;

/// Rewrites every def of `key`.
fn map_key(key: FnKey, defs: &mut MapDef<'_>) -> Result<FnKey, Ice> {
    Ok(match key {
        FnKey::Item(d) => FnKey::Item(defs(d)?),
        FnKey::ValueShim(d) => FnKey::ValueShim(defs(d)?),
        FnKey::Closure { owner, index } => FnKey::Closure {
            owner: defs(owner)?,
            index,
        },
        FnKey::ConstInit(d) => FnKey::ConstInit(defs(d)?),
        FnKey::Test(d) => FnKey::Test(defs(d)?),
        FnKey::Entry(kind) => FnKey::Entry(match kind {
            EntryKind::Main(d) => EntryKind::Main(defs(d)?),
            EntryKind::Test(d) => EntryKind::Test(defs(d)?),
            EntryKind::Const(d) => EntryKind::Const(defs(d)?),
            EntryKind::Repl(n) => EntryKind::Repl(n),
        }),
        FnKey::Trampoline(n) => FnKey::Trampoline(n),
        FnKey::TupleDictFn { iface_fn, arity } => FnKey::TupleDictFn {
            iface_fn: defs(iface_fn)?,
            arity,
        },
        FnKey::DynForward {
            iface_fn,
            dyn_iface,
        } => FnKey::DynForward {
            iface_fn: defs(iface_fn)?,
            dyn_iface: defs(dyn_iface)?,
        },
        FnKey::Continuation { owner, index } => FnKey::Continuation {
            owner: defs(owner)?,
            index,
        },
        FnKey::Specialized {
            owner,
            index,
            origin,
        } => FnKey::Specialized {
            owner: defs(owner)?,
            index,
            origin: defs(origin)?,
        },
    })
}

/// Rewrites the defs and statics of `c`.
fn map_const(c: &mut Const, defs: &mut MapDef<'_>, statics: &mut MapStatic<'_>) -> Result<(), Ice> {
    match c {
        Const::StaticCtor(d) | Const::FnValue(d) => *d = defs(*d)?,
        Const::Code(key) => *key = map_key(*key, defs)?,
        Const::Static(id) => *id = statics(*id)?,
        Const::Int(_)
        | Const::Float(_)
        | Const::Bool(_)
        | Const::Sized { .. }
        | Const::Str(_)
        | Const::Nil
        | Const::EmptyList
        | Const::BuiltinDesc(_)
        | Const::Ctx
        | Const::ErrNil => {}
    }
    Ok(())
}

fn map_operand(
    op: &mut Operand,
    defs: &mut MapDef<'_>,
    statics: &mut MapStatic<'_>,
) -> Result<(), Ice> {
    match op {
        Operand::Const(c) => map_const(c, defs, statics),
        Operand::Local(_) => Ok(()),
    }
}

fn map_operands(
    ops: &mut [Operand],
    defs: &mut MapDef<'_>,
    statics: &mut MapStatic<'_>,
) -> Result<(), Ice> {
    for op in ops {
        map_operand(op, defs, statics)?;
    }
    Ok(())
}

fn map_layout(layout: &mut LayoutRef, defs: &mut MapDef<'_>) -> Result<(), Ice> {
    match layout {
        LayoutRef::Ctor(d) => *d = defs(*d)?,
        LayoutRef::Tuple(_)
        | LayoutRef::Closure(_)
        | LayoutRef::Dict { .. }
        | LayoutRef::Dyn
        | LayoutRef::Desc { .. }
        | LayoutRef::DescNominal
        | LayoutRef::Builtin(_) => {}
    }
    Ok(())
}

fn map_callee(callee: &mut Callee, defs: &mut MapDef<'_>) -> Result<(), Ice> {
    match callee {
        Callee::Direct(d) | Callee::Borrowing(d, _) => *d = defs(*d)?,
        Callee::Code(key) => *key = map_key(*key, defs)?,
        Callee::Indirect(_) => {}
    }
    Ok(())
}

fn map_rvalue(
    value: &mut Rvalue,
    defs: &mut MapDef<'_>,
    statics: &mut MapStatic<'_>,
) -> Result<(), Ice> {
    match value {
        Rvalue::Use(op)
        | Rvalue::Box { value: op, .. }
        | Rvalue::Unbox { value: op, .. }
        | Rvalue::FloatNeg { value: op, .. }
        | Rvalue::Convert { value: op, .. }
        | Rvalue::FloatSqrt { value: op, .. }
        | Rvalue::Not(op) => map_operand(op, defs, statics)?,
        Rvalue::ReadTag { base, ty } => {
            map_operand(base, defs, statics)?;
            if let TagType::Adt(d) = ty {
                *d = defs(*d)?;
            }
        }
        Rvalue::Call { callee, args } => {
            map_callee(callee, defs)?;
            map_operands(args, defs, statics)?;
        }
        Rvalue::CallForeign { args, .. } | Rvalue::CallRuntime { args, .. } => {
            map_operands(args, defs, statics)?;
        }
        Rvalue::Alloc { layout, fields } | Rvalue::AllocReuse { layout, fields, .. } => {
            map_layout(layout, defs)?;
            map_operands(fields, defs, statics)?;
        }
        Rvalue::AllocPlaceholder { layout }
        | Rvalue::Field { layout, .. }
        | Rvalue::DropReuse { layout, .. } => map_layout(layout, defs)?,
        Rvalue::InitField { layout, value, .. } => {
            map_layout(layout, defs)?;
            map_operand(value, defs, statics)?;
        }
        Rvalue::LoadConst(d) => *d = defs(*d)?,
        Rvalue::Checked { lhs, rhs, .. }
        | Rvalue::Compare { lhs, rhs, .. }
        | Rvalue::FloatArith { lhs, rhs, .. }
        | Rvalue::Bitwise { lhs, rhs, .. }
        | Rvalue::Wrapping { lhs, rhs, .. } => {
            map_operand(lhs, defs, statics)?;
            map_operand(rhs, defs, statics)?;
        }
        Rvalue::ReadLayout { .. } => {}
    }
    Ok(())
}

/// Rewrites every def and static `f` names: its key, its callees,
/// constants, layouts and tag types.
fn map_function(
    f: &mut Function,
    defs: &mut MapDef<'_>,
    statics: &mut MapStatic<'_>,
) -> Result<(), Ice> {
    f.key = map_key(f.key, defs)?;
    for b in &mut f.blocks {
        for s in &mut b.stmts {
            match s {
                Stmt::Assign { value, .. } | Stmt::Eval(value) => {
                    map_rvalue(value, defs, statics)?;
                }
                Stmt::DropKeep { layout, .. } => map_layout(layout, defs)?,
                Stmt::Dup(_) | Stmt::Drop(_) | Stmt::ClearUnwind => {}
            }
        }
        match &mut b.term {
            Terminator::Return(op)
            | Terminator::Branch { cond: op, .. }
            | Terminator::Switch { on: op, .. } => map_operand(op, defs, statics)?,
            Terminator::TailCall { callee, args } => {
                map_callee(callee, defs)?;
                map_operands(args, defs, statics)?;
            }
            Terminator::Goto { args, .. } => map_operands(args, defs, statics)?,
            Terminator::Panic { message, .. } => {
                if let Some(m) = message {
                    map_operand(m, defs, statics)?;
                }
            }
            Terminator::Unreachable | Terminator::Resume => {}
        }
    }
    Ok(())
}

/// Rewrites every def and static `object` names.
fn map_static_object(
    object: &mut StaticObject,
    defs: &mut MapDef<'_>,
    statics: &mut MapStatic<'_>,
) -> Result<(), Ice> {
    map_layout(&mut object.layout, defs)?;
    let mut fields = std::mem::take(&mut object.fields).into_vec();
    for field in &mut fields {
        map_const(field, defs, statics)?;
    }
    object.fields = fields.into_boxed_slice();
    Ok(())
}

/// [`map_function`] for the importer in [`crate::inline`].
pub(crate) fn rewrite_function(
    f: &mut Function,
    keys: &mut dyn FnMut(FnKey) -> FnKey,
    statics: &mut MapStatic<'_>,
) -> Result<(), Ice> {
    // Only closure keys change in an import; a def stays a def.
    let mut same = |d: DefId| Ok(d);
    map_function(f, &mut same, statics)?;
    f.key = keys(f.key);
    rekey_function(f, keys);
    Ok(())
}

/// [`map_static_object`] for the importer in [`crate::inline`].
pub(crate) fn rewrite_static(
    object: &mut StaticObject,
    statics: &mut MapStatic<'_>,
) -> Result<(), Ice> {
    let mut same = |d: DefId| Ok(d);
    map_static_object(object, &mut same, statics)
}

/// Replaces every function key `f` names in a code word or a
/// `Callee::Code` by `keys`'s.
fn rekey_function(f: &mut Function, keys: &mut dyn FnMut(FnKey) -> FnKey) {
    let operand = |op: &mut Operand, keys: &mut dyn FnMut(FnKey) -> FnKey| {
        if let Operand::Const(Const::Code(key)) = op {
            *key = keys(*key);
        }
    };
    for b in &mut f.blocks {
        for s in &mut b.stmts {
            let (Stmt::Assign { value, .. } | Stmt::Eval(value)) = s else {
                continue;
            };
            if let Rvalue::Call {
                callee: Callee::Code(key),
                ..
            } = value
            {
                *key = keys(*key);
            }
            for_operands_mut(value, &mut |op| operand(op, keys));
        }
        match &mut b.term {
            Terminator::TailCall { callee, args } => {
                if let Callee::Code(key) = callee {
                    *key = keys(*key);
                }
                for a in args {
                    operand(a, keys);
                }
            }
            Terminator::Return(op)
            | Terminator::Branch { cond: op, .. }
            | Terminator::Switch { on: op, .. } => operand(op, keys),
            Terminator::Goto { args, .. } => {
                for a in args {
                    operand(a, keys);
                }
            }
            Terminator::Panic { message, .. } => {
                if let Some(m) = message {
                    operand(m, keys);
                }
            }
            Terminator::Unreachable | Terminator::Resume => {}
        }
    }
}

/// Calls `f` on every operand of `value`.
fn for_operands_mut(value: &mut Rvalue, f: &mut dyn FnMut(&mut Operand)) {
    match value {
        Rvalue::Use(op)
        | Rvalue::Box { value: op, .. }
        | Rvalue::Unbox { value: op, .. }
        | Rvalue::FloatNeg { value: op, .. }
        | Rvalue::Convert { value: op, .. }
        | Rvalue::FloatSqrt { value: op, .. }
        | Rvalue::Not(op)
        | Rvalue::ReadTag { base: op, .. }
        | Rvalue::InitField { value: op, .. } => f(op),
        Rvalue::Call { args, .. }
        | Rvalue::CallForeign { args, .. }
        | Rvalue::CallRuntime { args, .. }
        | Rvalue::Alloc { fields: args, .. }
        | Rvalue::AllocReuse { fields: args, .. } => args.iter_mut().for_each(f),
        Rvalue::Checked { lhs, rhs, .. }
        | Rvalue::Compare { lhs, rhs, .. }
        | Rvalue::FloatArith { lhs, rhs, .. }
        | Rvalue::Bitwise { lhs, rhs, .. }
        | Rvalue::Wrapping { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        Rvalue::AllocPlaceholder { .. }
        | Rvalue::Field { .. }
        | Rvalue::ReadLayout { .. }
        | Rvalue::LoadConst(_)
        | Rvalue::DropReuse { .. } => {}
    }
}

mod bytes;

pub use bytes::{
    EncodedBodies, Folded, PLACEHOLDER_SPAN, decode_bodies, decode_folded, encode_bodies,
    encode_folded,
};
