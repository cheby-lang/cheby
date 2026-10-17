//! Type descriptors (D-236, D-524, §8.6.1): the builtin nominal
//! descriptors `cheby_rt_desc_<name>` (D-523, plan §4.3.2), and reading
//! descriptor objects for structural equality, hashing and debug text
//! (D-530).
//!
//! - A **nominal descriptor** (layout [`layout::DESC_NOMINAL`]) is one
//!   static object per type: the header, then the type's printed name and
//!   its module path, both static `String`s. User types get theirs from
//!   code generation (`cb$desc$<module path>$$<Type>`); builtin types use
//!   the statics below, whose module path is empty, as in their type keys
//!   (D-559).
//! - A **descriptor** (layout [`layout::DESC`]) is the header, a nominal
//!   descriptor, then the descriptors of the type arguments; with more
//!   than 254 fields its scan count is escaped (D-555): the nominal
//!   descriptor stays at offset 8, the tagged count of the arguments is at
//!   offset 16, and the arguments follow.
//!
//! Wherever a descriptor is expected, a bare nominal descriptor stands for
//! the type applied to no arguments, so `Int`'s descriptor may be
//! `cheby_rt_desc_int` itself or a descriptor with no arguments.
//!
//! Builtin printed names are the types' names (`Int`, `List`, `Option`),
//! `Tuple` for tuples and `Fn` for function types, with an empty module
//! path (D-581).
//!
//! Two descriptors are equal when their nominal descriptors are the same
//! object and their arguments are equal (D-530, D-582). Each type has exactly one
//! nominal descriptor per JIT (D-561), so identity of nominal descriptors
//! is identity of nominal types: a type redefined in the REPL, or a type
//! of another major version of a package, has another one (§8.6.1). Hashing
//! uses the nominal descriptor's module path and name, never its address,
//! so hashes do not depend on where objects live (D-264).

use std::cell::UnsafeCell;

use crate::abi::{self, Word, layout, sym};
use crate::layout as table;
use crate::object::{ptr_to_word, read_field, scan_and_layout, word_to_ptr};
use crate::string;

/// A static `String` object of `N` bytes: count [`abi::RC_STATIC`],
/// `len == cap == N`.
#[derive(Debug)]
#[repr(C, align(8))]
pub struct StaticText<const N: usize> {
    // Writable memory for the atomic count load; never written.
    header: UnsafeCell<u64>,
    len: u64,
    cap: u64,
    bytes: [u8; N],
}

// SAFETY: never written after initialization (count `RC_STATIC`).
unsafe impl<const N: usize> Sync for StaticText<N> {}

impl<const N: usize> StaticText<N> {
    /// The static `String` holding `bytes`, which must be UTF-8.
    #[must_use]
    pub const fn new(bytes: &[u8; N]) -> Self {
        #[expect(
            clippy::as_conversions,
            reason = "a const fn widens usize to u64; lossless on 64-bit targets"
        )]
        let len = N as u64;
        Self {
            header: UnsafeCell::new(abi::header(abi::RC_STATIC, 0, layout::STRING)),
            len,
            cap: len,
            bytes: *bytes,
        }
    }
}

/// A static nominal descriptor: the header, then the words of two static
/// `String`s, the printed name and the module path (D-524).
#[derive(Debug)]
#[repr(C, align(8))]
pub struct StaticNominal {
    header: UnsafeCell<u64>,
    name: *const u8,
    module: *const u8,
}

// SAFETY: never written after initialization (count `RC_STATIC`); the
// pointers lead to immutable static strings.
unsafe impl Sync for StaticNominal {}

impl StaticNominal {
    /// The nominal descriptor of the type named by the static string at
    /// `name`, in the module named by the one at `module`.
    #[must_use]
    pub const fn new(name: *const u8, module: *const u8) -> Self {
        Self {
            header: UnsafeCell::new(abi::header(abi::RC_STATIC, 2, layout::DESC_NOMINAL)),
            name,
            module,
        }
    }

    /// The descriptor's word. Also exposes the provenance of the two
    /// static strings it points to, so that [`names`], which reads their
    /// addresses back as words, may turn them into usable pointers.
    #[must_use]
    pub fn word(&'static self) -> Word {
        self.expose_strings();
        ptr_to_word(std::ptr::from_ref(self).cast::<u8>())
    }

    /// Exposes the provenance of the name and the module path (MIRI-1,
    /// GAP(miri-static-provenance)). A static initializer cannot turn an
    /// address into an integer, so the fields are pointers, and the
    /// runtime reads them back as integer words (`read_field`), which
    /// carry no provenance. Exposing is free at run time; only the
    /// abstract machine (Miri) observes it.
    fn expose_strings(&self) {
        let _ = self.name.expose_provenance();
        let _ = self.module.expose_provenance();
    }
}

