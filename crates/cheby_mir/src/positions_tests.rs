//! Tests of source positions in MIR (D-938, GAP(mir-positions)): lowering
//! gives every statement and terminator the line and column (D-421) of the
//! construct it lowers, the passes keep them, the dumps never show them,
//! and positions change nothing else in MIR.

use std::fmt::Write as _;

use crate::ir::{Callee, Function, MirModule, Rvalue, Stmt, Terminator};
use crate::positions::{self, Pos};
use crate::test_support::{
    lower_package_options, lower_source, lower_source_split, lower_source_uncounted,
};
use crate::{MirOptions, dump, dump_function};

const CALLS: &str = "//! Calls and an interpolation.

import std::io

fn add(a: Int, b: Int) -> Int {
  a + b
}

fn main() {
  let n = add(1, 2)
  let m = add(n, 3)
  io::println(\"n={n} m={m}\")
}
";

const CASES: &str = "//! A case, a closure and a tuple.

import std::io
import std::list

fn describe(x: Option<Int>) -> Int {
  case x {
    Some(v) => v + 1
    None => 0
  }
}

fn pair(xs: List<Int>) -> (List<Int>, List<Int>) {
  (xs, xs)
}

fn main() {
  let ys = list::map([1, 2, 3], fn(k) { k * 2 })
  let (a, b) = pair(ys)
  let d = describe(Some(list::length(a)))
  let n = list::length(b)
  io::println(\"{d} {n}\")
}
";

fn pos(line: u32, column: u32) -> Pos {
    Pos { line, column }
}

/// The function of `unit` whose dump header starts `fn <key>(`.
fn function<'a>(unit: &'a MirModule, key: &str) -> &'a Function {
    unit.functions
        .iter()
        .find(|f| {
            dump_function(unit, f)
                .unwrap()
                .strip_prefix("fn ")
                .is_some_and(|rest| rest.starts_with(&format!("{key}(")))
        })
        .unwrap_or_else(|| panic!("no function `{key}` in:\n{}", dump(unit).unwrap()))
}

/// The positions of the statements of `f` that `pick` selects, in block
/// and statement order.
fn stmt_positions(f: &Function, pick: impl Fn(&Stmt) -> bool) -> Vec<Pos> {
    let mut out = Vec::new();
    for block in &f.blocks {
        for (i, stmt) in block.stmts.iter().enumerate() {
            if pick(stmt) {
                out.push(f.positions.get(block.pos_at(i)).unwrap());
            }
        }
    }
    out
}

/// The positions of the calls of `path` in `f`: statements and tail
/// calls.
fn call_positions(unit: &MirModule, f: &Function, path: &str) -> Vec<Pos> {
    let callee_is = |c: &Callee| match c {
        Callee::Direct(d) | Callee::Borrowing(d, _) => unit.name(*d).unwrap().path() == path,
        _ => false,
    };
    let mut out = stmt_positions(
        f,
        |s| matches!(s, Stmt::Assign { value: Rvalue::Call { callee, .. }, .. } | Stmt::Eval(Rvalue::Call { callee, .. }) if callee_is(callee)),
    );
    for block in &f.blocks {
        if let Terminator::TailCall { callee, .. } = &block.term
            && callee_is(callee)
        {
            out.push(f.positions.get(block.term_pos()).unwrap());
        }
    }
    out
}

/// Every block of every function of `unit` that has positions keeps one
/// per statement and one for its terminator, each in the table.
fn assert_complete(unit: &MirModule) {
    for f in &unit.functions {
        positions::check(f).unwrap();
        if f.positions.is_empty() {
            continue;
        }
        for (id, block) in f.blocks.iter_enumerated() {
            assert!(
                block.has_positions(),
                "{:?} {id:?} lost its positions",
                f.key
            );
            for i in 0..=block.stmts.len() {
                assert!(
                    f.positions.get(block.pos_at(i)).is_some(),
                    "{:?} {id:?} statement {i} has no position",
                    f.key
                );
            }
        }
    }
}

/// A call's statement is at the call expression, the function's entry at
/// its name, and the unit names its file by its diagnostic path (D-421).
#[test]
fn positions_of_calls_and_entries() {
    let lowered = lower_source_uncounted(&[("", CALLS)]);
    let app = lowered.unit("app");
    assert_eq!(app.source.as_deref(), Some("src/main.cheby"));
    let main = function(app, "app::main");
    assert_eq!(main.positions.get(main.positions.entry), Some(pos(9, 4)));
    assert_eq!(
        call_positions(app, main, "app::add"),
        vec![pos(10, 11), pos(11, 11)]
    );
    // `io::println(…)` is the body's tail call.
    assert_eq!(
        call_positions(app, main, "std::io::println"),
        vec![pos(12, 3)]
    );
    let add = function(app, "app::add");
    assert_eq!(add.positions.get(add.positions.entry), Some(pos(5, 4)));
    // The return is at the body's tail, `a + b`.
    let ret = add
        .blocks
        .iter()
        .find(|b| matches!(b.term, Terminator::Return(_)))
        .unwrap();
    assert_eq!(add.positions.get(ret.term_pos()), Some(pos(6, 3)));
    // std's units name their own files.
    assert_eq!(
        lowered.unit("std::io").source.as_deref(),
        Some("std/io.cheby")
    );
}

