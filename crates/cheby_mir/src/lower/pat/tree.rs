//! The decision-tree compiler (D3): a pattern matrix becomes a [`Tree`]
//! of tests on subject positions, after Maranget, "Compiling pattern
//! matching to good decision trees" (ML Workshop 2008).
//!
//! A row is an arm (or an alternative of one): one cell per column, the
//! arm's index, and the names it has bound so far. A column is a
//! position ([`Occ`]): a subject, or a step from another position (a
//! constructor or tuple field, a list element counted from either end,
//! the slice a spread binds). Positions are interned, so one access path
//! is one [`OccId`] everywhere in a tree.
//!
//! Compiling a matrix:
//!
//! - no rows: [`Tree::Fail`], a value no arm matches. In an exhaustive
//!   `case` this cannot happen (§6.9), so it lowers to `unreachable`;
//!   in a `let assert` it is the panic (§11.4).
//! - the first row has only wildcards: [`Tree::Leaf`] for its arm. With a
//!   guard, the rows of the later arms form the tree taken when the guard
//!   is `False` (§5.9.1, §5.9.3); a guard runs at most once per arm.
//! - otherwise one column is tested, its cases are the specialized
//!   matrices (the column replaced by the positions inside it) and its
//!   default the rows with a wildcard there (the column removed).
//!
//! Since a tested column is removed and positions below it are new
//! columns, no path through a tree tests one position twice.
//!
//! **Column heuristic.** First, the leftmost column that needs no test is
//! taken apart: tuples, constructors of single-constructor types, and
//! lists matched only by `[..]`/`[..rest]` patterns. Then, among the
//! columns where the first row is not a wildcard (only those can decide
//! whether the first row matches), the one with the longest run of
//! non-wildcard cells from the first row down (Maranget's "needed
//! prefix"), then the fewest distinct heads (small branching), then the
//! leftmost. The choice depends only on the patterns, so trees are
//! deterministic (D-264).
//!
//! **Tests.** A type with several constructors switches on its tag
//! (`read_tag`, plan §5.3; `Bool` branches unboxed). `Int` and sized
//! literals switch on the value, `String` literals compare in order
//! (`StrEq`). Lists switch on their length: each length an exact pattern
//! names, and each length below the longest fixed part of a spread
//! pattern, is its own case where every element has a fixed index; the
//! default holds the remaining lengths, where only spread patterns
//! match, and their suffix elements are counted from the end. A column
//! whose switch would need more than [`MAX_LENGTH_CASES`] lengths, and
//! whose patterns have no suffix, is matched one element at a time
//! instead: "exactly `k` elements, or more?", then element `k`, so its
//! size grows linearly with its patterns.
//! Alternatives become one row each (left to right, §6.7), with the same
//! arm, so their names join where the arm's body starts.

use cheby_iface::{DefId, Ice, Idx, ItemSig, Ty, TypeDefKind, builtin};
use cheby_resolve::LocalId;
use cheby_types::thir::{Lit, Pat, PatKind};
use rustc_hash::FxHashMap;

use crate::defs::Defs;
use crate::ir::TagType;
use crate::repr::{Repr, repr_of};

/// A position in the subjects: an index into [`Decision::occs`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct OccId(u32);

impl OccId {
    /// The index in [`Decision::occs`].
    pub(crate) fn index(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }

    /// The position with index `n`, for hand-built trees in tests.
    #[cfg(test)]
    pub(crate) const fn from_index(n: u32) -> Self {
        Self(n)
    }
}

/// How a position is reached from its parent.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Step {
    /// Subject `n` of the `case` (or the value of a `let`).
    Subject(u32),
    /// Field `field` (declaration index) of a value built with `ctor`.
    Field { ctor: DefId, field: u32 },
    /// Element `index` of a tuple of `arity` elements.
    Tuple { arity: u32, index: u32 },
    /// Element `n` of a list, from the start.
    Index(u32),
    /// Element `n` from the end of a list (1 is the last).
    FromEnd(u32),
    /// The elements of a list from `start` up to `from_end` before its
    /// end: what a spread binds (D-053, D-158).
    Slice { start: u32, from_end: u32 },
}

impl Step {
    /// Left-to-right order of the positions inside one value.
    fn order(self) -> (u8, u32) {
        match self {
            Self::Subject(n)
            | Self::Field { field: n, .. }
            | Self::Tuple { index: n, .. }
            | Self::Index(n) => (0, n),
            Self::Slice { start, .. } => (1, start),
            Self::FromEnd(n) => (2, u32::MAX.saturating_sub(n)),
        }
    }
}

/// A position: its parent, the step from there, and the type of the
/// values there.
#[derive(Clone, Debug)]
pub(crate) struct Occ {
    pub parent: Option<OccId>,
    pub step: Step,
    pub ty: Ty,
}

/// What a [`Tree::Test`] node examines.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Test {
    /// The constructor tag of a type with several constructors.
    Tag(TagType),
    /// An unboxed `Bool`.
    Bool,
    /// An integer of this representation.
    Int(Repr),
    /// A `String`, against literals.
    Str,
    /// A list's length.
    Len,
}

