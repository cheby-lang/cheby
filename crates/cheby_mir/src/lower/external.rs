//! Cheby-ABI wrappers of `@external(native, …)` functions (D-455,
//! §12.5.1).
//!
//! Callers in every module call the wrapper like any function, through its
//! slot, so externality is not part of the interface. The wrapper's body
//! is one foreign call with borrowed arguments (D-187), then the result
//! boxed into the uniform return. Foreign calls are never tail calls
//! (§12.8). E6 adds the "no externals during constant evaluation" check
//! here (§4.5).

use cheby_iface::{FnSig, Ice};
use cheby_types::thir;

use crate::builder::FunctionBuilder;
use crate::ir::{Abi, Const, FnKey, Function, Operand, Rvalue, Stmt, Terminator};
use crate::lower::LowerCx;
use crate::repr::{CType, Repr, foreign_sig, repr_of};

pub(super) fn lower_wrapper(
    f: &thir::FnDef,
    ext: &thir::External,
    sig: &FnSig,
    cx: &LowerCx<'_>,
) -> Result<Function, Ice> {
    if sig.generics.params.iter().any(|p| !p.bounds.is_empty()) {
        return Err(Ice::new(
            "a foreign function with bounded type parameters reached MIR lowering (§12.5)",
        ));
    }
    let globals = cx.defs.globals;
    let mut b = FunctionBuilder::new(FnKey::Item(f.def), Abi::Cheby, Repr::Value, f.span)?;
    b.set_entry_loc(&cx.location(f.span)?)?;
    let fsig = foreign_sig(sig, globals);
    let mut args = Vec::with_capacity(sig.params.len());
    for p in &sig.params {
        let local = b.add_param(repr_of(p.ty, globals), Some(p.name), Some(p.ty))?;
        args.push(Operand::Local(local));
    }
    let call = Rvalue::CallForeign {
        symbol: ext.symbol.clone(),
        sig: fsig.clone(),
        args,
    };
    let result = if fsig.ret == CType::Void {
        b.push(Stmt::Eval(call))?;
        Operand::Const(Const::Nil)
    } else {
        let repr = repr_of(sig.ret, globals);
        let raw = b.assign(repr, call)?;
        if repr == Repr::Value {
            Operand::Local(raw)
        } else {
            Operand::Local(b.assign(
                Repr::Value,
                Rvalue::Box {
                    value: Operand::Local(raw),
                    from: repr,
                },
            )?)
        }
    };
    b.terminate(Terminator::Return(result))?;
    b.finish()
}
