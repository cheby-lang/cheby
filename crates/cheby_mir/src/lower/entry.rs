//! Entry thunks (D-453): the C-ABI functions the host calls to run a
//! `main`, a test, a constant initializer or a REPL entry. They form their
//! own unit, so a module's code never depends on which entry runs it.
//!
//! A `main` that returns `Nil` exits with status 0. One that returns
//! `Result<Nil, E>` exits with 0 on `Ok`; on `Err(e)` it writes the text
//! of `e` and a newline to standard error, by `E`'s `show` when `E`
//! satisfies `Show` and by debug printing otherwise, with no prefix, and
//! exits with status 1 (§10.10, D-140, D-182, D-529).
//!
//! A test thunk prints nothing: it returns the static `Nil` when the test
//! passed, and on `Err(e)` the text of `e` as an owned `String`, made as
//! for `main`, which `cheby test` shows in the test's failure block
//! (D-541). GAP(test-err-thunk).
//!
//! An error type whose `show` needs dictionaries (`List<E>`, a tuple, a
//! generic user type, D-064, D-167) is called through its static
//! dictionary (D-524), whose symbol carries the entry item's module
//! (the entries unit has none). GAP(static-dict-names).

use std::collections::BTreeSet;

use cheby_iface::{DefId, Ice, builtin};
use cheby_types::thir::{Dict, EntryInfo};

use crate::abi::{abi_sig, entry_sig};
use crate::builder::FunctionBuilder;
use crate::ir::{
    Callee, CmpOp, Const, EntryKind, FnKey, Function, LayoutRef, Local, Operand, RuntimeFn, Rvalue,
    Stmt, TagType, Terminator, UnitKind,
};
use crate::lower::dict::{IfaceShape, Statics, dict_closed, dict_iface, dict_mentions_dyn};
use crate::lower::{
    LowerCx, LowerError, Pending, Unsupported, closure, finish_unit, not_supported,
};
use crate::repr::{CType, ForeignSig, Repr};
use crate::{Abi, MirModule};

/// The exit status of a `main` that returned normally
/// (`cheby_runtime::abi::status::SUCCESS`; MIR does not depend on the
/// runtime).
const STATUS_SUCCESS: u64 = 0;

