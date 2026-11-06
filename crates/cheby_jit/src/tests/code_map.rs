//! The JIT's part of the code map (D-856, D-867): after each batch is
//! finalized, every function of its code units is registered, through
//! its module records and the one routine (D-919), with its address,
//! length, ABI and the return offsets of its located calls; the entries
//! unit's thunks are not (D-1111); a failed batch registers nothing; a
//! later batch adds ranges and moves none; a large frame's inner body
//! carries the sites (D-679). The last test runs a deadlocking program
//! and reads the location its report names.

use std::sync::Arc;

use cheby_mir::{CType, ForeignSig, LayoutRef};
use cheby_runtime::alloc::Domain;
use cheby_runtime::locate::CodeRangeDesc;
use cheby_runtime::{DetachedGroup, Runtime, RuntimeConfig};
use cranelift_codegen::control::ControlPlane;

use super::*;

const IDENT: u32 = 0;
const CALLER: u32 = 1;
const FIRST: u32 = 2;
const TAIL_ONLY: u32 = 3;

/// The two locations of `caller`'s calls.
const LOC_A: &str = "src/app.cheby:3:5";
const LOC_B: &str = "<repl:2>:4:7";

/// `f` with a location for each `(local, text)`: the call that defines
/// the local is located there.
fn located(mut f: Function, locs: &[(u32, &str)]) -> Function {
    for &(l, text) in locs {
        let id = f.call_loc_strings.push(text.into()).unwrap();
        f.call_locs.push((local(l), id));
    }
    f
}

fn call(callee: u32, args: &[u32]) -> Rvalue {
    Rvalue::Call {
        callee: Callee::Direct(def(1, callee)),
        args: args.iter().map(|&n| op(n)).collect(),
    }
}

/// `ident(x) = x`, `first(x, y) = x`, `caller(x)`, which calls `ident`
/// twice at [`LOC_A`] and [`LOC_B`] and tail-calls `first(x, _)`, keeping
/// `x` live across both calls, and `tail_only(x)`, which only tail-calls
/// `ident`; a thunk calls `caller`. Uncounted: the tests pass the static
/// `Nil`, which counting skips.
fn program() -> (Prog, DefId) {
    let mut p = Prog::default();
    p.item(
        IDENT,
        "ident",
        &[Repr::Value],
        &[],
        vec![block(Vec::new(), ret(0))],
        &[],
    );
    p.item(
        FIRST,
        "first",
        &[Repr::Value, Repr::Value],
        &[],
        vec![block(Vec::new(), ret(0))],
        &[],
    );
    p.item(
        CALLER,
        "caller",
        &[Repr::Value],
        &[Repr::Value, Repr::Value],
        vec![block(
            vec![assign(1, call(IDENT, &[0])), assign(2, call(IDENT, &[1]))],
            Terminator::TailCall {
                callee: Callee::Direct(def(1, FIRST)),
                args: vec![op(0), op(2)],
            },
        )],
        &[],
    );
    p.item(
        TAIL_ONLY,
        "tail_only",
        &[Repr::Value],
        &[],
        vec![block(
            Vec::new(),
            Terminator::TailCall {
                callee: Callee::Direct(def(1, IDENT)),
                args: vec![op(0)],
            },
        )],
        &[],
    );
    let caller = p
        .functions
        .iter_mut()
        .find(|f| f.key == FnKey::Item(def(1, CALLER)))
        .unwrap();
    *caller = located(caller.clone(), &[(1, LOC_A), (2, LOC_B)]);
    let target = p.call(def(1, CALLER), vec![Operand::Const(Const::Nil)]);
    (p, target)
}

/// The address of `f`'s code in `jit`.
fn start_of(jit: &Jit, f: DefId) -> usize {
    let id = jit.func_id(FnKey::Item(f)).unwrap();
    jit.module.get_finalized_function(id).expose_provenance()
}

/// The ranges the code map holds under `jit`'s owner, sorted by start.
fn registered(jit: &Jit) -> Vec<CodeRangeDesc> {
    cheby_runtime::locate::owner_ranges(jit.keep_alive.code_owner)
}

/// The range `jit` registered at `start`.
fn range_at(jit: &Jit, start: usize) -> CodeRangeDesc {
    let ranges = registered(jit);
    ranges
        .iter()
        .find(|r| r.start == start)
        .cloned()
        .unwrap_or_else(|| panic!("no range at {start:#x} in {ranges:#?}"))
}

