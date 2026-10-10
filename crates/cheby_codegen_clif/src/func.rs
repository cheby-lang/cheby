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
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Switch, Variable};
use cranelift_module::{DataId, FuncId};
use rustc_hash::{FxHashMap, FxHashSet};

use cheby_mir::{
    Abi, AbiSig, BlockId, Callee, Const, FnKey, Function, Local, MirModule, Operand, PanicKind,
    Repr, Rvalue, Stmt, StrId, Terminator, uniform_sig,
};
use cheby_runtime::abi;

use crate::decls::{Decls, ForeignDecl};
use crate::mangle::{self, Kind};
use crate::runtime::RuntimeFunc;
use crate::sig::{self, WORD, repr_type};
use crate::{CallMode, CodegenCx, CodegenError, CodegenOptions};

/// The trap after a call that does not return (`cheby_rt_panic`).
const TRAP_NORETURN: u8 = 1;
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
    let decl = decls.func(func.key)?;
    let mut clif = ir::Function::with_name_signature(
        ir::UserFuncName::user(0, decl.id.as_u32()),
        decl.sig.clone(),
    );
    let mut fctx = FunctionBuilderContext::new();
    let builder = FunctionBuilder::new(&mut clif, &mut fctx);
    let mut t = Translator {
        b: builder,
        unit,
        func,
        decls,
        opts,
        vars: Vec::new(),
        blocks: Vec::new(),
        ctx: None,
        func_refs: FxHashMap::default(),
        data: FxHashMap::default(),
        sigs: FxHashMap::default(),
        panics: Vec::new(),
    };
    t.run()?;
    t.b.seal_all_blocks();
    t.b.finalize(decls.frontend_config());
    Ok(clif)
}

pub(crate) struct Translator<'a, 'f> {
    pub(crate) b: FunctionBuilder<'f>,
    pub(crate) unit: &'a MirModule,
    pub(crate) func: &'a Function,
    pub(crate) decls: &'a Decls,
    pub(crate) opts: &'a CodegenOptions,
    vars: Vec<Variable>,
    blocks: Vec<ir::Block>,
    ctx: Option<Value>,
    func_refs: FxHashMap<FuncId, FuncRef>,
    data: FxHashMap<DataId, GlobalValue>,
    sigs: FxHashMap<AbiSig, SigRef>,
    /// The cold blocks that panic with a kind at a location, created by
    /// checked operations and filled after the function's blocks, one per
    /// kind and location.
    panics: Vec<(PanicKind, StrId, ir::Block)>,
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
    fn run(&mut self) -> Result<(), CodegenError> {
        for decl in &self.func.locals {
            let var = self.b.declare_var(repr_type(decl.repr));
            self.vars.push(var);
        }
        for _ in &self.func.blocks {
            let block = self.b.create_block();
            self.blocks.push(block);
        }
        self.entry()?;
        for (id, block) in self.func.blocks.iter_enumerated() {
            let cl = self.block(id)?;
            self.b.switch_to_block(cl);
            for &param in &block.params {
                let ty = repr_type(self.func.local(param)?.repr);
                let value = self.b.append_block_param(cl, ty);
                let var = self.var(param)?;
                self.b.def_var(var, value);
            }
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
    /// function, then a jump to `bb0`.
    fn entry(&mut self) -> Result<(), CodegenError> {
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
            let var = self.var(local)?;
            self.b.def_var(var, value);
        }
        let first = self.block(BlockId::from_u32(0))?;
        if self.func.abi == Abi::Cheby && self.opts.stack_check {
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

    pub(crate) fn ctx(&self) -> Result<Value, CodegenError> {
        self.ctx
            .ok_or_else(|| CodegenError::ice("`ctx` used before the entry block"))
    }

    pub(crate) fn var(&self, local: Local) -> Result<Variable, CodegenError> {
        usize::try_from(local.as_u32())
            .ok()
            .and_then(|n| self.vars.get(n))
            .copied()
            .ok_or_else(|| CodegenError::ice(format!("unknown MIR local {local:?}")))
    }

    fn block(&self, block: BlockId) -> Result<ir::Block, CodegenError> {
        usize::try_from(block.as_u32())
            .ok()
            .and_then(|n| self.blocks.get(n))
            .copied()
            .ok_or_else(|| CodegenError::ice(format!("unknown MIR block {block:?}")))
    }

    /// The cold block that panics with `kind` at `loc`.
    pub(crate) fn panic_block(&mut self, kind: PanicKind, loc: StrId) -> ir::Block {
        if let Some(&(_, _, block)) = self.panics.iter().find(|&&(k, l, _)| k == kind && l == loc) {
            return block;
        }
        let block = self.b.create_block();
        self.b.set_cold_block(block);
        self.panics.push((kind, loc, block));
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
                let var = self.var(*dst)?;
                self.b.def_var(var, v);
            }
            Stmt::Eval(value) => {
                if self.rvalue(value)?.is_some() {
                    return Err(CodegenError::ice("`eval` of a call with a result"));
                }
            }
            Stmt::Dup(local) => {
                let var = self.var(*local)?;
                let v = self.b.use_var(var);
                self.dup(v)?;
            }
            Stmt::Drop(local) => {
                let var = self.var(*local)?;
                let v = self.b.use_var(var);
                self.drop_value(v)?;
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
                let var = self.var(*base)?;
                let base = self.b.use_var(var);
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
            Operand::Local(local) => {
                let var = self.var(*local)?;
                Ok(self.b.use_var(var))
            }
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
                let symbol = mangle::item(Kind::FnVal, self.unit.name(*def)?);
                let data = self.decls.static_object(&symbol)?;
                self.data_address(data, true)
            }
            Const::Static(id) => {
                let object = self
                    .unit
                    .static_object(*id)
                    .ok_or_else(|| CodegenError::ice(format!("unknown static {id:?}")))?;
                let data = self.decls.static_object(&Decls::static_symbol(object))?;
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
                        let path = self.unit.name(*def)?.path();
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
                let var = self.var(*closure)?;
                let closure = self.b.use_var(var);
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
