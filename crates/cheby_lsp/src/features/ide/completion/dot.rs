//! Completion after `xs.`: fields and pipe-style functions (X §4.2,
//! D-1394, plan §1.11).
//!
//! [`complete`] is called for [`Context::Dot`](super::context::Context::Dot): after
//! `x.`, while typing `x.fo`, and after `.` in an interpolation, where
//! only fields apply. Everything needs the receiver's type, from the facts:
//! an unknown receiver gives an empty list, since guessing by name would
//! be worse than nothing.
//!
//! - **Fields** (rank 05): those every constructor of the receiver's type
//!   has with one type (D-071), when the module may see the type's
//!   structure. The edit replaces the partial word.
//! - **Functions** (`completion.typeDriven`): the [`candidates`] of
//!   `pipe`. Accepting one replaces only `.partial` with the pipe form,
//!   ` |> list::map(_, ${1:f})$0` or ` |> list::length`, so the edit sits
//!   on one line whatever the receiver spans (D-1394). Where a
//!   pipe would change meaning as an operand (a binary operator that binds
//!   as tightly as `|>` or more, a prefix operator, a further `.` or a
//!   call), the call form `list::map(xs, ${1:f})$0` replaces the receiver
//!   too, and is offered only when the receiver is on one line, with `$`,
//!   `}` and `\` escaped in it.
//! - **Postfix `case`**: for a type with constructors and a client that
//!   takes snippets, `case xs {` with one arm per constructor, under the
//!   same one-line rule as the call form.
//!
//! `filterText` (X §4.2): Zed filters on it against the word before the
//! cursor, so it is the function's name; other clients filter on the text
//! of the edit's range, so it is that text up to the partial word, then
//! the name: `.map` for the pipe form, `xs.map` for the call form. This
//! is the one answer that depends on `clientInfo`.

use cheby_driver::analysis::{
    CtorFields, DefId, ItemSig, ModuleId, Name, Res, TextRange, Ty, TyKind, TypeDefKind,
    structure_visible,
};
use cheby_syntax::{SyntaxKind as K, SyntaxNode, SyntaxToken};

use super::context::Receiver;
use super::items::{Entry, EntryKind, Insert, escape, rank};
use super::pipe::{call_text, candidates, insert_at, param_names};
use super::{CompletionCx, Items};

/// Pushes the items after `dot`, which reads `receiver`. The partial word
/// after the `.`, if any, is `cx.partial()`.
pub(crate) fn complete(
    cx: &CompletionCx<'_>,
    receiver: &Receiver<'_>,
    dot: SyntaxToken<'_>,
    out: &mut Items,
) {
    match receiver {
        Receiver::Interpolation { names, .. } => {
            if let Some(ty) = interpolated_ty(cx, receiver, names) {
                fields(cx, ty, out);
            }
        }
        Receiver::Expr(node) => {
            let Some(ty) = cx.expr_ty(*node) else { return };
            fields(cx, ty, out);
            if cx.options.type_driven {
                functions(cx, *node, dot, ty, out);
                postfix_case(cx, *node, ty, out);
            }
        }
    }
}

