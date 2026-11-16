//! Context keys (§10.11): the rules of a function that carries the
//! standard library's `@std_initializer(constructor)`, today
//! `std::context::key` and `std::context::key_without_default` (D-1599).
//! The attribute is the one source of these rules: no function is known by
//! its name, and the constructor is read from the function's signature in
//! its module's interface ([`cheby_iface::FnSig::initializer`], D-246).
//!
//! - The call that is a constant's whole initializer, parentheses around it
//!   included, becomes a call of the constructor, `key_at` or
//!   `key_without_default_at`, with the constant's path text and its type
//!   text as two more arguments (D-1565, D-1572, D-1581). A constructor has
//!   the generics of the function it stands for, which `decl::initializer`
//!   checks in std's own module (D-1587, D-1599), so the recorded type
//!   arguments fit it unchanged. The swap is made in the finished body, so
//!   the language server's facts keep the function that is written.
//! - Every other mention of such a function in an expression is E0080
//!   (D-1565, D-1589). An import is not a mention (D-1581).
//! - The type check of §4.5 does not look inside the type argument of a
//!   `context::Key<T>` ([`is_key_type`], D-1578, D-1591). That rule is
//!   about the type, which is found by module path and name.
//! - Such a function never counts toward satisfying an interface, so no
//!   interface call, bound or `dyn` conversion reaches it: `ifaces::solve`
//!   asks [`key_fn`] where it picks a type's module function (D-1593).

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{
    DefId, DefKind, Ice, ItemSig, ModuleInterface, Name, Namespace, PackageId, Ty, TyKind, Vis,
    display, path_text,
};
use cheby_resolve::{BodyRes, Res};
use cheby_syntax::NodeId;
use cheby_syntax::ast::{Arg, AstNode, Expr};

use crate::TypesInput;
use crate::decl::initializer::fits;
use crate::infer::substitute;
use crate::thir::{self, Callee, ExprKind, Lit};

/// The module segment of `std::context` under `std`.
const CONTEXT: &str = "context";

/// The start of a REPL entry's module, `<repl:N>` or `<repl:N:G>`.
const REPL_MODULE_PREFIX: &str = "<repl:";

/// A function that carries `@std_initializer(constructor)` (D-1599):
/// `context::key` or `context::key_without_default` today.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyFn {
    /// The function.
    def: DefId,
    /// The name of the constructor a whole initializer calls instead, a
    /// function of the same module.
    ctor: Name,
}

/// Whether `def` is an item of the standard library's `std::context`.
/// It runs for every type a constant's type names, so it compares the
/// segments of `std::context` without building that path.
fn in_context(input: &TypesInput<'_>, def: DefId) -> bool {
    input.modules.get(def.module).is_some_and(|info| {
        info.package == PackageId::STD
            && matches!(info.path.segments(), [std, module] if std == "std" && module == CONTEXT)
    })
}

