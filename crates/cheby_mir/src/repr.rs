//! Machine representations of values (D-021, D-278, D-279) and the native
//! foreign-call mapping (§12.5.1).

use std::fmt;

use cheby_iface::{FnSig, Globals, Ty, TyKind, builtin};

/// How a MIR local holds its value.
///
/// [`Repr::Value`] is the uniform word: a tagged 63-bit integer (low bit 1)
/// or a pointer (low bit 0) to a heap or static object. The others are
/// unboxed machine values used where the type is known (D-278).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Repr {
    /// One uniform word, reference counted (D-273).
    Value,
    /// `Int`, unboxed: an `i64`.
    Int,
    /// `Float`, unboxed: an `f64`.
    Float,
    /// `Bool`, unboxed: an `i8` holding 0 or 1.
    Bool,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
}

impl Repr {
    /// Whether locals of this representation are reference counted.
    #[must_use]
    pub const fn is_counted(self) -> bool {
        matches!(self, Self::Value)
    }

    /// Whether this is one of the sized number representations
    /// (`I8` … `U64`, `F32`), which [`crate::Const::Sized`] holds.
    #[must_use]
    pub const fn is_sized(self) -> bool {
        matches!(
            self,
            Self::I8
                | Self::I16
                | Self::I32
                | Self::I64
                | Self::U8
                | Self::U16
                | Self::U32
                | Self::U64
                | Self::F32
        )
    }

    /// Whether this is a signed integer representation.
    #[must_use]
    pub const fn is_signed_int(self) -> bool {
        matches!(
            self,
            Self::Int | Self::I8 | Self::I16 | Self::I32 | Self::I64
        )
    }

    /// Whether this is an unsigned integer representation.
    #[must_use]
    pub const fn is_unsigned_int(self) -> bool {
        matches!(self, Self::U8 | Self::U16 | Self::U32 | Self::U64)
    }

    /// The name used in dumps.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Value => "value",
            Self::Int => "int",
            Self::Float => "float",
            Self::Bool => "bool",
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::F32 => "f32",
        }
    }
}

impl fmt::Display for Repr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The representation of values of the closed type `ty`: `Int`, `Float`,
/// `Bool` and the sized numbers are unboxed; everything else (type
/// parameters, `Nil`, `Never`, `String`, ADTs, functions, tuples, `dyn`)
/// is a uniform [`Repr::Value`].
#[must_use]
pub fn repr_of(ty: Ty, globals: &Globals) -> Repr {
    let TyKind::Named { def, .. } = globals.types.kind(ty) else {
        return Repr::Value;
    };
    match *def {
        d if d == builtin::INT => Repr::Int,
        d if d == builtin::FLOAT => Repr::Float,
        d if d == builtin::BOOL => Repr::Bool,
        d if d == builtin::I8 => Repr::I8,
        d if d == builtin::I16 => Repr::I16,
        d if d == builtin::I32 => Repr::I32,
        d if d == builtin::I64 => Repr::I64,
        d if d == builtin::U8 => Repr::U8,
        d if d == builtin::U16 => Repr::U16,
        d if d == builtin::U32 => Repr::U32,
        d if d == builtin::U64 => Repr::U64,
        d if d == builtin::F32 => Repr::F32,
        _ => Repr::Value,
    }
}

/// A C type of the native foreign ABI (§12.5.1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CType {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    Bool,
    /// An opaque pointer to a Cheby value: a uniform word. Arguments are
    /// borrowed, results owned (D-187).
    Ptr,
    /// `void`, for a `Nil` result only.
    Void,
}

impl CType {
    /// The MIR representation a value of this C type is held in, or `None`
    /// for `void`.
    #[must_use]
    pub const fn repr(self) -> Option<Repr> {
        Some(match self {
            Self::I8 => Repr::I8,
            Self::I16 => Repr::I16,
            Self::I32 => Repr::I32,
            Self::I64 => Repr::Int,
            Self::U8 => Repr::U8,
            Self::U16 => Repr::U16,
            Self::U32 => Repr::U32,
            Self::U64 => Repr::U64,
            Self::F32 => Repr::F32,
            Self::F64 => Repr::Float,
            Self::Bool => Repr::Bool,
            Self::Ptr => Repr::Value,
            Self::Void => return None,
        })
    }