/// The exit status of a `main` that returned `Err`
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
/// E0038 for a `main` or test returning `Result` whose error type
/// mentions `dyn` (W4-A), and an [`Ice`] for a REPL entry (G5) or one
/// whose item is not readable through `cx`.
pub fn lower_entries(
    entries: &[EntryRequest<'_>],
    cx: &LowerCx<'_>,
) -> Result<MirModule, LowerError> {
    let mut functions = Vec::with_capacity(entries.len());
    let mut unsupported = Unsupported::default();
    let mut pending = Pending::default();
    for req in entries {
        if let Some(f) = unsupported.take(entry_thunk(*req, cx, &mut pending))? {
            functions.push(f);
        }
    }
    // Nothing in MIR records which kernels call closures back (plan
    // §4.3.4), so every entries unit carries the trampolines of every
    // arity the runtime supports; the JIT defines each once (D-561).
    let arities: BTreeSet<u32> = (0..=MAX_TRAMPOLINE_ARITY).collect();
    functions.extend(closure::trampolines(&arities)?);
    let unit = finish_unit(UnitKind::Entries, functions, Vec::new(), pending, cx)?;
    unsupported.finish(unit)
}

/// The thunk of one entry; the static dictionaries it needs go to
/// `pending`.
fn entry_thunk(
    req: EntryRequest<'_>,
    cx: &LowerCx<'_>,
    pending: &mut Pending,
) -> Result<Function, LowerError> {
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
    let is_test = matches!(req.kind, EntryKind::Test(_));
    match (req.kind, req.info) {
        (EntryKind::Main(_) | EntryKind::Test(_), Some(EntryInfo::Nil)) => {
            // The `Nil` result is dropped by the RC pass.
            b.terminate(Terminator::Return(passed(is_test)))?;
        }
        (EntryKind::Main(_) | EntryKind::Test(_), Some(EntryInfo::Result { show, .. })) => {
            let show = show_call(show.as_ref(), def, cx, pending)?;
            report_result(&mut b, result, &show, is_test, cx)?;
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

/// What a `main` or test returns when it passed: status 0 for `main`, the
/// static `Nil` for a test (GAP(test-err-thunk)).
const fn passed(is_test: bool) -> Operand {
    if is_test {
        Operand::Const(Const::Nil)
    } else {
        status(STATUS_SUCCESS)
    }
}

/// How the text of an `Err` value is made.
enum ShowCall {
    /// Debug printing: the error type does not satisfy `Show`.
    Debug,
    /// A direct call of a `show` that needs no dictionaries.
    Direct(DefId),
    /// A call of the closure in `field` of the static dictionary `dict`
    /// (of layout `layout`).
    Dict {
        dict: Const,
        layout: LayoutRef,
        field: u32,
    },
}

/// The `show` to call for the error type, decided before any code is
/// built; its static dictionary goes to `pending`.
fn show_call(
    show: Option<&Dict>,
    def: DefId,
    cx: &LowerCx<'_>,
    pending: &mut Pending,
) -> Result<ShowCall, LowerError> {
    let Some(dict) = show else {
        return Ok(ShowCall::Debug);
    };
    if let Dict::Impl { fns, .. } = dict
        && let [f] = fns.as_ref()
        && f.dicts.is_empty()
    {
        return Ok(ShowCall::Direct(f.def));
    }
    if dict_mentions_dyn(cx, dict) || !dict_closed(cx, dict) {
        // A `main` or test has no type parameters, so only a `dyn`
        // error type gets here (W4-A).
        return Err(LowerError::Unsupported(vec![not_supported(
            span_of(def, cx)?,
            "errors whose `show` needs the dictionary of a `dyn` value",
        )]));
    }
    let module = cx.defs.name(def)?.module;
    let mut statics = Statics {
        cx,
        pending,
        module: &module,
    };
    let static_dict = statics.dict(dict)?;
    let shape = IfaceShape::of(cx, dict_iface(dict, None)?)?;
    Ok(ShowCall::Dict {
        dict: static_dict,
        layout: shape.layout()?,
        field: shape.fn_field(cx, builtin::SHOW_FN)?,
    })
}

/// Ends the thunk for a body that returned the `Result` `result`: passed
/// on `Ok`; on `Err(e)` the text of `e`, which `main` writes on standard
/// error before returning status 1 and a test returns (D-140, D-182,
/// D-529, D-541).
fn report_result(
    b: &mut FunctionBuilder,
    result: Local,
    show: &ShowCall,
    is_test: bool,
    cx: &LowerCx<'_>,
) -> Result<(), LowerError> {
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
    b.terminate(Terminator::Return(passed(is_test)))?;
    b.switch_to(err)?;
    let e = b.assign(
        Repr::Value,
        Rvalue::Field {
            base: result,
            layout: LayoutRef::Ctor(builtin::ERR),
            field: 0,
        },
    )?;
    let text = match show {
        ShowCall::Direct(show) => {
            let want = abi_sig(cx.defs.fn_sig(*show)?, cx.defs.globals);
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
                    callee: Callee::Direct(*show),
                    args: vec![arg],
                },
            )?
        }
        ShowCall::Dict {
            dict,
            layout,
            field,
        } => {
            let d = b.assign(Repr::Value, Rvalue::Use(Operand::Const(dict.clone())))?;
            let f = b.assign(
                Repr::Value,
                Rvalue::Field {
                    base: d,
                    layout: *layout,
                    field: *field,
                },
            )?;
            b.assign(
                Repr::Value,
                Rvalue::Call {
                    callee: Callee::Indirect(f),
                    args: vec![Operand::Local(e)],
                },
            )?
        }
        ShowCall::Debug => b
            .call_runtime(RuntimeFn::Debug, vec![Operand::Local(e)])?
            .ok_or_else(|| Ice::new("`debug` without a result"))?,
    };
    if is_test {
        // The host shows the text in the test's failure block (D-541).
        b.terminate(Terminator::Return(Operand::Local(text)))?;
        return Ok(());
    }
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
