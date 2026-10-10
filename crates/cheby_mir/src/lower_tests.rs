//! Reachability and the back-end gate (D-510) on hand-built THIR, so
//! they do not depend on what the front end accepts yet.

use std::sync::Arc;

use cheby_diag::{Diagnostic, FileId, SourceMap, Span, TextRange, TextSize};
use cheby_iface::{ConstSig, Globals};
use cheby_iface::{
    DefId, DefIndex, DefKind, DepView, FnSig, Generics, IfaceItem, IndexVec, InterfaceSet, ItemSig,
    ModuleId, ModuleInfo, ModuleInterface, ModuleTable, PackageId, ParamSig, Ty, Vis, builtin, sym,
};
use cheby_package::{ModulePath, Targets};
use cheby_resolve::LocalId;
use cheby_types::thir::{
    self, Callee, Dict, DictFn, EntryInfo, ExprId, ExprKind, External, Lit, Pat, PatId, PatKind,
    Stmt,
};

use crate::lower::{LowerCx, LowerError, lower_entries, lower_items};
use crate::reach::{Program, Root, reachable};
use crate::{Defs, EntryKind, EntryRequest};

const MODULE: ModuleId = ModuleId::from_u32(1);

fn def(n: u32) -> DefId {
    DefId::new(MODULE, DefIndex::from_u32(n))
}

/// The items of the test module, by index.
const MAIN: u32 = 0;
const WITH_CASE: u32 = 1;
const UNUSED: u32 = 2;
const SHOWN: u32 = 3;
const SAME_BOOL: u32 = 4;
const EXT_INT: u32 = 5;
const CONSTANT: u32 = 6;
const MANY: u32 = 7;
const SAY: u32 = 8;
const SHOWN_INNER: u32 = 9;

/// One byte of the test file, at `n`.
fn sp(file: FileId, n: u32) -> Span {
    Span::new(
        file,
        TextRange::at(TextSize::from(n), TextSize::from(1_u32)),
    )
}

/// A body under construction.
struct BodyB {
    file: FileId,
    exprs: IndexVec<ExprId, thir::Expr>,
    locals: IndexVec<LocalId, thir::Local>,
    params: Vec<LocalId>,
    pats: u32,
}

impl BodyB {
    fn new(file: FileId) -> Self {
        Self {
            file,
            exprs: IndexVec::new(),
            locals: IndexVec::new(),
            params: Vec::new(),
            pats: 0,
        }
    }

    fn expr(&mut self, kind: ExprKind, ty: Ty, at: u32) -> ExprId {
        self.exprs
            .push(thir::Expr {
                kind,
                ty,
                span: sp(self.file, at),
            })
            .unwrap()
    }

    fn local(&mut self, ty: Ty, at: u32) -> LocalId {
        self.locals
            .push(thir::Local {
                name: sym::T,
                ty,
                span: sp(self.file, at),
                synthetic: false,
            })
            .unwrap()
    }

    fn pat(&mut self, kind: PatKind, ty: Ty, at: u32) -> Pat {
        let id = PatId::from_u32(self.pats);
        self.pats = self.pats.checked_add(1).unwrap();
        Pat {
            id,
            kind,
            ty,
            span: sp(self.file, at),
        }
    }

    fn call(&mut self, callee: u32, args: Vec<ExprId>, ty: Ty, at: u32) -> ExprId {
        self.expr(
            ExprKind::Call {
                callee: Callee::Fn {
                    def: def(callee),
                    ty_args: Box::new([]),
                    dicts: Box::new([]),
                },
                args: args.into(),
            },
            ty,
            at,
        )
    }

    /// A construct the back end does not lower yet: a conversion to `dyn`
    /// (W4-A), of a string literal at the same position, which is never
    /// lowered. It reaches nothing (D-510).
    fn unsupported(&mut self, at: u32) -> ExprId {
        let value = self.expr(ExprKind::Lit(Lit::Str("u".into())), Ty::STRING, at);
        self.expr(
            ExprKind::DynCoerce {
                value,
                iface: builtin::SHOW,
                dict: Dict::Dyn {
                    iface: builtin::SHOW,
                    ty: Ty::STRING,
                },
            },
            Ty::STRING,
            at,
        )
    }

