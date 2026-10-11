//! Views of types and paths (App. A.3, A.5, ch. 3).

use super::{
    AstChildren, AstToken, AstTokens, Ident, InterfaceRef, Name, RetType, TypeList, TypeName,
    child, children,
};
use crate::SyntaxNode;

ast_nodes! {
    /// A path: `name`, `module::Name`, or an interface function such as
    /// `Show::show` (§5.17, D-173). It holds name tokens only.
    PathNode = PATH;
    /// A named type such as `Int`, `List<T>`, `map::Map<K, V>` or `Self`.
    PathType = PATH_TYPE;
    /// The `<T, U>` type arguments of a type or a turbofish.
    TypeArgList = TYPE_ARG_LIST;
    /// A tuple type `(A, B)` (§3.7).
    TupleType = TUPLE_TYPE;
    /// A function type `fn(A) -> B` (§3.9).
    FnType = FN_TYPE;
    /// `dyn Interface` (§8.6).
    DynType = DYN_TYPE;
}

ast_enum! {
    /// A written type (App. A.3).
    Type {
        Path(PathType),
        Tuple(TupleType),
        Fn(FnType),
        Dyn(DynType),
    }
}

impl<'t> PathNode<'t> {
    /// The name segments in order, such as `map` and `Map` in `map::Map`.
    #[must_use]
    pub fn segments(self) -> AstTokens<'t, Name<'t>> {
        AstTokens::new(self.0)
    }

    /// The module qualifier, such as `map` in `map::Map` or `shape` in
    /// `shape::Shape::area`. `None` for an unqualified path.
    #[must_use]
    pub fn qualifier(self) -> Option<Ident<'t>> {
        let mut segments = self.segments();
        let first = segments.next()?;
        segments.next()?;
        first.as_ident()
    }

    /// For an interface function path such as `Show::show`, the interface
    /// (`Show`). `None` for other paths (D-173).
    #[must_use]
    pub fn interface(self) -> Option<TypeName<'t>> {
        let mut prev = None;
        let mut last = None;
        for segment in self.segments() {
            prev = last.replace(segment);
        }
        last?.as_ident()?;
        prev?.as_type_name()
    }

    /// The last name: the item, type, variant or interface function named.
    #[must_use]
    pub fn name(self) -> Option<Name<'t>> {
        self.segments().last()
    }
}

impl<'t> PathType<'t> {
    /// The type's path.
    #[must_use]
    pub fn path(self) -> Option<PathNode<'t>> {
        child(self.0)
    }

    /// The `<…>` type arguments, if written.
    #[must_use]
    pub fn type_args(self) -> Option<TypeArgList<'t>> {
        child(self.0)
    }

    /// Whether this is `Self`, which is allowed only in interfaces (§8.1).
    #[must_use]
    pub fn is_self(self) -> bool {
        self.path().is_some_and(|path| {
            path.qualifier().is_none() && path.name().is_some_and(|name| name.text() == "Self")
        })
    }
}

impl<'t> TypeArgList<'t> {
    /// The type arguments, in order.
    #[must_use]
    pub fn types(self) -> AstChildren<'t, Type<'t>> {
        children(self.0)
    }
}

impl<'t> TupleType<'t> {
    /// The element types, in order.
    #[must_use]
    pub fn elements(self) -> AstChildren<'t, Type<'t>> {
        children(self.0)
    }
}

impl<'t> FnType<'t> {
    /// The `(A, B)` parameter types.
    #[must_use]
    pub fn param_types(self) -> Option<TypeList<'t>> {
        child(self.0)
    }

    /// The `-> T` return type, if written. Without it the function
    /// returns `Nil` (D-083).
    #[must_use]
    pub fn ret_type(self) -> Option<RetType<'t>> {
        child(self.0)
    }
}

impl<'t> DynType<'t> {
    /// The interface after `dyn`.
    #[must_use]
    pub fn interface(self) -> Option<InterfaceRef<'t>> {
        child(self.0)
    }
}
