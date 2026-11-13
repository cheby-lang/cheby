//! Semantic tokens (X §5.5, D-1396): what each identifier of a module
//! names, for a client that colours by it.
//!
//! Tree-sitter stays the highlighter, so only `LOWER_IDENT` and
//! `UPPER_IDENT` tokens get a token: keywords, literals, comments and
//! operators are left to it (D-1396). One walk over the module's tokens
//! reads each identifier's occurrence in the index (F §5): its
//! [`NameKind`] gives the type, its [`Role`] the `declaration` modifier. A
//! token the index cannot place, such as a name in a broken tree or an
//! unresolved path, gets the kind its parent's syntax gives (L0, syntax
//! map §7.5's fallback row). A token directly inside an `ERROR` node, and
//! an attribute's name or argument, gets none, so tree-sitter's colour
//! stands.
//!
//! `readonly` is never set: every binding is immutable (D-017), so it says
//! nothing. Identifiers never span lines, so no token is multi-line.

use cheby_driver::analysis::{
    DefId, ItemSig, ModuleId, ModuleView, Snapshot, TextRange, TextSize, TypeDef, TypeDefKind,
};
use cheby_syntax::{SyntaxKind as K, SyntaxNode, SyntaxToken};

use crate::features::index::{
    Classified, ImplicitKind, Meaning, ModuleIndex, NameKind, Occurrence, Role, Unnamed,
};
use crate::features::{CancelToken, RequestError};

/// A token type of the legend, in legend order (X §5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TokenType {
    /// A module.
    Namespace,
    /// A type that is neither an enum nor a struct: a builtin, an external
    /// type or an alias.
    Type,
    /// A type with variants.
    Enum,
    /// A record: a type whose one variant has the type's name (§4.3.3).
    Struct,
    /// An interface.
    Interface,
    /// A type parameter, or `Self` in an interface.
    TypeParameter,
    /// A parameter of a function or a closure.
    Parameter,
    /// A local binding or a constant.
    Variable,
    /// A field.
    Property,
    /// A constructor of a type with variants.
    EnumMember,
    /// A function, a local function or a test.
    Function,
    /// A function of an interface.
    Method,
}

impl TokenType {
    /// Every type, in legend order.
    #[cfg(test)]
    pub(crate) const ALL: [Self; 12] = [
        Self::Namespace,
        Self::Type,
        Self::Enum,
        Self::Struct,
        Self::Interface,
        Self::TypeParameter,
        Self::Parameter,
        Self::Variable,
        Self::Property,
        Self::EnumMember,
        Self::Function,
        Self::Method,
    ];

    /// The name the legend gives the type.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Namespace => "namespace",
            Self::Type => "type",
            Self::Enum => "enum",
            Self::Struct => "struct",
            Self::Interface => "interface",
            Self::TypeParameter => "typeParameter",
            Self::Parameter => "parameter",
            Self::Variable => "variable",
            Self::Property => "property",
            Self::EnumMember => "enumMember",
            Self::Function => "function",
            Self::Method => "method",
        }
    }

    /// The type's index in the legend.
    pub(crate) const fn index(self) -> u32 {
        match self {
            Self::Namespace => 0,
            Self::Type => 1,
            Self::Enum => 2,
            Self::Struct => 3,
            Self::Interface => 4,
            Self::TypeParameter => 5,
            Self::Parameter => 6,
            Self::Variable => 7,
            Self::Property => 8,
            Self::EnumMember => 9,
            Self::Function => 10,
            Self::Method => 11,
        }
    }
}

/// A set of token modifiers, as the legend's bits (X §5.5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct Modifiers(u32);

impl Modifiers {
    /// No modifier.
    pub(crate) const NONE: Self = Self(0);
    /// The token declares its name.
    pub(crate) const DECLARATION: Self = Self(1);
    /// The item has `@deprecated` (D-200).
    pub(crate) const DEPRECATED: Self = Self(1 << 1);
    /// The definition is in std or the builtin module.
    pub(crate) const DEFAULT_LIBRARY: Self = Self(1 << 2);
    /// The function has `@async`.
    pub(crate) const ASYNC: Self = Self(1 << 3);
    /// The name is a value converted to a `dyn` type where it stands.
    pub(crate) const IMPLICIT_DYN: Self = Self(1 << 4);

