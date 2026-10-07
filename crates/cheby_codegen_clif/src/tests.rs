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
