//! Borrowed parameters under the JIT (D-715): hand-built MIR whose
//! callees borrow, counted and given unwind edges by the real pipeline
//! (`cheby_mir::count_and_check`), and compiled with the checked count
//! operations. A borrowing callee never releases what it borrows; the
//! caller keeps the argument across the call and releases it after the
//! call returns, or in its cleanup pad when the callee unwinds (D-698), so
//! nothing of the entry's domain stays alive either way. Run with
//! `CHEBY_RT_POISON=1` for freed objects to be poisoned (D-458).

use cheby_codegen_clif::CodegenOptions;
use cheby_mir::LayoutRef;
use cheby_mir::borrow::BorrowMask;
use cheby_runtime::abi::PanicKind as RtPanicKind;

use super::*;

/// The mask that borrows parameter 0.
fn first() -> BorrowMask {
    BorrowMask::OWNED.with(0)
}

/// `alloc (first, Nil)` into local `dst`.
fn pair(dst: u32, first: Operand) -> Stmt {
    assign(
        dst,
        Rvalue::Alloc {
            layout: LayoutRef::Tuple(2),
            fields: vec![first, Operand::Const(Const::Nil)],
        },
    )
}

/// `panic Explicit("boom")`: `str#1` is the location, `str#2` the message.
fn boom() -> Terminator {
    Terminator::Panic {
        kind: PanicKind::Explicit,
        message: Some(Operand::Const(Const::Str(StrId::from_u32(2)))),
        loc: StrId::from_u32(1),
    }
}

const BOOM: [&str; 2] = [LOC, "boom"];

/// `app::<item>` with index `index` and one counted parameter, which it
/// borrows: its body and every callee declaration of it say so.
fn borrowing_item(
    p: &mut Prog,
    index: u32,
    item: &str,
    locals: &[Repr],
    blocks: Vec<Block>,
) -> DefId {
    let strings: &[&str] = if blocks
        .iter()
        .any(|b| matches!(b.term, Terminator::Panic { .. }))
    {
        &BOOM
    } else {
        &[]
    };
    let f = p.item(index, item, &[Repr::Value], locals, blocks, strings);
    if let Some(body) = p.functions.last_mut() {
        body.borrow = first();
    }
    for decl in p.callees.iter_mut().filter(|d| d.def == f) {
        decl.sig.borrow = first();
    }
    f
}

/// `f() { t = alloc (alloc (Nil, Nil), Nil); r = call <callee>(borrow t);
/// return r }`, written without counts.
fn caller(p: &mut Prog, callee: DefId) -> DefId {
    p.item(
        0,
        "f",
        &[],
        &[Repr::Value; 3],
        straight(
            vec![
                pair(0, Operand::Const(Const::Nil)),
                pair(1, op(0)),
                assign(
                    2,
                    Rvalue::Call {
                        callee: Callee::Borrowing(callee, first()),
                        args: vec![op(1)],
                    },
                ),
            ],
            2,
        ),
        &[],
    )
}

/// Counts both units with the pipeline every build uses, checks them, and
/// compiles them with the checked count operations. Returns the JIT and
/// the dump of the code unit.
fn compile_counted(p: Prog) -> (Jit, String) {
    let [mut code, mut entries] = p.units();
    cheby_mir::count_and_check(&mut code).unwrap();
    cheby_mir::count_and_check(&mut entries).unwrap();
    let text = cheby_mir::dump(&code).unwrap();
    let codegen = CodegenOptions {
        checked_rc: true,
        ..CodegenOptions::default()
    };
    let mut jit = Jit::configured(codegen, JitOptions::from_env()).unwrap();
    jit.compile(&[code, entries]).unwrap();
    (jit, text)
}

fn assert_nothing_live(jit: &Jit) {
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0, "objects leaked");
    }
}