    /// The legend's modifier names, bit 0 first.
    pub(crate) const NAMES: [&'static str; 5] = [
        "declaration",
        "deprecated",
        "defaultLibrary",
        "async",
        "implicitDyn",
    ];

    /// `self` with `other` added when `on`.
    #[must_use]
    pub(crate) const fn with(self, other: Self, on: bool) -> Self {
        if on { Self(self.0 | other.0) } else { self }
    }

    /// The bits, as the protocol sends them.
    pub(crate) const fn bits(self) -> u32 {
        self.0
    }

    /// The names of the bits set, in legend order.
    #[cfg(test)]
    pub(crate) fn names(self) -> Vec<&'static str> {
        Self::NAMES
            .iter()
            .enumerate()
            .filter(|&(bit, _)| u32::try_from(bit).is_ok_and(|bit| self.0 & (1 << bit) != 0))
            .map(|(_, &name)| name)
            .collect()
    }
}

/// One semantic token: an identifier and what it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Token {
    /// The identifier.
    pub range: TextRange,
    /// Its type.
    pub ty: TokenType,
    /// Its modifiers.
    pub mods: Modifiers,
}

/// The tokens of `module`, a module of `snapshot`, in text order. With
/// `within`, only the identifiers that overlap it (`semanticTokens/range`).
/// A module with no tree has none.
///
/// # Errors
///
/// [`RequestError::Cancelled`] when `cancel` is set, checked per
/// few hundred tokens.
pub(crate) fn tokens(
    snapshot: &Snapshot,
    module: &ModuleIndex,
    within: Option<TextRange>,
    cancel: &CancelToken,
) -> Result<Vec<Token>, RequestError> {
    let Some(view) = snapshot.module(module.module) else {
        return Ok(Vec::new());
    };
    let Some(tree) = view.tree() else {
        return Ok(Vec::new());
    };
    let text = module
        .file
        .and_then(|file| snapshot.source(file))
        .map_or("", |source| source.text());
    let cx = Cx {
        snapshot,
        module,
        converted: converted(module, text),
    };
    let mut out = Vec::new();
    for (n, token) in tree.root().tokens().enumerate() {
        // One check per few hundred tokens (A §3.5).
        if n % 256 == 0 {
            cancel.check()?;
        }
        if !matches!(token.kind(), K::LOWER_IDENT | K::UPPER_IDENT) {
            continue;
        }
        let range = token.text_range();
        if within.is_some_and(|w| !overlaps(range, w)) {
            continue;
        }
        if let Some((ty, mods)) = cx.classify(token) {
            out.push(Token { range, ty, mods });
        }
    }
    Ok(out)
}

/// Whether `range` has text in common with `within`.
const fn overlaps(range: TextRange, within: TextRange) -> bool {
    range.start() < within.end() && within.start() < range.end()
}

/// The ranges of the expressions converted to a `dyn` type in `module`,
/// whose text is `text`, without the blanks around them, sorted. Made
/// once per request, so each token is one search (X §5.5).
fn converted(module: &ModuleIndex, text: &str) -> Vec<(TextSize, TextSize)> {
    let mut out: Vec<(TextSize, TextSize)> = module
        .occurrences
        .iter()
        .filter(|o| o.role == Role::Implicit(ImplicitKind::Dyn))
        .filter_map(|o| {
            let inner = o.range.slice(text)?;
            let lead = inner.len().checked_sub(inner.trim_start().len())?;
            let trail = inner.len().checked_sub(inner.trim_end().len())?;
            let start = o
                .range
                .start()
                .checked_add(TextSize::try_from(lead).ok()?)?;
            let end = o.range.end().checked_sub(TextSize::try_from(trail).ok()?)?;
            (start < end).then_some((start, end))
        })
        .collect();
    out.sort_unstable();
    out
}

/// What classifying one module's identifiers reads.
struct Cx<'a> {
    snapshot: &'a Snapshot,
    module: &'a ModuleIndex,
    /// See [`converted`].
    converted: Vec<(TextSize, TextSize)>,
}

