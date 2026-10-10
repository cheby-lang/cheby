//! Exhaustiveness, reachability and irrefutability (C6, part of C5).
//!
//! [`check_body`] runs on a finished, error-free THIR body (§4.4 of the
//! front-end plan) and reports:
//!
//! - E0061 for a `case` with subjects that does not cover every value
//!   (§5.9.4, §6.9, D-022), with up to three missing values written as
//!   patterns, and W0010 for each arm that can never match.
//! - E0060 for a refutable pattern where an irrefutable one is required:
//!   `let`, `use` binders and closure and local-function parameters (§6.1,
//!   §5.7, §5.10, D-242).
//! - W0008 for a `let assert` whose pattern always matches (§11.4, D-159).
//!
//! A subjectless `case` is not checked: its final `_` arm is required by
//! the grammar (D-109, D-439).

//!
//! The matching itself is [`engine`], which knows nothing about THIR. This
//! file lowers THIR patterns and closed types into it. A type's
//! constructors come from interfaces only (D-246): the module's own
//! interface, its dependencies' and the builtin module's. A type whose
//! structure is not visible here (an opaque type of another package, a
//! `priv` type of another module, §4.3.4) has no constructors to match, so
//! only `_` or a binding covers it (§6.4, §6.9), like `Int`, `String`,
//! `Float`, functions, `dyn`, type parameters and external types.
//!
//! `Never` is treated like an opaque type (D-492). A
//! `case` over a `Never` subject therefore needs a `_` or binding arm,
//! which is the only pattern that type-checks against `Never` anyway
//! (§3.10), and `Option<Never>` needs a `Some(_)` arm. Treating `Never`
//! as an empty type would make every arm unreachable instead, which helps
//! nobody, since such a `case` never runs.

mod engine;

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{
    CtorFields, CtorSig, DefId, DefIndex, DepView, Globals, Ice, IfaceItem, ItemSig, ModuleId,
    ModuleInterface, ModuleTable, PackageId, Ty, TyKind, TypeDefKind, builtin, structure_visible,
};

use crate::TypesInput;
use crate::infer::{contains_error, substitute};
use crate::thir::{self, ExprId, ExprKind, Lit, ParamOrigin, PatKind, Stmt};
use engine::{Ctor, ListLen, LitKey, Pat, Shape, Space, Types};

/// How many missing values a diagnostic lists before "and N more".
const SHOWN: usize = 3;

/// Checks every `case`, `let`, `let assert` and parameter pattern of
/// `body`, which has no other errors and closed types.
///
/// # Errors
///
/// An [`Ice`] when the body breaks a THIR invariant (an expression id
/// out of range, a constructor that is not one, a `Float` pattern).
pub(crate) fn check_body(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    body: &thir::Body,
) -> Result<Vec<Diagnostic>, Ice> {
    let cx = TyCx {
        module: input.module,
        package: input.package,
        deps: &input.deps,
        modules: input.modules,
        globals: input.globals,
        own,
    };
    check(&cx, body)
}

/// [`check_body`] with the parts of [`TypesInput`] it reads.
fn check(cx: &TyCx<'_>, body: &thir::Body) -> Result<Vec<Diagnostic>, Ice> {
    let mut diags = Vec::new();
    let mut stack: Vec<ExprId> = vec![body.value];
    while let Some(id) = stack.pop() {
        let expr = body.exprs.try_get(id, "expression")?;
        match &expr.kind {
            ExprKind::Case { subjects, arms } if !subjects.is_empty() => {
                check_case(cx, body, subjects, arms, &mut diags)?;
            }
            ExprKind::Block { stmts, .. } => {
                for stmt in stmts {
                    match stmt {
                        Stmt::Let { pat, .. } => {
                            check_irrefutable(cx, pat, Site::Let, &mut diags)?;
                        }
                        Stmt::LetAssert { pat, .. } => check_let_assert(cx, pat, &mut diags)?,
                        Stmt::Expr(_) | Stmt::Assert { .. } => {}
                    }
                }
            }
            ExprKind::Closure { params, .. } | ExprKind::LocalFn { params, .. } => {
                for param in params {
                    let site = match param.origin {
                        ParamOrigin::Use => Site::UseBinder,
                        _ => Site::Param,
                    };
                    check_irrefutable(cx, &param.pat, site, &mut diags)?;
                }
            }
            _ => {}
        }
        // Reversed, so that the walk visits children in order.
        stack.extend(expr.kind.children().into_iter().rev());
    }
    Ok(diags)
}

/// Where an irrefutable pattern is required, for the wording of E0060.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Site {
    Let,
    Param,
    UseBinder,
}

