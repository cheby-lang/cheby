//! Inlay hints of types (X §7.1, plan W8): the type of a `let` binding and
//! of each name a destructuring `let` binds, of a `use` binder, of a
//! closure's or local function's parameters and result, of each stage of a
//! multi-line pipeline, and the implicit conversions to `dyn`.
//!
//! Types come from the body's facts (D-1380), which exist for an errored
//! body too, so hints stay while a body has a type error. A type with an
//! unknown part, which prints a `_`, gets no hint: an incomplete hint
//! misleads more than none (X §7.1).
//!
//! Every hint is computed over whole bodies and only then limited to the
//! request's range, so a range never changes which hints exist, only which
//! are sent. Parameter names (`inlayHints.parameterNames`), constant values
//! (W9) and the memory and tail rows (X §7.3) are not here.

use cheby_driver::analysis::{
    BodyFacts, BodyRes, DefId, DefIndex, DefKind, FileLocation, Generics, ItemSig, ModuleView, Res,
    Snapshot, TextRange, TextSize, Ty, TyKind,
};
use cheby_syntax::SyntaxKind as K;
use cheby_syntax::SyntaxNode;
use cheby_syntax::ast::{
    AstNode, BinExpr, BinOp, Closure, Expr, GuardStmt, LetStmt, Literal, LiteralKind, LocalFn,
    Param, UseBinder,
};

use crate::features::ide::print::Printer;
use crate::features::{CancelToken, RequestError};

/// Which rows are on: the `inlayHints.*` settings of D-1376, read by the
/// handler. `enable` is the handler's own test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one switch per row of X §7.1 (D-1376)"
)]
pub(crate) struct Switches {
    /// `inlayHints.letTypes`: a `let` whose pattern is one name (D-1480).
    pub let_types: bool,
    /// `inlayHints.patternBinders`: each name of a destructuring `let`
    /// (D-1480).
    pub pattern_binders: bool,
    /// `inlayHints.useBinders`: the binders of a `use`.
    pub use_binders: bool,
    /// `inlayHints.closureTypes`: the parameters and result of a closure
    /// (D-1480).
    pub closure_types: bool,
    /// `inlayHints.localFnTypes`: the parameters and result of a local
    /// function (D-1480).
    pub local_fn_types: bool,
    /// `inlayHints.pipeStages`: each stage of a multi-line pipeline.
    pub pipe_stages: bool,
    /// `inlayHints.dyn`: implicit conversions to `dyn`.
    pub dyn_: bool,
}

impl Switches {
    /// Every row on, as D-1376's defaults have them.
    #[cfg(test)]
    pub(crate) const ALL: Self = Self {
        let_types: true,
        pattern_binders: true,
        use_binders: true,
        closure_types: true,
        local_fn_types: true,
        pipe_stages: true,
        dyn_: true,
    };
}

/// One hint of a type (`InlayHintKind::Type`): a label at an offset of the
/// document's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hint {
    /// Where the label shows: the end of what it describes.
    pub offset: TextSize,
    /// The label, with its separator: `: Int`, ` -> Int`, ` as dyn Shape`.
    pub label: String,
    /// The text that writes the hint into the code at `offset`, when it
    /// would compile as written (X §7.1): `let` bindings only.
    pub edit: Option<String>,
}

