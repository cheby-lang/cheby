//! Compiling and running hand-built MIR.
//!
//! Every test's JIT has its own accounting domain, so tests run in
//! parallel and each checks only its own objects. The layout table is
//! process-wide, but registration gives each type consecutive ids under
//! one lock (D-522).

use std::sync::Mutex;

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, IndexVec, ModuleId, builtin};
use cheby_mir::{
    Abi, AbiSig, ArithOp, BitOp, Block, BlockId, Callee, CalleeDecl, CmpOp, Const, DefName,
    EntryKind, FieldDesc, FnKey, Function, LayoutDesc, LocalDecl, MirModule, Operand, PanicKind,
    Repr, RuntimeFn, Rvalue, StaticId, StaticKind, StaticObject, Stmt, StrId, TagType, Terminator,
    UnitKind,
};
use cheby_runtime::abi::{self, sym};
use cheby_runtime::{EntryConfig, EntryMode, EntryOutcome};

use super::*;

/// The process's memory is measured by one test; it runs alone.
static MEMORY: Mutex<()> = Mutex::new(());

/// The location every checked operation of these tests reports.
const LOC: &str = "src/ops.cheby:7:9";

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
/// location string `str#0` for their entry check; `strings` follow.
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
        entry_loc: (abi == Abi::Cheby).then(|| StrId::from_u32(0)),
        entry_unwind: None,
        precounted: false,
        borrow: cheby_mir::borrow::BorrowMask::OWNED,
        sites: Vec::new(),
        call_locs: Vec::new(),
        call_loc_strings: IndexVec::new(),
    }
}

fn local(n: u32) -> cheby_mir::Local {
    cheby_mir::Local::from_u32(n)
}

fn op(n: u32) -> Operand {
    Operand::Local(local(n))
}

fn bb(n: u32) -> BlockId {
    BlockId::from_u32(n)
}

fn block(stmts: Vec<Stmt>, term: Terminator) -> Block {
    Block {
        params: Vec::new(),
        stmts,
        term,
        unwind: Vec::new(),
    }
}

fn assign(dst: u32, value: Rvalue) -> Stmt {
    Stmt::Assign {
        dst: local(dst),
        value,
    }
}

fn ret(n: u32) -> Terminator {
    Terminator::Return(op(n))
}

fn cheby_sig(params: &[Repr]) -> AbiSig {
    AbiSig::owned(Abi::Cheby, params.to_vec(), Repr::Value)
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
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names,
    }
}

fn entries_unit(
    functions: Vec<Function>,
    callees: Vec<CalleeDecl>,
    mut names: Vec<DefName>,
) -> MirModule {
    names.sort_by_key(|n| n.def);
    names.dedup_by_key(|n| n.def);
    MirModule {
        unit: UnitKind::Entries,
        path: "".into(),
        functions,
        callees,
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
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
            vec![assign(
                0,
                Rvalue::Call {
                    callee: Callee::Direct(callee),
                    args,
                },
            )],
            ret(0),
        )],
        &[],
    )
}

fn run_const(jit: &Jit, target: DefId) -> EntryOutcome {
    run_with_stack(jit, target, cheby_runtime::entry::MIN_STACK_SIZE)
}

fn run_with_stack(jit: &Jit, target: DefId, stack_size: usize) -> EntryOutcome {
    let entry = jit.entry(EntryKind::Const(target)).unwrap();
    let config = EntryConfig {
        stack_size,
        ..EntryConfig::new(EntryMode::Test)
    };
    entry.run(&config).unwrap()
}

fn returned(outcome: EntryOutcome) -> u64 {
    match outcome {
        EntryOutcome::Returned(word) => word,
        EntryOutcome::Panicked(report) => panic!("the entry panicked: {report:?}"),
        // No runtime here: the isolated entry's thread is parked for good
        // (D-709), and a parked thread runs none of the JIT's code again.
        EntryOutcome::Exited(status) => panic!("the entry ended the program with status {status}"),
        EntryOutcome::Deadlocked(report) => panic!("the entry deadlocked:\n{report}"),
    }
}

