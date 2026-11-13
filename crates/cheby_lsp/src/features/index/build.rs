//! One module's index: the token walk of F §5.1 and the classification of
//! F §5.3.
//!
//! The inputs are merged per module first: the path, binding and name
//! resolutions of every body and of the signatures, by node; item names by
//! their name's range; attribute names; and imports by their path's range.
//! Then every identifier token is classified by its parent's kind, and
//! last the facts of each body add the implicit occurrences.

use std::collections::HashMap;

use cheby_driver::analysis::{
    CtorFields, DefId, DefIndex, DefKind, ItemSig, LocalKind, Member, ModuleFacts, ModuleId,
    ModulePath, ModuleView, Name, Namespace, Res, ResolvedModule, ScopeSource, Snapshot, Symbol,
    SymbolNs, TextRange, builtin, thir,
};
use cheby_syntax::{NodeId, SyntaxElement, SyntaxKind as K, SyntaxNode, SyntaxToken};

use super::{
    ImplicitKind, Meaning, ModuleIndex, NameKind, Occurrence, Role, SymbolEntry, Unknown,
    UnknownName, Unnamed, UnnamedName,
};

/// The index of `view`, a module of `snapshot`. A module with no tree has
/// an empty index.
pub(super) fn module(snapshot: &Snapshot, view: ModuleView<'_>) -> ModuleIndex {
    let path = view.path().cloned().unwrap_or_else(|| ModulePath::root(""));
    let mut cx = Cx::new(snapshot, view, path);
    if let Some(tree) = view.tree() {
        for token in tree.root().tokens() {
            if matches!(token.kind(), K::LOWER_IDENT | K::UPPER_IDENT) {
                let outcome = cx.classify(token);
                cx.record(token.text_range(), token.text(), outcome);
            }
        }
        cx.implicit();
    }
    cx.out.finish()
}

/// A resolution of the merged maps: a local is its body and slot.
#[derive(Clone, Copy, Debug)]
enum Ref {
    Local(DefIndex, u32),
    Def(DefId),
    Module(ModuleId),
    TyParam,
    SelfTy,
    Err,
}

impl Ref {
    /// `res`, which is no local.
    const fn of(res: Res) -> Self {
        match res {
            Res::Def(def) => Self::Def(def),
            Res::Module(module) => Self::Module(module),
            Res::TyParam(_) => Self::TyParam,
            Res::SelfTy => Self::SelfTy,
            Res::Local(_) | Res::Err => Self::Err,
        }
    }
}

/// What an attribute's argument names (F §5.3, the `ATTR` row).
#[derive(Clone, Copy, Debug)]
enum AttrRef {
    /// A function of `@std_hooks(…)`.
    Hook,
    /// A parameter of the function `@std_clamped(…)` is on.
    Clamp(DefIndex),
}

/// A symbol with what it means and its kind, before interning.
#[derive(Clone, Debug)]
struct Target {
    symbol: Symbol,
    meaning: Meaning,
    kind: NameKind,
}

/// What a token is.
#[derive(Debug)]
enum Outcome {
    Named {
        primary: Target,
        also: Option<Target>,
        role: Role,
        /// One of a punned token's two meanings is unknown.
        unknown: Option<Unknown>,
    },
    Unknown(Unknown),
    Unnamed(Unnamed),
}

/// One meaning, or why there is none.
fn single(target: Result<Target, Unknown>, role: Role) -> Outcome {
    match target {
        Ok(primary) => Outcome::Named {
            primary,
            also: None,
            role,
            unknown: None,
        },
        Err(why) => Outcome::Unknown(why),
    }
}

/// Up to two meanings. A punned token whose field is unknown keeps its
/// value or binding, and is unknown too.
fn pair(
    first: Result<Target, Unknown>,
    second: Option<Result<Target, Unknown>>,
    role: Role,
) -> Outcome {
    match (first, second) {
        (first, None) => single(first, role),
        (Ok(primary), Some(Ok(also))) => Outcome::Named {
            primary,
            also: Some(also),
            role,
            unknown: None,
        },
        (Ok(primary), Some(Err(why))) | (Err(why), Some(Ok(primary))) => Outcome::Named {
            primary,
            also: None,
            role,
            unknown: Some(why),
        },
        (Err(why), Some(Err(_))) => Outcome::Unknown(why),
    }
}