/// A panic inside a borrowing callee: `g(borrow t)` panics, so `g`
/// releases nothing (its entry pad and its pad hold no parameter), and
/// the caller's pad releases the tuple it lent, with the object inside it.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn borrow_panic_in_a_borrowing_callee_is_released_by_the_caller() {
    let mut p = Prog::default();
    let g = borrowing_item(&mut p, 1, "g", &[], vec![block(Vec::new(), boom())]);
    let f = caller(&mut p, g);
    let target = p.call(f, Vec::new());
    let (jit, text) = compile_counted(p);
    // `f` keeps `_1` across the call: no `dup` before it, a `drop` after
    // it, and the call's pad drops it.
    assert!(
        text.contains("call app::g(borrow _1) ; unwind bb"),
        "{text}"
    );
    assert!(text.contains("fn app::g(borrow _0: value)"), "{text}");
    let g_text = text
        .split("\nfn ")
        .find(|f| f.starts_with("app::g("))
        .unwrap();
    assert!(!g_text.contains("drop _0"), "{g_text}");
    let outcome = run_const(&jit, target);
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("the entry returned: {outcome:?}");
    };
    assert_eq!(report.kind, RtPanicKind::Explicit, "{report:?}");
    assert_eq!(report.message, "boom");
    assert_nothing_live(&jit);
}

/// A borrowed projection passed on borrowed, then a panic further down:
/// `g(borrow t) { i = field t.0; r = call h(borrow i); return r }` and
/// `h(borrow x)` panics. Neither `g` nor `h` counts anything; the caller
/// releases the whole tuple (D-807).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn borrow_projection_passed_on_and_a_panic_below_it() {
    let mut p = Prog::default();
    let below = borrowing_item(&mut p, 2, "h", &[], vec![block(Vec::new(), boom())]);
    let middle = borrowing_item(
        &mut p,
        1,
        "g",
        &[Repr::Value, Repr::Value],
        straight(
            vec![
                assign(
                    1,
                    Rvalue::Field {
                        base: local(0),
                        layout: LayoutRef::Tuple(2),
                        field: 0,
                    },
                ),
                assign(
                    2,
                    Rvalue::Call {
                        callee: Callee::Borrowing(below, first()),
                        args: vec![op(1)],
                    },
                ),
            ],
            2,
        ),
    );
    let top = caller(&mut p, middle);
    let target = p.call(top, Vec::new());
    let (jit, text) = compile_counted(p);
    let g_text = text
        .split("\nfn ")
        .find(|f| f.starts_with("app::g("))
        .unwrap();
    assert!(
        !g_text.contains("dup ") && !g_text.contains("drop "),
        "{g_text}"
    );
    let outcome = run_const(&jit, target);
    assert!(
        matches!(&outcome, EntryOutcome::Panicked(r) if r.kind == RtPanicKind::Explicit),
        "{outcome:?}"
    );
    assert_nothing_live(&jit);
}

/// The normal path: `g(borrow t) { i = field t.0; return i }` returns a
/// part of what it borrows, which needs its own reference (a `dup`), and
/// the caller releases the tuple after the call. The result outlives the
/// tuple.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn borrow_callee_returns_a_part_of_what_it_borrows() {
    let mut p = Prog::default();
    let g = borrowing_item(
        &mut p,
        1,
        "g",
        &[Repr::Value],
        straight(
            vec![assign(
                1,
                Rvalue::Field {
                    base: local(0),
                    layout: LayoutRef::Tuple(2),
                    field: 0,
                },
            )],
            1,
        ),
    );
    let f = caller(&mut p, g);
    let target = p.call(f, Vec::new());
    let (jit, text) = compile_counted(p);
    let g_text = text
        .split("\nfn ")
        .find(|f| f.starts_with("app::g("))
        .unwrap();
    assert!(g_text.contains("dup _1\n  return _1"), "{g_text}");
    assert!(!g_text.contains("drop "), "{g_text}");
    let word = returned(run_const(&jit, target));
    // The inner pair survives with the one reference the result holds.
    assert_eq!(rc_of(word), 1);
    release(&jit, word);
    assert_nothing_live(&jit);
}