impl Site {
    const fn message(self) -> &'static str {
        match self {
            Self::Let => "refutable pattern in `let`",
            Self::Param => "refutable pattern in a parameter",
            Self::UseBinder => "refutable pattern in a `use` binder",
        }
    }

    /// The help for alternatives that cover every value only together.
    const fn alternatives_help(self) -> &'static str {
        match self {
            Self::Let => "match the value with `case`, with one arm per alternative",
            Self::Param => {
                "take the parameter as a plain name and match it with `case` in the body"
            }
            Self::UseBinder => "bind a plain name and match it with `case` after the `use`",
        }
    }
}

fn check_case(
    cx: &TyCx<'_>,
    body: &thir::Body,
    subjects: &[ExprId],
    arms: &[thir::Arm],
    diags: &mut Vec<Diagnostic>,
) -> Result<(), Ice> {
    let mut tys = Vec::with_capacity(subjects.len());
    let mut spans = Vec::with_capacity(subjects.len());
    for &subject in subjects {
        let expr = body.exprs.try_get(subject, "case subject")?;
        tys.push(expr.ty);
        spans.push(expr.span);
    }
    if tys.iter().any(|&ty| cx.erroneous(ty)) {
        return Ok(());
    }
    let mut lowered = Vec::with_capacity(arms.len());
    for arm in arms {
        let pats = arm
            .pats
            .iter()
            .map(|pat| cx.lower(pat))
            .collect::<Result<Vec<_>, Ice>>()?;
        lowered.push(pats);
    }
    let engine_arms: Vec<engine::Arm<'_>> = lowered
        .iter()
        .zip(arms)
        .map(|(pats, arm)| engine::Arm {
            pats,
            guarded: arm.guard.is_some(),
        })
        .collect();
    let report = engine::check(cx, &tys, &engine_arms)?;

    if !report.missing.is_empty() {
        let shown = report
            .missing
            .iter()
            .map(|row| engine::render_row(cx, row))
            .collect::<Result<Vec<_>, Ice>>()?;
        let catch_all = vec!["_"; subjects.len()].join(", ");
        let help = match shown.as_slice() {
            [only] if *only == catch_all => format!("add a `{catch_all}` arm"),
            [only] => format!("add an arm for `{only}`"),
            _ => format!("add an arm for each missing value, or a `{catch_all}` arm"),
        };
        let mut diag = Diagnostic::new(
            codes::NON_EXHAUSTIVE_CASE,
            "`case` does not cover every value",
        )
        .with_primary(
            cover(&spans)?,
            format!("{} not covered", list(&shown, report.more)),
        );
        if arms.iter().any(|arm| arm.guard.is_some()) {
            diag =
                diag.with_note("an arm with a guard does not count, since its guard may be false");
        }
        if report.too_complex {
            diag = diag.with_note("the patterns are too complex to check in full");
        }
        diags.push(diag.with_help(help));
    }

    for &index in &report.unreachable {
        let arm = arms
            .get(index)
            .ok_or_else(|| Ice::new("exhaustiveness: no such arm"))?;
        let spans: Vec<Span> = arm.pats.iter().map(|pat| pat.span).collect();
        let span = if spans.is_empty() {
            arm.span
        } else {
            cover(&spans)?
        };
        diags.push(
            Diagnostic::new(codes::UNREACHABLE_ARM, "this arm can never match")
                .with_primary(span, "the arms before it match every value it matches")
                .with_help("remove the arm, or move it before the arms that cover it"),
        );
    }
    Ok(())
}

fn check_irrefutable(
    cx: &TyCx<'_>,
    pat: &thir::Pat,
    site: Site,
    diags: &mut Vec<Diagnostic>,
) -> Result<(), Ice> {
    if cx.erroneous(pat.ty) {
        return Ok(());
    }
    if cx.irrefutable(pat)? {
        return Ok(());
    }
    let pats = [cx.lower(pat)?];
    let report = engine::check(
        cx,
        &[pat.ty],
        &[engine::Arm {
            pats: &pats,
            guarded: false,
        }],
    )?;
    let Some(first) = report.missing.first() else {
        // The alternatives cover every value together, but none of them
        // does alone, so §6.1 calls the pattern refutable.
        diags.push(
            Diagnostic::new(codes::REFUTABLE_PATTERN, site.message())
                .with_primary(pat.span, "its alternatives match every value only together")
                .with_note(
                    "alternatives make a pattern irrefutable only when one of them is \
                     irrefutable on its own (§6.1)",
                )
                .with_help(site.alternatives_help()),
        );
        return Ok(());
    };
    let shown = report
        .missing
        .iter()
        .map(|row| engine::render_row(cx, row))
        .collect::<Result<Vec<_>, Ice>>()?;
    let example = engine::render_row(cx, first)?;
    let message = site.message();
    let help = match site {
        Site::Let => format!(
            "use `case` to handle `{example}` as well, or `let assert` to panic when the \
             value does not match"
        ),
        Site::Param => format!(
            "take the parameter as a plain name and use `case` in the body to handle \
             `{example}` as well, or `let assert` to panic when it does not match"
        ),
        Site::UseBinder => format!(
            "bind a plain name and use `case` after the `use` to handle `{example}` as \
             well, or `let assert` to panic when it does not match"
        ),
    };
    diags.push(
        Diagnostic::new(codes::REFUTABLE_PATTERN, message)
            .with_primary(
                pat.span,
                format!("{} not covered", list(&shown, report.more)),
            )
            .with_note("this pattern must match every value of its type (§6.1)")
            .with_help(help),
    );
    Ok(())
}

