//! Symbol names (D-456).
//!
//! Every symbol is `cb$<kind>$…`, built from module paths and item names,
//! never from ids, so names are the same in every build (D-264). The kind
//! comes first, so a package or module named after a kind (`slot`, `str`,
//! `fn`) cannot collide with another kind's symbol. A module path is
//! joined with `$` (`std$io`); names never contain `$`, so the item, which
//! is always the last part, is unambiguous.

use cheby_mir::{DefName, StrId};

/// The prefix of every generated symbol.
pub const PREFIX: &str = "cb$";

/// The kind word of a symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A top-level function, including an `@external`'s wrapper.
    Fn,
    /// A top-level function's indirection slot (D-043).
    Slot,
    /// The uniform-convention shim of a top-level function.
    Shim,
    /// The static closure of a non-generic top-level function.
    FnVal,
    /// A closure or local function.
    Clo,
    /// A constant's initializer (E6).
    Cinit,
    /// A constant's slot (E6).
    Const,
    /// A test body (G4).
    Test,
    /// An entry thunk.
    Entry,
    /// A trampoline into the uniform convention.
    Tramp,
    /// A string literal or location.
    Str,
    /// The static object of a user nullary constructor.
    Ctor,
    /// A type descriptor (D5).
    Desc,
}

impl Kind {
    /// Every kind.
    pub const ALL: [Self; 13] = [
        Self::Fn,
        Self::Slot,
        Self::Shim,
        Self::FnVal,
        Self::Clo,
        Self::Cinit,
        Self::Const,
        Self::Test,
        Self::Entry,
        Self::Tramp,
        Self::Str,
        Self::Ctor,
        Self::Desc,
    ];

    /// The word in the symbol.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Fn => "fn",
            Self::Slot => "slot",
            Self::Shim => "shim",
            Self::FnVal => "fnval",
            Self::Clo => "clo",
            Self::Cinit => "cinit",
            Self::Const => "const",
            Self::Test => "test",
            Self::Entry => "entry",
            Self::Tramp => "tramp",
            Self::Str => "str",
            Self::Ctor => "ctor",
            Self::Desc => "desc",
        }
    }
}

/// A module path (`std::io`) as a symbol part (`std$io`).
#[must_use]
pub fn path_part(module: &str) -> String {
    module.replace("::", "$")
}

/// `cb$<kind>$<module path>$<item>`, for a kind named by an item: `Fn`,
/// `Slot`, `Shim`, `FnVal`, `Cinit`, `Const`, `Ctor` and `Desc`.
#[must_use]
pub fn item(kind: Kind, name: &DefName) -> String {
    format!(
        "{PREFIX}{}${}${}",
        kind.word(),
        path_part(&name.module),
        path_part(&name.item)
    )
}

/// `cb$clo$<module path>$<owner>$<n>`.
#[must_use]
pub fn closure(owner: &DefName, index: u32) -> String {
    format!("{}${index}", item(Kind::Clo, owner))
}

/// `cb$test$<module path>$<n>`, `n` being the test's ordinal in its module.
#[must_use]
pub fn test(module: &str, ordinal: u32) -> String {
    format!("{PREFIX}test${}${ordinal}", path_part(module))
}

/// `cb$entry$main$<module path>`.
#[must_use]
pub fn entry_main(module: &str) -> String {
    format!("{PREFIX}entry$main${}", path_part(module))
}

/// `cb$entry$test$<module path>$<n>`.
#[must_use]
pub fn entry_test(module: &str, ordinal: u32) -> String {
    format!("{PREFIX}entry$test${}${ordinal}", path_part(module))
}

/// `cb$entry$const$<module path>$<item>`.
#[must_use]
pub fn entry_const(name: &DefName) -> String {
    format!(
        "{PREFIX}entry$const${}${}",
        path_part(&name.module),
        path_part(&name.item)
    )
}

/// `cb$entry$repl$<n>`.
#[must_use]
pub fn entry_repl(index: u32) -> String {
    format!("{PREFIX}entry$repl${index}")
}

/// `cb$tramp$<arity>`.
#[must_use]
pub fn trampoline(arity: u32) -> String {
    format!("{PREFIX}tramp${arity}")
}

/// `cb$str$<owner without cb$>$<n>`: string `n` of the function whose
/// symbol is `owner`.
#[must_use]
pub fn string(owner: &str, id: StrId) -> String {
    let owner = owner.strip_prefix(PREFIX).unwrap_or(owner);
    format!("{PREFIX}str${owner}${}", id.as_u32())
}
