//! The `try_call` prototype's exit stubs (D-696): one small generated
//! function per machine signature of the C functions that generated code
//! calls with an unwind edge.
//!
//! A `try_call` needs a callee whose convention supports exceptions and
//! whose handler edge assumes nothing about registers: `Tail` (every
//! register is clobbered at the handler, so the frame-pointer unwinder
//! restores only the stack and frame pointers). A C function has the
//! platform convention, so generated code `try_call`s the stub instead:
//!
//! ```text
//! function cb$exit$<params>$<result>(ctx, callee, args…) tail {
//!     fp = get_frame_pointer
//!     store [fp + 0], ctx+16     ; the caller's frame pointer
//!     store [fp + 8], ctx+24     ; the return address into the caller
//!     return call_indirect callee(args…)   ; the platform C convention
//! }
//! ```
//!
//! The record at `ctx+16`/`ctx+24` (GAP(try-call-exit-record)) is where the
//! runtime's unwinder starts when the C function raises: the caller's frame
//! and the return address of its `try_call`, whose handler it looks up.
//! The callee is passed as an address, so one stub serves every C function
//! of its signature (runtime functions and kernels alike).

use cranelift_codegen::ir::{self, AbiParam, InstBuilder, MemFlagsData, Signature, types};
use cranelift_codegen::isa::CallConv;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::FuncId;

use cheby_mir::CType;
use cheby_runtime::abi;

use crate::CodegenError;
use crate::decls::Decls;
use crate::mangle;
use crate::runtime::RuntimeFunc;
use crate::sig::{self, WORD};

/// The offset of the saved frame pointer in a frame record, from the frame
/// pointer, on aarch64 and x86-64 alike.
const SAVED_FP_OFFSET: i32 = 0;
/// The offset of the return address in a frame record, from the frame
/// pointer, on aarch64 (the saved link register) and x86-64 alike.
const RETURN_ADDRESS_OFFSET: i32 = 8;

/// A C parameter or result as the machine passes it: C types that the
/// platform conventions pass alike share one, so C functions of
/// different MIR signatures share a stub.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum MachineType {
    I8,
    I16,
    I32,
    /// `I64`, `U64` and `Ptr`.
    I64,
    /// `U8` and `Bool`.
    U8,
    U16,
    U32,
    F32,
    F64,
}

impl MachineType {
    /// The machine type of a C type, `None` for `Void`.
    const fn of(ty: CType) -> Option<Self> {
        Some(match ty {
            CType::I8 => Self::I8,
            CType::I16 => Self::I16,
            CType::I32 => Self::I32,
            CType::I64 | CType::U64 | CType::Ptr => Self::I64,
            CType::U8 | CType::Bool => Self::U8,
            CType::U16 => Self::U16,
            CType::U32 => Self::U32,
            CType::F32 => Self::F32,
            CType::F64 => Self::F64,
            CType::Void => return None,
        })
    }

    /// A C type with this machine type.
    const fn ctype(self) -> CType {
        match self {
            Self::I8 => CType::I8,
            Self::I16 => CType::I16,
            Self::I32 => CType::I32,
            Self::I64 => CType::I64,
            Self::U8 => CType::U8,
            Self::U16 => CType::U16,
            Self::U32 => CType::U32,
            Self::F32 => CType::F32,
            Self::F64 => CType::F64,
        }
    }

    /// The word in a stub's symbol.
    const fn word(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::F32 => "f32",
            Self::F64 => "f64",
        }
    }
}

/// What identifies an exit stub: the machine signature of the C functions
/// it calls. Ordered, so a batch declares its new stubs in the same order
/// in every build (D-264).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ExitKey {
    params: Box<[MachineType]>,
    ret: Option<MachineType>,
}

impl ExitKey {
    /// The key of C functions with `params` and `ret`.
    pub(crate) fn of(params: &[CType], ret: CType) -> Self {
        Self {
            params: params.iter().filter_map(|&t| MachineType::of(t)).collect(),
            ret: MachineType::of(ret),
        }
    }

    /// The key of the runtime function `f`.
    pub(crate) fn runtime(f: RuntimeFunc) -> Self {
        let (params, ret) = f.ctypes();
        Self::of(params, ret)
    }

    /// The stub's symbol, `cb$exit$<params>$<result>`.
    pub(crate) fn symbol(&self) -> String {
        let params: Vec<&str> = self.params.iter().map(|t| t.word()).collect();
        mangle::exit_stub(&params, self.ret.map_or("v", MachineType::word))
    }

