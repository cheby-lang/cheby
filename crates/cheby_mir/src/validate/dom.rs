//! Dominance for the validator.
//!
//! Immediate dominators come from Cooper, Harvey and Kennedy's iterative
//! algorithm over reverse postorder ("A Simple, Fast Dominance
//! Algorithm"). Pre- and post-order numbers on the dominator tree then
//! answer "does `a` dominate `b`" in constant time. The whole computation
//! is near-linear in the number of blocks and edges, so a function with
//! tens of thousands of blocks (a large `case`) validates quickly. The
//! earlier per-block dominator sets cost O(B^2).

use cheby_iface::{Ice, Idx};

use crate::ir::{BlockId, Function};

/// No number yet: an unreachable block, or an immediate dominator not
/// computed yet.
const NONE: u32 = u32::MAX;

/// Which block dominates which, for the blocks reachable from `bb0`.
#[derive(Debug)]
pub(super) struct Dominance {
    /// Each block's reverse-postorder number, by block index; `NONE` for
    /// an unreachable block.
    rpo: Vec<u32>,
    /// Each reachable block's pre-order number on the dominator tree, by
    /// reverse-postorder number.
    pre: Vec<u32>,
    /// Each reachable block's post-order number on the dominator tree, by
    /// reverse-postorder number.
    post: Vec<u32>,
}

impl Dominance {
    /// The dominance of `f`'s blocks.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a terminator names a block `f` does not have.
    pub(super) fn new(f: &Function) -> Result<Self, Ice> {
        let n = f.blocks.len();
        let mut succs = Vec::with_capacity(n);
        for (id, block) in f.blocks.iter_enumerated() {
            let mut out = Vec::new();
            for s in block.term.successors() {
                if s.index() >= n {
                    return Err(Ice::new(format!(
                        "bb{} jumps to unknown bb{}",
                        id.as_u32(),
                        s.as_u32()
                    )));
                }
                out.push(s.index());
            }
            succs.push(out);
        }
        Self::from_successors(&succs)
    }

    /// The dominance of the graph whose block `i` jumps to `succs[i]`,
    /// entered at block 0.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a successor is out of range or the graph is too
    /// large to number.
    pub(super) fn from_successors(succs: &[Vec<usize>]) -> Result<Self, Ice> {
        let n = succs.len();
        let rpo_order = reverse_postorder(succs)?;
        let mut rpo = vec![NONE; n];
        for (i, &b) in rpo_order.iter().enumerate() {
            *rpo.get_mut(b).ok_or_else(|| bad_block(b))? = to_u32(i)?;
        }
        let m = rpo_order.len();
        // Predecessors by reverse-postorder number.
        let mut preds: Vec<Vec<u32>> = vec![Vec::new(); m];
        for (i, &b) in rpo_order.iter().enumerate() {
            for &s in succs.get(b).ok_or_else(|| bad_block(b))? {
                let s_rpo = *rpo.get(s).ok_or_else(|| bad_block(s))?;
                let p = preds
                    .get_mut(usize_of(s_rpo)?)
                    .ok_or_else(|| bad_block(s))?;
                p.push(to_u32(i)?);
            }
        }
        let idom = immediate_dominators(&preds)?;
        let (pre, post) = tree_numbers(&idom)?;
        Ok(Self { rpo, pre, post })
    }

    /// Whether `b` is reachable from the entry block.
    pub(super) fn is_reachable(&self, b: BlockId) -> bool {
        self.rpo.get(b.index()).is_some_and(|&r| r != NONE)
    }

    /// Whether `a` dominates `b` (every block dominates itself). False when
    /// either block is unreachable.
    pub(super) fn dominates(&self, a: BlockId, b: BlockId) -> bool {
        let (Some(a), Some(b)) = (self.number(a), self.number(b)) else {
            return false;
        };
        let (Some(&pre_a), Some(&pre_b), Some(&post_a), Some(&post_b)) = (
            self.pre.get(a),
            self.pre.get(b),
            self.post.get(a),
            self.post.get(b),
        ) else {
            return false;
        };
        pre_a <= pre_b && post_b <= post_a
    }

    fn number(&self, b: BlockId) -> Option<usize> {
        let &r = self.rpo.get(b.index())?;
        if r == NONE {
            None
        } else {
            usize::try_from(r).ok()
        }
    }
}

fn bad_block(b: usize) -> Ice {
    Ice::new(format!("dominance: no block {b}"))
}

