//! Direct calls between Cheby functions in release code: a call reaches
//! its callee's symbol, not its slot (`CallMode::Direct`, D-916's
//! reconsideration), across units, records and batches; the symbol carries
//! the callee's borrow mask, so a caller compiled against another mask
//! fails to resolve (D-917, D-1109); and a JIT refuses a record of the
//! other profile.

use cheby_codegen_clif::record::{RecordFunction, RecordSymbol, SymbolKind, SymbolLinkage};
use cheby_codegen_clif::{CallMode, OptLevel};
use cheby_mir::Profile;
use cheby_mir::borrow::BorrowMask;

use super::*;

/// The code generation of a release build.
pub(super) fn release() -> CodegenOptions {
    CodegenOptions {
        call_mode: CallMode::Direct,
        opt_level: OptLevel::Speed,
        profile: Profile::Release,
        ..CodegenOptions::default()
    }
}

/// A release JIT.
fn release_jit() -> Jit {
    Jit::configured(release(), JitOptions::from_env()).unwrap()
}

/// The mask that borrows parameter 0.
fn first() -> BorrowMask {
    BorrowMask::OWNED.with(0)
}

/// `app::a::g(x)`, which returns `True` and borrows `x` under `borrow`.
fn callee_unit(borrow: BorrowMask) -> MirModule {
    let g = def(1, 0);
    let mut body = function(
        FnKey::Item(g),
        Abi::Cheby,
        &[Repr::Value],
        &[],
        Repr::Value,
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::StaticCtor(builtin::TRUE))),
        )],
        &[],
    );
    body.borrow = borrow;
    let mut sig = cheby_sig(&[Repr::Value]);
    sig.borrow = borrow;
    code_unit(
        1,
        "app::a",
        vec![body],
        vec![CalleeDecl { def: g, sig }],
        vec![name(g, "app::a", "g"), name(builtin::TRUE, "", "True")],
    )
}

/// `app::b::h()`, which tail-calls `app::a::g(Nil)` declared with the mask
/// `borrow`, and the entries unit whose constant runs `h`.
fn caller_units(borrow: BorrowMask) -> (MirModule, MirModule, DefId) {
    let g = def(1, 0);
    let h = def(2, 0);
    let target = def(2, 1);
    let call = if borrow.is_owned() {
        Callee::Direct(g)
    } else {
        Callee::Borrowing(g, borrow)
    };
    let h_fn = function(
        FnKey::Item(h),
        Abi::Cheby,
        &[],
        &[],
        Repr::Value,
        vec![block(
            Vec::new(),
            Terminator::TailCall {
                callee: call,
                args: vec![Operand::Const(Const::Nil)],
            },
        )],
        &[],
    );
    let mut g_sig = cheby_sig(&[Repr::Value]);
    g_sig.borrow = borrow;
    let calling = code_unit(
        2,
        "app::b",
        vec![h_fn],
        vec![
            CalleeDecl { def: g, sig: g_sig },
            CalleeDecl {
                def: h,
                sig: cheby_sig(&[]),
            },
        ],
        vec![name(g, "app::a", "g"), name(h, "app::b", "h")],
    );
    let entries = entries_unit(
        vec![const_thunk(target, h, Vec::new())],
        vec![CalleeDecl {
            def: h,
            sig: cheby_sig(&[]),
        }],
        vec![name(h, "app::b", "h"), name(target, "app::b", "VALUE")],
    );
    (calling, entries, target)
}

/// The names of the symbols of `record` with `linkage`.
fn symbols(record: &CodeRecord, linkage: SymbolLinkage) -> Vec<&str> {
    record
        .symbols
        .iter()
        .filter(|s| s.linkage == linkage)
        .map(|s| s.name.as_str())
        .collect()
}

/// A release record imports its callee's function, masked as its slot
/// would be, and no slot; the callee's record defines that name and still
/// defines its slot. The call runs, across units of one batch and of two.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn direct_calls_import_the_masked_function() {
    let defining = callee_unit(first());
    let (calling, entries, target) = caller_units(first());
    let mut jit = release_jit();
    let records = jit
        .compile_units(
            &[
                BatchUnit::Mir(&defining),
                BatchUnit::Mir(&calling),
                BatchUnit::Mir(&entries),
            ],
            &BatchCx::default(),
        )
        .unwrap();
    assert_eq!(
        run_const(&jit, target),
        EntryOutcome::Returned(static_word(abi::layout::TRUE))
    );
    let [Some(a), Some(b), None] = records.as_slice() else {
        panic!("not one record per code unit: {records:?}");
    };
    assert_eq!(a.profile, Profile::Release);
    let defined = symbols(a, SymbolLinkage::Hidden);
    assert!(defined.contains(&"cb$fn$app$a$g$$b1"), "{defined:?}");
    assert!(defined.contains(&"cb$slot$app$a$g$$b1"), "{defined:?}");
    let imports = symbols(b, SymbolLinkage::Import);
    assert!(imports.contains(&"cb$fn$app$a$g$$b1"), "{imports:?}");
    assert!(
        !imports.iter().any(|s| s.starts_with("cb$slot$")),
        "{imports:?}"
    );
    // Text holds only the PC-relative kinds of D-913: no absolute
    // relocation reaches a function's code.
    for record in [a, b] {
        for function in &record.functions {
            assert!(
                function
                    .relocs
                    .iter()
                    .all(|r| r.kind != cranelift_codegen::binemit::Reloc::Abs8),
                "{record:?}"
            );
        }
    }
    // A caller in a later batch reaches the earlier batch's callee.
    let mut later = release_jit();
    later.compile(std::slice::from_ref(&defining)).unwrap();
    later.compile(&[calling, entries]).unwrap();
    assert_eq!(
        run_const(&later, target),
        EntryOutcome::Returned(static_word(abi::layout::TRUE))
    );
}

