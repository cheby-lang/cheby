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
//! pub struct BuildFacts { /* private */ }          // Clone Debug Default PartialEq Eq
//! impl BuildFacts {
//!     pub fn used_as_value(&self, def: DefId) -> bool;
//!     pub fn named_by_other_module(&self, def: DefId) -> bool;
//!     pub fn tail_owned(&self, def: DefId) -> BorrowMask;
//!     pub fn union(parts: &[&UnitFacts], defs: &Defs<'_>) -> Result<BuildFacts, Ice>;
//!     pub fn about_module(&self, module: ModuleId, defs: &Defs<'_>) -> Result<Vec<u8>, Ice>;
//! }
//! pub fn collect_facts(units: &[&MirModule]) -> BuildFacts;
//! pub struct UnitFacts { /* private */ }           // Clone Debug Default PartialEq Eq
//! impl UnitFacts {
//!     pub fn encode(&self) -> Vec<u8>;
//!     pub fn decode(bytes: &[u8]) -> Result<UnitFacts, Ice>;
//!     pub fn callees_elsewhere(&self) -> Vec<String>;
//!     pub fn values(&self) -> Vec<String>;
//!     pub fn tail_owned(&self) -> Vec<(String, BorrowMask)>;
//! }
//! pub fn unit_facts(unit: &MirModule, defs: &Defs<'_>) -> Result<UnitFacts, Ice>;
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
//! A build lowers every unit first. [`collect_facts`] then reads the
//! units of its inference scope once (D-968): the code units, the tests
//! units and the entries unit (whose statics may hold `main`'s error
//! dictionary; GAP(entries-unit-facts)). The facts are the union of each unit's own
//! ([`unit_facts`], D-969), so a cache can store them per unit by name
//! and rebuild them ([`BuildFacts::union`]); a module's masks depend on
//! the build only through [`BuildFacts::about_module`].
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
//! 6. Profitability (D-806): no use consumes it. A use consumes when it
//!    returns the parameter, stores it (`Alloc`), moves it (`Use`),
//!    passes it as a `Goto` argument, in an owned position of a runtime
//!    helper, to `list_slice` or `list_uncons` at its last use (which the
//!    RC pass makes `list_slice_owned` or `list_uncons_owned`, D-634,
//!    D-909), in any position of a `Callee::Code` or indirect call, or in
//!    a position of a direct call that the callee does not borrow. Since
//!    patterns read spreads last (D-908), a parameter matched with
//!    `[..rest, x]` is consumed. Such a use of a counted projection
//!    of the parameter consumes the parameter too when the parameter is
//!    dead after it (and not read by it): counting without borrowing has
//!    released the parameter by then, so the projection may be unique for
//!    an in-place update (D-525, D-634), which borrowing would prevent.
//!    A parameter whose `drop` the reuse pass would pair with an
//!    allocation ([`crate::reuse::candidate_params`]) is consumed too:
//!    borrowed, it would never be dropped, so its block never reused
//!    (D-841, refining D-806 and through it D-715).
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
///
/// Every fact is a union of per-unit parts (D-969; GAP(facts-union)):
/// [`collect_facts`] is the union of each unit's own facts, and
/// [`BuildFacts::union`] rebuilds the same facts from [`UnitFacts`], which
/// name functions by module path and item so they can be stored. No fact
/// needs a fixed point across units: rule 7's fixed point runs inside one
/// module's [`infer`], over facts that are already unions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildFacts {
    values: BTreeSet<DefId>,
    foreign: BTreeSet<DefId>,
    /// Only positions some tail call fills with a non-constant: a def
    /// missing here is all-owned for rule 7.
    tail_owned: BTreeMap<DefId, BorrowMask>,
}

impl BuildFacts {
    /// Adds `other`'s facts to these: the union of sets, and of each
    /// def's tail-owned positions.
    fn merge(&mut self, other: Self) {
        self.values.extend(other.values);
        self.foreign.extend(other.foreign);
        for (def, mask) in other.tail_owned {
            let entry = self.tail_owned.entry(def).or_default();
            *entry = BorrowMask(entry.0 | mask.0);
        }
    }

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
/// (`Stage::Lowered`), in any order: the union of each unit's own facts
/// (D-969). A build passes the units of its inference scope (D-968): its
/// code units and tests units, and the entries unit, whose statics may
/// hold `main`'s error dictionary and so use a function as a value
/// (GAP(entries-unit-facts): D-968 says it contributes no facts, but its
/// value facts keep rule 2 sound).
#[must_use]
pub fn collect_facts(units: &[&MirModule]) -> BuildFacts {
    let mut facts = BuildFacts::default();
    for unit in units {
        facts.merge(own_facts(unit));
    }
    facts
}

/// The facts one lowered unit contributes, by `DefId`.
fn own_facts(unit: &MirModule) -> BuildFacts {
    let mut facts = BuildFacts::default();
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
                    let mut owned = BorrowMask::OWNED;
                    for (j, arg) in args.iter().enumerate() {
                        let constant = match arg {
                            Operand::Const(_) => true,
                            Operand::Local(l) => consts.contains(l),
                        };
                        if !constant {
                            owned = owned.with(j);
                        }
                    }
                    if !owned.is_owned() {
                        let entry = facts.tail_owned.entry(g).or_default();
                        *entry = BorrowMask(entry.0 | owned.0);
                    }
                }
            }
        }
    }
    facts
}