/// One case of a [`Tree::Test`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Key {
    Ctor {
        ctor: DefId,
        tag: u32,
    },
    Bool(bool),
    Int(i128),
    Str(Box<str>),
    Len(u64),
    /// Several lengths that no list pattern of the column matches: one
    /// case for all of them, where only wildcard rows are left.
    Lens(Vec<u64>),
}

/// A decision tree.
#[derive(Clone, Debug)]
pub(crate) enum Tree {
    /// No arm matches.
    Fail,
    /// Arm `arm` matches with these bindings. `otherwise` is the tree
    /// taken when its guard is `False` (`Some` exactly for guarded arms).
    Leaf {
        arm: usize,
        bindings: Vec<(LocalId, OccId)>,
        otherwise: Option<Box<Tree>>,
    },
    /// Examines `occ`. `default` is `None` exactly when the cases cover
    /// every value (all constructors of the type).
    Test {
        occ: OccId,
        test: Test,
        cases: Vec<(Key, Tree)>,
        default: Option<Box<Tree>>,
    },
}

impl Tree {
    /// How many leaves name each arm (of `arms` arms).
    pub(crate) fn leaf_counts(&self, arms: usize) -> Vec<usize> {
        let mut counts = vec![0_usize; arms];
        let mut work = vec![self];
        while let Some(t) = work.pop() {
            match t {
                Self::Fail => {}
                Self::Leaf { arm, otherwise, .. } => {
                    if let Some(c) = counts.get_mut(*arm) {
                        *c = c.saturating_add(1);
                    }
                    work.extend(otherwise.as_deref());
                }
                Self::Test { cases, default, .. } => {
                    work.extend(cases.iter().map(|(_, t)| t));
                    work.extend(default.as_deref());
                }
            }
        }
        counts
    }
}

/// A compiled match: the positions and the tree over them.
#[derive(Clone, Debug)]
pub(crate) struct Decision {
    pub occs: Vec<Occ>,
    pub tree: Tree,
}

impl Decision {
    /// Position `id`.
    pub(crate) fn occ(&self, id: OccId) -> Result<&Occ, Ice> {
        self.occs
            .get(id.index())
            .ok_or_else(|| Ice::new("a pattern position outside its decision"))
    }
}

/// The rows to compile: one pattern per subject and the arm's index.
pub(crate) struct Input<'p> {
    pub subjects: Vec<Ty>,
    pub rows: Vec<(Vec<&'p Pat>, usize)>,
    /// Whether each arm has a guard.
    pub guarded: Vec<bool>,
}

/// Compiles `input` into a decision tree.
///
/// # Errors
///
/// An [`Ice`] for a pattern the front end should have rejected.
pub(crate) fn decide(input: Input<'_>, defs: &Defs<'_>) -> Result<Decision, Ice> {
    let mut c = Compiler {
        defs,
        occs: Vec::new(),
        interned: FxHashMap::default(),
        guarded: input.guarded,
    };
    let mut cols = Vec::with_capacity(input.subjects.len());
    for (n, &ty) in input.subjects.iter().enumerate() {
        let n = u32::try_from(n).map_err(|_| Ice::new("too many `case` subjects"))?;
        cols.push(c.occ(None, Step::Subject(n), ty)?);
    }
    let mut rows = Vec::with_capacity(input.rows.len());
    for (pats, arm) in input.rows {
        if pats.len() != cols.len() {
            return Err(Ice::new("a `case` arm without one pattern per subject"));
        }
        let mut binds = Vec::new();
        let cells = pats
            .into_iter()
            .zip(&cols)
            .map(|(p, &occ)| normalize(p, occ, &mut binds))
            .collect();
        rows.push(Row { cells, arm, binds });
    }
    let bounds = vec![0; cols.len()];
    let tree = c.compile(Matrix { cols, bounds, rows })?;
    let decision = Decision { occs: c.occs, tree };
    #[cfg(test)]
    record(&decision);
    Ok(decision)
}

#[cfg(test)]
thread_local! {
    /// Every decision compiled on this thread, for the checks of
    /// `tests.rs` (no position tested twice on a path).
    pub(crate) static RECORDED: std::cell::RefCell<Vec<Decision>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn record(decision: &Decision) {
    RECORDED.with(|r| r.borrow_mut().push(decision.clone()));
}

#[derive(Clone)]
struct Row<'p> {
    /// `None` is a wildcard (bindings already moved to `binds`).
    cells: Vec<Option<&'p Pat>>,
    arm: usize,
    binds: Vec<(LocalId, OccId)>,
}

struct Matrix<'p> {
    cols: Vec<OccId>,
    /// Per column: the length a list column is known to have at least,
    /// with its first that many prefix elements already taken apart (see
    /// [`Compiler::split_list_elementwise`]); 0 for every other column.
    bounds: Vec<u32>,
    rows: Vec<Row<'p>>,
}

/// What one step of element-wise list matching found.
enum ElementStep<'p> {
    /// Every row has a wildcard in the list column now.
    Done(Matrix<'p>),
    /// Test the length against `lb`.
    Test {
        lb: u32,
        exact: Matrix<'p>,
        longer: Matrix<'p>,
    },
}

/// A list column whose length switch would need more cases than this is
/// matched one element at a time instead. The choice is the compiler's
/// own: every arm is tried in order either way (§6.6), and only the size
/// of the code differs.
const MAX_LENGTH_CASES: usize = 8;

