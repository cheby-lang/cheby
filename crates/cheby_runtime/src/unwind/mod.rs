//! Unwinding (D-077, D-696): the runtime side of the two unwinding
//! backends that are prototyped and measured before one is chosen. Both
//! sit behind the hidden `CHEBY_UNWIND` switch, whose default `off` keeps
//! M1's behavior ([`crate::abi::UnwindBackend`]).
//!
//! - [`try_call`]: the `try_call` prototype's resume entry and, later, its
//!   code registry, which `cheby_jit` reaches.

pub mod try_call;
