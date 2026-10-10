//! Records (§5.11, C4): named-field construction, record update and field
//! access, with the opacity rules of §4.3.4 (D-048, D-151), D-071 for
//! fields of multi-variant types and D-213 for known types.
//!
//! This file also holds the view of constructors and their types that the
//! pattern checker (`pat.rs`) shares: [`CtorInfo`], [`BodyCx::ctor_info`],
//! [`BodyCx::adt_instance`] and the structure check of §4.3.4.

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{
    CtorFields, DefId, Ice, Interner, ItemSig, ModuleId, Name, Ty, TyKind, TypeDefKind, Vis,
    structure_visible,
};
use cheby_resolve::{LocalKind, Res};
use cheby_syntax::ast::{self, AstNode, AstToken, FieldInit, RecordEntry};

use super::{BodyCx, Expected};
use crate::infer::{InferTy, VarKind, instantiate};
use crate::thir::{ExprId, ExprKind, UpdateField};

/// A constructor whose signature and type checked, as the pattern and
/// record checkers see it.
#[derive(Clone, Debug)]
pub(super) struct CtorInfo {
    pub def: DefId,
    /// The type it builds, in the same module.
    pub ty_def: DefId,
    pub name: &'static str,
    /// Where it is declared, for secondary labels. `None` for builtin
    /// constructors.
    pub decl: Option<Span>,
    /// Field types use the type's parameters.
    pub fields: CtorFields,
}

impl CtorInfo {
    /// The declaration index and type of the named field `name`.
    pub fn named_field(&self, names: &Interner, name: &str) -> Option<(u32, Ty)> {
        let CtorFields::Named(fields) = &self.fields else {
            return None;
        };
        fields
            .iter()
            .position(|&(n, _)| names.get(n) == name)
            .and_then(|i| Some((u32::try_from(i).ok()?, fields.get(i)?.1)))
    }

    /// The names of the named fields, in declaration order.
    pub fn field_names(&self, names: &Interner) -> Vec<&'static str> {
        match &self.fields {
            CtorFields::Named(fields) => fields.iter().map(|&(n, _)| names.get(n)).collect(),
            CtorFields::Unit | CtorFields::Positional(_) => Vec::new(),
        }
    }
}

/// A field common to every variant: its index in each constructor and
/// its type (D-071).
type CommonField = (Box<[(DefId, u32)]>, Ty);

/// The named fields of `all` whose index is not in `seen`.
pub(super) fn unlisted<'n>(all: &[&'n str], seen: &[(u32, Span)]) -> Vec<&'n str> {
    all.iter()
        .enumerate()
        .filter(|&(i, _)| {
            !seen
                .iter()
                .any(|&(s, _)| usize::try_from(s).is_ok_and(|s| s == i))
        })
        .map(|(_, &name)| name)
        .collect()
}

/// A named type's definition and type arguments.
pub(super) type NamedParts = (DefId, Box<[InferTy]>);

/// What E0043 is about, for its wording.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Structure {
    /// A constructor in an expression or a pattern.
    Ctor(DefId),
    /// Fields read with `.name` or replaced by a record update.
    Fields,
}

