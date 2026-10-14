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

use cheby_iface::{FnSig, Globals, Ice, Idx};

use crate::borrow::BorrowMask;
use crate::ir::{Abi, EntryKind, FnKey, Function, Local};
use crate::liveness::counted;
use crate::repr::{Repr, repr_of};

/// A function's signature without the hidden `ctx`.
///
/// The borrow mask is part of it (D-715): two signatures that differ only
/// in which parameters are borrowed are different signatures, so they key
/// different slots (D-456) and a body never meets a caller of the other
/// convention. The machine signature ([`crate::AbiSig`] in codegen's
/// `sig::signature`) ignores the mask: borrowing changes which `dup` and
/// `drop` run, not how arguments are passed.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct AbiSig {
    pub abi: Abi,
    pub params: Vec<Repr>,
    pub ret: Repr,
    /// Which parameters the callee borrows (D-715): bit `i` is
    /// `params[i]`. Only a top-level function's signature borrows; every
    /// other signature is [`BorrowMask::OWNED`].
    pub borrow: BorrowMask,
}

impl AbiSig {
    /// A signature whose parameters are all owned.
    #[must_use]
    pub const fn owned(abi: Abi, params: Vec<Repr>, ret: Repr) -> Self {
        Self {
            abi,
            params,
            ret,
            borrow: BorrowMask::OWNED,
        }
    }
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
    AbiSig::owned(Abi::Cheby, dicts.chain(params).collect(), Repr::Value)
}

/// The uniform convention for unknown callees (closures, local functions,
/// function values, dictionary entries, `dyn` calls): the owned `self`,
/// then `arity` uniform words (D-453).
#[must_use]
pub fn uniform_sig(arity: u32) -> AbiSig {
    let count = usize::try_from(arity).unwrap_or(0).saturating_add(1);
    AbiSig::owned(Abi::Cheby, vec![Repr::Value; count], Repr::Value)
}

/// The signature of an entry thunk: C ABI, only `ctx`. `Main` thunks
/// return the exit status as an `i64`; `Test` thunks return the static
/// `Nil` when the test passed and the owned text of its `Err` otherwise
/// (D-541); `Const` and `Repl` thunks return the owned value.
#[must_use]
pub const fn entry_sig(kind: EntryKind) -> AbiSig {
    let ret = match kind {
        EntryKind::Main(_) => Repr::I64,
        EntryKind::Test(_) | EntryKind::Const(_) | EntryKind::Repl(_) => Repr::Value,
    };
    AbiSig::owned(Abi::C, Vec::new(), ret)
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
/// items, whose signatures come from their declarations, for
/// continuations (D-780), whose signatures are their own, and for the
/// uniform keys ([`FnKey::is_uniform`]: shims, closures, tuple dictionary
/// functions and `dyn` forwarders), whose arity the key does not hold;
/// the validator checks that they have [`uniform_sig`] of some arity.
#[must_use]
pub fn fixed_sig(key: FnKey) -> Option<AbiSig> {
    match key {
        FnKey::ConstInit(_) | FnKey::Test(_) => {
            Some(AbiSig::owned(Abi::Cheby, Vec::new(), Repr::Value))
        }
        FnKey::Entry(kind) => Some(entry_sig(kind)),
        FnKey::Trampoline(arity) => Some(trampoline_sig(arity)),
        FnKey::Item(_)
        | FnKey::ValueShim(_)
        | FnKey::Closure { .. }
        | FnKey::TupleDictFn { .. }
        | FnKey::DynForward { .. }
        | FnKey::Continuation { .. } => None,
    }
}

/// Whether `sig` is [`uniform_sig`] of some arity: the Cheby ABI, at least
/// the `self` parameter, only uniform words, and nothing borrowed.
#[must_use]
pub fn is_uniform_sig(sig: &AbiSig) -> bool {
    sig.abi == Abi::Cheby
        && !sig.params.is_empty()
        && sig.params.iter().all(|&r| r == Repr::Value)
        && sig.ret == Repr::Value
        && sig.borrow.is_owned()
}

impl Function {
    /// The signature the function's body has, its borrow mask included.
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
            borrow: self.borrow,
        })
    }

    /// The counted ([`Repr::Value`]) parameters the function owns, sorted:
    /// those its [`Function::borrow`] mask does not borrow. They are what
    /// `bb0` starts holding and what the entry pad releases (D-698, D-715).
    ///
    /// # Errors
    ///
    /// An [`Ice`] for an unknown parameter local.
    pub fn owned_params(&self) -> Result<Vec<Local>, Ice> {
        let mut out = Vec::with_capacity(self.params.len());
        for (i, &p) in self.params.iter().enumerate() {
            if counted(self, p)? && !self.borrow.is_borrowed(i) {
                out.push(p);
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    /// By local index, whether the local is borrowed (D-715): a counted
    /// parameter that [`Function::borrow`] borrows, or a counted `field`
    /// result of a borrowed local, a borrowed projection
    /// (GAP(borrowed-projections)). A borrowed local holds no reference: the
    /// caller's reference keeps it alive for the whole call, and the objects
    /// reachable from it are immutable and shared (their count is at least
    /// two for anything that would update them in place). It gets a `dup`
    /// at each owned use and is never dropped. Empty when nothing is
    /// borrowed; linear in the function otherwise.
    ///
    /// # Errors
    ///
    /// An [`Ice`] for an unknown local.
    pub fn borrowed_locals(&self) -> Result<Vec<bool>, Ice> {
        if self.borrow.is_owned() {
            return Ok(Vec::new());
        }
        let mut borrowed = vec![false; self.locals.len()];
        let mut work = Vec::new();
        for (i, &p) in self.params.iter().enumerate() {
            if self.borrow.is_borrowed(i) && counted(self, p)? {
                if let Some(slot) = borrowed.get_mut(p.index()) {
                    *slot = true;
                }
                work.push(p);
            }
        }
        // The counted `field` results, by base.
        let mut fields: Vec<(Local, Local)> = Vec::new();
        for block in &self.blocks {
            for stmt in &block.stmts {
                if let crate::ir::Stmt::Assign {
                    dst,
                    value: crate::ir::Rvalue::Field { base, .. },
                } = stmt
                    && counted(self, *dst)?
                {
                    fields.push((*base, *dst));
                }
            }
        }
        fields.sort_unstable();
        while let Some(base) = work.pop() {
            let from = fields.partition_point(|&(b, _)| b < base);
            for &(b, dst) in fields.get(from..).unwrap_or_default() {
                if b != base {
                    break;
                }
                if let Some(slot) = borrowed.get_mut(dst.index())
                    && !*slot
                {
                    *slot = true;
                    work.push(dst);
                }
            }
        }
        Ok(borrowed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cheby_iface::{GenericParam, Generics, IndexVec, ParamIdx, ParamSig, Ty, builtin, sym};

    fn sig(generics: Generics, params: &[Ty], ret: Ty) -> FnSig {
        FnSig {
            generics,
            params: params
                .iter()
                .map(|&ty| ParamSig { name: sym::T, ty })
                .collect(),
            ret,
            clamped: Box::new([]),
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
                ret: Repr::Value,
                borrow: BorrowMask::OWNED,
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
