//! Tests of [`Entry::start_root_in`] and [`Entry::start_root_notify`]:
//! the root charges the caller's accounting domain (a per-test domain,
//! D-691, D-723), while [`Entry::run_root_in`] still charges the JIT's.
//! The program is hand-built MIR: `mk()` allocates a pair and returns it,
//! and a `Const` thunk calls it.

use std::sync::{Arc, mpsc};

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, IndexVec, ModuleId};
use cheby_mir::{
    Abi, AbiSig, Block, Callee, CalleeDecl, Const, DefName, EntryKind, FnKey, Function, LayoutRef,
    LocalDecl, MirModule, Operand, Repr, Rvalue, Stmt, StrId, Terminator, UnitKind,
};
use cheby_runtime::alloc::Domain;
use cheby_runtime::{DetachedGroup, EntryConfig, EntryMode, EntryOutcome, Runtime, RuntimeConfig};

use super::Jit;

fn def(index: u32) -> DefId {
    DefId::new(ModuleId::from_u32(1), DefIndex::from_u32(index))
}

fn name(def: DefId, item: &str) -> DefName {
    DefName {
        def,
        module: "app".into(),
        item: item.into(),
    }
}

/// A function of one block whose locals are all values, returning local
/// `result`.
fn function(key: FnKey, abi: Abi, locals: usize, stmts: Vec<Stmt>, result: u32) -> Function {
    let decls = (0..locals)
        .map(|_| LocalDecl {
            repr: Repr::Value,
            name: None,
            ty: None,
        })
        .collect();
    let strings: Vec<Box<str>> = if abi == Abi::Cheby {
        vec!["test.cheby:1:1".into()]
    } else {
        Vec::new()
    };
    Function {
        key,
        abi,
        params: Vec::new(),
        ret: Repr::Value,
        locals: IndexVec::from_raw(decls).unwrap(),
        blocks: IndexVec::from_raw(vec![Block {
            params: Vec::new(),
            stmts,
            term: Terminator::Return(Operand::Local(cheby_mir::Local::from_u32(result))),
            unwind: Vec::new(),
        }])
        .unwrap(),
        span: Span::new(FileId::new(0), TextRange::new(0, 0)),
        strings: IndexVec::from_raw(strings).unwrap(),
        entry_loc: (abi == Abi::Cheby).then(|| StrId::from_u32(0)),
        entry_unwind: None,
        precounted: false,
        borrow: cheby_mir::borrow::BorrowMask::OWNED,
        sites: Vec::new(),
    }
}

fn module(
    unit: UnitKind,
    functions: Vec<Function>,
    callees: Vec<CalleeDecl>,
    names: Vec<DefName>,
) -> MirModule {
    MirModule {
        unit,
        path: if unit == UnitKind::Entries { "" } else { "app" }.into(),
        functions,
        callees,
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names,
    }
}

/// A JIT holding `mk()`, which allocates a pair of `Nil`s, and a `Const`
/// thunk that calls it; returns the JIT and the thunk's target.
fn allocating_jit() -> (Jit, DefId) {
    let mk = def(0);
    let target = def(10_000);
    let decl = CalleeDecl {
        def: mk,
        sig: AbiSig::owned(Abi::Cheby, Vec::new(), Repr::Value),
    };
    let alloc = Stmt::Assign {
        dst: cheby_mir::Local::from_u32(0),
        value: Rvalue::Alloc {
            layout: LayoutRef::Tuple(2),
            fields: vec![Operand::Const(Const::Nil), Operand::Const(Const::Nil)],
        },
    };
    let mut code = module(
        UnitKind::Code(ModuleId::from_u32(1)),
        vec![function(FnKey::Item(mk), Abi::Cheby, 1, vec![alloc], 0)],
        vec![decl.clone()],
        vec![name(mk, "mk")],
    );
    let call = Stmt::Assign {
        dst: cheby_mir::Local::from_u32(0),
        value: Rvalue::Call {
            callee: Callee::Direct(mk),
            args: Vec::new(),
        },
    };
    let mut entries = module(
        UnitKind::Entries,
        vec![function(
            FnKey::Entry(EntryKind::Const(target)),
            Abi::C,
            1,
            vec![call],
            0,
        )],
        vec![decl],
        vec![name(mk, "mk"), name(target, "VALUE")],
    );
    cheby_mir::count_and_check(&mut code).unwrap();
    cheby_mir::count_and_check(&mut entries).unwrap();
    let mut jit = Jit::new().unwrap();
    jit.compile(&[code, entries]).unwrap();
    (jit, target)
}

