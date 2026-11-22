//! The release profile's `consts` lever (design T10, refining D-257 and
//! D-977): the reads of a constant whose value a release build knows
//! become that value, in lowered MIR ([`crate::Stage::Lowered`]) before
//! inlining ([`fold_unit`]) and in the clean-up after it
//! ([`crate::simplify::Known::set_consts`]).
//!
//! The driver gives the values ([`ConstValues`]): the scalar literal
//! constants of the package's modules and of std, and the package's
//! computed constants of a scalar representation, read from their
//! evaluated images. A read of a constant is a slot word
//! (`load_const`), which lowering unboxes when the reader wants the raw
//! value. So:
//!
//! - an `unbox` of the word to the representation of a raw value the
//!   build knows (`Int`, `Float`, `Bool`, a sized number) is that value,
//!   and a `box` of that `unbox` is the word again, so a read whose value
//!   is boxed again, such as a list's element, allocates nothing;
//! - the word of a constant whose value is a static object (`Nil` or a
//!   builtin unit constructor, a literal's only values that are no raw
//!   number) is that object;
//! - a slot read left unused goes.
//!
//! Any other use of a scalar constant's word, such as an argument of a
//! call, still reads the slot, which costs no allocation where a box of
//! the value might; the clean-up after inlining folds the `unbox`es that
//! inlining brings to it. The slots are still installed, for the readers
//! left, for function values and for every debug build and REPL session,
//! which never fold. The pass keeps the statements' positions (D-938) and
//! is deterministic: it walks blocks and statements in order.

use std::collections::BTreeMap;

use cheby_iface::{DefId, Idx};
use rustc_hash::FxHashMap;

use crate::ir::{Const, Function, Local, MirModule, Operand, Rvalue, Stmt};
use crate::repr::Repr;

/// The values a release build folds into the reads of their constants,
/// by constant.
pub type ConstValues = BTreeMap<DefId, Const>;

/// The representation of `c` when it is a raw scalar value: `Int`,
/// `Float`, `Bool` or a sized number.
#[must_use]
pub const fn raw_repr(c: &Const) -> Option<Repr> {
    match c {
        Const::Int(_) => Some(Repr::Int),
        Const::Float(_) => Some(Repr::Float),
        Const::Bool(_) => Some(Repr::Bool),
        Const::Sized { repr, .. } => Some(*repr),
        Const::Str(_)
        | Const::Nil
        | Const::StaticCtor(_)
        | Const::FnValue(_)
        | Const::Code(_)
        | Const::EmptyList
        | Const::Static(_)
        | Const::BuiltinDesc(_)
        | Const::Ctx
        | Const::ErrNil => None,
    }
}

/// Whether `c` is a static object a constant's word may be: `Nil` or a
/// builtin unit constructor.
#[must_use]
pub const fn is_static_word(c: &Const) -> bool {
    matches!(c, Const::Nil | Const::StaticCtor(_))
}

/// Whether a constant whose value is `c` can be folded: a raw scalar or
/// a static word.
#[must_use]
pub const fn folds(c: &Const) -> bool {
    raw_repr(c).is_some() || is_static_word(c)
}

/// The raw value an `unbox` to `to` of constant `def`'s word gives, when
/// `values` holds one of that representation.
#[must_use]
pub fn unboxed(values: &ConstValues, def: DefId, to: Repr) -> Option<Const> {
    values
        .get(&def)
        .filter(|c| raw_repr(c) == Some(to))
        .cloned()
}

/// Folds the reads of `values` in every function of `unit` (module
/// docs); returns whether anything changed. A `precounted` body is left
/// alone.
pub fn fold_unit(unit: &mut MirModule, values: &ConstValues) -> bool {
    if values.is_empty() {
        return false;
    }
    let mut changed = false;
    for f in &mut unit.functions {
        changed |= fold_function(f, values);
    }
    changed
}