/// The identifier tokens that are direct children of `node`.
fn idents(node: SyntaxNode<'_>) -> Vec<SyntaxToken<'_>> {
    node.children_with_tokens()
        .filter_map(|child| match child {
            SyntaxElement::Token(t) if matches!(t.kind(), K::LOWER_IDENT | K::UPPER_IDENT) => {
                Some(t)
            }
            SyntaxElement::Token(_) | SyntaxElement::Node(_) => None,
        })
        .collect()
}

/// Whether `node` has a direct token child of `kind`.
fn has_token(node: SyntaxNode<'_>, kind: K) -> bool {
    node.children_with_tokens()
        .any(|child| matches!(child, SyntaxElement::Token(t) if t.kind() == kind))
}

/// The namespace a path segment of this text looks in: type names start
/// upper case (§2.3).
fn ns_of(text: &str) -> Namespace {
    if text.starts_with(|c: char| c.is_ascii_uppercase()) {
        Namespace::Type
    } else {
        Namespace::Value
    }
}

/// The kind of an item of `kind`.
const fn item_kind(kind: DefKind) -> NameKind {
    match kind {
        DefKind::Fn => NameKind::Function,
        DefKind::Type => NameKind::Type,
        DefKind::Alias => NameKind::Alias,
        DefKind::Const => NameKind::Constant,
        DefKind::Interface => NameKind::Interface,
        DefKind::InterfaceFn => NameKind::InterfaceFn,
        DefKind::Ctor => NameKind::Constructor,
        DefKind::Test => NameKind::Test,
    }
}

/// The kind of a local of `kind`.
const fn local_kind(kind: LocalKind) -> NameKind {
    match kind {
        LocalKind::Param | LocalKind::ClosureParam => NameKind::Param,
        LocalKind::Let | LocalKind::CaseArm | LocalKind::UseBinder => NameKind::Local,
        LocalKind::LocalFn => NameKind::LocalFn,
    }
}

/// The item a call of `callee` runs, if it names one.
const fn callee_def(callee: &thir::Callee) -> Option<DefId> {
    match callee {
        thir::Callee::Fn { def, .. } => Some(*def),
        thir::Callee::Ctor { ctor, .. } => Some(*ctor),
        thir::Callee::Iface { iface_fn, .. } | thir::Callee::Dyn { iface_fn } => Some(*iface_fn),
        // A value of function type, and kinds added later, name no item.
        _ => None,
    }
}

/// The range of the interpolation that is part `part` of the string
/// literal `lit`, counted as `ExprKind::Interpolate` counts its parts: a
/// run of text is one part, and each interpolation one more.
fn interpolation_of(lit: SyntaxNode<'_>, part: u32) -> Option<TextRange> {
    let mut index = 0_u32;
    let mut text = false;
    for child in lit.children_with_tokens() {
        match child {
            SyntaxElement::Token(t) if matches!(t.kind(), K::STRING_TEXT | K::STRING_ESCAPE) => {
                text = true;
            }
            SyntaxElement::Node(n) if n.kind() == K::INTERPOLATION => {
                if text {
                    index = index.checked_add(1)?;
                    text = false;
                }
                if index == part {
                    return Some(n.text_range());
                }
                index = index.checked_add(1)?;
            }
            SyntaxElement::Token(_) | SyntaxElement::Node(_) => {}
        }
    }
    None
}

/// The index being built.
struct Out {
    index: ModuleIndex,
}

impl Out {
    /// The id of `target`, interned on first sight.
    fn intern(&mut self, target: Target) -> Option<u32> {
        if let Some(&id) = self.index.by_symbol.get(&target.symbol) {
            return Some(id);
        }
        let id = u32::try_from(self.index.symbols.len()).ok()?;
        self.index.by_symbol.insert(target.symbol.clone(), id);
        self.index.symbols.push(SymbolEntry {
            symbol: target.symbol,
            meaning: target.meaning,
            kind: target.kind,
        });
        Some(id)
    }

    /// The finished index, with its lists in order.
    fn finish(mut self) -> ModuleIndex {
        self.index
            .occurrences
            .sort_by_key(|o| (o.range.start(), o.role.is_implicit(), o.range.end()));
        self.index.unknown.sort_by_key(|u| u.range);
        self.index.unnamed.sort_by_key(|u| u.range);
        self.index
    }
}

/// The merged inputs of one module (F §5.3).
struct Cx<'s> {
    snapshot: &'s Snapshot,
    view: ModuleView<'s>,
    resolved: Option<&'s ResolvedModule>,
    facts: Option<&'s ModuleFacts>,
    /// Every `PATH` of the signatures and the bodies, with the body it is
    /// in.
    paths: HashMap<NodeId, Ref>,
    /// Every binding node.
    bindings: HashMap<NodeId, (DefIndex, u32)>,
    /// The names that are not paths: interpolation roots and punned
    /// field initializers.
    names: HashMap<NodeId, Ref>,
    /// Items by their declaration node. A record type's node is the type.
    items_by_node: HashMap<NodeId, DefIndex>,
    /// Items by their name's range: a record type and its constructor
    /// share one.
    items_by_name: HashMap<TextRange, Vec<DefIndex>>,
    attrs: HashMap<TextRange, AttrRef>,
    /// Imports by their module path's range.
    imports: HashMap<TextRange, usize>,
    out: Out,
}

