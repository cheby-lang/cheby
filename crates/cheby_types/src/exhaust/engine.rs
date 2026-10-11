//! Usefulness over an abstract pattern matrix, with witnesses (C6).
//!
//! This is the algorithm of Maranget, "Warnings for pattern matching"
//! (2007), with constructor splitting for lists as in rustc. It knows
//! nothing about THIR: a [`Types`] implementation tells it, for each
//! column type, which constructors exist and what their fields are.
//!
//! - A vector of patterns is *useful* against a matrix when some value
//!   matches it and no row of the matrix. An arm is reachable when its
//!   patterns are useful against the earlier arms without guards (§6.9),
//!   and a `case` is exhaustive when a row of wildcards is not useful
//!   against all arms without guards. The values that make the wildcard
//!   row useful are the witnesses of a non-exhaustive `case`.
//! - Constructor spaces ([`Space`]): a finite set of variants (an ADT, or a
//!   tuple or record as a single variant), lists split by length (D-158),
//!   and infinite spaces (literals of `Int`, the sized integers and
//!   `String`, and opaque types), which only a wildcard covers.
//! - The work is bounded. A `case` collects at most [`WITNESS_LIMIT`]
//!   witnesses, and each of the two passes (reachability of the arms,
//!   then the missing values) spends at most a fixed budget of steps.
//!   When reachability runs out, the remaining arms are reported as
//!   reachable. When the missing values run out, the `case` is reported
//!   as missing `_` (D-508). A `case` with an
//!   unguarded arm of wildcards is exhaustive without any search, so the
//!   fallback never reports a `case` that has one.
//! - Variants and literals are disjoint, so the rows of a matrix are
//!   grouped by the constructor their first pattern starts with
//!   ([`Groups`]), and specializing to one constructor looks only at its
//!   group and the rows that start with a wildcard. A large table of
//!   literal or variant arms is therefore checked in about linear time.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::rc::Rc;

use cheby_iface::Ice;

/// How many witnesses a check collects at most.
pub(crate) const WITNESS_LIMIT: usize = 64;

/// The steps one check may take: one per call of the recursion plus one
/// per matrix row it looks at.
const BUDGET: u64 = 2_000_000;

/// The value of a literal pattern.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) enum LitKey {
    Int(i128),
    Str(Box<str>),
}

/// The lengths a list constructor matches.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum ListLen {
    /// Exactly this many elements, with one field per element.
    Fixed(u32),
    /// At least `prefix + suffix` elements, with one field for each of the
    /// first `prefix` and the last `suffix` elements (one spread, D-158).
    Var { prefix: u32, suffix: u32 },
}

impl ListLen {
    /// The number of fields, which is also the minimum length.
    fn arity(self) -> Option<u32> {
        match self {
            Self::Fixed(n) => Some(n),
            Self::Var { prefix, suffix } => prefix.checked_add(suffix),
        }
    }
}

/// A constructor.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Ctor {
    /// The variant with this tag of a [`Space::Variants`] type. A tuple
    /// is the single variant 0.
    Variant(u32),
    /// A literal of a [`Space::Infinite`] type. It has no fields.
    Lit(LitKey),
    /// A list pattern of a [`Space::List`] type.
    List(ListLen),
}

/// An abstract pattern.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Pat {
    /// `_`, a binding, or anything else that matches every value.
    Wild,
    /// A constructor with one pattern per field ([`Types::field_tys`]).
    Ctor(Ctor, Vec<Pat>),
    /// Alternatives (§6.7).
    Or(Vec<Pat>),
}

/// The constructors of a type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Space {
    /// Variants with tags `0..n`.
    Variants(u32),
    /// `List<T>`: one constructor per length.
    List,
    /// Values that only a wildcard covers: literal types and opaque types.
    Infinite,
}

/// How a variant is written, for witnesses.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Shape {
    /// `(a, b)`.
    Tuple,
    /// `None`.
    Unit(String),
    /// `Some(_)`.
    Positional(String),
    /// `Circle { radius: _ }`, with the field names in declaration order.
    Named(String, Vec<String>),
}

/// What the engine needs to know about types.
pub(crate) trait Types {
    type Ty: Copy + std::fmt::Debug;

    /// The constructors of `ty`.
    fn space(&self, ty: Self::Ty) -> Result<Space, Ice>;

    /// The types of the fields of `ctor` at `ty`: one per field of a
    /// variant, `n` copies of the element type for a list constructor of
    /// arity `n`, none for a literal.
    fn field_tys(&self, ty: Self::Ty, ctor: &Ctor) -> Result<Vec<Self::Ty>, Ice>;

    /// How the variant `tag` of `ty` is written.
    fn shape(&self, ty: Self::Ty, tag: u32) -> Result<Shape, Ice>;
}

/// A value that no arm matches, as a pattern.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Witness<T> {
    pub ty: T,
    pub kind: WitnessKind<T>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum WitnessKind<T> {
    Wild,
    Ctor(Ctor, Vec<Witness<T>>),
}

impl<T> Witness<T> {
    const fn wild(ty: T) -> Self {
        Self {
            ty,
            kind: WitnessKind::Wild,
        }
    }
}

/// One arm of a match: one pattern per column.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Arm<'p> {
    pub pats: &'p [Pat],
    pub guarded: bool,
}