/// What a column holds, from the first cell that is not a wildcard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Tuple(u32),
    /// Constructors of the type `ty`, which has `count` of them.
    Ctor {
        ty: DefId,
        count: usize,
    },
    Int,
    Str,
    List,
}

struct Compiler<'a, 'd> {
    defs: &'a Defs<'d>,
    occs: Vec<Occ>,
    interned: FxHashMap<(Option<OccId>, Step), OccId>,
    guarded: Vec<bool>,
}

/// A row's sub-cells for one case: positions with their patterns.
type SubCells<'p> = Vec<(Step, &'p Pat)>;

/// Strips bindings and `as` from the top of `pat` at `occ`, recording
/// them in `binds`; `None` for a pattern that matches everything there.
/// An alternative whose first branch matches everything is that branch
/// (the later ones are never tried, §6.7).
fn normalize<'p>(pat: &'p Pat, occ: OccId, binds: &mut Vec<(LocalId, OccId)>) -> Option<&'p Pat> {
    let mut p = pat;
    loop {
        match &p.kind {
            PatKind::Wild => return None,
            PatKind::Bind(l) => {
                binds.push((*l, occ));
                return None;
            }
            PatKind::As { pat, local } => {
                binds.push((*local, occ));
                p = pat;
            }
            PatKind::Or(alts) => match alts.first() {
                Some(first) if is_catch_all(first) => p = first,
                _ => return Some(p),
            },
            _ => return Some(p),
        }
    }
}

/// Whether `pat` matches every value at the top (a wildcard or a name,
/// possibly under `as`).
fn is_catch_all(pat: &Pat) -> bool {
    match &pat.kind {
        PatKind::Wild | PatKind::Bind(_) => true,
        PatKind::As { pat, .. } => is_catch_all(pat),
        _ => false,
    }
}

/// The first pattern under `as` and alternatives that is not one of
/// them: what decides the kind of a column.
fn head(pat: &Pat) -> &Pat {
    match &pat.kind {
        PatKind::As { pat, .. } => head(pat),
        PatKind::Or(alts) => alts.first().map_or(pat, |a| head(a)),
        _ => pat,
    }
}

fn len_u32(n: usize) -> Result<u32, Ice> {
    u32::try_from(n).map_err(|_| Ice::new("a pattern too large to compile"))
}

/// The prefix and suffix length and the spread of a list pattern.
fn list_parts(pat: &Pat) -> Option<(&[Pat], Option<&Pat>, &[Pat])> {
    match &pat.kind {
        PatKind::List {
            prefix,
            rest,
            suffix,
        } => Some((prefix, rest.as_deref(), suffix)),
        _ => None,
    }
}

