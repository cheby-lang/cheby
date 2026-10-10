//! The MIR validator (D1), run after lowering and after RC
//! insertion in debug builds and tests. A failure is an internal compiler
//! error in the driver.
//!
//! Structure: one definition per local, every use dominated by its
//! definition, block parameters matching `goto` arguments, call arguments
//! matching callee signatures (direct callees from
//! [`MirModule::callees`]), representations of every operand and result,
//! and where the C ABI and tail calls may appear. After RC insertion
//! ([`Stage::Counted`]), ownership: on every path that returns or
//! tail-calls, each counted local is consumed exactly once more than it is
//! duplicated.

mod dom;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use cheby_iface::{Ice, Idx};

use self::dom::Dominance;

use crate::abi::{AbiSig, fixed_sig, is_uniform_sig};
use crate::ir::{
    Abi, ArithOp, BlockId, BuiltinLayout, Callee, CmpOp, Const, FnKey, Function, LayoutRef, Local,
    MirModule, Operand, RuntimeFn, Rvalue, StaticKind, StaticObject, Stmt, TagType, Terminator,
    UnitKind, UseKind,
};
use crate::layout::LayoutDesc;
use crate::repr::{CType, Repr};

/// Which invariants hold at this point of the pipeline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    /// After lowering: no `dup` or `drop` yet.
    Lowered,
    /// After [`crate::rc::insert`]: reference counts balance.
    Counted,
}

/// Checks `module`. Returns every problem found, each prefixed with the
/// function it is in.
///
/// # Errors
///
/// The list of problems, if any.
pub fn validate(module: &MirModule, stage: Stage) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    check_module(module, &mut errors);
    for f in &module.functions {
        let mut cx = FnCx {
            module,
            f,
            errors: Vec::new(),
        };
        if let Err(ice) = cx.check(stage) {
            cx.errors.push(format!("internal: {ice}"));
        }
        let name = key_label(f.key);
        errors.extend(cx.errors.into_iter().map(|e| format!("{name}: {e}")));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn key_label(key: FnKey) -> String {
    format!("{key:?}")
}

fn check_module(m: &MirModule, errors: &mut Vec<String>) {
    if !m
        .callees
        .windows(2)
        .all(|w| matches!(w, [a, b] if a.def < b.def))
    {
        errors.push("callees are not sorted by def without duplicates".to_owned());
    }
    if !m
        .names
        .windows(2)
        .all(|w| matches!(w, [a, b] if a.def < b.def))
    {
        errors.push("names are not sorted by def without duplicates".to_owned());
    }
    check_layouts(m, errors);
    for (n, s) in m.statics.iter().enumerate() {
        check_static(m, n, s, errors);
    }
    let mut keys = BTreeSet::new();
    for f in &m.functions {
        if !keys.insert(f.key) {
            errors.push(format!("{}: defined twice", key_label(f.key)));
        }
        if let FnKey::Item(def) = f.key
            && let Some(decl) = m.callee(def)
        {
            match f.sig() {
                Ok(sig) if sig == decl.sig => {}
                Ok(sig) => errors.push(format!(
                    "{}: signature {sig:?} differs from its callee declaration {:?}",
                    key_label(f.key),
                    decl.sig
                )),
                Err(ice) => errors.push(format!("{}: internal: {ice}", key_label(f.key))),
            }
        }
    }
}

/// Own layouts are constructors, listed once; foreign ones are sorted,
/// listed once and not own; every layout's counts match its fields.
fn check_layouts(m: &MirModule, errors: &mut Vec<String>) {
    let mut own = BTreeSet::new();
    for d in &m.layouts {
        if !own.insert(d.layout) {
            errors.push(format!("layout {:?} is listed twice", d.layout));
        }
    }
    if !m
        .foreign_layouts
        .windows(2)
        .all(|w| matches!(w, [a, b] if a.layout < b.layout))
    {
        errors.push("foreign layouts are not sorted without duplicates".to_owned());
    }
    for d in &m.foreign_layouts {
        if own.contains(&d.layout) {
            errors.push(format!("layout {:?} is both own and foreign", d.layout));
        }
    }
    for d in m.layouts.iter().chain(&m.foreign_layouts) {
        check_layout_desc(d, errors);
    }
}

fn check_layout_desc(d: &LayoutDesc, errors: &mut Vec<String>) {
    if !matches!(d.layout, LayoutRef::Ctor(_)) {
        errors.push(format!("layout {:?} is not a constructor", d.layout));
    }
    let values = d.fields.iter().filter(|f| f.repr == Repr::Value).count();
    let raws = d.fields.len().saturating_sub(values);
    if usize::try_from(d.value_fields).ok() != Some(values)
        || usize::try_from(d.raw_fields).ok() != Some(raws)
    {
        errors.push(format!(
            "layout {:?} counts {} value and {} raw fields, but has {values} and {raws}",
            d.layout, d.value_fields, d.raw_fields
        ));
    }
}

/// A static's id is its position, its name is unique, its layout fits
/// its kind, and its fields are uniform constants (D-523, plan §5.4).
fn check_static(m: &MirModule, n: usize, s: &StaticObject, errors: &mut Vec<String>) {
    let label = format!("static#{n}");
    if s.id.index() != n {
        errors.push(format!("{label}: has id {}", s.id.as_u32()));
    }
    if s.name.is_empty() || s.module.is_empty() {
        errors.push(format!("{label}: needs a module path and a name"));
    }
    if m.statics
        .iter()
        .take(n)
        .any(|o| o.kind == s.kind && o.module == s.module && o.name == s.name)
    {
        errors.push(format!("{label}: {}${} is defined twice", s.module, s.name));
    }
    let fits = matches!(
        (s.kind, s.layout),
        (StaticKind::Dict, LayoutRef::Dict { .. })
            | (
                StaticKind::Desc,
                LayoutRef::Desc { .. } | LayoutRef::DescNominal
            )
            | (StaticKind::FnValue, LayoutRef::Closure(_))
    );
    if !fits {
        errors.push(format!(
            "{label}: a {} static with layout {:?}",
            s.kind.name(),
            s.layout
        ));
    }
    let want = s.layout.uniform_fields().unwrap_or(0);
    if u64::try_from(s.fields.len()).ok() != Some(want) {
        errors.push(format!(
            "{label}: {} fields, its layout has {want}",
            s.fields.len()
        ));
    }
    for (i, c) in s.fields.iter().enumerate() {
        let code_slot = i == 0 && s.kind == StaticKind::FnValue;
        match c {
            Const::Code(key) if code_slot && key.is_uniform() => {}
            Const::Code(key) => {
                errors.push(format!("{label}: code {key:?} as field {i}"));
            }
            _ if code_slot => errors.push(format!("{label}: a function value without its code")),
            Const::Static(id) if id.index() == n || m.static_object(*id).is_none() => {
                errors.push(format!("{label}: field {i} is static#{}", id.as_u32()));
            }
            Const::Str(id) if s.strings.get(*id).is_none() => {
                errors.push(format!("{label}: unknown string str#{}", id.as_u32()));
            }
            c if c.repr() != Repr::Value => {
                errors.push(format!("{label}: field {i} is a raw {}", c.repr()));
            }
            _ => {}
        }
    }
}

struct FnCx<'a> {
    module: &'a MirModule,
    f: &'a Function,
    errors: Vec<String>,
}

