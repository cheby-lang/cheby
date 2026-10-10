//! Entry thunks (D-453): the C-ABI functions the host calls to run a
//! `main`, a test, a constant initializer or a REPL entry. They form their
//! own unit, so a module's code never depends on which entry runs it.
//!
//! A `main` or test that returns `Nil` exits with status 0. One that
//! returns `Result<Nil, E>` exits with 0 on `Ok`; on `Err(e)` it writes the
//! text of `e` and a newline to standard error, by `E`'s `show` when `E`
//! satisfies `Show` and by debug printing otherwise, with no prefix, and
//! exits with status 1 (§10.10, D-140, D-182, D-529).

use std::collections::BTreeSet;

use cheby_iface::{DefId, Ice, builtin};
use cheby_types::thir::{Dict, EntryInfo};

use crate::abi::{abi_sig, entry_sig};
use crate::builder::FunctionBuilder;
use crate::ir::{
    Callee, CmpOp, Const, EntryKind, FnKey, Function, LayoutRef, Local, Operand, RuntimeFn, Rvalue,
    Stmt, TagType, Terminator, UnitKind,
};
use crate::lower::{
    LowerCx, LowerError, Pending, Unsupported, closure, finish_unit, not_supported,
};
use crate::repr::{CType, ForeignSig, Repr};
use crate::{Abi, MirModule};

/// The exit status of a `main` or test that returned normally
/// (`cheby_runtime::abi::status::SUCCESS`; MIR does not depend on the
/// runtime).
const STATUS_SUCCESS: u64 = 0;

/// The exit status of a `main` or test that returned `Err`
/// (`cheby_runtime::abi::status::ERR`, D-182).
const STATUS_ERR: u64 = 1;

/// The kernel that writes a `String` and a newline to standard error
/// (`io::eprintln`).
const EPRINTLN: &str = "cheby_io_eprintln";

/// The largest arity of a closure the runtime calls back through a
/// trampoline (`cheby_runtime::Callbacks`, plan §4.3.4, D-526).
const MAX_TRAMPOLINE_ARITY: u32 = 4;

/// One entry thunk to build.
#[derive(Clone, Copy, Debug)]
pub struct EntryRequest<'a> {
    pub kind: EntryKind,
    /// How the body's result is reported; `None` for constants and REPL
    /// entries.
    pub info: Option<&'a EntryInfo>,
}

