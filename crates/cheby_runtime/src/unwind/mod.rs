//! Unwinding (D-077, D-696): the runtime side of the two unwinding
//! backends that are prototyped and measured before one is chosen. Both
//! sit behind the hidden `CHEBY_UNWIND` switch, whose default `off` keeps
//! M1's behavior ([`crate::abi::UnwindBackend`]).
//!
//! - [`try_call`]: the `try_call` prototype's resume entry and, later, its
//!   code registry, which `cheby_jit` reaches.
//!
//! [`UnwindState`] is the unwinding part of a fiber's control block
//! (D-685): the record of the unwinding in flight. It is empty until the
//! shared unwinding runtime lands (plan §2.4).

pub mod try_call;

/// The unwinding fields of a fiber control block (D-685, D-697): empty
/// until the unwinding runtime fills it.
#[derive(Debug, Default)]
pub(crate) struct UnwindState {}
