//! The Explain action (X §8.3, D-1693): one per distinct code of the
//! request's own `context.diagnostics` that touches the range, titled
//! "Explain E0044" (D-1736), of kind `quickfix`, with no edit and the
//! command `cheby.explain` with the code as its argument.
//!
//! It reads `FixCx::context`, not the published report, so it is offered
//! for a child's code (E0069, E0075 to E0078) and for a report that lags
//! (G1 plan §1.6, D-1685). The frame puts it after the fixes (X §6.1).

use cheby_driver::analysis::codes::{self, Code};
use cheby_driver::analysis::{PlainDiagnostic, TextRange};

use super::actions::{Action, ActionCommand, ActionKind, DiagRef, FixCx};

/// The Explain actions for `range`, in the order of the request's
/// diagnostics, each naming the first of them with its code.
pub(crate) fn provide(
    cx: &FixCx<'_>,
    range: TextRange,
    _diagnostics: &[(usize, &PlainDiagnostic)],
) -> Vec<Action> {
    let mut seen: Vec<Code> = Vec::new();
    let mut out = Vec::new();
    for (index, &(code, at)) in cx.context.iter().enumerate() {
        let touches = at.start() <= range.end() && range.start() <= at.end();
        if !touches || seen.contains(&code) || codes::explanation(code).is_none() {
            continue;
        }
        seen.push(code);
        out.push(Action {
            title: format!("Explain {code}"),
            kind: ActionKind::QuickFix,
            diagnostic: Some(DiagRef::Context(index)),
            preferred: false,
            removes: false,
            edit: None,
            data: None,
            disabled: None,
            command: Some(ActionCommand::Explain(code)),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use cheby_driver::analysis::TextRange;
    use cheby_driver::analysis::codes;

    use super::provide;
    use crate::features::ide::actions::{ActionCommand, DiagRef, FixCx};
    use crate::features::ide::habits::tests::fix_cx_for;

    /// One action per distinct code that touches the range, from the
    /// request's diagnostics alone: a code no report holds, such as a
    /// child's E0075, is explained too, and one outside the range is not.
    #[test]
    fn explain_is_one_per_code_of_the_context() {
        let context = [
            (codes::SHADOWED_NAME, TextRange::new(10, 12)),
            (codes::CONSTANT_PANICKED, TextRange::new(11, 14)),
            (codes::SHADOWED_NAME, TextRange::new(12, 13)),
            (codes::UNUSED_BINDING, TextRange::new(40, 44)),
        ];
        fix_cx_for("explain", &context, |cx: &FixCx<'_>| {
            let actions = provide(cx, TextRange::new(12, 12), &[]);
            let titles: Vec<&str> = actions.iter().map(|a| a.title.as_str()).collect();
            assert_eq!(titles, ["Explain E0044", "Explain E0075"]);
            assert_eq!(actions[0].diagnostic, Some(DiagRef::Context(0)));
            assert_eq!(actions[1].diagnostic, Some(DiagRef::Context(1)));
            assert_eq!(
                actions[1].command,
                Some(ActionCommand::Explain(codes::CONSTANT_PANICKED))
            );
            assert!(actions.iter().all(|a| a.edit.is_none() && a.data.is_none()));
            assert_eq!(provide(cx, TextRange::new(30, 30), &[]), Vec::new());
        });
    }
}
