//! The std kernels that are instructions (`cheby_mir::kernels_instr`):
//! each one's instruction, as code generation emits it, against the Rust
//! kernel it replaces, bit for bit, at both optimization levels. The
//! inputs are the edges of each type (the extremes, zero and `-0.0`, the
//! infinities, NaNs with payloads, values where a conversion rounds) and
//! a few thousand pseudo-random bit patterns from a fixed seed. Both sides
//! take and return every number as its bits in a `u64`, so one harness
//! serves every signature; the kernel is called through its C signature,
//! as a foreign call calls it.

#![expect(
    clippy::as_conversions,
    reason = "a JIT function's address is an integer constant of the code"
)]
#![expect(
    clippy::wildcard_enum_match_arm,
    reason = "a test matches only the variants it checks"
)]
#![expect(
    clippy::arithmetic_side_effects,
    reason = "test arithmetic on the widths of the integer types, 8 to 64"
)]

use cranelift_codegen::ir::{self, AbiParam, InstBuilder, MemFlagsData, Value, types};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::Module;

use cheby_mir::kernels_instr::{INSTR_KERNELS, rvalue};
use cheby_mir::{CType, Operand, Repr, Rvalue};

use crate::sig::{c_signature, repr_type};
use crate::{OptLevel, isa};

/// A JIT-compiled `fn(u64, u64) -> u64` over numbers as bits.
type Bits2 = extern "C" fn(u64, u64) -> u64;

/// The representation of a std module's type, by the module's name.
fn module_repr(name: &str) -> Repr {
    match name {
        "int" => Repr::Int,
        "float" => Repr::Float,
        "f32" => Repr::F32,
        "i8" => Repr::I8,
        "i16" => Repr::I16,
        "i32" => Repr::I32,
        "i64" => Repr::I64,
        "u8" => Repr::U8,
        "u16" => Repr::U16,
        "u32" => Repr::U32,
        "u64" => Repr::U64,
        other => panic!("no numeric module {other}"),
    }
}

/// The parameters and the result of the kernel `function` of `module`,
/// by the naming of std's numeric families (D-484).
fn signature_of(module: &str, function: &str) -> (Vec<Repr>, Repr) {
    let own = module_repr(module);
    if let Some(source) = function.strip_prefix("from_") {
        return (vec![module_repr(source)], own);
    }
    match function {
        "sqrt" | "wrapping_neg" => (vec![own], own),
        "wrapping_add" | "wrapping_sub" | "wrapping_mul" => (vec![own, own], own),
        other => panic!("no instruction kernel {other}"),
    }
}

/// The C type of a kernel's parameter or result of `repr` (plan §6).
fn ctype(repr: Repr) -> CType {
    match repr {
        Repr::Int | Repr::I64 => CType::I64,
        Repr::Float => CType::F64,
        Repr::F32 => CType::F32,
        Repr::I8 => CType::I8,
        Repr::I16 => CType::I16,
        Repr::I32 => CType::I32,
        Repr::U8 => CType::U8,
        Repr::U16 => CType::U16,
        Repr::U32 => CType::U32,
        Repr::U64 => CType::U64,
        other => panic!("no kernel type {other}"),
    }
}

/// The number of `repr` whose bits are the low bits of `x`.
fn decode(b: &mut FunctionBuilder<'_>, x: Value, repr: Repr) -> Value {
    let ty = repr_type(repr);
    match repr {
        Repr::Float => b.ins().bitcast(types::F64, MemFlagsData::new(), x),
        Repr::F32 => {
            let low = b.ins().ireduce(types::I32, x);
            b.ins().bitcast(types::F32, MemFlagsData::new(), low)
        }
        _ if ty == types::I64 => x,
        _ => b.ins().ireduce(ty, x),
    }
}

/// The bits of `v`, a number of `repr`, zero-extended to 64.
fn encode(b: &mut FunctionBuilder<'_>, v: Value, repr: Repr) -> Value {
    let ty = repr_type(repr);
    match repr {
        Repr::Float => b.ins().bitcast(types::I64, MemFlagsData::new(), v),
        Repr::F32 => {
            let bits = b.ins().bitcast(types::I32, MemFlagsData::new(), v);
            b.ins().uextend(types::I64, bits)
        }
        _ if ty == types::I64 => v,
        _ => b.ins().uextend(types::I64, v),
    }
}

