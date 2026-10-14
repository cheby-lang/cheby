//! Borrowing inference (D-715, D-072, D-250, D-787) over lowered MIR
//! ([`crate::Stage::Lowered`], before reference counting): which
//! parameters of a module's functions are borrowed ([`infer`]), and the
//! rewrite that puts the masks into a unit ([`apply`]): into the
//! functions' [`crate::Function::borrow`], into the ABI signatures of the
//! unit's callees ([`crate::AbiSig::borrow`], so into the slot key,
//! D-456), and into its call sites ([`Callee::Borrowing`]). Counting then
//! treats a borrowed parameter as holding no reference
//! ([`crate::rc`], [`crate::ownership`]).
//!
//! # API
//!
//! ```text
//! pub struct BorrowMask(pub u64);                  // Clone Copy Debug Default PartialEq Eq PartialOrd Ord Hash
//! impl BorrowMask {
//!     pub const OWNED: Self;                       // BorrowMask(0)
//!     pub const MAX_PARAMS: usize;                 // 64
//!     pub fn is_borrowed(self, index: usize) -> bool;
//!     pub fn with(self, index: usize) -> Self;     // index >= 64: unchanged
//!     pub fn without(self, index: usize) -> Self;
//!     pub const fn is_owned(self) -> bool;         // no parameter borrowed
//!     pub fn indices(self) -> impl Iterator<Item = usize>;
//! }
//! pub struct BuildFacts { /* private */ }          // Debug Default
//! impl BuildFacts {
//!     pub fn used_as_value(&self, def: DefId) -> bool;
//!     pub fn named_by_other_module(&self, def: DefId) -> bool;
//!     pub fn tail_owned(&self, def: DefId) -> BorrowMask;
//! }
//! pub fn collect_facts(units: &[&MirModule]) -> BuildFacts;
//! pub trait Eligibility {
//!     fn is_private_item(&self, def: DefId) -> bool;
//!     fn is_external_wrapper(&self, def: DefId) -> bool;
//!     fn handle_free(&self, def: DefId, ty: Ty) -> bool;
//! }
//! pub struct IfaceEligibility<'a> { /* private */ }
//! impl<'a> IfaceEligibility<'a> {
//!     pub fn new(defs: Defs<'a>, thir: &thir::Module) -> Self;
//! }
//! impl Eligibility for IfaceEligibility<'_> { … }
//! pub fn externals(thir: &thir::Module) -> BTreeSet<DefId>;
//! pub fn infer(module: &MirModule, facts: &BuildFacts, eligibility: &dyn Eligibility)
//!     -> BTreeMap<DefId, BorrowMask>;
//! pub fn apply(unit: &mut MirModule, masks: &BTreeMap<DefId, BorrowMask>) -> Result<(), Ice>;
//! pub type Masks = BTreeMap<DefId, BorrowMask>;
//! pub fn infer_build(units: &mut [MirModule],
//!     infer_unit: &dyn Fn(&MirModule, &BuildFacts) -> Result<Masks, Ice>) -> Result<Masks, Ice>;
//! pub fn borrows_anything(unit: &MirModule) -> bool;
//! ```
//!
//! Bit `i` of a [`BorrowMask`] is parameter `i` in `Function::params`
//! order, which is [`crate::AbiSig::params`] order: dictionaries first.
//! A parameter past 63 is never borrowed.
//!
//! # Use (D-779)
//!
//! A build lowers every unit first. [`collect_facts`] then reads all of
//! them once: the code units, the tests units and the entries unit.
//! [`infer`] runs per code unit, with an [`Eligibility`] for that unit's
//! module ([`IfaceEligibility`] over the unit's [`Defs`] and the module's
//! THIR), and returns the masks of the functions that borrow something.
//! A function missing from the result is all-owned. [`apply`] then writes
//! the build's masks into every unit, the tests units and the entries
//! unit included, and counting follows ([`infer_build`] does the first
//! two steps in order; the driver runs them in parallel). The analysis
//! is deterministic (D-264): it iterates only ordered maps and sets, and
//! the unit's own function order.
//!
//! A unit counted on its own ([`crate::count_and_check`]: the JIT's tests,
//! a REPL session, per-unit goldens) is never inferred and stays
//! all-owned. A REPL session borrows nothing in any unit it lowers, std
//! and package modules included (D-808), and neither
//! does a build under `CHEBY_RC=naive` ([`crate::MirOptions::borrows`]),
//! which keeps naive counting the oracle of D-718.
//!
//! D-779 (inference across the build's units): a `cheby test` build
//! infers over its tests units too, so its code units may get other
//! masks than a `cheby run` build's; `cheby run`'s code never depends on
//! tests.
//!
//! # The rules (survey-perceus §2.2)
//!
//! A parameter is borrowed only if all of these hold:
//!
//! 1. Its function is a `FnKey::Item` of the unit's own module that the
//!    [`Eligibility`] calls private (not `pub`, D-787) and not an
//!    `@external` wrapper; the body is not `precounted` (D-693) and has no
//!    foreign call (the shape of a wrapper, D-525). Other modules of the
//!    package may call a package-visible function: they call it with its
//!    mask, which they depend on (D-787 refines D-250 here).
//! 2. No unit of the build uses the function as a value: no value shim,
//!    `Const::FnValue`, `Const::Code` or `Callee::Code` of it, statics
//!    included ([`BuildFacts::used_as_value`]).
//! 3. (Not in the REPL: the caller turns the analysis off for every unit a
//!    REPL session lowers.)
//! 4. The parameter's representation is [`Repr::Value`].
//! 5. Its declared type ([`crate::LocalDecl::ty`]) is handle-free
//!    ([`Defs::handle_free`]); a dictionary has no declared type and is
//!    never borrowed.
//! 6. Profitability (D-806): no use consumes it. A
//!    use consumes when it returns the parameter, stores it (`Alloc`),
//!    moves it (`Use`), passes it as a `Goto` argument, in an owned
//!    position of a runtime helper, to `list_slice` at its last use (which
//!    the RC pass makes `list_slice_owned`, D-634), in any position of a
//!    `Callee::Code` or indirect call, or in a position of a direct call
//!    that the callee does not borrow. Such a use of a counted projection
//!    of the parameter consumes the parameter too when the parameter is
//!    dead after it (and not read by it): counting without borrowing has
//!    released the parameter by then, so the projection may be unique for
//!    an in-place update (D-525, D-634), which borrowing would prevent.
//! 7. The tail-call fixed point (D-015): at every tail call in any unit,
//!    the argument in a borrowed position of the callee is a constant (an
//!    operand, or a local defined as `_n = const …`, as lowering writes a
//!    string literal), the caller's own borrowed parameter, or a borrowed
//!    projection of one (a chain of `field` reads from it;
//!    D-807, which the counting implements).
//!    Otherwise that position becomes owned. A tail call is never turned
//!    into a call to fit a mask.
//!
//! Rules 6 and 7 depend on the masks themselves. The masks start at the
//! eligible parameters and only lose bits, over a worklist of the unit's
//! functions in unit order (items by `DefIndex`), revisiting a function's
//! callers when its mask shrinks: a fixed point, which exists because
//! the masks only decrease.
//!
//! # What the counting does
//!
//! A borrowed parameter holds no reference: a `dup` at each owned use,
//! never a `drop`, and it is not in any cleanup pad, the entry pad
//! included ([`crate::Function::owned_params`]). A `field` result of a
//! borrowed parameter or of a borrowed projection is a borrowed projection
//! too, with the same rule (D-807;
//! [`crate::Function::borrowed_locals`]). A call passes a borrowed
//! position without giving up a reference, so the caller drops an
//! argument it no longer needs after the call, and its cleanup pad
//! releases the argument if the callee unwinds. A tail call passes a
//! local defined as a constant in a borrowed position as that constant
//! ([`apply`] substitutes it: the static object needs no reference and
//! outlives the callee), so the caller holds nothing at the jump. Every
//! unit that names a borrowing function (its code unit, its tests unit,
//! the entries unit, and under D-787 the units of other modules of the
//! package) gets the mask in its `CalleeDecl` and its call sites.
//!
//! Visibility (D-787, refining D-715): parameters may be borrowed in `priv`
//! and package-visible functions, never in `pub` ones, whose ABI the code
//! of other packages is compiled against. A body edit can therefore
//! change the mask, and so the ABI signature, of a package-visible
//! function that other modules of the package call; they depend on it
//! (M3's code cache included). The inference of one module reads no other
//! module's masks: a call into another module's borrowing function counts
//! as consuming the caller's parameter there, and a tail call into it
//! from another unit with anything but a constant keeps that position
//! owned ([`BuildFacts::tail_owned`]).