    fn finish(self, value: ExprId, ret: Ty) -> thir::Body {
        thir::Body {
            params: self.params,
            locals: self.locals,
            exprs: self.exprs,
            value,
            ret,
        }
    }
}

struct Fixture {
    globals: Globals,
    modules: ModuleTable,
    sources: SourceMap,
    set: InterfaceSet,
    iface: ModuleInterface,
    thir: thir::Module,
}

fn item(globals: &Globals, file: FileId, n: u32, kind: DefKind, sig: ItemSig) -> IfaceItem {
    IfaceItem {
        name: globals.names.intern(&format!("item{n}")),
        kind,
        vis: Vis::Package,
        parent: None,
        span: Some(sp(file, n)),
        targets: Targets::ALL,
        deprecated: None,
        sig,
    }
}

fn fn_sig(params: &[Ty], ret: Ty) -> ItemSig {
    ItemSig::Fn(FnSig {
        generics: Generics::empty(),
        params: params
            .iter()
            .map(|&ty| ParamSig { name: sym::T, ty })
            .collect(),
        ret,
        clamped: Box::new([]),
    })
}

fn fn_def(n: u32, file: FileId, body: thir::Body) -> thir::FnDef {
    thir::FnDef {
        def: def(n),
        span: sp(file, n),
        native_external: None,
        body: Some(body),
        entry: (n == MAIN).then_some(EntryInfo::Nil),
    }
}

