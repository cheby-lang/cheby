//! Lowering tests for closures, local functions, function values, shims
//! and trampolines (W2-C): the free-variable analysis on hand-built THIR,
//! the rest through `test_support::lower_source`, pinned by goldens under
//! `tests/golden/closure/` (bless with `CHEBY_BLESS=1`).

use std::collections::BTreeSet;

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{Idx, IndexVec, Ty, sym};
use cheby_resolve::LocalId;
use cheby_types::thir::{self, ExprId, ExprKind, Param, ParamOrigin, Pat, PatId, PatKind, Stmt};

use super::{free_locals, trampolines};
use crate::golden_tests::check_golden;
use crate::ir::{Abi, Callee, FnKey, MirModule, Rvalue, Stmt as MirStmt, Terminator, UnitKind};
use crate::repr::Repr;
use crate::test_support::{Lowered, lower_source};
use crate::validate::{Stage, validate};
use crate::{abi, count_and_check};

// ---------------------------------------------------------------------
// Free-variable analysis on hand-built THIR.

/// A THIR body under construction.
#[derive(Default)]
struct Thir {
    exprs: Vec<thir::Expr>,
    locals: usize,
    pats: u32,
}

impl Thir {
    fn span() -> Span {
        Span::new(FileId::new(0), TextRange::empty(0))
    }

    fn expr(&mut self, kind: ExprKind) -> ExprId {
        let id = ExprId::from_usize(self.exprs.len()).unwrap();
        self.exprs.push(thir::Expr {
            kind,
            ty: Ty::STRING,
            span: Self::span(),
        });
        id
    }

    fn local(&mut self) -> LocalId {
        let id = LocalId::from_usize(self.locals).unwrap();
        self.locals = self.locals.checked_add(1).unwrap();
        id
    }

    fn bind(&mut self, l: LocalId) -> Pat {
        self.pats = self.pats.checked_add(1).unwrap();
        Pat {
            id: PatId::from_u32(self.pats),
            kind: PatKind::Bind(l),
            ty: Ty::STRING,
            span: Self::span(),
        }
    }

    fn param(&mut self, l: LocalId) -> Param {
        Param {
            pat: self.bind(l),
            span: Self::span(),
            origin: ParamOrigin::Closure,
        }
    }

    fn use_(&mut self, l: LocalId) -> ExprId {
        self.expr(ExprKind::Local(l))
    }

    fn finish(self, value: ExprId) -> thir::Body {
        let locals = (0..self.locals)
            .map(|_| thir::Local {
                name: sym::T,
                ty: Ty::STRING,
                span: Self::span(),
                synthetic: false,
            })
            .collect();
        thir::Body {
            params: Vec::new(),
            locals: IndexVec::from_raw(locals).unwrap(),
            exprs: IndexVec::from_raw(self.exprs).unwrap(),
            value,
            ret: Ty::NIL,
        }
    }
}

fn closure_parts(body: &thir::Body, e: ExprId) -> (&[Param], ExprId, Option<LocalId>) {
    match &body.expr(e).unwrap().kind {
        ExprKind::Closure { params, body } => (params, *body, None),
        ExprKind::LocalFn {
            local,
            params,
            body,
        } => (params, *body, Some(*local)),
        other => panic!("not a closure: {other:?}"),
    }
}

fn free(body: &thir::Body, e: ExprId) -> Vec<LocalId> {
    let (params, value, own) = closure_parts(body, e);
    free_locals(body, params, value, own).unwrap()
}

