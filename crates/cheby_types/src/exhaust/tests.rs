//! Adapter tests: hand-built THIR bodies over hand-built interfaces.
//!
//! The module under test is `app` of the local package. It declares
//!
//! ```cheby
//! type Color { Red, Green, Blue }
//! type Shape { Circle { radius: Int }, Dot }
//! priv type Secret { Hidden(Int) }   // in the sibling module `vault`
//! ```
//!
//! and imports `geo` from another package, which declares
//! `pub type Opaque { Opaque(Int) }` (not exposed) and
//! `pub exposed type Dir { North, South }`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::{Diagnostic, FileId, SourceMap, TextRange, codes, render_all};
use cheby_iface::{
    CtorFields, CtorSig, DefId, DefIndex, DefKind, DepView, Generics, Globals, Ice, IfaceItem,
    IndexVec, InterfaceSet, ItemSig, ModuleId, ModuleInfo, ModuleInterface, ModuleTable, PackageId,
    Ty, TyKind, TypeDef, TypeDefKind, Vis, builtin, sym,
};
use cheby_package::{ModulePath, Targets};
use cheby_resolve::{
    LocalId, Lookup, ModuleLookup, ResolveInput, StdCatalog, collect_imports,
    resolve_import_targets, resolve_module,
};
use cheby_syntax::{NodeId, SyntaxKind, SyntaxNode, SyntaxTree};

use super::engine::{Ctor, ListLen, LitKey, Space, Types, Witness, WitnessKind};
use super::{CaseFact, MissingPat, TyCx, check, collect_case_facts};
use crate::facts::ModuleFacts;
use crate::thir::{
    Arm, Body, CaseOrigin, Expr, ExprId, ExprKind, FloatLit, Lit, Local, Param, ParamOrigin, Pat,
    PatId, PatKind, Stmt,
};
use crate::{Tests, TypesInput, build_interface, check_bodies_recording};

/// A constructor: name and its fields (`None` for named-field names).
type CtorDecl<'a> = (&'a str, &'a [(Option<&'a str>, Ty)]);

fn push_adt(
    globals: &Globals,
    items: &mut Vec<IfaceItem>,
    name: &str,
    vis: Vis,
    exposed: bool,
    ctors: &[CtorDecl<'_>],
) {
    let ty_index = DefIndex::from_u32(u32::try_from(items.len()).unwrap());
    let ctor_indexes: Box<[DefIndex]> = (1..=ctors.len())
        .map(|n| {
            DefIndex::from_u32(
                ty_index
                    .as_u32()
                    .checked_add(u32::try_from(n).unwrap())
                    .unwrap(),
            )
        })
        .collect();
    let item = |name: &str, kind, parent, sig| IfaceItem {
        name: globals.names.intern(name),
        kind,
        vis,
        parent,
        span: None,
        targets: Targets::ALL,
        deprecated: None,
        sig,
    };
    items.push(item(
        name,
        DefKind::Type,
        None,
        ItemSig::Type(TypeDef {
            generics: Generics::empty(),
            kind: TypeDefKind::Adt {
                ctors: ctor_indexes,
            },
            exposed,
            home: None,
            hooks: None,
        }),
    ));
    for (tag, (ctor, fields)) in (0_u32..).zip(ctors) {
        let fields = match fields {
            [] => CtorFields::Unit,
            [(None, _), ..] => CtorFields::Positional(fields.iter().map(|f| f.1).collect()),
            _ => CtorFields::Named(
                fields
                    .iter()
                    .map(|(n, t)| (globals.names.intern(n.unwrap()), *t))
                    .collect(),
            ),
        };
        items.push(item(
            ctor,
            DefKind::Ctor,
            Some(ty_index),
            ItemSig::Ctor(CtorSig {
                ty: ty_index,
                tag,
                fields,
            }),
        ));
    }
}

fn interface(module: ModuleId, package: PackageId, items: Vec<IfaceItem>) -> ModuleInterface {
    ModuleInterface::new(module, package, IndexVec::from_raw(items).unwrap()).unwrap()
}

struct World {
    globals: Globals,
    modules: ModuleTable,
    set: InterfaceSet,
    app: ModuleId,
    vault: ModuleId,
    geo: ModuleId,
    own: ModuleInterface,
    allowed: Vec<ModuleId>,
}

const OTHER_PACKAGE: PackageId = PackageId::from_u32(2);

impl World {
    fn new() -> Self {
        let globals = Globals::new();
        let mut modules = ModuleTable::new();
        let mut add = |path: &str, package| {
            modules
                .add(ModuleInfo {
                    path: ModulePath::root(path),
                    package,
                    file: None,
                })
                .unwrap()
        };
        let app = add("app", PackageId::LOCAL);
        let vault = add("vault", PackageId::LOCAL);
        let geo = add("geo", OTHER_PACKAGE);

        let mut items = Vec::new();
        push_adt(
            &globals,
            &mut items,
            "Color",
            Vis::Package,
            false,
            &[("Red", &[]), ("Green", &[]), ("Blue", &[])],
        );
        push_adt(
            &globals,
            &mut items,
            "Shape",
            Vis::Package,
            false,
            &[("Circle", &[(Some("radius"), Ty::INT)]), ("Dot", &[])],
        );
        let own = interface(app, PackageId::LOCAL, items);

        let mut items = Vec::new();
        push_adt(
            &globals,
            &mut items,
            "Secret",
            Vis::Priv,
            false,
            &[("Hidden", &[(None, Ty::INT)])],
        );
        let vault_iface = interface(vault, PackageId::LOCAL, items);

        let mut items = Vec::new();
        push_adt(
            &globals,
            &mut items,
            "Opaque",
            Vis::Pub,
            false,
            &[("Opaque", &[(None, Ty::INT)])],
        );
        push_adt(
            &globals,
            &mut items,
            "Dir",
            Vis::Pub,
            true,
            &[("North", &[]), ("South", &[])],
        );
        let geo_iface = interface(geo, OTHER_PACKAGE, items);

        let mut set = InterfaceSet::new(builtin::interface(&globals));
        set.insert(Arc::new(vault_iface)).unwrap();
        set.insert(Arc::new(geo_iface)).unwrap();
        Self {
            globals,
            modules,
            set,
            app,
            vault,
            geo,
            own,
            allowed: vec![ModuleId::BUILTIN, vault, geo],
        }
    }

    fn named(&self, module: ModuleId, index: u32, args: &[Ty]) -> Ty {
        self.globals
            .types
            .named(DefId::new(module, DefIndex::from_u32(index)), args)
    }

    fn color(&self) -> Ty {
        self.named(self.app, 0, &[])
    }
    fn shape(&self) -> Ty {
        self.named(self.app, 4, &[])
    }
    fn option(&self, t: Ty) -> Ty {
        self.globals.types.named(builtin::OPTION, [t])
    }
    fn list(&self, t: Ty) -> Ty {
        self.globals.types.named(builtin::LIST, [t])
    }
    fn tuple(&self, elems: &[Ty]) -> Ty {
        self.globals.types.intern(TyKind::Tuple(elems.into()))
    }

    fn cx<'a>(&'a self, deps: &'a DepView<'a>) -> TyCx<'a> {
        TyCx {
            module: self.app,
            package: PackageId::LOCAL,
            deps,
            modules: &self.modules,
            globals: &self.globals,
            own: &self.own,
        }
    }

    /// The facts of the `case`s that `log` pairs with a node.
    fn facts(&self, body: &Body, log: &[(NodeId, ExprId)]) -> Vec<CaseFact> {
        let deps = DepView::new(&self.set, &self.allowed);
        collect_case_facts(&self.cx(&deps), &body.exprs, log.iter().copied(), &[])
    }

    /// The pattern of the constructor at `index` of this module.
    fn missing_ctor(&self, index: u32) -> MissingPat {
        MissingPat::Ctor {
            ctor: DefId::new(self.app, DefIndex::from_u32(index)),
            fields: Box::new([]),
        }
    }

    /// Checks `body` and renders the diagnostics against `src`.
    fn check(&self, src: &str, body: &Body) -> String {
        let deps = DepView::new(&self.set, &self.allowed);
        let diags = check(&self.cx(&deps), body).unwrap();
        let mut sources = SourceMap::new();
        sources.add_file("src/app.cheby", src).unwrap();
        render_all(&diags, &sources)
    }
}

