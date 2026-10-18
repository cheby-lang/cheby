//! Tests of std intrinsics and `@std_context` wrappers (D-692, D-693), on
//! a hand-built `std::fiber`: the shipped std has no tier-2 module yet,
//! and the front end's part (the attribute) is tested in `cheby_resolve`.

use cheby_diag::{FileId, SourceMap, Span, TextRange, TextSize};
use cheby_iface::{
    DefId, DefIndex, DefKind, DepView, FnSig, GenericParam, Generics, Globals, IfaceItem, IndexVec,
    InterfaceSet, ItemSig, ModuleId, ModuleInfo, ModuleInterface, ModuleTable, PackageId, ParamIdx,
    ParamSig, Ty, Vis, builtin,
};
use cheby_package::{ModulePath, Targets};
use cheby_types::thir::{self, External};

use crate::ir::{FnKey, Function, LayoutRef, MirModule, Rvalue, Stmt};
use crate::lower::{Intrinsic, LowerCx, intrinsic_symbols, lower_items};
use crate::repr::{CType, ForeignSig};
use crate::validate::{Stage, validate};
use crate::{Defs, count_and_check, dump, dump_function, rc};

const MODULE: ModuleId = ModuleId::from_u32(1);

/// The items of the hand-built `std::fiber`, by index.
const SCOPE_RUN: u32 = 0;
const CONTEXT_KERNEL: u32 = 1;
const PLAIN_KERNEL: u32 = 2;
const NULLARY_CONTEXT: u32 = 3;

/// The context kernel of the fixture and of the JIT test that runs a copy
/// of its wrapper (`cheby_jit`'s `tests::std_context`).
pub(crate) const CONTEXT_SYMBOL: &str = "cheby_fiber_test_context";

fn def(n: u32) -> DefId {
    DefId::new(MODULE, DefIndex::from_u32(n))
}

fn sp(file: FileId, n: u32) -> Span {
    Span::new(
        file,
        TextRange::at(TextSize::from(n), TextSize::from(1_u32)),
    )
}

struct Fixture {
    globals: Globals,
    modules: ModuleTable,
    sources: SourceMap,
    set: InterfaceSet,
    iface: ModuleInterface,
    thir: thir::Module,
}

/// `std::fiber` with `scope_run<T>(timed: Bool, seconds: Int, nanoseconds:
/// Int, body: fn(Int) -> T) -> Result<T, Nil>` bound to the intrinsic, a
/// `@std_context` kernel `context(count: Int, text: String) -> Int`, the
/// same kernel without the attribute, and a `@std_context` kernel without
/// parameters. The body's parameter type stands for `fn(Scope) -> T`:
/// lowering reads only representations.
#[expect(
    clippy::too_many_lines,
    reason = "the whole hand-built module: interface items and THIR"
)]
fn fixture(package: PackageId, scope_run_params: Option<&[Ty]>) -> Fixture {
    let globals = Globals::new();
    let mut modules = ModuleTable::new();
    let mut sources = SourceMap::new();
    let file = sources.add_file("std/fiber.cheby", " ".repeat(64)).unwrap();
    modules
        .add(ModuleInfo {
            path: ModulePath::root("std").child("fiber"),
            package,
            file: Some(file),
        })
        .unwrap();
    let t = globals.types.param(ParamIdx::from_u32(0));
    let mut generic = Generics::empty();
    generic
        .params
        .push(GenericParam {
            name: globals.names.intern("T"),
            bounds: Box::new([]),
        })
        .unwrap();
    let param = |name: &str, ty: Ty| ParamSig {
        name: globals.names.intern(name),
        ty,
    };
    let body_ty = globals.types.func([Ty::INT], t);
    let result = globals.types.named(builtin::RESULT, [t, Ty::NIL]);
    let scope_run_sig = FnSig {
        generics: generic,
        params: match scope_run_params {
            Some(tys) => tys.iter().map(|&ty| param("p", ty)).collect(),
            None => Box::new([
                param("timed", Ty::BOOL),
                param("seconds", Ty::INT),
                param("nanoseconds", Ty::INT),
                param("body", body_ty),
            ]),
        },
        ret: result,
        clamped: Box::new([]),
    };
    let kernel_sig = FnSig {
        generics: Generics::empty(),
        params: Box::new([param("count", Ty::INT), param("text", Ty::STRING)]),
        ret: Ty::INT,
        clamped: Box::new([]),
    };
    let nullary_sig = FnSig {
        generics: Generics::empty(),
        params: Box::new([]),
        ret: Ty::NIL,
        clamped: Box::new([]),
    };
    let mut items = IndexVec::new();
    for (n, name, sig) in [
        (SCOPE_RUN, "scope_run", scope_run_sig),
        (CONTEXT_KERNEL, "context", kernel_sig.clone()),
        (PLAIN_KERNEL, "plain", kernel_sig),
        (NULLARY_CONTEXT, "nullary", nullary_sig),
    ] {
        items
            .push(IfaceItem {
                name: globals.names.intern(name),
                kind: DefKind::Fn,
                vis: Vis::Package,
                parent: None,
                span: Some(sp(file, n)),
                targets: Targets::ALL,
                deprecated: None,
                sig: ItemSig::Fn(sig),
            })
            .unwrap();
    }
    let iface = ModuleInterface::new(MODULE, package, items).unwrap();
    let set = InterfaceSet::new(builtin::interface(&globals));
    let external = |n: u32, symbol: &str, context: bool| thir::FnDef {
        def: def(n),
        span: sp(file, n),
        native_external: Some(External {
            symbol: symbol.into(),
            span: sp(file, n.checked_add(32).unwrap()),
            context,
            blocking: false,
        }),
        body: None,
        entry: None,
    };
    let fns = vec![
        external(SCOPE_RUN, "cheby_rt_scope_run", false),
        external(CONTEXT_KERNEL, CONTEXT_SYMBOL, true),
        external(PLAIN_KERNEL, "cheby_fiber_test_plain", false),
        external(NULLARY_CONTEXT, "cheby_fiber_test_nullary", true),
    ];
    Fixture {
        globals,
        modules,
        sources,
        set,
        iface,
        thir: thir::Module {
            module: MODULE,
            fns,
            consts: Vec::new(),
            tests: Vec::new(),
        },
    }
}