/// Where a local is defined: its block and position (`None` for a
/// parameter, defined before the first statement).
type DefSite = (BlockId, Option<usize>);

impl FnCx<'_> {
    fn err(&mut self, message: impl Into<String>) {
        self.errors.push(message.into());
    }

    fn check(&mut self, stage: Stage) -> Result<(), Ice> {
        self.check_abi()?;
        let Some(defs) = self.check_definitions() else {
            return Ok(());
        };
        if !self.check_cfg() {
            return Ok(());
        }
        self.check_dominance(&defs)?;
        self.check_reprs(stage)?;
        if stage == Stage::Counted && self.errors.is_empty() {
            self.check_ownership()?;
        }
        Ok(())
    }

    fn check_abi(&mut self) -> Result<(), Ice> {
        let f = self.f;
        let entries = self.module.unit == UnitKind::Entries;
        match (f.key, f.abi) {
            (FnKey::Entry(_), Abi::C) if !entries => {
                self.err("entry thunks belong in an entries unit");
            }
            // In an entries unit, the static dictionary of an error
            // type's `show` brings value shims and tuple functions, which
            // depend on nothing but their key (D-523).
            (FnKey::Entry(_) | FnKey::Trampoline(_), Abi::C)
            | (FnKey::ValueShim(_) | FnKey::TupleDictFn { .. }, Abi::Cheby) => {}
            (FnKey::Entry(_) | FnKey::Trampoline(_), Abi::Cheby) => {
                self.err("entry thunks and trampolines use the C ABI");
            }
            (_, Abi::C) => self.err("only entry thunks and trampolines use the C ABI"),
            (_, Abi::Cheby) if entries => {
                self.err(
                    "an entries unit holds only entry thunks, trampolines, value shims and tuple functions",
                );
            }
            (_, Abi::Cheby) => {}
        }
        match f.abi {
            Abi::Cheby => {
                if f.ret != Repr::Value {
                    self.err("a Cheby-ABI function returns a uniform value");
                }
                if f.entry_loc.is_none() {
                    self.err("a Cheby-ABI function needs its entry-check location");
                }
            }
            Abi::C => {
                if f.entry_loc.is_some() {
                    self.err("a C-ABI function has no entry check");
                }
            }
        }
        if let Some(loc) = f.entry_loc
            && f.strings.get(loc).is_none()
        {
            self.err(format!("unknown string str#{}", loc.as_u32()));
        }
        if let Some(fixed) = fixed_sig(f.key) {
            let sig = f.sig()?;
            if sig != fixed {
                self.err(format!("signature {sig:?}, expected {fixed:?}"));
            }
        }
        if f.key.is_uniform() {
            let sig = f.sig()?;
            if !is_uniform_sig(&sig) {
                self.err(format!(
                    "signature {sig:?}, expected the uniform convention (D-453)"
                ));
            }
        }
        Ok(())
    }

    /// Every local defined exactly once; returns the definition sites.
    fn check_definitions(&mut self) -> Option<BTreeMap<Local, DefSite>> {
        let f = self.f;
        let mut sites: BTreeMap<Local, DefSite> = BTreeMap::new();
        let mut ok = true;
        let entry = BlockId::from_u32(0);
        let mut define = |cx: &mut Self, l: Local, site: DefSite| {
            if cx.f.locals.get(l).is_none() {
                cx.err(format!("unknown local _{}", l.as_u32()));
                ok = false;
            } else if sites.insert(l, site).is_some() {
                cx.err(format!("_{} is defined more than once", l.as_u32()));
                ok = false;
            }
        };
        for &p in &f.params {
            define(self, p, (entry, None));
        }
        for (id, block) in f.blocks.iter_enumerated() {
            for &p in &block.params {
                define(self, p, (id, None));
            }
            for (n, stmt) in block.stmts.iter().enumerate() {
                if let Stmt::Assign { dst, .. } = stmt {
                    define(self, *dst, (id, Some(n)));
                }
            }
        }
        ok.then_some(sites)
    }

    /// Block references exist, `bb0` is not a target and has no
    /// parameters, and `branch`/`switch` targets take no parameters.
    fn check_cfg(&mut self) -> bool {
        let f = self.f;
        let before = self.errors.len();
        let Some(entry) = f.blocks.get(BlockId::from_u32(0)) else {
            self.err("a function needs an entry block");
            return false;
        };
        if !entry.params.is_empty() {
            self.err("bb0 takes no parameters");
        }
        for (id, block) in f.blocks.iter_enumerated() {
            for succ in block.term.successors() {
                let Some(target) = f.blocks.get(succ) else {
                    self.err(format!(
                        "bb{} jumps to unknown bb{}",
                        id.as_u32(),
                        succ.as_u32()
                    ));
                    continue;
                };
                if succ.as_u32() == 0 {
                    self.err(format!("bb{} jumps to the entry block", id.as_u32()));
                }
                let branches = matches!(
                    block.term,
                    Terminator::Branch { .. } | Terminator::Switch { .. }
                );
                if branches && !target.params.is_empty() {
                    self.err(format!(
                        "bb{} branches to bb{}, which takes parameters",
                        id.as_u32(),
                        succ.as_u32()
                    ));
                }
            }
        }
        self.errors.len() == before
    }

    fn check_dominance(&mut self, defs: &BTreeMap<Local, DefSite>) -> Result<(), Ice> {
        let f = self.f;
        let dom = Dominance::new(f)?;
        for (id, block) in f.blocks.iter_enumerated() {
            // Unreachable blocks have no dominators; their uses are not
            // checked.
            if !dom.is_reachable(id) {
                continue;
            }
            let check = |cx: &mut Self, l: Local, at: Option<usize>| {
                let Some(&(def_block, def_pos)) = defs.get(&l) else {
                    cx.err(format!("_{} is used but never defined", l.as_u32()));
                    return;
                };
                let ok = if def_block == id {
                    match (def_pos, at) {
                        (Some(d), Some(u)) => d < u,
                        (None, _) | (Some(_), None) => true,
                    }
                } else {
                    dom.dominates(def_block, id)
                };
                if !ok {
                    cx.err(format!(
                        "_{} is used in bb{} where its definition does not dominate",
                        l.as_u32(),
                        id.as_u32()
                    ));
                }
            };
            for (n, stmt) in block.stmts.iter().enumerate() {
                for l in stmt_locals(stmt) {
                    check(self, l, Some(n));
                }
            }
            for (l, _) in block.term.uses() {
                check(self, l, None);
            }
        }
        Ok(())
    }

    fn repr(&mut self, op: &Operand) -> Option<Repr> {
        let repr = self.f.operand_repr(op).ok();
        if repr.is_none() {
            self.err(format!("unknown operand {op:?}"));
        }
        repr
    }

    fn expect_repr(&mut self, op: &Operand, want: Repr, what: &str) {
        if let Some(got) = self.repr(op)
            && got != want
        {
            self.err(format!("{what}: expected {want}, found {got}"));
        }
        self.check_const(op, false);
    }

    fn check_const(&mut self, op: &Operand, code_allowed: bool) {
        match op {
            Operand::Const(Const::Code(_)) if !code_allowed => {
                self.err("a code word appears only as a closure's first field");
            }
            Operand::Const(Const::Str(id)) if self.f.strings.get(*id).is_none() => {
                self.err(format!("unknown string str#{}", id.as_u32()));
            }
            Operand::Const(Const::Sized { repr, .. }) if !repr.is_sized() => {
                self.err(format!("a sized constant of representation {repr}"));
            }
            Operand::Const(Const::Static(id)) if self.module.static_object(*id).is_none() => {
                self.err(format!("unknown static#{}", id.as_u32()));
            }
            _ => {}
        }
    }

    fn callee_sig(&mut self, callee: &Callee) -> Option<AbiSig> {
        let sig = match callee {
            Callee::Direct(def) => {
                let sig = self.module.callee(*def).map(|d| d.sig.clone());
                if sig.is_none() {
                    self.err(format!("direct callee {def:?} has no declaration"));
                }
                sig
            }
            Callee::Code(key) => {
                let sig = self
                    .module
                    .function(*key)
                    .and_then(|f| f.sig().ok())
                    .or_else(|| fixed_sig(*key));
                if sig.is_none() {
                    self.err(format!("callee {key:?} is not in this unit"));
                }
                sig
            }
            Callee::Indirect(l) => {
                let repr = self.f.local(*l).map(|d| d.repr).ok();
                if repr != Some(Repr::Value) {
                    self.err(format!("indirect callee _{} is not a value", l.as_u32()));
                }
                None
            }
        };
        if let Some(sig) = &sig
            && sig.abi != Abi::Cheby
        {
            self.err(format!("call of a C-ABI function {callee:?}"));
        }
        sig
    }

    fn check_call(&mut self, callee: &Callee, args: &[Operand]) {
        if let Callee::Indirect(_) = callee {
            self.callee_sig(callee);
            for arg in args {
                self.expect_repr(arg, Repr::Value, "indirect call argument");
            }
            return;
        }
        let Some(sig) = self.callee_sig(callee) else {
            return;
        };
        if sig.params.len() != args.len() {
            self.err(format!(
                "call of {callee:?} with {} arguments, expected {}",
                args.len(),
                sig.params.len()
            ));
            return;
        }
        for (arg, &want) in args.iter().zip(&sig.params) {
            self.expect_repr(arg, want, "call argument");
        }
    }

    /// The representation the rvalue produces, after checking its operands.
    fn check_rvalue(&mut self, value: &Rvalue) -> Option<Repr> {
        match value {
            Rvalue::Use(op) => {
                self.check_const(op, false);
                self.repr(op)
            }
            Rvalue::Call { callee, args } => {
                self.check_call(callee, args);
                Some(Repr::Value)
            }
            Rvalue::CallForeign { sig, args, .. } => {
                if sig.params.len() != args.len() {
                    self.err("foreign call with the wrong number of arguments");
                }
                if sig.params.contains(&CType::Void) {
                    self.err("a foreign parameter of type void");
                }
                for (arg, &ctype) in args.iter().zip(&sig.params) {
                    self.check_const(arg, false);
                    if let Some(r) = self.repr(arg)
                        && !ctype.accepts(r)
                    {
                        self.err(format!("foreign argument {r} passed as {ctype}"));
                    }
                }
                sig.ret.repr()
            }
            Rvalue::CallRuntime { func, args } => self.check_runtime(*func, args),
            Rvalue::Alloc { layout, fields } => {
                self.check_alloc(*layout, fields);
                Some(Repr::Value)
            }
            Rvalue::Field {
                base,
                layout,
                field,
            } => {
                self.expect_repr(&Operand::Local(*base), Repr::Value, "field base");
                self.field_repr(*layout, *field)
            }
            Rvalue::ReadLayout { base } => {
                self.expect_repr(&Operand::Local(*base), Repr::Value, "read_layout base");
                Some(Repr::U32)
            }
            Rvalue::LoadConst(_) => Some(Repr::Value),
            Rvalue::Box { value, from } => {
                if *from == Repr::Value {
                    self.err("boxing a value that is already uniform");
                }
                self.expect_repr(value, *from, "box operand");
                Some(Repr::Value)
            }
            Rvalue::Unbox { value, to } => {
                if *to == Repr::Value {
                    self.err("unboxing into a uniform value");
                }
                self.expect_repr(value, Repr::Value, "unbox operand");
                Some(*to)
            }
            Rvalue::ReadTag { base, ty } => {
                self.expect_repr(base, Repr::Value, "read_tag base");
                self.check_tag_type(*ty);
                Some(Repr::Int)
            }
            Rvalue::Checked { .. }
            | Rvalue::Compare { .. }
            | Rvalue::FloatArith { .. }
            | Rvalue::Bitwise { .. }
            | Rvalue::FloatNeg { .. }
            | Rvalue::Not(_) => self.check_arith(value),
        }
    }

    /// Arithmetic, comparisons and the other operators on raw values
    /// (plan §4.1).
    fn check_arith(&mut self, value: &Rvalue) -> Option<Repr> {
        let integer = |r: Repr| r.is_signed_int() || r.is_unsigned_int();
        let float = |r: Repr| matches!(r, Repr::Float | Repr::F32);
        match value {
            Rvalue::Checked {
                op,
                lhs,
                rhs,
                repr,
                loc,
            } => {
                if self.f.strings.get(*loc).is_none() {
                    self.err(format!("unknown string str#{}", loc.as_u32()));
                }
                if !integer(*repr) {
                    self.err(format!("checked {} on {repr}", op.name()));
                }
                self.expect_repr(lhs, *repr, "checked operand");
                self.expect_repr(rhs, *repr, "checked operand");
                Some(*repr)
            }
            Rvalue::Compare { op, lhs, rhs, repr } => {
                if *repr == Repr::Value {
                    self.err("compare on uniform values");
                }
                if *repr == Repr::Bool && !matches!(op, CmpOp::Eq | CmpOp::Ne) {
                    self.err(format!("compare {} on bool", op.name()));
                }
                self.expect_repr(lhs, *repr, "compare operand");
                self.expect_repr(rhs, *repr, "compare operand");
                Some(Repr::Bool)
            }
            Rvalue::FloatArith { op, lhs, rhs, repr } => {
                if !float(*repr) {
                    self.err(format!("float_arith {} on {repr}", op.name()));
                }
                if matches!(op, ArithOp::Shl | ArithOp::Shr) {
                    self.err(format!("float_arith {}", op.name()));
                }
                self.expect_repr(lhs, *repr, "float operand");
                self.expect_repr(rhs, *repr, "float operand");
                Some(*repr)
            }
            Rvalue::Bitwise { op, lhs, rhs, repr } => {
                if !integer(*repr) {
                    self.err(format!("bitwise {} on {repr}", op.name()));
                }
                self.expect_repr(lhs, *repr, "bitwise operand");
                self.expect_repr(rhs, *repr, "bitwise operand");
                Some(*repr)
            }
            Rvalue::FloatNeg { value, repr } => {
                if !float(*repr) {
                    self.err(format!("float_neg on {repr}"));
                }
                self.expect_repr(value, *repr, "float_neg operand");
                Some(*repr)
            }
            Rvalue::Not(value) => {
                self.expect_repr(value, Repr::Bool, "not operand");
                Some(Repr::Bool)
            }
            _ => None,
        }
    }

    fn check_runtime(&mut self, func: RuntimeFn, args: &[Operand]) -> Option<Repr> {
        let params = func.params();
        if params.len() != args.len() {
            self.err(format!(
                "call_runtime {} with {} arguments, expected {}",
                func.name(),
                args.len(),
                params.len()
            ));
        }
        for (arg, &(want, _)) in args.iter().zip(params) {
            self.expect_repr(arg, want, "runtime argument");
        }
        // The location of a structural `==` is a static string (D-556).
        if func == RuntimeFn::Equal
            && let Some(location) = args.get(2)
            && !matches!(location, Operand::Const(Const::Str(_)))
        {
            self.err("call_runtime equal: the location is not a string constant".to_owned());
        }
        func.ret()
    }

    /// A tag is read only from a type with several constructors (D-522),
    /// whose layouts the unit holds in tag order.
    fn check_tag_type(&mut self, ty: TagType) {
        let TagType::Adt(def) = ty else {
            return;
        };
        let tags: Vec<u32> = self
            .module
            .type_layouts(def)
            .iter()
            .map(|d| d.tag)
            .collect();
        if tags.len() < 2 {
            self.err(format!(
                "read_tag of {def:?}, which has {} known constructor(s)",
                tags.len()
            ));
        } else if !tags
            .iter()
            .copied()
            .eq(0..u32::try_from(tags.len()).unwrap_or(0))
        {
            self.err(format!("read_tag of {def:?} without all its constructors"));
        }
    }

    /// The representation of field `field` of `layout`.
    fn field_repr(&mut self, layout: LayoutRef, field: u32) -> Option<Repr> {
        match layout {
            LayoutRef::Ctor(def) => {
                let Some(desc) = self.module.layout(layout) else {
                    self.err(format!("no layout for constructor {def:?}"));
                    return None;
                };
                let found = usize::try_from(field)
                    .ok()
                    .and_then(|n| desc.fields.get(n))
                    .map(|f| f.repr);
                if found.is_none() {
                    self.err(format!(
                        "field {field} of {def:?}, which has {}",
                        desc.fields.len()
                    ));
                }
                found
            }
            LayoutRef::Closure(_) if field == 0 => {
                self.err("field 0 of a closure is its code word, not a value");
                None
            }
            LayoutRef::Builtin(_) => {
                self.err(format!("field of a runtime-defined object {layout:?}"));
                None
            }
            LayoutRef::Tuple(_)
            | LayoutRef::Closure(_)
            | LayoutRef::Dict { .. }
            | LayoutRef::Dyn
            | LayoutRef::Desc { .. }
            | LayoutRef::DescNominal => {
                let count = layout.uniform_fields().unwrap_or(0);
                if u64::from(field) >= count {
                    self.err(format!("field {field} of {layout:?}, which has {count}"));
                }
                Some(Repr::Value)
            }
        }
    }

    fn check_alloc(&mut self, layout: LayoutRef, fields: &[Operand]) {
        match layout {
            LayoutRef::Closure(n) => {
                let Some((code, captures)) = fields.split_first() else {
                    self.err("a closure needs its code word");
                    return;
                };
                match code {
                    Operand::Const(Const::Code(key)) if key.is_uniform() => {}
                    Operand::Const(Const::Code(key)) => {
                        self.err(format!("a closure's code {key:?} is not uniform"));
                    }
                    _ => self.err("a closure's first field is its code word"),
                }
                if usize::try_from(n).ok() != Some(captures.len()) {
                    self.err("closure capture count differs from its layout");
                }
                for c in captures {
                    self.expect_repr(c, Repr::Value, "closure capture");
                }
            }
            LayoutRef::Tuple(_)
            | LayoutRef::Dict { .. }
            | LayoutRef::Dyn
            | LayoutRef::Desc { .. } => {
                let want = layout.uniform_fields().unwrap_or(0);
                if u64::try_from(fields.len()).ok() != Some(want) {
                    self.err(format!(
                        "alloc {layout:?} with {} fields, expected {want}",
                        fields.len()
                    ));
                }
                for c in fields {
                    self.expect_repr(c, Repr::Value, "field");
                }
            }
            LayoutRef::DescNominal => {
                self.err("a nominal descriptor is only a static object (D-524)");
            }
            LayoutRef::Ctor(def) => {
                let Some(desc) = self.module.layout(layout) else {
                    self.err(format!("no layout for constructor {def:?}"));
                    return;
                };
                let want: Vec<Repr> = desc.fields.iter().map(|f| f.repr).collect();
                if want.is_empty() {
                    self.err(format!(
                        "alloc of the unit constructor {def:?}, which is a static object"
                    ));
                }
                if want.len() != fields.len() {
                    self.err(format!(
                        "alloc of {def:?} with {} fields, expected {}",
                        fields.len(),
                        want.len()
                    ));
                }
                for (c, r) in fields.iter().zip(want) {
                    self.expect_repr(c, r, "constructor field");
                }
            }
            LayoutRef::Builtin(b) => {
                let want = match b {
                    BuiltinLayout::BoxedInt => Repr::Int,
                    BuiltinLayout::BoxedFloat => Repr::Float,
                    BuiltinLayout::BoxedF32 => Repr::F32,
                    BuiltinLayout::BoxedU64 => Repr::U64,
                    BuiltinLayout::String | BuiltinLayout::Bytes => {
                        self.err(format!("alloc of {b:?}, which the runtime builds"));
                        return;
                    }
                };
                if fields.len() != 1 {
                    self.err(format!("alloc of {b:?} with {} fields", fields.len()));
                }
                for c in fields {
                    self.expect_repr(c, want, "boxed number");
                }
            }
        }
    }

    fn check_reprs(&mut self, stage: Stage) -> Result<(), Ice> {
        let f = self.f;
        for (id, block) in f.blocks.iter_enumerated() {
            for stmt in &block.stmts {
                match stmt {
                    Stmt::Assign { dst, value } => {
                        if is_void_call(value) {
                            self.err("a void call cannot define a local");
                        }
                        let dst_repr = f.local(*dst)?.repr;
                        if let Some(r) = self.check_rvalue(value)
                            && r != dst_repr
                            && !foreign_i64(value, dst_repr)
                        {
                            self.err(format!(
                                "_{} has representation {dst_repr} but is assigned a {r}",
                                dst.as_u32()
                            ));
                        }
                    }
                    Stmt::Eval(value) => {
                        if !is_void_call(value) {
                            self.err("only a void call is evaluated for effect");
                        }
                        self.check_rvalue(value);
                    }
                    Stmt::Dup(l) | Stmt::Drop(l) => {
                        if stage == Stage::Lowered {
                            self.err("dup or drop before reference-count insertion");
                        }
                        if f.local(*l)?.repr != Repr::Value {
                            self.err(format!("dup or drop of uncounted _{}", l.as_u32()));
                        }
                    }
                }
            }
            self.check_terminator(id, &block.term)?;
        }
        Ok(())
    }

    fn check_terminator(&mut self, id: BlockId, term: &Terminator) -> Result<(), Ice> {
        let f = self.f;
        match term {
            Terminator::Return(op) => self.expect_repr(op, f.ret, "returned value"),
            Terminator::TailCall { callee, args } => {
                if f.abi != Abi::Cheby {
                    self.err("a tail call in a C-ABI function");
                }
                self.check_call(callee, args);
            }
            Terminator::Goto { target, args } => {
                let params = f.block(*target)?.params.clone();
                if params.len() != args.len() {
                    self.err(format!(
                        "bb{} passes {} arguments to bb{}, which takes {}",
                        id.as_u32(),
                        args.len(),
                        target.as_u32(),
                        params.len()
                    ));
                }
                for (arg, p) in args.iter().zip(params) {
                    let want = f.local(p)?.repr;
                    self.expect_repr(arg, want, "goto argument");
                }
            }
            Terminator::Branch { cond, .. } => {
                self.expect_repr(cond, Repr::Bool, "branch condition");
            }
            Terminator::Switch { on, cases, .. } => {
                if let Some(r) = self.repr(on)
                    && !(r == Repr::U32 || r.is_signed_int() || r.is_unsigned_int())
                {
                    self.err(format!("switch on {r}"));
                }
                let mut seen = BTreeSet::new();
                for (v, _) in cases {
                    if !seen.insert(*v) {
                        self.err(format!("switch case {v} appears twice"));
                    }
                }
            }
            Terminator::Panic { message, loc, .. } => {
                if f.strings.get(*loc).is_none() {
                    self.err(format!("unknown string str#{}", loc.as_u32()));
                }
                if let Some(m) = message {
                    self.expect_repr(m, Repr::Value, "panic message");
                }
            }
            Terminator::Unreachable => {}
        }
        Ok(())
    }

    /// Reference counts balance on every path (D7).
    fn check_ownership(&mut self) -> Result<(), Ice> {
        let f = self.f;
        let counted = |l: Local| f.local(l).map(|d| d.repr == Repr::Value);
        let mut entry_state: BTreeMap<BlockId, Counts> = BTreeMap::new();
        let mut start = Counts::default();
        for &p in &f.params {
            if counted(p)? {
                start.held.insert(p, 1);
            }
        }
        let bb0 = BlockId::from_u32(0);
        entry_state.insert(bb0, start);
        let mut work = vec![bb0];
        let mut visited = BTreeSet::new();
        while let Some(id) = work.pop() {
            if !visited.insert(id) {
                continue;
            }
            let block = f.block(id)?;
            let mut st = entry_state.get(&id).cloned().unwrap_or_default();
            for &p in &block.params {
                if counted(p)? {
                    st.held.insert(p, 1);
                }
            }
            let at = |n: usize| format!("bb{} statement {n}", id.as_u32());
            for (n, stmt) in block.stmts.iter().enumerate() {
                match stmt {
                    Stmt::Assign { dst, value } => {
                        self.consume(&mut st, &value.uses(), &at(n))?;
                        if counted(*dst)? {
                            if value.result_is_borrowed() {
                                st.held.insert(*dst, 0);
                                let base = match value {
                                    Rvalue::Field { base, .. } => Some(*base),
                                    _ => None,
                                };
                                st.borrowed_from.insert(*dst, base);
                            } else {
                                st.held.insert(*dst, 1);
                            }
                        }
                    }
                    Stmt::Eval(value) => self.consume(&mut st, &value.uses(), &at(n))?,
                    Stmt::Dup(l) => {
                        // A `field` or `load_const` result holds nothing
                        // until its dup, which needs the base still alive.
                        let ok = match (st.held.get(l).copied(), st.borrowed_from.get(l)) {
                            (Some(c), _) if c >= 1 => true,
                            (Some(0), Some(Some(base))) => {
                                st.held.get(base).is_some_and(|&c| c >= 1)
                            }
                            (Some(0), Some(None)) => true,
                            _ => false,
                        };
                        match st.held.get_mut(l) {
                            Some(c) if ok => *c = c.saturating_add(1),
                            _ => self.err(format!(
                                "{}: dup of _{} which holds no reference",
                                at(n),
                                l.as_u32()
                            )),
                        }
                    }
                    Stmt::Drop(l) => self.consume(&mut st, &[(*l, UseKind::Owned)], &at(n))?,
                }
            }
            let here = format!("bb{} terminator", id.as_u32());
            match &block.term {
                Terminator::Return(_) | Terminator::TailCall { .. } => {
                    self.consume(&mut st, &block.term.uses(), &here)?;
                    for (l, c) in &st.held {
                        if *c != 0 {
                            self.err(format!(
                                "{here}: _{} still holds {c} reference(s)",
                                l.as_u32()
                            ));
                        }
                    }
                }
                Terminator::Goto { target, .. } => {
                    self.consume(&mut st, &block.term.uses(), &here)?;
                    self.flow(&mut entry_state, &mut work, *target, &st);
                }
                Terminator::Branch { .. } | Terminator::Switch { .. } => {
                    self.consume(&mut st, &block.term.uses(), &here)?;
                    for succ in block.term.successors() {
                        self.flow(&mut entry_state, &mut work, succ, &st);
                    }
                }
                Terminator::Panic { .. } | Terminator::Unreachable => {}
            }
        }
        Ok(())
    }

    fn flow(
        &mut self,
        entry_state: &mut BTreeMap<BlockId, Counts>,
        work: &mut Vec<BlockId>,
        target: BlockId,
        st: &Counts,
    ) {
        let mut normalized = st.clone();
        normalized.held.retain(|_, c| *c != 0);
        normalized
            .borrowed_from
            .retain(|l, _| normalized.held.contains_key(l));
        match entry_state.get(&target) {
            Some(existing) if *existing != normalized => self.err(format!(
                "reference counts differ between the predecessors of bb{}",
                target.as_u32()
            )),
            Some(_) => {}
            None => {
                entry_state.insert(target, normalized);
                work.push(target);
            }
        }
    }

    /// Applies the uses of one instruction: borrowed uses need a reference
    /// that survives the instruction, owned uses consume one each.
    fn consume(&mut self, st: &mut Counts, uses: &[(Local, UseKind)], at: &str) -> Result<(), Ice> {
        let mut per: BTreeMap<Local, (i64, bool)> = BTreeMap::new();
        for &(l, kind) in uses {
            if self.f.local(l)?.repr != Repr::Value {
                continue;
            }
            let e = per.entry(l).or_insert((0, false));
            match kind {
                UseKind::Owned => e.0 = e.0.saturating_add(1),
                UseKind::Borrowed => e.1 = true,
            }
        }
        for (l, (owned, borrowed)) in per {
            let have = st.held.get(&l).copied();
            let need = owned.saturating_add(i64::from(borrowed));
            match have {
                Some(c) if c >= need && need > 0 => {
                    st.held.insert(l, c.saturating_sub(owned));
                }
                _ => {
                    let mut msg = format!("{at}: _{} is used without a reference", l.as_u32());
                    if let Some(c) = have {
                        let _ = write!(msg, " (holds {c}, needs {need})");
                    }
                    self.err(msg);
                }
            }
        }
        Ok(())
    }
}

/// The references each counted local holds on one path.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct Counts {
    held: BTreeMap<Local, i64>,
    /// Locals defined by `field` (with their base) or `load_const`.
    borrowed_from: BTreeMap<Local, Option<Local>>,
}

/// Whether `value` is a call without a result: a foreign function
/// returning `void` or a runtime helper returning nothing.
fn is_void_call(value: &Rvalue) -> bool {
    match value {
        Rvalue::CallForeign { sig, .. } => sig.ret == CType::Void,
        Rvalue::CallRuntime { func, .. } => func.ret().is_none(),
        _ => false,
    }
}

/// Whether a foreign `int64_t` result lands in an `I64` local.
fn foreign_i64(value: &Rvalue, dst: Repr) -> bool {
    matches!(value, Rvalue::CallForeign { sig, .. } if sig.ret == CType::I64 && dst == Repr::I64)
}

/// The locals a statement reads, Dup and Drop included.
fn stmt_locals(stmt: &Stmt) -> Vec<Local> {
    match stmt {
        Stmt::Assign { value, .. } | Stmt::Eval(value) => {
            value.uses().into_iter().map(|(l, _)| l).collect()
        }
        Stmt::Dup(l) | Stmt::Drop(l) => vec![*l],
    }
}