const FILE: FileId = FileId::new(0);

/// Builds a body over `src`, with spans found by text.
struct B<'s> {
    src: &'s str,
    exprs: IndexVec<ExprId, Expr>,
    next_pat: u32,
}

impl<'s> B<'s> {
    fn new(src: &'s str) -> Self {
        Self {
            src,
            exprs: IndexVec::new(),
            next_pat: 0,
        }
    }

    /// The span of the `nth` occurrence (from 0) of `needle`.
    fn at(&self, needle: &str, nth: usize) -> cheby_diag::Span {
        let start = self
            .src
            .match_indices(needle)
            .nth(nth)
            .unwrap_or_else(|| panic!("`{needle}` #{nth} not in the source"))
            .0;
        let start = u32::try_from(start).unwrap();
        let end = start
            .checked_add(u32::try_from(needle.len()).unwrap())
            .unwrap();
        cheby_diag::Span::new(FILE, TextRange::new(start, end))
    }

    fn expr(&mut self, kind: ExprKind, ty: Ty, needle: &str, nth: usize) -> ExprId {
        let span = self.at(needle, nth);
        self.exprs.push(Expr { kind, ty, span }).unwrap()
    }

    fn pat(&mut self, kind: PatKind, ty: Ty, needle: &str, nth: usize) -> Pat {
        let id = PatId::from_u32(self.next_pat);
        self.next_pat = self.next_pat.checked_add(1).unwrap();
        Pat {
            id,
            kind,
            ty,
            span: self.at(needle, nth),
        }
    }

    fn int(&mut self, n: i128, nth: usize) -> ExprId {
        self.expr(ExprKind::Lit(Lit::Int(n)), Ty::INT, &n.to_string(), nth)
    }

    fn local(&mut self, ty: Ty, needle: &str, nth: usize) -> ExprId {
        self.expr(ExprKind::Local(LocalId::from_u32(0)), ty, needle, nth)
    }

    fn body(self, value: ExprId) -> Body {
        let locals = IndexVec::from_raw(vec![Local {
            name: sym::T,
            ty: Ty::INT,
            span: cheby_diag::Span::new(FILE, TextRange::new(0, 0)),
            synthetic: false,
        }])
        .unwrap();
        Body {
            params: Vec::new(),
            locals,
            exprs: self.exprs,
            value,
            ret: Ty::INT,
        }
    }
}

fn arm(pats: Vec<Pat>, guard: Option<ExprId>, body: ExprId) -> Arm {
    let span = pats[0].span;
    Arm {
        pats: pats.into(),
        guard,
        body,
        span,
    }
}

fn ctor(module: ModuleId, index: u32) -> PatKind {
    unit(DefId::new(module, DefIndex::from_u32(index)))
}

fn unit(def: DefId) -> PatKind {
    PatKind::Ctor {
        ctor: def,
        fields: Box::new([]),
    }
}

fn bind() -> PatKind {
    PatKind::Bind(LocalId::from_u32(0))
}

/// Node ids in preorder. Facts only carry them, so any tree will do.
fn node_ids() -> Vec<NodeId> {
    let parse = cheby_syntax::parse(FILE, "fn f() { ((1)) }".into());
    parse
        .tree()
        .root()
        .descendants()
        .map(SyntaxNode::id)
        .collect()
}

/// A `case` over `subjects`.
fn case_expr(bld: &mut B<'_>, subjects: &[ExprId], arms: Vec<Arm>, nth: usize) -> ExprId {
    case_of(bld, subjects, arms, CaseOrigin::Case, nth)
}

fn case_of(
    bld: &mut B<'_>,
    subjects: &[ExprId],
    arms: Vec<Arm>,
    origin: CaseOrigin,
    nth: usize,
) -> ExprId {
    let kind = ExprKind::Case {
        subjects: subjects.into(),
        arms: arms.into(),
        origin,
    };
    bld.expr(kind, Ty::INT, "case", nth)
}

#[test]
fn missing_variant() {
    let world = World::new();
    let src = "\
fn name(color: Color) -> Int {
  case color {
    Red => 1
    Green => 2
  }
}
";
    let mut bld = B::new(src);
    let color = world.color();
    let subject = bld.local(color, "color", 1);
    let red = bld.pat(ctor(world.app, 1), color, "Red", 0);
    let green = bld.pat(ctor(world.app, 2), color, "Green", 0);
    let one = bld.int(1, 0);
    let two = bld.int(2, 0);
    let case = bld.expr(
        ExprKind::Case {
            subjects: Box::new([subject]),
            arms: Box::new([arm(vec![red], None, one), arm(vec![green], None, two)]),
            origin: crate::thir::CaseOrigin::Case,
        },
        Ty::INT,
        "case",
        0,
    );
    let out = world.check(src, &bld.body(case));
    assert_eq!(
        out,
        "\
error[E0061]: `case` does not cover every value
 --> src/app.cheby:2:8
  |
2 |   case color {
  |        ^^^^^ `Blue` not covered
  |
  = help: add an arm for `Blue`

"
    );
}

#[test]
fn two_subjects_and_guards() {
    let world = World::new();
    let src = "\
fn f(a: Bool, b: Bool) -> Int {
  case a, b {
    True, True => 1
    False, _ when b => 2
    False, _ => 3
  }
}
";
    let mut bld = B::new(src);
    let sa = bld.local(Ty::BOOL, "a", 2);
    let sb = bld.local(Ty::BOOL, "b", 1);
    let t1 = bld.pat(unit(builtin::TRUE), Ty::BOOL, "True", 0);
    let t2 = bld.pat(unit(builtin::TRUE), Ty::BOOL, "True", 1);
    let f1 = bld.pat(unit(builtin::FALSE), Ty::BOOL, "False", 0);
    let w1 = bld.pat(PatKind::Wild, Ty::BOOL, "_", 0);
    let f2 = bld.pat(unit(builtin::FALSE), Ty::BOOL, "False", 1);
    let w2 = bld.pat(PatKind::Wild, Ty::BOOL, "_", 1);
    let guard = bld.local(Ty::BOOL, "b", 2);
    let one = bld.int(1, 0);
    let two = bld.int(2, 0);
    let three = bld.int(3, 0);
    let case = bld.expr(
        ExprKind::Case {
            subjects: Box::new([sa, sb]),
            arms: Box::new([
                arm(vec![t1, t2], None, one),
                arm(vec![f1, w1], Some(guard), two),
                arm(vec![f2, w2], None, three),
            ]),
            origin: crate::thir::CaseOrigin::Case,
        },
        Ty::INT,
        "case",
        0,
    );
    let out = world.check(src, &bld.body(case));
    assert_eq!(
        out,
        "\
error[E0061]: `case` does not cover every value
 --> src/app.cheby:2:8
  |
2 |   case a, b {
  |        ^^^^ `True, False` not covered
  |
  = note: an arm with a guard does not count, since its guard may be false
  = help: add an arm for `True, False`

"
    );
}

#[test]
fn unreachable_arms() {
    let world = World::new();
    let src = "\
fn f(n: Int) -> Int {
  case n {
    _ => 1
    0 => 2
  }
}
";
    let mut bld = B::new(src);
    let subject = bld.local(Ty::INT, "n", 1);
    let wild = bld.pat(PatKind::Wild, Ty::INT, "_", 0);
    let zero = bld.pat(PatKind::Lit(Lit::Int(0)), Ty::INT, "0", 0);
    let one = bld.int(1, 0);
    let two = bld.int(2, 0);
    let case = bld.expr(
        ExprKind::Case {
            subjects: Box::new([subject]),
            arms: Box::new([arm(vec![wild], None, one), arm(vec![zero], None, two)]),
            origin: crate::thir::CaseOrigin::Case,
        },
        Ty::INT,
        "case",
        0,
    );
    let out = world.check(src, &bld.body(case));
    assert_eq!(
        out,
        "\
warning[W0010]: this arm can never match
 --> src/app.cheby:4:5
  |
4 |     0 => 2
  |     ^ the arms before it match every value it matches
  |
  = help: remove the arm, or move it before the arms that cover it

"
    );
}

