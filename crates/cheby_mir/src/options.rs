//! Options of the MIR pipeline that a build may change: the
//! reference-counting mode (D-718). The driver reads them once per build
//! with [`MirOptions::from_env`] and passes them to
//! [`count_and_check_with`].
//!
//! `CHEBY_RC` is a hidden developer switch, honored by every build,
//! release included, so that `bench-run` can measure a release `cheby`
//! under both modes (D-702, D-718); the name, the values and that release
//! builds honor it are D-746's. `CHEBY_UNWIND_EDGES=off` turns the unwind
//! pass off for D-702's measurement (D-742).

use cheby_iface::Ice;

use crate::ir::MirModule;
use crate::unwind::Recorded;
use crate::validate::{Stage, validate, validate_recorded};

/// How reference counts are inserted (D-273, D-718).
///
/// Naive counting stays selectable as the differential oracle of the
/// Perceus passes. Both modes run the same placement pass; Perceus adds
/// the borrowing a build infers (D-715, [`MirOptions::borrows`]), so
/// naive MIR is the MIR before borrowing (D-746).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RcMode {
    /// The optimized passes (D-273); the default.
    #[default]
    Perceus,
    /// M1's naive placement by the last-use rule (D-205), kept as the
    /// reference the optimized passes are compared with (D-718).
    Naive,
}

impl RcMode {
    /// The value of `CHEBY_RC` that selects this mode.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Perceus => "perceus",
            Self::Naive => "naive",
        }
    }
}

/// What the MIR pipeline does to a unit after lowering.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MirOptions {
    /// The reference-counting mode.
    pub rc: RcMode,
    /// Whether the unwind pass adds unwind edges and pads (D-698); `true`
    /// by default. `false` is the measurement baseline of D-702: no
    /// function gets new edges (a `precounted` body keeps its own), and
    /// the units are validated at [`Stage::Counted`].
    pub unwind_edges: bool,
    /// Whether the validation after the unwind pass runs the full,
    /// independent ownership simulation as well as checking each pad chain
    /// against the states the pass recorded (D-774, D-698, D-735). `true`
    /// by default in debug builds, so in every test run; `false` in
    /// release builds, where the pass's simulation is the only one. No
    /// variable sets it.
    pub full_ownership_check: bool,
    /// Whether a build infers borrowing across its units (D-715, D-779;
    /// [`crate::borrow`]); `true` by default. A REPL session sets it to
    /// `false` for every unit it lowers, std and package modules included
    /// (D-808): a later batch could tail-call a function
    /// compiled with a mask that its slot cannot change. Borrowing needs
    /// [`RcMode::Perceus`] too ([`MirOptions::borrows`]).
    pub borrow: bool,
}

impl Default for MirOptions {
    fn default() -> Self {
        Self {
            rc: RcMode::Perceus,
            unwind_edges: true,
            full_ownership_check: cfg!(debug_assertions),
            borrow: true,
        }
    }
}

impl MirOptions {
    /// The environment variable that selects the reference-counting mode.
    pub const RC_ENV: &'static str = "CHEBY_RC";

    /// Whether a build with these options infers borrowing (D-715): with
    /// [`MirOptions::borrow`] and under [`RcMode::Perceus`] only, so naive
    /// counting stays the MIR before borrowing, the oracle of D-718
    /// (D-808).
    #[must_use]
    pub const fn borrows(&self) -> bool {
        self.borrow && matches!(self.rc, RcMode::Perceus)
    }

    /// The environment variable that turns the unwind pass off, for the
    /// measurement of D-702 (`cheby_runtime::abi::UNWIND_EDGES_ENV`; this
    /// crate does not depend on the runtime; D-742).
    pub const UNWIND_EDGES_ENV: &'static str = "CHEBY_UNWIND_EDGES";

