//! The semantic support gate (D-451): E0038 for well-formed
//! code that the slice's MIR and code generation cannot handle yet, so
//! that `cheby check` and `cheby run` agree.
//!
//! The syntax-shape gate lives in `cheby_resolve`. This one rejects types:
//! in the slice, only `String` and `Nil` may appear in a signature, in a
//! `let` annotation or as the type of an expression. It also rejects, as a
//! fallback, every construct the checker does not implement yet, in case
//! the resolver's gate lets one through. Each later M1 phase deletes its
//! rows here when it lands.

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::Ty;
use cheby_syntax::ast::{BinOp, Expr, LiteralKind};

/// The E0038 diagnostic: "`<what>` are not supported yet", worded like the
/// resolver's gate.
pub(crate) fn not_supported(span: Span, what: &str) -> Diagnostic {
    Diagnostic::new(
        codes::NOT_SUPPORTED_YET,
        format!("{what} are not supported yet"),
    )
    .with_primary(span, "")
}

/// E0038 for a written type: "the type `Int` is not supported yet".
pub(crate) fn unsupported_type(span: Span, shown: &str) -> Diagnostic {
    Diagnostic::new(
        codes::NOT_SUPPORTED_YET,
        format!("the type `{shown}` is not supported yet"),
    )
    .with_primary(span, "")
}

/// E0038 for an expression of a type the slice cannot run.
pub(crate) fn unsupported_value(span: Span, shown: &str) -> Diagnostic {
    Diagnostic::new(
        codes::NOT_SUPPORTED_YET,
        format!("values of type `{shown}` are not supported yet"),
    )
    .with_primary(span, "")
}

/// Whether the slice supports values of type `ty`. [`Ty::ERROR`] passes:
/// its error was reported where it arose.
pub(crate) fn slice_type(ty: Ty) -> bool {
    ty == Ty::STRING || ty == Ty::NIL || ty == Ty::ERROR
}

/// What to call an expression the checker does not implement yet, in
/// E0038. `None` for expressions it implements, and for a stray `_`, which
/// the syntax layer already reported (E0025).
pub(crate) fn unimplemented_expr(expr: Expr<'_>) -> Option<&'static str> {
    Some(match expr {
        Expr::String(s) if s.has_interpolation() => "interpolated strings",
        Expr::String(_) | Expr::Path(_) | Expr::Call(_) | Expr::Hole(_) => return None,
        Expr::Literal(lit) => match lit.kind() {
            Some(LiteralKind::RawString(_)) => "raw strings",
            _ => "numeric literals",
        },
        Expr::Record(r) if r.is_update() => "record updates",
        Expr::Record(_) => "record expressions",
        Expr::Tuple(_) => "tuples",
        Expr::Paren(_) => "parenthesized expressions",
        Expr::List(_) => "list expressions",
        Expr::Block(_) => "block expressions",
        Expr::Closure(_) => "anonymous functions",
        Expr::Case(_) => "`case` expressions",
        Expr::Panic(_) => "`panic` expressions",
        Expr::Todo(_) => "`todo` expressions",
        Expr::Binary(b) if b.op() == Some(BinOp::Pipe) => "pipes",
        Expr::Binary(_) | Expr::Prefix(_) => "operators",
        Expr::Field(_) => "field access",
    })
}
