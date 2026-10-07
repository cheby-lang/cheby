//! Written types to closed [`Ty`]s, for signatures and annotations.
//!
//! Paths were resolved by `cheby_resolve`; this module checks what they
//! name: a type with the right number of type arguments (E0051), and not an
//! interface where a type is expected or the reverse (E0052). Aliases are
//! expanded (§4.4). A type with an error contains [`Ty::ERROR`]; its
//! diagnostic is already reported.

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{DefId, DefKind, Ice, IfaceItem, ItemSig, Ty, TyKind, builtin};
use cheby_resolve::Res;
use cheby_syntax::NodeId;
use cheby_syntax::ast::{AstNode, AstToken, InterfaceRef, PathNode, RetType, Type};
use rustc_hash::FxHashMap;

use crate::TypesInput;
use crate::infer::substitute;

/// Lowers the written types of one signature or body.
pub(crate) struct TyLowering<'a> {
    pub input: &'a TypesInput<'a>,
    /// The resolutions of the paths: `sig_paths` or a body's `paths`.
    pub paths: &'a FxHashMap<NodeId, Res>,
}

/// What a resolved type path names.
enum Named {
    /// A type with this many type parameters.
    Type {
        arity: usize,
    },
    /// An alias from a dependency's interface.
    Alias {
        arity: usize,
        target: Ty,
    },
    /// An alias of this module, which the slice does not lower yet.
    OwnAlias,
    Interface,
    /// An unsupported or erroneous item: silent.
    Error,
    Other(&'static str),
}

impl TyLowering<'_> {
    /// The type `ty`. Errors are reported to `diags`, and the result then
    /// contains [`Ty::ERROR`].
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a path was not resolved.
    pub fn lower(&self, ty: Type<'_>, diags: &mut Vec<Diagnostic>) -> Result<Ty, Ice> {
        let types = &self.input.globals.types;
        match ty {
            Type::Path(path_ty) => {
                let args = match path_ty.type_args() {
                    Some(list) => Some(
                        list.types()
                            .map(|arg| self.lower(arg, diags))
                            .collect::<Result<Vec<Ty>, Ice>>()?,
                    ),
                    None => None,
                };
                let Some(path) = path_ty.path() else {
                    return Ok(Ty::ERROR);
                };
                self.path_type(path, args, path_ty.significant_span(), diags)
            }
            Type::Tuple(tuple) => {
                let elems = tuple
                    .elements()
                    .map(|elem| self.lower(elem, diags))
                    .collect::<Result<Box<[Ty]>, Ice>>()?;
                Ok(types.intern(TyKind::Tuple(elems)))
            }
            Type::Fn(f) => {
                let params = match f.param_types() {
                    Some(list) => list
                        .types()
                        .map(|param| self.lower(param, diags))
                        .collect::<Result<Vec<Ty>, Ice>>()?,
                    None => Vec::new(),
                };
                let ret = self.ret(f.ret_type(), diags)?;
                Ok(types.func(params, ret))
            }
            Type::Dyn(d) => match d.interface() {
                Some(iface) => self.dyn_type(iface, diags),
                None => Ok(Ty::ERROR),
            },
        }
    }

    /// A return type: the written type, or `Nil` when it is omitted
    /// (D-083).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a path was not resolved.
    pub fn ret(&self, ret: Option<RetType<'_>>, diags: &mut Vec<Diagnostic>) -> Result<Ty, Ice> {
        match ret {
            None => Ok(Ty::NIL),
            Some(ret) => match ret.ty() {
                Some(ty) => self.lower(ty, diags),
                // `->` without a type is a syntax error.
                None => Ok(Ty::ERROR),
            },
        }
    }

    fn res(&self, path: PathNode<'_>) -> Result<Res, Ice> {
        self.paths
            .get(&path.syntax().id())
            .copied()
            .ok_or_else(|| Ice::new("a type path was not resolved"))
    }

    fn path_type(
        &self,
        path: PathNode<'_>,
        args: Option<Vec<Ty>>,
        span: Span,
        diags: &mut Vec<Diagnostic>,
    ) -> Result<Ty, Ice> {
        let text = path_text(path);
        let given = args.as_ref().map_or(0, Vec::len);
        match self.res(path)? {
            Res::Err => Ok(Ty::ERROR),
            Res::TyParam(param) => {
                if given > 0 {
                    diags.push(arity_error(span, &text, 0, given));
                    return Ok(Ty::ERROR);
                }
                Ok(self.input.globals.types.param(param))
            }
            // `Self` is type parameter 0 of an interface function (C8).
            Res::SelfTy => Ok(self
                .input
                .globals
                .types
                .param(cheby_iface::ParamIdx::from_u32(0))),
            Res::Def(def) => match self.named(def)? {
                Named::Error => Ok(Ty::ERROR),
                Named::Type { arity } => {
                    if given != arity {
                        diags.push(arity_error(span, &text, arity, given));
                        return Ok(Ty::ERROR);
                    }
                    if def == builtin::NEVER {
                        return Ok(Ty::NEVER);
                    }
                    Ok(self
                        .input
                        .globals
                        .types
                        .named(def, args.unwrap_or_default()))
                }
                Named::Alias { arity, target } => {
                    if given != arity {
                        diags.push(arity_error(span, &text, arity, given));
                        return Ok(Ty::ERROR);
                    }
                    let args = args.unwrap_or_default();
                    Ok(substitute(&self.input.globals.types, target, &args))
                }
                Named::OwnAlias => {
                    diags.push(crate::gate::not_supported(span, "type aliases"));
                    Ok(Ty::ERROR)
                }
                Named::Interface => {
                    diags.push(
                        Diagnostic::new(
                            codes::NOT_A_TYPE,
                            format!("`{text}` is an interface, not a type"),
                        )
                        .with_primary(span, "")
                        .with_help(format!(
                            "write `dyn {text}` for a value of any type that satisfies it, \
                             or use a type parameter bounded by it"
                        )),
                    );
                    Ok(Ty::ERROR)
                }
                Named::Other(what) => Err(Ice::new(format!("a type path resolved to a {what}"))),
            },
            Res::Local(_) | Res::Module(_) => {
                Err(Ice::new("a type path resolved to a value or a module"))
            }
        }
    }

