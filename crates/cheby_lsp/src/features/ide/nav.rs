//! Navigation over the occurrence index (X §2.2 to §2.7, F §5): what a
//! cursor names, where that is declared, the declaration of a satisfying
//! function, the types of a value, and every use of a name.
//!
//! The name at a cursor is the identifier `cursor::pick_ident` gives, and
//! the index's occurrence at exactly its range. With no identifier there,
//! it is the innermost implicit occurrence at the offset: the interface
//! function an operator calls, or the `show` of `{x}` (D-1353). A punned
//! field names its value or binding, as rename does (D-1392). A record
//! type's name names the type and its constructor, which are one name
//! (F §5.3).
//!
//! Everything here answers in run handles and byte ranges; the handlers
//! make protocol types of them.

use std::sync::Arc;

use cheby_driver::analysis::{
    DefId, DefKind, FileId, FileLocation, ItemSig, Member, ModuleId, ModuleView, Name, Namespace,
    ParamIdx, Snapshot, Symbol, TextRange, TextSize, Ty, TyKind, Verdict, builtin, implementors,
    interface_fns,
};
use cheby_driver::std_lib;
use cheby_syntax::{SyntaxKind as K, SyntaxNode, SyntaxToken, SyntaxTree};

use super::cursor::{pick_ident, token_at};
use super::scope;
use crate::features::index::{
    Classified, Index, Meaning, ModuleIndex, Occurrence, Partial, Role, SymbolEntry, Unknown,
};
use crate::features::{CancelToken, RequestError};

/// The most locations a type definition gives (X §2.4).
const MAX_TYPE_TARGETS: usize = 8;

/// How deep [`Heads::walk`] goes into a type.
const MAX_TYPE_DEPTH: u32 = 16;

/// The file a target is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NavFile {
    /// A file of the run: the package's, or std's.
    Run(FileId),
    /// A std module the run did not load, by its display path such as
    /// `std/list.cheby` (D-450): the home of a builtin type (§7.6).
    Std(String),
}

/// One place a request leads to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NavTarget {
    /// Its file.
    pub file: NavFile,
    /// The name: what the editor selects.
    pub name: TextRange,
    /// The whole declaration, which holds `name`.
    pub full: TextRange,
}

/// What a go-to request found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NavAnswer {
    /// The text at the cursor that names the target: an identifier, an
    /// operator, or the braces of `{x}`.
    pub origin: TextRange,
    /// The targets, in order, never empty.
    pub targets: Vec<NavTarget>,
}

/// What find references found (X §2.6).
#[derive(Clone, Debug)]
pub(crate) struct RefsAnswer {
    /// Every occurrence: its file and range, in module and text order.
    pub hits: Vec<(FileId, TextRange)>,
    /// The name, as the cursor's symbol writes it.
    pub text: String,
    /// Why the list may miss some (F §5.5).
    pub partial: Option<Partial>,
}

/// How an occurrence touches its symbol, for `documentHighlight`
/// (X §2.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum HighlightKind {
    /// An implicit occurrence: no text names it.
    Text,
    /// A use.
    Read,
    /// A declaration.
    Write,
}

/// What the cursor names.
struct Picked<'a> {
    /// The text that names it.
    origin: TextRange,
    /// The cursor's module.
    module: &'a ModuleIndex,
    /// The symbols named, the cursor's meaning first. A record type's
    /// name adds its constructor.
    entries: Vec<&'a SymbolEntry>,
    /// A path the resolver refused though the item exists, such as a
    /// hidden constructor (E0043): its item, for go-to only (X §2.2).
    hidden: Option<DefId>,
}

