//! The occurrence index (F §5, plan §1.3): every name of a module's text,
//! what it means, and the [`Symbol`] that names that meaning across
//! snapshots (D-1365).
//!
//! The index walks every identifier token of a module once and classifies
//! it by its parent's kind (F §5.1, §5.3), so each one lands in exactly one
//! of three lists of its [`ModuleIndex`]:
//!
//! - [`ModuleIndex::occurrences`]: the token names a [`SymbolEntry`], with
//!   a [`Role`]. Facts add [`Role::Implicit`] occurrences that no
//!   identifier writes: an operator's interface function, the `show` of
//!   `{x}`, the interface of a `dyn` conversion.
//! - [`ModuleIndex::unknown`]: the index cannot place the token
//!   ([`UnknownName`], F §5.5). Find references reports itself partial and
//!   rename refuses when one of these has the symbol's text.
//! - [`ModuleIndex::unnamed`]: the token names nothing by design, such as
//!   an attribute name or a signature's label ([`Unnamed`]).
//!
//! A punned token has two meanings: [`Occurrence::symbol`] is the field and
//! [`Occurrence::also`] the value or the binding (plan §1.3, D-1392). A
//! record type's name is the type, with its constructor as `also`. When the
//! field of a punned token cannot be placed, the occurrence holds the value
//! or binding alone and the token is in `unknown` too.
//!
//! # How stage 2 uses this
//!
//! A handler takes [`Analyzed::index`], which builds a module's index on
//! first need and keeps it for every later request of the snapshot.
//!
//! - **The name at a cursor**: `ide::cursor::pick_ident` gives the token;
//!   [`Index::of_file`] the module's index; [`ModuleIndex::at`] the
//!   occurrence at the token's exact range, and [`ModuleIndex::entry`] its
//!   [`SymbolEntry`]: the [`Symbol`], the [`Meaning`] (run handles: a
//!   `DefId`, a module, a field, a type parameter or a local of a body) and
//!   the [`NameKind`]. [`ModuleIndex::classify`] says why a token has none.
//!   Operators and the braces of `{x}` are not identifiers:
//!   [`ModuleIndex::implicit_at`] finds them by offset.
//! - **Hover** (B): the [`Meaning`] picks the row of F §1.8. A local's type
//!   comes from `ide::scope::local_fact`, an item's header from
//!   `ide::print::Printer`. A token in `unknown` gets the name fallback of
//!   F §1.8, through `ide::scope::locals_at`.
//! - **Navigation** (C): definition reads the [`Meaning`] (an alias's
//!   [`Symbol`] is a `Member::ImportAlias`, whose definition is its
//!   `RENAME` token: its declaration occurrence).
//!   References are [`Index::references`], with
//!   [`References::partial`] set per F §5.5. Highlight is
//!   [`ModuleIndex::occurrences_of`] in one module: [`Role::is_decl`] is a
//!   write, [`Role::Implicit`] is text, the rest are reads.
//! - **Call hierarchy and workspace symbols** (D): incoming calls are the
//!   references of a function with [`Role::Use`] or [`Role::Implicit`],
//!   grouped by the item that holds each one; outgoing calls walk the
//!   occurrences inside one item's range.
//! - **Semantic tokens** (E): walk [`ModuleIndex::occurrences`] that are
//!   not implicit, in order, and map [`NameKind`] to a token type;
//!   [`Role::is_decl`] is the `declaration` modifier.

mod build;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::OnceLock;

use cheby_driver::analysis::{
    DefId, DefIndex, FileId, ModuleId, ModulePath, Name, ParamIdx, Snapshot, SnapshotId, Symbol,
    TextRange, TextSize,
};

use super::{Analyzed, CancelToken, RequestError};

/// The [`super::Memo`] entry of the index: one [`PackageIndex`] per
/// snapshot, made on first need and shared by every request.
#[derive(Debug, Default)]
pub(crate) struct IndexMemo(OnceLock<PackageIndex>);

impl IndexMemo {
    /// The index of `snapshot`, which must be the snapshot this memo was
    /// made for: [`Analyzed::index`] pairs them.
    fn get(&self, snapshot: &Snapshot) -> &PackageIndex {
        self.0.get_or_init(|| PackageIndex::new(snapshot))
    }
}

impl Analyzed {
    /// The occurrence index of this snapshot (F §5).
    pub(crate) fn index(&self) -> Index<'_> {
        Index::new(&self.snapshot, &self.memo.index)
    }
}

/// The index of every module of a snapshot that keeps its tree: the
/// package's modules, or std's in the std base. Each module's part is
/// built on first need.
pub(crate) struct PackageIndex {
    snapshot: SnapshotId,
    /// By module id, ascending.
    modules: Vec<(ModuleId, OnceLock<ModuleIndex>)>,
}