/// The result of [`check`].
#[derive(Clone, Debug)]
pub(crate) struct Report<T> {
    /// The indexes of the arms that can never match.
    pub unreachable: Vec<usize>,
    /// The values no unguarded arm covers, one pattern per column, at most
    /// [`WITNESS_LIMIT`] of them. Empty when the arms are exhaustive.
    pub missing: Vec<Vec<Witness<T>>>,
    /// Whether there are more missing values than `missing` holds.
    pub more: bool,
    /// Whether the check ran out of budget (and fell back to `_`).
    pub too_complex: bool,
}

/// Checks the arms of a match over columns of types `tys`, in order:
/// which arms are unreachable, and which values no arm covers. A guarded
/// arm covers nothing, but is itself checked for reachability (§6.9).
pub(crate) fn check<C: Types>(
    cx: &C,
    tys: &[C::Ty],
    arms: &[Arm<'_>],
) -> Result<Report<C::Ty>, Ice> {
    check_budgeted(cx, tys, arms, BUDGET)
}

/// [`check`] with a budget of `budget` steps for each pass.
fn check_budgeted<C: Types>(
    cx: &C,
    tys: &[C::Ty],
    arms: &[Arm<'_>],
    budget: u64,
) -> Result<Report<C::Ty>, Ice> {
    let mut engine = Engine { cx, budget };
    let mut matrix: Vec<Row<'_>> = Vec::new();
    // The rows of `matrix` by the constructor of their first pattern.
    let mut groups = Groups::default();
    let mut unreachable = Vec::new();
    let mut reach_exhausted = false;
    for (index, arm) in arms.iter().enumerate() {
        if arm.pats.len() != tys.len() {
            return Err(Ice::new(
                "exhaustiveness: an arm with the wrong number of patterns",
            ));
        }
        let row: Row<'_> = arm.pats.iter().collect();
        if !reach_exhausted {
            // Only the rows whose first pattern can match what this arm's
            // first pattern matches can make it unreachable.
            let rows = match row.first() {
                Some(&head) => groups.compatible(&matrix, head),
                None => matrix.iter().collect(),
            };
            match engine.useful(&rows, &row, tys, 1) {
                Ok(found) if found.is_empty() => unreachable.push(index),
                Ok(_) => {}
                Err(Stop::Budget) => reach_exhausted = true,
                Err(Stop::Ice(ice)) => return Err(ice),
            }
        }
        if !arm.guarded {
            if let Some(&head) = row.first() {
                groups.add(matrix.len(), head);
            }
            matrix.push(row);
        }
    }
    // An unguarded arm of wildcards matches every value.
    if matrix
        .iter()
        .any(|row| row.iter().all(|pat| matches_all(pat)))
    {
        return Ok(Report {
            unreachable,
            missing: Vec::new(),
            more: false,
            too_complex: false,
        });
    }
    let mut engine = Engine { cx, budget };
    let mut too_complex = false;
    let rows: Vec<&Row<'_>> = matrix.iter().collect();
    let wild_row: Row<'_> = tys.iter().map(|_| &WILD).collect();
    let (missing, more) = match engine.useful(&rows, &wild_row, tys, WITNESS_LIMIT + 1) {
        Ok(mut found) => {
            let more = found.len() > WITNESS_LIMIT;
            found.truncate(WITNESS_LIMIT);
            (found, more)
        }
        Err(Stop::Budget) => {
            too_complex = true;
            (
                vec![tys.iter().map(|&ty| Witness::wild(ty)).collect()],
                false,
            )
        }
        Err(Stop::Ice(ice)) => return Err(ice),
    };
    Ok(Report {
        unreachable,
        missing,
        more,
        too_complex,
    })
}

/// A witness as a user writes the pattern: `None`, `Some(_)`,
/// `[_, _, ..]`, `(True, _)`, `Circle { .. }`.
pub(crate) fn render<C: Types>(cx: &C, witness: &Witness<C::Ty>) -> Result<String, Ice> {
    let mut out = String::new();
    write_witness(cx, witness, &mut out)?;
    Ok(out)
}

/// Witnesses of several columns, separated by commas as the patterns of
/// a `case` arm with several subjects are.
pub(crate) fn render_row<C: Types>(cx: &C, row: &[Witness<C::Ty>]) -> Result<String, Ice> {
    let mut out = String::new();
    for (n, witness) in row.iter().enumerate() {
        if n > 0 {
            out.push_str(", ");
        }
        write_witness(cx, witness, &mut out)?;
    }
    Ok(out)
}

fn write_witness<C: Types>(cx: &C, witness: &Witness<C::Ty>, out: &mut String) -> Result<(), Ice> {
    let WitnessKind::Ctor(ctor, fields) = &witness.kind else {
        out.push('_');
        return Ok(());
    };
    match ctor {
        Ctor::Lit(LitKey::Int(n)) => {
            let _ = write!(out, "{n}");
        }
        Ctor::Lit(LitKey::Str(s)) => write_string(s, out),
        Ctor::Variant(tag) => match cx.shape(witness.ty, *tag)? {
            Shape::Tuple => {
                out.push('(');
                write_list(cx, fields, out)?;
                out.push(')');
            }
            Shape::Unit(name) => out.push_str(&name),
            Shape::Positional(name) => {
                out.push_str(&name);
                out.push('(');
                write_list(cx, fields, out)?;
                out.push(')');
            }
            Shape::Named(name, names) => {
                out.push_str(&name);
                out.push_str(" {");
                let mut omitted = false;
                let mut first = true;
                for (field, name) in fields.iter().zip(&names) {
                    if matches!(field.kind, WitnessKind::Wild) {
                        omitted = true;
                        continue;
                    }
                    out.push_str(if first { " " } else { ", " });
                    first = false;
                    out.push_str(name);
                    out.push_str(": ");
                    write_witness(cx, field, out)?;
                }
                if omitted || fields.len() != names.len() {
                    out.push_str(if first { " .." } else { ", .." });
                }
                out.push_str(" }");
            }
        },
        Ctor::List(ListLen::Fixed(_)) => {
            out.push('[');
            write_list(cx, fields, out)?;
            out.push(']');
        }
        Ctor::List(ListLen::Var { prefix, .. }) => {
            let at = usize::try_from(*prefix)
                .unwrap_or(usize::MAX)
                .min(fields.len());
            let (front, back) = fields.split_at(at);
            let mut items: Vec<String> = Vec::new();
            for field in front {
                items.push(render(cx, field)?);
            }
            if back.iter().all(|w| matches!(w.kind, WitnessKind::Wild)) {
                // `[_, .., _]` is `[_, _, ..]`: say it the simple way.
                items.extend(back.iter().map(|_| "_".to_owned()));
                items.push("..".to_owned());
            } else {
                items.push("..".to_owned());
                for field in back {
                    items.push(render(cx, field)?);
                }
            }
            out.push('[');
            out.push_str(&items.join(", "));
            out.push(']');
        }
    }
    Ok(())
}

fn write_list<C: Types>(cx: &C, fields: &[Witness<C::Ty>], out: &mut String) -> Result<(), Ice> {
    for (n, field) in fields.iter().enumerate() {
        if n > 0 {
            out.push_str(", ");
        }
        write_witness(cx, field, out)?;
    }
    Ok(())
}

/// A string literal with the escapes of §2.5 (D-114).
fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '{' => out.push_str("\\{"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The pattern every missing field gets.
static WILD: Pat = Pat::Wild;

/// A row of the matrix: one pattern per remaining column.
type Row<'p> = Vec<&'p Pat>;

/// Witnesses of usefulness, one pattern per column.
type Found<T> = Vec<Vec<Witness<T>>>;

enum Stop {
    Budget,
    Ice(Ice),
}

impl From<Ice> for Stop {
    fn from(ice: Ice) -> Self {
        Self::Ice(ice)
    }
}

/// The constructors a column is split into.
struct Split {
    /// Disjoint constructors in canonical order, each with whether a row
    /// of the matrix has a constructor that covers it.
    ctors: Vec<(Ctor, bool)>,
    /// Whether some values match none of `ctors` (an infinite space, or a
    /// column no row looks into): those are reported as `_`.
    rest: bool,
}

struct Engine<'c, C> {
    cx: &'c C,
    budget: u64,
}

/// A matrix that calls of the search own, shared by the calls that look
/// at the same rows.
type Matrix<'p> = Rc<Vec<Row<'p>>>;

/// One call of the search: the witnesses, at most `want` of them, of
/// values that match `v` and no row of `matrix`.
struct Call<'p, T> {
    matrix: Matrix<'p>,
    v: Row<'p>,
    tys: Vec<T>,
    want: usize,
}

/// A specialized call, with the frame that rebuilds its witnesses.
type Specialized<'p, T> = (Frame<'p, T>, Call<'p, T>);

/// What a call does next.
enum Flow<'p, T> {
    /// It finished with these witnesses.
    Done(Found<T>),
    /// It waits, on the stack, for the witnesses of this nested call.
    Descend(Call<'p, T>),
}

/// A call that waits for the witnesses of a nested call.
enum Frame<'p, T> {
    /// Trying the alternatives of the first pattern from `next` on.
    Or {
        call: Call<'p, T>,
        alts: &'p [Pat],
        next: usize,
        out: Found<T>,
    },
    /// Trying the split list constructors that the first pattern covers,
    /// from `next` on.
    List {
        call: Call<'p, T>,
        ctors: Vec<Ctor>,
        next: usize,
        out: Found<T>,
    },
    /// Rebuilding the nested call's witnesses around the constructor
    /// `ctor` of `ty`, which has `arity` fields.
    Rebuild { ty: T, ctor: Ctor, arity: usize },
    /// A call whose first pattern is a wildcard.
    Wild(Box<WildFrame<'p, T>>),
}

/// The state of a call whose first pattern is a wildcard.
struct WildFrame<'p, T> {
    call: Call<'p, T>,
    ty: T,
    split: Split,
    groups: Groups<'p>,
    /// The witnesses of the default matrix, once computed.
    default: Option<Found<T>>,
    /// The next constructor of `split` to try.
    next: usize,
    out: Found<T>,
    wait: Wait<T>,
}

