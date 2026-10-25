//! Item collection (B4): every item in `DefIndex` order, duplicate names
//! per namespace (E0041, §4.1.2), attributes (E0045, §4.8, D-461) and
//! functions without a body (E0046).
//!
//! The rules of D-461 are E0045 here: `@external` only on functions and
//! bodiless types, at most once per target, in one of its two forms with
//! non-empty strings; `@target` and `@!target` with exactly one target,
//! `@target` on every top-level item kind; `@blocking` and `@async`
//! without arguments, on functions with an `@external` (a JS one for
//! `@async`); `@deprecated` with one message string, on `pub` items only;
//! no attribute but `@external` twice on an item; `@!target` at most once.
//! Attributes that leave an item on no target at all are E0045 too
//! (D-513), and so is a bound on a type parameter of a function with an
//! `@external` (§12.5, D-554). Attributes on a variant, a record field or
//! an interface function are the parser's E0045, so they never reach this
//! pass (D-514).
//!
//! The standard library may also write three attributes of its own
//! (§4.8, D-298, D-303, D-342, D-482, D-692): `@std_clamped(param, …)` on a
//! function names the count parameters it clamps,
//! `@std_hooks(equal_fn, hash_fn, debug_fn)` on a type names the functions
//! of its module that implement its equality, hashing and debug printing,
//! and `@std_context`, without arguments on a function with an `@external`,
//! makes its native kernel receive the running fiber's context first.
//! In any other package they are unknown attributes. `@std_context` on a
//! `@blocking` function is E0045, since a blocking kernel runs on a pool
//! thread, which never receives the context (D-191, D-819).

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{DefIndex, DefKind, Ice, IndexVec, Name, Namespace, PackageId, Vis};
use cheby_package::{Target, Targets};
use cheby_syntax::NodeId;
use cheby_syntax::ast::{
    AstChildren, AstNode, AstToken, Attr, AttrArg, FnDecl, InnerAttr, Item, SourceFile, TypeBody,
    TypeParamList, Visibility, VisibilityKind,
};
use rustc_hash::FxHashMap;

use crate::{ExternalAttr, ItemAttrs, ItemDef, ResolveInput};

/// The result of [`collect`].
pub(crate) struct Collected {
    pub items: IndexVec<DefIndex, ItemDef>,
    /// Where the module's code is built: the package's targets (D-190),
    /// narrowed by `@!target` (§12.6, D-141). Published as
    /// [`crate::ResolvedModule::targets`], so the cross-target check (C12)
    /// knows the package's targets without another input.
    pub module_targets: Targets,
}

/// Where an attribute is written.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Placement {
    Fn { has_body: bool },
    Type { external: bool },
    Alias,
    Const,
    Interface,
    Test,
}

impl Placement {
    const fn describe(self) -> &'static str {
        match self {
            Self::Fn { .. } => "a function",
            Self::Type { external: true } => "an external type",
            Self::Type { external: false } => "a type",
            Self::Alias => "a type alias",
            Self::Const => "a constant",
            Self::Interface => "an interface",
            Self::Test => "a test",
        }
    }
}

/// Native symbol prefixes only the toolchain may bind (E0055): the
/// runtime's internal functions and data, and generated code (D-455, D-456).
const RESERVED_SYMBOL_PREFIXES: [&str; 2] = ["cheby_rt_", "cb$"];

/// E0055 for an `@external(native, …)` outside the standard library that
/// names `symbol`, a name the runtime exports (D-944), at `span`, the
/// symbol's string literal; `None` when `exported` says the runtime does
/// not export it, or when the symbol has a reserved prefix, which
/// resolution reports already. The driver calls it after resolution with
/// the runtime's names (`cheby_runtime::symbols()`), so this crate does
/// not depend on the runtime. GAP(e0055-runtime-export-note): the note
/// says the runtime exports the symbol, where the prefixes' note names the
/// prefix.
#[must_use]
pub fn runtime_export_reserved(
    symbol: &str,
    span: Span,
    exported: &dyn Fn(&str) -> bool,
) -> Option<Diagnostic> {
    if RESERVED_SYMBOL_PREFIXES
        .iter()
        .any(|prefix| symbol.starts_with(prefix))
        || !exported(symbol)
    {
        return None;
    }
    Some(
        Diagnostic::new(
            codes::RESERVED_NATIVE_SYMBOL,
            format!("the native symbol `{symbol}` is reserved for the toolchain"),
        )
        .with_primary(span, "")
        .with_note(
            "the Cheby runtime exports this symbol, so only the standard library may bind it",
        ),
    )
}

const ATTRIBUTE_LIST: &str =
    "the attributes are `@external`, `@target`, `@blocking`, `@async` and `@deprecated`";

/// The attributes every package may write (§4.8).
const ATTRIBUTES: [&str; 5] = ["external", "target", "blocking", "async", "deprecated"];

/// The attributes only the standard library may write (D-482, D-692).
const STD_ATTRIBUTES: [&str; 3] = ["std_clamped", "std_hooks", "std_context"];

/// Why a function without a body can end up on no target.
const BODILESS: &str = "a function without a body exists only on the targets of its \
                        `@external` attributes";

pub(crate) fn collect(
    input: &ResolveInput<'_>,
    diags: &mut Vec<Diagnostic>,
) -> Result<Collected, Ice> {
    let mut cx = Cx {
        input,
        diags,
        items: IndexVec::new(),
        params: Vec::new(),
        types: FxHashMap::default(),
        values: FxHashMap::default(),
        tests: FxHashMap::default(),
        module_target: None,
        item_target: None,
    };
    let Some(file) = SourceFile::cast(input.tree.root()) else {
        return Err(Ice::new("the tree's root is not a source file"));
    };
    let module_targets = cx.inner_attrs(file.inner_attrs());
    for item in file.items() {
        cx.item(item, module_targets)?;
    }
    cx.check_hooks();
    Ok(Collected {
        items: cx.items,
        module_targets: input.package_targets.intersection(module_targets),
    })
}