/// What code generation emits for `value`, an instruction kernel's rvalue
/// over `args` (`func.rs`).
fn emit(b: &mut FunctionBuilder<'_>, value: &Rvalue, args: &[Value]) -> Value {
    match value {
        Rvalue::Convert { from, to, .. } => crate::arith::convert(b, args[0], *from, *to).unwrap(),
        Rvalue::FloatSqrt { .. } => b.ins().sqrt(args[0]),
        Rvalue::Wrapping { op, lhs, repr, .. } => {
            // `wrapping_neg` is `0 - n`: its first operand is the constant.
            let (a, c) = match lhs {
                Operand::Const(_) => (b.ins().iconst(repr_type(*repr), 0), args[0]),
                Operand::Local(_) => (args[0], args[1]),
            };
            crate::arith::wrapping(b, *op, a, c, *repr).unwrap()
        }
        other => panic!("no instruction kernel rvalue {other:?}"),
    }
}

/// How a test function computes the kernel's result.
#[derive(Clone, Copy)]
enum Side {
    /// The instruction code generation emits.
    Instruction,
    /// A C call of the kernel at `address`.
    Kernel(usize),
}

/// Compiles `fn(u64, u64) -> u64` that decodes its two parameters as
/// `params`, computes `value` on `side`, and encodes the `ret`.
/// Returns the function and whether its code has no relocation.
fn compile(
    m: &mut JITModule,
    value: &Rvalue,
    params: &[Repr],
    ret: Repr,
    side: Side,
) -> (Bits2, bool) {
    let cc = m.isa().default_call_conv();
    let frontend = m.isa().frontend_config();
    let mut ctx = m.make_context();
    ctx.func.signature.params = vec![AbiParam::new(types::I64); 2];
    ctx.func.signature.returns = vec![AbiParam::new(types::I64)];
    ctx.func.signature.call_conv = cc;
    let mut fctx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);
    let raw = b.block_params(entry).to_vec();
    let args: Vec<Value> = params
        .iter()
        .zip(&raw)
        .map(|(&repr, &x)| decode(&mut b, x, repr))
        .collect();
    let result = match side {
        Side::Instruction => emit(&mut b, value, &args),
        Side::Kernel(address) => {
            let ctypes: Vec<CType> = params.iter().map(|&p| ctype(p)).collect();
            let sig = b.import_signature(c_signature(&ctypes, ctype(ret), cc));
            let callee = b.ins().iconst(types::I64, i64::try_from(address).unwrap());
            let call = b.ins().call_indirect(sig, callee, &args);
            b.inst_results(call)[0]
        }
    };
    let out = encode(&mut b, result, ret);
    b.ins().return_(&[out]);
    b.finalize(frontend);
    let id = m.declare_anonymous_function(&ctx.func.signature).unwrap();
    m.define_function(id, &mut ctx).unwrap();
    let plain = ctx
        .compiled_code()
        .is_some_and(|code| code.buffer.relocs().is_empty());
    m.finalize_definitions().unwrap();
    let code = m.get_finalized_function(id);
    // SAFETY: `code` was compiled just now with the signature of `Bits2`
    // under the platform's C convention, and `m` keeps it alive.
    (
        unsafe { std::mem::transmute::<*const u8, Bits2>(code) },
        plain,
    )
}

/// Compiles `fn(out: ptr)` that stores, for each input of `inputs`, the
/// bits of `value` of that input's constants at `out`, one word each:
/// what the optimizer folds when a direct call's arguments are literals.
fn compile_folded(
    m: &mut JITModule,
    value: &Rvalue,
    params: &[Repr],
    ret: Repr,
    inputs: &[[u64; 2]],
) -> extern "C" fn(*mut u64) {
    let cc = m.isa().default_call_conv();
    let frontend = m.isa().frontend_config();
    let mut ctx = m.make_context();
    ctx.func.signature.params = vec![AbiParam::new(types::I64)];
    ctx.func.signature.call_conv = cc;
    let mut fctx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);
    let out = b.block_params(entry)[0];
    for (n, input) in inputs.iter().enumerate() {
        let args: Vec<Value> = params
            .iter()
            .zip(input)
            .map(|(&repr, &bits)| constant(&mut b, repr, bits))
            .collect();
        let result = emit(&mut b, value, &args);
        let bits = encode(&mut b, result, ret);
        let offset = i32::try_from(n.checked_mul(8).unwrap()).unwrap();
        b.ins().store(MemFlagsData::trusted(), bits, out, offset);
    }
    b.ins().return_(&[]);
    b.finalize(frontend);
    let id = m.declare_anonymous_function(&ctx.func.signature).unwrap();
    m.define_function(id, &mut ctx).unwrap();
    m.finalize_definitions().unwrap();
    let code = m.get_finalized_function(id);
    // SAFETY: as in `compile`, with the signature `fn(*mut u64)`.
    unsafe { std::mem::transmute::<*const u8, extern "C" fn(*mut u64)>(code) }
}