impl<'s> Cx<'s> {
    fn new(snapshot: &'s Snapshot, view: ModuleView<'s>, path: ModulePath) -> Self {
        let resolved = view.resolved();
        let facts = view.facts();
        let lost = resolved.is_none()
            || facts.is_some_and(|f| !f.lost.is_empty() || !f.lost_passes.is_empty());
        let mut cx = Self {
            snapshot,
            view,
            resolved,
            facts,
            paths: HashMap::new(),
            bindings: HashMap::new(),
            names: HashMap::new(),
            items_by_node: HashMap::new(),
            items_by_name: HashMap::new(),
            attrs: HashMap::new(),
            imports: HashMap::new(),
            out: Out {
                index: ModuleIndex {
                    module: view.id(),
                    file: view.file(),
                    path,
                    symbols: Vec::new(),
                    occurrences: Vec::new(),
                    unknown: Vec::new(),
                    unnamed: Vec::new(),
                    lost,
                    by_symbol: HashMap::new(),
                },
            },
        };
        if let Some(resolved) = resolved {
            cx.merge(resolved);
        }
        cx
    }

    /// Fills the maps from `resolved` and the module's imports.
    fn merge(&mut self, resolved: &'s ResolvedModule) {
        for (&item, body) in &resolved.bodies {
            let slots: HashMap<_, u32> = body
                .locals
                .iter_enumerated()
                .zip(0_u32..)
                .map(|((local, _), slot)| (local, slot))
                .collect();
            let convert = |res: Res| match res {
                Res::Local(local) => slots
                    .get(&local)
                    .map_or(Ref::Err, |&slot| Ref::Local(item, slot)),
                other => Ref::of(other),
            };
            for (&node, &res) in &body.paths {
                self.paths.insert(node, convert(res));
            }
            for (&node, local) in &body.bindings {
                if let Some(&slot) = slots.get(local) {
                    self.bindings.insert(node, (item, slot));
                }
            }
            for (&node, &res) in &body.names {
                self.names.insert(node, convert(res));
            }
        }
        for (&node, &res) in &resolved.sig_paths {
            self.paths.entry(node).or_insert_with(|| Ref::of(res));
        }
        for (index, item) in resolved.items.iter_enumerated() {
            let ctor = item.kind == DefKind::Ctor;
            self.items_by_node
                .entry(item.node)
                .and_modify(|held| {
                    if !ctor {
                        *held = index;
                    }
                })
                .or_insert(index);
            self.items_by_name
                .entry(item.name_span.range)
                .or_default()
                .push(index);
            for (_, span) in item.attrs.hooks.iter().flatten() {
                self.attrs.insert(span.range, AttrRef::Hook);
            }
            for (_, span) in &item.attrs.clamped {
                self.attrs.insert(span.range, AttrRef::Clamp(index));
            }
        }
        for (n, request) in self.view.import_requests().iter().enumerate() {
            self.imports.insert(request.path_span.range, n);
        }
    }

    /// Records what `classify` found for the token at `range`.
    fn record(&mut self, range: TextRange, text: &str, outcome: Outcome) {
        let unknown = |why| UnknownName {
            range,
            text: text.into(),
            why,
        };
        match outcome {
            Outcome::Named {
                primary,
                also,
                role,
                unknown: why,
            } => {
                let Some(symbol) = self.out.intern(primary) else {
                    self.out.index.unknown.push(unknown(Unknown::NotRecorded));
                    return;
                };
                let also = also.and_then(|t| self.out.intern(t));
                self.out.index.occurrences.push(Occurrence {
                    range,
                    symbol,
                    also,
                    role,
                });
                if let Some(why) = why {
                    self.out.index.unknown.push(unknown(why));
                }
            }
            Outcome::Unknown(why) => self.out.index.unknown.push(unknown(why)),
            Outcome::Unnamed(what) => self.out.index.unnamed.push(UnnamedName { range, what }),
        }
    }

