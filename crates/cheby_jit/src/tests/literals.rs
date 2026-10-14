//! Large literals under the JIT (D-720, D-780): tuples filled one field
//! at a time from a placeholder allocation (GAP(tuple-init-field)), and
//! literals continued in chains of continuations
//! (GAP(continuation-symbol)). Hand-built MIR in the shapes lowering
//! makes, counted and given unwind edges by the real pipeline
//! (`cheby_mir::count_and_check`), compiled with the checked count
//! operations. A panic in the middle of a tuple, or in continuation `k`,
//! leaves nothing of the entry's domain alive: the pad of the frame that
//! panics releases what it holds (the partly filled tuple, whose
//! placeholders hold nothing), and the call in the function holding the
//! literal releases the rest. Run with `CHEBY_RT_POISON=1` for freed
//! objects to be poisoned (D-458).

use cheby_codegen_clif::CodegenOptions;
use cheby_mir::{FunctionBuilder, LayoutRef, Local};
use cheby_runtime::abi::PanicKind as RtPanicKind;

use super::*;

/// The strings of [`panicking`]'s body: its location and message.
const BOOM: [&str; 2] = [LOC, "boom"];

/// `app::g()`, which panics with "boom". Index 1.
fn panicking(p: &mut Prog) -> DefId {
    p.item(
        1,
        "g",
        &[],
        &[],
        vec![block(
            Vec::new(),
            Terminator::Panic {
                kind: PanicKind::Explicit,
                message: Some(Operand::Const(Const::Str(StrId::from_u32(2)))),
                loc: StrId::from_u32(1),
            },
        )],
        &BOOM,
    )
}

/// `app::f`'s def: index 0.
fn owner() -> DefId {
    def(1, 0)
}

/// A Cheby-ABI function of `key` returning a uniform word.
fn builder(key: FnKey) -> FunctionBuilder {
    let mut b = FunctionBuilder::new(key, Abi::Cheby, Repr::Value, span()).unwrap();
    b.set_entry_loc("test.cheby:1:1").unwrap();
    b
}

/// The continuation `index` of `app::f`.
fn cont(index: u32) -> FnKey {
    FnKey::Continuation {
        owner: owner(),
        index,
    }
}

/// Field `k`'s value: every 97th a new pair `(Nil, Nil)` on the heap, so
/// a missing release shows in the domain, else the tagged `k`.
fn field_value(b: &mut FunctionBuilder, k: u32) -> Local {
    if k.checked_rem(97) == Some(0) {
        b.assign(
            Repr::Value,
            Rvalue::Alloc {
                layout: LayoutRef::Tuple(2),
                fields: vec![Operand::Const(Const::Nil), Operand::Const(Const::Nil)],
            },
        )
        .unwrap()
    } else {
        b.assign(
            Repr::Value,
            Rvalue::Box {
                value: Operand::Const(Const::Int(i64::from(k))),
                from: Repr::Int,
            },
        )
        .unwrap()
    }
}

/// Stores fields `range` of `obj`, a tuple of `layout`; field `panic_at`
/// is the result of calling `g`, which panics.
fn fill(
    b: &mut FunctionBuilder,
    obj: Local,
    layout: LayoutRef,
    range: std::ops::Range<u32>,
    panic_at: Option<(u32, DefId)>,
) {
    for k in range {
        let value = match panic_at {
            Some((at, g)) if at == k => b
                .assign(
                    Repr::Value,
                    Rvalue::Call {
                        callee: Callee::Direct(g),
                        args: Vec::new(),
                    },
                )
                .unwrap(),
            _ => field_value(b, k),
        };
        b.push(Stmt::Eval(Rvalue::InitField {
            obj,
            layout,
            field: k,
            value: Operand::Local(value),
        }))
        .unwrap();
    }
}

/// `app::f()` building a tuple of `arity` fields in `pieces` functions
/// (the owner, then continuations, each taking the partly filled tuple),
/// field `panic_at` (if any) calling `g`, which panics.
fn tuple_program(arity: u32, pieces: u32, panic_at: Option<u32>) -> (Prog, DefId) {
    let mut prog = Prog::default();
    let boom = panicking(&mut prog);
    let layout = LayoutRef::Tuple(arity);
    let per = arity.div_ceil(pieces);
    let at_piece = |n: u32| n.checked_mul(per).unwrap().min(arity);
    let range = |n: u32| at_piece(n)..at_piece(n.checked_add(1).unwrap());
    let at = panic_at.map(|k| (k, boom));
    let mut body = builder(FnKey::Item(owner()));
    let tuple = body
        .assign(Repr::Value, Rvalue::AllocPlaceholder { layout })
        .unwrap();
    fill(&mut body, tuple, layout, range(0), at);
    let result = if pieces > 1 {
        let called = body
            .assign(
                Repr::Value,
                Rvalue::Call {
                    callee: Callee::Code(cont(0)),
                    args: vec![Operand::Local(tuple)],
                },
            )
            .unwrap();
        Operand::Local(called)
    } else {
        Operand::Local(tuple)
    };
    body.terminate(Terminator::Return(result)).unwrap();
    prog.functions.push(body.finish().unwrap());
    prog.declare(owner(), "f", &[]);
    for piece in 1..pieces {
        let mut rest = builder(cont(piece.checked_sub(1).unwrap()));
        let partial = rest.add_param(Repr::Value, None, None).unwrap();
        fill(&mut rest, partial, layout, range(piece), at);
        let term = if piece.checked_add(1).unwrap() < pieces {
            Terminator::TailCall {
                callee: Callee::Code(cont(piece)),
                args: vec![Operand::Local(partial)],
            }
        } else {
            Terminator::Return(Operand::Local(partial))
        };
        rest.terminate(term).unwrap();
        prog.functions.push(rest.finish().unwrap());
    }
    let target = prog.call(owner(), Vec::new());
    (prog, target)
}

