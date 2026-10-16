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
    Abi, AbiSig, BlockId, Callee, Const, FnKey, ForeignSig, Function, Local, MirModule, Operand,
    PanicKind, Repr, Rvalue, Stmt, StrId, Terminator, uniform_sig,
};
use cheby_runtime::abi;

use crate::decls::{Decls, ForeignDecl};
use crate::mangle::{self, Kind};
use crate::runtime::RuntimeFunc;
use crate::sig::{self, WORD, repr_type};
use crate::{CallMode, CodegenCx, CodegenError, CodegenOptions};

/// The trap after a call that does not return (`cheby_rt_panic`).
pub(crate) const TRAP_NORETURN: u8 = 1;
/// The trap of an `unreachable` terminator or a `switch` without default.
const TRAP_UNREACHABLE: u8 = 2;

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
        pad: None,
        in_pad: false,
        tokens: FxHashSet::default(),
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
    pub(crate) values: Vec<Option<Value>>,
    /// Each MIR block's CLIF block, by MIR block index.
    pub(crate) blocks: Vec<ir::Block>,
    pub(crate) ctx: Option<Value>,
    pub(crate) func_refs: FxHashMap<FuncId, FuncRef>,
    pub(crate) data: FxHashMap<DataId, GlobalValue>,
    pub(crate) sigs: FxHashMap<AbiSig, SigRef>,
    /// The cold blocks that panic with a kind at a location, created by
    /// checked operations and filled after the function's blocks, one per
    /// kind, location and unwind pad (`None` for a check without an edge).
    panics: Vec<(PanicKind, StrId, Option<ir::Block>, ir::Block)>,
    /// The block of each entry of `panics`, by kind, location and pad, so
    /// a function with many checked operations finds them in constant
    /// time.
    panic_blocks: FxHashMap<(PanicKind, StrId, Option<ir::Block>), ir::Block>,
    /// The CLIF block of the unwind pad of the statement or terminator
    /// being translated (D-698); `None` when it has no edge.
    pub(crate) pad: Option<ir::Block>,
    /// Whether the MIR block being translated is a pad, whose drops call
    /// `cheby_rt_drop` out of line to keep the cold code small.
    in_pad: bool,
    /// The reuse tokens translated so far (D-716): a `drop` of one may
    /// find 0, which the inline drop must skip. A token is defined before
    /// any use, so it is here before its `drop`.
    pub(crate) tokens: FxHashSet<Local>,
}

/// Where a Cheby-ABI call goes: a known function, or an address with a
/// signature.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CallTarget {
    Direct(FuncRef),
    Indirect(SigRef, Value),
}

/// A C function that a statement with an unwind edge calls (D-698): a
/// runtime function (a helper, `cheby_rt_alloc`, a boxing function,
/// `cheby_rt_panic`, a stack slow path, `cheby_rt_const_missing`), a
/// foreign kernel by its symbol and C signature, or an `@blocking` foreign
/// function, called on the blocking pool through its shim
/// (`blocking.rs`, D-818).
#[derive(Clone, Debug)]
pub(crate) enum CCallee {
    Runtime(RuntimeFunc),
    Foreign { symbol: Box<str>, sig: ForeignSig },
    Blocking { symbol: Box<str>, sig: ForeignSig },
}

pub(crate) fn unsupported(what: &str) -> CodegenError {
    CodegenError::ice(format!("code generation does not support {what}"))
}

pub(crate) fn trap(code: u8) -> Result<TrapCode, CodegenError> {
    TrapCode::user(code).ok_or_else(|| CodegenError::ice("invalid trap code"))
}

/// The blocks reachable from `roots`, in reverse postorder: every block
/// after its dominators. Successors are visited last to first, so the
/// order follows the source (a branch's `then` before its `else`). A
/// block's successors are [`Function::successors`], its unwind pads
/// included (D-698). The roots are searched in order and their postorders
/// concatenated, so the last root's blocks come first. Iterative, as a
/// long function nests deeply.
fn reverse_postorder(func: &Function, roots: &[BlockId]) -> Result<Vec<BlockId>, CodegenError> {
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
            stack.push((id, func.successors(id)?));
        }
        Ok::<_, CodegenError>(())
    };
    for &root in roots {
        enter(root, &mut stack)?;
        while let Some((id, succs)) = stack.last_mut() {
            if let Some(next) = succs.pop() {
                enter(next, &mut stack)?;
            } else {
                post.push(*id);
                stack.pop();
            }
        }
    }
    post.reverse();
    Ok(post)
}