/// The hints of the module `view` of `snapshot`, in text order. With
/// `within`, only the hints whose offset lies in it, ends included.
///
/// # Errors
///
/// [`RequestError::Cancelled`] when the client cancelled the request.
pub(crate) fn hints(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    within: Option<TextRange>,
    switches: Switches,
    cancel: &CancelToken,
) -> Result<Vec<Hint>, RequestError> {
    let (Some(tree), Some(resolved), Some(facts)) = (view.tree(), view.resolved(), view.facts())
    else {
        return Ok(Vec::new());
    };
    // A std source cannot be edited, so its hints offer no edit.
    let editable = view.file().is_some_and(|file| {
        matches!(
            snapshot.location(file),
            Some(FileLocation::Package(_) | FileLocation::Synthetic)
        )
    });
    let cx = Cx {
        snapshot,
        view,
        printer: Printer::new(snapshot, view),
        switches,
        editable,
    };
    let mut bodies: Vec<(DefIndex, &BodyRes, SyntaxNode<'_>)> = resolved
        .bodies
        .iter()
        .filter_map(|(&index, res)| Some((index, res, tree.node(resolved.item(index)?.node)?)))
        .filter(|(_, _, node)| within.is_none_or(|w| overlaps(node.text_range(), w)))
        .collect();
    bodies.sort_by_key(|(_, _, node)| node.text_range().start());
    let mut out = Vec::new();
    for (index, res, node) in bodies {
        cancel.check()?;
        let Some(body_facts) = facts.body(index) else {
            continue;
        };
        let body = Body {
            res,
            facts: body_facts,
            generics: cx.generics(index),
        };
        cx.body(&body, node, &mut out);
    }
    cancel.check()?;
    if let Some(within) = within {
        out.retain(|hint| within.start() <= hint.offset && hint.offset <= within.end());
    }
    out.sort_by_key(|hint| hint.offset);
    Ok(out)
}

/// Whether `a` and `b` share a point, ends included.
fn overlaps(a: TextRange, b: TextRange) -> bool {
    a.start() <= b.end() && b.start() <= a.end()
}

/// What every row reads.
struct Cx<'s> {
    snapshot: &'s Snapshot,
    view: ModuleView<'s>,
    printer: Printer<'s>,
    switches: Switches,
    editable: bool,
}

/// One body: its names, its facts and the type parameters in force.
struct Body<'s> {
    res: &'s BodyRes,
    facts: &'s BodyFacts,
    generics: Option<&'s Generics>,
}

