//! Options of the MIR pipeline that a build may change: the
//! reference-counting mode (D-718). The driver reads them once per build
//! with [`MirOptions::from_env`] and passes them to
//! [`count_and_check_with`].
//!
//! `CHEBY_RC` is a hidden developer switch, honored by every build,
//! release included, so that `bench-run` can measure a release `cheby`
//! under both modes (D-702, D-718). GAP(rc-mode-env): the name, the values
//! and that release builds honor it are this plan's answer, not yet
//! logged.

use cheby_iface::Ice;

use crate::ir::MirModule;
use crate::validate::{Stage, validate};

/// How reference counts are inserted (D-273, D-718).
///
/// Naive counting stays selectable as the differential oracle of the
/// Perceus passes. Until those passes land, both modes run the same naive
/// pass, so their MIR is identical. GAP(rc-mode-env)
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
    /// Whether lowering keeps unwind edges. Read by the unwind pass, which
    /// is not in the pipeline yet; `true` by default.
    pub unwind_edges: bool,
}

impl Default for MirOptions {
    fn default() -> Self {
        Self {
            rc: RcMode::Perceus,
            unwind_edges: true,
        }
    }
}

impl MirOptions {
    /// The environment variable that selects the reference-counting mode.
    pub const RC_ENV: &'static str = "CHEBY_RC";

    /// Reads `CHEBY_RC`: `naive`, `perceus`, or unset or empty for the
    /// default. GAP(rc-mode-env)
    ///
    /// # Errors
    ///
    /// The message to report when the variable holds any other value
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
            ..Self::default()
        })
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

/// [`count_and_check`](crate::count_and_check) with options: validates a
/// freshly lowered unit, inserts reference counts as `options.rc` says,
/// and validates again with the ownership check.
///
/// # Errors
///
/// An [`Ice`] listing the validator's findings.
pub fn count_and_check_with(module: &mut MirModule, options: &MirOptions) -> Result<(), Ice> {
    let report = |stage: &str, errors: Vec<String>| {
        Ice::new(format!("invalid MIR {stage}:\n  {}", errors.join("\n  ")))
    };
    validate(module, Stage::Lowered).map_err(|e| report("after lowering", e))?;
    match options.rc {
        // Identical until the Perceus passes land (D-718).
        RcMode::Perceus | RcMode::Naive => crate::rc::insert_module(module)?,
    }
    validate(module, Stage::Counted).map_err(|e| report("after reference counting", e))
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
    fn defaults_are_perceus_with_unwind_edges() {
        let options = MirOptions::default();
        assert_eq!(options.rc, RcMode::Perceus);
        assert!(options.unwind_edges);
        assert_eq!(RcMode::default(), RcMode::Perceus);
    }
}
