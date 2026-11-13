//! Completion items (X §4.1, §4.6): the sources of each context, the item
//! builder, ranking, keywords and snippets.
//!
//! An [`Entry`] is one item before protocol types: its label, kind, rank
//! and the text that replaces the partial word. [`Items`] collects them;
//! [`Items::finish`] sorts by `sortText` and drops repeats. The helpers on
//! [`CompletionCx`] build entries the same way for every source, package
//! B's included: [`CompletionCx::def_entry`] for an item of any module,
//! [`CompletionCx::call_insert`] for a call, [`CompletionCx::value_name`]
//! for a value as the module writes it.
//!
//! **Ranking** (X §4.1): `sortText` is the rank's two digits and the label.
//! An entry with an `order`, such as a local, puts four digits of it
//! between them, so locals come nearest binding first.

use std::collections::BTreeSet;

use cheby_driver::analysis::{
    CtorFields, DefId, DefKind, Generics, IfaceItem, ItemSig, ModuleId, ModulePath, ModuleView,
    Name, Namespace, Res, ScopeSource, StdItem, Symbol, TextRange, Ty, TyKind, TypeDefKind,
    builtin, interface_fns, structure_visible,
};
use cheby_syntax::{SyntaxKind as K, SyntaxNode, SyntaxToken};

use super::context::{Context, ExprSite, PathNs, Subject};
use super::{CompletionCx, Completions, auto_import, dot, pipe};
use crate::features::ide::cursor::prev_significant;
use crate::features::ide::scope;

/// The ranks of X §4.1 and §4.2: lower sorts first.
pub(crate) mod rank {
    /// The type matches what the position expects.
    pub(crate) const EXPECTED: u8 = 0;
    /// A field after `.` (X §4.2), or of a record literal.
    pub(crate) const FIELD: u8 = 5;
    /// A local, nearest binding first; a type parameter.
    pub(crate) const LOCAL: u8 = 10;
    /// A local of function type after `|>` (X §4.3).
    pub(crate) const LOCAL_FN: u8 = 15;
    /// An item of the current module, or of the qualifying module.
    pub(crate) const MODULE: u8 = 20;
    /// An imported value or a module name.
    pub(crate) const IMPORTED: u8 = 30;
    /// The prelude; another loaded module's function after `|>`.
    pub(crate) const PRELUDE: u8 = 40;
    /// Keywords and snippets.
    pub(crate) const KEYWORD: u8 = 50;
    /// Not imported (X §4.4), or from std's index (X §4.3).
    pub(crate) const NOT_IMPORTED: u8 = 60;
    /// A function after `|>` that does not accept the left side.
    pub(crate) const NOT_ACCEPTING: u8 = 70;
}

/// The kind of an item, as `CompletionItemKind` names it (X §4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum EntryKind {
    /// A function or a local function.
    Function,
    /// An interface function.
    Method,
    /// A field.
    Field,
    /// A local.
    Variable,
    /// A constant.
    Constant,
    /// A record type or a record's constructor.
    Struct,
    /// A type with variants.
    Enum,
    /// A variant's constructor.
    EnumMember,
    /// An interface.
    Interface,
    /// A module.
    Module,
    /// A type parameter, `Self`, or an alias.
    TypeParameter,
    /// A keyword, an attribute name, or `..`.
    Keyword,
    /// A snippet (X §4.6).
    Snippet,
    /// `_`.
    Value,
}

/// The text that replaces the partial word.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Insert {
    /// What it replaces: the partial word, or more for package B's edits
    /// (D-1394). It must sit on one line.
    pub range: TextRange,
    /// The new text.
    pub text: String,
    /// `text` is a snippet with tab stops, sent only to a client with
    /// `snippetSupport`.
    pub snippet: bool,
}

/// One completion item before protocol types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    /// The label: the name as the cursor's module writes it.
    pub label: String,
    /// The kind.
    pub kind: EntryKind,
    /// The rank, [`rank`].
    pub rank: u8,
    /// An order inside the rank, before the label.
    pub order: Option<u16>,
    /// The detail: the item's header (F §6.1), a local's or a field's
    /// type.
    pub detail: Option<String>,
    /// `labelDetails.detail`: a function's parameter names, `(xs, f)`.
    pub label_detail: Option<String>,
    /// `labelDetails.description`: the module an item comes from.
    pub description: Option<String>,
    /// The edit at the cursor.
    pub insert: Insert,
    /// Edits elsewhere, such as an import (D-1395), as `additionalTextEdits`.
    pub additional: Vec<(TextRange, String)>,
    /// `filterText`, when the label is not what the client should match.
    pub filter_text: Option<String>,
    /// The item, its constructor's type or its function's interface is
    /// deprecated.
    pub deprecated: bool,
    /// The item, for `completionItem/resolve` (D-1365).
    pub key: Option<Symbol>,
}

impl Entry {
    /// An entry with this label, kind and rank, and nothing else.
    pub(crate) fn new(label: impl Into<String>, kind: EntryKind, rank: u8, insert: Insert) -> Self {
        Self {
            label: label.into(),
            kind,
            rank,
            order: None,
            detail: None,
            label_detail: None,
            description: None,
            insert,
            additional: Vec::new(),
            filter_text: None,
            deprecated: false,
            key: None,
        }
    }

    /// `sortText`: the rank's two digits, the order's four if any, then
    /// the label (X §4.1).
    pub(crate) fn sort_text(&self) -> String {
        match self.order {
            Some(order) => format!("{:02}{order:04}{}", self.rank, self.label),
            None => format!("{:02}{}", self.rank, self.label),
        }
    }
}

/// The items of one request, as sources push them.
#[derive(Clone, Debug, Default)]
pub(crate) struct Items {
    entries: Vec<Entry>,
    /// The list was capped (X §4.4).
    pub incomplete: bool,
}

impl Items {
    /// Adds `entry`.
    pub(crate) fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
    }

    /// The items sorted by `sortText`, then label, kind, module and text
    /// (D-264), so two items of one name come by module path (X §4.4).
    /// An item is dropped only as a repeat of an earlier one with the same
    /// label, kind, module and text: two functions of one name from
    /// different modules both stay.
    pub(crate) fn finish(self) -> Completions {
        let mut entries = self.entries;
        entries.sort_by_cached_key(|e| {
            (
                e.sort_text(),
                e.label.clone(),
                e.kind,
                e.description.clone(),
                e.insert.text.clone(),
            )
        });
        let mut seen: BTreeSet<(String, EntryKind, Option<String>, String)> = BTreeSet::new();
        entries.retain(|e| {
            seen.insert((
                e.label.clone(),
                e.kind,
                e.description.clone(),
                e.insert.text.clone(),
            ))
        });
        Completions {
            items: entries,
            incomplete: self.incomplete,
        }
    }
}

/// Statement keywords (X §4.6).
const STATEMENT_KEYWORDS: &[&str] = &[
    "let", "use", "assert", "guard", "case", "panic", "todo", "fn",
];
/// Keywords that start an expression.
const EXPRESSION_KEYWORDS: &[&str] = &["case", "panic", "todo", "fn"];
/// Keywords that start an item. `exposed` needs `pub` before it (D-150).
const ITEM_KEYWORDS: &[&str] = &["pub", "priv", "fn", "type", "const", "interface", "test"];
/// Keywords after `pub`: `exposed` too, which only `pub` admits (D-150).
const AFTER_PUB: &[&str] = &["exposed", "fn", "type", "const", "interface"];
/// Keywords after `priv`.
const AFTER_PRIV: &[&str] = &["fn", "type", "const", "interface"];
/// Keywords after `exposed`, which only a type takes (D-121).
const AFTER_EXPOSED: &[&str] = &["type"];

/// The snippets of X §4.6, by label. Each is in `cheby fmt`'s form: two
/// spaces of indentation, an arm's body on its line, a non-empty `else`
/// block broken (D-1343).
const SNIPPETS: &[(&str, &str)] = &[
    ("case", "case ${1:subject} {\n  ${2:pattern} => $0\n}"),
    ("case {", "case {\n  ${1:condition} => $2\n  _ => $0\n}"),
    ("guard", "guard ${1:condition} else {\n  $0\n}"),
    (
        "let guard",
        "let guard ${1:Some(x)} = ${2:value} else {\n  $0\n}",
    ),
    ("use", "use ${1:x} <- ${2:result::try(value)}\n$0"),
    ("fn", "fn ${1:name}($2) -> ${3:Nil} {\n  $0\n}"),
    ("test", "test \"${1:name}\" {\n  assert $0\n}"),
    ("type", "type ${1:Name} {\n  $0\n}"),
];
/// The snippets of a statement.
const STATEMENT_SNIPPETS: &[&str] = &["case", "case {", "guard", "let guard", "use", "fn"];
/// The snippets of an expression.
const EXPRESSION_SNIPPETS: &[&str] = &["case", "case {"];
/// The snippets of an item.
const ITEM_SNIPPETS: &[&str] = &["fn", "test", "type"];
/// The snippets after `pub`, `priv` or `exposed`, among the keywords each
/// admits.
const AFTER_VISIBILITY_SNIPPETS: &[&str] = &["fn", "type"];