impl Cx<'_> {
    /// The type parameters in force in the body of this module's `item`.
    fn generics(&self, item: DefIndex) -> Option<&Generics> {
        let iface = self.snapshot.interfaces().get(self.view.id())?;
        match &iface.item(item)?.sig {
            ItemSig::Fn(f) => Some(&f.generics),
            _ => None,
        }
    }

    /// Every hint of one body, in tree order.
    fn body(&self, body: &Body<'_>, node: SyntaxNode<'_>, out: &mut Vec<Hint>) {
        for n in node.descendants() {
            match n.kind() {
                K::IDENT_PAT | K::AS_PAT | K::REST_PAT | K::FIELD_PAT => {
                    out.extend(self.binder(body, n));
                }
                K::CLOSURE => out.extend(self.closure_result(body, n)),
                K::LOCAL_FN => out.extend(self.local_fn_result(body, n)),
                K::BIN_EXPR => self.pipeline(body, n, out),
                _ => {}
            }
        }
        if self.switches.dyn_ {
            out.extend(body.facts.dyns.iter().filter_map(|conversion| {
                let value = self.view.tree()?.node(conversion.node)?;
                self.conversion(body, value, conversion.iface)
            }));
        }
    }

    /// `ty` for reading, or `None` when it has an unknown part (X §7.1).
    fn shown(&self, ty: Ty, body: &Body<'_>) -> Option<String> {
        (!self.has_unknown(ty)).then(|| self.printer.type_display(ty, body.generics))
    }

    /// Whether any part of `ty` is unknown, which prints as `_`.
    fn has_unknown(&self, ty: Ty) -> bool {
        match self.snapshot.globals().types.kind(ty) {
            TyKind::Error => true,
            TyKind::Never | TyKind::Param(_) => false,
            TyKind::Named { args, .. } | TyKind::Dyn { args, .. } => {
                args.iter().any(|&arg| self.has_unknown(arg))
            }
            TyKind::Tuple(elems) => elems.iter().any(|&elem| self.has_unknown(elem)),
            TyKind::Fn { params, ret } => {
                params.iter().any(|&param| self.has_unknown(param)) || self.has_unknown(*ret)
            }
        }
    }

    /// The result of a function type, `None` for any other type.
    fn result_of(&self, ty: Ty) -> Option<Ty> {
        match self.snapshot.globals().types.kind(ty) {
            TyKind::Fn { ret, .. } => Some(*ret),
            _ => None,
        }
    }

    /// The hint of the binding pattern `node`: a `let` or destructuring
    /// binder, a `use` binder, or a parameter of a closure or a local
    /// function. Skipped: an annotated binding, a name starting with `_`,
    /// a synthetic local and a type with an unknown part (X §7.1).
    fn binder(&self, body: &Body<'_>, node: SyntaxNode<'_>) -> Option<Hint> {
        let &id = body.res.bindings.get(&node.id())?;
        let decl = body.res.locals.get(id)?;
        let local = body.facts.locals.get(id)?;
        if local.synthetic
            || self
                .snapshot
                .globals()
                .names
                .get(decl.name)
                .starts_with('_')
        {
            return None;
        }
        let owner = Owner::of(node)?;
        // A plain `let x = …`: the rows that show the type elsewhere on the
        // line, and the only row whose hint can be written into the code.
        let plain = match owner {
            Owner::Let(stmt) => {
                (stmt.pattern == Some(node) && node.kind() == K::IDENT_PAT).then_some(stmt)
            }
            Owner::Use(_) | Owner::Param(_) => None,
        };
        // One switch per row (D-1480): a plain `let`, a destructuring
        // binder, a closure's parameter and a local function's.
        let on = match owner {
            Owner::Let(_) if plain.is_some() => self.switches.let_types,
            Owner::Let(_) => self.switches.pattern_binders,
            Owner::Use(_) => self.switches.use_binders,
            Owner::Param(param) => match param_owner(param) {
                Some(K::CLOSURE) => self.switches.closure_types,
                Some(K::LOCAL_FN) => self.switches.local_fn_types,
                _ => false,
            },
        };
        if !on || owner.annotated() {
            return None;
        }
        if let Some(stmt) = plain
            && stmt
                .value
                .is_some_and(|value| type_on_line(self.snapshot, body, value, local.ty))
        {
            return None;
        }
        let label = self.shown(local.ty, body)?;
        let edit = plain
            .filter(|_| self.editable)
            .and_then(|_| self.printer.type_source(local.ty, body.generics))
            .filter(|source| source.imports.is_empty())
            .map(|source| format!(": {}", source.text));
        Some(Hint {
            offset: decl.span.range.end(),
            label: format!(": {label}"),
            edit,
        })
    }

    /// ` -> T` after the parameters of a closure with no written result.
    fn closure_result(&self, body: &Body<'_>, node: SyntaxNode<'_>) -> Option<Hint> {
        let closure = Closure::cast(node)?;
        if !self.switches.closure_types || closure.ret_type().is_some() {
            return None;
        }
        let params = closure.param_list()?;
        let ty = body.facts.expr(node.id())?.ty;
        self.result_hint(body, ty, params.significant_span().range.end())
    }

    /// ` -> T` after the parameters of a local function with no written
    /// result. Its local holds its function type.
    fn local_fn_result(&self, body: &Body<'_>, node: SyntaxNode<'_>) -> Option<Hint> {
        let local_fn = LocalFn::cast(node)?;
        if !self.switches.local_fn_types || local_fn.ret_type().is_some() {
            return None;
        }
        let params = local_fn.param_list()?;
        let &id = body.res.bindings.get(&node.id())?;
        let ty = body.facts.locals.get(id)?.ty;
        self.result_hint(body, ty, params.significant_span().range.end())
    }

    /// ` -> T` at `offset` for the function type `ty`.
    fn result_hint(&self, body: &Body<'_>, ty: Ty, offset: TextSize) -> Option<Hint> {
        let ret = self.shown(self.result_of(ty)?, body)?;
        Some(Hint {
            offset,
            label: format!(" -> {ret}"),
            edit: None,
        })
    }

    /// `: T` at the end of each stage of the pipeline whose outermost pipe
    /// is `node`, when it spans lines (X §7.1, D-1275). One hint per line:
    /// a stage that shares its line with the next one shows nothing. The
    /// last stage is skipped when the `let` it is the value of already
    /// shows the same type.
    fn pipeline(&self, body: &Body<'_>, node: SyntaxNode<'_>, out: &mut Vec<Hint>) {
        if !self.switches.pipe_stages || !is_pipe(node) || is_inner_pipe(node) {
            return;
        }
        let mut stages = Vec::new();
        let mut cur = Some(node);
        while let Some(stage) = cur.filter(|&n| is_pipe(n)) {
            stages.push(stage);
            cur = BinExpr::cast(stage)
                .and_then(BinExpr::lhs)
                .map(AstNode::syntax);
        }
        stages.reverse();
        let text = node.tree().text();
        let range = node_significant(node);
        let spans_lines = range
            .slice(text)
            .is_some_and(|between| between.contains('\n'));
        if !spans_lines {
            return;
        }
        let last = stages.len().saturating_sub(1);
        for (i, &stage) in stages.iter().enumerate() {
            let end = node_significant(stage).end();
            let next_on_line = stages.get(i.saturating_add(1)).is_some_and(|&next| {
                TextRange::new(end, node_significant(next).end())
                    .slice(text)
                    .is_some_and(|between| !between.contains('\n'))
            });
            if next_on_line {
                continue;
            }
            let Some(ty) = body.facts.expr(stage.id()).map(|fact| fact.ty) else {
                continue;
            };
            if i == last && self.let_shows(body, node, ty) {
                continue;
            }
            if let Some(shown) = self.shown(ty, body) {
                out.push(Hint {
                    offset: end,
                    label: format!(": {shown}"),
                    edit: None,
                });
            }
        }
    }

    /// Whether `value` is the value of a `let x = …` that shows `ty`: by
    /// its annotation, or by its own hint.
    fn let_shows(&self, body: &Body<'_>, value: SyntaxNode<'_>, ty: Ty) -> bool {
        let Some(stmt) = value.parent().and_then(LetLike::of) else {
            return false;
        };
        let Some(pattern) = stmt.pattern.filter(|p| p.kind() == K::IDENT_PAT) else {
            return false;
        };
        if stmt.value != Some(value) {
            return false;
        }
        let Some(local) = body
            .res
            .bindings
            .get(&pattern.id())
            .and_then(|&id| body.facts.locals.get(id))
        else {
            return false;
        };
        local.ty == ty && (stmt.annotated || self.binder(body, pattern).is_some())
    }

    /// ` as dyn I` after a value converted to `dyn` (X §7.1). The type is
    /// the one the value's context expected, which names the interface's
    /// arguments too.
    fn conversion(&self, body: &Body<'_>, value: SyntaxNode<'_>, iface: DefId) -> Option<Hint> {
        let expected = body.facts.expr(value.id())?.expected?;
        let is_dyn = matches!(
            self.snapshot.globals().types.kind(expected),
            TyKind::Dyn { iface: i, .. } if *i == iface
        );
        if !is_dyn {
            return None;
        }
        let shown = self.shown(expected, body)?;
        Some(Hint {
            offset: node_significant(value).end(),
            label: format!(" as {shown}"),
            edit: None,
        })
    }
}

