//! Signatures at the machine level (D-453).
//!
//! Every Cranelift signature of a Cheby function comes from here, used by
//! MIR lowering (for [`crate::CalleeDecl`]) and by codegen, so a slot, a
//! `return_call` and the callee never disagree. The verifier compares only
//! result types, so a disagreement it cannot see would be undefined
//! behavior, not an error.
//!
//! The hidden `ctx` parameter (D-453) is not part of an [`AbiSig`]:
//! codegen adds it first to every signature here. Foreign signatures
//! ([`crate::ForeignSig`]) have no `ctx`.

use cheby_iface::{FnSig, Globals, Ice};

use crate::ir::{Abi, EntryKind, FnKey, Function};
use crate::repr::{Repr, repr_of};

/// A function's signature without the hidden `ctx`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct AbiSig {
    pub abi: Abi,
    pub params: Vec<Repr>,
    pub ret: Repr,
}

/// The signature of a top-level function, or of an `@external`'s wrapper:
/// one uniform word per dictionary (bounds in generic order), then the
/// parameters by [`repr_of`] of their declared types, and always a uniform
/// result (D-453).
#[must_use]
pub fn abi_sig(sig: &FnSig, globals: &Globals) -> AbiSig {
    let dicts = sig
        .generics
        .params
        .iter()
        .flat_map(|p| p.bounds.iter())
        .map(|_| Repr::Value);
    let params = sig.params.iter().map(|p| repr_of(p.ty, globals));
    AbiSig {
        abi: Abi::Cheby,
        params: dicts.chain(params).collect(),
        ret: Repr::Value,
    }
}

/// The uniform convention for unknown callees (closures, local functions,
/// function values, dictionary entries, `dyn` calls): the owned `self`,
/// then `arity` uniform words (D-453).
#[must_use]
pub fn uniform_sig(arity: u32) -> AbiSig {
    let count = usize::try_from(arity).unwrap_or(0).saturating_add(1);
    AbiSig {
        abi: Abi::Cheby,
        params: vec![Repr::Value; count],
        ret: Repr::Value,
    }
}

/// The signature of an entry thunk: C ABI, only `ctx`. `Main` and `Test`
/// thunks return the exit status as an `i64`; `Const` and `Repl` thunks
/// return the owned value.
#[must_use]
pub const fn entry_sig(kind: EntryKind) -> AbiSig {
    let ret = match kind {
        EntryKind::Main(_) | EntryKind::Test(_) => Repr::I64,
        EntryKind::Const(_) | EntryKind::Repl(_) => Repr::Value,
    };
    AbiSig {
        abi: Abi::C,
        params: Vec::new(),
        ret,
    }
}

/// The signature of a trampoline: C ABI, the closure, then `arity`
/// uniform words.
#[must_use]
pub fn trampoline_sig(arity: u32) -> AbiSig {
    AbiSig {
        abi: Abi::C,
        ..uniform_sig(arity)
    }
}

/// The signature of a key whose signature does not depend on any item's
/// declaration: constant initializers and test bodies take nothing and
/// return a uniform word; entries and trampolines as above. `None` for
/// items, shims and closures, whose signatures come from their
/// declarations.
#[must_use]
pub fn fixed_sig(key: FnKey) -> Option<AbiSig> {
    match key {
        FnKey::ConstInit(_) | FnKey::Test(_) => Some(AbiSig {
            abi: Abi::Cheby,
            params: Vec::new(),
            ret: Repr::Value,
        }),
        FnKey::Entry(kind) => Some(entry_sig(kind)),
        FnKey::Trampoline(arity) => Some(trampoline_sig(arity)),
        FnKey::Item(_) | FnKey::ValueShim(_) | FnKey::Closure { .. } => None,
    }
}

impl Function {
    /// The signature the function's body has.
    ///
    /// # Errors
    ///
    /// An [`Ice`] for an unknown parameter local.
    pub fn sig(&self) -> Result<AbiSig, Ice> {
        let params = self
            .params
            .iter()
            .map(|&p| Ok(self.local(p)?.repr))
            .collect::<Result<_, Ice>>()?;
        Ok(AbiSig {
            abi: self.abi,
            params,
            ret: self.ret,
        })
    }
}

#[cfg(test)]
mod tests {
    use cheby_iface::{GenericParam, Generics, IndexVec, ParamIdx, ParamSig, Ty, builtin, sym};
    use cheby_package::Targets;

    use super::*;

    fn sig(generics: Generics, params: &[Ty], ret: Ty) -> FnSig {
        FnSig {
            generics,
            params: params
                .iter()
                .map(|&ty| ParamSig { name: sym::T, ty })
                .collect(),
            ret,
            targets: Targets::NONE,
            deprecated: None,
        }
    }

    #[test]
    fn item_signatures() {
        let globals = Globals::new();
        let s = abi_sig(&sig(Generics::empty(), &[Ty::STRING], Ty::NIL), &globals);
        assert_eq!(
            s,
            AbiSig {
                abi: Abi::Cheby,
                params: vec![Repr::Value],
                ret: Repr::Value
            }
        );
        // Unboxed parameters, but always a uniform result.
        let s = abi_sig(
            &sig(Generics::empty(), &[Ty::INT, Ty::BOOL], Ty::INT),
            &globals,
        );
        assert_eq!(s.params, [Repr::Int, Repr::Bool]);
        assert_eq!(s.ret, Repr::Value);
        // One dictionary per bound, before the parameters.
        let t = globals.types.param(ParamIdx::from_u32(0));
        let generics = Generics {
            params: IndexVec::from_raw(vec![GenericParam {
                name: sym::T,
                bounds: Box::new([builtin::SHOW]),
            }])
            .unwrap(),
        };
        let s = abi_sig(&sig(generics, &[t, Ty::FLOAT], t), &globals);
        assert_eq!(s.params, [Repr::Value, Repr::Value, Repr::Float]);
    }

    #[test]
    fn fixed_signatures() {
        assert_eq!(uniform_sig(2).params.len(), 3);
        assert_eq!(trampoline_sig(1).abi, Abi::C);
        let main = cheby_iface::DefId::new(
            cheby_iface::ModuleId::from_u32(1),
            cheby_iface::DefIndex::from_u32(0),
        );
        assert_eq!(entry_sig(EntryKind::Main(main)).ret, Repr::I64);
        assert_eq!(entry_sig(EntryKind::Const(main)).ret, Repr::Value);
        assert_eq!(fixed_sig(FnKey::Item(main)), None);
        assert_eq!(
            fixed_sig(FnKey::Test(main)).map(|s| s.abi),
            Some(Abi::Cheby)
        );
    }
}