impl<'p> Compiler<'_, '_> {
    /// The position `step` below `parent`, holding values of type `ty`.
    fn occ(&mut self, parent: Option<OccId>, step: Step, ty: Ty) -> Result<OccId, Ice> {
        if let Some(&id) = self.interned.get(&(parent, step)) {
            return Ok(id);
        }
        let id = OccId(len_u32(self.occs.len())?);
        self.occs.push(Occ { parent, step, ty });
        self.interned.insert((parent, step), id);
        Ok(id)
    }

    /// The constructor `ctor`: its type, tag, and how many constructors
    /// the type has.
    fn ctor_info(&self, ctor: DefId) -> Result<(DefId, u32, usize), Ice> {
        let ItemSig::Ctor(sig) = &self.defs.item(ctor)?.sig else {
            return Err(Ice::new(format!(
                "{ctor:?} in a pattern is not a constructor"
            )));
        };
        let ty = DefId::new(ctor.module, sig.ty);
        let ItemSig::Type(t) = &self.defs.item(ty)?.sig else {
            return Err(Ice::new(format!("{ty:?} is not a type")));
        };
        let TypeDefKind::Adt { ctors } = &t.kind else {
            return Err(Ice::new(format!("{ty:?} has no constructors")));
        };
        Ok((ty, sig.tag, ctors.len()))
    }

    /// The constructors of `ty`, in tag order.
    fn ctors_of(&self, ty: DefId) -> Result<Vec<DefId>, Ice> {
        let ItemSig::Type(t) = &self.defs.item(ty)?.sig else {
            return Err(Ice::new(format!("{ty:?} is not a type")));
        };
        let TypeDefKind::Adt { ctors } = &t.kind else {
            return Err(Ice::new(format!("{ty:?} has no constructors")));
        };
        let mut out = Vec::with_capacity(ctors.len());
        for &index in ctors {
            let ctor = DefId::new(ty.module, index);
            let (_, tag, _) = self.ctor_info(ctor)?;
            out.push((tag, ctor));
        }
        out.sort_unstable();
        Ok(out.into_iter().map(|(_, c)| c).collect())
    }

    fn kind_of(&self, pat: &Pat) -> Result<Option<Kind>, Ice> {
        Ok(Some(match &head(pat).kind {
            PatKind::Tuple(ps) => Kind::Tuple(len_u32(ps.len())?),
            PatKind::Ctor { ctor, .. } => {
                let (ty, _, count) = self.ctor_info(*ctor)?;
                Kind::Ctor { ty, count }
            }
            PatKind::Lit(Lit::Int(_)) => Kind::Int,
            PatKind::Lit(Lit::Str(_)) => Kind::Str,
            PatKind::Lit(_) => return Err(Ice::new("a float literal pattern (D-113)")),
            PatKind::List { .. } => Kind::List,
            _ => return Ok(None),
        }))
    }

    /// The kind of column `c`.
    fn column_kind(&self, m: &Matrix<'p>, c: usize) -> Result<Option<Kind>, Ice> {
        for row in &m.rows {
            if let Some(Some(p)) = row.cells.get(c)
                && let Some(kind) = self.kind_of(p)?
            {
                return Ok(Some(kind));
            }
        }
        Ok(None)
    }

    /// Whether column `c` can be taken apart without a test.
    fn is_structural(&self, m: &Matrix<'p>, c: usize) -> Result<bool, Ice> {
        Ok(match self.column_kind(m, c)? {
            Some(Kind::Tuple(_)) => true,
            Some(Kind::Ctor { count, .. }) => count == 1,
            Some(Kind::List) if m.bounds.get(c).is_some_and(|&b| b > 0) => false,
            Some(Kind::List) => m.rows.iter().all(|r| match r.cells.get(c) {
                Some(Some(p)) => {
                    matches!(list_parts(head(p)), Some((pre, Some(_), suf)) if pre.is_empty() && suf.is_empty())
                        && !matches!(p.kind, PatKind::Or(_))
                }
                _ => true,
            }),
            _ => false,
        })
    }

    fn compile(&mut self, m: Matrix<'p>) -> Result<Tree, Ice> {
        let m = drop_wild_columns(m);
        let Some(first) = m.rows.first() else {
            return Ok(Tree::Fail);
        };
        if first.cells.iter().all(Option::is_none) {
            let arm = first.arm;
            let bindings = first.binds.clone();
            let guarded = self.guarded.get(arm).copied().unwrap_or(false);
            let otherwise = if guarded {
                // D-571: the guard runs once, with the names of the first
                // alternative that matches (§5.9.1, §6.7: "trying them from
                // left to right"; §5.15.1: guards in arm order). When it is
                // `False` the arm is skipped, so its later alternatives are
                // not tried.
                let rest = Matrix {
                    cols: m.cols,
                    bounds: m.bounds,
                    rows: m.rows.into_iter().filter(|r| r.arm != arm).collect(),
                };
                Some(Box::new(self.compile(rest)?))
            } else {
                None
            };
            return Ok(Tree::Leaf {
                arm,
                bindings,
                otherwise,
            });
        }
        for c in 0..m.cols.len() {
            if self.is_structural(&m, c)? {
                return self.split(m, c);
            }
        }
        let mut best: Option<(usize, usize, usize)> = None;
        for c in 0..m.cols.len() {
            if !matches!(first.cells.get(c), Some(Some(_))) {
                continue;
            }
            let needed = m
                .rows
                .iter()
                .take_while(|r| matches!(r.cells.get(c), Some(Some(_))))
                .count();
            let branching = distinct_heads(&m, c);
            let better = best.is_none_or(|(_, n, b)| needed > n || (needed == n && branching < b));
            if better {
                best = Some((c, needed, branching));
            }
        }
        let (c, _, _) = best.ok_or_else(|| Ice::new("no column to test"))?;
        self.split(m, c)
    }

    /// Replaces alternatives in column `c` by one row per branch.
    fn expand_or(m: Matrix<'p>, c: usize) -> Result<Matrix<'p>, Ice> {
        let Matrix { cols, bounds, rows } = m;
        let occ = *cols.get(c).ok_or_else(|| Ice::new("no such column"))?;
        let mut out = Vec::with_capacity(rows.len());
        let mut work: Vec<Row<'p>> = rows.into_iter().rev().collect();
        while let Some(row) = work.pop() {
            let Some(Some(Pat {
                kind: PatKind::Or(alts),
                ..
            })) = row.cells.get(c).copied()
            else {
                out.push(row);
                continue;
            };
            let mut expanded = Vec::with_capacity(alts.len());
            for alt in alts {
                let mut r = row.clone();
                let cell = normalize(alt, occ, &mut r.binds);
                let done = cell.is_none();
                if let Some(slot) = r.cells.get_mut(c) {
                    *slot = cell;
                }
                expanded.push(r);
                if done {
                    break;
                }
            }
            // Pushed in reverse, so the first branch is handled next.
            work.extend(expanded.into_iter().rev());
        }
        Ok(Matrix {
            cols,
            bounds,
            rows: out,
        })
    }

    /// Tests (or takes apart) column `c`.
    fn split(&mut self, m: Matrix<'p>, c: usize) -> Result<Tree, Ice> {
        let m = Self::expand_or(m, c)?;
        let occ = *m.cols.get(c).ok_or_else(|| Ice::new("no such column"))?;
        let Some(kind) = self.column_kind(&m, c)? else {
            // Only wildcards are left after the alternatives.
            return self.compile(m);
        };
        // Each kind is split in a function of its own, so this frame, which
        // recurses once per tested position, stays small.
        match kind {
            Kind::Tuple(arity) => self.split_tuple(&m, c, arity),
            Kind::Ctor { ty, count } => self.split_ctor(&m, c, occ, ty, count),
            Kind::Int | Kind::Str => self.split_literals(&m, c, occ, kind),
            Kind::List if elementwise(&m, c)? => self.split_list_elementwise(m, c, occ),
            Kind::List => self.split_list(&m, c, occ),
        }
    }

    /// Takes apart tuple column `c`.
    fn split_tuple(&mut self, m: &Matrix<'p>, c: usize, arity: u32) -> Result<Tree, Ice> {
        let sub = self.specialize(m, c, |p| match &p.kind {
            PatKind::Tuple(ps) => ps
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    Ok((
                        Step::Tuple {
                            arity,
                            index: len_u32(i)?,
                        },
                        p,
                    ))
                })
                .collect::<Result<Vec<_>, Ice>>()
                .map(Some),
            _ => Err(Ice::new("a tuple column with another pattern")),
        })?;
        self.compile(sub)
    }

    /// Tests (or takes apart, for a single constructor) constructor column
    /// `c` of type `ty`, which has `count` constructors.
    fn split_ctor(
        &mut self,
        m: &Matrix<'p>,
        c: usize,
        occ: OccId,
        ty: DefId,
        count: usize,
    ) -> Result<Tree, Ice> {
        let all = self.ctors_of(ty)?;
        if all.len() != count {
            return Err(Ice::new("a type's constructor count changed"));
        }
        let present: Vec<DefId> = all
            .iter()
            .copied()
            .filter(|&ctor| {
                m.rows.iter().any(|r| {
                    matches!(r.cells.get(c), Some(Some(p)) if matches!(p.kind, PatKind::Ctor { ctor: k, .. } if k == ctor))
                })
            })
            .collect();
        let mut cases = Vec::with_capacity(present.len());
        for &ctor in &present {
            let sub = self.specialize(m, c, |p| match &p.kind {
                PatKind::Ctor { ctor: k, fields } if *k == ctor => Ok(Some(
                    fields
                        .iter()
                        .map(|(field, p)| {
                            (
                                Step::Field {
                                    ctor,
                                    field: *field,
                                },
                                p,
                            )
                        })
                        .collect(),
                )),
                PatKind::Ctor { .. } => Ok(None),
                _ => Err(Ice::new("a constructor column with another pattern")),
            })?;
            if count == 1 {
                return self.compile(sub);
            }
            let (_, tag, _) = self.ctor_info(ctor)?;
            let key = if ty == builtin::BOOL {
                Key::Bool(ctor == builtin::TRUE)
            } else {
                Key::Ctor { ctor, tag }
            };
            cases.push((key, self.compile(sub)?));
        }
        let default = if present.len() < count {
            Some(Box::new(self.compile(default_matrix(m, c))?))
        } else {
            None
        };
        let test = match ty {
            t if t == builtin::BOOL => Test::Bool,
            t if t == builtin::OPTION => Test::Tag(TagType::Option),
            t if t == builtin::RESULT => Test::Tag(TagType::Result),
            t if t == builtin::ORDER => Test::Tag(TagType::Order),
            t => Test::Tag(TagType::Adt(t)),
        };
        Ok(Tree::Test {
            occ,
            test,
            cases,
            default,
        })
    }

    /// Tests one more element of list column `c`, whose first `lb`
    /// elements are taken apart already (`m.bounds`): the list is known to
    /// have at least `lb` elements. A row whose spread starts at or before
    /// `lb` matches every longer list, so its cell becomes a wildcard that
    /// binds the rest. Then the length is tested against `lb`: equal, the
    /// rows that match exactly `lb` elements go on; longer, element `lb`
    /// becomes a column of its own and the bound grows by one.
    ///
    /// Each list pattern is visited once per element, so a column of
    /// spreads with growing prefixes compiles in linear size, where one
    /// switch over every length would read N(N+1)/2 elements
    /// (the gate-2 review's R2-2). The compiler recurses once per tested
    /// position, so a list pattern of several hundred elements needs a
    /// deep stack.
    fn split_list_elementwise(&mut self, m: Matrix<'p>, c: usize, occ: OccId) -> Result<Tree, Ice> {
        // The matrices are built in a function of their own, so the frame
        // that recurses once per element stays small.
        match self.element_step(m, c, occ)? {
            ElementStep::Done(m) => self.compile(m),
            ElementStep::Test { lb, exact, longer } => Ok(Tree::Test {
                occ,
                test: Test::Len,
                cases: vec![(Key::Len(u64::from(lb)), self.compile(exact)?)],
                default: Some(Box::new(self.compile(longer)?)),
            }),
        }
    }

    /// One step of [`Self::split_list_elementwise`]: the matrix when the
    /// list column needs no more tests, or the bound and the matrices for
    /// a list of exactly that many elements and for a longer one.
    #[inline(never)]
    fn element_step(
        &mut self,
        mut m: Matrix<'p>,
        c: usize,
        occ: OccId,
    ) -> Result<ElementStep<'p>, Ice> {
        let lb = *m.bounds.get(c).ok_or_else(|| Ice::new("no such column"))?;
        let lb_len = usize::try_from(lb).map_err(|_| Ice::new("a list bound out of range"))?;
        // Spreads that start at or before the bound match from here on.
        for row in &mut m.rows {
            let Some(slot) = row.cells.get_mut(c) else {
                continue;
            };
            let Some(p) = *slot else {
                continue;
            };
            let (pre, rest, _) =
                list_parts(p).ok_or_else(|| Ice::new("a list column with another pattern"))?;
            if let Some(r) = rest
                && pre.len() <= lb_len
            {
                let start = len_u32(pre.len())?;
                let slice = self.occ(Some(occ), Step::Slice { start, from_end: 0 }, r.ty)?;
                if normalize(r, slice, &mut row.binds).is_some() {
                    return Err(Ice::new("a spread with a pattern in a list column"));
                }
                *slot = None;
            }
        }
        if m.rows.iter().all(|r| matches!(r.cells.get(c), Some(None))) {
            return Ok(ElementStep::Done(m));
        }
        // The rows for a list of exactly `lb` elements: the wildcards, and
        // the exact patterns of that length, all of whose elements are
        // taken apart already.
        let mut exact_rows = Vec::new();
        for row in &m.rows {
            match row.cells.get(c) {
                Some(Some(p)) => {
                    let (pre, rest, _) = list_parts(p)
                        .ok_or_else(|| Ice::new("a list column with another pattern"))?;
                    if rest.is_none() && pre.len() == lb_len {
                        let mut row = row.clone();
                        if let Some(slot) = row.cells.get_mut(c) {
                            *slot = None;
                        }
                        exact_rows.push(row);
                    }
                }
                _ => exact_rows.push(row.clone()),
            }
        }
        let exact = Matrix {
            cols: m.cols.clone(),
            bounds: m.bounds.clone(),
            rows: exact_rows,
        };
        // The rows for a longer list, with element `lb` as a new column
        // to the left of the list.
        let mut element_ty = None;
        let mut longer_rows = Vec::with_capacity(m.rows.len());
        let mut element_cells = Vec::with_capacity(m.rows.len());
        for row in &m.rows {
            match row.cells.get(c) {
                Some(Some(p)) => {
                    let (pre, _, _) = list_parts(p)
                        .ok_or_else(|| Ice::new("a list column with another pattern"))?;
                    let Some(element) = pre.get(lb_len) else {
                        // An exact pattern of `lb` elements, or fewer.
                        continue;
                    };
                    element_ty.get_or_insert(element.ty);
                    element_cells.push(Some(element));
                }
                _ => element_cells.push(None),
            }
            longer_rows.push(row.clone());
        }
        let mut cols = m.cols;
        let mut bounds = m.bounds;
        let mut rows = longer_rows;
        if let Some(ty) = element_ty {
            let element = self.occ(Some(occ), Step::Index(lb), ty)?;
            cols.insert(c, element);
            bounds.insert(c, 0);
            if let Some(b) = bounds.get_mut(c.saturating_add(1)) {
                *b = lb.saturating_add(1);
            }
            for (row, cell) in rows.iter_mut().zip(element_cells) {
                let cell = cell.and_then(|p| normalize(p, element, &mut row.binds));
                row.cells.insert(c, cell);
            }
        } else if let Some(b) = bounds.get_mut(c) {
            *b = lb.saturating_add(1);
        }
        Ok(ElementStep::Test {
            lb,
            exact,
            longer: Matrix { cols, bounds, rows },
        })
    }

    fn split_literals(
        &mut self,
        m: &Matrix<'p>,
        c: usize,
        occ: OccId,
        kind: Kind,
    ) -> Result<Tree, Ice> {
        let mut keys: Vec<Key> = Vec::new();
        for row in &m.rows {
            let key = match row.cells.get(c) {
                Some(Some(p)) => match &p.kind {
                    PatKind::Lit(Lit::Int(v)) => Key::Int(*v),
                    PatKind::Lit(Lit::Str(s)) => Key::Str(s.clone()),
                    _ => return Err(Ice::new("a literal column with another pattern")),
                },
                _ => continue,
            };
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        let mut cases = Vec::with_capacity(keys.len());
        for key in keys {
            let sub = self.specialize(m, c, |p| {
                let same = match (&p.kind, &key) {
                    (PatKind::Lit(Lit::Int(v)), Key::Int(k)) => v == k,
                    (PatKind::Lit(Lit::Str(s)), Key::Str(k)) => s == k,
                    _ => false,
                };
                Ok(same.then(Vec::new))
            })?;
            cases.push((key, self.compile(sub)?));
        }
        let ty = self.occs.get(occ.index()).map(|o| o.ty);
        let test = match kind {
            Kind::Int => Test::Int(ty.map_or(Repr::Int, |t| repr_of(t, self.defs.globals))),
            _ => Test::Str,
        };
        let default = Some(Box::new(self.compile(default_matrix(m, c))?));
        Ok(Tree::Test {
            occ,
            test,
            cases,
            default,
        })
    }

    fn split_list(&mut self, m: &Matrix<'p>, c: usize, occ: OccId) -> Result<Tree, Ice> {
        // Exact lengths and the longest fixed part of a spread pattern.
        let mut exact: Vec<u32> = Vec::new();
        let mut max_min: u32 = 0;
        for row in &m.rows {
            if let Some(Some(p)) = row.cells.get(c) {
                let (pre, rest, suf) =
                    list_parts(p).ok_or_else(|| Ice::new("a list column with another pattern"))?;
                let n = len_u32(pre.len().saturating_add(suf.len()))?;
                match rest {
                    None => exact.push(n),
                    Some(_) => max_min = max_min.max(n),
                }
            }
        }
        let mut lengths: Vec<u32> = (0..max_min).chain(exact).collect();
        lengths.sort_unstable();
        lengths.dedup();
        let mut cases = Vec::with_capacity(lengths.len());
        let mut unmatched = Vec::new();
        for &len in &lengths {
            let mut matched = false;
            for row in &m.rows {
                if let Some(Some(p)) = row.cells.get(c) {
                    matched |= cells_at_length(p, len)?.is_some();
                }
            }
            if !matched {
                unmatched.push(u64::from(len));
                continue;
            }
            let sub = self.specialize(m, c, |p| cells_at_length(p, len))?;
            cases.push((Key::Len(u64::from(len)), self.compile(sub)?));
        }
        if !unmatched.is_empty() {
            cases.push((Key::Lens(unmatched), self.compile(default_matrix(m, c))?));
        }
        // Every other length: only spread patterns match, and the suffix
        // counts from the end.
        let sub = self.specialize(m, c, cells_beyond)?;
        let default = self.compile(sub)?;
        if lengths.is_empty() {
            // `[..]` and `[..rest]` only: nothing to test.
            return Ok(default);
        }
        Ok(Tree::Test {
            occ,
            test: Test::Len,
            cases,
            default: Some(Box::new(default)),
        })
    }

    /// The matrix of the rows whose cell in column `c` fits a case:
    /// `sub` gives a non-wildcard cell's positions and patterns inside
    /// it (`None` when the row does not fit), wildcard rows always fit.
    /// Column `c` is replaced by the positions any row uses, in
    /// left-to-right order.
    fn specialize(
        &mut self,
        m: &Matrix<'p>,
        c: usize,
        mut sub: impl FnMut(&'p Pat) -> Result<Option<SubCells<'p>>, Ice>,
    ) -> Result<Matrix<'p>, Ice> {
        let occ = *m.cols.get(c).ok_or_else(|| Ice::new("no such column"))?;
        let mut kept: Vec<(&Row<'p>, SubCells<'p>)> = Vec::new();
        let mut steps: Vec<(Step, Ty)> = Vec::new();
        for row in &m.rows {
            let cells = match row.cells.get(c) {
                Some(Some(p)) => match sub(p)? {
                    Some(cells) => cells,
                    None => continue,
                },
                _ => Vec::new(),
            };
            for &(step, p) in &cells {
                if !steps.iter().any(|&(s, _)| s == step) {
                    steps.push((step, p.ty));
                }
            }
            kept.push((row, cells));
        }
        steps.sort_by_key(|&(s, _)| s.order());
        let mut new_occs = Vec::with_capacity(steps.len());
        for &(step, ty) in &steps {
            new_occs.push(self.occ(Some(occ), step, ty)?);
        }
        let mut cols = m.cols.clone();
        cols.splice(c..=c, new_occs.iter().copied());
        let mut bounds = m.bounds.clone();
        bounds.splice(c..=c, new_occs.iter().map(|_| 0));
        let mut rows = Vec::with_capacity(kept.len());
        for (row, cells) in kept {
            let mut binds = row.binds.clone();
            let mut inner: Vec<Option<&'p Pat>> = Vec::with_capacity(steps.len());
            for (&(step, _), &new) in steps.iter().zip(&new_occs) {
                let cell = cells
                    .iter()
                    .find(|&&(s, _)| s == step)
                    .and_then(|&(_, p)| normalize(p, new, &mut binds));
                inner.push(cell);
            }
            let mut out = row.cells.clone();
            out.splice(c..=c, inner);
            rows.push(Row {
                cells: out,
                arm: row.arm,
                binds,
            });
        }
        Ok(Matrix { cols, bounds, rows })
    }
}

