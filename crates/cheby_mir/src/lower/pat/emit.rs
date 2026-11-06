//! Lowering a [`Decision`] to MIR blocks.
//!
//! Positions are read lazily: the first node on a path that needs a
//! position emits its access (`field`, a list `get` or `slice`) in the
//! current block, which dominates the rest of the path, and the value is
//! remembered for that path only ([`Env`]). Each read converts the value
//! to its type's representation, so a uniform field holding an `Int` is
//! unboxed once (D6). At a leaf, the positions an arm binds are read level
//! by level, every read of a level before its spreads, and an element next
//! to a spread is read with it in one `list_uncons` (D-908, D-909).
//!
//! An arm reached from one leaf has its body lowered right there. An arm
//! reached from several leaves (alternatives, or rows copied into several
//! cases) gets one block whose parameters are its names; each leaf jumps
//! there with its values, so the body is lowered once. A guard is lowered
//! at each leaf, since what happens when it is `False` depends on the
//! leaf. Bindings an arm does not use are dropped at its start by the RC
//! pass (D-205).

use cheby_diag::Span;
use cheby_iface::{Ice, Ty, TyKind};
use cheby_resolve::LocalId;
use cheby_types::thir::ExprId;
use rustc_hash::FxHashMap;

use crate::ir::{
    ArithOp, BlockId, Const, LayoutRef, Local, Operand, PanicKind, RuntimeFn, Rvalue, Terminator,
};
use crate::layout::ctor_layout;
use crate::lower::body::BodyCx;
use crate::lower::pat::tree::{Decision, Key, OccId, Step, Test, Tree};
use crate::repr::Repr;

/// The text of a failed `let assert` before its value (D-528).
const LET_ASSERT: &str = "`let assert` pattern did not match";

/// What happens when no arm matches.
#[derive(Clone, Copy)]
pub(super) enum OnFail {
    /// Impossible: the front end proved the match exhaustive (§6.9) or
    /// the pattern irrefutable (D-242).
    Unreachable,
    /// `let assert` (§11.4): panic with the value's debug text and the
    /// optional `as` message, at `at`.
    LetAssert {
        value: Local,
        ty: Ty,
        message: Option<ExprId>,
        at: Span,
    },
}

/// What a matched arm does.
#[derive(Clone, Copy)]
pub(super) enum Then {
    /// A `case` arm: lower its body, with `tail` (§5.15.2), and join with
    /// a value of type `ty`.
    Body { body: ExprId, tail: bool, ty: Ty },
    /// `let`, `let assert` or a parameter: bind the names and go on after
    /// the match.
    Continue,
}

/// One arm of a match.
pub(super) struct ArmPlan {
    pub guard: Option<ExprId>,
    pub then: Then,
    /// The names it binds, sorted by id.
    pub locals: Vec<LocalId>,
    /// Its first pattern's span: the position of the code that binds its
    /// names (D-938, D-1170); the shared tests keep the
    /// `case`'s.
    pub span: Option<Span>,
}

/// The state of lowering one decision.
struct Match<'t> {
    decision: &'t Decision,
    arms: Vec<ArmPlan>,
    /// How many leaves name each arm.
    leaves: Vec<usize>,
    /// The shared entry block of each arm reached from several leaves,
    /// with one parameter per name.
    shared: Vec<Option<(BlockId, Vec<Local>)>>,
    /// Where the names of a `Continue` arm reached from one leaf are bound.
    inline_continue: Option<BlockId>,
    on_fail: OnFail,
    fail_block: Option<BlockId>,
    /// The block after a `case` and the parameter holding its value.
    join: Option<(BlockId, Local)>,
    /// The match's span, the location of its index arithmetic.
    span: Span,
}

/// What [`BodyCx::emit_tree`] lowers next, on its explicit stack.
enum Emit<'t> {
    /// `tree` with the positions of `env`, after switching to `block` if
    /// there is one (otherwise in the current block).
    Tree {
        tree: &'t Tree,
        env: Env,
        block: Option<BlockId>,
    },
    /// The rest of a `String` test, in `block`, where the previous
    /// literal did not match: the next comparison of `value` with the
    /// first of `cases`, or `default` when none is left.
    Str {
        value: Local,
        cases: &'t [(Key, Tree)],
        default: &'t Tree,
        env: Env,
        block: BlockId,
    },
}

