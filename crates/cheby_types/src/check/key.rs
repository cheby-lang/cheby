//! Context keys (§10.11): the rules the checker knows about
//! `std::context::key` and `std::context::key_without_default`.
//!
//! - The call that is a constant's whole initializer, parentheses around it
//!   included, becomes a call of the key's constructor, `key_at` or
//!   `key_without_default_at`, with the constant's path text and its type
//!   text as two more arguments (D-1565, D-1572, D-1581). The constructors
//!   are package-visible functions of `std::context` with the generics of
//!   the functions they stand for (D-1587), so the recorded type arguments
//!   fit them unchanged. The swap is made in the finished body, so the
//!   language server's facts keep the function that is written.
//! - Every other mention of the two functions in an expression is E0080
//!   (D-1565, D-1589). An import is not a mention (D-1581).
//! - The type check of §4.5 does not look inside the type argument of a
//!   `context::Key<T>` ([`is_key_type`], D-1578, D-1591).
//! - The two functions never count toward satisfying an interface, so no
//!   interface call, bound or `dyn` conversion reaches them: `ifaces::solve`
//!   asks [`key_fn`] where it picks a type's module function (D-1593).
//!
//! Std items are found by module path and name, as `std::ops`'s are
//! (`ifaces::solve`), and only through interfaces (D-246).

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{
    DefId, DefKind, Ice, ItemSig, ModuleInterface, Namespace, PackageId, Ty, TyKind, builtin,
    path_text,
};
use cheby_resolve::{BodyRes, Res};
use cheby_syntax::NodeId;
use cheby_syntax::ast::{Arg, AstNode, Expr};

use crate::TypesInput;
use crate::thir::{self, Callee, ExprKind, Lit};

/// The module segment of `std::context` under `std`.
const CONTEXT: &str = "context";

/// The start of a REPL entry's module, `<repl:N>` or `<repl:N:G>`.
const REPL_MODULE_PREFIX: &str = "<repl:";

/// One of the two functions that make keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyFn {
    /// `context::key(default)`.
    Key,
    /// `context::key_without_default()`.
    WithoutDefault,
}

impl KeyFn {
    /// The function's name in `std::context`.
    const fn name(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::WithoutDefault => "key_without_default",
        }
    }

    /// The name of the constructor a whole initializer calls instead.
    const fn ctor_name(self) -> &'static str {
        match self {
            Self::Key => "key_at",
            Self::WithoutDefault => "key_without_default_at",
        }
    }

    /// A call of it as a constant's whole initializer, for E0080's help.
    const fn example(self) -> &'static str {
        match self {
            Self::Key => "const name: context::Key<Int> = context::key(0)",
            Self::WithoutDefault => {
                "const name: context::Key<Option<Int>> = context::key_without_default()"
            }
        }
    }
}

/// Whether `def` is an item of the standard library's `std::context`.
/// It runs for every function a body names, so it compares the segments of
/// `builtin::home_path(CONTEXT)` without building that path.
fn in_context(input: &TypesInput<'_>, def: DefId) -> bool {
    input.modules.get(def.module).is_some_and(|info| {
        info.package == PackageId::STD
            && matches!(info.path.segments(), [std, module] if std == "std" && module == CONTEXT)
    })
}

/// Which key function `def` is, if it is one.
///
/// # Errors
///
/// An [`Ice`] if `def` has no interface item.
pub(crate) fn key_fn(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    def: DefId,
) -> Result<Option<KeyFn>, Ice> {
    if !in_context(input, def) {
        return Ok(None);
    }
    let item = input.item(own, def)?;
    if item.kind != DefKind::Fn {
        return Ok(None);
    }
    Ok(match input.globals.names.get(item.name) {
        "key" => Some(KeyFn::Key),
        "key_without_default" => Some(KeyFn::WithoutDefault),
        _ => None,
    })
}

