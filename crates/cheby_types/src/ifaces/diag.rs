//! E0063 and E0065: the diagnostics of an unmet requirement, with the
//! notes and helps of §8.2 and §8.4 (D-086, D-164, D-236).

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{
    DefId, IfaceItem, ItemSig, ModuleId, PackageId, ParamIdx, Ty, TyKind, Vis, display,
};

use super::{IfaceCx, Need, Reason, Unsatisfied, Want};
use crate::thir::BinOpKind;

/// The diagnostic for `err` at `span`: E0063, or E0065 when the root cause
/// is a type parameter without a bound to supply a descriptor, with the
/// notes and helps of §8.2 and §8.4 (`{x:?}`, make `show` `pub`, add a
/// bound such as `T: Any`).
pub(crate) fn unsatisfied_diagnostic(
    cx: &IfaceCx<'_>,
    err: &Unsatisfied,
    span: Span,
    need: Need,
) -> Diagnostic {
    let w = Writer { cx };
    if let Some((leaf, param)) = descriptor_leaf(err) {
        return w.descriptor(err, leaf, param, span, need);
    }
    let ty = w.ty(err.ty);
    let want = w.want(err.want);
    let mut diag = Diagnostic::new(
        codes::UNSATISFIED_INTERFACE,
        format!("`{ty}` does not satisfy `{want}`"),
    )
    .with_primary(span, w.need_label(err, need));
    diag = w.explain(diag, err, true);
    if need == Need::Interpolation {
        let text = w.span_text(span).filter(|t| is_path(t));
        diag = diag.with_help(match text {
            Some(text) => format!("write `{{{text}:?}}` to use debug printing instead (D-086)"),
            None => "write `{…:?}` to use debug printing instead (D-086)".to_owned(),
        });
    }
    diag
}

/// The innermost reason, if it is a missing descriptor.
fn descriptor_leaf(err: &Unsatisfied) -> Option<(&Unsatisfied, ParamIdx)> {
    match &err.reason {
        Reason::Descriptor { param } => Some((err, *param)),
        Reason::Element { inner, .. } | Reason::Bound { inner, .. } => descriptor_leaf(inner),
        _ => None,
    }
}

/// Whether `text` is an interpolation path such as `user.name`.
fn is_path(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
}

struct Writer<'c, 'a> {
    cx: &'c IfaceCx<'a>,
}

impl Writer<'_, '_> {
    fn ty(&self, ty: Ty) -> String {
        let input = self.cx.input;
        display(ty, &input.display_cx(self.cx.own, Some(self.cx.generics)))
    }

    /// A type written with the generics of function `f`.
    fn ty_in(&self, ty: Ty, f: DefId) -> String {
        let input = self.cx.input;
        match self.item(f).map(|item| &item.sig) {
            Some(ItemSig::Fn(sig)) => {
                display(ty, &input.display_cx(self.cx.own, Some(&sig.generics)))
            }
            _ => self.ty(ty),
        }
    }

    fn item(&self, def: DefId) -> Option<&IfaceItem> {
        self.cx.item(def).ok()
    }