/// The positions inside list pattern `p` when the list has `len`
/// elements, every element by its index; `None` if `p` does not match
/// that length.
fn cells_at_length(p: &Pat, len: u32) -> Result<Option<SubCells<'_>>, Ice> {
    let Some((pre, rest, suf)) = list_parts(p) else {
        return Err(Ice::new("a list column with another pattern"));
    };
    let fixed = len_u32(pre.len().saturating_add(suf.len()))?;
    let fits = match rest {
        None => fixed == len,
        Some(_) => fixed <= len,
    };
    if !fits {
        return Ok(None);
    }
    let mut cells = Vec::new();
    for (i, p) in pre.iter().enumerate() {
        cells.push((Step::Index(len_u32(i)?), p));
    }
    let suffix_start = len.saturating_sub(len_u32(suf.len())?);
    for (i, p) in suf.iter().enumerate() {
        cells.push((Step::Index(suffix_start.saturating_add(len_u32(i)?)), p));
    }
    if let Some(r) = rest {
        cells.push((
            Step::Slice {
                start: len_u32(pre.len())?,
                from_end: len_u32(suf.len())?,
            },
            r,
        ));
    }
    Ok(Some(cells))
}

/// The positions inside list pattern `p` for a length no case names:
/// only spread patterns match, and their suffix counts from the end.
fn cells_beyond(p: &Pat) -> Result<Option<SubCells<'_>>, Ice> {
    let Some((pre, rest, suf)) = list_parts(p) else {
        return Err(Ice::new("a list column with another pattern"));
    };
    let Some(r) = rest else {
        return Ok(None);
    };
    let mut cells = Vec::new();
    for (i, p) in pre.iter().enumerate() {
        cells.push((Step::Index(len_u32(i)?), p));
    }
    let s = len_u32(suf.len())?;
    for (i, p) in suf.iter().enumerate() {
        cells.push((Step::FromEnd(s.saturating_sub(len_u32(i)?)), p));
    }
    cells.push((
        Step::Slice {
            start: len_u32(pre.len())?,
            from_end: s,
        },
        r,
    ));
    Ok(Some(cells))
}

