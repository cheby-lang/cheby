//! Reading module constants from their slots (E6, D-535).
//!
//! A constant's value is a uniform word in its `cb$const$` slot (D-456),
//! filled by constant evaluation before any code that reads it runs, or
//! forced on first read while constants are evaluated. `LoadConst` gives
//! the word borrowed from the slot; the RC pass duplicates it where the
//! reader keeps it. The word is then converted to the type's
//! representation, like any other uniform value.
//!
//! A constant whose initializer is a scalar literal ([`scalar_literal`])
//! is read as that literal in the code of its own module: its value is
//! fixed by the module's own source, so the read costs nothing and the
//! module's interface still holds only the constant's type (D-246, D-257).
//! Its slot and its `const_init` function stay, for other modules, function
//! values and the REPL. A REPL session lowers its code without this
//! ([`super::lower_items_late_bound`]): a later entry may give the slot
//! another value (D-043, D-547).

use std::collections::BTreeMap;

use cheby_diag::Span;
use cheby_iface::{DefId, Globals, Ice, Ty, builtin};
use cheby_types::thir::{self, ExprKind, Lit};

use crate::ir::{Const, Operand, Rvalue};
use crate::lower::body::BodyCx;
use crate::lower::scalar::{float_const, int_const};
use crate::repr::{Repr, repr_of};

/// The value of the constant `c` when its initializer is a scalar literal:
/// an integer or float literal, negative ones included (§5.3.1), or one of
/// the builtin unit constructors `Nil`, `True`, `False`, `None`, `Less`,
/// `Equal` and `Greater`. The value is the MIR constant that the literal
/// lowers to in its type's representation. `None` for any other
/// initializer: a string literal, a parenthesized or computed value, a
/// user's unit constructor.
#[must_use]
pub fn scalar_literal(c: &thir::ConstDef, globals: &Globals) -> Option<Const> {
    let body = &c.body;
    let expr = body.expr(body.value)?;
    if expr.ty != body.ret {
        return None;
    }
    let repr = repr_of(expr.ty, globals);
    if let ExprKind::Lit(lit) = &expr.kind {
        return if let Lit::Int(v) = lit {
            int_const(*v, repr)
        } else if let Lit::Float(v) = lit {
            float_const(*v, repr)
        } else {
            None
        };
    }
    let ExprKind::Ctor { ctor, .. } = expr.kind else {
        return None;
    };
    match ctor {
        c if c == builtin::NIL => Some(Const::Nil),
        c if c == builtin::TRUE && repr == Repr::Bool => Some(Const::Bool(true)),
        c if c == builtin::FALSE && repr == Repr::Bool => Some(Const::Bool(false)),
        c if [
            builtin::NONE,
            builtin::LESS,
            builtin::EQUAL,
            builtin::GREATER,
        ]
        .contains(&c) =>
        {
            Some(Const::StaticCtor(c))
        }
        _ => None,
    }
}

/// The constants of `module` whose initializer is a scalar literal, each
/// with its value and type: what its module's code reads in place of its
/// slot.
pub(super) fn literal_consts(
    module: &thir::Module,
    globals: &Globals,
) -> BTreeMap<DefId, (Const, Ty)> {
    module
        .consts
        .iter()
        .filter_map(|c| Some((c.def, (scalar_literal(c, globals)?, c.body.ret))))
        .collect()
}

impl BodyCx<'_, '_> {
    /// The constant `def`, of type `ty`: its literal when it is a scalar
    /// literal of this module (module docs), else its slot's word.
    pub(super) fn const_ref(
        &mut self,
        def: DefId,
        ty: Ty,
        _span: Span,
    ) -> Result<Option<Operand>, Ice> {
        if let Some((value, of)) = self.pending.literal_const(def)
            && of == ty
        {
            return Ok(Some(Operand::Const(value)));
        }
        let word = self.define(Repr::Value, Rvalue::LoadConst(def))?;
        self.from_uniform(Operand::Local(word), ty).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use crate::golden_tests::check_golden;
    use crate::test_support::{lower_source, lower_source_late_bound};

    /// A constant read in `main` and in another constant's initializer is
    /// a borrowed `load_const`; each reachable initializer is a
    /// `const_init` function of its module's unit (D-535).
    #[test]
    fn constants_load_from_their_slots() {
        let lowered = lower_source(&[(
            "",
            "//! Constants.

const base: Int = 4 * 10

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

    /// The package of the literal tests: scalar literal constants of every
    /// kind, two computed ones and an imported literal.
    const LITERALS: [(&str, &str); 2] = [
        (
            "",
            "//! Literal constants.

import app::util

const count: Int = 1_000_003

const huge: Int = -9_223_372_036_854_775_808

const ratio: Float = -2.5

const small: U8 = 200

const wide: U64 = 18_446_744_073_709_551_615

const flag: Bool = True

const nothing: Option<Int> = None

const product: Int = 2 * 3

const items: List<Int> = [1, 2]

const label: String = \"label\"

fn total() -> Int {
  count + huge + product + util::shared
}

fn numbers() -> (Float, U8, U64) {
  (ratio, small, wide)
}

fn others() -> (Bool, Option<Int>, List<Int>) {
  (flag, nothing, items)
}

fn name() -> String {
  label
}

fn main() {
  assert total() != 0
  assert numbers() == numbers()
  assert others() == others()
  assert name() == name()
}
",
        ),
        (
            "util",
            "//! A literal constant of another module.

pub const shared: Int = 7
",
        ),
    ];

    /// A scalar literal constant of the module is read as its literal;
    /// a computed constant, a list, a string and a constant of another
    /// module are read from their slots. Every constant keeps its
    /// `const_init` function, so its slot is filled as before.
    #[test]
    fn literal_constants_of_the_module_are_read_as_literals() {
        let lowered = lower_source(&LITERALS);
        check_golden(
            "consts/literal_constants",
            &crate::dump(lowered.unit("app")).unwrap(),
        );
        let total = lowered.function("app", "app::total");
        assert!(total.contains("load_const app::product"), "{total}");
        assert!(total.contains("load_const app::util::shared"), "{total}");
        assert!(!total.contains("load_const app::count"), "{total}");
        assert!(!total.contains("load_const app::huge"), "{total}");
        let parts = [
            lowered.function("app", "app::numbers"),
            lowered.function("app", "app::others"),
            lowered.function("app", "app::name"),
        ]
        .join("\n");
        for read in ["items", "label"] {
            assert!(
                parts.contains(&format!("load_const app::{read}")),
                "{parts}"
            );
        }
        for folded in ["ratio", "small", "wide", "flag", "nothing"] {
            assert!(
                !parts.contains(&format!("load_const app::{folded}")),
                "{parts}"
            );
        }
        for c in ["count", "huge", "ratio", "flag", "product", "items"] {
            assert!(
                lowered
                    .unit("app")
                    .functions
                    .iter()
                    .any(|f| crate::dump_function(lowered.unit("app"), f)
                        .unwrap()
                        .starts_with(&format!("fn const_init app::{c}("))),
                "{c} lost its initializer"
            );
        }
    }

    /// A REPL session's code reads every constant from its slot, a literal
    /// of its own module too: a later entry may redefine it in place
    /// (D-043, D-547).
    #[test]
    fn late_bound_code_reads_literal_constants_from_their_slots() {
        let lowered = lower_source_late_bound(&LITERALS);
        let total = lowered.function("app", "app::total");
        assert!(total.contains("load_const app::count"), "{total}");
        assert!(total.contains("load_const app::huge"), "{total}");
        let others = lowered.function("app", "app::others");
        assert!(others.contains("load_const app::flag"), "{others}");
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