/// [`fold_unit`] on one function.
fn fold_function(f: &mut Function, values: &ConstValues) -> bool {
    if f.precounted {
        return false;
    }
    // The locals that hold a known constant's word, and the unboxes of
    // those words that give a known raw value.
    let mut loads: FxHashMap<Local, DefId> = FxHashMap::default();
    let mut unboxes: FxHashMap<Local, (Local, Repr)> = FxHashMap::default();
    for b in &f.blocks {
        for s in &b.stmts {
            if let Stmt::Assign {
                dst,
                value: Rvalue::LoadConst(def),
            } = s
                && values.get(def).is_some_and(folds)
            {
                loads.insert(*dst, *def);
            }
        }
    }
    if loads.is_empty() {
        return false;
    }
    for b in &f.blocks {
        for s in &b.stmts {
            if let Stmt::Assign {
                dst,
                value:
                    Rvalue::Unbox {
                        value: Operand::Local(w),
                        to,
                    },
            } = s
                && loads
                    .get(w)
                    .is_some_and(|&def| unboxed(values, def, *to).is_some())
            {
                unboxes.insert(*dst, (*w, *to));
            }
        }
    }
    let mut changed = false;
    for b in &mut f.blocks {
        for s in &mut b.stmts {
            let Stmt::Assign { value, .. } = s else {
                continue;
            };
            let folded = if let Rvalue::Unbox {
                value: Operand::Local(w),
                to,
            } = value
            {
                loads
                    .get(w)
                    .and_then(|&def| unboxed(values, def, *to))
                    .map(Operand::Const)
            } else if let Rvalue::Box {
                value: Operand::Local(x),
                from,
            } = value
            {
                // A box of the raw value is the constant's word again, as
                // boxing is canonical (D-278): the slot is read, and
                // nothing is allocated.
                unboxes
                    .get(x)
                    .filter(|&&(_, to)| to == *from)
                    .map(|&(w, _)| Operand::Local(w))
            } else if let Rvalue::LoadConst(def) = value {
                values
                    .get(def)
                    .filter(|c| is_static_word(c))
                    .cloned()
                    .map(Operand::Const)
            } else {
                None
            };
            if let Some(op) = folded {
                *value = Rvalue::Use(op);
                changed = true;
            }
        }
    }
    // The slot reads and raw values nothing reads any more.
    let uses = crate::simplify::use_counts(f);
    let unused = |l: &Local| uses.get(l.index()).is_none_or(|&n| n == 0);
    for b in &mut f.blocks {
        let before = b.stmts.len();
        b.retain_stmts(|s| match s {
            Stmt::Assign {
                dst,
                value: Rvalue::LoadConst(def),
            } => !(loads.get(dst) == Some(def) && unused(dst)),
            Stmt::Assign {
                dst,
                value: Rvalue::Use(Operand::Const(_)),
            } => !(unboxes.contains_key(dst) && unused(dst)),
            Stmt::Assign { .. }
            | Stmt::Eval(_)
            | Stmt::Dup(_)
            | Stmt::Drop(_)
            | Stmt::DropKeep { .. }
            | Stmt::ClearUnwind => true,
        });
        changed |= b.stmts.len() != before;
    }
    changed
}

#[cfg(test)]
mod tests {
    use crate::test_support::lower_source_release;

    /// A release build's package with literal constants of its own, of
    /// another module and of std, a computed one and a list.
    const FILES: [(&str, &str); 2] = [
        (
            "",
            "//! Reads constants.

import std::float
import std::u8
import app::util

const own: Int = 7

const doubled: Int = util::count * 2

const items: List<Int> = [1, 2]

fn total() -> Int {
  own + util::count + doubled
}

fn sizes() -> (Float, U8, Bool) {
  (float::pi * util::ratio, u8::max_value, util::flag)
}

fn below(x: U8) -> Bool {
  x < u8::max_value
}

fn pick() -> Int {
  case util::flag {
    True => 1
    False => 2
  }
}

fn missing() -> Option<Int> {
  util::nothing
}

fn listed() -> List<Int> {
  items
}

fn main() {
  assert total() + pick() > 0
  assert sizes() == sizes()
  assert below(3)
  assert missing() == None
  assert listed() == listed()
}
",
        ),
        (
            "util",
            "//! Literal constants of another module.

pub const count: Int = 1_000_003

pub const ratio: Float = -2.5

pub const flag: Bool = True

pub const nothing: Option<Int> = None
",
        ),
    ];

    /// Design T10: the release build reads each scalar literal constant as
    /// its value, its own module's, another module's and std's alike, and
    /// the clean-up folds the branch on one; a computed constant, whose
    /// value only an evaluation gives, and a list read their slots.
    #[test]
    fn literal_constants_fold_across_modules() {
        let lowered = lower_source_release(&FILES, false);
        let total = lowered.function("app", "app::total");
        assert!(!total.contains("load_const app::util::count"), "{total}");
        // `own + util::count` folds too.
        assert!(total.contains("const int 1000010"), "{total}");
        assert!(total.contains("load_const app::doubled"), "{total}");
        // The raw values fold; a tuple's elements are words, so the words
        // of `u8::max_value` and `flag` are read from their slots, not
        // boxed again.
        let sizes = lowered.function("app", "app::sizes");
        for folded in ["std::float::pi", "app::util::ratio"] {
            assert!(!sizes.contains(&format!("load_const {folded}")), "{sizes}");
        }
        assert!(sizes.contains("const float -2.5"), "{sizes}");
        for word in ["std::u8::max_value", "app::util::flag"] {
            assert!(sizes.contains(&format!("load_const {word}")), "{sizes}");
        }
        assert!(
            !sizes.contains("box u8") && !sizes.contains("box bool"),
            "{sizes}"
        );
        let below = lowered.function("app", "app::below");
        assert!(!below.contains("load_const"), "{below}");
        assert!(below.contains("const u8 255"), "{below}");
        let pick = lowered.function("app", "app::pick");
        assert!(!pick.contains("branch"), "{pick}");
        assert!(!pick.contains("load_const"), "{pick}");
        let missing = lowered.function("app", "app::missing");
        assert!(!missing.contains("load_const"), "{missing}");
        let listed = lowered.function("app", "app::listed");
        assert!(listed.contains("load_const app::items"), "{listed}");
    }
}
