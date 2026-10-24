//! Tests that translation stays linear in the size of a function: a list
//! literal of `n` elements gives a fixed number of CLIF blocks per element
//! and no value Cranelift's SSA construction had to merge, and a MIR whose
//! block order is not dominance order still translates.

use cranelift_codegen::ir;
use cranelift_jit::{JITBuilder, JITModule};

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, IndexVec, ModuleId};
use cheby_mir::{
    Abi, AbiSig, ArithOp, Block, BlockId, CalleeDecl, Const, DefName, FnKey, Function, Local,
    LocalDecl, MirModule, Operand, Repr, RuntimeFn, Rvalue, Stmt, StrId, Terminator, UnitKind,
};

use crate::*;

fn module() -> JITModule {
    JITModule::new(JITBuilder::with_isa(
        isa::host_isa().unwrap(),
        isa::libcall_names(),
    ))
}

fn local(n: usize) -> Local {
    Local::from_u32(u32::try_from(n).unwrap())
}

fn str_id(n: usize) -> StrId {
    StrId::from_u32(u32::try_from(n).unwrap())
}

/// A unit with the one function `app::f(params) -> value` of `locals` and
/// `blocks`, whose strings are its entry location and then `strings`.
fn unit(params: &[Repr], locals: Vec<Repr>, blocks: Vec<Block>, strings: Vec<String>) -> MirModule {
    let f = DefId::new(ModuleId::from_u32(1), DefIndex::from_u32(0));
    let decls = locals
        .into_iter()
        .map(|repr| LocalDecl {
            repr,
            name: None,
            ty: None,
        })
        .collect();
    let mut all_strings: Vec<Box<str>> = vec!["a.cheby:1:4".into()];
    all_strings.extend(strings.into_iter().map(String::into_boxed_str));
    let function = Function {
        positions: cheby_mir::Positions::default(),
        key: FnKey::Item(f),
        abi: Abi::Cheby,
        params: (0..params.len()).map(local).collect(),
        ret: Repr::Value,
        locals: IndexVec::from_raw(decls).unwrap(),
        blocks: IndexVec::from_raw(blocks).unwrap(),
        span: Span::new(FileId::new(0), TextRange::new(0, 0)),
        strings: IndexVec::from_raw(all_strings).unwrap(),
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
            sig: AbiSig::owned(Abi::Cheby, params.to_vec(), Repr::Value),
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

/// `f(x: int)`: the list literal `[x + 0, …, x + (n - 1)]` with
/// `computed`, else `[0, …, n - 1]`, lowered as MIR lowers it (one
/// `list_push` per element onto one accumulator), each `+` panicking at
/// its own location.
fn literal(n: usize, computed: bool) -> MirModule {
    let mut locals = vec![Repr::Int];
    let mut stmts = Vec::new();
    let mut strings = Vec::new();
    let mut acc = Operand::Const(Const::EmptyList);
    for i in 0..n {
        let value = i64::try_from(i).unwrap();
        let element = if computed {
            strings.push(format!("a.cheby:2:{}", i.saturating_add(1)));
            let sum = local(locals.len());
            locals.push(Repr::Int);
            stmts.push(Stmt::Assign {
                dst: sum,
                value: Rvalue::Checked {
                    op: ArithOp::Add,
                    lhs: Operand::Local(local(0)),
                    rhs: Operand::Const(Const::Int(value)),
                    repr: Repr::Int,
                    loc: str_id(strings.len()),
                },
            });
            Operand::Local(sum)
        } else {
            Operand::Const(Const::Int(value))
        };
        let boxed = local(locals.len());
        locals.push(Repr::Value);
        stmts.push(Stmt::Assign {
            dst: boxed,
            value: Rvalue::Box {
                value: element,
                from: Repr::Int,
            },
        });
        let pushed = local(locals.len());
        locals.push(Repr::Value);
        stmts.push(Stmt::Assign {
            dst: pushed,
            value: Rvalue::CallRuntime {
                func: RuntimeFn::ListPush,
                args: vec![acc, Operand::Local(boxed)],
            },
        });
        acc = Operand::Local(pushed);
    }
    let block = Block {
        params: Vec::new(),
        stmts,
        term: Terminator::Return(acc),
        unwind: Vec::new(),
        pos: Vec::new(),
    };
    unit(&[Repr::Int], locals, vec![block], strings)
}

/// Translates the one function of `unit` and checks that Cranelift
/// compiles it (with its verifier in debug builds).
fn translate_one(m: &mut JITModule, unit: &MirModule) -> ir::Function {
    let mut decls = Decls::new(cranelift_module::Module::isa(m)).unwrap();
    let cx = CodegenCx {
        layouts: &BuiltinLayouts,
    };
    let opts = CodegenOptions::default();
    let units = std::slice::from_ref(unit);
    declare(m, &mut decls, units, &cx, &opts).unwrap();
    let clif = translate(unit, &unit.functions[0], &decls, &cx, &opts).unwrap();
    compile_function(cranelift_module::Module::isa(m), clif.clone()).unwrap();
    clif
}

fn block_count(clif: &ir::Function) -> usize {
    clif.layout.blocks().count()
}

/// Values that Cranelift's SSA construction turned into aliases. MIR is
/// SSA already, so translation defines every local once and needs none.
fn aliases(clif: &ir::Function) -> usize {
    clif.dfg
        .values()
        .filter(|&v| clif.dfg.resolve_aliases(v) != v)
        .count()
}

/// A literal of constants is one straight block: no element adds a block,
/// as a constant that fits a tagged word is boxed at compile time.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_literal_of_constants_adds_no_blocks() {
    let mut m = module();
    let one = translate_one(&mut m, &literal(1, false));
    let mut m = module();
    let many = translate_one(&mut m, &literal(2_000, false));
    assert_eq!(block_count(&one), block_count(&many));
    assert_eq!(aliases(&many), 0);
}

/// A literal of computed elements adds the same number of blocks per
/// element (the overflow check, its panic and the boxing's slow path),
/// and translation leaves no aliases.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_computed_literal_adds_a_fixed_number_of_blocks_per_element() {
    let counts: Vec<usize> = [0, 500, 1_000]
        .into_iter()
        .map(|n| {
            let mut m = module();
            let clif = translate_one(&mut m, &literal(n, true));
            assert_eq!(aliases(&clif), 0, "{n} elements");
            block_count(&clif)
        })
        .collect();
    let per_500 = counts[1].saturating_sub(counts[0]);
    assert!(per_500 > 0 && per_500 <= 2_500, "{counts:?}");
    assert_eq!(counts[2].saturating_sub(counts[1]), per_500, "{counts:?}");
}