/// Whether the value of `let x = value`, of type `ty`, already shows
/// the type (X §7.1): a literal of its default type, `Int`, `Float` or
/// `String`, but not one inference made `U8`; or a record literal of a
/// type that is not generic, whose name it writes. A constructor call, a
/// unit constructor and a variant with named fields do not write their
/// type's name, so their `let` keeps the hint (D-1483).
fn type_on_line(snapshot: &Snapshot, body: &Body<'_>, value: SyntaxNode<'_>, ty: Ty) -> bool {
    let Some(expr) = Expr::cast(value) else {
        return false;
    };
    let literal = match expr {
        Expr::Literal(lit) => lit.kind(),
        Expr::Prefix(prefix) => prefix.negative_literal().and_then(Literal::kind),
        Expr::String(_) => return ty == Ty::STRING,
        Expr::Record(record) => {
            let TyKind::Named { def, args } = snapshot.globals().types.kind(ty) else {
                return false;
            };
            return args.is_empty()
                && record
                    .path()
                    .and_then(|path| body.res.paths.get(&path.syntax().id()))
                    .is_some_and(|res| names_type(snapshot, res, *def));
        }
        _ => None,
    };
    literal.is_some_and(|kind| match kind {
        LiteralKind::Int(_) => ty == Ty::INT,
        LiteralKind::Float(_) => ty == Ty::FLOAT,
        LiteralKind::RawString(_) => ty == Ty::STRING,
    })
}