    /// What the identifier `token` is, by its parent's kind (F §5.3).
    fn classify(&self, token: SyntaxToken<'s>) -> Outcome {
        let Some(parent) = token.parent() else {
            return Outcome::Unknown(Unknown::NotRecorded);
        };
        match parent.kind() {
            K::ATTR | K::INNER_ATTR => return Outcome::Unnamed(Unnamed::AttrName),
            K::ERROR => return Outcome::Unknown(Unknown::InErrorNode),
            _ if self.resolved.is_none() => return Outcome::Unknown(Unknown::BodyLost),
            _ => {}
        }
        match parent.kind() {
            K::PATH => self.path_segment(parent, token),
            K::MODULE_PATH => self.module_segment(parent, token),
            K::IMPORT_ITEM => self.import_item(parent, token),
            K::RENAME => self.rename(parent, token),
            K::FN_DECL
            | K::TYPE_DECL
            | K::ALIAS_DECL
            | K::CONST_DECL
            | K::INTERFACE_DECL
            | K::VARIANT
            | K::INTERFACE_FN => self.declaration(parent, token),
            K::IDENT_PAT | K::AS_PAT | K::REST_PAT | K::LOCAL_FN => self.binding(parent),
            K::FIELD_EXPR => single(self.field(parent, token), Role::FieldRead),
            K::FIELD_INIT => {
                let value = (!has_token(parent, K::COLON)).then(|| self.name_of(parent, token));
                pair(self.field(parent, token), value, Role::FieldInit)
            }
            K::FIELD_PAT => {
                let binding = (!has_token(parent, K::COLON)).then(|| self.bound(parent));
                pair(self.field(parent, token), binding, Role::FieldPat)
            }
            K::RECORD_FIELD => single(self.record_field(parent, token), Role::Decl),
            K::INTERPOLATION => {
                if idents(parent).first() == Some(&token) {
                    single(self.name_of(parent, token), Role::InterpRoot)
                } else {
                    single(self.field(parent, token), Role::InterpField)
                }
            }
            K::TYPE_PARAM => {
                let holder = parent.parent().and_then(SyntaxNode::parent);
                let item = holder.and_then(|h| self.items_by_node.get(&h.id()).copied());
                single(
                    item.ok_or(Unknown::NotRecorded)
                        .and_then(|item| self.type_param(item, token.text())),
                    Role::Decl,
                )
            }
            K::ATTR_ARG_LIST => self.attr_arg(token),
            _ => Outcome::Unknown(self.missing(parent)),
        }
    }

    /// Why nothing was recorded for `node`.
    fn missing(&self, node: SyntaxNode<'_>) -> Unknown {
        if node.ancestors().any(|n| n.kind() == K::ERROR) {
            return Unknown::InErrorNode;
        }
        match (self.owner(node), self.facts) {
            (Some(item), Some(facts)) if facts.lost.iter().any(|&(lost, _)| lost == item) => {
                Unknown::BodyLost
            }
            _ => Unknown::NotRecorded,
        }
    }

    /// The top-level item whose node holds `node`.
    fn owner(&self, node: SyntaxNode<'_>) -> Option<DefIndex> {
        let top = node
            .ancestors()
            .find(|n| n.parent().is_some_and(|p| p.kind() == K::SOURCE_FILE))?;
        self.items_by_node.get(&top.id()).copied()
    }

    /// A segment of a `PATH` (F §5.3, the first two rows).
    fn path_segment(&self, path: SyntaxNode<'s>, token: SyntaxToken<'s>) -> Outcome {
        let segments = idents(path);
        let text = token.text();
        if segments.last() == Some(&token) {
            let Some(&found) = self.paths.get(&path.id()) else {
                return Outcome::Unknown(self.missing(path));
            };
            return match found {
                Ref::SelfTy => Outcome::Unnamed(Unnamed::SelfType),
                Ref::Err => Outcome::Unknown(Unknown::UnresolvedPath),
                Ref::TyParam => single(
                    self.owner(path)
                        .ok_or(Unknown::NotRecorded)
                        .and_then(|item| self.type_param(item, text)),
                    Role::Use,
                ),
                other => single(self.target_of(other, text), Role::Use),
            };
        }
        let Some(k) = segments.iter().position(|s| *s == token) else {
            return Outcome::Unknown(Unknown::NotRecorded);
        };
        let target = self.qualifier(&segments, k).map_err(|why| {
            if path.ancestors().any(|n| n.kind() == K::ERROR) {
                Unknown::InErrorNode
            } else {
                why
            }
        });
        single(target, Role::Qualifier)
    }

    /// Segment `k` of `segments`, which is not the last: the first through
    /// the module scope, each later one in the module before it.
    fn qualifier(&self, segments: &[SyntaxToken<'_>], k: usize) -> Result<Target, Unknown> {
        let first = segments.first().ok_or(Unknown::NotRecorded)?;
        let found = self.scope_ref(first.text())?;
        let mut target = self.target_of(found, first.text())?;
        for segment in segments.get(1..=k).into_iter().flatten() {
            let Meaning::Module(module) = target.meaning else {
                return Err(Unknown::NotRecorded);
            };
            let def = self
                .member(module, segment.text())
                .ok_or(Unknown::UnresolvedPath)?;
            target = self.item(def)?;
        }
        Ok(target)
    }

    /// What `text` names in the module scope, in the namespace its case
    /// picks.
    fn scope_ref(&self, text: &str) -> Result<Ref, Unknown> {
        let resolved = self.resolved.ok_or(Unknown::BodyLost)?;
        let name = self.lookup(text).ok_or(Unknown::UnresolvedPath)?;
        let entry = resolved
            .scope
            .get(ns_of(text), name)
            .ok_or(Unknown::UnresolvedPath)?;
        match entry.res {
            Res::Def(def) => Ok(Ref::Def(def)),
            Res::Module(module) => Ok(Ref::Module(module)),
            Res::Local(_) | Res::TyParam(_) | Res::SelfTy | Res::Err => {
                Err(Unknown::UnresolvedPath)
            }
        }
    }

    /// The item `text` of `module`, in the namespace its case picks. A
    /// builtin item homed at `module`, such as `Map` in `std::map`, counts
    /// (D-481).
    fn member(&self, module: ModuleId, text: &str) -> Option<DefId> {
        let name = self.lookup(text)?;
        let found = self
            .snapshot
            .interfaces()
            .get(module)
            .and_then(|iface| iface.lookup(ns_of(text), name))
            .map(|index| DefId::new(module, index));
        found.or_else(|| {
            let path = &self.snapshot.table().get(module)?.path;
            builtin::homed_at(path)
                .into_iter()
                .find(|&def| self.item_name(def) == Some(name))
        })
    }

    /// The name of the item `def`.
    fn item_name(&self, def: DefId) -> Option<Name> {
        let iface = self.snapshot.interfaces().get(def.module)?;
        Some(iface.item(def.index)?.name)
    }

    /// The interned name of `text`, if any name has it.
    fn lookup(&self, text: &str) -> Option<Name> {
        self.snapshot.globals().names.lookup(text)
    }

    /// The text of `name`.
    fn text_of(&self, name: Name) -> &'static str {
        self.snapshot.globals().names.get(name)
    }

