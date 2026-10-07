//! Checks of expressions: capture holes (E0026, E0027, D-202, D-245),
//! pipes without a hole (E0028, D-244), chained comparisons (E0029,
//! D-108), one-element tuples (E0030, D-076), record updates (E0031,
//! D-035, D-156), type arguments on interface functions (E0023, D-165)
//! and negative literals with a space after the `-` (E0056, §5.3.1).
//!
//! All of these are decided by syntax alone. In particular D-244 makes a
//! pipe into a call without a whole-argument `_` an error whatever the
//! callee's type, so E0028 needs no type information.

use cheby_diag::{Diagnostic, Span, TextRange, codes};

use super::{empty_at, source, spaced_negative_literal};
use crate::ast::{
    Arg, ArgList, AstNode, BinExpr, CallExpr, Expr, PathExpr, PrefixExpr, RecordEntry, RecordExpr,
    TupleExpr,
};

/// E0056: `- 5`. A negative literal is `-` directly before the number
/// (§5.3.1); `-(5)` negates a parenthesized expression and is fine.
pub(super) fn prefix_expr(expr: PrefixExpr<'_>, out: &mut Vec<Diagnostic>) {
    if let Some(gap) = expr.minus_gap() {
        spaced_negative_literal(expr.syntax(), gap, out);
    }
}

/// E0026 and E0027: one `_` per argument list, and not as the only
/// argument (§5.6).
pub(super) fn arg_list(list: ArgList<'_>, out: &mut Vec<Diagnostic>) {
    let mut holes = list.holes();
    let Some(first) = holes.next() else {
        return;
    };
    let extra: Vec<_> = holes.collect();
    if !extra.is_empty() {
        let mut diag = Diagnostic::new(
            codes::MULTIPLE_HOLES,
            "more than one `_` in one argument list",
        );
        for hole in &extra {
            diag = diag.with_primary(hole.significant_span(), "");
        }
        out.push(
            diag.with_secondary(first.significant_span(), "first `_` here")
                .with_help(
                    "a capture has exactly one `_` (D-202); write a closure, such as \
                     `fn(a, b) { f(a, b) }`, or pass the function itself",
                ),
        );
        return;
    }
    if list.args().count() != 1 {
        return;
    }
    let Some(call) = list.syntax().parent().and_then(CallExpr::cast) else {
        return;
    };
    let Some(callee) = call.callee() else {
        return;
    };
    let callee_text = source(callee);
    out.push(
        Diagnostic::new(codes::SOLE_HOLE, "`_` is the only argument of the call")
            .with_primary(call.significant_span(), "")
            .with_note(format!(
                "`{callee_text}(_)` means exactly `{callee_text}` (D-245)"
            ))
            .with_suggestion(
                format!("write `{callee_text}` instead"),
                call.significant_span(),
                callee_text,
            ),
    );
}

/// E0023: an interface function such as `Show::show` takes no type
/// arguments. App. A.5 allows `::<…>` only on the first form of
/// `path_expr`, and interface functions are not generic (D-165).
/// `tree-sitter-cheby` accepts the turbofish here (D-424).
pub(super) fn path_expr(expr: PathExpr<'_>, out: &mut Vec<Diagnostic>) {
    let (Some(path), Some(args)) = (expr.path(), expr.type_args()) else {
        return;
    };
    if path.interface().is_none() {
        return;
    }
    // From the `::` after the path to the `>`.
    let turbofish = Span::new(
        expr.syntax().tree().file(),
        TextRange::new(
            path.significant_span().range.end(),
            args.significant_span().range.end(),
        ),
    );
    out.push(
        Diagnostic::new(
            codes::INVALID_BOUND,
            "an interface function takes no type arguments",
        )
        .with_primary(turbofish, "")
        .with_note("interface functions are not generic (D-165)")
        .with_suggestion("remove the type arguments", turbofish, ""),
    );
}

/// E0028 and E0029 on a binary expression.
pub(super) fn bin_expr(expr: BinExpr<'_>, out: &mut Vec<Diagnostic>) {
    let Some(op) = expr.op() else {
        return;
    };
    if op.is_comparison() {
        chained_comparison(expr, out);
    } else if op == crate::ast::BinOp::Pipe {
        pipe(expr, out);
    }
}

