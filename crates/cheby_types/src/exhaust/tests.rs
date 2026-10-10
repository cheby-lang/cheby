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

use std::sync::Arc;

use cheby_diag::{FileId, SourceMap, TextRange, render_all};
use cheby_iface::{
    CtorFields, CtorSig, DefId, DefIndex, DefKind, DepView, Generics, Globals, IfaceItem, IndexVec,
    InterfaceSet, ItemSig, ModuleId, ModuleInfo, ModuleInterface, ModuleTable, PackageId, Ty,
    TyKind, TypeDef, TypeDefKind, Vis, builtin, sym,
};
use cheby_package::{ModulePath, Targets};
use cheby_resolve::LocalId;

use super::check;
use super::engine::{Space, Types};
use crate::thir::{
    Arm, Body, Expr, ExprId, ExprKind, Lit, Local, Param, ParamOrigin, Pat, PatId, PatKind, Stmt,
};

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

    /// Checks `body` and renders the diagnostics against `src`.
    fn check(&self, src: &str, body: &Body) -> String {
        let deps = DepView::new(&self.set, &self.allowed);
        let cx = super::TyCx {
            module: self.app,
            package: PackageId::LOCAL,
            deps: &deps,
            modules: &self.modules,
            globals: &self.globals,
            own: &self.own,
        };
        let diags = check(&cx, body).unwrap();
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
