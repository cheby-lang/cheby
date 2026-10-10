//! Checks of interface declarations (§8.1, §8.3) and of `dyn I` (§8.6).

use std::fmt::Write as _;

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{
    DefId, DefKind, GenericParam, Generics, Ice, IfaceItem, IndexVec, InterfaceDef, InterfaceFnSig,
    ItemSig, ModuleId, ModuleInterface, Ty, TyKind, display, interface_fns, sym,
};

use super::SELF;
use super::tyutil::mentions;
use crate::TypesInput;

/// Reads interface items of this module and its dependencies.
struct Reader<'r, 'a> {
    input: &'r TypesInput<'a>,
    own: &'r ModuleInterface,
}

impl<'r> Reader<'r, '_> {
    fn item(&self, def: DefId) -> Option<&'r IfaceItem> {
        if def.module == self.input.module {
            self.own.item(def.index)
        } else {
            self.input.deps.item(def)
        }
    }

    fn try_item(&self, def: DefId) -> Result<&'r IfaceItem, Ice> {
        self.item(def)
            .ok_or_else(|| Ice::new(format!("no interface item for {def:?}")))
    }

    fn name(&self, def: DefId) -> &'static str {
        self.item(def)
            .map_or("?", |item| self.input.globals.names.get(item.name))
    }

    fn interface(&self, def: DefId) -> Option<&'r InterfaceDef> {
        match &self.item(def)?.sig {
            ItemSig::Interface(def) => Some(def),
            _ => None,
        }
    }

    fn fn_sig(&self, def: DefId) -> Option<&'r InterfaceFnSig> {
        match &self.item(def)?.sig {
            ItemSig::InterfaceFn(sig) => Some(sig),
            _ => None,
        }
    }

    /// An interface as users write it here (D-463).
    fn iface_text(&self, def: DefId) -> String {
        let name = self.name(def);
        if def.module == ModuleId::BUILTIN || def.module == self.input.module {
            return name.to_owned();
        }
        let segment = self
            .input
            .modules
            .get(def.module)
            .and_then(|info| info.path.segments().last().cloned())
            .unwrap_or_else(|| "?".to_owned());
        format!("{segment}::{name}")
    }

    /// `fn name(Self, Float) -> Self` for interface function `def`.
    fn fn_text(&self, def: DefId) -> String {
        let Some(sig) = self.fn_sig(def) else {
            return format!("fn {}", self.name(def));
        };
        let mut params = IndexVec::new();
        let _ = params.push(GenericParam {
            name: sym::SELF_TY,
            bounds: Box::new([]),
        });
        let generics = Generics { params };
        let cx = self.input.display_cx(self.own, Some(&generics));
        let ty = self.input.globals.types.func(sig.params.clone(), sig.ret);
        let text = display(ty, &cx);
        format!(
            "fn {}{}",
            self.name(def),
            text.strip_prefix("fn").unwrap_or(&text)
        )
    }

    /// The interface that declares interface function `f`.
    fn parent(&self, f: DefId) -> Option<DefId> {
        self.item(f)?.parent.map(|p| DefId::new(f.module, p))
    }

    /// Every function `iface` requires through embedding and its own, by
    /// `DefId`, depth first, each once.
    fn closure_fns(&self, iface: DefId) -> Vec<DefId> {
        let mut out = Vec::new();
        let mut seen = Vec::new();
        self.collect(iface, &mut seen, &mut out);
        out
    }

    fn collect(&self, iface: DefId, seen: &mut Vec<DefId>, out: &mut Vec<DefId>) {
        if seen.contains(&iface) {
            return;
        }
        seen.push(iface);
        let Some(def) = self.interface(iface) else {
            return;
        };
        for &embed in &def.embeds {
            self.collect(embed, seen, out);
        }
        for &index in &def.fns {
            let f = DefId::new(iface.module, index);
            if !out.contains(&f) {
                out.push(f);
            }
        }
    }
}

/// Whether `ty` mentions `Self`.
fn has_self(input: &TypesInput<'_>, ty: Ty) -> bool {
    mentions(&input.globals.types, ty, SELF)
}