    /// What a use written `text` that resolved to `found` names. A use
    /// whose text differs from what it names goes through an import alias
    /// (F §5.3).
    fn target_of(&self, found: Ref, text: &str) -> Result<Target, Unknown> {
        let target = match found {
            Ref::Local(item, slot) => return self.local(item, slot),
            Ref::Def(def) => self.item(def)?,
            Ref::Module(module) => self.module_target(module)?,
            Ref::TyParam | Ref::SelfTy => return Err(Unknown::NotRecorded),
            Ref::Err => return Err(Unknown::UnresolvedPath),
        };
        if target.symbol.text() == text {
            return Ok(target);
        }
        let ns = match target.meaning {
            Meaning::Item(def) => self.alias_ns(def),
            _ => SymbolNs::Value,
        };
        Ok(Target {
            symbol: Symbol::import_alias(self.out.index.path.clone(), ns, text),
            ..target
        })
    }

    /// The namespace an import alias of `def` is bound in.
    fn alias_ns(&self, def: DefId) -> SymbolNs {
        match self.def_kind(def).and_then(DefKind::namespace) {
            Some(Namespace::Type) => SymbolNs::Type,
            Some(Namespace::Value) | None => SymbolNs::Value,
        }
    }

    /// The kind of the item `def`.
    fn def_kind(&self, def: DefId) -> Option<DefKind> {
        let iface = self.snapshot.interfaces().get(def.module)?;
        Some(iface.item(def.index)?.kind)
    }

    /// The item `def` itself.
    fn item(&self, def: DefId) -> Result<Target, Unknown> {
        let symbol = self.snapshot.key_of(def).ok_or(Unknown::NotRecorded)?;
        let kind = self.def_kind(def).map_or(NameKind::Type, item_kind);
        Ok(Target {
            symbol,
            meaning: Meaning::Item(def),
            kind,
        })
    }

    /// The module `module` itself.
    fn module_target(&self, module: ModuleId) -> Result<Target, Unknown> {
        let info = self
            .snapshot
            .table()
            .get(module)
            .ok_or(Unknown::NotRecorded)?;
        Ok(Target {
            symbol: Symbol::of_module(info.path.clone()),
            meaning: Meaning::Module(module),
            kind: NameKind::Module,
        })
    }

    /// The local `slot` of the body of `item` (D-1365): its item, name and
    /// ordinal among the item's locals of that name, in source order.
    fn local(&self, item: DefIndex, slot: u32) -> Result<Target, Unknown> {
        let resolved = self.resolved.ok_or(Unknown::BodyLost)?;
        let body = resolved.bodies.get(&item).ok_or(Unknown::NotRecorded)?;
        let at = usize::try_from(slot).map_err(|_| Unknown::NotRecorded)?;
        let decl = body.locals.iter().nth(at).ok_or(Unknown::NotRecorded)?;
        let key = (decl.span.range.start(), at);
        let before = body
            .locals
            .iter()
            .enumerate()
            .filter(|&(n, other)| other.name == decl.name && (other.span.range.start(), n) < key)
            .count();
        let ordinal = u32::try_from(before).map_err(|_| Unknown::NotRecorded)?;
        let owner = self
            .snapshot
            .key_of(resolved.def_id(item))
            .ok_or(Unknown::NotRecorded)?;
        Ok(Target {
            symbol: owner.with_member(Member::Local {
                name: self.text_of(decl.name).into(),
                ordinal,
            }),
            meaning: Meaning::Local { item, slot },
            kind: local_kind(decl.kind),
        })
    }