/// `let <pat> = <init>` as the only statement of a block whose value is
/// the statement's own init expression (types do not matter here).
fn let_block(bld: &mut B<'_>, pat: Pat, init: ExprId, assert: bool) -> ExprId {
    let stmt = if assert {
        Stmt::LetAssert {
            pat,
            init,
            message: None,
        }
    } else {
        Stmt::Let { pat, init }
    };
    bld.expr(
        ExprKind::Block {
            stmts: Box::new([stmt]),
            tail: None,
        },
        Ty::NIL,
        "{",
        0,
    )
}

#[test]
fn refutable_let() {
    let world = World::new();
    let src = "\
fn f(o: Option<Int>) {
  let Some(x) = o
}
";
    let mut bld = B::new(src);
    let opt = world.option(Ty::INT);
    let init = bld.local(opt, "o", 1);
    let x = bld.pat(bind(), Ty::INT, "x", 0);
    let pat = bld.pat(
        PatKind::Ctor {
            ctor: builtin::SOME,
            fields: Box::new([(0, x)]),
        },
        opt,
        "Some(x)",
        0,
    );
    let block = let_block(&mut bld, pat, init, false);
    let out = world.check(src, &bld.body(block));
    assert_eq!(
        out,
        "\
error[E0060]: refutable pattern in `let`
 --> src/app.cheby:2:7
  |
2 |   let Some(x) = o
  |       ^^^^^^^ `None` not covered
  |
  = note: this pattern must match every value of its type (§6.1)
  = help: use `case` to handle `None` as well, or `let assert` to panic when the value does not match

"
    );
}

#[test]
fn refutable_list_let_and_irrefutable_lets() {
    let world = World::new();
    let src = "let [first, ..] = xs; let (a, b) = t; let [..] = xs";
    let mut bld = B::new(src);
    let ints = world.list(Ty::INT);
    let first = bld.pat(bind(), Ty::INT, "first", 0);
    let rest = bld.pat(PatKind::Wild, ints, "..", 0);
    let pat = bld.pat(
        PatKind::List {
            prefix: Box::new([first]),
            rest: Some(Box::new(rest)),
            suffix: Box::new([]),
        },
        ints,
        "[first, ..]",
        0,
    );
    let init = bld.local(ints, "xs", 0);
    let s1 = Stmt::Let { pat, init };
    let pair = world.tuple(&[Ty::INT, world.color()]);
    let a = bld.pat(bind(), Ty::INT, "a", 0);
    let bb = bld.pat(bind(), world.color(), "b", 0);
    let pat = bld.pat(PatKind::Tuple(Box::new([a, bb])), pair, "(a, b)", 0);
    let init = bld.local(pair, "t", 1);
    let s2 = Stmt::Let { pat, init };
    let rest = bld.pat(PatKind::Wild, ints, "..", 1);
    let pat = bld.pat(
        PatKind::List {
            prefix: Box::new([]),
            rest: Some(Box::new(rest)),
            suffix: Box::new([]),
        },
        ints,
        "[..]",
        0,
    );
    let init = bld.local(ints, "xs", 1);
    let s3 = Stmt::Let { pat, init };
    let block = bld.expr(
        ExprKind::Block {
            stmts: Box::new([s1, s2, s3]),
            tail: None,
        },
        Ty::NIL,
        "let",
        0,
    );
    let out = world.check(src, &bld.body(block));
    assert!(
        out.starts_with("error[E0060]: refutable pattern in `let`\n"),
        "{out}"
    );
    assert!(out.contains("^^^^^^^^^^^ `[]` not covered"), "{out}");
    assert_eq!(out.matches("error[").count(), 1, "{out}");
}

#[test]
fn refutable_parameters_by_origin() {
    let world = World::new();
    let src = "fn(Red) { 1 } use Red <- f";
    let color = world.color();
    for (origin, message) in [
        (ParamOrigin::Closure, "refutable pattern in a parameter"),
        (ParamOrigin::LocalFn, "refutable pattern in a parameter"),
        (ParamOrigin::Use, "refutable pattern in a `use` binder"),
    ] {
        let mut bld = B::new(src);
        let pat = bld.pat(ctor(world.app, 1), color, "Red", 0);
        let span = pat.span;
        let body = bld.int(1, 0);
        let params = Box::new([Param { pat, span, origin }]);
        let kind = match origin {
            ParamOrigin::LocalFn => ExprKind::LocalFn {
                local: LocalId::from_u32(0),
                params,
                body,
            },
            _ => ExprKind::Closure { params, body },
        };
        let fn_ty = world.globals.types.func([color], Ty::INT);
        let closure = bld.expr(kind, fn_ty, "fn", 0);
        let out = world.check(src, &bld.body(closure));
        assert!(
            out.starts_with(&format!("error[E0060]: {message}\n")),
            "{out}"
        );
        assert!(out.contains("^^^ `Green` and `Blue` not covered"), "{out}");
    }
}

#[test]
fn let_assert() {
    let world = World::new();
    let src = "let assert (a, b) = t; let assert Some(x) = o";
    let mut bld = B::new(src);
    let pair = world.tuple(&[Ty::INT, Ty::STRING]);
    let a = bld.pat(bind(), Ty::INT, "a", 1);
    let bb = bld.pat(bind(), Ty::STRING, "b", 0);
    let pat = bld.pat(PatKind::Tuple(Box::new([a, bb])), pair, "(a, b)", 0);
    let init = bld.local(pair, "t", 1);
    let s1 = Stmt::LetAssert {
        pat,
        init,
        message: None,
    };
    let opt = world.option(Ty::INT);
    let x = bld.pat(bind(), Ty::INT, "x", 0);
    let pat = bld.pat(
        PatKind::Ctor {
            ctor: builtin::SOME,
            fields: Box::new([(0, x)]),
        },
        opt,
        "Some(x)",
        0,
    );
    let init = bld.local(opt, "o", 0);
    let s2 = Stmt::LetAssert {
        pat,
        init,
        message: None,
    };
    let block = bld.expr(
        ExprKind::Block {
            stmts: Box::new([s1, s2]),
            tail: None,
        },
        Ty::NIL,
        "let",
        0,
    );
    let out = world.check(src, &bld.body(block));
    assert_eq!(
        out,
        "\
warning[W0008]: `let assert` with a pattern that always matches
 --> src/app.cheby:1:12
  |
1 | let assert (a, b) = t; let assert Some(x) = o
  |            ^^^^^^ this pattern matches every value of its type
  |
  = help: use `let` instead, since the assertion can never fail

"
    );
}

#[test]
fn named_fields_and_other_modules() {
    let world = World::new();
    // `case s { Circle { radius: 0 } => 1 }` and
    // `case d { geo::North => 1 }`.
    let src = "case s { Circle { radius: 0 } => 1 } case d { geo::North => 1 }";
    let mut bld = B::new(src);
    let shape = world.shape();
    let s = bld.local(shape, "s", 1);
    let zero = bld.pat(PatKind::Lit(Lit::Int(0)), Ty::INT, "0", 0);
    let circle = bld.pat(
        PatKind::Ctor {
            ctor: DefId::new(world.app, DefIndex::from_u32(5)),
            fields: Box::new([(0, zero)]),
        },
        shape,
        "Circle { radius: 0 }",
        0,
    );
    let one = bld.int(1, 0);
    let c1 = bld.expr(
        ExprKind::Case {
            subjects: Box::new([s]),
            arms: Box::new([arm(vec![circle], None, one)]),
            origin: crate::thir::CaseOrigin::Case,
        },
        Ty::INT,
        "case",
        0,
    );
    let dir = world.named(world.geo, 2, &[]);
    let d = bld.local(dir, "d", 0);
    let north = bld.pat(ctor(world.geo, 3), dir, "geo::North", 0);
    let one = bld.int(1, 1);
    let c2 = bld.expr(
        ExprKind::Case {
            subjects: Box::new([d]),
            arms: Box::new([arm(vec![north], None, one)]),
            origin: crate::thir::CaseOrigin::Case,
        },
        Ty::INT,
        "case",
        1,
    );
    let block = bld.expr(
        ExprKind::Block {
            stmts: Box::new([Stmt::Expr(c1), Stmt::Expr(c2)]),
            tail: None,
        },
        Ty::NIL,
        "case",
        0,
    );
    let out = world.check(src, &bld.body(block));
    assert!(
        out.contains("`Circle { .. }` and `Dot` not covered"),
        "{out}"
    );
    assert!(out.contains("`geo::South` not covered"), "{out}");
}