/// What a [`WildFrame`] waits for.
enum Wait<T> {
    /// The default matrix, computed first because the split is
    /// incomplete.
    FirstDefault,
    /// The specialization to the constructor `split.ctors[next]`.
    Ctor,
    /// The default matrix, for the missing constructor
    /// `split.ctors[next]`, whose witness starts with this.
    MissingDefault(Witness<T>),
    /// The default matrix, for the values that no constructor covers.
    RestDefault,
}

/// Tries the alternative `next` of the first pattern, until `want`
/// witnesses are found.
fn or_step<'p, T: Copy>(
    call: Call<'p, T>,
    alts: &'p [Pat],
    next: usize,
    out: Found<T>,
    stack: &mut Vec<Frame<'p, T>>,
) -> Flow<'p, T> {
    let left = call.want.saturating_sub(out.len());
    let Some(alt) = alts.get(next) else {
        return Flow::Done(out);
    };
    if left == 0 {
        return Flow::Done(out);
    }
    let v: Row<'p> = std::iter::once(alt)
        .chain(call.v.iter().skip(1).copied())
        .collect();
    let nested = Call {
        matrix: Rc::clone(&call.matrix),
        v,
        tys: call.tys.clone(),
        want: left,
    };
    stack.push(Frame::Or {
        call,
        alts,
        next: next.saturating_add(1),
        out,
    });
    Flow::Descend(nested)
}