use std::collections::{BTreeMap, BTreeSet};

use cheby_iface::{DefId, DefKind, Ice, ItemSig, Ty, Vis};
use cheby_types::thir;

use crate::defs::Defs;
use crate::ir::{
    BlockId, Callee, Const, FnKey, Function, Local, MirModule, Operand, RuntimeFn, Rvalue, Stmt,
    Terminator, UnitKind, UseKind,
};
use crate::liveness::{BlockLive, Liveness};
use crate::repr::Repr;

/// Which parameters are borrowed (D-715): bit `i` for parameter `i` in
/// [`crate::AbiSig::params`] order (dictionaries first); a parameter past
/// 63 is never borrowed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BorrowMask(pub u64);

impl BorrowMask {
    /// Every parameter owned: the convention of every function today.
    pub const OWNED: Self = Self(0);

    /// The number of parameters a mask can describe.
    pub const MAX_PARAMS: usize = 64;

    /// The bit of parameter `index`, if a mask can hold it.
    fn bit(index: usize) -> Option<u64> {
        u32::try_from(index).ok().and_then(|i| 1_u64.checked_shl(i))
    }

    /// Whether parameter `index` is borrowed.
    #[must_use]
    pub fn is_borrowed(self, index: usize) -> bool {
        Self::bit(index).is_some_and(|b| self.0 & b != 0)
    }