/// The registered ranges are sorted by nothing in particular, but never
/// overlap.
fn assert_disjoint(ranges: &[CodeRangeDesc]) {
    let mut spans: Vec<(usize, usize)> = ranges
        .iter()
        .map(|r| {
            let end = r.start.checked_add(usize::try_from(r.len).unwrap());
            (r.start, end.unwrap())
        })
        .collect();
    spans.sort_unstable();
    for pair in spans.windows(2) {
        assert!(pair[0].1 <= pair[1].0, "overlapping ranges {pair:?}");
    }
}

/// Every return offset the JIT registered for `caller` is one of
/// Cranelift's call sites of the same function, in order, with its
/// location; the range has the function's address and length.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_map_offsets_are_return_addresses_of_calls() {
    let (p, target) = program();
    let units = p.units();
    let mut jit = Jit::new().unwrap();
    jit.compile(&units).unwrap();
    let range = range_at(&jit, start_of(&jit, def(1, CALLER)));
    assert!(range.cheby_abi);
    let locations: Vec<&str> = range.sites.iter().map(|s| &*s.location).collect();
    assert_eq!(locations, [LOC_A, LOC_B]);
    // The same function translated again, with the batch declared in a
    // module of its own, compiles to the same code; its call sites are
    // Cranelift's own.
    let cx = cheby_codegen_clif::CodegenCx {
        layouts: &cheby_codegen_clif::BuiltinLayouts,
    };
    let mut module = cheby_codegen_clif::record::RecordingModule::new(
        cheby_codegen_clif::isa::host_isa().unwrap(),
    );
    let mut decls = cheby_codegen_clif::Decls::new(module.isa()).unwrap();
    cheby_codegen_clif::declare(&mut module, &mut decls, &units, &cx, &jit.codegen).unwrap();
    let caller = units[0]
        .functions
        .iter()
        .find(|f| f.key == FnKey::Item(def(1, CALLER)))
        .unwrap();
    let func = cheby_codegen_clif::translate(&units[0], caller, &decls, &cx, &jit.codegen).unwrap();
    let mut ctx = cranelift_codegen::Context::for_function(func);
    ctx.compile(jit.module.isa(), &mut ControlPlane::default())
        .unwrap();
    let compiled = ctx.compiled_code().unwrap();
    assert_eq!(
        usize::try_from(range.len).unwrap(),
        compiled.code_buffer().len()
    );
    let returns: Vec<u32> = compiled.buffer.call_sites().map(|s| s.ret_addr).collect();
    let offsets: Vec<u32> = range.sites.iter().map(|s| s.ret_offset).collect();
    assert!(offsets.windows(2).all(|w| w[0] < w[1]), "{offsets:?}");
    for offset in &offsets {
        assert!(returns.contains(offset), "{offset} not in {returns:?}");
    }
    // The tail call is the last call site, and it is not recorded.
    assert!(!offsets.contains(returns.last().unwrap()), "{returns:?}");
    // The program still runs: `caller(Nil)` is `Nil`.
    assert_eq!(returned(run_const(&jit, target)), static_nil());
}

/// A function whose only call is a tail call has no site; every function
/// of the code unit is registered; the entry thunk, which the entries
/// unit defines and no module record lists, is in no range, as in an
/// executable (D-1111, D-1169).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_map_tail_calls_and_thunks_have_no_sites() {
    let (p, target) = program();
    let jit = p.compile();
    let tail_only = range_at(&jit, start_of(&jit, def(1, TAIL_ONLY)));
    assert!(
        tail_only.cheby_abi && tail_only.sites.is_empty(),
        "{tail_only:?}"
    );
    let ident = range_at(&jit, start_of(&jit, def(1, IDENT)));
    assert!(ident.cheby_abi && ident.sites.is_empty(), "{ident:?}");
    let thunk = jit.entry(EntryKind::Const(target)).unwrap().thunk;
    #[expect(clippy::as_conversions, reason = "a function's address")]
    let thunk = thunk as usize;
    let ranges = registered(&jit);
    assert!(
        ranges
            .iter()
            .all(|r| thunk < r.start || thunk >= r.start + usize::try_from(r.len).unwrap()),
        "{ranges:#?}"
    );
    assert_eq!(cheby_runtime::locate::range_at(thunk), None);
    // Four functions, at least, and no overlap.
    assert!(ranges.len() >= 4, "{ranges:#?}");
    assert_disjoint(&ranges);
}