/// The attributes of §4.8, with their snippets: a name, then its argument
/// list when it takes one.
const ATTRIBUTES: &[(&str, &str)] = &[
    ("async", "async"),
    ("blocking", "blocking"),
    ("deprecated", "deprecated(\"$1\")$0"),
    ("external", "external($1)$0"),
    ("target", "target($1)$0"),
];
/// The attributes only package `std` may write (§4.8, D-482, D-692).
const STD_ATTRIBUTES: &[(&str, &str)] = &[
    ("std_clamped", "std_clamped($1)$0"),
    ("std_context", "std_context"),
    ("std_hooks", "std_hooks($1)$0"),
];

/// Pushes the items of `cx`'s context into `out`.
pub(crate) fn collect(cx: &CompletionCx<'_>, out: &mut Items) {
    // Before a path's `::`, only what a path starts with fits (D-1530).
    if cx.before_path() && path_starts(cx, out) {
        return;
    }
    // After `xs.`, nothing fits before a turbofish (D-1537).
    if cx.before_turbofish() && matches!(cx.found.context, Context::Dot { .. }) {
        return;
    }
    match &cx.found.context {
        Context::None => {}
        Context::ImportPath { prefix } => import_paths(cx, prefix, out),
        Context::ImportItems { module, written } => import_items(cx, module, written, out),
        Context::Attribute { inner } => attributes(cx, *inner, out),
        Context::Qualified {
            qualifier,
            ns,
            piped: Some(lhs),
        } => pipe::qualified(cx, *lhs, qualifier, *ns, out),
        Context::Qualified {
            qualifier,
            ns,
            piped: None,
        } => qualified(cx, qualifier, *ns, out),
        Context::Dot { receiver, dot } => dot::complete(cx, receiver, *dot, out),
        Context::Pipe { pipe, lhs } => pipe::complete(cx, *pipe, *lhs, out),
        Context::Type => types(cx, out),
        Context::Pattern { subject } => patterns(cx, *subject, out),
        Context::RecordField {
            record,
            written,
            empty,
        } => record_fields(cx, *record, written, *empty, out),
        Context::Interpolation => interpolation(cx, out),
        Context::Expr { statement, site } => expr(cx, *statement, *site, out),
        Context::Item { after_visibility } => item_keywords(cx, *after_visibility, out),
    }
}

/// Pushes what a path may start with where a path's `::` follows the
/// word, not a turbofish's, and says whether the context is one this
/// rule covers (D-1530). In an expression and after `|>`, the modules
/// and the interfaces, bare or through an imported module, as `Compare`
/// and `shape::Shape` (D-173), the interfaces only when one segment
/// follows the `::`, as in `Comp‸::compare(a, b)`, not in
/// `sha‸::Shape::area(x)` (D-1539). In a type or a pattern, the modules.
/// After `xs.`, nothing. Each is written as the file binds it, with no
/// `::` added, since the user typed it. Nothing is imported, so nothing
/// waits for a second character and the list is complete (D-1525).
fn path_starts(cx: &CompletionCx<'_>, out: &mut Items) -> bool {
    let interfaces = match &cx.found.context {
        Context::Expr { .. } | Context::Pipe { .. } => !cx.next.more_segments,
        Context::Type | Context::Pattern { .. } => false,
        Context::Dot { .. } => return true,
        _ => return false,
    };
    modules(cx, out);
    if interfaces {
        scope_interfaces(cx, out);
        auto_import::imported_interfaces(cx, out);
    }
    true
}

/// Whether an item of `kind` is a value an expression may name: a
/// function, a constant or a constructor. Before a turbofish only a
/// function or a constructor, since a constant takes no type arguments
/// (D-1536).
pub(crate) fn value_fits(cx: &CompletionCx<'_>, kind: DefKind) -> bool {
    match kind {
        DefKind::Fn | DefKind::Ctor => true,
        DefKind::Const => !cx.before_turbofish(),
        DefKind::Type
        | DefKind::Alias
        | DefKind::Interface
        | DefKind::InterfaceFn
        | DefKind::Test => false,
    }
}

/// Pushes the modules the module scope binds, aliases included, at rank
/// 30: written `list::`, so a path can follow, or `list` where a path's
/// `::` follows (D-1530). None before a turbofish (D-1525).
fn modules(cx: &CompletionCx<'_>, out: &mut Items) {
    let Some(resolved) = cx.resolved_view().and_then(ModuleView::resolved) else {
        return;
    };
    for (name, entry) in resolved.scope.iter(Namespace::Value) {
        let Res::Module(module) = entry.res else {
            continue;
        };
        let label = cx.name_text(name);
        let Some(text) = cx.module_text(label) else {
            continue;
        };
        let mut item = Entry::new(label, EntryKind::Module, rank::IMPORTED, cx.insert(text));
        let info = cx.snapshot.table().get(module);
        item.description = info.map(|i| i.path.to_string());
        item.key = info.map(|i| Symbol::of_module(i.path.clone()));
        out.push(item);
    }
}

/// Pushes the interfaces the module scope binds, a path's first segment
/// that names their functions (D-173): the module's own, those imported
/// by name and the prelude's, bare, ranked by where they come from. Each
/// is written with the `::` of its path, as `Show::`, the way a module is
/// written `shape::` (D-1546), or bare, as `Show`, where a path's `::`
/// follows (D-1530). The interfaces of the modules the file imports,
/// through the import's binding, come from [`auto_import`] (D-1507).
pub(crate) fn scope_interfaces(cx: &CompletionCx<'_>, out: &mut Items) {
    let Some(resolved) = cx.resolved_view().and_then(ModuleView::resolved) else {
        return;
    };
    for (name, entry) in resolved.scope.iter(Namespace::Type) {
        let Res::Def(def) = entry.res else { continue };
        if cx
            .item(def)
            .is_none_or(|item| item.kind != DefKind::Interface)
        {
            continue;
        }
        let label = cx.name_text(name);
        if let Some(mut built) = cx.def_entry(def, label, source_rank(entry.source)) {
            built.insert = cx.insert(cx.interface_text(label));
            out.push(built);
        }
    }
}