/// Builds the entries unit for `entries`, in the order given. The unit has
/// no reference counts yet ([`crate::count_and_check`]).
///
/// # Errors
///
/// E0038 for a `main` or test returning `Result` whose error type's
/// `show` needs dictionaries (W3-A), and an [`Ice`] for a REPL entry
/// (G5) or one whose item is not readable through `cx`.
pub fn lower_entries(
    entries: &[EntryRequest<'_>],
    cx: &LowerCx<'_>,
) -> Result<MirModule, LowerError> {
    let mut functions = Vec::with_capacity(entries.len());
    let mut unsupported = Unsupported::default();
    for req in entries {
        if let Some(f) = unsupported.take(entry_thunk(*req, cx))? {
            functions.push(f);
        }
    }
    // Nothing in MIR records which kernels call closures back (plan
    // §4.3.4), so every entries unit carries the trampolines of every
    // arity the runtime supports; the JIT defines each once (D-561).
    let arities: BTreeSet<u32> = (0..=MAX_TRAMPOLINE_ARITY).collect();
    functions.extend(closure::trampolines(&arities)?);
    let unit = finish_unit(
        UnitKind::Entries,
        functions,
        Vec::new(),
        Pending::default(),
        cx,
    )?;
    unsupported.finish(unit)
}

/// The thunk of one entry.
fn entry_thunk(req: EntryRequest<'_>, cx: &LowerCx<'_>) -> Result<Function, LowerError> {
    let (def, callee) = match req.kind {
        EntryKind::Main(def) => (def, Callee::Direct(def)),
        EntryKind::Test(def) => (def, Callee::Code(FnKey::Test(def))),
        EntryKind::Const(def) => (def, Callee::Code(FnKey::ConstInit(def))),
        EntryKind::Repl(_) => {
            return Err(Ice::new("REPL entries are not supported yet (G5)").into());
        }
    };
    let sig = entry_sig(req.kind);
    let mut b = FunctionBuilder::new(FnKey::Entry(req.kind), Abi::C, sig.ret, span_of(def, cx)?)?;
    if let Callee::Direct(main) = callee
        && !abi_sig(cx.defs.fn_sig(main)?, cx.defs.globals)
            .params
            .is_empty()
    {
        return Err(Ice::new("an entry point with parameters reached MIR lowering").into());
    }
    let result = b.assign(
        Repr::Value,
        Rvalue::Call {
            callee,
            args: Vec::new(),
        },
    )?;
    match (req.kind, req.info) {
        (EntryKind::Main(_) | EntryKind::Test(_), Some(EntryInfo::Nil)) => {
            // The `Nil` result is dropped by the RC pass.
            b.terminate(Terminator::Return(status(STATUS_SUCCESS)))?;
        }
        (EntryKind::Main(_) | EntryKind::Test(_), Some(EntryInfo::Result { show, .. })) => {
            report_result(&mut b, result, show.as_ref(), def, cx)?;
        }
        (EntryKind::Main(_) | EntryKind::Test(_), None) => {
            return Err(Ice::new("a `main` or test entry without its entry info").into());
        }
        (EntryKind::Const(_) | EntryKind::Repl(_), _) => {
            b.terminate(Terminator::Return(Operand::Local(result)))?;
        }
    }
    Ok(b.finish()?)
}

/// Ends the thunk for a body that returned the `Result` `result`: status
/// 0 on `Ok`; on `Err(e)` the text of `e` on standard error, then status
/// 1 (D-140, D-182, D-529).
fn report_result(
    b: &mut FunctionBuilder,
    result: Local,
    show: Option<&Dict>,
    def: DefId,
    cx: &LowerCx<'_>,
) -> Result<(), LowerError> {
    // The `show` to call, decided before any code is built.
    let show_fn = match show {
        None => None,
        Some(Dict::Impl { fns, .. }) => match fns.as_ref() {
            [f] if f.dicts.is_empty() => Some(f.def),
            _ => return Err(unsupported_show(def, cx)?),
        },
        Some(_) => return Err(unsupported_show(def, cx)?),
    };
    let tag = b.assign(
        Repr::Int,
        Rvalue::ReadTag {
            base: Operand::Local(result),
            ty: TagType::Result,
        },
    )?;
    let is_ok = b.assign(
        Repr::Bool,
        Rvalue::Compare {
            op: CmpOp::Eq,
            lhs: Operand::Local(tag),
            rhs: Operand::Const(Const::Int(0)),
            repr: Repr::Int,
        },
    )?;
    let ok = b.new_block()?;
    let err = b.new_block()?;
    b.terminate(Terminator::Branch {
        cond: Operand::Local(is_ok),
        then_: ok,
        else_: err,
    })?;
    b.switch_to(ok)?;
    b.terminate(Terminator::Return(status(STATUS_SUCCESS)))?;
    b.switch_to(err)?;
    let e = b.assign(
        Repr::Value,
        Rvalue::Field {
            base: result,
            layout: LayoutRef::Ctor(builtin::ERR),
            field: 0,
        },
    )?;
    let text = match show_fn {
        Some(show) => {
            let want = abi_sig(cx.defs.fn_sig(show)?, cx.defs.globals);
            let [repr] = want.params.as_slice() else {
                return Err(Ice::new(format!("`show` {show:?} without one parameter")).into());
            };
            let arg = if *repr == Repr::Value {
                Operand::Local(e)
            } else {
                Operand::Local(b.assign(
                    *repr,
                    Rvalue::Unbox {
                        value: Operand::Local(e),
                        to: *repr,
                    },
                )?)
            };
            b.assign(
                Repr::Value,
                Rvalue::Call {
                    callee: Callee::Direct(show),
                    args: vec![arg],
                },
            )?
        }
        None => b
            .call_runtime(RuntimeFn::Debug, vec![Operand::Local(e)])?
            .ok_or_else(|| Ice::new("`debug` without a result"))?,
    };
    b.push(Stmt::Eval(Rvalue::CallForeign {
        symbol: EPRINTLN.into(),
        sig: ForeignSig {
            params: vec![CType::Ptr],
            ret: CType::Void,
        },
        args: vec![Operand::Local(text)],
    }))?;
    b.terminate(Terminator::Return(status(STATUS_ERR)))?;
    Ok(())
}

/// E0038 at the entry's item for an error type whose `show` needs
/// dictionaries, which come with interface calls (W3-A).
fn unsupported_show(def: DefId, cx: &LowerCx<'_>) -> Result<LowerError, Ice> {
    Ok(LowerError::Unsupported(vec![not_supported(
        span_of(def, cx)?,
        "errors whose `show` needs interface dictionaries",
    )]))
}

/// The exit status `code` as the thunk's `i64` result.
const fn status(code: u64) -> Operand {
    Operand::Const(Const::Sized {
        repr: Repr::I64,
        bits: code,
    })
}

fn span_of(def: DefId, cx: &LowerCx<'_>) -> Result<cheby_diag::Span, Ice> {
    cx.defs
        .item(def)?
        .span
        .ok_or_else(|| Ice::new(format!("entry item {def:?} has no span")))
}
