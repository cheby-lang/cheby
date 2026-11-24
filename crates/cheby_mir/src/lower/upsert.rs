//! A direct call of `map::upsert` whose callback generated code calls.
//!
//! The kernel `cheby_map_upsert` calls the callback itself, through a
//! trampoline from Rust (D-526), so no inlining or clone reaches it (D-1780)
//! and every call pays the trampoline. A direct call is the kernel's work in
//! steps instead, in one block, with the callback called as any function
//! value is:
//!
//! ```text
//! m1 = call_runtime map_upsert(m, key)            // hashes the key once
//! a = call_runtime map_upsert_address(m1)
//! found = call_runtime map_upsert_found(m1, a)    // Some(stored value) or None
//! r = update(found)
//! m2 = call_runtime map_upsert_store(m1, a, key, r)
//! ```
//!
//! `map_upsert` consumes the map and returns one with a root of its own,
//! unique or empty, with the path to the key writable, so the slot it
//! gives is this code's alone. An absent key is put in by `map_upsert`
//! with a placeholder value, so storing the callback's result needs no
//! second walk. An empty map is left alone and its key is not hashed: the
//! kernel calls the callback before it hashes the key of an empty map, and
//! `map_upsert_store` hashes it then, as the kernel does (D-233).
//!
//! What a program sees is the kernel's: the arguments are evaluated before
//! anything happens, the callback is called exactly once, with the value
//! the map held and its count (moved out, so a value only the map held
//! arrives unique, D-525), and a present key keeps the stored key
//! (D-588). The placeholder is a tagged word, so when the callback panics
//! or is cancelled, the map is an ordinary value that the cleanup releases
//! once (D-698), and so is the moved-out value, which the callback owns.
//! The root's update is counted in place or copied once, in `map_upsert`
//! (D-768).
//!
//! The steps stay in one block, as the kernel's call was, so a value that
//! dies before the upsert can still be reused by an allocation after it
//! (D-839). A known top-level function is called directly; any other
//! callback through the uniform convention (D-453). In release, the
//! clean-up turns a call of a known function value or closure into a
//! direct call that the inliner copies, and when the inlined callback only
//! takes the `Option` apart, `map_upsert_found` goes: its tag is the
//! address's lowest bit, and its value is `map_slot_take`'s, so nothing is
//! allocated (`crate::lookup::fold_upsert`).

use cheby_iface::{Ice, Ty};

use crate::abi::abi_sig;
use crate::ir::{Callee, Const, Local, Operand, RuntimeFn, Rvalue};
use crate::lower::body::BodyCx;
use crate::repr::Repr;

/// The kernel of `map::upsert`, which a direct call replaces.
pub(super) const MAP_UPSERT: &str = "cheby_map_upsert";

impl BodyCx<'_, '_> {
    /// A direct call of `map::upsert` with `args`, the map, the key and the
    /// callback, already lowered (module documentation). In tail position
    /// (`tail`) the result is returned, and the call records no site, as a
    /// kernel call in tail position does not (D-852).
    ///
    /// # Errors
    ///
    /// An [`Ice`] for malformed arguments.
    pub(super) fn call_upsert(
        &mut self,
        args: Vec<Operand>,
        ret_ty: Ty,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        let [map, key, update]: [Operand; 3] = args
            .try_into()
            .map_err(|_| Ice::new("map::upsert: not three arguments"))?;
        let opened = self.b.assign(
            Repr::Value,
            Rvalue::CallRuntime {
                func: RuntimeFn::MapUpsert,
                args: vec![map, key.clone()],
            },
        )?;
        // The site of the call's kernel line in the reuse report (D-852).
        if !tail && let Some(site) = self.site {
            self.b.record_site(opened, site);
        }
        let address = self.b.assign(
            Repr::U64,
            Rvalue::CallRuntime {
                func: RuntimeFn::MapUpsertAddress,
                args: vec![Operand::Local(opened)],
            },
        )?;
        let found = self.b.assign(
            Repr::Value,
            Rvalue::CallRuntime {
                func: RuntimeFn::MapUpsertFound,
                args: vec![Operand::Local(opened), Operand::Local(address)],
            },
        )?;
        let value = self.call_update(&update, Operand::Local(found))?;
        let result = self.b.assign(
            Repr::Value,
            Rvalue::CallRuntime {
                func: RuntimeFn::MapUpsertStore,
                args: vec![
                    Operand::Local(opened),
                    Operand::Local(address),
                    key,
                    Operand::Local(value),
                ],
            },
        )?;
        self.kernel_result(Operand::Local(result), Repr::Value, ret_ty, tail)
    }

    /// A call of the callback `update` with `arg`, an owned `Option`: a
    /// direct call of a top-level function given as a value with one
    /// uniform parameter, else a call through the uniform convention
    /// (D-453). The result is a uniform word, owned. The call's location is
    /// the upsert's (D-856).
    fn call_update(&mut self, update: &Operand, arg: Operand) -> Result<Local, Ice> {
        let callee = match update {
            Operand::Const(Const::FnValue(def)) if self.takes_one_value(*def)? => {
                Callee::Direct(*def)
            }
            Operand::Local(l) => Callee::Indirect(*l),
            Operand::Const(c) => Callee::Indirect(
                self.b
                    .assign(Repr::Value, Rvalue::Use(Operand::Const(c.clone())))?,
            ),
        };
        let result = self.b.assign(
            Repr::Value,
            Rvalue::Call {
                callee,
                args: vec![arg],
            },
        )?;
        if self.b.records_call_locs()
            && let Some(site) = self.site
        {
            let location = self.cx.location(site)?;
            self.b.record_call_loc(result, &location)?;
        }
        Ok(result)
    }

    /// Whether the top-level function `def` takes one uniform parameter and
    /// no dictionary, so a direct call can pass it the `Option` as it is.
    fn takes_one_value(&self, def: cheby_iface::DefId) -> Result<bool, Ice> {
        self.cx.defs.replaced(def)?;
        let sig = abi_sig(self.cx.defs.fn_sig(def)?, self.cx.defs.globals);
        Ok(sig.params.as_slice() == [Repr::Value])
    }
}

#[cfg(test)]
#[path = "upsert_tests.rs"]
mod tests;