    /// The mask with parameter `index` borrowed; unchanged for an index
    /// past 63.
    #[must_use]
    pub fn with(self, index: usize) -> Self {
        Self::bit(index).map_or(self, |b| Self(self.0 | b))
    }

    /// The mask with parameter `index` owned.
    #[must_use]
    pub fn without(self, index: usize) -> Self {
        Self::bit(index).map_or(self, |b| Self(self.0 & !b))
    }

    /// Whether no parameter is borrowed.
    #[must_use]
    pub const fn is_owned(self) -> bool {
        self.0 == 0
    }

    /// The borrowed parameters' indexes, ascending.
    pub fn indices(self) -> impl Iterator<Item = usize> {
        (0..Self::MAX_PARAMS).filter(move |&i| self.is_borrowed(i))
    }
}

/// Facts no single unit shows (survey-perceus §2.4, D-779): defs used as
/// values anywhere in the build, defs that a unit of another module
/// names, and the positions that tail calls from other units (a tests
/// unit, the entries unit) fill with something other than a constant.
#[derive(Debug, Default)]
pub struct BuildFacts {
    values: BTreeSet<DefId>,
    foreign: BTreeSet<DefId>,
    tail_owned: BTreeMap<DefId, BorrowMask>,
}

impl BuildFacts {
    /// Whether some unit uses `def` as a value: a value shim, a
    /// `Const::FnValue`, a `Const::Code` or a `Callee::Code` of it, in a
    /// body or a static.
    #[must_use]
    pub fn used_as_value(&self, def: DefId) -> bool {
        self.values.contains(&def)
    }

    /// Whether a code or tests unit of a module other than `def`'s names
    /// `def` as a callee. Informative: under D-787 such a call does not
    /// stop `def` from borrowing; the caller uses its mask.
    #[must_use]
    pub fn named_by_other_module(&self, def: DefId) -> bool {
        self.foreign.contains(&def)
    }

    /// The positions of `def` that a tail call from a unit other than its
    /// module's code unit fills with a non-constant: they must stay owned
    /// (rule 7; the callers there borrow nothing).
    #[must_use]
    pub fn tail_owned(&self, def: DefId) -> BorrowMask {
        self.tail_owned
            .get(&def)
            .copied()
            .unwrap_or(BorrowMask::OWNED)
    }
}

/// Collects the build-wide facts from the build's lowered units
/// (`Stage::Lowered`): every code unit, every tests unit and the entries
/// unit, in any order.
#[must_use]
pub fn collect_facts(units: &[&MirModule]) -> BuildFacts {
    let mut facts = BuildFacts::default();
    for unit in units {
        if let Some(own) = unit.module() {
            for decl in &unit.callees {
                if decl.def.module != own {
                    facts.foreign.insert(decl.def);
                }
            }
        }
        for s in &unit.statics {
            for c in &s.fields {
                value_const(c, &mut facts.values);
            }
        }
        for f in &unit.functions {
            if let FnKey::ValueShim(def) = f.key {
                facts.values.insert(def);
            }
            let consts = const_locals(f);
            for block in &f.blocks {
                for stmt in &block.stmts {
                    if let Stmt::Assign { value, .. } | Stmt::Eval(value) = stmt {
                        if let Rvalue::Call { callee, .. } = value {
                            value_callee(callee, &mut facts.values);
                        }
                        for op in value.operands() {
                            value_operand(&op, &mut facts.values);
                        }
                    }
                }
                for op in term_operands(&block.term) {
                    value_operand(op, &mut facts.values);
                }
                if let Terminator::TailCall { callee, args } = &block.term {
                    value_callee(callee, &mut facts.values);
                    if let Some(g) = callee.direct()
                        && unit.unit != UnitKind::Code(g.module)
                    {
                        let entry = facts.tail_owned.entry(g).or_default();
                        for (j, arg) in args.iter().enumerate() {
                            let constant = match arg {
                                Operand::Const(_) => true,
                                Operand::Local(l) => consts.contains(l),
                            };
                            if !constant {
                                *entry = entry.with(j);
                            }
                        }
                    }
                }
            }
        }
    }
    facts
}