fn static_nil() -> u64 {
    cheby_runtime::object::nil()
}

fn static_word(id: u32) -> u64 {
    cheby_runtime::object::static_ctor(id).unwrap().word()
}

/// The word at byte `offset` of the object `word`.
fn read_word(word: u64, offset: usize) -> u64 {
    let address = usize::try_from(word).unwrap().checked_add(offset).unwrap();
    // SAFETY: tests read only live or static objects their JIT returned,
    // within the object.
    unsafe { std::ptr::with_exposed_provenance::<u64>(address).read() }
}

fn layout_of(word: u64) -> u32 {
    abi::header_layout(read_word(word, 0))
}

fn rc_of(word: u64) -> i32 {
    abi::header_rc(read_word(word, 0))
}

/// The integer a uniform word of an integer type holds.
fn int_of(word: u64) -> i128 {
    if abi::is_tagged_int(word) {
        return i128::from(abi::untag_int(word));
    }
    match layout_of(word) {
        abi::layout::BOXED_INT => i128::from(read_word(word, 8).cast_signed()),
        abi::layout::BOXED_U64 => i128::from(read_word(word, 8)),
        other => panic!("not an integer word: layout {other}"),
    }
}

/// Gives up the reference to `word` that the host holds, charging `jit`'s
/// domain.
fn release(jit: &Jit, word: u64) {
    cheby_runtime::alloc::with_domain(jit.domain(), || {
        // SAFETY: `word` is a live value the entry returned, owned here.
        unsafe { cheby_runtime::rc::drop(word) };
    });
}

/// The text of a `String` word.
fn text_of(word: u64) -> String {
    // SAFETY: the word is a live `String` object.
    unsafe { cheby_runtime::string::to_rust_string(word) }.unwrap()
}

/// One module `app` of top-level functions, and an entries unit with one
/// constant thunk per call.
#[derive(Default)]
struct Prog {
    functions: Vec<Function>,
    callees: Vec<CalleeDecl>,
    names: Vec<DefName>,
    layouts: Vec<LayoutDesc>,
    foreign_layouts: Vec<LayoutDesc>,
    statics: Vec<StaticObject>,
    thunks: Vec<Function>,
    thunk_callees: Vec<CalleeDecl>,
    thunk_names: Vec<DefName>,
    /// Added to the index of each thunk's target, so batches of one JIT
    /// can use different targets.
    thunk_base: u32,
}

impl Prog {
    /// Adds `app::<item>` with index `index`, its body `blocks`; returns
    /// its def.
    fn item(
        &mut self,
        index: u32,
        item: &str,
        params: &[Repr],
        locals: &[Repr],
        blocks: Vec<Block>,
        strings: &[&str],
    ) -> DefId {
        let f = def(1, index);
        self.functions.push(function(
            FnKey::Item(f),
            Abi::Cheby,
            params,
            locals,
            Repr::Value,
            blocks,
            strings,
        ));
        self.declare(f, item, params);
        f
    }

    /// Declares `app::<item>` as a callee without a body in this unit.
    fn declare(&mut self, f: DefId, item: &str, params: &[Repr]) {
        self.callees.push(CalleeDecl {
            def: f,
            sig: cheby_sig(params),
        });
        self.names.push(name(f, "app", item));
    }

    /// A thunk calling `callee` with `args`; returns the thunk's target.
    fn call(&mut self, callee: DefId, args: Vec<Operand>) -> DefId {
        let n = u32::try_from(self.thunks.len()).unwrap();
        let base = 10_000_u32.checked_add(self.thunk_base).unwrap();
        let target = def(1, base.checked_add(n).unwrap());
        self.thunks.push(const_thunk(target, callee, args));
        let decl = self
            .callees
            .iter()
            .find(|c| c.def == callee)
            .unwrap()
            .clone();
        if !self.thunk_callees.iter().any(|c| c.def == callee) {
            self.thunk_callees.push(decl);
        }
        let callee_name = self.names.iter().find(|n| n.def == callee).unwrap().clone();
        self.thunk_names.push(callee_name);
        self.thunk_names.push(name(
            target,
            "app",
            &format!("VALUE{}", target.index.as_u32()),
        ));
        target
    }

