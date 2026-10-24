//! Call locations in code generation (D-856): each located call's debug
//! tag paired with its return offset, the tags leaving the machine code and
//! the CLIF text unchanged, and the wait-frame store before the waiting
//! kernels (D-868).

use cranelift_codegen::control::ControlPlane;
use cranelift_jit::{JITBuilder, JITModule};

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, IndexVec, ModuleId};
use cheby_mir::{
    Abi, AbiSig, Block, CType, Callee, CalleeDecl, Const, DefName, FnKey, ForeignSig, Function,
    Local, LocalDecl, MirModule, Operand, Repr, RuntimeFn, Rvalue, Stmt, StrId, Terminator,
    UnitKind,
};

use crate::*;

fn module() -> JITModule {
    JITModule::new(JITBuilder::with_isa(
        isa::host_isa().unwrap(),
        isa::libcall_names(),
    ))
}

fn def(index: u32) -> DefId {
    DefId::new(ModuleId::from_u32(1), DefIndex::from_u32(index))
}

fn local(n: u32) -> Local {
    Local::from_u32(n)
}

fn function(
    key: DefId,
    locals: &[Repr],
    stmts: Vec<Stmt>,
    term: Terminator,
    call_locs: Vec<(Local, StrId)>,
    call_loc_strings: Vec<Box<str>>,
) -> Function {
    Function {
        positions: cheby_mir::Positions::default(),
        key: FnKey::Item(key),
        abi: Abi::Cheby,
        params: vec![local(0)],
        ret: Repr::Value,
        locals: IndexVec::from_raw(
            locals
                .iter()
                .map(|&repr| LocalDecl {
                    repr,
                    name: None,
                    ty: None,
                })
                .collect(),
        )
        .unwrap(),
        blocks: IndexVec::from_raw(vec![Block {
            pos: Vec::new(),
            params: Vec::new(),
            stmts,
            term,
            unwind: Vec::new(),
        }])
        .unwrap(),
        span: Span::new(FileId::new(0), TextRange::new(0, 0)),
        strings: IndexVec::from_raw(vec!["src/main.cheby:1:4".into()]).unwrap(),
        entry_loc: Some(StrId::from_u32(0)),
        entry_unwind: None,
        precounted: false,
        borrow: cheby_mir::borrow::BorrowMask::OWNED,
        sites: Vec::new(),
        call_locs,
        call_loc_strings: IndexVec::from_raw(call_loc_strings).unwrap(),
    }
}

fn call_f1(arg: u32) -> Rvalue {
    Rvalue::Call {
        callee: Callee::Direct(def(1)),
        args: vec![Operand::Local(local(arg))],
    }
}

/// `app::f0(x)`: a `@std_context` kernel call (`cheby_channel_receive`
/// with the control block first), a plain kernel call, `scope_close`,
/// two located calls of `app::f1` and a tail call of it; and `app::f1(x)`,
/// which returns `x`. Not counted: code generation reads no counts.
fn unit() -> MirModule {
    let f0 = function(
        def(0),
        &[
            Repr::Value,
            Repr::Value,
            Repr::Bool,
            Repr::Value,
            Repr::Value,
        ],
        vec![
            Stmt::Assign {
                dst: local(1),
                value: Rvalue::CallForeign {
                    symbol: "cheby_channel_receive".into(),
                    sig: ForeignSig {
                        params: vec![CType::Ptr, CType::Ptr],
                        ret: CType::Ptr,
                    },
                    args: vec![Operand::Const(Const::Ctx), Operand::Local(local(0))],
                    blocking: false,
                },
            },
            Stmt::Eval(Rvalue::CallForeign {
                symbol: "cheby_io_print".into(),
                sig: ForeignSig {
                    params: vec![CType::Ptr],
                    ret: CType::Void,
                },
                args: vec![Operand::Local(local(0))],
                blocking: false,
            }),
            Stmt::Assign {
                dst: local(2),
                value: Rvalue::CallRuntime {
                    func: RuntimeFn::ScopeClose,
                    args: vec![Operand::Local(local(1)), Operand::Const(Const::Bool(true))],
                },
            },
            Stmt::Assign {
                dst: local(3),
                value: call_f1(0),
            },
            Stmt::Assign {
                dst: local(4),
                value: call_f1(3),
            },
        ],
        Terminator::TailCall {
            callee: Callee::Direct(def(1)),
            args: vec![Operand::Local(local(4))],
        },
        vec![
            (local(3), StrId::from_u32(0)),
            (local(4), StrId::from_u32(1)),
        ],
        vec!["src/main.cheby:3:5".into(), "<repl:2>:4:7".into()],
    );
    let f1 = function(
        def(1),
        &[Repr::Value],
        Vec::new(),
        Terminator::Return(Operand::Local(local(0))),
        Vec::new(),
        Vec::new(),
    );
    let sig = AbiSig::owned(Abi::Cheby, vec![Repr::Value], Repr::Value);
    MirModule {
        externals: Vec::new(),
        source: None,
        unit: UnitKind::Code(ModuleId::from_u32(1)),
        path: "app".into(),
        functions: vec![f0, f1],
        callees: vec![
            CalleeDecl {
                def: def(0),
                sig: sig.clone(),
            },
            CalleeDecl { def: def(1), sig },
        ],
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: (0..2)
            .map(|n| DefName {
                def: def(n),
                module: "app".into(),
                item: format!("f{n}").into(),
            })
            .collect(),
    }
}