impl<'s> CompletionCx<'s> {
    /// The text of `name`.
    pub(crate) fn name_text(&self, name: Name) -> &'static str {
        self.snapshot.globals().names.get(name)
    }

    /// The item `def` of the run.
    pub(crate) fn item(&self, def: DefId) -> Option<&'s IfaceItem> {
        self.snapshot.interfaces().get(def.module)?.item(def.index)
    }

    /// `text` in place of the partial word.
    pub(crate) fn insert(&self, text: impl Into<String>) -> Insert {
        Insert {
            range: self.partial(),
            text: text.into(),
            snippet: false,
        }
    }

    /// `snippet` in place of the partial word for a client that takes
    /// snippets, else `plain`.
    pub(crate) fn snippet(&self, snippet: impl Into<String>, plain: impl Into<String>) -> Insert {
        if self.options.snippets {
            Insert {
                range: self.partial(),
                text: snippet.into(),
                snippet: true,
            }
        } else {
            self.insert(plain)
        }
    }

    /// Whether the token right after the partial word is `(`, so a call
    /// is already written, or `::`, as in `f::<Int>(x)` or `m::f`, where
    /// no argument list may go.
    pub(crate) fn before_paren(&self) -> bool {
        matches!(self.next.adjacent, Some(K::L_PAREN | K::COLON2))
    }

    /// Whether the token right after the partial word is a `::` that
    /// goes on with a path, as in `st‸::length`, not a turbofish's
    /// (D-1529): the word starts a path, so only a module or an interface
    /// fits, written bare (D-1521, D-1525, D-1530), an interface only when
    /// one segment follows (D-1539). After `m::`, only `m`'s interfaces
    /// fit (D-1538). A `::` after white space does not count, since
    /// `cheby fmt` never writes one.
    pub(crate) fn before_path(&self) -> bool {
        self.next.adjacent == Some(K::COLON2) && !self.next.turbofish
    }

    /// Whether the token right after the partial word is the `::` of a
    /// turbofish, as in `re‸::<Int>(xs)`: an item fits as with no `::`
    /// after it, but no module name, since `list::<Int>` is no path
    /// (E0042, D-1525), and only a name that takes type arguments: a
    /// function, a constructor or a local, no keyword, snippet or
    /// constant (D-1536).
    pub(crate) fn before_turbofish(&self) -> bool {
        self.next.adjacent == Some(K::COLON2) && self.next.turbofish
    }

    /// Whether the next token on the line is `kind`, as `:` after a field
    /// name or `{` after a constructor's.
    pub(crate) fn before(&self, kind: K) -> bool {
        self.next.on_line == Some(kind)
    }

    /// The text an interface named `label` writes as a path's segment:
    /// `label::`, so the list of its functions opens next, as after a
    /// module, or `label` where a path's `::` follows (D-1530, D-1538,
    /// D-1546, D-1548). No interface is offered before a turbofish
    /// (D-1536).
    pub(crate) fn interface_text(&self, label: &str) -> String {
        if self.before_path() {
            label.to_owned()
        } else {
            format!("{label}::")
        }
    }

    /// The text a module named `label` writes: `label::`, or `label` where
    /// a path's `::` follows. `None` before a turbofish, where no module
    /// name fits (D-1525).
    pub(crate) fn module_text(&self, label: &str) -> Option<String> {
        if self.before_turbofish() {
            None
        } else if self.before_path() {
            Some(label.to_owned())
        } else {
            Some(format!("{label}::"))
        }
    }

    /// A call of `name` with `params` as placeholders (X §4.1):
    /// `map(${1:xs}, ${2:f})$0`. Only the name when a `(` follows, and
    /// for a client with no snippets when there are parameters; `name()`
    /// when there are none.
    pub(crate) fn call_insert(&self, name: &str, params: &[&str]) -> Insert {
        if self.before_paren() {
            return self.insert(name);
        }
        if params.is_empty() {
            return self.insert(format!("{name}()"));
        }
        let holes: Vec<String> = params
            .iter()
            .zip(1_u32..)
            .map(|(param, n)| format!("${{{n}:{}}}", escape(param)))
            .collect();
        self.snippet(format!("{name}({})$0", holes.join(", ")), name)
    }

    /// The name the module binds to `module`, if it imports it.
    pub(crate) fn module_binding(&self, module: ModuleId) -> Option<&'static str> {
        let resolved = self.resolved_view()?.resolved()?;
        let names = resolved.scope.import_names();
        names.modules.get(&module).map(|&n| self.name_text(n))
    }

    /// `def`, a value, as the module writes it: by the name its scope binds
    /// to it, else qualified by the name it binds to the declaring module
    /// (`shapes::Circle`). `None` when neither exists.
    pub(crate) fn value_name(&self, def: DefId) -> Option<String> {
        let item = self.item(def)?;
        let resolved = self.resolved_view()?.resolved()?;
        let own = resolved.scope.get(Namespace::Value, item.name);
        if own.is_some_and(|entry| entry.res == Res::Def(def)) {
            return Some(self.name_text(item.name).to_owned());
        }
        let module = if def.module == ModuleId::BUILTIN {
            let &(_, segment) = builtin::HOMES.iter().find(|(home, _)| {
                *home == def
                    || item
                        .parent
                        .is_some_and(|p| *home == DefId::new(def.module, p))
            })?;
            self.snapshot.table().find(&builtin::home_path(segment))?
        } else {
            def.module
        };
        let binding = self.module_binding(module)?;
        Some(format!("{binding}::{}", self.name_text(item.name)))
    }

    /// The type parameters of `def`'s signature: a constructor's are its
    /// type's.
    pub(crate) fn generics(&self, def: DefId) -> Option<&'s Generics> {
        let iface = self.snapshot.interfaces().get(def.module)?;
        match &iface.item(def.index)?.sig {
            ItemSig::Fn(f) => Some(&f.generics),
            ItemSig::Type(t) => Some(&t.generics),
            ItemSig::Alias(a) => Some(&a.generics),
            ItemSig::Ctor(ctor) => match &iface.item(ctor.ty)?.sig {
                ItemSig::Type(t) => Some(&t.generics),
                _ => None,
            },
            ItemSig::Const(_)
            | ItemSig::Interface(_)
            | ItemSig::InterfaceFn(_)
            | ItemSig::Test
            | ItemSig::Error => None,
        }
    }

    /// `ty` for reading, with the type parameters of the body at the
    /// cursor, or of `def` when given.
    pub(crate) fn type_text(&self, ty: Ty, def: Option<DefId>) -> Option<String> {
        let printer = self.printer.as_ref()?;
        let def = def.or_else(|| {
            let view = self.view?;
            Some(DefId::new(view.id(), self.body()?.0))
        });
        let generics = def.and_then(|d| self.generics(d));
        Some(printer.type_display(ty, generics))
    }

    /// The entry of item `def` under `label`: its kind, its header as
    /// detail, its parameters and module as label details, its deprecation
    /// and its key. The edit writes `label`; a caller that writes a call
    /// replaces it.
    pub(crate) fn def_entry(&self, def: DefId, label: &str, rank: u8) -> Option<Entry> {
        let item = self.item(def)?;
        let mut entry = Entry::new(label, entry_kind(self, def, item), rank, self.insert(label));
        entry.detail = self.printer.as_ref().and_then(|p| p.item_header(def));
        if let ItemSig::Fn(f) = &item.sig {
            let params: Vec<&str> = f.params.iter().map(|p| self.name_text(p.name)).collect();
            entry.label_detail = Some(format!("({})", params.join(", ")));
        }
        let own = self.view.map(ModuleView::id);
        if def.module != ModuleId::BUILTIN && Some(def.module) != own {
            entry.description = self
                .snapshot
                .table()
                .get(def.module)
                .and_then(|info| info.path.segments().last().cloned());
        }
        let parent = item
            .parent
            .and_then(|p| self.item(DefId::new(def.module, p)));
        entry.deprecated =
            item.deprecated.is_some() || parent.is_some_and(|p| p.deprecated.is_some());
        entry.key = self.snapshot.key_of(def);
        Some(entry)
    }

    /// The entry of `item` of std's index, for a module the run did not
    /// load: its name, its signature as detail. The edit writes the name.
    pub(crate) fn std_entry(&self, item: &StdItem, rank: u8) -> Entry {
        let label = item.key.name.to_string();
        let kind = match item.kind {
            DefKind::Fn => EntryKind::Function,
            DefKind::Const => EntryKind::Constant,
            DefKind::Ctor => EntryKind::EnumMember,
            DefKind::Type => EntryKind::Enum,
            DefKind::Alias => EntryKind::TypeParameter,
            DefKind::Interface => EntryKind::Interface,
            DefKind::InterfaceFn => EntryKind::Method,
            DefKind::Test => EntryKind::Keyword,
        };
        let mut entry = Entry::new(label.clone(), kind, rank, self.insert(label));
        entry.detail = Some(item.signature.clone());
        entry.deprecated = item.deprecated.is_some();
        entry.key = Some(item.key.clone());
        entry
    }

    /// The type an expression at `site` should have (X §4.1's rank 00):
    /// a path's own expected type, a call's parameter, a `let`'s
    /// annotation, a `case`'s type at an arm's body, a field's type.
    pub(crate) fn expected(&self, site: ExprSite<'_>) -> Option<Ty> {
        let (_, facts) = self.body()?;
        let ty = match site {
            ExprSite::Path(node) => facts.expr(node.id())?.expected?,
            ExprSite::Arg { args, index } => {
                let call = args.parent().filter(|c| c.kind() == K::CALL_EXPR)?;
                let callee = call.first_child()?;
                *facts.call(callee.id())?.params.get(index)?
            }
            ExprSite::LetValue(stmt) => {
                stmt.first_token_of_kind(K::COLON)?;
                let pattern = stmt.children().next()?;
                facts.pat(pattern.id())?
            }
            ExprSite::ArmBody(arm) => {
                let case = arm.ancestors().find(|n| n.kind() == K::CASE_EXPR)?;
                let fact = facts.expr(case.id())?;
                fact.expected.unwrap_or(fact.ty)
            }
            ExprSite::FieldValue(init) => {
                let name = init.first_token()?;
                facts.field_at(name.text_range().start())?.ty?
            }
            ExprSite::Unknown => return None,
        };
        self.known(ty)
    }

    /// The nominal head of `ty`, if it has one.
    pub(crate) fn head(&self, ty: Ty) -> Option<DefId> {
        match self.snapshot.globals().types.kind(ty) {
            TyKind::Named { def, .. } => Some(*def),
            _ => None,
        }
    }

    /// Whether a value of type `ty` fits `expected`: the same type, or the
    /// same nominal head.
    fn fits(&self, expected: Option<Ty>, ty: Option<Ty>) -> bool {
        let (Some(expected), Some(ty)) = (expected, ty) else {
            return false;
        };
        ty == expected
            || self
                .head(ty)
                .is_some_and(|h| self.head(expected) == Some(h))
    }

    /// The type a use of item `def` gives, for rank 00: a function's
    /// result, a constant's type, a constructor's type as its head.
    fn value_ty(&self, def: DefId) -> Option<Ty> {
        match &self.item(def)?.sig {
            ItemSig::Fn(f) => Some(f.ret),
            ItemSig::Const(c) => Some(c.ty),
            _ => None,
        }
    }

    /// Whether item `def`, a constructor, builds a value of `expected`'s
    /// head.
    fn ctor_fits(&self, def: DefId, expected: Option<Ty>) -> bool {
        let Some(head) = expected.and_then(|e| self.head(e)) else {
            return false;
        };
        let Some(item) = self.item(def) else {
            return false;
        };
        item.kind == DefKind::Ctor && item.parent.map(|p| DefId::new(def.module, p)) == Some(head)
    }

    /// Whether `name` could be bound by a new pattern at the cursor with no
    /// reuse (D-051, E0044): no local of the body has it, and no item or
    /// import of the module. The test of `scope::is_free`.
    fn is_free_name(&self, name: &str) -> bool {
        let Some(view) = self.resolved_view() else {
            return true;
        };
        let Some(name) = self.snapshot.globals().names.lookup(name) else {
            return true;
        };
        let in_body = scope::body_at(view, self.offset)
            .and_then(|item| view.resolved()?.bodies.get(&item))
            .is_some_and(|body| body.locals.iter().any(|decl| decl.name == name));
        let in_module = view
            .resolved()
            .and_then(|r| r.scope.get(Namespace::Value, name))
            .is_some_and(|entry| {
                matches!(entry.source, ScopeSource::Item | ScopeSource::Import(_))
                    && !matches!(entry.res, Res::Err)
            });
        !in_body && !in_module
    }
}