#[test]
fn opaque_types_need_a_wildcard() {
    let world = World::new();
    let src = "case o { x => 1 } case s { _ => 1 } case p { Some(_) => 1 }";
    let mut bld = B::new(src);
    // An opaque type of another package: a binding covers it.
    let opaque = world.named(world.geo, 0, &[]);
    let o = bld.local(opaque, "o", 0);
    let x = bld.pat(bind(), opaque, "x", 0);
    let one = bld.int(1, 0);
    let c1 = bld.expr(
        ExprKind::Case {
            subjects: Box::new([o]),
            arms: Box::new([arm(vec![x], None, one)]),
            origin: crate::thir::CaseOrigin::Case,
        },
        Ty::INT,
        "case",
        0,
    );
    // A `priv` type of a sibling module.
    let secret = world.named(world.vault, 0, &[]);
    let s = bld.local(secret, "s", 0);
    let wild = bld.pat(PatKind::Wild, secret, "_", 0);
    let one = bld.int(1, 1);
    let c2 = bld.expr(
        ExprKind::Case {
            subjects: Box::new([s]),
            arms: Box::new([arm(vec![wild], None, one)]),
            origin: crate::thir::CaseOrigin::Case,
        },
        Ty::INT,
        "case",
        1,
    );
    // `Option<geo::Opaque>` still needs `None`.
    let opt = world.option(opaque);
    let p = bld.local(opt, "p", 0);
    let inner = bld.pat(PatKind::Wild, opaque, "_", 1);
    let some = bld.pat(
        PatKind::Ctor {
            ctor: builtin::SOME,
            fields: Box::new([(0, inner)]),
        },
        opt,
        "Some(_)",
        0,
    );
    let one = bld.int(1, 2);
    let c3 = bld.expr(
        ExprKind::Case {
            subjects: Box::new([p]),
            arms: Box::new([arm(vec![some], None, one)]),
            origin: crate::thir::CaseOrigin::Case,
        },
        Ty::INT,
        "case",
        2,
    );
    let block = bld.expr(
        ExprKind::Block {
            stmts: Box::new([Stmt::Expr(c1), Stmt::Expr(c2), Stmt::Expr(c3)]),
            tail: None,
        },
        Ty::NIL,
        "case",
        0,
    );
    let out = world.check(src, &bld.body(block));
    assert_eq!(out.matches("error[E0061]").count(), 1, "{out}");
    assert!(out.contains("`None` not covered"), "{out}");
    // The structure of these types is not visible here.
    let deps = DepView::new(&world.set, &world.allowed);
    let cx = super::TyCx {
        module: world.app,
        package: PackageId::LOCAL,
        deps: &deps,
        modules: &world.modules,
        globals: &world.globals,
        own: &world.own,
    };
    assert_eq!(cx.space(opaque).unwrap(), Space::Infinite);
    assert_eq!(cx.space(secret).unwrap(), Space::Infinite);
    assert_eq!(
        cx.space(world.named(world.geo, 2, &[])).unwrap(),
        Space::Variants(2)
    );
    assert_eq!(cx.space(Ty::NEVER).unwrap(), Space::Infinite);
    assert_eq!(cx.space(Ty::STRING).unwrap(), Space::Infinite);
    assert_eq!(cx.space(world.list(Ty::INT)).unwrap(), Space::List);
    assert_eq!(cx.space(Ty::BOOL).unwrap(), Space::Variants(2));
}

#[test]
fn nested_and_subjectless_cases() {
    let world = World::new();
    // A subjectless `case` is skipped; the `case` inside a closure inside
    // a `let` initializer is found.
    let src = "case { _ => 1 } let f = fn(o) { case o { None => 1 } }";
    let mut bld = B::new(src);
    let one = bld.int(1, 0);
    let condless = bld.expr(
        ExprKind::Case {
            subjects: Box::new([]),
            arms: Box::new([Arm {
                pats: Box::new([]),
                guard: None,
                body: one,
                span: bld.at("_ => 1", 0),
            }]),
            origin: crate::thir::CaseOrigin::Case,
        },
        Ty::INT,
        "case",
        0,
    );
    let opt = world.option(Ty::BOOL);
    let o = bld.local(opt, "o", 1);
    let none = bld.pat(unit(builtin::NONE), opt, "None", 0);
    let one = bld.int(1, 1);
    let inner = bld.expr(
        ExprKind::Case {
            subjects: Box::new([o]),
            arms: Box::new([arm(vec![none], None, one)]),
            origin: crate::thir::CaseOrigin::Case,
        },
        Ty::INT,
        "case",
        1,
    );
    let param = bld.pat(bind(), opt, "o", 0);
    let span = param.span;
    let fn_ty = world.globals.types.func([opt], Ty::INT);
    let closure = bld.expr(
        ExprKind::Closure {
            params: Box::new([Param {
                pat: param,
                span,
                origin: ParamOrigin::Closure,
            }]),
            body: inner,
        },
        fn_ty,
        "fn",
        0,
    );
    let f = bld.pat(bind(), fn_ty, "f", 0);
    let block = bld.expr(
        ExprKind::Block {
            stmts: Box::new([
                Stmt::Expr(condless),
                Stmt::Let {
                    pat: f,
                    init: closure,
                },
            ]),
            tail: None,
        },
        Ty::NIL,
        "case",
        0,
    );
    let out = world.check(src, &bld.body(block));
    assert_eq!(out.matches("error[").count(), 1, "{out}");
    assert!(out.contains("`Some(_)` not covered"), "{out}");
}

#[test]
fn witness_lists() {
    let shown: Vec<String> = ["A", "B", "C", "D", "E"].map(str::to_owned).to_vec();
    assert_eq!(super::list(&shown[..1], false), "`A`");
    assert_eq!(super::list(&shown[..2], false), "`A` and `B`");
    assert_eq!(super::list(&shown[..3], false), "`A`, `B` and `C`");
    assert_eq!(super::list(&shown, false), "`A`, `B`, `C` and 2 more");
    assert_eq!(super::list(&shown, true), "`A`, `B`, `C` and more");
}

#[test]
fn patterns_over_the_error_type_are_not_checked() {
    // `fn f(x: Result<Int>)`: the signature's error leaves `x` at the
    // error type, and its constructor patterns must not be split.
    let world = World::new();
    let src = "case x { Some(_) => 1  None => 0 } let (a, b) = x";
    let mut bld = B::new(src);
    let subject = bld.local(Ty::ERROR, "x", 0);
    let wild = bld.pat(PatKind::Wild, Ty::ERROR, "_", 0);
    let some = bld.pat(
        PatKind::Ctor {
            ctor: builtin::SOME,
            fields: Box::new([(0, wild)]),
        },
        Ty::ERROR,
        "Some(_)",
        0,
    );
    let none = bld.pat(unit(builtin::NONE), Ty::ERROR, "None", 0);
    let one = bld.int(1, 0);
    let zero = bld.int(0, 0);
    let case = bld.expr(
        ExprKind::Case {
            subjects: Box::new([subject]),
            arms: Box::new([arm(vec![some], None, one), arm(vec![none], None, zero)]),
            origin: crate::thir::CaseOrigin::Case,
        },
        Ty::INT,
        "case",
        0,
    );
    let a = bld.pat(bind(), Ty::ERROR, "a", 0);
    let b = bld.pat(bind(), Ty::ERROR, "b", 0);
    let pat = bld.pat(PatKind::Tuple(Box::new([a, b])), Ty::ERROR, "(a, b)", 0);
    let init = bld.local(Ty::ERROR, "x", 1);
    let block = bld.expr(
        ExprKind::Block {
            stmts: Box::new([Stmt::Let { pat, init }]),
            tail: Some(case),
        },
        Ty::INT,
        "let",
        0,
    );
    assert_eq!(world.check(src, &bld.body(block)), "");
}