    fn units(mut self) -> [MirModule; 2] {
        self.callees.sort_by_key(|c| c.def);
        self.thunk_callees.sort_by_key(|c| c.def);
        let mut code = code_unit(1, "app", self.functions, self.callees, self.names);
        code.layouts = self.layouts;
        code.foreign_layouts = self.foreign_layouts;
        code.statics = self.statics;
        let entries = entries_unit(self.thunks, self.thunk_callees, self.thunk_names);
        [code, entries]
    }

    fn compile(self) -> Jit {
        let mut jit = Jit::new().unwrap();
        jit.compile(&self.units()).unwrap();
        jit
    }

    /// The units with their unwind edges and pads (D-698), as the driver's
    /// pipeline gives them, checked at `Stage::Unwound`.
    fn unwound_units(self) -> [MirModule; 2] {
        let mut units = self.units();
        for unit in &mut units {
            cheby_mir::unwind::insert_module(unit).unwrap();
            cheby_mir::validate(unit, cheby_mir::Stage::Unwound).unwrap();
        }
        units
    }

    /// [`Prog::compile`] after the unwind pass: code that unwinds by the
    /// return flag (D-733). Every test that expects a panic uses it, since
    /// a raise in code without unwind edges ends the process
    /// (D-770).
    fn compile_unwound(self) -> Jit {
        self.compile_unwound_with(CodegenOptions::default(), JitOptions::from_env())
    }

    /// [`Prog::compile_unwound`] with `codegen` and `options`.
    fn compile_unwound_with(self, codegen: CodegenOptions, options: JitOptions) -> Jit {
        let mut jit = Jit::configured(codegen, options).unwrap();
        jit.compile(&self.unwound_units()).unwrap();
        jit
    }
}

/// A function of `params` whose single block runs `stmts` and returns
/// local `result`.
fn straight(stmts: Vec<Stmt>, result: u32) -> Vec<Block> {
    vec![block(stmts, ret(result))]
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
    assert!(registered.contains(&sym::UNRESOLVED));
}

/// A `main` thunk returning status 0 without calling anything.
#[test]
fn an_entry_returns_a_constant() {
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
    let mut p = Prog::default();
    let g = def(1, 1);
    p.item(
        0,
        "f",
        &[Repr::Int, Repr::Value],
        &[],
        vec![block(
            Vec::new(),
            Terminator::TailCall {
                callee: Callee::Direct(g),
                args: vec![op(1)],
            },
        )],
        &[],
    );
    p.item(
        1,
        "g",
        &[Repr::Value],
        &[],
        vec![block(Vec::new(), ret(0))],
        &[],
    );
    let target = p.call(
        def(1, 0),
        vec![Operand::Const(Const::Int(7)), Operand::Const(Const::Nil)],
    );
    let jit = p.compile();
    assert_eq!(
        run_const(&jit, target),
        EntryOutcome::Returned(static_nil())
    );
}

/// A second batch calls a function of the first through its slot.
#[test]
fn a_later_batch_calls_an_earlier_one() {
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
            Terminator::Return(Operand::Const(Const::StaticCtor(builtin::TRUE))),
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
        vec![name(g, "app::a", "g"), name(builtin::TRUE, "", "True")],
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
    assert_eq!(
        run_const(&jit, target),
        EntryOutcome::Returned(static_word(abi::layout::TRUE))
    );
}