/// A module with one function per scenario.
#[expect(clippy::too_many_lines, reason = "one body per scenario, in order")]
#[expect(
    clippy::many_single_char_names,
    reason = "`b` builds each body; `p`, `x`, `y`, `z` mirror the Cheby source"
)]
fn fixture() -> Fixture {
    let globals = Globals::new();
    let mut modules = ModuleTable::new();
    let mut sources = SourceMap::new();
    let file = sources.add_file("src/main.cheby", " ".repeat(200)).unwrap();
    modules
        .add(ModuleInfo {
            path: ModulePath::root("app"),
            package: PackageId::LOCAL,
            file: Some(file),
        })
        .unwrap();
    let int_ty = Ty::INT;
    let mut items = IndexVec::new();
    for (n, kind, sig) in [
        (MAIN, DefKind::Fn, fn_sig(&[], Ty::NIL)),
        (WITH_CASE, DefKind::Fn, fn_sig(&[], Ty::NIL)),
        (UNUSED, DefKind::Fn, fn_sig(&[], Ty::NIL)),
        (SHOWN, DefKind::Fn, fn_sig(&[Ty::STRING], Ty::STRING)),
        (SAME_BOOL, DefKind::Fn, fn_sig(&[Ty::BOOL], Ty::BOOL)),
        (EXT_INT, DefKind::Fn, fn_sig(&[], int_ty)),
        (
            CONSTANT,
            DefKind::Const,
            ItemSig::Const(ConstSig { ty: Ty::STRING }),
        ),
        (MANY, DefKind::Fn, fn_sig(&[], Ty::NIL)),
        (SAY, DefKind::Fn, fn_sig(&[Ty::STRING], Ty::NIL)),
        (SHOWN_INNER, DefKind::Fn, fn_sig(&[Ty::STRING], Ty::STRING)),
    ] {
        items.push(item(&globals, file, n, kind, sig)).unwrap();
    }
    let iface = ModuleInterface::new(MODULE, PackageId::LOCAL, items).unwrap();
    let set = InterfaceSet::new(builtin::interface(&globals));

    let mut fns = Vec::new();
    // main: calls `with_case`, then `say("hi")` in tail position.
    let mut b = BodyB::new(file);
    let first = b.call(WITH_CASE, vec![], Ty::NIL, 100);
    let hi = b.expr(ExprKind::Lit(Lit::Str("hi".into())), Ty::STRING, 101);
    let say = b.call(SAY, vec![hi], Ty::NIL, 102);
    let block = b.expr(
        ExprKind::Block {
            stmts: Box::new([Stmt::Expr(first)]),
            tail: Some(say),
        },
        Ty::NIL,
        103,
    );
    fns.push(fn_def(MAIN, file, b.finish(block, Ty::NIL)));
    // with_case: an unsupported construct (E0038 at 110). It was a `case`
    // until W2-B lowered them.
    let mut b = BodyB::new(file);
    let case = b.unsupported(110);
    fns.push(fn_def(WITH_CASE, file, b.finish(case, Ty::NIL)));
    // unused: one too, never reached.
    let mut b = BodyB::new(file);
    let case = b.unsupported(120);
    fns.push(fn_def(UNUSED, file, b.finish(case, Ty::NIL)));
    // shown: returns its parameter.
    let mut b = BodyB::new(file);
    let p = b.local(Ty::STRING, 130);
    b.params.push(p);
    let value = b.expr(ExprKind::Local(p), Ty::STRING, 131);
    fns.push(fn_def(SHOWN, file, b.finish(value, Ty::STRING)));
    // same_bool: returns its unboxed `Bool` parameter, which needs boxing.
    let mut b = BodyB::new(file);
    let p = b.local(Ty::BOOL, 140);
    b.params.push(p);
    let value = b.expr(ExprKind::Local(p), Ty::BOOL, 141);
    let block = b.expr(
        ExprKind::Block {
            stmts: Box::new([]),
            tail: Some(value),
        },
        Ty::BOOL,
        142,
    );
    fns.push(fn_def(SAME_BOOL, file, b.finish(block, Ty::BOOL)));
    // ext_int: a native external returning `Int`.
    fns.push(thir::FnDef {
        def: def(EXT_INT),
        span: sp(file, EXT_INT),
        native_external: Some(External {
            symbol: "ext_int".into(),
            span: sp(file, 150),
        }),
        body: None,
        entry: None,
    });
    // many: `let x = (unsupported)`, `let (y, z) = …`, a tuple statement
    // holding an unsupported construct, then `say(x)` and `say(y)` with the
    // placeholders.
    let mut b = BodyB::new(file);
    let x = b.local(Ty::STRING, 160);
    let y = b.local(Ty::STRING, 161);
    let z = b.local(Ty::STRING, 162);
    let case_x = b.unsupported(163);
    let lit = b.expr(ExprKind::Lit(Lit::Str("s".into())), Ty::STRING, 164);
    let inner_case = b.unsupported(165);
    let tuple_ty = globals.types.intern(cheby_iface::TyKind::Tuple(Box::new([
        Ty::STRING,
        Ty::STRING,
    ])));
    let tuple = b.expr(ExprKind::Tuple(Box::new([inner_case, lit])), tuple_ty, 166);
    let use_x = b.expr(ExprKind::Local(x), Ty::STRING, 167);
    let say_x = b.call(SAY, vec![use_x], Ty::NIL, 168);
    let use_y = b.expr(ExprKind::Local(y), Ty::STRING, 169);
    let say_y = b.call(SAY, vec![use_y], Ty::NIL, 170);
    let bind_x = b.pat(PatKind::Bind(x), Ty::STRING, 171);
    let bind_y = b.pat(PatKind::Bind(y), Ty::STRING, 172);
    let bind_z = b.pat(PatKind::Bind(z), Ty::STRING, 173);
    let pair_ty = globals.types.intern(cheby_iface::TyKind::Tuple(Box::new([
        Ty::STRING,
        Ty::STRING,
    ])));
    let pair = b.pat(PatKind::Tuple(Box::new([bind_y, bind_z])), pair_ty, 174);
    let first = b.expr(ExprKind::Lit(Lit::Str("t".into())), Ty::STRING, 175);
    let second = b.expr(ExprKind::Lit(Lit::Str("u".into())), Ty::STRING, 177);
    let lit2 = b.expr(ExprKind::Tuple(Box::new([first, second])), pair_ty, 178);
    let block = b.expr(
        ExprKind::Block {
            stmts: Box::new([
                Stmt::Let {
                    pat: bind_x,
                    init: case_x,
                },
                Stmt::Let {
                    pat: pair,
                    init: lit2,
                },
                Stmt::Expr(tuple),
                Stmt::Expr(say_x),
            ]),
            tail: Some(say_y),
        },
        Ty::NIL,
        176,
    );
    fns.push(fn_def(MANY, file, b.finish(block, Ty::NIL)));
    // say: a native external taking a `String`.
    fns.push(thir::FnDef {
        def: def(SAY),
        span: sp(file, SAY),
        native_external: Some(External {
            symbol: "say".into(),
            span: sp(file, 180),
        }),
        body: None,
        entry: None,
    });
    // shown_inner: like `shown`.
    let mut b = BodyB::new(file);
    let p = b.local(Ty::STRING, 190);
    b.params.push(p);
    let value = b.expr(ExprKind::Local(p), Ty::STRING, 191);
    fns.push(fn_def(SHOWN_INNER, file, b.finish(value, Ty::STRING)));
    // CONSTANT = "c"
    let mut b = BodyB::new(file);
    let value = b.expr(ExprKind::Lit(Lit::Str("c".into())), Ty::STRING, 195);
    let consts = vec![thir::ConstDef {
        def: def(CONSTANT),
        span: sp(file, CONSTANT),
        body: b.finish(value, Ty::STRING),
        deps: Box::new([]),
    }];
    Fixture {
        globals,
        modules,
        sources,
        set,
        iface,
        thir: thir::Module {
            module: MODULE,
            fns,
            consts,
            tests: Vec::new(),
        },
    }
}