#[test]
fn alternatives_that_cover_only_together_are_refutable() {
    // §6.1: `Red | Green | Blue` covers `Color`, but no alternative does
    // alone, so `let` rejects it and `let assert` does not warn.
    let world = World::new();
    let src = "let Red | Green | Blue = c; let assert Red | Green | Blue = c";
    let color = world.color();
    let mut stmts = Vec::new();
    let mut bld = B::new(src);
    for (nth, assert) in [(0, false), (1, true)] {
        let alts: Vec<Pat> = [("Red", 1), ("Green", 2), ("Blue", 3)]
            .into_iter()
            .map(|(name, index)| bld.pat(ctor(world.app, index), color, name, nth))
            .collect();
        let pat = bld.pat(PatKind::Or(alts.into()), color, "Red | Green | Blue", nth);
        let init = bld.local(color, "c", nth);
        stmts.push(if assert {
            Stmt::LetAssert {
                pat,
                init,
                message: None,
            }
        } else {
            Stmt::Let { pat, init }
        });
    }
    let block = bld.expr(
        ExprKind::Block {
            stmts: stmts.into(),
            tail: None,
        },
        Ty::NIL,
        "let",
        0,
    );
    let out = world.check(src, &bld.body(block));
    assert_eq!(
        out,
        "\
error[E0060]: refutable pattern in `let`
 --> src/app.cheby:1:5
  |
1 | let Red | Green | Blue = c; let assert Red | Green | Blue = c
  |     ^^^^^^^^^^^^^^^^^^ its alternatives match every value only together
  |
  = note: alternatives make a pattern irrefutable only when one of them is irrefutable on its own (§6.1)
  = help: match the value with `case`, with one arm per alternative

"
    );
}

/// `case color { Red => 1 }`, with the text of [`one_arm_case`]'s bodies.
const ONE_ARM: &str = "case color { Red => 1 } case color { Red => 1 } case";

/// A `case` over a `Color` with one arm, `Red`.
fn one_arm_case(world: &World, bld: &mut B<'_>, nth: usize) -> ExprId {
    let color = world.color();
    let subject = bld.local(color, "color", nth);
    let red = bld.pat(ctor(world.app, 1), color, "Red", nth);
    let one = bld.int(1, nth);
    case_expr(bld, &[subject], vec![arm(vec![red], None, one)], nth)
}

#[test]
fn a_fact_names_each_missing_value() {
    let world = World::new();
    let mut bld = B::new(ONE_ARM);
    let case = one_arm_case(&world, &mut bld, 0);
    let nodes = node_ids();
    let facts = world.facts(&bld.body(case), &[(nodes[0], case)]);
    let [fact] = facts.as_slice() else {
        panic!("{facts:?}");
    };
    assert_eq!(fact.node, nodes[0]);
    assert_eq!(
        fact.missing,
        [[world.missing_ctor(2)], [world.missing_ctor(3)]]
    );
    assert!(!fact.more);
    assert!(!fact.too_complex);
    assert_eq!(fact.unreachable.len(), 0);
}

#[test]
fn a_fact_has_one_pattern_per_subject() {
    let world = World::new();
    let src = "case a, b { True, True => 1  False, _ => 2 }";
    let mut bld = B::new(src);
    let a = bld.local(Ty::BOOL, "a", 0);
    let b = bld.local(Ty::BOOL, "b", 0);
    let t1 = bld.pat(unit(builtin::TRUE), Ty::BOOL, "True", 0);
    let t2 = bld.pat(unit(builtin::TRUE), Ty::BOOL, "True", 1);
    let f = bld.pat(unit(builtin::FALSE), Ty::BOOL, "False", 0);
    let w = bld.pat(PatKind::Wild, Ty::BOOL, "_", 0);
    let one = bld.int(1, 0);
    let two = bld.int(2, 0);
    let arms = vec![arm(vec![t1, t2], None, one), arm(vec![f, w], None, two)];
    let case = case_expr(&mut bld, &[a, b], arms, 0);
    let nodes = node_ids();
    let facts = world.facts(&bld.body(case), &[(nodes[0], case)]);
    let bool_pat = |ctor| MissingPat::Ctor {
        ctor,
        fields: Box::new([]),
    };
    assert_eq!(
        facts[0].missing,
        [[bool_pat(builtin::TRUE), bool_pat(builtin::FALSE)]]
    );
}

#[test]
fn a_tuple_of_a_constructor_nests() {
    let world = World::new();
    let src = "case p { (True, _) => 1  (False, Some(_)) => 2 }";
    let mut bld = B::new(src);
    let opt = world.option(Ty::BOOL);
    let pair = world.tuple(&[Ty::BOOL, opt]);
    let p = bld.local(pair, "p", 0);
    let t = bld.pat(unit(builtin::TRUE), Ty::BOOL, "True", 0);
    let w = bld.pat(PatKind::Wild, opt, "_", 0);
    let first = bld.pat(PatKind::Tuple(Box::new([t, w])), pair, "(True, _)", 0);
    let f = bld.pat(unit(builtin::FALSE), Ty::BOOL, "False", 0);
    let inner = bld.pat(PatKind::Wild, Ty::BOOL, "_", 1);
    let some = bld.pat(
        PatKind::Ctor {
            ctor: builtin::SOME,
            fields: Box::new([(0, inner)]),
        },
        opt,
        "Some(_)",
        0,
    );
    let second = bld.pat(
        PatKind::Tuple(Box::new([f, some])),
        pair,
        "(False, Some(_))",
        0,
    );
    let one = bld.int(1, 0);
    let two = bld.int(2, 0);
    let arms = vec![arm(vec![first], None, one), arm(vec![second], None, two)];
    let case = case_expr(&mut bld, &[p], arms, 0);
    let nodes = node_ids();
    let facts = world.facts(&bld.body(case), &[(nodes[0], case)]);
    let unit_pat = |ctor| MissingPat::Ctor {
        ctor,
        fields: Box::new([]),
    };
    assert_eq!(
        facts[0].missing,
        [[MissingPat::Tuple(Box::new([
            unit_pat(builtin::FALSE),
            unit_pat(builtin::NONE),
        ]))]]
    );
}

#[test]
fn an_exhaustive_case_has_a_fact_with_no_rows() {
    let world = World::new();
    let src = "case b { True => 1  _ => 2  False => 3 }";
    let mut bld = B::new(src);
    let b = bld.local(Ty::BOOL, "b", 0);
    let t = bld.pat(unit(builtin::TRUE), Ty::BOOL, "True", 0);
    let w = bld.pat(PatKind::Wild, Ty::BOOL, "_", 0);
    let f = bld.pat(unit(builtin::FALSE), Ty::BOOL, "False", 0);
    let one = bld.int(1, 0);
    let two = bld.int(2, 0);
    let three = bld.int(3, 0);
    let arms = vec![
        arm(vec![t], None, one),
        arm(vec![w], None, two),
        arm(vec![f], None, three),
    ];
    let case = case_expr(&mut bld, &[b], arms, 0);
    let nodes = node_ids();
    let facts = world.facts(&bld.body(case), &[(nodes[0], case)]);
    assert_eq!(facts[0].missing.len(), 0);
    assert_eq!(facts[0].unreachable, [2]);
}

