//! The std kernels that are machine instructions: a direct call of one
//! (`lower/external.rs`, D-1765) is the instruction itself, an MIR
//! operation named for what it computes, with no foreign call.
//!
//! A foreign call clobbers every caller-saved register, so a loop that
//! calls `float::sqrt` or `float::from_int` spills and reloads its floats
//! around each call. The kernels listed here compute what one Cranelift
//! instruction computes, bit for bit, on every target of D-921:
//!
//! - the conversions that cannot fail ([`Rvalue::Convert`]): `float`'s and
//!   `f32`'s `from_<type>`, `int`'s `from_` of the narrower types and the
//!   widening `from_` of the sized integer modules. Rust's `as` and `From`
//!   in the kernels are `sextend`, `uextend`, `fcvt_from_sint`,
//!   `fcvt_from_uint`, `fpromote` and `fdemote`, which round to nearest,
//!   ties to even (D-359), and keep a NaN's payload as the hardware does;
//! - `float::sqrt` ([`Rvalue::FloatSqrt`]), IEEE 754's square root, which
//!   is `f64::sqrt` in the kernel;
//! - the `wrapping_add`, `wrapping_sub`, `wrapping_mul` and `wrapping_neg`
//!   of `int` and the sized integer modules ([`Rvalue::Wrapping`]); a
//!   negation is `0 - n`, as `wrapping_neg` is.
//!
//! The wrapper and its kernel stay, for function values and dictionaries.
//! Every other kernel keeps its foreign call: one that returns a `Result`
//! or a `String`, and `float`'s `pow`, `exp`, `ln`, `sin`, `cos`, `tan`
//! and `atan2`, which are correctly rounded in software (D-396). A test in
//! `cheby_codegen_clif` runs every instruction against its kernel.

use cheby_iface::Ice;

use crate::ir::{ArithOp, Const, Operand, Rvalue};
use crate::repr::Repr;

/// The MIR operation of an instruction kernel, for its parameters'
/// representations and its result's ([`rvalue`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum KernelOp {
    /// [`Rvalue::Convert`] from the parameter's representation into the
    /// result's.
    Convert,
    /// [`Rvalue::FloatSqrt`] of the parameter.
    Sqrt,
    /// [`Rvalue::Wrapping`] of the two parameters.
    Wrapping(ArithOp),
    /// [`Rvalue::Wrapping`] `Sub` of zero and the parameter.
    WrappingNeg,
}