/// The locals the pad chain from `pad` drops before it resumes, in order:
/// the unwind pass's chained pads (D-698) are drops, then a `goto` of the
/// next pad or a `resume`.
///
/// # Errors
///
/// An ICE if a block of the chain holds anything else.
fn pad_chain_drops(func: &Function, pad: BlockId) -> Result<Vec<Local>, CodegenError> {
    pad_chain(func, pad).map(|(drops, _)| drops)
}

/// The locals the pad chain from `pad` drops, in order, and its last
/// block, the `resume` ([`pad_chain_drops`]).
///
/// # Errors
///
/// An ICE if a block of the chain holds anything but drops, a `goto` of
/// the next pad and a `resume`.
fn pad_chain(func: &Function, pad: BlockId) -> Result<(Vec<Local>, BlockId), CodegenError> {
    let mut drops = Vec::new();
    let mut at = pad;
    // A chain visits each block at most once.
    for _ in 0..=func.blocks.len() {
        let block = func.block(at)?;
        for stmt in &block.stmts {
            let Stmt::Drop(local) = stmt else {
                return Err(CodegenError::ice(format!(
                    "the entry pad chain at {at:?} holds a statement other than `drop`"
                )));
            };
            drops.push(*local);
        }
        match &block.term {
            Terminator::Resume => return Ok((drops, at)),
            Terminator::Goto { target, args } if args.is_empty() => at = *target,
            _ => {
                return Err(CodegenError::ice(format!(
                    "the entry pad chain at {at:?} ends other than with `goto` or `resume`"
                )));
            }
        }
    }
    Err(CodegenError::ice("the entry pad chain loops"))
}

/// How the entry check's slow path of a function releases its counted
/// parameters itself (D-775, refining D-698 and D-743): those its entry
/// pad drops, which leaves out the borrowed ones (D-715).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EntryRelease {
    /// The positions in [`Function::params`] of the words the entry pad
    /// drops, in its order: at most [`abi::ENTRY_UNWIND_WORDS`].
    params: Vec<usize>,
    /// The entry pad chain's last block, the shared `resume` that the slow
    /// block branches to when the helper started an unwinding.
    resume: BlockId,
}

