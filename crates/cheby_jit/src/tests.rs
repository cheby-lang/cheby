//! Compiling and running hand-built MIR.

use std::sync::Mutex;

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, IndexVec, ModuleId};
use cheby_mir::{
    Abi, AbiSig, Block, Callee, CalleeDecl, Const, DefName, EntryKind, FnKey, Function, LocalDecl,
    MirModule, Operand, PanicKind, Repr, Rvalue, Stmt, Terminator, UnitKind,
};
use cheby_runtime::abi::sym;
use cheby_runtime::{EntryConfig, EntryMode, EntryOutcome};

use super::*;

/// The leak counter and the layout table are process-wide; tests that run
/// generated code take turns.
static RUNNING: Mutex<()> = Mutex::new(());

fn span() -> Span {
    Span::new(FileId::new(0), TextRange::new(0, 0))
}

fn def(module: u32, index: u32) -> DefId {
    DefId::new(ModuleId::from_u32(module), DefIndex::from_u32(index))
}

fn name(def: DefId, module: &str, item: &str) -> DefName {
    DefName {
        def,
        module: module.into(),
        item: item.into(),
    }
}

/// A function with the given locals and blocks. Cheby functions get a
/// location string `str#0` for their entry check.
fn function(
    key: FnKey,
    abi: Abi,
    params: &[Repr],
    locals: &[Repr],
    ret: Repr,
    blocks: Vec<Block>,
    strings: &[&str],
) -> Function {
    let mut all: Vec<LocalDecl> = Vec::new();
    for &repr in params.iter().chain(locals) {
        all.push(LocalDecl {
            repr,
            name: None,
            ty: None,
        });
    }
    let mut table: Vec<Box<str>> = Vec::new();
    if abi == Abi::Cheby {
        table.push("test.cheby:1:1".into());
    }
    table.extend(strings.iter().map(|s| Box::<str>::from(*s)));
    Function {
        key,
        abi,
        params: (0..params.len())
            .map(|n| cheby_mir::Local::from_u32(u32::try_from(n).unwrap()))
            .collect(),
        ret,
        locals: IndexVec::from_raw(all).unwrap(),
        blocks: IndexVec::from_raw(blocks).unwrap(),
        span: span(),
        strings: IndexVec::from_raw(table).unwrap(),
        entry_loc: (abi == Abi::Cheby).then(|| cheby_mir::StrId::from_u32(0)),
    }
}

fn local(n: u32) -> cheby_mir::Local {
    cheby_mir::Local::from_u32(n)
}

fn block(stmts: Vec<Stmt>, term: Terminator) -> Block {
    Block {
        params: Vec::new(),
        stmts,
        term,
    }
}

fn cheby_sig(params: &[Repr]) -> AbiSig {
    AbiSig {
        abi: Abi::Cheby,
        params: params.to_vec(),
        ret: Repr::Value,
    }
}

fn code_unit(
    module: u32,
    path: &str,
    functions: Vec<Function>,
    callees: Vec<CalleeDecl>,
    mut names: Vec<DefName>,
) -> MirModule {
    names.sort_by_key(|n| n.def);
    names.dedup_by_key(|n| n.def);
    MirModule {
        unit: UnitKind::Code(ModuleId::from_u32(module)),
        path: path.into(),
        functions,
        callees,
        layouts: Vec::new(),
        names,
    }
}

fn entries_unit(
    functions: Vec<Function>,
    callees: Vec<CalleeDecl>,
    names: Vec<DefName>,
) -> MirModule {
    MirModule {
        unit: UnitKind::Entries,
        path: "".into(),
        functions,
        callees,
        layouts: Vec::new(),
        names,
    }
}

