//! THIR to MIR (D2).
//!
//! [`lower_fn`] is the parallel unit (D-263): it reads one item's THIR,
//! the module's own interface and its dependencies' interfaces, and
//! returns the item's function followed by its shims and closures, with
//! function-local strings. [`lower_module`] runs it per item in `DefIndex`
//! order and computes the unit's callee declarations, layouts and names.
//! [`lower_tests`] and [`lower_entries`] produce the other two unit kinds.
//!
//! Lowering assumes input that passed the E0038 gates (D-451):
//! anything outside the slice is an [`Ice`], never a diagnostic, so
//! `cheby check` and `cheby run` agree.

mod entry;
mod expr;
mod external;

use std::collections::BTreeSet;

use cheby_diag::{SourceMap, Span};
use cheby_iface::{DefId, Ice};
use cheby_types::thir;

pub use entry::{EntryRequest, lower_entries};

use crate::abi::abi_sig;
use crate::defs::Defs;
use crate::ir::{
    Callee, CalleeDecl, Const, EntryKind, FnKey, Function, LayoutRef, MirModule, Operand, Rvalue,
    Stmt, Terminator, UnitKind,
};
use crate::layout::{LayoutDesc, module_layouts};

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
}

/// Lowers one module's functions and constant initializers into its code
/// unit. The result has no reference counts yet ([`crate::count_and_check`]).
///
/// # Errors
///
/// An [`Ice`] on input outside the slice or inconsistent with the
/// interfaces.
pub fn lower_module(module: &thir::Module, cx: &LowerCx<'_>) -> Result<MirModule, Ice> {
    let mut fns: Vec<&thir::FnDef> = module.fns.iter().collect();
    fns.sort_by_key(|f| f.def);
    let mut consts: Vec<&thir::ConstDef> = module.consts.iter().collect();
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
        if take_fn {
            if let Some(f) = fi.next() {
                functions.extend(lower_fn(f, cx)?);
            }
        } else if let Some(c) = ci.next() {
            functions.push(expr::lower_body(
                FnKey::ConstInit(c.def),
                c.span,
                &c.body,
                0,
                cx,
            )?);
        }
    }
    let layouts = module_layouts(module.module, &cx.defs)?;
    finish_unit(UnitKind::Code(module.module), functions, layouts, cx)
}

/// Lowers one module's `test` blocks into its tests unit (D-265).
///
/// # Errors
///
/// As [`lower_module`].
pub fn lower_tests(module: &thir::Module, cx: &LowerCx<'_>) -> Result<MirModule, Ice> {
    let mut tests: Vec<&thir::TestDef> = module.tests.iter().collect();
    tests.sort_by_key(|t| t.def);
    let mut functions = Vec::new();
    for t in tests {
        functions.push(expr::lower_body(
            FnKey::Test(t.def),
            t.span,
            &t.body,
            0,
            cx,
        )?);
    }
    finish_unit(UnitKind::Tests(module.module), functions, Vec::new(), cx)
}

/// Lowers one top-level function: its body, or the Cheby-ABI wrapper of
/// its `@external(native, …)` (D-455).
///
/// # Errors
///
/// As [`lower_module`].
pub fn lower_fn(f: &thir::FnDef, cx: &LowerCx<'_>) -> Result<Vec<Function>, Ice> {
    let sig = cx.defs.fn_sig(f.def)?;
    let dicts = sig
        .generics
        .params
        .iter()
        .map(|p| p.bounds.len())
        .sum::<usize>();
    match (&f.native_external, &f.body) {
        (Some(ext), None) => Ok(vec![external::lower_wrapper(f, ext, sig, cx)?]),
        (None, Some(body)) => Ok(vec![expr::lower_body(
            FnKey::Item(f.def),
            f.span,
            body,
            dicts,
            cx,
        )?]),
        (Some(_), Some(_)) => Err(Ice::new(
            "a function with both a body and a native external reached MIR lowering",
        )),
        (None, None) => Err(Ice::new(
            "a function with neither a body nor a native external",
        )),
    }
}