/// Whether `func`'s entry check goes through a releasing slow path
/// (`cheby_rt_stack_slow_unwind`, or the frame variant for a large frame),
/// and with which words: when its entry pad (D-698, D-752) is a chain of
/// at most [`abi::ENTRY_UNWIND_WORDS`] drops of parameters ending in
/// `resume`. The slow block then calls the helper and branches on its
/// result to `resume` or to the body, and codegen does not translate the
/// entry-only part of the chain; the MIR keeps its entry pad
/// (D-775). Otherwise, as with more counted parameters
/// or a precounted body's own entry pad (D-744), the chain is translated
/// as before. `None` too for a function without an entry pad.
pub(crate) fn entry_release(func: &Function) -> Option<EntryRelease> {
    let pad = func.entry_unwind?;
    let (drops, resume) = pad_chain(func, pad).ok()?;
    let most = usize::try_from(abi::ENTRY_UNWIND_WORDS).ok()?;
    if drops.len() > most {
        return None;
    }
    let params = drops
        .iter()
        .map(|local| func.params.iter().position(|p| p == local))
        .collect::<Option<Vec<usize>>>()?;
    Some(EntryRelease { params, resume })
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
        // so a jump may precede its target's translation. The blocks that
        // only unwinding reaches are the pads of the unwind pass (D-698,
        // D-733), and they are cold.
        let pads = unwind_only_blocks(self.func)?;
        for (block, &pad) in self.func.blocks.iter().zip(&pads) {
            let cl = self.b.create_block();
            if pad {
                self.b.set_cold_block(cl);
            }
            for &param in &block.params {
                let ty = repr_type(self.func.local(param)?.repr);
                let value = self.b.append_block_param(cl, ty);
                self.bind(param, value)?;
            }
            self.blocks.push(cl);
        }
        // The entry check's unwinding, when the check is made: through the
        // releasing slow path to the entry pad chain's `resume`
        // (D-775), or else to the translated entry pad.
        let (release, entry_pad) = if self.checks_entry(check) {
            match entry_release(self.func) {
                Some(release) => (Some(release), None),
                None => (None, self.func.entry_unwind),
            }
        } else {
            (None, None)
        };
        self.entry(check, entry_pad, release.as_ref())?;
        // Reverse postorder translates a definition before its uses, as a
        // block's dominators come before it. Unreachable blocks are never
        // translated (their CLIF blocks stay out of the layout): no
        // reachable block jumps to one, and their uses need not be
        // dominated. The order follows the unwind edges too, from the
        // entry pad as well as from `bb0`. With the releasing slow path,
        // only the chain's `resume` is a root: the rest of the chain is
        // translated only where other edges reach it.
        let mut roots: Vec<BlockId> = entry_pad
            .into_iter()
            .chain(release.map(|r| r.resume))
            .collect();
        roots.push(BlockId::from_u32(0));
        for id in reverse_postorder(self.func, &roots)? {
            let block = self.func.block(id)?;
            let cl = self.block(id)?;
            self.b.switch_to_block(cl);
            self.in_pad = usize::try_from(id.as_u32())
                .ok()
                .and_then(|n| pads.get(n))
                .copied()
                .unwrap_or(false);
            // `unwind` is sorted by statement index (D-698).
            let mut edges = block.unwind.iter().peekable();
            let mut pad_of = |i: usize, t: &Self| {
                let at = u32::try_from(i).ok()?;
                let &(_, pad) = edges.next_if(|&&(n, _)| n == at)?;
                Some(t.block(pad))
            };
            for (i, stmt) in block.stmts.iter().enumerate() {
                self.pad = pad_of(i, self).transpose()?;
                self.stmt(stmt)?;
            }
            self.pad = pad_of(block.stmts.len(), self).transpose()?;
            self.terminator(&block.term)?;
            self.pad = None;
            self.in_pad = false;
        }
        for (kind, loc, pad, block) in std::mem::take(&mut self.panics) {
            self.b.switch_to_block(block);
            self.panic(kind, None, loc, pad)?;
        }
        Ok(())
    }

    /// Whether the entry block makes the stack check of D-124.
    fn checks_entry(&self, check: EntryCheck) -> bool {
        self.func.abi == Abi::Cheby && self.opts.stack_check && check == EntryCheck::Frame
    }

    /// The entry block: the parameters, the stack check of a Cheby
    /// function (unless `check` is [`EntryCheck::None`]), then a jump to
    /// `bb0`. With `release`, the check's slow path releases the counted
    /// parameters itself and branches to the chain's `resume` when it
    /// started an unwinding (D-775); with `entry_pad`,
    /// it unwinds to that pad (D-698).
    fn entry(
        &mut self,
        check: EntryCheck,
        entry_pad: Option<BlockId>,
        release: Option<&EntryRelease>,
    ) -> Result<(), CodegenError> {
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
        if self.checks_entry(check) {
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
            if let Some(release) = release {
                // D-775: the helper releases the
                // parameters when it starts an unwinding.
                let words = Self::param_words(release, rest)?;
                let resume = self.block(release.resume)?;
                self.releasing_slow_call(
                    RuntimeFunc::StackSlowUnwind,
                    &[ctx, loc],
                    &words,
                    resume,
                    first,
                )?;
                return Ok(());
            }
            if let Some(pad) = entry_pad {
                // D-698: an overflow unwinds to the entry pad.
                let pad = self.block(pad)?;
                self.unwind_c_call(&CCallee::Runtime(RuntimeFunc::StackSlow), &[ctx, loc], pad)?;
            } else {
                self.runtime_call(RuntimeFunc::StackSlow, &[ctx, loc])?;
            }
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
    /// (D-743), then a `return_call` of the inner body
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
        let args = [ctx, loc, size];
        if let Some(release) = entry_release(self.func) {
            // D-775: the helper releases the parameters
            // when it starts an unwinding, and the outer function resumes
            // from a cold block of its own.
            let words = Self::param_words(&release, params.get(1..).unwrap_or_default())?;
            let resume = self.b.create_block();
            self.b.set_cold_block(resume);
            self.releasing_slow_call(
                RuntimeFunc::StackSlowFrameUnwind,
                &args,
                &words,
                resume,
                call,
            )?;
            self.b.switch_to_block(call);
            let signature = self.decls.func(self.func.key)?.sig.clone();
            let f = self.func_ref(inner, &signature, true);
            self.b.ins().return_call(f, &params);
            self.b.switch_to_block(resume);
            return self.unwind_resume();
        }
        let outer_pad = if let Some(pad) = self.func.entry_unwind {
            // D-698: an overflow unwinds to the outer function's own pad,
            // which drops what the body's entry pad would drop.
            let outer_pad = self.b.create_block();
            self.b.set_cold_block(outer_pad);
            self.unwind_c_call(
                &CCallee::Runtime(RuntimeFunc::StackSlowFrame),
                &args,
                outer_pad,
            )?;
            Some((pad, outer_pad))
        } else {
            self.runtime_call(RuntimeFunc::StackSlowFrame, &args)?;
            None
        };
        self.b.ins().jump(call, &[]);
        self.b.switch_to_block(call);
        let signature = self.decls.func(self.func.key)?.sig.clone();
        let f = self.func_ref(inner, &signature, true);
        self.b.ins().return_call(f, &params);
        if let Some((pad, outer_pad)) = outer_pad {
            self.outer_pad(pad, outer_pad, &params)?;
        }
        Ok(())
    }

    /// The pad of a large frame's outer function (D-679, D-698): the
    /// drops of the MIR entry pad chain from `pad`, out of line, applied
    /// to the outer function's parameters `params` (`ctx` first), then a
    /// resume, all in the cold block `block`.
    fn outer_pad(
        &mut self,
        pad: BlockId,
        block: ir::Block,
        params: &[Value],
    ) -> Result<(), CodegenError> {
        let values = params.get(1..).unwrap_or_default();
        self.b.switch_to_block(block);
        for local in pad_chain_drops(self.func, pad)? {
            let value = self
                .func
                .params
                .iter()
                .position(|&p| p == local)
                .and_then(|n| values.get(n))
                .copied()
                .ok_or_else(|| {
                    CodegenError::ice(format!(
                        "the entry pad drops {local:?}, which is not a parameter"
                    ))
                })?;
            // `ctx` is the outer function's first parameter (`outer_entry`).
            self.drop_call(value)?;
        }
        self.unwind_resume()
    }

    /// The values of the parameters `release` names, from `params`, the
    /// function's parameters after `ctx`.
    fn param_words(release: &EntryRelease, params: &[Value]) -> Result<Vec<Value>, CodegenError> {
        release
            .params
            .iter()
            .map(|&n| {
                params.get(n).copied().ok_or_else(|| {
                    CodegenError::ice("the entry pad drops a parameter the function lacks")
                })
            })
            .collect()
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
    pub(crate) fn bind(&mut self, local: Local, value: Value) -> Result<(), CodegenError> {
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

    pub(crate) fn block(&self, block: BlockId) -> Result<ir::Block, CodegenError> {
        usize::try_from(block.as_u32())
            .ok()
            .and_then(|n| self.blocks.get(n))
            .copied()
            .ok_or_else(|| CodegenError::ice(format!("unknown MIR block {block:?}")))
    }

    /// The cold block that panics with `kind` at `loc` and unwinds to the
    /// pad of the statement being translated, if it has one.
    pub(crate) fn panic_block(&mut self, kind: PanicKind, loc: StrId) -> ir::Block {
        let key = (kind, loc, self.pad);
        if let Some(&block) = self.panic_blocks.get(&key) {
            return block;
        }
        let block = self.b.create_block();
        self.b.set_cold_block(block);
        self.panics.push((kind, loc, self.pad, block));
        self.panic_blocks.insert(key, block);
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
                if let Rvalue::DropReuse { .. } = value {
                    self.tokens.insert(*dst);
                }
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
                if self.in_pad {
                    // A pad is cold: its drops call the runtime out of
                    // line, which keeps it small; `cheby_rt_drop` skips a
                    // reuse token of 0.
                    self.drop_call(v)?;
                } else if self.tokens.contains(local) {
                    self.drop_token(v)?;
                } else {
                    self.drop_value(v)?;
                }
            }
            // Never in a pad (D-735, D-850).
            Stmt::DropKeep {
                local,
                layout,
                keep,
            } => {
                if self.in_pad {
                    return Err(CodegenError::ice("a specialized drop in a cleanup pad"));
                }
                self.drop_keep(*local, *layout, keep)?;
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
            Rvalue::CallForeign {
                symbol,
                sig,
                args,
                blocking: true,
            } => {
                let vals = self.operands(args)?;
                let callee = CCallee::Blocking {
                    symbol: symbol.clone(),
                    sig: sig.clone(),
                };
                // The edge tests the flag after the call, never the result,
                // which may be 0 (D-734, `unwind/flag.rs`).
                if let Some(pad) = self.pad {
                    return self.unwind_c_call(&callee, &vals, pad);
                }
                return self.emit_c_call(&callee, &vals);
            }
            Rvalue::CallForeign {
                symbol,
                sig,
                args,
                blocking: false,
            } => {
                let vals = self.operands(args)?;
                if let Some(pad) = self.pad {
                    let callee = CCallee::Foreign {
                        symbol: symbol.clone(),
                        sig: sig.clone(),
                    };
                    return self.unwind_c_call(&callee, &vals, pad);
                }
                return self.foreign_call(symbol, sig, &vals);
            }
            Rvalue::CallRuntime { func, args } => return self.call_helper(*func, args),
            Rvalue::ReadLayout { base } => {
                let base = self.local(*base)?;
                let id = self.layout_id(base);
                self.b.ins().ireduce(types::I32, id)
            }
            Rvalue::Alloc { layout, fields } => self.alloc(*layout, fields)?,
            Rvalue::AllocPlaceholder { layout } => self.alloc_placeholder(*layout)?,
            Rvalue::DropReuse {
                local,
                layout,
                keep,
            } => self.drop_reuse(*local, *layout, keep)?,
            Rvalue::AllocReuse {
                token,
                layout,
                fields,
            } => self.alloc_reuse(*token, *layout, fields)?,
            Rvalue::InitField {
                obj,
                layout,
                field,
                value,
            } => {
                self.init_field(*obj, *layout, *field, value)?;
                return Ok(None);
            }
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
            // kernel (D-745).
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

    pub(crate) fn func_ref(
        &mut self,
        id: FuncId,
        signature: &Signature,
        colocated: bool,
    ) -> FuncRef {
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

    pub(crate) fn sig_ref(&mut self, abi_sig: &AbiSig) -> SigRef {
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

    /// Calls a runtime function that may unwind: with the flag's test
    /// when the statement being translated has an unwind edge (D-698,
    /// D-733), as [`Self::runtime_call`] otherwise.
    pub(crate) fn edge_runtime_call(
        &mut self,
        f: RuntimeFunc,
        args: &[Value],
    ) -> Result<Option<Value>, CodegenError> {
        match self.pad {
            Some(pad) => self.unwind_c_call(&CCallee::Runtime(f), args, pad),
            None => self.runtime_call(f, args),
        }
    }

    /// [`Self::edge_runtime_call`] of a runtime function that has a result.
    pub(crate) fn edge_runtime_value(
        &mut self,
        f: RuntimeFunc,
        args: &[Value],
    ) -> Result<Value, CodegenError> {
        self.edge_runtime_call(f, args)?
            .ok_or_else(|| CodegenError::ice(format!("the runtime function {f:?} returns nothing")))
    }

    /// Calls the foreign function `symbol` with the C signature `sig` and
    /// returns its result, if it has one.
    pub(crate) fn foreign_call(
        &mut self,
        symbol: &str,
        sig: &ForeignSig,
        vals: &[Value],
    ) -> Result<Option<Value>, CodegenError> {
        let inst = match self.decls.foreign(symbol)?.clone() {
            ForeignDecl::Function(id, signature) => {
                let f = self.func_ref(id, &signature, false);
                self.b.ins().call(f, vals)
            }
            // Signature-free imports: the call site's own C signature
            // (`ForeignImport::Address`).
            ForeignDecl::Data(id) => {
                let address = self.data_address(id, false);
                let s = self
                    .b
                    .import_signature(sig::foreign_signature(sig, self.decls.c_call_conv()));
                self.b.ins().call_indirect(s, address, vals)
            }
            ForeignDecl::FunctionAddress(id, declared) => {
                let f = self.func_ref(id, &declared, false);
                let address = self.b.ins().func_addr(WORD, f);
                let s = self
                    .b
                    .import_signature(sig::foreign_signature(sig, self.decls.c_call_conv()));
                self.b.ins().call_indirect(s, address, vals)
            }
        };
        Ok(self.b.inst_results(inst).first().copied())
    }

    /// A plain call of the C function `callee`, its result if it has one.
    pub(crate) fn emit_c_call(
        &mut self,
        callee: &CCallee,
        args: &[Value],
    ) -> Result<Option<Value>, CodegenError> {
        match callee {
            CCallee::Runtime(f) => self.runtime_call(*f, args),
            CCallee::Foreign { symbol, sig } => self.foreign_call(symbol, sig, args),
            CCallee::Blocking { symbol, sig } => self.blocking_call(symbol, sig, args),
        }
    }

    /// A plain, non-tail call of Cheby code.
    pub(crate) fn emit_cheby_call(&mut self, target: CallTarget, args: &[Value]) -> ir::Inst {
        match target {
            CallTarget::Direct(f) => self.b.ins().call(f, args),
            CallTarget::Indirect(s, code) => self.b.ins().call_indirect(s, code, args),
        }
    }

    /// A new block that continues the current one after a call: cold when
    /// the current block is, so that a call in a cold path keeps its
    /// continuation out of the hot code.
    pub(crate) fn continuation_block(&mut self) -> ir::Block {
        let block = self.b.create_block();
        if let Some(current) = self.b.current_block()
            && self.b.func.layout.is_cold(current)
        {
            self.b.set_cold_block(block);
        }
        block
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
        // The callee's MIR result, which decides how an unwind edge tests
        // the call (D-734).
        let ret;
        let target = match callee {
            // A borrowing call differs only in which `dup` and `drop` run
            // around it (D-715): the slot is keyed by the declaration's
            // ABI signature, its mask included (D-456).
            Callee::Direct(def) | Callee::Borrowing(def, _) => {
                let decl = self.unit.callee(*def).ok_or_else(|| {
                    CodegenError::ice(format!("callee {def:?} has no declaration in its unit"))
                })?;
                if decl.sig.borrow != callee.borrow_mask() {
                    return Err(CodegenError::ice(format!(
                        "a call of {def:?} with another borrow mask than its declaration"
                    )));
                }
                let abi_sig = decl.sig.clone();
                ret = abi_sig.ret;
                match self.opts.call_mode {
                    CallMode::Indirect => {
                        // D-043: every call of a top-level function loads
                        // its slot, which is writable (no `readonly`).
                        let path = self.decls.slot_path(self.unit, *def)?;
                        let slot = self.decls.slot(&path, &abi_sig)?.id;
                        let address = self.data_address(slot, true);
                        let code = self.b.ins().load(WORD, MemFlagsData::trusted(), address, 0);
                        CallTarget::Indirect(self.sig_ref(&abi_sig), code)
                    }
                    CallMode::Direct => {
                        let decl = self.decls.func(FnKey::Item(*def))?;
                        let (id, signature) = (decl.id, decl.sig.clone());
                        CallTarget::Direct(self.func_ref(id, &signature, true))
                    }
                }
            }
            Callee::Code(key) => {
                let decl = self.decls.func(*key)?;
                let (id, signature) = (decl.id, decl.sig.clone());
                ret = decl.abi.ret;
                CallTarget::Direct(self.func_ref(id, &signature, true))
            }
            Callee::Indirect(closure) => {
                ret = Repr::Value;
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
                CallTarget::Indirect(self.sig_ref(&uniform_sig(arity)), code)
            }
        };
        vals.extend(self.operands(args)?);
        if let (Some(pad), false) = (self.pad, tail) {
            return self.unwind_cheby_call(target, ret, &vals, pad).map(Some);
        }
        let inst = match (target, tail) {
            (CallTarget::Direct(f), false) => self.b.ins().call(f, &vals),
            (CallTarget::Direct(f), true) => self.b.ins().return_call(f, &vals),
            (CallTarget::Indirect(s, code), false) => self.b.ins().call_indirect(s, code, &vals),
            (CallTarget::Indirect(s, code), true) => {
                self.b.ins().return_call_indirect(s, code, &vals)
            }
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

    /// Calls `cheby_rt_panic(ctx, kind, message or 0, loc)`. With `pad`,
    /// the unwinding continues there (D-698, D-733). Without one (code
    /// compiled without unwind edges, D-742), the runtime does not return
    /// into the code (D-770), and a trap follows the call.
    fn panic(
        &mut self,
        kind: PanicKind,
        message: Option<&Operand>,
        loc: StrId,
        pad: Option<ir::Block>,
    ) -> Result<(), CodegenError> {
        let ctx = self.ctx()?;
        let kind = self.b.ins().iconst(types::I32, i64::from(kind.code()));
        let message = match message {
            Some(m) => self.operand(m)?,
            None => self.b.ins().iconst(WORD, 0),
        };
        let loc = self.string(loc)?;
        let args = [ctx, kind, message, loc];
        if let Some(pad) = pad {
            return self.unwind_panic(&args, pad);
        }
        self.runtime_call(RuntimeFunc::Panic, &args)?;
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
                self.panic(*kind, message.as_ref(), *loc, self.pad)?;
            }
            Terminator::Unreachable => {
                self.b.ins().trap(trap(TRAP_UNREACHABLE)?);
            }
            Terminator::Resume => self.unwind_resume()?,
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