    /// Reads `CHEBY_RC` (`naive`, `perceus`, or unset or empty for the
    /// default; D-746) and `CHEBY_UNWIND_EDGES` (see
    /// [`MirOptions::unwind_edges_from_env`]).
    ///
    /// # Errors
    ///
    /// The message to report when a variable holds any other value
    /// (including one that is not Unicode).
    pub fn from_env() -> Result<Self, String> {
        let value = std::env::var_os(Self::RC_ENV);
        let rc = match value.as_deref().map(std::ffi::OsStr::to_str) {
            None | Some(Some("")) => RcMode::default(),
            Some(Some(text)) => Self::parse_rc(text)?,
            Some(None) => {
                return Err(format!(
                    "{} must be `naive` or `perceus`, not a value that is not Unicode",
                    Self::RC_ENV
                ));
            }
        };
        Ok(Self {
            rc,
            unwind_edges: Self::unwind_edges_from_env()?,
            ..Self::default()
        })
    }

    /// Reads `CHEBY_UNWIND_EDGES`: `off` turns the unwind pass off (D-742:
    /// the measurement baseline of D-702, honored by every build, release
    /// included); unset or empty keeps it on. Code compiled without edges
    /// unwinds nowhere: a raise in it ends the process with its report
    /// (D-770).
    ///
    /// # Errors
    ///
    /// The message to report when the variable holds any other value
    /// (including one that is not Unicode).
    pub fn unwind_edges_from_env() -> Result<bool, String> {
        let value = std::env::var_os(Self::UNWIND_EDGES_ENV);
        match value.as_deref().map(std::ffi::OsStr::to_str) {
            None => Ok(true),
            Some(Some(text)) => Self::parse_unwind_edges(text),
            Some(None) => Err(format!(
                "{} must be off or empty, not a value that is not Unicode",
                Self::UNWIND_EDGES_ENV
            )),
        }
    }

    /// Whether the value `text` of `CHEBY_UNWIND_EDGES` keeps the edges.
    fn parse_unwind_edges(text: &str) -> Result<bool, String> {
        match text {
            "" => Ok(true),
            "off" => Ok(false),
            _ => Err(format!(
                "{} must be off or empty, not \"{}\"",
                Self::UNWIND_EDGES_ENV,
                text.escape_debug()
            )),
        }
    }

    /// The mode `text` names.
    fn parse_rc(text: &str) -> Result<RcMode, String> {
        [RcMode::Naive, RcMode::Perceus]
            .into_iter()
            .find(|mode| mode.name() == text)
            .ok_or_else(|| {
                format!(
                    "{} must be `naive` or `perceus`, not `{}`",
                    Self::RC_ENV,
                    text.escape_debug()
                )
            })
    }
}

fn report(stage: &str, errors: &[String]) -> Ice {
    Ice::new(format!("invalid MIR {stage}:\n  {}", errors.join("\n  ")))
}

/// [`count_and_check`](crate::count_and_check) with options: validates a
/// freshly lowered unit ([`check_lowered`]), then counts it
/// ([`count_lowered_with`]). The unit is counted on its own, so it is
/// never inferred: what it borrows was written by
/// [`crate::borrow::apply`], if anything.
///
/// # Errors
///
/// An [`Ice`] listing the validator's findings.
pub fn count_and_check_with(module: &mut MirModule, options: &MirOptions) -> Result<(), Ice> {
    check_lowered(module)?;
    count_lowered_with(module, options)
}

/// Validates a freshly lowered unit at [`Stage::Lowered`], the first step
/// of [`count_and_check_with`], which a build runs on every unit before
/// it infers borrowing across them (D-779).
///
/// # Errors
///
/// An [`Ice`] listing the validator's findings.
pub fn check_lowered(module: &MirModule) -> Result<(), Ice> {
    validate(module, Stage::Lowered).map_err(|e| report("after lowering", &e))
}