/// The function `def` with its initializer constructor, when its
/// signature records one (D-1599). It runs for every function a body
/// names, so it reads only the item.
///
/// # Errors
///
/// An [`Ice`] if `def` has no interface item.
pub(crate) fn key_fn(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    def: DefId,
) -> Result<Option<KeyFn>, Ice> {
    let item = input.item(own, def)?;
    if item.kind != DefKind::Fn {
        return Ok(None);
    }
    let ItemSig::Fn(sig) = &item.sig else {
        return Ok(None);
    };
    Ok(sig.initializer.map(|ctor| KeyFn { def, ctor }))
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

/// The constructor that stands for `which` (D-1587, D-1599): from `own`
/// when this module is the function's own, else through the
/// dependencies. `None` when the constructor's signature has errors or
/// does not have the shape the function gives it ([`fits`]): its own
/// module reports both, the first where the signature is written and the
/// second as E0045 at the attribute (`decl::initializer`), so the use is
/// silent and is never rewritten into an ill-typed call.
///
/// # Errors
///
/// An [`Ice`] if the function's module is not readable or has no such
/// function: a mention of `which` needs its module, and a module whose
/// signature records a constructor declares it.
fn key_ctor(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    which: KeyFn,
) -> Result<Option<DefId>, Ice> {
    let missing = || {
        Ice::new(format!(
            "no initializer constructor `{}` for `{}`",
            input.globals.names.get(which.ctor),
            input.name_of(own, which.def)
        ))
    };
    let ItemSig::Fn(sig) = &input.item(own, which.def)?.sig else {
        return Err(Ice::new(
            "an initializer function without a function signature",
        ));
    };
    let module = which.def.module;
    let iface = if module == input.module {
        own
    } else {
        input.deps.get(module).ok_or_else(missing)?
    };
    // The module's interface records a constructor only when it names a
    // function of the module, so a name a dependency's interface does not
    // hold is a `priv` function that the encoding left out. Its module
    // reports it as E0045, cold or warm, and the use stays silent (D-1587).
    let Some(index) = iface.lookup(Namespace::Value, which.ctor) else {
        return if module == input.module {
            Err(missing())
        } else {
            Ok(None)
        };
    };
    let item = iface.item(index).ok_or_else(missing)?;
    if item.vis == Vis::Priv {
        return Ok(None);
    }
    match &item.sig {
        ItemSig::Fn(ctor) => {
            Ok(fits(&input.globals.types, ctor, sig).then(|| DefId::new(module, index)))
        }
        ItemSig::Error => Ok(None),
        ItemSig::Type(_)
        | ItemSig::Ctor(_)
        | ItemSig::Alias(_)
        | ItemSig::Const(_)
        | ItemSig::Interface(_)
        | ItemSig::InterfaceFn(_)
        | ItemSig::Test => Err(missing()),
    }
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
/// pipe stage. The message names the function as a user writes it,
/// `context::key`, and the help shows a constant initialized by a call of
/// it ([`example`]).
///
/// # Errors
///
/// An [`Ice`] if the function's item or module is unreadable.
pub(crate) fn mention_error(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    which: KeyFn,
    span: Span,
    called: bool,
) -> Result<Diagnostic, Ice> {
    let label = if called {
        "this call is not the whole initializer of a constant"
    } else {
        "this uses it as a value"
    };
    let written = written_name(input, own, which.def)?;
    Ok(Diagnostic::new(
        codes::KEY_OUTSIDE_CONSTANT,
        format!("`{written}` may only be called as the whole initializer of a constant"),
    )
    .with_primary(span, label)
    .with_note("a key is identified by the path of the constant it initializes")
    .with_help(format!(
        "make the call the whole initializer of a `const`, as in `{}`",
        example(input, own, which.def, &written)?
    )))
}

/// The function `def` as a user writes it: the last segment of its
/// module's path and its name, `context::key` (D-463).
fn written_name(input: &TypesInput<'_>, own: &ModuleInterface, def: DefId) -> Result<String, Ice> {
    let module = input.modules.try_get(def.module)?;
    let name = input.name_of(own, def);
    Ok(match module.path.segments().last() {
        Some(last) => format!("{last}::{name}"),
        None => name.to_owned(),
    })
}

/// A constant initialized by a call of `def`, written `written`, for
/// E0080's help: every type parameter is `Int`, a parameter of type `Int`
/// is `0` and any other is written by its name, as in
/// `const name: context::Key<Int> = context::key(0)` and
/// `const name: context::Key<Option<Int>> = context::key_without_default()`.
fn example(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    def: DefId,
    written: &str,
) -> Result<String, Ice> {
    let item = input.item(own, def)?;
    let ItemSig::Fn(sig) = &item.sig else {
        return Err(Ice::new(
            "an initializer function without a function signature",
        ));
    };
    let types = &input.globals.types;
    let ints = vec![Ty::INT; sig.generics.len()];
    let args: Vec<String> = sig
        .params
        .iter()
        .map(|p| {
            if substitute(types, p.ty, &ints) == Ty::INT {
                "0".to_owned()
            } else {
                input.globals.names.get(p.name).to_owned()
            }
        })
        .collect();
    let ret = display(
        substitute(types, sig.ret, &ints),
        &input.display_cx(own, None),
    );
    Ok(format!(
        "const name: {ret} = {written}({})",
        args.join(", ")
    ))
}

/// Rewrites the finished initializer `body` of the constant `name`, whose
/// value [`key_site`] found to be a call of a key function: the callee
/// becomes the key's constructor, and the path text and the type text are
/// appended to the arguments as string literals (D-1572). A conversion to
/// `dyn` around the call is looked through: such a constant is E0068 and
/// never evaluated, but its body is still returned. `false`, with `body`
/// unchanged, when the constructor is unusable ([`key_ctor`]): the
/// constant then has no body, as one whose signature has an error.
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
) -> Result<bool, Ice> {
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
    let Some(ctor) = key_ctor(input, own, which)? else {
        return Ok(false);
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
    Ok(true)
}

#[cfg(test)]
mod tests;
