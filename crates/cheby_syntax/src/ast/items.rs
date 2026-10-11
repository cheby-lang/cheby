//! Views of the source file, its header and its items (App. A.1, A.2,
//! ch. 4).
//!
//! An item node starts at its first attribute, so attributes are children
//! of the item. Its doc comments are the `///` lines right before it, which
//! the tree builder puts inside the item node.

use super::{
    AstChildren, AstNode, AstToken, AstTokens, Block, DocComment, Expr, Ident, ModuleDoc, Name,
    Pat, PathNode, StringLit, Type, TypeArgList, TypeName, child, child_token, children,
    leading_docs, token,
};
use crate::{SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};

ast_nodes! {
    /// The root of a file (App. A.1).
    SourceFile = SOURCE_FILE;
    /// `@!name(args)`, an inner attribute (D-141, D-201).
    InnerAttr = INNER_ATTR;
    /// `@name(args)`, an attribute of the item that follows (§2.7, §4.8).
    Attr = ATTR;
    /// The `(…)` arguments of an attribute.
    AttrArgList = ATTR_ARG_LIST;
    /// `import a::b`, `import a::b as c` or `import a::b::{X, y}` (§7.5).
    ImportDecl = IMPORT_DECL;
    /// The `a::b` module path of an import.
    ModulePath = MODULE_PATH;
    /// The `{X, y as z}` list of an import.
    ImportList = IMPORT_LIST;
    /// One name of an import list, with its optional rename.
    ImportItem = IMPORT_ITEM;
    /// `as name` after an import or an imported name.
    Rename = RENAME;
    /// A function declaration (§4.2).
    FnDecl = FN_DECL;
    /// A type declaration, with variants, the record shorthand or no body
    /// (§4.3).
    TypeDecl = TYPE_DECL;
    /// A type alias `type A = T` (§4.4).
    AliasDecl = ALIAS_DECL;
    /// A constant `const name: T = expr` (§4.5).
    ConstDecl = CONST_DECL;
    /// An interface declaration (§4.6, ch. 8).
    InterfaceDecl = INTERFACE_DECL;
    /// A test `test "name" { … }` (§4.7).
    TestDecl = TEST_DECL;
    /// `pub` or `priv` (D-162).
    Visibility = VISIBILITY;
    /// The `<A, B: Show>` type parameters of a declaration.
    TypeParamList = TYPE_PARAM_LIST;
    /// One type parameter, with its optional bounds.
    TypeParam = TYPE_PARAM;
    /// `Show + Eq`, the bounds of a type parameter or interface.
    BoundList = BOUND_LIST;
    /// An interface named in a bound or a `dyn` type.
    InterfaceRef = INTERFACE_REF;
    /// The `(…)` parameters of a function, closure or local function.
    ParamList = PARAM_LIST;
    /// One parameter: a pattern with an optional type (D-242).
    Param = PARAM;
    /// `-> T`, a return type.
    RetType = RET_TYPE;
    /// The `{ A, B(T) }` variants of a type declaration.
    VariantList = VARIANT_LIST;
    /// One variant of a type declaration (§4.3.1).
    Variant = VARIANT;
    /// The `(T, U)` fields of a positional variant.
    PosFieldList = POS_FIELD_LIST;
    /// The `{ x: T }` fields of a record type or a variant.
    RecordFieldList = RECORD_FIELD_LIST;
    /// One named field `x: T`.
    RecordField = RECORD_FIELD;
    /// The `{ fn … }` body of an interface.
    InterfaceBody = INTERFACE_BODY;
    /// A function signature in an interface body.
    InterfaceFn = INTERFACE_FN;
    /// The `(T, U)` parameter types of a function type or interface
    /// function.
    TypeList = TYPE_LIST;
}

ast_enum! {
    /// A top-level item (App. A.2).
    Item {
        Fn(FnDecl),
        Type(TypeDecl),
        Alias(AliasDecl),
        Const(ConstDecl),
        Interface(InterfaceDecl),
        Test(TestDecl),
    }
}