/// Records the def a constant makes a value of.
fn value_const(c: &Const, values: &mut BTreeSet<DefId>) {
    match c {
        Const::FnValue(def) | Const::Code(FnKey::ValueShim(def) | FnKey::Item(def)) => {
            values.insert(*def);
        }
        _ => {}
    }
}

fn value_operand(op: &Operand, values: &mut BTreeSet<DefId>) {
    if let Operand::Const(c) = op {
        value_const(c, values);
    }
}

/// A call that reaches a top-level function by key, not through its slot.
fn value_callee(callee: &Callee, values: &mut BTreeSet<DefId>) {
    if let Callee::Code(FnKey::ValueShim(def) | FnKey::Item(def)) = callee {
        values.insert(*def);
    }
}

/// The operands a terminator reads, in order.
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

/// What the analysis asks of the front end for one module: visibility,
/// external wrappers, and whether a parameter's declared type is
/// handle-free (D-715).
pub trait Eligibility {
    /// Whether `def` is a function of this module that only its own
    /// package can call: `priv` or package-visible, not `pub` (D-787).
    fn is_private_item(&self, def: DefId) -> bool;

    /// Whether `def` has an `@external` (its function is a wrapper of a
    /// kernel, D-455, or does not exist on this target): never borrowed,
    /// since kernels may update unique arguments in place (D-525).
    fn is_external_wrapper(&self, def: DefId) -> bool;

    /// Whether `ty`, the declared type of a parameter of `def`, holds no
    /// handle as `def`'s module sees it (D-205, D-246).
    fn handle_free(&self, def: DefId, ty: Ty) -> bool;
}

/// The [`Eligibility`] of one module from its interfaces (D-246) and its
/// THIR (for `@external`s).
#[derive(Debug)]
pub struct IfaceEligibility<'a> {
    defs: Defs<'a>,
    externals: BTreeSet<DefId>,
}

impl<'a> IfaceEligibility<'a> {
    /// The eligibility of the module `defs.own`, whose checked THIR is
    /// `thir`: its `priv` and package-visible functions, never its `pub`
    /// ones (D-787).
    #[must_use]
    pub fn new(defs: Defs<'a>, thir: &thir::Module) -> Self {
        Self {
            defs,
            externals: externals(thir),
        }
    }
}

impl Eligibility for IfaceEligibility<'_> {
    fn is_private_item(&self, def: DefId) -> bool {
        let Some(own) = self.defs.own else {
            return false;
        };
        if own.module != def.module {
            return false;
        }
        own.item(def.index).is_some_and(|item| {
            item.kind == DefKind::Fn
                && matches!(item.sig, ItemSig::Fn(_))
                && match item.vis {
                    // D-787: not `pub`.
                    Vis::Priv | Vis::Package => true,
                    Vis::Pub => false,
                }
        })
    }

    fn is_external_wrapper(&self, def: DefId) -> bool {
        self.externals.contains(&def)
    }

    fn handle_free(&self, _def: DefId, ty: Ty) -> bool {
        self.defs.handle_free(ty)
    }
}

/// The functions of `thir` with a native `@external` or no body (only
/// other targets' externals): never borrowed.
#[must_use]
pub fn externals(thir: &thir::Module) -> BTreeSet<DefId> {
    thir.fns
        .iter()
        .filter(|f| f.native_external.is_some() || f.body.is_none())
        .map(|f| f.def)
        .collect()
}

/// A use of a parameter that may consume it (rule 6).
#[derive(Clone, Copy, Debug)]
enum Consumer {
    Always,
    /// Consumes unless the direct callee borrows this position.
    Unless(DefId, usize),
}

/// What a tail call passes in one position (rule 7).
#[derive(Clone, Copy, Debug)]
enum TailArg {
    Const,
    /// The caller's parameter `i` or a projection of it: fits a borrowed
    /// position while the caller borrows `i`.
    Param(usize),
    Owned,
}

/// A direct tail call's argument.
#[derive(Clone, Copy, Debug)]
struct Site {
    callee: DefId,
    pos: usize,
    arg: TailArg,
}

/// One function of the unit, as the fixed point sees it.
#[derive(Debug, Default)]
struct Node {
    /// Per parameter (up to 64), the uses that may consume it.
    consumers: Vec<Vec<Consumer>>,
    /// Direct tail calls into this module.
    sites: Vec<Site>,
}

