//! JavaScript emitter and link-time suspension analysis (ADR-0048).
//!
//! Planned for M4. See `docs/plan/architecture.md`.

// Dependencies of the planned crate graph (docs/plan/architecture.md) that
// no code uses yet; these keep cargo's `unused_dependencies` lint quiet
// until the milestone named on each line uses them.
use cheby_mir as _; // M4
