//! A record's functions in one span, and direct calls that reach (K4d;
//! D-914, D-922; D-1258; `memory.rs`'s module docs): a
//! batch whose code is larger than aarch64's `bl` reaches loads and runs,
//! with a direct call inside the first record and inside the last that
//! each crosses tens of MiB of code, and each record's functions lie in one
//! span no longer than the code run the loader asked for; a record whose
//! code alone is larger than the reach is an internal error naming its
//! module.

use super::*;

/// The padding put between a record's caller and its callee: never run.
const PAD: usize = 70 << 20;

/// `<path>::run` (index 0) calls `<path>::far` (index 1) directly, as code
/// calls its own unit's generated functions (`Callee::Code`); `far`
/// returns the static `True` or `False`.
fn caller_unit(module: u32, path: &str, value: bool) -> MirModule {
    let run = def(module, 0);
    let far = def(module, 1);
    let run_body = function(
        FnKey::Item(run),
        Abi::Cheby,
        &[],
        &[Repr::Value],
        Repr::Value,
        vec![block(
            vec![assign(
                0,
                Rvalue::Call {
                    callee: Callee::Code(FnKey::Item(far)),
                    args: Vec::new(),
                },
            )],
            ret(0),
        )],
        &[],
    );
    let ctor = if value { builtin::TRUE } else { builtin::FALSE };
    let far_body = function(
        FnKey::Item(far),
        Abi::Cheby,
        &[],
        &[],
        Repr::Value,
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::StaticCtor(ctor))),
        )],
        &[],
    );
    code_unit(
        module,
        path,
        vec![run_body, far_body],
        vec![
            CalleeDecl {
                def: run,
                sig: cheby_sig(&[]),
            },
            CalleeDecl {
                def: far,
                sig: cheby_sig(&[]),
            },
        ],
        vec![
            name(run, path, "run"),
            name(far, path, "far"),
            name(builtin::TRUE, "", "True"),
            name(builtin::FALSE, "", "False"),
        ],
    )
}

/// What `<module>::run` returns: `True` for `a`, `False` for `c`.
fn result_of(module: u32) -> u64 {
    if module == 1 {
        static_word(abi::layout::TRUE)
    } else {
        static_word(abi::layout::FALSE)
    }
}

/// The constant of module `module` whose thunk calls its `run`.
fn target_of(module: u32) -> DefId {
    def(module, 10_000)
}

/// The entries unit: one constant thunk per caller module.
fn entries(modules: &[(u32, &str)]) -> MirModule {
    let mut thunks = Vec::new();
    let mut callees = Vec::new();
    let mut names = Vec::new();
    for &(module, path) in modules {
        let run = def(module, 0);
        thunks.push(const_thunk(target_of(module), run, Vec::new()));
        callees.push(CalleeDecl {
            def: run,
            sig: cheby_sig(&[]),
        });
        names.push(name(run, path, "run"));
        names.push(name(target_of(module), path, "VALUE"));
    }
    entries_unit(thunks, callees, names)
}

/// Bytes of code that trap if ever run.
fn traps(len: usize) -> Vec<u8> {
    let word: &[u8] = if cfg!(target_arch = "aarch64") {
        // `brk #0`.
        &[0x00, 0x00, 0x20, 0xd4]
    } else {
        // `int3`.
        &[0xcc]
    };
    word.iter().copied().cycle().take(len).collect()
}

/// `record` with a function of `pad` trapping bytes put between `run` and
/// `far`, so `run`'s direct call of `far` crosses it.
fn padded(record: &CodeRecord, path: &str, pad: usize) -> CodeRecord {
    let mut out = record.clone();
    let at_of = |item: &str| {
        let symbol = format!("cb$fn${path}${item}");
        out.functions
            .iter()
            .position(|f| out.symbol_at(f.symbol).is_some_and(|s| s.name == symbol))
            .unwrap()
    };
    let (run, far) = (at_of("run"), at_of("far"));
    let symbol = u32::try_from(out.symbols.len()).unwrap();
    out.symbols.push(cheby_codegen_clif::record::RecordSymbol {
        name: format!("cb$pad${path}"),
        kind: cheby_codegen_clif::record::SymbolKind::Function,
        linkage: cheby_codegen_clif::record::SymbolLinkage::Local,
    });
    out.functions.insert(
        run.max(far),
        cheby_codegen_clif::record::RecordFunction {
            symbol,
            align: 16,
            bytes: traps(pad),
            relocs: Vec::new(),
            positions: Vec::new(),
        },
    );
    out
}