/// The masks of `module`'s functions that borrow something, at the fixed
/// point of rules 1-7 (see the module documentation). `module` is a code
/// unit at `Stage::Lowered`; any other unit gives an empty map. A
/// function not in the map is all-owned.
#[must_use]
pub fn infer(
    module: &MirModule,
    facts: &BuildFacts,
    eligibility: &dyn Eligibility,
) -> BTreeMap<DefId, BorrowMask> {
    let UnitKind::Code(own) = module.unit else {
        return BTreeMap::new();
    };
    // The unit's items by def, for direct calls within the module.
    let items: BTreeMap<DefId, usize> = module
        .functions
        .iter()
        .enumerate()
        .filter_map(|(n, f)| match f.key {
            FnKey::Item(def) if def.module == own => Some((def, n)),
            _ => None,
        })
        .collect();
    let nodes: Vec<Node> = module.functions.iter().map(scan).collect();
    let mut masks: Vec<BorrowMask> = module
        .functions
        .iter()
        .map(|f| initial_mask(f, own, facts, eligibility))
        .collect();
    // Callers by callee, both function indexes.
    let mut by_callee: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for (n, node) in nodes.iter().enumerate() {
        let targets = node
            .consumers
            .iter()
            .flatten()
            .filter_map(|c| match c {
                Consumer::Unless(g, _) => Some(*g),
                Consumer::Always => None,
            })
            .chain(node.sites.iter().map(|s| s.callee));
        for g in targets {
            if let Some(&m) = items.get(&g) {
                by_callee.entry(m).or_default().insert(n);
            }
        }
    }
    let callers = by_callee;
    let borrowed = |masks: &[BorrowMask], g: DefId, j: usize| {
        items
            .get(&g)
            .and_then(|&m| masks.get(m))
            .is_some_and(|mask| mask.is_borrowed(j))
    };
    let mut work: BTreeSet<usize> = (0..nodes.len()).collect();
    while let Some(n) = work.pop_first() {
        let (Some(node), Some(&current)) = (nodes.get(n), masks.get(n)) else {
            continue;
        };
        // Rule 6: drop what a use consumes.
        let mut next = current;
        for i in current.indices() {
            let consumed = node.consumers.get(i).is_some_and(|uses| {
                uses.iter().any(|c| match *c {
                    Consumer::Always => true,
                    Consumer::Unless(g, j) => !borrowed(&masks, g, j),
                })
            });
            if consumed {
                next = next.without(i);
            }
        }
        if next != current {
            if let Some(slot) = masks.get_mut(n) {
                *slot = next;
            }
            if let Some(cs) = callers.get(&n) {
                work.extend(cs.iter().copied());
            }
        }
        // Rule 7: this function's tail calls.
        for site in &node.sites {
            if !borrowed(&masks, site.callee, site.pos) {
                continue;
            }
            let fits = match site.arg {
                TailArg::Const => true,
                TailArg::Param(i) => masks.get(n).is_some_and(|m| m.is_borrowed(i)),
                TailArg::Owned => false,
            };
            if fits {
                continue;
            }
            let Some(&m) = items.get(&site.callee) else {
                continue;
            };
            if let Some(slot) = masks.get_mut(m) {
                *slot = slot.without(site.pos);
            }
            work.insert(m);
            if let Some(cs) = callers.get(&m) {
                work.extend(cs.iter().copied());
            }
        }
    }
    module
        .functions
        .iter()
        .zip(masks)
        .filter_map(|(f, mask)| match f.key {
            FnKey::Item(def) if !mask.is_owned() => Some((def, mask)),
            _ => None,
        })
        .collect()
}

/// The mask rules 1-5 allow for `f`, less the positions other units'
/// tail calls fill with non-constants.
fn initial_mask(
    f: &Function,
    own: cheby_iface::ModuleId,
    facts: &BuildFacts,
    eligibility: &dyn Eligibility,
) -> BorrowMask {
    let FnKey::Item(def) = f.key else {
        return BorrowMask::OWNED;
    };
    let eligible = def.module == own
        && !f.precounted
        && eligibility.is_private_item(def)
        && !eligibility.is_external_wrapper(def)
        && !has_foreign_call(f)
        && !facts.used_as_value(def);
    if !eligible {
        return BorrowMask::OWNED;
    }
    let mut mask = BorrowMask::OWNED;
    for (i, &p) in f.params.iter().enumerate().take(BorrowMask::MAX_PARAMS) {
        let Ok(decl) = f.local(p) else {
            continue;
        };
        if decl.repr == Repr::Value
            && let Some(ty) = decl.ty
            && eligibility.handle_free(def, ty)
        {
            mask = mask.with(i);
        }
    }
    let forced = facts.tail_owned(def);
    BorrowMask(mask.0 & !forced.0)
}