#[test]
fn a_guarded_arm_covers_nothing() {
    let world = World::new();
    let src = "case b { _ when g => 1  True => 2 }";
    let mut bld = B::new(src);
    let b = bld.local(Ty::BOOL, "b", 0);
    let guard = bld.local(Ty::BOOL, "g", 0);
    let w = bld.pat(PatKind::Wild, Ty::BOOL, "_", 0);
    let t = bld.pat(unit(builtin::TRUE), Ty::BOOL, "True", 0);
    let one = bld.int(1, 0);
    let two = bld.int(2, 0);
    let arms = vec![arm(vec![w], Some(guard), one), arm(vec![t], None, two)];
    let case = case_expr(&mut bld, &[b], arms, 0);
    let nodes = node_ids();
    let facts = world.facts(&bld.body(case), &[(nodes[0], case)]);
    assert_eq!(
        facts[0].missing,
        [[MissingPat::Ctor {
            ctor: builtin::FALSE,
            fields: Box::new([]),
        }]]
    );
}

#[test]
fn facts_follow_the_log_and_sort_by_node() {
    // Neither `case` hangs from the body's root, as in an orphaned
    // argument, and the log holds only the two it names.
    let world = World::new();
    let mut bld = B::new(ONE_ARM);
    let first = one_arm_case(&world, &mut bld, 0);
    let second = one_arm_case(&world, &mut bld, 1);
    let root = bld.int(1, 0);
    let body = bld.body(root);
    let nodes = node_ids();
    assert!(world.facts(&body, &[(nodes[0], root)]).is_empty());
    let facts = world.facts(&body, &[(nodes[2], first), (nodes[1], second)]);
    let found: Vec<NodeId> = facts.iter().map(|fact| fact.node).collect();
    assert_eq!(found, [nodes[1], nodes[2]]);
}

#[test]
fn a_parenthesis_leaves_one_fact_at_the_case_node() {
    let world = World::new();
    let mut bld = B::new(ONE_ARM);
    let case = one_arm_case(&world, &mut bld, 0);
    let body = bld.body(case);
    let nodes = node_ids();
    for log in [
        [(nodes[1], case), (nodes[0], case)],
        [(nodes[0], case), (nodes[1], case)],
    ] {
        let facts = world.facts(&body, &log);
        let found: Vec<NodeId> = facts.iter().map(|fact| fact.node).collect();
        assert_eq!(found, [nodes[1]]);
    }
}

#[test]
fn a_node_keeps_one_fact() {
    let world = World::new();
    let mut bld = B::new(ONE_ARM);
    let first = one_arm_case(&world, &mut bld, 0);
    let second = one_arm_case(&world, &mut bld, 1);
    let nodes = node_ids();
    let facts = world.facts(&bld.body(first), &[(nodes[0], first), (nodes[0], second)]);
    assert_eq!(facts.len(), 1);
}

#[test]
fn no_fact_without_something_to_split() {
    let world = World::new();
    let src = "case x { _ => 1 } case { _ => 1 } case y { Red => 1 } 1";
    let mut bld = B::new(src);
    let one = bld.int(1, 0);

    // A subject at the error type (D-494).
    let x = bld.local(Ty::ERROR, "x", 0);
    let w = bld.pat(PatKind::Wild, Ty::ERROR, "_", 0);
    let error_subject = case_expr(&mut bld, &[x], vec![arm(vec![w], None, one)], 0);

    // A subjectless `case`.
    let span = bld.at("_ => 1", 1);
    let arms = vec![Arm {
        pats: Box::new([]),
        guard: None,
        body: one,
        span,
    }];
    let subjectless = case_expr(&mut bld, &[], arms, 1);

    // A `let guard`, which is not a `case` to the user.
    let color = world.color();
    let y = bld.local(color, "y", 0);
    let red = bld.pat(ctor(world.app, 1), color, "Red", 0);
    let arms = vec![arm(vec![red], None, one)];
    let let_guard = case_of(&mut bld, &[y], arms, CaseOrigin::LetGuard, 2);

    let nodes = node_ids();
    let log = [
        (nodes[0], error_subject),
        (nodes[1], subjectless),
        (nodes[2], let_guard),
        (nodes[3], one),
    ];
    assert!(world.facts(&bld.body(one), &log).is_empty());
}

#[test]
fn an_arm_with_the_wrong_count_is_left_out() {
    let world = World::new();
    let src = "case a, b { True => 1  True, True => 2  _, _ => 3  True, _ => 4 }";
    let mut bld = B::new(src);
    let a = bld.local(Ty::BOOL, "a", 0);
    let b = bld.local(Ty::BOOL, "b", 0);
    let lone = bld.pat(unit(builtin::TRUE), Ty::BOOL, "True", 0);
    let t1 = bld.pat(unit(builtin::TRUE), Ty::BOOL, "True", 1);
    let t2 = bld.pat(unit(builtin::TRUE), Ty::BOOL, "True", 2);
    let w1 = bld.pat(PatKind::Wild, Ty::BOOL, "_", 0);
    let w2 = bld.pat(PatKind::Wild, Ty::BOOL, "_", 1);
    let t3 = bld.pat(unit(builtin::TRUE), Ty::BOOL, "True", 3);
    let w3 = bld.pat(PatKind::Wild, Ty::BOOL, "_", 2);
    let one = bld.int(1, 0);
    let arms = vec![
        arm(vec![lone], None, one),
        arm(vec![t1, t2], None, one),
        arm(vec![w1, w2], None, one),
        arm(vec![t3, w3], None, one),
    ];
    let case = case_expr(&mut bld, &[a, b], arms, 0);
    let nodes = node_ids();
    let facts = world.facts(&bld.body(case), &[(nodes[0], case)]);
    assert_eq!(facts[0].missing.len(), 0);
    // By index among the three checked arms.
    assert_eq!(facts[0].unreachable, [2]);
}

#[test]
fn an_ice_loses_one_case() {
    let world = World::new();
    let src = "case f { 1.5 => 1 } case c { Red => 1 } case color { Red => 1 }";
    let mut bld = B::new(src);
    let one = bld.int(1, 0);

    // A `Float` pattern is E0062's, and the engine has no such constructor.
    let f = bld.local(Ty::FLOAT, "f", 0);
    let float = bld.pat(
        PatKind::Lit(Lit::Float(FloatLit {
            wide: 1.5,
            narrow: 1.5,
        })),
        Ty::FLOAT,
        "1.5",
        0,
    );
    let bad_float = case_expr(&mut bld, &[f], vec![arm(vec![float], None, one)], 0);

    // A constructor that is not one.
    let color = world.color();
    let c = bld.local(color, "c", 0);
    let not_ctor = bld.pat(ctor(world.app, 0), color, "Red", 0);
    let bad_ctor = case_expr(&mut bld, &[c], vec![arm(vec![not_ctor], None, one)], 1);

    let subject = bld.local(color, "color", 0);
    let red = bld.pat(ctor(world.app, 1), color, "Red", 1);
    let good = case_expr(&mut bld, &[subject], vec![arm(vec![red], None, one)], 2);

    let nodes = node_ids();
    let log = [
        (nodes[0], bad_float),
        (nodes[1], good),
        (nodes[2], bad_ctor),
    ];
    let facts = world.facts(&bld.body(good), &log);
    let found: Vec<NodeId> = facts.iter().map(|fact| fact.node).collect();
    assert_eq!(found, [nodes[1]]);
}

#[test]
fn witnesses_become_data() {
    let world = World::new();
    let deps = DepView::new(&world.set, &world.allowed);
    let cx = world.cx(&deps);
    let leaf = |ty, ctor| Witness {
        ty,
        kind: WitnessKind::Ctor(ctor, Vec::new()),
    };
    let wild = |ty| Witness {
        ty,
        kind: WitnessKind::Wild,
    };
    let list = world.list(Ty::INT);
    let of = |witness: Witness<Ty>| cx.missing_pat(&witness).unwrap();

    assert_eq!(of(wild(Ty::INT)), MissingPat::Wild);
    assert_eq!(
        of(leaf(Ty::INT, Ctor::Lit(LitKey::Int(7)))),
        MissingPat::Int(7)
    );
    assert_eq!(
        of(leaf(Ty::STRING, Ctor::Lit(LitKey::Str("a".into())))),
        MissingPat::Str("a".into())
    );
    assert_eq!(
        of(Witness {
            ty: list,
            kind: WitnessKind::Ctor(Ctor::List(ListLen::Fixed(1)), vec![wild(Ty::INT)]),
        }),
        MissingPat::List {
            items: Box::new([MissingPat::Wild]),
            rest_at: None,
        }
    );
    assert_eq!(
        of(Witness {
            ty: list,
            kind: WitnessKind::Ctor(
                Ctor::List(ListLen::Var {
                    prefix: 1,
                    suffix: 1,
                }),
                vec![wild(Ty::INT), wild(Ty::INT)],
            ),
        }),
        MissingPat::List {
            items: Box::new([MissingPat::Wild, MissingPat::Wild]),
            rest_at: Some(1),
        }
    );
    // `Shape`'s first variant, by def, with its field.
    assert_eq!(
        of(Witness {
            ty: world.shape(),
            kind: WitnessKind::Ctor(Ctor::Variant(0), vec![wild(Ty::INT)]),
        }),
        MissingPat::Ctor {
            ctor: DefId::new(world.app, DefIndex::from_u32(5)),
            fields: Box::new([MissingPat::Wild]),
        }
    );
    // A variant at a type without variants is an `Ice`.
    assert!(cx.missing_pat(&leaf(Ty::INT, Ctor::Variant(0))).is_err());
}