/// A string literal is a static `String` object with count 0.
#[test]
fn string_literals_are_static_strings() {
    let mut p = Prog::default();
    let f = p.item(
        0,
        "f",
        &[],
        &[],
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::Str(StrId::from_u32(1)))),
        )],
        &["héllo, wörld"],
    );
    let target = p.call(f, Vec::new());
    let jit = p.compile();
    let word = returned(run_const(&jit, target));
    assert_eq!(text_of(word), "héllo, wörld");
    assert_eq!(rc_of(word), abi::RC_STATIC);
}

/// A panic terminator reaches the runtime with its kind and location.
#[test]
fn panics_report_kind_and_location() {
    let mut p = Prog::default();
    let f = p.item(
        0,
        "f",
        &[],
        &[],
        vec![block(
            Vec::new(),
            Terminator::Panic {
                kind: PanicKind::Todo,
                message: None,
                loc: StrId::from_u32(1),
            },
        )],
        &["src/main.cheby:3:5"],
    );
    let target = p.call(f, Vec::new());
    let jit = p.compile_unwound();
    let EntryOutcome::Panicked(report) = run_const(&jit, target) else {
        panic!("the entry returned");
    };
    assert_eq!(report.kind, cheby_runtime::abi::PanicKind::Todo);
    assert_eq!(report.location, "src/main.cheby:3:5");
}

/// Unbounded non-tail recursion reaches the entry check's slow path and
/// panics with a stack overflow instead of hitting the guard page (E3).
#[test]
fn the_entry_check_catches_stack_overflow() {
    let mut p = Prog::default();
    let f = def(1, 0);
    p.item(
        0,
        "loop",
        &[],
        &[Repr::Value],
        straight(
            vec![assign(
                0,
                Rvalue::Call {
                    callee: Callee::Direct(f),
                    args: Vec::new(),
                },
            )],
            0,
        ),
        &[],
    );
    let target = p.call(f, Vec::new());
    let jit = p.compile_unwound();
    let EntryOutcome::Panicked(report) = run_with_stack(&jit, target, 1 << 20) else {
        panic!("the recursion returned");
    };
    assert_eq!(report.kind, cheby_runtime::abi::PanicKind::StackOverflow);
    assert_eq!(report.location, "test.cheby:1:1");
}

/// A function calling the native symbols `symbols` in order, then
/// returning `Nil`.
fn calls_natives(symbols: &[&str]) -> Prog {
    let mut p = Prog::default();
    let stmts = symbols
        .iter()
        .map(|symbol| {
            Stmt::Eval(Rvalue::CallForeign {
                symbol: (*symbol).into(),
                sig: cheby_mir::ForeignSig {
                    params: Vec::new(),
                    ret: cheby_mir::CType::Void,
                },
                args: Vec::new(),
                blocking: false,
            })
        })
        .collect();
    p.item(
        0,
        "f",
        &[],
        &[],
        vec![block(stmts, Terminator::Return(Operand::Const(Const::Nil)))],
        &[],
    );
    p
}

/// `@external` symbols nobody defines are an error before finalizing, not
/// a panic inside Cranelift, and every one of them is reported (D-532).
#[test]
fn missing_native_symbols_are_all_reported() {
    let missing = ["cheby_test_missing_b", "cheby_test_missing_a"];
    let mut jit = Jit::new().unwrap();
    let err = jit.compile(&calls_natives(&missing).units()).unwrap_err();
    let JitError::UnresolvedSymbols(symbols) = &err else {
        panic!("{err}");
    };
    assert_eq!(symbols, &["cheby_test_missing_a", "cheby_test_missing_b"]);
    assert_eq!(
        err.to_string(),
        "the native symbols `cheby_test_missing_a`, `cheby_test_missing_b` are not defined"
    );
    // A failed batch leaves nothing behind: the corrected batch declares
    // the same item again, compiles and runs.
    let mut fixed = calls_natives(&[]);
    let target = fixed.call(def(1, 0), Vec::new());
    jit.compile(&fixed.units()).unwrap();
    assert_eq!(jit.decls.foreign_symbols(), Vec::<&str>::new());
    assert_eq!(
        run_const(&jit, target),
        EntryOutcome::Returned(static_nil())
    );
}