struct Cx<'a, 'd> {
    input: &'a ResolveInput<'a>,
    diags: &'d mut Vec<Diagnostic>,
    items: IndexVec<DefIndex, ItemDef>,
    /// The parameter names of the function whose attributes are being
    /// checked, for `@std_clamped`.
    params: Vec<Name>,
    types: FxHashMap<Name, Span>,
    values: FxHashMap<Name, Span>,
    tests: FxHashMap<Name, Span>,
    /// The module's `@!target`, once read.
    module_target: Option<(Target, Span)>,
    /// The `@target` of the item whose attributes were checked last.
    item_target: Option<(Target, Span)>,
}

impl Cx<'_, '_> {
    fn intern(&self, text: &str) -> Name {
        self.input.globals.names.intern(text)
    }

    #[expect(clippy::too_many_lines, reason = "one arm per item kind")]
    fn item(&mut self, item: Item<'_>, module_targets: Targets) -> Result<(), Ice> {
        let vis = vis_of(item.visibility());
        match item {
            Item::Fn(f) => {
                let Some(name) = f.name() else { return Ok(()) };
                let has_body = f.body().is_some();
                self.params = f
                    .params()
                    .filter_map(cheby_syntax::ast::Param::name)
                    .map(|n| self.intern(n.text()))
                    .collect();
                let attrs = self.attrs(f.attrs(), Placement::Fn { has_body }, vis);
                let attrs = self.narrow(
                    attrs,
                    module_targets,
                    &format!("`{}`", name.text()),
                    name.span(),
                );
                if let Some(external) = attrs.externals.first() {
                    self.external_bounds(f, name.text(), external_span(external));
                }
                if !has_body && attrs.externals.is_empty() {
                    self.diags.push(
                        Diagnostic::new(
                            codes::MISSING_BODY,
                            format!("the function `{}` has no body", name.text()),
                        )
                        .with_primary(name.span(), "")
                        .with_help(
                            "add a body, or an `@external` attribute that names its foreign \
                             implementation",
                        ),
                    );
                }
                self.push(
                    name.text(),
                    name.span(),
                    DefKind::Fn,
                    vis,
                    None,
                    f.syntax().id(),
                    attrs,
                )?;
            }
            Item::Type(t) => {
                let Some(name) = t.name() else { return Ok(()) };
                let body = t.body();
                let placement = Placement::Type {
                    external: body.is_none(),
                };
                let attrs = self.attrs(t.attrs(), placement, vis);
                let attrs = self.narrow(
                    attrs,
                    module_targets,
                    &format!("`{}`", name.text()),
                    name.span(),
                );
                // Constructors exist where their type does; the hooks
                // belong to the type alone.
                let member_attrs = ItemAttrs {
                    hooks: None,
                    ..attrs.clone()
                };
                let ty = self.push(
                    name.text(),
                    name.span(),
                    DefKind::Type,
                    vis,
                    None,
                    t.syntax().id(),
                    attrs,
                )?;
                match body {
                    Some(TypeBody::Variants(list)) => {
                        for variant in list.variants() {
                            let Some(ctor) = variant.name() else { continue };
                            self.push(
                                ctor.text(),
                                ctor.span(),
                                DefKind::Ctor,
                                vis,
                                Some(ty),
                                variant.syntax().id(),
                                member_attrs.clone(),
                            )?;
                        }
                    }
                    // The single-variant shorthand (§4.3.3): one constructor
                    // named like the type; its node is the field list.
                    Some(TypeBody::Record(fields)) => {
                        self.push(
                            name.text(),
                            name.span(),
                            DefKind::Ctor,
                            vis,
                            Some(ty),
                            fields.syntax().id(),
                            member_attrs,
                        )?;
                    }
                    None => {}
                }
            }
            Item::Alias(a) => {
                let Some(name) = a.name() else { return Ok(()) };
                let attrs = self.attrs(a.attrs(), Placement::Alias, vis);
                let attrs = self.narrow(
                    attrs,
                    module_targets,
                    &format!("`{}`", name.text()),
                    name.span(),
                );
                self.push(
                    name.text(),
                    name.span(),
                    DefKind::Alias,
                    vis,
                    None,
                    a.syntax().id(),
                    attrs,
                )?;
            }
            Item::Const(c) => {
                let Some(name) = c.name() else { return Ok(()) };
                let attrs = self.attrs(c.attrs(), Placement::Const, vis);
                let attrs = self.narrow(
                    attrs,
                    module_targets,
                    &format!("`{}`", name.text()),
                    name.span(),
                );
                self.push(
                    name.text(),
                    name.span(),
                    DefKind::Const,
                    vis,
                    None,
                    c.syntax().id(),
                    attrs,
                )?;
            }
            Item::Interface(i) => {
                let Some(name) = i.name() else { return Ok(()) };
                let attrs = self.attrs(i.attrs(), Placement::Interface, vis);
                let attrs = self.narrow(
                    attrs,
                    module_targets,
                    &format!("`{}`", name.text()),
                    name.span(),
                );
                let iface = self.push(
                    name.text(),
                    name.span(),
                    DefKind::Interface,
                    vis,
                    None,
                    i.syntax().id(),
                    attrs.clone(),
                )?;
                let mut seen: FxHashMap<Name, Span> = FxHashMap::default();
                for f in i.fns() {
                    let Some(fname) = f.name() else { continue };
                    let interned = self.intern(fname.text());
                    if let Some(&first) = seen.get(&interned) {
                        self.diags
                            .push(duplicate(fname.text(), fname.span(), first, "interface"));
                    } else {
                        seen.insert(interned, fname.span());
                    }
                    self.push(
                        fname.text(),
                        fname.span(),
                        DefKind::InterfaceFn,
                        vis,
                        Some(iface),
                        f.syntax().id(),
                        attrs.clone(),
                    )?;
                }
            }
            Item::Test(t) => {
                let Some(name) = t.name() else { return Ok(()) };
                let text = name.plain_value().unwrap_or_default();
                let span = name.significant_span();
                let attrs = self.attrs(t.attrs(), Placement::Test, vis);
                let attrs =
                    self.narrow(attrs, module_targets, &format!("the test \"{text}\""), span);
                self.push(
                    &text,
                    span,
                    DefKind::Test,
                    Vis::Priv,
                    None,
                    t.syntax().id(),
                    attrs,
                )?;
            }
        }
        Ok(())
    }