/// What the tests below read of one module's recording run.
struct Run<'a> {
    cx: TyCx<'a>,
    tree: &'a SyntaxTree,
    diags: &'a [Diagnostic],
    facts: &'a ModuleFacts,
    fns: usize,
}

impl Run<'_> {
    fn cases(&self) -> impl Iterator<Item = &CaseFact> {
        self.facts.bodies.iter().flat_map(|body| body.cases.iter())
    }

    /// The module's facts with missing rows, as `missing` shows them.
    fn rows(&self) -> Vec<Vec<String>> {
        self.cases()
            .filter(|fact| !fact.missing.is_empty())
            .map(|fact| rows(&self.cx, fact))
            .collect()
    }

    fn e0061(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diags
            .iter()
            .filter(|diag| diag.code == codes::NON_EXHAUSTIVE_CASE)
    }
}

/// Finds the modules of the package among themselves.
struct LocalModules<'a>(&'a ModuleTable);

impl ModuleLookup for LocalModules<'_> {
    fn lookup(&mut self, _from: PackageId, path: &[&str]) -> Result<Lookup, Ice> {
        let Some((&first, rest)) = path.split_first() else {
            return Ok(Lookup::NoPackage);
        };
        let path = rest.iter().fold(ModulePath::root(first), |p, s| p.child(s));
        Ok(self.0.find(&path).map_or(Lookup::NoPackage, Lookup::Found))
    }
}

/// Runs package `app` through the front end with recording on, and gives
/// `each` every module. `local` is `(path under the root, text)` in
/// dependency order, and the root's path is empty. Unlike
/// [`crate::test_support`], this keeps a source with a syntax error, as a
/// tolerant run does.
fn record(local: &[(String, String)], mut each: impl FnMut(&Run<'_>)) {
    let globals = Globals::new();
    let mut table = ModuleTable::new();
    let mut sources = SourceMap::new();
    let mut parsed = Vec::new();
    for (path, text) in local {
        let shown = format!("src/{}.cheby", path.replace("::", "/"));
        let file = sources.add_file(shown, text.as_str()).unwrap();
        let module = path
            .split("::")
            .filter(|s| !s.is_empty())
            .fold(ModulePath::root("app"), |p, s| p.child(s));
        let id = table
            .add(ModuleInfo {
                path: module,
                package: PackageId::LOCAL,
                file: Some(file),
            })
            .unwrap();
        let (tree, _) = cheby_syntax::parse(file, Arc::from(text.as_str())).into_parts();
        parsed.push((id, tree));
    }
    let mut set = InterfaceSet::new(builtin::interface(&globals));
    let mut deps: Vec<ModuleId> = Vec::new();
    for (id, tree) in &parsed {
        let imports = collect_imports(tree, &globals.names);
        let mut lookup = LocalModules(&table);
        let (targets, _) =
            resolve_import_targets(&imports, PackageId::LOCAL, &mut lookup, &globals.names)
                .unwrap();
        let (resolved, _) = resolve_module(&ResolveInput {
            module: *id,
            package: PackageId::LOCAL,
            tree,
            imports: &imports,
            targets: &targets,
            deps: DepView::new(&set, &deps),
            modules: &table,
            globals: &globals,
            std: StdCatalog { known: &[] },
            package_targets: Targets::ALL,
        })
        .unwrap();
        let input = TypesInput {
            module: *id,
            package: PackageId::LOCAL,
            tree,
            resolved: &resolved,
            deps: DepView::new(&set, &deps),
            modules: &table,
            globals: &globals,
        };
        let (own, _) = build_interface(&input).unwrap();
        let (module, diags, facts) = check_bodies_recording(&input, &own, Tests::All).unwrap();
        each(&Run {
            cx: TyCx::new(&input, &own),
            tree,
            diags: &diags,
            facts: &facts,
            fns: module.fns.len(),
        });
        set.insert(Arc::new(own)).unwrap();
        deps.push(*id);
    }
}

/// [`record`] for one module.
fn record_one(text: &str, each: impl FnOnce(&Run<'_>)) {
    let mut each = Some(each);
    record(&[(String::new(), text.to_owned())], |run| {
        each.take().unwrap()(run);
    });
}

/// Each row of `fact`, written as `render_row` writes it.
fn rows(cx: &TyCx<'_>, fact: &CaseFact) -> Vec<String> {
    let row = |row: &Vec<MissingPat>| shown(cx, row).join(", ");
    fact.missing.iter().map(row).collect()
}

fn shown(cx: &TyCx<'_>, pats: &[MissingPat]) -> Vec<String> {
    pats.iter().map(|pat| show(cx, pat)).collect()
}

/// A pattern as `engine::render` writes one. The server names constructors
/// its own way, but what it gets to work with is this.
fn show(cx: &TyCx<'_>, pat: &MissingPat) -> String {
    match pat {
        MissingPat::Wild => "_".to_owned(),
        MissingPat::Int(n) => n.to_string(),
        MissingPat::Str(s) => format!("{s:?}"),
        MissingPat::Tuple(items) => format!("({})", shown(cx, items).join(", ")),
        MissingPat::Ctor { ctor, fields } => {
            let name = cx.ctor_name(*ctor).unwrap();
            match &cx.ctor_sig(*ctor).unwrap().fields {
                CtorFields::Unit => name,
                CtorFields::Positional(_) => format!("{name}({})", shown(cx, fields).join(", ")),
                CtorFields::Named(names) => {
                    let mut parts: Vec<String> = names
                        .iter()
                        .zip(fields)
                        .filter(|(_, field)| **field != MissingPat::Wild)
                        .map(|((label, _), field)| {
                            let label = cx.globals.names.get(*label);
                            format!("{label}: {}", show(cx, field))
                        })
                        .collect();
                    if parts.len() != names.len() {
                        parts.push("..".to_owned());
                    }
                    format!("{name} {{ {} }}", parts.join(", "))
                }
            }
        }
        MissingPat::List { items, rest_at } => {
            let mut parts = shown(cx, items);
            if let Some(at) = rest_at {
                let at = usize::try_from(*at).unwrap();
                let back = items.get(at..).unwrap();
                let tail = back.iter().all(|item| *item == MissingPat::Wild);
                parts.insert(if tail { parts.len() } else { at }, "..".to_owned());
            }
            format!("[{}]", parts.join(", "))
        }
    }
}

/// The sources of the UI test whose golden is `stderr`, in dependency order.
fn ui_sources(stderr: &Path) -> Vec<(String, String)> {
    let file = stderr.with_extension("cheby");
    if file.exists() {
        return vec![(String::new(), std::fs::read_to_string(file).unwrap())];
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(stderr.with_extension("").join("src"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    // No directory test here imports anything but from its root.
    files.sort_by_key(|path| path.file_stem().is_some_and(|stem| stem == "main"));
    files
        .iter()
        .map(|path| {
            let stem = path.file_stem().unwrap().to_str().unwrap();
            let name = if stem == "main" { "" } else { stem };
            (name.to_owned(), std::fs::read_to_string(path).unwrap())
        })
        .collect()
}

/// Every `.stderr` golden under `dir` that holds an E0061.
fn e0061_goldens(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            e0061_goldens(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "stderr")
            && std::fs::read_to_string(&path)
                .unwrap()
                .contains("error[E0061]")
        {
            out.push(path);
        }
    }
}

#[test]
fn every_e0061_of_the_ui_tests_has_its_rows() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/ui");
    let mut goldens = Vec::new();
    e0061_goldens(&root, &mut goldens);
    assert!(goldens.len() >= 9, "{goldens:?}");
    for golden in goldens {
        let want = std::fs::read_to_string(&golden)
            .unwrap()
            .matches("error[E0061]")
            .count();
        let mut found = 0_usize;
        record(&ui_sources(&golden), |run| {
            let with_rows = run.cases().filter(|fact| !fact.missing.is_empty()).count();
            assert_eq!(with_rows, run.e0061().count(), "{golden:?}");
            for diag in run.e0061() {
                found = found.checked_add(1).unwrap();
                let span = diag.primary_span().unwrap();
                // Preorder, so the last `case` around the subject is its own.
                let fact = run
                    .cases()
                    .filter(|fact| {
                        let node = run.tree.node(fact.node).unwrap();
                        assert_eq!(node.kind(), SyntaxKind::CASE_EXPR, "{golden:?}");
                        node.text_range().contains_range(span.range)
                    })
                    .last()
                    .unwrap();
                let label = format!(
                    "{} not covered",
                    super::list(&rows(&run.cx, fact), fact.more)
                );
                assert_eq!(diag.labels[0].message, label, "{golden:?}");
            }
        });
        assert_eq!(found, want, "{golden:?}");
    }
}

const COLOR: &str = "\
type Color {
  Red,
  Green,
  Blue,
}

";

/// `COLOR`, then `rest`.
fn with_color(rest: &str) -> String {
    format!("{COLOR}{rest}")
}

/// A type `Big` with more variants than a fact holds rows, then `rest`.
fn with_big(rest: &str) -> String {
    let variants: Vec<String> = (0..70).map(|n| format!("  V{n},")).collect();
    format!("type Big {{\n{}\n}}\n\n{rest}", variants.join("\n"))
}

#[test]
fn a_body_with_another_error_keeps_its_fact() {
    let src = with_color(
        r#"fn f(c: Color) -> Int {
  let bad: Int = "text"
  case c {
    Red => 1
  }
}
"#,
    );
    record_one(&src, |run| {
        assert!(run.e0061().next().is_none(), "{:?}", run.diags);
        let mismatch = codes::TYPE_MISMATCH;
        assert!(
            run.diags.iter().any(|d| d.code == mismatch),
            "{:?}",
            run.diags
        );
        assert_eq!(run.fns, 0);
        assert_eq!(run.rows(), [["Green", "Blue"]]);
        assert!(run.facts.bodies.iter().all(|body| body.errored));
    });
}

#[test]
fn a_case_in_an_orphaned_argument_has_a_fact() {
    let src = with_color(
        "fn g(n: Int) -> Int {
  n
}

fn f(c: Color) -> Int {
  g(
    1,
    case c {
      Red => 1
    },
  )
}
",
    );
    record_one(&src, |run| {
        assert_ne!(run.diags.len(), 0);
        assert_eq!(run.rows(), [["Green", "Blue"]]);
    });
}

