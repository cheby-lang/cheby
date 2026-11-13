//! Best-effort facts about bodies, for the language server (D-1380).
//!
//! Facts sit beside THIR, never in it (ADR-0078). A body with an error is
//! still not returned, so MIR never sees one, and what is kept of it here
//! is closed types and ids, not an `Expr`, a `Pat` or a `Body`.
//!
//! Recording never changes checking. Hooks only append to `FactLog`, and
//! the log becomes [`BodyFacts`] after the checker's last diagnostic that
//! could depend on it. Recording happens only in
//! [`crate::check_bodies_recording`].
//!
//! Every [`Ty`] and [`Name`] belongs to the run's `Globals`, so facts must
//! stay inside their snapshot.

use cheby_diag::{Span, TextRange, TextSize};
use cheby_iface::{DefId, DefIndex, Ice, IndexVec, Name, Ty};
use cheby_resolve::LocalId;
use cheby_syntax::NodeId;

pub use crate::exhaust::{CaseFact, MissingPat};
use crate::ifaces::Unsatisfied;
use crate::infer::InferTy;
use crate::thir::{self, Callee, Dict, ExprId, PatId};

/// What one module gave. `bodies` is in item order.
#[derive(Clone, Debug, Default)]
pub struct ModuleFacts {
    /// The facts of each body that was checked.
    pub bodies: Vec<BodyFacts>,
    /// Bodies an `Ice` ended, with the `Ice`.
    pub lost: Vec<(DefIndex, Ice)>,
    /// A module-wide pass that an `Ice` ended. Its diagnostics are missing.
    pub lost_passes: Vec<Ice>,
}

impl ModuleFacts {
    /// The facts of the body of `item`.
    #[must_use]
    pub fn body(&self, item: DefIndex) -> Option<&BodyFacts> {
        find(&self.bodies, &item, |b| b.item)
    }
}

/// What checking one body found, errored or not.
#[derive(Clone, Debug)]
pub struct BodyFacts {
    /// The item that holds the body.
    pub item: DefIndex,
    /// Whether the body had an error, so THIR does not hold it.
    pub errored: bool,
    /// Sorted by `node`.
    pub exprs: Box<[ExprFact]>,
    /// Sorted by node.
    pub pats: Box<[(NodeId, Ty)]>,
    /// Every local, the resolver's first, then synthetic ones.
    pub locals: IndexVec<LocalId, thir::Local>,
    /// Sorted by `name_span` start.
    pub fields: Box<[FieldFact]>,
    /// Sorted by `callee`.
    pub calls: Box<[CallFact]>,
    /// Sorted by `node`.
    pub ops: Box<[OpFact]>,
    /// Sorted by `(node, part)`.
    pub shows: Box<[ShowFact]>,
    /// Sorted by `node`.
    pub dyns: Box<[DynFact]>,
    /// Sorted by `node`.
    pub cases: Box<[CaseFact]>,
    /// Sorted by span start.
    pub mismatches: Box<[MismatchFact]>,
    /// Sorted by span start.
    pub unsatisfied: Box<[UnsatisfiedFact]>,
}

impl BodyFacts {
    /// The fact of the expression `node`.
    #[must_use]
    pub fn expr(&self, node: NodeId) -> Option<&ExprFact> {
        find(&self.exprs, &node, |e| e.node)
    }

    /// The type of the pattern `node`.
    #[must_use]
    pub fn pat(&self, node: NodeId) -> Option<Ty> {
        find(&self.pats, &node, |p| p.0).map(|p| p.1)
    }

    /// The call whose callee expression is `callee`.
    #[must_use]
    pub fn call(&self, callee: NodeId) -> Option<&CallFact> {
        find(&self.calls, &callee, |c| c.callee)
    }

    /// The field whose name token starts at `name_start`.
    #[must_use]
    pub fn field_at(&self, name_start: TextSize) -> Option<&FieldFact> {
        find(&self.fields, &name_start, |f| f.name_span.range.start())
    }

    /// The operator `node`, a `BIN_EXPR` or a `PREFIX_EXPR`.
    #[must_use]
    pub fn op(&self, node: NodeId) -> Option<&OpFact> {
        find(&self.ops, &node, |o| o.node)
    }

    /// The conversion to `dyn` of the expression `node`.
    #[must_use]
    pub fn dyn_at(&self, node: NodeId) -> Option<&DynFact> {
        find(&self.dyns, &node, |d| d.node)
    }

    /// The matching of the `case` `node`.
    #[must_use]
    pub fn case(&self, node: NodeId) -> Option<&CaseFact> {
        find(&self.cases, &node, |c| c.node)
    }

    /// The E0047 whose primary span covers exactly `range`.
    #[must_use]
    pub fn mismatch_at(&self, range: TextRange) -> Option<&MismatchFact> {
        find(&self.mismatches, &range, |m| m.span.range)
    }

    /// The E0063 or E0065 whose primary span covers exactly `range`.
    #[must_use]
    pub fn unsatisfied_at(&self, range: TextRange) -> Option<&UnsatisfiedFact> {
        find(&self.unsatisfied, &range, |u| u.span.range)
    }
}

