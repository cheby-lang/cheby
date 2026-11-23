//! Tests of code generation.

use cranelift_codegen::ir::{ArgumentExtension, types};
use cranelift_codegen::isa::CallConv;
use cranelift_jit::{JITBuilder, JITModule};

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, IndexVec, ModuleId};
use cheby_mir::{
    Abi, AbiSig, Block, CType, CalleeDecl, Const, DefName, FnKey, ForeignSig, Function, LocalDecl,
    Operand, Repr, Stmt, Terminator, UnitKind,
};

use crate::mangle::{self, Kind};
use crate::*;

fn def(module: u32, index: u32) -> DefId {
    DefId::new(ModuleId::from_u32(module), DefIndex::from_u32(index))
}

fn name(module: &str, item: &str) -> DefName {
    DefName {
        def: def(1, 0),
        module: module.into(),
        item: item.into(),
    }
}

/// Distinct (kind, module path, item) triples never mangle alike, even
/// for modules and packages named after kinds.
#[test]
fn mangled_names_do_not_collide() {
    let paths = [
        "a",
        "slot",
        "str",
        "fn",
        "a::b",
        "slot::a",
        "a::slot",
        "fn::str",
        "entry",
        "entry::main",
    ];
    let items = ["a", "b", "main", "slot", "str", "fn"];
    let mut seen = std::collections::BTreeMap::new();
    for kind in Kind::ALL {
        for path in paths {
            for item in items {
                let symbol = mangle::item(kind, &name(path, item));
                let key = (kind.word(), path, item);
                if let Some(previous) = seen.insert(symbol.clone(), key) {
                    panic!("{symbol} is both {previous:?} and {key:?}");
                }
            }
        }
    }
    // The symbol forms of D-456.
    assert_eq!(
        mangle::item(Kind::Fn, &name("std::io", "println")),
        "cb$fn$std$io$println"
    );
    assert_eq!(
        mangle::item(Kind::Slot, &name("hello_world", "main")),
        "cb$slot$hello_world$main"
    );
    assert_ne!(
        mangle::item(Kind::Fn, &name("slot::a", "b")),
        mangle::item(Kind::Slot, &name("a", "b"))
    );
    assert_eq!(
        mangle::entry_main("hello_world"),
        "cb$entry$main$hello_world"
    );
    assert_eq!(
        mangle::string("cb$fn$std$io$println", StrId::from_u32(0)),
        "cb$str$fn$std$io$println$0"
    );
    assert_eq!(mangle::trampoline(2), "cb$tramp$2");
    // D-679's inner body of a large frame: the kind first, then the
    // owner's symbol without `cb$`, as for strings.
    assert_eq!(
        mangle::large_frame_body("cb$fn$std$io$println"),
        "cb$big$fn$std$io$println"
    );
    assert_ne!(
        mangle::large_frame_body(&mangle::item(Kind::Fn, &name("a", "b"))),
        mangle::item(Kind::Fn, &name("big::fn::a", "b"))
    );
    // D-780's continuations, named as closures are under their own kind
    // (D-811).
    assert_eq!(
        mangle::continuation(&name("app::data", "build"), 3),
        "cb$cont$app$data$build$3"
    );
    assert_eq!(
        mangle::test_continuation("app::data", 2, 0),
        "cb$cont$test$app$data$2$0"
    );
    assert_ne!(
        mangle::continuation(&name("app", "build"), 0),
        mangle::closure(&name("app", "build"), 0)
    );
    // D-454's layout words, D-523's statics.
    assert_eq!(
        mangle::item(Kind::Layout, &name("geo", "Circle")),
        "cb$layout$geo$Circle"
    );
    assert_eq!(
        mangle::static_object(Kind::Dict, "app::shapes", "Show$Point"),
        "cb$dict$app$shapes$$Show$Point"
    );
    // `$$` ends the unit's module path, and the name keeps its own paths,
    // so statics of different units never share a symbol.
    assert_ne!(
        mangle::static_object(Kind::Dict, "app::app", "app::m::I$app::t::T"),
        mangle::static_object(Kind::Dict, "app", "app::app::m::I$app::t::T")
    );
    assert_eq!(
        mangle::static_object(Kind::Desc, "app", "List<app::t::T>"),
        "cb$desc$app$$List<app::t::T>"
    );
    assert_eq!(
        mangle::tuple_fn(&name("std::fmt", "Show::show"), 2),
        "cb$tfn$std$fmt$Show$show$2"
    );
    // Closures: by the owner's name, or by a test's ordinal, since a
    // test's name is free text that may equal a function's (D-456).
    assert_eq!(
        mangle::closure(&name("app", "spin"), 1),
        "cb$clo$app$spin$1"
    );
    assert_eq!(mangle::test_closure("app", 3, 1), "cb$clo$test$app$3$1");
    assert_ne!(
        mangle::test_closure("app", 0, 0),
        mangle::closure(&name("app", "test"), 0)
    );
}

/// The C signatures of every runtime helper (plan §4.3.1): `ctx` first for
/// those that allocate, may panic or call back, `bool` results
/// zero-extended, floats in float registers.
#[test]
fn runtime_helper_signatures() {
    use cheby_mir::RuntimeFn;
    use runtime::RuntimeFunc;
    for helper in RuntimeFn::ALL {
        // The element a `list_uncons` keeps is read from its stack slot.
        if helper.is_uncons_head() {
            continue;
        }
        // `map_value_at`'s map only keeps the map held; the C function
        // takes the address alone, and so do an upsert's `map_slot_take`
        // and `map_upsert_found`.
        if matches!(
            helper,
            RuntimeFn::MapValueAt | RuntimeFn::MapSlotTake | RuntimeFn::MapUpsertFound
        ) {
            let (f, takes_ctx) = RuntimeFunc::of(helper);
            assert_eq!(
                f.ctypes().0.len(),
                helper.params().len() - 1 + usize::from(takes_ctx),
                "{helper:?}"
            );
            continue;
        }
        // An upsert's address is read from the stack slot its
        // `map_upsert` wrote; a heap's slot is a stack address and its
        // first pair a load from it; the loop's check calls the entry
        // check's slow path with the function's location.
        if matches!(
            helper,
            RuntimeFn::MapUpsertAddress
                | RuntimeFn::HeapSlot
                | RuntimeFn::HeapFirst
                | RuntimeFn::LoopCheck
        ) {
            continue;
        }
        let (f, takes_ctx) = RuntimeFunc::of(helper);
        let (params, ret) = f.ctypes();
        let ctx = usize::from(takes_ctx);
        // `list_uncons` also takes the address it writes the element to,
        // and `map_upsert` the address of its word.
        let out = usize::from(matches!(
            helper,
            RuntimeFn::ListUncons | RuntimeFn::ListUnconsOwned | RuntimeFn::MapUpsert
        ));
        assert_eq!(
            params.len(),
            helper.params().len() + ctx + out,
            "{helper:?}"
        );
        assert_eq!(ret == CType::Void, helper.ret().is_none(), "{helper:?}");
    }
    assert_eq!(
        RuntimeFunc::ALL
            .iter()
            .enumerate()
            .filter(|(n, f)| f.index() != *n)
            .count(),
        0
    );
    let fmod = sig::c_signature(
        RuntimeFunc::FmodF32.ctypes().0,
        RuntimeFunc::FmodF32.ctypes().1,
        CallConv::SystemV,
    );
    assert_eq!(fmod.params[0].value_type, types::F32);
    assert_eq!(fmod.returns[0].value_type, types::F32);
}

/// Every Cheby signature starts with `ctx` and uses the tail convention;
/// C entry thunks use the platform's convention.
#[test]
fn cheby_signatures_start_with_ctx() {
    let s = sig::signature(
        &AbiSig::owned(
            Abi::Cheby,
            vec![Repr::Int, Repr::Bool, Repr::Float],
            Repr::Value,
        ),
        CallConv::SystemV,
    );
    assert_eq!(s.call_conv, CallConv::Tail);
    let params: Vec<_> = s.params.iter().map(|p| p.value_type).collect();
    assert_eq!(params, [types::I64, types::I64, types::I8, types::F64]);
    assert_eq!(s.returns.len(), 1);
    let thunk = sig::signature(
        &cheby_mir::entry_sig(cheby_mir::EntryKind::Repl(0)),
        CallConv::SystemV,
    );
    assert_eq!(thunk.call_conv, CallConv::SystemV);
    assert_eq!(thunk.params.len(), 1);
}

/// Narrow integers are extended by the caller (§12.5.1).
#[test]
fn c_signatures_extend_narrow_integers() {
    let s = sig::foreign_signature(
        &ForeignSig {
            params: vec![
                CType::I8,
                CType::U8,
                CType::Bool,
                CType::I16,
                CType::U32,
                CType::I64,
                CType::Ptr,
                CType::F64,
            ],
            ret: CType::U16,
        },
        CallConv::AppleAarch64,
    );
    let ext: Vec<_> = s.params.iter().map(|p| p.extension).collect();
    assert_eq!(
        ext,
        [
            ArgumentExtension::Sext,
            ArgumentExtension::Uext,
            ArgumentExtension::Uext,
            ArgumentExtension::Sext,
            ArgumentExtension::Uext,
            ArgumentExtension::None,
            ArgumentExtension::None,
            ArgumentExtension::None,
        ]
    );
    assert_eq!(
        s.returns.first().map(|r| r.extension),
        Some(ArgumentExtension::Uext)
    );
    let void = sig::foreign_signature(
        &ForeignSig {
            params: Vec::new(),
            ret: CType::Void,
        },
        CallConv::SystemV,
    );
    assert_eq!(void.returns.len(), 0);
}

fn module() -> JITModule {
    JITModule::new(JITBuilder::with_isa(
        isa::host_isa().unwrap(),
        isa::libcall_names(),
    ))
}

/// `f(x) { call_foreign "print"(borrow x); drop x; return "hi" }`.
fn unit() -> MirModule {
    let f = def(1, 0);
    let locals = IndexVec::from_raw(vec![LocalDecl {
        repr: Repr::Value,
        name: None,
        ty: None,
    }])
    .unwrap();
    let block = Block {
        pos: Vec::new(),
        params: Vec::new(),
        stmts: vec![
            Stmt::Eval(cheby_mir::Rvalue::CallForeign {
                symbol: "cheby_io_print".into(),
                sig: ForeignSig {
                    params: vec![CType::Ptr],
                    ret: CType::Void,
                },
                args: vec![Operand::Local(cheby_mir::Local::from_u32(0))],
                blocking: false,
            }),
            Stmt::Drop(cheby_mir::Local::from_u32(0)),
        ],
        term: Terminator::Return(Operand::Const(Const::Str(StrId::from_u32(1)))),
        unwind: Vec::new(),
    };
    let function = Function {
        positions: cheby_mir::Positions::default(),
        key: FnKey::Item(f),
        abi: Abi::Cheby,
        params: vec![cheby_mir::Local::from_u32(0)],
        ret: Repr::Value,
        locals,
        blocks: IndexVec::from_raw(vec![block]).unwrap(),
        span: Span::new(FileId::new(0), TextRange::new(0, 0)),
        strings: IndexVec::from_raw(vec!["a.cheby:1:4".into(), "hi".into()]).unwrap(),
        entry_loc: Some(StrId::from_u32(0)),
        entry_unwind: None,
        precounted: false,
        borrow: cheby_mir::borrow::BorrowMask::OWNED,
        sites: Vec::new(),
        call_locs: Vec::new(),
        call_loc_strings: IndexVec::new(),
    };
    MirModule {
        externals: Vec::new(),
        source: None,
        unit: UnitKind::Code(ModuleId::from_u32(1)),
        path: "app".into(),
        functions: vec![function],
        callees: vec![CalleeDecl {
            def: f,
            sig: AbiSig::owned(Abi::Cheby, vec![Repr::Value], Repr::Value),
        }],
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: vec![DefName {
            def: f,
            module: "app".into(),
            item: "f".into(),
        }],
    }
}