/// Adds `doc_comments` and `attrs` to item-like views.
macro_rules! docs_and_attrs {
    ($($name:ident),*) => {$(
        impl<'t> $name<'t> {
            /// The `///` doc comments before the node (§2.2).
            pub fn doc_comments(self) -> impl Iterator<Item = DocComment<'t>> + 't {
                leading_docs(self.0)
            }

            /// The attributes before the node (§2.7, §4.8).
            #[must_use]
            pub fn attrs(self) -> AstChildren<'t, Attr<'t>> {
                children(self.0)
            }
        }
    )*};
}

docs_and_attrs!(
    FnDecl,
    TypeDecl,
    AliasDecl,
    ConstDecl,
    InterfaceDecl,
    TestDecl,
    Variant,
    RecordField,
    InterfaceFn
);

/// Adds `visibility` to views that may have one.
macro_rules! has_visibility {
    ($($name:ident),*) => {$(
        impl<'t> $name<'t> {
            /// `pub` or `priv`, if written (D-162).
            #[must_use]
            pub fn visibility(self) -> Option<Visibility<'t>> {
                child(self.0)
            }
        }
    )*};
}

has_visibility!(
    FnDecl,
    TypeDecl,
    AliasDecl,
    ConstDecl,
    InterfaceDecl,
    TestDecl
);

impl<'t> SourceFile<'t> {
    /// Wraps the root node of a tree, which is always a `SOURCE_FILE`.
    pub(crate) const fn new(root: SyntaxNode<'t>) -> Self {
        Self(root)
    }

    /// The `//!` module doc comments directly in the file (§2.2).
    #[must_use]
    pub fn module_docs(self) -> AstTokens<'t, ModuleDoc<'t>> {
        AstTokens::new(self.0)
    }

    /// The inner attributes, in order.
    #[must_use]
    pub fn inner_attrs(self) -> AstChildren<'t, InnerAttr<'t>> {
        children(self.0)
    }

    /// The imports, in order.
    #[must_use]
    pub fn imports(self) -> AstChildren<'t, ImportDecl<'t>> {
        children(self.0)
    }

    /// The items, in order.
    #[must_use]
    pub fn items(self) -> AstChildren<'t, Item<'t>> {
        children(self.0)
    }
}

/// The name and arguments shared by attributes and inner attributes.
macro_rules! attr_accessors {
    ($($name:ident),*) => {$(
        impl<'t> $name<'t> {
            /// The attribute's name, such as `external`.
            #[must_use]
            pub fn name(self) -> Option<Ident<'t>> {
                child_token(self.0)
            }

            /// The `(…)` arguments, if written.
            #[must_use]
            pub fn arg_list(self) -> Option<AttrArgList<'t>> {
                child(self.0)
            }

            /// The arguments, in order. Empty if there are none.
            pub fn args(self) -> impl Iterator<Item = AttrArg<'t>> + 't {
                self.arg_list().into_iter().flat_map(AttrArgList::args)
            }
        }
    )*};
}

attr_accessors!(Attr, InnerAttr);

/// One argument of an attribute: a name or a string (App. A.2).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AttrArg<'t> {
    Name(Ident<'t>),
    String(StringLit<'t>),
}

impl<'t> AttrArgList<'t> {
    /// The arguments, in order.
    pub fn args(self) -> impl Iterator<Item = AttrArg<'t>> + 't {
        self.0
            .children_with_tokens()
            .filter_map(|child| match child {
                SyntaxElement::Token(token) => Ident::cast(token).map(AttrArg::Name),
                SyntaxElement::Node(node) => StringLit::cast(node).map(AttrArg::String),
            })
    }
}

impl<'t> ImportDecl<'t> {
    /// The imported module's path.
    #[must_use]
    pub fn module_path(self) -> Option<ModulePath<'t>> {
        child(self.0)
    }

    /// The `{…}` list of imported names, if written.
    #[must_use]
    pub fn list(self) -> Option<ImportList<'t>> {
        child(self.0)
    }

    /// The `as name` of the module, if written.
    #[must_use]
    pub fn rename(self) -> Option<Rename<'t>> {
        child(self.0)
    }
}

