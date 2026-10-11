//! The spelling of names where they are bound or declared: the case
//! conventions (E0006, §2.3) and the reserved words (E0007, §2.4).
//!
//! Each binding and declaration of the module is checked once. A use of
//! such a name adds nothing. A reserved word used as a name that nothing binds is
//! reported where it is used, instead of "cannot find value" (E0007, see
//! [`crate::paths`]).

use cheby_diag::Diagnostic;
use cheby_syntax::SyntaxTree;
use cheby_syntax::names::{bound_name, check_name};

/// E0006 and E0007 for the bound and declared names of `tree`.
pub(crate) fn check_names(tree: &SyntaxTree, diags: &mut Vec<Diagnostic>) {
    diags.extend(
        tree.root()
            .descendants()
            .filter_map(bound_name)
            .filter_map(check_name),
    );
}