fn to_u32(n: usize) -> Result<u32, Ice> {
    u32::try_from(n)
        .ok()
        .filter(|&v| v != NONE)
        .ok_or_else(|| Ice::new("dominance: too many blocks"))
}

fn usize_of(n: u32) -> Result<usize, Ice> {
    usize::try_from(n).map_err(|_| Ice::new("dominance: block number does not fit"))
}

/// The blocks reachable from block 0, in reverse postorder. Uses an
/// explicit stack, so a long chain of blocks cannot overflow the Rust
/// stack.
fn reverse_postorder(succs: &[Vec<usize>]) -> Result<Vec<usize>, Ice> {
    let mut order = Vec::new();
    if succs.is_empty() {
        return Ok(order);
    }
    let mut visited = vec![false; succs.len()];
    let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
    if let Some(v) = visited.first_mut() {
        *v = true;
    }
    while let Some(&(b, next)) = stack.last() {
        let out = succs.get(b).ok_or_else(|| bad_block(b))?;
        if let Some(&s) = out.get(next) {
            if let Some(top) = stack.last_mut() {
                top.1 = next.saturating_add(1);
            }
            let seen = visited.get_mut(s).ok_or_else(|| bad_block(s))?;
            if !*seen {
                *seen = true;
                stack.push((s, 0));
            }
        } else {
            stack.pop();
            order.push(b);
        }
    }
    order.reverse();
    Ok(order)
}

/// Each block's immediate dominator, by reverse-postorder number; the
/// entry (number 0) is its own.
fn immediate_dominators(preds: &[Vec<u32>]) -> Result<Vec<u32>, Ice> {
    let mut idom = vec![NONE; preds.len()];
    if let Some(entry) = idom.first_mut() {
        *entry = 0;
    }
    let mut changed = true;
    while changed {
        changed = false;
        for (b, ps) in preds.iter().enumerate().skip(1) {
            let mut new = NONE;
            for &p in ps {
                if *idom.get(usize_of(p)?).ok_or_else(|| bad_block(b))? == NONE {
                    continue;
                }
                new = if new == NONE {
                    p
                } else {
                    intersect(&idom, p, new)?
                };
            }
            let slot = idom.get_mut(b).ok_or_else(|| bad_block(b))?;
            if new != NONE && *slot != new {
                *slot = new;
                changed = true;
            }
        }
    }
    Ok(idom)
}

/// The nearest common dominator of `a` and `b`, walking up `idom`.
fn intersect(idom: &[u32], mut a: u32, mut b: u32) -> Result<u32, Ice> {
    let up = |x: u32| -> Result<u32, Ice> {
        let &d = idom
            .get(usize_of(x)?)
            .ok_or_else(|| Ice::new("dominance: no idom"))?;
        // A processed block's immediate dominator comes before it in
        // reverse postorder; anything else would loop forever.
        if d >= x {
            return Err(Ice::new("dominance: immediate dominators out of order"));
        }
        Ok(d)
    };
    while a != b {
        while a > b {
            a = up(a)?;
        }
        while b > a {
            b = up(b)?;
        }
    }
    Ok(a)
}