/// The name at `offset` in `view`, a module of `index`'s snapshot.
fn pick<'a>(index: Index<'a>, view: ModuleView<'a>, offset: TextSize) -> Option<Picked<'a>> {
    let module = index.module(view.id())?;
    let tree = view.tree()?;
    let token = pick_ident(token_at(tree.root(), offset));
    if let Some(token) = token.filter(|t| matches!(t.kind(), K::LOWER_IDENT | K::UPPER_IDENT)) {
        let range = token.text_range();
        return match module.classify(range) {
            Classified::Occurrence(occurrence) => Some(Picked {
                origin: range,
                module,
                entries: entries_of(module, occurrence),
                hidden: None,
            }),
            Classified::Unknown(name) if name.why == Unknown::UnresolvedPath => {
                hidden(index, module, token).map(|def| Picked {
                    origin: range,
                    module,
                    entries: Vec::new(),
                    hidden: Some(def),
                })
            }
            Classified::Unknown(_) | Classified::Unnamed(_) | Classified::Missing => None,
        };
    }
    // An operator, the braces of `{x}`, or a converted expression (F §5.3).
    let occurrence = module.implicit_at(offset).into_iter().next()?;
    Some(Picked {
        origin: occurrence.range,
        module,
        entries: entries_of(module, occurrence),
        hidden: None,
    })
}

/// The symbols `occurrence` names, the cursor's meaning first: the value
/// or binding of a punned field (D-1392), else the symbol, then a record
/// type's constructor at its declaration.
fn entries_of<'a>(module: &'a ModuleIndex, occurrence: &Occurrence) -> Vec<&'a SymbolEntry> {
    let first = match occurrence.role {
        Role::FieldInit | Role::FieldPat => occurrence.also.unwrap_or(occurrence.symbol),
        _ => occurrence.symbol,
    };
    let twin = (occurrence.role == Role::Decl)
        .then_some(occurrence.also)
        .flatten();
    std::iter::once(first)
        .chain(twin)
        .filter_map(|id| module.entry(id))
        .collect()
}

/// The item a refused path `m::name` names, found in `m`'s interface
/// whatever its visibility (X §2.2): the path's segment before `token`
/// must name a module.
fn hidden(index: Index<'_>, module: &ModuleIndex, token: SyntaxToken<'_>) -> Option<DefId> {
    let path = token.parent().filter(|p| p.kind() == K::PATH)?;
    let before = path
        .tokens()
        .filter(|t| matches!(t.kind(), K::LOWER_IDENT | K::UPPER_IDENT))
        .take_while(|t| *t != token)
        .last()?;
    let qualifier = module.at(before.text_range())?;
    let Meaning::Module(target) = module.entry(qualifier.symbol)?.meaning else {
        return None;
    };
    let snapshot = index.snapshot();
    let iface = snapshot.interfaces().get(target)?;
    let name = snapshot.globals().names.lookup(token.text())?;
    let spaces: &[Namespace] = if token.kind() == K::UPPER_IDENT {
        &[Namespace::Value, Namespace::Type]
    } else {
        &[Namespace::Value]
    };
    spaces
        .iter()
        .find_map(|&ns| iface.lookup(ns, name))
        .map(|found| DefId::new(target, found))
}

/// `textDocument/definition` at `offset` of `view` (X §2.2).
pub(crate) fn definition(
    index: Index<'_>,
    view: ModuleView<'_>,
    offset: TextSize,
) -> Option<NavAnswer> {
    let picked = pick(index, view, offset)?;
    let places = Places { index };
    let targets = match (picked.hidden, picked.entries.first()) {
        (Some(def), _) => places.item(def),
        (None, Some(entry)) => places.definition(entry),
        (None, None) => Vec::new(),
    };
    answer(picked.origin, targets)
}

/// `textDocument/declaration` at `offset` (X §2.3): on a function that
/// satisfies an interface, the interface functions it provides; else the
/// definition. A function that provides only `show` answers its
/// definition, since the builtin `Show` has no source (D-1478).
pub(crate) fn declaration(
    index: Index<'_>,
    view: ModuleView<'_>,
    offset: TextSize,
) -> Option<NavAnswer> {
    let picked = pick(index, view, offset)?;
    if let Some(&SymbolEntry {
        meaning: Meaning::Item(def),
        ..
    }) = picked.entries.first().copied()
    {
        let places = Places { index };
        let targets: Vec<NavTarget> = places
            .provided(view, def)
            .into_iter()
            .flat_map(|f| places.item(f))
            .collect();
        if !targets.is_empty() {
            return answer(picked.origin, targets);
        }
    }
    // No provided function has a place, as the builtin `Show`'s `show`
    // has none: the definition is the nearest answer (D-1478).
    definition(index, view, offset)
}

