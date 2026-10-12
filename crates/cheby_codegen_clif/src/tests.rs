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
        &AbiSig {
            abi: Abi::Cheby,
            params: vec![Repr::Int, Repr::Bool, Repr::Float],
            ret: Repr::Value,
        },
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
    };
    MirModule {
        unit: UnitKind::Code(ModuleId::from_u32(1)),
        path: "app".into(),
        functions: vec![function],
        callees: vec![CalleeDecl {
            def: f,
            sig: AbiSig {
                abi: Abi::Cheby,
                params: vec![Repr::Value],
                ret: Repr::Value,
            },
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
        }],
        callees: vec![CalleeDecl {
            def: g,
            sig: AbiSig {
                abi: Abi::Cheby,
                params: Vec::new(),
                ret: Repr::Value,
            },
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

/// The runtime functions of the slice (plan §2.1): their C types, indexes
/// 24 to 27, the helpers they implement, and the static `Err(Nil)`.
#[test]
fn slice_runtime_functions() {
    use cheby_mir::RuntimeFn;
    use runtime::{RuntimeFunc, STATIC_DATA};
    let expected = [
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
        (
            RuntimeFunc::UnwindResume,
            27,
            &[CType::Ptr][..],
            CType::Void,
        ),
    ];
    for (f, index, params, ret) in expected {
        assert_eq!(f.index(), index, "{f:?}");
        assert_eq!(RuntimeFunc::ALL[index], f);
        assert_eq!(f.ctypes(), (params, ret), "{f:?}");
        assert!(f.on_demand(), "{f:?}");
    }
    assert_eq!(RuntimeFunc::ALL.len(), 28);
    assert_eq!(
        RuntimeFunc::ALL.iter().filter(|f| f.on_demand()).count(),
        4,
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
    assert!(runtime::imported_symbols().contains(&cheby_runtime::abi::sym::UNWIND_RESUME));
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
    };
    MirModule {
        unit: UnitKind::Code(ModuleId::from_u32(1)),
        path: "app".into(),
        functions: vec![function],
        callees: vec![CalleeDecl {
            def: f,
            sig: AbiSig {
                abi: Abi::Cheby,
                params: vec![Repr::Bool],
                ret: Repr::Value,
            },
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

/// Code generation of the slice's IR forms (plan §2.2): unwind edges are
/// ignored, so the pad is never translated; `resume` traps with its own
/// code; `clear_unwind` stores a zero `u32` at `ctx+12`; `static Err(Nil)`
/// is the runtime's data symbol; the scope helpers are declared on demand
/// with their C signatures; the result compiles.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn slice_forms_translate_with_edges_ignored() {
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
        &format!("trap user{}", func::TRAP_RESUME),
        "iconst.i32 0",
        &format!("+{}", cheby_runtime::abi::CTX_UNWIND),
    ] {
        assert!(text.contains(needle), "{needle:?} in:\n{text}");
    }
    assert!(text.contains("store"), "{text}");
    assert!(
        !text.contains("cheby_rt_debug"),
        "the pad was translated:\n{text}"
    );
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

/// The options of wave 2 (plan §2.4, D-679): the unwinding backend is
/// `off` and the large-frame threshold 64 KiB by default; `compile_function`
/// reports the frame size Cranelift laid out, which is small for a small
/// function.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn default_options_and_the_frame_size() {
    let opts = CodegenOptions::default();
    assert_eq!(opts.unwind, cheby_runtime::abi::UnwindBackend::Off);
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

/// With unwind edges ignored (`off`), the pads of the unwind pass (D-698)
/// get no CLIF blocks, so a function's CLIF is the same with and without
/// them: block and value numbers do not move (U1b-7).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn unwind_pads_do_not_renumber_the_clif_under_off() {
    let cx = CodegenCx {
        layouts: &BuiltinLayouts,
    };
    let opts = CodegenOptions::default();
    let text_of = |units: [MirModule; 1]| {
        let mut m = module();
        let mut decls = Decls::new(cranelift_module::Module::isa(&m)).unwrap();
        declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
        let clif = translate(&units[0], &units[0].functions[0], &decls, &cx, &opts).unwrap();
        decls.describe(&clif)
    };
    let plain = text_of([unit()]);
    let mut padded = unit();
    let function = &mut padded.functions[0];
    let extra = function
        .locals
        .push(LocalDecl {
            repr: Repr::Value,
            name: None,
            ty: None,
        })
        .unwrap();
    // bb1(_1): drop _0; goto bb2   bb2: resume
    function
        .blocks
        .push(Block {
            params: vec![extra],
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
    assert_eq!(plain, padded_text);
    assert!(!padded_text.contains(&format!("user{}", func::TRAP_RESUME)));
}