/// The type of the names of an interpolation before its `.`: the root's,
/// a local or a constant, then each field's, from the facts.
fn interpolated_ty(
    cx: &CompletionCx<'_>,
    receiver: &Receiver<'_>,
    names: &[SyntaxToken<'_>],
) -> Option<Ty> {
    let Receiver::Interpolation { node, .. } = receiver else {
        return None;
    };
    let (item, facts) = cx.body()?;
    let view = cx.resolved_view()?;
    let rest = names.get(1..)?;
    let res = view.resolved()?.bodies.get(&item)?.names.get(&node.id())?;
    let mut ty = match *res {
        Res::Local(local) => facts.locals.get(local)?.ty,
        Res::Def(def) => match &cx.item(def)?.sig {
            ItemSig::Const(c) => c.ty,
            _ => return None,
        },
        _ => return None,
    };
    for name in rest {
        ty = facts.field_at(name.text_range().start())?.ty?;
    }
    cx.known(ty)
}

/// Pushes the fields of `ty` (D-071): those every constructor has with
/// one type, when the module may see the type's structure, in
/// declaration order.
fn fields(cx: &CompletionCx<'_>, ty: Ty, out: &mut Items) {
    let TyKind::Named { def, args } = cx.snapshot.globals().types.kind(ty) else {
        return;
    };
    let (def, args) = (*def, args.clone());
    let Some(item) = cx.item(def) else { return };
    let ItemSig::Type(sig) = &item.sig else {
        return;
    };
    let TypeDefKind::Adt { ctors } = &sig.kind else {
        return;
    };
    if !structure_seen(cx, def) {
        return;
    }
    let mut common: Option<Vec<(Name, Ty)>> = None;
    for &ctor in ctors {
        let Some(ItemSig::Ctor(ctor)) = cx.item(DefId::new(def.module, ctor)).map(|i| &i.sig)
        else {
            return;
        };
        let CtorFields::Named(named) = &ctor.fields else {
            return;
        };
        common = Some(match common {
            None => named.to_vec(),
            Some(kept) => kept
                .into_iter()
                .filter(|field| named.contains(field))
                .collect(),
        });
    }
    let types = &cx.snapshot.globals().types;
    for ((name, field_ty), order) in common.unwrap_or_default().into_iter().zip(0_u16..) {
        let label = cx.name_text(name);
        let mut entry = Entry::new(label, EntryKind::Field, rank::FIELD, cx.insert(label));
        entry.order = Some(order);
        // A field whose type is one of the type's parameters shows the
        // receiver's argument; any other shows the declared type.
        entry.detail = match types.kind(field_ty) {
            TyKind::Param(param) => sig
                .generics
                .params
                .iter_enumerated()
                .position(|(p, _)| p == *param)
                .and_then(|n| args.get(n))
                .and_then(|&arg| cx.type_text(arg, None)),
            _ => cx.type_text(field_ty, Some(def)),
        };
        out.push(entry);
    }
}

/// Whether the module at the cursor may see the structure of the type
/// `def` (§4.3.4, D-048).
fn structure_seen(cx: &CompletionCx<'_>, def: DefId) -> bool {
    let Some(view) = cx.view else { return false };
    if def.module == view.id() || def.module == ModuleId::BUILTIN {
        return true;
    }
    let table = cx.snapshot.table();
    let (Some(item), Some(theirs), Some(ours)) =
        (cx.item(def), table.get(def.module), table.get(view.id()))
    else {
        return false;
    };
    structure_visible(item, def.module, theirs.package, view.id(), ours.package)
}

/// How a function after `xs.` is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Form {
    /// The pipe form, replacing `.partial` (D-1394).
    Pipe,
    /// The call form, replacing the receiver too: the receiver is an
    /// operand a pipe would rebind, and it is on one line.
    Call,
}

/// The form functions take after the receiver `node`, or `None` when no
/// form fits: a pipe would change meaning and the receiver spans lines,
/// or the `.` and the partial word are on different lines.
fn form(cx: &CompletionCx<'_>, node: SyntaxNode<'_>, dot: SyntaxToken<'_>) -> Option<Form> {
    let partial = cx.partial();
    if spans_lines(cx, TextRange::new(dot.text_range().start(), partial.end())) {
        return None;
    }
    let field = node.parent().filter(|p| p.kind() == K::FIELD_EXPR)?;
    if !is_tight(field) {
        return Some(Form::Pipe);
    }
    let whole = TextRange::new(node.text_range().start(), partial.end());
    (!spans_lines(cx, whole)).then_some(Form::Call)
}

/// Whether `range` of the text holds a line break.
fn spans_lines(cx: &CompletionCx<'_>, range: TextRange) -> bool {
    range.slice(cx.text).is_none_or(|text| text.contains('\n'))
}

/// Whether a pipe in place of `field`, a `FIELD_EXPR`, would change the
/// meaning (X §4.2's tight context): it is an operand of a binary operator
/// that binds as tightly as `|>` or more (binding power 4 and up, §5.4.1),
/// of a prefix operator, or the receiver of a further `.` or the callee of
/// a call.
fn is_tight(field: SyntaxNode<'_>) -> bool {
    let Some(parent) = field.parent() else {
        return false;
    };
    match parent.kind() {
        K::BIN_EXPR => parent
            .children_with_tokens()
            .filter_map(cheby_syntax::SyntaxElement::as_token)
            .find_map(|t| binding_power(t.kind()))
            .is_some_and(|bp| bp >= 4),
        K::PREFIX_EXPR | K::FIELD_EXPR => true,
        K::CALL_EXPR => parent.first_child() == Some(field),
        _ => false,
    }
}

/// The binding power of a binary operator (§5.4.1), as the parser has it.
const fn binding_power(kind: K) -> Option<u8> {
    let bp = match kind {
        K::PIPE2 => 1,
        K::AMP2 => 2,
        K::EQ2 | K::NEQ | K::LT | K::LE | K::GT | K::GE => 3,
        K::PIPE_GT => 4,
        K::PIPE => 5,
        K::CARET => 6,
        K::AMP => 7,
        K::SHL | K::SHR => 8,
        K::PLUS | K::MINUS => 9,
        K::STAR | K::SLASH | K::PERCENT => 10,
        _ => return None,
    };
    Some(bp)
}

/// `filterText` for an item labelled `label` whose edit replaces `range`
/// (X §4.2): the name for Zed, else the range's text before the partial
/// word followed by the name.
fn filter_text(cx: &CompletionCx<'_>, range: TextRange, label: &str) -> String {
    if cx.options.zed {
        return label.to_owned();
    }
    let before = TextRange::new(range.start(), cx.partial().start())
        .slice(cx.text)
        .unwrap_or("");
    format!("{before}{label}")
}

