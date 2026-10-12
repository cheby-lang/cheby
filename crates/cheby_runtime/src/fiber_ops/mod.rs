//! The runtime side of `std::fiber` and `std::channel`: scopes, spawning,
//! joining and channels (D-693, D-706). Only the symbols generated code
//! calls exist so far, as stubs; their bodies arrive with the scheduler.
//!
//! [`FiberConc`] is the structured-concurrency part of a fiber's control
//! block (D-685): its scope, its open scopes, its join cell and the fields
//! other fibers reach under the fiber lock (plan §2.4). It is empty until
//! scopes, spawning and channels land.

pub(crate) mod scope;

/// The structured-concurrency fields of a fiber control block (D-685,
/// D-706): empty until the runtime of `std::fiber` fills it.
#[derive(Debug, Default)]
pub(crate) struct FiberConc {}