/// `extra(x)` with index `index`, which runs `stmts` and returns `x`, and
/// a thunk calling it; its thunk's target is numbered from `base`.
fn extra_batch(index: u32, base: u32, stmts: Vec<Stmt>) -> Prog {
    let mut p = Prog {
        thunk_base: base,
        ..Prog::default()
    };
    let f = p.item(
        index,
        "extra",
        &[Repr::Value],
        &[Repr::Value],
        vec![block(stmts, ret(0))],
        &[],
    );
    p.call(f, vec![Operand::Const(Const::Nil)]);
    p
}

/// A second batch adds its ranges after the first's and changes none of
/// them, as a REPL session's batches do (D-712): the first batch's code
/// stays where it was.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_map_second_batch_adds_ranges_and_moves_none() {
    let (p, _) = program();
    let mut jit = p.compile();
    let first = registered(&jit);
    jit.compile(&extra_batch(20, 100, Vec::new()).units())
        .unwrap();
    let now = registered(&jit);
    assert!(now.len() > first.len());
    for range in &first {
        assert!(now.contains(range), "{range:?} moved");
    }
    assert_disjoint(&now);
    let extra = range_at(&jit, start_of(&jit, def(1, 20)));
    assert!(extra.cheby_abi && extra.sites.is_empty());
}

/// A batch that fails registers nothing, and the corrected batch does.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_map_failed_batch_adds_none() {
    let (p, _) = program();
    let mut jit = p.compile();
    let before = registered(&jit);
    // An unresolved native symbol fails the batch (D-532).
    let failing = extra_batch(
        25,
        200,
        vec![Stmt::Eval(Rvalue::CallForeign {
            symbol: "cheby_test_code_map_missing".into(),
            sig: ForeignSig {
                params: Vec::new(),
                ret: CType::Void,
            },
            args: Vec::new(),
            blocking: false,
        })],
    );
    let err = jit.compile(&failing.units()).unwrap_err();
    assert!(matches!(err, JitError::UnresolvedSymbols(_)), "{err}");
    assert_eq!(registered(&jit), before);
    // A batch whose code generation fails registers nothing either: a
    // call of a function no unit declares.
    let broken = extra_batch(
        30,
        300,
        vec![assign(
            1,
            Rvalue::Call {
                callee: Callee::Direct(def(1, 31)),
                args: Vec::new(),
            },
        )],
    );
    assert!(jit.compile(&broken.units()).is_err());
    assert_eq!(registered(&jit), before);
    jit.compile(&extra_batch(40, 400, Vec::new()).units())
        .unwrap();
    assert!(registered(&jit).len() > before.len());
}

/// A large frame (D-679): the outer function, which tail-calls its inner
/// body, has no site; the inner body has the calls' sites.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_map_large_frame_inner_body_has_the_sites() {
    let (p, target) = program();
    let mut jit = Jit::with_options(CodegenOptions {
        large_frame_threshold: 0,
        capture_clif: true,
        ..CodegenOptions::default()
    })
    .unwrap();
    jit.compile(&p.units()).unwrap();
    let clif = jit.take_clif().join("\n");
    assert!(clif.contains("; cb$big$fn$app$caller\n"), "{clif}");
    let outer = range_at(&jit, start_of(&jit, def(1, CALLER)));
    assert!(outer.cheby_abi && outer.sites.is_empty(), "{outer:?}");
    let ranges = registered(&jit);
    let inner: Vec<&CodeRangeDesc> = ranges.iter().filter(|r| r.sites.len() == 2).collect();
    assert_eq!(inner.len(), 1, "{ranges:#?}");
    assert!(inner[0].cheby_abi);
    assert_ne!(inner[0].start, outer.start);
    let locations: Vec<&str> = inner[0].sites.iter().map(|s| &*s.location).collect();
    assert_eq!(locations, [LOC_A, LOC_B]);
    assert_eq!(returned(run_const(&jit, target)), static_nil());
}

/// The location of `waits`'s receive in the program below.
const RECEIVE_AT: &str = "src/app.cheby:5:22";