/// Translation reads only `&Decls`, gives the same text twice, and the
/// result compiles.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn translation_is_pure_and_deterministic() {
    let mut m = module();
    let mut decls = Decls::new(Module::isa(&m)).unwrap();
    let units = [unit()];
    let cx = CodegenCx {
        layouts: &BuiltinLayouts,
    };
    let opts = CodegenOptions::default();
    declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
    let decls_ref: &Decls = &decls;
    let first = translate(&units[0], &units[0].functions[0], decls_ref, &cx, &opts).unwrap();
    let second = translate(&units[0], &units[0].functions[0], decls_ref, &cx, &opts).unwrap();
    let text = decls.describe(&first);
    assert_eq!(text, decls.describe(&second));
    assert!(text.starts_with("; cb$fn$app$f\n"), "{text}");
    // D-913: the foreign symbol through its import word.
    assert!(text.contains("= cb$imp$cheby_io_print"), "{text}");
    assert!(text.contains("= cb$str$fn$app$f$1"), "{text}");
    assert!(text.contains(" tail {"), "{text}");
    let compiled = compile_function(Module::isa(&m), first).unwrap();
    assert_ne!(compiled.bytes.len(), 0);
    assert_eq!(compiled.align % FUNCTION_ALIGN, 0);
}

/// Declaring the same function twice is an ICE, not a silent redefinition.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_function_is_declared_once() {
    let mut m = module();
    let mut decls = Decls::new(Module::isa(&m)).unwrap();
    let units = [unit()];
    let cx = CodegenCx {
        layouts: &BuiltinLayouts,
    };
    let opts = CodegenOptions::default();
    declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
    let err = declare(&mut m, &mut decls, &units, &cx, &opts).unwrap_err();
    assert!(matches!(err, CodegenError::Ice(_)), "{err}");
}

#[test]
fn builtin_layout_ids() {
    use cheby_runtime::abi::layout;
    let ids = BuiltinLayouts;
    assert_eq!(
        ids.id(&cheby_mir::LayoutRef::Ctor(cheby_iface::builtin::NIL)),
        Ok(layout::NIL)
    );
    assert_eq!(
        ids.id(&cheby_mir::LayoutRef::Builtin(
            cheby_mir::BuiltinLayout::String
        )),
        Ok(layout::STRING)
    );
    assert!(ids.id(&cheby_mir::LayoutRef::Ctor(def(1, 0))).is_err());
}

/// `g<index>() { call_foreign "labs"(params...) ; return Nil }`: a wrapper
/// of `labs` with the C parameters `params`, as item `index` of `app`.
fn labs_unit(index: u32, params: &[CType]) -> MirModule {
    let g = def(1, index);
    let block = Block {
        pos: Vec::new(),
        params: Vec::new(),
        stmts: vec![Stmt::Eval(cheby_mir::Rvalue::CallForeign {
            symbol: "labs".into(),
            sig: ForeignSig {
                params: params.to_vec(),
                ret: CType::Void,
            },
            args: params
                .iter()
                .map(|_| {
                    Operand::Const(Const::Sized {
                        repr: Repr::I64,
                        bits: 0,
                    })
                })
                .collect(),
            blocking: false,
        })],
        term: Terminator::Return(Operand::Const(Const::Nil)),
        unwind: Vec::new(),
    };
    let item = format!("g{index}");
    MirModule {
        externals: Vec::new(),
        source: None,
        unit: UnitKind::Code(ModuleId::from_u32(1)),
        path: "app".into(),
        functions: vec![Function {
            positions: cheby_mir::Positions::default(),
            key: FnKey::Item(g),
            abi: Abi::Cheby,
            params: Vec::new(),
            ret: Repr::Value,
            locals: IndexVec::from_raw(Vec::new()).unwrap(),
            blocks: IndexVec::from_raw(vec![block]).unwrap(),
            span: Span::new(FileId::new(0), TextRange::new(0, 0)),
            strings: IndexVec::from_raw(vec!["a.cheby:1:4".into()]).unwrap(),
            entry_loc: Some(StrId::from_u32(0)),
            entry_unwind: None,
            precounted: false,
            borrow: cheby_mir::borrow::BorrowMask::OWNED,
            sites: Vec::new(),
            call_locs: Vec::new(),
            call_loc_strings: IndexVec::new(),
        }],
        callees: vec![CalleeDecl {
            def: g,
            sig: AbiSig::owned(Abi::Cheby, Vec::new(), Repr::Value),
        }],
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: vec![DefName {
            def: g,
            module: "app".into(),
            item: item.as_str().into(),
        }],
    }
}

/// Under `ForeignImport::Address` (the JIT's mode) a foreign symbol is a
/// signature-free data import, whose address its import word holds
/// (D-913), called with `call_indirect`, so later
/// batches may call it with another C signature, as a REPL redefinition
/// may (D-455 counts only the current definitions). Under
/// `ForeignImport::Function` a module keeps one signature per symbol.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn foreign_imports_by_address_allow_new_signatures() {
    let cx = CodegenCx {
        layouts: &BuiltinLayouts,
    };
    let jit = CodegenOptions {
        capture_clif: true,
        ..CodegenOptions::default()
    };
    assert_eq!(jit.foreign_import, ForeignImport::Address);
    let mut m = module();
    let mut decls = Decls::new(Module::isa(&m)).unwrap();
    for (index, params) in [(0, &[CType::I64][..]), (1, &[CType::I64, CType::I64][..])] {
        let units = [labs_unit(index, params)];
        declare(&mut m, &mut decls, &units, &cx, &jit).unwrap();
        let clif = define(&mut m, &units, &mut decls, &cx, &jit).unwrap();
        let text = clif.join("\n");
        // D-913: the address loaded from the import word.
        assert!(text.contains("= cb$imp$labs"), "{text}");
        assert!(text.contains("symbol_value"), "{text}");
        assert!(text.contains("load.i64 notrap aligned readonly"), "{text}");
        assert!(text.contains("call_indirect"), "{text}");
    }
    assert_eq!(decls.foreign_symbols(), vec!["labs"]);

    let aot = CodegenOptions {
        call_mode: CallMode::Direct,
        foreign_import: ForeignImport::Function,
        ..CodegenOptions::default()
    };
    let mut m = module();
    let mut decls = Decls::new(Module::isa(&m)).unwrap();
    declare(
        &mut m,
        &mut decls,
        &[labs_unit(0, &[CType::I64])],
        &cx,
        &aot,
    )
    .unwrap();
    let err = declare(
        &mut m,
        &mut decls,
        &[labs_unit(1, &[CType::I64, CType::I64])],
        &cx,
        &aot,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("two different signatures"),
        "{err}"
    );
}

/// `g<index>(params…) { _r = call_foreign blocking "<symbol>"(params…) ;
/// unwind bb1; _b = box _r ; unwind bb1; return _b }`, `bb1: resume`,
/// precounted with its own edges, as the unwind pass would give an
/// `@blocking` wrapper whose parameters are unboxed (D-818).
fn blocking_fn(index: u32, symbol: &str, params: &[CType], ret: CType) -> MirModule {
    use cheby_mir::{BlockId, Local, Rvalue};
    let g = def(1, index);
    let param_reprs: Vec<Repr> = params.iter().map(|c| c.repr().unwrap()).collect();
    let ret_repr = ret.repr().unwrap();
    let n = u32::try_from(params.len()).unwrap();
    let boxed = n.checked_add(1).unwrap();
    let locals = IndexVec::from_raw(
        param_reprs
            .iter()
            .copied()
            .chain([ret_repr, Repr::Value])
            .map(|repr| LocalDecl {
                repr,
                name: None,
                ty: None,
            })
            .collect(),
    )
    .unwrap();
    let bb0 = Block {
        params: Vec::new(),
        stmts: vec![
            Stmt::Assign {
                dst: Local::from_u32(n),
                value: Rvalue::CallForeign {
                    symbol: symbol.into(),
                    sig: ForeignSig {
                        params: params.to_vec(),
                        ret,
                    },
                    args: (0..n).map(|p| Operand::Local(Local::from_u32(p))).collect(),
                    blocking: true,
                },
            },
            Stmt::Assign {
                dst: Local::from_u32(boxed),
                value: Rvalue::Box {
                    value: Operand::Local(Local::from_u32(n)),
                    from: ret_repr,
                },
            },
        ],
        term: Terminator::Return(Operand::Local(Local::from_u32(boxed))),
        unwind: vec![(0, BlockId::from_u32(1)), (1, BlockId::from_u32(1))],
        pos: Vec::new(),
    };
    let resume = Block {
        pos: Vec::new(),
        params: Vec::new(),
        stmts: Vec::new(),
        term: Terminator::Resume,
        unwind: Vec::new(),
    };
    let function = Function {
        key: FnKey::Item(g),
        abi: Abi::Cheby,
        params: (0..n).map(Local::from_u32).collect(),
        ret: Repr::Value,
        locals,
        blocks: IndexVec::from_raw(vec![bb0, resume]).unwrap(),
        positions: cheby_mir::Positions::default(),
        span: Span::new(FileId::new(0), TextRange::new(0, 0)),
        strings: IndexVec::from_raw(vec!["a.cheby:1:4".into()]).unwrap(),
        entry_loc: Some(StrId::from_u32(0)),
        entry_unwind: Some(BlockId::from_u32(1)),
        precounted: true,
        borrow: cheby_mir::borrow::BorrowMask::OWNED,
        sites: Vec::new(),
        call_locs: Vec::new(),
        call_loc_strings: IndexVec::new(),
    };
    MirModule {
        externals: Vec::new(),
        source: None,
        unit: UnitKind::Code(ModuleId::from_u32(1)),
        path: "app".into(),
        functions: vec![function],
        callees: vec![CalleeDecl {
            def: g,
            sig: AbiSig::owned(Abi::Cheby, param_reprs, Repr::Value),
        }],
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: vec![DefName {
            def: g,
            module: "app".into(),
            item: format!("g{index}").into(),
        }],
    }
}

/// D-818: an `@blocking` call spills its arguments to a stack
/// array and calls `cheby_rt_blocking_call` with the address of the shim
/// `cb$blk$<symbol>`, then tests the flag at `ctx+12`, never the result
/// (D-734); the shim loads each argument by its C type and stores the
/// result word. One shim serves every call of a symbol and C signature,
/// across batches; a later signature of the symbol (a signature-free
/// import, as a REPL redefinition may give) gets the next number. A unit
/// without a blocking call never declares `cheby_rt_blocking_call`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn blocking_calls_go_through_one_shim_per_symbol_and_signature() {
    let cx = CodegenCx {
        layouts: &BuiltinLayouts,
    };
    let opts = CodegenOptions {
        capture_clif: true,
        ..CodegenOptions::default()
    };
    let mut m = module();
    let mut decls = Decls::new(Module::isa(&m)).unwrap();
    let int_float = [CType::I64, CType::F64];
    let units = [
        blocking_fn(0, "kern", &int_float, CType::I64),
        blocking_fn(1, "kern", &int_float, CType::I64),
    ];
    declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
    let clif = define(&mut m, &units, &mut decls, &cx, &opts).unwrap();
    assert_eq!(clif.len(), 3, "two functions and one shim: {clif:#?}");
    let load_flag = format!(
        "load.i32 notrap aligned v0+{}",
        cheby_runtime::abi::CTX_UNWIND
    );
    for text in &clif[..2] {
        for needle in [
            "= cb$imp$cheby_rt_blocking_call",
            "= cb$blk$kern\n",
            "func_addr",
            "stack_addr",
            "bitcast.i64",
        ] {
            assert!(text.contains(needle), "{needle:?} in:\n{text}");
        }
        // Two words: the `Int` at offset 0, the `Float`'s bits at 8.
        assert!(text.contains("stack_addr.i64 ss0+8"), "{text}");
        assert!(!text.contains("ss0+16"), "{text}");
        // The edge after the blocking call tests the flag, and so does
        // the entry check's releasing slow path, which releases nothing
        // (D-775); the box tests its own result.
        assert_eq!(text.matches(&load_flag).count(), 2, "{text}");
        assert!(
            !text.contains("= kern"),
            "the function calls the shim:\n{text}"
        );
    }
    let shim = &clif[2];
    assert!(shim.starts_with("; cb$blk$kern\n"), "{shim}");
    for needle in [
        "= cb$imp$kern",
        "load.i64",
        "load.f64",
        "call_indirect",
        "store",
    ] {
        assert!(shim.contains(needle), "{needle:?} in:\n{shim}");
    }
    assert!(
        !shim.contains("cheby_rt_"),
        "the shim calls only the kernel:\n{shim}"
    );

    // A later batch: the same symbol and signature reuse the shim; another
    // signature gets `cb$blk$kern$$1`.
    let units = [
        blocking_fn(2, "kern", &int_float, CType::I64),
        blocking_fn(3, "kern", &[CType::F32], CType::F32),
    ];
    declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
    let clif = define(&mut m, &units, &mut decls, &cx, &opts).unwrap();
    assert_eq!(clif.len(), 3, "two functions and one new shim: {clif:#?}");
    assert!(clif[0].contains("= cb$blk$kern\n"), "{}", clif[0]);
    assert!(clif[1].contains("= cb$blk$kern$$1\n"), "{}", clif[1]);
    assert!(clif[2].starts_with("; cb$blk$kern$$1\n"), "{}", clif[2]);
    assert!(clif[2].contains("load.f32"), "{}", clif[2]);

    // No blocking call, no `cheby_rt_blocking_call`.
    let mut m = module();
    let mut decls = Decls::new(Module::isa(&m)).unwrap();
    let units = [unit()];
    declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
    let text = define(&mut m, &units, &mut decls, &cx, &opts)
        .unwrap()
        .join("\n");
    assert!(!text.contains("blocking"), "{text}");
}

