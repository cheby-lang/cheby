//! THIR to MIR (D2).
//!
//! [`lower_fn`] lowers one item: it reads the item's THIR, the module's
//! own interface and its dependencies' interfaces (D-246), and returns
//! the item's function followed by its closures, with function-local
//! strings. [`lower_items`] lowers the selected items in `DefIndex` order
//! with one [`Pending`] per unit, which collects what the unit needs
//! besides its items (value shims, static objects, trampoline arities),
//! and computes the unit's callee declarations, layouts, foreign layouts
//! and names; [`lower_module`] selects every item. [`lower_tests`] and
//! [`lower_entries`] produce the other two unit kinds.
//!
//! Files (plan §4.2), each owning one part of [`body::BodyCx`]:
//! `body.rs` (state, helpers, statements), `expr.rs` (dispatch, blocks,
//! direct calls), `coerce.rs` (boxing, D6), `scalar.rs` (literals and
//! operators), `panic.rs`, `pat.rs` (patterns), `closure.rs` (closures,
//! function values, shims, trampolines), `data.rs` (constructors,
//! records, fields, tuples), `list.rs`, `interp.rs`, `dict.rs`
//! (dictionaries and `dyn`), `consts.rs`, `external.rs` and `entry.rs`.
//!
//! Every THIR construct lowers (D-554): lowering reports no diagnostics,
//! and its only error is an [`Ice`] for input inconsistent with the
//! interfaces. THIR enums are `#[non_exhaustive]`, so the dispatch keeps a
//! fallback arm, and a kind added to THIR later is an [`Ice`] until it is
//! lowered. E0038 remains only for imports of standard-library modules
//! that are not shipped yet, which the resolver reports (D-480).
//! `cheby run` lowers only the items reachable from its entry
//! ([`crate::reach`], D-510).

mod body;
mod closure;
mod coerce;
mod consts;
mod data;
mod dict;
mod entry;
mod expr;
mod external;
mod interp;
mod intrinsic;
#[cfg(test)]
mod intrinsic_tests;
mod list;
mod panic;
mod pat;
mod scalar;
#[cfg(test)]
mod scalar_tests;
mod split;
#[cfg(test)]
mod split_tests;

use std::collections::{BTreeMap, BTreeSet};

use cheby_diag::{SourceMap, Span};
use cheby_iface::{DefId, DisplayCx, Generics, Ice, ModuleId, Ty, display};
use cheby_types::thir;

pub use entry::{EntryRequest, lower_entries};
pub use intrinsic::{Intrinsic, intrinsic_symbols};
pub use split::{SPLIT_SIZE, STATEMENT_BUDGET, TUPLE_INIT_THRESHOLD};

use crate::abi::abi_sig;
use crate::defs::Defs;
use crate::ir::{
    Callee, CalleeDecl, Const, EntryKind, FnKey, Function, LayoutRef, MirModule, Operand, Rvalue,
    StaticId, StaticObject, Stmt, TagType, Terminator, UnitKind,
};
use crate::layout::{LayoutDesc, ctor_layout, module_layouts, type_layouts};

/// What lowering reads besides THIR.
#[derive(Clone, Copy, Debug)]
pub struct LowerCx<'a> {
    pub defs: Defs<'a>,
    /// To render locations as `path:line:col` (D-421), so MIR and its dump
    /// are self-contained.
    pub sources: &'a SourceMap,
}

impl LowerCx<'_> {
    /// `path:line:col` of the start of `span`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the span's file is not in the source map.
    pub fn location(&self, span: Span) -> Result<String, Ice> {
        self.sources
            .location(span)
            .map(|loc| loc.to_string())
            .ok_or_else(|| Ice::new(format!("no source file for span {span:?}")))
    }

    /// `ty` as users write it (D-463), for diagnostics about the module
    /// being lowered.
    #[must_use]
    pub fn show(&self, ty: Ty, generics: Option<&Generics>) -> String {
        display(
            ty,
            &DisplayCx {
                globals: self.defs.globals,
                modules: self.defs.modules,
                own: self.defs.own,
                deps: self.defs.deps,
                generics,
            },
        )
    }
}

