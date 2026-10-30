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
//!
//! The kernel of a `@std_context` function receives the running fiber's
//! context first (D-692): the wrapper passes `Const::Ctx` as the first
//! operand of its `CallForeign`, whose C signature starts with `ptr`
//! (`foreign_sig_with_context`, D-745). A std external
//! whose symbol is a std intrinsic gets the intrinsic's generated body
//! instead of a foreign call (D-693, `intrinsic.rs`).
//!
//! The wrapper of a package's `@external(native, …)` outside std calls C
//! code (D-940), which needs three more things (§12.5.1; std's wrappers do
//! not change, since its kernels are Rust, never return NULL and take and
//! return its handle types as runtime kinds):
//!
//! - An argument of an external type declared outside std crosses as its
//!   handle, the word at offset 8 of its D-590 object (§12.5.1: an
//!   external type crosses as a pointer), read with an `unbox` to `U64`
//!   (the object has a boxed `U64`'s shape) and passed as `u64`. The
//!   argument stays held until the
//!   call is over (a `use` of it last).
//! - A result of such an external type is the raw handle, `u64`, which
//!   `cheby_rt_ffi_external` wraps in a new value of the type's layout,
//!   NULL included (D-947, D-590).
//! - A result of any other counted type is checked by
//!   `cheby_rt_ffi_value`: a NULL result panics with ``foreign function
//!   `<symbol>` returned null`` at the declaration (D-947,
//!   D-1229).
//!
//! A failed `cheby_ffi_` allocation sets the unwind flag (D-946). After a
//! call whose result one of the two helpers takes, code generation tests
//! the flag after the helper instead of after the call (`cheby_codegen_clif`'s
//! `func.rs`), so the helper sees it: the external helper releases the
//! handle, and the cleanup of the value helper's edge releases the owned
//! result, before the wrapper unwinds. A scalar or `Nil` result needs no
//! release: the flag's test after the call unwinds at once. The same holds
//! for a package's `@blocking` external (D-950): a cancellation or a failed
//! allocation during its pool job comes back with the result word and the
//! flag set, and the helper releases the result on the fiber, an external
//! one through its drop function (D-171).

use cheby_iface::{FnSig, Ice, ItemSig, PackageId, Ty, TyKind, TypeDefKind};
use cheby_types::thir;

use crate::builder::FunctionBuilder;
use crate::ir::{
    Abi, Const, FnKey, Function, Local, Operand, PanicKind, RuntimeFn, Rvalue, Stmt, Terminator,
};
use crate::layout::type_key;
use crate::lower::LowerCx;
use crate::lower::intrinsic::{Intrinsic, lower_intrinsic};
use crate::repr::{CType, ForeignSig, Repr, foreign_sig_with_context, repr_of};

/// The runtime's helper that wraps a package's foreign result of an
/// external type (`cheby_runtime::abi::sym::FFI_EXTERNAL`; this crate does
/// not depend on the runtime, and a test compares the names).
const FFI_EXTERNAL: &str = "cheby_rt_ffi_external";

/// The runtime's helper that checks a package's foreign result of a
/// counted type (`cheby_runtime::abi::sym::FFI_VALUE`).
const FFI_VALUE: &str = "cheby_rt_ffi_value";

/// The function of `f`, whose `@external(native, …)` is `ext`: the
/// Cheby-ABI wrapper of its kernel, or a std intrinsic's generated body.
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
    if let Some(intrinsic) = Intrinsic::from_symbol(&ext.symbol) {
        // Only std binds `cheby_rt_` symbols; the resolver reports E0055
        // anywhere else (D-455), so no other package gets here.
        if !in_std(f, cx)? {
            return Err(Ice::new(format!(
                "a package other than std binds the intrinsic `{}`; the resolver reports E0055",
                ext.symbol
            )));
        }
        return lower_intrinsic(intrinsic, f, sig, cx);
    }
    // C code of a package outside std (D-940): external values cross as
    // their handles and its results are checked (module documentation).
    let package = !in_std(f, cx)?;
    let ret_repr = repr_of(sig.ret, globals);
    let mut b = FunctionBuilder::new(FnKey::Item(f.def), Abi::Cheby, Repr::Value, f.span)?;
    let location = cx.location(f.span)?;
    b.set_entry_loc(&location)?;
    let mut fsig = foreign_sig_with_context(sig, globals, ext.context);
    let mut args = Vec::with_capacity(sig.params.len().saturating_add(1));
    if ext.context {
        // The running fiber's context, the kernel's first argument
        // (D-692, D-745).
        args.push(Operand::Const(Const::Ctx));
    }
    let mut params = Vec::with_capacity(sig.params.len());
    for p in &sig.params {
        let local = b.add_param(repr_of(p.ty, globals), Some(p.name), Some(p.ty))?;
        params.push((local, package && external_type(p.ty, cx)?.is_some()));
    }
    if !allowed_in_constants(f, cx)? {
        let symbol = b.string(&ext.symbol)?;
        b.call_runtime(
            RuntimeFn::EffectCheck,
            vec![Operand::Const(Const::Str(symbol))],
        )?;
    }
    let first_param = usize::from(ext.context);
    let kept = pass_params(&mut b, &params, &mut args, &mut fsig, first_param)?;
    let external_ret = if package {
        external_type(sig.ret, cx)?
    } else {
        None
    };
    if external_ret.is_some() {
        // The raw handle, wrapped below (D-947).
        fsig.ret = CType::U64;
    }
    let call = Rvalue::CallForeign {
        symbol: ext.symbol.clone(),
        sig: fsig.clone(),
        args,
        // An `@blocking` external runs on the blocking pool (D-683); code
        // generation reads the flag.
        blocking: ext.blocking,
    };
    let result = if let Some(type_key) = external_ret {
        let handle = b.assign(Repr::U64, call)?;
        Operand::Local(wrap_external(&mut b, handle, type_key)?)
    } else if package && fsig.ret == CType::Ptr {
        let raw = b.assign(Repr::Value, call)?;
        check_value(&mut b, raw, &ext.symbol, &location)?;
        Operand::Local(raw)
    } else if fsig.ret == CType::Void {
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
    // An external argument whose handle C was given stays held until the
    // call and its checks are over: without this use its last use would be
    // the read of its handle, and counting would release it (and run its
    // drop function) before C sees the handle. Last, so no allocation
    // follows its release.
    for local in kept {
        b.assign(Repr::Value, Rvalue::Use(Operand::Local(local)))?;
    }
    b.terminate(Terminator::Return(result))?;
    b.finish()
}

