//! One MIR function to Cranelift IR (E1, D-453).
//!
//! This file holds the translator, calls, constants, statements and
//! terminators; `arith.rs` lowers the scalar operations and `values.rs`
//! the objects (boxing, allocation, fields, tags, constants' slots,
//! runtime helpers and count operations).

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::immediates::{Ieee32, Ieee64, Imm64};
use cranelift_codegen::ir::{
    self, BlockArg, ExtFuncData, ExternalName, FuncRef, GlobalValue, GlobalValueData, InstBuilder,
    MemFlagsData, SigRef, Signature, TrapCode, UserExternalName, Value, types,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Switch};
use cranelift_module::{DataId, FuncId};
use rustc_hash::{FxHashMap, FxHashSet};

use cheby_mir::{
    Abi, AbiSig, BlockId, Callee, Const, FnKey, Function, Local, MirModule, Operand, PanicKind,
    Repr, Rvalue, Stmt, StrId, Terminator, uniform_sig,
};
use cheby_runtime::abi::{self, UnwindBackend};

use crate::decls::{Decls, ForeignDecl};
use crate::mangle::{self, Kind};
use crate::runtime::RuntimeFunc;
use crate::sig::{self, WORD, repr_type};
use crate::{CallMode, CodegenCx, CodegenError, CodegenOptions};

/// The trap after a call that does not return (`cheby_rt_panic`).
const TRAP_NORETURN: u8 = 1;
/// The trap of an `unreachable` terminator or a `switch` without default.
const TRAP_UNREACHABLE: u8 = 2;
/// The trap of a `resume` terminator (D-698). With unwind edges ignored,
/// pads are never translated, so a `resume` is reached only through
/// normal control flow, which the validator rules out; the unwinding
/// backends replace it (D-696).
pub(crate) const TRAP_RESUME: u8 = 3;

/// Translates `func` of `unit` into a Cranelift function named by its
/// declaration. Reads only `decls`, so it needs no `&mut Module` and can
/// run in parallel (D-263).
///
/// # Errors
///
/// An ICE if the MIR uses something code generation does not support or
/// breaks an invariant the validator checks.
///
/// Translation reads no layout id: code loads user headers from their
/// `cb$layout$` words (D-454), so `_cx` is only needed by [`crate::define`]
/// for the data objects.
pub fn translate(
    unit: &MirModule,
    func: &Function,
    decls: &Decls,
    _cx: &CodegenCx<'_>,
    opts: &CodegenOptions,
) -> Result<ir::Function, CodegenError> {
    let id = decls.func(func.key)?.id;
    build(unit, func, decls, opts, id, |t| t.run(EntryCheck::Frame))
}

/// Translates `func` as the unchecked inner body `inner` of a function
/// with a large frame (D-679): the code [`translate`] gives, without the
/// entry check, which the outer function of [`translate_large_frame_outer`]
/// makes for the whole frame.
///
/// # Errors
///
/// As for [`translate`].
pub(crate) fn translate_large_frame_body(
    unit: &MirModule,
    func: &Function,
    decls: &Decls,
    opts: &CodegenOptions,
    inner: FuncId,
) -> Result<ir::Function, CodegenError> {
    build(unit, func, decls, opts, inner, |t| t.run(EntryCheck::None))
}

/// Translates the small outer function of `func`, whose body `inner` has a
/// frame of `frame_size` bytes (D-679). It has `func`'s symbol and
/// signature, so slots and callers are unchanged (D-043), checks the stack
/// for the whole frame and tail-calls `inner` ([`Translator::outer_entry`]).
///
/// # Errors
///
/// An ICE if `func` is not a Cheby function with an entry location.
pub(crate) fn translate_large_frame_outer(
    unit: &MirModule,
    func: &Function,
    decls: &Decls,
    opts: &CodegenOptions,
    inner: FuncId,
    frame_size: u32,
) -> Result<ir::Function, CodegenError> {
    let id = decls.func(func.key)?.id;
    build(unit, func, decls, opts, id, |t| {
        t.outer_entry(frame_size, inner)
    })
}

/// A CLIF function named `id` with `func`'s signature, its body made by
/// `body`.
fn build(
    unit: &MirModule,
    func: &Function,
    decls: &Decls,
    opts: &CodegenOptions,
    id: FuncId,
    body: impl FnOnce(&mut Translator<'_, '_>) -> Result<(), CodegenError>,
) -> Result<ir::Function, CodegenError> {
    let decl = decls.func(func.key)?;
    let mut clif =
        ir::Function::with_name_signature(ir::UserFuncName::user(0, id.as_u32()), decl.sig.clone());
    let mut fctx = FunctionBuilderContext::new();
    let builder = FunctionBuilder::new(&mut clif, &mut fctx);
    let mut t = Translator {
        b: builder,
        unit,
        func,
        decls,
        opts,
        values: Vec::new(),
        blocks: Vec::new(),
        ctx: None,
        func_refs: FxHashMap::default(),
        data: FxHashMap::default(),
        sigs: FxHashMap::default(),
        panics: Vec::new(),
        panic_blocks: FxHashMap::default(),
    };
    body(&mut t)?;
    t.b.seal_all_blocks();
    t.b.finalize(decls.frontend_config());
    Ok(clif)
}