impl<'t> ModulePath<'t> {
    /// The segments, such as `std` and `io` in `std::io`.
    #[must_use]
    pub fn segments(self) -> AstTokens<'t, Ident<'t>> {
        AstTokens::new(self.0)
    }

    /// The last segment, which names the module in the importing file.
    #[must_use]
    pub fn last(self) -> Option<Ident<'t>> {
        self.segments().last()
    }
}

impl<'t> ImportList<'t> {
    /// The imported names, in order.
    #[must_use]
    pub fn items(self) -> AstChildren<'t, ImportItem<'t>> {
        children(self.0)
    }
}

impl<'t> ImportItem<'t> {
    /// The imported name.
    #[must_use]
    pub fn name(self) -> Option<Name<'t>> {
        child_token(self.0)
    }

    /// The `as name`, if written.
    #[must_use]
    pub fn rename(self) -> Option<Rename<'t>> {
        child(self.0)
    }
}

impl<'t> Rename<'t> {
    /// The new name.
    #[must_use]
    pub fn name(self) -> Option<Name<'t>> {
        child_token(self.0)
    }
}

/// Whether a visibility is `pub` or `priv` (D-162).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum VisibilityKind {
    Pub,
    Priv,
}

impl<'t> Visibility<'t> {
    /// `pub` or `priv`.
    #[must_use]
    pub fn kind(self) -> Option<VisibilityKind> {
        match self.keyword()?.kind() {
            SyntaxKind::PUB_KW => Some(VisibilityKind::Pub),
            SyntaxKind::PRIV_KW => Some(VisibilityKind::Priv),
            _ => None,
        }
    }

    /// The `pub` or `priv` keyword.
    #[must_use]
    pub fn keyword(self) -> Option<SyntaxToken<'t>> {
        token(self.0, SyntaxKind::PUB_KW).or_else(|| token(self.0, SyntaxKind::PRIV_KW))
    }

    /// Whether this is `pub`.
    #[must_use]
    pub fn is_pub(self) -> bool {
        self.kind() == Some(VisibilityKind::Pub)
    }
}

impl<'t> Item<'t> {
    /// The `///` doc comments before the item.
    pub fn doc_comments(self) -> impl Iterator<Item = DocComment<'t>> + 't {
        leading_docs(self.syntax())
    }

    /// The attributes of the item.
    #[must_use]
    pub fn attrs(self) -> AstChildren<'t, Attr<'t>> {
        children(self.syntax())
    }

    /// `pub` or `priv`, if written.
    #[must_use]
    pub fn visibility(self) -> Option<Visibility<'t>> {
        child(self.syntax())
    }

    /// The keyword that introduces the item: `fn`, `type` (types and
    /// aliases), `const`, `interface` or `test`.
    #[must_use]
    pub fn keyword(self) -> Option<SyntaxToken<'t>> {
        let kind = match self {
            Self::Fn(_) => SyntaxKind::FN_KW,
            Self::Type(_) | Self::Alias(_) => SyntaxKind::TYPE_KW,
            Self::Const(_) => SyntaxKind::CONST_KW,
            Self::Interface(_) => SyntaxKind::INTERFACE_KW,
            Self::Test(_) => SyntaxKind::TEST_KW,
        };
        token(self.syntax(), kind)
    }

    /// The declared name: a [`Name`] for every item except a test, whose
    /// name is a string ([`TestDecl::name`]).
    #[must_use]
    pub fn name(self) -> Option<Name<'t>> {
        match self {
            Self::Fn(item) => item.name().map(Name::from),
            Self::Type(item) => item.name().map(Name::from),
            Self::Alias(item) => item.name().map(Name::from),
            Self::Const(item) => item.name().map(Name::from),
            Self::Interface(item) => item.name().map(Name::from),
            Self::Test(_) => None,
        }
    }
}