fn check_let_assert(
    cx: &TyCx<'_>,
    pat: &thir::Pat,
    diags: &mut Vec<Diagnostic>,
) -> Result<(), Ice> {
    if cx.erroneous(pat.ty) {
        return Ok(());
    }
    if cx.irrefutable(pat)? {
        diags.push(
            Diagnostic::new(
                codes::IRREFUTABLE_LET_ASSERT,
                "`let assert` with a pattern that always matches",
            )
            .with_primary(pat.span, "this pattern matches every value of its type")
            .with_help("use `let` instead, since the assertion can never fail"),
        );
    }
    Ok(())
}

/// "`A`", "`A` and `B`", "`A`, `B` and `C`", "`A`, `B`, `C` and 2 more".
fn list(shown: &[String], more: bool) -> String {
    let quoted: Vec<String> = shown.iter().take(SHOWN).map(|s| format!("`{s}`")).collect();
    let rest = shown.len().saturating_sub(SHOWN);
    match (quoted.as_slice(), rest, more) {
        ([only], 0, false) => only.clone(),
        ([init @ .., last], 0, false) => format!("{} and {last}", init.join(", ")),
        (_, _, true) => format!("{} and more", quoted.join(", ")),
        _ => format!("{} and {rest} more", quoted.join(", ")),
    }
}

/// The span from the start of the first span to the end of the last one.
fn cover(spans: &[Span]) -> Result<Span, Ice> {
    let (Some(first), Some(last)) = (spans.first(), spans.last()) else {
        return Err(Ice::new("exhaustiveness: nothing to point at"));
    };
    Ok(Span::new(first.file, first.range.cover(last.range)))
}

/// The engine's view of closed types, through interfaces only: the parts
/// of [`TypesInput`] that matching needs.
struct TyCx<'a> {
    module: ModuleId,
    package: PackageId,
    deps: &'a DepView<'a>,
    modules: &'a ModuleTable,
    globals: &'a Globals,
    own: &'a ModuleInterface,
}