/// Closures nested three deep, all reading locals of the one shared
/// `Body.locals` table: each captures what it and the closures inside it
/// use from outside, in first-use order.
///
/// ```text
/// let a = …; let b = …
/// let f = fn(x) {          // c1
///   let g = fn(y) {        // c2
///     let h = fn(z) {      // c3
///       (z, b, a, y, x, b)
///     }
///     h
///   }
///   g
/// }
/// ```
#[test]
#[expect(
    clippy::many_single_char_names,
    reason = "the names of the program in the doc comment"
)]
fn free_locals_of_closures_nested_three_deep() {
    let mut t = Thir::default();
    let (a, b, x, y, z) = (t.local(), t.local(), t.local(), t.local(), t.local());
    let (f, g, h) = (t.local(), t.local(), t.local());
    let uses = [z, b, a, y, x, b].map(|l| t.use_(l));
    let tuple = t.expr(ExprKind::Tuple(Box::new(uses)));
    let p = t.param(z);
    let c3 = t.expr(ExprKind::Closure {
        params: Box::new([p]),
        body: tuple,
    });
    let h_use = t.use_(h);
    let pat = t.bind(h);
    let c2_body = t.expr(ExprKind::Block {
        stmts: Box::new([Stmt::Let { pat, init: c3 }]),
        tail: Some(h_use),
    });
    let p = t.param(y);
    let c2 = t.expr(ExprKind::Closure {
        params: Box::new([p]),
        body: c2_body,
    });
    let g_use = t.use_(g);
    let pat = t.bind(g);
    let c1_body = t.expr(ExprKind::Block {
        stmts: Box::new([Stmt::Let { pat, init: c2 }]),
        tail: Some(g_use),
    });
    let p = t.param(x);
    let c1 = t.expr(ExprKind::Closure {
        params: Box::new([p]),
        body: c1_body,
    });
    let s = t.expr(ExprKind::Lit(thir::Lit::Str("s".into())));
    let s2 = t.expr(ExprKind::Lit(thir::Lit::Str("s".into())));
    let f_use = t.use_(f);
    let (pa, pb, pf) = (t.bind(a), t.bind(b), t.bind(f));
    let outer = t.expr(ExprKind::Block {
        stmts: Box::new([
            Stmt::Let { pat: pa, init: s },
            Stmt::Let { pat: pb, init: s2 },
            Stmt::Let { pat: pf, init: c1 },
        ]),
        tail: Some(f_use),
    });
    let body = t.finish(outer);
    assert_eq!(free(&body, c3), [b, a, y, x]);
    assert_eq!(free(&body, c2), [b, a, x]);
    assert_eq!(free(&body, c1), [b, a]);
}

/// A local function never captures itself: its own name is `self`
/// (`SelfRef`). A closure inside it that names it captures it, with the
/// local function's parameter, in first-use order; locals bound inside
/// (a `let`, a nested local function) are not captures.
///
/// ```text
/// fn spin(i) {
///   fn inner(j) { j }
///   let k = fn() { spin(inner(i)) }
///   k
/// }
/// ```
#[test]
#[expect(
    clippy::many_single_char_names,
    reason = "the names of the program in the doc comment"
)]
fn free_locals_of_local_functions_and_self_references() {
    let mut t = Thir::default();
    let (spin, i, inner, j, k) = (t.local(), t.local(), t.local(), t.local(), t.local());
    let j_use = t.use_(j);
    let p = t.param(j);
    let inner_fn = t.expr(ExprKind::LocalFn {
        local: inner,
        params: Box::new([p]),
        body: j_use,
    });
    let self_ref = t.expr(ExprKind::SelfRef { local: spin });
    let inner_use = t.use_(inner);
    let i_use = t.use_(i);
    let inner_call = t.expr(ExprKind::Call {
        callee: thir::Callee::Value(inner_use),
        args: Box::new([i_use]),
    });
    let spin_call = t.expr(ExprKind::Call {
        callee: thir::Callee::Value(self_ref),
        args: Box::new([inner_call]),
    });
    let closure = t.expr(ExprKind::Closure {
        params: Box::new([]),
        body: spin_call,
    });
    let k_use = t.use_(k);
    let pat = t.bind(k);
    let spin_body = t.expr(ExprKind::Block {
        stmts: Box::new([Stmt::Expr(inner_fn), Stmt::Let { pat, init: closure }]),
        tail: Some(k_use),
    });
    let p = t.param(i);
    let spin_fn = t.expr(ExprKind::LocalFn {
        local: spin,
        params: Box::new([p]),
        body: spin_body,
    });
    let body = t.finish(spin_fn);
    assert_eq!(free(&body, spin_fn), Vec::<LocalId>::new());
    assert_eq!(free(&body, closure), [spin, inner, i]);
    assert_eq!(free(&body, inner_fn), Vec::<LocalId>::new());
}

// ---------------------------------------------------------------------
// Lowering from source.

fn lowered(source: &str) -> Lowered {
    let lowered = lower_source(&[("", source)]);
    assert_eq!(lowered.unsupported(), Vec::<String>::new());
    lowered
}

