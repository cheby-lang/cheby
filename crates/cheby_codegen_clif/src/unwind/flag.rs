//! The return flag (D-733, ADR-0061): an unwinding Cheby frame runs its
//! pad and returns the word 0, which no uniform word is (D-453), and C
//! code that unwinds sets the fiber's flag at `ctx+12`
//! ([`abi::CTX_UNWIND`], D-685) before it returns.
//!
//! What each statement with an unwind edge becomes:
//!
//! - a Cheby call: `brif result, cont, pad` on its result when the
//!   callee's MIR result is a uniform word (`Repr::Value`); after a call
//!   whose result is unboxed (an entry point of D-453), `load.i32 notrap
//!   aligned ctx+12; brif v, pad, cont`, never a test of the result
//!   (D-734);
//! - a C call: `load.i32 notrap aligned ctx+12; brif v, pad, cont`, except
//!   `cheby_rt_alloc` and the boxing functions, whose result is 0 exactly
//!   when they unwound: `brif result, cont, pad`;
//! - `cheby_rt_panic`, which records the panic, sets the flag and returns:
//!   `jump pad`;
//! - `resume`: `return 0` (zero of each result type), in every ABI.
//!
//! The lowering is the one D-702 measured, with no further optimization.
//!
//! It depends on the uniform word never being 0. One path breaks that
//! today: a foreign function whose result is an external type hands its
//! raw C pointer back as the word, so a NULL result reads as an unwinding
//! with no record, and the program silently skips the rest of the caller
//! (review finding R3-ROB-2). D-590 makes an external value a 16-byte
//! object, which is never 0, but the compiler does not build it yet (the
//! native host library comes with M3, OQ-P9); such a value is unsupported
//! at run time until then. The external-value work must keep the word
//! non-zero.

use cranelift_codegen::ir::{self, InstBuilder, MemFlagsData, Value, types};

use cheby_mir::Repr;
use cheby_runtime::abi;

use crate::CodegenError;
use crate::func::{CCallee, CallTarget, Translator};
use crate::runtime::RuntimeFunc;

/// Whether `callee`'s result is 0 exactly when it unwound, so the code
/// tests the result instead of loading the flag: `cheby_rt_alloc` and the
/// boxing functions return an object, which is never 0.
const fn tests_result(callee: &CCallee) -> bool {
    matches!(
        callee,
        CCallee::Runtime(
            RuntimeFunc::Alloc
                | RuntimeFunc::BoxI64
                | RuntimeFunc::BoxU64
                | RuntimeFunc::BoxF64
                | RuntimeFunc::BoxF32
        )
    )
}

impl Translator<'_, '_> {
    /// A non-tail call of Cheby code whose MIR result is `ret`, with an
    /// unwind edge to `pad`: the call's result, with the builder in the
    /// block that continues after it.
    pub(crate) fn unwind_cheby_call(
        &mut self,
        target: CallTarget,
        ret: Repr,
        args: &[Value],
        pad: ir::Block,
    ) -> Result<Value, CodegenError> {
        let inst = self.emit_cheby_call(target, args);
        let result = self
            .b
            .inst_results(inst)
            .first()
            .copied()
            .ok_or_else(|| CodegenError::ice("a Cheby call without a result"))?;
        let cont = self.continuation_block();
        if ret == Repr::Value {
            // A uniform word is never 0 (D-453): 0 means it unwound.
            self.b.ins().brif(result, cont, &[], pad, &[]);
        } else {
            // D-734: an unboxed result (an entry point of D-453) can be 0
            // legitimately, even when it is a machine word like `Int`'s, so
            // the flag tells, never the result. The MIR representation
            // decides, not the machine type.
            self.branch_on_flag(pad, cont)?;
        }
        self.b.switch_to_block(cont);
        Ok(result)
    }

    /// A call of a C function (kernel, runtime helper, `cheby_rt_alloc`,
    /// `cheby_rt_panic`, the stack slow paths) with an unwind edge to
    /// `pad`: its result, if it has one, with the builder in the block
    /// that continues after it.
    pub(crate) fn unwind_c_call(
        &mut self,
        callee: &CCallee,
        args: &[Value],
        pad: ir::Block,
    ) -> Result<Option<Value>, CodegenError> {
        let result = self.emit_c_call(callee, args)?;
        let cont = self.continuation_block();
        match result {
            Some(object) if tests_result(callee) => {
                self.b.ins().brif(object, cont, &[], pad, &[]);
            }
            _ => self.branch_on_flag(pad, cont)?,
        }
        self.b.switch_to_block(cont);
        Ok(result)
    }

    /// `cheby_rt_panic(args)`, which sets the flag and returns, then a
    /// jump to `pad`; it ends the current block.
    pub(crate) fn unwind_panic(
        &mut self,
        args: &[Value],
        pad: ir::Block,
    ) -> Result<(), CodegenError> {
        self.runtime_call(RuntimeFunc::Panic, args)?;
        self.b.ins().jump(pad, &[]);
        Ok(())
    }

    /// `Terminator::Resume`: returns 0 to the caller, which tests it (a
    /// Cheby caller) or reads the flag (a catch point: the runtime that
    /// called an entry thunk or a trampoline).
    pub(crate) fn unwind_resume(&mut self) -> Result<(), CodegenError> {
        let returns: Vec<ir::Type> = self
            .b
            .func
            .signature
            .returns
            .iter()
            .map(|param| param.value_type)
            .collect();
        let mut zeros = Vec::with_capacity(returns.len());
        for ty in returns {
            let zero = if ty == types::F64 {
                self.b.ins().f64const(0.0)
            } else if ty == types::F32 {
                self.b.ins().f32const(0.0)
            } else if ty.is_int() {
                self.b.ins().iconst(ty, 0)
            } else {
                return Err(CodegenError::ice(format!(
                    "`resume` in a function returning {ty}"
                )));
            };
            zeros.push(zero);
        }
        self.b.ins().return_(&zeros);
        Ok(())
    }

    /// `v = load.i32 notrap aligned ctx+12; brif v, pad, cont`.
    fn branch_on_flag(&mut self, pad: ir::Block, cont: ir::Block) -> Result<(), CodegenError> {
        let ctx = self.ctx()?;
        let flag = self
            .b
            .ins()
            .load(types::I32, MemFlagsData::trusted(), ctx, abi::CTX_UNWIND);
        self.b.ins().brif(flag, pad, &[], cont, &[]);
        Ok(())
    }
}
