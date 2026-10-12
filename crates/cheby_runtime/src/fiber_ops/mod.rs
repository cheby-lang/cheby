//! The runtime side of `std::fiber` and `std::channel`: scopes, spawning,
//! joining and channels (D-693, D-706). Only the symbols generated code
//! calls exist so far, as stubs; their bodies arrive with the scheduler.

pub(crate) mod scope;