impl Fixture {
    fn file(&self) -> FileId {
        self.modules.get(MODULE).unwrap().file.unwrap()
    }

    fn lower(&self, wanted: &[u32]) -> Result<crate::MirModule, LowerError> {
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

    fn unsupported(&self, wanted: &[u32]) -> Vec<(String, Span)> {
        match self.lower(wanted) {
            Err(LowerError::Unsupported(diags)) => diags.iter().map(summary).collect(),
            other => panic!("expected E0038, got {other:?}"),
        }
    }

    fn program(&self) -> Program<'_> {
        Program::from([(MODULE, &self.thir)])
    }

    fn fn_def(&self, n: u32) -> &thir::FnDef {
        self.thir.fns.iter().find(|f| f.def == def(n)).unwrap()
    }
}

fn summary(d: &Diagnostic) -> (String, Span) {
    assert_eq!(d.code, cheby_diag::codes::NOT_SUPPORTED_YET);
    let primary = d.labels.first().unwrap().span;
    (d.message.clone(), primary)
}

#[test]
fn reachability_follows_calls_only() {
    let f = fixture();
    let reach = reachable(&f.program(), &[Root::Fn(f.fn_def(MAIN))]).unwrap();
    let items: Vec<DefId> = reach.items().collect();
    assert_eq!(items, [def(MAIN), def(WITH_CASE), def(SAY)]);
    assert!(reach.needs_module(MODULE));
    assert!(!reach.contains(def(UNUSED)));
}

#[test]
fn reachable_unsupported_construct_is_reported_at_it() {
    let f = fixture();
    let file = f.file();
    let reach = reachable(&f.program(), &[Root::Fn(f.fn_def(MAIN))]).unwrap();
    let wanted: Vec<u32> = reach.items().map(|d| d.index.as_u32()).collect();
    assert_eq!(
        f.unsupported(&wanted),
        [(
            "conversions to `dyn` are not supported yet".to_owned(),
            sp(file, 110)
        )]
    );
}

#[test]
fn unreachable_items_are_not_lowered() {
    let f = fixture();
    // `main` without `with_case`'s body: the unused `case`s never matter.
    let unit = f.lower(&[SAY, SHOWN, SHOWN_INNER, CONSTANT]).unwrap();
    let keys: Vec<String> = unit
        .functions
        .iter()
        .map(|func| format!("{:?}", func.key))
        .collect();
    assert_eq!(keys.len(), 4, "{keys:?}");
}

/// Each outermost construct once; nothing inside it, and no follow-up
/// error where its placeholder value is used.
#[test]
fn every_outermost_construct_is_reported_once() {
    let f = fixture();
    let file = f.file();
    assert_eq!(
        f.unsupported(&[MANY]),
        [
            (
                "conversions to `dyn` are not supported yet".to_owned(),
                sp(file, 163)
            ),
            (
                "conversions to `dyn` are not supported yet".to_owned(),
                sp(file, 165)
            ),
        ]
    );
}

/// Returning an unboxed `Bool` boxes it, and an external returning `Int`
/// boxes its result (D6); neither is E0038 any more.
#[test]
fn boxing_is_lowered_where_it_is_needed() {
    let f = fixture();
    let unit = f.lower(&[SAME_BOOL, EXT_INT]).unwrap();
    let text = crate::dump(&unit).unwrap();
    assert!(
        text.contains("_1: value = box bool _0\n  return _1"),
        "{text}"
    );
    assert!(
        text.contains("_0: int = call_foreign \"ext_int\"() -> i64\n  _1: value = box int _0"),
        "{text}"
    );
}