/// A constant of `repr` whose bits are the low bits of `bits`.
fn constant(b: &mut FunctionBuilder<'_>, repr: Repr, bits: u64) -> Value {
    match repr {
        Repr::Float => b.ins().f64const(ir::immediates::Ieee64::with_bits(bits)),
        Repr::F32 => {
            let low = u32::try_from(bits & u64::from(u32::MAX)).unwrap();
            b.ins().f32const(ir::immediates::Ieee32::with_bits(low))
        }
        _ => {
            let ty = repr_type(repr);
            let width = ty.bits();
            let masked = if width == 64 {
                bits
            } else {
                bits & ((1_u64 << width) - 1)
            };
            b.ins().iconst(ty, masked.cast_signed())
        }
    }
}

/// `SplitMix64`, a fixed sequence of bit patterns.
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// The edges of `repr`, as bits: zero, one, the extremes, the values next
/// to them, and the integers where a conversion into `F32` or `Float`
/// rounds (2^24 and 2^53 plus 1 and 3, a tie each, and the odd values
/// below 2^63 and 2^64 whose low bit decides the tie); for floats the
/// signed zeros and infinities, NaNs with payloads, quiet and signaling,
/// of both signs, the subnormals and the edges of `F32`'s range.
#[expect(clippy::too_many_lines, reason = "a table of values")]
fn edges(repr: Repr) -> Vec<u64> {
    match repr {
        Repr::Float => {
            let mut v: Vec<u64> = [
                0.0,
                -0.0,
                1.0,
                -1.0,
                0.5,
                2.0,
                4.0,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::MAX,
                f64::MIN,
                f64::MIN_POSITIVE,
                f64::EPSILON,
                f64::from(f32::MAX),
                f64::from(f32::MIN_POSITIVE),
                f64::from(f32::MIN_POSITIVE) / 2.0,
                3.402_823_567_797_336_6e38,
                3.402_823_567_797_336_7e38,
                1.401_298_464_324_817e-45,
                7.006_492_321_624_085e-46,
                1.0e-50,
                16_777_217.0,
                0.1,
                -2.5,
            ]
            .map(f64::to_bits)
            .to_vec();
            v.extend([
                0x7ff8_0000_0000_0000,
                0xfff8_0000_0000_0000,
                0x7ff8_0000_0000_0001,
                0x7ff0_0000_0000_0001,
                0xfff0_0000_dead_beef,
                0x7ff4_0000_0000_0000,
                0x7fff_ffff_ffff_ffff,
                0x0000_0000_0000_0001,
                0x000f_ffff_ffff_ffff,
                0x8000_0000_0000_0001,
                0x3ff0_0000_1000_0000,
                0x3ff0_0000_3000_0000,
            ]);
            v
        }
        Repr::F32 => {
            let mut v: Vec<u64> = [
                0.0,
                -0.0,
                1.0,
                -1.0,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::MAX,
                f32::MIN,
                f32::MIN_POSITIVE,
                f32::EPSILON,
                0.1,
            ]
            .map(|x| u64::from(x.to_bits()))
            .to_vec();
            v.extend([
                0x7fc0_0000,
                0xffc0_0000,
                0x7fc0_0001,
                0x7f80_0001,
                0xff80_beef,
                0x7fa0_0000,
                0x0000_0001,
                0x007f_ffff,
                0x8000_0001,
            ]);
            v
        }
        _ => {
            let width = repr_type(repr).bits();
            let signed = repr.is_signed_int();
            let mask = if width == 64 {
                u64::MAX
            } else {
                (1_u64 << width) - 1
            };
            let (min, max) = if signed {
                (1_u64 << (width - 1), (1_u64 << (width - 1)) - 1)
            } else {
                (0, mask)
            };
            let mut v = vec![
                0,
                1,
                2,
                3,
                mask,
                mask - 1,
                min,
                min.wrapping_add(1),
                max,
                max - 1,
                0x55,
                0xaa,
            ];
            for p in [24_u32, 25, 31, 32, 53, 54, 62, 63] {
                let base = 1_u64.checked_shl(p).unwrap_or(0);
                for d in [0_u64, 1, 2, 3, 5] {
                    v.push(base.wrapping_add(d));
                    v.push(base.wrapping_sub(d));
                    v.push(base.wrapping_add(d).wrapping_neg());
                }
            }
            v.extend([
                0x8000_0000_0000_0401,
                0x8000_0000_0000_0400,
                0x8000_0000_0000_0c00,
                0xffff_ffff_ffff_f400,
                0xffff_ffff_ffff_fc00,
                0x0020_0000_0000_0001,
                0x0020_0000_0000_0003,
                0x0000_0000_0100_0001,
                0x0000_0000_0100_0003,
                0x7fff_ffff_ffff_fdff,
                0x7fff_ffff_ffff_fe00,
            ]);
            for x in &mut v {
                *x &= mask;
            }
            v.sort_unstable();
            v.dedup();
            v
        }
    }
}

