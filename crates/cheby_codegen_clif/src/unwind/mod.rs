//! The unwinding backends' code generation (D-696, D-698, D-702).
//!
//! The unwind pass gives every statement that may start or pass on an
//! unwinding an edge to a pad (D-698). With
//! [`UnwindBackend::Off`](cheby_runtime::abi::UnwindBackend::Off), the
//! default, codegen ignores the edges and the pads stay untranslated. With
//! one of the two prototypes behind the hidden `CHEBY_UNWIND` switch, the
//! translator (`func.rs`, the seam) dispatches each statement that has an
//! edge to the backend's methods, which emit the call and the branch to
//! the pad:
//!
//! - [`flag`]: the return-flag prototype.
//! - [`try_call`]: the `try_call` prototype.
//!
//! Neither prototype is the chosen mechanism: both are measured before the
//! user decides (D-696, D-702).

pub(crate) mod exit_stub;
pub(crate) mod flag;
pub(crate) mod try_call;

use cranelift_codegen::ir::{self, InstBuilder, Value};

use cheby_runtime::abi::UnwindBackend;

use crate::CodegenError;
use crate::func::{CCallee, CallTarget, TRAP_NORETURN, Translator, trap};
use crate::runtime::RuntimeFunc;

/// The error of an unwind edge translated with the edges ignored.
fn edges_ignored() -> CodegenError {
    CodegenError::ice("an unwind edge translated with the unwinding backend off")
}

impl Translator<'_, '_> {
    /// A non-tail call of Cheby code with an unwind edge to `pad`, by the
    /// backend: the call's result, with the builder in the block that
    /// continues after it.
    pub(crate) fn unwind_cheby_call(
        &mut self,
        target: CallTarget,
        args: &[Value],
        pad: ir::Block,
    ) -> Result<Value, CodegenError> {
        match self.opts.unwind {
            UnwindBackend::Off => Err(edges_ignored()),
            UnwindBackend::Flag => self.flag_cheby_call(target, args, pad),
            UnwindBackend::TryCall => self.tc_cheby_call(target, args, pad),
        }
    }

    /// A call of the C function `callee` with an unwind edge to `pad`, by
    /// the backend: its result, if it has one, with the builder in the
    /// block that continues after it.
    pub(crate) fn unwind_c_call(
        &mut self,
        callee: &CCallee,
        args: &[Value],
        pad: ir::Block,
    ) -> Result<Option<Value>, CodegenError> {
        match self.opts.unwind {
            UnwindBackend::Off => Err(edges_ignored()),
            UnwindBackend::Flag => self.flag_c_call(callee, args, pad),
            UnwindBackend::TryCall => self.tc_c_call(callee, args, pad),
        }
    }

    /// `cheby_rt_panic(args)` with an unwind edge to `pad`, which ends the
    /// current block: under the return flag the runtime returns and the
    /// code jumps to `pad`; under `try_call` the call never returns, and
    /// a trap ends its continuation.
    pub(crate) fn unwind_panic(
        &mut self,
        args: &[Value],
        pad: ir::Block,
    ) -> Result<(), CodegenError> {
        match self.opts.unwind {
            UnwindBackend::Off => Err(edges_ignored()),
            UnwindBackend::Flag => self.flag_panic(args, pad),
            UnwindBackend::TryCall => {
                self.tc_c_call(&CCallee::Runtime(RuntimeFunc::Panic), args, pad)?;
                self.b.ins().trap(trap(TRAP_NORETURN)?);
                Ok(())
            }
        }
    }

    /// `Terminator::Resume`, by the backend.
    pub(crate) fn unwind_resume(&mut self) -> Result<(), CodegenError> {
        match self.opts.unwind {
            UnwindBackend::Off => Err(edges_ignored()),
            UnwindBackend::Flag => self.flag_resume(),
            UnwindBackend::TryCall => self.tc_resume(),
        }
    }
}