/// The diagnostics of every selected item, so a run reports them all.
#[test]
fn diagnostics_of_several_items_are_collected() {
    let f = fixture();
    let messages: Vec<String> = f
        .unsupported(&[WITH_CASE, UNUSED, SAME_BOOL])
        .into_iter()
        .map(|(m, _)| m)
        .collect();
    assert_eq!(
        messages,
        [
            "conversions to `dyn` are not supported yet",
            "conversions to `dyn` are not supported yet",
        ]
    );
}

#[test]
fn reachability_follows_dictionaries_constants_and_operators() {
    let f = fixture();
    let file = f.file();
    let mut b = BodyB::new(file);
    let inner = Dict::Impl {
        iface: builtin::SHOW,
        ty: Ty::STRING,
        fns: Box::new([DictFn {
            def: def(SHOWN_INNER),
            ty_args: Box::new([]),
            dicts: Box::new([]),
        }]),
    };
    let outer = Dict::Embedded {
        of: Box::new(Dict::Tuple {
            iface: builtin::SHOW,
            ty: Ty::STRING,
            elems: Box::new([Dict::Impl {
                iface: builtin::SHOW,
                ty: Ty::STRING,
                fns: Box::new([DictFn {
                    def: def(SHOWN),
                    ty_args: Box::new([]),
                    dicts: Box::new([inner]),
                }]),
            }]),
        }),
        iface: builtin::SHOW,
    };
    let constant = b.expr(ExprKind::ConstRef { def: def(CONSTANT) }, Ty::STRING, 10);
    let call = b.expr(
        ExprKind::Call {
            callee: Callee::Fn {
                def: def(SAY),
                ty_args: Box::new([]),
                dicts: Box::new([outer]),
            },
            args: Box::new([constant]),
        },
        Ty::NIL,
        11,
    );
    let entry = thir::FnDef {
        def: def(MAIN),
        span: sp(file, 0),
        native_external: None,
        body: Some(b.finish(call, Ty::NIL)),
        entry: Some(EntryInfo::Nil),
    };
    let mut module = f.thir.clone();
    module.fns.retain(|func| func.def != def(MAIN));
    module.fns.insert(0, entry.clone());
    let program = Program::from([(MODULE, &module)]);
    let reach = reachable(&program, &[Root::Fn(&entry)]).unwrap();
    let items: Vec<DefId> = reach.items().collect();
    assert_eq!(
        items,
        [
            def(MAIN),
            def(SHOWN),
            def(CONSTANT),
            def(SAY),
            def(SHOWN_INNER)
        ]
    );
}

/// A `Result` main whose error's `show` needs the dictionary of a `dyn`
/// value waits for W4-A (E0038 at `main`); one printed by debug text has
/// its thunk. (Closed dictionaries are static: `dict::tests`.)
#[test]
fn result_mains_with_dyn_dictionaries_are_unsupported() {
    let f = fixture();
    let deps_list = [MODULE];
    let mut set = InterfaceSet::new(builtin::interface(&f.globals));
    set.insert(Arc::new(f.iface.clone())).unwrap();
    let deps = DepView::new(&set, &deps_list);
    let cx = LowerCx {
        defs: Defs {
            own: None,
            deps: &deps,
            modules: &f.modules,
            globals: &f.globals,
        },
        sources: &f.sources,
    };
    let info = EntryInfo::Result {
        err_ty: Ty::STRING,
        show: Some(Dict::Impl {
            iface: builtin::SHOW,
            ty: Ty::STRING,
            fns: Box::new([DictFn {
                def: def(SHOWN),
                ty_args: Box::new([]),
                dicts: Box::new([Dict::Dyn {
                    iface: builtin::SHOW,
                    ty: Ty::STRING,
                }]),
            }]),
        }),
    };
    let result = lower_entries(
        &[EntryRequest {
            kind: EntryKind::Main(def(MAIN)),
            info: Some(&info),
        }],
        &cx,
    );
    let Err(LowerError::Unsupported(diags)) = result else {
        panic!("expected E0038, got {result:?}");
    };
    let got: Vec<(String, Span)> = diags.iter().map(summary).collect();
    assert_eq!(
        got,
        [(
            "errors whose `show` needs the dictionary of a `dyn` value are not supported yet"
                .to_owned(),
            sp(f.file(), MAIN)
        )]
    );
    // Debug printing needs no dictionary; a `Nil` main has its thunk too.
    let debug = EntryInfo::Result {
        err_ty: Ty::STRING,
        show: None,
    };
    let unit = lower_entries(
        &[EntryRequest {
            kind: EntryKind::Main(def(MAIN)),
            info: Some(&debug),
        }],
        &cx,
    )
    .unwrap();
    assert!(
        crate::dump(&unit).unwrap().contains("call_runtime debug("),
        "{unit:?}"
    );
    let unit = lower_entries(
        &[EntryRequest {
            kind: EntryKind::Main(def(MAIN)),
            info: Some(&EntryInfo::Nil),
        }],
        &cx,
    )
    .unwrap();
    // The thunk, then the trampolines of arities 0-4.
    assert_eq!(unit.functions.len(), 6);
}