    fn dyn_type(&self, iface: InterfaceRef<'_>, diags: &mut Vec<Diagnostic>) -> Result<Ty, Ice> {
        Ok(match self.interface(iface, diags)? {
            Some(def) => self.input.globals.types.intern(TyKind::Dyn {
                iface: def,
                args: Box::new([]),
            }),
            None => Ty::ERROR,
        })
    }

    /// The interface named by `dyn I` or a bound `T: I`. `None` after an
    /// error, already reported.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the path was not resolved.
    pub fn interface(
        &self,
        iface: InterfaceRef<'_>,
        diags: &mut Vec<Diagnostic>,
    ) -> Result<Option<DefId>, Ice> {
        let Some(path) = iface.path() else {
            return Ok(None);
        };
        let span = iface.significant_span();
        let text = path_text(path);
        let given = match iface.type_args() {
            Some(list) => {
                for arg in list.types() {
                    self.lower(arg, diags)?;
                }
                list.types().count()
            }
            None => 0,
        };
        let def = match self.res(path)? {
            Res::Err => return Ok(None),
            Res::Def(def) => def,
            Res::TyParam(_) | Res::SelfTy => {
                diags.push(not_an_interface(span, &text));
                return Ok(None);
            }
            Res::Local(_) | Res::Module(_) => {
                return Err(Ice::new(
                    "an interface path resolved to a value or a module",
                ));
            }
        };
        match self.named(def)? {
            Named::Error => Ok(None),
            Named::Interface => {
                if given > 0 {
                    // Interfaces have no type parameters (D-045).
                    diags.push(arity_error(span, &text, 0, given));
                    return Ok(None);
                }
                Ok(Some(def))
            }
            Named::Type { .. } | Named::Alias { .. } | Named::OwnAlias => {
                diags.push(not_an_interface(span, &text));
                Ok(None)
            }
            Named::Other(what) => Err(Ice::new(format!("an interface path resolved to a {what}"))),
        }
    }

    /// What `def` is, as far as written types care.
    fn named(&self, def: DefId) -> Result<Named, Ice> {
        if def.module == self.input.module {
            let item = self
                .input
                .resolved
                .item(def.index)
                .ok_or_else(|| Ice::new("a type path resolved to a missing item"))?;
            let arity = self
                .input
                .resolved
                .generics
                .get(&def.index)
                .map_or(0, Vec::len);
            return Ok(match item.kind {
                DefKind::Type => Named::Type { arity },
                DefKind::Alias => Named::OwnAlias,
                DefKind::Interface => Named::Interface,
                other => Named::Other(other.describe()),
            });
        }
        let item: &IfaceItem = self
            .input
            .deps
            .item(def)
            .ok_or_else(|| Ice::new(format!("no interface item for {def:?}")))?;
        Ok(match (&item.sig, item.kind) {
            (ItemSig::Unsupported | ItemSig::Error, _) => Named::Error,
            (ItemSig::Type(ty), _) => Named::Type {
                arity: ty.generics.len(),
            },
            (ItemSig::Alias(alias), _) => Named::Alias {
                arity: alias.generics.len(),
                target: alias.target,
            },
            (ItemSig::Interface(_), _) => Named::Interface,
            (_, kind) => Named::Other(kind.describe()),
        })
    }
}

/// The path as written, without trivia: `map::Map`.
pub(crate) fn path_text(path: PathNode<'_>) -> String {
    let segments: Vec<&str> = path.segments().map(AstToken::text).collect();
    segments.join("::")
}

fn arity_error(span: Span, text: &str, expected: usize, given: usize) -> Diagnostic {
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let message = if expected == 0 {
        format!("`{text}` takes no type arguments")
    } else {
        format!(
            "`{text}` takes {expected} type argument{} but {given} {} given",
            plural(expected),
            if given == 1 { "was" } else { "were" }
        )
    };
    Diagnostic::new(codes::WRONG_TYPE_ARGUMENT_COUNT, message).with_primary(span, "")
}

fn not_an_interface(span: Span, text: &str) -> Diagnostic {
    Diagnostic::new(codes::NOT_A_TYPE, format!("`{text}` is not an interface"))
        .with_primary(span, "")
        .with_note("only an interface may follow `dyn` (§8.6)")
}
