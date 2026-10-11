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

#[test]
fn long_list_patterns_need_no_deep_stack() {
    // A list pattern of `n` elements specializes to `n` columns, which the
    // search walks one level each. It keeps those levels on the heap, so a
    // small thread stack is enough (W5-C-1, D-601).
    let n: u32 = 3000;
    let report = std::thread::Builder::new()
        .stack_size(256 << 10)
        .spawn(move || {
            let ints = list((0..n).map(|i| int(i128::from(i))).collect());
            let wilds = list((0..n).map(|_| W).collect());
            run(&[T::List(&T::Int)], &arms1(vec![ints, wilds]))
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(!report.too_complex);
    assert_eq!(report.unreachable, NO_ARM);
    assert_eq!(missing(&report).first().map(String::as_str), Some("[]"));
}

/// The recursive search that the explicit-stack one replaced, kept to
/// check that both spend the same budget and find the same witnesses.
struct RefEngine<'c, C> {
    inner: Engine<'c, C>,
}

impl<C: Types> RefEngine<'_, C> {
    /// The witnesses, at most `want` of them, of values that match `v` and
    /// no row of `matrix`. Empty when `v` is not useful.
    fn useful<'p>(
        &mut self,
        matrix: &[&Row<'p>],
        v: &[&'p Pat],
        tys: &[C::Ty],
        want: usize,
    ) -> Result<Found<C::Ty>, Stop> {
        self.inner.spend(matrix.len())?;
        let (Some((&head, rest)), Some((&ty, _))) = (v.split_first(), tys.split_first()) else {
            if v.len() != tys.len() {
                return Err(
                    Ice::new("exhaustiveness: a row and its types differ in length").into(),
                );
            }
            return Ok(if matrix.is_empty() {
                vec![Vec::new()]
            } else {
                Vec::new()
            });
        };
        let mut out = Vec::new();
        match head {
            Pat::Or(alts) => {
                for alt in alts {
                    let left = want.saturating_sub(out.len());
                    if left == 0 {
                        break;
                    }
                    let row: Row<'p> = std::iter::once(alt).chain(rest.iter().copied()).collect();
                    out.extend(self.useful(matrix, &row, tys, left)?);
                }
            }
            Pat::Ctor(ctor @ Ctor::List(_), _) => {
                let split = self.inner.split(ty, matrix, Some(ctor))?;
                for (c, _) in split.ctors.iter().filter(|(c, _)| covers(ctor, c)) {
                    let left = want.saturating_sub(out.len());
                    if left == 0 {
                        break;
                    }
                    out.extend(self.specialized(matrix, v, tys, c, left)?);
                }
            }
            Pat::Ctor(ctor, _) => {
                // Variants and literals are disjoint, so `ctor` is itself
                // the one split constructor it covers.
                out.extend(self.specialized(matrix, v, tys, ctor, want)?);
            }
            Pat::Wild => {
                let split = self.inner.split(ty, matrix, None)?;
                let groups = Groups::of(matrix);
                let mut default: Option<Found<C::Ty>> = None;
                let incomplete = split.rest || split.ctors.iter().any(|(_, present)| !present);
                if incomplete {
                    // Every specialized matrix holds the default rows, so
                    // when the default matrix leaves nothing, neither does
                    // any constructor (Maranget's incomplete case). When it
                    // leaves something and one witness is enough, the
                    // default's witness will do: the present constructors
                    // are explored only to list more witnesses.
                    let found = self.default_found(&mut default, matrix, v, tys, want)?;
                    if found.is_empty() {
                        return Ok(out);
                    }
                    if want == 1 {
                        let head = match split.ctors.iter().find(|(_, present)| !present) {
                            Some((c, _)) => self.inner.missing_witness(ty, c)?,
                            None => Witness::wild(ty),
                        };
                        out.extend(found.iter().take(1).map(|w| prepend(head.clone(), w)));
                        return Ok(out);
                    }
                }
                for (c, present) in &split.ctors {
                    let left = want.saturating_sub(out.len());
                    if left == 0 {
                        break;
                    }
                    if *present {
                        let rows: Vec<&Row<'p>> = groups
                            .rows_for(c)
                            .into_iter()
                            .filter_map(|i| matrix.get(i).copied())
                            .collect();
                        out.extend(self.specialized(&rows, v, tys, c, left)?);
                        continue;
                    }
                    let missing = self.inner.missing_witness(ty, c)?;
                    let found = self.default_found(&mut default, matrix, v, tys, want)?;
                    out.extend(found.iter().take(left).map(|w| prepend(missing.clone(), w)));
                }
                let left = want.saturating_sub(out.len());
                if split.rest && left > 0 {
                    let found = self.default_found(&mut default, matrix, v, tys, want)?;
                    out.extend(
                        found
                            .iter()
                            .take(left)
                            .map(|w| prepend(Witness::wild(ty), w)),
                    );
                }
            }
        }
        Ok(out)
    }

    /// The witnesses for the columns after the first, against the rows
    /// whose first pattern is a wildcard, computed once per column.
    fn default_found<'d, 'p>(
        &mut self,
        cache: &'d mut Option<Found<C::Ty>>,
        matrix: &[&Row<'p>],
        v: &[&'p Pat],
        tys: &[C::Ty],
        want: usize,
    ) -> Result<&'d Found<C::Ty>, Stop> {
        if cache.is_none() {
            let mut rows = Vec::new();
            for row in matrix {
                default_rows(row, &mut rows);
            }
            let refs: Vec<&Row<'p>> = rows.iter().collect();
            let found = self.useful(
                &refs,
                v.get(1..).unwrap_or(&[]),
                tys.get(1..).unwrap_or(&[]),
                want,
            )?;
            *cache = Some(found);
        }
        cache
            .as_ref()
            .ok_or_else(|| Stop::Ice(Ice::new("exhaustiveness: default not computed")))
    }

    /// Usefulness of `v` specialized to the constructor `c` of its first
    /// column, with the witnesses rebuilt around `c`.
    fn specialized<'p>(
        &mut self,
        matrix: &[&Row<'p>],
        v: &[&'p Pat],
        tys: &[C::Ty],
        c: &Ctor,
        want: usize,
    ) -> Result<Found<C::Ty>, Stop> {
        let (Some(&ty), Some(rest_tys)) = (tys.first(), tys.get(1..)) else {
            return Err(Ice::new("exhaustiveness: specializing an empty row").into());
        };
        let field_tys = self.inner.cx.field_tys(ty, c)?;
        let arity = field_tys.len();
        let mut rows = Vec::new();
        for row in matrix {
            specialize(row, c, arity, &mut rows)?;
        }
        let mut vs = Vec::new();
        specialize(v, c, arity, &mut vs)?;
        let [v] = vs.as_slice() else {
            return Err(Ice::new("exhaustiveness: the row does not specialize to one row").into());
        };
        let tys: Vec<C::Ty> = field_tys.iter().chain(rest_tys).copied().collect();
        let refs: Vec<&Row<'p>> = rows.iter().collect();
        let found = self.useful(&refs, v, &tys, want)?;
        found
            .into_iter()
            .map(|mut w| {
                if w.len() < arity {
                    return Err(Ice::new("exhaustiveness: a witness is too short").into());
                }
                let rest = w.split_off(arity);
                let head = Witness {
                    ty,
                    kind: WitnessKind::Ctor(c.clone(), w),
                };
                Ok(std::iter::once(head).chain(rest).collect())
            })
            .collect()
    }
}

/// A small deterministic generator (xorshift), so the comparison needs no
/// extra crate and every run sees the same cases.
struct Gen(u64);

impl Gen {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next().checked_rem(n).unwrap()
    }

    fn ty(&mut self) -> T {
        const TYS: [T; 8] = [
            T::Bool,
            T::Opt(&T::Bool),
            T::Pair(&T::Bool, &T::Color),
            T::List(&T::Bool),
            T::Int,
            T::Color,
            T::Opt(&T::Opt(&T::Color)),
            T::List(&T::Opt(&T::Int)),
        ];
        TYS[usize::try_from(self.below(8)).unwrap()]
    }

    fn pat(&mut self, ty: T, depth: u32) -> Pat {
        let roll = self.below(10);
        if depth == 0 || roll < 3 {
            return W;
        }
        if roll == 3 {
            let alts = (0..self.below(2).saturating_add(2))
                .map(|_| self.pat(ty, depth.saturating_sub(1)))
                .collect();
            return Pat::Or(alts);
        }
        match ty {
            T::Bool => v(u32::try_from(self.below(2)).unwrap(), vec![]),
            T::Color => v(u32::try_from(self.below(3)).unwrap(), vec![]),
            T::Opt(inner) => {
                if self.below(2) == 0 {
                    some(self.pat(*inner, depth.saturating_sub(1)))
                } else {
                    none()
                }
            }
            T::Pair(a, b) => pair(
                self.pat(*a, depth.saturating_sub(1)),
                self.pat(*b, depth.saturating_sub(1)),
            ),
            T::List(elem) => {
                let len = self.below(4);
                let items: Vec<Pat> = (0..len)
                    .map(|_| self.pat(*elem, depth.saturating_sub(1)))
                    .collect();
                if self.below(2) == 0 {
                    list(items)
                } else {
                    let cut = usize::try_from(self.below(len.saturating_add(1))).unwrap();
                    let mut items = items;
                    let suffix = items.split_off(cut);
                    spread(items, suffix)
                }
            }
            T::Int => int(i128::from(self.below(4))),
            _ => W,
        }
    }
}

#[derive(PartialEq, Debug)]
enum Outcome {
    Found(Found<T>),
    Budget,
}

fn outcome(result: Result<Found<T>, Stop>) -> Outcome {
    match result {
        Ok(found) => Outcome::Found(found),
        Err(Stop::Budget) => Outcome::Budget,
        Err(Stop::Ice(ice)) => panic!("{ice:?}"),
    }
}

#[test]
fn the_explicit_stack_matches_the_recursion() {
    let mut g = Gen(0x9e37_79b9_7f4a_7c15);
    let (mut budget_stops, mut useful, mut useless) = (0_u32, 0_u32, 0_u32);
    for case in 0..3000 {
        let width = g.below(3).saturating_add(1);
        let tys: Vec<T> = (0..width).map(|_| g.ty()).collect();
        let rows: Vec<Vec<Pat>> = (0..g.below(7))
            .map(|_| tys.iter().map(|&ty| g.pat(ty, 3)).collect())
            .collect();
        let probe: Vec<Pat> = if g.below(3) == 0 {
            tys.iter().map(|_| W).collect()
        } else {
            tys.iter().map(|&ty| g.pat(ty, 3)).collect()
        };
        let want = [1, 2, 65][usize::try_from(g.below(3)).unwrap()];
        let budget = if g.below(2) == 0 {
            g.below(200).saturating_add(5)
        } else {
            BUDGET
        };
        let rows: Vec<Row<'_>> = rows.iter().map(|row| row.iter().collect()).collect();
        let refs: Vec<&Row<'_>> = rows.iter().collect();
        let probe: Row<'_> = probe.iter().collect();
        let mut old = RefEngine {
            inner: Engine { cx: &Cx, budget },
        };
        let mut new = Engine { cx: &Cx, budget };
        let expected = outcome(old.useful(&refs, &probe, &tys, want));
        let actual = outcome(new.useful(&refs, &probe, &tys, want));
        assert_eq!(actual, expected, "case {case}: {tys:?} {rows:?} {probe:?}");
        assert_eq!(new.budget, old.inner.budget, "case {case}: budget");
        match actual {
            Outcome::Budget => budget_stops = budget_stops.saturating_add(1),
            Outcome::Found(found) if found.is_empty() => useless = useless.saturating_add(1),
            Outcome::Found(_) => useful = useful.saturating_add(1),
        }
    }
    // The cases exercise every outcome.
    assert!(
        budget_stops > 100 && useful > 100 && useless > 100,
        "{budget_stops} {useful} {useless}"
    );
}
