//! Patterns (chapter 6, C4, C5).
//!
//! A pattern is checked against the type of the values it matches, which
//! its context already knows or leaves as a variable: the subject of a
//! `case` arm, the value of a `let`, a parameter's type. Checking binds
//! the pattern's names to their types and builds THIR. Whether a pattern
//! may be refutable is a property of the finished pattern and its closed
//! type, so it is checked later, on THIR (`crate::exhaust`), like
//! exhaustiveness.

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{CtorFields, Ice, Ty, builtin};
use cheby_resolve::Res;
use cheby_syntax::NodeId;
use cheby_syntax::ast::{
    self, AstNode, AstToken, CtorPatFields, FieldPatEntry, ListPatItem, LiteralPatKind,
};

use super::BodyCx;
use super::record::{CtorInfo, Structure, plural, quoted_list, unlisted};
use crate::infer::{InferTy, VarKind};
use crate::thir::{self, Lit, PatKind};

/// Where a pattern is checked, which decides whether it may be refutable
/// (§6.1, D-242) and how errors about it are worded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PatSite {
    /// `let pat = …`.
    Let,
    /// `let assert pat = …`.
    LetAssert,
    /// A `case` arm, one pattern per subject.
    CaseArm,
    /// A closure or local-function parameter.
    Param,
    /// A `use` binder (§5.10).
    UseBinder,
}

impl PatSite {
    /// What the type a root pattern is checked against belongs to, for
    /// the note of a mismatch.
    const fn matched(self) -> &'static str {
        match self {
            Self::Let | Self::LetAssert => "the value after `=`",
            Self::CaseArm => "its subject",
            Self::Param => "the parameter",
            Self::UseBinder => "the value the function passes to the rest of the block",
        }
    }
}

/// The pattern checker's state in a body. W2-E2 may add fields.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PatState;

/// How one pattern is being checked.
#[derive(Clone, Debug)]
struct PatCx {
    site: PatSite,
    /// Where the root pattern's expected type comes from, for E0047.
    origin: Option<(Span, String)>,
    /// Whether this is the root pattern (or an alternative or `as` of it).
    root: bool,
}

impl PatCx {
    /// The context of a sub-pattern.
    fn nested(&self) -> Self {
        Self {
            site: self.site,
            origin: None,
            root: false,
        }
    }
}

