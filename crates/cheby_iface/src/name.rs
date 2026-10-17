//! Interned identifiers.
//!
//! [`Name`] is deliberately not `Ord`: its number depends on interning
//! order, which depends on which thread interned a string first when
//! phases run in parallel (D-263). Code that sorts by name sorts by the
//! string, and nothing that is output may iterate a hash map keyed by
//! names.

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

/// The number of shards of an [`Interner`], a power of two.
const SHARDS: u32 = 64;
/// The low bits of a name's number (past the pre-interned ones) that pick
/// its shard.
const SHARD_MASK: u32 = SHARDS - 1;
const SHARD_BITS: u32 = SHARDS.trailing_zeros();

/// An append-only, thread-safe string interner.
///
/// Interned strings are leaked so that [`Interner::get`] returns
/// `&'static str`. Every command is its own process (D-268), and the
/// language server's growth is bounded by the distinct identifiers it sees.
///
/// The table is split into [`SHARDS`] shards by the string's hash, each
/// behind its own lock, so modules and bodies checked in parallel (D-263)
/// rarely wait for each other (I2). A name past the [`sym`] names numbers
/// its shard in its low bits and its place in the shard above them; the
/// [`sym`] names keep their constant numbers and are read without a lock.
pub struct Interner {
    shards: Box<[RwLock<Shard>]>,
}

#[derive(Default)]
struct Shard {
    /// Every name whose text hashes to this shard, the [`sym`] names
    /// included.
    map: FxHashMap<&'static str, Name>,
    /// The texts of this shard's names past the [`sym`] names, by place.
    strings: Vec<&'static str>,
}

/// How many names are pre-interned: the [`sym`] names.
fn preinterned() -> u32 {
    u32::try_from(sym::ALL.len()).unwrap_or(u32::MAX)
}

/// The shard of `text`.
fn shard_of(text: &str) -> u32 {
    use std::hash::BuildHasher as _;
    let hash = rustc_hash::FxBuildHasher.hash_one(text);
    // Middle bits: the shard's own map indexes buckets by the low bits and
    // tags them with the top ones, which must still vary inside a shard.
    // The mask keeps the value below `SHARDS`.
    u32::try_from((hash >> 32) & u64::from(SHARD_MASK)).unwrap_or(0)
}

impl Interner {
    /// An interner holding exactly the [`sym`] names, with their constant
    /// numbers.
    #[must_use]
    pub fn new() -> Self {
        let interner = Self {
            shards: (0..SHARDS).map(|_| RwLock::default()).collect(),
        };
        for &(name, text) in sym::ALL {
            if let Some(shard) = interner.shard(text) {
                shard
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .map
                    .insert(text, name);
            }
        }
        interner
    }

    fn shard(&self, text: &str) -> Option<&RwLock<Shard>> {
        self.shards.get(usize::try_from(shard_of(text)).ok()?)
    }

    /// The name of `text`, interning it on first use.
    pub fn intern(&self, text: &str) -> Name {
        let Some(shard) = self.shard(text) else {
            return Name(u32::MAX);
        };
        if let Some(&name) = shard
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .map
            .get(text)
        {
            return name;
        }
        let mut shard_data = shard.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(&name) = shard_data.map.get(text) {
            return name;
        }
        // More than `u32::MAX` distinct names cannot fit in memory next to
        // their text, so the saturation never happens in practice.
        let number = u32::try_from(shard_data.strings.len())
            .ok()
            .and_then(|place| place.checked_shl(SHARD_BITS))
            .map(|high| high | shard_of(text))
            .and_then(|n| n.checked_add(preinterned()))
            .unwrap_or(u32::MAX);
        let name = Name(number);
        let leaked: &'static str = Box::leak(text.to_owned().into_boxed_str());
        crate::leak::intended(leaked);
        shard_data.strings.push(leaked);
        shard_data.map.insert(leaked, name);
        name
    }

    /// The name of `text` if it was interned.
    #[must_use]
    pub fn lookup(&self, text: &str) -> Option<Name> {
        self.shard(text)?
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
        let Some(number) = name.0.checked_sub(preinterned()) else {
            return usize::try_from(name.0)
                .ok()
                .and_then(|n| sym::ALL.get(n))
                .map_or("", |&(_, text)| text);
        };
        let shard = usize::try_from(number & SHARD_MASK).unwrap_or(0);
        let Ok(place) = usize::try_from(number >> SHARD_BITS) else {
            return "";
        };
        self.shards.get(shard).map_or("", |shard| {
            shard
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .strings
                .get(place)
                .copied()
                .unwrap_or("")
        })
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
            .shards
            .iter()
            .map(|shard| {
                shard
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .strings
                    .len()
            })
            .sum::<usize>()
            .saturating_add(sym::ALL.len());
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
        let names: Vec<Vec<Name>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(|| {
                        (0..2000)
                            .map(|n| {
                                let text = format!("x{n}");
                                let name = interner.intern(&text);
                                assert_eq!(interner.get(name), text);
                                name
                            })
                            .collect::<Vec<Name>>()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        // Every thread got the same name for each text, and the names are
        // distinct.
        for other in &names[1..] {
            assert_eq!(other, &names[0]);
        }
        let distinct: std::collections::HashSet<Name> = names[0].iter().copied().collect();
        assert_eq!(distinct.len(), 2000);
    }

    #[test]
    fn many_names_round_trip_through_every_shard() {
        let interner = Interner::new();
        let names: Vec<(Name, String)> = (0..20_000)
            .map(|n| {
                let text = format!("name_{n}");
                (interner.intern(&text), text)
            })
            .collect();
        for (name, text) in &names {
            assert!(name.as_u32() >= u32::try_from(sym::ALL.len()).unwrap());
            assert_eq!(interner.get(*name), text);
            assert_eq!(interner.lookup(text), Some(*name));
        }
        for &(name, text) in sym::ALL {
            assert_eq!(interner.lookup(text), Some(name));
        }
    }
}