impl std::fmt::Debug for PackageIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let built = self
            .modules
            .iter()
            .filter(|(_, m)| m.get().is_some())
            .count();
        f.debug_struct("PackageIndex")
            .field("snapshot", &self.snapshot)
            .field("modules", &self.modules.len())
            .field("built", &built)
            .finish()
    }
}

impl PackageIndex {
    /// An index of `snapshot` with nothing built yet.
    fn new(snapshot: &Snapshot) -> Self {
        let modules = snapshot
            .all_modules()
            .filter(|m| m.tree().is_some())
            .map(|m| (m.id(), OnceLock::new()))
            .collect();
        Self {
            snapshot: snapshot.id(),
            modules,
        }
    }
}

/// A snapshot with its index, as handlers read it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Index<'a> {
    snapshot: &'a Snapshot,
    package: &'a PackageIndex,
}

impl<'a> Index<'a> {
    /// The index of `snapshot`, kept in `memo`.
    pub(crate) fn new(snapshot: &'a Snapshot, memo: &'a IndexMemo) -> Self {
        Self {
            snapshot,
            package: memo.get(snapshot),
        }
    }

    /// The snapshot the index reads.
    pub(crate) const fn snapshot(self) -> &'a Snapshot {
        self.snapshot
    }

    /// The index of module `id`, built on the first call. `None` for a
    /// module with no tree in this snapshot, such as std in a package run.
    pub(crate) fn module(self, id: ModuleId) -> Option<&'a ModuleIndex> {
        let n = self
            .package
            .modules
            .binary_search_by_key(&id, |(m, _)| *m)
            .ok()?;
        let (_, cell) = self.package.modules.get(n)?;
        let view = self.snapshot.module(id)?;
        Some(cell.get_or_init(|| build::module(self.snapshot, view)))
    }

    /// The index of the module `file` holds.
    pub(crate) fn of_file(self, file: FileId) -> Option<&'a ModuleIndex> {
        self.module(self.snapshot.module_at(file)?.id())
    }

    /// Every indexed module, in id order, each built on first need.
    pub(crate) fn modules(self) -> impl Iterator<Item = &'a ModuleIndex> + 'a {
        self.package
            .modules
            .iter()
            .filter_map(move |(id, _)| self.module(*id))
    }

    /// Every occurrence of `symbol` in every indexed module, in module and
    /// text order, with whether the list may be partial (F §5.5). A punned
    /// occurrence counts for both its symbols.
    ///
    /// # Errors
    ///
    /// [`RequestError::Cancelled`] when `cancel` is set, checked per
    /// module.
    pub(crate) fn references(
        self,
        symbol: &Symbol,
        cancel: &CancelToken,
    ) -> Result<References<'a>, RequestError> {
        let mut hits = Vec::new();
        for (id, _) in &self.package.modules {
            cancel.check()?;
            let Some(module) = self.module(*id) else {
                continue;
            };
            hits.extend(
                module
                    .occurrences_of(symbol)
                    .map(|occurrence| Hit { module, occurrence }),
            );
        }
        Ok(References {
            hits,
            partial: self.partial(symbol.text()),
        })
    }

    /// Why an answer about a name with `text` may miss some of its uses
    /// (F §5.5): the first indexed module, in id order, with an unknown
    /// name of that text, else the first module the run lost. `None` when
    /// the index placed every name that could be it.
    pub(crate) fn partial(self, text: &str) -> Option<Partial> {
        let mut lost = self.snapshot.lost().first().map(|&(id, _)| id);
        for module in self.modules() {
            if let Some(name) = module.unknown.iter().find(|u| &*u.text == text) {
                return Some(Partial::Unknown {
                    module: module.module,
                    name: name.clone(),
                });
            }
            if module.lost {
                lost = lost.or(Some(module.module));
            }
        }
        lost.map(|module| Partial::Lost { module })
    }
}

/// One occurrence that [`Index::references`] found.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Hit<'a> {
    /// The module it is in.
    pub module: &'a ModuleIndex,
    /// The occurrence.
    pub occurrence: &'a Occurrence,
}

/// What [`Index::references`] gives.
#[derive(Debug)]
pub(crate) struct References<'a> {
    /// Every occurrence, in module and text order.
    pub hits: Vec<Hit<'a>>,
    /// Why the list may miss some, or `None` (F §5.5).
    pub partial: Option<Partial>,
}

/// Why a list of occurrences may be incomplete (F §5.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Partial {
    /// A module has a name of the same text that the index cannot place.
    Unknown {
        /// The module.
        module: ModuleId,
        /// The first such name in it.
        name: UnknownName,
    },
    /// The run lost a module (D-1362), or a module has no resolution.
    Lost {
        /// The module.
        module: ModuleId,
    },
}