impl Cx<'_> {
    /// The type and modifiers of the identifier `token`, or `None` to
    /// leave it to tree-sitter.
    fn classify(&self, token: SyntaxToken<'_>) -> Option<(TokenType, Modifiers)> {
        match self.module.classify(token.text_range()) {
            Classified::Occurrence(occurrence) => self
                .placed(occurrence)
                .or_else(|| syntactic(token).map(|ty| (ty, decl_by_syntax(token)))),
            Classified::Unnamed(Unnamed::AttrName | Unnamed::AttrArg) => None,
            Classified::Unknown(_) | Classified::Unnamed(_) | Classified::Missing => {
                syntactic(token).map(|ty| (ty, decl_by_syntax(token)))
            }
        }
    }

    /// The token of a placed name. A punned field shows its value or its
    /// binding, which the field's own text also names (X §5.5, D-1392): a
    /// field pattern `{ x }` declares `x`.
    fn placed(&self, occurrence: &Occurrence) -> Option<(TokenType, Modifiers)> {
        let (id, decl) = match (occurrence.role, occurrence.also) {
            (Role::FieldInit, Some(value)) => (value, false),
            (Role::FieldPat, Some(binding)) => (binding, true),
            (role, _) => (occurrence.symbol, role.is_decl()),
        };
        let entry = self.module.entry(id)?;
        let ty = self.token_type(entry.kind, entry.meaning);
        let def = match entry.meaning {
            Meaning::Item(def) => Some(def),
            Meaning::Module(_)
            | Meaning::Field { .. }
            | Meaning::TypeParam { .. }
            | Meaning::Local { .. } => None,
        };
        let mods = Modifiers::NONE
            .with(Modifiers::DECLARATION, decl)
            .with(Modifiers::DEFAULT_LIBRARY, self.is_library(entry.meaning))
            .with(
                Modifiers::DEPRECATED,
                def.is_some_and(|d| self.is_deprecated(d)),
            )
            .with(Modifiers::ASYNC, def.is_some_and(|d| self.is_async(d)))
            .with(Modifiers::IMPLICIT_DYN, self.is_converted(occurrence.range));
        Some((ty, mods))
    }

    /// The type of a name of `kind` that means `meaning`.
    fn token_type(&self, kind: NameKind, meaning: Meaning) -> TokenType {
        match kind {
            NameKind::Module => TokenType::Namespace,
            NameKind::Function | NameKind::Test | NameKind::LocalFn => TokenType::Function,
            NameKind::Constant | NameKind::Local => TokenType::Variable,
            NameKind::Type => match meaning {
                Meaning::Item(def) => self.type_kind(def),
                Meaning::Module(_)
                | Meaning::Field { .. }
                | Meaning::TypeParam { .. }
                | Meaning::Local { .. } => TokenType::Type,
            },
            NameKind::Alias => TokenType::Type,
            NameKind::Interface => TokenType::Interface,
            NameKind::Constructor => match meaning {
                Meaning::Item(def) if self.is_record_ctor(def) => TokenType::Struct,
                Meaning::Item(_)
                | Meaning::Module(_)
                | Meaning::Field { .. }
                | Meaning::TypeParam { .. }
                | Meaning::Local { .. } => TokenType::EnumMember,
            },
            NameKind::InterfaceFn => TokenType::Method,
            NameKind::Field => TokenType::Property,
            NameKind::TypeParam => TokenType::TypeParameter,
            NameKind::Param => TokenType::Parameter,
        }
    }

    /// `struct` for a record, `enum` for another type with variants, and
    /// `type` for a builtin or external type.
    fn type_kind(&self, def: DefId) -> TokenType {
        match self.type_def(def) {
            Some(TypeDef {
                kind: TypeDefKind::Adt { .. },
                ..
            }) if self.is_record(def) => TokenType::Struct,
            Some(TypeDef {
                kind: TypeDefKind::Adt { .. },
                ..
            }) => TokenType::Enum,
            Some(_) | None => TokenType::Type,
        }
    }

    /// The definition of the type `def`.
    fn type_def(&self, def: DefId) -> Option<&TypeDef> {
        match &self
            .snapshot
            .interfaces()
            .get(def.module)?
            .item(def.index)?
            .sig
        {
            ItemSig::Type(ty) => Some(ty),
            _ => None,
        }
    }

    /// Whether the type `def` has exactly one constructor, named as the
    /// type: a record (§4.3.3).
    fn is_record(&self, def: DefId) -> bool {
        let Some(iface) = self.snapshot.interfaces().get(def.module) else {
            return false;
        };
        let Some(item) = iface.item(def.index) else {
            return false;
        };
        let ItemSig::Type(TypeDef {
            kind: TypeDefKind::Adt { ctors },
            ..
        }) = &item.sig
        else {
            return false;
        };
        match &**ctors {
            [only] => iface.item(*only).is_some_and(|ctor| ctor.name == item.name),
            _ => false,
        }
    }

    /// Whether `def` is the constructor of a record.
    fn is_record_ctor(&self, def: DefId) -> bool {
        let parent = self
            .snapshot
            .interfaces()
            .get(def.module)
            .and_then(|iface| iface.item(def.index))
            .and_then(|item| item.parent);
        parent.is_some_and(|index| {
            self.is_record(DefId {
                module: def.module,
                index,
            })
        })
    }

    /// Whether what `meaning` names is defined in std or the builtin
    /// module.
    fn is_library(&self, meaning: Meaning) -> bool {
        let module: ModuleId = match meaning {
            Meaning::Module(module) => module,
            Meaning::Item(def) | Meaning::Field { ty: def, .. } => def.module,
            Meaning::TypeParam { item, .. } => item.module,
            Meaning::Local { .. } => return false,
        };
        self.snapshot.module(module).is_some_and(|m| !m.is_local())
    }

    /// Whether the item `def` has `@deprecated` (D-200).
    fn is_deprecated(&self, def: DefId) -> bool {
        self.snapshot
            .interfaces()
            .get(def.module)
            .and_then(|iface| iface.item(def.index))
            .is_some_and(|item| item.deprecated.is_some())
    }

    /// Whether the function `def` has `@async`. Only a module this run
    /// resolved says so, since interfaces do not carry it.
    fn is_async(&self, def: DefId) -> bool {
        self.snapshot
            .module(def.module)
            .and_then(ModuleView::resolved)
            .and_then(|resolved| resolved.items.get(def.index))
            .is_some_and(|item| item.attrs.is_async)
    }

    /// Whether the expression that is exactly the name at `range` is
    /// converted to a `dyn` type (X §5.5): an implicit `dyn` occurrence
    /// over it with nothing else in its text.
    fn is_converted(&self, range: TextRange) -> bool {
        self.converted
            .binary_search(&(range.start(), range.end()))
            .is_ok()
    }
}

