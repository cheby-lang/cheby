//! Interned identifiers.
//!
//! [`Name`] is deliberately not `Ord`: its number is interning order, which
//! depends on which thread interned a string first once phases run in
//! parallel (D-263). Code that sorts by name sorts by the string.

use std::fmt;
use std::sync::{PoisonError, RwLock};

use rustc_hash::FxHashMap;

/// An interned identifier or other short string.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Name(u32);

impl Name {
    /// The interning number, for debugging only.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Name({})", self.0)
    }
}

macro_rules! symbols {
    ($( $name:ident = $number:literal $text:literal; )*) => {
        /// Names interned by every [`Interner`] before anything else, so
        /// they are constants.
        pub mod sym {
            use super::Name;
            $(
                #[doc = concat!("`", $text, "`")]
                pub const $name: Name = Name($number);
            )*

            /// The pre-interned names in interning order.
            pub(crate) const ALL: &[(Name, &str)] = &[$( ($name, $text), )*];
        }
    };
}

symbols! {
    MAIN = 0 "main";
    STD = 1 "std";
    NATIVE = 2 "native";
    JS = 3 "js";
    EXTERNAL = 4 "external";
    TARGET = 5 "target";
    BLOCKING = 6 "blocking";
    ASYNC = 7 "async";
    DEPRECATED = 8 "deprecated";
    INT = 9 "Int";
    FLOAT = 10 "Float";
    STRING = 11 "String";
    BOOL = 12 "Bool";
    NIL = 13 "Nil";
    LIST = 14 "List";
    RESULT = 15 "Result";
    OPTION = 16 "Option";
    ORDER = 17 "Order";
    NEVER = 18 "Never";
    I8 = 19 "I8";
    I16 = 20 "I16";
    I32 = 21 "I32";
    I64 = 22 "I64";
    U8 = 23 "U8";
    U16 = 24 "U16";
    U32 = 25 "U32";
    U64 = 26 "U64";
    F32 = 27 "F32";
    TRUE = 28 "True";
    FALSE = 29 "False";
    OK = 30 "Ok";
    ERR = 31 "Err";
    SOME = 32 "Some";
    NONE = 33 "None";
    LESS = 34 "Less";
    EQUAL = 35 "Equal";
    GREATER = 36 "Greater";
    SHOW = 37 "Show";
    SHOW_FN = 38 "show";
    UNDERSCORE = 39 "_";
    SELF_TY = 40 "Self";
    T = 41 "T";
    E = 42 "E";
    K = 43 "K";
    V = 44 "V";
    MAP = 45 "Map";
    SET = 46 "Set";
    BYTES = 47 "Bytes";
}

/// An append-only, thread-safe string interner.
///
/// Interned strings are leaked so that [`Interner::get`] returns
/// `&'static str`. Every command is its own process (D-268), and the
/// language server's growth is bounded by the distinct identifiers it sees.
pub struct Interner {
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    map: FxHashMap<&'static str, Name>,
    strings: Vec<&'static str>,
}

impl Interner {
    /// An interner holding exactly the [`sym`] names, with their constant
    /// numbers.
    #[must_use]
    pub fn new() -> Self {
        let interner = Self {
            inner: RwLock::new(Inner::default()),
        };
        for &(_, text) in sym::ALL {
            interner.intern(text);
        }
        interner
    }

    /// The name of `text`, interning it on first use.
    pub fn intern(&self, text: &str) -> Name {
        if let Some(&name) = self
            .inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .map
            .get(text)
        {
            return name;
        }
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(&name) = inner.map.get(text) {
            return name;
        }
        // More than `u32::MAX` distinct names cannot fit in memory next to
        // their text, so the saturation never happens in practice.
        let name = Name(u32::try_from(inner.strings.len()).unwrap_or(u32::MAX));
        let leaked: &'static str = Box::leak(text.to_owned().into_boxed_str());
        inner.strings.push(leaked);
        inner.map.insert(leaked, name);
        name
    }

    /// The name of `text` if it was interned.
    #[must_use]
    pub fn lookup(&self, text: &str) -> Option<Name> {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .map
            .get(text)
            .copied()
    }

    /// The text of `name`. A name from another interner may give the wrong
    /// text, and an unknown name gives `""`.
    #[must_use]
    pub fn get(&self, name: Name) -> &'static str {
        let inner = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        usize::try_from(name.0)
            .ok()
            .and_then(|n| inner.strings.get(n))
            .copied()
            .unwrap_or("")
    }
}

impl Default for Interner {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Interner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let len = self
            .inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .strings
            .len();
        f.debug_struct("Interner").field("len", &len).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols_are_preinterned_in_order() {
        let interner = Interner::new();
        for (n, &(name, text)) in sym::ALL.iter().enumerate() {
            assert_eq!(name.as_u32(), u32::try_from(n).unwrap(), "{text}");
            assert_eq!(interner.get(name), text);
            assert_eq!(interner.intern(text), name);
        }
        assert_eq!(interner.get(sym::MAIN), "main");
        assert_eq!(interner.get(sym::SHOW_FN), "show");
    }

    #[test]
    fn interning_round_trips() {
        let interner = Interner::new();
        let a = interner.intern("hello_world");
        let b = interner.intern("hello_world");
        let c = interner.intern("other");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(interner.get(a), "hello_world");
        assert_eq!(interner.lookup("other"), Some(c));
        assert_eq!(interner.lookup("missing"), None);
        assert_eq!(interner.get(Name(u32::MAX)), "");
    }

    #[test]
    fn interning_is_thread_safe() {
        let interner = Interner::new();
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for n in 0..100 {
                        let text = format!("x{n}");
                        let name = interner.intern(&text);
                        assert_eq!(interner.get(name), text);
                    }
                });
            }
        });
    }
}