/// Whether `f` calls a foreign function: the shape of an `@external`
/// wrapper (D-455), which D-525 needs owned.
fn has_foreign_call(f: &Function) -> bool {
    f.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
        matches!(
            s,
            Stmt::Assign {
                value: Rvalue::CallForeign { .. },
                ..
            } | Stmt::Eval(Rvalue::CallForeign { .. })
        )
    })
}

/// The uses and tail calls of `f`'s parameters.
fn scan(f: &Function) -> Node {
    let params: BTreeMap<Local, usize> = f
        .params
        .iter()
        .enumerate()
        .take(BorrowMask::MAX_PARAMS)
        .map(|(i, &p)| (p, i))
        .collect();
    let consts = const_locals(f);
    let mut scan = Scan {
        f,
        node: Node {
            consumers: vec![Vec::new(); params.len()],
            sites: Vec::new(),
        },
        roots: projection_roots(f, &params),
        params,
        liveness: None,
        blocks: BTreeMap::new(),
    };
    for (id, block) in f.blocks.iter_enumerated() {
        for (k, stmt) in block.stmts.iter().enumerate() {
            if let Stmt::Assign { value, .. } | Stmt::Eval(value) = stmt {
                scan.rvalue(id, k, value);
            }
        }
        let at = Point::Term(id);
        let uses = block.term.uses();
        match &block.term {
            Terminator::TailCall { callee, args } => {
                if let Some(g) = callee.direct() {
                    for (j, arg) in args.iter().enumerate() {
                        let kind = match arg {
                            Operand::Const(_) => TailArg::Const,
                            Operand::Local(l) if consts.contains(l) => TailArg::Const,
                            Operand::Local(l) => scan
                                .params
                                .get(l)
                                .or_else(|| scan.roots.get(l))
                                .map_or(TailArg::Owned, |&i| TailArg::Param(i)),
                        };
                        scan.node.sites.push(Site {
                            callee: g,
                            pos: j,
                            arg: kind,
                        });
                    }
                }
                scan.call(callee, args, at, &uses);
            }
            _ => scan.owned_uses(&uses, at),
        }
    }
    scan.node
}

/// Where an instruction is: statement `k` of a block, or its terminator.
#[derive(Clone, Copy, Debug)]
enum Point {
    Stmt(BlockId, usize),
    Term(BlockId),
}

/// The state of [`scan`] over one function.
struct Scan<'f> {
    f: &'f Function,
    /// The parameters (up to 64) by local.
    params: BTreeMap<Local, usize>,
    /// The projection locals, by the parameter they read from.
    roots: BTreeMap<Local, usize>,
    node: Node,
    /// Computed at the first question about it; an error answers "not
    /// live", which makes the use consuming (the safe side).
    liveness: Option<Result<Liveness, Ice>>,
    blocks: BTreeMap<BlockId, Option<BlockLive>>,
}