/// "`a`", "`a` and `b`", "`a`, `b` and `c`".
pub(super) fn quoted_list(items: &[&str]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| format!("`{s}`")).collect();
    match quoted.as_slice() {
        [] => String::new(),
        [only] => only.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// `""` or `"s"`.
pub(super) const fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

impl BodyCx<'_> {
    // ---- constructors and their types ----------------------------------

    /// The constructor `def`, or `None` when it is not a constructor whose
    /// signature and type checked (an error reported elsewhere).
    pub(super) fn ctor_info(&self, def: DefId) -> Result<Option<CtorInfo>, Ice> {
        let item = self.input.item(self.own, def)?;
        let ItemSig::Ctor(sig) = &item.sig else {
            return Ok(None);
        };
        let ty_def = DefId::new(def.module, sig.ty);
        if !matches!(self.input.item(self.own, ty_def)?.sig, ItemSig::Type(_)) {
            return Ok(None);
        }
        Ok(Some(CtorInfo {
            def,
            ty_def,
            name: self.input.globals.names.get(item.name),
            decl: item.span,
            fields: sig.fields.clone(),
        }))
    }

    /// The constructors of the type `ty_def`, in declaration order, or
    /// `None` when it is not an ADT whose signatures checked.
    pub(super) fn adt_ctors(&self, ty_def: DefId) -> Result<Option<Vec<CtorInfo>>, Ice> {
        let ItemSig::Type(def) = &self.input.item(self.own, ty_def)?.sig else {
            return Ok(None);
        };
        let TypeDefKind::Adt { ctors } = &def.kind else {
            return Ok(None);
        };
        let mut out = Vec::with_capacity(ctors.len());
        for &index in ctors {
            match self.ctor_info(DefId::new(ty_def.module, index))? {
                Some(info) => out.push(info),
                None => return Ok(None),
            }
        }
        Ok(Some(out))
    }

    /// The type `ty_def` with a fresh variable for each of its parameters,
    /// and those variables, created at `span`.
    pub(super) fn adt_instance(
        &mut self,
        ty_def: DefId,
        span: Span,
    ) -> Result<(InferTy, Box<[InferTy]>), Ice> {
        let arity = match &self.input.item(self.own, ty_def)?.sig {
            ItemSig::Type(def) => def.generics.len(),
            _ => return Err(Ice::new("an instance of something that is not a type")),
        };
        let args = (0..arity)
            .map(|_| self.infcx.new_var(span, VarKind::General))
            .collect::<Result<Box<[InferTy]>, Ice>>()?;
        let ty = if args.is_empty() {
            InferTy::Known(self.input.globals.types.named(ty_def, []))
        } else {
            InferTy::Named {
                def: ty_def,
                args: args.clone(),
            }
        };
        Ok((ty, args))
    }

    /// The definition and type arguments of a named type, or `None` for
    /// any other type (a variable, a tuple, a function, `dyn`, a type
    /// parameter, `Never`, the error type).
    pub(super) fn named_parts(&mut self, ty: &InferTy) -> Result<Option<NamedParts>, Ice> {
        Ok(match self.infcx.resolve(ty)? {
            InferTy::Named { def, args } => Some((def, args)),
            InferTy::Known(t) => match self.input.globals.types.kind(t) {
                TyKind::Named { def, .. } => Some((*def, Box::default())),
                _ => None,
            },
            _ => None,
        })
    }

    /// `ty` with the type's parameters replaced by `args`.
    pub(super) fn field_ty(&self, ty: Ty, args: &[InferTy]) -> InferTy {
        instantiate(&self.input.globals.types, ty, args)
    }

    /// Makes each of `vars` an error type, so that types built from them
    /// after a reported mismatch stay silent.
    pub(super) fn silence(&mut self, vars: &[InferTy]) -> Result<(), Ice> {
        for var in vars {
            // An error type unifies with anything.
            let _ = self.infcx.unify(var, &InferTy::ERROR)?;
        }
        Ok(())
    }

    /// Whether the structure of `ty_def` (§4.3.4) is usable here. If not,
    /// reports E0043 at `span`. The resolver already reports constructors
    /// and record-update type names; this covers what only types reveal,
    /// such as field access on a value of an opaque type.
    pub(super) fn check_structure(
        &mut self,
        ty_def: DefId,
        what: Structure,
        span: Span,
    ) -> Result<bool, Ice> {
        if ty_def.module == self.input.module || ty_def.module == ModuleId::BUILTIN {
            return Ok(true);
        }
        let item = self.input.item(self.own, ty_def)?;
        let package = self.input.modules.try_get(ty_def.module)?.package;
        if structure_visible(
            item,
            ty_def.module,
            package,
            self.input.module,
            self.input.package,
        ) {
            return Ok(true);
        }
        let ty_name = self.input.globals.names.get(item.name);
        let boundary = if item.vis == Vis::Priv {
            "module"
        } else {
            "package"
        };
        let message = match what {
            Structure::Ctor(ctor) => format!(
                "the constructor `{}` is not visible outside its {boundary}",
                self.input.name_of(self.own, ctor)
            ),
            Structure::Fields => {
                format!("the fields of `{ty_name}` are not visible outside its {boundary}")
            }
        };
        let note = if item.vis == Vis::Priv {
            format!(
                "`{ty_name}` is `priv`, so its constructors and fields are usable only in its \
                 own module"
            )
        } else {
            "a `pub` type hides its constructors and fields unless it is `pub exposed`".to_owned()
        };
        self.report(
            Diagnostic::new(codes::PRIVATE_ITEM, message)
                .with_primary(span, "")
                .with_note(note),
        );
        Ok(false)
    }

    /// Checks the expression `id` against `expected`, as
    /// [`BodyCx::check_expr`] does after building an expression, and
    /// returns the expression to use in its place.
    pub(super) fn expect_expr(
        &mut self,
        id: ExprId,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        self.coerce_expr(id, expected)
    }

    // ---- construction and update ----------------------------------------

    /// Checks a record construction `P { x: 1 }` or update
    /// `T { ..base, x: 1 }`, against `expected` if given (the result is
    /// already coerced to it), and returns its
    /// [`crate::thir::ExprKind::Record`] or
    /// [`crate::thir::ExprKind::RecordUpdate`].
    pub fn check_record(
        &mut self,
        rec: ast::RecordExpr<'_>,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        let id = if rec.is_update() {
            self.record_update(rec)?
        } else {
            self.construction(rec, expected)?
        };
        self.expect_expr(id, expected)
    }

    /// The field values of a record whose constructor or type is in
    /// error: checked for their own errors only.
    fn error_record(&mut self, rec: ast::RecordExpr<'_>) -> Result<ExprId, Ice> {
        for entry in rec.entries() {
            match entry {
                RecordEntry::Field(field) => {
                    if let Some(value) = field.value() {
                        self.check_expr(value, None)?;
                    }
                }
                RecordEntry::Spread(spread) => {
                    if let Some(value) = spread.value() {
                        self.check_expr(value, None)?;
                    }
                }
            }
        }
        self.error_expr(rec.significant_span())
    }

    /// `P { x: 1, y }` (§5.11.1).
    fn construction(
        &mut self,
        rec: ast::RecordExpr<'_>,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        let span = rec.significant_span();
        let Some(path) = rec.path() else {
            return self.error_record(rec);
        };
        let info = match self.path_res(path.syntax().id())? {
            Res::Def(def) => self.ctor_info(def)?,
            _ => None,
        };
        // An unresolved or erroneous constructor was reported already.
        let Some(info) = info else {
            return self.error_record(rec);
        };
        let path_span = path.significant_span();
        if !self.check_structure(info.ty_def, Structure::Ctor(info.def), path_span)? {
            return self.error_record(rec);
        }
        if !matches!(info.fields, CtorFields::Named(_)) {
            self.not_named(&info, span);
            return self.error_record(rec);
        }
        let names = &self.input.globals.names;
        let all_fields = info.field_names(names);
        let (ty, args) = self.adt_instance(info.ty_def, span)?;
        // The expected type, when it is this type, informs the fields
        // (§3.12.5); any other expectation is left to the final check.
        if let Some(expected) = expected
            && let InferTy::Named { def, .. } = self.infcx.resolve(&expected.ty)?
            && def == info.ty_def
        {
            let _ = self.infcx.unify(&ty, &expected.ty)?;
        }
        let mut fields: Vec<(u32, ExprId)> = Vec::new();
        let mut seen: Vec<(u32, Span)> = Vec::new();
        for field in rec.fields() {
            let Some(name) = field.name() else {
                self.errored = true;
                continue;
            };
            let found = info.named_field(names, name.text());
            let Some((index, field_ty)) = found else {
                self.unknown_field(&info, name.text(), name.span(), &all_fields);
                if let Some(value) = field.value() {
                    self.check_expr(value, None)?;
                }
                continue;
            };
            if let Some(&(_, first)) = seen.iter().find(|(i, _)| *i == index) {
                self.report(
                    Diagnostic::new(
                        codes::INVALID_RECORD,
                        format!("field `{}` is initialized twice", name.text()),
                    )
                    .with_primary(name.span(), "initialized again here")
                    .with_secondary(first, "first initialized here"),
                );
            } else {
                seen.push((index, name.span()));
            }
            let expected = Expected::new(self.field_ty(field_ty, &args), None);
            let value = self.field_value(field, Some(&expected))?;
            fields.push((index, value));
        }
        let missing = unlisted(&all_fields, &seen);
        if !missing.is_empty() {
            let mut diag = Diagnostic::new(
                codes::INVALID_RECORD,
                format!(
                    "missing field{} {} in `{}`",
                    plural(missing.len()),
                    quoted_list(&missing),
                    info.name
                ),
            )
            .with_primary(path_span, format!("missing {}", quoted_list(&missing)))
            .with_note("a named-field constructor must initialize every field (§5.11.1)");
            if let Some(decl) = info.decl {
                diag = diag.with_secondary(decl, format!("`{}` is declared here", info.name));
            }
            self.report(diag);
        }
        let id = self.push(
            ExprKind::Record {
                ctor: info.def,
                ty_args: Box::new([]),
                fields: fields.into(),
            },
            ty,
            span,
        )?;
        if !args.is_empty() {
            self.inst.push((id, args));
        }
        Ok(id)
    }

    /// E0059 for braces after a unit or positional constructor.
    fn not_named(&mut self, info: &CtorInfo, span: Span) {
        let (message, help) = match info.fields {
            CtorFields::Unit => (
                format!("`{}` has no fields", info.name),
                format!("write the constructor alone: `{}`", info.name),
            ),
            _ => (
                format!("`{}` has positional fields, not named ones", info.name),
                format!("call it with its fields in order: `{}(…)`", info.name),
            ),
        };
        let mut diag = Diagnostic::new(codes::INVALID_RECORD, message)
            .with_primary(span, "")
            .with_help(help);
        if let Some(decl) = info.decl {
            diag = diag.with_secondary(decl, format!("`{}` is declared here", info.name));
        }
        self.report(diag);
    }

    /// The value of a field initializer, punned or not, checked against
    /// `expected`.
    fn field_value(
        &mut self,
        field: FieldInit<'_>,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        if let Some(value) = field.value() {
            return self.check_expr(value, expected);
        }
        // `Point { x }` is `Point { x: x }` (D-113).
        let span = field.significant_span();
        let res = self
            .res
            .names
            .get(&field.syntax().id())
            .copied()
            .ok_or_else(|| Ice::new("a punned field was not resolved"))?;
        let id = match res {
            Res::Local(local) => {
                let ty = self.local_ty(local)?;
                // A local function's own name inside its body (D-110).
                let kind = if self.local_fns.contains(&local) {
                    ExprKind::SelfRef { local }
                } else {
                    ExprKind::Local(local)
                };
                self.push(kind, ty, span)?
            }
            Res::Def(def) => self.def_value(def, None, span)?,
            Res::Err => self.error_expr(span)?,
            Res::Module(_) | Res::TyParam(_) | Res::SelfTy => {
                return Err(Ice::new("a punned field resolved to a module or a type"));
            }
        };
        self.expect_expr(id, expected)
    }

    /// E0058 for a field the constructor does not have.
    fn unknown_field(&mut self, info: &CtorInfo, name: &str, span: Span, all: &[&str]) {
        let mut diag = Diagnostic::new(
            codes::UNKNOWN_FIELD,
            format!("`{}` has no field `{name}`", info.name),
        )
        .with_primary(span, "unknown field");
        if !all.is_empty() {
            diag = diag.with_note(format!(
                "the field{} of `{}`: {}",
                plural(all.len()),
                info.name,
                quoted_list(all)
            ));
        }
        self.report(diag);
    }

    /// `T { ..base, x: 1 }` (§5.11.2, D-156, D-071).
    ///
    /// The result has the base's type, and each listed value is checked
    /// against its field's type under the base's type arguments, so an
    /// update never changes a type argument (D-501).
    fn record_update(&mut self, rec: ast::RecordExpr<'_>) -> Result<ExprId, Ice> {
        let span = rec.significant_span();
        let (Some(path), Some(base)) = (rec.path(), rec.spread().and_then(ast::Spread::value))
        else {
            return self.error_record(rec);
        };
        let path_span = path.significant_span();
        let ty_def = match self.path_res(path.syntax().id())? {
            Res::Def(def) => self.update_type(def, path_span)?,
            _ => None,
        };
        // `base` is evaluated and checked first.
        let base_id = self.check_expr(base, None)?;
        let Some(ty_def) = ty_def else {
            return self.update_fields_in_error(rec, span);
        };
        let Some(ctors) = self.adt_ctors(ty_def)? else {
            self.not_an_update_type(ty_def, path_span)?;
            return self.update_fields_in_error(rec, span);
        };
        if !self.check_structure(ty_def, Structure::Fields, path_span)? {
            return self.update_fields_in_error(rec, span);
        }
        let base_ty = self.ty_of(base_id)?;
        let Some(args) = self.update_base(base_id, ty_def, path_span)? else {
            return self.update_fields_in_error(rec, span);
        };
        let mut fields: Vec<UpdateField> = Vec::new();
        let mut seen: Vec<(&str, Span)> = Vec::new();
        for field in rec.fields() {
            let Some(name) = field.name() else {
                self.errored = true;
                continue;
            };
            let text = name.text();
            if let Some(&(_, first)) = seen.iter().find(|(n, _)| *n == text) {
                self.report(
                    Diagnostic::new(
                        codes::INVALID_RECORD,
                        format!("field `{text}` is listed twice"),
                    )
                    .with_primary(name.span(), "listed again here")
                    .with_secondary(first, "first listed here"),
                );
            } else {
                seen.push((text, name.span()));
            }
            let Some((index_per_ctor, field_ty)) =
                self.common_field(ty_def, &ctors, text, name.span(), "updated")
            else {
                if let Some(value) = field.value() {
                    self.check_expr(value, None)?;
                }
                continue;
            };
            let expected = Expected::new(self.field_ty(field_ty, &args), None);
            let value = self.field_value(field, Some(&expected))?;
            fields.push(UpdateField {
                value,
                index_per_ctor,
            });
        }
        self.push(
            ExprKind::RecordUpdate {
                base: base_id,
                fields: fields.into(),
            },
            base_ty,
            span,
        )
    }

    /// The fields of an update that cannot be checked against a type:
    /// checked for their own errors only.
    fn update_fields_in_error(
        &mut self,
        rec: ast::RecordExpr<'_>,
        span: Span,
    ) -> Result<ExprId, Ice> {
        for field in rec.fields() {
            if let Some(value) = field.value() {
                self.check_expr(value, None)?;
            }
        }
        self.error_expr(span)
    }

    /// The type arguments of `base`, whose type must be known here
    /// (D-213) and be `ty_def`. Otherwise reports E0057 or E0047 and
    /// gives `None`.
    fn update_base(
        &mut self,
        base: ExprId,
        ty_def: DefId,
        path_span: Span,
    ) -> Result<Option<Box<[InferTy]>>, Ice> {
        let base_ty = self.ty_of(base)?;
        let base_span = self.span_of(base)?;
        let Some(known) = self.known_ty(&base_ty)? else {
            let name = self.type_hint(ty_def);
            self.type_needed(base, base_span, "update it", name.as_deref())?;
            return Ok(None);
        };
        if known == InferTy::ERROR {
            return Ok(None);
        }
        if let Some((def, args)) = self.named_parts(&known)?
            && def == ty_def
        {
            return Ok(Some(args));
        }
        let (instance, vars) = self.adt_instance(ty_def, path_span)?;
        self.silence(&vars)?;
        let label = format!(
            "expected `{}`, found `{}`",
            self.show(&instance),
            self.show(&known)
        );
        self.report(
            Diagnostic::new(codes::TYPE_MISMATCH, "mismatched types")
                .with_primary(base_span, label)
                .with_secondary(path_span, "expected because of this type name"),
        );
        Ok(None)
    }

    /// The type a record update's path names: the type itself, or the
    /// named type an alias stands for. An alias is interchangeable with its
    /// definition everywhere (§4.4, D-081), so `T { ..base, … }` works
    /// through one (D-501). `None` for an alias of something else,
    /// reported here.
    fn update_type(&mut self, def: DefId, path_span: Span) -> Result<Option<DefId>, Ice> {
        let ItemSig::Alias(alias) = &self.input.item(self.own, def)?.sig else {
            return Ok(Some(def));
        };
        if let TyKind::Named { def: target, .. } = self.input.globals.types.kind(alias.target) {
            return Ok(Some(*target));
        }
        let cx = self.input.display_cx(self.own, Some(&self.generics));
        let shown = cheby_iface::display(alias.target, &cx);
        let name = self.input.name_of(self.own, def);
        let span = self.input.item(self.own, def)?.span;
        let mut diag = Diagnostic::new(
            codes::UNKNOWN_FIELD,
            format!("`{name}` stands for `{shown}`, which has no fields to update"),
        )
        .with_primary(path_span, "");
        if let Some(span) = span {
            diag = diag.with_secondary(span, format!("`{name}` is declared here"));
        }
        self.report(diag);
        Ok(None)
    }

    /// The path of a record update names something without named fields:
    /// a type without constructors, or a type in error.
    fn not_an_update_type(&mut self, def: DefId, span: Span) -> Result<(), Ice> {
        let name = self.input.name_of(self.own, def);
        match &self.input.item(self.own, def)?.sig {
            ItemSig::Type(_) => {
                self.report(
                    Diagnostic::new(
                        codes::UNKNOWN_FIELD,
                        format!("`{name}` has no fields to update"),
                    )
                    .with_primary(span, "")
                    .with_note("a record update copies a value of a type with named fields"),
                );
            }
            // Reported with the declaration.
            _ => self.errored = true,
        }
        Ok(())
    }

    /// The field `name` of every constructor of `ty_def`, with one type
    /// (D-071): its index in each constructor and its type. Otherwise
    /// E0058 at `span` and `None`. `done` is "read" or "updated".
    fn common_field(
        &mut self,
        ty_def: DefId,
        ctors: &[CtorInfo],
        name: &str,
        span: Span,
        done: &str,
    ) -> Option<CommonField> {
        let names = &self.input.globals.names;
        let ty_name = self.input.name_of(self.own, ty_def);
        let mut found: Vec<(DefId, u32, Ty)> = Vec::new();
        let mut lacking: Vec<&str> = Vec::new();
        for ctor in ctors {
            match ctor.named_field(names, name) {
                Some((index, ty)) => found.push((ctor.def, index, ty)),
                None => lacking.push(ctor.name),
            }
        }
        let Some(&(_, _, first_ty)) = found.first() else {
            let mut diag = Diagnostic::new(
                codes::UNKNOWN_FIELD,
                format!("`{ty_name}` has no field `{name}`"),
            )
            .with_primary(span, "unknown field");
            let mut all: Vec<&str> = ctors.iter().flat_map(|c| c.field_names(names)).collect();
            all.sort_unstable();
            all.dedup();
            if all.is_empty() {
                diag = diag.with_note(format!(
                    "`{ty_name}` has no named fields; positional fields are read with a pattern \
                     (D-241)"
                ));
            } else if let [only] = ctors {
                diag = diag.with_note(format!(
                    "the field{} of `{}`: {}",
                    plural(all.len()),
                    only.name,
                    quoted_list(&all)
                ));
            }
            self.report(diag);
            return None;
        };
        if !lacking.is_empty() {
            let shown: Vec<&str> = lacking.iter().copied().take(3).collect();
            let has = if lacking.len() == 1 { "has" } else { "have" };
            let mut list = quoted_list(&shown);
            if lacking.len() > shown.len() {
                list = format!(
                    "{list} and {} more",
                    lacking.len().saturating_sub(shown.len())
                );
            }
            self.report(
                Diagnostic::new(
                    codes::UNKNOWN_FIELD,
                    format!("not every variant of `{ty_name}` has the field `{name}`"),
                )
                .with_primary(span, format!("{list} {has} no field `{name}`"))
                .with_note(format!(
                    "a field can be {done} without `case` only if every variant has it with \
                     the same type (D-071)"
                ))
                .with_help("use `case` to handle each variant"),
            );
            return None;
        }
        if let Some(&(other, _, other_ty)) = found.iter().find(|(_, _, t)| *t != first_ty) {
            // The declared field types use the type's own parameters, so
            // they are shown with the type's names (`A`, `B`).
            let ty_generics = match self.input.item(self.own, ty_def).map(|item| &item.sig) {
                Ok(ItemSig::Type(def)) => &def.generics,
                _ => &self.generics,
            };
            let cx = self.input.display_cx(self.own, Some(ty_generics));
            let first_name = found
                .first()
                .map_or("?", |&(def, _, _)| self.input.name_of(self.own, def));
            let label = format!(
                "`{name}` is `{}` in `{first_name}` and `{}` in `{}`",
                cheby_iface::display(first_ty, &cx),
                cheby_iface::display(other_ty, &cx),
                self.input.name_of(self.own, other)
            );
            self.report(
                Diagnostic::new(
                    codes::UNKNOWN_FIELD,
                    format!(
                        "the field `{name}` does not have one type in every variant of `{ty_name}`"
                    ),
                )
                .with_primary(span, label)
                .with_note(format!(
                    "a field can be {done} without `case` only if every variant has it with \
                     the same type (D-071)"
                ))
                .with_help("use `case` to handle each variant"),
            );
            return None;
        }
        let per_ctor = found.iter().map(|&(def, index, _)| (def, index)).collect();
        Some((per_ctor, first_ty))
    }

    /// E0057: the type of `id` is not known where `what` needs it (D-213).
    /// `ty` names the type to suggest in the help, when one is known: the
    /// type of a record update, or the only type of this module with the
    /// field read (a hint only, never used for checking).
    fn type_needed(
        &mut self,
        id: ExprId,
        span: Span,
        what: &str,
        ty: Option<&str>,
    ) -> Result<(), Ice> {
        let ty = ty.unwrap_or("Type");
        let local = match &self.exprs.try_get(id, "expression")?.kind {
            ExprKind::Local(local) => Some(*local),
            _ => None,
        };
        let mut diag = Diagnostic::new(codes::TYPE_ANNOTATION_NEEDED, "type annotation needed");
        match local.and_then(|l| self.res.locals.get(l).map(|decl| (l, decl.clone()))) {
            Some((_, decl)) => {
                let name = self.input.globals.names.get(decl.name);
                diag = diag
                    .with_primary(
                        span,
                        format!("the type of `{name}` must be known here to {what}"),
                    )
                    .with_secondary(decl.span, format!("`{name}` is bound here"));
                let help = match decl.kind {
                    LocalKind::ClosureParam => {
                        format!("annotate the parameter: `fn({name}: {ty})`")
                    }
                    LocalKind::Let => format!("annotate the binding: `let {name}: {ty} = …`"),
                    _ => format!("annotate the type where `{name}` is bound"),
                };
                diag = diag.with_help(help);
            }
            None => {
                diag = diag
                    .with_primary(
                        span,
                        format!("the type of this value must be known here to {what}"),
                    )
                    .with_help(format!(
                        "bind the value with a type annotation first: `let value: {ty} = …`"
                    ));
            }
        }
        self.report(diag.with_note(
            "field names are not unique across types, so the type must come from an \
                 annotation, a signature or a use checked earlier (D-213)",
        ));
        Ok(())
    }

    /// How to write the type `def` in an annotation, for a help: its name,
    /// when it has no type parameters (an annotation needs their
    /// arguments, which nothing here knows).
    fn type_hint(&self, def: DefId) -> Option<String> {
        let item = self.input.item(self.own, def).ok()?;
        let ItemSig::Type(ty) = &item.sig else {
            return None;
        };
        ty.generics.is_empty().then(|| {
            let shown = InferTy::Known(self.input.globals.types.named(def, []));
            self.show(&shown)
        })
    }

    /// The only type of this module whose every variant has the named
    /// field `field`, for the help of E0057.
    fn field_owner(&self, field: &str) -> Option<String> {
        let names = &self.input.globals.names;
        let mut owners = self
            .own
            .items
            .iter_enumerated()
            .filter_map(|(index, item)| {
                let def = self.input.def_id(index);
                let ctors = self.adt_ctors(def).ok()??;
                (!ctors.is_empty() && ctors.iter().all(|c| c.named_field(names, field).is_some()))
                    .then_some(def)
                    .filter(|_| matches!(item.sig, ItemSig::Type(_)))
            });
        let only = owners.next()?;
        if owners.next().is_some() {
            return None;
        }
        self.type_hint(only)
    }

    // ---- field access ----------------------------------------------------

    /// Checks a field access `x.name`, against `expected` if given (the
    /// result is already coerced to it), and returns its
    /// [`crate::thir::ExprKind::Field`].
    pub fn check_field(
        &mut self,
        field: ast::FieldExpr<'_>,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        let id = self.field_access(field)?;
        self.expect_expr(id, expected)
    }

    fn field_access(&mut self, field: ast::FieldExpr<'_>) -> Result<ExprId, Ice> {
        let span = field.significant_span();
        let Some(receiver) = field.receiver() else {
            return self.error_expr(span);
        };
        let base = self.check_expr(receiver, None)?;
        // `x.0` was reported by syntax validation (E0024, D-241).
        let Some(name) = field.field() else {
            return self.error_expr(span);
        };
        self.read_field(base, name.text(), name.span(), span)
    }

    /// The field access `base.text` at `span`, with the field's name at
    /// `name_span`, for a `base` already checked: in an expression, or in
    /// an interpolation `{p.x}` (§5.3.2). The base's type must be known now
    /// (D-213, E0057), be visible here (§4.3.4, E0043), and have a named
    /// field `text` in every variant with one type (D-071, E0058).
    pub(crate) fn read_field(
        &mut self,
        base: ExprId,
        text: &str,
        name_span: Span,
        span: Span,
    ) -> Result<ExprId, Ice> {
        let base_ty = self.ty_of(base)?;
        let base_span = self.span_of(base)?;
        let Some(known) = self.known_ty(&base_ty)? else {
            let hint = self.field_owner(text);
            let what = format!("read its field `{text}`");
            self.type_needed(base, base_span, &what, hint.as_deref())?;
            return self.error_expr(span);
        };
        if known == InferTy::ERROR {
            return self.error_expr(span);
        }
        let Some((ty_def, args)) = self.named_parts(&known)? else {
            self.no_fields(&known, text, name_span, base_span);
            return self.error_expr(span);
        };
        let Some(ctors) = self.adt_ctors(ty_def)? else {
            if matches!(self.input.item(self.own, ty_def)?.sig, ItemSig::Type(_)) {
                self.no_fields(&known, text, name_span, base_span);
            } else {
                self.errored = true;
            }
            return self.error_expr(span);
        };
        if !self.check_structure(ty_def, Structure::Fields, name_span)? {
            return self.error_expr(span);
        }
        let Some((index_per_ctor, field_ty)) =
            self.common_field(ty_def, &ctors, text, name_span, "read")
        else {
            return self.error_expr(span);
        };
        let field_name: Name = ctors
            .iter()
            .find_map(|c| match &c.fields {
                CtorFields::Named(fields) => fields
                    .iter()
                    .find(|&&(n, _)| self.input.globals.names.get(n) == text)
                    .map(|&(n, _)| n),
                _ => None,
            })
            .ok_or_else(|| Ice::new("a common field without a name"))?;
        let ty = self.field_ty(field_ty, &args);
        self.push(
            ExprKind::Field {
                base,
                field: field_name,
                index_per_ctor,
            },
            ty,
            span,
        )
    }

    /// E0058 for `.name` on a type without named fields.
    fn no_fields(&mut self, ty: &InferTy, name: &str, span: Span, base_span: Span) {
        let shown = self.show(ty);
        let mut diag = Diagnostic::new(
            codes::UNKNOWN_FIELD,
            format!("`{shown}` has no field `{name}`"),
        )
        .with_primary(span, "unknown field")
        .with_secondary(base_span, format!("this has type `{shown}`"));
        if matches!(ty, InferTy::Tuple(_))
            || matches!(ty, InferTy::Known(t) if matches!(self.input.globals.types.kind(*t), TyKind::Tuple(_)))
        {
            diag = diag.with_help("read tuple elements with a pattern: `let (a, b) = pair`");
        }
        self.report(diag);
    }
}

#[cfg(test)]
mod tests;