/// The empty module path of every builtin type (D-559).
static BUILTIN_MODULE: StaticText<0> = StaticText::new(b"");

/// Defines the static printed name and the nominal descriptor
/// `cheby_rt_desc_<name>` of each builtin type.
macro_rules! builtin_descs {
    ($($desc:ident, $text:ident = $printed:literal;)*) => {
        $(
            static $text: StaticText<{ $printed.len() }> = StaticText::new($printed);

            #[doc = concat!(
                "The builtin nominal descriptor of `",
                stringify!($desc),
                "` (D-523)."
            )]
            #[unsafe(no_mangle)]
            pub static $desc: StaticNominal = StaticNominal::new(
                std::ptr::from_ref(&$text).cast::<u8>(),
                std::ptr::from_ref(&BUILTIN_MODULE).cast::<u8>(),
            );
        )*

        /// Every builtin nominal descriptor, in the order of
        /// [`sym::DESC_SYMBOLS`].
        static BUILTINS: [&StaticNominal; 23] = [$(&$desc),*];
    };
}

builtin_descs! {
    cheby_rt_desc_int, NAME_INT = b"Int";
    cheby_rt_desc_float, NAME_FLOAT = b"Float";
    cheby_rt_desc_f32, NAME_F32 = b"F32";
    cheby_rt_desc_i8, NAME_I8 = b"I8";
    cheby_rt_desc_i16, NAME_I16 = b"I16";
    cheby_rt_desc_i32, NAME_I32 = b"I32";
    cheby_rt_desc_i64, NAME_I64 = b"I64";
    cheby_rt_desc_u8, NAME_U8 = b"U8";
    cheby_rt_desc_u16, NAME_U16 = b"U16";
    cheby_rt_desc_u32, NAME_U32 = b"U32";
    cheby_rt_desc_u64, NAME_U64 = b"U64";
    cheby_rt_desc_bool, NAME_BOOL = b"Bool";
    cheby_rt_desc_nil, NAME_NIL = b"Nil";
    cheby_rt_desc_string, NAME_STRING = b"String";
    cheby_rt_desc_bytes, NAME_BYTES = b"Bytes";
    cheby_rt_desc_list, NAME_LIST = b"List";
    cheby_rt_desc_map, NAME_MAP = b"Map";
    cheby_rt_desc_set, NAME_SET = b"Set";
    cheby_rt_desc_option, NAME_OPTION = b"Option";
    cheby_rt_desc_result, NAME_RESULT = b"Result";
    cheby_rt_desc_order, NAME_ORDER = b"Order";
    cheby_rt_desc_tuple, NAME_TUPLE = b"Tuple";
    cheby_rt_desc_fn, NAME_FN = b"Fn";
}

/// The builtin nominal descriptors with their symbols, in the order of
/// [`sym::DESC_SYMBOLS`], for the symbol registry.
pub(crate) fn symbols() -> impl Iterator<Item = (&'static str, *const u8)> {
    sym::DESC_SYMBOLS
        .into_iter()
        .zip(BUILTINS)
        .map(|(symbol, desc)| {
            desc.expose_strings();
            (symbol, std::ptr::from_ref(desc).cast::<u8>())
        })
}

/// The builtin nominal descriptor `cheby_rt_desc_<name>`, by `<name>`
/// (one of [`sym::DESC_NAMES`]).
#[must_use]
pub fn builtin(name: &str) -> Option<Word> {
    sym::DESC_NAMES
        .iter()
        .zip(BUILTINS)
        .find(|(candidate, _)| **candidate == name)
        .map(|(_, desc)| desc.word())
}

/// Whether the nominal descriptor `word` is the builtin one of tuples or of
/// function types, whose arguments print as `(A, B)` and `fn(A) -> B`.
fn builtin_is(word: Word, desc: &'static StaticNominal) -> bool {
    word == desc.word()
}

/// What a descriptor describes, for debug text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    /// A tuple type: its arguments are the elements.
    Tuple,
    /// A function type: the parameters, then the result.
    Fn,
    /// A named type.
    Named,
}

/// A descriptor read for a generic walk: its nominal descriptor and its
/// argument descriptors (borrowed).
#[derive(Debug)]
pub(crate) struct Parts {
    /// The nominal descriptor (a static object).
    pub nominal: Word,
    /// The descriptors of the type arguments, in order.
    pub args: Vec<Word>,
}

impl Parts {
    /// How the type prints.
    pub(crate) fn shape(&self) -> Shape {
        if builtin_is(self.nominal, &cheby_rt_desc_tuple) {
            Shape::Tuple
        } else if builtin_is(self.nominal, &cheby_rt_desc_fn) {
            Shape::Fn
        } else {
            Shape::Named
        }
    }
}