/// Which stack check a function's entry block makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryCheck {
    /// The check of D-124, for a Cheby function when
    /// [`CodegenOptions::stack_check`] is set.
    Frame,
    /// None: the inner body of a large frame, whose outer function checked
    /// (D-679).
    None,
}

pub(crate) struct Translator<'a, 'f> {
    pub(crate) b: FunctionBuilder<'f>,
    pub(crate) unit: &'a MirModule,
    pub(crate) func: &'a Function,
    pub(crate) decls: &'a Decls,
    pub(crate) opts: &'a CodegenOptions,
    /// Each MIR local's CLIF value, set where the local is defined. MIR is
    /// SSA with block parameters (every local defined once, every use
    /// dominated by its definition), so a local is one CLIF value and
    /// translation needs no `Variable`s: Cranelift's SSA construction
    /// costs memory per variable and block, which made a function of `n`
    /// locals spread over `n` blocks quadratic (a list literal of `n`
    /// elements).
    values: Vec<Option<Value>>,
    /// Each MIR block's CLIF block, by MIR block index; `None` for a block
    /// that only unwinding reaches when unwind edges are ignored.
    blocks: Vec<Option<ir::Block>>,
    ctx: Option<Value>,
    func_refs: FxHashMap<FuncId, FuncRef>,
    data: FxHashMap<DataId, GlobalValue>,
    sigs: FxHashMap<AbiSig, SigRef>,
    /// The cold blocks that panic with a kind at a location, created by
    /// checked operations and filled after the function's blocks, one per
    /// kind and location.
    panics: Vec<(PanicKind, StrId, ir::Block)>,
    /// The block of each entry of `panics`, by kind and location, so a
    /// function with many checked operations finds them in constant time.
    panic_blocks: FxHashMap<(PanicKind, StrId), ir::Block>,
}

/// Where a call goes: a known function, or an address with a signature.
enum Target {
    Direct(FuncRef),
    Indirect(SigRef, Value),
}

pub(crate) fn unsupported(what: &str) -> CodegenError {
    CodegenError::ice(format!("code generation does not support {what}"))
}

fn trap(code: u8) -> Result<TrapCode, CodegenError> {
    TrapCode::user(code).ok_or_else(|| CodegenError::ice("invalid trap code"))
}

/// The blocks reachable from `bb0`, in reverse postorder: every block
/// after its dominators. Successors are visited last to first, so the
/// order follows the source (a branch's `then` before its `else`).
/// Iterative, as a long function nests deeply.
fn reverse_postorder(func: &Function) -> Result<Vec<BlockId>, CodegenError> {
    let count = func.blocks.len();
    let mut visited = vec![false; count];
    let mut post = Vec::with_capacity(count);
    // Each frame: a block and its successors not yet visited; `pop`
    // takes the last one first.
    let mut stack: Vec<(BlockId, Vec<BlockId>)> = Vec::new();
    let mut enter = |id: BlockId, stack: &mut Vec<(BlockId, Vec<BlockId>)>| {
        let seen = usize::try_from(id.as_u32())
            .ok()
            .and_then(|n| visited.get_mut(n))
            .ok_or_else(|| CodegenError::ice(format!("unknown MIR block {id:?}")))?;
        if !std::mem::replace(seen, true) {
            stack.push((id, func.block(id)?.term.successors()));
        }
        Ok::<_, CodegenError>(())
    };
    enter(BlockId::from_u32(0), &mut stack)?;
    while let Some((id, succs)) = stack.last_mut() {
        if let Some(next) = succs.pop() {
            enter(next, &mut stack)?;
        } else {
            post.push(*id);
            stack.pop();
        }
    }
    post.reverse();
    Ok(post)
}

