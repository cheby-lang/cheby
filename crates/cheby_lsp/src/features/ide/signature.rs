//! Signature help (X §4.7): the signature of the call whose argument list
//! holds the cursor, with the parameter the cursor is at.
//!
//! The call is found in the tree alone ([`call_at`]): the nearest argument
//! list that holds the cursor, or that is still open and has only trivia
//! between its last token and the cursor, so `foo(1, ` answers at the end
//! of a file and before a `let` on the next line. The parameter is the
//! number of the list's own commas before the cursor. A hole `_` is a
//! whole argument, so a pipe stage `xs |> list::map(_, f)` needs no special
//! case (D-244).
//!
//! The callee comes from the occurrence index (F §5), which needs only the
//! module's resolution (X §1.3, L1): a function, a positional constructor
//! or an interface function, printed as its declaration (D-1384). A local
//! or any other expression of function type needs the body's facts (L2):
//! the call's `CallFact` (D-1380), else the type of the callee. The label
//! is the declared signature, which stays the same while the call is
//! typed, and a generic item's instantiated signature, from the
//! `CallFact`, is shown first in the documentation, as hover shows it
//! (D-1482).

use cheby_driver::analysis::{
    BodyFacts, CtorFields, DefId, DefIndex, DefKind, FileId, FileLocation, Generics, ItemSig,
    ModuleView, Snapshot, StdIndex, TextRange, TextSize, Ty, TyKind, item_docs,
};
use cheby_syntax::{SyntaxElement, SyntaxKind as K, SyntaxNode, SyntaxToken};

use crate::features::Analyzed;
use crate::features::ide::cursor::{is_significant, token_at};
use crate::features::ide::hover::StdDocs;
use crate::features::ide::print::Printer;
use crate::features::ide::scope;
use crate::features::index::{Classified, Meaning};

/// The call at a cursor, found in the tree alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CallAt<'t> {
    /// The `CALL_EXPR`.
    pub call: SyntaxNode<'t>,
    /// Its callee expression.
    pub callee: SyntaxNode<'t>,
    /// The argument the cursor is at: the list's commas before it.
    pub index: u32,
    /// The call is the value of a `use`, whose callback is the implicit
    /// last argument (§6.4).
    pub in_use: bool,
}

/// The call whose argument list holds `offset` (X §4.7, steps 1 and 2):
/// the innermost list that the cursor is inside, after its `(` and up to
/// its `)`, or that has no `)` and only trivia between its last token and
/// the cursor. `None` outside every argument list, and on a list whose
/// call has no callee.
pub(crate) fn call_at(root: SyntaxNode<'_>, offset: TextSize) -> Option<CallAt<'_>> {
    let left = token_at(root, offset).left()?;
    // The last significant token at or before the cursor: an open list
    // that holds it, holds the cursor.
    let anchor = std::iter::successors(Some(left), |t| t.prev_token())
        .find(|t| is_significant(t.kind()) && t.text_range().start() < offset)?;
    let list = anchor
        .parent()?
        .ancestors()
        .find(|node| node.kind() == K::ARG_LIST && holds(*node, offset))?;
    let call = list.parent().filter(|p| p.kind() == K::CALL_EXPR)?;
    let callee = call.children().find(|child| *child != list)?;
    let commas = list
        .children_with_tokens()
        .filter_map(SyntaxElement::as_token)
        .filter(|t| t.kind() == K::COMMA && t.text_range().end() <= offset)
        .count();
    Some(CallAt {
        call,
        callee,
        index: u32::try_from(commas).ok()?,
        in_use: call.parent().is_some_and(|p| p.kind() == K::USE_STMT),
    })
}

/// Whether the argument list `list` holds `offset`: after its `(`, and
/// before its `)` when it has one.
fn holds(list: SyntaxNode<'_>, offset: TextSize) -> bool {
    let tokens = || {
        list.children_with_tokens()
            .filter_map(SyntaxElement::as_token)
    };
    let opened = tokens()
        .find(|t| t.kind() == K::L_PAREN)
        .is_some_and(|open| open.text_range().end() <= offset);
    let closed = tokens()
        .find(|t| t.kind() == K::R_PAREN)
        .is_none_or(|close| offset <= close.text_range().start());
    opened && closed
}

/// What signature help shows, before it becomes protocol types.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SignatureText {
    /// The signature, on one line (X §4.7's table).
    pub label: String,
    /// Each parameter's byte range in `label`, in order.
    pub params: Vec<TextRange>,
    /// The argument the cursor is at, which may be past the last
    /// parameter: then no parameter is active (X §4.7, D-1481).
    pub index: u32,
    /// Code shown first in the documentation: a generic item's
    /// instantiated signature at this call (D-1482).
    pub code: Option<String>,
    /// Short lines: the `use` note.
    pub notes: Vec<String>,
    /// The callee's doc comment, as Markdown, with bare fences marked as
    /// Cheby.
    pub docs: Option<String>,
}

