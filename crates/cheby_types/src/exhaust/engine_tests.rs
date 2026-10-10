//! Engine tests over abstract spaces.

use super::*;

/// A small type language for the tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum T {
    Bool,
    Opt(&'static T),
    Pair(&'static T, &'static T),
    List(&'static T),
    Int,
    Str,
    Opaque,
    /// `Red | Green | Blue`.
    Color,
    /// `Circle { radius: Int } | Dot`.
    Shape,
}

struct Cx;

impl Types for Cx {
    type Ty = T;

    fn space(&self, ty: T) -> Result<Space, Ice> {
        Ok(match ty {
            T::Bool | T::Opt(_) | T::Shape => Space::Variants(2),
            T::Pair(..) => Space::Variants(1),
            T::Color => Space::Variants(3),
            T::List(_) => Space::List,
            T::Int | T::Str | T::Opaque => Space::Infinite,
        })
    }

    fn field_tys(&self, ty: T, ctor: &Ctor) -> Result<Vec<T>, Ice> {
        Ok(match (ty, ctor) {
            (T::Opt(inner), Ctor::Variant(0)) => vec![*inner],
            (T::Pair(a, b), Ctor::Variant(0)) => vec![*a, *b],
            (T::Shape, Ctor::Variant(0)) => vec![T::Int],
            (T::List(elem), Ctor::List(len)) => {
                vec![*elem; usize::try_from(len.arity().unwrap()).unwrap()]
            }
            _ => Vec::new(),
        })
    }

    fn shape(&self, ty: T, tag: u32) -> Result<Shape, Ice> {
        let unit = |s: &str| Shape::Unit(s.to_owned());
        Ok(match (ty, tag) {
            (T::Bool, 0) => unit("False"),
            (T::Bool, _) => unit("True"),
            (T::Opt(_), 0) => Shape::Positional("Some".to_owned()),
            (T::Opt(_), _) => unit("None"),
            (T::Pair(..), _) => Shape::Tuple,
            (T::Color, 0) => unit("Red"),
            (T::Color, 1) => unit("Green"),
            (T::Color, _) => unit("Blue"),
            (T::Shape, 0) => Shape::Named("Circle".to_owned(), vec!["radius".to_owned()]),
            (T::Shape, _) => unit("Dot"),
            _ => return Err(Ice::new("no variants")),
        })
    }
}

const W: Pat = Pat::Wild;
const NOTHING: [&str; 0] = [];
const NO_ARM: [usize; 0] = [];
const INT_PAIR: T = T::Pair(&T::Int, &T::Int);

fn v(tag: u32, fields: Vec<Pat>) -> Pat {
    Pat::Ctor(Ctor::Variant(tag), fields)
}
fn tru() -> Pat {
    v(1, vec![])
}
fn fls() -> Pat {
    v(0, vec![])
}
fn some(p: Pat) -> Pat {
    v(0, vec![p])
}
fn none() -> Pat {
    v(1, vec![])
}
fn pair(a: Pat, b: Pat) -> Pat {
    v(0, vec![a, b])
}
fn red() -> Pat {
    v(0, vec![])
}
fn green() -> Pat {
    v(1, vec![])
}
fn blue() -> Pat {
    v(2, vec![])
}
fn int(n: i128) -> Pat {
    Pat::Ctor(Ctor::Lit(LitKey::Int(n)), vec![])
}
fn string(s: &str) -> Pat {
    Pat::Ctor(Ctor::Lit(LitKey::Str(s.into())), vec![])
}
fn list(items: Vec<Pat>) -> Pat {
    let n = u32::try_from(items.len()).unwrap();
    Pat::Ctor(Ctor::List(ListLen::Fixed(n)), items)
}
fn spread(prefix: Vec<Pat>, suffix: Vec<Pat>) -> Pat {
    let len = ListLen::Var {
        prefix: u32::try_from(prefix.len()).unwrap(),
        suffix: u32::try_from(suffix.len()).unwrap(),
    };
    Pat::Ctor(Ctor::List(len), prefix.into_iter().chain(suffix).collect())
}

/// One arm per entry: its patterns, and whether it has a guard.
fn run_with(tys: &[T], arms: &[(Vec<Pat>, bool)], budget: u64) -> Report<T> {
    let arms: Vec<Arm<'_>> = arms
        .iter()
        .map(|(pats, guarded)| Arm {
            pats,
            guarded: *guarded,
        })
        .collect();
    check_budgeted(&Cx, tys, &arms, budget).unwrap()
}

fn run(tys: &[T], arms: &[(Vec<Pat>, bool)]) -> Report<T> {
    run_with(tys, arms, BUDGET)
}

fn missing(report: &Report<T>) -> Vec<String> {
    report
        .missing
        .iter()
        .map(|row| render_row(&Cx, row).unwrap())
        .collect()
}

fn arms1(pats: Vec<Pat>) -> Vec<(Vec<Pat>, bool)> {
    pats.into_iter().map(|p| (vec![p], false)).collect()
}

#[test]
fn bool_pairs() {
    const PAIR: T = T::Pair(&T::Bool, &T::Bool);
    let r = run(&[PAIR], &arms1(vec![pair(tru(), tru()), pair(fls(), W)]));
    assert_eq!(missing(&r), ["(True, False)"]);
    assert_eq!(r.unreachable, NO_ARM);

    let two = [T::Bool, T::Bool];
    let r = run(
        &two,
        &[(vec![tru(), tru()], false), (vec![fls(), W], false)],
    );
    assert_eq!(missing(&r), ["True, False"]);

    let r = run(
        &two,
        &[
            (vec![tru(), W], false),
            (vec![W, tru()], false),
            (vec![fls(), fls()], false),
            (vec![fls(), tru()], false),
        ],
    );
    assert_eq!(missing(&r), NOTHING);
    assert_eq!(r.unreachable, [3]);
}

#[test]
fn nested_options() {
    const OOB: T = T::Opt(&T::Opt(&T::Bool));
    let r = run(&[OOB], &arms1(vec![some(some(tru())), none()]));
    assert_eq!(missing(&r), ["Some(Some(False))", "Some(None)"]);

    let r = run(&[OOB], &arms1(vec![some(some(W)), some(none()), none()]));
    assert_eq!(missing(&r), NOTHING);

    // Nothing looks inside: one `_`.
    let r = run(&[OOB], &[]);
    assert_eq!(missing(&r), ["_"]);
    let r = run(&[OOB], &arms1(vec![some(W)]));
    assert_eq!(missing(&r), ["None"]);
}

#[test]
fn lists_by_length() {
    const L: T = T::List(&T::Bool);
    let exhaustive = |pats: Vec<Pat>| run(&[L], &arms1(pats)).missing.is_empty();
    assert!(exhaustive(vec![list(vec![]), spread(vec![W], vec![])]));
    assert!(exhaustive(vec![list(vec![]), spread(vec![], vec![W])]));
    assert!(exhaustive(vec![spread(vec![], vec![W]), list(vec![])]));
    assert!(exhaustive(vec![
        spread(vec![W], vec![W]),
        list(vec![W]),
        list(vec![]),
    ]));
    assert!(exhaustive(vec![spread(vec![], vec![])]));

    let r = run(&[L], &arms1(vec![list(vec![]), list(vec![W])]));
    assert_eq!(missing(&r), ["[_, _, ..]"]);
    let r = run(&[L], &arms1(vec![spread(vec![W], vec![])]));
    assert_eq!(missing(&r), ["[]"]);
    let r = run(&[L], &arms1(vec![list(vec![W, W]), list(vec![])]));
    assert_eq!(missing(&r), ["[_]", "[_, _, _, ..]"]);
}

#[test]
fn list_prefix_and_suffix_spreads() {
    const L: T = T::List(&T::Bool);
    let r = run(
        &[L],
        &arms1(vec![spread(vec![tru()], vec![]), list(vec![])]),
    );
    assert_eq!(missing(&r), ["[False, ..]"]);
    let r = run(
        &[L],
        &arms1(vec![spread(vec![], vec![tru()]), list(vec![])]),
    );
    assert_eq!(missing(&r), ["[.., False]"]);
    let r = run(
        &[L],
        &arms1(vec![
            spread(vec![tru()], vec![tru()]),
            spread(vec![fls()], vec![]),
            spread(vec![], vec![fls()]),
            list(vec![]),
        ]),
    );
    // `[True]` is a list of one whose first and last element is `True`,
    // which the first arm does not match (it needs two elements).
    assert_eq!(missing(&r), ["[True]"]);

    // `[first, ..rest]` after `[]` and `[_, ..]` can never match.
    let r = run(
        &[L],
        &arms1(vec![
            list(vec![]),
            spread(vec![W], vec![]),
            spread(vec![W], vec![]),
        ]),
    );
    assert_eq!(r.unreachable, [2]);
    // `[.., last]` is covered by `[_, ..]`.
    let r = run(
        &[L],
        &arms1(vec![
            spread(vec![W], vec![]),
            spread(vec![], vec![W]),
            list(vec![]),
        ]),
    );
    assert_eq!(r.unreachable, [1]);
    assert_eq!(missing(&r), NOTHING);
}

#[test]
fn literals_need_a_catch_all() {
    let r = run(&[T::Int], &arms1(vec![int(200), int(-1)]));
    assert_eq!(missing(&r), ["_"]);
    let r = run(&[T::Int], &arms1(vec![int(200), int(-1), W]));
    assert_eq!(missing(&r), NOTHING);
    let r = run(&[T::Int], &arms1(vec![int(1), int(1), W, int(2)]));
    assert_eq!(r.unreachable, [1, 3]);

    let r = run(&[T::Str], &arms1(vec![string("a")]));
    assert_eq!(missing(&r), ["_"]);

    // fizzbuzz (§5.9.2).
    let r = run(
        &[T::Int, T::Int],
        &[
            (vec![int(0), int(0)], false),
            (vec![int(0), W], false),
            (vec![W, int(0)], false),
            (vec![W, W], false),
        ],
    );
    assert_eq!(missing(&r), NOTHING);
    assert_eq!(r.unreachable, NO_ARM);
    let r = run(
        &[T::Int, T::Int],
        &[(vec![int(0), int(0)], false), (vec![int(0), W], false)],
    );
    assert_eq!(missing(&r), ["_, _"]);
}

#[test]
fn or_patterns() {
    let r = run(
        &[T::Color],
        &arms1(vec![Pat::Or(vec![red(), green()]), blue()]),
    );
    assert_eq!(missing(&r), NOTHING);
    let r = run(&[T::Color], &arms1(vec![Pat::Or(vec![red(), green()])]));
    assert_eq!(missing(&r), ["Blue"]);
    let r = run(
        &[T::Color],
        &arms1(vec![red(), green(), Pat::Or(vec![red(), green()]), W]),
    );
    assert_eq!(r.unreachable, [2]);
    // Alternatives nested in a tuple: `(0, n) | (n, 0)`.
    let r = run(
        &[INT_PAIR],
        &arms1(vec![
            Pat::Or(vec![pair(int(0), W), pair(W, int(0))]),
            pair(W, W),
        ]),
    );
    assert_eq!(missing(&r), NOTHING);
    assert_eq!(r.unreachable, NO_ARM);
    let r = run(&[INT_PAIR], &arms1(vec![pair(Pat::Or(vec![int(0), W]), W)]));
    assert_eq!(missing(&r), NOTHING);
}

#[test]
fn guards_do_not_cover() {
    let r = run(&[T::Bool], &[(vec![tru()], true), (vec![fls()], false)]);
    assert_eq!(missing(&r), ["True"]);
    assert_eq!(r.unreachable, NO_ARM);
    // A guarded arm is itself unreachable after a catch-all.
    let r = run(&[T::Bool], &[(vec![W], false), (vec![tru()], true)]);
    assert_eq!(r.unreachable, [1]);
    // An arm after a guarded one with the same pattern is reachable.
    let r = run(&[T::Bool], &[(vec![W], true), (vec![W], false)]);
    assert_eq!(missing(&r), NOTHING);
    assert_eq!(r.unreachable, NO_ARM);
}

#[test]
fn opaque_only_by_wildcard() {
    let r = run(&[T::Opaque], &arms1(vec![W]));
    assert_eq!(missing(&r), NOTHING);
    let r = run(&[T::Opaque], &arms1(vec![W, W]));
    assert_eq!(r.unreachable, [1]);
    let r = run(&[T::Opt(&T::Opaque)], &arms1(vec![some(W)]));
    assert_eq!(missing(&r), ["None"]);
    let r = run(&[T::Opaque], &[]);
    assert_eq!(missing(&r), ["_"]);
}

#[test]
fn named_fields() {
    let circle = |p: Pat| v(0, vec![p]);
    let r = run(&[T::Shape], &arms1(vec![circle(int(0))]));
    assert_eq!(missing(&r), ["Circle { .. }", "Dot"]);
    let r = run(
        &[T::Opt(&T::Shape)],
        &arms1(vec![some(v(1, vec![])), none()]),
    );
    assert_eq!(missing(&r), ["Some(Circle { .. })"]);
}

#[test]
fn witness_cap() {
    let tys = vec![T::Color; 40];
    let arm = vec![red(); 40];
    let r = run(&tys, &[(arm, false)]);
    assert_eq!(r.missing.len(), WITNESS_LIMIT);
    assert!(r.more);
    let r = run(&[T::Color, T::Color], &[(vec![red(), red()], false)]);
    assert_eq!(
        missing(&r),
        ["Red, Green", "Red, Blue", "Green, _", "Blue, _"]
    );
    assert!(!r.more);
}

#[test]
fn budget_falls_back_to_wildcard() {
    let tys = [T::Bool, T::Bool];
    let arms = [(vec![tru(), tru()], false), (vec![tru(), W], false)];
    let r = run_with(&tys, &arms, 3);
    assert!(r.too_complex);
    assert_eq!(missing(&r), ["_, _"]);
    assert_eq!(r.unreachable, NO_ARM);
}

#[test]
fn a_wildcard_arm_is_exhaustive_whatever_the_budget() {
    // Running out never turns a `case` with an unguarded `_, _` arm into
    // a missing `_`; reachability that runs out reports the remaining arms
    // as reachable (D-508).
    let tys = [T::Bool, T::Bool];
    let arms = [
        (vec![tru(), tru()], false),
        (vec![tru(), W], false),
        (vec![W, W], false),
        (vec![tru(), tru()], false),
    ];
    let r = run_with(&tys, &arms, 3);
    assert!(!r.too_complex);
    assert_eq!(missing(&r), NOTHING);
    assert_eq!(r.unreachable, NO_ARM);
    let r = run(&tys, &arms);
    assert_eq!(r.unreachable, [3]);
}

#[test]
fn large_literal_tables_stay_cheap() {
    // 3000 literal arms and `_`: every arm is reachable, nothing is
    // missing, and the default budget is enough for both passes.
    let mut arms: Vec<(Vec<Pat>, bool)> = (0..3000).map(|n| (vec![int(n)], false)).collect();
    arms.push((vec![W], false));
    let r = run(&[T::Int], &arms);
    assert!(!r.too_complex);
    assert_eq!(r.unreachable, NO_ARM);
    assert_eq!(missing(&r), NOTHING);
    // Without `_`, only a wildcard is missing.
    arms.pop();
    let r = run(&[T::Int], &arms);
    assert!(!r.too_complex);
    assert_eq!(missing(&r), ["_"]);
    // A repeated literal is unreachable however far from the first.
    arms.push((vec![int(5)], false));
    let r = run(&[T::Int], &arms);
    assert_eq!(r.unreachable, [3000]);
}

#[test]
fn irrefutable_patterns() {
    const P: T = T::Pair(&T::Bool, &T::Opaque);
    let r = run(&[P], &arms1(vec![pair(W, W)]));
    assert_eq!(missing(&r), NOTHING);
    let r = run(&[T::List(&T::Int)], &arms1(vec![spread(vec![], vec![])]));
    assert_eq!(missing(&r), NOTHING);
    let r = run(&[P], &arms1(vec![pair(tru(), W)]));
    assert_eq!(missing(&r), ["(False, _)"]);
}

#[test]
fn string_witnesses_escape() {
    let w = Witness {
        ty: T::Str,
        kind: WitnessKind::Ctor(Ctor::Lit(LitKey::Str("a\"{\n".into())), Vec::new()),
    };
    assert_eq!(render(&Cx, &w).unwrap(), r#""a\"\{\n""#);
}

#[test]
fn wide_matrices_stay_cheap() {
    // Row `i` is `True` in column `i` and `_` elsewhere: the only missing
    // value is all `False`.
    let n = 40;
    let tys = vec![T::Bool; n];
    let arms: Vec<(Vec<Pat>, bool)> = (0..n)
        .map(|i| {
            let row = (0..n).map(|j| if i == j { tru() } else { W }).collect();
            (row, false)
        })
        .collect();
    let r = run(&tys, &arms);
    assert!(!r.too_complex);
    assert_eq!(r.unreachable, NO_ARM);
    assert_eq!(missing(&r), [vec!["False"; n].join(", ")]);
}
