//! Cheby-ABI wrappers of `@external(native, …)` functions (D-455,
//! §12.5.1).
//!
//! Callers call the wrapper like any function, through its slot. The
//! wrapper's body is one foreign call with borrowed arguments (D-187),
//! then the result boxed into the uniform return (D-453, D-521): a `Nil`
//! result is the static `Nil`, a scalar result (`Int`, `Float`, `Bool`, a
//! sized number) is boxed, and a uniform word is returned as it is.
//! Foreign calls are never tail calls (§12.8).
//!
//! A direct call of a std function bound to a plain kernel skips the
//! wrapper (D-976): it is the wrapper's foreign call, made at the call site
//! ([`direct_kernel`], `BodyCx::call_kernel`). Std's interface carries the
//! binding (`FnSig::kernel`); std is keyed by the build ID, so the binding
//! never makes a cache entry stale. Plain means no `@std_context` or `@blocking`, no
//! intrinsic, no bounds, and a module whose wrappers make no effect check
//! ([`DIRECT_MODULES`]). The call gives the kernel the counts the wrapper
//! gave it (D-525): every counted argument is moved into an owned
//! temporary that the call borrows, so a value used after the call is
//! `dup`ed first and the kernel never updates it in place. A kernel that
//! only reads its arguments (`crate::kernels_reading`) borrows them as
//! they are, with no temporary. A kernel that is one machine instruction
//! (`crate::kernels_instr`) is that instruction, with no call. The result
//! keeps its own representation, so a `Float` result is never boxed. The
//! wrapper stays for function values, dictionaries and every other call.
//! A foreign call may raise (out of memory, a callback's panic), so it
//! gets an unwind edge that releases the caller's live values, as a Cheby
//! call does (D-698), unless its kernel never raises (D-767). It runs on
//! the caller's frame, which passed its own entry check, so the stack
//! headroom below the limit (D-457) is the same as below the wrapper's
//! frame.
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

use cheby_iface::{DefId, FnSig, Ice, ItemSig, ModuleId, PackageId, Ty, TyKind, TypeDefKind};
use cheby_types::thir;

use crate::builder::FunctionBuilder;
use crate::defs::Defs;
use crate::ir::{
    Abi, Const, FnKey, Function, Local, Operand, PanicKind, RuntimeFn, Rvalue, Stmt, Terminator,
};
use crate::layout::type_key;
use crate::lower::LowerCx;
use crate::lower::body::BodyCx;
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

/// The kernel of `list::set`, whose direct call is the runtime helper
/// `list_set` ([`RuntimeFn::ListSet`]), which owns its list and its item.
const LIST_SET: &str = "cheby_list_set";

/// The std modules whose plain kernels a direct call calls at the call
/// site, without the wrapper: the modules of [`CONST_SAFE_MODULES`]
/// except `option`. Their wrappers make no effect check, so the wrapper's
/// body is the foreign call and the box of its result, nothing more.
/// `option`'s one kernel, `panic_at`, starts its backtrace at the frame
/// of the function that called it (D-1647), which must stay the wrapper's.
const DIRECT_MODULES: [&str; 16] = [
    "list", "map", "set", "string", "bytes", "int", "float", "f32", "i8", "i16", "i32", "i64",
    "u8", "u16", "u32", "u64",
];

/// The native symbol that a direct call of `def` calls in place of its
/// wrapper, when `def` is bound to a plain kernel: a std function whose
/// interface carries the binding (`FnSig::kernel`: an
/// `@external(native, …)` without `@std_context` or `@blocking`), of
/// one of [`DIRECT_MODULES`], whose symbol is no intrinsic (D-693) and
/// whose type parameters have no bounds. Every other function, a
/// package's `@external` included (D-940), is called through its slot.
/// Only the interface is read (D-246).
///
/// # Errors
///
/// An [`Ice`] if `def` or its module cannot be read.
pub(super) fn direct_kernel(def: DefId, defs: &Defs<'_>) -> Result<Option<Box<str>>, Ice> {
    if def.module == ModuleId::BUILTIN {
        return Ok(None);
    }
    let ItemSig::Fn(sig) = &defs.item(def)?.sig else {
        return Ok(None);
    };
    let Some(symbol) = sig.kernel.as_deref() else {
        return Ok(None);
    };
    if Intrinsic::from_symbol(symbol).is_some()
        || sig.generics.params.iter().any(|p| !p.bounds.is_empty())
    {
        return Ok(None);
    }
    let info = defs.modules.try_get(def.module)?;
    let direct = info.package == PackageId::STD
        && matches!(info.path.segments(), [std, module]
            if std == "std" && DIRECT_MODULES.contains(&module.as_str()));
    Ok(direct.then(|| symbol.into()))
}

