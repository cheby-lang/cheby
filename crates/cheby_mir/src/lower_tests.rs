//! Reachability and the back-end gate (plan §8) on hand-built THIR, so
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

    fn case(&mut self, at: u32) -> ExprId {
        self.expr(
            ExprKind::Case {
                subjects: Box::new([]),
                arms: Box::new([]),
            },
            Ty::NIL,
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
    // with_case: a `case` (E0038 at 110).
    let mut b = BodyB::new(file);
    let case = b.case(110);
    fns.push(fn_def(WITH_CASE, file, b.finish(case, Ty::NIL)));
    // unused: a `case` too, never reached.
    let mut b = BodyB::new(file);
    let case = b.case(120);
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
    // many: `let x = (case)`, `let (y, z) = …`, a tuple statement holding a
    // `case`, then `say(x)` and `say(y)` with the placeholders.
    let mut b = BodyB::new(file);
    let x = b.local(Ty::STRING, 160);
    let y = b.local(Ty::STRING, 161);
    let z = b.local(Ty::STRING, 162);
    let case_x = b.case(163);
    let lit = b.expr(ExprKind::Lit(Lit::Str("s".into())), Ty::STRING, 164);
    let inner_case = b.case(165);
    let tuple_ty = globals
        .types
        .intern(cheby_iface::TyKind::Tuple(Box::new([Ty::NIL, Ty::STRING])));
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
    let lit2 = b.expr(ExprKind::Lit(Lit::Str("t".into())), Ty::STRING, 175);
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
fn reachable_case_is_unsupported_at_the_case() {
    let f = fixture();
    let file = f.file();
    let reach = reachable(&f.program(), &[Root::Fn(f.fn_def(MAIN))]).unwrap();
    let wanted: Vec<u32> = reach.items().map(|d| d.index.as_u32()).collect();
    assert_eq!(
        f.unsupported(&wanted),
        [(
            "`case` expressions are not supported yet".to_owned(),
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
                "`case` expressions are not supported yet".to_owned(),
                sp(file, 163)
            ),
            (
                "patterns other than a name or `_` in `let` are not supported yet".to_owned(),
                sp(file, 174)
            ),
            ("tuples are not supported yet".to_owned(), sp(file, 166)),
        ]
    );
}

#[test]
fn boxing_is_unsupported_where_it_is_needed() {
    let f = fixture();
    let file = f.file();
    assert_eq!(
        f.unsupported(&[SAME_BOOL]),
        [(
            "values of type `Bool` are not supported yet".to_owned(),
            sp(file, 141)
        )]
    );
    assert_eq!(
        f.unsupported(&[EXT_INT]),
        [(
            "external functions that return `Int` are not supported yet".to_owned(),
            sp(file, 150)
        )]
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
            "`case` expressions are not supported yet",
            "`case` expressions are not supported yet",
            "values of type `Bool` are not supported yet",
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

#[test]
fn result_mains_are_unsupported() {
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
        show: None,
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
            "`main` functions that return `Result` are not supported yet".to_owned(),
            sp(f.file(), MAIN)
        )]
    );
    // A `Nil` main has its thunk.
    let unit = lower_entries(
        &[EntryRequest {
            kind: EntryKind::Main(def(MAIN)),
            info: Some(&EntryInfo::Nil),
        }],
        &cx,
    )
    .unwrap();
    assert_eq!(unit.functions.len(), 1);
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