/// `textDocument/typeDefinition` at `offset` (X §2.4): the types the
/// value at the cursor has, user types first, then builtin ones, then
/// `dyn` interfaces, at most eight.
pub(crate) fn type_definition(
    index: Index<'_>,
    view: ModuleView<'_>,
    offset: TextSize,
) -> Option<NavAnswer> {
    let snapshot = index.snapshot();
    let mut heads = Heads::default();
    let origin = if let Some(picked) = pick(index, view, offset) {
        let entry = picked.entries.first()?;
        heads.of_entry(snapshot, view, offset, picked.origin, entry);
        picked.origin
    } else {
        let (origin, ty, owner) = expression_at(view, offset)?;
        heads.walk(snapshot, ty, Some(owner), 0);
        origin
    };
    let places = Places { index };
    let mut targets: Vec<NavTarget> = Vec::new();
    for head in heads.ordered() {
        for target in places.head(head) {
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
    }
    targets.truncate(MAX_TYPE_TARGETS);
    answer(origin, targets)
}

/// `textDocument/references` at `offset` (X §2.6), declarations kept when
/// `with_decls`. Implicit occurrences are in it (D-515 says nothing
/// against it). `None` when the cursor names nothing.
///
/// # Errors
///
/// [`RequestError::Cancelled`] when `cancel` is set.
pub(crate) fn references(
    index: Index<'_>,
    view: ModuleView<'_>,
    offset: TextSize,
    with_decls: bool,
    cancel: &CancelToken,
) -> Result<Option<RefsAnswer>, RequestError> {
    let Some(picked) = pick(index, view, offset) else {
        return Ok(None);
    };
    let Some(first) = picked.entries.first() else {
        return Ok(None);
    };
    let mut hits = Vec::new();
    // Why the list of the first symbol may miss some (F §5.5).
    let mut first_partial = None;
    for (n, entry) in picked.entries.iter().enumerate() {
        let found = index.references(&entry.symbol, cancel)?;
        if n == 0 {
            first_partial = found.partial;
        }
        for hit in found.hits {
            let Some(file) = hit.module.file else {
                continue;
            };
            if (with_decls || !declares(hit.module, hit.occurrence, &entry.symbol))
                && !hits.contains(&(file, hit.occurrence.range))
            {
                hits.push((file, hit.occurrence.range));
            }
        }
    }
    // The index covers the package's modules only, so the declaration of
    // a std item is added here, where definition finds it.
    if with_decls {
        for entry in &picked.entries {
            let Meaning::Item(def) = entry.meaning else {
                continue;
            };
            let snapshot = index.snapshot();
            let Some(span) = snapshot
                .interfaces()
                .get(def.module)
                .and_then(|iface| iface.item(def.index))
                .and_then(|item| item.span)
            else {
                continue;
            };
            let in_std = matches!(snapshot.location(span.file), Some(FileLocation::Std(_)));
            if in_std && !hits.contains(&(span.file, span.range)) {
                hits.push((span.file, span.range));
            }
        }
    }
    let text = first.symbol.text().to_owned();
    // A local is named in its own module only.
    let partial = match first.meaning {
        Meaning::Local { .. } => own_partial(picked.module, &text),
        _ => first_partial,
    };
    Ok(Some(RefsAnswer {
        hits,
        text,
        partial,
    }))
}

/// Why references of a local named `text` may be incomplete: an unknown
/// name of that text in its module, or a lost module (F §5.5).
fn own_partial(module: &ModuleIndex, text: &str) -> Option<Partial> {
    if let Some(name) = module.unknown.iter().find(|u| &*u.text == text) {
        return Some(Partial::Unknown {
            module: module.module,
            name: name.clone(),
        });
    }
    module.lost.then_some(Partial::Lost {
        module: module.module,
    })
}

/// `textDocument/documentHighlight` at `offset` (X §2.7): the
/// occurrences of the cursor's name in its module, sorted, with how each
/// touches it. `None` when the cursor names nothing.
pub(crate) fn highlights(
    index: Index<'_>,
    view: ModuleView<'_>,
    offset: TextSize,
) -> Option<Vec<(TextRange, HighlightKind)>> {
    let picked = pick(index, view, offset)?;
    let ids: Vec<u32> = picked
        .entries
        .iter()
        .filter_map(|entry| picked.module.id_of(&entry.symbol))
        .collect();
    if ids.is_empty() {
        return None;
    }
    let mut found: Vec<(TextRange, HighlightKind)> = picked
        .module
        .occurrences
        .iter()
        .filter(|o| ids.iter().any(|&id| o.names(id)))
        .map(|o| {
            let write = picked
                .entries
                .iter()
                .any(|entry| declares(picked.module, o, &entry.symbol));
            let kind = match o.role {
                Role::Implicit(_) => HighlightKind::Text,
                _ if write => HighlightKind::Write,
                _ => HighlightKind::Read,
            };
            (o.range, kind)
        })
        .collect();
    // One per range: a declaration wins over a use of the same token.
    found.sort_by_key(|&(range, kind)| (range.start(), range.end(), std::cmp::Reverse(kind)));
    found.dedup_by_key(|(range, _)| *range);
    Some(found)
}

/// Whether `occurrence`, in `module`, declares `symbol`: a declaration of
/// it, or the binding of a punned field pattern, `P { x }`, whose field
/// is a use (F §5.3).
fn declares(module: &ModuleIndex, occurrence: &Occurrence, symbol: &Symbol) -> bool {
    let Some(id) = module.id_of(symbol) else {
        return false;
    };
    match occurrence.role {
        Role::FieldPat => occurrence.also == Some(id),
        role => role.is_decl() && occurrence.names(id),
    }
}

/// `targets` at `origin`, or `None` when there are none.
fn answer(origin: TextRange, targets: Vec<NavTarget>) -> Option<NavAnswer> {
    (!targets.is_empty()).then_some(NavAnswer { origin, targets })
}

/// Where things are declared in one snapshot.
#[derive(Clone, Copy)]
struct Places<'a> {
    index: Index<'a>,
}