/// Whether the parent of `token` declares it: the L0 rows with
/// `declaration` (syntax map §7.5).
fn decl_by_syntax(token: SyntaxToken<'_>) -> Modifiers {
    let declares = token.parent().is_some_and(|parent| match parent.kind() {
        K::FN_DECL
        | K::LOCAL_FN
        | K::INTERFACE_FN
        | K::CONST_DECL
        | K::TYPE_DECL
        | K::ALIAS_DECL
        | K::INTERFACE_DECL
        | K::TYPE_PARAM
        | K::VARIANT
        | K::RECORD_FIELD
        | K::IDENT_PAT
        | K::AS_PAT
        | K::REST_PAT => true,
        K::FIELD_PAT => !has_token(parent, K::COLON),
        _ => false,
    });
    Modifiers::NONE.with(Modifiers::DECLARATION, declares)
}

/// The type the syntax gives the identifier `token`, with no resolution
/// (syntax map §7.5): its parent's row, else the fallback row. `None` for
/// an attribute's name or argument and for a token directly inside an
/// `ERROR` node, whose parent says nothing.
fn syntactic(token: SyntaxToken<'_>) -> Option<TokenType> {
    let parent = token.parent()?;
    let upper = token.kind() == K::UPPER_IDENT;
    Some(match parent.kind() {
        K::ATTR | K::INNER_ATTR | K::ATTR_ARG_LIST | K::ERROR => return None,
        K::FN_DECL | K::LOCAL_FN => TokenType::Function,
        K::INTERFACE_FN => TokenType::Method,
        K::TYPE_DECL => type_by_body(parent, token),
        K::ALIAS_DECL => TokenType::Type,
        K::INTERFACE_DECL => TokenType::Interface,
        K::TYPE_PARAM => TokenType::TypeParameter,
        K::VARIANT => TokenType::EnumMember,
        K::IDENT_PAT if parent.parent().is_some_and(|p| p.kind() == K::PARAM) => {
            TokenType::Parameter
        }
        // A punned field is also its value or binding (X §5.5).
        K::FIELD_INIT | K::FIELD_PAT if !has_token(parent, K::COLON) => TokenType::Variable,
        K::INTERPOLATION if first_ident(parent) == Some(token) => TokenType::Variable,
        K::CONST_DECL | K::IDENT_PAT | K::AS_PAT | K::REST_PAT => TokenType::Variable,
        K::RECORD_FIELD | K::FIELD_EXPR | K::FIELD_INIT | K::FIELD_PAT | K::INTERPOLATION => {
            TokenType::Property
        }
        K::MODULE_PATH => TokenType::Namespace,
        K::RENAME => match parent.parent().map(SyntaxNode::kind) {
            Some(K::IMPORT_DECL) => TokenType::Namespace,
            _ => imported(upper),
        },
        K::IMPORT_ITEM => imported(upper),
        K::PATH => in_path(parent, token, upper),
        _ => fallback(upper),
    })
}