/// The translated functions of `unit()`, declared in a fresh module, and
/// the module's declarations.
fn translated() -> (JITModule, Decls, Vec<cranelift_codegen::ir::Function>) {
    let mut m = module();
    let mut decls = Decls::new(cranelift_module::Module::isa(&m)).unwrap();
    let units = [unit()];
    let cx = CodegenCx {
        layouts: &BuiltinLayouts,
    };
    let opts = CodegenOptions::default();
    declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
    let funcs = units[0]
        .functions
        .iter()
        .map(|f| translate(&units[0], f, &decls, &cx, &opts).unwrap())
        .collect();
    (m, decls, funcs)
}

/// Cranelift's own call sites of `func`: every call's return offset.
fn cranelift_call_sites(m: &JITModule, func: cranelift_codegen::ir::Function) -> Vec<u32> {
    let mut ctx = cranelift_codegen::Context::for_function(func);
    ctx.compile(
        cranelift_module::Module::isa(m),
        &mut ControlPlane::default(),
    )
    .unwrap();
    ctx.compiled_code()
        .unwrap()
        .buffer
        .call_sites()
        .map(|s| s.ret_addr)
        .collect()
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn call_tags_pair_with_return_offsets_and_leave_the_code_alone() {
    let (m, _decls, funcs) = translated();
    let tagged = funcs[0].clone();
    assert!(!tagged.debug_tags.is_empty());
    let mut untagged = tagged.clone();
    untagged.debug_tags.clear();
    let all = cranelift_call_sites(&m, tagged.clone());
    let isa = cranelift_module::Module::isa(&m);
    let with = compile_function(isa, tagged).unwrap();
    let without = compile_function(isa, untagged).unwrap();
    // Metadata only (D-264): the same bytes and relocations.
    assert_eq!(with.bytes, without.bytes);
    assert_eq!(with.relocs.len(), without.relocs.len());
    assert_eq!(without.call_sites, []);
    assert!(with.cheby_abi);
    // The two located calls, in order, each at one of Cranelift's return
    // offsets; the tail call, the last call site, has none.
    let ids: Vec<u32> = with.call_sites.iter().map(|&(_, s)| s.as_u32()).collect();
    assert_eq!(ids, [0, 1], "{:?}", with.call_sites);
    for &(ret, _) in &with.call_sites {
        assert!(all.contains(&ret), "{ret} not in {all:?}");
    }
    assert!(with.call_sites[0].0 < with.call_sites[1].0);
    let last = *all.last().unwrap();
    assert!(with.call_sites.iter().all(|&(ret, _)| ret != last));
    // `app::f1` makes no call with a location.
    let f1 = compile_function(isa, funcs[1].clone()).unwrap();
    assert!(f1.call_sites.is_empty() && f1.cheby_abi);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn clif_dumps_leave_out_call_tags() {
    let (_m, decls, funcs) = translated();
    let tagged = funcs[0].clone();
    let mut untagged = tagged.clone();
    untagged.debug_tags.clear();
    let text = decls.describe(&tagged);
    assert_eq!(text, decls.describe(&untagged));
    assert!(!text.contains("<0>") && !text.contains("<1>"), "{text}");
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn wait_frame_stores_precede_waiting_kernels_only() {
    let (_m, decls, funcs) = translated();
    let text = decls.describe(&funcs[0]);
    let offset = format!("+{}", cheby_runtime::abi::CTX_WAIT_FRAME);
    let lines: Vec<&str> = text.lines().collect();
    let fps: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains("get_frame_pointer"))
        .map(|(i, _)| i)
        .collect();
    // One before `cheby_channel_receive` (the control block first) and one
    // before `scope_close`; none before `cheby_io_print` or the Cheby
    // calls.
    assert_eq!(fps.len(), 2, "{text}");
    let stores = lines
        .iter()
        .filter(|l| l.contains(" store ") && l.contains(&offset))
        .count();
    assert_eq!(stores, 2, "{text}");
    for &at in &fps {
        // The store of the frame pointer, then the call, with nothing
        // but the call's own address in between.
        let next = lines[at + 1];
        assert!(next.contains(" store ") && next.contains(&offset), "{text}");
        let call = lines[at + 2..].iter().find(|l| l.contains("call")).unwrap();
        assert!(
            lines[at + 2..]
                .iter()
                .take_while(|l| !l.contains("call"))
                .all(|l| !l.contains(" store ")),
            "{text}"
        );
        assert!(!call.contains("return_call"), "{call}");
    }
    // `app::f1` waits on nothing.
    assert!(!decls.describe(&funcs[1]).contains("get_frame_pointer"));
}

#[test]
fn call_tags_pair_with_the_last_return_at_or_before_them() {
    // The probe's numbers on x86-64 (survey-locations §1.7): the `Post`
    // tags sit 4 bytes past the return addresses; a `Pre` tag is ignored;
    // the third site is a tail call's and has no tag.
    let tags = [(80, false, 7), (100, true, 7), (153, true, 9)];
    let sites = pair_call_sites(&[96, 149, 214], tags.into_iter()).unwrap();
    assert_eq!(sites, [(96, StrId::from_u32(7)), (149, StrId::from_u32(9))]);
    // A tag exactly at a return address pairs with it.
    let sites = pair_call_sites(&[96], [(96, true, 3)].into_iter()).unwrap();
    assert_eq!(sites, [(96, StrId::from_u32(3))]);
    // A tag before every call is an internal error.
    assert!(pair_call_sites(&[96], [(40, true, 1)].into_iter()).is_err());
    assert!(pair_call_sites(&[], [(40, true, 1)].into_iter()).is_err());
}