/// What a unit needs besides its items' functions, collected while its
/// items are lowered in `DefIndex` order, so ids and order never depend
/// on scheduling (D-264): closures and local functions, the value shims
/// of top-level functions used as values, static objects and the
/// trampoline arities that kernels calling back into Cheby need.
#[derive(Debug, Default)]
pub(crate) struct Pending {
    /// Functions made while lowering the current item, in creation
    /// order; [`lower_items`] puts them right after the item.
    functions: Vec<Function>,
    /// The next closure index per owner.
    closures: BTreeMap<DefId, u32>,
    /// The next continuation index per owner (D-780).
    continuations: BTreeMap<DefId, u32>,
    /// Where large literals are cut, when a test asks for another size
    /// than [`split::SPLIT_SIZE`].
    split: Option<usize>,
    /// Top-level functions whose [`FnKey::ValueShim`] the unit defines.
    shims: BTreeSet<DefId>,
    /// The [`FnKey::TupleDictFn`] codes the unit defines, by interface
    /// function and arity (D-167).
    tuple_fns: BTreeSet<(DefId, u32)>,
    /// The [`FnKey::DynForward`] codes the unit defines, by interface
    /// function and the interface of the `dyn` receiver (§8.6).
    dyn_forwards: BTreeSet<(DefId, DefId)>,
    /// In [`StaticId`] order.
    statics: Vec<StaticObject>,
    /// Arities of the trampolines the program needs (plan §4.3.4).
    trampolines: BTreeSet<u32>,
}

impl Pending {
    /// The size at which a literal's piece is cut (D-780).
    pub(crate) fn split_size(&self) -> usize {
        self.split.unwrap_or(split::SPLIT_SIZE)
    }

    /// Adds a function made while lowering the current item, such as a
    /// closure body.
    pub(crate) fn add_function(&mut self, f: Function) {
        self.functions.push(f);
    }

    /// A new closure key of `owner`, numbered in lowering order.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `owner` has too many closures.
    pub(crate) fn closure_key(&mut self, owner: DefId) -> Result<FnKey, Ice> {
        let next = self.closures.entry(owner).or_insert(0);
        let index = *next;
        *next = next
            .checked_add(1)
            .ok_or_else(|| Ice::new("too many closures"))?;
        Ok(FnKey::Closure { owner, index })
    }

    /// A new continuation key of `owner`, numbered in creation order
    /// (D-780, D-811).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `owner` has too many continuations.
    pub(crate) fn continuation_key(&mut self, owner: DefId) -> Result<FnKey, Ice> {
        let next = self.continuations.entry(owner).or_insert(0);
        let index = *next;
        *next = next
            .checked_add(1)
            .ok_or_else(|| Ice::new("too many continuations"))?;
        Ok(FnKey::Continuation { owner, index })
    }

    /// Asks for the [`FnKey::ValueShim`] of `def` in this unit (D-453).
    pub(crate) fn request_shim(&mut self, def: DefId) {
        self.shims.insert(def);
    }

    /// Asks for the [`FnKey::TupleDictFn`] of `iface_fn` for tuples of
    /// `arity` elements in this unit (D-167).
    pub(crate) fn request_tuple_fn(&mut self, iface_fn: DefId, arity: u32) {
        self.tuple_fns.insert((iface_fn, arity));
    }

    /// Asks for the [`FnKey::DynForward`] of `iface_fn` for receivers of
    /// type `dyn dyn_iface` in this unit (§8.6).
    pub(crate) fn request_dyn_forward(&mut self, iface_fn: DefId, dyn_iface: DefId) {
        self.dyn_forwards.insert((iface_fn, dyn_iface));
    }