    fn name(&self, def: DefId) -> &'static str {
        self.cx.input.name_of(self.cx.own, def)
    }

    /// The last segment of `module`'s path: `angle` for `geometry::angle`.
    fn segment(&self, module: ModuleId) -> String {
        self.cx
            .input
            .modules
            .get(module)
            .and_then(|info| info.path.segments().last().cloned())
            .unwrap_or_else(|| "?".to_owned())
    }

    /// `module`'s full path: `geometry::angle`, `std::int`.
    fn path(&self, module: ModuleId) -> String {
        self.cx
            .input
            .modules
            .get(module)
            .map_or_else(|| "?".to_owned(), |info| info.path.to_string())
    }

    /// An interface as users write it here (D-463): bare in its own module
    /// and for the prelude's `Show`, else `<module>::<Name>`.
    fn iface(&self, def: DefId) -> String {
        let name = self.name(def);
        if def.module == ModuleId::BUILTIN || def.module == self.cx.input.module {
            name.to_owned()
        } else {
            format!("{}::{name}", self.segment(def.module))
        }
    }

    /// A function of a type's module, `angle::show`, bare in the checked
    /// module itself.
    fn func(&self, def: DefId) -> String {
        if def.module == self.cx.input.module {
            return self.name(def).to_owned();
        }
        format!("{}::{}", self.segment(def.module), self.name(def))
    }

    fn want(&self, want: Want) -> String {
        match want {
            Want::Iface(def) => self.iface(def),
            Want::Op(op) => op.iface_name().to_owned(),
        }
    }

    fn span_text(&self, span: Span) -> Option<&str> {
        let tree = self.cx.input.tree;
        if span.file != tree.file() {
            return None;
        }
        let start = usize::try_from(span.range.start()).ok()?;
        let end = usize::try_from(span.range.end()).ok()?;
        tree.text().get(start..end)
    }

    fn need_label(&self, err: &Unsatisfied, need: Need) -> String {
        match need {
            Need::Bound { callee } => format!("required by a bound of `{}`", self.name(callee)),
            Need::Interpolation => "interpolated here".to_owned(),
            Need::Dyn => format!("converted to `dyn {}` here", self.want(err.want)),
            Need::Operator(op) => {
                let fn_name = match err.want {
                    Want::Op(op) => op.fn_name(),
                    Want::Iface(_) => "compare",
                };
                format!("`{}` calls `{fn_name}` of the operand type", op_text(op))
            }
            Need::Neg => "unary `-` calls `neg` of the operand type".to_owned(),
            Need::IfaceCall { iface_fn } => {
                let parent = self
                    .item(iface_fn)
                    .and_then(|item| item.parent)
                    .map(|p| DefId::new(iface_fn.module, p));
                let iface = parent.map_or_else(|| "?".to_owned(), |p| self.iface(p));
                format!(
                    "`{iface}::{}` is called with `Self` = `{}`",
                    self.name(iface_fn),
                    self.ty(err.ty)
                )
            }
        }
    }

    /// When the required function `func` comes from an interface other
    /// than the wanted one, a note saying so.
    fn embedded_note(&self, diag: Diagnostic, want: Want, func: Option<DefId>) -> Diagnostic {
        let (Want::Iface(iface), Some(func)) = (want, func) else {
            return diag;
        };
        let Some(parent) = self.item(func).and_then(|item| item.parent) else {
            return diag;
        };
        let parent = DefId::new(func.module, parent);
        if parent == iface {
            return diag;
        }
        diag.with_note(format!(
            "`{}` is required by `{}`, which `{}` embeds (§8.3)",
            self.name(func),
            self.iface(parent),
            self.iface(iface)
        ))
    }

    /// Adds the notes, labels and helps of `err`'s reason and of the
    /// reasons nested in it.
    fn explain(&self, diag: Diagnostic, err: &Unsatisfied, top: bool) -> Diagnostic {
        let ty = self.ty(err.ty);
        match &err.reason {
            Reason::Missing {
                name,
                func,
                required,
                module,
            } => {
                let name = self.cx.input.globals.names.get(*name);
                self.missing(diag, err, name, *func, *required, *module, top)
            }
            Reason::LessVisible {
                found,
                func,
                type_vis,
            } => self.less_visible(diag, err, *found, *func, *type_vis),
            Reason::Signature {
                found,
                func,
                required,
                actual,
            } => self.signature(diag, err, *found, *func, *required, *actual),
            // A std module reads only what it imports (D-447). A module of
            // any other package depends on every shipped home module, so
            // this home module is not shipped yet (D-451).
            Reason::HomeNotInDeps { home } if self.cx.input.package == PackageId::STD => diag
                .with_note(format!(
                    "`{ty}` gets its functions from `std::{home}`, which this module does not import (D-447)"
                ))
                .with_help(format!("add `import std::{home}`")),
            Reason::HomeNotInDeps { home } => diag.with_note(format!(
                "`{ty}` gets its functions from `std::{home}`, which is not supported yet (D-451)"
            )),
            Reason::NoModule => {
                let note = match self.cx.input.globals.types.kind(err.ty) {
                    TyKind::Tuple(_) => {
                        "tuples satisfy only `Show`, `Compare` and empty interfaces (§8.2, D-167)"
                    }
                    TyKind::Fn { .. } => "function types satisfy only empty interfaces (§8.2, D-166)",
                    TyKind::Dyn { .. } => "operators are not defined on `dyn` values (§8.6, §8.7)",
                    _ => "`Never` is declared in no module, so it satisfies only empty interfaces",
                };
                diag.with_note(note)
            }
            Reason::Param { param } => self.param(diag, err, *param),
            Reason::Dyn { has } => diag.with_note(format!(
                "`dyn {}` satisfies only `{}` and the interfaces it embeds (§8.6)",
                self.iface(*has),
                self.iface(*has)
            )),
            Reason::Element { index, inner } => {
                let diag = diag.with_note(format!(
                    "element {} of `{ty}` is `{}`, which does not satisfy `{}` (§8.2, D-167)",
                    index.saturating_add(1),
                    self.ty(inner.ty),
                    self.want(inner.want)
                ));
                self.explain(diag, inner, false)
            }
            Reason::Bound {
                found,
                param,
                inner,
            } => {
                let diag = diag.with_note(format!(
                    "`{}` requires `{}: {}`, and `{}` does not satisfy it (§8.2.2, D-064)",
                    self.func(*found),
                    self.generic_name(*found, *param),
                    self.want(inner.want),
                    self.ty(inner.ty)
                ));
                self.explain(diag, inner, false)
            }
            Reason::Undetermined {
                found,
                param,
                actual,
            } => self.undetermined(diag, err, *found, *param, *actual),
            Reason::Descriptor { param } => diag.with_note(format!(
                "`{ty}` mentions the type parameter `{}`, which has no bound (D-236)",
                self.param_name(*param)
            )),
            Reason::TooDeep => diag.with_note(format!(
                "the requirement nests more than {} levels deep",
                super::solve::DEPTH_LIMIT
            )),
        }
    }

    /// The module has no function `name` of type `required`.
    #[expect(clippy::too_many_arguments, reason = "the fields of `Reason::Missing`")]
    fn missing(
        &self,
        diag: Diagnostic,
        err: &Unsatisfied,
        name: &str,
        func: Option<DefId>,
        required: Ty,
        module: ModuleId,
        top: bool,
    ) -> Diagnostic {
        let ty = self.ty(err.ty);
        let sig = sig(name, &self.ty(required));
        let builtin_ty = matches!(
            self.cx.input.globals.types.kind(err.ty),
            TyKind::Named { def, .. } if def.module == ModuleId::BUILTIN
        );
        let note = if builtin_ty {
            format!(
                "`{ty}` gets its functions from `{}`, which has no function `{sig}` (§8.2, D-447)",
                self.path(module)
            )
        } else if module == self.cx.input.module {
            format!("this module declares `{ty}` but has no function `{sig}` (§8.2, D-061)")
        } else {
            format!(
                "only functions of the module that declares `{ty}` count, and `{}` has no function `{sig}` (§8.2, D-061)",
                self.path(module)
            )
        };
        let diag = self.embedded_note(diag.with_note(note), err.want, func);
        if top && module == self.cx.input.module {
            diag.with_help(format!("add a function `{sig}` to this module"))
        } else {
            diag
        }
    }

    /// `found` would count but is less visible than the type (D-164).
    fn less_visible(
        &self,
        diag: Diagnostic,
        err: &Unsatisfied,
        found: DefId,
        func: Option<DefId>,
        type_vis: Vis,
    ) -> Diagnostic {
        let ty = self.ty(err.ty);
        let found_name = self.name(found);
        let found_vis = self.item(found).map_or(Vis::Priv, |item| item.vis);
        let label = format!(
            "`{}` is {}, less visible than `{ty}`",
            self.func(found),
            vis_text(found_vis)
        );
        let diag = match self.item(found).and_then(|item| item.span) {
            Some(span) => diag.with_secondary(span, label),
            None => diag.with_note(label),
        };
        let diag = diag.with_note(
            "a function counts toward an interface only if it is at least as visible as the type (§8.2, D-164)",
        );
        let diag = self.embedded_note(diag, err.want, func);
        let make = match type_vis {
            Vis::Pub => "`pub`",
            Vis::Package | Vis::Priv => "package-visible or `pub`",
        };
        diag.with_help(format!(
            "make `{found_name}` {make}, or make `{ty}` less visible"
        ))
    }

    /// `found` has the signature `actual` instead of `required`.
    fn signature(
        &self,
        diag: Diagnostic,
        err: &Unsatisfied,
        found: DefId,
        func: Option<DefId>,
        required: Ty,
        actual: Ty,
    ) -> Diagnostic {
        let ty = self.ty(err.ty);
        let found_name = self.name(found);
        let label = format!(
            "`{}` has the signature `{}`",
            self.func(found),
            sig(found_name, &self.ty_in(actual, found))
        );
        let diag = match self.item(found).and_then(|item| item.span) {
            Some(span) => diag.with_secondary(span, label),
            None => diag.with_note(label),
        };
        let mut diag = diag.with_note(format!(
            "`{}` needs `{}` (§8.2)",
            self.want(err.want),
            sig(found_name, &self.ty(required))
        ));
        if let Some(other) = self.other_type_of_module(err.ty, actual) {
            diag = diag
                .with_note(format!(
                    "the module's `{found_name}` is for `{other}`: a module satisfies an interface for one type only (§8.2.1, D-045)"
                ))
                .with_help(format!(
                    "move `{ty}` and its functions to a module of their own"
                ));
        }
        self.embedded_note(diag, err.want, func)
    }

    /// `found` would count, but its type parameter `param` is not
    /// determined by the type (D-504).
    fn undetermined(
        &self,
        diag: Diagnostic,
        err: &Unsatisfied,
        found: DefId,
        param: ParamIdx,
        actual: Ty,
    ) -> Diagnostic {
        let ty = self.ty(err.ty);
        let found_name = self.name(found);
        let param_name = self.generic_name(found, param);
        let label = format!(
            "`{}` has the signature `{}`",
            self.func(found),
            sig(found_name, &self.ty_in(actual, found))
        );
        let diag = match self.item(found).and_then(|item| item.span) {
            Some(span) => diag.with_secondary(span, label),
            None => diag.with_note(label),
        };
        diag.with_note(format!(
            "for `{ty}`, nothing determines the type parameter `{param_name}` of `{}`, so it \
             does not count toward `{}` (§8.2)",
            self.func(found),
            self.want(err.want)
        ))
        .with_help(format!(
            "remove the type parameter `{param_name}` from `{found_name}`"
        ))
    }

    /// A type parameter none of whose bounds gives the requirement.
    fn param(&self, diag: Diagnostic, err: &Unsatisfied, param: ParamIdx) -> Diagnostic {
        let name = self.param_name(param);
        let diag = diag.with_note(
            "a type parameter satisfies only the interfaces of its bounds and the ones they embed (§8.4)",
        );
        match err.want {
            Want::Op(op) => diag.with_help(format!(
                "add the bound `{name}: {}` (`import std::ops::{{{}}}`)",
                op.iface_name(),
                op.iface_name()
            )),
            Want::Iface(_) => {
                diag.with_help(format!("add the bound `{name}: {}`", self.want(err.want)))
            }
        }
    }

    /// The name of type parameter `param` of function `f`.
    fn generic_name(&self, f: DefId, param: ParamIdx) -> &'static str {
        match self.item(f).map(|item| &item.sig) {
            Some(ItemSig::Fn(sig)) => sig
                .generics
                .params
                .get(param)
                .map_or("?", |p| self.cx.input.globals.names.get(p.name)),
            _ => "?",
        }
    }

    /// Another type of `ty`'s own module that a parameter of the found
    /// function's type `actual` names: the type the module's function is
    /// for (D-045).
    fn other_type_of_module(&self, ty: Ty, actual: Ty) -> Option<String> {
        let types = &self.cx.input.globals.types;
        let TyKind::Named { def, .. } = types.kind(ty) else {
            return None;
        };
        let TyKind::Fn { params, .. } = types.kind(actual) else {
            return None;
        };
        params.iter().find_map(|&p| match types.kind(p) {
            TyKind::Named { def: other, .. }
                if other.module == def.module
                    && other != def
                    && def.module != ModuleId::BUILTIN =>
            {
                Some(self.ty(types.named(*other, [])))
            }
            _ => None,
        })
    }

    fn param_name(&self, param: ParamIdx) -> &'static str {
        self.cx
            .generics
            .params
            .get(param)
            .map_or("?", |p| self.cx.input.globals.names.get(p.name))
    }

    /// E0065: `leaf.ty` mentions `param`, which has no bound to supply its
    /// descriptor (D-236).
    fn descriptor(
        &self,
        err: &Unsatisfied,
        leaf: &Unsatisfied,
        param: ParamIdx,
        span: Span,
        need: Need,
    ) -> Diagnostic {
        let name = self.param_name(param);
        let message = match need {
            Need::Dyn => format!(
                "cannot convert `{}` to `dyn {}`: the type parameter `{name}` has no bound",
                self.ty(err.ty),
                self.want(err.want)
            ),
            Need::Bound { callee } => format!(
                "cannot pass `{}` for a bound of `{}`: the type parameter `{name}` has no bound",
                self.ty(err.ty),
                self.name(callee)
            ),
            _ => format!(
                "cannot build the dictionaries for `{}`: the type parameter `{name}` has no bound",
                self.ty(err.ty)
            ),
        };
        let mut diag = Diagnostic::new(codes::DYN_NEEDS_BOUND, message)
            .with_primary(span, self.need_label(err, need));
        if !std::ptr::eq(err, leaf) {
            diag = self.chain_notes(diag, err);
        }
        let note = match need {
            Need::Dyn => format!(
                "a `dyn` value carries a descriptor of its value's type, and the descriptor of `{name}` can come only from one of its bounds (§8.6.1, D-236)"
            ),
            _ => format!(
                "a dictionary passed for a bound carries a descriptor of its type, and the descriptor of `{name}` can come only from one of its bounds (§8.5.1, D-236)"
            ),
        };
        diag.with_note(note).with_help(format!(
            "add a bound such as `{name}: Any`, with `interface Any`: every type satisfies an empty interface (D-166)"
        ))
    }

    /// The notes of the nesting above a descriptor leaf.
    fn chain_notes(&self, diag: Diagnostic, err: &Unsatisfied) -> Diagnostic {
        match &err.reason {
            Reason::Element { index, inner } => {
                let diag = diag.with_note(format!(
                    "element {} of `{}` is `{}`",
                    index.saturating_add(1),
                    self.ty(err.ty),
                    self.ty(inner.ty)
                ));
                self.chain_notes(diag, inner)
            }
            Reason::Bound {
                found,
                param,
                inner,
            } => {
                let diag = diag.with_note(format!(
                    "`{}` requires `{}: {}`, so it receives a dictionary for `{}`",
                    self.func(*found),
                    self.generic_name(*found, *param),
                    self.want(inner.want),
                    self.ty(inner.ty)
                ));
                self.chain_notes(diag, inner)
            }
            _ => diag,
        }
    }
}

/// `show(Point) -> String` for a function type `fn(Point) -> String`.
fn sig(name: &str, fn_text: &str) -> String {
    format!("{name}{}", fn_text.strip_prefix("fn").unwrap_or(fn_text))
}

const fn vis_text(vis: Vis) -> &'static str {
    match vis {
        Vis::Pub => "`pub`",
        Vis::Package => "package-visible",
        Vis::Priv => "`priv`",
    }
}

const fn op_text(op: BinOpKind) -> &'static str {
    match op {
        BinOpKind::Add => "+",
        BinOpKind::Sub => "-",
        BinOpKind::Mul => "*",
        BinOpKind::Div => "/",
        BinOpKind::Lt => "<",
        BinOpKind::Le => "<=",
        BinOpKind::Gt => ">",
        BinOpKind::Ge => ">=",
        BinOpKind::Rem => "%",
        BinOpKind::Eq => "==",
        BinOpKind::Ne => "!=",
        BinOpKind::Or => "||",
        BinOpKind::And => "&&",
        BinOpKind::BitOr => "|",
        BinOpKind::BitXor => "^",
        BinOpKind::BitAnd => "&",
        BinOpKind::Shl => "<<",
        BinOpKind::Shr => ">>",
    }
}