/// For each block, whether only unwinding reaches it: it is reachable from
/// `bb0` and the entry pad through normal and unwind edges
/// ([`Function::successors`]), but not from `bb0` through normal edges
/// alone. These are the pads of the unwind pass (D-698). A block that
/// nothing reaches is not one of them. Iterative, as a long function nests
/// deeply.
fn unwind_only_blocks(func: &Function) -> Result<Vec<bool>, CodegenError> {
    let count = func.blocks.len();
    let reach = |roots: &[BlockId], with_unwind: bool| {
        let mut seen = vec![false; count];
        let mut stack = roots.to_vec();
        while let Some(id) = stack.pop() {
            let slot = usize::try_from(id.as_u32())
                .ok()
                .and_then(|n| seen.get_mut(n))
                .ok_or_else(|| CodegenError::ice(format!("unknown MIR block {id:?}")))?;
            if std::mem::replace(slot, true) {
                continue;
            }
            if with_unwind {
                stack.extend(func.successors(id)?);
            } else {
                stack.extend(func.block(id)?.term.successors());
            }
        }
        Ok::<_, CodegenError>(seen)
    };
    let first = BlockId::from_u32(0);
    let normal = reach(&[first], false)?;
    let mut roots = vec![first];
    roots.extend(func.entry_unwind);
    let all = reach(&roots, true)?;
    Ok(all
        .into_iter()
        .zip(normal)
        .map(|(any, normal)| any && !normal)
        .collect())
}

/// `bits` truncated to the width of `ty`, as Cranelift wants an `iconst`
/// immediate of a narrow type.
pub(crate) fn truncate(bits: u64, ty: ir::Type) -> i64 {
    let width = ty.bits();
    let masked = if width >= 64 {
        bits
    } else {
        bits & ((1_u64 << width).wrapping_sub(1))
    };
    masked.cast_signed()
}

impl Translator<'_, '_> {
    fn run(&mut self, check: EntryCheck) -> Result<(), CodegenError> {
        self.values = vec![None; self.func.locals.len()];
        // CLIF block `n` is MIR block `n`, its parameters created up front
        // so a jump may precede its target's translation. With unwind
        // edges ignored (`off`), a block that only unwinding reaches gets
        // no CLIF block: it is never translated, and creating it would
        // renumber every later block and value, so the CLIF of a function
        // would differ from M1's with the unwind pass in (D-698).
        let skipped = if self.opts.unwind == UnwindBackend::Off {
            unwind_only_blocks(self.func)?
        } else {
            vec![false; self.func.blocks.len()]
        };
        for (block, skip) in self.func.blocks.iter().zip(skipped) {
            if skip {
                self.blocks.push(None);
                continue;
            }
            let cl = self.b.create_block();
            for &param in &block.params {
                let ty = repr_type(self.func.local(param)?.repr);
                let value = self.b.append_block_param(cl, ty);
                self.bind(param, value)?;
            }
            self.blocks.push(Some(cl));
        }
        self.entry(check)?;
        // Reverse postorder translates a definition before its uses, as a
        // block's dominators come before it. Unreachable blocks are never
        // translated (their CLIF blocks stay out of the layout): no
        // reachable block jumps to one, and their uses need not be
        // dominated.
        for id in reverse_postorder(self.func)? {
            let block = self.func.block(id)?;
            let cl = self.block(id)?;
            self.b.switch_to_block(cl);
            for stmt in &block.stmts {
                self.stmt(stmt)?;
            }
            self.terminator(&block.term)?;
        }
        for (kind, loc, block) in std::mem::take(&mut self.panics) {
            self.b.switch_to_block(block);
            self.panic(kind, None, loc)?;
        }
        Ok(())
    }

    /// The entry block: the parameters, the stack check of a Cheby
    /// function (unless `check` is [`EntryCheck::None`]), then a jump to
    /// `bb0`.
    fn entry(&mut self, check: EntryCheck) -> Result<(), CodegenError> {
        let entry = self.b.create_block();
        self.b.append_block_params_for_function_params(entry);
        self.b.switch_to_block(entry);
        let params = self.b.block_params(entry).to_vec();
        let (&ctx, rest) = params
            .split_first()
            .ok_or_else(|| CodegenError::ice("a function without `ctx`"))?;
        if rest.len() != self.func.params.len() {
            return Err(CodegenError::ice(
                "parameter count differs from the signature",
            ));
        }
        self.ctx = Some(ctx);
        for (&local, &value) in self.func.params.iter().zip(rest) {
            self.bind(local, value)?;
        }
        if let FnKey::Trampoline(_) = self.func.key {
            // D-526: the runtime lends a trampoline its closure, but MIR
            // parameters are owned, and the trampoline's call consumes
            // the closure as `self`. One more reference here keeps the
            // caller's.
            let closure = rest
                .first()
                .copied()
                .ok_or_else(|| CodegenError::ice("a trampoline without its closure"))?;
            self.dup(closure)?;
        }
        let first = self.block(BlockId::from_u32(0))?;
        if self.func.abi == Abi::Cheby && self.opts.stack_check && check == EntryCheck::Frame {
            // D-124: one compare on the fast path; the runtime's slow path
            // tells a real overflow (a panic) from an interrupt request.
            let loc = self
                .func
                .entry_loc
                .ok_or_else(|| CodegenError::ice("a Cheby function without an entry location"))?;
            let sp = self.b.ins().get_stack_pointer(WORD);
            let limit = self
                .b
                .ins()
                .load(WORD, MemFlagsData::trusted(), ctx, abi::CTX_STACK_LIMIT);
            let below = self.b.ins().icmp(IntCC::UnsignedLessThan, sp, limit);
            let slow = self.b.create_block();
            self.b.set_cold_block(slow);
            self.b.ins().brif(below, slow, &[], first, &[]);
            self.b.switch_to_block(slow);
            let loc = self.string(loc)?;
            self.runtime_call(RuntimeFunc::StackSlow, &[ctx, loc])?;
            self.b.ins().jump(first, &[]);
        } else {
            self.b.ins().jump(first, &[]);
        }
        Ok(())
    }

