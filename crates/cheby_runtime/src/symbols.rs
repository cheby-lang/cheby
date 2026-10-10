//! The symbol registry (plan §4.3.3): every C-ABI symbol generated code
//! imports, with its address, for the JIT to resolve (E5). Taking the
//! addresses here also keeps the linker from dropping them.
//!
//! [`symbols`] lists the runtime's own symbols first, in the order of
//! [`sym::ALL`], then every kernel module's kernels. Each kernel module
//! (`io`, `num`, `list`, `text`, `bytes`, `env`, `hamt`, `consteval`)
//! registers its kernels through its own `kernels` function and names the
//! std modules it implements completely in its `COMPLETE`, so kernel
//! packages never edit this file.
//!
//! A module that defines runtime kinds (`layout::register_builtin`, ids
//! 26-63) also registers them from its `kernels` function: the JIT calls
//! [`symbols`] before any code runs, so a static object of such a kind
//! (the empty list) is known to the layout table before generated code
//! can `dup` or print it.

use std::ptr;

use crate::abi::sym;
use crate::alloc::cheby_rt_alloc;
use crate::consteval::{cheby_rt_const_missing, cheby_rt_effect_check, cheby_rt_unresolved};
use crate::helpers::{
    cheby_rt_box_f32, cheby_rt_box_f64, cheby_rt_box_i64, cheby_rt_box_u64, cheby_rt_debug,
    cheby_rt_equal, cheby_rt_fmod_f32, cheby_rt_fmod_f64, cheby_rt_str_append, cheby_rt_str_eq,
    cheby_rt_str_with_capacity,
};
use crate::list::{
    cheby_rt_list_append, cheby_rt_list_get, cheby_rt_list_length, cheby_rt_list_push,
    cheby_rt_list_slice, cheby_rt_static_empty_list,
};
use crate::object::{
    cheby_rt_static_equal, cheby_rt_static_false, cheby_rt_static_greater, cheby_rt_static_less,
    cheby_rt_static_nil, cheby_rt_static_none, cheby_rt_static_true,
};
use crate::panic::{cheby_rt_panic, cheby_rt_stack_slow};
use crate::rc::{cheby_rt_drop, cheby_rt_dup};
use crate::{bytes, consteval, desc, env, hamt, io, list, num, text};

/// The address of a static object.
fn data<T>(object: &'static T) -> *const u8 {
    ptr::from_ref(object).cast::<u8>()
}