impl TyCx<'_> {
    fn kind(&self, ty: Ty) -> &'static TyKind {
        self.globals.types.kind(ty)
    }

    /// Whether `ty` contains the error type. The body has no errors, but
    /// a type in its signature may: an unknown type, or a wrong number of
    /// type arguments, already reported where the signature is checked.
    /// Matching over such a type is not checked at all, so that no E0060,
    /// E0061, W0008 or W0010 follows from that error (D-494).
    fn erroneous(&self, ty: Ty) -> bool {
        contains_error(&self.globals.types, ty)
    }

    fn item(&self, def: DefId) -> Result<&IfaceItem, Ice> {
        let item = if def.module == self.module {
            self.own.item(def.index)
        } else {
            self.deps.item(def)
        };
        item.ok_or_else(|| Ice::new(format!("exhaustiveness: no interface item for {def:?}")))
    }

    /// The constructors of the ADT `def`, whether or not they are visible.
    fn ctors(&self, def: DefId) -> Result<Option<&[DefIndex]>, Ice> {
        Ok(match &self.item(def)?.sig {
            ItemSig::Type(ty) => match &ty.kind {
                TypeDefKind::Adt { ctors } => Some(ctors),
                TypeDefKind::Builtin(_) | TypeDefKind::External => None,
            },
            _ => None,
        })
    }

    /// Whether this module may match on the constructors of type `def`
    /// (§4.3.4, D-048, D-151).
    fn structure_visible(&self, def: DefId) -> Result<bool, Ice> {
        let item = self.item(def)?;
        let package = self.modules.try_get(def.module)?.package;
        Ok(structure_visible(
            item,
            def.module,
            package,
            self.module,
            self.package,
        ))
    }

    fn ctor_sig(&self, ctor: DefId) -> Result<&CtorSig, Ice> {
        match &self.item(ctor)?.sig {
            ItemSig::Ctor(sig) => Ok(sig),
            _ => Err(Ice::new(format!(
                "exhaustiveness: {ctor:?} is not a constructor"
            ))),
        }
    }

    /// The constructor with tag `tag` of the ADT `def`.
    fn ctor_def(&self, def: DefId, tag: u32) -> Result<DefId, Ice> {
        let index = self
            .ctors(def)?
            .and_then(|ctors| ctors.get(usize::try_from(tag).ok()?))
            .ok_or_else(|| Ice::new(format!("exhaustiveness: no variant {tag} of {def:?}")))?;
        Ok(DefId::new(def.module, *index))
    }

    /// A constructor's name as users write it here (D-463): prelude and
    /// own constructors bare, others as `<module>::Name` (D-507).
    fn ctor_name(&self, ctor: DefId) -> Result<String, Ice> {
        let name = self.globals.names.get(self.item(ctor)?.name);
        if ctor.module == ModuleId::BUILTIN || ctor.module == self.module {
            return Ok(name.to_owned());
        }
        let info = self.modules.try_get(ctor.module)?;
        let segment = info.path.segments().last().map_or("?", String::as_str);
        Ok(format!("{segment}::{name}"))
    }

    /// Whether `pat` is irrefutable by the rules of §6.1: `_` and
    /// bindings, a constructor of a type with exactly one variant with
    /// irrefutable fields, a tuple of irrefutable patterns, `[..]` and
    /// `[..rest]`, alternatives with at least one irrefutable
    /// alternative, and `p as x` with `p` irrefutable. Alternatives that
    /// cover every value only together (`True | False`) do not count.
    fn irrefutable(&self, pat: &thir::Pat) -> Result<bool, Ice> {
        Ok(match &pat.kind {
            PatKind::Wild | PatKind::Bind(_) => true,
            PatKind::Lit(_) => false,
            PatKind::Ctor { fields, .. } => {
                let TyKind::Named { def, .. } = self.kind(pat.ty) else {
                    return Err(Ice::new(
                        "exhaustiveness: a constructor at a type without it",
                    ));
                };
                let one = self.ctors(*def)?.is_some_and(|ctors| ctors.len() == 1);
                one && self.all_irrefutable(fields.iter().map(|(_, p)| p))?
            }
            PatKind::Tuple(elems) => self.all_irrefutable(elems.iter())?,
            PatKind::List {
                prefix,
                rest,
                suffix,
            } => rest.is_some() && prefix.is_empty() && suffix.is_empty(),
            PatKind::Or(alts) => {
                for alt in alts {
                    if self.irrefutable(alt)? {
                        return Ok(true);
                    }
                }
                false
            }
            PatKind::As { pat, .. } => self.irrefutable(pat)?,
        })
    }

    fn all_irrefutable<'p>(&self, pats: impl Iterator<Item = &'p thir::Pat>) -> Result<bool, Ice> {
        for pat in pats {
            if !self.irrefutable(pat)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The engine pattern of a THIR pattern.
    ///
    /// A pattern at the error type lowers to a wildcard, since that type
    /// has no constructors to split (D-494). The
    /// callers skip such columns already; this keeps the engine safe.
    fn lower(&self, pat: &thir::Pat) -> Result<Pat, Ice> {
        if matches!(self.kind(pat.ty), TyKind::Error) {
            return Ok(Pat::Wild);
        }
        Ok(match &pat.kind {
            PatKind::Wild | PatKind::Bind(_) => Pat::Wild,
            PatKind::Lit(Lit::Int(n)) => Pat::Ctor(Ctor::Lit(LitKey::Int(*n)), Vec::new()),
            PatKind::Lit(Lit::Str(s)) => Pat::Ctor(Ctor::Lit(LitKey::Str(s.clone())), Vec::new()),
            PatKind::Lit(Lit::Float(_)) => {
                return Err(Ice::new("exhaustiveness: a float pattern (D-113)"));
            }
            PatKind::Ctor { ctor, fields } => {
                let sig = self.ctor_sig(*ctor)?;
                let count = match &sig.fields {
                    CtorFields::Unit => 0,
                    CtorFields::Positional(tys) => tys.len(),
                    CtorFields::Named(fields) => fields.len(),
                };
                let mut lowered = vec![Pat::Wild; count];
                for (index, field) in fields {
                    let slot = usize::try_from(*index)
                        .ok()
                        .and_then(|i| lowered.get_mut(i))
                        .ok_or_else(|| Ice::new("exhaustiveness: no such field"))?;
                    *slot = self.lower(field)?;
                }
                Pat::Ctor(Ctor::Variant(sig.tag), lowered)
            }
            PatKind::Tuple(elems) => Pat::Ctor(
                Ctor::Variant(0),
                elems
                    .iter()
                    .map(|p| self.lower(p))
                    .collect::<Result<_, _>>()?,
            ),
            PatKind::List {
                prefix,
                rest,
                suffix,
            } => {
                let count = |pats: &[thir::Pat]| {
                    u32::try_from(pats.len())
                        .map_err(|_| Ice::new("exhaustiveness: list pattern too long"))
                };
                let len = match rest {
                    None => ListLen::Fixed(
                        count(prefix)?
                            .checked_add(count(suffix)?)
                            .ok_or_else(|| Ice::new("exhaustiveness: list pattern too long"))?,
                    ),
                    Some(_) => ListLen::Var {
                        prefix: count(prefix)?,
                        suffix: count(suffix)?,
                    },
                };
                let fields = prefix
                    .iter()
                    .chain(suffix.iter())
                    .map(|p| self.lower(p))
                    .collect::<Result<_, _>>()?;
                Pat::Ctor(Ctor::List(len), fields)
            }
            PatKind::Or(alts) => Pat::Or(
                alts.iter()
                    .map(|p| self.lower(p))
                    .collect::<Result<_, _>>()?,
            ),
            PatKind::As { pat, .. } => self.lower(pat)?,
        })
    }
}

impl Types for TyCx<'_> {
    type Ty = Ty;

    fn space(&self, ty: Ty) -> Result<Space, Ice> {
        Ok(match self.kind(ty) {
            TyKind::Tuple(_) => Space::Variants(1),
            TyKind::Named { def, .. } if *def == builtin::LIST => Space::List,
            TyKind::Named { def, .. } => match self.ctors(*def)? {
                Some(ctors) if self.structure_visible(*def)? => Space::Variants(
                    u32::try_from(ctors.len())
                        .map_err(|_| Ice::new("exhaustiveness: too many variants"))?,
                ),
                _ => Space::Infinite,
            },
            TyKind::Error
            | TyKind::Never
            | TyKind::Fn { .. }
            | TyKind::Param(_)
            | TyKind::Dyn { .. } => Space::Infinite,
        })
    }

    fn field_tys(&self, ty: Ty, ctor: &Ctor) -> Result<Vec<Ty>, Ice> {
        let types = &self.globals.types;
        match (self.kind(ty), ctor) {
            (_, Ctor::Lit(_)) => Ok(Vec::new()),
            (TyKind::Tuple(elems), Ctor::Variant(0)) => Ok(elems.to_vec()),
            (TyKind::Named { def, args }, Ctor::List(len)) if *def == builtin::LIST => {
                let elem = args.first().copied().unwrap_or(Ty::ERROR);
                let arity = match *len {
                    ListLen::Fixed(n) => Some(n),
                    ListLen::Var { prefix, suffix } => prefix.checked_add(suffix),
                }
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| Ice::new("exhaustiveness: list pattern too long"))?;
                Ok(vec![elem; arity])
            }
            (TyKind::Named { def, args }, Ctor::Variant(tag)) => {
                let sig = self.ctor_sig(self.ctor_def(*def, *tag)?)?;
                Ok(match &sig.fields {
                    CtorFields::Unit => Vec::new(),
                    CtorFields::Positional(tys) => {
                        tys.iter().map(|&t| substitute(types, t, args)).collect()
                    }
                    CtorFields::Named(fields) => fields
                        .iter()
                        .map(|&(_, t)| substitute(types, t, args))
                        .collect(),
                })
            }
            _ => Err(Ice::new(format!(
                "exhaustiveness: constructor {ctor:?} at a type without it"
            ))),
        }
    }

    fn shape(&self, ty: Ty, tag: u32) -> Result<Shape, Ice> {
        let TyKind::Named { def, .. } = self.kind(ty) else {
            return Ok(Shape::Tuple);
        };
        let ctor = self.ctor_def(*def, tag)?;
        let name = self.ctor_name(ctor)?;
        Ok(match &self.ctor_sig(ctor)?.fields {
            CtorFields::Unit => Shape::Unit(name),
            CtorFields::Positional(_) => Shape::Positional(name),
            CtorFields::Named(fields) => Shape::Named(
                name,
                fields
                    .iter()
                    .map(|&(field, _)| self.globals.names.get(field).to_owned())
                    .collect(),
            ),
        })
    }
}

#[cfg(test)]
mod tests;