impl<'t> FnDecl<'t> {
    /// The function's name.
    #[must_use]
    pub fn name(self) -> Option<Ident<'t>> {
        child_token(self.0)
    }

    /// The `<…>` type parameters, if written.
    #[must_use]
    pub fn type_params(self) -> Option<TypeParamList<'t>> {
        child(self.0)
    }

    /// The `(…)` parameter list.
    #[must_use]
    pub fn param_list(self) -> Option<ParamList<'t>> {
        child(self.0)
    }

    /// The parameters, in order. Empty if the list is missing.
    pub fn params(self) -> impl Iterator<Item = Param<'t>> + 't {
        self.param_list().into_iter().flat_map(ParamList::params)
    }

    /// The `-> T` return type, if written.
    #[must_use]
    pub fn ret_type(self) -> Option<RetType<'t>> {
        child(self.0)
    }

    /// The body. `None` for an `@external` function (App. A.2).
    #[must_use]
    pub fn body(self) -> Option<Block<'t>> {
        child(self.0)
    }
}

impl<'t> TypeDecl<'t> {
    /// The `exposed` keyword, if written (D-035, D-150).
    #[must_use]
    pub fn exposed_keyword(self) -> Option<SyntaxToken<'t>> {
        token(self.0, SyntaxKind::EXPOSED_KW)
    }

    /// Whether the type is `exposed`.
    #[must_use]
    pub fn is_exposed(self) -> bool {
        self.exposed_keyword().is_some()
    }

    /// The type's name.
    #[must_use]
    pub fn name(self) -> Option<TypeName<'t>> {
        child_token(self.0)
    }

    /// The `<…>` type parameters, if written.
    #[must_use]
    pub fn type_params(self) -> Option<TypeParamList<'t>> {
        child(self.0)
    }

    /// The body. `None` for an external type (§12.5.3).
    #[must_use]
    pub fn body(self) -> Option<TypeBody<'t>> {
        child(self.0)
    }
}

ast_enum! {
    /// The body of a type declaration: variants, or the fields of the
    /// single-variant shorthand (§4.3.3).
    TypeBody {
        Variants(VariantList),
        Record(RecordFieldList),
    }
}

impl<'t> AliasDecl<'t> {
    /// The alias's name.
    #[must_use]
    pub fn name(self) -> Option<TypeName<'t>> {
        child_token(self.0)
    }

    /// The `<…>` type parameters, if written.
    #[must_use]
    pub fn type_params(self) -> Option<TypeParamList<'t>> {
        child(self.0)
    }

    /// The aliased type.
    #[must_use]
    pub fn ty(self) -> Option<Type<'t>> {
        child(self.0)
    }
}

impl<'t> ConstDecl<'t> {
    /// The constant's name.
    #[must_use]
    pub fn name(self) -> Option<Ident<'t>> {
        child_token(self.0)
    }

    /// The written type. Required (D-011), so `None` is reported (E0020).
    #[must_use]
    pub fn ty(self) -> Option<Type<'t>> {
        child(self.0)
    }

    /// The value.
    #[must_use]
    pub fn value(self) -> Option<Expr<'t>> {
        child(self.0)
    }
}

impl<'t> InterfaceDecl<'t> {
    /// The interface's name.
    #[must_use]
    pub fn name(self) -> Option<TypeName<'t>> {
        child_token(self.0)
    }

    /// The `: A + B` super-interfaces, if written.
    #[must_use]
    pub fn bounds(self) -> Option<BoundList<'t>> {
        child(self.0)
    }

    /// The `{ fn … }` body, if written.
    #[must_use]
    pub fn body(self) -> Option<InterfaceBody<'t>> {
        child(self.0)
    }

    /// The function signatures, in order. Empty without a body.
    pub fn fns(self) -> impl Iterator<Item = InterfaceFn<'t>> + 't {
        self.body().into_iter().flat_map(InterfaceBody::fns)
    }
}

impl<'t> TestDecl<'t> {
    /// The test's name, a string literal.
    #[must_use]
    pub fn name(self) -> Option<StringLit<'t>> {
        child(self.0)
    }

    /// The body.
    #[must_use]
    pub fn body(self) -> Option<Block<'t>> {
        child(self.0)
    }
}

impl<'t> TypeParamList<'t> {
    /// The parameters, in order.
    #[must_use]
    pub fn params(self) -> AstChildren<'t, TypeParam<'t>> {
        children(self.0)
    }
}