/// The blocking shim's name (D-456: the kind first).
#[test]
fn blocking_shim_names() {
    assert_eq!(mangle::blocking_shim("read_all", 0), "cb$blk$read_all");
    assert_eq!(mangle::blocking_shim("read_all", 2), "cb$blk$read_all$$2");
    assert_eq!(Kind::Blk.word(), "blk");
}

/// The runtime functions of the slice (plan §2.1) and the entry check's
/// releasing slow paths (D-775): their C types, indexes
/// 24 to 28, the helpers they implement, and the static `Err(Nil)`.
#[test]
fn slice_runtime_functions() {
    use cheby_mir::RuntimeFn;
    use runtime::{RuntimeFunc, STATIC_DATA};
    // `ctx`, the location and four words: six, which System V x86-64
    // passes in registers.
    let releasing = [CType::Ptr; 6];
    let expected = [
        (
            RuntimeFunc::StackSlowUnwind,
            27,
            &releasing[..],
            CType::Void,
        ),
        (
            RuntimeFunc::StackSlowFrameUnwind,
            28,
            &releasing[..],
            CType::Void,
        ),
        (
            RuntimeFunc::StackSlowFrame,
            24,
            &[CType::Ptr, CType::Ptr, CType::U64][..],
            CType::Void,
        ),
        (
            RuntimeFunc::ScopeOpen,
            25,
            &[CType::Ptr, CType::Bool, CType::I64, CType::I64][..],
            CType::U64,
        ),
        (
            RuntimeFunc::ScopeClose,
            26,
            &[CType::Ptr, CType::U64, CType::Bool][..],
            CType::Bool,
        ),
        // D-818: `cheby_rt_blocking_call(ctx, shim, args, nargs)`.
        (
            RuntimeFunc::BlockingCall,
            29,
            &[CType::Ptr, CType::Ptr, CType::Ptr, CType::U64][..],
            CType::U64,
        ),
    ];
    for (f, index, params, ret) in expected {
        assert_eq!(f.index(), index, "{f:?}");
        assert_eq!(RuntimeFunc::ALL[index], f);
        assert_eq!(f.ctypes(), (params, ret), "{f:?}");
        assert!(f.on_demand(), "{f:?}");
    }
    assert_eq!(RuntimeFunc::ALL.len(), 58);
    assert_eq!(
        RuntimeFunc::ALL.iter().filter(|f| f.on_demand()).count(),
        34,
        "only the functions added since M2 are declared on demand"
    );
    // The lookups taken apart at once and `list::set` at a direct call.
    for (f, index, helper, takes_ctx) in [
        (RuntimeFunc::MapFind, 39, RuntimeFn::MapFind, true),
        (RuntimeFunc::MapValueAt, 40, RuntimeFn::MapValueAt, false),
        (RuntimeFunc::ListSet, 41, RuntimeFn::ListSet, true),
    ] {
        assert_eq!(f.index(), index, "{f:?}");
        assert!(f.on_demand(), "{f:?}");
        assert_eq!(RuntimeFunc::of(helper), (f, takes_ctx), "{f:?}");
    }
    assert_eq!(
        RuntimeFunc::MapFind.ctypes(),
        (&[CType::Ptr, CType::U64, CType::U64][..], CType::U64)
    );
    assert_eq!(
        RuntimeFunc::MapValueAt.ctypes(),
        (&[CType::U64][..], CType::U64)
    );
    assert_eq!(
        RuntimeFunc::ListSet.ctypes(),
        (
            &[CType::Ptr, CType::U64, CType::I64, CType::U64][..],
            CType::U64
        )
    );
    // `cheby_rt_list_prepend(ctx, list, elem)`, as `cheby_rt_list_push`.
    assert_eq!(RuntimeFunc::ListPrepend.index(), 38);
    assert_eq!(
        RuntimeFunc::ListPrepend.ctypes(),
        RuntimeFunc::ListPush.ctypes()
    );
    assert!(RuntimeFunc::ListPrepend.on_demand() && !RuntimeFunc::ListPush.on_demand());
    assert_eq!(
        RuntimeFunc::of(RuntimeFn::ListPrepend),
        (RuntimeFunc::ListPrepend, true)
    );
    assert_eq!(
        RuntimeFunc::of(RuntimeFn::ScopeOpen),
        (RuntimeFunc::ScopeOpen, true)
    );
    assert_eq!(
        RuntimeFunc::of(RuntimeFn::ScopeClose),
        (RuntimeFunc::ScopeClose, true)
    );
    assert!(STATIC_DATA.contains(&cheby_runtime::abi::sym::STATIC_ERR_NIL));
}

/// The steps of std's stepped intrinsics (`cheby_mir`'s
/// `lower/intrinsic.rs`): indexes 50 to 57, declared on demand; a pair's
/// reads are `map_value_at`'s, and the loop's check is the entry check's
/// slow path.
#[test]
fn step_runtime_functions() {
    use cheby_mir::RuntimeFn;
    use runtime::RuntimeFunc;
    for (f, index, helper, takes_ctx) in [
        (RuntimeFunc::SortStart, 50, RuntimeFn::SortStart, true),
        (
            RuntimeFunc::HeapListStart,
            51,
            RuntimeFn::HeapListStart,
            true,
        ),
        (RuntimeFunc::BufferStep, 52, RuntimeFn::BufferStep, false),
        (RuntimeFunc::BufferFinish, 53, RuntimeFn::BufferFinish, true),
        (RuntimeFunc::HeapPush, 54, RuntimeFn::HeapPush, true),
        (RuntimeFunc::HeapPop, 55, RuntimeFn::HeapPop, true),
        (RuntimeFunc::HeapHeapify, 56, RuntimeFn::HeapHeapify, true),
        (RuntimeFunc::HeapStep, 57, RuntimeFn::HeapStep, true),
    ] {
        assert_eq!(f.index(), index, "{f:?}");
        assert!(f.on_demand(), "{f:?}");
        assert_eq!(RuntimeFunc::of(helper), (f, takes_ctx), "{f:?}");
    }
    assert_eq!(
        RuntimeFunc::of(RuntimeFn::PairLeft),
        (RuntimeFunc::MapValueAt, false)
    );
    assert_eq!(
        RuntimeFunc::of(RuntimeFn::LoopCheck),
        (RuntimeFunc::StackSlow, true)
    );
}

/// The slow paths of inline allocations and sized frees, and the cold drop
/// of a shared object on a reuse's path (`crate::alloc_inline`): saving
/// stubs at indexes 42 to 45, declared on demand, like the entry check's
/// slow paths and the count of a reuse hit, which is a saving stub too.
#[test]
fn inline_allocation_runtime_functions() {
    use runtime::RuntimeFunc;
    let sized = &[CType::Ptr, CType::U64, CType::U64][..];
    for (f, index, params) in [
        (RuntimeFunc::AllocSlow, 42, sized),
        (RuntimeFunc::AllocMissSlow, 43, sized),
        (RuntimeFunc::FreeUniqueSlow, 44, sized),
        (RuntimeFunc::DropSaving, 45, &[CType::Ptr, CType::Ptr][..]),
    ] {
        assert_eq!(f.index(), index, "{f:?}");
        assert_eq!(RuntimeFunc::ALL[index], f);
        assert_eq!(f.ctypes(), (params, CType::Void), "{f:?}");
        assert!(f.on_demand() && f.preserves_all(), "{f:?}");
    }
    assert!(RuntimeFunc::ReuseHit.preserves_all());
    assert!(!RuntimeFunc::Alloc.preserves_all() && !RuntimeFunc::FreeUnique.preserves_all());
}

/// Reuse's runtime entries (D-716), inline and checked, and fusion's
/// `cheby_rt_free_unique` (D-850): their C types and indexes
/// 30 to 35, all declared on demand, and the one size function's
/// constants, which MIR mirrors from the runtime.
#[test]
fn reuse_runtime_functions() {
    use runtime::{ON_DEMAND_DATA, RuntimeFunc};
    let reuse = [
        (
            RuntimeFunc::ReleaseFields,
            30,
            &[CType::Ptr, CType::U64][..],
            CType::Void,
        ),
        (RuntimeFunc::ReuseHit, 31, &[][..], CType::Void),
        (
            RuntimeFunc::AllocMiss,
            32,
            &[CType::Ptr, CType::U64, CType::U64][..],
            CType::U64,
        ),
        (
            RuntimeFunc::DropReuseChecked,
            33,
            &[CType::Ptr, CType::U64, CType::U64][..],
            CType::U64,
        ),
        (
            RuntimeFunc::AllocReuseChecked,
            34,
            &[CType::Ptr, CType::U64, CType::U64, CType::U64][..],
            CType::U64,
        ),
        (
            RuntimeFunc::FreeUnique,
            35,
            &[CType::Ptr, CType::U64, CType::U64][..],
            CType::Void,
        ),
    ];
    for (f, index, params, ret) in reuse {
        assert_eq!(f.index(), index, "{f:?}");
        assert_eq!(RuntimeFunc::ALL[index], f);
        assert_eq!(f.ctypes(), (params, ret), "{f:?}");
        assert!(f.on_demand(), "{f:?}");
    }
    // The counting flag is data imported on demand (D-768).
    assert_eq!(ON_DEMAND_DATA, [cheby_runtime::abi::sym::STATS_COUNTING]);
    // The one size function of reuse is MIR's, with the runtime's rounding.
    assert_eq!(
        cheby_mir::layout::OBJECT_ALIGN,
        cheby_runtime::abi::OBJECT_ALIGN
    );
    assert_eq!(
        cheby_mir::layout::SCAN_MAX,
        u64::from(cheby_runtime::abi::SCAN_MAX)
    );
}