    /// The whole body of the outer function of a large frame (D-679): the
    /// entry check `sp - frame_size - 64 KiB <u [ctx+0]` (the runtime's
    /// `LARGE_FRAME_MARGIN`, which its slow path uses too), whose slow path
    /// is `cheby_rt_stack_slow_frame(ctx, location, frame_size)`
    /// (GAP(stack-slow-frame)), then a `return_call` of the inner body
    /// `inner` with every parameter.
    ///
    /// The check accounts for the frame before it is allocated: the inner
    /// body then starts with at least 64 KiB above the limit below its
    /// frame, the margin every function-entry check keeps for a frame that
    /// is not large. The subtraction cannot wrap, as a stack is never
    /// within `frame_size + 64 KiB` of address 0, and comparing `sp` with
    /// `limit + …` instead would wrap when an interrupt sets the limit to
    /// the highest address (D-453).
    fn outer_entry(&mut self, frame_size: u32, inner: FuncId) -> Result<(), CodegenError> {
        if self.func.abi != Abi::Cheby {
            return Err(CodegenError::ice("a large frame outside a Cheby function"));
        }
        let entry = self.b.create_block();
        self.b.append_block_params_for_function_params(entry);
        self.b.switch_to_block(entry);
        let params = self.b.block_params(entry).to_vec();
        let &ctx = params
            .first()
            .ok_or_else(|| CodegenError::ice("a function without `ctx`"))?;
        self.ctx = Some(ctx);
        let loc = self
            .func
            .entry_loc
            .ok_or_else(|| CodegenError::ice("a Cheby function without an entry location"))?;
        let needed = u64::from(frame_size)
            .checked_add(cheby_runtime::panic::LARGE_FRAME_MARGIN)
            .and_then(|n| i64::try_from(n).ok())
            .ok_or_else(|| CodegenError::ice("a frame size that overflows"))?;
        let call = self.b.create_block();
        let sp = self.b.ins().get_stack_pointer(WORD);
        let needed = self.b.ins().iconst(WORD, needed);
        let low = self.b.ins().isub(sp, needed);
        let limit = self
            .b
            .ins()
            .load(WORD, MemFlagsData::trusted(), ctx, abi::CTX_STACK_LIMIT);
        let below = self.b.ins().icmp(IntCC::UnsignedLessThan, low, limit);
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        self.b.ins().brif(below, slow, &[], call, &[]);
        self.b.switch_to_block(slow);
        let loc = self.string(loc)?;
        let size = self.b.ins().iconst(types::I64, i64::from(frame_size));
        self.runtime_call(RuntimeFunc::StackSlowFrame, &[ctx, loc, size])?;
        self.b.ins().jump(call, &[]);
        self.b.switch_to_block(call);
        let signature = self.decls.func(self.func.key)?.sig.clone();
        let f = self.func_ref(inner, &signature, true);
        self.b.ins().return_call(f, &params);
        Ok(())
    }

    pub(crate) fn ctx(&self) -> Result<Value, CodegenError> {
        self.ctx
            .ok_or_else(|| CodegenError::ice("`ctx` used before the entry block"))
    }

    /// The value of `local`, defined by a block already translated.
    pub(crate) fn local(&self, local: Local) -> Result<Value, CodegenError> {
        usize::try_from(local.as_u32())
            .ok()
            .and_then(|n| self.values.get(n))
            .ok_or_else(|| CodegenError::ice(format!("unknown MIR local {local:?}")))?
            .ok_or_else(|| {
                CodegenError::ice(format!(
                    "MIR local {local:?} used where its definition does not dominate"
                ))
            })
    }