/// The records of `a` and `c`, compiled alone in a JIT of their own.
fn records() -> (MirModule, MirModule, CodeRecord, CodeRecord) {
    let a = caller_unit(1, "a", true);
    let c = caller_unit(3, "c", false);
    let e = entries(&[(1, "a"), (3, "c")]);
    let mut jit = Jit::new().unwrap();
    let fresh = jit
        .compile_units(
            &[BatchUnit::Mir(&a), BatchUnit::Mir(&c), BatchUnit::Mir(&e)],
            &BatchCx::default(),
        )
        .unwrap();
    // The direct calls run fresh too.
    check_results(&jit);
    let [Some(ra), Some(rc), None] = fresh.as_slice() else {
        panic!("not one record per code unit: {fresh:?}");
    };
    (a, c, ra.clone(), rc.clone())
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn record_placement_batch_beyond_the_reach_runs() {
    let (a, c, ra, rc) = records();
    let (pa, pc) = (padded(&ra, "a", PAD), padded(&rc, "c", PAD));
    let e = entries(&[(1, "a"), (3, "c")]);
    let mut jit = Jit::new().unwrap();
    jit.compile_units(
        &[
            BatchUnit::Record {
                record: &pa,
                unit: a.unit,
                path: "a",
            },
            BatchUnit::Record {
                record: &pc,
                unit: c.unit,
                path: "c",
            },
            BatchUnit::Mir(&e),
        ],
        &BatchCx::default(),
    )
    .unwrap();
    let spans = jit.loaded.spans.clone();
    let lowest = spans.iter().map(|s| s.1).min().unwrap();
    let highest = spans.iter().map(|s| s.2).max().unwrap();
    // The batch's code is larger than `bl` reaches.
    assert!(highest - lowest > 128 << 20, "{spans:?}");
    // Each record's functions lie in one span: the run the loader asked for.
    for (path, low, high, run) in &spans {
        assert!(high - low <= *run, "{path}: {low:#x}..{high:#x} over {run}");
    }
    check_results(&jit);
}

/// Each caller's constant runs its `run`, whose direct call returns.
fn check_results(jit: &Jit) {
    for module in [1, 3] {
        assert_eq!(
            returned(run_const(jit, target_of(module))),
            result_of(module)
        );
    }
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn record_placement_spans_hold_with_data_between() {
    // Records loaded in two batches, each with its data: the second
    // batch's functions still lie in one span each.
    let (a, c, ra, rc) = records();
    let e = entries(&[(1, "a"), (3, "c")]);
    let mut jit = Jit::new().unwrap();
    jit.compile_units(
        &[BatchUnit::Record {
            record: &padded(&ra, "a", 1 << 20),
            unit: a.unit,
            path: "a",
        }],
        &BatchCx::default(),
    )
    .unwrap();
    jit.compile_units(
        &[
            BatchUnit::Record {
                record: &padded(&rc, "c", 3 << 20),
                unit: c.unit,
                path: "c",
            },
            BatchUnit::Mir(&e),
        ],
        &BatchCx::default(),
    )
    .unwrap();
    for (path, low, high, run) in &jit.loaded.spans {
        assert!(high - low <= *run, "{path}: {low:#x}..{high:#x} over {run}");
    }
    check_results(&jit);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn record_placement_record_beyond_the_reach_is_an_internal_error() {
    if load::CALL_REACH > 256 << 20 {
        // x86-64: a record beyond the arena's reach would not fit in it.
        return;
    }
    let (a, _, ra, _) = records();
    let big = padded(&ra, "a", load::CALL_REACH + 1);
    let mut jit = Jit::new().unwrap();
    let error = jit
        .compile_units(
            &[BatchUnit::Record {
                record: &big,
                unit: a.unit,
                path: "a",
            }],
            &BatchCx::default(),
        )
        .unwrap_err();
    let text = error.to_string();
    assert!(
        text.contains("`a`") && text.contains("more than a direct call reaches"),
        "{text}"
    );
    // Nothing of the batch was declared.
    assert!(!jit.declares("cb$pad$a"));
}