impl<'t> TypeParam<'t> {
    /// The parameter's name.
    #[must_use]
    pub fn name(self) -> Option<TypeName<'t>> {
        child_token(self.0)
    }

    /// The `: A + B` bounds, if written.
    #[must_use]
    pub fn bounds(self) -> Option<BoundList<'t>> {
        child(self.0)
    }
}

impl<'t> BoundList<'t> {
    /// The interfaces, in order.
    #[must_use]
    pub fn interfaces(self) -> AstChildren<'t, InterfaceRef<'t>> {
        children(self.0)
    }
}

impl<'t> InterfaceRef<'t> {
    /// The interface's path.
    #[must_use]
    pub fn path(self) -> Option<PathNode<'t>> {
        child(self.0)
    }

    /// Type arguments, which are not allowed here (E0023).
    #[must_use]
    pub fn type_args(self) -> Option<TypeArgList<'t>> {
        child(self.0)
    }
}

impl<'t> ParamList<'t> {
    /// The parameters, in order.
    #[must_use]
    pub fn params(self) -> AstChildren<'t, Param<'t>> {
        children(self.0)
    }
}

impl<'t> Param<'t> {
    /// The parameter's pattern. At the top level it must be a plain name
    /// (D-242, E0021).
    #[must_use]
    pub fn pattern(self) -> Option<Pat<'t>> {
        child(self.0)
    }

    /// The parameter's name, if its pattern is a plain name.
    #[must_use]
    pub fn name(self) -> Option<Ident<'t>> {
        match self.pattern()? {
            Pat::Ident(pat) => pat.name(),
            _ => None,
        }
    }

    /// The written type, if any.
    #[must_use]
    pub fn ty(self) -> Option<Type<'t>> {
        child(self.0)
    }
}

impl<'t> RetType<'t> {
    /// The type after `->`.
    #[must_use]
    pub fn ty(self) -> Option<Type<'t>> {
        child(self.0)
    }
}

impl<'t> VariantList<'t> {
    /// The variants, in order.
    #[must_use]
    pub fn variants(self) -> AstChildren<'t, Variant<'t>> {
        children(self.0)
    }
}

ast_enum! {
    /// The fields of a variant: positional or named.
    VariantFields {
        Positional(PosFieldList),
        Record(RecordFieldList),
    }
}

impl<'t> Variant<'t> {
    /// The variant's name.
    #[must_use]
    pub fn name(self) -> Option<TypeName<'t>> {
        child_token(self.0)
    }

    /// The fields. `None` for a unit variant.
    #[must_use]
    pub fn fields(self) -> Option<VariantFields<'t>> {
        child(self.0)
    }
}

impl<'t> PosFieldList<'t> {
    /// The field types, in order.
    #[must_use]
    pub fn types(self) -> AstChildren<'t, Type<'t>> {
        children(self.0)
    }
}

impl<'t> RecordFieldList<'t> {
    /// The fields, in order.
    #[must_use]
    pub fn fields(self) -> AstChildren<'t, RecordField<'t>> {
        children(self.0)
    }
}

impl<'t> RecordField<'t> {
    /// The field's name.
    #[must_use]
    pub fn name(self) -> Option<Ident<'t>> {
        child_token(self.0)
    }

    /// The field's type.
    #[must_use]
    pub fn ty(self) -> Option<Type<'t>> {
        child(self.0)
    }
}

impl<'t> InterfaceBody<'t> {
    /// The function signatures, in order.
    #[must_use]
    pub fn fns(self) -> AstChildren<'t, InterfaceFn<'t>> {
        children(self.0)
    }
}

impl<'t> InterfaceFn<'t> {
    /// The function's name.
    #[must_use]
    pub fn name(self) -> Option<Ident<'t>> {
        child_token(self.0)
    }

    /// The `(T, U)` parameter types.
    #[must_use]
    pub fn param_types(self) -> Option<TypeList<'t>> {
        child(self.0)
    }

    /// The `-> T` return type, if written.
    #[must_use]
    pub fn ret_type(self) -> Option<RetType<'t>> {
        child(self.0)
    }
}

impl<'t> TypeList<'t> {
    /// The types, in order.
    #[must_use]
    pub fn types(self) -> AstChildren<'t, Type<'t>> {
        children(self.0)
    }
}
