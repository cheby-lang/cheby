//! Item collection (B4): every item in `DefIndex` order, duplicate names
//! (E0041), attributes (E0045, §4.8) and functions without a body (E0046).

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{DefIndex, DefKind, Ice, IndexVec, Name, Namespace, PackageId, Vis};
use cheby_package::{Target, Targets};
use cheby_syntax::NodeId;
use cheby_syntax::ast::{
    AstChildren, AstNode, AstToken, Attr, AttrArg, InnerAttr, Item, SourceFile, TypeBody,
    Visibility, VisibilityKind,
};
use rustc_hash::FxHashMap;

use crate::{ExternalAttr, ItemAttrs, ItemDef, ItemStatus, ResolveInput};

/// The result of [`collect`].
pub(crate) struct Collected {
    pub items: IndexVec<DefIndex, ItemDef>,
    /// From `@!target`.
    pub module_targets: Targets,
    /// Valid attributes the slice does not support yet: the item, the
    /// attribute's span, and what to call it in E0038.
    pub attr_gates: Vec<(DefIndex, Span, &'static str)>,
    /// The same for inner attributes.
    pub inner_gates: Vec<(Span, &'static str)>,
}

/// Where an attribute is written.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Placement {
    Fn {
        has_body: bool,
    },
    Type {
        external: bool,
    },
    Alias,
    Const,
    Interface,
    Test,
    /// A variant, a record field or an interface function.
    Member,
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
            Self::Member => "a part of a declaration",
        }
    }
}

/// Native symbol prefixes only the toolchain may bind (E0055): the
/// runtime's internal functions and data, and generated code (D-455, D-456).
const RESERVED_SYMBOL_PREFIXES: [&str; 2] = ["cheby_rt_", "cb$"];

const ATTRIBUTE_LIST: &str =
    "the attributes are `@external`, `@target`, `@blocking`, `@async` and `@deprecated`";

pub(crate) fn collect(
    input: &ResolveInput<'_>,
    diags: &mut Vec<Diagnostic>,
) -> Result<Collected, Ice> {
    let mut cx = Cx {
        input,
        diags,
        items: IndexVec::new(),
        attr_gates: Vec::new(),
        types: FxHashMap::default(),
        values: FxHashMap::default(),
        tests: FxHashMap::default(),
    };
    let Some(file) = SourceFile::cast(input.tree.root()) else {
        return Err(Ice::new("the tree's root is not a source file"));
    };
    let (module_targets, inner_gates) = cx.inner_attrs(file.inner_attrs());
    for item in file.items() {
        cx.item(item, module_targets)?;
    }
    Ok(Collected {
        items: cx.items,
        module_targets,
        attr_gates: cx.attr_gates,
        inner_gates,
    })
}