/// `(std module, function, native symbol, operation)`, sorted by module
/// and function.
pub const INSTR_KERNELS: &[(&str, &str, &str, KernelOp)] = &[
    (
        "f32",
        "from_float",
        "cheby_f32_from_float",
        KernelOp::Convert,
    ),
    ("f32", "from_i16", "cheby_f32_from_i16", KernelOp::Convert),
    ("f32", "from_i32", "cheby_f32_from_i32", KernelOp::Convert),
    ("f32", "from_i64", "cheby_f32_from_i64", KernelOp::Convert),
    ("f32", "from_i8", "cheby_f32_from_i8", KernelOp::Convert),
    ("f32", "from_int", "cheby_f32_from_int", KernelOp::Convert),
    ("f32", "from_u16", "cheby_f32_from_u16", KernelOp::Convert),
    ("f32", "from_u32", "cheby_f32_from_u32", KernelOp::Convert),
    ("f32", "from_u64", "cheby_f32_from_u64", KernelOp::Convert),
    ("f32", "from_u8", "cheby_f32_from_u8", KernelOp::Convert),
    (
        "float",
        "from_f32",
        "cheby_float_from_f32",
        KernelOp::Convert,
    ),
    (
        "float",
        "from_i16",
        "cheby_float_from_i16",
        KernelOp::Convert,
    ),
    (
        "float",
        "from_i32",
        "cheby_float_from_i32",
        KernelOp::Convert,
    ),
    (
        "float",
        "from_i64",
        "cheby_float_from_i64",
        KernelOp::Convert,
    ),
    ("float", "from_i8", "cheby_float_from_i8", KernelOp::Convert),
    (
        "float",
        "from_int",
        "cheby_float_from_int",
        KernelOp::Convert,
    ),
    (
        "float",
        "from_u16",
        "cheby_float_from_u16",
        KernelOp::Convert,
    ),
    (
        "float",
        "from_u32",
        "cheby_float_from_u32",
        KernelOp::Convert,
    ),
    (
        "float",
        "from_u64",
        "cheby_float_from_u64",
        KernelOp::Convert,
    ),
    ("float", "from_u8", "cheby_float_from_u8", KernelOp::Convert),
    ("float", "sqrt", "cheby_float_sqrt", KernelOp::Sqrt),
    ("i16", "from_i8", "cheby_i16_from_i8", KernelOp::Convert),
    ("i16", "from_u8", "cheby_i16_from_u8", KernelOp::Convert),
    (
        "i16",
        "wrapping_add",
        "cheby_i16_wrapping_add",
        KernelOp::Wrapping(ArithOp::Add),
    ),
    (
        "i16",
        "wrapping_mul",
        "cheby_i16_wrapping_mul",
        KernelOp::Wrapping(ArithOp::Mul),
    ),
    (
        "i16",
        "wrapping_neg",
        "cheby_i16_wrapping_neg",
        KernelOp::WrappingNeg,
    ),
    (
        "i16",
        "wrapping_sub",
        "cheby_i16_wrapping_sub",
        KernelOp::Wrapping(ArithOp::Sub),
    ),
    ("i32", "from_i16", "cheby_i32_from_i16", KernelOp::Convert),
    ("i32", "from_i8", "cheby_i32_from_i8", KernelOp::Convert),
    ("i32", "from_u16", "cheby_i32_from_u16", KernelOp::Convert),
    ("i32", "from_u8", "cheby_i32_from_u8", KernelOp::Convert),
    (
        "i32",
        "wrapping_add",
        "cheby_i32_wrapping_add",
        KernelOp::Wrapping(ArithOp::Add),
    ),
    (
        "i32",
        "wrapping_mul",
        "cheby_i32_wrapping_mul",
        KernelOp::Wrapping(ArithOp::Mul),
    ),
    (
        "i32",
        "wrapping_neg",
        "cheby_i32_wrapping_neg",
        KernelOp::WrappingNeg,
    ),
    (
        "i32",
        "wrapping_sub",
        "cheby_i32_wrapping_sub",
        KernelOp::Wrapping(ArithOp::Sub),
    ),
    ("i64", "from_i16", "cheby_i64_from_i16", KernelOp::Convert),
    ("i64", "from_i32", "cheby_i64_from_i32", KernelOp::Convert),
    ("i64", "from_i8", "cheby_i64_from_i8", KernelOp::Convert),
    ("i64", "from_int", "cheby_i64_from_int", KernelOp::Convert),
    ("i64", "from_u16", "cheby_i64_from_u16", KernelOp::Convert),
    ("i64", "from_u32", "cheby_i64_from_u32", KernelOp::Convert),
    ("i64", "from_u8", "cheby_i64_from_u8", KernelOp::Convert),
    (
        "i64",
        "wrapping_add",
        "cheby_i64_wrapping_add",
        KernelOp::Wrapping(ArithOp::Add),
    ),
    (
        "i64",
        "wrapping_mul",
        "cheby_i64_wrapping_mul",
        KernelOp::Wrapping(ArithOp::Mul),
    ),
    (
        "i64",
        "wrapping_neg",
        "cheby_i64_wrapping_neg",
        KernelOp::WrappingNeg,
    ),
    (
        "i64",
        "wrapping_sub",
        "cheby_i64_wrapping_sub",
        KernelOp::Wrapping(ArithOp::Sub),
    ),
    (
        "i8",
        "wrapping_add",
        "cheby_i8_wrapping_add",
        KernelOp::Wrapping(ArithOp::Add),
    ),
    (
        "i8",
        "wrapping_mul",
        "cheby_i8_wrapping_mul",
        KernelOp::Wrapping(ArithOp::Mul),
    ),
    (
        "i8",
        "wrapping_neg",
        "cheby_i8_wrapping_neg",
        KernelOp::WrappingNeg,
    ),
    (
        "i8",
        "wrapping_sub",
        "cheby_i8_wrapping_sub",
        KernelOp::Wrapping(ArithOp::Sub),
    ),
    ("int", "from_i16", "cheby_int_from_i16", KernelOp::Convert),
    ("int", "from_i32", "cheby_int_from_i32", KernelOp::Convert),
    ("int", "from_i8", "cheby_int_from_i8", KernelOp::Convert),
    ("int", "from_u16", "cheby_int_from_u16", KernelOp::Convert),
    ("int", "from_u32", "cheby_int_from_u32", KernelOp::Convert),
    ("int", "from_u8", "cheby_int_from_u8", KernelOp::Convert),
    (
        "int",
        "wrapping_add",
        "cheby_int_wrapping_add",
        KernelOp::Wrapping(ArithOp::Add),
    ),
    (
        "int",
        "wrapping_mul",
        "cheby_int_wrapping_mul",
        KernelOp::Wrapping(ArithOp::Mul),
    ),
    (
        "int",
        "wrapping_neg",
        "cheby_int_wrapping_neg",
        KernelOp::WrappingNeg,
    ),
    (
        "int",
        "wrapping_sub",
        "cheby_int_wrapping_sub",
        KernelOp::Wrapping(ArithOp::Sub),
    ),
    ("u16", "from_u8", "cheby_u16_from_u8", KernelOp::Convert),
    (
        "u16",
        "wrapping_add",
        "cheby_u16_wrapping_add",
        KernelOp::Wrapping(ArithOp::Add),
    ),
    (
        "u16",
        "wrapping_mul",
        "cheby_u16_wrapping_mul",
        KernelOp::Wrapping(ArithOp::Mul),
    ),
    (
        "u16",
        "wrapping_neg",
        "cheby_u16_wrapping_neg",
        KernelOp::WrappingNeg,
    ),
    (
        "u16",
        "wrapping_sub",
        "cheby_u16_wrapping_sub",
        KernelOp::Wrapping(ArithOp::Sub),
    ),
    ("u32", "from_u16", "cheby_u32_from_u16", KernelOp::Convert),
    ("u32", "from_u8", "cheby_u32_from_u8", KernelOp::Convert),
    (
        "u32",
        "wrapping_add",
        "cheby_u32_wrapping_add",
        KernelOp::Wrapping(ArithOp::Add),
    ),
    (
        "u32",
        "wrapping_mul",
        "cheby_u32_wrapping_mul",
        KernelOp::Wrapping(ArithOp::Mul),
    ),
    (
        "u32",
        "wrapping_neg",
        "cheby_u32_wrapping_neg",
        KernelOp::WrappingNeg,
    ),
    (
        "u32",
        "wrapping_sub",
        "cheby_u32_wrapping_sub",
        KernelOp::Wrapping(ArithOp::Sub),
    ),
    ("u64", "from_u16", "cheby_u64_from_u16", KernelOp::Convert),
    ("u64", "from_u32", "cheby_u64_from_u32", KernelOp::Convert),
    ("u64", "from_u8", "cheby_u64_from_u8", KernelOp::Convert),
    (
        "u64",
        "wrapping_add",
        "cheby_u64_wrapping_add",
        KernelOp::Wrapping(ArithOp::Add),
    ),
    (
        "u64",
        "wrapping_mul",
        "cheby_u64_wrapping_mul",
        KernelOp::Wrapping(ArithOp::Mul),
    ),
    (
        "u64",
        "wrapping_neg",
        "cheby_u64_wrapping_neg",
        KernelOp::WrappingNeg,
    ),
    (
        "u64",
        "wrapping_sub",
        "cheby_u64_wrapping_sub",
        KernelOp::Wrapping(ArithOp::Sub),
    ),
    (
        "u8",
        "wrapping_add",
        "cheby_u8_wrapping_add",
        KernelOp::Wrapping(ArithOp::Add),
    ),
    (
        "u8",
        "wrapping_mul",
        "cheby_u8_wrapping_mul",
        KernelOp::Wrapping(ArithOp::Mul),
    ),
    (
        "u8",
        "wrapping_neg",
        "cheby_u8_wrapping_neg",
        KernelOp::WrappingNeg,
    ),
    (
        "u8",
        "wrapping_sub",
        "cheby_u8_wrapping_sub",
        KernelOp::Wrapping(ArithOp::Sub),
    ),
];