#[test]
fn a_parenthesized_case_has_one_fact_at_its_node() {
    let src = with_color(
        "fn f(c: Color) -> Int {
  (
    case c {
      Red => 1
    }
  )
}
",
    );
    record_one(&src, |run| {
        let found: Vec<&CaseFact> = run.cases().collect();
        let [fact] = found.as_slice() else {
            panic!("{found:?}");
        };
        let node = run.tree.node(fact.node).unwrap();
        assert_eq!(node.kind(), SyntaxKind::CASE_EXPR);
    });
}

#[test]
fn a_failed_pattern_leaves_its_arm_out() {
    let cases: [(&str, &str, &[&str]); 5] = [
        // A misspelled constructor.
        ("Option<Int>", "Som(x) => 1\n    None => 2", &["Some(_)"]),
        // Every arm failed, so every constructor is missing.
        ("Option<Int>", "Som(x) => 1", &["Some(_)", "None"]),
        // A literal of the wrong type.
        ("Int", "\"a\" => 1\n    2 => 3", &["_"]),
        // Nested.
        (
            "Option<Int>",
            "Some(\"a\") => 1\n    None => 2",
            &["Some(_)"],
        ),
        // In one alternative of an or-pattern.
        (
            "Option<Int>",
            "None | Som(x) => 1\n    Some(y) => 2",
            &["None"],
        ),
    ];
    for (subject, arms, missing) in cases {
        let src = format!("fn f(o: {subject}) -> Int {{\n  case o {{\n    {arms}\n  }}\n}}\n");
        record_one(&src, |run| {
            let found: Vec<&CaseFact> = run.cases().collect();
            let [fact] = found.as_slice() else {
                panic!("{found:?}");
            };
            assert_eq!(rows(&run.cx, fact), missing, "{arms}");
            assert_eq!(fact.unreachable.len(), 0, "{arms}");
        });
    }
}

#[test]
fn a_clean_case_has_a_fact_without_rows() {
    let src = with_color(
        "fn f(c: Color) -> Int {
  case c {
    _ => 1
    Red => 2
  }
}
",
    );
    record_one(&src, |run| {
        let found: Vec<&CaseFact> = run.cases().collect();
        let [fact] = found.as_slice() else {
            panic!("{found:?}");
        };
        assert_eq!(fact.missing.len(), 0);
        assert_eq!(fact.unreachable, [1]);
    });
}

#[test]
fn a_case_with_no_arms_misses_every_constructor() {
    let src = with_color(
        "fn f(c: Color) -> Int {
  case c {
  }
}
",
    );
    record_one(&src, |run| {
        assert_eq!(run.rows(), [["Red", "Green", "Blue"]]);
    });
    let src = "\
fn f(o: Option<Int>) -> Int {
  case o {
  }
}
";
    record_one(src, |run| {
        assert_eq!(run.rows(), [["Some(_)", "None"]]);
    });
    // Nothing to name: the engine's `_` stays.
    let src = "\
fn f(n: Int, b: Bool) -> Int {
  case n {
  }
  case n, b {
  }
}
";
    record_one(src, |run| {
        assert_eq!(run.rows(), [["_"], ["_, _"]]);
    });
}

#[test]
fn the_limit_holds_with_no_arms() {
    let src = with_big(
        "fn f(b: Big) -> Int {
  case b {
  }
}
",
    );
    record_one(&src, |run| {
        let found: Vec<&CaseFact> = run.cases().collect();
        let [fact] = found.as_slice() else {
            panic!("{found:?}");
        };
        assert_eq!(fact.missing.len(), 64);
        assert!(fact.more);
    });
}

#[test]
fn rows_stop_at_the_limit() {
    let src = with_big(
        "fn f(b: Big) -> Int {
  case b {
    V0 => 1
  }
}
",
    );
    record_one(&src, |run| {
        let found: Vec<&CaseFact> = run.cases().collect();
        let [fact] = found.as_slice() else {
            panic!("{found:?}");
        };
        assert_eq!(fact.missing.len(), 64);
        assert!(fact.more);
    });
}

#[test]
fn a_subject_with_an_error_has_no_fact() {
    let src = "\
fn f() -> Int {
  case missing {
    Some(_) => 1
  }
}
";
    record_one(src, |run| {
        assert_eq!(run.cases().count(), 0);
        assert!(run.facts.bodies.iter().all(|body| body.errored));
    });
}

#[test]
fn subjectless_cases_and_let_guards_have_no_fact() {
    let src = "\
fn f(o: Option<Int>, b: Bool) -> Int {
  let guard Some(n) = o else {
    0
  }
  case {
    b => n
    _ => 1
  }
}
";
    record_one(src, |run| {
        assert!(run.diags.is_empty(), "{:?}", run.diags);
        assert_eq!(run.cases().count(), 0);
    });
}