/// The parts of a live descriptor or nominal descriptor; `None` for any
/// other object.
///
/// # Safety
///
/// `word` is a live object.
pub(crate) unsafe fn parts(word: Word) -> Option<Parts> {
    let obj = word_to_ptr(word);
    // SAFETY: a live object.
    let (scan, id) = unsafe { scan_and_layout(obj) };
    match id {
        layout::DESC_NOMINAL => Some(Parts {
            nominal: word,
            args: Vec::new(),
        }),
        layout::DESC => {
            // SAFETY: a live descriptor: the nominal descriptor at 8, then
            // the arguments, or the escaped form of D-555.
            let nominal = unsafe { read_field(obj, 8) };
            let (count, first) = if scan == abi::SCAN_ESCAPE {
                // SAFETY: as above.
                let count = abi::untag_int(unsafe { read_field(obj, 16) });
                (usize::try_from(count).unwrap_or(0), 24_usize)
            } else {
                (usize::from(scan).saturating_sub(1), 16)
            };
            let args = (0..count)
                // SAFETY: within the descriptor.
                .map(|index| unsafe {
                    read_field(obj, first.saturating_add(index.saturating_mul(8)))
                })
                .collect();
            Some(Parts { nominal, args })
        }
        _ => None,
    }
}

/// The printed name and module path of a live nominal descriptor.
///
/// # Safety
///
/// `nominal` is a live nominal descriptor whose fields are `String`s.
pub(crate) unsafe fn names<'a>(nominal: Word) -> (&'a [u8], &'a [u8]) {
    let obj = word_to_ptr(nominal);
    // SAFETY: a live nominal descriptor holds two strings at 8 and 16.
    let (name, module) = unsafe { (read_field(obj, 8), read_field(obj, 16)) };
    // SAFETY: as above.
    unsafe {
        (
            string::text_bytes(name).unwrap_or_default(),
            string::text_bytes(module).unwrap_or_default(),
        )
    }
}

/// Whether `word` is a descriptor or a nominal descriptor, by its layout.
///
/// # Safety
///
/// `word` is a tagged integer or a live object.
#[must_use]
pub unsafe fn is_descriptor(word: Word) -> bool {
    if abi::is_tagged_int(word) || word == 0 {
        return false;
    }
    // SAFETY: a live object.
    let (_, id) = unsafe { scan_and_layout(word_to_ptr(word)) };
    matches!(
        table::get(id).map(|info| info.kind),
        Some(table::LayoutKind::Desc | table::LayoutKind::DescNominal)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object;

    #[test]
    fn builtin_descriptors_are_static_nominal_descriptors() {
        let mut seen = std::collections::BTreeSet::new();
        for (name, (symbol, address)) in sym::DESC_NAMES.iter().zip(symbols()) {
            assert_eq!(sym::desc_symbol(name), Some(symbol));
            let word = builtin(name).unwrap();
            assert_eq!(word, ptr_to_word(address));
            assert!(seen.insert(word), "{name} shares its object");
            // SAFETY: a static nominal descriptor.
            let (scan, id) = unsafe { scan_and_layout(word_to_ptr(word)) };
            assert_eq!((scan, id), (2, layout::DESC_NOMINAL));
            // SAFETY: as above.
            assert_eq!(
                unsafe { object::load_rc(word_to_ptr(word)) },
                abi::RC_STATIC
            );
            // SAFETY: as above.
            let (printed, module) = unsafe { names(word) };
            assert_eq!(module, b"");
            assert_eq!(std::str::from_utf8(printed).unwrap().to_lowercase(), *name);
            // SAFETY: as above.
            let parts = unsafe { parts(word) }.unwrap();
            assert_eq!((parts.nominal, parts.args.len()), (word, 0));
            // SAFETY: as above.
            assert!(unsafe { is_descriptor(word) });
        }
        assert!(builtin("dyn").is_none());
        let tuple = builtin("tuple").unwrap();
        // SAFETY: a static nominal descriptor.
        assert_eq!(unsafe { parts(tuple) }.unwrap().shape(), Shape::Tuple);
        // SAFETY: as above.
        assert_eq!(
            unsafe { parts(builtin("fn").unwrap()) }.unwrap().shape(),
            Shape::Fn
        );
        // SAFETY: as above.
        assert_eq!(
            unsafe { parts(builtin("list").unwrap()) }.unwrap().shape(),
            Shape::Named
        );
        let text = crate::values::string("x");
        // SAFETY: a live string and a tagged integer.
        unsafe {
            assert!(!is_descriptor(text));
            assert!(!is_descriptor(crate::values::int_word(3)));
            crate::rc::drop(text);
        }
    }
}
