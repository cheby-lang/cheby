//! Static counts of MIR for reports: `bench-compile`'s RC-operations
//! column (D-759, D-718).

use cheby_iface::Idx;

use crate::ir::{BlockId, Function, Rvalue, Stmt};

/// D-759's RC-operations count of one function: its `dup`, `drop` and
/// `drop_reuse` statements outside unwind cleanup pads, each one
/// operation. A pad is a block that normal control flow does not reach
/// (only an unwind edge or the entry pad leads to it). `alloc_reuse` is
/// an allocation, not a count operation, and is not counted
/// (GAP(rc-column-forms)). Until checkpoint 4 the column counted the
/// pads' drops too, against D-759's text (`.m2-work/slice4/P2/rc-column.md`).
#[must_use]
pub fn rc_ops(function: &Function) -> usize {
    let normal = normally_reachable(function);
    function
        .blocks
        .iter_enumerated()
        .filter(|(id, _)| normal.get(id.index()).copied().unwrap_or(false))
        .flat_map(|(_, block)| &block.stmts)
        .filter(|stmt| {
            matches!(
                stmt,
                Stmt::Dup(_)
                    | Stmt::Drop(_)
                    | Stmt::Assign {
                        value: Rvalue::DropReuse { .. },
                        ..
                    }
            )
        })
        .count()
}

/// The blocks reached from `bb0` through normal successors only, by block
/// index.
fn normally_reachable(f: &Function) -> Vec<bool> {
    let mut seen = vec![false; f.blocks.len()];
    let mut work = vec![BlockId::from_u32(0)];
    while let Some(id) = work.pop() {
        let Some(block) = f.blocks.get(id) else {
            continue;
        };
        match seen.get_mut(id.index()) {
            Some(s) if !*s => *s = true,
            _ => continue,
        }
        work.extend(block.term.successors());
    }
    seen
}

#[cfg(test)]
mod tests {
    use crate::test_support::lower_source;

    /// The column leaves pads out (D-759): a function whose call may
    /// unwind with a value held has a pad that drops it, which is not
    /// counted.
    #[test]
    fn rc_ops_leave_pads_out() {
        let lowered = lower_source(&[(
            "",
            r#"//! A value held across a call.

fn keep(s: String) -> String {
  let t = id(s)
  "{s}{t}"
}

fn id(s: String) -> String {
  s
}

fn main() {
  let _ = keep("a")
}
"#,
        )]);
        let unit = lowered.unit("app");
        let mut all = 0;
        let mut counted = 0;
        for f in &unit.functions {
            all += f
                .blocks
                .iter()
                .flat_map(|b| &b.stmts)
                .filter(|s| matches!(s, crate::Stmt::Dup(_) | crate::Stmt::Drop(_)))
                .count();
            counted += super::rc_ops(f);
        }
        assert!(counted > 0, "{counted}");
        assert!(counted < all, "{counted} of {all}");
    }
}
