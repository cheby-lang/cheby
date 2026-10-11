//! Paths: unqualified names in the module scope, `m::x` through an
//! imported module's interface, interface functions `I::f` and
//! `m::I::f` (D-173), visibility (§7.3, D-162) and the opacity of
//! constructors (§4.3.4, D-048, D-151).

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{
    DefId, DefIndex, DefKind, Ice, IndexVec, ItemSig, ModuleId, ModuleInterface, Name, Namespace,
    ParamIdx, Vis, builtin,
};
use cheby_syntax::ast::{AstNode, AstToken, InterfaceDecl, PathNode};

use crate::{ItemDef, ModuleScope, Res, ResolveInput, closest_name};

/// The interfaces of `std::ops` that operators dispatch to (§8.7).
const OPERATOR_INTERFACES: [&str; 6] = ["Add", "Sub", "Mul", "Div", "Neg", "Compare"];

/// The segments of a path, classified.
pub(crate) enum PathShape {
    /// `x` or `T`.
    Name(Name, Span),
    /// `m::x` or `m::T`.
    Qualified {
        module: (Name, Span),
        name: (Name, Span),
    },
    /// `I::f` or `m::I::f` (D-173).
    InterfaceFn {
        module: Option<(Name, Span)>,
        iface: (Name, Span),
        name: (Name, Span),
    },
    /// Anything else, such as `a::b::c`.
    Invalid,
}

pub(crate) struct PathCx<'a> {
    pub input: &'a ResolveInput<'a>,
    pub items: &'a IndexVec<DefIndex, ItemDef>,
    pub scope: &'a ModuleScope,
}