impl<'a> Places<'a> {
    /// The snapshot.
    const fn snapshot(self) -> &'a Snapshot {
        self.index.snapshot()
    }

    /// Where `entry` is declared (X §2.2).
    fn definition(self, entry: &SymbolEntry) -> Vec<NavTarget> {
        match (&entry.symbol.member, entry.meaning) {
            // Its `as` name (F §5.3).
            (Member::ImportAlias(_), _) | (_, Meaning::Local { .. }) => self.decls(entry),
            (_, Meaning::Module(module)) => self.module_start(module),
            (_, Meaning::Item(def)) => self.item(def),
            (_, Meaning::TypeParam { item, .. }) => {
                let found = self.decls(entry);
                if found.is_empty() {
                    self.item(item)
                } else {
                    found
                }
            }
            (_, Meaning::Field { ty, name }) => {
                let found = self.decls(entry);
                if found.is_empty() {
                    self.unindexed_fields(ty, name)
                } else {
                    found
                }
            }
        }
    }

    /// The declaration occurrences of `entry` in the index of the module
    /// its symbol belongs to: every variant's field, every alternative of
    /// an or-pattern.
    fn decls(self, entry: &SymbolEntry) -> Vec<NavTarget> {
        let Some(home) = self
            .snapshot()
            .table()
            .find(&entry.symbol.module)
            .and_then(|id| self.index.module(id))
        else {
            return Vec::new();
        };
        let Some(file) = home.file else {
            return Vec::new();
        };
        home.occurrences_of(&entry.symbol)
            .filter(|o| declares(home, o, &entry.symbol))
            .map(|o| NavTarget {
                file: NavFile::Run(file),
                name: o.range,
                full: self.full(file, o.range),
            })
            .collect()
    }

    /// The start of module `module`'s file.
    fn module_start(self, module: ModuleId) -> Vec<NavTarget> {
        let file = self.snapshot().table().get(module).and_then(|m| m.file);
        file.map(|file| NavTarget {
            file: NavFile::Run(file),
            name: TextRange::empty(0),
            full: TextRange::empty(0),
        })
        .into_iter()
        .collect()
    }

    /// The name of item `def`. A builtin item has no source, so it is the
    /// start of its home module's file (§7.6), and a constructor's home is
    /// its type's. No home: nothing.
    fn item(self, def: DefId) -> Vec<NavTarget> {
        let Some(item) = self
            .snapshot()
            .interfaces()
            .get(def.module)
            .and_then(|iface| iface.item(def.index))
        else {
            return Vec::new();
        };
        if let Some(span) = item.span {
            return vec![NavTarget {
                file: NavFile::Run(span.file),
                name: span.range,
                full: self.full(span.file, span.range),
            }];
        }
        let home = |d: DefId| {
            builtin::HOMES
                .iter()
                .find(|&&(homed, _)| homed == d)
                .map(|&(_, segment)| segment)
        };
        let segment = home(def).or_else(|| {
            item.parent
                .and_then(|parent| home(DefId::new(def.module, parent)))
        });
        let Some(segment) = segment else {
            return Vec::new();
        };
        let path = builtin::home_path(segment);
        let table = self.snapshot().table();
        let file = match table
            .find(&path)
            .and_then(|id| table.get(id))
            .and_then(|m| m.file)
        {
            Some(file) => NavFile::Run(file),
            None => NavFile::Std(std_lib::display_path(segment)),
        };
        vec![NavTarget {
            file,
            name: TextRange::empty(0),
            full: TextRange::empty(0),
        }]
    }

    /// The fields `name` of the type `ty` in a module the index does not
    /// hold, such as std's in a package run: read from its parsed text.
    /// None found: nothing, never the type's name.
    fn unindexed_fields(self, ty: DefId, name: Name) -> Vec<NavTarget> {
        let types = self.item(ty);
        let Some(NavTarget {
            file: NavFile::Run(file),
            name: type_name,
            ..
        }) = types.first().cloned()
        else {
            return types;
        };
        let wanted = self.snapshot().globals().names.get(name);
        self.with_tree(file, |tree| {
            let Some(decl) = tree
                .root()
                .token_at_offset(type_name.start())
                .and_then(SyntaxToken::parent)
                .filter(|n| n.kind() == K::TYPE_DECL)
            else {
                return Vec::new();
            };
            decl.descendants()
                .filter(|n| n.kind() == K::RECORD_FIELD)
                .filter_map(|field| field.first_token_of_kind(K::LOWER_IDENT))
                .filter(|t| t.text() == wanted)
                .map(|t| NavTarget {
                    file: NavFile::Run(file),
                    name: t.text_range(),
                    full: t.parent().map_or(t.text_range(), SyntaxNode::text_range),
                })
                .collect()
        })
        .unwrap_or_default()
    }

    /// The node that declares the name at `name` in `file`: its parent,
    /// such as the `FN_DECL` of a function's name. `name` itself when the
    /// file has no such token.
    fn full(self, file: FileId, name: TextRange) -> TextRange {
        self.with_tree(file, |tree| {
            tree.root()
                .token_at_offset(name.start())
                .filter(|t| t.text_range() == name)
                .and_then(SyntaxToken::parent)
                .map(SyntaxNode::text_range)
        })
        .flatten()
        .unwrap_or(name)
    }

    /// `f` of the tree of `file`: the run's when it kept one, else a parse
    /// of the run's text, as for std in a package run.
    fn with_tree<R>(self, file: FileId, f: impl FnOnce(&SyntaxTree) -> R) -> Option<R> {
        let snapshot = self.snapshot();
        if let Some(tree) = snapshot.module_at(file).and_then(ModuleView::tree) {
            return Some(f(tree));
        }
        let text: Arc<str> = snapshot.source(file)?.text_arc();
        let (tree, _) = cheby_syntax::parse(file, text).into_parts();
        Some(f(&tree))
    }

    /// The interface functions the function `def` provides (X §2.3): for
    /// each interface with a function of its name, those whose satisfying
    /// function, for a type of `def`'s module (D-045), is `def`. Empty for
    /// anything else, and for an `@external`, whose declaration is its
    /// definition. `view` is the cursor's module.
    fn provided(self, view: ModuleView<'_>, def: DefId) -> Vec<DefId> {
        let snapshot = self.snapshot();
        let interfaces = snapshot.interfaces();
        let Some(own) = interfaces.get(def.module) else {
            return Vec::new();
        };
        let Some(func) = own.item(def.index).filter(|f| f.kind == DefKind::Fn) else {
            return Vec::new();
        };
        let external = snapshot
            .module(def.module)
            .and_then(ModuleView::resolved)
            .and_then(|resolved| resolved.item(def.index))
            .is_some_and(|item| !item.attrs.externals.is_empty());
        // The solver reads no tree or resolution, so the cursor's module
        // may lend them for a home without a tree (D-1386).
        let Some(input) = snapshot.query_input(def.module, view) else {
            return Vec::new();
        };
        if external {
            return Vec::new();
        }
        let types: Vec<DefId> = own
            .items
            .iter_enumerated()
            .filter(|(_, item)| item.kind == DefKind::Type)
            .map(|(index, _)| DefId::new(def.module, index))
            .collect();
        if types.is_empty() {
            return Vec::new();
        }
        let item_of = |d: DefId| interfaces.get(d.module).and_then(|i| i.item(d.index));
        let mut out = Vec::new();
        for &module in snapshot.all_ids() {
            let Some(iface) = interfaces.get(module) else {
                continue;
            };
            for (index, item) in iface.items.iter_enumerated() {
                if item.kind != DefKind::Interface {
                    continue;
                }
                let interface = DefId::new(module, index);
                let Ok(fns) = interface_fns(interface, &item_of) else {
                    continue;
                };
                if !fns
                    .iter()
                    .any(|&f| item_of(f).is_some_and(|f| f.name == func.name))
                {
                    continue;
                }
                let Ok(found) = implementors(&input, interface, &types) else {
                    continue;
                };
                for (_, verdict) in found {
                    let Verdict::Yes {
                        fns: satisfying, ..
                    } = verdict
                    else {
                        continue;
                    };
                    for (satisfying, provided) in satisfying.iter().zip(&fns) {
                        if *satisfying == def && !out.contains(provided) {
                            out.push(*provided);
                        }
                    }
                }
            }
        }
        out
    }

    /// The targets of one type head (X §2.4).
    fn head(self, head: Head) -> Vec<NavTarget> {
        match head {
            Head::Def(def) => self.item(def),
            Head::Param(item, param) => {
                let found: Vec<NavTarget> = self
                    .index
                    .module(item.module)
                    .into_iter()
                    .flat_map(|module| {
                        module
                            .symbols
                            .iter()
                            .filter(move |e| e.meaning == Meaning::TypeParam { item, param })
                    })
                    .flat_map(|entry| self.decls(entry))
                    .collect();
                if found.is_empty() {
                    self.item(item)
                } else {
                    found
                }
            }
        }
    }
}