/// E0064 if `dyn iface`, written at `span`, is not allowed: a function of
/// the interface, embedded ones included, uses `Self` other than as its
/// first parameter (D-117). Called when lowering a written `dyn I`.
///
/// A function without `Self` in any parameter is E0066 at its
/// declaration and is not reported again here.
///
/// # Errors
///
/// An [`Ice`] if the interface cannot be read.
pub(crate) fn check_object_safe(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    iface: DefId,
    span: Span,
) -> Result<Option<Diagnostic>, Ice> {
    let r = Reader { input, own };
    let fns = interface_fns(iface, &|def| r.item(def))?;
    let types = &input.globals.types;
    let mut offending = Vec::new();
    for f in fns {
        let Some(sig) = r.fn_sig(f) else {
            continue;
        };
        if !sig.params.iter().any(|&p| has_self(input, p)) {
            continue;
        }
        let first_ok = sig
            .params
            .first()
            .is_some_and(|&p| matches!(types.kind(p), TyKind::Param(param) if *param == SELF));
        let rest_ok = sig.params.iter().skip(1).all(|&p| !has_self(input, p));
        if !(first_ok && rest_ok && !has_self(input, sig.ret)) {
            offending.push(f);
        }
    }
    let Some(&first) = offending.first() else {
        return Ok(None);
    };
    let iface_text = r.iface_text(iface);
    let name = r.name(first);
    let mut diag = Diagnostic::new(
        codes::NOT_OBJECT_SAFE,
        format!("`dyn {iface_text}` is not allowed: `{name}` uses `Self` other than as its first parameter"),
    )
    .with_primary(span, format!("`dyn {iface_text}` written here"));
    let fn_label = format!("`{}`", r.fn_text(first));
    match r.item(first).and_then(|item| item.span) {
        Some(fn_span) => diag = diag.with_secondary(fn_span, fn_label),
        None => diag = diag.with_note(format!("{fn_label} is declared in `{iface_text}`")),
    }
    if let Some(parent) = r.parent(first).filter(|&p| p != iface) {
        diag = diag.with_note(format!(
            "`{name}` comes from `{}`, which `{iface_text}` embeds",
            r.iface_text(parent)
        ));
    }
    if offending.len() > 1 {
        let others: Vec<String> = offending
            .iter()
            .skip(1)
            .map(|&f| format!("`{}`", r.name(f)))
            .collect();
        diag = diag.with_note(format!("so do {}", others.join(", ")));
    }
    Ok(Some(
        diag.with_note(
            "a `dyn` value can supply `Self` only as the first argument of each function, and nowhere else (§8.6, D-117)",
        )
        .with_help("put the functions that `dyn` needs in an interface of their own"),
    ))
}

/// E0066 for every interface declaration of the module that breaks a rule
/// of §8.1 or §8.3: a function without `Self` in a parameter (D-209,
/// ADR-0037), a function with the name of one the interface inherits by
/// embedding, two embedded functions with the same name and different
/// signatures (identical ones count once), and cyclic embedding.
///
/// Type parameters on interfaces or their functions (D-165) cannot be
/// written (the grammar has none), and duplicate functions within one
/// interface are E0041 from name resolution.
///
/// # Errors
///
/// An [`Ice`] if an interface cannot be read.
pub(crate) fn check_interface_decls(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
) -> Result<Vec<Diagnostic>, Ice> {
    let r = Reader { input, own };
    let mut diags = Vec::new();
    let mut in_cycle: Vec<DefId> = Vec::new();
    for (index, item) in own.items.iter_enumerated() {
        if item.kind != DefKind::Interface {
            continue;
        }
        let ItemSig::Interface(def) = &item.sig else {
            continue;
        };
        let iface = input.def_id(index);
        let span = item.span;
        if !in_cycle.contains(&iface)
            && let Some(cycle) = find_cycle(&r, iface)
        {
            diags.extend(cycle_diag(&r, &cycle, span));
            in_cycle.extend(cycle);
        }
        diags.extend(embedding_conflicts(&r, iface, def, span)?);
        for &f_index in &def.fns {
            let f = input.def_id(f_index);
            let f_item = r.try_item(f)?;
            let Some(sig) = r.fn_sig(f) else {
                continue;
            };
            if sig.params.iter().any(|&p| has_self(input, p)) {
                continue;
            }
            if let Some(f_span) = f_item.span {
                diags.push(
                    Diagnostic::new(
                        codes::INVALID_INTERFACE,
                        format!(
                            "interface function `{}` must take `Self` in a parameter",
                            r.name(f)
                        ),
                    )
                    .with_primary(f_span, "`Self` appears in no parameter")
                    .with_note(
                        "the type that satisfies the interface is found from the arguments, so a function with `Self` only in its result cannot be declared in v1 (§8.1, D-209)",
                    )
                    .with_help(
                        "add a `Self` parameter, or construct values with an ordinary value such as a decoder",
                    ),
                );
            }
        }
    }
    Ok(diags)
}

/// A cycle of embedding through `start`: `[start, …]`, each embedding
/// the next and the last embedding `start`. Only this module's interfaces
/// can form one, because module imports are acyclic.
fn find_cycle(r: &Reader<'_, '_>, start: DefId) -> Option<Vec<DefId>> {
    let mut path = vec![start];
    let mut seen = vec![start];
    if walk_cycle(r, start, start, &mut path, &mut seen) {
        Some(path)
    } else {
        None
    }
}

fn walk_cycle(
    r: &Reader<'_, '_>,
    start: DefId,
    at: DefId,
    path: &mut Vec<DefId>,
    seen: &mut Vec<DefId>,
) -> bool {
    let Some(def) = r.interface(at) else {
        return false;
    };
    for &embed in &def.embeds {
        if embed == start {
            return true;
        }
        if seen.contains(&embed) {
            continue;
        }
        seen.push(embed);
        path.push(embed);
        if walk_cycle(r, start, embed, path, seen) {
            return true;
        }
        path.pop();
    }
    false
}