/// `text` with the characters a snippet placeholder treats specially
/// escaped: `$`, `}` and `\`.
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '$' | '}' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The kind of item `def`.
fn entry_kind(cx: &CompletionCx<'_>, def: DefId, item: &IfaceItem) -> EntryKind {
    match (&item.kind, &item.sig) {
        (DefKind::Fn, _) => EntryKind::Function,
        (DefKind::InterfaceFn, _) => EntryKind::Method,
        (DefKind::Const, _) => EntryKind::Constant,
        (DefKind::Interface, _) => EntryKind::Interface,
        (DefKind::Alias, _) => EntryKind::TypeParameter,
        // A record, or a type with no variants such as `Int`, is a
        // struct; a type with variants an enum.
        (DefKind::Type, ItemSig::Type(ty)) => match &ty.kind {
            TypeDefKind::Adt { .. } if !is_record(cx, def.module, item.name, &ty.kind) => {
                EntryKind::Enum
            }
            TypeDefKind::Adt { .. } | TypeDefKind::Builtin(_) | TypeDefKind::External => {
                EntryKind::Struct
            }
        },
        (DefKind::Ctor, ItemSig::Ctor(ctor)) => {
            let parent = cx.item(DefId::new(def.module, ctor.ty));
            match parent.map(|p| &p.sig) {
                Some(ItemSig::Type(ty)) if is_record(cx, def.module, item.name, &ty.kind) => {
                    EntryKind::Struct
                }
                _ => EntryKind::EnumMember,
            }
        }
        (DefKind::Type | DefKind::Ctor, _) => EntryKind::Enum,
        (DefKind::Test, _) => EntryKind::Keyword,
    }
}

/// Whether a type with this definition is a record: one constructor that
/// has the type's name (§4.3.2).
fn is_record(cx: &CompletionCx<'_>, module: ModuleId, name: Name, kind: &TypeDefKind) -> bool {
    match kind {
        TypeDefKind::Adt { ctors } => {
            ctors.len() == 1
                && ctors.iter().all(|&c| {
                    cx.item(DefId::new(module, c))
                        .is_some_and(|ctor| ctor.name == name)
                })
        }
        TypeDefKind::Builtin(_) | TypeDefKind::External => false,
    }
}

/// Pushes the keywords of `keywords` and the snippets of `snippets`: with
/// snippets, a snippet stands for the keyword of its label, else the
/// keyword stands (X §4.6).
/// The keywords and snippets of an item: after a visibility keyword, those
/// that may follow it, else those that start an item, with `import` only
/// before the first item (D-143).
fn item_keywords(cx: &CompletionCx<'_>, after_visibility: bool, out: &mut Items) {
    if after_visibility {
        let keywords = match cx.found.anchor.map(SyntaxToken::kind) {
            Some(K::PUB_KW) => AFTER_PUB,
            Some(K::EXPOSED_KW) => AFTER_EXPOSED,
            _ => AFTER_PRIV,
        };
        let snippets: Vec<&str> = AFTER_VISIBILITY_SNIPPETS
            .iter()
            .copied()
            .filter(|snippet| keywords.contains(snippet))
            .collect();
        keywords_and_snippets(cx, keywords, &snippets, out);
        return;
    }
    keywords_and_snippets(cx, ITEM_KEYWORDS, ITEM_SNIPPETS, out);
    let start = cx.found.partial.start();
    let item_before = cx.tree.root().children().any(|child| {
        matches!(
            child.kind(),
            K::FN_DECL
                | K::TYPE_DECL
                | K::ALIAS_DECL
                | K::CONST_DECL
                | K::INTERFACE_DECL
                | K::TEST_DECL
        ) && child.text_range().end() <= start
    });
    if !item_before {
        keywords_and_snippets(cx, &["import"], &[], out);
    }
}

fn keywords_and_snippets(
    cx: &CompletionCx<'_>,
    keywords: &[&str],
    snippets: &[&str],
    out: &mut Items,
) {
    let with_snippets = cx.options.snippets;
    for &keyword in keywords {
        if with_snippets && snippets.contains(&keyword) {
            continue;
        }
        out.push(Entry::new(
            keyword,
            EntryKind::Keyword,
            rank::KEYWORD,
            cx.insert(keyword),
        ));
    }
    if !with_snippets {
        return;
    }
    for &(label, body) in SNIPPETS {
        if snippets.contains(&label) {
            out.push(Entry::new(
                label,
                EntryKind::Snippet,
                rank::KEYWORD,
                cx.snippet(body, label),
            ));
        }
    }
}

/// An expression (X §4.1): locals, module values, imported values and
/// modules, the interfaces in scope, written `Show::` (D-1546), the
/// prelude, keywords and snippets, the items of imported modules
/// (D-1507), and names to import, interfaces among them (D-1546).
fn expr(cx: &CompletionCx<'_>, statement: bool, site: ExprSite<'_>, out: &mut Items) {
    let site = match site {
        ExprSite::Unknown => partial_path(cx).map_or(site, ExprSite::Path),
        other => other,
    };
    let expected = if cx.options.type_driven {
        cx.expected(site)
    } else {
        None
    };
    let as_value = expected.is_some_and(|ty| is_fn(cx, ty));
    locals(cx, expected, false, out);
    scope_values(cx, expected, as_value, out);
    // Before a turbofish only names fit, so no keyword, snippet or
    // interface, and `scope_values` and `auto_import::complete` give no
    // constant, no module and no interface (D-1536, D-1546).
    if !cx.before_turbofish() {
        scope_interfaces(cx, out);
        let (keywords, snippets) = if statement {
            (STATEMENT_KEYWORDS, STATEMENT_SNIPPETS)
        } else {
            (EXPRESSION_KEYWORDS, EXPRESSION_SNIPPETS)
        };
        keywords_and_snippets(cx, keywords, snippets, out);
    }
    // The items of other modules (D-1507, X §4.4), by the rule
    // `scope_values` writes by: a function's name where a function is
    // expected, so an imported name is no call either.
    auto_import::complete(cx, Namespace::Value, as_value, out);
}