/// Pushes the pipe-style functions whose parameter accepts `ty`, the
/// receiver's type, in the form the receiver's place allows.
fn functions(
    cx: &CompletionCx<'_>,
    node: SyntaxNode<'_>,
    dot: SyntaxToken<'_>,
    ty: Ty,
    out: &mut Items,
) {
    let Some(form) = form(cx, node, dot) else {
        return;
    };
    let partial = cx.partial();
    let (range, receiver) = match form {
        Form::Pipe => (
            TextRange::new(dot.text_range().start(), partial.end()),
            None,
        ),
        Form::Call => {
            let range = TextRange::new(node.text_range().start(), partial.end());
            (range, node.text_range().slice(cx.text))
        }
    };
    // No space is added where one is already before the `.`.
    let spaced = dot
        .prev_token()
        .is_some_and(|t| t.kind().is_trivia() || t.kind() == K::NEWLINE);
    for candidate in candidates(cx, ty, false) {
        let name = cx
            .item(candidate.def)
            .map_or("", |item| cx.name_text(item.name));
        let Some(mut entry) = cx.def_entry(candidate.def, name, candidate.rank) else {
            continue;
        };
        let params = param_names(cx, candidate.def);
        let (snippet, plain) = call_text(&candidate.path, &params, candidate.hole, receiver);
        let lead = if form == Form::Pipe {
            if spaced { "|> " } else { " |> " }
        } else {
            ""
        };
        entry.insert = insert_at(
            cx,
            range,
            (
                snippet.map(|s| format!("{lead}{s}")),
                format!("{lead}{plain}"),
            ),
        );
        entry.filter_text = Some(filter_text(cx, range, name));
        entry.additional = candidate.import.into_iter().collect();
        out.push(entry);
    }
}

/// Pushes the postfix `case` (X §4.2): the receiver as a `case` subject,
/// with one arm per constructor of its type, each body a `todo`
/// placeholder. Offered to a client that takes snippets, for a type whose
/// structure the module sees, where the receiver is on one line and no
/// operator binds it. Its later lines are written relative to its first:
/// the arms at two spaces, the `}` at none, for the client to indent as
/// the line it starts on (D-1506).
fn postfix_case(cx: &CompletionCx<'_>, node: SyntaxNode<'_>, ty: Ty, out: &mut Items) {
    if !cx.options.snippets {
        return;
    }
    let Some(field) = node.parent().filter(|p| p.kind() == K::FIELD_EXPR) else {
        return;
    };
    if is_tight(field) {
        return;
    }
    let range = TextRange::new(node.text_range().start(), cx.partial().end());
    if spans_lines(cx, range) {
        return;
    }
    let Some(receiver) = node.text_range().slice(cx.text) else {
        return;
    };
    let Some(head) = cx.head(ty) else { return };
    let Some(ItemSig::Type(sig)) = cx.item(head).map(|i| &i.sig) else {
        return;
    };
    let TypeDefKind::Adt { ctors } = &sig.kind else {
        return;
    };
    if ctors.is_empty() || !structure_seen(cx, head) {
        return;
    }
    let mut arms: Vec<String> = Vec::new();
    for (&ctor, stop) in ctors.iter().zip(1_u32..) {
        let def = DefId::new(head.module, ctor);
        let Some(pattern) = arm_pattern(cx, def) else {
            return;
        };
        arms.push(format!("  {} => ${{{stop}:todo}}", escape(&pattern)));
    }
    let text = format!("case {} {{\n{}\n}}$0", escape(receiver), arms.join("\n"));
    let mut entry = Entry::new(
        "case",
        EntryKind::Snippet,
        rank::KEYWORD,
        Insert {
            range,
            text,
            snippet: true,
        },
    );
    entry.filter_text = Some(filter_text(cx, range, "case"));
    out.push(entry);
}

/// The pattern of constructor `def` in a postfix `case`'s arm: `None`,
/// `Some(_)`, `Circle { .. }`. `None` when the module cannot name it.
fn arm_pattern(cx: &CompletionCx<'_>, def: DefId) -> Option<String> {
    let name = cx.value_name(def)?;
    let ItemSig::Ctor(ctor) = &cx.item(def)?.sig else {
        return None;
    };
    Some(match &ctor.fields {
        CtorFields::Unit => name,
        CtorFields::Positional(fields) => {
            let holes: Vec<&str> = fields.iter().map(|_| "_").collect();
            format!("{name}({})", holes.join(", "))
        }
        CtorFields::Named(_) => format!("{name} {{ .. }}"),
    })
}

#[cfg(test)]
#[path = "dot_tests.rs"]
pub(crate) mod tests;