/// A `Const` entry thunk that calls `callee` with `args` and returns its
/// result word.
fn const_thunk(target: DefId, callee: DefId, args: Vec<Operand>) -> Function {
    function(
        FnKey::Entry(EntryKind::Const(target)),
        Abi::C,
        &[],
        &[Repr::Value],
        Repr::Value,
        vec![block(
            vec![Stmt::Assign {
                dst: local(0),
                value: Rvalue::Call {
                    callee: Callee::Direct(callee),
                    args,
                },
            }],
            Terminator::Return(Operand::Local(local(0))),
        )],
        &[],
    )
}

fn run_const(jit: &Jit, target: DefId) -> EntryOutcome {
    let entry = jit.entry(EntryKind::Const(target)).unwrap();
    entry.run(&EntryConfig::new(EntryMode::Test)).unwrap()
}

fn static_nil() -> u64 {
    cheby_runtime::object::nil()
}

#[test]
fn panic_kinds_match_the_runtime() {
    let mir: Vec<u32> = PanicKind::ALL.iter().map(|k| k.code()).collect();
    let runtime: Vec<u32> = cheby_runtime::abi::PanicKind::ALL
        .iter()
        .map(|k| k.code())
        .collect();
    assert_eq!(mir, runtime);
    for (m, r) in PanicKind::ALL
        .iter()
        .zip(cheby_runtime::abi::PanicKind::ALL)
    {
        assert_eq!(format!("{m:?}"), format!("{r:?}"));
    }
}

#[test]
fn every_imported_runtime_symbol_is_registered() {
    let registered: Vec<&str> = cheby_runtime::symbols()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    for symbol in cheby_codegen_clif::runtime::imported_symbols() {
        assert!(registered.contains(&symbol), "`{symbol}` is not registered");
    }
    assert!(registered.contains(&sym::IO_PRINTLN));
}

/// A `main` thunk returning status 0 without calling anything.
#[test]
fn an_entry_returns_a_constant() {
    let _guard = RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let main = def(1, 0);
    let thunk = function(
        FnKey::Entry(EntryKind::Main(main)),
        Abi::C,
        &[],
        &[],
        Repr::I64,
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::Sized {
                repr: Repr::I64,
                bits: 0,
            })),
        )],
        &[],
    );
    let mut jit = Jit::new().unwrap();
    jit.compile(&[entries_unit(
        vec![thunk],
        Vec::new(),
        vec![name(main, "app", "main")],
    )])
    .unwrap();
    let entry = jit.entry(EntryKind::Main(main)).unwrap();
    let outcome = entry.run(&EntryConfig::new(EntryMode::Run)).unwrap();
    assert_eq!(outcome, EntryOutcome::Returned(0));
}

/// `f(a: Int, b: Value) -> value { tail_call g(b) }` and
/// `g(x: Value) -> value { return x }`: a tail call between functions
/// whose parameters differ in number and representation, called from a
/// thunk. The result is the static `Nil` passed in.
#[test]
fn tail_calls_between_different_signatures() {
    let _guard = RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = def(1, 0);
    let g = def(1, 1);
    let target = def(1, 2);
    let f_fn = function(
        FnKey::Item(f),
        Abi::Cheby,
        &[Repr::Int, Repr::Value],
        &[],
        Repr::Value,
        vec![block(
            Vec::new(),
            Terminator::TailCall {
                callee: Callee::Direct(g),
                args: vec![Operand::Local(local(1))],
            },
        )],
        &[],
    );
    let g_fn = function(
        FnKey::Item(g),
        Abi::Cheby,
        &[Repr::Value],
        &[],
        Repr::Value,
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Local(local(0))),
        )],
        &[],
    );
    let f_sig = cheby_sig(&[Repr::Int, Repr::Value]);
    let g_sig = cheby_sig(&[Repr::Value]);
    let names = vec![name(f, "app", "f"), name(g, "app", "g")];
    let code = code_unit(
        1,
        "app",
        vec![f_fn, g_fn],
        vec![
            CalleeDecl {
                def: f,
                sig: f_sig.clone(),
            },
            CalleeDecl { def: g, sig: g_sig },
        ],
        names.clone(),
    );
    let thunk = const_thunk(
        target,
        f,
        vec![Operand::Const(Const::Int(7)), Operand::Const(Const::Nil)],
    );
    let entries = entries_unit(
        vec![thunk],
        vec![CalleeDecl { def: f, sig: f_sig }],
        vec![name(f, "app", "f"), name(target, "app", "VALUE")],
    );
    let mut jit = Jit::new().unwrap();
    jit.compile(&[code, entries]).unwrap();
    assert_eq!(
        run_const(&jit, target),
        EntryOutcome::Returned(static_nil())
    );
}

