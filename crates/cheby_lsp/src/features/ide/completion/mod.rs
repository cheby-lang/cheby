//! `textDocument/completion` (X §4.1, §4.6): find the context from the
//! tree around the cursor ([`context`]), build its items ([`items`]), then
//! sort them by rank.
//!
//! Each [`context::Context`] names its sources. Package B's contexts
//! dispatch to [`dot::complete`], [`pipe::complete`] and
//! [`pipe::qualified`], and the names of modules a file does not import
//! come from [`auto_import::complete`] (X §4.2 to §4.4). Those modules build
//! [`Entry`]s through [`CompletionCx`]'s helpers, with ranks from
//! [`items::rank`] and kinds from [`items::EntryKind`], and push them into
//! [`Items`], which sorts and deduplicates them.
//!
//! The state each source needs (X §1.3): a resolved module (L1) for names,
//! a body's facts (L2) for types. On less it gives what it can: with no
//! resolution, the keywords and snippets of the context, and the names an
//! import path or an attribute takes, which need no checker (X §4.1's
//! failure rule).
//!
//! Everything here is syntax and the snapshot's data: no edit is checked
//! and nothing is resolved later but docs (`completionItem/resolve`).

pub(crate) mod auto_import;
pub(crate) mod context;
pub(crate) mod dot;
pub(crate) mod items;
pub(crate) mod pipe;

use cheby_driver::analysis::{
    DefIndex, ModuleView, Snapshot, StdIndex, TextRange, TextSize, Ty, TyKind,
};
use cheby_syntax::{SyntaxKind, SyntaxNode, SyntaxToken, SyntaxTree};

pub(crate) use self::context::{Found, detect};
pub(crate) use self::items::{Entry, Items};

use crate::features::ide::cursor::token_at;
use crate::features::ide::print::Printer;
use crate::features::ide::scope;
use crate::features::stdlib::StdService;

/// What the client and the settings allow (D-1376, X §4.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one switch per client capability and setting"
)]
pub(crate) struct Options {
    /// `completionItem.snippetSupport`: snippets and placeholders.
    pub snippets: bool,
    /// `completion.typeDriven`: rank 00 for a type match, and package B's
    /// pipe-style functions after `.`.
    pub type_driven: bool,
    /// `completion.autoImport`: names of modules the file does not import
    /// (X §4.4).
    pub auto_import: bool,
    /// The client is Zed, which filters on `filterText` against the word
    /// (X §4.2): package B's one use of `clientInfo`.
    pub zed: bool,
}

/// Where std's index comes from: the std base's, waited for only when an
/// item needs it, or a given one (D-1372).
#[derive(Clone, Copy, Debug)]
pub(crate) enum StdAccess<'a> {
    /// No index: items from std's unloaded modules are left out.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the server always has the std base")
    )]
    None,
    /// This index.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the server reads the std base's")
    )]
    Index(&'a StdIndex),
    /// The std base's index.
    Service(&'a StdService),
}

impl<'a> StdAccess<'a> {
    /// The index, if there is one. The std base is waited for here.
    pub(crate) fn get(self) -> Option<&'a StdIndex> {
        match self {
            Self::None => None,
            Self::Index(index) => Some(index),
            Self::Service(service) => service.index(),
        }
    }
}

/// What every source of items reads: the module at the cursor, its tree
/// and text, and the cursor's context.
pub(crate) struct CompletionCx<'s> {
    /// The snapshot.
    pub snapshot: &'s Snapshot,
    /// The module at the cursor. `None` for a file that is no module of
    /// the run, which gets L0 items from `tree`.
    pub view: Option<ModuleView<'s>>,
    /// The file's tree.
    pub tree: &'s SyntaxTree,
    /// The file's text, which `tree` covers.
    pub text: &'s str,
    /// The cursor.
    pub offset: TextSize,
    /// The context, the partial and the anchor.
    pub found: Found<'s>,
    /// Types and items as the module writes them. `None` at L0.
    pub printer: Option<Printer<'s>>,
    /// What the client and settings allow.
    pub options: Options,
    /// Std's index, for modules the run did not load.
    pub std: StdAccess<'s>,
    /// What follows the partial word, read once per request.
    pub next: Next,
}