/// Pushes `frame`, waiting as `wait` for the witnesses of the columns
/// after the first, against the rows whose first pattern is a wildcard,
/// and returns that call.
fn wait_default<'p, T: Copy>(
    mut frame: Box<WildFrame<'p, T>>,
    wait: Wait<T>,
    stack: &mut Vec<Frame<'p, T>>,
) -> Flow<'p, T> {
    let mut rows = Vec::new();
    for row in frame.call.matrix.iter() {
        default_rows(row, &mut rows);
    }
    let nested = Call {
        matrix: Rc::new(rows),
        v: frame.call.v.get(1..).unwrap_or(&[]).to_vec(),
        tys: frame.call.tys.get(1..).unwrap_or(&[]).to_vec(),
        want: frame.call.want,
    };
    frame.wait = wait;
    stack.push(Frame::Wild(frame));
    Flow::Descend(nested)
}

/// The witnesses of a specialized call, each rebuilt with the
/// constructor `ctor` of `ty` around its first `arity` patterns.
fn rebuild<T>(ty: T, ctor: &Ctor, arity: usize, found: Found<T>) -> Result<Found<T>, Stop>
where
    T: Copy,
{
    found
        .into_iter()
        .map(|mut w| {
            if w.len() < arity {
                return Err(Ice::new("exhaustiveness: a witness is too short").into());
            }
            let rest = w.split_off(arity);
            let head = Witness {
                ty,
                kind: WitnessKind::Ctor(ctor.clone(), w),
            };
            Ok(std::iter::once(head).chain(rest).collect())
        })
        .collect()
}