impl Fixture {
    fn lower(&self, wanted: &[u32]) -> Result<MirModule, cheby_iface::Ice> {
        let deps = DepView::new(&self.set, &[]);
        let cx = LowerCx {
            defs: Defs {
                own: Some(&self.iface),
                deps: &deps,
                modules: &self.modules,
                globals: &self.globals,
            },
            sources: &self.sources,
        };
        let wanted: Vec<DefId> = wanted.iter().map(|&n| def(n)).collect();
        lower_items(&self.thir, &|d| wanted.contains(&d), &cx)
    }
}

fn function(unit: &MirModule, key: FnKey) -> &Function {
    unit.functions.iter().find(|f| f.key == key).unwrap()
}

fn function_dump(unit: &MirModule, n: u32) -> String {
    dump_function(unit, function(unit, FnKey::Item(def(n)))).unwrap()
}

/// `scope_run`'s generated body, exactly as plan §2.4 gives it (blocks
/// numbered in creation order): the effect check of its own symbol first
/// (D-537), the open, the body's call with an edge to the pad that closes
/// the scope after clearing the unwind flag, the close, `Ok` or the static
/// `Err(Nil)` (D-693).
#[test]
fn intrinsic_scope_run_body() {
    let f = fixture(PackageId::STD, None);
    let unit = f.lower(&[SCOPE_RUN]).unwrap();
    assert_eq!(
        function_dump(&unit, SCOPE_RUN),
        r#"fn std::fiber::scope_run(_0: bool, _1: int, _2: int, _3: value) -> value check str#0 {
  entry unwind bb5
  precounted
  str#0 = "std/fiber.cheby:1:1"
  str#1 = "cheby_rt_scope_run"
bb0:
  call_runtime effect_check(borrow const str#1) ; unwind bb5
  _4: value = call_runtime scope_open(borrow _0, borrow _1, borrow _2) ; unwind bb5
  dup _4
  _5: value = call _3(_4) ; unwind bb3
  _6: bool = call_runtime scope_close(_4, borrow const bool false) ; unwind bb4
  branch _6, bb1, bb2
bb1:
  _7: value = alloc ctor Ok(_5) ; unwind bb4
  return _7
bb2:
  drop _5
  return const static Err(Nil)
bb3:
  clear_unwind
  _8: bool = call_runtime scope_close(_4, borrow const bool true) ; unwind bb6
  return const static Err(Nil)
bb4:
  drop _5
  goto bb6()
bb5:
  drop _3
  goto bb6()
bb6:
  resume
}
"#
    );
    // The `Ok` it allocates is the unit's one foreign layout; `cheby_jit`'s
    // copy of this unit gives the same one.
    let layouts: Vec<(LayoutRef, &str, u64)> = unit
        .foreign_layouts
        .iter()
        .map(|l| (l.layout, &*l.type_name, l.type_key))
        .collect();
    assert_eq!(
        layouts,
        [(
            LayoutRef::Ctor(builtin::OK),
            "Result",
            crate::type_key("", "Result")
        )]
    );
    // A generated body has no trampoline of its own.
    assert!(
        !unit
            .functions
            .iter()
            .any(|f| matches!(f.key, FnKey::Trampoline(_)))
    );
}

