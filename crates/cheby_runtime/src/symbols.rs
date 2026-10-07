//! The symbol registry: every C-ABI symbol generated code imports, with
//! its address, for the JIT to resolve (E5). Taking the
//! addresses here also keeps the linker from dropping them.

use std::ptr;

use crate::abi::sym;
use crate::alloc::cheby_rt_alloc;
use crate::io::{cheby_io_eprint, cheby_io_eprintln, cheby_io_print, cheby_io_println};
use crate::object::{
    StaticObject, cheby_rt_static_equal, cheby_rt_static_false, cheby_rt_static_greater,
    cheby_rt_static_less, cheby_rt_static_nil, cheby_rt_static_none, cheby_rt_static_true,
};
use crate::panic::{cheby_rt_panic, cheby_rt_stack_slow};
use crate::rc::{cheby_rt_drop, cheby_rt_dup};

fn data(object: &'static StaticObject) -> *const u8 {
    ptr::from_ref(object).cast::<u8>()
}

/// Every runtime symbol and its address, in the order of [`sym::ALL`].
#[must_use]
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
pub fn symbols() -> Vec<(&'static str, *const u8)> {
    vec![
        (sym::ALLOC, cheby_rt_alloc as *const u8),
        (sym::DUP, cheby_rt_dup as *const u8),
        (sym::DROP, cheby_rt_drop as *const u8),
        (sym::PANIC, cheby_rt_panic as *const u8),
        (sym::STACK_SLOW, cheby_rt_stack_slow as *const u8),
        (sym::STATIC_NIL, data(&cheby_rt_static_nil)),
        (sym::STATIC_FALSE, data(&cheby_rt_static_false)),
        (sym::STATIC_TRUE, data(&cheby_rt_static_true)),
        (sym::STATIC_LESS, data(&cheby_rt_static_less)),
        (sym::STATIC_EQUAL, data(&cheby_rt_static_equal)),
        (sym::STATIC_GREATER, data(&cheby_rt_static_greater)),
        (sym::STATIC_NONE, data(&cheby_rt_static_none)),
        (sym::IO_PRINT, cheby_io_print as *const u8),
        (sym::IO_PRINTLN, cheby_io_println as *const u8),
        (sym::IO_EPRINT, cheby_io_eprint as *const u8),
        (sym::IO_EPRINTLN, cheby_io_eprintln as *const u8),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi;
    use crate::object::static_ctor;

    #[test]
    fn symbols_list_every_abi_name_in_order() {
        let names: Vec<_> = symbols().into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, sym::ALL);
        assert!(symbols().iter().all(|(_, address)| !address.is_null()));
    }

    #[test]
    fn static_ctor_symbols_point_to_their_objects() {
        let table = symbols();
        for id in 0..abi::layout::FIRST_USER {
            let Some(name) = sym::static_ctor(id) else {
                assert!(static_ctor(id).is_none());
                continue;
            };
            let address = table
                .iter()
                .find(|(entry, _)| *entry == name)
                .map(|(_, address)| *address);
            assert_eq!(address, static_ctor(id).map(data));
        }
    }
}
