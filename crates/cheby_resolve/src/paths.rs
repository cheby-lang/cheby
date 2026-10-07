//! Paths: unqualified names in the module scope, `m::x` through an
//! imported module's interface, interface functions `I::f` and
//! `m::I::f`, and visibility (§7.3, §7.7, D-173).

use std::cell::Cell;

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{
    DefId, DefIndex, DefKind, Ice, IndexVec, ItemSig, ModuleId, ModuleInterface, Name, Namespace,
    PackageId, ParamIdx, Vis, builtin,
};
use cheby_syntax::ast::{AstNode, AstToken, PathNode};

use crate::{ItemDef, ItemStatus, ModuleScope, Res, ResolveInput, closest_name};

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
    /// Set when a lookup reported E0038, so that the item containing the
    /// path becomes unsupported.
    pub gated: Cell<bool>,
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
        diag
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

    /// `x` in module `target`, checked for visibility (E0043) and for the
    /// opacity of constructors (§4.3.4, D-048).
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
            let info = self.input.modules.try_get(target)?;
            let segments = info.path.segments();
            let under_std = segments.get(1..).unwrap_or_default().join("::");
            if info.package == PackageId::STD
                && self.input.std.partial.contains(&under_std.as_str())
            {
                diags.push(
                    Diagnostic::new(
                        codes::NOT_SUPPORTED_YET,
                        format!("`{path}::{text}` is not supported yet"),
                    )
                    .with_primary(span, "")
                    .with_note(format!("the toolchain ships only part of `{path}` so far")),
                );
                self.gated.set(true);
                return Ok(Res::Err);
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
        if item.kind == DefKind::Ctor && iface.package != self.input.package {
            let exposed = item
                .parent
                .and_then(|p| iface.item(p))
                .is_some_and(|ty| matches!(&ty.sig, ItemSig::Type(def) if def.exposed));
            if !exposed {
                diags.push(
                    Diagnostic::new(
                        codes::PRIVATE_ITEM,
                        format!("the constructor `{text}` is not visible outside its package"),
                    )
                    .with_primary(span, "")
                    .with_note("a `pub` type hides its constructors unless it is `pub exposed`"),
                );
                return Ok(Res::Err);
            }
        }
        if matches!(item.sig, ItemSig::Unsupported) {
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

    /// `I::f` or `m::I::f`: the function `f` of interface `I` (D-173).
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
        let found = if iface_def.module == self.input.module {
            let parent = self
                .items
                .get(iface_def.index)
                .ok_or_else(|| Ice::new("own interface missing"))?;
            if parent.kind == DefKind::Interface {
                self.items
                    .iter_enumerated()
                    .find(|(_, item)| item.parent == Some(iface_def.index) && item.name == name)
                    .map(|(index, item)| (index, item.status == ItemStatus::Unsupported))
            } else {
                None
            }
        } else {
            let iface = self.iface(iface_def.module)?;
            match iface.item(iface_def.index).map(|item| &item.sig) {
                Some(ItemSig::Interface(def)) => def
                    .fns
                    .iter()
                    .find(|&&f| iface.item(f).is_some_and(|item| item.name == name))
                    .map(|&f| (f, false)),
                _ => None,
            }
        };
        let iface_text = self.text(iface_name);
        let text = self.text(name);
        match found {
            Some((_, true)) => Ok(Res::Err),
            Some((index, false)) => Ok(Res::Def(DefId::new(iface_def.module, index))),
            None => {
                diags.push(
                    Diagnostic::new(
                        codes::UNRESOLVED_NAME,
                        format!("`{iface_text}` has no function `{text}`"),
                    )
                    .with_primary(span, "not found"),
                );
                Ok(Res::Err)
            }
        }
    }
}
