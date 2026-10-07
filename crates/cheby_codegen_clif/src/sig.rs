//! Machine types and signatures (E1, D-453).
//!
//! Every Cheby-ABI signature is built here from a [`cheby_mir::AbiSig`],
//! the one source of signatures, with the hidden `ctx` pointer added first
//! (D-453). Foreign signatures follow §12.5.1 with the extension attributes
//! the platform ABIs require of the caller.

use cranelift_codegen::ir::{AbiParam, Signature, Type, types};
use cranelift_codegen::isa::CallConv;

use cheby_mir::{Abi, AbiSig, CType, ForeignSig, Repr};

/// The machine word: a uniform value, a pointer, `Int`.
pub const WORD: Type = types::I64;

/// The Cranelift type of a MIR representation: uniform words and 64-bit
/// integers are `i64`, `Bool` and 8-bit integers `i8`.
#[must_use]
pub const fn repr_type(repr: Repr) -> Type {
    match repr {
        Repr::Value | Repr::Int | Repr::I64 | Repr::U64 => types::I64,
        Repr::Float => types::F64,
        Repr::F32 => types::F32,
        Repr::Bool | Repr::I8 | Repr::U8 => types::I8,
        Repr::I16 | Repr::U16 => types::I16,
        Repr::I32 | Repr::U32 => types::I32,
    }
}

/// The calling convention of `abi`: Cranelift's `tail` for Cheby functions
/// (so every Cheby-to-Cheby tail call can be `return_call`, D-015), the
/// platform's C convention for entry thunks and trampolines.
#[must_use]
pub const fn call_conv(abi: Abi, c: CallConv) -> CallConv {
    match abi {
        Abi::Cheby => CallConv::Tail,
        Abi::C => c,
    }
}

/// The machine signature of `sig`: `ctx` first, then the parameters, then
/// one result.
#[must_use]
pub fn signature(sig: &AbiSig, c: CallConv) -> Signature {
    let mut out = Signature::new(call_conv(sig.abi, c));
    out.params.push(AbiParam::new(WORD));
    out.params
        .extend(sig.params.iter().map(|&r| AbiParam::new(repr_type(r))));
    out.returns.push(AbiParam::new(repr_type(sig.ret)));
    out
}

/// A C parameter or result of type `ty`, extended to its register by the
/// caller when narrower than 64 bits: Apple's arm64 ABI makes the caller
/// extend small arguments, and Cranelift extends only when asked.
#[must_use]
pub fn c_param(ty: CType) -> Option<AbiParam> {
    let param = match ty {
        CType::I8 => AbiParam::new(types::I8).sext(),
        CType::I16 => AbiParam::new(types::I16).sext(),
        CType::I32 => AbiParam::new(types::I32).sext(),
        CType::U8 | CType::Bool => AbiParam::new(types::I8).uext(),
        CType::U16 => AbiParam::new(types::I16).uext(),
        CType::U32 => AbiParam::new(types::I32).uext(),
        CType::I64 | CType::U64 | CType::Ptr => AbiParam::new(types::I64),
        CType::F32 => AbiParam::new(types::F32),
        CType::F64 => AbiParam::new(types::F64),
        CType::Void => return None,
    };
    Some(param)
}

/// The signature of a foreign function (§12.5.1): no `ctx`, the platform C
/// convention.
#[must_use]
pub fn foreign_signature(sig: &ForeignSig, c: CallConv) -> Signature {
    let mut out = Signature::new(c);
    out.params
        .extend(sig.params.iter().copied().filter_map(c_param));
    out.returns.extend(c_param(sig.ret));
    out
}

/// A C signature from parameter and result types, for runtime functions.
#[must_use]
pub fn c_signature(params: &[CType], ret: CType, c: CallConv) -> Signature {
    foreign_signature(
        &ForeignSig {
            params: params.to_vec(),
            ret,
        },
        c,
    )
}