/// Whether `ty` is a function type, so a position that expects it takes a
/// function's name, not a call (X §4.1).
fn is_fn(cx: &CompletionCx<'_>, ty: Ty) -> bool {
    matches!(cx.snapshot.globals().types.kind(ty), TyKind::Fn { .. })
}

/// Whether the qualified path at the cursor sits where a function value
/// is expected, as `list::map(xs, string::len‸)`: by the fact of its
/// `PATH_EXPR`, or of the path before `::` when nothing is typed yet.
fn qualified_as_value(cx: &CompletionCx<'_>) -> bool {
    if !cx.options.type_driven {
        return false;
    }
    let path = partial_path(cx).or_else(|| {
        let colon2 = cx.found.anchor.filter(|t| t.kind() == K::COLON2)?;
        prev_significant(colon2)?
            .parent()
            .filter(|p| p.kind() == K::PATH)?
            .parent()
            .filter(|p| p.kind() == K::PATH_EXPR)
    });
    path.and_then(|p| cx.expected(ExprSite::Path(p)))
        .is_some_and(|ty| is_fn(cx, ty))
}

/// The `PATH_EXPR` of the partial word, if it is one: its fact may hold
/// the expected type.
fn partial_path<'s>(cx: &CompletionCx<'s>) -> Option<SyntaxNode<'s>> {
    let partial = cx.partial();
    if partial.is_empty() {
        return None;
    }
    let token = cx.tree.root().token_at_offset(partial.start())?;
    token
        .parent()?
        .parent()
        .filter(|n| n.kind() == K::PATH_EXPR)
}

/// Pushes the locals visible at the cursor, nearest binding first
/// (X §4.1), with `expected` ranking a fitting one 00. A local whose
/// binding statement holds the cursor is not visible there (D-1383), and
/// names the compiler made are left out. With `functions_only`, only
/// locals of function type, as after `|>` (X §4.3).
pub(crate) fn locals(
    cx: &CompletionCx<'_>,
    expected: Option<Ty>,
    functions_only: bool,
    out: &mut Items,
) {
    let Some(view) = cx.resolved_view() else {
        return;
    };
    let visible = scope::locals_at(view, cx.offset);
    for (local, order) in visible.iter().zip(0_u16..) {
        let name = cx.name_text(local.decl.name);
        if !is_plain_name(name) {
            continue;
        }
        let ty = scope::local_fact(view, local.item, local.slot).and_then(|l| cx.known(l.ty));
        let is_fn =
            ty.is_some_and(|t| matches!(cx.snapshot.globals().types.kind(t), TyKind::Fn { .. }));
        if functions_only && !is_fn {
            continue;
        }
        let kind = if local.decl.kind == cheby_driver::analysis::LocalKind::LocalFn {
            EntryKind::Function
        } else {
            EntryKind::Variable
        };
        let rank = if cx.fits(expected, ty) {
            rank::EXPECTED
        } else {
            rank::LOCAL
        };
        let mut entry = Entry::new(name, kind, rank, cx.insert(name));
        entry.order = Some(order);
        entry.detail = ty.and_then(|t| cx.type_text(t, Some(DefId::new(view.id(), local.item))));
        out.push(entry);
    }
}

/// Whether `name` is a name a user wrote, not `_` or one the compiler
/// made.
fn is_plain_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
        && name != "_"
}

/// The rank of a module-scope name by where it comes from (X §4.1).
fn source_rank(source: ScopeSource) -> u8 {
    match source {
        ScopeSource::Item => rank::MODULE,
        ScopeSource::Import(_) => rank::IMPORTED,
        ScopeSource::Prelude | ScopeSource::HomeReexport | ScopeSource::Withdrawn => rank::PRELUDE,
    }
}

/// The values of the module scope: the module's functions, constants and
/// constructors, imported values and modules, and the prelude's. Before a
/// turbofish, no constant and no module (D-1536). A function writes a
/// call unless `as_value` or a `(` follows.
fn scope_values(cx: &CompletionCx<'_>, expected: Option<Ty>, as_value: bool, out: &mut Items) {
    let Some(resolved) = cx.resolved_view().and_then(ModuleView::resolved) else {
        return;
    };
    for (name, entry) in resolved.scope.iter(Namespace::Value) {
        let label = cx.name_text(name);
        match entry.res {
            Res::Def(def) => {
                let Some(item) = cx.item(def) else { continue };
                if !value_fits(cx, item.kind) {
                    continue;
                }
                let fits = cx.fits(expected, cx.value_ty(def)) || cx.ctor_fits(def, expected);
                let rank = if fits {
                    rank::EXPECTED
                } else {
                    source_rank(entry.source)
                };
                let Some(mut built) = cx.def_entry(def, label, rank) else {
                    continue;
                };
                built.insert = value_insert(cx, label, item, as_value);
                out.push(built);
            }
            Res::Module(_) | Res::Local(_) | Res::TyParam(_) | Res::SelfTy | Res::Err => {}
        }
    }
    modules(cx, out);
}

/// What a value item writes in an expression: a function a call (X §4.1),
/// a positional constructor its argument list, anything else its name.
fn value_insert(cx: &CompletionCx<'_>, label: &str, item: &IfaceItem, as_value: bool) -> Insert {
    if as_value {
        return cx.insert(label);
    }
    match &item.sig {
        ItemSig::Fn(f) => {
            let params: Vec<&str> = f.params.iter().map(|p| cx.name_text(p.name)).collect();
            cx.call_insert(label, &params)
        }
        ItemSig::Ctor(ctor) => match &ctor.fields {
            CtorFields::Positional(fields) if !cx.before_paren() => {
                let stops: Vec<String> = (1..=fields.len()).map(|n| format!("${n}")).collect();
                cx.snippet(format!("{label}({})$0", stops.join(", ")), label)
            }
            CtorFields::Positional(_) | CtorFields::Unit | CtorFields::Named(_) => cx.insert(label),
        },
        _ => cx.insert(label),
    }
}

/// A type (X §4.1): type parameters and `Self` in scope, the module
/// scope's types, modules for `m::`, `dyn`, the types of imported modules
/// (D-1507), and names to import. Before a turbofish, which no type takes,
/// the names stay as D-1525 has them, with no module and no `dyn`
/// (D-1536, D-1544).
fn types(cx: &CompletionCx<'_>, out: &mut Items) {
    type_params(cx, out);
    if let Some(resolved) = cx.resolved_view().and_then(ModuleView::resolved) {
        for (name, entry) in resolved.scope.iter(Namespace::Type) {
            let Res::Def(def) = entry.res else { continue };
            let label = cx.name_text(name);
            if let Some(built) = cx.def_entry(def, label, source_rank(entry.source)) {
                out.push(built);
            }
        }
        for (name, entry) in resolved.scope.iter(Namespace::Value) {
            let Res::Module(module) = entry.res else {
                continue;
            };
            let label = cx.name_text(name);
            let Some(text) = cx.module_text(label) else {
                continue;
            };
            let mut item = Entry::new(label, EntryKind::Module, rank::IMPORTED, cx.insert(text));
            item.description = cx.snapshot.table().get(module).map(|i| i.path.to_string());
            out.push(item);
        }
    }
    // Before a turbofish no keyword fits, so no `dyn` (D-1536, D-1544).
    if !cx.before_turbofish() {
        out.push(Entry::new(
            "dyn",
            EntryKind::Keyword,
            rank::KEYWORD,
            cx.insert("dyn"),
        ));
    }
    auto_import::complete(cx, Namespace::Type, false, out);
}

/// The type parameters of the items around the cursor, read from the
/// tree, and `Self` inside an interface.
fn type_params(cx: &CompletionCx<'_>, out: &mut Items) {
    // The anchor, since at the end of the file the token at the cursor
    // is the file's trailing white space, outside every item.
    let Some(token) = cx
        .found
        .anchor
        .or_else(|| cx.tree.root().token_at_offset(cx.partial().start()))
    else {
        return;
    };
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for node in token.parent().into_iter().flat_map(SyntaxNode::ancestors) {
        if node.kind() == K::INTERFACE_DECL && seen.insert("Self".to_owned()) {
            out.push(Entry::new(
                "Self",
                EntryKind::TypeParameter,
                rank::LOCAL,
                cx.insert("Self"),
            ));
        }
        let Some(list) = node.first_child_of_kind(K::TYPE_PARAM_LIST) else {
            continue;
        };
        for param in list.children().filter(|c| c.kind() == K::TYPE_PARAM) {
            let Some(name) = param.first_token_of_kind(K::UPPER_IDENT) else {
                continue;
            };
            if seen.insert(name.text().to_owned()) {
                out.push(Entry::new(
                    name.text(),
                    EntryKind::TypeParameter,
                    rank::LOCAL,
                    cx.insert(name.text()),
                ));
            }
        }
    }
}