/// A function with a native external and a body runs the external on
/// native; the body is for the other targets (D-207). Its body is neither
/// walked nor lowered.
#[test]
fn a_native_external_wins_over_a_body() {
    let mut f = fixture();
    let file = f.file();
    let mut b = BodyB::new(file);
    let p = b.local(Ty::STRING, 185);
    b.params.push(p);
    let unused = b.call(UNUSED, vec![], Ty::NIL, 186);
    let body = b.finish(unused, Ty::NIL);
    let say = f.thir.fns.iter_mut().find(|d| d.def == def(SAY)).unwrap();
    say.body = Some(body);
    let reach = reachable(&f.program(), &[Root::Fn(f.fn_def(SAY))]).unwrap();
    let items: Vec<DefId> = reach.items().collect();
    assert_eq!(items, [def(SAY)]);
    let unit = f.lower(&[SAY]).unwrap();
    assert_eq!(unit.functions.len(), 1);
}

// D6 from source (plan §5.1, D-453, D-521), through
// `test_support::lower_source`.

const D6_SOURCE: &str = r#"//! Boxing at generic boundaries (D6).

import std::io

type Color {
  Red,
  Green,
}

@external(native, "get_int")
@external(js, "./ffi.mjs", "get_int")
fn get_int() -> Int

@external(native, "get_i64")
@external(js, "./ffi.mjs", "get_i64")
fn get_i64() -> I64

@external(native, "get_u8")
@external(js, "./ffi.mjs", "get_u8")
fn get_u8() -> U8

@external(native, "get_u32")
@external(js, "./ffi.mjs", "get_u32")
fn get_u32() -> U32

@external(native, "get_u64")
@external(js, "./ffi.mjs", "get_u64")
fn get_u64() -> U64

@external(native, "get_float")
@external(js, "./ffi.mjs", "get_float")
fn get_float() -> Float

@external(native, "get_f32")
@external(js, "./ffi.mjs", "get_f32")
fn get_f32() -> F32

@external(native, "get_bool")
@external(js, "./ffi.mjs", "get_bool")
fn get_bool() -> Bool

@external(native, "say")
@external(js, "./ffi.mjs", "say")
fn say(text: String)

fn id<T>(x: T) -> T {
  x
}

fn ints(n: Int) -> Int {
  id(n)
}

fn i64s(n: I64) -> I64 {
  id(n)
}

fn u8s(n: U8) -> U8 {
  id(n)
}

fn u64s(n: U64) -> U64 {
  id(n)
}

fn floats(x: Float) -> Float {
  id(x)
}

fn f32s(x: F32) -> F32 {
  id(x)
}

fn bools(b: Bool) -> Bool {
  id(b)
}

fn round_trip(n: Int, b: Bool) -> Bool {
  let m = id(n)
  let _ = ints(m)
  id(b)
}

fn yes() -> Bool {
  id(True)
}

fn red() -> Color {
  Red
}