/// A second batch calls a function of the first through its slot.
#[test]
fn a_later_batch_calls_an_earlier_one() {
    let _guard = RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let g = def(1, 0);
    let h = def(2, 0);
    let target = def(2, 1);
    let g_sig = cheby_sig(&[]);
    let g_fn = function(
        FnKey::Item(g),
        Abi::Cheby,
        &[],
        &[],
        Repr::Value,
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::StaticCtor(
                cheby_iface::builtin::TRUE,
            ))),
        )],
        &[],
    );
    let first = code_unit(
        1,
        "app::a",
        vec![g_fn],
        vec![CalleeDecl {
            def: g,
            sig: g_sig.clone(),
        }],
        vec![
            name(g, "app::a", "g"),
            name(cheby_iface::builtin::TRUE, "", "True"),
        ],
    );
    let mut jit = Jit::new().unwrap();
    jit.compile(&[first]).unwrap();

    let h_fn = function(
        FnKey::Item(h),
        Abi::Cheby,
        &[],
        &[],
        Repr::Value,
        vec![block(
            Vec::new(),
            Terminator::TailCall {
                callee: Callee::Direct(g),
                args: Vec::new(),
            },
        )],
        &[],
    );
    let second = code_unit(
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
    jit.compile(&[second, entries]).unwrap();
    let truth = cheby_runtime::object::static_ctor(cheby_runtime::abi::layout::TRUE)
        .unwrap()
        .word();
    assert_eq!(run_const(&jit, target), EntryOutcome::Returned(truth));
}

/// A string literal is a static `String` object with count 0.
#[test]
fn string_literals_are_static_strings() {
    let _guard = RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = def(1, 0);
    let target = def(1, 1);
    let f_fn = function(
        FnKey::Item(f),
        Abi::Cheby,
        &[],
        &[],
        Repr::Value,
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::Str(cheby_mir::StrId::from_u32(1)))),
        )],
        &["héllo, wörld"],
    );
    let sig = cheby_sig(&[]);
    let code = code_unit(
        1,
        "app",
        vec![f_fn],
        vec![CalleeDecl {
            def: f,
            sig: sig.clone(),
        }],
        vec![name(f, "app", "f")],
    );
    let entries = entries_unit(
        vec![const_thunk(target, f, Vec::new())],
        vec![CalleeDecl { def: f, sig }],
        vec![name(f, "app", "f"), name(target, "app", "VALUE")],
    );
    let mut jit = Jit::new().unwrap();
    jit.compile(&[code, entries]).unwrap();
    let EntryOutcome::Returned(word) = run_const(&jit, target) else {
        panic!("the entry panicked");
    };
    // SAFETY: the word is a static `String` object built by codegen.
    let text = unsafe { cheby_runtime::string::to_rust_string(word) };
    assert_eq!(text.as_deref(), Some("héllo, wörld"));
    // SAFETY: a static object is 8-aligned and readable.
    let header =
        unsafe { std::ptr::with_exposed_provenance::<u64>(usize::try_from(word).unwrap()).read() };
    assert_eq!(
        cheby_runtime::abi::header_rc(header),
        cheby_runtime::abi::RC_STATIC
    );
}

