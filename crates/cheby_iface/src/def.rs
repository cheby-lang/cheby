//! Kinds of definitions, namespaces and visibility.

/// What kind of item a definition is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DefKind {
    Fn,
    Type,
    Alias,
    Const,
    Interface,
    /// A function declared inside an interface (§8.1).
    InterfaceFn,
    /// A constructor of a type (§4.3.2).
    Ctor,
    Test,
}

/// One of a module's two namespaces (§7.7).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Namespace {
    /// Types, aliases, interfaces and type parameters.
    Type,
    /// Functions, constants, constructors, module names and locals.
    Value,
}

/// An item's visibility (§7.3, D-162). `exposed` is a flag of type
/// definitions, not a visibility.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Vis {
    /// `priv`: its own module only.
    Priv,
    /// No keyword: every module of the same package.
    Package,
    /// `pub`: every module of every package.
    Pub,
}

impl DefKind {
    /// The namespace the item's name lives in. Tests and interface
    /// functions are in neither: interface functions are named through
    /// their interface (D-173).
    #[must_use]
    pub const fn namespace(self) -> Option<Namespace> {
        match self {
            Self::Type | Self::Alias | Self::Interface => Some(Namespace::Type),
            Self::Fn | Self::Const | Self::Ctor => Some(Namespace::Value),
            Self::InterfaceFn | Self::Test => None,
        }
    }

    /// A description for diagnostics, such as "function".
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Fn => "function",
            Self::Type => "type",
            Self::Alias => "type alias",
            Self::Const => "constant",
            Self::Interface => "interface",
            Self::InterfaceFn => "interface function",
            Self::Ctor => "constructor",
            Self::Test => "test",
        }
    }
}

impl Namespace {
    /// "type" or "value", for diagnostics.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Type => "type",
            Self::Value => "value",
        }
    }
}