/// Whether `def` is the type `std::context::Key`, whose type argument
/// §4.5's type check does not look at (D-1578, D-1591).
///
/// # Errors
///
/// An [`Ice`] if `def` has no interface item.
pub(crate) fn is_key_type(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    def: DefId,
) -> Result<bool, Ice> {
    if !in_context(input, def) {
        return Ok(false);
    }
    let item = input.item(own, def)?;
    Ok(item.kind == DefKind::Type && input.globals.names.get(item.name) == "Key")
}

/// The constructor that stands for `which` (D-1587): from `own` when this
/// module is `std::context` itself, else through the dependencies.
///
/// # Errors
///
/// An [`Ice`] if `std::context` is not readable or has no such function
/// with the generics of `which`: a mention of `which` needs its module, and
/// std declares both.
fn key_ctor(input: &TypesInput<'_>, own: &ModuleInterface, which: KeyFn) -> Result<DefId, Ice> {
    let missing = || {
        Ice::new(format!(
            "`std::context` has no constructor `{}` for `{}`",
            which.ctor_name(),
            which.name()
        ))
    };
    let module = input
        .modules
        .find(&builtin::home_path(CONTEXT))
        .ok_or_else(missing)?;
    let iface = if module == input.module {
        own
    } else {
        input.deps.get(module).ok_or_else(missing)?
    };
    let generics = |name: &str| -> Option<(DefId, usize)> {
        let name = input.globals.names.lookup(name)?;
        let index = iface.lookup(Namespace::Value, name)?;
        match &iface.item(index)?.sig {
            ItemSig::Fn(sig) => Some((DefId::new(module, index), sig.generics.len())),
            ItemSig::Error
            | ItemSig::Type(_)
            | ItemSig::Ctor(_)
            | ItemSig::Alias(_)
            | ItemSig::Const(_)
            | ItemSig::Interface(_)
            | ItemSig::InterfaceFn(_)
            | ItemSig::Test => None,
        }
    };
    let (ctor, ctor_generics) = generics(which.ctor_name()).ok_or_else(missing)?;
    let (_, fn_generics) = generics(which.name()).ok_or_else(missing)?;
    if ctor_generics != fn_generics {
        return Err(Ice::new(format!(
            "`std::context::{}` and `{}` have different generics",
            which.ctor_name(),
            which.name()
        )));
    }
    Ok(ctor)
}

/// The path text of the constant `name` of the module whose internal path
/// has `segments` (D-1572, D-1580): the module path and the name joined by
/// `::`, with the internal forms mapped. A constant of a REPL entry, whose
/// module is `<repl:N>`, or `<repl:N:G>` when the entry is checked again,
/// has its bare name. A module that `:reload` loaded again has the mark
/// `<rN>` on its last segment, which the text leaves out.
///
/// The checker has no API for these forms, so they are read from the path's
/// text. This is the third copy of the convention: the driver writes the
/// forms (`crates/cheby_driver/src/session.rs`, `add_entry_module`,
/// `recheck_one` and `reload_path`), and code generation reads them for
/// backtraces (`crates/cheby_codegen_clif/src/modrec.rs`,
/// `source_module`).
pub(crate) fn key_path_text(segments: &[String], name: &str) -> String {
    if let [only] = segments
        && only.starts_with(REPL_MODULE_PREFIX)
        && only.ends_with('>')
    {
        return name.to_owned();
    }
    let mut out = String::new();
    for segment in segments {
        out.push_str(without_reload_mark(segment));
        out.push_str("::");
    }
    out.push_str(name);
    out
}