/// The rest of [`count_and_check_with`], for a unit [`check_lowered`]
/// accepted (and [`crate::borrow::apply`] may have changed since): inserts
/// reference counts as `options.rc` says, then, when
/// `options.unwind_edges`, the unwind edges and pads (D-698, the last
/// pass, D-262), and validates again: at [`Stage::Unwound`] after the
/// unwind pass, at [`Stage::Counted`] without it.
///
/// With the pass, its ownership simulation is the check of the counts
/// (D-774): a broken rule it finds is reported as invalid MIR after
/// reference counting, and the validation after the pass checks each pad
/// chain against the states it recorded, adding the full simulation only
/// when `options.full_ownership_check`. A unit that borrows something
/// under options that do not borrow ([`MirOptions::borrows`]) is an
/// internal error.
///
/// # Errors
///
/// An [`Ice`] listing the validator's findings.
pub fn count_lowered_with(module: &mut MirModule, options: &MirOptions) -> Result<(), Ice> {
    if !options.borrows() && crate::borrow::borrows_anything(module) {
        return Err(Ice::new(format!(
            "a unit of {} borrows, but its options do not (D-715)",
            module.path
        )));
    }
    match options.rc {
        // The same placement pass in both modes; Perceus differs only by
        // the borrowing the build inferred before (D-715, D-718).
        RcMode::Perceus | RcMode::Naive => crate::rc::insert_module(module)?,
    }
    if options.unwind_edges {
        // The unwind pass simulates the counts itself; `Stage::Unwound`
        // checks everything else `Counted` does, so the unit is validated
        // once more, not twice.
        let recorded = unwind_recording(module)?;
        validate_recorded(module, &recorded, options.full_ownership_check)
            .map_err(|e| report("after the unwind pass", &e))
    } else {
        validate(module, Stage::Counted).map_err(|e| report("after reference counting", &e))
    }
}

/// Runs the unwind pass over every function of `module` and returns what
/// its simulation recorded for each, in order (D-774).
///
/// # Errors
///
/// An [`Ice`] listing the broken ownership rules the simulation found, as
/// invalid MIR after reference counting (each function is left as it
/// was), or for malformed MIR.
pub(crate) fn unwind_recording(module: &mut MirModule) -> Result<Vec<Recorded>, Ice> {
    let mut recorded = Vec::with_capacity(module.functions.len());
    let mut errors = Vec::new();
    for f in &mut module.functions {
        match crate::unwind::insert_recording(f)? {
            Ok(states) => recorded.push(states),
            Err(found) => {
                let name = format!("{:?}", f.key);
                errors.extend(found.into_iter().map(|e| format!("{name}: {e}")));
            }
        }
    }
    if errors.is_empty() {
        Ok(recorded)
    } else {
        Err(Ice::new(format!(
            "invalid MIR after reference counting:\n  {}",
            errors.join("\n  ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rc_modes_parse_by_name() {
        assert_eq!(MirOptions::parse_rc("naive"), Ok(RcMode::Naive));
        assert_eq!(MirOptions::parse_rc("perceus"), Ok(RcMode::Perceus));
        let err = MirOptions::parse_rc("Naive").unwrap_err();
        assert_eq!(err, "CHEBY_RC must be `naive` or `perceus`, not `Naive`");
        assert!(MirOptions::parse_rc("bogus").is_err());
    }

    #[test]
    fn unwind_edges_parse_off_or_empty() {
        assert_eq!(MirOptions::parse_unwind_edges(""), Ok(true));
        assert_eq!(MirOptions::parse_unwind_edges("off"), Ok(false));
        for bogus in ["on", "Off", "0", "false", " off"] {
            let err = MirOptions::parse_unwind_edges(bogus).unwrap_err();
            assert!(err.starts_with("CHEBY_UNWIND_EDGES must be off or empty, not \""));
        }
        assert_eq!(
            MirOptions::parse_unwind_edges("on").unwrap_err(),
            "CHEBY_UNWIND_EDGES must be off or empty, not \"on\""
        );
    }

    #[test]
    fn defaults_are_perceus_with_unwind_edges() {
        let options = MirOptions::default();
        assert_eq!(options.rc, RcMode::Perceus);
        assert!(options.unwind_edges);
        // Every test run checks ownership with the full simulation
        // (D-774).
        assert_eq!(options.full_ownership_check, cfg!(debug_assertions));
        assert_eq!(RcMode::default(), RcMode::Perceus);
    }

    #[test]
    fn borrow_only_with_perceus_and_the_switch() {
        // D-715, D-718: naive counting is the MIR before borrowing, and a
        // REPL session turns borrowing off (D-808).
        let options = MirOptions::default();
        assert!(options.borrow && options.borrows());
        let naive = MirOptions {
            rc: RcMode::Naive,
            ..MirOptions::default()
        };
        assert!(!naive.borrows());
        let repl = MirOptions {
            borrow: false,
            ..MirOptions::default()
        };
        assert!(!repl.borrows());
    }
}