    /// Adds a static object and returns its id (its `id` field is
    /// ignored). A static whose kind, module and name the unit already has
    /// is the same object (names are built from paths and names, D-523),
    /// and its first id is returned.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a static of the same symbol differs, or there are
    /// too many statics.
    pub(crate) fn add_static(&mut self, mut object: StaticObject) -> Result<StaticId, Ice> {
        if let Some(existing) = self
            .statics
            .iter()
            .find(|s| s.kind == object.kind && s.module == object.module && s.name == object.name)
        {
            let same = existing.layout == object.layout
                && existing.fields == object.fields
                && existing.strings == object.strings;
            return if same {
                Ok(existing.id)
            } else {
                Err(Ice::new(format!(
                    "two different statics named {:?}",
                    object.name
                )))
            };
        }
        let id = StaticId::from_u32(
            u32::try_from(self.statics.len()).map_err(|_| Ice::new("too many statics"))?,
        );
        object.id = id;
        self.statics.push(object);
        Ok(id)
    }

    /// Records that a kernel may call a closure of `arity` arguments back
    /// through a trampoline (plan §4.3.4): the unit then defines
    /// `FnKey::Trampoline(arity)`, which the JIT defines once (D-561).
    pub(crate) fn need_trampoline(&mut self, arity: u32) {
        self.trampolines.insert(arity);
    }

    /// The trampoline arities recorded so far.
    pub(crate) const fn trampolines(&self) -> &BTreeSet<u32> {
        &self.trampolines
    }
}

/// Lowers every function and constant initializer of a module into its
/// code unit. The result has no reference counts yet
/// ([`crate::count_and_check`]).
///
/// # Errors
///
/// An [`Ice`] on input inconsistent with the interfaces.
pub fn lower_module(module: &thir::Module, cx: &LowerCx<'_>) -> Result<MirModule, Ice> {
    lower_items(module, &|_| true, cx)
}

/// Lowers the functions and constant initializers of `module` that
/// `wanted` selects, in `DefIndex` order, with every constructor layout
/// of the module, into its code unit. `cheby run` selects the items
/// reachable from its entry ([`crate::reach`]).
///
/// # Errors
///
/// As [`lower_module`].
pub fn lower_items(
    module: &thir::Module,
    wanted: &dyn Fn(DefId) -> bool,
    cx: &LowerCx<'_>,
) -> Result<MirModule, Ice> {
    lower_items_with(module, wanted, cx, Pending::default())
}

/// [`lower_items`] with large literals cut at `split` (D-780), or at
/// [`SPLIT_SIZE`] for `None`: for tests.
#[cfg(test)]
pub(crate) fn lower_items_split(
    module: &thir::Module,
    wanted: &dyn Fn(DefId) -> bool,
    cx: &LowerCx<'_>,
    split: Option<usize>,
) -> Result<MirModule, Ice> {
    let pending = Pending {
        split,
        ..Pending::default()
    };
    lower_items_with(module, wanted, cx, pending)
}

/// [`lower_tests`] with large literals cut at `split`, as
/// [`lower_items_split`].
#[cfg(test)]
pub(crate) fn lower_tests_split(
    module: &thir::Module,
    cx: &LowerCx<'_>,
    split: Option<usize>,
) -> Result<MirModule, Ice> {
    let pending = Pending {
        split,
        ..Pending::default()
    };
    lower_tests_with(module, cx, pending)
}