    #[expect(clippy::too_many_arguments, reason = "one call site per item kind")]
    fn push(
        &mut self,
        text: &str,
        span: Span,
        kind: DefKind,
        vis: Vis,
        parent: Option<DefIndex>,
        node: NodeId,
        attrs: ItemAttrs,
    ) -> Result<DefIndex, Ice> {
        let name = self.intern(text);
        let seen = match kind.namespace() {
            Some(Namespace::Type) => Some((&mut self.types, "type")),
            Some(Namespace::Value) => Some((&mut self.values, "value")),
            None if kind == DefKind::Test => Some((&mut self.tests, "test")),
            None => None,
        };
        if let Some((seen, what)) = seen {
            if let Some(&first) = seen.get(&name) {
                self.diags.push(duplicate(text, span, first, what));
            } else {
                seen.insert(name, span);
            }
        }
        self.items.push(ItemDef {
            name,
            name_span: span,
            kind,
            vis,
            parent,
            node,
            attrs,
        })
    }

    /// Checks the attributes of an item and returns what they say.
    #[expect(clippy::too_many_lines, reason = "one branch per attribute")]
    fn attrs(
        &mut self,
        attrs: AstChildren<'_, Attr<'_>>,
        placement: Placement,
        vis: Vis,
    ) -> ItemAttrs {
        self.item_target = None;
        let mut out = ItemAttrs::default();
        let mut seen: Vec<(&'static str, Span)> = Vec::new();
        let mut external_targets: Vec<(Target, Span)> = Vec::new();
        let mut target: Option<Target> = None;
        let mut blocking: Option<Span> = None;
        let mut is_async: Option<Span> = None;
        let mut std_context: Option<Span> = None;
        for attr in attrs {
            let span = attr.significant_span();
            let Some(name) = attr.name() else { continue };
            let Some(known) = self.known_attr(name.text()) else {
                self.diags.push(
                    Diagnostic::new(
                        codes::INVALID_ATTRIBUTE,
                        format!("unknown attribute `@{}`", name.text()),
                    )
                    .with_primary(span, "")
                    .with_help(ATTRIBUTE_LIST),
                );
                continue;
            };
            let args: Vec<AttrArg<'_>> = attr.args().collect();
            if known == "external" {
                let Some(external) = self.external(&args, span) else {
                    continue;
                };
                let t = external_target(&external);
                if let Some(&(_, first)) = external_targets.iter().find(|(x, _)| *x == t) {
                    self.diags.push(
                        Diagnostic::new(
                            codes::INVALID_ATTRIBUTE,
                            format!("two `@external` attributes for the `{t}` target"),
                        )
                        .with_primary(span, "")
                        .with_secondary(first, "the first one")
                        .with_note("an item may have at most one `@external` per target"),
                    );
                    continue;
                }
                if !matches!(
                    placement,
                    Placement::Fn { .. } | Placement::Type { external: true }
                ) {
                    self.misplaced(span, "external", placement, Some(external_help(placement)));
                    continue;
                }
                external_targets.push((t, span));
                out.externals.push(external);
                continue;
            }
            if let Some(&(_, first)) = seen.iter().find(|(n, _)| *n == known) {
                self.diags.push(
                    Diagnostic::new(
                        codes::INVALID_ATTRIBUTE,
                        format!("duplicate `@{known}` attribute"),
                    )
                    .with_primary(span, "")
                    .with_secondary(first, "first written here"),
                );
                continue;
            }
            seen.push((known, span));
            match known {
                "target" => {
                    let Some(t) = self.target_arg(&args, span, "@target") else {
                        continue;
                    };
                    target = Some(t);
                    self.item_target = Some((t, span));
                }
                "blocking" | "async" | "std_context" => {
                    if !args.is_empty() || attr.arg_list().is_some() {
                        self.diags.push(
                            Diagnostic::new(
                                codes::INVALID_ATTRIBUTE,
                                format!("`@{known}` takes no arguments"),
                            )
                            .with_primary(span, ""),
                        );
                        continue;
                    }
                    match known {
                        "blocking" => blocking = Some(span),
                        "async" => is_async = Some(span),
                        _ => std_context = Some(span),
                    }
                }
                "deprecated" => {
                    let message = match args.as_slice() {
                        [AttrArg::String(s)] => s.plain_value(),
                        _ => None,
                    };
                    let Some(message) = message else {
                        self.diags.push(
                            Diagnostic::new(
                                codes::INVALID_ATTRIBUTE,
                                "`@deprecated` takes one message string",
                            )
                            .with_primary(span, "")
                            .with_help("write it as `@deprecated(\"use `other` instead\")`"),
                        );
                        continue;
                    };
                    if vis != Vis::Pub || matches!(placement, Placement::Test) {
                        self.diags.push(
                            Diagnostic::new(
                                codes::INVALID_ATTRIBUTE,
                                "`@deprecated` is allowed only on `pub` items",
                            )
                            .with_primary(span, "")
                            .with_note(
                                "it warns about uses from other modules, so it applies to the \
                                 public API",
                            ),
                        );
                        continue;
                    }
                    out.deprecated = Some(message.into_boxed_str());
                }
                "std_clamped" => {
                    if !matches!(placement, Placement::Fn { .. }) {
                        self.misplaced(span, known, placement, None);
                        continue;
                    }
                    out.clamped = self.clamped(&args, span);
                }
                "std_hooks" => {
                    if !matches!(placement, Placement::Type { .. }) {
                        self.misplaced(span, known, placement, None);
                        continue;
                    }
                    out.hooks = self.hooks(&args, span);
                }
                _ => {}
            }
        }
        let has_external = !external_targets.is_empty();
        let has_js = external_targets.iter().any(|(t, _)| *t == Target::Js);
        let is_fn = matches!(placement, Placement::Fn { .. });
        if let Some(span) = blocking {
            if is_fn && has_external {
                out.blocking = true;
            } else {
                self.misplaced(
                    span,
                    "blocking",
                    placement,
                    Some("`@blocking` applies to functions with an `@external` attribute"),
                );
            }
        }
        if let Some(span) = is_async {
            if is_fn && has_js {
                out.is_async = true;
            } else {
                self.misplaced(
                    span,
                    "async",
                    placement,
                    Some("`@async` applies to functions with an `@external(js, …)` attribute"),
                );
            }
        }
        if let Some(span) = std_context {
            if let (true, Some(first)) = (out.blocking, blocking) {
                // D-819: a blocking call runs its
                // kernel on a pool thread, which must never receive the
                // fiber's context (D-191, D-683). The function keeps
                // `@blocking`.
                self.diags.push(
                    Diagnostic::new(
                        codes::INVALID_ATTRIBUTE,
                        "`@std_context` is not allowed on a `@blocking` function",
                    )
                    .with_primary(span, "")
                    .with_secondary(first, "`@blocking` here")
                    .with_note(
                        "a blocking call runs on a pool thread, which never receives the \
                         running fiber's context",
                    ),
                );
            } else if is_fn && has_external {
                out.std_context = true;
            } else {
                self.misplaced(
                    span,
                    "std_context",
                    placement,
                    Some("`@std_context` applies to functions with an `@external` attribute"),
                );
            }
        }
        out.targets = target.map_or(Targets::ALL, Targets::only);
        if let Placement::Fn { has_body: false } = placement
            && has_external
        {
            let externals = external_targets
                .iter()
                .fold(Targets::NONE, |acc, &(t, _)| acc.with(t));
            out.targets = out.targets.intersection(externals);
        }
        out
    }

    /// Narrows an item's targets by its module's `@!target`, and reports
    /// E0045 when its attributes leave it on no target at all (D-513):
    /// `@target` against the module's `@!target`, or a function without a
    /// body whose `@external`s miss the only target its `@target` or module
    /// allows. Such an item could never be compiled, which is never what was
    /// meant. `what` names the item: "`f`" or "the test \"t\"".
    fn narrow(&mut self, attrs: ItemAttrs, module: Targets, what: &str, name: Span) -> ItemAttrs {
        let attrs = with_module_targets(attrs, module);
        if !attrs.targets.is_empty() {
            return attrs;
        }
        let message = format!("{what} exists on no target");
        let diag = match (self.item_target, self.module_target) {
            (Some((t, span)), Some((m, module_span))) if t != m => {
                Diagnostic::new(codes::INVALID_ATTRIBUTE, message)
                    .with_primary(span, format!("only on `{t}` here"))
                    .with_secondary(module_span, format!("but the module exists only on `{m}`"))
                    .with_help(format!(
                        "remove `@target({t})`, or move the item to a module for `{t}`"
                    ))
            }
            (Some((t, span)), _) => Diagnostic::new(codes::INVALID_ATTRIBUTE, message)
                .with_primary(span, format!("only on `{t}` here"))
                .with_note(format!(
                    "{BODILESS}, and {what} has no `@external` for `{t}`"
                ))
                .with_help(format!(
                    "add an `@external({t}, …)` attribute, or remove `@target({t})`"
                )),
            (None, Some((m, module_span))) => Diagnostic::new(codes::INVALID_ATTRIBUTE, message)
                .with_primary(name, "")
                .with_secondary(module_span, format!("the module exists only on `{m}`"))
                .with_note(format!(
                    "{BODILESS}, and {what} has no `@external` for `{m}`"
                ))
                .with_help(format!("add an `@external({m}, …)` attribute")),
            (None, None) => return attrs,
        };
        self.diags.push(diag);
        attrs
    }

    /// E0045 at each bound on a type parameter of a function with an
    /// `@external`, with or without a body (§12.5, D-207, D-554): a bound
    /// passes a function package and a type descriptor, and neither has a
    /// foreign ABI.
    fn external_bounds(&mut self, f: FnDecl<'_>, name: &str, external: Span) {
        let params = f.type_params().into_iter().flat_map(TypeParamList::params);
        for param in params {
            let Some(bounds) = param.bounds() else {
                continue;
            };
            let param_name = param.name().map_or("", |n| n.text());
            self.diags.push(
                Diagnostic::new(
                    codes::INVALID_ATTRIBUTE,
                    format!(
                        "the type parameter `{param_name}` of `{name}` has a bound, but `{name}` \
                         has an `@external` attribute"
                    ),
                )
                .with_primary(bounds.significant_span(), "bounds are not allowed here")
                .with_secondary(external, "the foreign implementation is named here")
                .with_note(
                    "a bound passes a function package and a type descriptor, which have no \
                     foreign ABI",
                )
                .with_help(format!(
                    "remove the bound from `{param_name}`, and call `{name}` from a Cheby \
                     function that uses the bound"
                )),
            );
        }
    }

    fn misplaced(
        &mut self,
        span: Span,
        name: &str,
        placement: Placement,
        help: Option<&'static str>,
    ) {
        let mut diag = Diagnostic::new(
            codes::INVALID_ATTRIBUTE,
            format!("`@{name}` is not allowed on {}", placement.describe()),
        )
        .with_primary(span, "");
        if let Some(help) = help {
            diag = diag.with_help(help);
        }
        self.diags.push(diag);
    }

    /// Reads `@external(native, "symbol")` or `@external(js, "module",
    /// "name")`.
    fn external(&mut self, args: &[AttrArg<'_>], span: Span) -> Option<ExternalAttr> {
        let target = match args.first() {
            Some(AttrArg::Name(name)) => parse_target(name.text()),
            _ => None,
        };
        let strings: Option<Vec<String>> = args
            .get(1..)
            .unwrap_or_default()
            .iter()
            .map(|arg| match arg {
                AttrArg::String(s) => s.plain_value(),
                AttrArg::Name(_) => None,
            })
            .collect();
        let external = match (target, strings.as_deref()) {
            (Some(Target::Native), Some([symbol])) if !symbol.is_empty() => {
                let symbol_span = match args.get(1) {
                    Some(AttrArg::String(s)) => s.significant_span(),
                    _ => span,
                };
                self.reserved_symbol(symbol, symbol_span);
                Some(ExternalAttr::Native {
                    symbol: symbol.clone().into_boxed_str(),
                    span,
                    symbol_span,
                })
            }
            (Some(Target::Js), Some([module, name])) if !module.is_empty() && !name.is_empty() => {
                Some(ExternalAttr::Js {
                    module: module.clone().into_boxed_str(),
                    name: name.clone().into_boxed_str(),
                    span,
                })
            }
            _ => None,
        };
        if external.is_none() {
            self.diags.push(
                Diagnostic::new(codes::INVALID_ATTRIBUTE, "malformed `@external` attribute")
                    .with_primary(span, "")
                    .with_help(
                        "write `@external(native, \"symbol\")` or `@external(js, \"./module.mjs\", \
                     \"name\")`",
                    ),
            );
        }
        external
    }

    /// E0055 for a native symbol in the toolchain's namespace: the
    /// runtime's internals (`cheby_rt_`) and generated code (`cb$`). Only
    /// the standard library may bind them (D-455).
    fn reserved_symbol(&mut self, symbol: &str, span: Span) {
        if self.input.package == PackageId::STD {
            return;
        }
        let Some(prefix) = RESERVED_SYMBOL_PREFIXES
            .iter()
            .find(|prefix| symbol.starts_with(**prefix))
        else {
            return;
        };
        self.diags.push(
            Diagnostic::new(
                codes::RESERVED_NATIVE_SYMBOL,
                format!("the native symbol `{symbol}` is reserved for the toolchain"),
            )
            .with_primary(span, "")
            .with_note(format!(
                "symbols starting with `{prefix}` belong to the Cheby runtime and generated code"
            )),
        );
    }

    /// The single target argument of `@target` or `@!target`.
    fn target_arg(&mut self, args: &[AttrArg<'_>], span: Span, what: &str) -> Option<Target> {
        let target = match args {
            [AttrArg::Name(name)] => parse_target(name.text()),
            _ => None,
        };
        if target.is_none() {
            self.diags.push(
                Diagnostic::new(
                    codes::INVALID_ATTRIBUTE,
                    format!("malformed `{what}` attribute"),
                )
                .with_primary(span, "")
                .with_help(format!("write `{what}(native)` or `{what}(js)`")),
            );
        }
        target
    }

    /// `@!target(…)`, the only inner attribute (§4.8).
    fn inner_attrs(&mut self, attrs: AstChildren<'_, InnerAttr<'_>>) -> Targets {
        let mut targets = Targets::ALL;
        let mut first: Option<Span> = None;
        for attr in attrs {
            let span = attr.significant_span();
            let Some(name) = attr.name() else { continue };
            if name.text() != "target" {
                self.diags.push(
                    Diagnostic::new(
                        codes::INVALID_ATTRIBUTE,
                        format!("unknown inner attribute `@!{}`", name.text()),
                    )
                    .with_primary(span, "")
                    .with_help("the only inner attribute is `@!target(native)` or `@!target(js)`"),
                );
                continue;
            }
            if let Some(first) = first {
                self.diags.push(
                    Diagnostic::new(codes::INVALID_ATTRIBUTE, "duplicate `@!target` attribute")
                        .with_primary(span, "")
                        .with_secondary(first, "first written here"),
                );
                continue;
            }
            first = Some(span);
            let args: Vec<AttrArg<'_>> = attr.args().collect();
            if let Some(t) = self.target_arg(&args, span, "@!target") {
                targets = Targets::only(t);
                self.module_target = Some((t, span));
            }
        }
        targets
    }

    /// The name of a known attribute: one of §4.8, or in the standard
    /// library one of its own (D-482).
    fn known_attr(&self, name: &str) -> Option<&'static str> {
        let std_only = if self.input.package == PackageId::STD {
            &STD_ATTRIBUTES[..]
        } else {
            &[]
        };
        ATTRIBUTES
            .iter()
            .chain(std_only)
            .copied()
            .find(|&known| known == name)
    }

    /// `@std_clamped(param, …)`: one or more distinct parameters of the
    /// function.
    fn clamped(&mut self, args: &[AttrArg<'_>], span: Span) -> Vec<(Name, Span)> {
        let mut out: Vec<(Name, Span)> = Vec::new();
        if args.is_empty() {
            self.diags.push(
                Diagnostic::new(
                    codes::INVALID_ATTRIBUTE,
                    "`@std_clamped` needs at least one parameter name",
                )
                .with_primary(span, "")
                .with_help("write `@std_clamped(count)`"),
            );
        }
        for arg in args {
            let ident = match *arg {
                AttrArg::Name(ident) => ident,
                AttrArg::String(string) => {
                    self.diags.push(
                        Diagnostic::new(
                            codes::INVALID_ATTRIBUTE,
                            "`@std_clamped` takes parameter names",
                        )
                        .with_primary(string.significant_span(), "not a name"),
                    );
                    continue;
                }
            };
            let name = self.intern(ident.text());
            let arg_span = ident.span();
            if !self.params.contains(&name) {
                self.diags.push(
                    Diagnostic::new(
                        codes::INVALID_ATTRIBUTE,
                        format!("the function has no parameter `{}`", ident.text()),
                    )
                    .with_primary(arg_span, "not a parameter")
                    .with_note("`@std_clamped` names the count parameters the function clamps"),
                );
                continue;
            }
            if let Some(&(_, first)) = out.iter().find(|(n, _)| *n == name) {
                self.diags.push(
                    Diagnostic::new(
                        codes::INVALID_ATTRIBUTE,
                        format!("`{}` is named twice in `@std_clamped`", ident.text()),
                    )
                    .with_primary(arg_span, "")
                    .with_secondary(first, "first named here"),
                );
                continue;
            }
            out.push((name, arg_span));
        }
        out
    }

    /// `@std_hooks(equal_fn, hash_fn, debug_fn)`: exactly three function
    /// names. Whether they name functions of the module is checked once
    /// every item is collected ([`Self::check_hooks`]).
    fn hooks(&mut self, args: &[AttrArg<'_>], span: Span) -> Option<[(Name, Span); 3]> {
        let names: Vec<(Name, Span)> = args
            .iter()
            .map_while(|arg| match arg {
                AttrArg::Name(ident) => Some((self.intern(ident.text()), ident.span())),
                AttrArg::String(_) => None,
            })
            .collect();
        let hooks = match (names.len() == args.len(), names.as_slice()) {
            (true, &[equal, hash, debug]) => Some([equal, hash, debug]),
            _ => None,
        };
        if hooks.is_none() {
            self.diags.push(
                Diagnostic::new(codes::INVALID_ATTRIBUTE, "malformed `@std_hooks` attribute")
                    .with_primary(span, "")
                    .with_help(
                        "write `@std_hooks(equal_fn, hash_fn, debug_fn)` with the names of three \
                         functions of this module",
                    ),
            );
        }
        hooks
    }

    /// E0042 for an `@std_hooks` name that is not a function of the
    /// module.
    fn check_hooks(&mut self) {
        let mut missing = Vec::new();
        for item in &self.items {
            let Some(hooks) = &item.attrs.hooks else {
                continue;
            };
            if item.kind != DefKind::Type {
                continue;
            }
            for &(name, span) in hooks {
                let found = self
                    .items
                    .iter()
                    .any(|f| f.kind == DefKind::Fn && f.name == name);
                if !found {
                    missing.push((name, span));
                }
            }
        }
        for (name, span) in missing {
            let text = self.input.globals.names.get(name);
            self.diags.push(
                Diagnostic::new(
                    codes::UNRESOLVED_NAME,
                    format!("cannot find function `{text}` in this module"),
                )
                .with_primary(span, "not found")
                .with_note("`@std_hooks` names functions declared in the type's own module"),
            );
        }
    }
}

fn duplicate(text: &str, span: Span, first: Span, what: &str) -> Diagnostic {
    let message = match what {
        "test" => format!("two tests are named \"{text}\""),
        "interface" => format!("the interface declares `{text}` twice"),
        _ => format!("`{text}` is declared twice in this module"),
    };
    Diagnostic::new(codes::DUPLICATE_NAME, message)
        .with_primary(span, "declared again here")
        .with_secondary(first, "first declared here")
}

const fn external_help(placement: Placement) -> &'static str {
    match placement {
        Placement::Type { external: false } => {
            "`@external` on a type names the drop function of an external type, which has no \
             body"
        }
        _ => "`@external` applies to functions and external types",
    }
}

const fn external_span(external: &ExternalAttr) -> Span {
    match external {
        ExternalAttr::Native { span, .. } | ExternalAttr::Js { span, .. } => *span,
    }
}

const fn external_target(external: &ExternalAttr) -> Target {
    match external {
        ExternalAttr::Native { .. } => Target::Native,
        ExternalAttr::Js { .. } => Target::Js,
    }
}

fn parse_target(text: &str) -> Option<Target> {
    match text {
        "native" => Some(Target::Native),
        "js" => Some(Target::Js),
        _ => None,
    }
}

fn vis_of(vis: Option<Visibility<'_>>) -> Vis {
    match vis.and_then(Visibility::kind) {
        Some(VisibilityKind::Pub) => Vis::Pub,
        Some(VisibilityKind::Priv) => Vis::Priv,
        None => Vis::Package,
    }
}

const fn with_module_targets(mut attrs: ItemAttrs, module: Targets) -> ItemAttrs {
    attrs.targets = attrs.targets.intersection(module);
    attrs
}

#[cfg(test)]
mod tests {
    //! The attribute rules of D-461 and §4.8, one module at a time, and
    //! the targets item collection computes.

    use std::sync::Arc;

    use cheby_diag::FileId;
    use cheby_iface::{DepView, Globals, InterfaceSet, ModuleInfo, ModuleTable, builtin};
    use cheby_package::ModulePath;

    use super::*;
    use crate::{ResolvedModule, StdCatalog, resolve_module};

    struct Resolved {
        module: ResolvedModule,
        diags: Vec<Diagnostic>,
        globals: Globals,
    }

    impl Resolved {
        /// `CODE: message` for every diagnostic, sorted.
        fn messages(&self) -> Vec<String> {
            let mut out: Vec<String> = self
                .diags
                .iter()
                .map(|d| format!("{}: {}", d.code, d.message))
                .collect();
            out.sort();
            out
        }

        fn targets(&self, name: &str) -> Targets {
            self.module
                .items
                .iter()
                .find(|item| self.globals.names.get(item.name) == name)
                .unwrap()
                .attrs
                .targets
        }
    }

    /// Resolves `text` as the root module of a package with `targets`,
    /// with no imports.
    fn resolve_in(package: PackageId, targets: Targets, text: &str) -> Resolved {
        let globals = Globals::new();
        let mut modules = ModuleTable::new();
        let module = modules
            .add(ModuleInfo {
                path: ModulePath::root("app"),
                package,
                file: None,
            })
            .unwrap();
        let (tree, errors) =
            cheby_syntax::parse(FileId::new(module.as_u32()), Arc::from(text)).into_parts();
        assert!(errors.is_empty(), "{errors:?}");
        let set = InterfaceSet::new(builtin::interface(&globals));
        let input = ResolveInput {
            module,
            package,
            tree: &tree,
            imports: &[],
            targets: &[],
            deps: DepView::new(&set, &[]),
            modules: &modules,
            globals: &globals,
            std: StdCatalog::default(),
            package_targets: targets,
        };
        let (module, diags) = resolve_module(&input).unwrap();
        Resolved {
            module,
            diags,
            globals,
        }
    }

    fn resolve(text: &str) -> Resolved {
        resolve_in(PackageId::LOCAL, Targets::ALL, text)
    }

    const NATIVE: Targets = Targets::only(Target::Native);
    const JS: Targets = Targets::only(Target::Js);

    #[test]
    fn external_only_on_functions_and_bodiless_types() {
        let r = resolve(
            "@external(native, \"a\")\nfn a()\n\n@external(native, \"drop\")\npub type Db\n\n@external(native, \"t\")\ntype T {\n  T,\n}\n\n@external(native, \"x\")\ntype X = Int\n\n@external(native, \"c\")\nconst c: Int = 1\n\n@external(native, \"i\")\ninterface I {\n  fn f(Self)\n}\n\n@external(native, \"t\")\ntest \"t\" {\n  Nil\n}\n",
        );
        assert_eq!(
            r.messages(),
            [
                "E0045: `@external` is not allowed on a constant",
                "E0045: `@external` is not allowed on a test",
                "E0045: `@external` is not allowed on a type",
                "E0045: `@external` is not allowed on a type alias",
                "E0045: `@external` is not allowed on an interface",
            ]
        );
    }

    #[test]
    fn external_forms_and_one_per_target() {
        let r = resolve(
            "@external(native, \"\")\nfn a()\n\n@external(js, \"./m.mjs\")\nfn b()\n\n@external(js, \"\", \"b\")\nfn c()\n\n@external(wasm, \"d\")\nfn d()\n\n@external(native, \"e\", \"f\")\nfn e()\n\n@external(js, \"./m.mjs\", \"f\")\n@external(js, \"./n.mjs\", \"f\")\nfn f()\n\n@external(native, \"g\")\n@external(js, \"./m.mjs\", \"g\")\nfn g()\n",
        );
        assert_eq!(
            r.messages(),
            [
                "E0045: malformed `@external` attribute",
                "E0045: malformed `@external` attribute",
                "E0045: malformed `@external` attribute",
                "E0045: malformed `@external` attribute",
                "E0045: malformed `@external` attribute",
                "E0045: two `@external` attributes for the `js` target",
                // The malformed ones leave their functions without an
                // implementation.
                "E0046: the function `a` has no body",
                "E0046: the function `b` has no body",
                "E0046: the function `c` has no body",
                "E0046: the function `d` has no body",
                "E0046: the function `e` has no body",
            ]
        );
        assert_eq!(r.targets("g"), Targets::ALL);
    }

    #[test]
    fn target_takes_one_target_on_every_top_level_item() {
        let r = resolve(
            "@target(native)\nfn a() {\n  Nil\n}\n\n@target(js)\ntype T {\n  T,\n}\n\n@target(native)\ntype X = Int\n\n@target(js)\nconst c: Int = 1\n\n@target(native)\ninterface I {\n  fn f(Self)\n}\n\n@target(js)\ntest \"t\" {\n  Nil\n}\n\n@target(native, js)\nfn b() {\n  Nil\n}\n\n@target\nfn d() {\n  Nil\n}\n\n@target(\"js\")\nfn e() {\n  Nil\n}\n",
        );
        assert_eq!(
            r.messages(),
            [
                "E0045: malformed `@target` attribute",
                "E0045: malformed `@target` attribute",
                "E0045: malformed `@target` attribute",
            ]
        );
        assert_eq!(r.targets("a"), NATIVE);
        assert_eq!(r.targets("T"), JS);
        assert_eq!(r.targets("X"), NATIVE);
        assert_eq!(r.targets("c"), JS);
        assert_eq!(r.targets("I"), NATIVE);
        assert_eq!(r.targets("f"), NATIVE);
        assert_eq!(r.targets("t"), JS);
        assert_eq!(r.targets("b"), Targets::ALL);
    }

    #[test]
    fn blocking_and_async_need_externals_and_no_arguments() {
        let r = resolve(
            "@external(native, \"a\")\n@blocking\nfn a()\n\n@blocking\nfn b() {\n  Nil\n}\n\n@external(js, \"./m.mjs\", \"c\")\n@async\nfn c()\n\n@external(native, \"d\")\n@async\nfn d()\n\n@external(native, \"e\")\n@blocking()\nfn e()\n\n@external(js, \"./m.mjs\", \"f\")\n@async(js)\nfn f()\n\n@blocking\ntype T {\n  T,\n}\n\n@external(native, \"g\")\n@blocking\nfn g() {\n  Nil\n}\n",
        );
        assert_eq!(
            r.messages(),
            [
                "E0045: `@async` is not allowed on a function",
                "E0045: `@async` takes no arguments",
                "E0045: `@blocking` is not allowed on a function",
                "E0045: `@blocking` is not allowed on a type",
                "E0045: `@blocking` takes no arguments",
            ]
        );
        let blocking = |name: &str| {
            r.module
                .items
                .iter()
                .find(|i| r.globals.names.get(i.name) == name)
                .unwrap()
                .attrs
                .blocking
        };
        assert!(blocking("a"));
        assert!(blocking("g"));
    }

    // D-819: `@std_context` hands the fiber's
    // context to the kernel, and a blocking kernel runs on a pool thread
    // (D-191), so the two never go together. In either order the function
    // keeps `@blocking` and loses `@std_context`.
    #[test]
    fn blocking_with_std_context_is_e0045() {
        let r = resolve_in(
            PackageId::STD,
            Targets::ALL,
            "@external(native, \"a\")\n@blocking\n@std_context\nfn a()\n\n@external(native, \"b\")\n@std_context\n@blocking\nfn b()\n\n@external(native, \"c\")\n@std_context\nfn c()\n\n@external(native, \"d\")\n@blocking\nfn d()\n",
        );
        assert_eq!(
            r.messages(),
            [
                "E0045: `@std_context` is not allowed on a `@blocking` function",
                "E0045: `@std_context` is not allowed on a `@blocking` function",
            ]
        );
        let attrs = |name: &str| {
            let attrs = &r
                .module
                .items
                .iter()
                .find(|i| r.globals.names.get(i.name) == name)
                .unwrap()
                .attrs;
            (attrs.blocking, attrs.std_context)
        };
        assert_eq!(attrs("a"), (true, false));
        assert_eq!(attrs("b"), (true, false));
        assert_eq!(attrs("c"), (false, true));
        assert_eq!(attrs("d"), (true, false));
        let labels: Vec<&str> = r.diags[0]
            .labels
            .iter()
            .map(|l| l.message.as_str())
            .collect();
        assert_eq!(labels, ["", "`@blocking` here"]);
    }

    #[test]
    fn deprecated_takes_one_message_on_pub_items() {
        let r = resolve(
            "@deprecated(\"use b\")\npub fn a() {\n  Nil\n}\n\n@deprecated(\"old\")\nfn b() {\n  Nil\n}\n\n@deprecated(\"old\")\npriv fn c() {\n  Nil\n}\n\n@deprecated\npub fn d() {\n  Nil\n}\n\n@deprecated(\"a\", \"b\")\npub fn e() {\n  Nil\n}\n\n@deprecated(old)\npub fn f() {\n  Nil\n}\n\n@deprecated(\"old\")\npub type T {\n  T,\n}\n\n@deprecated(\"old\")\ntest \"t\" {\n  Nil\n}\n",
        );
        assert_eq!(
            r.messages(),
            [
                "E0045: `@deprecated` is allowed only on `pub` items",
                "E0045: `@deprecated` is allowed only on `pub` items",
                "E0045: `@deprecated` is allowed only on `pub` items",
                "E0045: `@deprecated` takes one message string",
                "E0045: `@deprecated` takes one message string",
                "E0045: `@deprecated` takes one message string",
            ]
        );
    }

    // No attribute on a variant, a record field or an interface function
    // (D-461): the grammar has no place for one (App. A.2), so the parser
    // reports it and keeps it out of the member's node (D-514). Resolution
    // never sees one.

    #[test]
    fn attributes_other_than_external_appear_once() {
        let r = resolve(
            "@target(native)\n@target(native)\nfn a() {\n  Nil\n}\n\n@external(js, \"./m.mjs\", \"b\")\n@async\n@async\npub fn b()\n\n@deprecated(\"x\")\n@deprecated(\"y\")\npub fn c() {\n  Nil\n}\n",
        );
        assert_eq!(
            r.messages(),
            [
                "E0045: duplicate `@async` attribute",
                "E0045: duplicate `@deprecated` attribute",
                "E0045: duplicate `@target` attribute",
            ]
        );
    }

    #[test]
    fn inner_target_once_with_one_target() {
        let r = resolve("//! Docs.\n\n@!target(js)\n@!target(native)\n\nfn a() {\n  Nil\n}\n");
        assert_eq!(r.messages(), ["E0045: duplicate `@!target` attribute"]);
        assert_eq!(r.targets("a"), JS);
        let r = resolve("@!blocking\n\nfn a() {\n  Nil\n}\n");
        assert_eq!(
            r.messages(),
            ["E0045: unknown inner attribute `@!blocking`"]
        );
        let r = resolve("@!target(native, js)\n\nfn a() {\n  Nil\n}\n");
        assert_eq!(r.messages(), ["E0045: malformed `@!target` attribute"]);
        assert_eq!(r.targets("a"), Targets::ALL);
    }

    #[test]
    fn unknown_and_std_only_attributes() {
        let source =
            "@inline\nfn a() {\n  Nil\n}\n\n@std_clamped(n)\npub fn b(n: Int) {\n  Nil\n}\n";
        let r = resolve(source);
        assert_eq!(
            r.messages(),
            [
                "E0045: unknown attribute `@inline`",
                "E0045: unknown attribute `@std_clamped`",
            ]
        );
        let r = resolve_in(PackageId::STD, Targets::ALL, source);
        assert_eq!(r.messages(), ["E0045: unknown attribute `@inline`"]);
    }

    #[test]
    fn bodiless_functions_exist_on_their_externals_targets() {
        let r = resolve(
            "@external(native, \"a\")\nfn a()\n\n@external(js, \"./m.mjs\", \"b\")\nfn b()\n\n@target(native)\n@external(native, \"c\")\n@external(js, \"./m.mjs\", \"c\")\nfn c()\n\n@external(native, \"d\")\nfn d() {\n  Nil\n}\n\n@external(native, \"drop\")\npub type Db\n",
        );
        assert!(r.diags.is_empty(), "{:?}", r.messages());
        assert_eq!(r.targets("a"), NATIVE);
        assert_eq!(r.targets("b"), JS);
        assert_eq!(r.targets("c"), NATIVE);
        // A body runs on the targets without an `@external` (D-207); the
        // function itself exists everywhere.
        assert_eq!(r.targets("d"), Targets::ALL);
        // A drop function does not restrict an external type.
        assert_eq!(r.targets("Db"), Targets::ALL);
    }

    #[test]
    fn items_on_no_target_are_errors() {
        let r = resolve(
            "//! Only on JS.\n\n@!target(js)\n\n@target(native)\nfn a() {\n  Nil\n}\n\n@target(js)\nfn b() {\n  Nil\n}\n\n@external(native, \"c\")\nfn c()\n\n@target(native)\ntest \"t\" {\n  Nil\n}\n\n@target(native)\ntype T {\n  T,\n}\n",
        );
        assert_eq!(
            r.messages(),
            [
                "E0045: `T` exists on no target",
                "E0045: `a` exists on no target",
                "E0045: `c` exists on no target",
                "E0045: the test \"t\" exists on no target",
            ]
        );
        assert_eq!(r.targets("b"), JS);
        let r = resolve("@target(js)\n@external(native, \"n\")\nfn n()\n");
        assert_eq!(r.messages(), ["E0045: `n` exists on no target"]);
        let labels: Vec<&str> = r.diags[0]
            .labels
            .iter()
            .map(|l| l.message.as_str())
            .collect();
        assert_eq!(labels, ["only on `js` here"]);
    }

    #[test]
    fn module_targets_include_the_package_targets() {
        // D-190: the cross-target check covers only the package's targets,
        // so the module's code is built for those, narrowed by `@!target`.
        let source = "fn a() {\n  Nil\n}\n";
        assert_eq!(
            resolve_in(PackageId::LOCAL, NATIVE, source).module.targets,
            NATIVE
        );
        assert_eq!(resolve(source).module.targets, Targets::ALL);
        let r = resolve_in(
            PackageId::LOCAL,
            NATIVE,
            "@!target(js)\n\nfn a() {\n  Nil\n}\n",
        );
        assert!(r.diags.is_empty(), "{:?}", r.messages());
        assert!(r.module.targets.is_empty());
        // Item targets stay what the attributes say.
        assert_eq!(r.targets("a"), JS);
    }
}