/// With `stub_unresolved` (`cheby check`), the same batch builds, both
/// symbols are bound to `cheby_rt_unresolved`, and calling one panics with
/// `ForbiddenEffect` (D-532).
#[test]
fn unresolved_symbols_can_be_stubbed() {
    let mut p = calls_natives(&["cheby_test_missing_b", "cheby_test_missing_a"]);
    let target = p.call(def(1, 0), Vec::new());
    let jit = p.compile_unwound_with(
        CodegenOptions::default(),
        JitOptions {
            stub_unresolved: true,
            ..JitOptions::from_env()
        },
    );
    let EntryOutcome::Panicked(report) = run_const(&jit, target) else {
        panic!("a stub returned");
    };
    assert_eq!(report.kind, cheby_runtime::abi::PanicKind::ForbiddenEffect);
}

#[test]
fn clif_is_deterministic() {
    let build = || {
        let mut p = Prog::default();
        p.item(
            0,
            "f",
            &[Repr::Value],
            &[],
            vec![block(
                vec![Stmt::Drop(local(0))],
                Terminator::Return(Operand::Const(Const::Nil)),
            )],
            &[],
        );
        let mut jit = Jit::with_options(CodegenOptions {
            capture_clif: true,
            ..CodegenOptions::default()
        })
        .unwrap();
        jit.compile(&p.units()).unwrap();
        jit.take_clif().join("\n")
    };
    let first = build();
    assert!(first.contains("; cb$fn$app$f"), "{first}");
    assert!(first.contains("get_stack_pointer"), "{first}");
    assert_eq!(first, build());
}

/// `run_main` applies `cheby run`'s policy: status 0 from the thunk, and in
/// debug builds 102 when an object of the JIT's domain is still live at
/// exit (D-458), which is what makes 001's status 0 a leak check.
#[test]
fn run_reports_leaks_in_debug_builds() {
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
    let leaked = cheby_runtime::alloc::with_domain(jit.domain(), || {
        cheby_runtime::string::new_string("leaked")
    });
    let status = entry.run_main(RunConfig::default()).unwrap();
    release(&jit, leaked);
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
fn rc_placement_frees_every_object() {
    let (mk, keep, user) = (def(1, 0), def(1, 1), def(1, 2));
    let mut p = Prog::default();
    p.item(
        0,
        "mk",
        &[],
        &[Repr::Value],
        straight(
            vec![assign(
                0,
                Rvalue::Alloc {
                    layout: LayoutRef::Tuple(0),
                    fields: Vec::new(),
                },
            )],
            0,
        ),
        &[],
    );
    p.item(
        1,
        "keep",
        &[Repr::Value],
        &[],
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::Nil)),
        )],
        &[],
    );
    let call = |dst, callee, args| {
        assign(
            dst,
            Rvalue::Call {
                callee: Callee::Direct(callee),
                args,
            },
        )
    };
    p.item(
        2,
        "user",
        &[],
        &[Repr::Value; 3],
        vec![block(
            vec![
                call(0, mk, Vec::new()),
                call(1, keep, vec![op(0)]),
                call(2, keep, vec![op(0)]),
            ],
            Terminator::Return(Operand::Const(Const::Nil)),
        )],
        &[],
    );
    let target = p.call(user, Vec::new());
    let [mut code, mut entries] = p.units();
    cheby_mir::count_and_check(&mut code).unwrap();
    cheby_mir::count_and_check(&mut entries).unwrap();
    let mut jit = Jit::new().unwrap();
    jit.compile(&[code, entries]).unwrap();
    assert_eq!(
        run_const(&jit, target),
        EntryOutcome::Returned(static_nil())
    );
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0);
    }
}

