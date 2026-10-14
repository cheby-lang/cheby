//! Tests of code generation.

use cranelift_codegen::ir::{ArgumentExtension, types};
use cranelift_codegen::isa::CallConv;
use cranelift_jit::{JITBuilder, JITModule};

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, IndexVec, ModuleId};
use cheby_mir::{
    Abi, AbiSig, Block, CType, CalleeDecl, Const, DefName, FnKey, ForeignSig, Function, LocalDecl,
    MirModule, Operand, Repr, Stmt, StrId, Terminator, UnitKind,
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
        let (f, takes_ctx) = RuntimeFunc::of(helper);
        let (params, ret) = f.ctypes();
        let ctx = usize::from(takes_ctx);
        assert_eq!(params.len(), helper.params().len() + ctx, "{helper:?}");
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
    let isa = cranelift_native::builder()
        .unwrap()
        .finish(isa::flags().unwrap())
        .unwrap();
    JITModule::new(JITBuilder::with_isa(
        isa,
        cranelift_module::default_libcall_names(),
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
        params: Vec::new(),
        stmts: vec![
            Stmt::Eval(cheby_mir::Rvalue::CallForeign {
                symbol: "cheby_io_print".into(),
                sig: ForeignSig {
                    params: vec![CType::Ptr],
                    ret: CType::Void,
                },
                args: vec![Operand::Local(cheby_mir::Local::from_u32(0))],
            }),
            Stmt::Drop(cheby_mir::Local::from_u32(0)),
        ],
        term: Terminator::Return(Operand::Const(Const::Str(StrId::from_u32(1)))),
        unwind: Vec::new(),
    };
    let function = Function {
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
    };
    MirModule {
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
    let mut decls = Decls::new(cranelift_module::Module::isa(&m)).unwrap();
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
    assert!(text.contains("= cheby_io_print"), "{text}");
    assert!(text.contains("= cb$str$fn$app$f$1"), "{text}");
    assert!(text.contains(" tail {"), "{text}");
    let compiled = compile_function(cranelift_module::Module::isa(&m), first).unwrap();
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
    let mut decls = Decls::new(cranelift_module::Module::isa(&m)).unwrap();
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
        })],
        term: Terminator::Return(Operand::Const(Const::Nil)),
        unwind: Vec::new(),
    };
    let item = format!("g{index}");
    MirModule {
        unit: UnitKind::Code(ModuleId::from_u32(1)),
        path: "app".into(),
        functions: vec![Function {
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
/// signature-free data import called with `call_indirect`, so later
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
    let mut decls = Decls::new(cranelift_module::Module::isa(&m)).unwrap();
    for (index, params) in [(0, &[CType::I64][..]), (1, &[CType::I64, CType::I64][..])] {
        let units = [labs_unit(index, params)];
        declare(&mut m, &mut decls, &units, &cx, &jit).unwrap();
        let clif = define(&mut m, &units, &mut decls, &cx, &jit).unwrap();
        let text = clif.join("\n");
        assert!(text.contains("= labs"), "{text}");
        assert!(text.contains("symbol_value"), "{text}");
        assert!(text.contains("call_indirect"), "{text}");
    }
    assert_eq!(decls.foreign_symbols(), vec!["labs"]);

    let aot = CodegenOptions {
        call_mode: CallMode::Direct,
        foreign_import: ForeignImport::Function,
        ..CodegenOptions::default()
    };
    let mut m = module();
    let mut decls = Decls::new(cranelift_module::Module::isa(&m)).unwrap();
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

/// The runtime functions of the slice (plan §2.1) and the entry check's
/// releasing slow paths (D-775): their C types, indexes
/// 24 to 28, the helpers they implement, and the static `Err(Nil)`.
#[test]
fn slice_runtime_functions() {
    use cheby_mir::RuntimeFn;
    use runtime::{RuntimeFunc, STATIC_DATA};
    let words = [CType::Ptr; 4];
    let plain: Vec<CType> = [CType::Ptr, CType::Ptr, CType::U32]
        .into_iter()
        .chain(words)
        .collect();
    let frame: Vec<CType> = [CType::Ptr, CType::Ptr, CType::U64, CType::U32]
        .into_iter()
        .chain(words)
        .collect();
    let expected = [
        (RuntimeFunc::StackSlowUnwind, 27, &plain[..], CType::U32),
        (
            RuntimeFunc::StackSlowFrameUnwind,
            28,
            &frame[..],
            CType::U32,
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
    ];
    for (f, index, params, ret) in expected {
        assert_eq!(f.index(), index, "{f:?}");
        assert_eq!(RuntimeFunc::ALL[index], f);
        assert_eq!(f.ctypes(), (params, ret), "{f:?}");
        assert!(f.on_demand(), "{f:?}");
    }
    assert_eq!(RuntimeFunc::ALL.len(), 29);
    assert_eq!(
        RuntimeFunc::ALL.iter().filter(|f| f.on_demand()).count(),
        5,
        "only the M2 functions are declared on demand"
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
        params: Vec::new(),
        stmts: Vec::new(),
        term: Terminator::Return(Operand::Const(Const::ErrNil)),
        unwind: Vec::new(),
    };
    let pad = Block {
        params: Vec::new(),
        stmts: vec![Stmt::Assign {
            dst: local(4),
            value: runtime(RuntimeFn::Debug, vec![Operand::Const(Const::Nil)]),
        }],
        term: Terminator::Return(Operand::Local(local(4))),
        unwind: Vec::new(),
    };
    let bb3 = Block {
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
        span: Span::new(FileId::new(0), TextRange::new(0, 0)),
        strings: IndexVec::from_raw(vec!["a.cheby:1:4".into()]).unwrap(),
        entry_loc: Some(StrId::from_u32(0)),
        entry_unwind: Some(BlockId::from_u32(2)),
        precounted: true,
        borrow: cheby_mir::borrow::BorrowMask::OWNED,
    };
    MirModule {
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
    let mut decls = Decls::new(cranelift_module::Module::isa(&m)).unwrap();
    let units = [slice_forms_unit()];
    let cx = CodegenCx {
        layouts: &BuiltinLayouts,
    };
    let opts = CodegenOptions::default();
    declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
    let clif = translate(&units[0], &units[0].functions[0], &decls, &cx, &opts).unwrap();
    let text = decls.describe(&clif);
    for needle in [
        "= cheby_rt_scope_open",
        "= cheby_rt_scope_close",
        "= cheby_rt_static_err_nil",
        "= cheby_test_kernel",
        "= cheby_rt_debug",
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
    let compiled = compile_function(cranelift_module::Module::isa(&m), clif).unwrap();
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
    let mut decls = Decls::new(cranelift_module::Module::isa(&m)).unwrap();
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
    let mut decls = Decls::new(cranelift_module::Module::isa(&m)).unwrap();
    let units = [unit()];
    let cx = CodegenCx {
        layouts: &BuiltinLayouts,
    };
    declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
    let clif = translate(&units[0], &units[0].functions[0], &decls, &cx, &opts).unwrap();
    let compiled = compile_function(cranelift_module::Module::isa(&m), clif).unwrap();
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
        let mut decls = Decls::new(cranelift_module::Module::isa(&m)).unwrap();
        declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
        let clif = translate(&units[0], &units[0].functions[0], &decls, &cx, &opts).unwrap();
        let text = decls.describe(&clif);
        compile_function(cranelift_module::Module::isa(&m), clif).unwrap();
        text
    };
    let plain = text_of([unit()]);
    let mut padded = unit();
    let function = &mut padded.functions[0];
    // bb1: drop _0; goto bb2   bb2: resume
    function
        .blocks
        .push(Block {
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
    // The foreign call's; the entry check goes through the releasing slow
    // path, which tests its own result (D-775).
    assert_eq!(padded_text.matches(&load_flag).count(), 1, "{padded_text}");
    assert!(
        padded_text.contains("= cheby_rt_stack_slow_unwind"),
        "{padded_text}"
    );
    // The pad's drop is a plain call: one more call than the inline drop's
    // slow path makes.
    assert_eq!(
        padded_text.matches("call fn").count(),
        plain.matches("call fn").count() + 1,
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
    let mut decls = Decls::new(cranelift_module::Module::isa(&m)).unwrap();
    declare(&mut m, &mut decls, units, &cx, opts).unwrap();
    let clif = translate(&units[0], &units[0].functions[0], &decls, &cx, opts).unwrap();
    let again = translate(&units[0], &units[0].functions[0], &decls, &cx, opts).unwrap();
    let text = decls.describe(&clif);
    assert_eq!(text, decls.describe(&again), "translation is deterministic");
    compile_function(cranelift_module::Module::isa(&m), clif).unwrap();
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
        !edgeless.contains("= cheby_rt_drop"),
        "the pads were translated:\n{edgeless}"
    );
    assert!(
        edgeless.contains(&format!("trap user{}", func::TRAP_NORETURN)),
        "{edgeless}"
    );
    // With edges.
    let loads = flag.matches(&load_flag).count();
    // The foreign call; the Cheby call, the allocation, the panic and the
    // entry check's releasing slow path (D-775) do not
    // load the flag.
    assert_eq!(loads, 1, "{flag}");
    assert!(flag.contains("= cheby_rt_stack_slow_unwind"), "{flag}");
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
/// with at most four counted parameters is `r = call
/// cheby_rt_stack_slow_unwind(ctx, loc, n, p…, 0…); brif r, resume, bb0`,
/// with no flag load, no continuation block and no translated entry pad
/// chain; with five, it calls `cheby_rt_stack_slow`, loads the flag and
/// translates the chain, whose drops call `cheby_rt_drop` out of line.
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
            .position(|line| line.contains("= call fn") && line.contains("(v0, "))
            .unwrap_or_else(|| panic!("{n}: no helper call in:\n{text}"));
        let call = lines[at];
        let result = call.split(" = ").next().unwrap();
        // The parameters after `ctx` and the `Int` are `v2` on, and the
        // unused words are one zero.
        let words: Vec<String> = (0..n).map(|i| format!("v{}", i + 2)).collect();
        assert!(
            words.iter().all(|word| call.contains(word.as_str())),
            "{n}: {call}"
        );
        assert!(
            lines[at + 1].starts_with(&format!("brif {result}, block"))
                && lines[at + 1].ends_with(", block0"),
            "{n}: the result goes to `resume` or the body:\n{text}"
        );
        assert!(
            text.contains(&format!("iconst.i32 {n}")),
            "{n}: the count:\n{text}"
        );
        assert!(
            text.lines()
                .any(|l| l.ends_with("= cheby_rt_stack_slow_unwind")),
            "{n}: {text}"
        );
        assert!(
            !text.lines().any(|l| l.ends_with("= cheby_rt_stack_slow")),
            "{n}: {text}"
        );
        assert!(!text.contains(&load_flag), "{n}: no flag load:\n{text}");
        // Only the shared `resume` of the entry pad chain is translated:
        // its `return 0` is the only return of 0 besides the body's.
        assert_eq!(text.matches("return").count(), 2, "{n}: {text}");
    }
    let five = clif_of(&[counted_params_unit(5)], &opts);
    assert!(
        five.lines().any(|l| l.ends_with("= cheby_rt_stack_slow")),
        "{five}"
    );
    assert!(!five.contains("stack_slow_unwind"), "{five}");
    assert_eq!(five.matches(&load_flag).count(), 1, "{five}");
    assert!(
        five.lines().any(|l| l.ends_with("= cheby_rt_drop")),
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
    assert!(text.contains("= cheby_rt_drop"), "{text}");
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
        let mut decls = Decls::new(cranelift_module::Module::isa(&m)).unwrap();
        declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
        let (id, signature) = decls.runtime(runtime::RuntimeFunc::Drop).unwrap();
        assert_eq!(signature.params.len(), 2, "{signature}");
        assert!(signature.returns.is_empty(), "{signature}");
        let clif = translate(&units[0], &units[0].functions[0], &decls, &cx, &opts).unwrap();
        let text = decls.describe(&clif);
        let drop_ref = format!(" = u0:{} sig", id.as_u32());
        let name = text
            .lines()
            .find_map(|line| {
                let (name, rest) = line.trim().split_once(" = ")?;
                format!(" = {rest}")
                    .starts_with(&drop_ref)
                    .then(|| name.to_owned())
            })
            .unwrap_or_else(|| panic!("no reference to `cheby_rt_drop`:\n{text}"));
        // The entry block's first parameter is `ctx`.
        let ctx = text
            .lines()
            .find_map(|line| {
                let params = line.strip_prefix("block")?.split_once('(')?.1;
                Some(params.split_once(':')?.0.to_owned())
            })
            .unwrap();
        let call = format!("call {name}(");
        let calls: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with(&call))
            .collect();
        assert!(calls.len() >= 2, "checked_rc {checked_rc}:\n{text}");
        for line in calls {
            assert!(
                line.starts_with(&format!("{call}{ctx}, ")),
                "checked_rc {checked_rc}: {line}\n{text}"
            );
        }
    }
}

/// `f() { _0 = call g() unwind bb1; return nil }  bb1: resume`, and
/// `g() -> ret { return 0 }`, both Cheby functions: `f` calls `g`, whose
/// MIR result is `ret`, with an unwind edge.
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
        key: FnKey::Item(f),
        abi: Abi::Cheby,
        params: Vec::new(),
        ret: Repr::Value,
        locals: IndexVec::from_raw(vec![decl(ret)]).unwrap(),
        blocks: IndexVec::from_raw(vec![
            Block {
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
    };
    let g_fn = Function {
        key: FnKey::Item(g),
        abi: Abi::Cheby,
        params: Vec::new(),
        ret,
        locals: IndexVec::from_raw(Vec::new()).unwrap(),
        blocks: IndexVec::from_raw(vec![Block {
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
    };
    let sig = |ret| AbiSig::owned(Abi::Cheby, Vec::new(), ret);
    MirModule {
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
/// after it.
fn call_and_next(text: &str) -> (String, String, String) {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let at = lines
        .iter()
        .position(|line| line.contains("= call_indirect"))
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
    // Only the call: the entry check's releasing slow path tests its own
    // result (D-775).
    assert_eq!(unboxed.matches(&load_flag).count(), 1, "{unboxed}");
    let uniform = clif_of(&[unboxed_call_unit(Repr::Value)], &opts);
    let (_, result, next) = call_and_next(&uniform);
    assert!(
        next.starts_with(&format!("brif {result},")),
        "a uniform word is tested:\n{uniform}"
    );
    assert_eq!(uniform.matches(&load_flag).count(), 0, "{uniform}");
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
    assert!(
        !small.contains(", v8+8 ") && large.contains(", v8+8 "),
        "{large}"
    );
    // Field 39 of 40 at 8 + 39 * 8; field 99999 of 100k after the count,
    // at 8 + 8 + 99999 * 8 = 0xc3508.
    assert!(small.contains("v1, v8+320\n"), "{small}");
    assert!(large.contains("v1, v8+0x000c_3508\n"), "{large}");
}