/// A pattern (X §4.1): with a known subject, its constructors first,
/// written with their fields, then `_`. Otherwise the constructors in
/// scope and `_`. Before a turbofish, no `_` (D-1536, D-1544).
fn patterns(cx: &CompletionCx<'_>, subject: Subject<'_>, out: &mut Items) {
    let subject_ty = match subject {
        Subject::Case(case) => {
            let subjects: Vec<SyntaxNode<'_>> = case
                .children()
                .take_while(|c| c.kind() != K::CASE_BODY)
                .collect();
            match subjects.as_slice() {
                [one] => cx.expr_ty(*one),
                _ => None,
            }
        }
        Subject::Let(stmt) => let_value(stmt).and_then(|value| cx.expr_ty(value)),
        Subject::Unknown => None,
    };
    let ctors = subject_ty
        .and_then(|ty| cx.head(ty))
        .map(|head| ctors_of(cx, head))
        .unwrap_or_default();
    if ctors.is_empty() {
        scope_ctors(cx, out);
    } else {
        for (def, order) in ctors.into_iter().zip(0_u16..) {
            let Some(label) = cx.value_name(def) else {
                continue;
            };
            let Some(mut entry) = cx.def_entry(def, &label, rank::EXPECTED) else {
                continue;
            };
            entry.order = Some(order);
            entry.insert = pattern_insert(cx, def, &label);
            out.push(entry);
        }
    }
    // Before a turbofish only a name fits, and `_` is none (D-1536,
    // D-1544).
    if !cx.before_turbofish() {
        out.push(Entry::new(
            "_",
            EntryKind::Value,
            rank::KEYWORD,
            cx.insert("_"),
        ));
    }
}