/// The memory the OS charges this process for: the physical footprint on
/// Apple, the resident set on Linux, the commit charge on Windows. `None`
/// elsewhere.
#[cfg(target_vendor = "apple")]
fn process_memory() -> Option<u64> {
    // SAFETY: `rusage_info_v2` is plain data that `proc_pid_rusage` fills.
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a writable `rusage_info_v2`, the flavor asked for.
    let status = unsafe {
        libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V2, (&raw mut info).cast())
    };
    (status == 0).then_some(info.ri_phys_footprint)
}

#[cfg(target_os = "linux")]
fn process_memory() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: `sysconf` only reads a configuration value.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    pages.checked_mul(page)
}

// TODO(M6): this module is not built on Windows until M6 (D-545, see
// `lib.rs`), so this function is compiled nowhere for now. It is kept for
// M6, which runs these tests on Windows.
#[cfg(windows)]
fn process_memory() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain data that `K32GetProcessMemoryInfo` fills.
    let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    let size = u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>()).ok()?;
    counters.cb = size;
    // SAFETY: `counters` is writable and `size` is its size.
    let ok =
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut counters).cast(), size) };
    (ok != 0).then(|| u64::try_from(counters.PrivateUsage).ok())?
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux", windows)))]
fn process_memory() -> Option<u64> {
    None
}

/// Many JITs alive at once each reserve 1 GiB of address space but commit
/// only the pages their code and data use, so 64 of them cost megabytes,
/// not 64 GiB; each runs its own code, and dropping one releases its
/// reservation.
#[test]
fn jits_commit_only_what_they_use() {
    const JITS: usize = 64;
    let _guard = MEMORY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let before = process_memory();
    let mut jits = Vec::new();
    let mut target = def(1, 0);
    for _ in 0..JITS {
        let mut p = Prog::default();
        let f = p.item(
            0,
            "f",
            &[],
            &[],
            vec![block(
                Vec::new(),
                Terminator::Return(Operand::Const(Const::Nil)),
            )],
            &[],
        );
        target = p.call(f, Vec::new());
        jits.push(p.compile());
    }
    for jit in &jits {
        assert_eq!(run_const(jit, target), EntryOutcome::Returned(static_nil()));
        let memory = jit.code_memory();
        assert_eq!(memory.reserved, 1 << 30);
        assert!(
            memory.committed > 0 && memory.committed <= 1 << 20,
            "{memory:?}"
        );
    }
    if let (Some(before), Some(after)) = (before, process_memory()) {
        let grown = after.saturating_sub(before);
        assert!(grown < 256 << 20, "{JITS} JITs added {grown} bytes");
    }
    let usage: Vec<_> = jits.iter().map(|jit| Arc::clone(&jit.memory)).collect();
    drop(jits);
    for usage in usage {
        assert_eq!((usage.reserved(), usage.committed()), (0, 0));
    }
}

/// `g<index>()` returns `labs` of `args`, called with the C parameters
/// `params`; then, if `missing`, calls a symbol nobody defines.
fn labs_wrapper(index: u32, params: &[cheby_mir::CType], args: &[i64], missing: bool) -> Function {
    let sized = |n: &i64| {
        Operand::Const(Const::Sized {
            repr: Repr::I64,
            bits: n.cast_unsigned(),
        })
    };
    let mut stmts = vec![assign(
        0,
        Rvalue::CallForeign {
            symbol: "labs".into(),
            sig: cheby_mir::ForeignSig {
                params: params.to_vec(),
                ret: cheby_mir::CType::I64,
            },
            args: args.iter().map(sized).collect(),
            blocking: false,
        },
    )];
    if missing {
        stmts.push(Stmt::Eval(Rvalue::CallForeign {
            symbol: "cheby_no_such_symbol_anywhere".into(),
            sig: cheby_mir::ForeignSig {
                params: Vec::new(),
                ret: cheby_mir::CType::Void,
            },
            args: Vec::new(),
            blocking: false,
        }));
    }
    function(
        FnKey::Item(def(1, index)),
        Abi::Cheby,
        &[],
        &[Repr::Value],
        Repr::Value,
        vec![block(stmts, ret(0))],
        &[],
    )
}