fn main() {
  let _ = ints(get_int())
  let _ = i64s(get_i64())
  let _ = u8s(get_u8())
  let _ = get_u32()
  let _ = u64s(get_u64())
  let _ = floats(get_float())
  let _ = f32s(get_f32())
  let _ = bools(get_bool())
  let _ = round_trip(get_int(), get_bool())
  let _ = yes()
  let _ = red()
  say("done")
  io::println("done")
}
"#;

fn d6() -> crate::test_support::Lowered {
    let lowered = crate::test_support::lower_source(&[("", D6_SOURCE)]);
    assert_eq!(lowered.unsupported(), Vec::<String>::new());
    lowered
}

/// A value of each unboxed type is boxed where it is passed to a generic
/// parameter, and unboxed where it comes back from a generic callee; a
/// tail call needs neither (D-453).
#[test]
fn values_are_boxed_and_unboxed_at_generic_boundaries() {
    let lowered = d6();
    for (name, repr, line) in [
        ("ints", "int", 50),
        ("i64s", "i64", 54),
        ("u8s", "u8", 58),
        ("u64s", "u64", 62),
        ("floats", "float", 66),
        ("f32s", "f32", 70),
        ("bools", "bool", 74),
    ] {
        assert_eq!(
            lowered.function("app", &format!("app::{name}")),
            format!(
                "fn app::{name}(_0: {repr}) -> value check str#0 {{\n  \
                 str#0 = \"src/main.cheby:{line}:4\"\nbb0:\n  \
                 _1: value = box {repr} _0\n  tail_call app::id(_1)\n}}\n"
            )
        );
    }
    assert_eq!(
        lowered.function("app", "app::round_trip"),
        r#"fn app::round_trip(_0: int, _1: bool) -> value check str#0 {
  str#0 = "src/main.cheby:78:4"
bb0:
  _2: value = box int _0
  _3: value = call app::id(_2)
  _4: int = unbox int _3
  drop _3
  _5: value = call app::ints(_4)
  _6: int = unbox int _5
  drop _5
  _7: value = box bool _1
  tail_call app::id(_7)
}
"#
    );
    let main = lowered.function("app", "app::main");
    for repr in ["int", "i64", "u8", "u32", "u64", "float", "f32", "bool"] {
        assert!(
            main.contains(&format!(": {repr} = unbox {repr} _")),
            "{main}"
        );
    }
}

/// `True` passed to a generic parameter is the static `True` (D-454).
#[test]
fn bool_constants_in_uniform_positions_are_static_objects() {
    assert_eq!(
        d6().function("app", "app::yes"),
        r#"fn app::yes() -> value check str#0 {
  str#0 = "src/main.cheby:84:4"
bb0:
  tail_call app::id(const ctor True)
}
"#
    );
}

/// An external's scalar result is boxed into the uniform return; a `Nil`
/// result is the static `Nil` (D-455, D-521).
#[test]
fn external_wrappers_box_scalar_results() {
    let lowered = d6();
    for (name, repr, ctype, line) in [
        ("get_int", "int", "i64", 12),
        ("get_bool", "bool", "bool", 40),
        ("get_u32", "u32", "u32", 24),
        ("get_float", "float", "f64", 32),
    ] {
        assert_eq!(
            lowered.function("app", &format!("app::{name}")),
            format!(
                "fn app::{name}() -> value check str#0 {{\n  \
                 str#0 = \"src/main.cheby:{line}:4\"\n  \
                 str#1 = \"{name}\"\nbb0:\n  \
                 call_runtime effect_check(borrow const str#1)\n  \
                 _0: {repr} = call_foreign \"{name}\"() -> {ctype}\n  \
                 _1: value = box {repr} _0\n  return _1\n}}\n"
            )
        );
    }
    assert_eq!(
        lowered.function("app", "app::say"),
        r#"fn app::say(_0: value) -> value check str#0 {
  str#0 = "src/main.cheby:44:4"
  str#1 = "say"
bb0:
  call_runtime effect_check(borrow const str#1)
  call_foreign "say"(borrow _0) -> void
  drop _0
  return const nil
}
"#
    );
}