fn cycle_diag(r: &Reader<'_, '_>, cycle: &[DefId], span: Option<Span>) -> Option<Diagnostic> {
    let span = span?;
    let first = *cycle.first()?;
    let name = r.name(first);
    // "`A` embeds `B`, which embeds `A`".
    let mut chain = format!("`{name}` embeds");
    for (n, &d) in cycle
        .iter()
        .skip(1)
        .chain(std::iter::once(&first))
        .enumerate()
    {
        if n > 0 {
            chain.push_str(", which embeds");
        }
        let _ = write!(chain, " `{}`", r.name(d));
    }
    let mut diag = Diagnostic::new(
        codes::INVALID_INTERFACE,
        format!("interface `{name}` embeds itself"),
    )
    .with_primary(span, "embedding cycle");
    for &other in cycle.iter().skip(1) {
        if let Some(other_span) = r.item(other).and_then(|item| item.span) {
            diag = diag.with_secondary(
                other_span,
                format!("`{}` is part of the cycle", r.name(other)),
            );
        }
    }
    Some(
        diag.with_note(format!("{chain} (§8.3)"))
            .with_help("remove one of the embeddings in the cycle"),
    )
}

/// Functions with the same name from two embedded interfaces with
/// different signatures, and own functions with the name of an embedded
/// one, even with the same signature (§8.1, §8.3,
/// GAP(own-and-embedded-name)).
fn embedding_conflicts(
    r: &Reader<'_, '_>,
    iface: DefId,
    def: &InterfaceDef,
    span: Option<Span>,
) -> Result<Vec<Diagnostic>, Ice> {
    let mut diags = Vec::new();
    let Some(span) = span else {
        return Ok(diags);
    };
    let name_of = |f: DefId| r.item(f).map(|item| item.name);
    // Each direct embed's functions; conflicts inside one embed are that
    // embed's own error.
    let groups: Vec<Vec<DefId>> = def
        .embeds
        .iter()
        .filter(|&&e| e != iface)
        .map(|&e| r.closure_fns(e))
        .collect();
    let mut inherited: Vec<DefId> = Vec::new();
    let mut reported: Vec<cheby_iface::Name> = Vec::new();
    for group in &groups {
        for &f in group {
            // Through a cycle, the interface's own functions come back.
            if inherited.contains(&f) || r.parent(f) == Some(iface) {
                continue;
            }
            let name = name_of(f);
            let clash = inherited.iter().copied().find(|&g| {
                name_of(g) == name
                    && r.fn_sig(g).is_some()
                    && r.fn_sig(f).is_some()
                    && r.fn_sig(g) != r.fn_sig(f)
            });
            if let (Some(g), Some(name)) = (clash, name)
                && !reported.contains(&name)
            {
                reported.push(name);
                let mut diag = Diagnostic::new(
                    codes::INVALID_INTERFACE,
                    format!(
                        "`{}` inherits two different functions named `{}`",
                        r.name(iface),
                        r.input.globals.names.get(name)
                    ),
                )
                .with_primary(span, "embeds both");
                for h in [g, f] {
                    let label = format!(
                        "`{}::{}` is `{}`",
                        r.iface_text(r.parent(h).unwrap_or(iface)),
                        r.name(h),
                        r.fn_text(h)
                    );
                    match r.item(h).and_then(|item| item.span) {
                        Some(h_span) => diag = diag.with_secondary(h_span, label),
                        None => diag = diag.with_note(label),
                    }
                }
                diags.push(
                    diag.with_note(
                        "embedded interfaces may require the same function only with identical signatures, so no type could satisfy this interface (§8.3)",
                    )
                    .with_help("rename one of the functions"),
                );
            }
            inherited.push(f);
        }
    }
    for &f_index in &def.fns {
        let f = DefId::new(iface.module, f_index);
        let name = name_of(f);
        let Some(g) = inherited.iter().copied().find(|&g| name_of(g) == name) else {
            continue;
        };
        let Some(f_span) = r.try_item(f)?.span else {
            continue;
        };
        let embedded_from = r.parent(g).unwrap_or(iface);
        let mut diag = Diagnostic::new(
            codes::INVALID_INTERFACE,
            format!(
                "`{}` is already required by the embedded interface `{}`",
                r.name(f),
                r.iface_text(embedded_from)
            ),
        )
        .with_primary(f_span, "declared again here");
        if let Some(g_span) = r.item(g).and_then(|item| item.span) {
            diag = diag.with_secondary(g_span, "first required here");
        }
        diags.push(
            diag.with_note(
                "function names must be distinct within an interface, including the functions it inherits by embedding (§8.1)",
            )
            .with_help("remove this function or rename it"),
        );
    }
    Ok(diags)
}