/// Whether list column `c` is matched one element at a time
/// ([`Compiler::split_list_elementwise`]): it already is (its bound is
/// above 0), or its patterns have no suffix and no pattern after a
/// spread, and one switch over the lengths would need more than
/// [`MAX_LENGTH_CASES`] cases.
fn elementwise(m: &Matrix<'_>, c: usize) -> Result<bool, Ice> {
    if m.bounds.get(c).is_some_and(|&b| b > 0) {
        return Ok(true);
    }
    let mut lengths: Vec<usize> = Vec::new();
    let mut longest_spread: usize = 0;
    for row in &m.rows {
        let Some(Some(p)) = row.cells.get(c) else {
            continue;
        };
        let (pre, rest, suf) =
            list_parts(p).ok_or_else(|| Ice::new("a list column with another pattern"))?;
        if !suf.is_empty() {
            return Ok(false);
        }
        match rest {
            None => lengths.push(pre.len()),
            Some(r) if is_catch_all(r) => longest_spread = longest_spread.max(pre.len()),
            Some(_) => return Ok(false),
        }
    }
    lengths.extend(0..longest_spread);
    lengths.sort_unstable();
    lengths.dedup();
    Ok(lengths.len() > MAX_LENGTH_CASES)
}

/// The rows with a wildcard in column `c`, without that column.
fn default_matrix<'p>(m: &Matrix<'p>, c: usize) -> Matrix<'p> {
    let mut cols = m.cols.clone();
    if c < cols.len() {
        cols.remove(c);
    }
    let mut bounds = m.bounds.clone();
    if c < bounds.len() {
        bounds.remove(c);
    }
    let rows = m
        .rows
        .iter()
        .filter(|r| matches!(r.cells.get(c), Some(None)))
        .map(|r| {
            let mut r = r.clone();
            if c < r.cells.len() {
                r.cells.remove(c);
            }
            r
        })
        .collect();
    Matrix { cols, bounds, rows }
}