impl<C: Types> Engine<'_, C> {
    fn spend(&mut self, rows: usize) -> Result<(), Stop> {
        let cost = u64::try_from(rows).unwrap_or(u64::MAX).saturating_add(1);
        self.budget = self.budget.checked_sub(cost).ok_or(Stop::Budget)?;
        Ok(())
    }

    /// The witnesses, at most `want` of them, of values that match `v` and
    /// no row of `matrix`. Empty when `v` is not useful.
    fn useful<'p>(
        &mut self,
        matrix: &[&Row<'p>],
        v: &[&'p Pat],
        tys: &[C::Ty],
        want: usize,
    ) -> Result<Found<C::Ty>, Stop> {
        let call = Call {
            matrix: Rc::new(matrix.iter().map(|row| (*row).clone()).collect()),
            v: v.to_vec(),
            tys: tys.to_vec(),
            want,
        };
        self.search(call)
    }

    /// Runs the recursion of Maranget's algorithm for `call` with an
    /// explicit stack of the calls that wait for a nested one. A list
    /// pattern adds a level per element, so a recursion on the thread's
    /// own stack overflowed a 2 MiB worker on a few thousand elements. The
    /// calls run in the same order as a recursion would, so the budget and
    /// the witnesses are the same (D-508).
    fn search<'p>(&mut self, call: Call<'p, C::Ty>) -> Result<Found<C::Ty>, Stop> {
        let mut stack: Vec<Frame<'p, C::Ty>> = Vec::new();
        let mut flow = self.enter(call, &mut stack)?;
        loop {
            flow = match flow {
                Flow::Descend(call) => self.enter(call, &mut stack)?,
                Flow::Done(found) => match stack.pop() {
                    None => return Ok(found),
                    Some(frame) => self.resume(frame, found, &mut stack)?,
                },
            };
        }
    }

    /// Starts `call`: finishes it at once, or pushes what it waits for
    /// and returns the nested call to run first.
    fn enter<'p>(
        &mut self,
        call: Call<'p, C::Ty>,
        stack: &mut Vec<Frame<'p, C::Ty>>,
    ) -> Result<Flow<'p, C::Ty>, Stop> {
        self.spend(call.matrix.len())?;
        let (Some(&head), Some(&ty)) = (call.v.first(), call.tys.first()) else {
            if call.v.len() != call.tys.len() {
                return Err(
                    Ice::new("exhaustiveness: a row and its types differ in length").into(),
                );
            }
            return Ok(Flow::Done(if call.matrix.is_empty() {
                vec![Vec::new()]
            } else {
                Vec::new()
            }));
        };
        match head {
            Pat::Or(alts) => Ok(or_step(call, alts, 0, Vec::new(), stack)),
            Pat::Ctor(ctor @ Ctor::List(_), _) => {
                let split = {
                    let rows: Vec<&Row<'p>> = call.matrix.iter().collect();
                    self.split(ty, &rows, Some(ctor))?
                };
                let ctors = split
                    .ctors
                    .into_iter()
                    .map(|(c, _)| c)
                    .filter(|c| covers(ctor, c))
                    .collect();
                self.list_step(call, ctors, 0, Vec::new(), stack)
            }
            Pat::Ctor(ctor, _) => {
                // Variants and literals are disjoint, so `ctor` is itself
                // the one split constructor it covers.
                let (rebuild, nested) = {
                    let rows: Vec<&Row<'p>> = call.matrix.iter().collect();
                    self.specialize_call(&rows, &call.v, &call.tys, ctor, call.want)?
                };
                stack.push(rebuild);
                Ok(Flow::Descend(nested))
            }
            Pat::Wild => self.wild_start(call, ty, stack),
        }
    }

    /// Continues the frame that waited for `found`, the witnesses of its
    /// nested call.
    fn resume<'p>(
        &mut self,
        frame: Frame<'p, C::Ty>,
        found: Found<C::Ty>,
        stack: &mut Vec<Frame<'p, C::Ty>>,
    ) -> Result<Flow<'p, C::Ty>, Stop> {
        match frame {
            Frame::Or {
                call,
                alts,
                next,
                mut out,
            } => {
                out.extend(found);
                Ok(or_step(call, alts, next, out, stack))
            }
            Frame::List {
                call,
                ctors,
                next,
                mut out,
            } => {
                out.extend(found);
                self.list_step(call, ctors, next, out, stack)
            }
            Frame::Rebuild { ty, ctor, arity } => Ok(Flow::Done(rebuild(ty, &ctor, arity, found)?)),
            Frame::Wild(mut frame) => match std::mem::replace(&mut frame.wait, Wait::Ctor) {
                Wait::FirstDefault => {
                    if found.is_empty() {
                        // Every specialized matrix holds the default rows,
                        // so when the default matrix leaves nothing,
                        // neither does any constructor (Maranget's
                        // incomplete case).
                        return Ok(Flow::Done(frame.out));
                    }
                    if frame.call.want == 1 {
                        // One witness is enough, and the default's will do:
                        // the present constructors are explored only to
                        // list more witnesses.
                        let head = match frame.split.ctors.iter().find(|(_, present)| !present) {
                            Some((c, _)) => self.missing_witness(frame.ty, c)?,
                            None => Witness::wild(frame.ty),
                        };
                        frame
                            .out
                            .extend(found.iter().take(1).map(|w| prepend(head.clone(), w)));
                        return Ok(Flow::Done(frame.out));
                    }
                    frame.default = Some(found);
                    self.wild_step(frame, stack)
                }
                Wait::Ctor => {
                    frame.out.extend(found);
                    frame.next = frame.next.saturating_add(1);
                    self.wild_step(frame, stack)
                }
                Wait::MissingDefault(missing) => {
                    let left = frame.call.want.saturating_sub(frame.out.len());
                    frame
                        .out
                        .extend(found.iter().take(left).map(|w| prepend(missing.clone(), w)));
                    frame.default = Some(found);
                    frame.next = frame.next.saturating_add(1);
                    self.wild_step(frame, stack)
                }
                Wait::RestDefault => {
                    let left = frame.call.want.saturating_sub(frame.out.len());
                    let ty = frame.ty;
                    frame.out.extend(
                        found
                            .iter()
                            .take(left)
                            .map(|w| prepend(Witness::wild(ty), w)),
                    );
                    Ok(Flow::Done(frame.out))
                }
            },
        }
    }

    /// Tries the next split list constructor that the first pattern
    /// covers, until `want` witnesses are found.
    fn list_step<'p>(
        &mut self,
        call: Call<'p, C::Ty>,
        ctors: Vec<Ctor>,
        next: usize,
        out: Found<C::Ty>,
        stack: &mut Vec<Frame<'p, C::Ty>>,
    ) -> Result<Flow<'p, C::Ty>, Stop> {
        let left = call.want.saturating_sub(out.len());
        let Some(c) = ctors.get(next) else {
            return Ok(Flow::Done(out));
        };
        if left == 0 {
            return Ok(Flow::Done(out));
        }
        let (rebuild, nested) = {
            let rows: Vec<&Row<'p>> = call.matrix.iter().collect();
            self.specialize_call(&rows, &call.v, &call.tys, c, left)?
        };
        stack.push(Frame::List {
            call,
            ctors,
            next: next.saturating_add(1),
            out,
        });
        stack.push(rebuild);
        Ok(Flow::Descend(nested))
    }

    /// Starts a call whose first pattern is a wildcard: splits the column,
    /// and computes the default matrix first when the split is incomplete.
    fn wild_start<'p>(
        &mut self,
        call: Call<'p, C::Ty>,
        ty: C::Ty,
        stack: &mut Vec<Frame<'p, C::Ty>>,
    ) -> Result<Flow<'p, C::Ty>, Stop> {
        let (split, groups) = {
            let rows: Vec<&Row<'p>> = call.matrix.iter().collect();
            (self.split(ty, &rows, None)?, Groups::of(&rows))
        };
        let incomplete = split.rest || split.ctors.iter().any(|(_, present)| !present);
        let frame = Box::new(WildFrame {
            call,
            ty,
            split,
            groups,
            default: None,
            next: 0,
            out: Vec::new(),
            wait: Wait::Ctor,
        });
        if incomplete {
            return Ok(wait_default(frame, Wait::FirstDefault, stack));
        }
        self.wild_step(frame, stack)
    }

    /// Tries the split constructors of a wildcard column from `next` on,
    /// then the values none of them covers.
    fn wild_step<'p>(
        &mut self,
        mut frame: Box<WildFrame<'p, C::Ty>>,
        stack: &mut Vec<Frame<'p, C::Ty>>,
    ) -> Result<Flow<'p, C::Ty>, Stop> {
        loop {
            let left = frame.call.want.saturating_sub(frame.out.len());
            let Some((c, present)) = frame.split.ctors.get(frame.next) else {
                break;
            };
            if left == 0 {
                break;
            }
            if *present {
                let (rebuild, nested) = {
                    let rows: Vec<&Row<'p>> = frame
                        .groups
                        .rows_for(c)
                        .into_iter()
                        .filter_map(|i| frame.call.matrix.get(i))
                        .collect();
                    self.specialize_call(&rows, &frame.call.v, &frame.call.tys, c, left)?
                };
                frame.wait = Wait::Ctor;
                stack.push(Frame::Wild(frame));
                stack.push(rebuild);
                return Ok(Flow::Descend(nested));
            }
            let missing = self.missing_witness(frame.ty, c)?;
            let Some(found) = &frame.default else {
                return Ok(wait_default(frame, Wait::MissingDefault(missing), stack));
            };
            let extra: Found<C::Ty> = found
                .iter()
                .take(left)
                .map(|w| prepend(missing.clone(), w))
                .collect();
            frame.out.extend(extra);
            frame.next = frame.next.saturating_add(1);
        }
        let left = frame.call.want.saturating_sub(frame.out.len());
        if frame.split.rest && left > 0 {
            let Some(found) = &frame.default else {
                return Ok(wait_default(frame, Wait::RestDefault, stack));
            };
            let ty = frame.ty;
            let extra: Found<C::Ty> = found
                .iter()
                .take(left)
                .map(|w| prepend(Witness::wild(ty), w))
                .collect();
            frame.out.extend(extra);
        }
        Ok(Flow::Done(frame.out))
    }

    /// `c(_, …, _)` at `ty`.
    fn missing_witness(&self, ty: C::Ty, c: &Ctor) -> Result<Witness<C::Ty>, Ice> {
        let fields = self
            .cx
            .field_tys(ty, c)?
            .into_iter()
            .map(Witness::wild)
            .collect();
        Ok(Witness {
            ty,
            kind: WitnessKind::Ctor(c.clone(), fields),
        })
    }

    /// The call that specializes `v` against `matrix` to the constructor
    /// `c` of its first column, with the frame that rebuilds its witnesses
    /// around `c`.
    fn specialize_call<'p>(
        &self,
        matrix: &[&Row<'p>],
        v: &[&'p Pat],
        tys: &[C::Ty],
        c: &Ctor,
        want: usize,
    ) -> Result<Specialized<'p, C::Ty>, Stop> {
        let (Some(&ty), Some(rest_tys)) = (tys.first(), tys.get(1..)) else {
            return Err(Ice::new("exhaustiveness: specializing an empty row").into());
        };
        let field_tys = self.cx.field_tys(ty, c)?;
        let arity = field_tys.len();
        let mut rows = Vec::new();
        for row in matrix {
            specialize(row, c, arity, &mut rows)?;
        }
        let mut vs = Vec::new();
        specialize(v, c, arity, &mut vs)?;
        let mut vs = vs.into_iter();
        let (Some(v), None) = (vs.next(), vs.next()) else {
            return Err(Ice::new("exhaustiveness: the row does not specialize to one row").into());
        };
        let tys = field_tys
            .into_iter()
            .chain(rest_tys.iter().copied())
            .collect();
        let rebuild = Frame::Rebuild {
            ty,
            ctor: c.clone(),
            arity,
        };
        let call = Call {
            matrix: Rc::new(rows),
            v,
            tys,
            want,
        };
        Ok((rebuild, call))
    }

    /// The constructors to try for a column of type `ty`, given the
    /// constructors that start the rows of `matrix` and, when the row
    /// being tested starts with a constructor, that one.
    fn split(&self, ty: C::Ty, matrix: &[&Row<'_>], own: Option<&Ctor>) -> Result<Split, Ice> {
        let mut heads: Vec<&Ctor> = Vec::new();
        for row in matrix {
            if let Some(head) = row.first() {
                head_ctors(head, &mut heads);
            }
        }
        match self.cx.space(ty)? {
            Space::Variants(n) => {
                if heads.is_empty() && own.is_none() {
                    return Ok(Split {
                        ctors: Vec::new(),
                        rest: true,
                    });
                }
                let present: HashSet<&Ctor> = heads.iter().copied().collect();
                let ctors = (0..n)
                    .map(|tag| {
                        let c = Ctor::Variant(tag);
                        let p = present.contains(&c);
                        (c, p)
                    })
                    .collect();
                Ok(Split { ctors, rest: false })
            }
            Space::List => {
                let lens: Vec<ListLen> = heads
                    .iter()
                    .copied()
                    .chain(own)
                    .filter_map(|c| match c {
                        Ctor::List(len) => Some(*len),
                        _ => None,
                    })
                    .collect();
                if lens.is_empty() {
                    return Ok(Split {
                        ctors: Vec::new(),
                        rest: true,
                    });
                }
                let ctors = split_lengths(&lens)?
                    .into_iter()
                    .map(|len| {
                        let c = Ctor::List(len);
                        let p = heads.iter().any(|h| covers(h, &c));
                        (c, p)
                    })
                    .collect();
                Ok(Split { ctors, rest: false })
            }
            Space::Infinite => {
                // In the order the rows first name them, for stable
                // witnesses.
                let mut seen: HashSet<&Ctor> = HashSet::new();
                let mut ctors: Vec<(Ctor, bool)> = Vec::new();
                for c in heads.iter().copied() {
                    if seen.insert(c) {
                        ctors.push((c.clone(), true));
                    }
                }
                if let Some(c) = own
                    && !seen.contains(c)
                {
                    ctors.push((c.clone(), false));
                }
                Ok(Split { ctors, rest: true })
            }
        }
    }
}