/// An interpolation's value parts: each part's `show` call and append are
/// at the part, on the interpolation's line.
#[test]
fn positions_of_interpolation_parts() {
    let lowered = lower_source_uncounted(&[("", CALLS)]);
    let app = lowered.unit("app");
    let main = function(app, "app::main");
    let appends = stmt_positions(main, |s| {
        matches!(
            s,
            Stmt::Assign {
                value: Rvalue::CallRuntime {
                    func: crate::ir::RuntimeFn::StrAppend,
                    ..
                },
                ..
            }
        )
    });
    // "n=", {n}, " m=", {m}: the text parts at the string, the values at
    // `n` (column 19) and `m` (column 25).
    assert_eq!(appends.len(), 4, "{appends:?}");
    assert!(appends.iter().all(|p| p.line == 12), "{appends:?}");
    let columns: Vec<u32> = appends.iter().map(|p| p.column).collect();
    assert_eq!(columns, vec![15, 19, 15, 25], "{appends:?}");
    let shows = call_positions(app, main, "std::int::show");
    assert_eq!(shows, vec![pos(12, 19), pos(12, 25)]);
}

/// A `case`: the code that binds an arm's names is at the arm's pattern,
/// the arm's body at its own expression.
#[test]
fn positions_of_case_arms() {
    let lowered = lower_source_uncounted(&[("", CASES)]);
    let app = lowered.unit("app");
    let describe = function(app, "app::describe");
    let fields = stmt_positions(describe, |s| {
        matches!(
            s,
            Stmt::Assign {
                value: Rvalue::Field { .. },
                ..
            }
        )
    });
    // `Some(v)` at line 8, column 5: `v`'s read.
    assert_eq!(fields, vec![pos(8, 5)]);
    let checked = stmt_positions(describe, |s| {
        matches!(
            s,
            Stmt::Assign {
                value: Rvalue::Checked { .. },
                ..
            }
        )
    });
    // `v + 1` at column 16.
    assert_eq!(checked, vec![pos(8, 16)]);
}

/// A closure's code: its entry at the closure, its body at its own
/// expression.
#[test]
fn positions_of_a_closure() {
    let lowered = lower_source_uncounted(&[("", CASES)]);
    let app = lowered.unit("app");
    let closure = app
        .functions
        .iter()
        .find(|f| matches!(f.key, crate::ir::FnKey::Closure { .. }))
        .unwrap();
    // `fn(k) { k * 2 }` starts at line 18, column 33.
    assert_eq!(
        closure.positions.get(closure.positions.entry),
        Some(pos(18, 33))
    );
    let checked = stmt_positions(closure, |s| {
        matches!(
            s,
            Stmt::Assign {
                value: Rvalue::Checked { .. },
                ..
            }
        )
    });
    assert_eq!(checked, vec![pos(18, 41)]);
}

/// A literal split into continuations (D-780): each continuation keeps
/// positions, its entry at the literal and its items at their own lines.
#[test]
fn positions_of_a_split_literal() {
    let items: String = (0..24).fold(String::new(), |mut items, k| {
        let _ = writeln!(items, "    x + {k},");
        items
    });
    let src = format!(
        "//! A split list.\n\nimport std::io\nimport std::list\n\nfn build(x: Int) -> List<Int> {{\n  [\n{items}  ]\n}}\n\nfn main() {{\n  let n = list::length(build(1))\n  io::println(\"{{n}}\")\n}}\n"
    );
    let lowered = lower_source_split(&[("", &src)], 10, false);
    let app = lowered.unit("app");
    assert_complete(app);
    let chain: Vec<&Function> = app
        .functions
        .iter()
        .filter(|f| matches!(f.key, crate::ir::FnKey::Continuation { .. }))
        .collect();
    assert!(chain.len() >= 4, "{}", dump(app).unwrap());
    let mut lines = Vec::new();
    for f in chain {
        // The literal starts at line 7, column 3.
        assert_eq!(f.positions.get(f.positions.entry), Some(pos(7, 3)));
        lines.extend(stmt_positions(f, |s| {
            matches!(
                s,
                Stmt::Assign {
                    value: Rvalue::Checked { .. },
                    ..
                }
            )
        }));
    }
    // Each item `x + k` on its own line, 8 to 31, in order.
    let items: Vec<u32> = lines.iter().map(|p| p.line).collect();
    assert!(items.windows(2).all(|w| w[0] < w[1]), "{items:?}");
    assert!(items.iter().all(|&l| (8..=31).contains(&l)), "{items:?}");
}

/// A count the RC pass inserts takes the position of the statement it
/// belongs to: `(xs, xs)` needs one more reference to `xs`, at the tuple.
#[test]
fn positions_of_a_statement_the_rc_pass_inserts() {
    let lowered = lower_source(&[("", CASES)]);
    let app = lowered.unit("app");
    assert_complete(app);
    let pair = function(app, "app::pair");
    let dups = stmt_positions(pair, |s| matches!(s, Stmt::Dup(_)));
    // `(xs, xs)` at line 14, column 3.
    assert_eq!(dups, vec![pos(14, 3)]);
    // Every unit of the build keeps positions through every pass.
    for unit in &lowered.units {
        assert_complete(unit);
    }
}

/// Positions change nothing else (D-938): the units counted with
/// positions off equal those counted with them once their positions are
/// cleared, and the dumps are the same (GAP(dumps-without-positions)).
#[test]
fn positions_change_no_mir_and_no_dump() {
    for src in [CALLS, CASES] {
        let on = lower_package_options("app", &[("", src)], MirOptions::default()).unwrap();
        let off = lower_package_options(
            "app",
            &[("", src)],
            MirOptions {
                positions: false,
                ..MirOptions::default()
            },
        )
        .unwrap();
        assert_eq!(on.dump(), off.dump());
        assert!(on.units.iter().any(|u| u.source.is_some()));
        for (mut with, without) in on.units.into_iter().zip(off.units) {
            assert!(without.source.is_none());
            assert!(
                without
                    .functions
                    .iter()
                    .all(|f| f.positions.is_empty() && f.blocks.iter().all(|b| !b.has_positions()))
            );
            positions::clear(&mut with);
            assert_eq!(with, without);
        }
    }
}
