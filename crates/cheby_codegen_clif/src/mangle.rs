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
    /// A static dictionary (D-523).
    Dict,
    /// The header word of a user constructor's layout (D-454).
    Layout,
    /// The code of a built-in tuple `Show`/`Compare` function (D-560).
    TupleFn,
    /// The code of a `dyn` dictionary's forwarding entry (D-560).
    DynFwd,
    /// The unchecked inner body of a function with a large frame (D-679).
    Big,
    /// A continuation of a large literal (D-780, D-811).
    Cont,
    /// The C-ABI shim through which the blocking pool calls an `@blocking`
    /// foreign function (D-683, D-818).
    Blk,
    /// An import word: a read-only 8-byte word holding the address of a
    /// runtime function, a runtime static or a foreign symbol, which
    /// generated code loads (D-913, D-917).
    Imp,
}

impl Kind {
    /// Every kind.
    pub const ALL: [Self; 21] = [
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
        Self::Dict,
        Self::Layout,
        Self::TupleFn,
        Self::DynFwd,
        Self::Big,
        Self::Cont,
        Self::Blk,
        Self::Imp,
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
            Self::Dict => "dict",
            Self::Layout => "layout",
            Self::TupleFn => "tfn",
            Self::DynFwd => "dynfwd",
            Self::Big => "big",
            Self::Cont => "cont",
            Self::Blk => "blk",
            Self::Imp => "imp",
        }
    }
}

/// A module path (`std::io`) as a symbol part (`std$io`).
#[must_use]
pub fn path_part(module: &str) -> String {
    module.replace("::", "$")
}

/// `cb$<kind>$<module path>$<item>`, for a kind named by an item: `Fn`,
/// `Slot`, `Shim`, `FnVal`, `Cinit`, `Const`, `Ctor`, `Layout`, `Desc` and
/// `Dict`.
#[must_use]
pub fn item(kind: Kind, name: &DefName) -> String {
    format!(
        "{PREFIX}{}${}${}",
        kind.word(),
        path_part(&name.module),
        path_part(&name.item)
    )
}

/// `cb$<kind>$<module path>$$<name>`: a static object of a unit (D-523),
/// whose `name` MIR built from paths and names, unique in the unit.
///
/// The name is free text with paths in it (`app::m::I$app::t::T`), so it
/// is kept as it is, and `$$` ends the module path: a module path joined
/// with `$` never holds `$$`, so the first `$$` is always the boundary and
/// statics of different units never share a symbol
/// (D-579). Joining the name's paths with `$` too
/// would make `app::app` with `app::m::I$…` and `app` with
/// `app::app::m::I$…` the same symbol.
#[must_use]
pub fn static_object(kind: Kind, unit_path: &str, name: &str) -> String {
    format!("{PREFIX}{}${}$${name}", kind.word(), path_part(unit_path))
}

/// `cb$clo$<module path>$<owner>$<n>`.
#[must_use]
pub fn closure(owner: &DefName, index: u32) -> String {
    format!("{}${index}", item(Kind::Clo, owner))
}

/// `cb$clo$test$<module path>$<ordinal>$<n>`: closure `n` of the test with
/// that ordinal in its module. A test's name is free text, so its closures
/// are named by the ordinal, like the test itself (D-456, D-572); an item
/// name never starts with a digit, so this never equals [`closure`]'s name.
#[must_use]
pub fn test_closure(module: &str, ordinal: u32, index: u32) -> String {
    format!(
        "{PREFIX}{}$test${}${ordinal}${index}",
        Kind::Clo.word(),
        path_part(module)
    )
}

/// `cb$cont$<module path>$<owner>$<n>`: continuation `n` of the item or
/// constant `owner` (D-780, D-811), named as its
/// closures are ([`closure`]) under a kind of its own.
#[must_use]
pub fn continuation(owner: &DefName, index: u32) -> String {
    format!("{}${index}", item(Kind::Cont, owner))
}

/// `cb$cont$test$<module path>$<ordinal>$<n>`: continuation `n` of the
/// test with that ordinal in its module, named as its closures are
/// ([`test_closure`], D-780).
#[must_use]
pub fn test_continuation(module: &str, ordinal: u32, index: u32) -> String {
    format!(
        "{PREFIX}{}$test${}${ordinal}${index}",
        Kind::Cont.word(),
        path_part(module)
    )
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

/// `cb$tfn$<module path>$<interface function>$<arity>`: the built-in
/// tuple function `iface_fn` for tuples of `arity` elements (D-167).
/// The kind `tfn` is D-560's, which extends D-456 and D-523.
#[must_use]
pub fn tuple_fn(iface_fn: &DefName, arity: u32) -> String {
    format!("{}${arity}", item(Kind::TupleFn, iface_fn))
}

/// `cb$dynfwd$<module path>$<interface function>$$<module path of J>$<J>`:
/// the forwarding entry of `iface_fn` in the dictionaries of `dyn J`
/// (D-560). Its field in a `J` dictionary depends on `J`, so `J` is part of
/// the name; an interface function's item is always `Iface$fn`, so the first
/// `$$` after it ends it, even when a module path is empty (D-602).
#[must_use]
pub fn dyn_forward(iface_fn: &DefName, dyn_iface: &DefName) -> String {
    format!(
        "{}$${}${}",
        item(Kind::DynFwd, iface_fn),
        path_part(&dyn_iface.module),
        path_part(&dyn_iface.item)
    )
}

/// `cb$big$<owner without cb$>`: the unchecked inner body of the function
/// whose symbol is `owner`, when its frame is large (D-679). The outer
/// function keeps `owner`, so slots and callers are unchanged. The kind
/// comes first, as in every symbol (D-456), so the name never collides
/// with another kind's (D-751).
#[must_use]
pub fn large_frame_body(owner: &str) -> String {
    let owner = owner.strip_prefix(PREFIX).unwrap_or(owner);
    format!("{PREFIX}{}${owner}", Kind::Big.word())
}

/// `cb$blk$<foreign symbol>`: the shim through which the blocking pool
/// calls the `@blocking` foreign function `symbol` (D-683,
/// D-818); the kind comes first (D-456). A later call site of
/// the same symbol with another C signature, which only signature-free
/// imports allow (a REPL redefinition), gets `cb$blk$<symbol>$$<n>` with
/// `n` from 1: a C symbol holds no `$$`.
#[must_use]
pub fn blocking_shim(symbol: &str, variant: u32) -> String {
    if variant == 0 {
        format!("{PREFIX}{}${symbol}", Kind::Blk.word())
    } else {
        format!("{PREFIX}{}${symbol}$${variant}", Kind::Blk.word())
    }
}

/// `cb$imp$<symbol>`: the import word of the runtime or foreign symbol
/// `symbol` (D-913, D-917), kept as written: the kind comes first, so it
/// never equals another kind's symbol (D-456).
#[must_use]
pub fn import_word(symbol: &str) -> String {
    format!("{PREFIX}{}${symbol}", Kind::Imp.word())
}

/// `cb$str$<owner without cb$>$<n>`: string `n` of the function whose
/// symbol is `owner`.
#[must_use]
pub fn string(owner: &str, id: StrId) -> String {
    let owner = owner.strip_prefix(PREFIX).unwrap_or(owner);
    format!("{PREFIX}str${owner}${}", id.as_u32())
}
