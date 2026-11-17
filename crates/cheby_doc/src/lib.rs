//! Documentation generator (§13.10).
//!
//! Planned for M5. See `docs/plan/architecture.md`.

// Dependencies of the planned crate graph (docs/plan/architecture.md) that
// no code uses yet; these keep cargo's `unused_dependencies` lint quiet
// until the milestone named on each line uses them.
use cheby_driver as _; // M5