/// The innermost expression at `offset` that has a type: its range, the
/// type, and the item whose generics the type's parameters are.
fn expression_at(view: ModuleView<'_>, offset: TextSize) -> Option<(TextRange, Ty, DefId)> {
    let tree = view.tree()?;
    let item = scope::body_at(view, offset)?;
    let body = view.facts()?.body(item)?;
    let owner = view.resolved()?.def_id(item);
    let at = token_at(tree.root(), offset);
    let token = at.right().or_else(|| at.left())?;
    token
        .parent()
        .into_iter()
        .flat_map(SyntaxNode::ancestors)
        .find_map(|node| {
            body.expr(node.id())
                .map(|e| (node.text_range(), e.ty, owner))
        })
}

/// One head a type definition leads to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Head {
    /// A type or an interface.
    Def(DefId),
    /// A type parameter of an item.
    Param(DefId, ParamIdx),
}

/// The heads of the types at a cursor, in X §2.4's three groups.
#[derive(Debug, Default)]
struct Heads {
    /// Types of the package or std, and type parameters, in preorder.
    user: Vec<Head>,
    /// Builtin types.
    builtin: Vec<Head>,
    /// `dyn` interfaces.
    dyns: Vec<Head>,
}

impl Heads {
    /// The heads in order, each once.
    fn ordered(&self) -> Vec<Head> {
        let mut out: Vec<Head> = Vec::new();
        for &head in self.user.iter().chain(&self.builtin).chain(&self.dyns) {
            if !out.contains(&head) {
                out.push(head);
            }
        }
        out
    }