/// The rows of a matrix by the variant or literal their first pattern
/// starts with, as row indexes in order. A row whose first pattern may
/// start with anything else (a wildcard, a list pattern, or alternatives
/// with one of those) is *open*: it may match any constructor. Since
/// variants and literals are disjoint, the rows that can match a value
/// of the constructor `c` are exactly `c`'s group and the open rows.
#[derive(Default)]
struct Groups<'p> {
    by_ctor: HashMap<&'p Ctor, Vec<usize>>,
    open: Vec<usize>,
    len: usize,
}

impl<'p> Groups<'p> {
    fn of(matrix: &[&Row<'p>]) -> Self {
        let mut groups = Self::default();
        for (index, row) in matrix.iter().enumerate() {
            if let Some(&head) = row.first() {
                groups.add(index, head);
            } else {
                groups.open.push(index);
                groups.len = index.saturating_add(1);
            }
        }
        groups
    }

    /// Adds row `index`, which starts with `head`. Rows are added in
    /// order.
    fn add(&mut self, index: usize, head: &'p Pat) {
        let mut keys = Vec::new();
        if keyed_heads(head, &mut keys) {
            for key in keys {
                let group = self.by_ctor.entry(key).or_default();
                if group.last() != Some(&index) {
                    group.push(index);
                }
            }
        } else {
            self.open.push(index);
        }
        self.len = index.saturating_add(1);
    }

    /// The indexes, in order, of the rows that may match a value of the
    /// variant or literal `c`; every row for a list constructor.
    fn rows_for(&self, c: &Ctor) -> Vec<usize> {
        if matches!(c, Ctor::List(_)) {
            return (0..self.len).collect();
        }
        merge(
            self.by_ctor.get(c).map_or(&[][..], Vec::as_slice),
            &self.open,
        )
    }

    /// The rows of `matrix` that may match some value that `head`
    /// matches, in order.
    fn compatible<'m>(&self, matrix: &'m [Row<'p>], head: &'p Pat) -> Vec<&'m Row<'p>> {
        let mut keys = Vec::new();
        let indexes = if keyed_heads(head, &mut keys) {
            let mut all = self.open.clone();
            for key in keys {
                if let Some(group) = self.by_ctor.get(key) {
                    all.extend_from_slice(group);
                }
            }
            all.sort_unstable();
            all.dedup();
            all
        } else {
            (0..self.len).collect()
        };
        indexes.into_iter().filter_map(|i| matrix.get(i)).collect()
    }
}

/// Collects the variants and literals `pat` starts with into `keys`, and
/// whether it starts with nothing else.
fn keyed_heads<'p>(pat: &'p Pat, keys: &mut Vec<&'p Ctor>) -> bool {
    match pat {
        Pat::Wild | Pat::Ctor(Ctor::List(_), _) => false,
        Pat::Ctor(c, _) => {
            if !keys.contains(&c) {
                keys.push(c);
            }
            true
        }
        Pat::Or(alts) => alts.iter().all(|alt| keyed_heads(alt, keys)),
    }
}

