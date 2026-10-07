//! Import-graph algorithms over dense node indices (B3, D-145).
//!
//! Node `i` imports the nodes `edges[i]`. Results depend only on the graph,
//! never on the order edges were added, so module order and cycle reports
//! are deterministic (D-264).

use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// A directed graph: node `i` imports the nodes in `edges[i]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Graph {
    /// Sorted and without duplicates.
    edges: Vec<Vec<u32>>,
}

/// An import cycle: the nodes in import order (each imports the next, and
/// the last imports the first), starting at the smallest index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cycle {
    pub nodes: Vec<u32>,
}

impl Graph {
    /// A graph of `len` nodes and no edges.
    #[must_use]
    pub fn new(len: usize) -> Self {
        Self {
            edges: vec![Vec::new(); len],
        }
    }

    /// The number of nodes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.edges.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// Adds a node and returns its index, or `None` if the graph is full.
    pub fn add_node(&mut self) -> Option<u32> {
        let index = u32::try_from(self.edges.len()).ok()?;
        self.edges.push(Vec::new());
        Some(index)
    }

    /// Records that `from` imports `to`. Returns false, and changes
    /// nothing, if either node does not exist.
    pub fn add_edge(&mut self, from: u32, to: u32) -> bool {
        if usize::try_from(to).map_or(true, |to| to >= self.edges.len()) {
            return false;
        }
        let Some(list) = usize::try_from(from)
            .ok()
            .and_then(|from| self.edges.get_mut(from))
        else {
            return false;
        };
        if let Err(at) = list.binary_search(&to) {
            list.insert(at, to);
        }
        true
    }

    /// The nodes `node` imports, sorted.
    #[must_use]
    pub fn successors(&self, node: u32) -> &[u32] {
        usize::try_from(node)
            .ok()
            .and_then(|n| self.edges.get(n))
            .map_or(&[], Vec::as_slice)
    }

    fn nodes(&self) -> impl DoubleEndedIterator<Item = u32> + use<> {
        0..u32::try_from(self.edges.len()).unwrap_or(u32::MAX)
    }
}

/// One cycle per strongly connected component that has one (a component
/// with two or more nodes, or a node that imports itself), ordered by the
/// smallest node of each.
///
/// The reported cycle of a component is found by a breadth-first search
/// from its smallest node back to itself, following edges in index order,
/// so it is a shortest cycle through that node.
#[must_use]
pub fn find_cycles(g: &Graph) -> Vec<Cycle> {
    let components = strongly_connected(g);
    let mut cycles = Vec::new();
    for component in components {
        let Some(&start) = component.first() else {
            continue;
        };
        let cyclic = component.len() > 1 || g.successors(start).contains(&start);
        if !cyclic {
            continue;
        }
        if let Some(nodes) = shortest_cycle(g, start, &component) {
            cycles.push(Cycle { nodes });
        }
    }
    cycles.sort_by_key(|c| c.nodes.first().copied());
    cycles
}

/// The nodes ordered so that every node comes after the nodes it imports,
/// ties broken by smallest index first (Kahn's algorithm with a min-heap).
///
/// # Errors
///
/// The cycles of [`find_cycles`] if the graph has any.
pub fn topological_order(g: &Graph) -> Result<Vec<u32>, Vec<Cycle>> {
    let cycles = find_cycles(g);
    if !cycles.is_empty() {
        return Err(cycles);
    }
    // `remaining[n]`: imports of `n` not yet emitted. `importers[m]`: nodes
    // that import `m`.
    let len = g.len();
    let mut remaining = vec![0_usize; len];
    let mut importers: Vec<Vec<u32>> = vec![Vec::new(); len];
    for node in g.nodes() {
        for &dep in g.successors(node) {
            if let Some(count) = slot(&mut remaining, node) {
                *count = count.saturating_add(1);
            }
            if let Some(list) = slot(&mut importers, dep) {
                list.push(node);
            }
        }
    }
    let mut ready: BinaryHeap<Reverse<u32>> = g
        .nodes()
        .filter(|&n| slot(&mut remaining, n).is_some_and(|c| *c == 0))
        .map(Reverse)
        .collect();
    let mut order = Vec::with_capacity(len);
    while let Some(Reverse(node)) = ready.pop() {
        order.push(node);
        let dependents = slot(&mut importers, node)
            .map(std::mem::take)
            .unwrap_or_default();
        for importer in dependents {
            if let Some(count) = slot(&mut remaining, importer) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    ready.push(Reverse(importer));
                }
            }
        }
    }
    Ok(order)
}