impl Scan<'_> {
    /// Whether `local` is live right after the instruction at `at`.
    fn live_after(&mut self, local: Local, at: Point) -> bool {
        let f = self.f;
        let id = match at {
            Point::Stmt(id, _) | Point::Term(id) => id,
        };
        let live = self.liveness.get_or_insert_with(|| Liveness::compute(f));
        let block = self
            .blocks
            .entry(id)
            .or_insert_with(|| live.as_ref().ok().and_then(|l| l.block(f, id).ok()));
        block.as_ref().is_some_and(|b| match at {
            Point::Stmt(_, k) => b.after.get(k).is_some_and(|set| set.contains(&local)),
            Point::Term(_) => b.out.contains(&local),
        })
    }

    /// A use of `local` that consumes it unless `c` says otherwise, by the
    /// instruction at `at`, which reads `uses`.
    ///
    /// A parameter's use counts as is. So does a projection's, when its
    /// parameter is dead after the instruction and not read by it: there
    /// counting without borrowing has released the parameter before, so
    /// the projection may be unique, as an in-place update needs (D-525,
    /// D-634); a borrowed parameter would keep it shared.
    fn consume(&mut self, local: Local, c: Consumer, at: Point, uses: &[(Local, UseKind)]) {
        let index = match (self.params.get(&local), self.roots.get(&local)) {
            (Some(&i), _) => Some(i),
            (None, Some(&i)) => {
                let param = self.f.params.get(i).copied();
                param
                    .filter(|&p| !uses.iter().any(|&(l, _)| l == p) && !self.live_after(p, at))
                    .map(|_| i)
            }
            (None, None) => None,
        };
        if let Some(i) = index
            && let Some(list) = self.node.consumers.get_mut(i)
        {
            list.push(c);
        }
    }

    /// The owned uses among `uses` consume.
    fn owned_uses(&mut self, uses: &[(Local, UseKind)], at: Point) {
        for &(l, kind) in uses {
            if kind == UseKind::Owned {
                self.consume(l, Consumer::Always, at, uses);
            }
        }
    }

    /// A Cheby call or tail call: a direct callee may borrow a position.
    fn call(&mut self, callee: &Callee, args: &[Operand], at: Point, uses: &[(Local, UseKind)]) {
        if let Callee::Indirect(fl) = callee {
            self.consume(*fl, Consumer::Always, at, uses);
        }
        for (j, arg) in args.iter().enumerate() {
            let Some(l) = arg.local() else { continue };
            let c = match callee {
                Callee::Direct(g) | Callee::Borrowing(g, _) => Consumer::Unless(*g, j),
                Callee::Code(_) | Callee::Indirect(_) => Consumer::Always,
            };
            self.consume(l, c, at, uses);
        }
    }

    /// Statement `k` of block `id`, which computes `value`.
    fn rvalue(&mut self, id: BlockId, k: usize, value: &Rvalue) {
        let at = Point::Stmt(id, k);
        let uses = value.uses();
        match value {
            Rvalue::Call { callee, args } => self.call(callee, args, at, &uses),
            Rvalue::CallRuntime {
                func: RuntimeFn::ListSlice,
                args,
            } => {
                // The list at its last use: the RC pass would slice it in
                // place (`list_slice_owned`, D-634).
                if let Some(Operand::Local(list)) = args.first()
                    && (self.params.contains_key(list) || self.roots.contains_key(list))
                    && !self.live_after(*list, at)
                {
                    self.consume(*list, Consumer::Always, at, &[]);
                }
            }
            _ => self.owned_uses(&uses, at),
        }
    }
}

/// The locals of `f` defined as a constant (`_n = const …`): a static
/// object or a raw value, which a borrowed tail position may receive.
/// The counting passes the constant itself there (rule 7).
fn const_locals(f: &Function) -> BTreeSet<Local> {
    f.blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter_map(|s| match s {
            Stmt::Assign {
                dst,
                value: Rvalue::Use(Operand::Const(_)),
            } => Some(*dst),
            _ => None,
        })
        .collect()
}