    /// The heads of the name `entry` at `origin`.
    fn of_entry(
        &mut self,
        snapshot: &Snapshot,
        view: ModuleView<'_>,
        offset: TextSize,
        origin: TextRange,
        entry: &SymbolEntry,
    ) {
        match entry.meaning {
            Meaning::Local { item, slot } => {
                let owner = view.resolved().map(|r| r.def_id(item));
                if let Some(local) = scope::local_fact(view, item, slot) {
                    self.walk(snapshot, local.ty, owner, 0);
                }
            }
            Meaning::Field { .. } => {
                let fact = scope::body_at(view, offset).and_then(|item| {
                    let fact = view.facts()?.body(item)?.field_at(origin.start())?;
                    Some((fact.ty?, view.resolved()?.def_id(item)))
                });
                if let Some((ty, owner)) = fact {
                    self.walk(snapshot, ty, Some(owner), 0);
                }
            }
            Meaning::Item(def) => self.of_item(snapshot, def),
            Meaning::TypeParam { item, param } => self.user.push(Head::Param(item, param)),
            Meaning::Module(_) => {}
        }
    }

    /// The heads of the item `def`: a constant's type, a function's
    /// parameter and return types, a constructor's type, a type or an
    /// interface itself, an alias's target.
    fn of_item(&mut self, snapshot: &Snapshot, def: DefId) {
        let Some(item) = snapshot
            .interfaces()
            .get(def.module)
            .and_then(|iface| iface.item(def.index))
        else {
            return;
        };
        match &item.sig {
            ItemSig::Const(c) => self.walk(snapshot, c.ty, None, 0),
            ItemSig::Fn(f) => {
                for param in &f.params {
                    self.walk(snapshot, param.ty, Some(def), 0);
                }
                self.walk(snapshot, f.ret, Some(def), 0);
            }
            ItemSig::InterfaceFn(f) => {
                for &param in &f.params {
                    self.walk(snapshot, param, None, 0);
                }
                self.walk(snapshot, f.ret, None, 0);
            }
            ItemSig::Ctor(c) => self.push_def(DefId::new(def.module, c.ty)),
            ItemSig::Type(_) | ItemSig::Interface(_) => self.push_def(def),
            ItemSig::Alias(a) => self.walk(snapshot, a.target, Some(def), 0),
            ItemSig::Test | ItemSig::Error => {}
        }
    }