/// The values of the positions read on the current path.
#[derive(Clone)]
struct Env {
    vals: Vec<Option<Local>>,
    /// A list position's length: its local and, inside a length case, its
    /// known value.
    lens: Vec<Option<(Local, Option<u64>)>>,
}

impl BodyCx<'_, '_> {
    /// Lowers `decision` over the subjects `subjects` (locals in their
    /// types' representations). Returns `None` when no path goes on after
    /// the match (every arm diverged); otherwise the current block is the
    /// one after it, with the names of a `Continue` arm bound, and the
    /// value is the `case`'s.
    pub(super) fn emit_decision(
        &mut self,
        decision: &Decision,
        subjects: &[Local],
        arms: Vec<ArmPlan>,
        on_fail: OnFail,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let mut env = Env {
            vals: vec![None; decision.occs.len()],
            lens: vec![None; decision.occs.len()],
        };
        for (slot, &l) in env.vals.iter_mut().zip(subjects) {
            *slot = Some(l);
        }
        let leaves = decision.tree.leaf_counts(arms.len());
        let mut m = Match {
            decision,
            shared: arms.iter().map(|_| None).collect(),
            arms,
            leaves,
            inline_continue: None,
            on_fail,
            fail_block: None,
            join: None,
            span,
        };
        self.emit_tree(&mut m, &decision.tree, env)?;
        // Shared arm bodies, in arm order.
        for arm in 0..m.arms.len() {
            let Some((block, params)) = m.shared.get(arm).cloned().flatten() else {
                continue;
            };
            self.switch_to(block)?;
            let locals = m
                .arms
                .get(arm)
                .map(|a| a.locals.clone())
                .unwrap_or_default();
            for (&id, &param) in locals.iter().zip(&params) {
                self.bind_local(id, Operand::Local(param))?;
            }
            // The shared arm's code takes the arm's position (D-938).
            let span = m.arms.get(arm).and_then(|a| a.span);
            let outer = match span {
                Some(span) => Some(self.enter_position(span)?),
                None => None,
            };
            self.finish_arm(&mut m, arm)?;
            if let Some(outer) = outer {
                self.leave_position(outer)?;
            }
        }
        if let Some(block) = m.fail_block {
            self.switch_to(block)?;
            self.emit_fail_panic(m.on_fail)?;
        }
        if let Some(block) = m.inline_continue {
            self.switch_to(block)?;
            return Ok(Some(Operand::Const(Const::Nil)));
        }
        if let Some((block, value)) = m.join {
            self.switch_to(block)?;
            return Ok(Some(Operand::Local(value)));
        }
        Ok(None)
    }

