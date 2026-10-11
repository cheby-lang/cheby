//! Reading module constants from their slots (E6, D-535).
//!
//! A constant's value is a uniform word in its `cb$const$` slot (D-456),
//! filled by constant evaluation before any code that reads it runs, or
//! forced on first read while constants are evaluated. `LoadConst` gives
//! the word borrowed from the slot; the RC pass duplicates it where the
//! reader keeps it. The word is then converted to the type's
//! representation, like any other uniform value.

use cheby_diag::Span;
use cheby_iface::{DefId, Ice, Ty};

use crate::ir::{Operand, Rvalue};
use crate::lower::body::BodyCx;
use crate::repr::Repr;

impl BodyCx<'_, '_> {
    /// The constant `def`, of type `ty`.
    pub(super) fn const_ref(
        &mut self,
        def: DefId,
        ty: Ty,
        _span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let word = self.define(Repr::Value, Rvalue::LoadConst(def))?;
        self.from_uniform(Operand::Local(word), ty).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use crate::golden_tests::check_golden;
    use crate::test_support::lower_source;

    /// A constant read in `main` and in another constant's initializer is
    /// a borrowed `load_const`; each reachable initializer is a
    /// `const_init` function of its module's unit (D-535).
    #[test]
    fn constants_load_from_their_slots() {
        let lowered = lower_source(&[(
            "",
            "//! Constants.

const base: Int = 40

const answer: Int = base + two()

const label: String = \"answer\"

fn two() -> Int {
  2
}

fn main() {
  assert answer == 42 as label
}
",
        )]);
        check_golden(
            "consts/load_const",
            &crate::dump(lowered.unit("app")).unwrap(),
        );
    }

    /// The wrapper of an external that constant evaluation may not run
    /// starts with the effect check; the kernels of the allowed std modules
    /// do not have it (D-537).
    #[test]
    fn only_forbidden_externals_check_effects() {
        let lowered = lower_source(&[(
            "",
            "//! Externals.

import std::io
import std::string

fn main() {
  io::println(string::uppercase(\"a\"))
}
",
        )]);
        let println = lowered.function("std::io", "std::io::println");
        assert!(println.contains("effect_check"), "{println}");
        let upper = lowered.function("std::string", "std::string::uppercase");
        assert!(!upper.contains("effect_check"), "{upper}");
    }
}
