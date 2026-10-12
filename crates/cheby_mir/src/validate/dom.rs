//! Dominance for the validator.
//!
//! Immediate dominators come from the SEMI-NCA algorithm (Georgiadis,
//! "Linear-Time Algorithms for Dominators and Related Problems"):
//! Lengauer and Tarjan's semidominators over a depth-first numbering, with
//! path compression, then a walk from each node's DFS parent up to its
//! semidominator. Pre- and post-order numbers on the dominator tree then
//! answer "does `a` dominate `b`" in constant time. The whole computation
//! is near-linear in the number of blocks and edges, so a function with
//! tens of thousands of blocks (a large `case`) validates quickly. The
//! earlier per-block dominator sets cost O(B^2), and Cooper, Harvey and
//! Kennedy's iterative algorithm, used before this one, cost O(B * depth)
//! when many segments of one long block unwind to a shared cleanup pad
//! (D-698): each segment's walk to the pad's dominator ran up the chain.

use cheby_iface::Ice;

/// No number yet: an unreachable block, or an immediate dominator not
/// computed yet.
const NONE: u32 = u32::MAX;

/// Which node dominates which, for the nodes reachable from node 0. The
/// validator's nodes are the blocks split at their unwind edges
/// ([`super::Segments`]).
#[derive(Debug)]
pub(super) struct Dominance {
    /// Each node's depth-first (preorder) number, by node index; `NONE`
    /// for an unreachable node.
    num: Vec<u32>,
    /// Each reachable node's pre-order number on the dominator tree, by
    /// depth-first number.
    pre: Vec<u32>,
    /// Each reachable node's post-order number on the dominator tree, by
    /// depth-first number.
    post: Vec<u32>,
}

impl Dominance {
    /// The dominance of the graph whose block `i` jumps to `succs[i]`,
    /// entered at block 0.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a successor is out of range or the graph is too
    /// large to number.
    pub(super) fn from_successors(succs: &[Vec<usize>]) -> Result<Self, Ice> {
        let n = succs.len();
        let (order, parent) = preorder(succs)?;
        let mut num = vec![NONE; n];
        for (i, &b) in order.iter().enumerate() {
            *num.get_mut(b).ok_or_else(|| bad_block(b))? = to_u32(i)?;
        }
        // Predecessors by depth-first number. Every successor of a
        // reachable node is reachable, so it has a number.
        let mut preds: Vec<Vec<u32>> = vec![Vec::new(); order.len()];
        for (i, &b) in order.iter().enumerate() {
            for &s in succs.get(b).ok_or_else(|| bad_block(b))? {
                let s_num = *num.get(s).ok_or_else(|| bad_block(s))?;
                let p = preds
                    .get_mut(usize_of(s_num)?)
                    .ok_or_else(|| bad_block(s))?;
                p.push(to_u32(i)?);
            }
        }
        let idom = immediate_dominators(&parent, &preds)?;
        let (pre, post) = tree_numbers(&idom)?;
        Ok(Self { num, pre, post })
    }

    /// Whether node `b` is reachable from node 0.
    pub(super) fn is_reachable(&self, b: usize) -> bool {
        self.num.get(b).is_some_and(|&r| r != NONE)
    }