/// The entry of the sorted `table` whose key is `key`.
fn find<'t, T, K: Ord>(table: &'t [T], key: &K, key_of: impl FnMut(&T) -> K) -> Option<&'t T> {
    let at = table.binary_search_by_key(key, key_of).ok()?;
    table.get(at)
}

/// The type of one expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExprFact {
    /// The expression's node.
    pub node: NodeId,
    /// The expression's own type, before any conversion or E0047.
    pub ty: Ty,
    /// The type its context expected, if one did.
    pub expected: Option<Ty>,
}

/// A callee and its instantiated signature.
#[derive(Clone, Debug)]
pub struct CallFact {
    /// The callee expression's node: `PATH_EXPR`, or any expression.
    pub callee: NodeId,
    /// `None` for a value of function type.
    pub def: Option<DefId>,
    /// The instantiated type arguments; for `I::f`, `[Self]`.
    pub ty_args: Box<[Ty]>,
    /// The instantiated signature's parameters.
    pub params: Box<[Ty]>,
    /// The instantiated signature's result.
    pub ret: Ty,
    /// The finished call's callee, with interface calls resolved. `None`
    /// when the call failed.
    pub resolved: Option<Callee>,
}

/// Where a field name is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldSite {
    /// `x.name`, or `{x.name}` in a string.
    Read,
    /// `P { name: 1 }`.
    Init,
    /// `T { ..base, name: 1 }`.
    Update,
    /// A field of a constructor pattern.
    Pattern,
}

/// One field name in a body.
#[derive(Clone, Debug)]
pub struct FieldFact {
    /// The name token.
    pub name_span: Span,
    /// The name.
    pub name: Name,
    /// Where it is written.
    pub site: FieldSite,
    /// The receiver's type, for `Read`.
    pub base: Option<Ty>,
    /// The type that has the field. `None` when the receiver's type is
    /// unknown: a field read nothing can place.
    pub owner: Option<DefId>,
    /// The constructor named at `Init` and `Pattern`.
    pub ctor: Option<DefId>,
    /// `None` when the field does not exist, or has no one type.
    pub ty: Option<Ty>,
}

/// What an operator calls.
#[derive(Clone, Debug)]
pub struct OpFact {
    /// The `BIN_EXPR` or `PREFIX_EXPR`.
    pub node: NodeId,
    /// `None` for a built-in operator, or one that did not resolve.
    pub callee: Option<Callee>,
}

/// The `show` of one `{x}`.
#[derive(Clone, Debug)]
pub struct ShowFact {
    /// The `STRING_LIT`.
    pub node: NodeId,
    /// The index in `ExprKind::Interpolate`'s parts.
    pub part: u32,
    /// The function that prints the value.
    pub callee: Callee,
}

/// A conversion to `dyn`.
#[derive(Clone, Debug)]
pub struct DynFact {
    /// The converted expression's node.
    pub node: NodeId,
    /// The interface of the `dyn`.
    pub iface: DefId,
    /// The placeholder when the constraint did not resolve.
    pub dict: Dict,
}

/// The two types of an E0047.
#[derive(Clone, Copy, Debug)]
pub struct MismatchFact {
    /// E0047's primary span.
    pub span: Span,
    /// The type the context wanted.
    pub expected: Ty,
    /// The type it got.
    pub found: Ty,
}

/// Why a requirement failed, for an E0063 or an E0065.
#[derive(Clone, Debug)]
pub struct UnsatisfiedFact {
    /// E0063's or E0065's primary span.
    pub span: Span,
    /// The failure.
    pub err: Unsatisfied,
}

/// What the hooks append while a body is checked.
#[derive(Default)]
pub(crate) struct FactLog {
    pub exprs: Vec<ExprLog>,
    pub pats: Vec<(NodeId, PatId)>,
    /// The patterns that failed and became `PatKind::Wild`, which match
    /// nothing a reader wrote.
    pub failed_pats: Vec<PatId>,
    /// The types [`crate::check`]'s `set_ty` replaced, oldest first.
    pub overwritten: Vec<(ExprId, InferTy)>,
    pub calls: Vec<CallLog>,
    pub fields: Vec<FieldLog>,
    pub mismatches: Vec<(Span, InferTy, InferTy)>,
    pub unsatisfied: Vec<(Span, Unsatisfied)>,
}

/// One checked expression: `id` as it was built, `out` as its context got
/// it, which may be a `dyn` conversion around `id`.
pub(crate) struct ExprLog {
    pub node: NodeId,
    pub id: ExprId,
    pub out: ExprId,
    pub expected: Option<InferTy>,
}

/// One callee.
pub(crate) struct CallLog {
    pub callee: NodeId,
    pub def: Option<DefId>,
    pub ty_args: Box<[InferTy]>,
    pub params: Vec<InferTy>,
    pub ret: InferTy,
    /// The call expression, once one is built.
    pub call: Option<ExprId>,
    /// The arguments of a call with the wrong number of them, by parameter
    /// position. A capture's hole is a `None`.
    pub args: Vec<Option<ExprId>>,
}

/// One field name.
pub(crate) struct FieldLog {
    pub name_span: Span,
    pub name: Name,
    pub site: FieldSite,
    pub base: Option<InferTy>,
    pub owner: Option<DefId>,
    pub ctor: Option<DefId>,
    pub ty: Option<InferTy>,
}