/// `f(_0: bool)` with every IR form of the slice: helper calls with
/// unwind edges to a pad `bb2` that normal control flow never reaches,
/// a foreign call with `ctx` first, `clear_unwind`, `resume` and the
/// static `Err(Nil)`. The validator would not accept `clear_unwind` and
/// `resume` on the normal path; code generation does not care.
#[expect(clippy::too_many_lines, reason = "one hand-built MIR function")]
fn slice_forms_unit() -> MirModule {
    use cheby_mir::{BlockId, Local, RuntimeFn, Rvalue};
    let f = def(1, 0);
    let local = Local::from_u32;
    let locals = IndexVec::from_raw(
        [Repr::Bool, Repr::Value, Repr::Bool, Repr::Int, Repr::Value]
            .into_iter()
            .map(|repr| LocalDecl {
                repr,
                name: None,
                ty: None,
            })
            .collect(),
    )
    .unwrap();
    let runtime = |func, args| Rvalue::CallRuntime { func, args };
    let bb0 = Block {
        pos: Vec::new(),
        params: Vec::new(),
        stmts: vec![
            Stmt::Assign {
                dst: local(1),
                value: runtime(
                    RuntimeFn::ScopeOpen,
                    vec![
                        Operand::Local(local(0)),
                        Operand::Const(Const::Int(1)),
                        Operand::Const(Const::Int(2)),
                    ],
                ),
            },
            Stmt::Assign {
                dst: local(2),
                value: runtime(
                    RuntimeFn::ScopeClose,
                    vec![Operand::Local(local(1)), Operand::Local(local(0))],
                ),
            },
            Stmt::Assign {
                dst: local(3),
                value: Rvalue::CallForeign {
                    symbol: "cheby_test_kernel".into(),
                    sig: ForeignSig {
                        params: vec![CType::Ptr, CType::I64],
                        ret: CType::I64,
                    },
                    args: vec![Operand::Const(Const::Ctx), Operand::Const(Const::Int(5))],
                    blocking: false,
                },
            },
            Stmt::ClearUnwind,
        ],
        term: Terminator::Branch {
            cond: Operand::Local(local(2)),
            then_: BlockId::from_u32(1),
            else_: BlockId::from_u32(3),
        },
        unwind: vec![(0, BlockId::from_u32(2)), (1, BlockId::from_u32(2))],
    };
    let bb1 = Block {
        pos: Vec::new(),
        params: Vec::new(),
        stmts: Vec::new(),
        term: Terminator::Return(Operand::Const(Const::ErrNil)),
        unwind: Vec::new(),
    };
    let pad = Block {
        pos: Vec::new(),
        params: Vec::new(),
        stmts: vec![Stmt::Assign {
            dst: local(4),
            value: runtime(RuntimeFn::Debug, vec![Operand::Const(Const::Nil)]),
        }],
        term: Terminator::Return(Operand::Local(local(4))),
        unwind: Vec::new(),
    };
    let bb3 = Block {
        pos: Vec::new(),
        params: Vec::new(),
        stmts: Vec::new(),
        term: Terminator::Resume,
        unwind: Vec::new(),
    };
    let function = Function {
        key: FnKey::Item(f),
        abi: Abi::Cheby,
        params: vec![local(0)],
        ret: Repr::Value,
        locals,
        blocks: IndexVec::from_raw(vec![bb0, bb1, pad, bb3]).unwrap(),
        positions: cheby_mir::Positions::default(),
        span: Span::new(FileId::new(0), TextRange::new(0, 0)),
        strings: IndexVec::from_raw(vec!["a.cheby:1:4".into()]).unwrap(),
        entry_loc: Some(StrId::from_u32(0)),
        entry_unwind: Some(BlockId::from_u32(2)),
        precounted: true,
        borrow: cheby_mir::borrow::BorrowMask::OWNED,
        sites: Vec::new(),
        call_locs: Vec::new(),
        call_loc_strings: IndexVec::new(),
    };
    MirModule {
        externals: Vec::new(),
        source: None,
        unit: UnitKind::Code(ModuleId::from_u32(1)),
        path: "app".into(),
        functions: vec![function],
        callees: vec![CalleeDecl {
            def: f,
            sig: AbiSig::owned(Abi::Cheby, vec![Repr::Bool], Repr::Value),
        }],
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: vec![DefName {
            def: f,
            module: "app".into(),
            item: "f".into(),
        }],
    }
}

/// Code generation of the slice's IR forms (plan §2.2) under the return
/// flag (D-733): the pad that only the edges reach is translated as a cold
/// block; `resume` returns 0; the two helper calls with an edge and the
/// entry check's slow path load the flag; `clear_unwind` stores a zero
/// `u32` at `ctx+12`; `static Err(Nil)` is the runtime's data symbol; the
/// scope helpers are declared on demand with their C signatures; the
/// result compiles.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn slice_forms_translate_with_their_edges() {
    let mut m = module();
    let mut decls = Decls::new(Module::isa(&m)).unwrap();
    let units = [slice_forms_unit()];
    let cx = CodegenCx {
        layouts: &BuiltinLayouts,
    };
    let opts = CodegenOptions::default();
    declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
    let clif = translate(&units[0], &units[0].functions[0], &decls, &cx, &opts).unwrap();
    let text = decls.describe(&clif);
    for needle in [
        "= cb$imp$cheby_rt_scope_open",
        "= cb$imp$cheby_rt_scope_close",
        "= cb$imp$cheby_rt_static_err_nil",
        "= cb$imp$cheby_test_kernel",
        "= cb$imp$cheby_rt_debug",
        "iconst.i32 0",
        "iconst.i64 0",
        " cold:",
        &format!("+{}", cheby_runtime::abi::CTX_UNWIND),
    ] {
        assert!(text.contains(needle), "{needle:?} in:\n{text}");
    }
    assert!(text.contains("store"), "{text}");
    let load_flag = format!(
        "load.i32 notrap aligned v0+{}",
        cheby_runtime::abi::CTX_UNWIND
    );
    assert_eq!(text.matches(&load_flag).count(), 3, "{text}");
    assert!(!text.contains("trap user"), "no trap is left:\n{text}");
    let compiled = compile_function(Module::isa(&m), clif).unwrap();
    assert_ne!(compiled.bytes.len(), 0);
}

/// The scope helpers, declared on demand, do not shift the function ids
/// of a unit that does not use them, so its CLIF stays as before.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn units_without_the_slice_forms_keep_their_function_ids() {
    let mut m = module();
    let mut decls = Decls::new(Module::isa(&m)).unwrap();
    let units = [unit()];
    let cx = CodegenCx {
        layouts: &BuiltinLayouts,
    };
    let opts = CodegenOptions::default();
    declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
    let clif = translate(&units[0], &units[0].functions[0], &decls, &cx, &opts).unwrap();
    let text = decls.describe(&clif);
    let first_own = runtime::RuntimeFunc::ALL
        .iter()
        .filter(|f| !f.on_demand())
        .count();
    assert!(
        text.contains(&format!("function u0:{first_own}(")),
        "{text}"
    );
    assert!(!text.contains("scope_"), "{text}");
}

/// The default options (D-679, D-742): the code is compiled with unwind
/// edges and the large-frame threshold is 64 KiB; `compile_function`
/// reports the frame size Cranelift laid out, which is small for a small
/// function.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn default_options_and_the_frame_size() {
    let opts = CodegenOptions::default();
    assert!(opts.unwind_edges);
    assert_eq!(opts.large_frame_threshold, 64 << 10);
    assert_eq!(LARGE_FRAME_THRESHOLD, 64 << 10);
    let mut m = module();
    let mut decls = Decls::new(Module::isa(&m)).unwrap();
    let units = [unit()];
    let cx = CodegenCx {
        layouts: &BuiltinLayouts,
    };
    declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
    let clif = translate(&units[0], &units[0].functions[0], &decls, &cx, &opts).unwrap();
    let compiled = compile_function(Module::isa(&m), clif).unwrap();
    assert!(compiled.frame_size < 1 << 10, "{compiled:?}");
}

/// Under the return flag (D-733), the pads of the unwind pass (D-698) are
/// translated as cold blocks: the foreign call with an edge loads the flag
/// and branches to them, the entry check's releasing slow path branches on
/// its result to `resume` (D-775), the pad drops out of
/// line and `resume` returns 0. The same function without the pads has
/// none of it.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_pads_are_translated_as_cold_blocks() {
    let cx = CodegenCx {
        layouts: &BuiltinLayouts,
    };
    let opts = CodegenOptions::default();
    let text_of = |units: [MirModule; 1]| {
        let mut m = module();
        let mut decls = Decls::new(Module::isa(&m)).unwrap();
        declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
        let clif = translate(&units[0], &units[0].functions[0], &decls, &cx, &opts).unwrap();
        let text = decls.describe(&clif);
        compile_function(Module::isa(&m), clif).unwrap();
        text
    };
    let plain = text_of([unit()]);
    let mut padded = unit();
    let function = &mut padded.functions[0];
    // bb1: drop _0; goto bb2   bb2: resume
    function
        .blocks
        .push(Block {
            pos: Vec::new(),
            params: Vec::new(),
            stmts: vec![Stmt::Drop(cheby_mir::Local::from_u32(0))],
            term: Terminator::Goto {
                target: cheby_mir::BlockId::from_u32(2),
                args: Vec::new(),
            },
            unwind: Vec::new(),
        })
        .unwrap();
    function
        .blocks
        .push(Block {
            pos: Vec::new(),
            params: Vec::new(),
            stmts: Vec::new(),
            term: Terminator::Resume,
            unwind: Vec::new(),
        })
        .unwrap();
    function.blocks.iter_mut().next().unwrap().unwind = vec![(0, cheby_mir::BlockId::from_u32(1))];
    function.entry_unwind = Some(cheby_mir::BlockId::from_u32(2));
    let padded_text = text_of([padded]);
    let load_flag = format!(
        "load.i32 notrap aligned v0+{}",
        cheby_runtime::abi::CTX_UNWIND
    );
    assert_eq!(plain.matches(&load_flag).count(), 0, "{plain}");
    // The foreign call's, and the entry check's: it goes through the
    // releasing slow path, which returns nothing, so the flag tells
    // (D-775).
    assert_eq!(padded_text.matches(&load_flag).count(), 2, "{padded_text}");
    assert!(
        padded_text.contains("= cb$imp$cheby_rt_stack_slow_unwind"),
        "{padded_text}"
    );
    // The pad's drop is a plain call: one more call than the inline drop's
    // slow path makes. Runtime calls go through import words (D-913).
    assert_eq!(
        padded_text.matches("call_indirect").count(),
        plain.matches("call_indirect").count() + 1,
        "{padded_text}"
    );
    assert!(padded_text.contains("iconst.i64 0"), "{padded_text}");
    assert!(
        padded_text.matches(" cold:").count() > plain.matches(" cold:").count(),
        "the pads are cold:\n{padded_text}"
    );
    assert!(!padded_text.contains("trap user"), "{padded_text}");
}