    /// The field `name` of the type `ty`, over all its variants (D-071).
    fn field_of(&self, ty: DefId, name: Name) -> Result<Target, Unknown> {
        let owner = self.snapshot.key_of(ty).ok_or(Unknown::NotRecorded)?;
        Ok(Target {
            symbol: owner.with_member(Member::Field(self.text_of(name).into())),
            meaning: Meaning::Field { ty, name },
            kind: NameKind::Field,
        })
    }

    /// The type parameter written `text` of the item `item`.
    fn type_param(&self, item: DefIndex, text: &str) -> Result<Target, Unknown> {
        let resolved = self.resolved.ok_or(Unknown::BodyLost)?;
        let name = self.lookup(text).ok_or(Unknown::NotRecorded)?;
        let &(_, param) = resolved
            .generics
            .get(&item)
            .and_then(|params| params.iter().find(|(n, _)| *n == name))
            .ok_or(Unknown::NotRecorded)?;
        let def = resolved.def_id(item);
        let owner = self.snapshot.key_of(def).ok_or(Unknown::NotRecorded)?;
        Ok(Target {
            symbol: owner.with_member(Member::TypeParam(text.into())),
            meaning: Meaning::TypeParam { item: def, param },
            kind: NameKind::TypeParam,
        })
    }

    /// The field at `token`, from the facts of the body that holds it.
    fn field(&self, node: SyntaxNode<'_>, token: SyntaxToken<'_>) -> Result<Target, Unknown> {
        let item = self.owner(node).ok_or_else(|| self.missing(node))?;
        let facts = self.facts.ok_or(Unknown::Unchecked)?;
        let Some(body) = facts.body(item) else {
            return Err(if facts.lost.iter().any(|&(lost, _)| lost == item) {
                Unknown::BodyLost
            } else {
                Unknown::Unchecked
            });
        };
        let fact = body
            .field_at(token.text_range().start())
            .ok_or(Unknown::FieldOfUnknownType)?;
        let ty = fact.owner.ok_or(Unknown::FieldOfUnknownType)?;
        if !self.declares_field(ty, fact.name) {
            return Err(Unknown::FieldOfUnknownType);
        }
        self.field_of(ty, fact.name)
    }

    /// Whether some constructor of the type `ty` declares the field
    /// `name`. A field the type lacks is a typo nothing can place.
    fn declares_field(&self, ty: DefId, name: Name) -> bool {
        self.snapshot
            .interfaces()
            .get(ty.module)
            .is_some_and(|iface| {
                iface.items.iter().any(|item| match &item.sig {
                    ItemSig::Ctor(ctor) if item.parent == Some(ty.index) => match &ctor.fields {
                        CtorFields::Named(fields) => fields.iter().any(|(n, _)| *n == name),
                        CtorFields::Unit | CtorFields::Positional(_) => false,
                    },
                    _ => false,
                })
            })
    }

    /// What the name-reference node `node` resolved to: an interpolation's
    /// root or a punned field initializer's value.
    fn name_of(&self, node: SyntaxNode<'_>, token: SyntaxToken<'_>) -> Result<Target, Unknown> {
        let &found = self
            .names
            .get(&node.id())
            .ok_or_else(|| self.missing(node))?;
        self.target_of(found, token.text())
    }

    /// The local the binding node `node` binds. A top-level parameter is
    /// keyed by its `PARAM`.
    fn bound(&self, node: SyntaxNode<'_>) -> Result<Target, Unknown> {
        let key = self.bindings.get(&node.id()).or_else(|| {
            node.parent()
                .filter(|p| p.kind() == K::PARAM)
                .and_then(|p| self.bindings.get(&p.id()))
        });
        match key {
            Some(&(item, slot)) => self.local(item, slot),
            None => Err(self.missing(node)),
        }
    }

    /// A binding's name, or a label of a function with no body.
    fn binding(&self, node: SyntaxNode<'_>) -> Outcome {
        match self.bound(node) {
            Err(_) if is_label(node) => Outcome::Unnamed(Unnamed::Label),
            found => single(found, Role::Decl),
        }
    }

    /// An item's name where it is declared. A record type comes first,
    /// with its constructor as the second meaning.
    fn declaration(&self, parent: SyntaxNode<'_>, token: SyntaxToken<'_>) -> Outcome {
        let Some(found) = self.items_by_name.get(&token.text_range()) else {
            return Outcome::Unknown(self.missing(parent));
        };
        let Some(resolved) = self.resolved else {
            return Outcome::Unknown(Unknown::BodyLost);
        };
        let mut defs: Vec<DefIndex> = found.clone();
        defs.sort_by_key(|&index| {
            resolved
                .item(index)
                .is_some_and(|item| item.kind == DefKind::Ctor)
        });
        let mut targets = defs
            .into_iter()
            .map(|index| self.item(resolved.def_id(index)));
        match targets.next() {
            Some(first) => pair(first, targets.next(), Role::Decl),
            None => Outcome::Unknown(Unknown::NotRecorded),
        }
    }