    /// Whether a MIR local of `repr` may be passed as, or receive, this C
    /// type. `int64_t` carries both `Int` and `I64`.
    #[must_use]
    pub fn accepts(self, repr: Repr) -> bool {
        self.repr() == Some(repr) || (self == Self::I64 && repr == Repr::I64)
    }

    /// Whether the running fiber's context ([`crate::Const::Ctx`]) may be
    /// passed as this C type: only a pointer. Only the first operand of a
    /// foreign call may be `ctx`, the `@std_context` kernel's first
    /// parameter (D-692). GAP(const-ctx-operand).
    #[must_use]
    pub const fn accepts_ctx(self) -> bool {
        matches!(self, Self::Ptr)
    }

    /// The name used in dumps.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Bool => "bool",
            Self::Ptr => "ptr",
            Self::Void => "void",
        }
    }
}

impl fmt::Display for CType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The C signature of a foreign function: arguments in order, then the
/// result. Integers narrower than 64 bits are extended by the caller
/// (codegen adds `sext`/`uext`, §12.5.1).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ForeignSig {
    pub params: Vec<CType>,
    pub ret: CType,
}

/// The C type of a parameter or result of type `ty` (§12.5.1). `Nil` maps
/// to `void` only as a result; as a parameter it is an opaque pointer.
#[must_use]
pub fn ctype_of(ty: Ty, globals: &Globals, is_result: bool) -> CType {
    if is_result && ty == Ty::NIL {
        return CType::Void;
    }
    match repr_of(ty, globals) {
        Repr::Value => CType::Ptr,
        Repr::Int | Repr::I64 => CType::I64,
        Repr::Float => CType::F64,
        Repr::Bool => CType::Bool,
        Repr::I8 => CType::I8,
        Repr::I16 => CType::I16,
        Repr::I32 => CType::I32,
        Repr::U8 => CType::U8,
        Repr::U16 => CType::U16,
        Repr::U32 => CType::U32,
        Repr::U64 => CType::U64,
        Repr::F32 => CType::F32,
    }
}

/// The C signature of an `@external(native, …)` function (§12.5.1).
#[must_use]
pub fn foreign_sig(sig: &FnSig, globals: &Globals) -> ForeignSig {
    ForeignSig {
        params: sig
            .params
            .iter()
            .map(|p| ctype_of(p.ty, globals, false))
            .collect(),
        ret: ctype_of(sig.ret, globals, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reprs_of_prelude_types() {
        let globals = Globals::new();
        assert_eq!(repr_of(Ty::INT, &globals), Repr::Int);
        assert_eq!(repr_of(Ty::FLOAT, &globals), Repr::Float);
        assert_eq!(repr_of(Ty::BOOL, &globals), Repr::Bool);
        assert_eq!(repr_of(Ty::U64, &globals), Repr::U64);
        assert_eq!(repr_of(Ty::STRING, &globals), Repr::Value);
        assert_eq!(repr_of(Ty::NIL, &globals), Repr::Value);
        assert_eq!(repr_of(Ty::NEVER, &globals), Repr::Value);
        let list = globals.types.named(builtin::LIST, [Ty::INT]);
        assert_eq!(repr_of(list, &globals), Repr::Value);
    }

    #[test]
    fn ctypes_follow_the_native_abi() {
        let globals = Globals::new();
        assert_eq!(ctype_of(Ty::NIL, &globals, true), CType::Void);
        assert_eq!(ctype_of(Ty::NIL, &globals, false), CType::Ptr);
        assert_eq!(ctype_of(Ty::STRING, &globals, false), CType::Ptr);
        assert_eq!(ctype_of(Ty::INT, &globals, false), CType::I64);
        assert_eq!(ctype_of(Ty::FLOAT, &globals, true), CType::F64);
        assert!(CType::I64.accepts(Repr::Int));
        assert!(CType::I64.accepts(Repr::I64));
        assert!(!CType::Ptr.accepts(Repr::Int));
        assert_eq!(CType::Void.repr(), None);
    }
}