/// A panic terminator reaches the runtime with its kind and location.
#[test]
fn panics_report_kind_and_location() {
    let _guard = RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = def(1, 0);
    let target = def(1, 1);
    let f_fn = function(
        FnKey::Item(f),
        Abi::Cheby,
        &[],
        &[],
        Repr::Value,
        vec![block(
            Vec::new(),
            Terminator::Panic {
                kind: PanicKind::Todo,
                message: None,
                loc: cheby_mir::StrId::from_u32(1),
            },
        )],
        &["src/main.cheby:3:5"],
    );
    let sig = cheby_sig(&[]);
    let code = code_unit(
        1,
        "app",
        vec![f_fn],
        vec![CalleeDecl {
            def: f,
            sig: sig.clone(),
        }],
        vec![name(f, "app", "f")],
    );
    let entries = entries_unit(
        vec![const_thunk(target, f, Vec::new())],
        vec![CalleeDecl { def: f, sig }],
        vec![name(f, "app", "f"), name(target, "app", "VALUE")],
    );
    let mut jit = Jit::new().unwrap();
    jit.compile(&[code, entries]).unwrap();
    let EntryOutcome::Panicked(report) = run_const(&jit, target) else {
        panic!("the entry returned");
    };
    assert_eq!(report.kind, cheby_runtime::abi::PanicKind::Todo);
    assert_eq!(report.location, "src/main.cheby:3:5");
}

/// Unbounded non-tail recursion reaches the entry check's slow path and
/// panics with a stack overflow instead of hitting the guard page.
#[test]
fn the_entry_check_catches_stack_overflow() {
    let _guard = RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = def(1, 0);
    let target = def(1, 1);
    let sig = cheby_sig(&[]);
    let f_fn = function(
        FnKey::Item(f),
        Abi::Cheby,
        &[],
        &[Repr::Value],
        Repr::Value,
        vec![block(
            vec![Stmt::Assign {
                dst: local(0),
                value: Rvalue::Call {
                    callee: Callee::Direct(f),
                    args: Vec::new(),
                },
            }],
            Terminator::Return(Operand::Local(local(0))),
        )],
        &[],
    );
    let code = code_unit(
        1,
        "app",
        vec![f_fn],
        vec![CalleeDecl {
            def: f,
            sig: sig.clone(),
        }],
        vec![name(f, "app", "loop")],
    );
    let entries = entries_unit(
        vec![const_thunk(target, f, Vec::new())],
        vec![CalleeDecl { def: f, sig }],
        vec![name(f, "app", "loop"), name(target, "app", "VALUE")],
    );
    let mut jit = Jit::new().unwrap();
    jit.compile(&[code, entries]).unwrap();
    let entry = jit.entry(EntryKind::Const(target)).unwrap();
    let config = EntryConfig {
        stack_size: 1 << 20,
        mode: EntryMode::Test,
    };
    let EntryOutcome::Panicked(report) = entry.run(&config).unwrap() else {
        panic!("the recursion returned");
    };
    assert_eq!(report.kind, cheby_runtime::abi::PanicKind::StackOverflow);
    assert_eq!(report.location, "test.cheby:1:1");
}