/// A function as stored facts name it (D-969, D-264): its module's path
/// (empty for the builtin module) and its item, `Parent::name` for an
/// interface function or a constructor; never a `DefId`, which is
/// build-local.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct FactName {
    module: Box<str>,
    item: Box<str>,
}

impl FactName {
    /// The name of `def`, read through `defs`.
    fn of(def: DefId, defs: &Defs<'_>) -> Result<Self, Ice> {
        let item = defs.item(def)?;
        let module = if def.module == cheby_iface::ModuleId::BUILTIN {
            String::new()
        } else {
            defs.modules.try_get(def.module)?.path.to_string()
        };
        Ok(Self {
            module: module.into(),
            item: item_text(def, item, defs)?.into(),
        })
    }
}

/// The text of `item` (the item `def`) in a [`FactName`].
fn item_text(def: DefId, item: &cheby_iface::IfaceItem, defs: &Defs<'_>) -> Result<String, Ice> {
    let names = &defs.globals.names;
    let mut text = String::new();
    if let Some(parent) = item.parent {
        text.push_str(names.get(defs.item(DefId::new(def.module, parent))?.name));
        text.push_str("::");
    }
    text.push_str(names.get(item.name));
    Ok(text)
}

/// The borrowing facts one unit's lowered MIR contributes (D-969): the
/// functions it uses as values, those of other modules it names as
/// callees, and the positions its tail calls into other modules fill with
/// a non-constant, each function named by module path and item (never a
/// `DefId`, which is build-local), so the facts can be stored
/// ([`UnitFacts::encode`]). [`BuildFacts::union`] of every unit's facts
/// is [`collect_facts`] over the units.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnitFacts {
    values: BTreeSet<FactName>,
    foreign: BTreeSet<FactName>,
    tail_owned: BTreeMap<FactName, BorrowMask>,
}

/// The first bytes of [`UnitFacts::encode`].
const UNIT_FACTS_MAGIC: &[u8; 4] = b"CBUF";
/// The version of [`UnitFacts::encode`]'s bytes.
const UNIT_FACTS_VERSION: u8 = 1;

impl UnitFacts {
    /// The facts' canonical bytes: a magic, a version, then the values,
    /// the callees elsewhere and the tail-owned positions, each list
    /// sorted by module path and item, every string and count
    /// length-prefixed (D-264: no order but the names').
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(UNIT_FACTS_MAGIC);
        out.push(UNIT_FACTS_VERSION);
        put_len(&mut out, self.values.len());
        for name in &self.values {
            put_name(&mut out, name);
        }
        put_len(&mut out, self.foreign.len());
        for name in &self.foreign {
            put_name(&mut out, name);
        }
        put_len(&mut out, self.tail_owned.len());
        for (name, mask) in &self.tail_owned {
            put_name(&mut out, name);
            out.extend_from_slice(&mask.0.to_le_bytes());
        }
        out
    }

    /// The facts [`UnitFacts::encode`] wrote.
    ///
    /// # Errors
    ///
    /// An [`Ice`] when the bytes are malformed: a cache record that
    /// decodes wrongly after its seal matched is a compiler bug.
    pub fn decode(bytes: &[u8]) -> Result<Self, Ice> {
        let bad = || Ice::new("malformed borrowing facts (D-969)");
        let mut r = bytes
            .strip_prefix(UNIT_FACTS_MAGIC.as_slice())
            .ok_or_else(bad)?;
        let (&version, rest) = r.split_first().ok_or_else(bad)?;
        if version != UNIT_FACTS_VERSION {
            return Err(bad());
        }
        r = rest;
        let mut facts = Self::default();
        for _ in 0..take_len(&mut r).ok_or_else(bad)? {
            facts.values.insert(take_name(&mut r).ok_or_else(bad)?);
        }
        for _ in 0..take_len(&mut r).ok_or_else(bad)? {
            facts.foreign.insert(take_name(&mut r).ok_or_else(bad)?);
        }
        for _ in 0..take_len(&mut r).ok_or_else(bad)? {
            let name = take_name(&mut r).ok_or_else(bad)?;
            let (word, rest) = r.split_first_chunk::<8>().ok_or_else(bad)?;
            r = rest;
            facts
                .tail_owned
                .insert(name, BorrowMask(u64::from_le_bytes(*word)));
        }
        if r.is_empty() { Ok(facts) } else { Err(bad()) }
    }

    /// The functions of other modules the unit calls (D-969's list), as
    /// `module::item` paths, sorted.
    #[must_use]
    pub fn callees_elsewhere(&self) -> Vec<String> {
        self.foreign.iter().map(path_of).collect()
    }

    /// The functions the unit uses as values, as `module::item` paths,
    /// sorted.
    #[must_use]
    pub fn values(&self) -> Vec<String> {
        self.values.iter().map(path_of).collect()
    }

    /// The positions the unit's tail calls into other modules fill with a
    /// non-constant, by `module::item` path.
    #[must_use]
    pub fn tail_owned(&self) -> Vec<(String, BorrowMask)> {
        self.tail_owned
            .iter()
            .map(|(name, &mask)| (path_of(name), mask))
            .collect()
    }
}