/// The tokens after the partial word, which decide what an edit may
/// write: no call where a `(` follows, no `::` where one follows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Next {
    /// The kind of the token that starts where the partial word ends.
    pub adjacent: Option<SyntaxKind>,
    /// The kind of the first token after the partial word on its line,
    /// past white space and comments. `None` at the end of the line.
    pub on_line: Option<SyntaxKind>,
}

impl Next {
    /// What follows `partial` in the tree under `root`.
    fn after(root: SyntaxNode<'_>, partial: TextRange) -> Self {
        let end = partial.end();
        let first = token_at(root, end)
            .right()
            .filter(|t| t.text_range().end() > end);
        let adjacent = first
            .filter(|t| t.text_range().start() == end)
            .map(SyntaxToken::kind);
        let on_line = std::iter::successors(first, |t| t.next_token())
            .find(|t| !t.kind().is_trivia() && !t.text_range().is_empty())
            .map(SyntaxToken::kind)
            .filter(|&k| k != SyntaxKind::NEWLINE);
        Self { adjacent, on_line }
    }
}

impl std::fmt::Debug for CompletionCx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompletionCx")
            .field("offset", &self.offset)
            .field("found", &self.found)
            .finish_non_exhaustive()
    }
}

impl<'s> CompletionCx<'s> {
    /// The range every item's edit replaces: the partial word (X §4.1).
    pub(crate) const fn partial(&self) -> TextRange {
        self.found.partial
    }

    /// The text of the partial word.
    pub(crate) fn partial_text(&self) -> &'s str {
        let range = self.found.partial;
        let (Ok(start), Ok(end)) = (usize::try_from(range.start()), usize::try_from(range.end()))
        else {
            return "";
        };
        self.text.get(start..end).unwrap_or("")
    }

    /// The module at the cursor, when it has a resolution (L1).
    pub(crate) fn resolved_view(&self) -> Option<ModuleView<'s>> {
        self.view.filter(|view| view.resolved().is_some())
    }

    /// The item whose body holds the cursor, with its facts (L2).
    pub(crate) fn body(&self) -> Option<(DefIndex, &'s cheby_driver::analysis::BodyFacts)> {
        let view = self.view?;
        let item = scope::body_at(view, self.offset)?;
        Some((item, view.facts()?.body(item)?))
    }

    /// The fact type of the expression `node`, unless unknown at its head.
    pub(crate) fn expr_ty(&self, node: SyntaxNode<'_>) -> Option<Ty> {
        let (_, facts) = self.body()?;
        let ty = facts.expr(node.id())?.ty;
        self.known(ty)
    }

    /// `ty`, unless it is unknown at its head.
    pub(crate) fn known(&self, ty: Ty) -> Option<Ty> {
        (!matches!(self.snapshot.globals().types.kind(ty), TyKind::Error)).then_some(ty)
    }
}

/// The items of one request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Completions {
    /// The items, sorted by `sortText`, then label.
    pub items: Vec<Entry>,
    /// The list was capped (X §4.4), so the client must ask again as the
    /// word grows.
    pub incomplete: bool,
}

/// The completions at `offset` of `tree`, the tree of `view` or of a file
/// that is no module of `snapshot`.
pub(crate) fn complete<'s>(
    snapshot: &'s Snapshot,
    view: Option<ModuleView<'s>>,
    tree: &'s SyntaxTree,
    text: &'s str,
    offset: TextSize,
    options: Options,
    std: StdAccess<'s>,
) -> Completions {
    let found = detect(tree.root(), offset);
    let next = Next::after(tree.root(), found.partial);
    let printer = view
        .filter(|v| v.resolved().is_some())
        .map(|v| Printer::new(snapshot, v));
    let cx = CompletionCx {
        snapshot,
        view,
        tree,
        text,
        offset,
        found,
        printer,
        options,
        std,
        next,
    };
    let mut out = Items::default();
    items::collect(&cx, &mut out);
    out.finish()
}

#[cfg(test)]
#[path = "tests.rs"]
pub(crate) mod tests;
