//! Shared test support for the numeric kernels, and the check of every
//! kernel's symbol and C signature against the `@external` declarations in
//! `std/`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{COMPLETE, Kernel, math, table};
use crate::abi::Word;
use crate::{alloc, rc, values};

/// A value read back from a kernel's result word.
pub(crate) trait FromWord: Sized {
    /// Reads a borrowed word.
    fn from_word(word: Word) -> Self;
}

macro_rules! from_word {
    (
        safe: [$($ty:ty => $reader:path),* $(,)?],
        unsafe: [$($uty:ty => $ureader:path),* $(,)?] $(,)?
    ) => {
        $(impl FromWord for $ty {
            fn from_word(word: Word) -> Self {
                $reader(word)
            }
        })*
        $(impl FromWord for $uty {
            fn from_word(word: Word) -> Self {
                // SAFETY: the word is a live value of this type.
                unsafe { $ureader(word) }
            }
        })*
    };
}

from_word! {
    safe: [
        i8 => values::word_to_i8,
        i16 => values::word_to_i16,
        i32 => values::word_to_i32,
        u8 => values::word_to_u8,
        u16 => values::word_to_u16,
        u32 => values::word_to_u32,
    ],
    unsafe: [
        i64 => values::word_to_i64,
        u64 => values::word_to_u64,
        f64 => values::word_to_f64,
        f32 => values::word_to_f32,
    ],
}

impl FromWord for String {
    fn from_word(word: Word) -> Self {
        // SAFETY: the word is a live `String`.
        unsafe { values::str_of(word) }.to_owned()
    }
}

/// Reads and frees an owned `Result<T, Nil>` word.
pub(crate) fn take_result<T: FromWord>(word: Word) -> Option<T> {
    // SAFETY: `word` is an owned, live `Result`; its payload is read
    // before the word is dropped.
    unsafe {
        let value = match values::read_result(word) {
            Ok(value) => Some(T::from_word(value)),
            Err(error) => {
                assert_eq!(error, values::nil(), "the error of a numeric kernel is Nil");
                None
            }
        };
        rc::drop(word);
        value
    }
}

/// Reads and frees an owned `String` word.
pub(crate) fn take_string(word: Word) -> String {
    let text = String::from_word(word);
    // SAFETY: an owned, live string.
    unsafe { rc::drop(word) };
    text
}

/// Calls a kernel with a temporary `String` argument, which it borrows.
pub(crate) fn with_text<R>(text: &str, kernel: impl FnOnce(Word) -> R) -> R {
    let word = values::string(text);
    let result = kernel(word);
    // SAFETY: the test's own reference to the string.
    unsafe { rc::drop(word) };
    result
}

/// Runs `body` and checks that it leaves no object alive in this thread's
/// accounting domain.
pub(crate) fn no_leaks<R>(body: impl FnOnce() -> R) -> R {
    let before = alloc::live();
    let result = body();
    assert_eq!(alloc::live(), before, "the kernel calls leaked objects");
    result
}

/// The std modules whose kernels live in `num`.
const MODULES: &[&str] = &[
    "int", "float", "f32", "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64",
];

/// The kernels of D-396, which `num::math` registers only once a
/// correctly rounded implementation verifies.
const MATH: &[&str] = &[
    "cheby_float_pow",
    "cheby_float_exp",
    "cheby_float_ln",
    "cheby_float_sin",
    "cheby_float_cos",
    "cheby_float_tan",
    "cheby_float_atan2",
];

/// The C type of a Cheby type at the kernel boundary (§12.5.1, plan §6).
fn c_type(cheby: &str) -> &'static str {
    match cheby {
        "Int" | "I64" => "i64",
        "I8" => "i8",
        "I16" => "i16",
        "I32" => "i32",
        "U8" => "u8",
        "U16" => "u16",
        "U32" => "u32",
        "U64" => "u64",
        "Float" => "f64",
        "F32" => "f32",
        "Bool" => "bool",
        _ => "Word",
    }
}