/// The names of one module: every identifier token's meaning (F §5.2).
#[derive(Debug)]
pub(crate) struct ModuleIndex {
    /// The module.
    pub module: ModuleId,
    /// Its file. `None` only for the builtin module, which has no tree.
    pub file: Option<FileId>,
    /// Its path.
    pub path: ModulePath,
    /// The symbols named here, interned: an occurrence holds an index into
    /// this list.
    pub symbols: Vec<SymbolEntry>,
    /// Every placed name, sorted by range, then implicit ones after the
    /// written one at the same start.
    pub occurrences: Vec<Occurrence>,
    /// The names the index cannot place, sorted by range (F §5.5).
    pub unknown: Vec<UnknownName>,
    /// The identifier tokens that name nothing by design, sorted by range.
    pub unnamed: Vec<UnnamedName>,
    /// The module has no resolution, or the run lost a body of it to an
    /// internal error (D-1362): what it says is incomplete.
    pub lost: bool,
    by_symbol: HashMap<Symbol, u32>,
}

impl ModuleIndex {
    /// The symbol interned as `id`.
    pub(crate) fn entry(&self, id: u32) -> Option<&SymbolEntry> {
        self.symbols.get(usize::try_from(id).ok()?)
    }

    /// The id `symbol` is interned as here, if this module names it.
    pub(crate) fn id_of(&self, symbol: &Symbol) -> Option<u32> {
        self.by_symbol.get(symbol).copied()
    }

    /// The written occurrence of the identifier token at exactly `range`:
    /// never an implicit one.
    pub(crate) fn at(&self, range: TextRange) -> Option<&Occurrence> {
        let first = self
            .occurrences
            .partition_point(|o| o.range.start() < range.start());
        self.occurrences
            .get(first..)?
            .iter()
            .take_while(|o| o.range.start() == range.start())
            .find(|o| o.range == range && !o.role.is_implicit())
    }

    /// The implicit occurrences whose range holds `offset`, or ends at it:
    /// an operator, the braces of `{x}`, a `dyn` conversion. Innermost
    /// first.
    pub(crate) fn implicit_at(&self, offset: TextSize) -> Vec<&Occurrence> {
        let mut found: Vec<&Occurrence> = self
            .occurrences
            .iter()
            .filter(|o| {
                o.role.is_implicit() && o.range.start() <= offset && offset <= o.range.end()
            })
            .collect();
        found.sort_by_key(|o| o.range.len());
        found
    }

    /// What the index says about the identifier token at exactly `range`.
    pub(crate) fn classify(&self, range: TextRange) -> Classified<'_> {
        if let Some(occurrence) = self.at(range) {
            return Classified::Occurrence(occurrence);
        }
        if let Some(name) = self.unknown.iter().find(|u| u.range == range) {
            return Classified::Unknown(name);
        }
        if let Some(name) = self.unnamed.iter().find(|u| u.range == range) {
            return Classified::Unnamed(name.what);
        }
        Classified::Missing
    }

    /// The occurrences in this module that name `symbol`, as `symbol` or
    /// as `also`, in text order.
    pub(crate) fn occurrences_of<'s>(
        &'s self,
        symbol: &Symbol,
    ) -> impl Iterator<Item = &'s Occurrence> + 's {
        let id = self.id_of(symbol);
        self.occurrences
            .iter()
            .filter(move |o| id.is_some_and(|id| o.names(id)))
    }
}

/// What [`ModuleIndex::classify`] found at a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Classified<'a> {
    /// A placed name.
    Occurrence(&'a Occurrence),
    /// A name the index cannot place.
    Unknown(&'a UnknownName),
    /// A name that names nothing by design.
    Unnamed(Unnamed),
    /// No identifier token of the module has this range.
    Missing,
}

/// One interned symbol of a module: its name across snapshots, what it is
/// in this run, and its kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SymbolEntry {
    /// The identity that crosses snapshots (D-1365).
    pub symbol: Symbol,
    /// What it means in this snapshot. An import alias means what it
    /// names.
    pub meaning: Meaning,
    /// What kind of thing it names. An import alias has the kind of what
    /// it names.
    pub kind: NameKind,
}

/// What a symbol means in one snapshot: handles of that run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Meaning {
    /// A module.
    Module(ModuleId),
    /// An item: a function, constant, type, alias, interface, constructor,
    /// interface function or test.
    Item(DefId),
    /// A field of the type `ty`, over all its variants (D-071).
    Field {
        /// The type.
        ty: DefId,
        /// The field's name.
        name: Name,
    },
    /// A type parameter of `item`.
    TypeParam {
        /// The item that declares it.
        item: DefId,
        /// Its index in the item's generics.
        param: ParamIdx,
    },
    /// A local of the body of `item`, in the indexed module.
    /// `ide::scope::local_decl` and `ide::scope::local_fact` read it.
    Local {
        /// The item whose body declares it.
        item: DefIndex,
        /// Its position in the body's locals, which is also its position
        /// in the body's facts (F §1.3).
        slot: u32,
    },
}