/// An imported item's name that did not resolve.
const fn imported(upper: bool) -> TokenType {
    if upper {
        TokenType::Type
    } else {
        TokenType::Variable
    }
}

/// The fallback row of syntax map §7.5 for a name whose parent says
/// nothing more.
const fn fallback(upper: bool) -> TokenType {
    if upper {
        TokenType::Type
    } else {
        TokenType::Variable
    }
}

/// A segment of a `PATH` with no resolution: a qualifier is a module, or a
/// type when it is upper case; the last segment takes the kind of what
/// holds the path.
fn in_path(path: SyntaxNode<'_>, token: SyntaxToken<'_>, upper: bool) -> TokenType {
    let last = path
        .children_with_tokens()
        .filter_map(cheby_syntax::SyntaxElement::as_token)
        .filter(|t| matches!(t.kind(), K::LOWER_IDENT | K::UPPER_IDENT))
        .last();
    if last != Some(token) {
        return if upper {
            TokenType::Type
        } else {
            TokenType::Namespace
        };
    }
    let Some(holder) = path.parent() else {
        return fallback(upper);
    };
    match holder.kind() {
        K::PATH_TYPE if token.text() == "Self" => TokenType::TypeParameter,
        K::PATH_TYPE => TokenType::Type,
        K::INTERFACE_REF => TokenType::Interface,
        K::CTOR_PAT | K::RECORD_EXPR => TokenType::EnumMember,
        K::PATH_EXPR if upper => TokenType::EnumMember,
        K::PATH_EXPR if is_callee(holder) => TokenType::Function,
        _ => fallback(upper),
    }
}

/// Whether `expr` is the callee of a call.
fn is_callee(expr: SyntaxNode<'_>) -> bool {
    expr.parent()
        .is_some_and(|call| call.kind() == K::CALL_EXPR && call.first_child() == Some(expr))
}

/// `struct`, `enum` or `type` by a type declaration's body: a record's
/// fields, or variants, of which one with the type's name is a record
/// too (§4.3.3).
fn type_by_body(decl: SyntaxNode<'_>, name: SyntaxToken<'_>) -> TokenType {
    if decl.first_child_of_kind(K::RECORD_FIELD_LIST).is_some() {
        return TokenType::Struct;
    }
    let Some(variants) = decl.first_child_of_kind(K::VARIANT_LIST) else {
        return TokenType::Type;
    };
    let mut all = variants.children().filter(|v| v.kind() == K::VARIANT);
    match (all.next(), all.next()) {
        (Some(only), None) if first_ident(only).is_some_and(|t| t.text() == name.text()) => {
            TokenType::Struct
        }
        _ => TokenType::Enum,
    }
}

/// The first identifier token directly in `node`.
fn first_ident(node: SyntaxNode<'_>) -> Option<SyntaxToken<'_>> {
    node.children_with_tokens()
        .filter_map(cheby_syntax::SyntaxElement::as_token)
        .find(|t| matches!(t.kind(), K::LOWER_IDENT | K::UPPER_IDENT))
}

/// Whether `node` has a direct token of `kind`.
fn has_token(node: SyntaxNode<'_>, kind: K) -> bool {
    node.children_with_tokens()
        .filter_map(cheby_syntax::SyntaxElement::as_token)
        .any(|t| t.kind() == kind)
}
