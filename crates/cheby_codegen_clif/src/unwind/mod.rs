//! Unwinding by return-flag propagation (D-733, ADR-0061).
//!
//! The unwind pass gives every statement that may start or pass on an
//! unwinding an edge to a pad (D-698). Codegen lowers every edge the MIR
//! has: each pad gets a cold block, and the translator (`func.rs`) hands
//! each statement or terminator with an edge to [`flag`]'s methods, which
//! emit the call and the branch to the pad. A function without edges
//! (code compiled with `CHEBY_UNWIND_EDGES=off`, D-742, outside the
//! precounted bodies of D-744) lowers as it would without the pass.
//!
//! The return flag is the only mechanism (D-733): it has no
//! architecture-specific code, and its pads are ordinary blocks. The other
//! mechanism that D-702 measured is gone, with its exit record and its
//! exit stubs (D-763, D-764).

pub(crate) mod flag;