struct Cx<'a, 'd> {
    input: &'a ResolveInput<'a>,
    diags: &'d mut Vec<Diagnostic>,
    items: IndexVec<DefIndex, ItemDef>,
    attr_gates: Vec<(DefIndex, Span, &'static str)>,
    types: FxHashMap<Name, Span>,
    values: FxHashMap<Name, Span>,
    tests: FxHashMap<Name, Span>,
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
                let index = self.next_index()?;
                let attrs = self.attrs(index, f.attrs(), Placement::Fn { has_body }, vis);
                let attrs = with_module_targets(attrs, module_targets);
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
                let index = self.next_index()?;
                let placement = Placement::Type {
                    external: body.is_none(),
                };
                let attrs = self.attrs(index, t.attrs(), placement, vis);
                let attrs = with_module_targets(attrs, module_targets);
                let ty = self.push(
                    name.text(),
                    name.span(),
                    DefKind::Type,
                    vis,
                    None,
                    t.syntax().id(),
                    attrs.clone(),
                )?;
                match body {
                    Some(TypeBody::Variants(list)) => {
                        for variant in list.variants() {
                            let Some(ctor) = variant.name() else { continue };
                            let index = self.next_index()?;
                            self.member_attrs(index, variant.attrs());
                            self.push(
                                ctor.text(),
                                ctor.span(),
                                DefKind::Ctor,
                                vis,
                                Some(ty),
                                variant.syntax().id(),
                                attrs.clone(),
                            )?;
                            if let Some(cheby_syntax::ast::VariantFields::Record(fields)) =
                                variant.fields()
                            {
                                for field in fields.fields() {
                                    self.member_attrs(index, field.attrs());
                                }
                            }
                        }
                    }
                    // The single-variant shorthand (§4.3.3): one constructor
                    // named like the type; its node is the field list.
                    Some(TypeBody::Record(fields)) => {
                        let index = self.next_index()?;
                        for field in fields.fields() {
                            self.member_attrs(index, field.attrs());
                        }
                        self.push(
                            name.text(),
                            name.span(),
                            DefKind::Ctor,
                            vis,
                            Some(ty),
                            fields.syntax().id(),
                            attrs,
                        )?;
                    }
                    None => {}
                }
            }
            Item::Alias(a) => {
                let Some(name) = a.name() else { return Ok(()) };
                let index = self.next_index()?;
                let attrs = self.attrs(index, a.attrs(), Placement::Alias, vis);
                let attrs = with_module_targets(attrs, module_targets);
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
                let index = self.next_index()?;
                let attrs = self.attrs(index, c.attrs(), Placement::Const, vis);
                let attrs = with_module_targets(attrs, module_targets);
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
                let index = self.next_index()?;
                let attrs = self.attrs(index, i.attrs(), Placement::Interface, vis);
                let attrs = with_module_targets(attrs, module_targets);
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
                    let index = self.next_index()?;
                    self.member_attrs(index, f.attrs());
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
                let index = self.next_index()?;
                let attrs = self.attrs(index, t.attrs(), Placement::Test, vis);
                let attrs = with_module_targets(attrs, module_targets);
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

    fn next_index(&self) -> Result<DefIndex, Ice> {
        self.items
            .next_index()
            .ok_or_else(|| Ice::new("too many items in one module"))
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
            status: ItemStatus::Ok,
        })
    }

    /// Attributes on a variant, a record field or an interface function,
    /// where none is allowed.
    fn member_attrs(&mut self, index: DefIndex, attrs: AstChildren<'_, Attr<'_>>) {
        let _ = self.attrs(index, attrs, Placement::Member, Vis::Priv);
    }

    /// Checks the attributes of an item and returns what they say.
    #[expect(clippy::too_many_lines, reason = "one branch per attribute")]
    fn attrs(
        &mut self,
        index: DefIndex,
        attrs: AstChildren<'_, Attr<'_>>,
        placement: Placement,
        vis: Vis,
    ) -> ItemAttrs {
        let mut out = ItemAttrs::default();
        let mut seen: Vec<(&'static str, Span)> = Vec::new();
        let mut external_targets: Vec<(Target, Span)> = Vec::new();
        let mut target: Option<Target> = None;
        let mut blocking: Option<Span> = None;
        let mut is_async: Option<Span> = None;
        for attr in attrs {
            let span = attr.significant_span();
            let Some(name) = attr.name() else { continue };
            let Some(known) = known_attr(name.text()) else {
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
                    if matches!(placement, Placement::Member) {
                        self.misplaced(span, known, placement, None);
                        continue;
                    }
                    target = Some(t);
                    self.attr_gates.push((index, span, "`@target` attributes"));
                }
                "blocking" | "async" => {
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
                    if known == "blocking" {
                        blocking = Some(span);
                    } else {
                        is_async = Some(span);
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
                    if vis != Vis::Pub || matches!(placement, Placement::Member | Placement::Test) {
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
                    self.attr_gates
                        .push((index, span, "`@deprecated` attributes"));
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
                self.attr_gates
                    .push((index, span, "`@blocking` attributes"));
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
                self.attr_gates.push((index, span, "`@async` attributes"));
            } else {
                self.misplaced(
                    span,
                    "async",
                    placement,
                    Some("`@async` applies to functions with an `@external(js, …)` attribute"),
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
    fn inner_attrs(
        &mut self,
        attrs: AstChildren<'_, InnerAttr<'_>>,
    ) -> (Targets, Vec<(Span, &'static str)>) {
        let mut targets = Targets::ALL;
        let mut gates = Vec::new();
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
                gates.push((span, "`@!target` attributes"));
            }
        }
        (targets, gates)
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

const fn external_target(external: &ExternalAttr) -> Target {
    match external {
        ExternalAttr::Native { .. } => Target::Native,
        ExternalAttr::Js { .. } => Target::Js,
    }
}

fn known_attr(name: &str) -> Option<&'static str> {
    ["external", "target", "blocking", "async", "deprecated"]
        .into_iter()
        .find(|&known| known == name)
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