impl BodyCx<'_, '_> {
    /// A direct call of `def`, bound to the plain kernel `symbol`
    /// ([`direct_kernel`]), as the foreign call its wrapper makes, at the
    /// call site. `args` are the call's arguments, already lowered and in
    /// the parameters' representations, which are the kernel's (D-453).
    ///
    /// The kernel must see the counts the wrapper gives it (D-525): each
    /// counted argument is moved into an owned temporary that the call
    /// borrows (D-187) and counting releases after it. A value at its
    /// last use arrives with the count it has, so a unique one may be
    /// updated in place; a value used afterwards is `dup`ed into its
    /// temporary, so the kernel sees it shared and copies it. A kernel of
    /// [`crate::kernels_reading`] only reads its arguments, so the call
    /// borrows them as they are, as a Cheby call borrows a parameter. A
    /// kernel of [`crate::kernels_instr`] is no call at all, but the
    /// instruction it computes, on its raw arguments.
    /// `list::set` is the helper `list_set` instead, which owns the list
    /// and the item, so a unique list is written in place without a
    /// temporary. The result
    /// is used as it comes: a scalar stays unboxed, a `Nil` result is the
    /// static `Nil`, and a counted one is owned by the caller. In tail
    /// position (`tail`) a Cheby function returns it as a uniform word,
    /// boxed as the wrapper boxes it (D-453, D-521), since a foreign call
    /// is never a tail call (§12.8); the call then records no site, as a
    /// tail call of the wrapper defines no local (D-852).
    ///
    /// # Errors
    ///
    /// An [`Ice`] for an unknown local or a malformed signature.
    pub(super) fn call_kernel(
        &mut self,
        def: DefId,
        symbol: Box<str>,
        args: Vec<Operand>,
        ret_ty: Ty,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        let globals = self.cx.defs.globals;
        let sig = self.cx.defs.fn_sig(def)?;
        let fsig = foreign_sig_with_context(sig, globals, false);
        let ret_repr = repr_of(sig.ret, globals);
        if let Some(op) = crate::kernels_instr::op_of(&symbol) {
            // The kernel is an instruction (`crate::kernels_instr`): its
            // operands are raw numbers, so nothing is counted or unwound.
            let params: Vec<Repr> = sig.params.iter().map(|p| repr_of(p.ty, globals)).collect();
            let value = crate::kernels_instr::rvalue(op, &params, ret_repr, args)?;
            let raw = self.b.assign(ret_repr, value)?;
            return self.kernel_result(Operand::Local(raw), ret_repr, ret_ty, tail);
        }
        if &*symbol == LIST_SET {
            // `list::set` takes its list and its item over, so code
            // generation can write a unique list in place (`list_set`).
            let call = Rvalue::CallRuntime {
                func: RuntimeFn::ListSet,
                args,
            };
            return self.finish_kernel_call(call, &fsig, ret_repr, ret_ty, tail);
        }
        let ops = if crate::kernels_reading::only_reads(&symbol) {
            // A kernel that only reads borrows the arguments themselves.
            args
        } else {
            self.owned_temporaries(args)?
        };
        let call = Rvalue::CallForeign {
            symbol,
            sig: fsig.clone(),
            args: ops,
            blocking: false,
        };
        self.finish_kernel_call(call, &fsig, ret_repr, ret_ty, tail)
    }

    /// The operands of a direct kernel call for `args`: each counted local
    /// moved into an owned temporary (D-525, [`Self::call_kernel`]).
    fn owned_temporaries(&mut self, args: Vec<Operand>) -> Result<Vec<Operand>, Ice> {
        let mut ops = Vec::with_capacity(args.len());
        for op in args {
            let Operand::Local(local) = op else {
                ops.push(op);
                continue;
            };
            let decl = self.b.local(local)?;
            if decl.repr != Repr::Value {
                ops.push(op);
                continue;
            }
            let ty = decl.ty;
            let owned = self.b.new_local(Repr::Value, None, ty)?;
            self.emit(Stmt::Assign {
                dst: owned,
                value: Rvalue::Use(op),
            })?;
            ops.push(Operand::Local(owned));
        }
        Ok(ops)
    }

    /// The rest of a direct kernel call ([`Self::call_kernel`]): `call`,
    /// whose C signature is `fsig` and whose result has `ret_repr`,
    /// emitted, and its result converted to `ret_ty`'s representation, or
    /// returned in tail position (`tail`).
    fn finish_kernel_call(
        &mut self,
        call: Rvalue,
        fsig: &ForeignSig,
        ret_repr: Repr,
        ret_ty: Ty,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        let (result, repr) = if fsig.ret == CType::Void {
            self.emit(Stmt::Eval(call))?;
            (Operand::Const(Const::Nil), Repr::Value)
        } else {
            let raw = self.b.assign(ret_repr, call)?;
            // The site of a kernel line of the reuse report (D-852).
            if !tail && let Some(site) = self.site {
                self.b.record_site(raw, site);
            }
            (Operand::Local(raw), ret_repr)
        };
        self.kernel_result(result, repr, ret_ty, tail)
    }

    /// The result of a direct kernel call, `result` of `repr`, converted to
    /// `ret_ty`'s representation, or returned as a uniform word in tail
    /// position (`tail`).
    fn kernel_result(
        &mut self,
        result: Operand,
        repr: Repr,
        ret_ty: Ty,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        if tail && self.b.abi() == Abi::Cheby {
            let uniform = self.to_repr(result, repr, Repr::Value, ret_ty)?;
            self.terminate(Terminator::Return(uniform))?;
            return Ok(None);
        }
        let want = self.repr_of(ret_ty);
        self.to_repr(result, repr, want, ret_ty).map(Some)
    }
}

/// The std modules whose kernels constant evaluation may run (D-360,
/// D-537): effect-free, with the same result on every target. `option`'s
/// one kernel, `panic_at`, only panics, so a failing `expect` in a
/// constant is E0075 at its caller (D-1660).
const CONST_SAFE_MODULES: [&str; 17] = [
    "list", "map", "set", "string", "bytes", "int", "float", "f32", "i8", "i16", "i32", "i64",
    "u8", "u16", "u32", "u64", "option",
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