    /// Lowers `tree`, depth first, on an explicit stack ([`Emit`]), so a
    /// deep tree (a long list pattern) needs no deep Rust stack (D-601).
    /// Siblings are lowered in order, each subtree completely before the
    /// next, as a recursive walk would.
    fn emit_tree<'t>(&mut self, m: &mut Match<'t>, tree: &'t Tree, env: Env) -> Result<(), Ice> {
        let mut work = vec![Emit::Tree {
            tree,
            env,
            block: None,
        }];
        while let Some(item) = work.pop() {
            match item {
                Emit::Tree { tree, env, block } => {
                    if let Some(block) = block {
                        self.switch_to(block)?;
                    }
                    match tree {
                        Tree::Fail => self.emit_fail(m)?,
                        Tree::Leaf {
                            arm,
                            bindings,
                            otherwise,
                        } => {
                            self.emit_leaf(
                                m,
                                *arm,
                                bindings,
                                otherwise.as_deref(),
                                env,
                                &mut work,
                            )?;
                        }
                        Tree::Test {
                            occ,
                            test,
                            cases,
                            default,
                        } => {
                            self.emit_test(
                                m,
                                *occ,
                                test,
                                cases,
                                default.as_deref(),
                                env,
                                &mut work,
                            )?;
                        }
                    }
                }
                Emit::Str {
                    value,
                    cases,
                    default,
                    env,
                    block,
                } => {
                    self.switch_to(block)?;
                    self.emit_str_case(value, cases, default, env, &mut work)?;
                }
            }
        }
        Ok(())
    }

    /// Ends the current block where no arm matches.
    fn emit_fail(&mut self, m: &mut Match<'_>) -> Result<(), Ice> {
        match m.on_fail {
            OnFail::Unreachable => self.terminate(Terminator::Unreachable),
            OnFail::LetAssert { .. } => {
                let block = if let Some(b) = m.fail_block {
                    b
                } else {
                    let b = self.new_block()?;
                    m.fail_block = Some(b);
                    b
                };
                self.goto(block, Vec::new())
            }
        }
    }

    /// Lowers a leaf; the tree taken when its guard is `False` goes on
    /// `work`. Its code takes the arm's position (D-938).
    fn emit_leaf<'t>(
        &mut self,
        m: &mut Match<'t>,
        arm: usize,
        bindings: &[(LocalId, OccId)],
        otherwise: Option<&'t Tree>,
        env: Env,
        work: &mut Vec<Emit<'t>>,
    ) -> Result<(), Ice> {
        let span = m.arms.get(arm).and_then(|a| a.span);
        let outer = match span {
            Some(span) => Some(self.enter_position(span)?),
            None => None,
        };
        let done = self.emit_leaf_here(m, arm, bindings, otherwise, env, work);
        if let Some(outer) = outer {
            self.leave_position(outer)?;
        }
        done
    }

    /// [`BodyCx::emit_leaf`] at the current position.
    fn emit_leaf_here<'t>(
        &mut self,
        m: &mut Match<'t>,
        arm: usize,
        bindings: &[(LocalId, OccId)],
        otherwise: Option<&'t Tree>,
        mut env: Env,
        work: &mut Vec<Emit<'t>>,
    ) -> Result<(), Ice> {
        let plan = m
            .arms
            .get(arm)
            .ok_or_else(|| Ice::new("a leaf for an unknown arm"))?;
        let (guard, locals) = (plan.guard, plan.locals.clone());
        self.read_bound(m, bindings, &locals, &mut env)?;
        let mut values = Vec::with_capacity(locals.len());
        for id in &locals {
            let occ = bindings
                .iter()
                .find(|(l, _)| l == id)
                .map(|&(_, o)| o)
                .ok_or_else(|| Ice::new("an alternative that does not bind every name"))?;
            let v = self.occ_value(m, occ, &mut env)?;
            let occ_ty = m.decision.occ(occ)?.ty;
            let ty = self.local_ty(*id)?;
            let v = self.to_repr(
                Operand::Local(v),
                self.repr_of(occ_ty),
                self.repr_of(ty),
                ty,
            )?;
            values.push(v);
        }
        let inline = m.leaves.get(arm).copied().unwrap_or(0) == 1;
        if let Some(guard) = guard {
            for (&id, v) in locals.iter().zip(&values) {
                self.bind_local(id, v.clone())?;
            }
            let Some(cond) = self.expr_as(guard, Repr::Bool)? else {
                return Ok(());
            };
            let (yes, no) = (self.new_block()?, self.new_block()?);
            self.terminate(Terminator::Branch {
                cond,
                then_: yes,
                else_: no,
            })?;
            self.switch_to(yes)?;
            if inline {
                self.finish_arm(m, arm)?;
            } else {
                let values = locals
                    .iter()
                    .map(|&id| self.lookup(id))
                    .collect::<Result<Vec<_>, Ice>>()?;
                self.jump_to_arm(m, arm, values)?;
            }
            if let Some(tree) = otherwise {
                work.push(Emit::Tree {
                    tree,
                    env,
                    block: Some(no),
                });
                return Ok(());
            }
            self.switch_to(no)?;
            return self.emit_fail(m);
        }
        if inline {
            for (&id, v) in locals.iter().zip(values) {
                self.bind_local(id, v)?;
            }
            self.finish_arm(m, arm)
        } else {
            self.jump_to_arm(m, arm, values)
        }
    }

    /// Ends the current block with a jump to the shared block of `arm`.
    fn jump_to_arm(
        &mut self,
        m: &mut Match<'_>,
        arm: usize,
        values: Vec<Operand>,
    ) -> Result<(), Ice> {
        let slot = m
            .shared
            .get_mut(arm)
            .ok_or_else(|| Ice::new("a jump to an unknown arm"))?;
        let block = if let Some((b, _)) = slot {
            *b
        } else {
            let locals = m
                .arms
                .get(arm)
                .map(|a| a.locals.clone())
                .unwrap_or_default();
            let b = self.new_block()?;
            let mut params = Vec::with_capacity(locals.len());
            for id in locals {
                let ty = self.local_ty(id)?;
                params.push(self.block_param(b, self.repr_of(ty))?);
            }
            if let Some(slot) = m.shared.get_mut(arm) {
                *slot = Some((b, params));
            }
            b
        };
        self.goto(block, values)
    }

    /// Lowers what `arm` does once its names are bound in the current
    /// block.
    fn finish_arm(&mut self, m: &mut Match<'_>, arm: usize) -> Result<(), Ice> {
        let then = m
            .arms
            .get(arm)
            .map(|a| a.then)
            .ok_or_else(|| Ice::new("an unknown arm"))?;
        match then {
            Then::Continue => {
                if m.inline_continue.is_some() {
                    return Err(Ice::new("a `let` pattern continued twice"));
                }
                m.inline_continue = Some(self.b.current());
                Ok(())
            }
            Then::Body { body, tail, ty } => {
                let Some(value) = self.expr(body, tail)? else {
                    return Ok(());
                };
                let body_ty = self.ty(body)?;
                if matches!(self.cx.defs.globals.types.kind(body_ty), TyKind::Never) {
                    // A call of a function returning `Never` (§3.10).
                    return self.terminate(Terminator::Unreachable);
                }
                let repr = self.repr_of(ty);
                let value = self.to_repr(value, self.repr_of(body_ty), repr, ty)?;
                let join = if let Some((b, _)) = m.join {
                    b
                } else {
                    let b = self.new_block()?;
                    let p = self.block_param(b, repr)?;
                    m.join = Some((b, p));
                    b
                };
                self.goto(join, vec![value])
            }
        }
    }

    /// Lowers a test; its subtrees go on `work`, the first case on top.
    #[expect(
        clippy::too_many_arguments,
        reason = "the parts of a `Tree::Test`, the state and the work stack"
    )]
    fn emit_test<'t>(
        &mut self,
        m: &mut Match<'t>,
        occ: OccId,
        test: &'t Test,
        cases: &'t [(Key, Tree)],
        default: Option<&'t Tree>,
        mut env: Env,
        work: &mut Vec<Emit<'t>>,
    ) -> Result<(), Ice> {
        let value = self.occ_value(m, occ, &mut env)?;
        // When the cases cover every value, the last one is the default.
        let (cases, default) = match (default, cases.split_last()) {
            (Some(d), _) => (cases, d),
            (None, Some(((_, last), rest))) => (rest, last),
            (None, None) => return self.emit_fail(m),
        };
        match test {
            Test::Bool => {
                let (yes, no) = (self.new_block()?, self.new_block()?);
                self.terminate(Terminator::Branch {
                    cond: Operand::Local(value),
                    then_: yes,
                    else_: no,
                })?;
                let pick = |want: bool| {
                    cases
                        .iter()
                        .find(|(k, _)| *k == Key::Bool(want))
                        .map_or(default, |(_, t)| t)
                };
                let (t, f) = (pick(true), pick(false));
                work.push(Emit::Tree {
                    tree: f,
                    env: env.clone(),
                    block: Some(no),
                });
                work.push(Emit::Tree {
                    tree: t,
                    env,
                    block: Some(yes),
                });
                Ok(())
            }
            Test::Str => self.emit_str_case(value, cases, default, env, work),
            Test::Tag(_) | Test::Int(_) | Test::Len => {
                let (on, len) = match test {
                    Test::Tag(ty) => {
                        let tag = self.define(
                            Repr::Int,
                            Rvalue::ReadTag {
                                base: Operand::Local(value),
                                ty: *ty,
                            },
                        )?;
                        (tag, None)
                    }
                    Test::Len => {
                        let len = self.list_len(occ, value, &mut env)?;
                        (len, Some(len))
                    }
                    _ => (value, None),
                };
                let repr = match test {
                    Test::Int(r) => *r,
                    _ => Repr::Int,
                };
                let mut blocks = Vec::with_capacity(cases.len());
                let mut targets = Vec::with_capacity(cases.len());
                for (key, _) in cases {
                    let block = self.new_block()?;
                    blocks.push(block);
                    match key {
                        Key::Ctor { tag, .. } => targets.push((u64::from(*tag), block)),
                        Key::Len(n) => targets.push((*n, block)),
                        Key::Lens(ns) => targets.extend(ns.iter().map(|&n| (n, block))),
                        Key::Int(v) => targets.push((int_bits(*v, repr)?, block)),
                        Key::Bool(_) | Key::Str(_) => {
                            return Err(Ice::new("a switch with a non-integer key"));
                        }
                    }
                }
                let otherwise = self.new_block()?;
                self.terminate(Terminator::Switch {
                    on: Operand::Local(on),
                    cases: targets,
                    default: Some(otherwise),
                })?;
                // Pushed in reverse: the first case is lowered first, the
                // default last.
                let mut items = Vec::with_capacity(cases.len().saturating_add(1));
                for ((key, sub), block) in cases.iter().zip(blocks) {
                    let mut env = env.clone();
                    if let (Some(len), Key::Len(n)) = (len, key)
                        && let Some(slot) = env.lens.get_mut(occ.index())
                    {
                        *slot = Some((len, Some(*n)));
                    }
                    items.push(Emit::Tree {
                        tree: sub,
                        env,
                        block: Some(block),
                    });
                }
                work.push(Emit::Tree {
                    tree: default,
                    env,
                    block: Some(otherwise),
                });
                work.extend(items.into_iter().rev());
                Ok(())
            }
        }
    }

    /// Compares the `String` `value` with the first literal of `cases` in
    /// the current block: equal, its subtree goes on; different, the next
    /// comparison, or `default` after the last one. Both go on `work`.
    fn emit_str_case<'t>(
        &mut self,
        value: Local,
        cases: &'t [(Key, Tree)],
        default: &'t Tree,
        env: Env,
        work: &mut Vec<Emit<'t>>,
    ) -> Result<(), Ice> {
        let Some(((key, sub), rest)) = cases.split_first() else {
            work.push(Emit::Tree {
                tree: default,
                env,
                block: None,
            });
            return Ok(());
        };
        let Key::Str(literal) = key else {
            return Err(Ice::new("a string test with another key"));
        };
        let lit = self.string_const(literal)?;
        let eq = self.define(
            Repr::Bool,
            Rvalue::CallRuntime {
                func: RuntimeFn::StrEq,
                args: vec![Operand::Local(value), lit],
            },
        )?;
        let (yes, no) = (self.new_block()?, self.new_block()?);
        self.terminate(Terminator::Branch {
            cond: Operand::Local(eq),
            then_: yes,
            else_: no,
        })?;
        work.push(Emit::Str {
            value,
            cases: rest,
            default,
            env: env.clone(),
            block: no,
        });
        work.push(Emit::Tree {
            tree: sub,
            env,
            block: Some(yes),
        });
        Ok(())
    }

    /// The length of the list at `occ` (whose value is `list`), read once
    /// per path.
    fn list_len(&mut self, occ: OccId, list: Local, env: &mut Env) -> Result<Local, Ice> {
        if let Some(Some((len, _))) = env.lens.get(occ.index()) {
            return Ok(*len);
        }
        let len = self.define(
            Repr::Int,
            Rvalue::CallRuntime {
                func: RuntimeFn::ListLength,
                args: vec![Operand::Local(list)],
            },
        )?;
        if let Some(slot) = env.lens.get_mut(occ.index()) {
            *slot = Some((len, None));
        }
        Ok(len)
    }

    /// `len - n` as an operand: a constant when the length is known.
    fn len_minus(
        &mut self,
        occ: OccId,
        list: Local,
        n: u32,
        env: &mut Env,
        span: Span,
    ) -> Result<Operand, Ice> {
        if let Some(Some((_, Some(known)))) = env.lens.get(occ.index()) {
            let v = known
                .checked_sub(u64::from(n))
                .and_then(|v| i64::try_from(v).ok())
                .ok_or_else(|| Ice::new("a list index outside its known length"))?;
            return Ok(Operand::Const(Const::Int(v)));
        }
        let len = self.list_len(occ, list, env)?;
        if n == 0 {
            return Ok(Operand::Local(len));
        }
        let loc = self.location_id(span)?;
        let at = self.define(
            Repr::Int,
            Rvalue::Checked {
                op: ArithOp::Sub,
                lhs: Operand::Local(len),
                rhs: Operand::Const(Const::Int(i64::from(n))),
                repr: Repr::Int,
                loc,
            },
        )?;
        Ok(Operand::Local(at))
    }

    /// Reads the positions that `locals`, an arm's names, are bound to
    /// (`bindings`), and their parents, in the current block, level by
    /// level from the subjects down (D-908). Within a level every read (a
    /// field, a tuple element or a list element) comes before any spread,
    /// so a spread is its list's last read: a list that dies at the match
    /// is cut in place (D-634) and the elements cut off it are released
    /// there (D-893). A matched element's own fields are read before a
    /// list among them is cut, so that list is unique when the element
    /// was. Inside one pattern the order has no meaning in the source: the
    /// match is the subject's last use (D-205).
    ///
    /// An element next to its list's spread, `x` in `[x, ..rest]` or
    /// `[..rest, x]`, is read with the spread in one `list_uncons`
    /// ([`Self::read_uncons`], D-909), which moves it out of a unique list.
    fn read_bound(
        &mut self,
        m: &Match<'_>,
        bindings: &[(LocalId, OccId)],
        locals: &[LocalId],
        env: &mut Env,
    ) -> Result<(), Ice> {
        // Each position to read, once, with its depth and whether it is a
        // spread; parents before their children.
        let mut todo: Vec<(usize, bool, OccId)> = Vec::new();
        let mut queued = vec![false; m.decision.occs.len()];
        for id in locals {
            let occ = bindings
                .iter()
                .find(|(l, _)| l == id)
                .map(|&(_, o)| o)
                .ok_or_else(|| Ice::new("an alternative that does not bind every name"))?;
            let mut chain = Vec::new();
            let mut cur = Some(occ);
            while let Some(o) = cur {
                if queued.get(o.index()).copied().unwrap_or(true) {
                    break;
                }
                if let Some(q) = queued.get_mut(o.index()) {
                    *q = true;
                }
                chain.push(o);
                cur = m.decision.occ(o)?.parent;
            }
            for o in chain.into_iter().rev() {
                if env.vals.get(o.index()).copied().flatten().is_some() {
                    continue;
                }
                let data = m.decision.occ(o)?;
                let mut depth = 0_usize;
                let mut up = data.parent;
                while let Some(p) = up {
                    depth = depth.saturating_add(1);
                    up = m.decision.occ(p)?.parent;
                }
                todo.push((depth, matches!(data.step, Step::Slice { .. }), o));
            }
        }
        // The unread elements next to a spread: the element before it, or
        // the one after it.
        let mut elems: FxHashMap<(OccId, Step), OccId> = FxHashMap::default();
        for &(_, slice, o) in &todo {
            let data = m.decision.occ(o)?;
            if let (false, Some(parent), Step::Index(_) | Step::FromEnd(_)) =
                (slice, data.parent, data.step)
            {
                elems.insert((parent, data.step), o);
            }
        }
        let mut pairs: FxHashMap<OccId, OccId> = FxHashMap::default();
        let mut fused = vec![false; m.decision.occs.len()];
        for &(_, slice, o) in &todo {
            let data = m.decision.occ(o)?;
            let (true, Some(parent), Step::Slice { start, from_end }) =
                (slice, data.parent, data.step)
            else {
                continue;
            };
            let before = start.checked_sub(1).map(Step::Index);
            let after = (from_end > 0).then_some(Step::FromEnd(from_end));
            let Some(&elem) = [before, after]
                .into_iter()
                .flatten()
                .find_map(|step| elems.get(&(parent, step)))
            else {
                continue;
            };
            pairs.insert(o, elem);
            if let Some(f) = fused.get_mut(elem.index()) {
                *f = true;
            }
        }
        todo.sort_by_key(|&(depth, slice, _)| (depth, slice));
        for (_, _, o) in todo {
            if fused.get(o.index()).copied().unwrap_or(false) {
                continue;
            }
            match pairs.get(&o) {
                Some(&elem) => self.read_uncons(m, o, elem, env)?,
                None => {
                    self.occ_value(m, o, env)?;
                }
            }
        }
        Ok(())
    }

    /// Reads the spread at `slice` and the element at `elem` next to it in
    /// one step (D-909): `rest = list_uncons(list, at, start, end)` and,
    /// right after it, `elem = list_uncons_head()`. The RC pass makes it
    /// `list_uncons_owned` where the list dies (D-634).
    fn read_uncons(
        &mut self,
        m: &Match<'_>,
        slice: OccId,
        elem: OccId,
        env: &mut Env,
    ) -> Result<(), Ice> {
        let s = m.decision.occ(slice)?.clone();
        let e = m.decision.occ(elem)?.clone();
        let (Step::Slice { start, from_end }, Some(parent)) = (s.step, s.parent) else {
            return Err(Ice::new("an uncons without its spread"));
        };
        if e.parent != Some(parent) {
            return Err(Ice::new("an uncons of an element of another list"));
        }
        let list = self.occ_value(m, parent, env)?;
        let end = self.len_minus(parent, list, from_end, env, m.span)?;
        let at = match e.step {
            Step::Index(i) => Operand::Const(Const::Int(i64::from(i))),
            // `len - from_end`, the spread's end.
            Step::FromEnd(_) => end.clone(),
            _ => return Err(Ice::new("an uncons of a position that is not an element")),
        };
        let rest = self.define(
            Repr::Value,
            Rvalue::CallRuntime {
                func: RuntimeFn::ListUncons,
                args: vec![
                    Operand::Local(list),
                    at,
                    Operand::Const(Const::Int(i64::from(start))),
                    end,
                ],
            },
        )?;
        let head = self.define(
            Repr::Value,
            Rvalue::CallRuntime {
                func: RuntimeFn::ListUnconsHead,
                args: Vec::new(),
            },
        )?;
        for (occ, ty, read) in [(elem, e.ty, head), (slice, s.ty, rest)] {
            let want = self.repr_of(ty);
            let value = self.to_repr(Operand::Local(read), Repr::Value, want, ty)?;
            let local = self.as_local(value, want)?;
            if let Some(slot) = env.vals.get_mut(occ.index()) {
                *slot = Some(local);
            }
        }
        Ok(())
    }

    /// The value at `occ` on this path, in its type's representation,
    /// reading it (and its parents) in the current block if needed.
    fn occ_value(&mut self, m: &Match<'_>, occ: OccId, env: &mut Env) -> Result<Local, Ice> {
        if let Some(Some(l)) = env.vals.get(occ.index()) {
            return Ok(*l);
        }
        let data = m.decision.occ(occ)?.clone();
        let parent = data
            .parent
            .ok_or_else(|| Ice::new("a subject without its value"))?;
        let base = self.occ_value(m, parent, env)?;
        let want = self.repr_of(data.ty);
        let (read, repr) = match data.step {
            Step::Subject(_) => return Err(Ice::new("a subject below another position")),
            Step::Field { ctor, field } => {
                let layout = ctor_layout(ctor, &self.cx.defs)?;
                let repr = usize::try_from(field)
                    .ok()
                    .and_then(|i| layout.fields.get(i))
                    .map(|f| f.repr)
                    .ok_or_else(|| Ice::new("a field pattern outside its constructor"))?;
                let l = self.define(
                    repr,
                    Rvalue::Field {
                        base,
                        layout: LayoutRef::Ctor(ctor),
                        field,
                    },
                )?;
                (l, repr)
            }
            Step::Tuple { arity, index } => {
                let l = self.define(
                    Repr::Value,
                    Rvalue::Field {
                        base,
                        layout: LayoutRef::Tuple(arity),
                        field: index,
                    },
                )?;
                (l, Repr::Value)
            }
            Step::Index(i) => {
                let l = self.list_get(base, Operand::Const(Const::Int(i64::from(i))))?;
                (l, Repr::Value)
            }
            Step::FromEnd(n) => {
                let at = self.len_minus(parent, base, n, env, m.span)?;
                let l = self.list_get(base, at)?;
                (l, Repr::Value)
            }
            Step::Slice { start, from_end } => {
                let end = self.len_minus(parent, base, from_end, env, m.span)?;
                let l = self.define(
                    Repr::Value,
                    Rvalue::CallRuntime {
                        func: RuntimeFn::ListSlice,
                        args: vec![
                            Operand::Local(base),
                            Operand::Const(Const::Int(i64::from(start))),
                            end,
                        ],
                    },
                )?;
                (l, Repr::Value)
            }
        };
        let value = self.to_repr(Operand::Local(read), repr, want, data.ty)?;
        let local = self.as_local(value, want)?;
        if let Some(slot) = env.vals.get_mut(occ.index()) {
            *slot = Some(local);
        }
        Ok(local)
    }

    fn list_get(&mut self, list: Local, index: Operand) -> Result<Local, Ice> {
        self.define(
            Repr::Value,
            Rvalue::CallRuntime {
                func: RuntimeFn::ListGet,
                args: vec![Operand::Local(list), index],
            },
        )
    }

    /// `op` as a local of `repr` (constants are moved into one).
    pub(super) fn as_local(&mut self, op: Operand, repr: Repr) -> Result<Local, Ice> {
        match op {
            Operand::Local(l) => Ok(l),
            op @ Operand::Const(_) => self.define(repr, Rvalue::Use(op)),
        }
    }

    /// The type of the THIR local `id`.
    pub(super) fn local_ty(&self, id: LocalId) -> Result<Ty, Ice> {
        self.body
            .local(id)
            .map(|d| d.ty)
            .ok_or_else(|| Ice::new("a pattern name without its local"))
    }

    /// Binds the THIR local `id` to `op` (in its type's representation).
    pub(super) fn bind_local(&mut self, id: LocalId, op: Operand) -> Result<(), Ice> {
        let decl = self
            .body
            .local(id)
            .ok_or_else(|| Ice::new("a pattern name without its local"))?;
        let (name, ty) = (decl.name, decl.ty);
        self.bind_name(id, name, ty, op)
    }

    /// The panic of a failed `let assert` (§11.4, D-528):
    /// `` `let assert` pattern did not match[: m] `` and
    /// `  value: <debug text>`.
    fn emit_fail_panic(&mut self, on_fail: OnFail) -> Result<(), Ice> {
        let OnFail::LetAssert {
            value,
            ty,
            message,
            at,
        } = on_fail
        else {
            return self.terminate(Terminator::Unreachable);
        };
        let mut acc = self.define(
            Repr::Value,
            Rvalue::CallRuntime {
                func: RuntimeFn::StrWithCapacity,
                args: vec![Operand::Const(Const::Int(64))],
            },
        )?;
        match message {
            Some(m) => {
                acc = self.append_str(acc, &format!("{LET_ASSERT}: "))?;
                let Some(text) = self.expr_as(m, Repr::Value)? else {
                    return Ok(());
                };
                acc = self.append_op(acc, text)?;
                acc = self.append_str(acc, "\n  value: ")?;
            }
            None => acc = self.append_str(acc, &format!("{LET_ASSERT}\n  value: "))?,
        }
        let uniform = self.to_uniform(Operand::Local(value), ty)?;
        let debug = self.define(
            Repr::Value,
            Rvalue::CallRuntime {
                func: RuntimeFn::Debug,
                args: vec![uniform],
            },
        )?;
        acc = self.append_op(acc, Operand::Local(debug))?;
        let loc = self.location_id(at)?;
        self.terminate(Terminator::Panic {
            kind: PanicKind::LetAssert,
            message: Some(Operand::Local(acc)),
            loc,
        })
    }

    fn append_op(&mut self, acc: Local, piece: Operand) -> Result<Local, Ice> {
        self.define(
            Repr::Value,
            Rvalue::CallRuntime {
                func: RuntimeFn::StrAppend,
                args: vec![Operand::Local(acc), piece],
            },
        )
    }

    fn append_str(&mut self, acc: Local, text: &str) -> Result<Local, Ice> {
        let piece = self.string_const(text)?;
        self.append_op(acc, piece)
    }
}