/// The dumps of the functions of `app` whose keys print as `keys`, for a
/// golden.
fn functions(l: &Lowered, keys: &[&str]) -> String {
    keys.iter()
        .map(|k| l.function("app", k))
        .collect::<Vec<_>>()
        .join("\n")
}

fn app(l: &Lowered) -> &MirModule {
    l.unit("app")
}

/// Every rvalue of the function of `app` with key `key`.
fn rvalues(l: &Lowered, key: FnKey) -> Vec<Rvalue> {
    let f = app(l).function(key).unwrap();
    f.blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter_map(|s| match s {
            MirStmt::Assign { value, .. } | MirStmt::Eval(value) => Some(value.clone()),
            MirStmt::Dup(_) | MirStmt::Drop(_) => None,
        })
        .collect()
}

fn closure_key(l: &Lowered, owner: &str, index: u32) -> FnKey {
    let unit = app(l);
    let owner = unit
        .names
        .iter()
        .find(|n| &*n.item == owner)
        .unwrap_or_else(|| panic!("no def `{owner}`"))
        .def;
    FnKey::Closure { owner, index }
}

const CAPTURES: &str = r"
fn pick(n: Int, _k: Int, _x: Float, _b: Bool, _s: String, _small: U8, _f: F32) -> Int {
  n
}

fn make(n: Int, x: Float, b: Bool, s: String, small: U8, f: F32) -> fn(Int) -> Int {
  fn(k) { pick(n, k, x, b, s, small, f) }
}

fn main() {
  let _m = make
}
";

/// A closure captures values of every representation: unboxed ones are
/// boxed into the closure object (D-453) and unboxed again where the
/// closure's code reads them from `self`; its parameter arrives as a
/// uniform word and is unboxed for the direct call.
#[test]
fn closures_capture_every_representation() {
    let l = lowered(CAPTURES);
    let key = closure_key(&l, "make", 0);
    let make = rvalues(
        &l,
        FnKey::Item(
            app(&l)
                .names
                .iter()
                .find(|n| &*n.item == "make")
                .unwrap()
                .def,
        ),
    );
    let boxed: Vec<Repr> = make
        .iter()
        .filter_map(|r| match r {
            Rvalue::Box { from, .. } => Some(*from),
            _ => None,
        })
        .collect();
    assert_eq!(
        boxed,
        [Repr::Int, Repr::Float, Repr::Bool, Repr::U8, Repr::F32]
    );
    assert!(make.iter().any(|r| matches!(
        r,
        Rvalue::Alloc { layout: crate::LayoutRef::Closure(6), fields } if fields.len() == 7
    )));
    let code = rvalues(&l, key);
    let fields: Vec<u32> = code
        .iter()
        .filter_map(|r| match r {
            Rvalue::Field { field, .. } => Some(*field),
            _ => None,
        })
        .collect();
    assert_eq!(fields, [1, 2, 3, 4, 5, 6]);
    let unboxed: Vec<Repr> = code
        .iter()
        .filter_map(|r| match r {
            Rvalue::Unbox { to, .. } => Some(*to),
            _ => None,
        })
        .collect();
    assert_eq!(
        unboxed,
        [
            Repr::Int,
            Repr::Float,
            Repr::Bool,
            Repr::U8,
            Repr::F32,
            Repr::Int
        ]
    );
    check_golden(
        "closure/captures",
        &functions(&l, &["app::make", "closure app::make#0", "shim app::make"]),
    );
}

/// Closures nested three deep: each level captures from the one around
/// it, and the innermost reads every level's value.
#[test]
fn nested_closures() {
    let l = lowered(
        r#"
import std::string

fn join(a: String, b: String, c: String, d: String) -> String {
  string::add(a, string::add(b, string::add(c, d)))
}

fn nest(a: String) -> fn(String) -> fn(String) -> fn(String) -> String {
  fn(b) { fn(c) { fn(d) { join(a, b, c, d) } } }
}

fn main() {
  let one = nest("a")
  let two = one("b")
  let three = two("c")
  let _all = three("d")
}
"#,
    );
    let text = functions(
        &l,
        &[
            "app::nest",
            "closure app::nest#0",
            "closure app::nest#1",
            "closure app::nest#2",
        ],
    );
    // The outermost closure captures `a`, the next `a` and `b`, the
    // innermost `a`, `b` and `c`.
    assert!(text.contains("alloc closure/1(const code <closure app::nest#0>, _0)"));
    assert!(text.contains("closure/2(const code <closure app::nest#1>"));
    assert!(text.contains("closure/3(const code <closure app::nest#2>"));
    check_golden("closure/nested", &text);
}

