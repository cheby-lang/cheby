//! Cheby-ABI wrappers of `@external(native, …)` functions (D-455,
//! §12.5.1).
//!
//! Callers in every module call the wrapper like any function, through its
//! slot, so externality is not part of the interface. The wrapper's body
//! is one foreign call with borrowed arguments (D-187), then the result
//! boxed into the uniform return (D-453, D-521): a `Nil` result is the
//! static `Nil`, a scalar result (`Int`, `Float`, `Bool`, a sized number)
//! is boxed, and a uniform word is returned as it is. Foreign calls are
//! never tail calls (§12.8).
//!
//! Constant evaluation may run only the effect-free kernels of the std
//! modules `list`, `map`, `set`, `string`, `bytes` and the numeric modules
//! (D-360, D-537). Every other wrapper starts with
//! `CallRuntime(EffectCheck, [symbol])`, which panics `ForbiddenEffect`
//! during constant evaluation (E0077) and does nothing otherwise.

use cheby_iface::{FnSig, Ice, PackageId};
use cheby_types::thir;

use crate::builder::FunctionBuilder;
use crate::ir::{
    Abi, Const, FnKey, Function, Operand, PanicKind, RuntimeFn, Rvalue, Stmt, Terminator,
};
use crate::lower::LowerCx;
use crate::repr::{CType, Repr, foreign_sig, repr_of};

pub(super) fn lower_wrapper(
    f: &thir::FnDef,
    ext: &thir::External,
    sig: &FnSig,
    cx: &LowerCx<'_>,
) -> Result<Function, Ice> {
    let globals = cx.defs.globals;
    if sig.generics.params.iter().any(|p| !p.bounds.is_empty()) {
        // Dictionaries cannot cross the C ABI (§12.5): the front end
        // reports E0045 at the bound and stops before MIR (D-554).
        return Err(Ice::new(format!(
            "an external function with a bounded type parameter reached MIR at {:?}; \
             the front end reports E0045 (D-554)",
            ext.span
        )));
    }
    let ret_repr = repr_of(sig.ret, globals);
    let mut b = FunctionBuilder::new(FnKey::Item(f.def), Abi::Cheby, Repr::Value, f.span)?;
    b.set_entry_loc(&cx.location(f.span)?)?;
    let fsig = foreign_sig(sig, globals);
    let mut args = Vec::with_capacity(sig.params.len());
    for p in &sig.params {
        let local = b.add_param(repr_of(p.ty, globals), Some(p.name), Some(p.ty))?;
        args.push(Operand::Local(local));
    }
    if !allowed_in_constants(f, cx)? {
        let symbol = b.string(&ext.symbol)?;
        b.call_runtime(
            RuntimeFn::EffectCheck,
            vec![Operand::Const(Const::Str(symbol))],
        )?;
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
        let raw = b.assign(ret_repr, call)?;
        if ret_repr == Repr::Value {
            Operand::Local(raw)
        } else {
            // The uniform return (D-453): boxed canonically (D-521).
            let boxed = b.assign(
                Repr::Value,
                Rvalue::Box {
                    value: Operand::Local(raw),
                    from: ret_repr,
                },
            )?;
            Operand::Local(boxed)
        }
    };
    b.terminate(Terminator::Return(result))?;
    b.finish()
}

/// The native body of a function whose only implementation is an
/// `@external(js, …)` (§12.5): it does not exist on native, and the
/// cross-target check keeps native code from calling it, but a constant's
/// initializer is not target-checked and runs on the host (§12.6, D-512).
/// So the body is the effect check, which makes constant evaluation report
/// E0077 naming the function (D-533, D-537), then a panic that says the
/// function exists only on `js`, which nothing reaches in a checked
/// program (D-591).
pub(super) fn lower_js_only(
    f: &thir::FnDef,
    sig: &FnSig,
    cx: &LowerCx<'_>,
) -> Result<Function, Ice> {
    let globals = cx.defs.globals;
    let dicts: usize = sig.generics.params.iter().map(|p| p.bounds.len()).sum();
    let mut b = FunctionBuilder::new(FnKey::Item(f.def), Abi::Cheby, Repr::Value, f.span)?;
    let location = cx.location(f.span)?;
    b.set_entry_loc(&location)?;
    for _ in 0..dicts {
        b.add_param(Repr::Value, None, None)?;
    }
    for p in &sig.params {
        b.add_param(repr_of(p.ty, globals), Some(p.name), Some(p.ty))?;
    }
    let path = cx.defs.name(f.def)?.path();
    let symbol = b.string(&path)?;
    b.call_runtime(
        RuntimeFn::EffectCheck,
        vec![Operand::Const(Const::Str(symbol))],
    )?;
    let message = b.string(&format!("`{path}` exists only on the `js` target"))?;
    let loc = b.string(&location)?;
    b.terminate(Terminator::Panic {
        kind: PanicKind::Explicit,
        message: Some(Operand::Const(Const::Str(message))),
        loc,
    })?;
    b.finish()
}

/// The std modules whose kernels constant evaluation may run (D-360,
/// D-537): effect-free, with the same result on every target.
const CONST_SAFE_MODULES: [&str; 16] = [
    "list", "map", "set", "string", "bytes", "int", "float", "f32", "i8", "i16", "i32", "i64",
    "u8", "u16", "u32", "u64",
];

/// Whether constant evaluation may run `f`'s external: `f` is declared in
/// one of [`CONST_SAFE_MODULES`] of the standard library, decided from the
/// external's module path (D-537).
fn allowed_in_constants(f: &thir::FnDef, cx: &LowerCx<'_>) -> Result<bool, Ice> {
    let info = cx.defs.modules.try_get(f.def.module)?;
    Ok(info.package == PackageId::STD
        && matches!(info.path.segments(), [std, module]
            if std == "std" && CONST_SAFE_MODULES.contains(&module.as_str())))
}