/// The union of two ascending lists, ascending.
fn merge(a: &[usize], b: &[usize]) -> Vec<usize> {
    let mut out = Vec::with_capacity(a.len().saturating_add(b.len()));
    let (mut i, mut j) = (0, 0);
    while let (Some(&x), Some(&y)) = (a.get(i), b.get(j)) {
        if x <= y {
            out.push(x);
            i = i.saturating_add(1);
            if x == y {
                j = j.saturating_add(1);
            }
        } else {
            out.push(y);
            j = j.saturating_add(1);
        }
    }
    out.extend_from_slice(a.get(i..).unwrap_or(&[]));
    out.extend_from_slice(b.get(j..).unwrap_or(&[]));
    out
}

/// Whether `pat` matches every value without looking at it: a wildcard,
/// or alternatives with one.
fn matches_all(pat: &Pat) -> bool {
    match pat {
        Pat::Wild => true,
        Pat::Ctor(..) => false,
        Pat::Or(alts) => alts.iter().any(matches_all),
    }
}

/// The disjoint list constructors that cover every length, given the
/// list patterns of a column: one per length below `L`, and one for
/// every length from `L`, where `L` is past every fixed length and fits
/// every spread pattern's prefix and suffix. All lengths from `L` on
/// behave alike for these patterns.
fn split_lengths(lens: &[ListLen]) -> Result<Vec<ListLen>, Ice> {
    let overflow = || Ice::new("exhaustiveness: list pattern too long");
    let mut max_fixed: Option<u32> = None;
    let mut max_prefix = 0_u32;
    let mut max_suffix = 0_u32;
    for len in lens {
        match *len {
            ListLen::Fixed(n) => max_fixed = Some(max_fixed.map_or(n, |m| m.max(n))),
            ListLen::Var { prefix, suffix } => {
                max_prefix = max_prefix.max(prefix);
                max_suffix = max_suffix.max(suffix);
            }
        }
    }
    if let Some(fixed) = max_fixed {
        let past = fixed.checked_add(1).ok_or_else(overflow)?;
        let both = max_prefix.checked_add(max_suffix).ok_or_else(overflow)?;
        if past > both {
            max_prefix = past.checked_sub(max_suffix).ok_or_else(overflow)?;
        }
    }
    let from = max_prefix.checked_add(max_suffix).ok_or_else(overflow)?;
    let mut out: Vec<ListLen> = (0..from).map(ListLen::Fixed).collect();
    out.push(ListLen::Var {
        prefix: max_prefix,
        suffix: max_suffix,
    });
    Ok(out)
}