/// The operation of `symbol`, if it is the kernel of an entry of
/// [`INSTR_KERNELS`].
#[must_use]
pub fn op_of(symbol: &str) -> Option<KernelOp> {
    INSTR_KERNELS
        .iter()
        .find(|&&(_, _, s, _)| s == symbol)
        .map(|&(_, _, _, op)| op)
}

/// The rvalue of `op` on `args`, a direct call's arguments in the
/// representations `params` of the kernel's parameters, whose result is a
/// `ret`.
///
/// # Errors
///
/// An [`Ice`] if the signature is not one `op` computes.
pub fn rvalue(op: KernelOp, params: &[Repr], ret: Repr, args: Vec<Operand>) -> Result<Rvalue, Ice> {
    let integer = |r: Repr| r.int_bits().is_some();
    let shape = || {
        Ice::new(format!(
            "an instruction kernel {op:?} of signature {params:?} -> {ret}"
        ))
    };
    let mut args = args.into_iter();
    let (Some(first), second, None) = (args.next(), args.next(), args.next()) else {
        return Err(shape());
    };
    Ok(match (op, params, second) {
        (KernelOp::Convert, &[from], None) if from.converts_exactly(ret) => Rvalue::Convert {
            value: first,
            from,
            to: ret,
        },
        (KernelOp::Sqrt, &[Repr::Float], None) if ret == Repr::Float => Rvalue::FloatSqrt {
            value: first,
            repr: ret,
        },
        (KernelOp::Wrapping(op), &[a, b], Some(rhs)) if a == ret && b == ret && integer(ret) => {
            Rvalue::Wrapping {
                op,
                lhs: first,
                rhs,
                repr: ret,
            }
        }
        (KernelOp::WrappingNeg, &[a], None) if a == ret && integer(ret) => {
            let zero = if ret == Repr::Int {
                Const::Int(0)
            } else {
                Const::Sized { repr: ret, bits: 0 }
            };
            Rvalue::Wrapping {
                op: ArithOp::Sub,
                lhs: Operand::Const(zero),
                rhs: first,
                repr: ret,
            }
        }
        _ => return Err(shape()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernels_in_place::IN_PLACE_KERNELS;
    use crate::kernels_reading::READING_KERNELS;

    /// The representation of a type named in a std signature.
    fn repr(name: &str) -> Repr {
        match name {
            "Int" => Repr::Int,
            "Float" => Repr::Float,
            "I8" => Repr::I8,
            "I16" => Repr::I16,
            "I32" => Repr::I32,
            "I64" => Repr::I64,
            "U8" => Repr::U8,
            "U16" => Repr::U16,
            "U32" => Repr::U32,
            "U64" => Repr::U64,
            "F32" => Repr::F32,
            other => panic!("no scalar type {other}"),
        }
    }

    /// The table is sorted without duplicates, and each entry is a `pub`
    /// function of its std module, right after the `@external(native, …)`
    /// attribute that names its symbol, a plain kernel whose signature is
    /// one its operation computes, and on no other kernel table.
    #[test]
    fn the_table_names_native_wrappers_of_std_with_the_signatures_of_their_operations() {
        assert!(
            INSTR_KERNELS
                .windows(2)
                .all(|w| (w[0].0, w[0].1) < (w[1].0, w[1].1)),
            "{INSTR_KERNELS:?}"
        );
        let std_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std");
        for &(module, function, symbol, op) in INSTR_KERNELS {
            assert!(
                !IN_PLACE_KERNELS.iter().any(|&(_, _, s)| s == symbol)
                    && !READING_KERNELS.iter().any(|&(_, _, s)| s == symbol),
                "{symbol} is on another table"
            );
            let text = std::fs::read_to_string(std_dir.join(format!("{module}.cheby"))).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            let prefix = format!("pub fn {function}(");
            let at = lines
                .iter()
                .position(|l| l.starts_with(&prefix))
                .unwrap_or_else(|| panic!("{module}::{function} is not a pub fn"));
            let attrs: Vec<&str> = lines[..at]
                .iter()
                .rev()
                .take_while(|l| l.starts_with('@'))
                .copied()
                .collect();
            let native = format!("@external(native, \"{symbol}\")");
            assert!(
                attrs.contains(&native.as_str()),
                "{module}::{function}: no `{native}` in {attrs:?}"
            );
            assert!(
                !attrs
                    .iter()
                    .any(|a| a.starts_with("@std_context") || a.starts_with("@blocking")),
                "{module}::{function} is no plain kernel"
            );
            // `pub fn f(a: A, b: B) -> R`, with no body.
            let tail = lines[at].strip_prefix(&prefix).unwrap();
            let (params, ret) = tail.split_once(") -> ").unwrap();
            let params: Vec<Repr> = params
                .split(", ")
                .map(|p| repr(p.split_once(": ").unwrap().1))
                .collect();
            let args = params
                .iter()
                .map(|_| Operand::Const(Const::Int(0)))
                .collect();
            let value = rvalue(op, &params, repr(ret), args);
            assert!(value.is_ok(), "{module}::{function}: {value:?}");
            assert_eq!(op_of(symbol), Some(op));
        }
        assert_eq!(op_of("cheby_float_pow"), None);
        assert_eq!(op_of("cheby_int_checked_add"), None);
    }

    /// An operation refuses a signature it does not compute.
    #[test]
    fn a_signature_the_operation_does_not_compute_is_an_ice() {
        let one = || vec![Operand::Const(Const::Int(0))];
        assert!(rvalue(KernelOp::Convert, &[Repr::Int], Repr::I32, one()).is_err());
        assert!(rvalue(KernelOp::Convert, &[Repr::Float], Repr::Int, one()).is_err());
        assert!(rvalue(KernelOp::Sqrt, &[Repr::F32], Repr::F32, one()).is_err());
        assert!(
            rvalue(
                KernelOp::Wrapping(ArithOp::Add),
                &[Repr::Int],
                Repr::Int,
                one()
            )
            .is_err()
        );
        assert!(rvalue(KernelOp::WrappingNeg, &[Repr::Float], Repr::Float, one()).is_err());
    }
}