/// Pre- and post-order numbers of the dominator tree given by `idom`,
/// by reverse-postorder number.
fn tree_numbers(idom: &[u32]) -> Result<(Vec<u32>, Vec<u32>), Ice> {
    let m = idom.len();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); m];
    for (b, &d) in idom.iter().enumerate().skip(1) {
        if d == NONE {
            return Err(Ice::new("dominance: a reachable block without idom"));
        }
        children
            .get_mut(usize_of(d)?)
            .ok_or_else(|| bad_block(b))?
            .push(b);
    }
    let mut pre = vec![0; m];
    let mut post = vec![0; m];
    if m == 0 {
        return Ok((pre, post));
    }
    let mut clock: u32 = 0;
    let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
    *pre.first_mut().ok_or_else(|| bad_block(0))? = clock;
    while let Some(&(b, next)) = stack.last() {
        let kids = children.get(b).ok_or_else(|| bad_block(b))?;
        if let Some(&c) = kids.get(next) {
            if let Some(top) = stack.last_mut() {
                top.1 = next.saturating_add(1);
            }
            clock = clock.saturating_add(1);
            *pre.get_mut(c).ok_or_else(|| bad_block(c))? = clock;
            stack.push((c, 0));
        } else {
            stack.pop();
            clock = clock.saturating_add(1);
            *post.get_mut(b).ok_or_else(|| bad_block(b))? = clock;
        }
    }
    Ok((pre, post))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// The dominator sets by the textbook fixpoint, as a reference.
    fn reference(succs: &[Vec<usize>]) -> Vec<Option<BTreeSet<usize>>> {
        let n = succs.len();
        let mut reach = vec![false; n];
        let mut stack = if n == 0 { vec![] } else { vec![0] };
        while let Some(b) = stack.pop() {
            if !reach[b] {
                reach[b] = true;
                stack.extend(succs[b].iter().copied());
            }
        }
        let all: BTreeSet<usize> = (0..n).filter(|&b| reach[b]).collect();
        let mut doms: Vec<Option<BTreeSet<usize>>> = (0..n)
            .map(|b| {
                reach[b].then(|| {
                    if b == 0 {
                        BTreeSet::from([0])
                    } else {
                        all.clone()
                    }
                })
            })
            .collect();
        let mut changed = true;
        while changed {
            changed = false;
            for b in 1..n {
                if !reach[b] {
                    continue;
                }
                let mut new: Option<BTreeSet<usize>> = None;
                for p in 0..n {
                    if reach[p] && succs[p].contains(&b) {
                        let pd = doms[p].clone().unwrap();
                        new = Some(match new {
                            None => pd,
                            Some(acc) => acc.intersection(&pd).copied().collect(),
                        });
                    }
                }
                let mut new = new.unwrap_or_default();
                new.insert(b);
                if doms[b].as_ref() != Some(&new) {
                    doms[b] = Some(new);
                    changed = true;
                }
            }
        }
        doms
    }

    fn agrees(succs: &[Vec<usize>]) {
        let d = Dominance::from_successors(succs).unwrap();
        let want = reference(succs);
        for (b, want_b) in want.iter().enumerate() {
            let bb = BlockId::from_u32(u32::try_from(b).unwrap());
            assert_eq!(d.is_reachable(bb), want_b.is_some(), "reachability of {b}");
            for a in 0..succs.len() {
                let ab = BlockId::from_u32(u32::try_from(a).unwrap());
                let expected = want_b.as_ref().is_some_and(|s| s.contains(&a));
                assert_eq!(d.dominates(ab, bb), expected, "{a} dominates {b}");
            }
        }
    }

    #[test]
    fn matches_the_reference_on_small_graphs() {
        // A diamond, a loop with an exit, an irreducible loop, an
        // unreachable block, a self loop.
        agrees(&[vec![1, 2], vec![3], vec![3], vec![]]);
        agrees(&[vec![1], vec![2, 3], vec![1], vec![]]);
        agrees(&[vec![1, 2], vec![2], vec![1, 3], vec![]]);
        agrees(&[vec![1], vec![], vec![1]]);
        agrees(&[vec![0, 1], vec![1]]);
        agrees(&[]);
    }

    #[test]
    fn matches_the_reference_on_pseudo_random_graphs() {
        // A fixed linear congruential generator keeps the test
        // deterministic.
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move |bound: usize| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            usize::try_from(seed >> 33).unwrap() % bound
        };
        for _ in 0..300 {
            let n = 1 + next(12);
            let succs: Vec<Vec<usize>> = (0..n)
                .map(|_| (0..next(4)).map(|_| next(n)).collect())
                .collect();
            agrees(&succs);
        }
    }

    #[test]
    fn a_long_chain_of_diamonds_is_fast() {
        // 3 * 40_000 + 1 blocks: the shape a 40_000-arm `case` or a long
        // run of `let x = case …` produces. The old per-block sets needed
        // minutes for this; the budget is generous for a debug build.
        let diamonds = 40_000;
        let mut succs = Vec::new();
        for i in 0..diamonds {
            let top = 3 * i;
            succs.push(vec![top + 1, top + 2]);
            succs.push(vec![top + 3]);
            succs.push(vec![top + 3]);
        }
        succs.push(vec![]);
        let start = std::time::Instant::now();
        let d = Dominance::from_successors(&succs).unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        let last = BlockId::from_u32(u32::try_from(3 * diamonds).unwrap());
        assert!(d.dominates(BlockId::from_u32(0), last));
        assert!(d.dominates(BlockId::from_u32(3), last));
        assert!(!d.dominates(BlockId::from_u32(1), last));
    }
}
