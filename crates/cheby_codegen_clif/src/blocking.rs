//! `@blocking` foreign calls (D-042, D-683, GAP(blocking-shim)).
//!
//! An `@blocking` call runs its foreign function on the blocking pool while
//! the calling fiber is parked. The pool runs Rust code that knows nothing
//! of the function's C signature, and float arguments travel in other
//! registers than integers on both `SysV` and `AAPCS64`, so no generic
//! trampoline can call it. Code generation therefore defines one C-ABI shim
//! per blocking symbol, `cb$blk$<symbol>` (D-456: the kind first),
//! `void shim(const uint64_t *args, uint64_t *out)`, which unpacks each
//! argument word by its C type (integers, `f32` and `f64` bit patterns,
//! pointers), calls the function and stores its result as one word.
//!
//! The call site spills the arguments to a stack array, one word each
//! (an integer extended by its signedness, a `float`'s bits in the low
//! half), calls `cheby_rt_blocking_call(ctx, shim, args, nargs)` and
//! reinterprets the word it returns as the result type. The runtime copies
//! the words before the fiber parks, so the array lives only for the call.
//! The call keeps its unwind edge and is never in the never-raise table
//! (D-767): a cancellation that arrived during the call raises after it
//! returns, through the flag at `ctx+12`, which the edge tests
//! (`unwind/flag.rs`). The edge never tests the result, which a foreign
//! function may legitimately make 0 (an `Int` 0, a NULL pointer): D-734's
//! rule for raw results, while D-733's result test applies only to uniform
//! words.
//!
//! Borrowed Cheby pointers among the arguments stay valid while the fiber
//! is parked, since it holds them (D-187). Foreign code on a pool thread
//! never builds Cheby values in M2 (D-191, D-683): the result word comes
//! back unchanged, and how foreign code builds values waits for OQ-P9 (M3).

use cranelift_codegen::ir::immediates::Imm64;
use cranelift_codegen::ir::{
    self, ExtFuncData, ExternalName, FuncRef, GlobalValueData, InstBuilder, MemFlagsData,
    Signature, StackSlotData, StackSlotKind, Type, UserExternalName, Value, types,
};
use cranelift_codegen::isa::CallConv;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::FuncId;

use cheby_mir::{CType, ForeignSig};

use crate::CodegenError;
use crate::decls::{Decls, ForeignDecl};
use crate::func::Translator;
use crate::runtime::RuntimeFunc;
use crate::sig::{self, WORD};

/// The bytes of one argument or result word.
const WORD_BYTES: u32 = 8;

/// The C signature of every shim: `void (const uint64_t *, uint64_t *)`.
pub(crate) fn shim_signature(c: CallConv) -> Signature {
    sig::c_signature(&[CType::Ptr, CType::Ptr], CType::Void, c)
}

/// The Cranelift type a value of C type `ty` has in a register, `None` for
/// `void`.
fn clif_type(ty: CType) -> Option<Type> {
    sig::c_param(ty).map(|p| p.value_type)
}

/// The byte offset of word `n` of an argument array.
fn word_offset(n: usize) -> Result<i32, CodegenError> {
    u32::try_from(n)
        .ok()
        .and_then(|n| n.checked_mul(WORD_BYTES))
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| CodegenError::ice("too many arguments for a blocking call"))
}

/// `value`, of C type `ty`, as one 64-bit word: an integer extended by its
/// signedness, a float's bit pattern, a `float`'s in the low half.
fn to_word(b: &mut FunctionBuilder<'_>, ty: CType, value: Value) -> Result<Value, CodegenError> {
    Ok(match ty {
        CType::I8 | CType::I16 | CType::I32 => b.ins().sextend(types::I64, value),
        CType::U8 | CType::U16 | CType::U32 | CType::Bool => b.ins().uextend(types::I64, value),
        CType::I64 | CType::U64 | CType::Ptr => value,
        CType::F64 => b.ins().bitcast(types::I64, MemFlagsData::new(), value),
        CType::F32 => {
            let bits = b.ins().bitcast(types::I32, MemFlagsData::new(), value);
            b.ins().uextend(types::I64, bits)
        }
        CType::Void => return Err(CodegenError::ice("a `void` value in a blocking call")),
    })
}

/// The word `word` as a value of C type `ty`, the inverse of [`to_word`];
/// `None` for `void`.
fn from_word(b: &mut FunctionBuilder<'_>, ty: CType, word: Value) -> Option<Value> {
    Some(match ty {
        CType::I8 | CType::U8 | CType::Bool => b.ins().ireduce(types::I8, word),
        CType::I16 | CType::U16 => b.ins().ireduce(types::I16, word),
        CType::I32 | CType::U32 => b.ins().ireduce(types::I32, word),
        CType::I64 | CType::U64 | CType::Ptr => word,
        CType::F64 => b.ins().bitcast(types::F64, MemFlagsData::new(), word),
        CType::F32 => {
            let bits = b.ins().ireduce(types::I32, word);
            b.ins().bitcast(types::F32, MemFlagsData::new(), bits)
        }
        CType::Void => return None,
    })
}

/// A reference to the function `id` with `signature` in `b`'s function.
fn import_function(
    b: &mut FunctionBuilder<'_>,
    id: FuncId,
    signature: &Signature,
    colocated: bool,
) -> FuncRef {
    let sig_ref = b.import_signature(signature.clone());
    let name = b
        .func
        .declare_imported_user_function(UserExternalName::new(0, id.as_u32()));
    b.import_function(ExtFuncData {
        name: ExternalName::user(name),
        signature: sig_ref,
        colocated,
        patchable: false,
    })
}