impl BodyCx<'_> {
    /// Checks `pat` against the type of the values it matches, binding
    /// its names (their types are set with [`BodyCx::set_local`]), and
    /// returns its THIR. The pattern gets an id from
    /// [`BodyCx::new_pat_id`] with its inference type, and its `ty` stays
    /// `Ty::ERROR` until `finish`. Refutability is checked later, on THIR
    /// (`crate::exhaust`). An error is reported here and leaves the body
    /// errored; this never returns an [`Ice`] for a user error.
    pub fn check_pat(
        &mut self,
        pat: ast::Pat<'_>,
        expected: &InferTy,
        site: PatSite,
    ) -> Result<thir::Pat, Ice> {
        self.check_pat_from(pat, expected, site, None)
    }

    /// [`Self::check_pat`] with the origin of the expected type, for the
    /// secondary label of a mismatch at the root pattern.
    pub(super) fn check_pat_from(
        &mut self,
        pat: ast::Pat<'_>,
        expected: &InferTy,
        site: PatSite,
        origin: Option<(Span, String)>,
    ) -> Result<thir::Pat, Ice> {
        let cx = PatCx {
            site,
            origin,
            root: true,
        };
        self.pat(pat, expected, &cx)
    }

    fn pat(&mut self, pat: ast::Pat<'_>, expected: &InferTy, cx: &PatCx) -> Result<thir::Pat, Ice> {
        let span = pat.significant_span();
        let kind = match pat {
            ast::Pat::Paren(p) => {
                if let Some(inner) = p.inner() {
                    return self.pat(inner, expected, cx);
                }
                // A syntax error.
                self.errored = true;
                PatKind::Wild
            }
            ast::Pat::Wildcard(_) => PatKind::Wild,
            ast::Pat::Ident(p) => PatKind::Bind(self.bind_node(p.syntax().id(), expected, span)?),
            ast::Pat::Literal(p) => self.literal_pat(p, expected, cx)?,
            ast::Pat::Ctor(p) => self.ctor_pat(p, expected, cx)?,
            ast::Pat::Tuple(p) => self.tuple_pat(p, expected, cx)?,
            ast::Pat::List(p) => self.list_pat(p, expected, cx)?,
            ast::Pat::Or(p) => {
                // One type across the alternatives (§6.7): each is checked
                // against the same type, and a name bound in several of
                // them is one local, unified in `bind_node`.
                let mut alts = Vec::new();
                for alt in p.alternatives() {
                    alts.push(self.pat(alt, expected, cx)?);
                }
                PatKind::Or(alts.into())
            }
            ast::Pat::As(p) => {
                let inner = if let Some(inner) = p.pattern() {
                    self.pat(inner, expected, cx)?
                } else {
                    self.errored = true;
                    self.make_pat(PatKind::Wild, expected, span)?
                };
                let name_span = p.name().map_or(span, AstToken::span);
                let local = self.bind_node(p.syntax().id(), expected, name_span)?;
                PatKind::As {
                    pat: Box::new(inner),
                    local,
                }
            }
        };
        self.make_pat(kind, expected, span)
    }

    fn make_pat(&mut self, kind: PatKind, ty: &InferTy, span: Span) -> Result<thir::Pat, Ice> {
        let id = self.new_pat_id(ty.clone(), span)?;
        Ok(thir::Pat {
            id,
            kind,
            ty: Ty::ERROR,
            span,
        })
    }

    /// Binds the local of the binding node `node` to `ty`. A name bound in
    /// several alternatives is one local (§6.7): a later alternative's
    /// binding must have the same type as the first one.
    fn bind_node(
        &mut self,
        node: NodeId,
        ty: &InferTy,
        span: Span,
    ) -> Result<cheby_resolve::LocalId, Ice> {
        let local = self
            .res
            .bindings
            .get(&node)
            .copied()
            .ok_or_else(|| Ice::new("a pattern binding was not resolved"))?;
        let previous = self.locals.try_get(local, "local")?.clone();
        match previous {
            None => self.set_local(local, ty.clone())?,
            Some(previous) => {
                if self.infcx.unify(&previous, ty)?.is_err() {
                    let decl = self.res.locals.try_get(local, "local")?;
                    let name = self.input.globals.names.get(decl.name);
                    let first = decl.span;
                    let label = format!("`{name}` is `{}` here", self.show(ty));
                    let earlier = format!(
                        "`{name}` is `{}` in an earlier alternative",
                        self.show(&previous)
                    );
                    self.report(
                        Diagnostic::new(codes::TYPE_MISMATCH, "mismatched types")
                            .with_primary(span, label)
                            .with_secondary(first, earlier)
                            .with_note(
                                "a name bound by several alternatives must have the same type \
                                 in each (§6.7)",
                            ),
                    );
                }
            }
        }
        Ok(local)
    }

    /// Whether `expected` is the error type. A pattern checked against it
    /// unifies with anything, so its sub-patterns are given the error type
    /// too instead of fresh variables nothing would determine (no E0057
    /// cascade).
    fn is_error(&mut self, expected: &InferTy) -> Result<bool, Ice> {
        Ok(self.infcx.shallow(expected)? == InferTy::ERROR)
    }

    /// Unifies the type a pattern matches, `found`, with the expected one.
    /// A mismatch is E0047 at `span`; returns whether they unified.
    fn pat_unify(
        &mut self,
        expected: &InferTy,
        found: &InferTy,
        span: Span,
        cx: &PatCx,
    ) -> Result<bool, Ice> {
        let Err(mismatch) = self.infcx.unify(expected, found)? else {
            return Ok(true);
        };
        let label = format!(
            "expected `{}`, found `{}`",
            self.show(expected),
            self.show(found)
        );
        let mut diag =
            Diagnostic::new(codes::TYPE_MISMATCH, "mismatched types").with_primary(span, label);
        if cx.root {
            if let Some((origin, text)) = cx.origin.clone() {
                diag = diag.with_secondary(origin, text);
            }
            diag = diag.with_note(format!(
                "a pattern matches values of the type of {}",
                cx.site.matched()
            ));
        }
        if mismatch == crate::infer::Mismatch::Occurs {
            diag = diag.with_note("the type would have to contain itself");
        }
        self.report(diag);
        Ok(false)
    }

    /// `1`, `-1`, `"text"`, a raw string (§6.3).
    fn literal_pat(
        &mut self,
        p: ast::LiteralPat<'_>,
        expected: &InferTy,
        cx: &PatCx,
    ) -> Result<PatKind, Ice> {
        let span = p.significant_span();
        let text = match p.kind() {
            Some(LiteralPatKind::Int(int)) => return self.int_pat(p, int, expected, cx),
            Some(LiteralPatKind::String(s)) => s.plain_value(),
            Some(LiteralPatKind::RawString(raw)) => Some(raw.value().into_owned()),
            // Float patterns (E0033, D-113) and interpolated strings are
            // syntax errors, reported by validation.
            Some(LiteralPatKind::Float(_)) | None => None,
        };
        let Some(text) = text else {
            self.errored = true;
            return Ok(PatKind::Wild);
        };
        if !self.pat_unify(expected, &InferTy::STRING, span, cx)? {
            return Ok(PatKind::Wild);
        }
        Ok(PatKind::Lit(Lit::Str(text.into())))
    }

    /// An integer literal pattern, possibly negative (§5.3.1, D-470). Its
    /// type is the expected one, which must be an integer type.
    fn int_pat(
        &mut self,
        p: ast::LiteralPat<'_>,
        int: cheby_syntax::ast::IntLit<'_>,
        expected: &InferTy,
        cx: &PatCx,
    ) -> Result<PatKind, Ice> {
        let span = p.significant_span();
        let var = self.infcx.new_var(span, VarKind::IntLit)?;
        if !self.pat_unify(expected, &var, span, cx)? {
            return Ok(PatKind::Wild);
        }
        // A malformed literal was reported by the lexer.
        let Some(magnitude) = int.value() else {
            self.errored = true;
            return Ok(PatKind::Wild);
        };
        let value = if p.is_negative() {
            // `-m` for every `m` up to 2^127, which is `i128::MIN`.
            0i128.checked_sub_unsigned(magnitude)
        } else {
            i128::try_from(magnitude).ok()
        };
        if let Some(value) = value {
            // Whether it fits its type is checked with the other literals
            // at the end of the body, when the type is final (E0071).
            return Ok(PatKind::Lit(Lit::Int(value)));
        }
        let shown = self.show(&var);
        self.report(
            Diagnostic::new(
                codes::LITERAL_OUT_OF_RANGE,
                format!("literal out of range for `{shown}`"),
            )
            .with_primary(span, "this literal does not fit in any integer type"),
        );
        Ok(PatKind::Wild)
    }

    /// Checks the sub-patterns of a constructor pattern whose constructor
    /// is in error, so their names are bound (to the error type) and their
    /// own errors reported.
    fn ctor_fields_in_error(&mut self, p: ast::CtorPat<'_>, cx: &PatCx) -> Result<(), Ice> {
        let cx = cx.nested();
        match p.fields() {
            Some(CtorPatFields::Positional(list)) => {
                for sub in list.patterns() {
                    self.pat(sub, &InferTy::ERROR, &cx)?;
                }
            }
            Some(CtorPatFields::Named(list)) => {
                for field in list.fields() {
                    self.field_pat(field, &InferTy::ERROR, &cx)?;
                }
            }
            None => {}
        }
        Ok(())
    }

    /// One `name: pattern` or punned `name` of a named constructor pattern,
    /// against the field's type.
    fn field_pat(
        &mut self,
        field: ast::FieldPat<'_>,
        ty: &InferTy,
        cx: &PatCx,
    ) -> Result<thir::Pat, Ice> {
        if let Some(sub) = field.pattern() {
            return self.pat(sub, ty, cx);
        }
        // `Point { x }` binds `x` (D-113).
        let span = field
            .name()
            .map_or(field.significant_span(), AstToken::span);
        let local = self.bind_node(field.syntax().id(), ty, span)?;
        self.make_pat(PatKind::Bind(local), ty, span)
    }

    /// `None`, `Some(x)`, `Point { x, .. }` (§6.4).
    fn ctor_pat(
        &mut self,
        p: ast::CtorPat<'_>,
        expected: &InferTy,
        cx: &PatCx,
    ) -> Result<PatKind, Ice> {
        let span = p.significant_span();
        let Some(path) = p.path() else {
            self.errored = true;
            self.ctor_fields_in_error(p, cx)?;
            return Ok(PatKind::Wild);
        };
        let info = match self.path_res(path.syntax().id())? {
            Res::Def(def) => self.ctor_info(def)?,
            _ => None,
        };
        // An unresolved or invisible constructor was reported by the
        // resolver, an erroneous one with its declaration.
        let Some(info) = info else {
            self.errored = true;
            self.ctor_fields_in_error(p, cx)?;
            return Ok(PatKind::Wild);
        };
        let path_span = path.significant_span();
        if !self.check_structure(info.ty_def, Structure::Ctor(info.def), path_span)? {
            self.ctor_fields_in_error(p, cx)?;
            return Ok(PatKind::Wild);
        }
        let (ty, args) = self.adt_instance(info.ty_def, span)?;
        if !self.pat_unify(expected, &ty, span, cx)? || self.is_error(expected)? {
            self.silence(&args)?;
        }
        let nested = cx.nested();
        let fields = match (&info.fields, p.fields()) {
            (CtorFields::Unit, None) => Vec::new(),
            (CtorFields::Positional(tys), Some(CtorPatFields::Positional(list))) => {
                let tys: Vec<InferTy> = tys.iter().map(|&t| self.field_ty(t, &args)).collect();
                self.positional_fields(&info, &tys, list, &nested)?
            }
            (CtorFields::Named(_), Some(CtorPatFields::Named(list))) => {
                self.named_fields(&info, &args, list, &nested)?
            }
            (fields, written) => {
                self.ctor_shape_error(&info, fields, written, span);
                self.ctor_fields_in_error(p, cx)?;
                return Ok(PatKind::Wild);
            }
        };
        Ok(PatKind::Ctor {
            ctor: info.def,
            fields: fields.into(),
        })
    }

    /// E0048 or E0059 for a constructor pattern whose shape does not match
    /// the constructor's: fields on a unit constructor, none on one with
    /// fields, positional for named or the reverse.
    /// GAP(pattern-shape-codes): a count of fields that differs (none on a
    /// positional constructor, some on a unit one) is E0048, like the
    /// arity of a positional pattern; matching fields by the wrong form is
    /// E0059, like a named-field constructor called positionally (D-138).
    fn ctor_shape_error(
        &mut self,
        info: &CtorInfo,
        fields: &CtorFields,
        written: Option<CtorPatFields<'_>>,
        span: Span,
    ) {
        let name = info.name;
        let names = info.field_names(&self.input.globals.names);
        let named_example = match names.first() {
            Some(first) => format!("`{name} {{ {first}, .. }}`"),
            None => format!("`{name} {{ .. }}`"),
        };
        let (code, message, label, help) = match (fields, written) {
            (CtorFields::Unit, _) => (
                codes::WRONG_ARGUMENT_COUNT,
                format!("the constructor `{name}` has no fields"),
                "expected no fields".to_owned(),
                format!("write the constructor alone: `{name}`"),
            ),
            (CtorFields::Positional(tys), None) => {
                let blanks = vec!["_"; tys.len()].join(", ");
                (
                    codes::WRONG_ARGUMENT_COUNT,
                    format!(
                        "the constructor `{name}` has {} field{}, but the pattern has none",
                        tys.len(),
                        plural(tys.len())
                    ),
                    format!("expected {} field{}", tys.len(), plural(tys.len())),
                    format!("match the fields, or ignore them with `{name}({blanks})`"),
                )
            }
            (CtorFields::Positional(_), Some(_)) => (
                codes::INVALID_RECORD,
                format!("`{name}` has positional fields, not named ones"),
                "matched by name here".to_owned(),
                format!("match its fields by position: `{name}(…)`"),
            ),
            (CtorFields::Named(_), None) => (
                codes::INVALID_RECORD,
                format!("the pattern `{name}` does not list the fields of `{name}`"),
                "named fields are not listed".to_owned(),
                format!("list the fields, or ignore them with `..`: {named_example}"),
            ),
            (CtorFields::Named(_), Some(_)) => (
                codes::INVALID_RECORD,
                format!("`{name}` has named fields, not positional ones"),
                "matched by position here".to_owned(),
                format!("match its fields by name: {named_example}"),
            ),
        };
        let mut diag = Diagnostic::new(code, message)
            .with_primary(span, label)
            .with_help(help);
        if let Some(decl) = info.decl {
            diag = diag.with_secondary(decl, format!("`{name}` is declared here"));
        }
        self.report(diag);
    }

    /// The sub-patterns of `Some(x)`: one per field, in order (E0048).
    fn positional_fields(
        &mut self,
        info: &CtorInfo,
        tys: &[InferTy],
        list: ast::PosPatList<'_>,
        cx: &PatCx,
    ) -> Result<Vec<(u32, thir::Pat)>, Ice> {
        let subs: Vec<ast::Pat<'_>> = list.patterns().collect();
        if subs.len() != tys.len() {
            let mut diag = Diagnostic::new(
                codes::WRONG_ARGUMENT_COUNT,
                format!(
                    "the constructor `{}` has {} field{}, but the pattern has {}",
                    info.name,
                    tys.len(),
                    plural(tys.len()),
                    subs.len()
                ),
            )
            .with_primary(
                list.significant_span(),
                format!("expected {} field{}", tys.len(), plural(tys.len())),
            );
            if let Some(decl) = info.decl {
                diag = diag.with_secondary(decl, format!("`{}` is declared here", info.name));
            }
            self.report(diag);
        }
        let mut fields = Vec::with_capacity(subs.len());
        for (index, sub) in subs.into_iter().enumerate() {
            let ty = tys.get(index).cloned().unwrap_or(InferTy::ERROR);
            let index = u32::try_from(index).map_err(|_| Ice::new("too many fields"))?;
            fields.push((index, self.pat(sub, &ty, cx)?));
        }
        Ok(fields)
    }

    /// The sub-patterns of `Point { x, y: p, .. }`: listed fields only, by
    /// declaration index, in written order (E0058, E0059).
    fn named_fields(
        &mut self,
        info: &CtorInfo,
        args: &[InferTy],
        list: ast::FieldPatList<'_>,
        cx: &PatCx,
    ) -> Result<Vec<(u32, thir::Pat)>, Ice> {
        let all = info.field_names(&self.input.globals.names);
        let mut fields: Vec<(u32, thir::Pat)> = Vec::new();
        let mut seen: Vec<(u32, Span)> = Vec::new();
        let mut rest = false;
        for entry in list.entries() {
            let field = match entry {
                FieldPatEntry::Rest(_) => {
                    rest = true;
                    continue;
                }
                FieldPatEntry::Field(field) => field,
            };
            let Some(name) = field.name() else {
                self.errored = true;
                continue;
            };
            let Some((index, ty)) = info.named_field(&self.input.globals.names, name.text()) else {
                let mut diag = Diagnostic::new(
                    codes::UNKNOWN_FIELD,
                    format!("`{}` has no field `{}`", info.name, name.text()),
                )
                .with_primary(name.span(), "unknown field");
                if !all.is_empty() {
                    diag = diag.with_note(format!(
                        "the field{} of `{}`: {}",
                        plural(all.len()),
                        info.name,
                        quoted_list(&all)
                    ));
                }
                self.report(diag);
                self.field_pat(field, &InferTy::ERROR, cx)?;
                continue;
            };
            if let Some(&(_, first)) = seen.iter().find(|(i, _)| *i == index) {
                self.report(
                    Diagnostic::new(
                        codes::INVALID_RECORD,
                        format!("field `{}` is listed twice in this pattern", name.text()),
                    )
                    .with_primary(name.span(), "listed again here")
                    .with_secondary(first, "first listed here"),
                );
            } else {
                seen.push((index, name.span()));
            }
            let ty = self.field_ty(ty, args);
            fields.push((index, self.field_pat(field, &ty, cx)?));
        }
        if !rest {
            let missing = unlisted(&all, &seen);
            if !missing.is_empty() {
                self.report(
                    Diagnostic::new(
                        codes::INVALID_RECORD,
                        format!(
                            "the pattern does not list the field{} {} of `{}`",
                            plural(missing.len()),
                            quoted_list(&missing),
                            info.name
                        ),
                    )
                    .with_primary(
                        list.significant_span(),
                        format!("missing {}", quoted_list(&missing)),
                    )
                    .with_help(
                        "list the missing fields, or add `..` to ignore the fields not listed",
                    ),
                );
            }
        }
        Ok(fields)
    }

    /// `(a, b)` (§6.5).
    fn tuple_pat(
        &mut self,
        p: ast::TuplePat<'_>,
        expected: &InferTy,
        cx: &PatCx,
    ) -> Result<PatKind, Ice> {
        let span = p.significant_span();
        let subs: Vec<ast::Pat<'_>> = p.elements().collect();
        let vars = subs
            .iter()
            .map(|sub| self.infcx.new_var(sub.significant_span(), VarKind::General))
            .collect::<Result<Box<[InferTy]>, Ice>>()?;
        let ty = InferTy::Tuple(vars.clone());
        if !self.pat_unify(expected, &ty, span, cx)? || self.is_error(expected)? {
            self.silence(&vars)?;
        }
        let nested = cx.nested();
        let mut pats = Vec::with_capacity(subs.len());
        for (sub, var) in subs.into_iter().zip(vars.iter()) {
            pats.push(self.pat(sub, var, &nested)?);
        }
        Ok(PatKind::Tuple(pats.into()))
    }

    /// `[a, ..rest, z]` (§6.6, D-158).
    fn list_pat(
        &mut self,
        p: ast::ListPat<'_>,
        expected: &InferTy,
        cx: &PatCx,
    ) -> Result<PatKind, Ice> {
        let span = p.significant_span();
        let elem = self.infcx.new_var(span, VarKind::General)?;
        let list_ty = InferTy::Named {
            def: builtin::LIST,
            args: Box::new([elem.clone()]),
        };
        if !self.pat_unify(expected, &list_ty, span, cx)? || self.is_error(expected)? {
            self.silence(std::slice::from_ref(&elem))?;
        }
        let nested = cx.nested();
        let mut prefix = Vec::new();
        let mut suffix = Vec::new();
        let mut rest: Option<Box<thir::Pat>> = None;
        for item in p.items() {
            match item {
                ListPatItem::Pat(sub) => {
                    let checked = self.pat(sub, &elem, &nested)?;
                    if rest.is_some() {
                        suffix.push(checked);
                    } else {
                        prefix.push(checked);
                    }
                }
                ListPatItem::Rest(spread) => {
                    let spread_span = spread.significant_span();
                    if rest.is_some() {
                        // A second spread is a syntax error (E0032, D-158).
                        self.errored = true;
                        if spread.name().is_some() {
                            self.bind_node(spread.syntax().id(), &InferTy::ERROR, spread_span)?;
                        }
                        continue;
                    }
                    let kind = match spread.name() {
                        Some(_) => PatKind::Bind(self.bind_node(
                            spread.syntax().id(),
                            &list_ty,
                            spread_span,
                        )?),
                        None => PatKind::Wild,
                    };
                    rest = Some(Box::new(self.make_pat(kind, &list_ty, spread_span)?));
                }
            }
        }
        Ok(PatKind::List {
            prefix: prefix.into(),
            rest,
            suffix: suffix.into(),
        })
    }
}

#[cfg(test)]
mod tests;