/// `f(_0)` with an unwind edge on each kind of statement the return flag
/// lowers (D-698, D-733): a Cheby call, a checked addition, a foreign
/// call, an allocation and a `panic` terminator, the chained pads `bb1`
/// (`drop _1`) and `bb2` (`drop _0`, the entry pad), and `bb3: resume`.
fn edges_unit() -> MirModule {
    use cheby_mir::{ArithOp, BlockId, Callee, LayoutRef, Local, PanicKind, Rvalue};
    let mut u = unit();
    let f = u.functions[0].key;
    let FnKey::Item(f_def) = f else {
        panic!("unit() has an item")
    };
    let local = Local::from_u32;
    let function = &mut u.functions[0];
    function.locals = IndexVec::from_raw(
        [Repr::Value, Repr::Value, Repr::Int, Repr::Value]
            .into_iter()
            .map(|repr| LocalDecl {
                repr,
                name: None,
                ty: None,
            })
            .collect(),
    )
    .unwrap();
    let goto = |n| Terminator::Goto {
        target: BlockId::from_u32(n),
        args: Vec::new(),
    };
    let bb0 = Block {
        pos: Vec::new(),
        params: Vec::new(),
        stmts: vec![
            Stmt::Dup(local(0)),
            Stmt::Assign {
                dst: local(1),
                value: Rvalue::Call {
                    callee: Callee::Direct(f_def),
                    args: vec![Operand::Local(local(0))],
                },
            },
            Stmt::Assign {
                dst: local(2),
                value: Rvalue::Checked {
                    op: ArithOp::Add,
                    lhs: Operand::Const(Const::Int(1)),
                    rhs: Operand::Const(Const::Int(2)),
                    repr: Repr::Int,
                    loc: StrId::from_u32(0),
                },
            },
            Stmt::Eval(Rvalue::CallForeign {
                symbol: "cheby_io_print".into(),
                sig: ForeignSig {
                    params: vec![CType::Ptr],
                    ret: CType::Void,
                },
                args: vec![Operand::Local(local(1))],
                blocking: false,
            }),
            Stmt::Assign {
                dst: local(3),
                value: Rvalue::Alloc {
                    layout: LayoutRef::Ctor(cheby_iface::builtin::SOME),
                    fields: vec![Operand::Local(local(1))],
                },
            },
        ],
        term: Terminator::Panic {
            kind: PanicKind::Overflow,
            message: None,
            loc: StrId::from_u32(0),
        },
        unwind: vec![
            (1, BlockId::from_u32(2)),
            (2, BlockId::from_u32(1)),
            (3, BlockId::from_u32(1)),
            (4, BlockId::from_u32(1)),
            (5, BlockId::from_u32(1)),
        ],
    };
    let pad = |drop: u32, term| Block {
        pos: Vec::new(),
        params: Vec::new(),
        stmts: vec![Stmt::Drop(local(drop))],
        term,
        unwind: Vec::new(),
    };
    function.blocks = IndexVec::from_raw(vec![
        bb0,
        pad(1, goto(2)),
        pad(0, goto(3)),
        Block {
            pos: Vec::new(),
            params: Vec::new(),
            stmts: Vec::new(),
            term: Terminator::Resume,
            unwind: Vec::new(),
        },
    ])
    .unwrap();
    function.entry_unwind = Some(BlockId::from_u32(2));
    u
}

/// The CLIF text of `units[0]`'s first function under `opts`, checked to
/// compile.
fn clif_of(units: &[MirModule; 1], opts: &CodegenOptions) -> String {
    let cx = CodegenCx {
        layouts: &BuiltinLayouts,
    };
    let mut m = module();
    let mut decls = Decls::new(Module::isa(&m)).unwrap();
    declare(&mut m, &mut decls, units, &cx, opts).unwrap();
    let clif = translate(&units[0], &units[0].functions[0], &decls, &cx, opts).unwrap();
    let again = translate(&units[0], &units[0].functions[0], &decls, &cx, opts).unwrap();
    let text = decls.describe(&clif);
    assert_eq!(text, decls.describe(&again), "translation is deterministic");
    compile_function(Module::isa(&m), clif).unwrap();
    text
}

/// `edges_unit()` without its unwind edges and its entry pad, as the MIR
/// is under `CHEBY_UNWIND_EDGES=off` (D-742): its pads are unreachable.
fn edges_unit_without_edges() -> MirModule {
    let mut u = edges_unit();
    let function = &mut u.functions[0];
    for block in &mut function.blocks {
        block.unwind.clear();
    }
    function.entry_unwind = None;
    u
}

/// The return flag (D-733, architecture §6.3): with edges, the pads are
/// translated as cold blocks whose drops call `cheby_rt_drop`; a C call
/// loads the flag at `ctx+12`; a Cheby call and `cheby_rt_alloc` test
/// their result; the panic block jumps to its pad instead of trapping;
/// `resume` returns 0; the entry check's slow path tests the flag. The
/// same MIR without edges lowers with none of it: no flag load, no pad,
/// and a trap after each `cheby_rt_panic`.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn the_flag_lowers_every_edge() {
    let opts = CodegenOptions::default();
    let flag = clif_of(&[edges_unit()], &opts);
    let edgeless = clif_of(&[edges_unit_without_edges()], &opts);
    let load_flag = format!(
        "load.i32 notrap aligned v0+{}",
        cheby_runtime::abi::CTX_UNWIND
    );
    // Without edges: the code of a build without the unwind pass.
    assert!(!edgeless.contains(&load_flag), "{edgeless}");
    assert!(
        !edgeless.contains("= cb$imp$cheby_rt_drop"),
        "the pads were translated:\n{edgeless}"
    );
    assert!(
        edgeless.contains(&format!("trap user{}", func::TRAP_NORETURN)),
        "{edgeless}"
    );
    // With edges.
    let loads = flag.matches(&load_flag).count();
    // The foreign call and the entry check's releasing slow path (D-775),
    // which returns nothing; the Cheby call, the allocation and the panic
    // do not load the flag.
    assert_eq!(loads, 2, "{flag}");
    assert!(
        flag.contains("= cb$imp$cheby_rt_stack_slow_unwind"),
        "{flag}"
    );
    assert!(flag.contains(" cold:"), "{flag}");
    assert!(
        !flag.contains(&format!("trap user{}", func::TRAP_NORETURN)),
        "the panic blocks jump to their pads:\n{flag}"
    );
    assert!(flag.contains("iconst.i64 0"), "{flag}");
    // Pads drop out of line: no inline drop's `rc > 1` test.
    assert!(!flag.contains("icmp_imm sgt"), "{flag}");
    // The two panics (the checked add's and the terminator's) and the
    // two calls whose result is tested branch to a pad: six `brif`s on
    // top of the dup's two and the entry check's two (its compare and
    // its helper's result), the add's overflow test and the flag load.
    assert!(flag.matches("brif").count() >= 6, "{flag}");
}

/// `f(_0: Int, _1 … _n: Value)`, whose body drops its counted parameters
/// and returns `Nil`, given its entry pad by the unwind pass (D-698).
fn counted_params_unit(n: u32) -> MirModule {
    let mut u = unit();
    let local = cheby_mir::Local::from_u32;
    let function = &mut u.functions[0];
    let reprs = std::iter::once(Repr::Int).chain((0..n).map(|_| Repr::Value));
    function.locals = IndexVec::from_raw(
        reprs
            .clone()
            .map(|repr| LocalDecl {
                repr,
                name: None,
                ty: None,
            })
            .collect(),
    )
    .unwrap();
    function.params = (0..=n).map(local).collect();
    function.blocks = IndexVec::from_raw(vec![Block {
        pos: Vec::new(),
        params: Vec::new(),
        stmts: (1..=n).map(|l| Stmt::Drop(local(l))).collect(),
        term: Terminator::Return(Operand::Const(Const::Nil)),
        unwind: Vec::new(),
    }])
    .unwrap();
    cheby_mir::unwind::insert(function).unwrap();
    u.callees[0].sig.params = reprs.collect();
    u
}

/// D-775: the entry check's slow path of a function
/// with at most four counted parameters is `call
/// cheby_rt_stack_slow_unwind(ctx, loc, p…, 0…); v = load.i32 ctx+12;
/// brif v, resume, bb0`, six arguments with 0 for each word the function
/// lacks, with no continuation block and no translated
/// entry pad chain; with five, it calls `cheby_rt_stack_slow`, loads the
/// flag and translates the chain, whose drops call `cheby_rt_drop` out of
/// line. Both helpers are called with `preserve_all` where the runtime's
/// stubs save every register, and return nothing.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn entry_unwind_through_the_releasing_slow_path() {
    let opts = CodegenOptions::default();
    let load_flag = format!(
        "load.i32 notrap aligned v0+{}",
        cheby_runtime::abi::CTX_UNWIND
    );
    for n in [0, 1, 4] {
        let text = clif_of(&[counted_params_unit(n)], &opts);
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        let at = lines
            .iter()
            .position(|line| line.starts_with("call_indirect") && line.contains("(v0, "))
            .unwrap_or_else(|| panic!("{n}: no helper call in:\n{text}"));
        let call = lines[at];
        let flag = lines[at + 1]
            .strip_suffix(&format!(" = {load_flag}"))
            .unwrap_or_else(|| panic!("{n}: the flag is loaded after the call:\n{text}"));
        // `ctx`, the location, then the parameters after `ctx` and the
        // `Int`, `v2` on, in the entry pad's order, and one zero for the
        // unused words.
        let (head, consts) = call.split_once("  ; ").unwrap_or((call, ""));
        let args: Vec<&str> = head
            .rsplit_once('(')
            .and_then(|(_, rest)| rest.strip_suffix(')'))
            .unwrap_or_else(|| panic!("{n}: {call}"))
            .split(", ")
            .collect();
        assert_eq!(args.len(), 6, "{n}: {call}");
        assert_eq!(args[0], "v0", "{n}: {call}");
        let words: Vec<String> = (0..n).map(|i| format!("v{}", i + 2)).collect();
        let used = usize::try_from(n).unwrap().checked_add(2).unwrap();
        let mut passed = args[2..used].to_vec();
        passed.sort_unstable();
        assert_eq!(passed, words, "{n}: {call}");
        let zero = format!("{} = 0", args[5]);
        assert!(
            n == 4 || (args[used..].iter().all(|a| *a == args[5]) && consts.contains(&zero)),
            "{n}: {call}\n{text}"
        );
        assert!(
            lines[at + 2].starts_with(&format!("brif {flag}, block"))
                && lines[at + 2].ends_with(", block0"),
            "{n}: the flag goes to `resume` or the body:\n{text}"
        );
        if cfg!(all(
            any(target_arch = "aarch64", target_arch = "x86_64"),
            not(windows)
        )) {
            assert!(text.contains(") preserve_all\n"), "{n}: {text}");
        }
        assert!(
            text.lines()
                .any(|l| l.ends_with("= cb$imp$cheby_rt_stack_slow_unwind")),
            "{n}: {text}"
        );
        assert!(
            !text
                .lines()
                .any(|l| l.ends_with("= cb$imp$cheby_rt_stack_slow")),
            "{n}: {text}"
        );
        assert_eq!(text.matches(&load_flag).count(), 1, "{n}: {text}");
        // Only the shared `resume` of the entry pad chain is translated:
        // its `return 0` is the only return of 0 besides the body's.
        assert_eq!(text.matches("return").count(), 2, "{n}: {text}");
    }
    let five = clif_of(&[counted_params_unit(5)], &opts);
    assert!(
        five.lines()
            .any(|l| l.ends_with("= cb$imp$cheby_rt_stack_slow")),
        "{five}"
    );
    assert!(!five.contains("stack_slow_unwind"), "{five}");
    assert_eq!(five.matches(&load_flag).count(), 1, "{five}");
    assert!(
        five.lines().any(|l| l.ends_with("= cb$imp$cheby_rt_drop")),
        "{five}"
    );
}

/// A pad is translated even when only an unwind edge reaches it, and
/// reverse postorder puts it after the block whose statement unwinds to
/// it: a pad's drop of a local defined in that block compiles (the
/// verifier checks dominance).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn flag_pads_follow_their_sources() {
    let opts = CodegenOptions {
        checked_rc: true,
        ..CodegenOptions::default()
    };
    let text = clif_of(&[edges_unit()], &opts);
    // Both pads and `resume` are there: two out-of-line drops in the pads.
    assert!(text.contains("= cb$imp$cheby_rt_drop"), "{text}");
    // Without the stack check there is no entry pad, and the function
    // still compiles.
    let unchecked = CodegenOptions {
        stack_check: false,
        ..opts
    };
    let text = clif_of(&[edges_unit()], &unchecked);
    assert_eq!(
        text.matches(&format!(
            "load.i32 notrap aligned v0+{}",
            cheby_runtime::abi::CTX_UNWIND
        ))
        .count(),
        1,
        "{text}"
    );
}