/// The switch bits of the integer literal `v` in `repr` (plan §4.1): the
/// two's-complement value truncated to the representation's width.
fn int_bits(v: i128, repr: Repr) -> Result<u64, Ice> {
    let width: u32 = match repr {
        Repr::I8 | Repr::U8 => 8,
        Repr::I16 | Repr::U16 => 16,
        Repr::I32 | Repr::U32 => 32,
        Repr::Int | Repr::I64 | Repr::U64 => 64,
        _ => return Err(Ice::new(format!("an integer literal pattern of {repr}"))),
    };
    let bits = if repr.is_unsigned_int() {
        u64::try_from(v).ok()
    } else {
        i64::try_from(v).ok().map(i64::cast_unsigned)
    }
    .ok_or_else(|| Ice::new("an integer literal pattern outside its type"))?;
    Ok(if width >= 64 {
        bits
    } else {
        bits & 1_u64
            .checked_shl(width)
            .map_or(u64::MAX, |b| b.wrapping_sub(1))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_bits_truncate_to_the_width() {
        assert_eq!(int_bits(-1, Repr::I8).unwrap(), 0xff);
        assert_eq!(int_bits(-1, Repr::Int).unwrap(), u64::MAX);
        assert_eq!(int_bits(200, Repr::U8).unwrap(), 200);
        assert_eq!(int_bits(-2, Repr::I32).unwrap(), 0xffff_fffe);
        assert_eq!(int_bits(i128::from(u64::MAX), Repr::U64).unwrap(), u64::MAX);
        assert!(int_bits(-1, Repr::U8).is_err());
    }
}