/// The runtime's helpers and static objects, in the order of [`sym::ALL`]
/// up to the builtin nominal descriptors, which [`desc::symbols`] lists.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
fn runtime_symbols() -> [(&'static str, *const u8); 32] {
    [
        (sym::ALLOC, cheby_rt_alloc as *const u8),
        (sym::DUP, cheby_rt_dup as *const u8),
        (sym::DROP, cheby_rt_drop as *const u8),
        (sym::PANIC, cheby_rt_panic as *const u8),
        (sym::STACK_SLOW, cheby_rt_stack_slow as *const u8),
        (sym::STATIC_NIL, data(&cheby_rt_static_nil)),
        (sym::STATIC_FALSE, data(&cheby_rt_static_false)),
        (sym::STATIC_TRUE, data(&cheby_rt_static_true)),
        (sym::STATIC_LESS, data(&cheby_rt_static_less)),
        (sym::STATIC_EQUAL, data(&cheby_rt_static_equal)),
        (sym::STATIC_GREATER, data(&cheby_rt_static_greater)),
        (sym::STATIC_NONE, data(&cheby_rt_static_none)),
        (
            sym::STR_WITH_CAPACITY,
            cheby_rt_str_with_capacity as *const u8,
        ),
        (sym::STR_APPEND, cheby_rt_str_append as *const u8),
        (sym::STR_EQ, cheby_rt_str_eq as *const u8),
        (sym::LIST_PUSH, cheby_rt_list_push as *const u8),
        (sym::LIST_APPEND, cheby_rt_list_append as *const u8),
        (sym::LIST_LENGTH, cheby_rt_list_length as *const u8),
        (sym::LIST_GET, cheby_rt_list_get as *const u8),
        (sym::LIST_SLICE, cheby_rt_list_slice as *const u8),
        (sym::STATIC_EMPTY_LIST, data(&cheby_rt_static_empty_list)),
        (sym::EQUAL, cheby_rt_equal as *const u8),
        (sym::DEBUG, cheby_rt_debug as *const u8),
        (sym::EFFECT_CHECK, cheby_rt_effect_check as *const u8),
        (sym::BOX_I64, cheby_rt_box_i64 as *const u8),
        (sym::BOX_U64, cheby_rt_box_u64 as *const u8),
        (sym::BOX_F64, cheby_rt_box_f64 as *const u8),
        (sym::BOX_F32, cheby_rt_box_f32 as *const u8),
        (sym::FMOD_F64, cheby_rt_fmod_f64 as *const u8),
        (sym::FMOD_F32, cheby_rt_fmod_f32 as *const u8),
        (sym::CONST_MISSING, cheby_rt_const_missing as *const u8),
        (sym::UNRESOLVED, cheby_rt_unresolved as *const u8),
    ]
}

/// Every runtime symbol and its address: the runtime's own symbols in the
/// order of [`sym::ALL`], then every kernel.
#[must_use]
pub fn symbols() -> Vec<(&'static str, *const u8)> {
    let mut out = runtime_symbols().to_vec();
    out.extend(desc::symbols());
    io::kernels(&mut out);
    num::kernels(&mut out);
    list::kernels(&mut out);
    text::kernels(&mut out);
    bytes::kernels(&mut out);
    env::kernels(&mut out);
    hamt::kernels(&mut out);
    consteval::kernels(&mut out);
    out
}

/// The std modules whose kernels are all registered, from every kernel
/// module's `COMPLETE`.
#[must_use]
pub fn complete_modules() -> Vec<&'static str> {
    [
        io::COMPLETE,
        num::COMPLETE,
        list::COMPLETE,
        text::COMPLETE,
        bytes::COMPLETE,
        env::COMPLETE,
        hamt::COMPLETE,
        consteval::COMPLETE,
    ]
    .concat()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fmt::Write as _;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::abi;
    use crate::object::static_ctor;

    #[test]
    fn symbols_list_every_abi_name_in_order() {
        let names: Vec<_> = symbols().into_iter().map(|(name, _)| name).collect();
        assert_eq!(names.get(..sym::ALL.len()), Some(&sym::ALL[..]));
        for name in names.get(sym::ALL.len()..).unwrap() {
            assert!(
                name.starts_with("cheby_") && !name.starts_with("cheby_rt_"),
                "`{name}` is not a kernel name"
            );
        }
        let unique: BTreeSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "a symbol is registered twice");
        assert!(symbols().iter().all(|(_, address)| !address.is_null()));
    }

    #[test]
    fn static_ctor_symbols_point_to_their_objects() {
        let table = symbols();
        for id in 0..abi::layout::FIRST_USER {
            let Some(name) = sym::static_ctor(id) else {
                assert!(static_ctor(id).is_none());
                continue;
            };
            let address = table
                .iter()
                .find(|(entry, _)| *entry == name)
                .map(|(_, address)| *address);
            assert_eq!(address, static_ctor(id).map(data));
        }
    }

    /// Every `std/**/*.cheby` file, sorted.
    fn std_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                std_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "cheby") {
                out.push(path);
            }
        }
    }

    /// The native symbols of every `@external(native, "…")` in `source`.
    fn native_symbols(source: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut rest = source;
        while let Some((_, after)) = rest.split_once("@external(") {
            rest = after;
            let Some(after) = after.trim_start().strip_prefix("native") else {
                continue;
            };
            let after = after.trim_start().trim_start_matches(',').trim_start();
            if let Some((symbol, _)) = after
                .strip_prefix('"')
                .and_then(|quoted| quoted.split_once('"'))
            {
                found.push(symbol.to_owned());
            }
        }
        found
    }

    /// The std module path of a file under `root`: `int`, `json::decode`.
    fn module_path(root: &Path, file: &Path) -> String {
        let relative = file.strip_prefix(root).unwrap().with_extension("");
        relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("::")
    }

    /// Plan §4.3.3: a std module listed in some `COMPLETE` must have every
    /// kernel registered, and every registered kernel must be declared in
    /// std. Prints the kernels still missing, per std module.
    #[test]
    fn registered_kernels_match_std() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std");
        let mut files = Vec::new();
        std_files(&root, &mut files);
        assert!(!files.is_empty(), "no std sources under {}", root.display());
        let mut declared: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for file in &files {
            let source = std::fs::read_to_string(file).unwrap();
            let symbols = native_symbols(&source);
            if !symbols.is_empty() {
                declared.insert(module_path(&root, file), symbols);
            }
        }
        let registered: BTreeSet<&str> = symbols().into_iter().map(|(name, _)| name).collect();
        let complete = complete_modules();
        let mut errors = Vec::new();
        let mut report = String::new();
        for (module, symbols) in &declared {
            let missing: Vec<&str> = symbols
                .iter()
                .map(String::as_str)
                .filter(|symbol| !registered.contains(symbol))
                .collect();
            writeln!(
                report,
                "std::{module}: {} of {} kernels missing{}{}",
                missing.len(),
                symbols.len(),
                if missing.is_empty() { "" } else { ": " },
                missing.join(", ")
            )
            .unwrap();
            if complete.contains(&module.as_str()) && !missing.is_empty() {
                errors.push(format!(
                    "std::{module} is listed as complete but misses {}",
                    missing.join(", ")
                ));
            }
        }
        for module in &complete {
            if !declared.contains_key(*module) {
                errors.push(format!("`{module}` in COMPLETE declares no kernels in std"));
            }
        }
        let all_declared: BTreeSet<&str> =
            declared.values().flatten().map(String::as_str).collect();
        for name in &registered {
            if !name.starts_with("cheby_rt_") && !all_declared.contains(name) {
                errors.push(format!("`{name}` is registered but not declared in std"));
            }
        }
        println!("{report}");
        assert!(errors.is_empty(), "{}", errors.join("\n"));
    }

    /// The C type of a Cheby type at the kernel boundary (§12.5.1, plan
    /// §6); `()` for a missing or `Nil` result.
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
            "Bool" => "bool",
            "Float" => "f64",
            "" | "Nil" => "()",
            _ => "Word",
        }
    }

    /// The C signature of the function declared on `line` (`fn name(…) ->
    /// T` or `pub fn …`), or `None` if the line declares none.
    fn c_signature(line: &str) -> Option<(Vec<&'static str>, &'static str)> {
        let line = line.trim();
        let rest = line
            .strip_prefix("pub fn ")
            .or_else(|| line.strip_prefix("fn "))?;
        let (_, rest) = rest.split_once('(')?;
        let mut depth = 0_u32;
        let close = rest.char_indices().find_map(|(i, c)| match c {
            '(' => {
                depth = depth.saturating_add(1);
                None
            }
            ')' if depth == 0 => Some(i),
            ')' => {
                depth = depth.saturating_sub(1);
                None
            }
            _ => None,
        })?;
        let (params, result) = rest.split_at(close);
        let params = params
            .split(',')
            .map(str::trim)
            .filter(|param| !param.is_empty())
            .map(|param| c_type(param.split_once(':').unwrap().1.trim()))
            .collect();
        let result = result
            .get(1..)?
            .trim()
            .strip_prefix("->")
            .unwrap_or("")
            .trim();
        let result = result.strip_suffix('{').unwrap_or(result).trim();
        Some((params, c_type(result)))
    }

    /// Every kernel of `std::string`, `std::bytes`, `std::io` and
    /// `std::env` has the C signature of its `@external` declaration in
    /// `std/` (plan §6). The numeric modules check theirs in `num`'s tests.
    #[test]
    fn text_bytes_io_and_env_kernels_match_their_declarations() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std");
        let mut declared = BTreeMap::new();
        for module in ["string", "bytes", "io", "env"] {
            let source = std::fs::read_to_string(root.join(format!("{module}.cheby"))).unwrap();
            let mut pending: Option<String> = None;
            for line in source.lines() {
                if let Some(symbol) = native_symbols(line).pop() {
                    pending = Some(symbol);
                    continue;
                }
                if pending.is_some()
                    && let Some(signature) = c_signature(line)
                    && let Some(symbol) = pending.take()
                {
                    declared.insert(symbol, signature);
                }
            }
        }
        let mut registered = BTreeMap::new();
        for (name, params, result) in [
            text::signatures(),
            bytes::signatures(),
            io::signatures(),
            env::signatures(),
        ]
        .into_iter()
        .flatten()
        {
            assert!(
                registered
                    .insert(name.to_owned(), (params.to_vec(), result))
                    .is_none(),
                "{name} twice"
            );
        }
        assert_eq!(registered, declared);
        assert_eq!(registered.len(), 27 + 36 + 11 + 3);
    }

    #[test]
    fn signatures_are_read_from_declarations() {
        assert_eq!(
            c_signature("fn sys_read_line() -> Result<Option<String>, (Int, String)>"),
            Some((vec![], "Word"))
        );
        assert_eq!(
            c_signature("pub fn slice(text: String, start: Int, end: Int) -> String"),
            Some((vec!["Word", "i64", "i64"], "Word"))
        );
        assert_eq!(
            c_signature("pub fn print(text: String)"),
            Some((vec!["Word"], "()"))
        );
        assert_eq!(c_signature("pub type Error {"), None);
    }

    #[test]
    fn native_symbols_are_found_in_attributes() {
        let source = "@external(native, \"cheby_a_b\")\n@external(js, \"./a.mjs\", \"b\")\n\
                      @external( native ,\"cheby_c\")";
        assert_eq!(native_symbols(source), ["cheby_a_b", "cheby_c"]);
    }
}