impl SignatureText {
    /// The active parameter: `index`, unless the call has more arguments
    /// than the signature has parameters. The handler then says so as the
    /// client can take it (D-1481).
    pub(crate) fn active(&self) -> Option<u32> {
        let count = u32::try_from(self.params.len()).ok()?;
        (self.index < count).then_some(self.index)
    }

    /// The text of parameter `n` of the label.
    pub(crate) fn param_text(&self, n: usize) -> Option<&str> {
        self.params.get(n)?.slice(&self.label)
    }

    /// The documentation as Markdown, or as plain text without fences:
    /// the code, the notes and the docs, separated by rules as hover
    /// separates them. `None` when there is nothing to say.
    pub(crate) fn documentation(&self, markdown: bool) -> Option<String> {
        let code = self.code.as_ref().map(|code| {
            if markdown {
                format!("```cheby\n{code}\n```")
            } else {
                code.clone()
            }
        });
        let parts: Vec<String> = [
            code,
            (!self.notes.is_empty()).then(|| self.notes.join("\n\n")),
            self.docs.clone(),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join("\n\n---\n\n"))
    }
}

/// The file of `snapshot` a request names by its normalized `path`: a
/// package file, or a std file of the std base by its display path.
pub(crate) fn file_at(snapshot: &Snapshot, path: &std::path::Path) -> Option<FileId> {
    snapshot.file_of(path).or_else(|| {
        snapshot.sources().files().find_map(|(file, _)| {
            matches!(
                snapshot.location(file),
                Some(FileLocation::Std(display)) if path.ends_with(display)
            )
            .then_some(file)
        })
    })
}

/// The signature help at `offset` of `file`, a file of `analyzed`'s
/// snapshot. `std` serves docs of std items whose trees the snapshot does
/// not keep. `None` outside a call, and where the callee is unknown
/// (X §1.3: L1, and L2 for a function value).
pub(crate) fn signature_help(
    analyzed: &Analyzed,
    file: FileId,
    offset: TextSize,
    std: StdDocs<'_>,
) -> Option<SignatureText> {
    let snapshot = &*analyzed.snapshot;
    let view = snapshot.module_at(file)?;
    let tree = view.tree()?;
    let at = call_at(tree.root(), offset)?;
    let cx = Cx {
        snapshot,
        view,
        printer: Printer::new(snapshot, view),
        body: scope::body_at(view, offset).and_then(|item| Some((item, view.facts()?.body(item)?))),
        std,
    };
    let mut text = match cx.callee_def(analyzed, file, at.callee) {
        Some(def) => cx.item(def, at.callee)?,
        None => cx.value(at.callee)?,
    };
    text.index = at.index;
    if at.in_use {
        let last = text
            .params
            .len()
            .checked_sub(1)
            .and_then(|n| text.param_text(n))
            .and_then(|p| p.split_once(": ").map(|(name, _)| name.to_owned()));
        text.notes.push(match last {
            Some(name) => format!("The rest of the block is passed as `{name}`."),
            None => "The rest of the block is passed as the last argument.".to_owned(),
        });
    }
    Some(text)
}

/// What one request reads.
struct Cx<'s> {
    snapshot: &'s Snapshot,
    view: ModuleView<'s>,
    printer: Printer<'s>,
    /// The body at the cursor and its facts, when it has them.
    body: Option<(DefIndex, &'s BodyFacts)>,
    std: StdDocs<'s>,
}

impl<'s> Cx<'s> {
    /// The item a path callee names, from the index (L1), else from the
    /// call's fact. `None` for a local or another expression.
    fn callee_def(
        &self,
        analyzed: &Analyzed,
        file: FileId,
        callee: SyntaxNode<'_>,
    ) -> Option<DefId> {
        let from_index = (|| {
            let token = last_segment(callee)?;
            let index = analyzed.index().of_file(file)?;
            let Classified::Occurrence(occurrence) = index.classify(token.text_range()) else {
                return None;
            };
            match index.entry(occurrence.symbol)?.meaning {
                Meaning::Item(def) => Some(def),
                Meaning::Module(_)
                | Meaning::Field { .. }
                | Meaning::TypeParam { .. }
                | Meaning::Local { .. } => None,
            }
        })();
        from_index.or_else(|| self.body?.1.call(callee.id())?.def)
    }

