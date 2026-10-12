//! The `try_call` prototype (D-696): calls with an unwind edge become
//! Cranelift `try_call`s with one catch-all handler that jumps to the MIR
//! pad, and the runtime's frame-pointer unwinder resumes at the handler.
//!
//! - A Cheby callee (the `tail` convention) is `try_call`ed directly.
//! - A C callee (a runtime function or a kernel) is called through the
//!   exit stub of its machine signature (`exit_stub.rs`), a `tail`
//!   function that records its caller's frame pointer and return address
//!   in the fiber's control block (GAP(try-call-exit-record)) and calls
//!   the C function: a `try_call` to a `tail` callee assumes every
//!   register is clobbered at the handler, so the unwinder restores only
//!   the stack and frame pointers.
//! - `Resume` calls `cheby_rt_unwind_resume` through its exit stub, with
//!   no handler in this frame, so the unwinder continues with the caller;
//!   entry thunks and trampolines (`Abi::C`) are catch points and return 0
//!   instead (D-699).
//!
//! What it costs, against the return flag: every value live across a
//! `try_call` is spilled (the `tail` callee clobbers every register on
//! both edges), C calls with an edge pass through a stub frame, and each
//! function gets a table registered with the runtime. Its one
//! optimization beyond what the MIR gives both backends: in a Cheby
//! function, a call whose pad only resumes keeps a plain call
//! (`passes_through`), since the unwinder can pass a frame
//! that has nothing to drop; the return flag has no counterpart, as its
//! caller must still test and return.

use cranelift_codegen::ir::{
    self, BlockArg, BlockCall, ExceptionTableData, ExceptionTableItem, InstBuilder, SigRef,
    TrapCode, Value, types,
};

use cheby_mir::{Abi, BlockId, Terminator};

use crate::CodegenError;
use crate::decls::ForeignDecl;
use crate::func::{CCallee, CallTarget, TRAP_RESUME, Translator};
use crate::runtime::RuntimeFunc;
use crate::sig::WORD;
use crate::unwind::exit_stub::ExitKey;

impl Translator<'_, '_> {
    /// A non-tail call of Cheby code with an unwind edge to `pad`.
    pub(crate) fn tc_cheby_call(
        &mut self,
        target: CallTarget,
        args: &[Value],
        pad: ir::Block,
    ) -> Result<Value, CodegenError> {
        if self.passes_through(pad) {
            let call = self.emit_cheby_call(target, args);
            return self
                .b
                .inst_results(call)
                .first()
                .copied()
                .ok_or_else(|| CodegenError::ice("a Cheby call without a result"));
        }
        let sig = match target {
            CallTarget::Direct(f) => {
                self.b
                    .func
                    .dfg
                    .ext_funcs
                    .get(f)
                    .ok_or_else(|| CodegenError::ice("a call of an unknown function"))?
                    .signature
            }
            CallTarget::Indirect(sig, _) => sig,
        };
        let (cont, results) = self.continuation(sig)?;
        let [result] = results.as_slice() else {
            return Err(CodegenError::ice(
                "a Cheby call whose signature has not one result",
            ));
        };
        let result = *result;
        let (table, handler) = self.exception_table(sig, cont, results.len())?;
        match target {
            CallTarget::Direct(f) => self.b.ins().try_call(f, args, table),
            CallTarget::Indirect(_, code) => self.b.ins().try_call_indirect(code, args, table),
        };
        self.fill_handler(handler, pad, cont);
        Ok(result)
    }

    /// A call of a C function (kernel, runtime helper, `cheby_rt_alloc`,
    /// `cheby_rt_panic`, the stack slow paths) with an unwind edge to
    /// `pad`: a `try_call` of its exit stub.
    pub(crate) fn tc_c_call(
        &mut self,
        callee: &CCallee,
        args: &[Value],
        pad: ir::Block,
    ) -> Result<Option<Value>, CodegenError> {
        let (stub, sig, stub_args) = self.exit_call(callee, args)?;
        if self.passes_through(pad) {
            // The stub still records the exit record the unwinder starts
            // from; the unwinder then passes this frame.
            let call = self.b.ins().call(stub, &stub_args);
            return match self.b.inst_results(call) {
                [] => Ok(None),
                [result] => Ok(Some(*result)),
                _ => Err(CodegenError::ice("a C function with several results")),
            };
        }
        let (cont, results) = self.continuation(sig)?;
        let (table, handler) = self.exception_table(sig, cont, results.len())?;
        self.b.ins().try_call(stub, &stub_args, table);
        self.fill_handler(handler, pad, cont);
        match results.as_slice() {
            [] => Ok(None),
            [result] => Ok(Some(*result)),
            _ => Err(CodegenError::ice("a C function with several results")),
        }
    }

    /// `Terminator::Resume`: in a Cheby function, a call of
    /// `cheby_rt_unwind_resume` through its exit stub, which never
    /// returns; in an entry thunk or a trampoline, a catch point, `return
    /// 0` (D-699).
    pub(crate) fn tc_resume(&mut self) -> Result<(), CodegenError> {
        if self.func.abi == Abi::C {
            let ty = self
                .b
                .func
                .signature
                .returns
                .first()
                .map(|p| p.value_type)
                .ok_or_else(|| CodegenError::ice("an entry thunk without a result"))?;
            let zero = if ty == types::F64 {
                self.b.ins().f64const(0.0)
            } else if ty == types::F32 {
                self.b.ins().f32const(0.0)
            } else {
                self.b.ins().iconst(ty, 0)
            };
            self.b.ins().return_(&[zero]);
            return Ok(());
        }
        let ctx = self.ctx()?;
        let (stub, _, stub_args) =
            self.exit_call(&CCallee::Runtime(RuntimeFunc::UnwindResume), &[ctx])?;
        self.b.ins().call(stub, &stub_args);
        let code =
            TrapCode::user(TRAP_RESUME).ok_or_else(|| CodegenError::ice("invalid trap code"))?;
        self.b.ins().trap(code);
        Ok(())
    }