/// Counts both units with the pipeline every build uses, checks them, and
/// compiles them with the checked count operations.
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

/// The panic of `g`, with nothing of the domain left alive.
fn assert_boom(jit: &Jit, target: DefId) {
    let outcome = run_const(jit, target);
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("the entry returned: {outcome:?}");
    };
    assert_eq!(report.kind, RtPanicKind::Explicit, "{report:?}");
    assert_eq!(report.message, "boom");
    assert_nothing_live(jit);
}

/// The fields of the tuple `word` of `arity` fields hold what
/// [`field_value`] stored; the escaped count of a tuple above 254 fields
/// comes first (D-279, D-555).
fn assert_fields(word: u64, arity: u32) {
    let escaped = arity > u32::from(abi::SCAN_MAX);
    let first = if escaped {
        assert_eq!(
            read_word(word, 8),
            abi::tag_int(i64::from(arity)).unwrap(),
            "the escaped count"
        );
        16
    } else {
        8
    };
    for k in 0..arity {
        let at = usize::try_from(k)
            .unwrap()
            .checked_mul(8)
            .and_then(|n| n.checked_add(first))
            .unwrap();
        let field = read_word(word, at);
        if k.checked_rem(97) == Some(0) {
            assert_eq!(layout_of(field), abi::layout::TUPLE, "field {k}");
        } else {
            assert_eq!(field, abi::tag_int(i64::from(k)).unwrap(), "field {k}");
        }
    }
}

/// A 100-field tuple filled one field at a time: every field is stored
/// where the layout puts it, and releasing the tuple releases its pairs.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn literal_tuple_filled_by_field() {
    for arity in [100, 300] {
        let (p, target) = tuple_program(arity, 1, None);
        let (jit, text) = compile_counted(p);
        assert!(
            text.contains(&format!("alloc_placeholder tuple/{arity}")),
            "{text}"
        );
        let word = returned(run_const(&jit, target));
        assert_eq!(layout_of(word), abi::layout::TUPLE);
        assert_fields(word, arity);
        release(&jit, word);
        assert_nothing_live(&jit);
    }
}

/// A panic in the middle of a 100-field tuple: the call of field 50
/// unwinds to the pad that drops the tuple with its 49 stored fields and
/// its placeholders.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn literal_tuple_panic_in_the_middle_leaks_nothing() {
    let (p, target) = tuple_program(100, 1, Some(50));
    let (jit, text) = compile_counted(p);
    assert!(text.contains("call app::g() ; unwind bb"), "{text}");
    assert_boom(&jit, target);
}

/// A tuple built by the owner and four continuations, each taking the
/// partly filled tuple: the whole tuple comes back filled.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn literal_tuple_in_continuations() {
    let (p, target) = tuple_program(500, 5, None);
    let (jit, text) = compile_counted(p);
    assert!(
        text.contains("tail_call <continuation app::f#1>(_0)"),
        "{text}"
    );
    let word = returned(run_const(&jit, target));
    assert_fields(word, 500);
    release(&jit, word);
    assert_nothing_live(&jit);
}

/// A panic in continuation 2 of a 500-field tuple: that continuation's
/// pad drops the partly filled tuple, and the owner's call releases
/// nothing more.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn literal_tuple_panic_in_continuation_k_leaks_nothing() {
    let (p, target) = tuple_program(500, 5, Some(250));
    let (jit, _) = compile_counted(p);
    assert_boom(&jit, target);
}

/// A panic in continuation 57 of a 100k-field tuple in 100 pieces
/// (D-780's tuples above the threshold are split too).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn literal_huge_tuple_panic_in_continuation_k_leaks_nothing() {
    let arity = 100_000;
    let (p, target) = tuple_program(arity, 100, Some(57_500));
    let (jit, _) = compile_counted(p);
    assert_boom(&jit, target);
    // And without the panic, every field arrives.
    let (p, target) = tuple_program(arity, 100, None);
    let (jit, _) = compile_counted(p);
    let word = returned(run_const(&jit, target));
    assert_fields(word, arity);
    release(&jit, word);
    assert_nothing_live(&jit);
}

