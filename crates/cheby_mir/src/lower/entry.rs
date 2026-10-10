//! Entry thunks (D-453): the C-ABI functions the host calls to run a
//! `main`, a test, a constant initializer or a REPL entry. They form their
//! own unit, so a module's code never depends on which entry runs it.

use cheby_iface::{DefId, Ice};
use cheby_types::thir::EntryInfo;

use crate::abi::{abi_sig, entry_sig};
use crate::builder::FunctionBuilder;
use crate::ir::{Callee, Const, EntryKind, FnKey, Operand, Rvalue, Terminator, UnitKind};
use crate::lower::{LowerCx, LowerError, Pending, Unsupported, finish_unit, not_supported};
use crate::repr::Repr;
use crate::{Abi, MirModule};

/// The exit status of a `main` or test that returned normally
/// (`cheby_runtime::abi::status::SUCCESS`; MIR does not depend on the
/// runtime).
const STATUS_SUCCESS: u64 = 0;

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
/// E0038 for a `main` or test returning `Result` (D-182 needs `Show`
/// dictionaries and debug printing), and an [`Ice`] for a REPL entry
/// (G5) or one whose item is not readable through `cx`.
pub fn lower_entries(
    entries: &[EntryRequest<'_>],
    cx: &LowerCx<'_>,
) -> Result<MirModule, LowerError> {
    let mut functions = Vec::with_capacity(entries.len());
    let mut unsupported = Unsupported::default();
    for req in entries {
        let (def, callee) = match req.kind {
            EntryKind::Main(def) => (def, Callee::Direct(def)),
            EntryKind::Test(def) => (def, Callee::Code(FnKey::Test(def))),
            EntryKind::Const(def) => (def, Callee::Code(FnKey::ConstInit(def))),
            EntryKind::Repl(_) => {
                return Err(Ice::new("REPL entries are not supported yet (G5)").into());
            }
        };
        let sig = entry_sig(req.kind);
        let mut b =
            FunctionBuilder::new(FnKey::Entry(req.kind), Abi::C, sig.ret, span_of(def, cx)?)?;
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
        let ret = match (req.kind, req.info) {
            (EntryKind::Main(_) | EntryKind::Test(_), Some(EntryInfo::Nil)) => {
                // The `Nil` result is dropped by the RC pass.
                Operand::Const(Const::Sized {
                    repr: Repr::I64,
                    bits: STATUS_SUCCESS,
                })
            }
            (EntryKind::Main(_) | EntryKind::Test(_), Some(EntryInfo::Result { .. })) => {
                let what = if matches!(req.kind, EntryKind::Main(_)) {
                    "`main` functions that return `Result`"
                } else {
                    "tests that return `Result`"
                };
                unsupported.push(not_supported(span_of(def, cx)?, what));
                continue;
            }
            (EntryKind::Main(_) | EntryKind::Test(_), None) => {
                return Err(Ice::new("a `main` or test entry without its entry info").into());
            }
            (EntryKind::Const(_) | EntryKind::Repl(_), _) => Operand::Local(result),
        };
        b.terminate(Terminator::Return(ret))?;
        functions.push(b.finish()?);
    }
    let unit = finish_unit(
        UnitKind::Entries,
        functions,
        Vec::new(),
        Pending::default(),
        cx,
    )?;
    unsupported.finish(unit)
}

fn span_of(def: DefId, cx: &LowerCx<'_>) -> Result<cheby_diag::Span, Ice> {
    cx.defs
        .item(def)?
        .span
        .ok_or_else(|| Ice::new(format!("entry item {def:?} has no span")))
}