    /// Whether a call with an unwind edge to `pad` needs no handler: in a
    /// Cheby function, when `pad` is the empty pad, `resume` with nothing
    /// to drop (`pad(∅)`, D-698). The unwinder then passes the frame, as
    /// it passes a resuming one, so the call stays a plain call, without
    /// the cost of a `try_call` (every register clobbered, so every
    /// callee-saved register saved by the prologue and every live value
    /// spilled). Entry thunks and trampolines catch (D-699), so their calls
    /// always keep their handlers.
    ///
    /// With a backend, CLIF block `n` is MIR block `n` (`Translator::run`);
    /// the check falls back to a handler if that ever stops holding.
    fn passes_through(&self, pad: ir::Block) -> bool {
        if self.func.abi != Abi::Cheby {
            return false;
        }
        let index = pad.as_u32();
        let is_mir_block = usize::try_from(index)
            .ok()
            .and_then(|n| self.blocks.get(n))
            .is_some_and(|b| *b == Some(pad));
        is_mir_block
            && self
                .func
                .block(BlockId::from_u32(index))
                .is_ok_and(|b| b.stmts.is_empty() && matches!(b.term, Terminator::Resume))
    }

    /// The exit stub that calls `callee`, its signature, and its arguments:
    /// `ctx`, the callee's address, then `args`.
    fn exit_call(
        &mut self,
        callee: &CCallee,
        args: &[Value],
    ) -> Result<(ir::FuncRef, SigRef, Vec<Value>), CodegenError> {
        let ctx = self.ctx()?;
        let (key, address) = match callee {
            CCallee::Runtime(f) => {
                let (id, signature) = self.decls.runtime(*f)?;
                let signature = signature.clone();
                let f_ref = self.func_ref(id, &signature, false);
                (ExitKey::runtime(*f), self.b.ins().func_addr(WORD, f_ref))
            }
            CCallee::Foreign { symbol, sig } => {
                let address = match self.decls.foreign(symbol)?.clone() {
                    ForeignDecl::Function(id, signature)
                    | ForeignDecl::FunctionAddress(id, signature) => {
                        let f_ref = self.func_ref(id, &signature, false);
                        self.b.ins().func_addr(WORD, f_ref)
                    }
                    ForeignDecl::Data(id) => self.data_address(id, false),
                };
                (ExitKey::of(&sig.params, sig.ret), address)
            }
        };
        let (id, signature) = self.decls.exit_stub(&key)?;
        let signature = signature.clone();
        let stub = self.func_ref(id, &signature, true);
        let sig = self
            .b
            .func
            .dfg
            .ext_funcs
            .get(stub)
            .ok_or_else(|| CodegenError::ice("an exit stub without its import"))?
            .signature;
        let mut stub_args = Vec::with_capacity(args.len().saturating_add(2));
        stub_args.push(ctx);
        stub_args.push(address);
        stub_args.extend_from_slice(args);
        Ok((stub, sig, stub_args))
    }

    /// The block a `try_call` of signature `sig` continues in when the
    /// callee returns, with one parameter per result; cold when the call is
    /// in a cold block, as for the return flag.
    fn continuation(&mut self, sig: SigRef) -> Result<(ir::Block, Vec<Value>), CodegenError> {
        let returns: Vec<ir::Type> = self
            .b
            .func
            .dfg
            .signatures
            .get(sig)
            .ok_or_else(|| CodegenError::ice("a call of an unknown signature"))?
            .returns
            .iter()
            .map(|p| p.value_type)
            .collect();
        let cont = self.continuation_block();
        let results = returns
            .into_iter()
            .map(|ty| self.b.append_block_param(cont, ty))
            .collect();
        Ok((cont, results))
    }

    /// The exception table of a `try_call` of signature `sig`: the normal
    /// return to `cont` with the call's `results` results, and one
    /// catch-all handler, a new cold block, returned for
    /// [`Self::fill_handler`] once the `try_call` ends the current block.
    fn exception_table(
        &mut self,
        sig: SigRef,
        cont: ir::Block,
        results: usize,
    ) -> Result<(ir::ExceptionTable, ir::Block), CodegenError> {
        let returned = (0..results)
            .map(|n| u32::try_from(n).map(BlockArg::TryCallRet))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| CodegenError::ice("too many results"))?;
        let handler = self.b.create_block();
        self.b.set_cold_block(handler);
        let normal = BlockCall::new(cont, returned, &mut self.b.func.dfg.value_lists);
        let catch = BlockCall::new(handler, [], &mut self.b.func.dfg.value_lists);
        let table = self
            .b
            .func
            .dfg
            .exception_tables
            .push(ExceptionTableData::new(
                sig,
                normal,
                [ExceptionTableItem::Default(catch)],
            ));
        Ok((table, handler))
    }

    /// After the `try_call` that ends the current block: the handler jumps
    /// to the MIR pad `pad` (which takes no parameters), and translation
    /// continues in `cont`.
    fn fill_handler(&mut self, handler: ir::Block, pad: ir::Block, cont: ir::Block) {
        self.b.switch_to_block(handler);
        self.b.ins().jump(pad, &[]);
        self.b.switch_to_block(cont);
    }
}