/// The parameter each projection local of `f` reads from: a counted
/// ([`Repr::Value`]) result of a chain of `field` reads that starts at a
/// parameter. A raw field is a copy and holds nothing.
fn projection_roots(f: &Function, params: &BTreeMap<Local, usize>) -> BTreeMap<Local, usize> {
    let fields: Vec<(Local, Local)> = f
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter_map(|s| match s {
            Stmt::Assign {
                dst,
                value: Rvalue::Field { base, .. },
            } if f.local(*dst).is_ok_and(|d| d.repr == Repr::Value) => Some((*dst, *base)),
            _ => None,
        })
        .collect();
    let mut roots: BTreeMap<Local, usize> = BTreeMap::new();
    // Every pass settles at least one more link of each chain.
    for _ in 0..=fields.len() {
        let mut changed = false;
        for &(dst, base) in &fields {
            if roots.contains_key(&dst) {
                continue;
            }
            if let Some(&i) = params.get(&base).or_else(|| roots.get(&base)) {
                roots.insert(dst, i);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    roots
}

/// The masks of the functions that borrow something, by def; a function
/// missing from it is all-owned.
pub type Masks = BTreeMap<DefId, BorrowMask>;

/// Infers the masks of a build's lowered units and writes them into every
/// unit (D-779): [`collect_facts`] over all of `units`, `infer_unit` for
/// each code unit (normally [`infer`] with the unit's
/// [`IfaceEligibility`]), then [`apply`] to each unit with the masks of
/// the whole build. Returns those masks. Sequential; the driver runs the
/// same steps in parallel.
///
/// # Errors
///
/// What `infer_unit` or [`apply`] returns.
pub fn infer_build(
    units: &mut [MirModule],
    infer_unit: &dyn Fn(&MirModule, &BuildFacts) -> Result<Masks, Ice>,
) -> Result<Masks, Ice> {
    let facts = {
        let refs: Vec<&MirModule> = units.iter().collect();
        collect_facts(&refs)
    };
    let mut masks = BTreeMap::new();
    for unit in units.iter() {
        if matches!(unit.unit, UnitKind::Code(_)) {
            masks.extend(infer_unit(unit, &facts)?);
        }
    }
    for unit in units.iter_mut() {
        apply(unit, &masks)?;
    }
    Ok(masks)
}

/// Writes the build's `masks` (from [`infer`] over every code unit) into
/// `unit`, a lowered unit of the same build (D-715): each borrowing
/// function's [`Function::borrow`], the [`crate::AbiSig::borrow`] of each
/// borrowing callee in [`MirModule::callees`] (so its ABI signature and
/// slot key, D-456), and each call or tail call of one, which becomes
/// [`Callee::Borrowing`]. A local defined as a constant (`_n = const …`)
/// in a borrowed position is replaced by that constant, which needs no
/// reference; its definition goes when nothing else reads it. In a tail
/// call this is what lets the caller hold nothing at the jump (rule 7);
/// in a call it saves the caller's `drop` after the call.
///
/// # Errors
///
/// An [`Ice`] for a mask that names a parameter the callee does not have.
pub fn apply(unit: &mut MirModule, masks: &BTreeMap<DefId, BorrowMask>) -> Result<(), Ice> {
    if masks.is_empty() {
        return Ok(());
    }
    for decl in &mut unit.callees {
        if let Some(&mask) = masks.get(&decl.def) {
            if mask.indices().any(|i| i >= decl.sig.params.len()) {
                return Err(Ice::new(format!(
                    "the borrow mask {mask:?} of {:?} names a parameter it does not have",
                    decl.def
                )));
            }
            decl.sig.borrow = mask;
        }
    }
    for f in &mut unit.functions {
        if let FnKey::Item(def) = f.key
            && let Some(&mask) = masks.get(&def)
        {
            f.borrow = mask;
        }
        apply_to_calls(f, masks);
    }
    Ok(())
}

/// The call sites of `f`: see [`apply`].
fn apply_to_calls(f: &mut Function, masks: &BTreeMap<DefId, BorrowMask>) {
    let rewrite = |callee: &mut Callee| {
        if let Callee::Direct(g) = *callee
            && let Some(&mask) = masks.get(&g)
            && !mask.is_owned()
        {
            *callee = Callee::Borrowing(g, mask);
        }
        callee.borrow_mask()
    };
    // The value of each local defined as a constant.
    let consts: BTreeMap<Local, Const> = f
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter_map(|s| match s {
            Stmt::Assign {
                dst,
                value: Rvalue::Use(Operand::Const(c)),
            } => Some((*dst, c.clone())),
            _ => None,
        })
        .collect();
    let substitute = |mask: BorrowMask, args: &mut [Operand], replaced: &mut BTreeSet<Local>| {
        for (j, arg) in args.iter_mut().enumerate() {
            if mask.is_borrowed(j)
                && let Operand::Local(l) = *arg
                && let Some(c) = consts.get(&l)
            {
                *arg = Operand::Const(c.clone());
                replaced.insert(l);
            }
        }
    };
    let mut replaced = BTreeSet::new();
    for block in &mut f.blocks {
        for stmt in &mut block.stmts {
            if let Stmt::Assign {
                value: Rvalue::Call { callee, args },
                ..
            }
            | Stmt::Eval(Rvalue::Call { callee, args }) = stmt
            {
                let mask = rewrite(callee);
                substitute(mask, args, &mut replaced);
            }
        }
        if let Terminator::TailCall { callee, args } = &mut block.term {
            let mask = rewrite(callee);
            substitute(mask, args, &mut replaced);
        }
    }
    if replaced.is_empty() {
        return;
    }
    // The constants nothing reads any more.
    let mut read = BTreeSet::new();
    for block in &f.blocks {
        for stmt in &block.stmts {
            if let Stmt::Assign { value, .. } | Stmt::Eval(value) = stmt {
                read.extend(value.operands().iter().filter_map(Operand::local));
            }
        }
        read.extend(
            term_operands(&block.term)
                .into_iter()
                .filter_map(Operand::local),
        );
        if let Terminator::TailCall {
            callee: Callee::Indirect(l),
            ..
        } = &block.term
        {
            read.insert(*l);
        }
    }
    for block in &mut f.blocks {
        block.stmts.retain(|s| match s {
            Stmt::Assign { dst, .. } => !replaced.contains(dst) || read.contains(dst),
            _ => true,
        });
    }
}

/// Whether `unit` borrows anything (D-715): a function's mask or a
/// callee's (each call's mask is its callee's, which the validator
/// checks).
#[must_use]
pub fn borrows_anything(unit: &MirModule) -> bool {
    unit.functions.iter().any(|f| !f.borrow.is_owned())
        || unit.callees.iter().any(|d| !d.sig.borrow.is_owned())
}