/// A batch of the wrapper `g<index>` and a constant entry calling it.
fn labs_batch(index: u32, function: Function) -> [MirModule; 2] {
    let g = def(1, index);
    let target = def(1, index.saturating_add(100));
    let callees = vec![CalleeDecl {
        def: g,
        sig: cheby_sig(&[]),
    }];
    let g_name = format!("g{index}");
    let code = code_unit(
        1,
        "app",
        vec![function],
        callees.clone(),
        vec![name(g, "app", &g_name)],
    );
    let entries = entries_unit(
        vec![const_thunk(target, g, Vec::new())],
        callees,
        vec![
            name(g, "app", &g_name),
            name(target, "app", &format!("VALUE{index}")),
        ],
    );
    [code, entries]
}

/// D-455 in the REPL counts only the session's current definitions, so a
/// redefinition may bind a foreign symbol with another C signature, and a
/// failed entry's signature does not count. The JIT imports foreign
/// symbols by address, so each batch below compiles and runs, although
/// they call `labs` with three different C signatures.
#[cfg(unix)]
#[test]
fn batches_may_call_a_foreign_symbol_with_new_signatures() {
    use cheby_mir::CType;
    let mut jit = Jit::new().unwrap();
    // A failed entry that binds `labs` as `int labs(int)`.
    let failed = labs_batch(0, labs_wrapper(0, &[CType::I32], &[-3], true));
    let err = jit.compile(&failed).unwrap_err();
    assert!(matches!(err, JitError::UnresolvedSymbols(_)), "{err}");
    // `long labs(long)`.
    jit.compile(&labs_batch(1, labs_wrapper(1, &[CType::I64], &[-5], false)))
        .unwrap();
    assert_eq!(run_const(&jit, def(1, 101)), EntryOutcome::Returned(5));
    // A redefinition with an extra parameter the callee ignores.
    jit.compile(&labs_batch(
        2,
        labs_wrapper(2, &[CType::I64, CType::I64], &[-7, 0], false),
    ))
    .unwrap();
    assert_eq!(run_const(&jit, def(1, 102)), EntryOutcome::Returned(7));
    assert_eq!(run_const(&jit, def(1, 101)), EntryOutcome::Returned(5));
}

/// Whether every import word `jit` declared is defined, with the words.
fn words_defined(jit: &Jit) -> (bool, Vec<String>) {
    let words = jit.decls.import_words();
    let all = words.iter().all(|&(_, defined)| defined);
    (all, words.iter().map(|&(s, _)| s.to_owned()).collect())
}