    /// Makes `value` the value of `local`, which has no value yet and is
    /// declared with `value`'s type.
    fn bind(&mut self, local: Local, value: Value) -> Result<(), CodegenError> {
        let ty = repr_type(self.func.local(local)?.repr);
        if self.b.func.dfg.value_type(value) != ty {
            return Err(CodegenError::ice(format!(
                "MIR local {local:?} of type {ty} defined by a value of another type"
            )));
        }
        let slot = usize::try_from(local.as_u32())
            .ok()
            .and_then(|n| self.values.get_mut(n))
            .ok_or_else(|| CodegenError::ice(format!("unknown MIR local {local:?}")))?;
        if slot.replace(value).is_some() {
            return Err(CodegenError::ice(format!(
                "MIR local {local:?} is defined more than once"
            )));
        }
        Ok(())
    }

    fn block(&self, block: BlockId) -> Result<ir::Block, CodegenError> {
        usize::try_from(block.as_u32())
            .ok()
            .and_then(|n| self.blocks.get(n))
            .copied()
            .ok_or_else(|| CodegenError::ice(format!("unknown MIR block {block:?}")))?
            .ok_or_else(|| {
                CodegenError::ice(format!(
                    "MIR block {block:?}, reached only by unwinding, used with unwind edges ignored"
                ))
            })
    }

    /// The cold block that panics with `kind` at `loc`.
    pub(crate) fn panic_block(&mut self, kind: PanicKind, loc: StrId) -> ir::Block {
        if let Some(&block) = self.panic_blocks.get(&(kind, loc)) {
            return block;
        }
        let block = self.b.create_block();
        self.b.set_cold_block(block);
        self.panics.push((kind, loc, block));
        self.panic_blocks.insert((kind, loc), block);
        block
    }

    /// Branches to the panic block of `kind` at `loc` when `cond` is
    /// non-zero, and continues in a new block otherwise.
    pub(crate) fn panic_if(&mut self, cond: Value, kind: PanicKind, loc: StrId) {
        let panic = self.panic_block(kind, loc);
        let ok = self.b.create_block();
        self.b.ins().brif(cond, panic, &[], ok, &[]);
        self.b.switch_to_block(ok);
    }

    fn stmt(&mut self, stmt: &Stmt) -> Result<(), CodegenError> {
        match stmt {
            Stmt::Assign { dst, value } => {
                let v = self
                    .rvalue(value)?
                    .ok_or_else(|| CodegenError::ice("a void call assigned to a local"))?;
                self.bind(*dst, v)?;
            }
            Stmt::Eval(value) => {
                if self.rvalue(value)?.is_some() {
                    return Err(CodegenError::ice("`eval` of a call with a result"));
                }
            }
            Stmt::Dup(local) => {
                let v = self.local(*local)?;
                self.dup(v)?;
            }
            Stmt::Drop(local) => {
                let v = self.local(*local)?;
                self.drop_value(v)?;
            }
            Stmt::ClearUnwind => {
                // D-693: the fiber's unwind flag, a `u32` (D-685).
                let ctx = self.ctx()?;
                let zero = self.b.ins().iconst(types::I32, 0);
                self.b
                    .ins()
                    .store(MemFlagsData::trusted(), zero, ctx, abi::CTX_UNWIND);
            }
        }
        Ok(())
    }

    fn rvalue(&mut self, value: &Rvalue) -> Result<Option<Value>, CodegenError> {
        Ok(Some(match value {
            Rvalue::Use(op) => self.operand(op)?,
            Rvalue::Call { callee, args } => return self.call(callee, args, false),
            Rvalue::CallForeign { symbol, sig, args } => {
                let vals = self.operands(args)?;
                let inst = match self.decls.foreign(symbol)?.clone() {
                    ForeignDecl::Function(id, signature) => {
                        let f = self.func_ref(id, &signature, false);
                        self.b.ins().call(f, &vals)
                    }
                    // Signature-free imports: the call site's own C
                    // signature (`ForeignImport::Address`).
                    ForeignDecl::Data(id) => {
                        let address = self.data_address(id, false);
                        let s = self.b.import_signature(sig::foreign_signature(
                            sig,
                            self.decls.c_call_conv(),
                        ));
                        self.b.ins().call_indirect(s, address, &vals)
                    }
                    ForeignDecl::FunctionAddress(id, declared) => {
                        let f = self.func_ref(id, &declared, false);
                        let address = self.b.ins().func_addr(WORD, f);
                        let s = self.b.import_signature(sig::foreign_signature(
                            sig,
                            self.decls.c_call_conv(),
                        ));
                        self.b.ins().call_indirect(s, address, &vals)
                    }
                };
                return Ok(self.b.inst_results(inst).first().copied());
            }
            Rvalue::CallRuntime { func, args } => return self.call_helper(*func, args),
            Rvalue::ReadLayout { base } => {
                let base = self.local(*base)?;
                let id = self.layout_id(base);
                self.b.ins().ireduce(types::I32, id)
            }
            Rvalue::Alloc { layout, fields } => self.alloc(*layout, fields)?,
            Rvalue::Field {
                base,
                layout,
                field,
            } => self.field(*base, *layout, *field)?,
            Rvalue::LoadConst(def) => self.load_const(*def)?,
            Rvalue::Box { value, from } => self.box_value(value, *from)?,
            Rvalue::Unbox { value, to } => self.unbox(value, *to)?,
            Rvalue::Checked {
                op,
                lhs,
                rhs,
                repr,
                loc,
            } => self.checked(*op, lhs, rhs, *repr, *loc)?,
            Rvalue::Compare { op, lhs, rhs, repr } => self.compare(*op, lhs, rhs, *repr)?,
            Rvalue::FloatArith { op, lhs, rhs, repr } => self.float_arith(*op, lhs, rhs, *repr)?,
            Rvalue::Bitwise { op, lhs, rhs, repr } => self.bitwise(*op, lhs, rhs, *repr)?,
            Rvalue::FloatNeg { value, .. } => {
                let v = self.operand(value)?;
                self.b.ins().fneg(v)
            }
            Rvalue::Not(value) => {
                let v = self.operand(value)?;
                self.b.ins().bxor_imm_u(v, 1)
            }
            Rvalue::ReadTag { base, ty } => self.read_tag(base, *ty)?,
        }))
    }