    /// Whether node `a` dominates node `b` (every node dominates itself).
    /// False when either node is unreachable.
    pub(super) fn dominates(&self, a: usize, b: usize) -> bool {
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

    fn number(&self, b: usize) -> Option<usize> {
        let &r = self.num.get(b)?;
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

/// The blocks reachable from block 0 in depth-first preorder, and each
/// one's parent in the depth-first tree, by depth-first number (the entry,
/// number 0, is its own). Uses an explicit stack, so a long chain of
/// blocks cannot overflow the Rust stack.
fn preorder(succs: &[Vec<usize>]) -> Result<(Vec<usize>, Vec<u32>), Ice> {
    let mut order = Vec::new();
    let mut parent = Vec::new();
    if succs.is_empty() {
        return Ok((order, parent));
    }
    let mut visited = vec![false; succs.len()];
    if let Some(v) = visited.first_mut() {
        *v = true;
    }
    order.push(0);
    parent.push(0);
    // (block, its number, the next successor to look at)
    let mut stack: Vec<(usize, u32, usize)> = vec![(0, 0, 0)];
    while let Some(&(b, number, next)) = stack.last() {
        let out = succs.get(b).ok_or_else(|| bad_block(b))?;
        if let Some(&s) = out.get(next) {
            if let Some(top) = stack.last_mut() {
                top.2 = next.saturating_add(1);
            }
            let seen = visited.get_mut(s).ok_or_else(|| bad_block(s))?;
            if !*seen {
                *seen = true;
                let s_number = to_u32(order.len())?;
                order.push(s);
                parent.push(number);
                stack.push((s, s_number, 0));
            }
        } else {
            stack.pop();
        }
    }
    Ok((order, parent))
}

/// `v[i]`, or an [`Ice`].
fn at(v: &[u32], i: u32) -> Result<u32, Ice> {
    v.get(usize_of(i)?)
        .copied()
        .ok_or_else(|| Ice::new("dominance: a number out of range"))
}

/// `v[i] = x`, or an [`Ice`].
fn put(v: &mut [u32], i: u32, x: u32) -> Result<(), Ice> {
    *v.get_mut(usize_of(i)?)
        .ok_or_else(|| Ice::new("dominance: a number out of range"))? = x;
    Ok(())
}

/// Each block's immediate dominator, by depth-first number, given each
/// one's depth-first parent and predecessors; the entry (number 0) is its
/// own. SEMI-NCA: semidominators in decreasing number with Lengauer and
/// Tarjan's `eval` over a forest linked as nodes are done, then, in
/// increasing number, the nearest ancestor of the parent on the dominator
/// tree whose number is at most the semidominator's.
fn immediate_dominators(parent: &[u32], preds: &[Vec<u32>]) -> Result<Vec<u32>, Ice> {
    let m = preds.len();
    let mut semi = (0..m).map(to_u32).collect::<Result<Vec<u32>, Ice>>()?;
    let mut label = semi.clone();
    let mut ancestor = vec![NONE; m];
    let mut path = Vec::new();
    for w in (1..m).rev() {
        let w = to_u32(w)?;
        let mut s = at(&semi, w)?;
        for &v in preds.get(usize_of(w)?).into_iter().flatten() {
            let u = eval(v, &mut ancestor, &mut label, &semi, &mut path)?;
            s = s.min(at(&semi, u)?);
        }
        put(&mut semi, w, s)?;
        put(&mut ancestor, w, at(parent, w)?)?;
    }
    let mut idom = parent.to_vec();
    for w in 1..m {
        let w = to_u32(w)?;
        let s = at(&semi, w)?;
        let mut d = at(&idom, w)?;
        while d > s {
            let up = at(&idom, d)?;
            // A node's immediate dominator has a smaller number; anything
            // else would loop forever.
            if up >= d {
                return Err(Ice::new("dominance: immediate dominators out of order"));
            }
            d = up;
        }
        put(&mut idom, w, d)?;
    }
    Ok(idom)
}

/// Lengauer and Tarjan's `eval(v)`: `v` itself while it is a root of the
/// forest, else the node of least semidominator on its path below the
/// root, compressing the path. Iterative, with `path` as scratch, so a
/// long chain cannot overflow the Rust stack.
fn eval(
    v: u32,
    ancestor: &mut [u32],
    label: &mut [u32],
    semi: &[u32],
    path: &mut Vec<u32>,
) -> Result<u32, Ice> {
    if at(ancestor, v)? == NONE {
        return Ok(v);
    }
    // The nodes whose ancestor is not a root, from `v` up; compressed from
    // the top down, as the recursive `compress` does.
    path.clear();
    let mut x = v;
    loop {
        let a = at(ancestor, x)?;
        if at(ancestor, a)? == NONE {
            break;
        }
        path.push(x);
        x = a;
    }
    while let Some(y) = path.pop() {
        let a = at(ancestor, y)?;
        let (label_a, label_y) = (at(label, a)?, at(label, y)?);
        if at(semi, label_a)? < at(semi, label_y)? {
            put(label, y, label_a)?;
        }
        put(ancestor, y, at(ancestor, a)?)?;
    }
    at(label, v)
}

/// Pre- and post-order numbers of the dominator tree given by `idom`,
/// by depth-first number.
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
            assert_eq!(d.is_reachable(b), want_b.is_some(), "reachability of {b}");
            for a in 0..succs.len() {
                let expected = want_b.as_ref().is_some_and(|s| s.contains(&a));
                assert_eq!(d.dominates(a, b), expected, "{a} dominates {b}");
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
        // Larger graphs, so path compression and the parent walk see long
        // forest paths and cross edges.
        for _ in 0..40 {
            let n = 20 + next(60);
            let succs: Vec<Vec<usize>> = (0..n)
                .map(|i| {
                    let mut out: Vec<usize> = (0..next(3)).map(|_| next(n)).collect();
                    // Mostly forward, so most nodes are reachable.
                    if i + 1 < n {
                        out.push(i + 1);
                    }
                    out
                })
                .collect();
            agrees(&succs);
        }
    }

    /// The segments of one long block, each unwinding to a cleanup pad
    /// that ends in one shared `resume` (D-698), as the unwind pass and
    /// `Segments` build them: node `2i + 1` is segment `i`, node `2i + 2`
    /// its pad, and the last node the shared pad.
    fn segments_with_shared_pad(segments: usize) -> Vec<Vec<usize>> {
        let resume = segments.saturating_mul(2).saturating_add(1);
        let mut succs = vec![vec![1]];
        for i in 0..segments {
            let seg = i.saturating_mul(2).saturating_add(1);
            let next = if i.saturating_add(1) < segments {
                seg.saturating_add(2)
            } else {
                resume
            };
            // The pad first, then the next segment, as `Segments` lists them.
            succs.push(vec![seg.saturating_add(1), next]);
            succs.push(vec![resume]);
        }
        succs.push(vec![]);
        succs
    }

    #[test]
    fn segments_unwinding_to_a_shared_pad_agree() {
        agrees(&segments_with_shared_pad(1));
        agrees(&segments_with_shared_pad(7));
    }

    #[test]
    fn a_long_block_unwinding_to_a_shared_pad_is_fast() {
        // Review finding R2-ROB-1: Cooper, Harvey and Kennedy's algorithm
        // walked from every pad up the chain of segments, O(n^2): about
        // 2 s of a 10_000-line `main` in a release build. 200_000 segments
        // took minutes; the budget is generous for a debug build.
        let segments = 200_000;
        let succs = segments_with_shared_pad(segments);
        let start = std::time::Instant::now();
        let d = Dominance::from_successors(&succs).unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        let resume = 2 * segments + 1;
        assert!(d.dominates(1, resume));
        assert!(!d.dominates(3, resume));
        assert!(d.dominates(2 * segments - 1, 2 * segments));
        assert!(!d.dominates(2, resume));
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
        let last = 3 * diamonds;
        assert!(d.dominates(0, last));
        assert!(d.dominates(3, last));
        assert!(!d.dominates(1, last));
    }
}