/// D-772: `cheby_rt_drop` takes the function's `ctx` first,
/// so that the free path reads the block cache through the control block
/// (D-739). Every call of it passes the entry block's `ctx`: the pads'
/// drops, and with `checked_rc` every drop.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn drops_pass_their_functions_ctx() {
    let cx = CodegenCx {
        layouts: &BuiltinLayouts,
    };
    for checked_rc in [false, true] {
        let opts = CodegenOptions {
            checked_rc,
            ..CodegenOptions::default()
        };
        let units = [edges_unit()];
        let mut m = module();
        let mut decls = Decls::new(Module::isa(&m)).unwrap();
        declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
        let (_, signature) = decls.runtime(runtime::RuntimeFunc::Drop).unwrap();
        assert_eq!(signature.params.len(), 2, "{signature}");
        assert!(signature.returns.is_empty(), "{signature}");
        let clif = translate(&units[0], &units[0].functions[0], &decls, &cx, &opts).unwrap();
        let text = decls.describe(&clif);
        // D-913: the drop is called through its import word, whose loaded
        // address each call names.
        let drops = word_loads(&text, "cb$imp$cheby_rt_drop");
        assert!(
            !drops.is_empty(),
            "no reference to `cheby_rt_drop`:\n{text}"
        );
        // The entry block's first parameter is `ctx`.
        let ctx = text
            .lines()
            .find_map(|line| {
                let params = line.strip_prefix("block")?.split_once('(')?.1;
                Some(params.split_once(':')?.0.to_owned())
            })
            .unwrap();
        let calls: Vec<(&str, &str)> = text
            .lines()
            .map(str::trim)
            .filter_map(|line| {
                let rest = line.strip_prefix("call_indirect ")?;
                let (_, rest) = rest.split_once(", ")?;
                let (callee, args) = rest.split_once('(')?;
                drops.contains(callee).then_some((line, args))
            })
            .collect();
        assert!(calls.len() >= 2, "checked_rc {checked_rc}:\n{text}");
        for (line, args) in calls {
            assert!(
                args.starts_with(&format!("{ctx}, ")),
                "checked_rc {checked_rc}: {line}\n{text}"
            );
        }
    }
}

/// The values that hold the address loaded from the import word `word`
/// in the CLIF `text` (D-913): `vA = symbol_value.i64 gvN` of the word's
/// global value, then `vB = load.i64 notrap aligned readonly vA`.
fn word_loads(text: &str, word: &str) -> std::collections::BTreeSet<String> {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let name = lines
        .iter()
        .find_map(|line| {
            let (name, symbol) = line.strip_prefix("; ")?.split_once(" = ")?;
            (symbol == word).then_some(name)
        })
        .unwrap_or_else(|| panic!("no `{word}` in:\n{text}"));
    let gv = lines
        .iter()
        .find_map(|line| {
            let (gv, rest) = line.split_once(" = ")?;
            (rest == format!("symbol colocated {name}")).then_some(gv)
        })
        .unwrap_or_else(|| panic!("no global value of `{word}` in:\n{text}"));
    let mut addresses = std::collections::BTreeSet::new();
    for pair in lines.windows(2) {
        let [at, load] = pair else { continue };
        let Some((at, _)) = at
            .split_once(" = ")
            .filter(|(_, rest)| *rest == format!("symbol_value.i64 {gv}"))
        else {
            continue;
        };
        let Some((address, _)) = load
            .split_once(" = ")
            .filter(|(_, rest)| *rest == format!("load.i64 notrap aligned readonly {at}"))
        else {
            continue;
        };
        addresses.insert(address.to_owned());
    }
    addresses
}

/// `f() { _0 = call g() unwind bb1; return nil }  bb1: resume`, and
/// `g() -> ret { return 0 }`, both Cheby functions: `f` calls `g`, whose
/// MIR result is `ret`, with an unwind edge.
#[expect(clippy::too_many_lines, reason = "two hand-built MIR functions")]
fn unboxed_call_unit(ret: Repr) -> MirModule {
    use cheby_mir::{BlockId, Callee, Local, Rvalue};
    let f = def(1, 0);
    let g = def(1, 1);
    let decl = |repr| LocalDecl {
        repr,
        name: None,
        ty: None,
    };
    let span = Span::new(FileId::new(0), TextRange::new(0, 0));
    let zero = if ret == Repr::Value {
        Const::Nil
    } else {
        Const::Int(0)
    };
    let caller = Function {
        positions: cheby_mir::Positions::default(),
        key: FnKey::Item(f),
        abi: Abi::Cheby,
        params: Vec::new(),
        ret: Repr::Value,
        locals: IndexVec::from_raw(vec![decl(ret)]).unwrap(),
        blocks: IndexVec::from_raw(vec![
            Block {
                pos: Vec::new(),
                params: Vec::new(),
                stmts: vec![Stmt::Assign {
                    dst: Local::from_u32(0),
                    value: Rvalue::Call {
                        callee: Callee::Direct(g),
                        args: Vec::new(),
                    },
                }],
                term: Terminator::Return(Operand::Const(Const::Nil)),
                unwind: vec![(0, BlockId::from_u32(1))],
            },
            Block {
                pos: Vec::new(),
                params: Vec::new(),
                stmts: Vec::new(),
                term: Terminator::Resume,
                unwind: Vec::new(),
            },
        ])
        .unwrap(),
        span,
        strings: IndexVec::from_raw(vec!["a.cheby:1:4".into()]).unwrap(),
        entry_loc: Some(StrId::from_u32(0)),
        entry_unwind: Some(BlockId::from_u32(1)),
        precounted: false,
        borrow: cheby_mir::borrow::BorrowMask::OWNED,
        sites: Vec::new(),
        call_locs: Vec::new(),
        call_loc_strings: IndexVec::new(),
    };
    let g_fn = Function {
        positions: cheby_mir::Positions::default(),
        key: FnKey::Item(g),
        abi: Abi::Cheby,
        params: Vec::new(),
        ret,
        locals: IndexVec::from_raw(Vec::new()).unwrap(),
        blocks: IndexVec::from_raw(vec![Block {
            pos: Vec::new(),
            params: Vec::new(),
            stmts: Vec::new(),
            term: Terminator::Return(Operand::Const(zero)),
            unwind: Vec::new(),
        }])
        .unwrap(),
        span,
        strings: IndexVec::from_raw(vec!["a.cheby:2:4".into()]).unwrap(),
        entry_loc: Some(StrId::from_u32(0)),
        entry_unwind: None,
        precounted: false,
        borrow: cheby_mir::borrow::BorrowMask::OWNED,
        sites: Vec::new(),
        call_locs: Vec::new(),
        call_loc_strings: IndexVec::new(),
    };
    let sig = |ret| AbiSig::owned(Abi::Cheby, Vec::new(), ret);
    MirModule {
        externals: Vec::new(),
        source: None,
        unit: UnitKind::Code(ModuleId::from_u32(1)),
        path: "app".into(),
        functions: vec![caller, g_fn],
        callees: vec![
            CalleeDecl {
                def: f,
                sig: sig(Repr::Value),
            },
            CalleeDecl {
                def: g,
                sig: sig(ret),
            },
        ],
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: vec![
            DefName {
                def: f,
                module: "app".into(),
                item: "f".into(),
            },
            DefName {
                def: g,
                module: "app".into(),
                item: "g".into(),
            },
        ],
    }
}

/// The CLIF line of the call of `g` in `text`, its result, and the line
/// after it: the first `call_indirect` whose callee is not an address
/// loaded from an import word (a runtime call, D-913).
fn call_and_next(text: &str) -> (String, String, String) {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let imported: Vec<&str> = lines
        .iter()
        .filter(|line| line.contains("= load.i64 notrap aligned readonly "))
        .filter_map(|line| line.split(" = ").next())
        .collect();
    let at = lines
        .iter()
        .position(|line| {
            line.contains("= call_indirect")
                && !imported
                    .iter()
                    .any(|value| line.contains(&format!(", {value}(")))
        })
        .unwrap_or_else(|| panic!("no call in:\n{text}"));
    let result = lines[at].split(" = ").next().unwrap().to_owned();
    let next = lines[at.checked_add(1).unwrap()].to_owned();
    (lines[at].to_owned(), result, next)
}

/// D-734: a call of a Cheby function whose result is unboxed (an entry
/// point of D-453, here an `Int`, which is an `i64` like a uniform word)
/// with an unwind edge loads the flag at `ctx+12` right after the call and
/// never branches on the result, which can be 0 legitimately. A call of
/// the uniform-word entry keeps the result test (D-733).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unboxed_result_call_tests_the_flag_not_the_result() {
    let load_flag = format!(
        "load.i32 notrap aligned v0+{}",
        cheby_runtime::abi::CTX_UNWIND
    );
    let opts = CodegenOptions::default();
    let unboxed = clif_of(&[unboxed_call_unit(Repr::Int)], &opts);
    let (call, result, next) = call_and_next(&unboxed);
    assert!(call.contains("call_indirect"), "{unboxed}");
    assert!(
        next.ends_with(&load_flag),
        "the flag is loaded right after `{call}`:\n{unboxed}"
    );
    assert!(
        !unboxed.contains(&format!("brif {result},")),
        "the unboxed result {result} is never tested:\n{unboxed}"
    );
    // The call and the entry check's releasing slow path, which returns
    // nothing (D-775).
    assert_eq!(unboxed.matches(&load_flag).count(), 2, "{unboxed}");
    let uniform = clif_of(&[unboxed_call_unit(Repr::Value)], &opts);
    let (_, result, next) = call_and_next(&uniform);
    assert!(
        next.starts_with(&format!("brif {result},")),
        "a uniform word is tested:\n{uniform}"
    );
    assert_eq!(uniform.matches(&load_flag).count(), 1, "{uniform}");
}

/// `f(x) { t = alloc_placeholder tuple/<arity>; init_field t.<last> = x;
/// return t }`, without counts or edges.
fn placeholder_unit(arity: u32) -> MirModule {
    use cheby_mir::{LayoutRef, Local, Rvalue};
    let mut u = unit();
    let function = &mut u.functions[0];
    function.locals = IndexVec::from_raw(
        [Repr::Value, Repr::Value]
            .into_iter()
            .map(|repr| LocalDecl {
                repr,
                name: None,
                ty: None,
            })
            .collect(),
    )
    .unwrap();
    let layout = LayoutRef::Tuple(arity);
    function.blocks = IndexVec::from_raw(vec![Block {
        pos: Vec::new(),
        params: Vec::new(),
        stmts: vec![
            Stmt::Assign {
                dst: Local::from_u32(1),
                value: Rvalue::AllocPlaceholder { layout },
            },
            Stmt::Eval(Rvalue::InitField {
                obj: Local::from_u32(1),
                layout,
                field: arity.checked_sub(1).unwrap(),
                value: Operand::Local(Local::from_u32(0)),
            }),
        ],
        term: Terminator::Return(Operand::Local(Local::from_u32(1))),
        unwind: Vec::new(),
    }])
    .unwrap();
    u
}

/// D-720's placeholder tuple (D-810): one `cheby_rt_alloc`
/// and a loop that stores the static `Nil` in every field, whatever the
/// arity, then a plain store of the field at its offset, after the
/// escaped count above 254 fields (D-279, D-555).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn placeholder_tuples_fill_in_a_loop_and_store_fields_in_place() {
    let opts = CodegenOptions::default();
    let small = clif_of(&[placeholder_unit(40)], &opts);
    let large = clif_of(&[placeholder_unit(100_000)], &opts);
    for text in [&small, &large] {
        assert_eq!(text.matches("icmp ule").count(), 1, "{text}");
        assert!(text.contains(cheby_runtime::abi::sym::STATIC_NIL), "{text}");
    }
    // The same code for 40 fields and for 100k, but for the constants and
    // the escaped count's constant and store at +8.
    assert_eq!(
        small.lines().count().checked_add(2),
        Some(large.lines().count()),
        "{small}\n{large}"
    );
    // The object is `v12`: the stack check's slow path and the allocation
    // each load their import word first (D-913).
    assert!(
        !small.contains(", v12+8 ") && large.contains(", v12+8 "),
        "{large}"
    );
    // Field 39 of 40 at 8 + 39 * 8; field 99999 of 100k after the count,
    // at 8 + 8 + 99999 * 8 = 0xc3508.
    assert!(small.contains("v1, v12+320\n"), "{small}");
    assert!(large.contains("v1, v12+0x000c_3508\n"), "{large}");
}