/// Translates the shim `id` of the blocking foreign function `symbol` with
/// the C signature `sig` ([`Decls::declare_blocking_shim`]): it loads each
/// argument from `args[n]` as its C type, calls `symbol` the way a plain
/// foreign call does (`Translator::foreign_call`), and stores the result
/// word in `*out` (0 for `void`). It runs on a pool thread with Rust's
/// stack, so it has no stack check, and it never touches a Cheby object.
///
/// # Errors
///
/// An ICE if `symbol` was not declared or `sig` has a `void` parameter.
pub(crate) fn translate_shim(
    decls: &Decls,
    id: FuncId,
    symbol: &str,
    sig: &ForeignSig,
) -> Result<ir::Function, CodegenError> {
    let c = decls.c_call_conv();
    let mut clif = ir::Function::with_name_signature(
        ir::UserFuncName::user(0, id.as_u32()),
        shim_signature(c),
    );
    let mut fctx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut clif, &mut fctx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let &[args, out] = b.block_params(entry) else {
        return Err(CodegenError::ice(
            "a blocking shim without its two parameters",
        ));
    };
    let flags = MemFlagsData::trusted();
    let mut vals = Vec::with_capacity(sig.params.len());
    for (n, &ty) in sig.params.iter().enumerate() {
        let t = clif_type(ty)
            .ok_or_else(|| CodegenError::ice("a `void` parameter of a blocking call"))?;
        // Little-endian (`Decls::new`): a narrow value is the low bytes
        // of its word.
        vals.push(b.ins().load(t, flags, args, word_offset(n)?));
    }
    let inst = match decls.foreign(symbol)?.clone() {
        ForeignDecl::Function(fid, signature) => {
            let f = import_function(&mut b, fid, &signature, false);
            b.ins().call(f, &vals)
        }
        ForeignDecl::Data(data) => {
            let name = b
                .func
                .declare_imported_user_function(UserExternalName::new(1, data.as_u32()));
            let gv = b.create_global_value(GlobalValueData::Symbol {
                name: ExternalName::user(name),
                offset: Imm64::new(0),
                colocated: false,
                tls: false,
            });
            let address = b.ins().symbol_value(WORD, gv);
            let s = b.import_signature(sig::foreign_signature(sig, c));
            b.ins().call_indirect(s, address, &vals)
        }
        ForeignDecl::FunctionAddress(fid, declared) => {
            let f = import_function(&mut b, fid, &declared, false);
            let address = b.ins().func_addr(WORD, f);
            let s = b.import_signature(sig::foreign_signature(sig, c));
            b.ins().call_indirect(s, address, &vals)
        }
    };
    let result = b.inst_results(inst).first().copied();
    let word = match (sig.ret, result) {
        (CType::Void, None) => b.ins().iconst(types::I64, 0),
        (ty, Some(value)) => to_word(&mut b, ty, value)?,
        (_, None) => return Err(CodegenError::ice("a blocking call without its result")),
    };
    b.ins().store(flags, word, out, 0);
    b.ins().return_(&[]);
    b.seal_all_blocks();
    b.finalize(decls.frontend_config());
    Ok(clif)
}

impl Translator<'_, '_> {
    /// The call of the `@blocking` foreign function `symbol` with the C
    /// signature `sig` on the arguments `vals`: the arguments spilled to
    /// a stack array, `cheby_rt_blocking_call(ctx, shim, args, nargs)`, and
    /// its word reinterpreted as the result type. The caller adds the
    /// flag's test of an unwind edge (`unwind_c_call`).
    ///
    /// # Errors
    ///
    /// An ICE if the shim was not declared or the arguments do not match
    /// `sig`.
    pub(crate) fn blocking_call(
        &mut self,
        symbol: &str,
        sig: &ForeignSig,
        vals: &[Value],
    ) -> Result<Option<Value>, CodegenError> {
        if vals.len() != sig.params.len() {
            return Err(CodegenError::ice(format!(
                "a blocking call of `{symbol}` with the wrong number of arguments"
            )));
        }
        let shim = self.decls.blocking_shim(symbol, sig)?;
        // At least one word, so the array's address is never dangling.
        let words = u32::try_from(vals.len().max(1))
            .ok()
            .and_then(|n| n.checked_mul(WORD_BYTES))
            .ok_or_else(|| CodegenError::ice("too many arguments for a blocking call"))?;
        let slot = self.b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            words,
            3,
        ));
        for (n, (&value, &ty)) in vals.iter().zip(&sig.params).enumerate() {
            let word = to_word(&mut self.b, ty, value)?;
            self.b.ins().stack_store(WORD, word, slot, word_offset(n)?);
        }
        let ctx = self.ctx()?;
        let args = self.b.ins().stack_addr(WORD, slot, 0);
        let f = self.func_ref(shim, &shim_signature(self.decls.c_call_conv()), true);
        let shim = self.b.ins().func_addr(WORD, f);
        let count = i64::try_from(vals.len())
            .map_err(|_| CodegenError::ice("too many arguments for a blocking call"))?;
        let count = self.b.ins().iconst(types::I64, count);
        let word = self.runtime_value(RuntimeFunc::BlockingCall, &[ctx, shim, args, count])?;
        Ok(from_word(&mut self.b, sig.ret, word))
    }
}