/// `module::item`, or `item` for the builtin module.
fn path_of(name: &FactName) -> String {
    if name.module.is_empty() {
        name.item.to_string()
    } else {
        format!("{}::{}", name.module, name.item)
    }
}

/// Appends a count as four little-endian bytes (saturated).
fn put_len(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(&u32::try_from(len).unwrap_or(u32::MAX).to_le_bytes());
}

/// Appends a string as its length and its bytes.
fn put_str(out: &mut Vec<u8>, s: &str) {
    put_len(out, s.len());
    out.extend_from_slice(s.as_bytes());
}

fn put_name(out: &mut Vec<u8>, name: &FactName) {
    put_str(out, &name.module);
    put_str(out, &name.item);
}

/// Reads what [`put_len`] wrote.
fn take_len(r: &mut &[u8]) -> Option<usize> {
    let (word, rest) = r.split_first_chunk::<4>()?;
    *r = rest;
    usize::try_from(u32::from_le_bytes(*word)).ok()
}

/// Reads what [`put_str`] wrote.
fn take_str(r: &mut &[u8]) -> Option<Box<str>> {
    let len = take_len(r)?;
    let (text, rest) = r.split_at_checked(len)?;
    *r = rest;
    std::str::from_utf8(text).ok().map(Box::from)
}

fn take_name(r: &mut &[u8]) -> Option<FactName> {
    Some(FactName {
        module: take_str(r)?,
        item: take_str(r)?,
    })
}

/// The facts `unit`, a lowered unit at `Stage::Lowered`, contributes
/// (D-969), its functions named through `defs`, which must see every def
/// the unit names (the whole build's interfaces do).
///
/// # Errors
///
/// An [`Ice`] if `defs` cannot see a def the facts name.
pub fn unit_facts(unit: &MirModule, defs: &Defs<'_>) -> Result<UnitFacts, Ice> {
    let own = own_facts(unit);
    let mut facts = UnitFacts::default();
    for def in own.values {
        facts.values.insert(FactName::of(def, defs)?);
    }
    for def in own.foreign {
        facts.foreign.insert(FactName::of(def, defs)?);
    }
    for (def, mask) in own.tail_owned {
        facts.tail_owned.insert(FactName::of(def, defs)?, mask);
    }
    Ok(facts)
}

/// Finds the defs that [`FactName`]s name, through one [`Defs`].
struct Resolver<'d, 'a> {
    defs: &'d Defs<'a>,
    /// Module paths to ids, the builtin module as the empty path.
    modules: BTreeMap<String, cheby_iface::ModuleId>,
    /// Per module read so far: each item's text, `None` when two items
    /// share it.
    items: BTreeMap<cheby_iface::ModuleId, BTreeMap<String, Option<cheby_iface::DefIndex>>>,
}

impl<'d, 'a> Resolver<'d, 'a> {
    fn new(defs: &'d Defs<'a>) -> Self {
        let mut modules: BTreeMap<String, cheby_iface::ModuleId> = defs
            .modules
            .iter()
            .filter(|&(id, _)| id != cheby_iface::ModuleId::BUILTIN)
            .map(|(id, info)| (info.path.to_string(), id))
            .collect();
        modules.insert(String::new(), cheby_iface::ModuleId::BUILTIN);
        Self {
            defs,
            modules,
            items: BTreeMap::new(),
        }
    }