    /// A field's name where a record or variant declares it.
    fn record_field(
        &self,
        parent: SyntaxNode<'_>,
        token: SyntaxToken<'_>,
    ) -> Result<Target, Unknown> {
        let resolved = self.resolved.ok_or(Unknown::BodyLost)?;
        let holder = parent
            .parent()
            .and_then(SyntaxNode::parent)
            .ok_or(Unknown::NotRecorded)?;
        let &index = self
            .items_by_node
            .get(&holder.id())
            .ok_or_else(|| self.missing(holder))?;
        let item = resolved.item(index).ok_or(Unknown::NotRecorded)?;
        let ty = match item.kind {
            DefKind::Ctor => item.parent.ok_or(Unknown::NotRecorded)?,
            _ => index,
        };
        let name = self.lookup(token.text()).ok_or(Unknown::NotRecorded)?;
        self.field_of(resolved.def_id(ty), name)
    }

    /// The import whose module path is `path`'s, by index.
    fn import_index(&self, decl: Option<SyntaxNode<'_>>) -> Option<usize> {
        let path = decl?.children().find(|n| n.kind() == K::MODULE_PATH)?;
        self.imports.get(&path.text_range()).copied()
    }

    /// The module import `n` names.
    fn import_target(&self, n: Option<usize>) -> Option<ModuleId> {
        self.view.imports().get(n?)?.module()
    }

    /// A segment of an import's module path: the module that the path up
    /// to it names.
    fn module_segment(&self, path: SyntaxNode<'_>, token: SyntaxToken<'_>) -> Outcome {
        let segments = idents(path);
        let text = token.text();
        if segments.last() == Some(&token) {
            let n = self.imports.get(&path.text_range()).copied();
            return match self.import_target(n) {
                Some(module) => single(self.module_target(module), Role::Import),
                None => Outcome::Unknown(Unknown::UnresolvedPath),
            };
        }
        let Some(k) = segments.iter().position(|s| *s == token) else {
            return Outcome::Unknown(Unknown::NotRecorded);
        };
        let mut prefix: Option<ModulePath> = None;
        for segment in segments.get(..=k).into_iter().flatten() {
            prefix = Some(match prefix {
                None => ModulePath::root(segment.text()),
                Some(p) => p.child(segment.text()),
            });
        }
        match prefix.and_then(|p| self.snapshot.table().find(&p)) {
            Some(module) => single(self.module_target(module), Role::Import),
            None if k == 0 && self.is_package_name(text) => Outcome::Unnamed(Unnamed::Package),
            None => Outcome::Unknown(Unknown::UnresolvedPath),
        }
    }

    /// Whether `text` is std's name or this package's.
    fn is_package_name(&self, text: &str) -> bool {
        text == "std"
            || self
                .out
                .index
                .path
                .segments()
                .first()
                .is_some_and(|first| first == text)
    }

    /// The items the import `n` binds as `bound`, in each namespace: the
    /// type first.
    fn imported(&self, n: Option<usize>, bound: &str) -> Vec<DefId> {
        let (Some(n), Some(resolved), Some(name)) = (n, self.resolved, self.lookup(bound)) else {
            return Vec::new();
        };
        [Namespace::Type, Namespace::Value]
            .into_iter()
            .filter_map(|ns| resolved.scope.get(ns, name))
            .filter(|entry| entry.source == ScopeSource::Import(n))
            .filter_map(|entry| match entry.res {
                Res::Def(def) => Some(def),
                _ => None,
            })
            .collect()
    }

    /// An imported item's name in `import m::{name}`: the item itself.
    fn import_item(&self, item: SyntaxNode<'_>, token: SyntaxToken<'_>) -> Outcome {
        let decl = item.parent().and_then(SyntaxNode::parent);
        let n = self.import_index(decl);
        let bound = item
            .children()
            .find(|c| c.kind() == K::RENAME)
            .and_then(|rename| idents(rename).first().map(|t| t.text()))
            .unwrap_or_else(|| token.text());
        let mut targets = self
            .imported(n, bound)
            .into_iter()
            .map(|def| self.item(def));
        match targets.next() {
            Some(first) => pair(first, targets.next(), Role::Import),
            None => Outcome::Unknown(Unknown::UnresolvedPath),
        }
    }

    /// An import's `as` name: the alias, which means what it names.
    fn rename(&self, rename: SyntaxNode<'_>, token: SyntaxToken<'_>) -> Outcome {
        let text = token.text();
        let path = &self.out.index.path;
        let Some(holder) = rename.parent() else {
            return Outcome::Unknown(Unknown::NotRecorded);
        };
        if holder.kind() == K::IMPORT_DECL {
            let module = self.import_target(self.import_index(Some(holder)));
            return single(
                module
                    .ok_or(Unknown::UnresolvedPath)
                    .and_then(|m| self.module_target(m))
                    .map(|target| Target {
                        symbol: Symbol::import_alias(path.clone(), SymbolNs::Value, text),
                        ..target
                    }),
                Role::Decl,
            );
        }
        let decl = holder.parent().and_then(SyntaxNode::parent);
        let mut targets = self
            .imported(self.import_index(decl), text)
            .into_iter()
            .map(|def| {
                self.item(def).map(|target| Target {
                    symbol: Symbol::import_alias(path.clone(), self.alias_ns(def), text),
                    ..target
                })
            });
        match targets.next() {
            Some(first) => pair(first, targets.next(), Role::Decl),
            None => Outcome::Unknown(Unknown::UnresolvedPath),
        }
    }