/// Pushes the foreign call's arguments for `params` (each a parameter and
/// whether it is a package's external value) onto `args`, the C parameter
/// at `first_param` plus its index in `fsig`: a plain parameter as it is,
/// an external value as its handle, a `u64` (§12.5.1, D-590). Returns the
/// external parameters, which the wrapper keeps held past the call.
fn pass_params(
    b: &mut FunctionBuilder,
    params: &[(Local, bool)],
    args: &mut Vec<Operand>,
    fsig: &mut ForeignSig,
    first_param: usize,
) -> Result<Vec<Local>, Ice> {
    let mut kept = Vec::new();
    for (n, &(local, external)) in params.iter().enumerate() {
        if !external {
            args.push(Operand::Local(local));
            continue;
        }
        // The handle is the word at offset 8 of the 16-byte object, which
        // has a boxed `U64`'s shape (a header, then one raw word), so
        // `unbox` reads it (an object is never a tagged word).
        let handle = b.assign(
            Repr::U64,
            Rvalue::Unbox {
                value: Operand::Local(local),
                to: Repr::U64,
            },
        )?;
        args.push(Operand::Local(handle));
        if let Some(ctype) = fsig.params.get_mut(first_param.saturating_add(n)) {
            *ctype = CType::U64;
        }
        kept.push(local);
    }
    Ok(kept)
}

/// The key of `ty` (D-559) when it is an external type declared outside
/// std (§12.5.3): its values are D-590 objects of a layout the runtime
/// registered from the declaring module's record (D-1168). Std's handle
/// types are runtime kinds (D-647, D-703, D-982), never wrapped here.
fn external_type(ty: Ty, cx: &LowerCx<'_>) -> Result<Option<u64>, Ice> {
    let TyKind::Named { def, .. } = cx.defs.globals.types.kind(ty) else {
        return Ok(None);
    };
    let def = *def;
    let item = cx.defs.item(def)?;
    if !matches!(&item.sig, ItemSig::Type(t) if matches!(t.kind, TypeDefKind::External)) {
        return Ok(None);
    }
    if cx.defs.modules.try_get(def.module)?.package == PackageId::STD {
        return Ok(None);
    }
    let name = cx.defs.name(def)?;
    Ok(Some(type_key(&name.module, &name.item)))
}

/// Wraps `handle`, the raw result of a package's foreign function whose
/// result is an external type, in a new value of that type, NULL included
/// (D-947, D-590): `cheby_rt_ffi_external(ctx, handle, type_key)`, which
/// releases the handle and unwinds instead when a `cheby_ffi_` allocation
/// of the call failed (D-946).
fn wrap_external(b: &mut FunctionBuilder, handle: Local, type_key: u64) -> Result<Local, Ice> {
    b.assign(
        Repr::Value,
        Rvalue::CallForeign {
            symbol: FFI_EXTERNAL.into(),
            sig: ForeignSig {
                params: vec![CType::Ptr, CType::U64, CType::U64],
                ret: CType::Ptr,
            },
            args: vec![
                Operand::Const(Const::Ctx),
                Operand::Local(handle),
                Operand::Const(Const::Sized {
                    repr: Repr::U64,
                    bits: type_key,
                }),
            ],
            blocking: false,
        },
    )
}

/// Checks `raw`, the owned result of the package's foreign function
/// `symbol` whose result is a counted value:
/// `cheby_rt_ffi_value(ctx, raw, message, location)` unwinds after a
/// failed `cheby_ffi_` allocation of the call, and the cleanup releases
/// `raw` (D-946); a NULL `raw` panics with D-947's message at the
/// `@external` function's declaration, `location`
/// (D-1229: std's kernels are Rust and never
/// return NULL, so only packages' wrappers check).
fn check_value(
    b: &mut FunctionBuilder,
    raw: Local,
    symbol: &str,
    location: &str,
) -> Result<(), Ice> {
    let message = b.string(&format!("foreign function `{symbol}` returned null"))?;
    let location = b.string(location)?;
    b.push(Stmt::Eval(Rvalue::CallForeign {
        symbol: FFI_VALUE.into(),
        sig: ForeignSig {
            params: vec![CType::Ptr, CType::Ptr, CType::Ptr, CType::Ptr],
            ret: CType::Void,
        },
        args: vec![
            Operand::Const(Const::Ctx),
            Operand::Local(raw),
            Operand::Const(Const::Str(message)),
            Operand::Const(Const::Str(location)),
        ],
        blocking: false,
    }))
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

/// Whether `f` is declared in the standard library.
fn in_std(f: &thir::FnDef, cx: &LowerCx<'_>) -> Result<bool, Ice> {
    Ok(cx.defs.modules.try_get(f.def.module)?.package == PackageId::STD)
}

#[cfg(test)]
#[path = "external_tests.rs"]
mod tests;