    /// The C signature the stub calls its callee with, in the platform
    /// convention `c`, with the extensions the platform needs.
    fn c_signature(&self, c: CallConv) -> Signature {
        let params: Vec<CType> = self.params.iter().map(|t| t.ctype()).collect();
        let ret = self.ret.map_or(CType::Void, MachineType::ctype);
        sig::c_signature(&params, ret, c)
    }

    /// The stub's own signature: `tail`, `ctx` and the callee's address,
    /// then the C parameters, and the C result if there is one.
    pub(crate) fn signature(&self, c: CallConv) -> Signature {
        let c_sig = self.c_signature(c);
        let mut out = Signature::new(CallConv::Tail);
        out.params.push(AbiParam::new(WORD));
        out.params.push(AbiParam::new(WORD));
        out.params
            .extend(c_sig.params.iter().map(|p| AbiParam::new(p.value_type)));
        out.returns
            .extend(c_sig.returns.iter().map(|p| AbiParam::new(p.value_type)));
        out
    }
}

/// Whether generated code calls the runtime function `f` with an unwind
/// edge, and so through an exit stub: every runtime function that may
/// start or pass on an unwinding (D-698). Count operations and `fmod`
/// never do; the helpers follow [`cheby_mir::RuntimeFn::may_unwind`].
pub(crate) const fn runtime_needs_stub(f: RuntimeFunc) -> bool {
    match f {
        RuntimeFunc::Dup
        | RuntimeFunc::Drop
        | RuntimeFunc::FmodF64
        | RuntimeFunc::FmodF32
        | RuntimeFunc::StrEq
        | RuntimeFunc::ListLength
        | RuntimeFunc::ListGet => false,
        RuntimeFunc::Alloc
        | RuntimeFunc::Panic
        | RuntimeFunc::StackSlow
        | RuntimeFunc::BoxI64
        | RuntimeFunc::BoxU64
        | RuntimeFunc::BoxF64
        | RuntimeFunc::BoxF32
        | RuntimeFunc::ConstMissing
        | RuntimeFunc::StrWithCapacity
        | RuntimeFunc::StrAppend
        | RuntimeFunc::ListPush
        | RuntimeFunc::ListAppend
        | RuntimeFunc::ListSlice
        | RuntimeFunc::Equal
        | RuntimeFunc::Debug
        | RuntimeFunc::EffectCheck
        | RuntimeFunc::ListSliceOwned
        | RuntimeFunc::StackSlowFrame
        | RuntimeFunc::ScopeOpen
        | RuntimeFunc::ScopeClose
        | RuntimeFunc::UnwindResume => true,
    }
}

/// The CLIF of the exit stub `key`, declared as `id`: it records its
/// caller's frame pointer and return address in the fiber's control block
/// and calls the C function at its second parameter.
///
/// # Errors
///
/// An ICE if Cranelift's builder gives the stub no parameters.
pub(crate) fn translate(
    decls: &Decls,
    key: &ExitKey,
    id: FuncId,
) -> Result<ir::Function, CodegenError> {
    let c = decls.c_call_conv();
    let mut func =
        ir::Function::with_name_signature(ir::UserFuncName::user(0, id.as_u32()), key.signature(c));
    let mut fctx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut fctx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let params = b.block_params(entry).to_vec();
    let [ctx, callee, args @ ..] = params.as_slice() else {
        return Err(CodegenError::ice("an exit stub without `ctx` and callee"));
    };
    // The stub's own frame record holds its caller's frame pointer and
    // the return address of the caller's `try_call` (frame pointers are
    // kept, `isa::flags`).
    let flags = MemFlagsData::trusted();
    let fp = b.ins().get_frame_pointer(WORD);
    let caller_fp = b.ins().load(types::I64, flags, fp, SAVED_FP_OFFSET);
    let return_address = b.ins().load(types::I64, flags, fp, RETURN_ADDRESS_OFFSET);
    b.ins().store(flags, caller_fp, *ctx, abi::CTX_EXIT_FP);
    b.ins().store(flags, return_address, *ctx, abi::CTX_EXIT_PC);
    let c_sig = b.import_signature(key.c_signature(c));
    let call = b.ins().call_indirect(c_sig, *callee, args);
    let results = b.inst_results(call).to_vec();
    b.ins().return_(&results);
    b.seal_all_blocks();
    b.finalize(decls.frontend_config());
    Ok(func)
}