/// The value of `stmt`, a `LET_STMT`: the node after its `=`, so `v` in
/// `let p: T = v`, not the annotation `T`, and in `let guard p = v else
/// { … }` and `let assert p = v` (D-1531).
fn let_value(stmt: SyntaxNode<'_>) -> Option<SyntaxNode<'_>> {
    let eq = stmt.first_token_of_kind(K::EQ)?.text_range().end();
    stmt.children()
        .find(|child| child.text_range().start() >= eq)
}

/// The constructors of the type `head` the module may use, in
/// declaration order. Empty for a type whose structure is hidden here.
fn ctors_of(cx: &CompletionCx<'_>, head: DefId) -> Vec<DefId> {
    let Some(iface) = cx.snapshot.interfaces().get(head.module) else {
        return Vec::new();
    };
    let Some(item) = iface.item(head.index) else {
        return Vec::new();
    };
    let ItemSig::Type(ty) = &item.sig else {
        return Vec::new();
    };
    if !structure_seen(cx, head, item) {
        return Vec::new();
    }
    match &ty.kind {
        TypeDefKind::Adt { ctors } => ctors.iter().map(|&c| DefId::new(head.module, c)).collect(),
        TypeDefKind::Builtin(_) | TypeDefKind::External => Vec::new(),
    }
}

/// Whether the module at the cursor may see the structure of the type
/// `def`, whose item is `item` (§4.3.4, D-048).
fn structure_seen(cx: &CompletionCx<'_>, def: DefId, item: &IfaceItem) -> bool {
    let Some(view) = cx.view else {
        return false;
    };
    if def.module == view.id() || def.module == ModuleId::BUILTIN {
        return true;
    }
    let table = cx.snapshot.table();
    let (Some(theirs), Some(ours)) = (table.get(def.module), table.get(view.id())) else {
        return false;
    };
    structure_visible(item, def.module, theirs.package, view.id(), ours.package)
}

/// The constructors the module scope binds, ranked by where they come
/// from.
fn scope_ctors(cx: &CompletionCx<'_>, out: &mut Items) {
    let Some(resolved) = cx.resolved_view().and_then(ModuleView::resolved) else {
        return;
    };
    for (name, entry) in resolved.scope.iter(Namespace::Value) {
        let Res::Def(def) = entry.res else { continue };
        if cx.item(def).is_none_or(|item| item.kind != DefKind::Ctor) {
            continue;
        }
        let label = cx.name_text(name);
        if let Some(mut built) = cx.def_entry(def, label, source_rank(entry.source)) {
            built.insert = pattern_insert(cx, def, label);
            out.push(built);
        }
    }
}

/// A constructor `def` written as a pattern under `label` (X §4.1):
/// `None`, `Some(_)`, `Circle { radius }`. A field whose name is taken at
/// the cursor is written `radius: _`, since a punned field binds its name
/// (D-051). Only `label` where its `(…)` or `{…}` follows.
fn pattern_insert(cx: &CompletionCx<'_>, def: DefId, label: &str) -> Insert {
    let Some(ItemSig::Ctor(ctor)) = cx.item(def).map(|i| &i.sig) else {
        return cx.insert(label);
    };
    match &ctor.fields {
        CtorFields::Unit => cx.insert(label),
        // The fields are already written.
        CtorFields::Positional(_) if cx.before_paren() => cx.insert(label),
        CtorFields::Named(_) if cx.before(K::L_BRACE) => cx.insert(label),
        CtorFields::Positional(fields) => {
            let plain: Vec<&str> = fields.iter().map(|_| "_").collect();
            let stops: Vec<String> = (1..=fields.len()).map(|n| format!("${{{n}:_}}")).collect();
            cx.snippet(
                format!("{label}({})", stops.join(", ")),
                format!("{label}({})", plain.join(", ")),
            )
        }
        CtorFields::Named(fields) => {
            let parts: Vec<String> = fields
                .iter()
                .map(|&(name, _)| {
                    let text = cx.name_text(name);
                    if cx.is_free_name(text) {
                        text.to_owned()
                    } else {
                        format!("{text}: _")
                    }
                })
                .collect();
            cx.insert(format!("{label} {{ {} }}", parts.join(", ")))
        }
    }
}

/// The fields of a record literal not yet written, as `name: ` (X §4.1),
/// and `..` when nothing is written, so an update may start.
fn record_fields(
    cx: &CompletionCx<'_>,
    record: SyntaxNode<'_>,
    written: &[String],
    empty: bool,
    out: &mut Items,
) {
    if let Some((owner, fields)) = record_def(cx, record) {
        for ((name, ty), order) in fields.into_iter().zip(0_u16..) {
            let label = cx.name_text(name);
            if written.iter().any(|w| w == label) {
                continue;
            }
            let mut entry = Entry::new(
                label,
                EntryKind::Field,
                rank::FIELD,
                if cx.before(K::COLON) {
                    cx.insert(label)
                } else {
                    cx.insert(format!("{label}: "))
                },
            );
            entry.order = Some(order);
            entry.detail = cx.type_text(ty, Some(owner));
            out.push(entry);
        }
    }
    if empty {
        out.push(Entry::new(
            "..",
            EntryKind::Keyword,
            rank::KEYWORD,
            cx.insert(".."),
        ));
    }
}

/// The type a record literal builds, with its named fields over all its
/// constructors, in first-seen order. `None` when its path does not
/// resolve or its structure is hidden here.
fn record_def(cx: &CompletionCx<'_>, record: SyntaxNode<'_>) -> Option<(DefId, Vec<(Name, Ty)>)> {
    let path = record.first_child_of_kind(K::PATH)?;
    let view = cx.resolved_view()?;
    let resolved = view.resolved()?;
    let from_body = scope::body_at(view, path.text_range().start())
        .and_then(|item| resolved.bodies.get(&item))
        .and_then(|body| body.paths.get(&path.id()).copied());
    let res = from_body.or_else(|| {
        let names: Vec<_> = path
            .children_with_tokens()
            .filter_map(cheby_syntax::SyntaxElement::as_token)
            .filter(|t| matches!(t.kind(), K::UPPER_IDENT | K::LOWER_IDENT))
            .collect();
        let [only] = names.as_slice() else {
            return None;
        };
        let name = cx.snapshot.globals().names.lookup(only.text())?;
        resolved.scope.get(Namespace::Value, name).map(|e| e.res)
    })?;
    let Res::Def(def) = res else { return None };
    let item = cx.item(def)?;
    let ty = match item.kind {
        DefKind::Ctor => DefId::new(def.module, item.parent?),
        DefKind::Type => def,
        _ => return None,
    };
    let ty_item = cx.item(ty)?;
    if !structure_seen(cx, ty, ty_item) {
        return None;
    }
    let ctors = if item.kind == DefKind::Ctor {
        vec![def]
    } else {
        ctors_of(cx, ty)
    };
    let mut fields: Vec<(Name, Ty)> = Vec::new();
    for ctor in ctors {
        if let Some(ItemSig::Ctor(sig)) = cx.item(ctor).map(|i| &i.sig)
            && let CtorFields::Named(named) = &sig.fields
        {
            for &(name, field_ty) in named {
                if !fields.iter().any(|&(n, _)| n == name) {
                    fields.push((name, field_ty));
                }
            }
        }
    }
    Some((ty, fields))
}

/// The root of an interpolation (X §4.1): locals and constants only,
/// since an interpolation holds a name (E0012).
fn interpolation(cx: &CompletionCx<'_>, out: &mut Items) {
    locals(cx, None, false, out);
    let Some(resolved) = cx.resolved_view().and_then(ModuleView::resolved) else {
        return;
    };
    for (name, entry) in resolved.scope.iter(Namespace::Value) {
        let Res::Def(def) = entry.res else { continue };
        if cx.item(def).is_none_or(|item| item.kind != DefKind::Const) {
            continue;
        }
        if let Some(built) = cx.def_entry(def, cx.name_text(name), source_rank(entry.source)) {
            out.push(built);
        }
    }
}

/// A name after `m::`, `I::` or `m::I::` (X §4.1): the visible items of
/// the module in the path's namespace, with the builtin items homed there,
/// or the functions of the interface. A module the run did not load reads
/// std's index. In an expression, a module's interfaces come too, written
/// `Shape::`, so `shape::Shape::area(x)` (D-173) completes one segment at
/// a time (D-1548), and they alone, written `Shape`, where a path's `::`
/// follows (D-1538). In a type or a pattern nothing is offered there,
/// since a type's or a constructor's path has one qualifier (E0013,
/// D-1547). Before a turbofish, an expression gets only a module's
/// functions and constructors, and an interface's functions are not
/// offered, since they take no type arguments (E0023, D-1536). A type and
/// a pattern keep their items there, as D-1525 has it.
fn qualified(cx: &CompletionCx<'_>, qualifier: &[String], ns: PathNs, out: &mut Items) {
    // A third segment never follows in a type or a pattern (D-1547).
    if ns != PathNs::Value && cx.before_path() {
        return;
    }
    if let Some(def) = path_interface(cx, qualifier) {
        // An interface's function takes no type arguments (E0023, D-1536).
        if cx.before_turbofish() {
            return;
        }
        return interface_items(cx, def, false, out);
    }
    let [one] = qualifier else { return };
    let Some(module) = qualified_module(cx, one) else {
        return;
    };
    module_items(cx, module, ns, out);
}

/// The module the module scope binds to `text`, as the first segment of a
/// path.
pub(crate) fn qualified_module(cx: &CompletionCx<'_>, text: &str) -> Option<ModuleId> {
    let resolved = cx.resolved_view().and_then(ModuleView::resolved)?;
    let name = cx.snapshot.globals().names.lookup(text)?;
    match resolved.scope.get(Namespace::Value, name)?.res {
        Res::Module(module) => Some(module),
        _ => None,
    }
}

/// The interface `qualifier` names, `I` or `m::I`, if it names one: the
/// segments before the partial word of `I::‸` or `m::I::‸` (D-173).
pub(crate) fn path_interface(cx: &CompletionCx<'_>, qualifier: &[String]) -> Option<DefId> {
    let resolved = cx.resolved_view().and_then(ModuleView::resolved)?;
    let lookup = |ns: Namespace, text: &str| {
        let name = cx.snapshot.globals().names.lookup(text)?;
        resolved.scope.get(ns, name).map(|e| e.res)
    };
    let module_of = |text: &str| qualified_module(cx, text);
    let interface = match qualifier {
        [one] if one.starts_with(|c: char| c.is_ascii_uppercase()) => {
            match lookup(Namespace::Type, one) {
                Some(Res::Def(def)) => Some(def),
                _ => None,
            }
        }
        [module, one] => module_of(module).and_then(|m| {
            let name = cx.snapshot.globals().names.lookup(one)?;
            let index = cx
                .snapshot
                .interfaces()
                .get(m)?
                .lookup(Namespace::Type, name)?;
            Some(DefId::new(m, index))
        }),
        _ => None,
    }?;
    cx.item(interface)
        .is_some_and(|item| item.kind == DefKind::Interface)
        .then_some(interface)
}

/// The functions of the interface `def`, embedded ones included. With
/// `piped`, as after `xs |> Show::`, each is written in the pipe form,
/// with the hole `_` at its first parameter of type `Self`, which the
/// left side fills (D-173), and bare when that is its only parameter
/// (D-244, D-245), as a module's functions are written there; one with
/// no `Self` parameter takes no pipe and is left out. One whose `Self`
/// comes later ranks five later (X §4.3, D-1545). Where a function value
/// is expected, as in `list::map(xs, Show::‸)`, each is written bare, as
/// a module's functions are, since `I::f` is a value there (X §4.1,
/// D-500).
pub(crate) fn interface_items(cx: &CompletionCx<'_>, def: DefId, piped: bool, out: &mut Items) {
    let as_value = !piped && qualified_as_value(cx);
    let set = cx.snapshot.interfaces();
    let Ok(fns) = interface_fns(def, &|d| set.get(d.module)?.item(d.index)) else {
        return;
    };
    let types = &cx.snapshot.globals().types;
    for func in fns {
        let Some(item) = cx.item(func) else { continue };
        let ItemSig::InterfaceFn(sig) = &item.sig else {
            continue;
        };
        let label = cx.name_text(item.name);
        // `Self` is the one type parameter an interface function's
        // signature names (`InterfaceFnSig` has no generics of its own).
        let hole = sig
            .params
            .iter()
            .position(|&param| matches!(types.kind(param), TyKind::Param(_)));
        let rank = match (piped, hole) {
            (false, _) | (true, Some(0)) => rank::MODULE,
            (true, Some(_)) => rank::MODULE.saturating_add(pipe::ranks::LATER),
            (true, None) => continue,
        };
        let Some(mut entry) = cx.def_entry(func, label, rank) else {
            continue;
        };
        entry.insert = if cx.before_paren() || as_value || (piped && sig.params.len() == 1) {
            cx.insert(label)
        } else {
            let mut stop = 0_usize;
            let args: Vec<String> = (0..sig.params.len())
                .map(|k| {
                    if piped && Some(k) == hole {
                        "_".to_owned()
                    } else {
                        stop = stop.saturating_add(1);
                        format!("${stop}")
                    }
                })
                .collect();
            cx.snippet(format!("{label}({})$0", args.join(", ")), label)
        };
        out.push(entry);
    }
}

/// The items of `module` that the cursor's module may name in `ns`. In an
/// expression, its interfaces too, which name their functions, written
/// `Shape::` (D-1548), and only they, written `Shape`, where a path's
/// `::` follows (D-1538). Before a turbofish, no interface and no
/// constant (D-1536).
fn module_items(cx: &CompletionCx<'_>, module: ModuleId, ns: PathNs, out: &mut Items) {
    let fits = |kind: DefKind| match ns {
        PathNs::Value if cx.before_path() => kind == DefKind::Interface,
        PathNs::Value => {
            value_fits(cx, kind) || (kind == DefKind::Interface && !cx.before_turbofish())
        }
        PathNs::Type => matches!(kind, DefKind::Type | DefKind::Alias | DefKind::Interface),
        PathNs::Pattern => kind == DefKind::Ctor,
    };
    module_defs(cx, module, ns, &fits, out);
}

/// Pushes the interfaces of `module` after `xs |> shape::`, written
/// `Shape::` (D-1548), or `Shape` where a path's `::` follows (D-1538).
pub(crate) fn module_interfaces(cx: &CompletionCx<'_>, module: ModuleId, out: &mut Items) {
    module_defs(
        cx,
        module,
        PathNs::Value,
        &|kind| kind == DefKind::Interface,
        out,
    );
}

/// Pushes the items of `module` of the kinds `fits` takes that the
/// cursor's module may name, written as `ns` writes them.
fn module_defs(
    cx: &CompletionCx<'_>,
    module: ModuleId,
    ns: PathNs,
    fits: &dyn Fn(DefKind) -> bool,
    out: &mut Items,
) {
    let Some(path) = cx.snapshot.table().get(module).map(|i| i.path.clone()) else {
        return;
    };
    let Some(iface) = cx.snapshot.interfaces().get(module) else {
        if let Some(index) = cx.std.get() {
            let text = path.to_string();
            for item in index
                .items
                .iter()
                .filter(|i| i.module == text && fits(i.kind))
            {
                out.push(cx.std_entry(item, rank::MODULE));
            }
        }
        return;
    };
    let Some(package) = cx
        .view
        .and_then(|v| cx.snapshot.table().get(v.id()))
        .map(|i| i.package)
    else {
        return;
    };
    let mut defs: Vec<DefId> = iface
        .items
        .iter_enumerated()
        .filter(|&(index, item)| {
            fits(item.kind)
                && iface.visible_from(index, package)
                && iface.is_current(index)
                && !iface.is_hidden(index)
        })
        .map(|(index, _)| DefId::new(module, index))
        .collect();
    for homed in builtin::homed_at(&path) {
        defs.push(homed);
        if let Some(ItemSig::Type(ty)) = cx.item(homed).map(|i| &i.sig)
            && let TypeDefKind::Adt { ctors } = &ty.kind
        {
            defs.extend(ctors.iter().map(|&c| DefId::new(homed.module, c)));
        }
    }
    let as_value = ns == PathNs::Value && qualified_as_value(cx);
    for def in defs {
        let Some(item) = cx.item(def) else { continue };
        if !fits(item.kind) {
            continue;
        }
        let label = cx.name_text(item.name);
        let Some(mut entry) = cx.def_entry(def, label, rank::MODULE) else {
            continue;
        };
        entry.insert = match ns {
            PathNs::Pattern => pattern_insert(cx, def, label),
            // An interface names its functions, so a path follows it
            // (D-1548).
            PathNs::Value if item.kind == DefKind::Interface => cx.insert(cx.interface_text(label)),
            PathNs::Value => value_insert(cx, label, item, as_value),
            PathNs::Type => cx.insert(label),
        };
        out.push(entry);
    }
}

/// A module path after `import` (X §4.1): the next segment of every
/// module of the package and of std under `prefix`, the importing module
/// left out.
fn import_paths(cx: &CompletionCx<'_>, prefix: &[String], out: &mut Items) {
    let own: Option<&ModulePath> = cx.view.and_then(ModuleView::path);
    let mut paths: Vec<Vec<String>> = cx
        .snapshot
        .all_modules()
        .filter_map(|view| {
            let path = view.path()?;
            let std = path.segments().first().is_some_and(|s| s == "std");
            (view.is_local() || std)
                .then_some(path)
                .filter(|p| Some(*p) != own)
                .map(|p| p.segments().to_vec())
        })
        .collect();
    if let Some(index) = cx.std.get() {
        paths.extend(
            index
                .modules
                .iter()
                .map(|m| m.path.split("::").map(str::to_owned).collect()),
        );
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for path in paths {
        let Some(rest) = path.strip_prefix(prefix) else {
            continue;
        };
        let Some(next) = rest.first() else { continue };
        if !seen.insert(next.clone()) {
            continue;
        }
        let full: Vec<&str> = prefix
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(next.as_str()))
            .collect();
        let mut entry = Entry::new(
            next.as_str(),
            EntryKind::Module,
            rank::MODULE,
            cx.insert(next.as_str()),
        );
        entry.detail = Some(full.join("::"));
        out.push(entry);
    }
}

/// The items an import list may name (X §4.1): the visible items of the
/// module, both namespaces, not yet listed. Constructors come with their
/// type (D-111), so only items with a namespace of their own are offered.
fn import_items(cx: &CompletionCx<'_>, module: &[String], written: &[String], out: &mut Items) {
    let Some(path) = module_path(module) else {
        return;
    };
    let fits = |kind: DefKind| {
        matches!(
            kind,
            DefKind::Fn | DefKind::Const | DefKind::Type | DefKind::Alias | DefKind::Interface
        )
    };
    let loaded = cx
        .snapshot
        .table()
        .find(&path)
        .and_then(|m| Some((m, cx.snapshot.interfaces().get(m)?)));
    let Some((id, iface)) = loaded else {
        if let Some(index) = cx.std.get() {
            let text = path.to_string();
            for item in index
                .items
                .iter()
                .filter(|i| i.module == text && fits(i.kind))
            {
                if !written.iter().any(|w| **w == *item.key.name) {
                    out.push(cx.std_entry(item, rank::MODULE));
                }
            }
        }
        return;
    };
    let package = cx
        .view
        .and_then(|v| cx.snapshot.table().get(v.id()))
        .map(|i| i.package);
    let mut defs: Vec<DefId> = iface
        .items
        .iter_enumerated()
        .filter(|&(index, item)| {
            fits(item.kind)
                && package.is_some_and(|p| iface.visible_from(index, p))
                && iface.is_current(index)
                && !iface.is_hidden(index)
        })
        .map(|(index, _)| DefId::new(id, index))
        .collect();
    defs.extend(builtin::homed_at(&path));
    for def in defs {
        let Some(item) = cx.item(def) else { continue };
        let label = cx.name_text(item.name);
        if written.iter().any(|w| w == label) {
            continue;
        }
        if let Some(entry) = cx.def_entry(def, label, rank::MODULE) {
            out.push(entry);
        }
    }
}

/// The module path `segments` write.
fn module_path(segments: &[String]) -> Option<ModulePath> {
    let (first, rest) = segments.split_first()?;
    Some(
        rest.iter()
            .fold(ModulePath::root(first.as_str()), |path, s| {
                path.child(s.as_str())
            }),
    )
}

/// The attribute names of §4.8: `@!` takes only `target`, and only std
/// may write `@std_*`.
fn attributes(cx: &CompletionCx<'_>, inner: bool, out: &mut Items) {
    let in_std = cx
        .view
        .and_then(ModuleView::path)
        .and_then(|p| p.segments().first())
        .is_some_and(|s| s == "std");
    let std: &[(&str, &str)] = if in_std { STD_ATTRIBUTES } else { &[] };
    for &(name, snippet) in ATTRIBUTES.iter().chain(std) {
        if inner && name != "target" {
            continue;
        }
        out.push(Entry::new(
            name,
            EntryKind::Keyword,
            rank::KEYWORD,
            cx.snippet(snippet, name),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, EntryKind, Items, escape, is_plain_name, rank};
    use cheby_driver::analysis::TextRange;

    use super::Insert;

    fn entry(label: &str, rank: u8, order: Option<u16>) -> Entry {
        let mut e = Entry::new(
            label,
            EntryKind::Variable,
            rank,
            Insert {
                range: TextRange::empty(0),
                text: label.to_owned(),
                snippet: false,
            },
        );
        e.order = order;
        e
    }

    #[test]
    fn sort_text_is_rank_then_order_then_label() {
        assert_eq!(entry("map", rank::MODULE, None).sort_text(), "20map");
        assert_eq!(entry("xs", rank::LOCAL, Some(3)).sort_text(), "100003xs");
        assert_eq!(entry("y", rank::EXPECTED, None).sort_text(), "00y");
    }

    #[test]
    fn finish_sorts_and_drops_repeats() {
        let mut items = Items::default();
        items.push(entry("b", rank::MODULE, None));
        items.push(entry("a", rank::KEYWORD, None));
        items.push(entry("b", rank::LOCAL, Some(0)));
        items.push(entry("c", rank::LOCAL, Some(1)));
        let labels: Vec<(String, u8)> = items
            .finish()
            .items
            .into_iter()
            .map(|e| (e.label, e.rank))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("b".to_owned(), rank::LOCAL),
                ("c".to_owned(), rank::LOCAL),
                ("a".to_owned(), rank::KEYWORD),
            ]
        );
    }

    #[test]
    fn snippet_text_is_escaped() {
        assert_eq!(escape("a$b}c\\d"), "a\\$b\\}c\\\\d");
        assert_eq!(escape("items"), "items");
    }

    #[test]
    fn compiler_names_are_not_offered() {
        assert!(is_plain_name("total"));
        assert!(is_plain_name("_unused"));
        assert!(is_plain_name("é"));
        assert!(!is_plain_name("_"));
        assert!(!is_plain_name("$use0"));
        assert!(!is_plain_name(""));
    }
}