/// The inputs of an operation on `params`: every edge, every pair of
/// edges for two parameters, then `random` pseudo-random bit patterns.
fn inputs(params: &[Repr], random: usize, rng: &mut SplitMix) -> Vec<[u64; 2]> {
    let mut out = Vec::new();
    match params {
        [a] => out.extend(edges(*a).into_iter().map(|x| [x, 0])),
        [a, b] => {
            for x in edges(*a) {
                out.extend(edges(*b).into_iter().map(|y| [x, y]));
            }
        }
        other => panic!("no instruction kernel of {} parameters", other.len()),
    }
    out.extend((0..random).map(|_| [rng.next(), rng.next()]));
    out
}

/// The bits of `x` that a value of `repr` holds.
fn masked(x: u64, repr: Repr) -> u64 {
    match repr_type(repr).bits() {
        64 => x,
        width => x & ((1_u64 << width) - 1),
    }
}

/// Every instruction kernel gives its kernel's bits on every input, in
/// debug and in release code, and its instruction is no call and no
/// libcall (its code has no relocation). With literal arguments, where
/// the optimizer folds the instruction, the bits are the same.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn every_instruction_kernel_computes_its_kernels_bits() {
    let symbols = cheby_runtime::symbols();
    let mut rng = SplitMix(0x5eed_cafe_f00d_1234);
    for opt in [OptLevel::None, OptLevel::Speed] {
        let mut m = JITModule::new(JITBuilder::with_isa(
            isa::host_isa_with(opt).unwrap(),
            isa::libcall_names(),
        ));
        for &(module, function, symbol, op) in INSTR_KERNELS {
            let (params, ret) = signature_of(module, function);
            let locals = params
                .iter()
                .enumerate()
                .map(|(n, _)| Operand::Local(cheby_mir::Local::from_u32(u32::try_from(n).unwrap())))
                .collect();
            let value = rvalue(op, &params, ret, locals).unwrap();
            let address = symbols
                .iter()
                .find(|&&(name, _)| name == symbol)
                .map_or_else(
                    || panic!("the runtime has no {symbol}"),
                    |&(_, a)| a as usize,
                );
            let (instruction, plain) = compile(&mut m, &value, &params, ret, Side::Instruction);
            assert!(plain, "{symbol}: the instruction's code has a relocation");
            let (kernel, _) = compile(&mut m, &value, &params, ret, Side::Kernel(address));
            let cases = inputs(&params, 4000, &mut rng);
            for &[a, b] in &cases {
                let want = kernel(a, b);
                let got = instruction(a, b);
                assert_eq!(
                    got,
                    want,
                    "{symbol} at {opt:?} of {:#x}, {:#x}: {got:#x} for the kernel's {want:#x}",
                    masked(a, params[0]),
                    params.get(1).map_or(0, |&r| masked(b, r)),
                );
            }
            // Literal arguments: the edges, folded where the optimizer folds.
            let literal: Vec<[u64; 2]> = cases.iter().take(256).copied().collect();
            let folded = compile_folded(&mut m, &value, &params, ret, &literal);
            let mut out = vec![0_u64; literal.len()];
            folded(out.as_mut_ptr());
            for (&[a, b], &got) in literal.iter().zip(&out) {
                let want = kernel(a, b);
                assert_eq!(
                    got,
                    want,
                    "{symbol} at {opt:?} folded, of {:#x}, {:#x}: {got:#x} for {want:#x}",
                    masked(a, params[0]),
                    params.get(1).map_or(0, |&r| masked(b, r)),
                );
            }
        }
    }
}

/// The harness covers every entry of the table, so no instruction kernel
/// goes untested.
#[test]
fn every_entry_has_a_signature() {
    for &(module, function, symbol, op) in INSTR_KERNELS {
        let (params, ret) = signature_of(module, function);
        let args = params
            .iter()
            .map(|_| Operand::Const(cheby_mir::Const::Int(0)))
            .collect();
        assert!(rvalue(op, &params, ret, args).is_ok(), "{symbol}");
    }
}