/// E0028: `x |> f(a)` needs a `_` that is a whole argument of `f`'s call
/// (D-244). A parenthesized call, `x |> (f(a))`, is a function value and
/// fine.
fn pipe(expr: BinExpr<'_>, out: &mut Vec<Diagnostic>) {
    let Some(Expr::Call(call)) = expr.rhs() else {
        return;
    };
    let Some(list) = call.arg_list() else {
        return;
    };
    if list.holes().next().is_some() {
        return;
    }
    let Some(callee) = call.callee() else {
        return;
    };
    let callee_text = source(callee);
    let mut diag = Diagnostic::new(codes::PIPE_WITHOUT_HOLE, "pipe into a call without `_`")
        .with_primary(call.significant_span(), "this call has no `_` argument");
    if let Some(op) = expr.op_token() {
        diag = diag.with_secondary(op.span(), "the value piped in");
    }
    if let Some(inner) = nested_capture(call) {
        let inner_callee = inner.callee().map_or("", source);
        diag = diag.with_note(format!(
            "the `_` in `{inner_callee}(…)` belongs to `{inner_callee}` (D-202)"
        ));
    }
    if list.args().next().is_none() {
        diag = diag.with_suggestion(
            format!("to call `{callee_text}` with the piped value, write `{callee_text}`"),
            call.significant_span(),
            callee_text,
        );
    } else if let Some(open) = list.syntax().first_significant_token() {
        diag = diag.with_suggestion(
            "add `_` where the piped value goes",
            empty_at(list.syntax(), open.text_range().end()),
            "_, ",
        );
    }
    out.push(diag);
}

/// The first call nested in `call`'s arguments that has a `_` of its own.
fn nested_capture(call: CallExpr<'_>) -> Option<CallExpr<'_>> {
    call.args()
        .filter_map(|arg| match arg {
            Arg::Expr(expr) => Some(expr),
            Arg::Hole(_) => None,
        })
        .flat_map(|expr| expr.syntax().descendants())
        .filter_map(CallExpr::cast)
        .find(|inner| inner.arg_list().is_some_and(|l| l.holes().next().is_some()))
}

/// E0029: a comparison whose operand is an unparenthesized comparison
/// (D-108). Reported once per extra operator.
fn chained_comparison(expr: BinExpr<'_>, out: &mut Vec<Diagnostic>) {
    let Some(op) = expr.op_token() else {
        return;
    };
    let inner = [expr.lhs(), expr.rhs()]
        .into_iter()
        .flatten()
        .find_map(|operand| match operand {
            Expr::Binary(inner) if inner.op().is_some_and(crate::ast::BinOp::is_comparison) => {
                inner.op_token()
            }
            _ => None,
        });
    let Some(inner) = inner else {
        return;
    };
    out.push(
        Diagnostic::new(
            codes::CHAINED_COMPARISON,
            "comparison operators cannot be chained",
        )
        .with_primary(op.span(), "")
        .with_secondary(inner.span(), "first comparison here")
        .with_help("combine the comparisons with `&&`, such as `a < b && b < c`"),
    );
}

/// E0030: `(e,)` is not a tuple (D-076). `()` is a grammar error, which
/// the parser reports.
pub(super) fn tuple_expr(tuple: TupleExpr<'_>, out: &mut Vec<Diagnostic>) {
    if tuple.elements().count() != 1 {
        return;
    }
    out.push(
        Diagnostic::new(codes::SHORT_TUPLE, "a tuple has at least two elements")
            .with_primary(tuple.significant_span(), "")
            .with_help("write the value alone; `(e)` is just `e` in parentheses"),
    );
}

/// E0031: a record update has one spread, first, and at least one field
/// (§5.11.2, D-035, D-156).
pub(super) fn record_expr(record: RecordExpr<'_>, out: &mut Vec<Diagnostic>) {
    let mut spreads = 0_usize;
    let mut fields = 0_usize;
    for (index, entry) in record.entries().enumerate() {
        match entry {
            RecordEntry::Field(_) => fields = fields.saturating_add(1),
            RecordEntry::Spread(spread) => {
                spreads = spreads.saturating_add(1);
                if spreads > 1 {
                    out.push(
                        Diagnostic::new(
                            codes::INVALID_RECORD_UPDATE,
                            "a record update has only one `..base`",
                        )
                        .with_primary(spread.significant_span(), "")
                        .with_help("remove the extra spread"),
                    );
                } else if index > 0 {
                    out.push(
                        Diagnostic::new(
                            codes::INVALID_RECORD_UPDATE,
                            "`..base` must come first in a record update",
                        )
                        .with_primary(spread.significant_span(), "")
                        .with_help("move the spread before the fields"),
                    );
                }
            }
        }
    }
    if spreads > 0 && fields == 0 {
        out.push(
            Diagnostic::new(
                codes::INVALID_RECORD_UPDATE,
                "a record update needs at least one field",
            )
            .with_primary(record.significant_span(), "")
            .with_help("to copy a value unchanged, use it as it is"),
        );
    }
}