fn runtime(threads: usize) -> Runtime {
    Runtime::new(&RuntimeConfig {
        threads: Some(threads),
        ..RuntimeConfig::default()
    })
    .unwrap()
}

/// An entry configuration in test mode charging `domain`.
fn config(domain: &Arc<Domain>) -> EntryConfig {
    EntryConfig {
        stack_size: 1 << 20,
        domain: Arc::clone(domain),
        ..EntryConfig::new(EntryMode::Test)
    }
}

fn returned(outcome: EntryOutcome) -> u64 {
    match outcome {
        EntryOutcome::Returned(word) => word,
        other => panic!("the root did not return: {other:?}"),
    }
}

/// Releases `word`, charging `domain`.
fn release(domain: &Arc<Domain>, word: u64) {
    cheby_runtime::alloc::with_domain(Arc::clone(domain), || {
        // SAFETY: `word` is a live value the root returned, owned here.
        unsafe { cheby_runtime::rc::drop(word) };
    });
}

/// D-691, D-723: `Entry::start_root_in` and `Entry::start_root_notify`
/// keep the caller's domain, so the pair the root allocates is charged to
/// it and the root's time is counted there; `Entry::run_root_in` charges
/// the JIT's domain as before.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn start_root_charges_the_callers_domain() {
    let (jit, target) = allocating_jit();
    for threads in [1, 2] {
        let runtime = runtime(threads);
        let mine = Domain::new();
        let group = DetachedGroup::new();
        let entry = jit.entry(EntryKind::Const(target)).unwrap();
        let handle = entry
            .start_root_in(&runtime, &config(&mine), &group)
            .unwrap();
        let word = returned(handle.wait().unwrap());
        if cheby_runtime::LEAK_CHECK_ENABLED {
            assert_eq!(
                mine.live_objects(),
                1,
                "the root did not charge the caller's domain"
            );
            assert_eq!(
                jit.domain().live_objects(),
                0,
                "the root charged the JIT's domain"
            );
        }
        assert_eq!(mine.stats().entries().0, 1);
        release(&mine, word);

        let (done, reports) = mpsc::channel();
        let entry = jit.entry(EntryKind::Const(target)).unwrap();
        entry
            .start_root_notify(&runtime, &config(&mine), &group, 3, done)
            .unwrap();
        let (tag, outcome) = reports.recv().unwrap();
        assert_eq!(tag, 3);
        let word = returned(outcome.unwrap());
        if cheby_runtime::LEAK_CHECK_ENABLED {
            assert_eq!(mine.live_objects(), 1);
            assert_eq!(jit.domain().live_objects(), 0);
        }
        assert_eq!(mine.stats().entries().0, 2);
        release(&mine, word);

        let entry = jit.entry(EntryKind::Const(target)).unwrap();
        let word = returned(entry.run_root_in(&runtime, &config(&mine), &group).unwrap());
        if cheby_runtime::LEAK_CHECK_ENABLED {
            assert_eq!(
                mine.live_objects(),
                0,
                "run_root_in charged the caller's domain"
            );
            assert_eq!(jit.domain().live_objects(), 1);
        }
        release(&jit.domain(), word);
        runtime.shutdown();
        if cheby_runtime::LEAK_CHECK_ENABLED {
            assert_eq!(mine.live_objects(), 0);
            assert_eq!(jit.domain().live_objects(), 0);
        }
    }
}