/// What kind of thing a name is, as hover and semantic tokens tell them
/// apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum NameKind {
    /// A module.
    Module,
    /// A top-level function.
    Function,
    /// A constant.
    Constant,
    /// A type.
    Type,
    /// A type alias.
    Alias,
    /// An interface.
    Interface,
    /// A constructor.
    Constructor,
    /// A function of an interface.
    InterfaceFn,
    /// A `test` item.
    Test,
    /// A field.
    Field,
    /// A type parameter.
    TypeParam,
    /// A parameter of a function or a closure.
    Param,
    /// A `let`, `case` arm or `use` binding.
    Local,
    /// A local function.
    LocalFn,
}

/// One placed name (F §5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Occurrence {
    /// The identifier token, or for an implicit occurrence the operator
    /// token, the `{…}` of an interpolation or the converted expression.
    pub range: TextRange,
    /// What it names, in [`ModuleIndex::symbols`].
    pub symbol: u32,
    /// The second meaning: the value or binding of a punned field, or a
    /// record type's constructor at its declaration.
    pub also: Option<u32>,
    /// How the token names it.
    pub role: Role,
}

impl Occurrence {
    /// Whether the occurrence names the symbol interned as `id`.
    pub(crate) fn names(&self, id: u32) -> bool {
        self.symbol == id || self.also == Some(id)
    }
}

/// How an occurrence names its symbol (F §5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Role {
    /// Its declaration: an item's name, a binding, a field or type
    /// parameter declaration, an import's `as` name.
    Decl,
    /// A use: the last segment of a path, in an expression, a pattern or a
    /// type.
    Use,
    /// A segment before the last of a path: a module or a type.
    Qualifier,
    /// A segment of an import's module path, or an imported item's name.
    Import,
    /// A field read, `x.f`.
    FieldRead,
    /// A field of a record construction or update.
    FieldInit,
    /// A field of a pattern.
    FieldPat,
    /// The name that starts `{x}` or `{x.f}`.
    InterpRoot,
    /// A field after the name in `{x.f}`.
    InterpField,
    /// A name in an attribute's arguments: `@std_hooks`, `@std_clamped`.
    Attr,
    /// No text names it.
    Implicit(ImplicitKind),
}

impl Role {
    /// Whether the occurrence declares its symbol: a write, for highlight.
    pub(crate) const fn is_decl(self) -> bool {
        matches!(self, Self::Decl)
    }

    /// Whether no identifier writes the occurrence.
    pub(crate) const fn is_implicit(self) -> bool {
        matches!(self, Self::Implicit(_))
    }
}

/// What an implicit occurrence is (F §5.3). They are references, never
/// rename targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ImplicitKind {
    /// An operator that calls an interface function (§8.7).
    Operator,
    /// The `show` that `{x}` calls.
    Show,
    /// The interface of a `dyn` conversion.
    Dyn,
}

/// A name the index cannot place (F §5.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnknownName {
    /// The token.
    pub range: TextRange,
    /// Its text.
    pub text: Box<str>,
    /// Why.
    pub why: Unknown,
}

/// Why the index cannot place a name (F §5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Unknown {
    /// The token is inside a syntax error, where nothing is resolved.
    InErrorNode,
    /// The path or import does not resolve: an error was reported.
    UnresolvedPath,
    /// The field's receiver has no known type (`FieldFact.owner` is
    /// `None`), or its type declares no field of that name.
    FieldOfUnknownType,
    /// The run lost the body, or the module, to an internal error
    /// (D-1362).
    BodyLost,
    /// The body was resolved but not checked, so it has no facts: a `test`
    /// of std (D-1370).
    Unchecked,
    /// Resolution recorded nothing for the name, though nothing above
    /// explains why: a declaration the resolver skipped, a binding the
    /// checker reported.
    NotRecorded,
}

/// An identifier token that names nothing by design (F §5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UnnamedName {
    /// The token.
    pub range: TextRange,
    /// What it is.
    pub what: Unnamed,
}

/// What an identifier that names nothing is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Unnamed {
    /// An attribute's name: `external` in `@external(…)`.
    AttrName,
    /// An attribute's argument that names nothing: `native` in
    /// `@external(native, "f")`.
    AttrArg,
    /// A parameter's name in a function with no body (`@external`): a
    /// signature's label.
    Label,
    /// `Self` in an interface.
    SelfType,
    /// A package's name at the start of an import's path, `std` in
    /// `import std::io`, where no module has that path.
    Package,
}