    /// The layout id in the header of the object `base`, as an `i64`.
    pub(crate) fn layout_id(&mut self, base: Value) -> Value {
        let header = self
            .b
            .ins()
            .load(types::I64, MemFlagsData::trusted(), base, 0);
        let shift = i64::from(abi::HEADER_LAYOUT_SHIFT);
        self.b.ins().ushr_imm_u(header, shift)
    }

    pub(crate) fn operands(&mut self, ops: &[Operand]) -> Result<Vec<Value>, CodegenError> {
        ops.iter().map(|op| self.operand(op)).collect()
    }

    pub(crate) fn operand(&mut self, op: &Operand) -> Result<Value, CodegenError> {
        match op {
            Operand::Local(local) => self.local(*local),
            Operand::Const(c) => self.constant(c),
        }
    }

    fn constant(&mut self, c: &Const) -> Result<Value, CodegenError> {
        Ok(match c {
            Const::Int(n) => self.b.ins().iconst(types::I64, *n),
            Const::Float(bits) => self.b.ins().f64const(Ieee64::with_bits(*bits)),
            Const::Bool(v) => self.b.ins().iconst(types::I8, i64::from(*v)),
            Const::Sized { repr, bits } => {
                if *repr == Repr::F32 {
                    let bits = u32::try_from(bits & u64::from(u32::MAX))
                        .map_err(|_| CodegenError::ice("an `f32` constant wider than 32 bits"))?;
                    self.b.ins().f32const(Ieee32::with_bits(bits))
                } else if repr.is_sized() {
                    let ty = repr_type(*repr);
                    self.b.ins().iconst(ty, truncate(*bits, ty))
                } else {
                    return Err(CodegenError::ice(format!(
                        "a sized constant of representation {repr}"
                    )));
                }
            }
            Const::Str(id) => self.string(*id)?,
            Const::Nil => self.static_object(abi::sym::STATIC_NIL)?,
            Const::EmptyList => self.static_object(abi::sym::STATIC_EMPTY_LIST)?,
            Const::BuiltinDesc(desc) => {
                let data = self.decls.builtin_desc(*desc)?;
                self.data_address(data, false)
            }
            Const::StaticCtor(def) => {
                let data = self.decls.ctor_object(*def)?;
                let local = crate::layout::builtin_ctor_layout(*def).is_none();
                self.data_address(data, local)
            }
            Const::FnValue(def) => {
                let symbol = mangle::item(Kind::FnVal, &self.decls.def_name(self.unit, *def)?);
                let data = self.decls.static_object(&symbol)?;
                self.data_address(data, true)
            }
            Const::Static(id) => {
                let object = self
                    .unit
                    .static_object(*id)
                    .ok_or_else(|| CodegenError::ice(format!("unknown static {id:?}")))?;
                let data = self
                    .decls
                    .static_object(&self.decls.static_symbol(object))?;
                self.data_address(data, true)
            }
            Const::Code(key) => {
                // D-453: a code word is `address | 1`, so freeing skips it
                // like a tagged integer; functions are aligned to 16.
                let decl = self.decls.func(*key)?;
                let (id, signature) = (decl.id, decl.sig.clone());
                let f = self.func_ref(id, &signature, true);
                let address = self.b.ins().func_addr(WORD, f);
                self.b.ins().bor_imm_u(address, 1)
            }
            // D-692: the fiber's context, passed to a `@std_context`
            // kernel. GAP(const-ctx-operand).
            Const::Ctx => self.ctx()?,
            Const::ErrNil => self.static_object(abi::sym::STATIC_ERR_NIL)?,
        })
    }