    /// The type parameters of `def`'s signature: a constructor's are its
    /// type's.
    fn generics(&self, def: DefId) -> Option<&'s Generics> {
        let iface = self.snapshot.interfaces().get(def.module)?;
        match &iface.item(def.index)?.sig {
            ItemSig::Fn(f) => Some(&f.generics),
            ItemSig::Type(t) => Some(&t.generics),
            ItemSig::Ctor(ctor) => match &iface.item(ctor.ty)?.sig {
                ItemSig::Type(t) => Some(&t.generics),
                _ => None,
            },
            ItemSig::Alias(_)
            | ItemSig::Const(_)
            | ItemSig::Interface(_)
            | ItemSig::InterfaceFn(_)
            | ItemSig::Test
            | ItemSig::Error => None,
        }
    }

    /// The type parameters in force in the body at the cursor.
    fn body_generics(&self) -> Option<&'s Generics> {
        let (item, _) = self.body?;
        self.generics(DefId::new(self.view.id(), item))
    }

    /// A function, a positional constructor or an interface function, as
    /// its declaration reads (X §4.7's table, D-1384), with its docs. The
    /// label is the declared signature and the instantiated one goes first
    /// in the documentation (D-1482): a declared label keeps the parameter
    /// offsets valid while the arguments are typed.
    fn item(&self, def: DefId, callee: SyntaxNode<'_>) -> Option<SignatureText> {
        let iface = self.snapshot.interfaces().get(def.module)?;
        let item = iface.item(def.index)?;
        let callable = match &item.sig {
            ItemSig::Fn(_) | ItemSig::InterfaceFn(_) => true,
            ItemSig::Ctor(ctor) => matches!(ctor.fields, CtorFields::Positional(_)),
            _ => false,
        };
        if !callable {
            return None;
        }
        let header = self.printer.item_header(def)?;
        let line = header.lines().next()?;
        let label = line
            .strip_prefix("pub ")
            .or_else(|| line.strip_prefix("priv "))
            .unwrap_or(line)
            .to_owned();
        let params = param_ranges(&label)?;
        let generic = item.kind == DefKind::InterfaceFn
            || self.generics(def).is_some_and(|g| !g.params.is_empty());
        let code = generic.then(|| self.instantiated(callee)).flatten();
        Some(SignatureText {
            label,
            params,
            code,
            docs: self.item_docs(def),
            ..SignatureText::default()
        })
    }

    /// The call's instantiated signature, from its fact, when it is known
    /// (D-1482): not when every part of it is unknown, nor when every type
    /// argument is, since it then says no more than the declared label.
    fn instantiated(&self, callee: SyntaxNode<'_>) -> Option<String> {
        let call = self.body?.1.call(callee.id())?;
        let all = call.params.iter().chain(std::iter::once(&call.ret));
        if all.clone().all(|&ty| self.printer.is_unknown(ty))
            || call.ty_args.iter().all(|&ty| self.printer.is_unknown(ty))
        {
            return None;
        }
        let generics = self.body_generics();
        let params: Vec<String> = call
            .params
            .iter()
            .map(|&p| self.printer.type_display(p, generics))
            .collect();
        let ret = self.printer.type_display(call.ret, generics);
        Some(format!("fn({}) -> {ret}", params.join(", ")))
    }

    /// A value of function type (L2): `fn(Int) -> String`, with the
    /// parameter types as labels (X §4.7). Its type is the call's fact,
    /// else the callee's own type, else a local's.
    fn value(&self, callee: SyntaxNode<'_>) -> Option<SignatureText> {
        let (_, facts) = self.body?;
        let (params, ret): (Vec<Ty>, Ty) = if let Some(call) = facts.call(callee.id()) {
            (call.params.to_vec(), call.ret)
        } else {
            let ty = facts
                .expr(callee.id())
                .map(|e| e.ty)
                .or_else(|| self.local_type(callee))?;
            let TyKind::Fn { params, ret } = self.snapshot.globals().types.kind(ty) else {
                return None;
            };
            (params.to_vec(), *ret)
        };
        let generics = self.body_generics();
        let mut label = "fn(".to_owned();
        let mut ranges = Vec::with_capacity(params.len());
        for (n, &param) in params.iter().enumerate() {
            if n > 0 {
                label.push_str(", ");
            }
            let start = TextSize::try_from(label.len()).ok()?;
            label.push_str(&self.printer.type_display(param, generics));
            let end = TextSize::try_from(label.len()).ok()?;
            ranges.push(TextRange::new(start, end));
        }
        label.push_str(") -> ");
        label.push_str(&self.printer.type_display(ret, generics));
        Some(SignatureText {
            label,
            params: ranges,
            ..SignatureText::default()
        })
    }

    /// The type of the local a one-segment path callee names, found by its
    /// text among the locals in scope, which no shadowing makes unique
    /// (D-051, D-1383). This serves a callee the checker never reached,
    /// such as one in an `ERROR` region.
    fn local_type(&self, callee: SyntaxNode<'_>) -> Option<Ty> {
        let token = last_segment(callee)?;
        let path = token.parent()?;
        let one = path
            .children_with_tokens()
            .filter_map(SyntaxElement::as_token)
            .filter(|t| matches!(t.kind(), K::LOWER_IDENT | K::UPPER_IDENT))
            .count()
            == 1;
        if !one {
            return None;
        }
        let names = &self.snapshot.globals().names;
        let local = scope::locals_at(self.view, token.text_range().start())
            .into_iter()
            .find(|local| names.get(local.decl.name) == token.text())?;
        Some(scope::local_fact(self.view, local.item, local.slot)?.ty)
    }

    /// The doc comment of `def`, from its module's tree, else from the std
    /// index (D-1385).
    fn item_docs(&self, def: DefId) -> Option<String> {
        let local = self.snapshot.module(def.module).and_then(|view| {
            let resolved = view.resolved()?;
            item_docs(view.tree()?, resolved.item(def.index)?)
        });
        let docs = local.or_else(|| {
            let key = self.snapshot.key_of(def)?;
            std_index(self.std)?.get(&key)?.doc.clone()
        })?;
        Some(cheby_fences(&docs))
    }
}