/// `bb0` jumps to `bb2`, which defines `_1` and jumps back to `bb1`, which
/// returns it: block order is not dominance order. `bb3` is unreachable
/// and is not translated.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn blocks_translate_in_dominance_order() {
    let blocks = vec![
        Block {
            params: Vec::new(),
            stmts: Vec::new(),
            term: Terminator::Goto {
                target: BlockId::from_u32(2),
                args: Vec::new(),
            },
            unwind: Vec::new(),
            pos: Vec::new(),
        },
        Block {
            params: vec![local(1)],
            stmts: Vec::new(),
            term: Terminator::Return(Operand::Local(local(1))),
            unwind: Vec::new(),
            pos: Vec::new(),
        },
        Block {
            params: Vec::new(),
            stmts: vec![Stmt::Assign {
                dst: local(2),
                value: Rvalue::Box {
                    value: Operand::Local(local(0)),
                    from: Repr::Int,
                },
            }],
            term: Terminator::Goto {
                target: BlockId::from_u32(1),
                args: vec![Operand::Local(local(2))],
            },
            unwind: Vec::new(),
            pos: Vec::new(),
        },
        Block {
            params: Vec::new(),
            stmts: vec![Stmt::Assign {
                dst: local(3),
                value: Rvalue::Use(Operand::Const(Const::Nil)),
            }],
            term: Terminator::Return(Operand::Local(local(3))),
            unwind: Vec::new(),
            pos: Vec::new(),
        },
    ];
    let unit = unit(
        &[Repr::Int],
        vec![Repr::Int, Repr::Value, Repr::Value, Repr::Value],
        blocks,
        Vec::new(),
    );
    let mut m = module();
    let clif = translate_one(&mut m, &unit);
    assert_eq!(aliases(&clif), 0);
    // CLIF block `n` is MIR block `n`; the unreachable one stays out of
    // the layout.
    for n in 0..3 {
        assert!(
            clif.layout.is_block_inserted(ir::Block::from_u32(n)),
            "bb{n}"
        );
    }
    assert!(!clif.layout.is_block_inserted(ir::Block::from_u32(3)));
}