/// A user unit constructor is its static object `cb$ctor$…` (D-454),
/// and its layout is in the unit.
#[test]
fn user_unit_constructors_are_static_objects() {
    let lowered = d6();
    assert_eq!(
        lowered.function("app", "app::red"),
        r#"fn app::red() -> value check str#0 {
  str#0 = "src/main.cheby:88:4"
bb0:
  return const ctor app::Red
}
"#
    );
    let dump = crate::dump(lowered.unit("app")).unwrap();
    let key = crate::type_key("app", "Color");
    assert!(
        dump.contains(&format!(
            "layout ctor app::Red: Color.Red tag 0 key {key:016x} []\n\
             layout ctor app::Green: Color.Green tag 1 key {key:016x} []\n"
        )),
        "{dump}"
    );
}

/// Two builds of one source give the same MIR, std units included
/// (D-264).
#[test]
fn lowering_from_source_is_deterministic() {
    let first = d6().dump();
    assert!(first.contains("module std::io"), "{first}");
    assert_eq!(first, d6().dump());
}

/// The `.cheby` files of an example as `(module path under the package,
/// text)`, and the package name from its manifest.
fn example_sources(dir: &std::path::Path) -> (String, Vec<(String, String)>) {
    fn walk(dir: &std::path::Path, prefix: &str, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            let module = match (prefix, stem.as_str()) {
                ("", "main") => String::new(),
                ("", _) => stem.clone(),
                _ => format!("{prefix}::{stem}"),
            };
            if path.is_dir() {
                walk(&path, &module, out);
            } else if path.extension().is_some_and(|e| e == "cheby") {
                out.push((module, std::fs::read_to_string(&path).unwrap()));
            }
        }
    }
    let manifest = std::fs::read_to_string(dir.join("cheby.toml")).unwrap();
    let name = manifest
        .lines()
        .find_map(|l| l.strip_prefix("name = \""))
        .and_then(|l| l.strip_suffix('"'))
        .unwrap()
        .to_owned();
    let mut files = Vec::new();
    walk(&dir.join("src"), "", &mut files);
    (name, files)
}

/// Every example the front end accepts lowers without an internal error,
/// and no E0038 about representations remains (D6): values crossing
/// generic boundaries are boxed, and externals with scalar results are
/// wrapped.
#[test]
fn examples_lower_without_representation_errors() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
    let mut dirs: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("cheby.toml").exists())
        .collect();
    dirs.sort();
    assert!(dirs.len() >= 32, "{dirs:?}");
    let mut lowered_any = false;
    for dir in dirs {
        let (name, files) = example_sources(&dir);
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        let Ok(lowered) = crate::test_support::lower_package(&name, &refs) else {
            // Unshipped std modules or dependencies (D-477, D-446).
            continue;
        };
        lowered_any = true;
        for message in lowered.unsupported() {
            assert!(
                !message.starts_with("values of type")
                    && !message.starts_with("external functions that return"),
                "{}: {message}",
                dir.display()
            );
        }
        if std::env::var_os("CHEBY_SHOW_E0038").is_some() {
            println!("== {}", dir.display());
            for message in lowered.unsupported() {
                println!("{message}");
            }
        }
    }
    assert!(lowered_any);
}

/// A module whose constructor reachable code only names gets a unit with
/// its layouts and no functions, and the naming unit carries the
/// constructor's layout as a foreign one (D-454, D-510).
#[test]
fn modules_of_named_constructors_register_their_layouts() {
    let lowered = crate::test_support::lower_source(&[
        (
            "",
            r"//! Names a constructor of another module.

import app::colors

fn keep(color: colors::Color) -> colors::Color {
  color
}

fn main() {
  let _ = keep(colors::Green)
}
",
        ),
        (
            "colors",
            r"//! Colors.

/// A color.
pub type Color {
  Red,
  Green,
}
",
        ),
    ]);
    assert_eq!(lowered.unsupported(), Vec::<String>::new());
    let key = crate::type_key("app::colors", "Color");
    assert_eq!(
        crate::dump(lowered.unit("app::colors")).unwrap(),
        format!(
            "module app::colors\n\n\
             layout ctor app::colors::Red: Color.Red tag 0 key {key:016x} []\n\
             layout ctor app::colors::Green: Color.Green tag 1 key {key:016x} []\n"
        )
    );
    let app = crate::dump(lowered.unit("app")).unwrap();
    assert!(
        app.contains(&format!(
            "\nforeign_layout ctor app::colors::Green: Color.Green tag 1 key {key:016x} []\n"
        )),
        "{app}"
    );
}