/// Whether every value of the split constructor `c` matches the pattern
/// constructor `pat`.
fn covers(pat: &Ctor, c: &Ctor) -> bool {
    match (pat, c) {
        (Ctor::List(ListLen::Fixed(n)), Ctor::List(ListLen::Fixed(m))) => n == m,
        (Ctor::List(ListLen::Var { prefix, suffix }), Ctor::List(len)) => match *len {
            ListLen::Fixed(m) => prefix.checked_add(*suffix).is_some_and(|min| min <= m),
            ListLen::Var {
                prefix: p,
                suffix: s,
            } => *prefix <= p && *suffix <= s,
        },
        (a, b) => a == b,
    }
}

/// The constructors a pattern starts with, alternatives included.
fn head_ctors<'p>(pat: &'p Pat, out: &mut Vec<&'p Ctor>) {
    match pat {
        Pat::Wild => {}
        Pat::Ctor(c, _) => out.push(c),
        Pat::Or(alts) => {
            for alt in alts {
                head_ctors(alt, out);
            }
        }
    }
}

/// The rows of the default matrix that `row` gives: the rest of the row
/// for each alternative of its first pattern that is a wildcard.
fn default_rows<'p>(row: &[&'p Pat], out: &mut Vec<Row<'p>>) {
    let Some((&head, rest)) = row.split_first() else {
        return;
    };
    match head {
        Pat::Wild => out.push(rest.to_vec()),
        Pat::Ctor(..) => {}
        Pat::Or(alts) => {
            for alt in alts {
                let expanded: Row<'p> = std::iter::once(alt).chain(rest.iter().copied()).collect();
                default_rows(&expanded, out);
            }
        }
    }
}

/// The rows that `row` gives when its first column is specialized to the
/// split constructor `c` of `arity` fields.
fn specialize<'p>(
    row: &[&'p Pat],
    c: &Ctor,
    arity: usize,
    out: &mut Vec<Row<'p>>,
) -> Result<(), Ice> {
    let Some((&head, rest)) = row.split_first() else {
        return Err(Ice::new("exhaustiveness: specializing an empty row"));
    };
    match head {
        Pat::Wild => {
            let mut new: Row<'p> = Vec::with_capacity(arity.saturating_add(rest.len()));
            new.extend(std::iter::repeat_n(&WILD, arity));
            new.extend_from_slice(rest);
            out.push(new);
        }
        Pat::Ctor(pc, fields) => {
            if covers(pc, c) {
                let mut new: Row<'p> = Vec::with_capacity(arity.saturating_add(rest.len()));
                spread_fields(pc, fields, arity, &mut new)?;
                new.extend_from_slice(rest);
                out.push(new);
            }
        }
        Pat::Or(alts) => {
            for alt in alts {
                let expanded: Row<'p> = std::iter::once(alt).chain(rest.iter().copied()).collect();
                specialize(&expanded, c, arity, out)?;
            }
        }
    }
    Ok(())
}

/// The fields of a pattern with constructor `pc`, laid out for a split
/// constructor of `arity` fields that `pc` covers: a spread pattern's
/// prefix, then wildcards for the middle, then its suffix.
fn spread_fields<'p>(
    pc: &Ctor,
    fields: &'p [Pat],
    arity: usize,
    out: &mut Row<'p>,
) -> Result<(), Ice> {
    let wrong = || Ice::new("exhaustiveness: a pattern with the wrong number of fields");
    if let Ctor::List(len @ ListLen::Var { prefix, .. }) = pc {
        let own = len
            .arity()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(wrong)?;
        if fields.len() != own || arity < own {
            return Err(wrong());
        }
        let at = usize::try_from(*prefix).map_err(|_| wrong())?;
        let (front, back) = fields.split_at(at.min(fields.len()));
        out.extend(front);
        let middle = arity.checked_sub(own).ok_or_else(wrong)?;
        out.extend(std::iter::repeat_n(&WILD, middle));
        out.extend(back);
    } else {
        if fields.len() != arity {
            return Err(wrong());
        }
        out.extend(fields);
    }
    Ok(())
}

/// `head` followed by the witnesses of the other columns.
fn prepend<T: Clone>(head: Witness<T>, rest: &[Witness<T>]) -> Vec<Witness<T>> {
    std::iter::once(head).chain(rest.iter().cloned()).collect()
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