/// A local function calls itself through `self` (D-453, D-499) as a tail
/// call; a closure inside it that calls the local function captures that
/// `self`, never the other way round.
#[test]
fn local_functions_call_themselves_through_self() {
    let l = lowered(
        r"
fn spinner(len: Int) -> Int {
  fn spin(i: Int) -> Int {
    let again = fn() { spin(i) }
    let _x = again()
    spin(i)
  }
  spin(len)
}

fn main() {
  let _s = spinner
}
",
    );
    let spin = closure_key(&l, "spinner", 0);
    let again = closure_key(&l, "spinner", 1);
    let f = app(&l).function(spin).unwrap();
    let this = f.params[0];
    // The self call is a tail call through `self`.
    assert!(f.blocks.iter().any(|b| matches!(
        &b.term,
        Terminator::TailCall { callee: Callee::Indirect(c), .. } if *c == this
    )));
    // `again` captures `spin`'s `self`, then `i`.
    assert!(rvalues(&l, spin).iter().any(|r| matches!(
        r,
        Rvalue::Alloc { fields, .. }
            if fields.len() == 3 && fields[1] == crate::ir::Operand::Local(this)
    )));
    // `again` calls the captured `spin` in tail position.
    let g = app(&l).function(again).unwrap();
    assert!(matches!(
        g.block(crate::BlockId::from_u32(0)).unwrap().term,
        Terminator::TailCall {
            callee: Callee::Indirect(_),
            ..
        }
    ));
    check_golden(
        "closure/local_fn_self",
        &functions(
            &l,
            &[
                "app::spinner",
                "closure app::spinner#0",
                "closure app::spinner#1",
            ],
        ),
    );
}

/// `use` and function captures are closures after desugaring (C7,
/// D-155, D-498): the callback is the rest of the block, and a capture's
/// other arguments are evaluated once, when the capture is, and captured.
#[test]
fn use_and_capture_desugarings() {
    let l = lowered(
        r#"
import std::string

fn with(value: String, next: fn(String) -> String) -> String {
  next(value)
}

fn both(a: String, b: String) -> String {
  use x <- with(a)
  use y <- with(b)
  string::add(x, y)
}

fn add3(a: String, b: String, c: String) -> String {
  string::add(a, string::add(b, c))
}

fn prefix() -> String {
  "p"
}

fn captured(s: String) -> String {
  let f = add3(prefix(), _, s)
  f("m")
}

fn main() {
  let _b = both
  let _c = captured
}
"#,
    );
    let text = functions(
        &l,
        &[
            "app::both",
            "closure app::both#0",
            "closure app::both#1",
            "app::captured",
            "closure app::captured#0",
        ],
    );
    check_golden("closure/use_and_captures", &text);
    // The `use` callback is the last argument, and the call is the
    // block's tail call.
    let both = app(&l)
        .functions
        .iter()
        .find(
            |f| matches!(f.key, FnKey::Item(d) if app(&l).name(d).unwrap().item.as_ref() == "both"),
        )
        .unwrap();
    assert!(matches!(
        both.block(crate::BlockId::from_u32(0)).unwrap().term,
        Terminator::TailCall {
            callee: Callee::Direct(_),
            ..
        }
    ));
    // `prefix()` is called once, in `captured`, before the closure is
    // made; the closure captures its result and `s`.
    let captured = text
        .split("fn closure app::captured#0")
        .next()
        .unwrap()
        .to_owned();
    assert!(captured.contains("call app::prefix()"));
    assert!(captured.contains("alloc closure/2(const code <closure app::captured#0>"));
}

/// Calls of function values use the uniform convention: unboxed
/// arguments are boxed, the result is unboxed, and a call in tail
/// position is an indirect tail call (D-453, D-015).
#[test]
fn calls_of_function_values_box_arguments_and_results() {
    let l = lowered(
        r"
fn call_once(f: fn(Int, Bool) -> Float, n: Int, b: Bool) -> Float {
  let x = f(n, b)
  f(n, b)
}

fn main() {
  let _c = call_once
}
",
    );
    let text = l.function("app", "app::call_once");
    check_golden("closure/value_calls", &text);
    assert!(text.contains("box int _1"));
    assert!(text.contains("box bool _2"));
    assert!(text.contains("unbox float"));
    assert!(text.contains("tail_call _0("));
}