/// The index behind `std`, waited for only when asked.
fn std_index(std: StdDocs<'_>) -> Option<&StdIndex> {
    match std {
        StdDocs::None => None,
        StdDocs::Index(index) => Some(index),
        StdDocs::Service(service) => service.index(),
    }
}

/// The last name of a path callee: `map` of `list::map`. `None` for a
/// callee that is not a path.
fn last_segment(callee: SyntaxNode<'_>) -> Option<SyntaxToken<'_>> {
    let path = callee
        .children()
        .find(|c| c.kind() == K::PATH)
        .filter(|_| callee.kind() == K::PATH_EXPR)?;
    path.children_with_tokens()
        .filter_map(SyntaxElement::as_token)
        .filter(|t| matches!(t.kind(), K::LOWER_IDENT | K::UPPER_IDENT))
        .last()
}

/// The byte ranges of the parameters of a one-line signature `label`:
/// the parts between its first `(` and the `)` that closes it, split at
/// commas outside brackets. `->` is an arrow, not a bracket. `None` when
/// the list does not close.
fn param_ranges(label: &str) -> Option<Vec<TextRange>> {
    let open = label.find('(')?;
    let mut ranges = Vec::new();
    let mut depth = 0_u32;
    let mut start = open.checked_add(1)?;
    let mut prev = '\0';
    for (at, c) in label.char_indices().skip_while(|&(at, _)| at <= open) {
        match c {
            '(' | '<' | '[' => depth = depth.checked_add(1)?,
            '>' if prev == '-' => {}
            ')' | '>' | ']' if depth > 0 => depth = depth.checked_sub(1)?,
            ')' => {
                push_part(label, start, at, &mut ranges)?;
                return Some(ranges);
            }
            ',' if depth == 0 => {
                push_part(label, start, at, &mut ranges)?;
                start = at.checked_add(1)?;
            }
            _ => {}
        }
        prev = c;
    }
    None
}

/// Adds the part of `label` from `start` to `end`, without its spaces,
/// to `ranges`. An empty part, as in `()`, adds nothing.
fn push_part(label: &str, start: usize, end: usize, ranges: &mut Vec<TextRange>) -> Option<()> {
    let part = label.get(start..end)?;
    let trimmed = part.trim_start();
    let first = start.checked_add(part.len().checked_sub(trimmed.len())?)?;
    let last = first.checked_add(trimmed.trim_end().len())?;
    if first < last {
        ranges.push(TextRange::new(
            TextSize::try_from(first).ok()?,
            TextSize::try_from(last).ok()?,
        ));
    }
    Some(())
}

/// `docs` with each bare opening fence marked as Cheby, since doc
/// comments are Cheby by default (§13.10), as hover marks them.
fn cheby_fences(docs: &str) -> String {
    let mut open = false;
    let lines: Vec<String> = docs
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("```") {
                return line.to_owned();
            }
            let was_open = open;
            open = !open;
            if !was_open && trimmed.trim_end() == "```" {
                let indent = line.len().saturating_sub(trimmed.len());
                format!("{}```cheby", line.get(..indent).unwrap_or(""))
            } else {
                line.to_owned()
            }
        })
        .collect();
    lines.join("\n")
}

#[cfg(test)]
#[path = "signature_tests.rs"]
mod tests;