impl PathCx<'_> {
    fn intern(&self, text: &str) -> Name {
        self.input.globals.names.intern(text)
    }

    fn text(&self, name: Name) -> &'static str {
        self.input.globals.names.get(name)
    }

    pub(crate) fn shape(&self, path: PathNode<'_>) -> PathShape {
        let segments: Vec<(Name, Span, bool)> = path
            .segments()
            .map(|s| (self.intern(s.text()), s.span(), s.is_type_name()))
            .collect();
        match segments.as_slice() {
            [(name, span, _)] => PathShape::Name(*name, *span),
            [(m, ms, false), (n, ns, _)] => PathShape::Qualified {
                module: (*m, *ms),
                name: (*n, *ns),
            },
            [(i, is, true), (n, ns, false)] => PathShape::InterfaceFn {
                module: None,
                iface: (*i, *is),
                name: (*n, *ns),
            },
            [(m, ms, false), (i, is, true), (n, ns, false)] => PathShape::InterfaceFn {
                module: Some((*m, *ms)),
                iface: (*i, *is),
                name: (*n, *ns),
            },
            _ => PathShape::Invalid,
        }
    }

    /// Resolves a type path: a type parameter, a type of this module, an
    /// imported type or a prelude type (§7.7), or `m::T`.
    pub(crate) fn type_path(
        &self,
        path: PathNode<'_>,
        generics: &[(Name, ParamIdx)],
        in_interface: bool,
        diags: &mut Vec<Diagnostic>,
    ) -> Result<Res, Ice> {
        match self.shape(path) {
            PathShape::Name(name, span) => {
                if self.text(name) == "Self" {
                    if in_interface {
                        return Ok(Res::SelfTy);
                    }
                    diags.push(
                        Diagnostic::new(
                            codes::UNRESOLVED_NAME,
                            "`Self` can be used only in interface declarations",
                        )
                        .with_primary(span, ""),
                    );
                    return Ok(Res::Err);
                }
                if let Some(&(_, param)) = generics.iter().find(|(n, _)| *n == name) {
                    return Ok(Res::TyParam(param));
                }
                if let Some(entry) = self.scope.get(Namespace::Type, name) {
                    return Ok(entry.res);
                }
                diags.push(self.unknown(name, span, Namespace::Type, "type"));
                Ok(Res::Err)
            }
            PathShape::Qualified { module, name } => {
                let Some(target) = self.module_qualifier(module, diags) else {
                    return Ok(Res::Err);
                };
                self.in_module(target, Namespace::Type, name, module.1, diags)
            }
            PathShape::InterfaceFn { .. } | PathShape::Invalid => {
                diags.push(
                    Diagnostic::new(codes::UNRESOLVED_NAME, "this is not a type")
                        .with_primary(path.significant_span(), "")
                        .with_help("a type is written `Name` or `module::Name`"),
                );
                Ok(Res::Err)
            }
        }
    }

    /// An unknown unqualified name, with a suggestion if a scope name is
    /// close. A reserved word is E0007 instead, with the word's help: it
    /// cannot be bound anywhere, so "not found" would mislead (§2.4).
    pub(crate) fn unknown(&self, name: Name, span: Span, ns: Namespace, what: &str) -> Diagnostic {
        let text = self.text(name);
        if let Some(diag) = cheby_syntax::names::reserved_word_error(text, span) {
            return diag;
        }
        let mut diag = Diagnostic::new(
            codes::UNRESOLVED_NAME,
            format!("cannot find {what} `{text}` in this scope"),
        )
        .with_primary(span, "not found");
        let mut candidates: Vec<&str> = self
            .items
            .iter()
            .filter(|item| item.kind.namespace() == Some(ns))
            .map(|item| self.text(item.name))
            .collect();
        candidates.sort_unstable();
        if let Some(close) = closest_name(text, candidates) {
            diag = diag.with_suggestion(
                format!("a {} with a similar name exists: `{close}`", ns.describe()),
                span,
                close,
            );
        }
        if ns == Namespace::Type
            && let Some(help) = self.type_hint(name, text)
        {
            diag = diag.with_help(help);
        }
        diag
    }

    /// Where an unknown type name may come from: a builtin type outside
    /// the prelude (D-447, D-481), an operator interface of
    /// `std::ops` (§8.7), or a constructor written where its type belongs,
    /// as in a record update (§5.11.2).
    fn type_hint(&self, name: Name, text: &str) -> Option<String> {
        if let Ok(builtin_iface) = self.iface(ModuleId::BUILTIN)
            && let Some(index) = builtin_iface.lookup(Namespace::Type, name)
            && let Some(&(_, home)) = builtin::HOMES
                .iter()
                .find(|(def, _)| *def == DefId::new(ModuleId::BUILTIN, index))
            && !builtin::is_prelude(DefId::new(ModuleId::BUILTIN, index))
        {
            return Some(format!(
                "`{text}` is not in the prelude: add `import std::{home}` and write `{home}::{text}`"
            ));
        }
        if OPERATOR_INTERFACES.contains(&text) && self.input.std.known.contains(&"ops") {
            return Some(format!(
                "the operator interfaces are in `std::ops`: add `import std::ops` and write `ops::{text}`"
            ));
        }
        let Res::Def(ctor) = self.scope.get(Namespace::Value, name)?.res else {
            return None;
        };
        let parent = if ctor.module == self.input.module {
            let item = self.items.get(ctor.index)?;
            (item.kind == DefKind::Ctor).then_some(())?;
            self.text(self.items.get(item.parent?)?.name)
        } else {
            let iface = self.iface(ctor.module).ok()?;
            let item = iface.item(ctor.index)?;
            (item.kind == DefKind::Ctor).then_some(())?;
            self.text(iface.item(item.parent?)?.name)
        };
        Some(format!(
            "`{text}` is a constructor of the type `{parent}`; name the type here"
        ))
    }

    /// The module named by the qualifier `m` of `m::x`. `None` if it is
    /// not a module (reported) or a failed import (already reported).
    pub(crate) fn module_qualifier(
        &self,
        (name, span): (Name, Span),
        diags: &mut Vec<Diagnostic>,
    ) -> Option<ModuleId> {
        let text = self.text(name);
        if let Some(entry) = self.scope.get(Namespace::Value, name) {
            match entry.res {
                Res::Module(module) => Some(module),
                Res::Err => None,
                // An import of this name collided (E0041): no follow-up.
                _ if self.scope.is_conflicted(name) => None,
                _ => {
                    diags.push(
                        Diagnostic::new(
                            codes::UNRESOLVED_NAME,
                            format!("`{text}` is not a module"),
                        )
                        .with_primary(span, "")
                        .with_note(
                            "a qualified path starts with an imported module: `module::name`",
                        ),
                    );
                    None
                }
            }
        } else {
            {
                let mut diag =
                    Diagnostic::new(codes::UNRESOLVED_NAME, format!("`{text}` is not imported"))
                        .with_primary(span, "");
                let mut known: Vec<&str> = self
                    .input
                    .std
                    .known
                    .iter()
                    .copied()
                    .filter(|path| path.rsplit("::").next() == Some(text))
                    .collect();
                known.sort_unstable();
                if let Some(path) = known.first() {
                    diag =
                        diag.with_help(format!("add `import std::{path}` at the top of the file"));
                } else {
                    diag = diag.with_note("every module must be imported before use (§7.5)");
                }
                diags.push(diag);
                None
            }
        }
    }

    /// The interface of a dependency.
    fn iface(&self, module: ModuleId) -> Result<&ModuleInterface, Ice> {
        self.input
            .deps
            .get(module)
            .ok_or_else(|| Ice::new(format!("the interface of {module:?} is not readable")))
    }

    /// `x` in module `target`, checked for visibility (E0043, §7.3). The
    /// opacity of a constructor is checked where it is used
    /// ([`Self::check_structure`]), since an item import of a type brings
    /// its constructor along (D-111).
    pub(crate) fn in_module(
        &self,
        target: ModuleId,
        ns: Namespace,
        (name, span): (Name, Span),
        module_span: Span,
        diags: &mut Vec<Diagnostic>,
    ) -> Result<Res, Ice> {
        let iface = self.iface(target)?;
        let path = &self.input.modules.try_get(target)?.path;
        let text = self.text(name);
        let Some(index) = iface.lookup(ns, name) else {
            if let Some(def) = self.home_lookup(target, ns, name)? {
                return Ok(Res::Def(def));
            }
            let mut diag = Diagnostic::new(
                codes::UNRESOLVED_NAME,
                format!("there is no {} `{text}` in module `{path}`", ns.describe()),
            )
            .with_primary(span, "not found")
            .with_secondary(module_span, format!("`{path}`"));
            let mut candidates: Vec<&str> = iface
                .items
                .iter_enumerated()
                .filter(|(i, item)| {
                    item.kind.namespace() == Some(ns) && iface.visible_from(*i, self.input.package)
                })
                .map(|(_, item)| self.text(item.name))
                .collect();
            candidates.sort_unstable();
            if let Some(close) = closest_name(text, candidates) {
                diag = diag.with_suggestion(format!("did you mean `{close}`?"), span, close);
            }
            diags.push(diag);
            return Ok(Res::Err);
        };
        let item = iface
            .item(index)
            .ok_or_else(|| Ice::new("interface lookup gave a missing item"))?;
        if !iface.visible_from(index, self.input.package) {
            let reason = match item.vis {
                Vis::Priv => format!("`{text}` is private to module `{path}`"),
                _ => format!("`{text}` is visible only inside its package"),
            };
            let mut diag = Diagnostic::new(codes::PRIVATE_ITEM, reason).with_primary(span, "");
            if let Some(decl) = item.span {
                diag = diag.with_secondary(decl, "declared here");
            }
            diags.push(diag);
            return Ok(Res::Err);
        }
        Ok(Res::Def(DefId::new(target, index)))
    }

    /// A prelude type re-exported by its home module, or one of its
    /// constructors (D-447): `option::Option`, `option::Some`.
    fn home_lookup(
        &self,
        target: ModuleId,
        ns: Namespace,
        name: Name,
    ) -> Result<Option<DefId>, Ice> {
        let path = &self.input.modules.try_get(target)?.path;
        let homed = builtin::homed_at(path);
        if homed.is_empty() {
            return Ok(None);
        }
        let builtin_iface = self.iface(ModuleId::BUILTIN)?;
        let Some(index) = builtin_iface.lookup(ns, name) else {
            return Ok(None);
        };
        let def = DefId::new(ModuleId::BUILTIN, index);
        let owner = match builtin_iface.item(index) {
            Some(item) if item.kind == DefKind::Ctor => {
                item.parent.map(|p| DefId::new(ModuleId::BUILTIN, p))
            }
            Some(_) => Some(def),
            None => None,
        };
        Ok(owner.filter(|o| homed.contains(o)).map(|_| def))
    }

    /// Whether the constructors, patterns, fields and record updates of
    /// type `def`, or of the type of constructor `def`, are usable here
    /// (§4.3.4, D-048, D-151). If not, reports E0043 at `span` and returns
    /// `false`. Any other item passes.
    pub(crate) fn check_structure(
        &self,
        def: DefId,
        span: Span,
        diags: &mut Vec<Diagnostic>,
    ) -> Result<bool, Ice> {
        if def.module == self.input.module || def.module == ModuleId::BUILTIN {
            return Ok(true);
        }
        let iface = self.iface(def.module)?;
        let Some(item) = iface.item(def.index) else {
            return Err(Ice::new("a resolved item is missing from its interface"));
        };
        let ty_index = match item.kind {
            DefKind::Ctor => item
                .parent
                .ok_or_else(|| Ice::new("a constructor without a type"))?,
            DefKind::Type => def.index,
            _ => return Ok(true),
        };
        let ty = iface
            .item(ty_index)
            .ok_or_else(|| Ice::new("a constructor's type is missing"))?;
        if cheby_iface::structure_visible(
            ty,
            def.module,
            iface.package,
            self.input.module,
            self.input.package,
        ) {
            return Ok(true);
        }
        let ty_text = self.text(ty.name);
        let message = match item.kind {
            DefKind::Ctor => format!(
                "the constructor `{}` is not visible outside its package",
                self.text(item.name)
            ),
            _ => format!("the fields of `{ty_text}` are not visible outside its package"),
        };
        let mut diag = Diagnostic::new(codes::PRIVATE_ITEM, message).with_primary(span, "");
        diag = match ty.vis {
            Vis::Priv => diag.with_note(format!(
                "`{ty_text}` is `priv`, so its constructors and fields are usable only in its \
                 own module"
            )),
            _ if item.kind == DefKind::Ctor => {
                diag.with_note("a `pub` type hides its constructors unless it is `pub exposed`")
            }
            // The same note as for field access in a body.
            _ => diag.with_note(
                "a `pub` type hides its constructors and fields unless it is `pub exposed`",
            ),
        };
        diags.push(diag);
        Ok(false)
    }

    /// `I::f` or `m::I::f`: the function `f` of interface `I`, or of an
    /// interface that `I` embeds, as the `InterfaceFn` item of the
    /// interface that declares it (D-173, §8.3).
    pub(crate) fn interface_fn(
        &self,
        module: Option<(Name, Span)>,
        (iface_name, iface_span): (Name, Span),
        (name, span): (Name, Span),
        diags: &mut Vec<Diagnostic>,
    ) -> Result<Res, Ice> {
        let iface_res = if let Some(module) = module {
            let Some(target) = self.module_qualifier(module, diags) else {
                return Ok(Res::Err);
            };
            self.in_module(
                target,
                Namespace::Type,
                (iface_name, iface_span),
                module.1,
                diags,
            )?
        } else {
            let Some(entry) = self.scope.get(Namespace::Type, iface_name) else {
                diags.push(self.unknown(iface_name, iface_span, Namespace::Type, "interface"));
                return Ok(Res::Err);
            };
            entry.res
        };
        let Res::Def(iface_def) = iface_res else {
            return Ok(Res::Err);
        };
        let iface_text = self.text(iface_name);
        if self.kind_of(iface_def)? != Some(DefKind::Interface) {
            diags.push(
                Diagnostic::new(
                    codes::UNRESOLVED_NAME,
                    format!("`{iface_text}` is not an interface"),
                )
                .with_primary(iface_span, "")
                .with_note(
                    "`Name::function` calls a function through an interface (§8.5); a module's \
                     functions are called as `module::function`",
                ),
            );
            return Ok(Res::Err);
        }
        match self.find_interface_fn(iface_def, name, &mut Vec::new())? {
            Found::Fn(def) => Ok(Res::Def(def)),
            Found::Silent => Ok(Res::Err),
            Found::None => {
                diags.push(
                    Diagnostic::new(
                        codes::UNRESOLVED_NAME,
                        format!("`{iface_text}` has no function `{}`", self.text(name)),
                    )
                    .with_primary(span, "not found"),
                );
                Ok(Res::Err)
            }
        }
    }

    /// The kind of item `def`, of this module or a dependency.
    fn kind_of(&self, def: DefId) -> Result<Option<DefKind>, Ice> {
        if def.module == self.input.module {
            return Ok(self.items.get(def.index).map(|item| item.kind));
        }
        Ok(self
            .iface(def.module)?
            .item(def.index)
            .map(|item| item.kind))
    }

    /// The function `name` of interface `iface`: its own first, then those
    /// of the interfaces it embeds, depth first in written order (§8.3).
    fn find_interface_fn(
        &self,
        iface: DefId,
        name: Name,
        visited: &mut Vec<DefId>,
    ) -> Result<Found, Ice> {
        if visited.contains(&iface) {
            // A cycle of embedding, reported with the declarations.
            return Ok(Found::None);
        }
        visited.push(iface);
        let embeds: Vec<DefId> = if iface.module == self.input.module {
            let parent = self
                .items
                .get(iface.index)
                .ok_or_else(|| Ice::new("own interface missing"))?;
            if parent.kind != DefKind::Interface {
                return Ok(Found::None);
            }
            if let Some((index, _)) = self
                .items
                .iter_enumerated()
                .find(|(_, item)| item.parent == Some(iface.index) && item.name == name)
            {
                return Ok(Found::Fn(DefId::new(iface.module, index)));
            }
            self.own_embeds(parent)?
        } else {
            let module = self.iface(iface.module)?;
            match module.item(iface.index).map(|item| &item.sig) {
                Some(ItemSig::Interface(def)) => {
                    if let Some(&f) = def
                        .fns
                        .iter()
                        .find(|&&f| module.item(f).is_some_and(|item| item.name == name))
                    {
                        return Ok(Found::Fn(DefId::new(iface.module, f)));
                    }
                    def.embeds.to_vec()
                }
                Some(ItemSig::Error) => return Ok(Found::Silent),
                _ => return Ok(Found::None),
            }
        };
        for embedded in embeds {
            match self.find_interface_fn(embedded, name, visited)? {
                Found::None => {}
                found => return Ok(found),
            }
        }
        Ok(Found::None)
    }

    /// The interfaces an interface of this module embeds, resolved from
    /// its declaration. Errors in them are reported with the declaration.
    fn own_embeds(&self, iface: &ItemDef) -> Result<Vec<DefId>, Ice> {
        let decl = self
            .input
            .tree
            .node(iface.node)
            .and_then(InterfaceDecl::cast)
            .ok_or_else(|| Ice::new("an interface's node is not an interface declaration"))?;
        let mut scratch = Vec::new();
        let mut out = Vec::new();
        for bound in decl.bounds().iter().flat_map(|b| b.interfaces()) {
            let Some(path) = bound.path() else { continue };
            if let Res::Def(def) = self.type_path(path, &[], true, &mut scratch)? {
                out.push(def);
            }
        }
        Ok(out)
    }
}

/// What [`PathCx::find_interface_fn`] found.
enum Found {
    Fn(DefId),
    /// Something already reported is in the way.
    Silent,
    None,
}