    /// Adds the type or interface `def` to its group.
    fn push_def(&mut self, def: DefId) {
        if def.module == ModuleId::BUILTIN {
            self.builtin.push(Head::Def(def));
        } else {
            self.user.push(Head::Def(def));
        }
    }

    /// Adds the heads of `ty` in preorder. `owner` declares its
    /// parameters.
    fn walk(&mut self, snapshot: &Snapshot, ty: Ty, owner: Option<DefId>, depth: u32) {
        if depth > MAX_TYPE_DEPTH {
            return;
        }
        let deeper = depth.saturating_add(1);
        match snapshot.globals().types.kind(ty) {
            TyKind::Named { def, args } => {
                self.push_def(*def);
                for &arg in args {
                    self.walk(snapshot, arg, owner, deeper);
                }
            }
            TyKind::Tuple(items) => {
                for &item in items {
                    self.walk(snapshot, item, owner, deeper);
                }
            }
            TyKind::Fn { params, ret } => {
                for &param in params {
                    self.walk(snapshot, param, owner, deeper);
                }
                self.walk(snapshot, *ret, owner, deeper);
            }
            TyKind::Param(param) => {
                if let Some(owner) = owner {
                    self.user.push(Head::Param(owner, *param));
                }
            }
            TyKind::Dyn { iface, args } => {
                self.dyns.push(Head::Def(*iface));
                for &arg in args {
                    self.walk(snapshot, arg, owner, deeper);
                }
            }
            TyKind::Error | TyKind::Never => {}
        }
    }
}