/// `f(xs) { n = list_length(xs); rest = list_uncons(xs, 0, 1, n); x =
/// list_uncons_head(); ... }`, counts left out (D-909, D-910).
fn uncons_unit() -> MirModule {
    use cheby_mir::{Local, RuntimeFn, Rvalue};
    let mut u = unit();
    let function = &mut u.functions[0];
    function.locals = IndexVec::from_raw(
        [Repr::Value, Repr::Int, Repr::Value, Repr::Value]
            .into_iter()
            .map(|repr| LocalDecl {
                repr,
                name: None,
                ty: None,
            })
            .collect(),
    )
    .unwrap();
    let l = Local::from_u32;
    let call = |dst: u32, func: RuntimeFn, args: Vec<Operand>| Stmt::Assign {
        dst: l(dst),
        value: Rvalue::CallRuntime { func, args },
    };
    function.blocks = IndexVec::from_raw(vec![Block {
        pos: Vec::new(),
        params: Vec::new(),
        stmts: vec![
            call(1, RuntimeFn::ListLength, vec![Operand::Local(l(0))]),
            call(
                2,
                RuntimeFn::ListUncons,
                vec![
                    Operand::Local(l(0)),
                    Operand::Const(Const::Int(0)),
                    Operand::Const(Const::Int(1)),
                    Operand::Local(l(1)),
                ],
            ),
            call(3, RuntimeFn::ListUnconsHead, Vec::new()),
        ],
        term: Terminator::Return(Operand::Local(l(3))),
        unwind: Vec::new(),
    }])
    .unwrap();
    u
}

/// `f(xs) { n = list_length(xs); m = n - 1 (as a constant here: the
/// length itself); rest = list_uncons_owned(xs, at, start, end); x =
/// list_uncons_head(); ... }` for the step that takes the first
/// (`front`) or the last element, as pattern lowering and the RC pass
/// leave it.
fn owned_step_unit(front: bool) -> MirModule {
    use cheby_mir::{Local, RuntimeFn, Rvalue};
    let mut u = uncons_unit();
    let l = Local::from_u32;
    let (at, start) = if front {
        (Operand::Const(Const::Int(0)), 1)
    } else {
        (Operand::Local(l(1)), 0)
    };
    let step = Stmt::Assign {
        dst: l(2),
        value: Rvalue::CallRuntime {
            func: RuntimeFn::ListUnconsOwned,
            args: vec![
                Operand::Local(l(0)),
                at,
                Operand::Const(Const::Int(start)),
                Operand::Local(l(1)),
            ],
        },
    };
    u.functions[0].blocks.iter_mut().next().unwrap().stmts[1] = step;
    u
}

/// An owned pattern step at either end of a list is written inline for a
/// unique list whose leaf at that end is at hand, with the helper's call
/// in a cold block for every other list: it tests the root's header
/// against the two unique roots', the leaf's layout and the counting
/// flag, then takes the element into the step's stack slot and writes the
/// position and the length. The element moves out of a leaf whose header
/// is a unique leaf's, and is dup'd from one that another list holds, a
/// walk over a list the caller still uses. With `checked_rc` only the
/// call is left.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn owned_list_steps_are_written_inline() {
    use cheby_runtime::abi::{list, sym};
    for front in [true, false] {
        let text = clif_of(&[owned_step_unit(front)], &CodegenOptions::default());
        for header in [list::UNIQUE_ROOT, list::UNIQUE_BIG_ROOT, list::UNIQUE_LEAF] {
            // As CLIF prints a constant: hexadecimal, in groups of four.
            let digits = format!("{header:x}");
            let grouped: Vec<&str> = digits
                .as_bytes()
                .rchunks(4)
                .rev()
                .map(|group| std::str::from_utf8(group).unwrap())
                .collect();
            let constant = format!("iconst.i64 0x{}", grouped.join("_"));
            assert!(text.contains(&constant), "{front}: no {constant}\n{text}");
        }
        assert!(text.contains(sym::STATS_COUNTING), "{front}\n{text}");
        // The leaf's layout, read from its header, and the dup of an
        // element under a shared leaf, whose slow path is a call.
        let shift = cheby_runtime::abi::HEADER_LAYOUT_SHIFT;
        assert!(
            text.contains(&format!("iconst.i64 {shift}\n")),
            "{front}\n{text}"
        );
        assert!(
            text.contains(&format!("iconst.i64 {}\n", list::LEAF_LAYOUT)),
            "{front}\n{text}"
        );
        assert!(text.contains(sym::DUP), "{front}\n{text}");
        // One slot: the inline code stores the element there, the
        // helper gets its address in a cold block, and the head is read
        // back from it.
        assert_eq!(
            text.matches("explicit_slot 8").count(),
            1,
            "{front}\n{text}"
        );
        assert_eq!(
            text.matches("stack_addr.i64 ss0").count(),
            3,
            "{front}\n{text}"
        );
        assert!(text.contains(sym::LIST_UNCONS_OWNED), "{front}\n{text}");
        assert!(text.contains(" cold:"), "{front}\n{text}");
        // The front step stores the cleared word, a constant, in the
        // slot it empties; the back step shortens the leaf instead.
        assert_eq!(list::CLEARED, 1);
        let slots = list::LEAF_SLOTS_OFFSET;
        let clears = text
            .lines()
            .any(|line| line.contains(&format!("+{slots}  ; ")) && line.ends_with(" = 1"));
        assert_eq!(clears, front, "{front}\n{text}");
        let checked = CodegenOptions {
            checked_rc: true,
            ..CodegenOptions::default()
        };
        let text = clif_of(&[owned_step_unit(front)], &checked);
        assert_eq!(
            text.matches("stack_addr.i64 ss0").count(),
            2,
            "{front}\n{text}"
        );
        assert!(!text.contains(sym::STATS_COUNTING), "{front}\n{text}");
        assert!(!text.contains(sym::DUP), "{front}\n{text}");
        assert!(text.contains(sym::LIST_UNCONS_OWNED), "{front}\n{text}");
    }
}

/// A list's length is a load of its root's length word, with no call
/// (D-910), and a pattern step passes the address of one stack slot that
/// the head is then loaded from (D-909).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn list_lengths_load_and_steps_pass_a_slot() {
    let text = clif_of(&[uncons_unit()], &CodegenOptions::default());
    let off = cheby_runtime::abi::list::ROOT_LEN_OFFSET;
    assert!(
        text.contains(&format!("load.i64 notrap aligned v1+{off}\n")),
        "{text}"
    );
    assert!(
        !text.contains(cheby_runtime::abi::sym::LIST_LENGTH),
        "{text}"
    );
    // One slot: its address is passed to the call, then read back.
    assert_eq!(text.matches("explicit_slot 8").count(), 1, "{text}");
    assert_eq!(text.matches("stack_addr.i64 ss0").count(), 2, "{text}");
    assert!(
        text.contains(cheby_runtime::abi::sym::LIST_UNCONS),
        "{text}"
    );
}

/// `f(xs) { x = helper(xs, args…); return x }`, counts left out: one call
/// of a list or map helper on the parameter.
fn index_unit(helper: cheby_mir::RuntimeFn, rest: Vec<Operand>, result: Repr) -> MirModule {
    use cheby_mir::{Local, Rvalue};
    let mut u = unit();
    let function = &mut u.functions[0];
    function.locals = IndexVec::from_raw(
        [Repr::Value, result]
            .into_iter()
            .map(|repr| LocalDecl {
                repr,
                name: None,
                ty: None,
            })
            .collect(),
    )
    .unwrap();
    let l = Local::from_u32;
    let mut args = vec![Operand::Local(l(0))];
    args.extend(rest);
    function.blocks = IndexVec::from_raw(vec![Block {
        pos: Vec::new(),
        params: Vec::new(),
        stmts: vec![Stmt::Assign {
            dst: l(1),
            value: Rvalue::CallRuntime { func: helper, args },
        }],
        term: Terminator::Return(Operand::Local(l(1))),
        unwind: Vec::new(),
    }])
    .unwrap();
    u
}

/// A header word as CLIF prints a constant: hexadecimal, in groups of
/// four.
fn clif_constant(word: u64) -> String {
    let digits = format!("{word:x}");
    let grouped: Vec<&str> = digits
        .as_bytes()
        .rchunks(4)
        .rev()
        .map(|group| std::str::from_utf8(group).unwrap())
        .collect();
    format!("iconst.i64 0x{}", grouped.join("_"))
}

/// `list_get` of a list that is one leaf is a load of the slot and an
/// inline `dup`, after tests of the root's and the leaf's layouts, with
/// the helper's call in a cold block for every other list; no count and
/// no counting flag is read. With `checked_rc` only the call is left.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn list_reads_at_an_index_are_written_inline() {
    use cheby_mir::RuntimeFn;
    use cheby_runtime::abi::{list, sym};
    let get = || {
        index_unit(
            RuntimeFn::ListGet,
            vec![Operand::Const(Const::Int(1))],
            Repr::Value,
        )
    };
    let text = clif_of(&[get()], &CodegenOptions::default());
    for offset in [list::ROOT_TREE_OFFSET, list::ROOT_OFFSET_OFFSET] {
        assert!(text.contains(&format!("+{offset}\n")), "{offset}\n{text}");
    }
    assert!(
        text.contains(&format!("+{}\n", list::LEAF_SLOTS_OFFSET)),
        "{text}"
    );
    assert!(text.contains(sym::LIST_GET), "{text}");
    assert!(text.contains(" cold:"), "{text}");
    assert!(!text.contains(sym::STATS_COUNTING), "{text}");
    assert!(
        !text.contains(&clif_constant(list::UNIQUE_ROOT)),
        "a read tests no count\n{text}"
    );
    let checked = CodegenOptions {
        checked_rc: true,
        ..CodegenOptions::default()
    };
    let text = clif_of(&[get()], &checked);
    assert!(text.contains(sym::LIST_GET), "{text}");
    assert!(
        !text.contains(&format!("+{}\n", list::ROOT_OFFSET_OFFSET)),
        "{text}"
    );
}

/// `owned_step_unit` with the head read as a number of `repr`
/// (`list_uncons_head_float`), boxed again to be returned.
fn scalar_step_unit(front: bool, repr: Repr) -> MirModule {
    use cheby_mir::{Local, RuntimeFn, Rvalue};
    let mut u = owned_step_unit(front);
    let l = Local::from_u32;
    let function = &mut u.functions[0];
    function.locals.get_mut(l(3)).unwrap().repr = repr;
    function
        .locals
        .push(LocalDecl {
            repr: Repr::Value,
            name: None,
            ty: None,
        })
        .unwrap();
    let block = function.blocks.iter_mut().next().unwrap();
    block.stmts[2] = Stmt::Assign {
        dst: l(3),
        value: Rvalue::CallRuntime {
            func: RuntimeFn::as_scalar(RuntimeFn::ListUnconsHead, repr).unwrap(),
            args: Vec::new(),
        },
    };
    block.stmts.push(Stmt::Assign {
        dst: l(4),
        value: Rvalue::Box {
            value: Operand::Local(l(3)),
            from: repr,
        },
    });
    block.term = Terminator::Return(Operand::Local(l(4)));
    u
}

/// `f(xs) { x = list_get_float(xs, 1); return box x }`, or its `F32`
/// form.
fn scalar_index_unit(repr: Repr) -> MirModule {
    use cheby_mir::{Local, RuntimeFn, Rvalue};
    let get = RuntimeFn::as_scalar(RuntimeFn::ListGet, repr).unwrap();
    let mut u = index_unit(get, vec![Operand::Const(Const::Int(1))], repr);
    let l = Local::from_u32;
    let function = &mut u.functions[0];
    function
        .locals
        .push(LocalDecl {
            repr: Repr::Value,
            name: None,
            ty: None,
        })
        .unwrap();
    let block = function.blocks.iter_mut().next().unwrap();
    block.stmts.push(Stmt::Assign {
        dst: l(2),
        value: Rvalue::Box {
            value: Operand::Local(l(1)),
            from: repr,
        },
    });
    block.term = Terminator::Return(Operand::Local(l(2)));
    u
}