    /// The def `name` names.
    fn find(&mut self, name: &FactName) -> Result<DefId, Ice> {
        let missing = || {
            Ice::new(format!(
                "borrowing facts name `{}`, which this build cannot see",
                path_of(name)
            ))
        };
        let module = *self.modules.get(&*name.module).ok_or_else(missing)?;
        if !self.items.contains_key(&module) {
            let iface = self.defs.interface(module).ok_or_else(missing)?;
            let mut texts: BTreeMap<String, Option<cheby_iface::DefIndex>> = BTreeMap::new();
            for (index, item) in iface.items.iter_enumerated() {
                let text = item_text(DefId::new(module, index), item, self.defs)?;
                texts
                    .entry(text)
                    .and_modify(|seen| *seen = None)
                    .or_insert(Some(index));
            }
            self.items.insert(module, texts);
        }
        self.items
            .get(&module)
            .and_then(|texts| texts.get(&*name.item))
            .copied()
            .flatten()
            .map(|index| DefId::new(module, index))
            .ok_or_else(missing)
    }
}

/// The canonical bytes of the masks' inputs about one module's functions
/// ([`BuildFacts::about_module`]).
const ABOUT_MODULE_VERSION: u8 = 1;

impl BuildFacts {
    /// The build's facts: the union of its units' (D-969), each name found
    /// through `defs`, which must see every module the facts name (the
    /// whole build's interfaces do).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a name is not a def `defs` can see, or names two.
    pub fn union(parts: &[&UnitFacts], defs: &Defs<'_>) -> Result<Self, Ice> {
        let mut resolver = Resolver::new(defs);
        let mut facts = Self::default();
        for part in parts {
            let mut own = Self::default();
            for name in &part.values {
                own.values.insert(resolver.find(name)?);
            }
            for name in &part.foreign {
                own.foreign.insert(resolver.find(name)?);
            }
            for (name, &mask) in &part.tail_owned {
                own.tail_owned.insert(resolver.find(name)?, mask);
            }
            facts.merge(own);
        }
        Ok(facts)
    }

    /// The facts the masks of `module`'s functions depend on, as canonical
    /// bytes (D-969's "digest of the build's facts about the module's own
    /// functions"): which of its functions some unit uses as a value
    /// (rule 2) and the positions of each that other units' tail calls
    /// keep owned (rule 7), by item, sorted. Calls from other modules are
    /// left out: under D-787 they change no mask. `defs` must see
    /// `module`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `defs` cannot see a def of `module` that the facts
    /// name.
    pub fn about_module(
        &self,
        module: cheby_iface::ModuleId,
        defs: &Defs<'_>,
    ) -> Result<Vec<u8>, Ice> {
        let item = |def: DefId| FactName::of(def, defs).map(|name| name.item);
        let mut values: BTreeSet<Box<str>> = BTreeSet::new();
        for &def in self.values.iter().filter(|d| d.module == module) {
            values.insert(item(def)?);
        }
        let mut owned: BTreeMap<Box<str>, BorrowMask> = BTreeMap::new();
        for (&def, &mask) in self.tail_owned.iter().filter(|(d, _)| d.module == module) {
            owned.insert(item(def)?, mask);
        }
        let mut out = vec![ABOUT_MODULE_VERSION];
        put_len(&mut out, values.len());
        for text in &values {
            put_str(&mut out, text);
        }
        put_len(&mut out, owned.len());
        for (text, mask) in &owned {
            put_str(&mut out, text);
            out.extend_from_slice(&mask.0.to_le_bytes());
        }
        Ok(out)
    }
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
    let mut masks: Vec<BorrowMask> = module
        .functions
        .iter()
        .map(|f| initial_mask(f, own, facts, eligibility))
        .collect();
    let nodes = scan_all(module, &masks);
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

/// [`scan`] of every function of `module`, each eligible when its initial
/// mask borrows something.
fn scan_all(module: &MirModule, masks: &[BorrowMask]) -> Vec<Node> {
    module
        .functions
        .iter()
        .zip(masks)
        .map(|(f, mask)| scan(module, f, !mask.is_owned()))
        .collect()
}

/// The uses and tail calls of `f`, a function of `module`'s, parameters.
/// When `eligible` (some parameter may borrow), a parameter whose drop the
/// reuse pass would pair with an allocation counts as consumed
/// ([`crate::reuse::candidate_params`], D-841,
/// refining D-806): borrowed, it would never be dropped, so never reused.
fn scan(module: &MirModule, f: &Function, eligible: bool) -> Node {
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
    // An error here keeps the parameter borrowable: reuse is an
    // optimization, and borrowing stays sound either way.
    let reused = if eligible {
        crate::reuse::candidate_params(module, f).unwrap_or_default()
    } else {
        Vec::new()
    };
    for p in reused {
        if let Some(list) = scan
            .params
            .get(&p)
            .and_then(|&i| scan.node.consumers.get_mut(i))
        {
            list.push(Consumer::Always);
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
                func: RuntimeFn::ListSlice | RuntimeFn::ListUncons,
                args,
            } => {
                // The list at its last use: the RC pass would slice it in
                // place (`list_slice_owned` or `list_uncons_owned`, D-634,
                // D-909).
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
        block.retain_stmts(|s| match s {
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