    /// A name in an attribute's arguments: a function of `@std_hooks`, a
    /// parameter of `@std_clamped`, or nothing.
    fn attr_arg(&self, token: SyntaxToken<'_>) -> Outcome {
        let text = token.text();
        match self.attrs.get(&token.text_range()) {
            None => Outcome::Unnamed(Unnamed::AttrArg),
            Some(AttrRef::Hook) => single(
                self.scope_ref(text)
                    .and_then(|found| self.target_of(found, text)),
                Role::Attr,
            ),
            // A function with no body, an `@external`, has labels, not
            // parameters, so the name binds nothing.
            Some(&AttrRef::Clamp(item))
                if self
                    .resolved
                    .is_some_and(|resolved| !resolved.bodies.contains_key(&item)) =>
            {
                Outcome::Unnamed(Unnamed::AttrArg)
            }
            Some(&AttrRef::Clamp(item)) => single(self.param(item, text), Role::Attr),
        }
    }

    /// The parameter written `text` of the function `item`.
    fn param(&self, item: DefIndex, text: &str) -> Result<Target, Unknown> {
        let resolved = self.resolved.ok_or(Unknown::BodyLost)?;
        let body = resolved.bodies.get(&item).ok_or(Unknown::NotRecorded)?;
        let slot = body
            .locals
            .iter_enumerated()
            .position(|(local, decl)| {
                body.params.contains(&local) && self.text_of(decl.name) == text
            })
            .ok_or(Unknown::NotRecorded)?;
        self.local(item, u32::try_from(slot).map_err(|_| Unknown::NotRecorded)?)
    }

    /// The occurrences no identifier writes, from each body's facts: an
    /// operator's callee, an interpolation's `show`, a `dyn` conversion's
    /// interface (F §5.3).
    fn implicit(&mut self) {
        let (Some(facts), Some(tree)) = (self.facts, self.view.tree()) else {
            return;
        };
        let mut found: Vec<(TextRange, DefId, ImplicitKind)> = Vec::new();
        for body in &facts.bodies {
            for op in &body.ops {
                let token = tree
                    .node(op.node)
                    .filter(|n| matches!(n.kind(), K::BIN_EXPR | K::PREFIX_EXPR))
                    .and_then(operator_token);
                if let (Some(token), Some(def)) = (token, op.callee.as_ref().and_then(callee_def)) {
                    found.push((token.text_range(), def, ImplicitKind::Operator));
                }
            }
            for show in &body.shows {
                let range = tree
                    .node(show.node)
                    .filter(|n| n.kind() == K::STRING_LIT)
                    .and_then(|lit| interpolation_of(lit, show.part));
                if let (Some(range), Some(def)) = (range, callee_def(&show.callee)) {
                    found.push((range, def, ImplicitKind::Show));
                }
            }
            for conversion in &body.dyns {
                if let Some(node) = tree.node(conversion.node) {
                    found.push((node.text_range(), conversion.iface, ImplicitKind::Dyn));
                }
            }
        }
        for (range, def, kind) in found {
            if let Ok(target) = self.item(def)
                && let Some(symbol) = self.out.intern(target)
            {
                self.out.index.occurrences.push(Occurrence {
                    range,
                    symbol,
                    also: None,
                    role: Role::Implicit(kind),
                });
            }
        }
    }
}

/// The operator token of a binary or prefix expression: its first direct
/// token that is not trivia.
fn operator_token(node: SyntaxNode<'_>) -> Option<SyntaxToken<'_>> {
    node.children_with_tokens().find_map(|child| match child {
        SyntaxElement::Token(t) if !t.kind().is_trivia() && t.kind() != K::NEWLINE => Some(t),
        SyntaxElement::Token(_) | SyntaxElement::Node(_) => None,
    })
}

/// Whether the binding `node` is a parameter's name in a function with no
/// body: a label of the signature, which binds nothing.
fn is_label(node: SyntaxNode<'_>) -> bool {
    let decl = node
        .parent()
        .filter(|p| p.kind() == K::PARAM)
        .and_then(SyntaxNode::parent)
        .filter(|list| list.kind() == K::PARAM_LIST)
        .and_then(SyntaxNode::parent);
    decl.is_some_and(|d| d.kind() == K::FN_DECL && !d.children().any(|c| c.kind() == K::BLOCK))
}