/// The nodes that `roots` reach, including the roots, sorted.
#[must_use]
pub fn reachable(g: &Graph, roots: &[u32]) -> Vec<u32> {
    let mut seen = vec![false; g.len()];
    let mut stack: Vec<u32> = roots.to_vec();
    while let Some(node) = stack.pop() {
        match slot(&mut seen, node) {
            Some(flag) if !*flag => *flag = true,
            _ => continue,
        }
        stack.extend(g.successors(node).iter().rev());
    }
    g.nodes()
        .filter(|&n| slot(&mut seen, n).is_some_and(|f| *f))
        .collect()
}

fn slot<T>(v: &mut [T], node: u32) -> Option<&mut T> {
    usize::try_from(node).ok().and_then(|n| v.get_mut(n))
}

/// Strongly connected components (iterative Tarjan), each sorted, in no
/// particular order.
fn strongly_connected(g: &Graph) -> Vec<Vec<u32>> {
    const UNVISITED: usize = usize::MAX;
    let len = g.len();
    let mut index = vec![UNVISITED; len];
    let mut low = vec![0_usize; len];
    let mut on_stack = vec![false; len];
    let mut stack: Vec<u32> = Vec::new();
    let mut components = Vec::new();
    let mut counter = 0_usize;
    for root in g.nodes() {
        if slot(&mut index, root).is_some_and(|i| *i != UNVISITED) {
            continue;
        }
        // Frames: (node, position in its successor list).
        let mut frames: Vec<(u32, usize)> = vec![(root, 0)];
        visit(
            root,
            &mut index,
            &mut low,
            &mut on_stack,
            &mut stack,
            &mut counter,
        );
        while let Some(&mut (node, ref mut pos)) = frames.last_mut() {
            let succs = g.successors(node);
            if let Some(&next) = succs.get(*pos) {
                *pos = pos.saturating_add(1);
                let next_index = slot(&mut index, next).map_or(UNVISITED, |i| *i);
                if next_index == UNVISITED {
                    visit(
                        next,
                        &mut index,
                        &mut low,
                        &mut on_stack,
                        &mut stack,
                        &mut counter,
                    );
                    frames.push((next, 0));
                } else if slot(&mut on_stack, next).is_some_and(|f| *f) {
                    let node_low = slot(&mut low, node).map_or(0, |l| *l);
                    if let Some(l) = slot(&mut low, node) {
                        *l = node_low.min(next_index);
                    }
                }
                continue;
            }
            frames.pop();
            let node_low = slot(&mut low, node).map_or(0, |l| *l);
            if let Some(&(parent, _)) = frames.last()
                && let Some(l) = slot(&mut low, parent)
            {
                *l = (*l).min(node_low);
            }
            if slot(&mut index, node).is_some_and(|i| *i == node_low) {
                let mut component = Vec::new();
                while let Some(member) = stack.pop() {
                    if let Some(f) = slot(&mut on_stack, member) {
                        *f = false;
                    }
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                component.sort_unstable();
                components.push(component);
            }
        }
    }
    components
}

fn visit(
    node: u32,
    index: &mut [usize],
    low: &mut [usize],
    on_stack: &mut [bool],
    stack: &mut Vec<u32>,
    counter: &mut usize,
) {
    if let Some(i) = slot(index, node) {
        *i = *counter;
    }
    if let Some(l) = slot(low, node) {
        *l = *counter;
    }
    if let Some(f) = slot(on_stack, node) {
        *f = true;
    }
    stack.push(node);
    *counter = counter.saturating_add(1);
}

/// A shortest cycle from `start` back to itself inside `component`
/// (sorted), following edges in index order.
fn shortest_cycle(g: &Graph, start: u32, component: &[u32]) -> Option<Vec<u32>> {
    if g.successors(start).contains(&start) {
        return Some(vec![start]);
    }
    let mut parent: Vec<Option<u32>> = vec![None; g.len()];
    let mut queue = std::collections::VecDeque::from([start]);
    let mut seen = vec![false; g.len()];
    if let Some(f) = slot(&mut seen, start) {
        *f = true;
    }
    while let Some(node) = queue.pop_front() {
        for &next in g.successors(node) {
            if component.binary_search(&next).is_err() {
                continue;
            }
            if next == start {
                let mut path = vec![node];
                let mut at = node;
                while let Some(p) = slot(&mut parent, at).and_then(|p| *p) {
                    path.push(p);
                    at = p;
                }
                path.reverse();
                return Some(path);
            }
            if slot(&mut seen, next).is_some_and(|f| !*f) {
                if let Some(f) = slot(&mut seen, next) {
                    *f = true;
                }
                if let Some(p) = slot(&mut parent, next) {
                    *p = Some(node);
                }
                queue.push_back(next);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(len: usize, edges: &[(u32, u32)]) -> Graph {
        let mut g = Graph::new(len);
        for &(a, b) in edges {
            assert!(g.add_edge(a, b));
        }
        g
    }

    #[test]
    fn order_puts_dependencies_first() {
        // 0 imports 2 and 1, 1 imports 3, 2 imports 3.
        let mut g = graph(4, &[(0, 2), (0, 1), (1, 3), (2, 3)]);
        assert_eq!(topological_order(&g).unwrap(), [3, 1, 2, 0]);
        assert_eq!(reachable(&g, &[1]), [1, 3]);
        assert!(!g.add_edge(0, 99));
        assert!(!g.add_edge(99, 0));
    }

    #[test]
    fn self_import_is_a_cycle() {
        let g = graph(2, &[(0, 1), (1, 1)]);
        let cycles = topological_order(&g).unwrap_err();
        assert_eq!(cycles, [Cycle { nodes: vec![1] }]);
    }

    #[test]
    fn two_disjoint_cycles() {
        // 0 → 1 → 0 and 2 → 3 → 4 → 2, plus 5 importing both.
        let g = graph(6, &[(0, 1), (1, 0), (2, 3), (3, 4), (4, 2), (5, 0), (5, 2)]);
        let cycles = find_cycles(&g);
        assert_eq!(
            cycles,
            [
                Cycle { nodes: vec![0, 1] },
                Cycle {
                    nodes: vec![2, 3, 4]
                }
            ]
        );
    }

    #[test]
    fn deterministic_for_permuted_edges() {
        let edges = [(0, 3), (3, 1), (1, 0), (2, 4), (4, 3), (5, 2)];
        let a = graph(6, &edges);
        let mut reversed = edges;
        reversed.reverse();
        let b = graph(6, &reversed);
        assert_eq!(a, b);
        assert_eq!(find_cycles(&a), find_cycles(&b));
        assert_eq!(
            find_cycles(&a),
            [Cycle {
                nodes: vec![0, 3, 1]
            }]
        );
        let acyclic = [(0, 1), (2, 1), (3, 0), (3, 2), (4, 3)];
        let mut permuted = acyclic;
        permuted.swap(0, 4);
        permuted.swap(1, 3);
        assert_eq!(
            topological_order(&graph(5, &acyclic)),
            topological_order(&graph(5, &permuted))
        );
        assert_eq!(
            topological_order(&graph(5, &acyclic)).unwrap(),
            [1, 0, 2, 3, 4]
        );
    }

    #[test]
    fn long_chain_needs_no_recursion() {
        let len = 100_000_u32;
        let mut g = Graph::new(usize::try_from(len).unwrap());
        for n in 1..len {
            g.add_edge(n, n - 1);
        }
        g.add_edge(0, len - 1);
        let cycles = find_cycles(&g);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].nodes.len(), usize::try_from(len).unwrap());
        assert_eq!(cycles[0].nodes[..3], [0, len - 1, len - 2]);
    }
}