/// The lines of `text` that load a number of `ty` (`f64`, `f32`), each
/// with the line before it.
fn number_loads<'t>(text: &'t str, ty: &str) -> Vec<(&'t str, &'t str)> {
    let lines: Vec<&str> = text.lines().collect();
    lines
        .windows(2)
        .filter(|pair| pair[1].contains(&format!("= load.{ty} ")))
        .map(|pair| (pair[0], pair[1]))
        .collect()
}

/// Whether `load`, a load of a number, reads through the value that
/// `before`, a load of a word, defines: the two loads are next to each
/// other, with no call and no yield point between them.
fn loads_through(before: &str, load: &str) -> bool {
    let defined = before.trim().split(' ').next().unwrap_or_default();
    (before.contains("= load.i64 ") || before.contains("= stack_load.i64 "))
        && load.contains(&format!(" {defined}+"))
}

/// A pattern step whose head reads the element as a number yields it from
/// the inline step: on every path the number is loaded right after the
/// word of its box, with nothing between the two loads. No path dups the
/// element: a shared leaf keeps its box and no count changes, and an
/// element moved out of a unique leaf is dropped after the read, as is
/// the word the helper writes to the slot. The slot is written only by
/// the helper and read only on its path. With `checked_rc` the head
/// reads the slot, unboxes and drops, as a head and its `unbox` and
/// `drop` would.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn float_steps_read_the_number_without_a_count() {
    use cheby_runtime::abi::sym;
    for (repr, ty) in [(Repr::Float, "f64"), (Repr::F32, "f32")] {
        for front in [true, false] {
            let text = clif_of(&[scalar_step_unit(front, repr)], &CodegenOptions::default());
            let loads = number_loads(&text, ty);
            // The inline paths, one for each kind of leaf at the back but
            // a shared load before the branch at the front, and the
            // helper's.
            assert_eq!(
                loads.len(),
                if front { 2 } else { 3 },
                "{front}
{text}"
            );
            for (before, load) in &loads {
                assert!(
                    loads_through(before, load),
                    "{before}
{load}
{text}"
                );
            }
            assert!(
                !text.contains(sym::DUP),
                "{front}
{text}"
            );
            assert!(
                text.contains(sym::DROP),
                "{front}
{text}"
            );
            // The helper gets the slot's address and its path reads the
            // word back through it; the inline paths store nothing there.
            assert_eq!(text.matches("stack_addr.i64 ss0").count(), 2, "{text}");
            assert!(!text.contains("stack_store"), "{text}");
            assert!(
                text.contains(sym::LIST_UNCONS_OWNED),
                "{front}
{text}"
            );
            let checked = CodegenOptions {
                checked_rc: true,
                ..CodegenOptions::default()
            };
            let text = clif_of(&[scalar_step_unit(front, repr)], &checked);
            let loads = number_loads(&text, ty);
            assert_eq!(
                loads.len(),
                1,
                "{front}
{text}"
            );
            assert!(loads_through(loads[0].0, loads[0].1), "{text}");
            assert!(
                text.contains(sym::DROP),
                "{front}
{text}"
            );
            assert!(
                !text.contains(sym::STATS_COUNTING),
                "{front}
{text}"
            );
        }
    }
}

/// `list_get_float` of a list that is one leaf loads the slot's word and
/// the number through it, with no count touched, and the helper's path
/// unboxes the word the helper returns and drops it. With `checked_rc`
/// only that path is left.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn float_index_reads_load_the_number() {
    use cheby_runtime::abi::{list, sym};
    for (repr, ty) in [(Repr::Float, "f64"), (Repr::F32, "f32")] {
        let text = clif_of(&[scalar_index_unit(repr)], &CodegenOptions::default());
        let loads = number_loads(&text, ty);
        assert_eq!(loads.len(), 2, "{text}");
        let (before, load) = loads[0];
        assert!(
            loads_through(before, load),
            "{before}
{load}
{text}"
        );
        assert!(
            before.contains(
                &format!(
                    "+{}
",
                    list::LEAF_SLOTS_OFFSET
                )
                .trim_end()
                .to_owned()
            ),
            "the slot's word
{text}"
        );
        assert!(!text.contains(sym::DUP), "{text}");
        assert!(text.contains(sym::LIST_GET), "{text}");
        assert!(
            text.contains(sym::DROP),
            "the helper's word
{text}"
        );
        let checked = CodegenOptions {
            checked_rc: true,
            ..CodegenOptions::default()
        };
        let text = clif_of(&[scalar_index_unit(repr)], &checked);
        assert_eq!(number_loads(&text, ty).len(), 1, "{text}");
        assert!(text.contains(sym::LIST_GET), "{text}");
        assert!(text.contains(sym::DROP), "{text}");
        assert!(
            !text.contains(&format!(
                "+{}
",
                list::ROOT_OFFSET_OFFSET
            )),
            "{text}"
        );
    }
}

/// `list_set` of a unique list that is one leaf stores the item and
/// releases the old element inline, after tests of the counting flag, the
/// root's and the leaf's headers and an unsigned bounds test, with the
/// helper's call in a cold block for every other list. With `checked_rc`
/// only the call is left.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn list_writes_at_an_index_are_written_inline() {
    use cheby_mir::RuntimeFn;
    use cheby_runtime::abi::{list, sym};
    let set = || {
        index_unit(
            RuntimeFn::ListSet,
            vec![Operand::Const(Const::Int(2)), Operand::Const(Const::Nil)],
            Repr::Value,
        )
    };
    let text = clif_of(&[set()], &CodegenOptions::default());
    for header in [list::UNIQUE_ROOT, list::UNIQUE_LEAF] {
        let constant = clif_constant(header);
        assert!(text.contains(&constant), "no {constant}\n{text}");
    }
    assert!(text.contains(sym::STATS_COUNTING), "{text}");
    assert!(text.contains("icmp ult"), "{text}");
    assert!(
        text.contains(&format!("+{}\n", list::LEAF_SLOTS_OFFSET)),
        "{text}"
    );
    assert!(text.contains("store notrap aligned"), "{text}");
    assert!(text.contains(sym::LIST_SET), "{text}");
    assert!(text.contains(" cold:"), "{text}");
    let checked = CodegenOptions {
        checked_rc: true,
        ..CodegenOptions::default()
    };
    let text = clif_of(&[set()], &checked);
    assert!(text.contains(sym::LIST_SET), "{text}");
    assert!(!text.contains(sym::STATS_COUNTING), "{text}");
    assert!(!text.contains("store notrap aligned"), "{text}");
}

/// The helpers of a direct `map::upsert`: `map_upsert` takes `ctx`, the map,
/// the key and the address of its word, its address step needs only its
/// declaration, `map_slot_take` takes the address alone, and the
/// `Option` and the store take `ctx` too.
#[test]
fn upsert_runtime_functions() {
    use cheby_mir::RuntimeFn;
    use runtime::RuntimeFunc;
    for (f, index, helper, takes_ctx) in [
        (RuntimeFunc::MapUpsert, 46, RuntimeFn::MapUpsert, true),
        (
            RuntimeFunc::MapUpsert,
            46,
            RuntimeFn::MapUpsertAddress,
            true,
        ),
        (RuntimeFunc::MapSlotTake, 47, RuntimeFn::MapSlotTake, false),
        (
            RuntimeFunc::MapUpsertFound,
            48,
            RuntimeFn::MapUpsertFound,
            true,
        ),
        (
            RuntimeFunc::MapUpsertStore,
            49,
            RuntimeFn::MapUpsertStore,
            true,
        ),
    ] {
        assert_eq!(f.index(), index, "{f:?}");
        assert!(f.on_demand(), "{f:?}");
        assert_eq!(RuntimeFunc::of(helper), (f, takes_ctx), "{f:?}");
    }
    assert_eq!(
        RuntimeFunc::MapUpsert.ctypes(),
        (
            &[CType::Ptr, CType::U64, CType::U64, CType::Ptr][..],
            CType::U64
        )
    );
    assert_eq!(
        RuntimeFunc::MapUpsertFound.ctypes(),
        (&[CType::Ptr, CType::U64][..], CType::U64)
    );
    assert_eq!(
        RuntimeFunc::MapUpsertStore.ctypes(),
        (
            &[CType::Ptr, CType::U64, CType::U64, CType::U64, CType::U64][..],
            CType::U64
        )
    );
}

/// The steps of an upsert after its `map_upsert`: the address is a load of
/// the stack slot whose address `map_upsert` was given, the value is moved
/// out with a load and a store, and the result is stored with a store and
/// the map given back, with the runtime's store called only in a cold block
/// for an empty map; with `checked_rc` the value is taken by the runtime's
/// call.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn upsert_steps_are_inline() {
    use cheby_mir::{Local, RuntimeFn, Rvalue};
    use cheby_runtime::abi::sym;
    let steps = || {
        let mut u = unit();
        let function = &mut u.functions[0];
        function.locals = IndexVec::from_raw(
            [
                Repr::Value,
                Repr::Value,
                Repr::U64,
                Repr::Value,
                Repr::Value,
            ]
            .into_iter()
            .map(|repr| LocalDecl {
                repr,
                name: None,
                ty: None,
            })
            .collect(),
        )
        .unwrap();
        let l = Local::from_u32;
        let call = |func, args| Rvalue::CallRuntime { func, args };
        let assign = |dst: u32, value| Stmt::Assign { dst: l(dst), value };
        function.blocks = IndexVec::from_raw(vec![Block {
            pos: Vec::new(),
            params: Vec::new(),
            stmts: vec![
                assign(
                    1,
                    call(
                        RuntimeFn::MapUpsert,
                        vec![Operand::Local(l(0)), Operand::Const(Const::Nil)],
                    ),
                ),
                assign(
                    2,
                    call(RuntimeFn::MapUpsertAddress, vec![Operand::Local(l(1))]),
                ),
                assign(
                    3,
                    call(
                        RuntimeFn::MapSlotTake,
                        vec![Operand::Local(l(1)), Operand::Local(l(2))],
                    ),
                ),
                assign(
                    4,
                    call(
                        RuntimeFn::MapUpsertStore,
                        vec![
                            Operand::Local(l(1)),
                            Operand::Local(l(2)),
                            Operand::Const(Const::Nil),
                            Operand::Local(l(3)),
                        ],
                    ),
                ),
            ],
            term: Terminator::Return(Operand::Local(l(4))),
            unwind: Vec::new(),
        }])
        .unwrap();
        u
    };
    let text = clif_of(&[steps()], &CodegenOptions::default());
    assert!(text.contains(sym::MAP_UPSERT), "{text}");
    assert!(text.contains("stack_addr"), "{text}");
    assert!(text.contains("band"), "{text}");
    assert!(text.contains("cold"), "{text}");
    assert!(!text.contains(sym::MAP_SLOT_TAKE), "{text}");
    let checked = CodegenOptions {
        checked_rc: true,
        ..CodegenOptions::default()
    };
    let text = clif_of(&[steps()], &checked);
    assert!(text.contains(sym::MAP_SLOT_TAKE), "{text}");
    assert!(text.contains(sym::MAP_UPSERT_STORE), "{text}");
}

/// `map_value_at` is a load at the address `map_find` gave and an inline
/// `dup`; with `checked_rc` it is the runtime's call of the address.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn map_values_are_read_inline() {
    use cheby_mir::RuntimeFn;
    use cheby_runtime::abi::sym;
    let read = || {
        index_unit(
            RuntimeFn::MapValueAt,
            vec![Operand::Const(Const::Sized {
                repr: Repr::U64,
                bits: 64,
            })],
            Repr::Value,
        )
    };
    let text = clif_of(&[read()], &CodegenOptions::default());
    assert!(!text.contains(sym::MAP_VALUE_AT), "{text}");
    assert!(text.contains(sym::DUP), "the slow path of the dup\n{text}");
    let checked = CodegenOptions {
        checked_rc: true,
        ..CodegenOptions::default()
    };
    let text = clif_of(&[read()], &checked);
    assert!(text.contains(sym::MAP_VALUE_AT), "{text}");
}