/// A top-level function as a value is the static closure
/// `cb$fnval$…` (`Const::FnValue`) whose code is the function's value
/// shim: the shim unboxes the uniform arguments to the declared
/// representations and tail-calls the function through its slot. An
/// external function as a value is the same shim over its wrapper
/// (D-455), and a generic function without bounds has a shim too.
#[test]
fn function_values_and_their_shims() {
    let l = lowered(
        r"
import std::io
import std::string

fn twice(n: Int, _b: Bool) -> Int {
  n
}

fn identity<T>(x: T) -> T {
  x
}

fn main() {
  let _t = twice
  let _p = io::println
  let _l = string::length
  let _i = identity::<Float>
}
",
    );
    let main = l.function("app", "app::main");
    for f in [
        "app::twice",
        "std::io::println",
        "std::string::length",
        "app::identity",
    ] {
        assert!(main.contains(&format!("const fn {f}")), "{main}");
    }
    let shims = functions(
        &l,
        &[
            "shim app::twice",
            "shim app::identity",
            "shim std::io::println",
            "shim std::string::length",
        ],
    );
    check_golden("closure/shims", &shims);
    // Each shim tail-calls its function directly (through the slot).
    for key in [
        "app::twice",
        "app::identity",
        "std::io::println",
        "std::string::length",
    ] {
        assert!(shims.contains(&format!("tail_call {key}(")), "{shims}");
    }
}

/// A function with interface bounds as a value needs its dictionaries:
/// a closure of the shim's code and the dictionaries, which the shim
/// reads from `self`. Until W3-A lowers dictionaries, the dictionary is
/// the stub's E0038.
#[test]
fn function_values_with_dictionaries() {
    let l = lower_source(&[(
        "",
        r#"
fn shown<T: Show>(_x: T) -> String {
  "x"
}

fn main() {
  let _s = shown::<String>
}
"#,
    )]);
    let unsupported = l.unsupported();
    if unsupported.is_empty() {
        let main = l.function("app", "app::main");
        assert!(
            main.contains("alloc closure/1(const code <shim app::shown>"),
            "{main}"
        );
        let shim = l.function("app", "shim app::shown");
        assert!(shim.contains("field _0.1 of closure/1"), "{shim}");
    } else {
        assert_eq!(
            unsupported,
            [
                "calls of functions with interface bounds are not supported yet @ src/main.cheby:7:12"
            ]
        );
    }
}

/// Trampolines of arities 0-4 (D-526): C ABI, the closure and `n`
/// uniform words, one ordinary indirect call, and the counted MIR
/// validates in an entries unit.
#[test]
fn trampolines_for_arities_zero_to_four() {
    let arities: BTreeSet<u32> = (0..=4).collect();
    let functions = trampolines(&arities).unwrap();
    assert_eq!(functions.len(), 5);
    for (n, f) in (0_u32..).zip(&functions) {
        assert_eq!(f.key, FnKey::Trampoline(n));
        assert_eq!(f.abi, Abi::C);
        assert_eq!(f.sig().unwrap(), abi::trampoline_sig(n));
        let closure = f.params[0];
        let call = f
            .block(crate::BlockId::from_u32(0))
            .unwrap()
            .stmts
            .iter()
            .find_map(|s| match s {
                MirStmt::Assign {
                    value: Rvalue::Call { callee, args },
                    ..
                } => Some((callee.clone(), args.len())),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            call,
            (Callee::Indirect(closure), usize::try_from(n).unwrap())
        );
    }
    assert_eq!(trampolines(&BTreeSet::new()).unwrap(), Vec::new());
    let mut unit = MirModule {
        unit: UnitKind::Entries,
        path: "".into(),
        functions,
        callees: Vec::new(),
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: Vec::new(),
    };
    validate(&unit, Stage::Lowered).unwrap();
    count_and_check(&mut unit).unwrap();
    let text = crate::dump(&unit).unwrap();
    check_golden("closure/trampolines", &text);
}