/// A native kernel declared in std: its symbol and C signature.
#[derive(Debug, PartialEq, Eq)]
struct Declared {
    module: String,
    params: Vec<&'static str>,
    result: &'static str,
}

/// Every `@external(native, "…")` of the numeric std modules, read from
/// the `std/` sources, with the signature of the `pub fn` it binds.
fn declared() -> BTreeMap<String, Declared> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../std");
    let mut out = BTreeMap::new();
    for module in MODULES {
        let path = root.join(format!("{module}.cheby"));
        let source = std::fs::read_to_string(&path).unwrap();
        let mut pending: Option<String> = None;
        for line in source.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("@external(native, \"") {
                pending = Some(rest.split('"').next().unwrap().to_owned());
                continue;
            }
            let Some(signature) = line.strip_prefix("pub fn ") else {
                continue;
            };
            let Some(symbol) = pending.take() else {
                continue;
            };
            let (_, rest) = signature.split_once('(').unwrap();
            let (params, result) = rest.rsplit_once(')').unwrap();
            let params = params
                .split(',')
                .map(str::trim)
                .filter(|param| !param.is_empty())
                .map(|param| c_type(param.split_once(':').unwrap().1.trim()))
                .collect();
            let result = result
                .trim()
                .strip_prefix("->")
                .map_or("()", |r| c_type(r.trim()));
            let previous = out.insert(
                symbol.clone(),
                Declared {
                    module: (*module).to_owned(),
                    params,
                    result,
                },
            );
            assert!(previous.is_none(), "{symbol} is declared twice");
        }
    }
    out
}

#[test]
fn every_kernel_matches_its_std_declaration() {
    let declared = declared();
    let table = table();
    let mut registered = BTreeMap::new();
    for kernel in &table {
        assert!(!kernel.address.is_null(), "{}", kernel.name);
        let previous = registered.insert(kernel.name, kernel);
        assert!(previous.is_none(), "{} is registered twice", kernel.name);
    }
    // Every registered kernel is declared in std with the same C signature.
    for (name, kernel) in &registered {
        let Some(declaration) = declared.get(*name) else {
            panic!("{name} is registered but not declared in std");
        };
        let Kernel { params, result, .. } = kernel;
        assert_eq!(
            (params.to_vec(), *result),
            (declaration.params.clone(), declaration.result),
            "{name}: the kernel's C signature differs from std's"
        );
    }
    // Every declared kernel is registered, except the D-396 functions until
    // they verify.
    let math_registered = MATH.iter().all(|name| registered.contains_key(name));
    let mut missing: Vec<&str> = Vec::new();
    for (name, declaration) in &declared {
        if !registered.contains_key(name.as_str()) {
            assert!(
                MATH.contains(&name.as_str()),
                "{name} ({}) is not registered",
                declaration.module
            );
            missing.push(name);
        }
    }
    assert!(
        math_registered || missing.len() == MATH.len(),
        "the D-396 functions are registered all together or not at all: missing {missing:?}"
    );
    // COMPLETE lists exactly the modules with every kernel registered.
    for module in MODULES {
        let complete = declared
            .iter()
            .filter(|(_, declaration)| declaration.module == *module)
            .all(|(name, _)| registered.contains_key(name.as_str()));
        assert_eq!(
            COMPLETE.contains(module),
            complete,
            "COMPLETE and module {module}"
        );
    }
    assert_eq!(math_registered, math::VERIFIED);
    assert_eq!(table.len(), declared.len() - missing.len());
}

#[test]
fn kernels_lists_the_table() {
    let mut out = Vec::new();
    super::kernels(&mut out);
    let names: Vec<_> = out.iter().map(|(name, _)| *name).collect();
    let table: Vec<_> = table().iter().map(|kernel| kernel.name).collect();
    assert_eq!(names, table);
    assert!(names.iter().all(|name| name.starts_with("cheby_")));
}
