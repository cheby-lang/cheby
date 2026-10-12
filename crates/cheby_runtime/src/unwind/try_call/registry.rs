//! The process registry of the `try_call` prototype's handler tables
//! (D-696): for each registered function, its code range and its call
//! sites with a handler, looked up by return address in O(log n).
//!
//! The JIT registers each function of a batch once its code is finalized
//! and keeps the [`CodeRegistration`] with its code memory, so a range is
//! registered exactly while its code exists. Lookups take a read lock for
//! the time of one walk and never allocate; registering takes the write
//! lock. Neither is held across a fiber switch.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;

use super::CallSite;

/// A registered function: the end of its code and its call sites, sorted
/// by return address.
#[derive(Debug)]
struct Registered {
    len: usize,
    sites: Box<[CallSite]>,
    /// Which registration added it, so a stale registration never removes
    /// a newer one of the same address.
    token: u64,
}

/// Every registered function, by the address of its first byte.
static REGISTRY: RwLock<BTreeMap<usize, Registered>> = RwLock::new(BTreeMap::new());

/// The token of the next registration.
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// The registration of one function's handler table in the process
/// registry; it ends when the value drops.
#[derive(Debug)]
pub struct CodeRegistration {
    start: usize,
    token: u64,
}

impl Drop for CodeRegistration {
    fn drop(&mut self) {
        let mut registry = REGISTRY.write();
        if registry
            .get(&self.start)
            .is_some_and(|entry| entry.token == self.token)
        {
            registry.remove(&self.start);
        }
    }
}

/// Registers a finalized function's handler table; the registration ends
/// when the value drops. `start` is the address of the function's first
/// byte, `len` the size of its code, and each site's offsets are from
/// `start`. A function registered at the same address before is replaced.
#[must_use]
pub fn register_code(start: usize, len: usize, sites: Box<[CallSite]>) -> CodeRegistration {
    let mut sites = sites;
    sites.sort_unstable_by_key(|site| site.ret_offset);
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    REGISTRY
        .write()
        .insert(start, Registered { len, sites, token });
    CodeRegistration { start, token }
}

/// Whether a registered function's code holds `address`: for tests of
/// the registrations' lifetime.
#[doc(hidden)]
#[must_use]
pub fn is_registered(address: usize) -> bool {
    REGISTRY
        .read()
        .range(..=address)
        .next_back()
        .is_some_and(|(&start, entry)| address.wrapping_sub(start) < entry.len)
}

/// What a return address names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lookup {
    /// A call site of a registered function with a handler: the handler's
    /// address, and the distance from the frame pointer down to the stack
    /// pointer at the call.
    Handler { pc: usize, frame_offset: usize },
    /// A registered function, at a call without a handler: the unwinding
    /// continues in its caller.
    NoHandler,
    /// No registered function: not generated code.
    Unknown,
}

/// A view of the registry for one walk, holding its read lock.
pub(crate) struct Tables<'a> {
    map: parking_lot::RwLockReadGuard<'a, BTreeMap<usize, Registered>>,
}

/// The registry, read-locked until the value drops. The caller drops it
/// before it transfers control anywhere.
pub(crate) fn tables() -> Tables<'static> {
    Tables {
        map: REGISTRY.read(),
    }
}

impl Tables<'_> {
    /// What the return address `pc` names. A return address lies after its
    /// call, so it is above the function's start and at most its end.
    pub(crate) fn lookup(&self, pc: usize) -> Lookup {
        let Some((&start, entry)) = self.map.range(..pc).next_back() else {
            return Lookup::Unknown;
        };
        // `start < pc`, from the range.
        let offset = pc.wrapping_sub(start);
        if offset > entry.len {
            return Lookup::Unknown;
        }
        let Ok(offset) = u32::try_from(offset) else {
            return Lookup::Unknown;
        };
        let Ok(index) = entry
            .sites
            .binary_search_by_key(&offset, |site| site.ret_offset)
        else {
            return Lookup::NoHandler;
        };
        let Some(site) = entry.sites.get(index) else {
            return Lookup::NoHandler;
        };
        let Ok(handler) = usize::try_from(site.handler_offset) else {
            return Lookup::NoHandler;
        };
        let Ok(frame_offset) = usize::try_from(site.frame_offset) else {
            return Lookup::NoHandler;
        };
        Lookup::Handler {
            pc: start.wrapping_add(handler),
            frame_offset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(ret: u32, frame: u32, handler: u32) -> CallSite {
        CallSite {
            ret_offset: ret,
            frame_offset: frame,
            handler_offset: handler,
        }
    }

    // Addresses no real code has, so tests running in parallel with JIT
    // tests never meet them.
    const BASE: usize = 0x7fff_0000_0000;

    #[test]
    fn lookup_finds_handlers_by_return_address() {
        let a = register_code(
            BASE,
            0x100,
            vec![site(0x40, 0x20, 0x90), site(0x10, 0x30, 0x80)].into_boxed_slice(),
        );
        let b = register_code(BASE + 0x200, 0x80, Box::new([]));
        let tables = tables();
        assert_eq!(
            tables.lookup(BASE + 0x10),
            Lookup::Handler {
                pc: BASE + 0x80,
                frame_offset: 0x30
            }
        );
        assert_eq!(
            tables.lookup(BASE + 0x40),
            Lookup::Handler {
                pc: BASE + 0x90,
                frame_offset: 0x20
            }
        );
        assert_eq!(tables.lookup(BASE + 0x20), Lookup::NoHandler);
        // The end of a function can be a return address; past it is not.
        assert_eq!(tables.lookup(BASE + 0x100), Lookup::NoHandler);
        assert_eq!(tables.lookup(BASE + 0x101), Lookup::Unknown);
        assert_eq!(tables.lookup(BASE + 0x240), Lookup::NoHandler);
        assert_eq!(tables.lookup(BASE + 0x281), Lookup::Unknown);
        // A function's first byte is never a return address.
        assert_eq!(tables.lookup(BASE), Lookup::Unknown);
        drop(tables);
        assert!(is_registered(BASE));
        assert!(is_registered(BASE + 0xff));
        assert!(!is_registered(BASE + 0x100));
        assert!(is_registered(BASE + 0x27f));
        drop((a, b));
        let after = super::tables();
        assert_eq!(after.lookup(BASE + 0x10), Lookup::Unknown);
        assert_eq!(after.lookup(BASE + 0x240), Lookup::Unknown);
        drop(after);
        assert!(!is_registered(BASE));
    }

    #[test]
    fn a_stale_registration_keeps_the_newer_one() {
        const START: usize = BASE + 0x1000;
        let old = register_code(START, 0x40, vec![site(0x8, 0, 0x30)].into_boxed_slice());
        let new = register_code(START, 0x40, vec![site(0x10, 0, 0x30)].into_boxed_slice());
        drop(old);
        assert_eq!(
            tables().lookup(START + 0x10),
            Lookup::Handler {
                pc: START + 0x30,
                frame_offset: 0
            }
        );
        drop(new);
        assert_eq!(tables().lookup(START + 0x10), Lookup::Unknown);
    }
}