    /// The address of string `id` of this function: a static `String`.
    pub(crate) fn string(&mut self, id: StrId) -> Result<Value, CodegenError> {
        let data = self.decls.string(self.func.key, id)?;
        Ok(self.data_address(data, true))
    }

    pub(crate) fn static_object(&mut self, symbol: &str) -> Result<Value, CodegenError> {
        let data = self.decls.static_data(symbol)?;
        Ok(self.data_address(data, false))
    }

    pub(crate) fn data_address(&mut self, data: DataId, colocated: bool) -> Value {
        let gv = if let Some(&gv) = self.data.get(&data) {
            gv
        } else {
            let name = self
                .b
                .func
                .declare_imported_user_function(UserExternalName::new(1, data.as_u32()));
            let gv = self.b.create_global_value(GlobalValueData::Symbol {
                name: ExternalName::user(name),
                offset: Imm64::new(0),
                colocated,
                tls: false,
            });
            self.data.insert(data, gv);
            gv
        };
        self.b.ins().symbol_value(WORD, gv)
    }

    fn func_ref(&mut self, id: FuncId, signature: &Signature, colocated: bool) -> FuncRef {
        if let Some(&f) = self.func_refs.get(&id) {
            return f;
        }
        let sig_ref = self.b.import_signature(signature.clone());
        let name = self
            .b
            .func
            .declare_imported_user_function(UserExternalName::new(0, id.as_u32()));
        let f = self.b.import_function(ExtFuncData {
            name: ExternalName::user(name),
            signature: sig_ref,
            colocated,
            patchable: false,
        });
        self.func_refs.insert(id, f);
        f
    }

    fn sig_ref(&mut self, abi_sig: &AbiSig) -> SigRef {
        if let Some(&s) = self.sigs.get(abi_sig) {
            return s;
        }
        let signature = sig::signature(abi_sig, self.decls.c_call_conv());
        let s = self.b.import_signature(signature);
        self.sigs.insert(abi_sig.clone(), s);
        s
    }

    /// Calls a runtime function and returns its result, if it has one.
    pub(crate) fn runtime_call(
        &mut self,
        f: RuntimeFunc,
        args: &[Value],
    ) -> Result<Option<Value>, CodegenError> {
        let (id, signature) = self.decls.runtime(f)?;
        let signature = signature.clone();
        let f = self.func_ref(id, &signature, false);
        let inst = self.b.ins().call(f, args);
        Ok(self.b.inst_results(inst).first().copied())
    }

    /// Calls a runtime function that has a result.
    pub(crate) fn runtime_value(
        &mut self,
        f: RuntimeFunc,
        args: &[Value],
    ) -> Result<Value, CodegenError> {
        self.runtime_call(f, args)?
            .ok_or_else(|| CodegenError::ice(format!("the runtime function {f:?} returns nothing")))
    }

    /// A Cheby-ABI call; with `tail`, a `return_call` (D-015) that ends
    /// the block.
    fn call(
        &mut self,
        callee: &Callee,
        args: &[Operand],
        tail: bool,
    ) -> Result<Option<Value>, CodegenError> {
        let ctx = self.ctx()?;
        let mut vals = vec![ctx];
        let target = match callee {
            Callee::Direct(def) => {
                let decl = self.unit.callee(*def).ok_or_else(|| {
                    CodegenError::ice(format!("callee {def:?} has no declaration in its unit"))
                })?;
                let abi_sig = decl.sig.clone();
                match self.opts.call_mode {
                    CallMode::Indirect => {
                        // D-043: every call of a top-level function loads
                        // its slot, which is writable (no `readonly`).
                        let path = self.decls.slot_path(self.unit, *def)?;
                        let slot = self.decls.slot(&path, &abi_sig)?.id;
                        let address = self.data_address(slot, true);
                        let code = self.b.ins().load(WORD, MemFlagsData::trusted(), address, 0);
                        Target::Indirect(self.sig_ref(&abi_sig), code)
                    }
                    CallMode::Direct => {
                        let decl = self.decls.func(FnKey::Item(*def))?;
                        let (id, signature) = (decl.id, decl.sig.clone());
                        Target::Direct(self.func_ref(id, &signature, true))
                    }
                }
            }
            Callee::Code(key) => {
                let decl = self.decls.func(*key)?;
                let (id, signature) = (decl.id, decl.sig.clone());
                Target::Direct(self.func_ref(id, &signature, true))
            }
            Callee::Indirect(closure) => {
                // The uniform convention (D-453): the code word
                // `addr | 1` at offset 8, the closure itself as `self`.
                let closure = self.local(*closure)?;
                let word = self.b.ins().load(
                    WORD,
                    MemFlagsData::trusted(),
                    closure,
                    abi::FIRST_FIELD_OFFSET,
                );
                let code = self.b.ins().band_imm_s(word, !1_i64);
                vals.push(closure);
                let arity = u32::try_from(args.len())
                    .map_err(|_| CodegenError::ice("too many arguments"))?;
                Target::Indirect(self.sig_ref(&uniform_sig(arity)), code)
            }
        };
        vals.extend(self.operands(args)?);
        let inst = match (target, tail) {
            (Target::Direct(f), false) => self.b.ins().call(f, &vals),
            (Target::Direct(f), true) => self.b.ins().return_call(f, &vals),
            (Target::Indirect(s, code), false) => self.b.ins().call_indirect(s, code, &vals),
            (Target::Indirect(s, code), true) => self.b.ins().return_call_indirect(s, code, &vals),
        };
        if tail {
            return Ok(None);
        }
        Ok(self.b.inst_results(inst).first().copied())
    }