/// `app::f(n)` building a list of `count` pairs in `pieces` functions
/// and returning its length: each continuation takes the list and `n`,
/// pushes its share and tail-calls the next; element `panic_at` (if any)
/// calls `g`.
fn list_program(count: u32, pieces: u32, panic_at: Option<u32>) -> (Prog, DefId) {
    let mut prog = Prog::default();
    let boom = panicking(&mut prog);
    let at = panic_at.map(|k| (k, boom));
    let per = count.div_ceil(pieces);
    let mut body = builder(FnKey::Item(owner()));
    let number = body.add_param(Repr::Int, None, None).unwrap();
    let mut acc = Operand::Const(Const::EmptyList);
    for k in 0..per.min(count) {
        acc = Operand::Local(push_pair(&mut body, acc, number, k, at));
    }
    let list = body
        .assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Code(cont(0)),
                args: vec![acc, Operand::Local(number)],
            },
        )
        .unwrap();
    // The list's length, boxed; the list is released after it.
    let length = body
        .assign(
            Repr::Int,
            Rvalue::CallRuntime {
                func: RuntimeFn::ListLength,
                args: vec![Operand::Local(list)],
            },
        )
        .unwrap();
    let boxed = body
        .assign(
            Repr::Value,
            Rvalue::Box {
                value: Operand::Local(length),
                from: Repr::Int,
            },
        )
        .unwrap();
    body.terminate(Terminator::Return(Operand::Local(boxed)))
        .unwrap();
    prog.functions.push(body.finish().unwrap());
    prog.declare(owner(), "f", &[Repr::Int]);
    let at_piece = |piece: u32| piece.checked_mul(per).unwrap().min(count);
    for piece in 1..pieces {
        let mut rest = builder(cont(piece.checked_sub(1).unwrap()));
        let list = rest.add_param(Repr::Value, None, None).unwrap();
        let number = rest.add_param(Repr::Int, None, None).unwrap();
        let mut acc = Operand::Local(list);
        for k in at_piece(piece)..at_piece(piece.checked_add(1).unwrap()) {
            acc = Operand::Local(push_pair(&mut rest, acc, number, k, at));
        }
        let term = if piece.checked_add(1).unwrap() < pieces {
            Terminator::TailCall {
                callee: Callee::Code(cont(piece)),
                args: vec![acc, Operand::Local(number)],
            }
        } else {
            Terminator::Return(acc)
        };
        rest.terminate(term).unwrap();
        prog.functions.push(rest.finish().unwrap());
    }
    let target = prog.call(owner(), vec![Operand::Const(Const::Int(7))]);
    (prog, target)
}

/// Pushes element `k` onto `list`: the pair `(number, Nil)`, or at
/// `panic_at` the result of calling `g`, which panics.
fn push_pair(
    body: &mut FunctionBuilder,
    list: Operand,
    number: Local,
    k: u32,
    panic_at: Option<(u32, DefId)>,
) -> Local {
    let elem = match panic_at {
        Some((at, boom)) if at == k => body
            .assign(
                Repr::Value,
                Rvalue::Call {
                    callee: Callee::Direct(boom),
                    args: Vec::new(),
                },
            )
            .unwrap(),
        _ => {
            let boxed = body
                .assign(
                    Repr::Value,
                    Rvalue::Box {
                        value: Operand::Local(number),
                        from: Repr::Int,
                    },
                )
                .unwrap();
            body.assign(
                Repr::Value,
                Rvalue::Alloc {
                    layout: LayoutRef::Tuple(2),
                    fields: vec![Operand::Local(boxed), Operand::Const(Const::Nil)],
                },
            )
            .unwrap()
        }
    };
    body.assign(
        Repr::Value,
        Rvalue::CallRuntime {
            func: RuntimeFn::ListPush,
            args: vec![list, Operand::Local(elem)],
        },
    )
    .unwrap()
}

/// A list built through four continuations comes back whole.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn literal_list_in_continuations() {
    let (p, target) = list_program(400, 5, None);
    let (jit, text) = compile_counted(p);
    assert!(
        text.contains("fn continuation app::f#3(_0: value, _1: int)"),
        "{text}"
    );
    let word = returned(run_const(&jit, target));
    assert_eq!(int_of(word), 400);
    assert_nothing_live(&jit);
}

/// A panic in continuation 3 of a list: its pad releases the list built
/// so far, with every pair in it.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn literal_list_panic_in_continuation_k_leaks_nothing() {
    for at in [10, 290, 399] {
        let (p, target) = list_program(400, 5, Some(at));
        let (jit, _) = compile_counted(p);
        assert_boom(&jit, target);
    }
}
