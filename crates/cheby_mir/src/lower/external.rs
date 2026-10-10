//! Cheby-ABI wrappers of `@external(native, …)` functions (D-455,
//! §12.5.1).
//!
//! Callers in every module call the wrapper like any function, through its
//! slot, so externality is not part of the interface. The wrapper's body
//! is one foreign call with borrowed arguments (D-187), then the result
//! boxed into the uniform return. Foreign calls are never tail calls
//! (§12.8). E6 adds the "no externals during constant evaluation" check
//! here (§4.5).
//!
//! Boxing a result of type `Int`, `Float`, `Bool` or a sized number is
//! not implemented yet (D6), so such an external is E0038 at its
//! `@external` when a run needs it (D-510).

use cheby_iface::FnSig;
use cheby_types::thir;

use crate::builder::FunctionBuilder;
use crate::ir::{Abi, Const, FnKey, Function, Operand, Rvalue, Stmt, Terminator};
use crate::lower::{LowerCx, LowerError, not_supported};
use crate::repr::{CType, Repr, foreign_sig, repr_of};

pub(super) fn lower_wrapper(
    f: &thir::FnDef,
    ext: &thir::External,
    sig: &FnSig,
    cx: &LowerCx<'_>,
) -> Result<Function, LowerError> {
    let globals = cx.defs.globals;
    let unsupported = |what: &str| LowerError::Unsupported(vec![not_supported(ext.span, what)]);
    if sig.generics.params.iter().any(|p| !p.bounds.is_empty()) {
        // Dictionaries cannot cross the C ABI (§12.5).
        return Err(unsupported(
            "external functions with bounded type parameters",
        ));
    }
    let ret_repr = repr_of(sig.ret, globals);
    if sig.ret != cheby_iface::Ty::NIL && ret_repr != Repr::Value {
        return Err(unsupported(&format!(
            "external functions that return `{}`",
            cx.show(sig.ret, Some(&sig.generics))
        )));
    }
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
        // A uniform word already: checked above.
        Operand::Local(b.assign(ret_repr, call)?)
    };
    b.terminate(Terminator::Return(result))?;
    Ok(b.finish()?)
}