/// Removes the columns where every row has a wildcard.
fn drop_wild_columns(m: Matrix<'_>) -> Matrix<'_> {
    fn pick<T>(items: Vec<T>, keep: &[bool]) -> Vec<T> {
        items
            .into_iter()
            .zip(keep)
            .filter_map(|(x, &k)| k.then_some(x))
            .collect()
    }
    let keep: Vec<bool> = (0..m.cols.len())
        .map(|c| {
            m.rows
                .iter()
                .any(|r| matches!(r.cells.get(c), Some(Some(_))))
        })
        .collect();
    if keep.iter().all(|&k| k) {
        return m;
    }
    Matrix {
        cols: pick(m.cols, &keep),
        bounds: pick(m.bounds, &keep),
        rows: m
            .rows
            .into_iter()
            .map(|r| Row {
                cells: pick(r.cells, &keep),
                arm: r.arm,
                binds: r.binds,
            })
            .collect(),
    }
}

/// How many different heads column `c` has (a rough branching factor).
fn distinct_heads(m: &Matrix<'_>, column: usize) -> usize {
    let mut seen: Vec<&PatKind> = Vec::new();
    for row in &m.rows {
        if let Some(Some(pat)) = row.cells.get(column) {
            let kind = &head(pat).kind;
            let same = |seen: &&PatKind| match (seen, kind) {
                (PatKind::Ctor { ctor: left, .. }, PatKind::Ctor { ctor: right, .. }) => {
                    left == right
                }
                (PatKind::List { .. }, PatKind::List { .. }) => true,
                (left, right) => left == &right,
            };
            if !seen.iter().any(same) {
                seen.push(kind);
            }
        }
    }
    seen.len()
}

/// The locals an arm's patterns bind, sorted by id (alternatives bind the
/// same ones, D-506).
pub(crate) fn bound_locals<'p>(pats: impl IntoIterator<Item = &'p Pat>) -> Vec<LocalId> {
    let mut out: Vec<LocalId> = Vec::new();
    for pat in pats {
        pat.walk(&mut |p| match &p.kind {
            PatKind::Bind(l) | PatKind::As { local: l, .. } if !out.contains(l) => out.push(*l),
            _ => {}
        });
    }
    out.sort_by_key(|l| l.index());
    out
}