/// An `@external` symbol nobody defines is an error before finalizing,
/// not a panic inside Cranelift.
#[test]
fn a_missing_native_symbol_is_an_error() {
    let f = def(1, 0);
    let f_fn = function(
        FnKey::Item(f),
        Abi::Cheby,
        &[],
        &[],
        Repr::Value,
        vec![block(
            vec![Stmt::Eval(Rvalue::CallForeign {
                symbol: "cheby_no_such_symbol_anywhere".into(),
                sig: cheby_mir::ForeignSig {
                    params: Vec::new(),
                    ret: cheby_mir::CType::Void,
                },
                args: Vec::new(),
            })],
            Terminator::Return(Operand::Const(Const::Nil)),
        )],
        &[],
    );
    let code = code_unit(
        1,
        "app",
        vec![f_fn],
        vec![CalleeDecl {
            def: f,
            sig: cheby_sig(&[]),
        }],
        vec![name(f, "app", "f")],
    );
    let mut jit = Jit::new().unwrap();
    let err = jit.compile(&[code]).unwrap_err();
    assert!(
        matches!(&err, JitError::UnresolvedSymbol(s) if s == "cheby_no_such_symbol_anywhere"),
        "{err}"
    );
    // A failed batch leaves nothing behind: the corrected batch declares
    // the same item again, compiles and runs.
    let _guard = RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixed = function(
        FnKey::Item(f),
        Abi::Cheby,
        &[],
        &[],
        Repr::Value,
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::Nil)),
        )],
        &[],
    );
    let target = def(1, 1);
    let code = code_unit(
        1,
        "app",
        vec![fixed],
        vec![CalleeDecl {
            def: f,
            sig: cheby_sig(&[]),
        }],
        vec![name(f, "app", "f")],
    );
    let entries = entries_unit(
        vec![const_thunk(target, f, Vec::new())],
        vec![CalleeDecl {
            def: f,
            sig: cheby_sig(&[]),
        }],
        vec![name(f, "app", "f"), name(target, "app", "VALUE")],
    );
    jit.compile(&[code, entries]).unwrap();
    assert_eq!(jit.decls.foreign_symbols(), Vec::<&str>::new());
    assert_eq!(
        run_const(&jit, target),
        EntryOutcome::Returned(static_nil())
    );
}

#[test]
fn clif_is_deterministic() {
    let build = || {
        let f = def(1, 0);
        let f_fn = function(
            FnKey::Item(f),
            Abi::Cheby,
            &[Repr::Value],
            &[],
            Repr::Value,
            vec![block(
                vec![Stmt::Drop(local(0))],
                Terminator::Return(Operand::Const(Const::Nil)),
            )],
            &[],
        );
        let code = code_unit(
            1,
            "app",
            vec![f_fn],
            vec![CalleeDecl {
                def: f,
                sig: cheby_sig(&[Repr::Value]),
            }],
            vec![name(f, "app", "f")],
        );
        let mut jit = Jit::with_options(CodegenOptions {
            capture_clif: true,
            ..CodegenOptions::default()
        })
        .unwrap();
        jit.compile(&[code]).unwrap();
        jit.take_clif().join("\n")
    };
    let first = build();
    assert!(first.contains("; cb$fn$app$f"), "{first}");
    assert!(first.contains("get_stack_pointer"), "{first}");
    assert_eq!(first, build());
}

/// `run` applies `cheby run`'s policy: status 0 from the thunk, and in
/// debug builds 102 when an object is still live at exit (D-458), which is
/// what makes 001's status 0 a leak check.
#[test]
fn run_reports_leaks_in_debug_builds() {
    let _guard = RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let main = def(1, 0);
    let thunk = function(
        FnKey::Entry(EntryKind::Main(main)),
        Abi::C,
        &[],
        &[],
        Repr::I64,
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::Sized {
                repr: Repr::I64,
                bits: 0,
            })),
        )],
        &[],
    );
    let mut jit = Jit::new().unwrap();
    jit.compile(&[entries_unit(
        vec![thunk],
        Vec::new(),
        vec![name(main, "app", "main")],
    )])
    .unwrap();
    let entry = jit.entry(EntryKind::Main(main)).unwrap();
    assert_eq!(entry.run_main(RunConfig::default()).unwrap(), 0);
    let leaked = cheby_runtime::string::new_string("leaked");
    let status = entry.run_main(RunConfig::default()).unwrap();
    // SAFETY: `leaked` is a live heap string this test owns.
    unsafe { cheby_runtime::rc::drop(leaked) };
    let expected = if cheby_runtime::LEAK_CHECK_ENABLED {
        cheby_runtime::abi::status::LEAK_CHECK
    } else {
        0
    };
    assert_eq!(status, expected);
}