    fn block_args(&mut self, args: &[Operand]) -> Result<Vec<BlockArg>, CodegenError> {
        Ok(self
            .operands(args)?
            .into_iter()
            .map(BlockArg::Value)
            .collect())
    }

    /// Calls `cheby_rt_panic(ctx, kind, message or 0, loc)`, which does not
    /// return, and traps after it.
    fn panic(
        &mut self,
        kind: PanicKind,
        message: Option<&Operand>,
        loc: StrId,
    ) -> Result<(), CodegenError> {
        let ctx = self.ctx()?;
        let kind = self.b.ins().iconst(types::I32, i64::from(kind.code()));
        let message = match message {
            Some(m) => self.operand(m)?,
            None => self.b.ins().iconst(WORD, 0),
        };
        let loc = self.string(loc)?;
        self.runtime_call(RuntimeFunc::Panic, &[ctx, kind, message, loc])?;
        self.b.ins().trap(trap(TRAP_NORETURN)?);
        Ok(())
    }

    fn terminator(&mut self, term: &Terminator) -> Result<(), CodegenError> {
        match term {
            Terminator::Return(op) => {
                let v = self.operand(op)?;
                self.b.ins().return_(&[v]);
            }
            Terminator::TailCall { callee, args } => {
                self.call(callee, args, true)?;
            }
            Terminator::Goto { target, args } => {
                let target = self.block(*target)?;
                let args = self.block_args(args)?;
                self.b.ins().jump(target, &args);
            }
            Terminator::Branch { cond, then_, else_ } => {
                let cond = self.operand(cond)?;
                let then_ = self.block(*then_)?;
                let else_ = self.block(*else_)?;
                self.b.ins().brif(cond, then_, &[], else_, &[]);
            }
            Terminator::Switch { on, cases, default } => self.switch(on, cases, *default)?,
            Terminator::Panic { kind, message, loc } => {
                self.panic(*kind, message.as_ref(), *loc)?;
            }
            Terminator::Unreachable => {
                self.b.ins().trap(trap(TRAP_UNREACHABLE)?);
            }
            Terminator::Resume => {
                self.b.ins().trap(trap(TRAP_RESUME)?);
            }
        }
        Ok(())
    }

    fn switch(
        &mut self,
        on: &Operand,
        cases: &[(u64, BlockId)],
        default: Option<BlockId>,
    ) -> Result<(), CodegenError> {
        let value = self.operand(on)?;
        let ty = self.b.func.dfg.value_type(value);
        if !ty.is_int() {
            return Err(CodegenError::ice(
                "`switch` on a value that is not an integer",
            ));
        }
        let mut switch = Switch::new();
        let mut seen = FxHashSet::default();
        for &(bits, target) in cases {
            if truncate(bits, ty).cast_unsigned() != bits {
                return Err(CodegenError::ice(format!(
                    "`switch` case {bits} does not fit its value's type"
                )));
            }
            if !seen.insert(bits) {
                return Err(CodegenError::ice(format!("duplicate `switch` case {bits}")));
            }
            let target = self.block(target)?;
            switch.set_entry(u128::from(bits), target);
        }
        let (otherwise, trap_block) = if let Some(d) = default {
            (self.block(d)?, None)
        } else {
            let t = self.b.create_block();
            (t, Some(t))
        };
        switch.emit(&mut self.b, value, otherwise);
        if let Some(t) = trap_block {
            self.b.switch_to_block(t);
            self.b.ins().trap(trap(TRAP_UNREACHABLE)?);
        }
        Ok(())
    }
}