/// A caller compiled against another mask than its callee's names another
/// symbol, so its batch fails to resolve instead of calling with the wrong
/// convention (D-917, D-1109), as a debug caller's slot does.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn direct_calls_with_a_stale_mask_fail_to_resolve() {
    for (callee, caller) in [(BorrowMask::OWNED, first()), (first(), BorrowMask::OWNED)] {
        let mut jit = release_jit();
        jit.compile(&[callee_unit(callee)]).unwrap();
        let (stale, entries, _) = caller_units(caller);
        let error = jit.compile(&[stale, entries]).unwrap_err().to_string();
        assert!(
            error.contains("is defined by no loaded record") && error.contains("cb$fn$app$a$g"),
            "{error}"
        );
    }
}

/// A JIT loads only records of its own profile: a debug record in a
/// release batch, or a release record in a debug one, is an internal
/// error, and nothing of the batch is loaded.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_batch_of_mixed_profiles_is_refused() {
    let defining = callee_unit(BorrowMask::OWNED);
    let record_of = |jit: &mut Jit| {
        jit.compile_units(&[BatchUnit::Mir(&defining)], &BatchCx::default())
            .unwrap()
            .remove(0)
            .unwrap()
    };
    let debug = record_of(&mut Jit::new().unwrap());
    let release = record_of(&mut release_jit());
    assert_eq!(debug.profile, Profile::Debug);
    assert_eq!(release.profile, Profile::Release);
    for (mut jit, record, wrong) in [
        (
            release_jit(),
            &debug,
            "a batch of release code holds a debug record of `app::a`",
        ),
        (
            Jit::new().unwrap(),
            &release,
            "a batch of debug code holds a release record of `app::a`",
        ),
    ] {
        let error = jit
            .compile_units(
                &[BatchUnit::Record {
                    record,
                    unit: defining.unit,
                    path: "app::a",
                }],
                &BatchCx::default(),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains(wrong), "{error}");
        assert!(!jit.defines("cb$fn$app$a$g"));
    }
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

/// `record` with a local function `cb$pad$<tag>` of `pad` trapping bytes
/// put at `at` among its functions.
fn padded(record: &CodeRecord, tag: &str, at: usize, pad: usize) -> CodeRecord {
    let mut out = record.clone();
    let symbol = u32::try_from(out.symbols.len()).unwrap();
    out.symbols.push(RecordSymbol {
        name: "cb$pad$".to_owned() + tag,
        kind: SymbolKind::Function,
        linkage: SymbolLinkage::Local,
    });
    out.functions.insert(
        at.min(out.functions.len()),
        RecordFunction {
            symbol,
            align: 16,
            bytes: traps(pad),
            relocs: Vec::new(),
            positions: Vec::new(),
        },
    );
    out
}

/// A direct call from one record to another across more code than
/// aarch64's `bl` reaches still runs: the callee's code sits before 70 MiB
/// of padding at the end of its record, and the caller's after 70 MiB at
/// the start of its own, so the call goes through cranelift-jit's veneer.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_direct_call_between_records_beyond_the_reach_runs() {
    const PAD: usize = 70 << 20;
    let defining = callee_unit(BorrowMask::OWNED);
    let (calling, entries, target) = caller_units(BorrowMask::OWNED);
    let mut fresh = release_jit();
    let records = fresh
        .compile_units(
            &[BatchUnit::Mir(&defining), BatchUnit::Mir(&calling)],
            &BatchCx::default(),
        )
        .unwrap();
    let [Some(a), Some(b)] = records.as_slice() else {
        panic!("not one record per code unit: {records:?}");
    };
    let a = padded(a, "a", usize::MAX, PAD);
    let b = padded(b, "b", 0, PAD);
    let mut jit = release_jit();
    jit.compile_units(
        &[
            BatchUnit::Record {
                record: &a,
                unit: defining.unit,
                path: "app::a",
            },
            BatchUnit::Record {
                record: &b,
                unit: calling.unit,
                path: "app::b",
            },
            BatchUnit::Mir(&entries),
        ],
        &BatchCx::default(),
    )
    .unwrap();
    let spans = jit.loaded.spans.clone();
    let lowest = spans.iter().map(|s| s.1).min().unwrap();
    let highest = spans.iter().map(|s| s.2).max().unwrap();
    assert!(highest - lowest > 128 << 20, "{spans:?}");
    assert_eq!(
        run_const(&jit, target),
        EntryOutcome::Returned(static_word(abi::layout::TRUE))
    );
}