/// A batch that fails restores the JIT's declarations, so it leaves no
/// import word declared and undefined (D-913): the next batch that uses
/// the same symbol declares its word again and defines it, and a session's
/// later batches share the words of its earlier ones
/// (GAP(jit-import-words-per-session)).
#[cfg(unix)]
#[test]
fn failed_batches_leave_no_import_word_undefined() {
    use cheby_mir::CType;
    let mut jit = Jit::new().unwrap();
    // The first batch fails: nothing of it stays, the runtime's words
    // included.
    let failed = labs_batch(0, labs_wrapper(0, &[CType::I64], &[-3], true));
    let err = jit.compile(&failed).unwrap_err();
    assert!(matches!(err, JitError::UnresolvedSymbols(_)), "{err}");
    assert_eq!(jit.decls.import_words(), Vec::<(&str, bool)>::new());
    jit.compile(&labs_batch(1, labs_wrapper(1, &[CType::I64], &[-5], false)))
        .unwrap();
    let (all, words) = words_defined(&jit);
    assert!(all, "{words:?}");
    assert!(words.iter().any(|w| w == "labs"), "{words:?}");
    assert!(words.iter().any(|w| w == sym::ALLOC), "{words:?}");
    assert!(
        !words.iter().any(|w| w == "cheby_no_such_symbol_anywhere"),
        "{words:?}"
    );
    assert_eq!(run_const(&jit, def(1, 101)), EntryOutcome::Returned(5));
    // A failure after a batch compiled keeps that batch's words, defined.
    let err = jit
        .compile(&labs_batch(2, labs_wrapper(2, &[CType::I64], &[-7], true)))
        .unwrap_err();
    assert!(matches!(err, JitError::UnresolvedSymbols(_)), "{err}");
    assert_eq!(words_defined(&jit), (true, words.clone()));
    // A later batch uses the session's word for `labs`.
    jit.compile(&labs_batch(3, labs_wrapper(3, &[CType::I64], &[-9], false)))
        .unwrap();
    assert_eq!(words_defined(&jit), (true, words));
    assert_eq!(run_const(&jit, def(1, 103)), EntryOutcome::Returned(9));
    assert_eq!(run_const(&jit, def(1, 101)), EntryOutcome::Returned(5));
}

/// The JIT compiles for [`cheby_codegen_clif::isa::host_isa`], the
/// target's fixed feature set (D-921), not the host's detected features.
#[test]
fn isa_fixed_the_jit_uses_the_fixed_isa() {
    let jit = Jit::new().unwrap();
    let fixed = cheby_codegen_clif::isa::host_isa().unwrap();
    let flags = |isa: &dyn cranelift_codegen::isa::TargetIsa| -> Vec<String> {
        isa.isa_flags().iter().map(ToString::to_string).collect()
    };
    assert_eq!(flags(jit.module.isa()), flags(&*fixed));
    assert_eq!(
        jit.module.isa().flags().to_string(),
        fixed.flags().to_string()
    );
    assert_eq!(jit.module.isa().triple(), fixed.triple());
    assert!(!jit.module.isa().flags().is_pic());
}

/// Compile speed of the debug toolchain (plan §4.4): translates and
/// compiles 400 functions of 40 blocks each and prints the time. Run with
/// `cargo test -p cheby_jit -- --ignored compile_speed_probe --nocapture`.
#[test]
#[ignore = "a timing probe, not a check"]
fn compile_speed_probe() {
    const FUNCTIONS: u32 = 400;
    const BLOCKS: u32 = 40;
    let mut p = Prog::default();
    for n in 0..FUNCTIONS {
        let next = def(1, n.checked_add(1).unwrap() % FUNCTIONS);
        let blocks = (0..BLOCKS)
            .map(|b| {
                let term = if b.checked_add(1) == Some(BLOCKS) {
                    Terminator::TailCall {
                        callee: Callee::Direct(next),
                        args: vec![op(0)],
                    }
                } else {
                    Terminator::Switch {
                        on: op(1),
                        cases: vec![(0, bb(b.checked_add(1).unwrap()))],
                        default: Some(bb(BLOCKS.checked_sub(1).unwrap())),
                    }
                };
                block(
                    vec![assign(
                        1,
                        Rvalue::Use(Operand::Const(Const::Int(i64::from(b)))),
                    )],
                    term,
                )
            })
            .collect();
        p.item(
            n,
            &format!("f{n}"),
            &[Repr::Value],
            &[Repr::Int],
            blocks,
            &[],
        );
    }
    let units = p.units();
    let start = std::time::Instant::now();
    let mut jit = Jit::new().unwrap();
    jit.compile(&units).unwrap();
    println!("compile_speed_probe: {:?}", start.elapsed());
}

mod blocking;
mod borrow;
mod code_map;
mod control;
mod entry_unwind;
mod large_frame;
mod literals;
mod objects;
mod repl;
mod reuse;
mod scalars;
mod special;
mod std_context;
mod unwind_flag;
mod unwind_ir;