fn lower_items_with(
    module: &thir::Module,
    wanted: &dyn Fn(DefId) -> bool,
    cx: &LowerCx<'_>,
    mut pending: Pending,
) -> Result<MirModule, Ice> {
    let mut fns: Vec<&thir::FnDef> = module.fns.iter().filter(|f| wanted(f.def)).collect();
    fns.sort_by_key(|f| f.def);
    let mut consts: Vec<&thir::ConstDef> = module.consts.iter().filter(|c| wanted(c.def)).collect();
    consts.sort_by_key(|c| c.def);
    let mut functions = Vec::new();
    // Items in `DefIndex` order: merge functions and constants.
    let mut fi = fns.into_iter().peekable();
    let mut ci = consts.into_iter().peekable();
    loop {
        let take_fn = match (fi.peek(), ci.peek()) {
            (Some(f), Some(c)) => f.def < c.def,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        let lowered = if take_fn {
            let Some(f) = fi.next() else { break };
            lower_item(f, cx, &mut pending)?
        } else {
            let Some(c) = ci.next() else { break };
            body::lower_body(
                FnKey::ConstInit(c.def),
                c.span,
                &c.body,
                0,
                cx,
                &mut pending,
            )?
        };
        // Each item is followed by the closures it made.
        functions.push(lowered);
        functions.append(&mut pending.functions);
    }
    let layouts = module_layouts(module.module, &cx.defs)?;
    finish_unit(
        UnitKind::Code(module.module),
        functions,
        layouts,
        pending,
        cx,
    )
}

/// Lowers one module's `test` blocks into its tests unit (D-265).
///
/// # Errors
///
/// As [`lower_module`].
pub fn lower_tests(module: &thir::Module, cx: &LowerCx<'_>) -> Result<MirModule, Ice> {
    lower_tests_with(module, cx, Pending::default())
}

fn lower_tests_with(
    module: &thir::Module,
    cx: &LowerCx<'_>,
    mut pending: Pending,
) -> Result<MirModule, Ice> {
    let mut tests: Vec<&thir::TestDef> = module.tests.iter().collect();
    tests.sort_by_key(|t| t.def);
    let mut functions = Vec::new();
    for t in tests {
        let lowered = body::lower_body(FnKey::Test(t.def), t.span, &t.body, 0, cx, &mut pending)?;
        functions.push(lowered);
        functions.append(&mut pending.functions);
    }
    finish_unit(
        UnitKind::Tests(module.module),
        functions,
        Vec::new(),
        pending,
        cx,
    )
}

/// Lowers one top-level function: its body, or the Cheby-ABI wrapper of
/// its `@external(native, …)` (D-455), followed by the closures it made.
/// What the function needs at the unit level (value shims, statics,
/// trampolines) is kept only by [`lower_items`].
///
/// # Errors
///
/// As [`lower_module`].
pub fn lower_fn(f: &thir::FnDef, cx: &LowerCx<'_>) -> Result<Vec<Function>, Ice> {
    let mut pending = Pending::default();
    let item = lower_item(f, cx, &mut pending)?;
    let mut out = vec![item];
    out.append(&mut pending.functions);
    Ok(out)
}

/// The function of one top-level function; closures go to `pending`.
fn lower_item(f: &thir::FnDef, cx: &LowerCx<'_>, pending: &mut Pending) -> Result<Function, Ice> {
    let sig = cx.defs.fn_sig(f.def)?;
    let dicts = sig
        .generics
        .params
        .iter()
        .map(|p| p.bounds.len())
        .sum::<usize>();
    match (&f.native_external, &f.body) {
        // With a body too, the body is for the other targets (D-207, D-510).
        (Some(ext), _) => {
            if ext.context {
                // A `@std_context` kernel may start a fiber, whose base
                // frame calls its function through `cb$tramp$0` (D-692,
                // D-526), so the unit that binds one defines it (D-561).
                pending.need_trampoline(0);
            }
            external::lower_wrapper(f, ext, sig, cx)
        }
        (None, Some(body)) => {
            body::lower_body(FnKey::Item(f.def), f.span, body, dicts, cx, pending)
        }
        // Only a JS external: the function does not exist on native
        // (§12.5). Only a constant's initializer can reach it here.
        (None, None) => external::lower_js_only(f, sig, cx),
    }
}

/// What a unit's functions and statics refer to.
#[derive(Default)]
struct Mentions {
    /// [`Callee::Direct`] targets.
    direct: BTreeSet<DefId>,
    /// Every def that needs a name.
    defs: BTreeSet<DefId>,
    /// Constructors whose layouts the code needs: allocated, read, or
    /// used as user static objects.
    ctors: BTreeSet<DefId>,
    /// Types whose tags the code reads.
    tag_types: BTreeSet<DefId>,
}

/// Builds a unit from its items' functions and what they need: the value
/// shims `pending` asks for (by `DefId`, after every item) and its
/// statics, then callee declarations, foreign layouts, names and path.
fn finish_unit(
    unit: UnitKind,
    mut functions: Vec<Function>,
    layouts: Vec<LayoutDesc>,
    mut pending: Pending,
    cx: &LowerCx<'_>,
) -> Result<MirModule, Ice> {
    if !pending.functions.is_empty() {
        return Err(Ice::new("pending functions outside of an item"));
    }
    // The runtime calls a type's `@std_hooks` through their static
    // function values, which the type's own unit defines (D-482), when
    // the hooks are lowered (reachability adds them, D-510).
    for hooks in layouts.iter().filter_map(|l| l.hooks) {
        for def in hooks {
            if functions.iter().any(|f| f.key == FnKey::Item(def)) {
                pending.shims.insert(def);
            }
        }
    }
    for &def in &pending.shims {
        functions.push(closure::value_shim(def, cx)?);
    }
    for &(iface_fn, arity) in &pending.tuple_fns {
        functions.push(dict::tuple_fn(iface_fn, arity, cx)?);
    }
    for &(iface_fn, dyn_iface) in &pending.dyn_forwards {
        functions.push(dict::dyn_forward(iface_fn, dyn_iface, cx)?);
    }
    functions.extend(closure::trampolines(pending.trampolines())?);
    let statics = pending.statics;
    let mut m = Mentions::default();
    for f in &functions {
        collect_defs(f, &mut m);
    }
    for s in &statics {
        if let LayoutRef::Ctor(d) = s.layout {
            m.ctors.insert(d);
        }
        for c in &s.fields {
            const_defs(c, &mut m);
        }
    }
    let own: BTreeSet<LayoutRef> = layouts.iter().map(|l| l.layout).collect();
    let mut foreign: Vec<LayoutDesc> = Vec::new();
    for &ctor in &m.ctors {
        if !own.contains(&LayoutRef::Ctor(ctor)) {
            foreign.push(ctor_layout(ctor, &cx.defs)?);
        }
    }
    for &ty in &m.tag_types {
        foreign.extend(
            type_layouts(ty, &cx.defs)?
                .into_iter()
                .filter(|d| !own.contains(&d.layout)),
        );
        m.defs.insert(ty);
    }
    foreign.sort_by_key(|d| d.layout);
    foreign.dedup_by_key(|d| d.layout);
    let mut mentioned = std::mem::take(&mut m.defs);
    for l in layouts.iter().chain(&foreign) {
        if let LayoutRef::Ctor(d) = l.layout {
            mentioned.insert(d);
        }
        if let Some(hooks) = l.hooks {
            mentioned.extend(hooks);
        }
    }
    let direct = m.direct;
    let callees = direct
        .iter()
        .map(|&def| {
            Ok(CalleeDecl {
                def,
                sig: abi_sig(cx.defs.fn_sig(def)?, cx.defs.globals),
            })
        })
        .collect::<Result<Vec<_>, Ice>>()?;
    mentioned.extend(direct);
    let names = mentioned
        .into_iter()
        .map(|d| cx.defs.name(d))
        .collect::<Result<Vec<_>, Ice>>()?;
    let path = match unit {
        UnitKind::Code(m) | UnitKind::Tests(m) => cx.defs.modules.try_get(m)?.path.to_string(),
        UnitKind::Entries => String::new(),
    };
    Ok(MirModule {
        unit,
        path: path.into(),
        functions,
        callees,
        layouts,
        foreign_layouts: foreign,
        statics,
        names,
    })
}

/// The direct callees of `f`, and every other def, constructor layout and
/// tag type it mentions.
fn collect_defs(f: &Function, m: &mut Mentions) {
    key_defs(f.key, &mut m.defs);
    let callee = |c: &Callee, m: &mut Mentions| match c {
        // Lowering writes only `Direct`; the inference adds `Borrowing`.
        Callee::Direct(d) | Callee::Borrowing(d, _) => {
            m.direct.insert(*d);
        }
        Callee::Code(key) => key_defs(*key, &mut m.defs),
        Callee::Indirect(_) => {}
    };
    let operand = |op: &Operand, m: &mut Mentions| {
        if let Operand::Const(c) = op {
            const_defs(c, m);
        }
    };
    for block in &f.blocks {
        for stmt in &block.stmts {
            let (Stmt::Assign { value, .. } | Stmt::Eval(value)) = stmt else {
                continue;
            };
            for op in value.operands() {
                operand(&op, m);
            }
            match value {
                Rvalue::Call { callee: c, .. } => callee(c, m),
                Rvalue::Alloc { layout, .. }
                | Rvalue::AllocPlaceholder { layout }
                | Rvalue::InitField { layout, .. }
                | Rvalue::Field { layout, .. } => {
                    if let LayoutRef::Ctor(d) = layout {
                        m.ctors.insert(*d);
                    }
                }
                Rvalue::LoadConst(d) => {
                    m.defs.insert(*d);
                }
                Rvalue::ReadTag {
                    ty: TagType::Adt(ty),
                    ..
                } => {
                    m.tag_types.insert(*ty);
                }
                // A kind of rvalue added later mentions nothing but its
                // operands unless listed here.
                _ => {}
            }
        }
        match &block.term {
            Terminator::Return(op) => operand(op, m),
            Terminator::TailCall { callee: c, args } => {
                callee(c, m);
                for a in args {
                    operand(a, m);
                }
            }
            Terminator::Goto { args, .. } => args.iter().for_each(|a| operand(a, m)),
            Terminator::Branch { cond, .. } => operand(cond, m),
            Terminator::Switch { on, .. } => operand(on, m),
            Terminator::Panic { message, .. } => {
                if let Some(msg) = message {
                    operand(msg, m);
                }
            }
            Terminator::Unreachable | Terminator::Resume => {}
        }
    }
}

/// What a constant refers to: a user unit constructor's static object
/// needs its layout (D-454); builtin ones are the runtime's.
fn const_defs(c: &Const, m: &mut Mentions) {
    match c {
        Const::StaticCtor(d) => {
            m.defs.insert(*d);
            if d.module != ModuleId::BUILTIN {
                m.ctors.insert(*d);
            }
        }
        Const::FnValue(d) => {
            m.defs.insert(*d);
        }
        Const::Code(key) => key_defs(*key, &mut m.defs),
        Const::Int(_)
        | Const::Float(_)
        | Const::Bool(_)
        | Const::Sized { .. }
        | Const::Str(_)
        | Const::Nil
        | Const::EmptyList
        | Const::Static(_)
        | Const::BuiltinDesc(_)
        | Const::Ctx
        | Const::ErrNil => {}
    }
}

fn key_defs(key: FnKey, out: &mut BTreeSet<DefId>) {
    match key {
        FnKey::Item(d)
        | FnKey::ValueShim(d)
        | FnKey::Closure { owner: d, .. }
        | FnKey::Continuation { owner: d, .. }
        | FnKey::ConstInit(d)
        | FnKey::Test(d)
        | FnKey::TupleDictFn { iface_fn: d, .. }
        | FnKey::Entry(EntryKind::Main(d) | EntryKind::Test(d) | EntryKind::Const(d)) => {
            out.insert(d);
        }
        FnKey::DynForward {
            iface_fn,
            dyn_iface,
        } => {
            out.insert(iface_fn);
            out.insert(dyn_iface);
        }
        FnKey::Entry(EntryKind::Repl(_)) | FnKey::Trampoline(_) => {}
    }
}