/// The body passes the checks of lowering and of reference counting as
/// generated (`precounted`, D-744), and `count_and_check`
/// leaves it as it is. Once the unwind pass runs inside `count_and_check`,
/// it keeps the body's own edges and entry pad (plan §2.4).
#[test]
fn intrinsic_scope_run_passes_validation() {
    let f = fixture(PackageId::STD, None);
    let unit = f.lower(&[SCOPE_RUN]).unwrap();
    validate(&unit, Stage::Lowered).unwrap();
    validate(&unit, Stage::Counted).unwrap();
    let before = function(&unit, FnKey::Item(def(SCOPE_RUN))).clone();
    let mut counted = unit;
    count_and_check(&mut counted)
        .unwrap_or_else(|ice| panic!("{ice}\n{}", dump(&counted).unwrap_or_default()));
    let after = function(&counted, FnKey::Item(def(SCOPE_RUN)));
    assert_eq!(after.blocks, before.blocks);
    assert_eq!(after.entry_unwind, before.entry_unwind);
    assert!(after.precounted);
}

/// The table holds `scope_run` under its runtime name, and nothing else in
/// M2 (D-693).
#[test]
fn intrinsic_table() {
    assert_eq!(intrinsic_symbols(), ["cheby_rt_scope_run"]);
    assert_eq!(
        Intrinsic::ALL.map(Intrinsic::symbol).as_slice(),
        intrinsic_symbols()
    );
    assert_eq!(
        Intrinsic::from_symbol("cheby_rt_scope_run"),
        Some(Intrinsic::ScopeRun)
    );
    assert_eq!(Intrinsic::from_symbol("cheby_rt_scope_open"), None);
    assert_eq!(Intrinsic::from_symbol("scope_run"), None);
}

/// A std declaration of the intrinsic with other parameters is an
/// internal error, never a foreign call of a symbol nothing defines.
#[test]
fn intrinsic_with_another_signature_is_an_ice() {
    let f = fixture(
        PackageId::STD,
        Some(&[Ty::INT, Ty::INT, Ty::INT, Ty::STRING]),
    );
    let ice = f.lower(&[SCOPE_RUN]).unwrap_err();
    assert!(ice.to_string().contains("cheby_rt_scope_run"), "{ice}");
    let f = fixture(PackageId::STD, Some(&[Ty::BOOL, Ty::INT, Ty::INT]));
    assert!(f.lower(&[SCOPE_RUN]).is_err());
}

/// Only std binds `cheby_rt_` symbols (E0055): an intrinsic bound anywhere
/// else is an internal error in lowering.
#[test]
fn intrinsic_outside_std_is_an_ice() {
    let f = fixture(PackageId::LOCAL, None);
    let ice = f.lower(&[SCOPE_RUN]).unwrap_err();
    assert!(ice.to_string().contains("E0055"), "{ice}");
}