/// Builds a unit from its functions: callee declarations, names, path.
fn finish_unit(
    unit: UnitKind,
    functions: Vec<Function>,
    layouts: Vec<LayoutDesc>,
    cx: &LowerCx<'_>,
) -> Result<MirModule, Ice> {
    let mut direct = BTreeSet::new();
    let mut mentioned = BTreeSet::new();
    for f in &functions {
        collect_defs(f, &mut direct, &mut mentioned);
    }
    for l in &layouts {
        if let LayoutRef::Ctor(d) = l.layout {
            mentioned.insert(d);
        }
    }
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
        names,
    })
}

/// The direct callees of `f`, and every other def it mentions.
fn collect_defs(f: &Function, direct: &mut BTreeSet<DefId>, mentioned: &mut BTreeSet<DefId>) {
    key_defs(f.key, mentioned);
    let operand = |op: &Operand, mentioned: &mut BTreeSet<DefId>| {
        if let Operand::Const(c) = op {
            match c {
                Const::StaticCtor(d) | Const::FnValue(d) => {
                    mentioned.insert(*d);
                }
                Const::Code(key) => key_defs(*key, mentioned),
                Const::Int(_)
                | Const::Float(_)
                | Const::Bool(_)
                | Const::Sized { .. }
                | Const::Str(_)
                | Const::Nil => {}
            }
        }
    };
    let callee = |c: &Callee, direct: &mut BTreeSet<DefId>, mentioned: &mut BTreeSet<DefId>| match c
    {
        Callee::Direct(d) => {
            direct.insert(*d);
        }
        Callee::Code(key) => key_defs(*key, mentioned),
        Callee::Indirect(_) => {}
    };
    for block in &f.blocks {
        for stmt in &block.stmts {
            let (Stmt::Assign { value, .. } | Stmt::Eval(value)) = stmt else {
                continue;
            };
            match value {
                Rvalue::Use(op)
                | Rvalue::Box { value: op, .. }
                | Rvalue::Unbox { value: op, .. } => {
                    operand(op, mentioned);
                }
                Rvalue::Call { callee: c, args } => {
                    callee(c, direct, mentioned);
                    for a in args {
                        operand(a, mentioned);
                    }
                }
                Rvalue::CallForeign { args, .. } => args.iter().for_each(|a| operand(a, mentioned)),
                Rvalue::CallRuntime { func, .. } => match *func {},
                Rvalue::Alloc { layout, fields } => {
                    if let LayoutRef::Ctor(d) = layout {
                        mentioned.insert(*d);
                    }
                    for a in fields {
                        operand(a, mentioned);
                    }
                }
                Rvalue::Field { layout, .. } => {
                    if let LayoutRef::Ctor(d) = layout {
                        mentioned.insert(*d);
                    }
                }
                Rvalue::LoadConst(d) => {
                    mentioned.insert(*d);
                }
                Rvalue::ReadLayout { .. } => {}
                Rvalue::Checked { lhs, rhs, .. } | Rvalue::Compare { lhs, rhs, .. } => {
                    operand(lhs, mentioned);
                    operand(rhs, mentioned);
                }
            }
        }
        match &block.term {
            Terminator::Return(op) => operand(op, mentioned),
            Terminator::TailCall { callee: c, args } => {
                callee(c, direct, mentioned);
                for a in args {
                    operand(a, mentioned);
                }
            }
            Terminator::Goto { args, .. } => args.iter().for_each(|a| operand(a, mentioned)),
            Terminator::Branch { cond, .. } => operand(cond, mentioned),
            Terminator::Switch { on, .. } => operand(on, mentioned),
            Terminator::Panic { message, .. } => {
                if let Some(m) = message {
                    operand(m, mentioned);
                }
            }
            Terminator::Unreachable => {}
        }
    }
}

fn key_defs(key: FnKey, out: &mut BTreeSet<DefId>) {
    match key {
        FnKey::Item(d)
        | FnKey::ValueShim(d)
        | FnKey::Closure { owner: d, .. }
        | FnKey::ConstInit(d)
        | FnKey::Test(d)
        | FnKey::Entry(EntryKind::Main(d) | EntryKind::Test(d) | EntryKind::Const(d)) => {
            out.insert(d);
        }
        FnKey::Entry(EntryKind::Repl(_)) | FnKey::Trampoline(_) => {}
    }
}