/// `segment` without the generation mark `<rN>` that a REPL `:reload` gives
/// it; no identifier contains `<`.
fn without_reload_mark(segment: &str) -> &str {
    segment
        .strip_suffix('>')
        .and_then(|rest| rest.rsplit_once("<r"))
        .filter(|(_, digits)| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .map_or(segment, |(name, _)| name)
}

/// The path node of the callee when the initializer `value` is a call of a
/// key function with no hole, parentheses around it included (D-1581): the
/// one mention that is not E0080.
///
/// # Errors
///
/// An [`Ice`] if the callee's item has no interface item.
pub(crate) fn key_site(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    res: &BodyRes,
    value: Expr<'_>,
) -> Result<Option<NodeId>, Ice> {
    let mut value = value;
    while let Expr::Paren(paren) = value {
        let Some(inner) = paren.inner() else {
            return Ok(None);
        };
        value = inner;
    }
    let Expr::Call(call) = value else {
        return Ok(None);
    };
    if call.args().any(|arg| matches!(arg, Arg::Hole(_))) {
        return Ok(None);
    }
    let Some(Expr::Path(callee)) = call.callee() else {
        return Ok(None);
    };
    let Some(path) = callee.path() else {
        return Ok(None);
    };
    let node = path.syntax().id();
    let Some(&Res::Def(def)) = res.paths.get(&node) else {
        return Ok(None);
    };
    Ok(key_fn(input, own, def)?.map(|_| node))
}

/// E0080 at a mention of `which` that is not a constant's whole
/// initializer; `called` when the mention is the callee of a call or a
/// pipe stage.
pub(crate) fn mention_error(which: KeyFn, span: Span, called: bool) -> Diagnostic {
    let label = if called {
        "this call is not the whole initializer of a constant"
    } else {
        "this uses it as a value"
    };
    Diagnostic::new(
        codes::KEY_OUTSIDE_CONSTANT,
        format!(
            "`context::{}` may only be called as the whole initializer of a constant",
            which.name()
        ),
    )
    .with_primary(span, label)
    .with_note("a key is identified by the path of the constant it initializes")
    .with_help(format!(
        "make the call the whole initializer of a `const`, as in `{}`",
        which.example()
    ))
}

/// Rewrites the finished initializer `body` of the constant `name`, whose
/// value [`key_site`] found to be a call of a key function: the callee
/// becomes the key's constructor, and the path text and the type text are
/// appended to the arguments as string literals (D-1572). A conversion to
/// `dyn` around the call is looked through: such a constant is E0068 and
/// never evaluated, but its body is still returned.
///
/// # Errors
///
/// An [`Ice`] if the value is not a call of a key function, if the call's
/// type is not a key, or if an item is unreadable.
pub(crate) fn rewrite(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    name: &str,
    body: &mut thir::Body,
) -> Result<(), Ice> {
    let mut call = body.value;
    while let ExprKind::DynCoerce { value, .. } = body.exprs.try_get(call, "expression")?.kind {
        call = value;
    }
    let root = body.exprs.try_get(call, "expression")?;
    let ExprKind::Call {
        callee: Callee::Fn { def, .. },
        ..
    } = &root.kind
    else {
        return Err(Ice::new("a key site whose finished value is not a call"));
    };
    let Some(which) = key_fn(input, own, *def)? else {
        return Err(Ice::new(
            "a key site whose finished value calls another function",
        ));
    };
    let span = root.span;
    // `Key<T>`: the type of either function's result, which is the
    // constant's declared type with its aliases expanded.
    let TyKind::Named { args, .. } = input.globals.types.kind(root.ty) else {
        return Err(Ice::new("a key call whose type is not a named type"));
    };
    let [value_ty] = **args else {
        return Err(Ice::new("a key call whose type has not one argument"));
    };
    let type_text = path_text(
        value_ty,
        Some(own),
        &input.deps,
        input.modules,
        input.globals,
    )?;
    let module = input.modules.try_get(input.module)?;
    let path = key_path_text(module.path.segments(), name);
    let ctor = key_ctor(input, own, which)?;
    let mut text_arg = |text: String| {
        body.exprs.push(thir::Expr {
            kind: ExprKind::Lit(Lit::Str(text.into())),
            ty: Ty::STRING,
            span,
        })
    };
    let path_id = text_arg(path)?;
    let type_id = text_arg(type_text)?;
    let root = body
        .exprs
        .get_mut(call)
        .ok_or_else(|| Ice::new("the key call disappeared"))?;
    let ExprKind::Call {
        callee: Callee::Fn { def, .. },
        args,
    } = &mut root.kind
    else {
        return Err(Ice::new("the key call changed its kind"));
    };
    *def = ctor;
    let mut all = std::mem::take(args).into_vec();
    all.extend([path_id, type_id]);
    *args = all.into();
    Ok(())
}

#[cfg(test)]
mod tests;