/// A deadlocking program: `chan()` makes a channel of capacity 0
/// (`cheby_channel_new`), `recv(rx)` is the `@std_context` wrapper of
/// `cheby_channel_receive` (a Cheby function without call locations, as
/// std's), and `waits()` receives at [`RECEIVE_AT`] while it holds the
/// only `Sender`, so its fiber waits for good.
fn deadlocking() -> (Prog, DefId) {
    let mut p = Prog::default();
    let ptr_sig = |params: Vec<CType>| ForeignSig {
        params,
        ret: CType::Ptr,
    };
    p.item(
        0,
        "recv",
        &[Repr::Value],
        &[Repr::Value],
        vec![block(
            vec![assign(
                1,
                Rvalue::CallForeign {
                    symbol: "cheby_channel_receive".into(),
                    sig: ptr_sig(vec![CType::Ptr, CType::Ptr]),
                    args: vec![Operand::Const(Const::Ctx), op(0)],
                    blocking: false,
                },
            )],
            ret(1),
        )],
        &[],
    );
    p.item(
        1,
        "chan",
        &[],
        &[Repr::Value],
        vec![block(
            vec![assign(
                0,
                Rvalue::CallForeign {
                    symbol: "cheby_channel_new".into(),
                    sig: ptr_sig(vec![CType::Ptr, CType::I64]),
                    args: vec![Operand::Const(Const::Ctx), Operand::Const(Const::Int(0))],
                    blocking: false,
                },
            )],
            ret(0),
        )],
        &[],
    );
    let field = |base: u32, field: u32| Rvalue::Field {
        base: local(base),
        layout: LayoutRef::Tuple(2),
        field,
    };
    let waits = p.item(
        2,
        "waits",
        &[],
        &[Repr::Value; 5],
        vec![block(
            vec![
                assign(0, call(1, &[])),
                assign(1, field(0, 0)),
                assign(2, field(0, 1)),
                assign(3, call(0, &[2])),
                // The `Sender` stays live across the receive.
                assign(
                    4,
                    Rvalue::Alloc {
                        layout: LayoutRef::Tuple(2),
                        fields: vec![op(3), op(1)],
                    },
                ),
            ],
            ret(4),
        )],
        &[],
    );
    let w = p
        .functions
        .iter_mut()
        .find(|f| f.key == FnKey::Item(waits))
        .unwrap();
    *w = located(w.clone(), &[(3, RECEIVE_AT)]);
    let target = p.call(waits, Vec::new());
    (p, target)
}

/// End to end (after the detector's packages are done): a root that
/// deadlocks in a receive, run through the JIT on a runtime with deadlock
/// detection on (every shipped host's default), fails with a report that
/// names the receive's location, found through the wait-frame word, the
/// wrapper's frame record and this JIT's code map (D-856, D-858). In test
/// mode the deadlocked root is cancelled and unwinds (D-859).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_map_locates_a_blocked_receive() {
    let (p, target) = deadlocking();
    let [mut code, mut entries] = p.units();
    cheby_mir::count_and_check(&mut code).unwrap();
    cheby_mir::count_and_check(&mut entries).unwrap();
    let mut jit = Jit::new().unwrap();
    jit.compile(&[code, entries]).unwrap();
    for threads in [1, 2] {
        let runtime = Runtime::new(&RuntimeConfig {
            threads: Some(threads),
            ..RuntimeConfig::default()
        })
        .unwrap();
        let domain: Arc<Domain> = Domain::new();
        let config = EntryConfig {
            stack_size: 1 << 20,
            domain: Arc::clone(&domain),
            ..EntryConfig::new(EntryMode::Test)
        };
        let group = DetachedGroup::new();
        let entry = jit.entry(EntryKind::Const(target)).unwrap();
        let outcome = entry
            .start_root_in(&runtime, &config, &group)
            .unwrap()
            .wait()
            .unwrap();
        let EntryOutcome::Deadlocked(report) = outcome else {
            panic!("the root did not deadlock: {outcome:?}");
        };
        assert_eq!(
            report,
            format!(
                "fatal error: all fibers are blocked\n  blocked in channel::receive at {RECEIVE_AT}\n"
            ),
            "at {threads} threads"
        );
        runtime.shutdown();
        if cheby_runtime::LEAK_CHECK_ENABLED {
            assert_eq!(domain.live_objects(), 0, "at {threads} threads");
        }
    }
}