/// D-273's placement observed at run time. `user` gets a heap object from
/// `mk` and passes it to `keep` twice, and `keep` ignores its owned
/// parameter. The MIR is written without dups or drops; the real RC pass
/// (`count_and_check`) must put a dup before the first call and a drop in
/// `keep`, so nothing is live after the run. (A missing drop leaves the
/// object live; a misplaced one frees it twice, which the runtime aborts
/// on.)
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one hand-built MIR program of three functions"
)]
fn rc_placement_frees_every_object() {
    let _guard = RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let leaked = || -> i64 {
        let (mk, keep, user, target) = (def(1, 0), def(1, 1), def(1, 2), def(1, 3));
        let sized = |bits| {
            Operand::Const(Const::Sized {
                repr: Repr::I64,
                bits,
            })
        };
        let header = cheby_runtime::abi::header(1, 0, cheby_runtime::abi::layout::TUPLE);
        // An empty tuple, allocated the way `Alloc` will be (no context
        // needed in M1).
        let mk_fn = function(
            FnKey::Item(mk),
            Abi::Cheby,
            &[],
            &[Repr::Value],
            Repr::Value,
            vec![block(
                vec![Stmt::Assign {
                    dst: local(0),
                    value: Rvalue::CallForeign {
                        symbol: sym::ALLOC.into(),
                        sig: cheby_mir::ForeignSig {
                            params: vec![cheby_mir::CType::I64; 3],
                            ret: cheby_mir::CType::Ptr,
                        },
                        args: vec![sized(0), sized(8), sized(header)],
                    },
                }],
                Terminator::Return(Operand::Local(local(0))),
            )],
            &[],
        );
        let keep_fn = function(
            FnKey::Item(keep),
            Abi::Cheby,
            &[Repr::Value],
            &[],
            Repr::Value,
            vec![block(
                Vec::new(),
                Terminator::Return(Operand::Const(Const::Nil)),
            )],
            &[],
        );
        let call = |dst, callee, args| Stmt::Assign {
            dst: local(dst),
            value: Rvalue::Call {
                callee: Callee::Direct(callee),
                args,
            },
        };
        let user_fn = function(
            FnKey::Item(user),
            Abi::Cheby,
            &[],
            &[Repr::Value; 3],
            Repr::Value,
            vec![block(
                vec![
                    call(0, mk, Vec::new()),
                    call(1, keep, vec![Operand::Local(local(0))]),
                    call(2, keep, vec![Operand::Local(local(0))]),
                ],
                Terminator::Return(Operand::Const(Const::Nil)),
            )],
            &[],
        );
        let callees = vec![
            CalleeDecl {
                def: mk,
                sig: cheby_sig(&[]),
            },
            CalleeDecl {
                def: keep,
                sig: cheby_sig(&[Repr::Value]),
            },
            CalleeDecl {
                def: user,
                sig: cheby_sig(&[]),
            },
        ];
        let names = vec![
            name(mk, "app", "mk"),
            name(keep, "app", "keep"),
            name(user, "app", "user"),
        ];
        let mut code = code_unit(1, "app", vec![mk_fn, keep_fn, user_fn], callees, names);
        let mut entries = entries_unit(
            vec![const_thunk(target, user, Vec::new())],
            vec![CalleeDecl {
                def: user,
                sig: cheby_sig(&[]),
            }],
            vec![name(user, "app", "user"), name(target, "app", "VALUE")],
        );
        cheby_mir::count_and_check(&mut code).unwrap();
        cheby_mir::count_and_check(&mut entries).unwrap();
        let mut jit = Jit::new().unwrap();
        jit.compile(&[code, entries]).unwrap();
        let before = cheby_runtime::live_objects();
        assert_eq!(
            run_const(&jit, target),
            EntryOutcome::Returned(static_nil())
        );
        cheby_runtime::live_objects() - before
    };
    let live = leaked();
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(live, 0);
    }
}