/// The wrapper of a `@std_context` kernel passes `ctx` first, and its C
/// signature starts with `ptr` (D-692, D-745). It starts
/// with the effect check, since `std::fiber` is not a module whose kernels
/// constants may run (D-537). `cheby_jit`'s `tests::std_context` builds
/// this exact MIR by hand and runs it.
#[test]
fn intrinsic_context_wrapper() {
    let f = fixture(PackageId::STD, None);
    let mut unit = f.lower(&[CONTEXT_KERNEL, PLAIN_KERNEL]).unwrap();
    assert_eq!(
        function_dump(&unit, CONTEXT_KERNEL),
        r#"fn std::fiber::context(_0: int, _1: value) -> value check str#0 {
  str#0 = "std/fiber.cheby:1:2"
  str#1 = "cheby_fiber_test_context"
bb0:
  call_runtime effect_check(borrow const str#1)
  _2: int = call_foreign "cheby_fiber_test_context"(borrow const ctx, borrow _0, borrow _1) -> i64
  _3: value = box int _2
  return _3
}
"#
    );
    assert_eq!(
        foreign_sig_of(&unit, CONTEXT_KERNEL),
        ForeignSig {
            params: vec![CType::Ptr, CType::I64, CType::Ptr],
            ret: CType::I64,
        }
    );
    assert_eq!(
        function_dump(&unit, PLAIN_KERNEL),
        r#"fn std::fiber::plain(_0: int, _1: value) -> value check str#0 {
  str#0 = "std/fiber.cheby:1:3"
  str#1 = "cheby_fiber_test_plain"
bb0:
  call_runtime effect_check(borrow const str#1)
  _2: int = call_foreign "cheby_fiber_test_plain"(borrow _0, borrow _1) -> i64
  _3: value = box int _2
  return _3
}
"#
    );
    assert_eq!(
        foreign_sig_of(&unit, PLAIN_KERNEL),
        ForeignSig {
            params: vec![CType::I64, CType::Ptr],
            ret: CType::I64,
        }
    );
    let mut checked = unit.clone();
    count_and_check(&mut checked)
        .unwrap_or_else(|ice| panic!("{ice}\n{}", dump(&checked).unwrap_or_default()));
    // Reference counting adds only the drop of the owned `text`. This is
    // the MIR `cheby_jit`'s `tests::std_context` runs; the unwind pass's
    // edges come after it, and are ignored by the default backend.
    validate(&unit, Stage::Lowered).unwrap();
    rc::insert_module(&mut unit).unwrap();
    validate(&unit, Stage::Counted).unwrap();
    assert_eq!(
        function_dump(&unit, CONTEXT_KERNEL),
        r#"fn std::fiber::context(_0: int, _1: value) -> value check str#0 {
  str#0 = "std/fiber.cheby:1:2"
  str#1 = "cheby_fiber_test_context"
bb0:
  call_runtime effect_check(borrow const str#1)
  _2: int = call_foreign "cheby_fiber_test_context"(borrow const ctx, borrow _0, borrow _1) -> i64
  drop _1
  _3: value = box int _2
  return _3
}
"#
    );
}

/// A `@std_context` kernel without parameters takes only `ctx`.
#[test]
fn intrinsic_context_wrapper_without_parameters() {
    let f = fixture(PackageId::STD, None);
    let mut unit = f.lower(&[NULLARY_CONTEXT]).unwrap();
    assert_eq!(
        function_dump(&unit, NULLARY_CONTEXT),
        r#"fn std::fiber::nullary() -> value check str#0 {
  str#0 = "std/fiber.cheby:1:4"
  str#1 = "cheby_fiber_test_nullary"
bb0:
  call_runtime effect_check(borrow const str#1)
  call_foreign "cheby_fiber_test_nullary"(borrow const ctx) -> void
  return const nil
}
"#
    );
    assert_eq!(
        foreign_sig_of(&unit, NULLARY_CONTEXT),
        ForeignSig {
            params: vec![CType::Ptr],
            ret: CType::Void,
        }
    );
    count_and_check(&mut unit).unwrap();
}

/// The C signature of the one foreign call in item `n`'s function.
fn foreign_sig_of(unit: &MirModule, n: u32) -> ForeignSig {
    let f = function(unit, FnKey::Item(def(n)));
    let sigs: Vec<&ForeignSig> = f
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter_map(|s| match s {
            Stmt::Assign {
                value: Rvalue::CallForeign { sig, .. },
                ..
            }
            | Stmt::Eval(Rvalue::CallForeign { sig, .. }) => Some(sig),
            _ => None,
        })
        .collect();
    assert_eq!(sigs.len(), 1);
    sigs[0].clone()
}

/// A unit that lowers a `@std_context` wrapper defines `cb$tramp$0`, so
/// a fiber the kernel starts can call its function (D-526, D-561); a unit
/// of plain kernels and intrinsics does not.
#[test]
fn intrinsic_context_wrapper_brings_trampoline_zero() {
    let f = fixture(PackageId::STD, None);
    let unit = f.lower(&[CONTEXT_KERNEL, NULLARY_CONTEXT]).unwrap();
    let trampolines: Vec<FnKey> = unit
        .functions
        .iter()
        .map(|f| f.key)
        .filter(|k| matches!(k, FnKey::Trampoline(_)))
        .collect();
    assert_eq!(trampolines, [FnKey::Trampoline(0)]);
    // After the unit's items.
    assert_eq!(
        unit.functions.last().map(|f| f.key),
        Some(FnKey::Trampoline(0))
    );
    let plain = f.lower(&[PLAIN_KERNEL, SCOPE_RUN]).unwrap();
    assert!(
        !plain
            .functions
            .iter()
            .any(|f| matches!(f.key, FnKey::Trampoline(_)))
    );
    let mut counted = unit;
    count_and_check(&mut counted).unwrap();
}