/// Whether a record literal's path, resolved to `res`, writes the name of
/// the type `ty`: the type itself, or the single-variant shorthand's
/// constructor, which shares the type's name (§4.3.3).
fn names_type(snapshot: &Snapshot, res: &Res, ty: DefId) -> bool {
    let Res::Def(def) = *res else {
        return false;
    };
    if def == ty {
        return true;
    }
    let Some(iface) = snapshot.interfaces().get(def.module) else {
        return false;
    };
    let (Some(ctor), Some(type_item)) = (iface.item(def.index), iface.item(ty.index)) else {
        return false;
    };
    def.module == ty.module
        && ctor.kind == DefKind::Ctor
        && ctor.parent == Some(ty.index)
        && ctor.name == type_item.name
}

/// The kind of what declares `param`: `CLOSURE`, `LOCAL_FN`, or an item's
/// function.
fn param_owner(param: Param<'_>) -> Option<K> {
    Some(param.syntax().parent()?.parent()?.kind())
}

/// Whether `node` is a `|>` expression.
fn is_pipe(node: SyntaxNode<'_>) -> bool {
    BinExpr::cast(node).and_then(BinExpr::op) == Some(BinOp::Pipe)
}

/// Whether `node` is the left side of a `|>`, a stage of a longer
/// pipeline.
fn is_inner_pipe(node: SyntaxNode<'_>) -> bool {
    node.parent()
        .filter(|&parent| is_pipe(parent))
        .and_then(BinExpr::cast)
        .and_then(BinExpr::lhs)
        .is_some_and(|lhs| lhs.syntax() == node)
}

/// The range of `node` from its first to its last significant token.
fn node_significant(node: SyntaxNode<'_>) -> TextRange {
    let significant =
        |t: &cheby_syntax::SyntaxToken<'_>| !t.kind().is_trivia() && t.kind() != K::NEWLINE;
    let mut tokens = node.tokens().filter(significant);
    let Some(first) = tokens.next() else {
        return node.text_range();
    };
    let last = tokens.next_back().unwrap_or(first);
    TextRange::new(first.text_range().start(), last.text_range().end())
}

/// A `let` or a `let guard`: what its binders' rows read.
#[derive(Clone, Copy)]
struct LetLike<'t> {
    pattern: Option<SyntaxNode<'t>>,
    value: Option<SyntaxNode<'t>>,
    annotated: bool,
}

impl<'t> LetLike<'t> {
    /// `node` as a `let` or a `let guard`.
    fn of(node: SyntaxNode<'t>) -> Option<Self> {
        if let Some(stmt) = LetStmt::cast(node) {
            return Some(Self {
                pattern: stmt.pattern().map(AstNode::syntax),
                value: stmt.value().map(AstNode::syntax),
                annotated: stmt.ty().is_some(),
            });
        }
        let stmt = GuardStmt::cast(node).filter(|s| s.let_keyword().is_some())?;
        Some(Self {
            pattern: stmt.pattern().map(AstNode::syntax),
            value: stmt.value().map(AstNode::syntax),
            annotated: stmt.ty().is_some(),
        })
    }
}

/// What a binding pattern belongs to.
#[derive(Clone, Copy)]
enum Owner<'t> {
    /// A `let` or `let guard`.
    Let(LetLike<'t>),
    /// A binder of a `use`.
    Use(UseBinder<'t>),
    /// A parameter of a closure or a local function.
    Param(Param<'t>),
}

impl<'t> Owner<'t> {
    /// The nearest owner of the binding pattern `node`. `None` inside a
    /// `case` arm, whose binders get no hint.
    fn of(node: SyntaxNode<'t>) -> Option<Self> {
        node.ancestors().find_map(|a| match a.kind() {
            K::LET_STMT | K::GUARD_STMT => Some(LetLike::of(a).map(Self::Let)),
            K::USE_BINDER => Some(UseBinder::cast(a).map(Self::Use)),
            K::PARAM => Some(Param::cast(a).map(Self::Param)),
            K::CASE_ARM | K::COND_ARM | K::CLOSURE | K::LOCAL_FN | K::BLOCK => Some(None),
            _ => None,
        })?
    }

    /// Whether the binding has a written type.
    fn annotated(self) -> bool {
        match self {
            Self::Let(stmt) => stmt.annotated,
            Self::Use(binder) => binder.ty().is_some(),
            Self::Param(param) => param.ty().is_some(),
        }
    }
}

#[cfg(test)]
mod tests;
