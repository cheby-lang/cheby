//! The wrappers of a package's `@external(native, …)` (D-590, D-946,
//! D-947, §12.5.1): external values cross as handles, external results are
//! wrapped, counted results are checked, and std's wrappers do not change.

use crate::test_support::{Lowered, lower_source};

/// A package with an external type and C functions over it, as example
/// 020 binds them.
const STORE: &str = r#"//! A store kept by C code.

import std::io

@external(native, "app_store_close")
@external(js, "./ffi.mjs", "close")
type Store

@external(native, "app_store_open")
@external(js, "./ffi.mjs", "open")
fn open(name: String) -> Store

@external(native, "app_store_write")
@external(js, "./ffi.mjs", "write")
fn write(store: Store, entry: String)

@external(native, "app_store_contents")
@external(js, "./ffi.mjs", "contents")
fn contents(store: Store) -> String

@external(native, "app_store_count")
@external(js, "./ffi.mjs", "count")
fn count(store: Store) -> Int

fn main() {
  let store = open("log")
  write(store, "one")
  let n = count(store)
  let text = contents(store)
  io::println("{n}: {text}")
}
"#;

fn store() -> Lowered {
    lower_source(&[("", STORE)])
}

/// D-590, §12.5.1: an external argument crosses as its handle, the word at
/// offset 8 (an `unbox` to `U64`), passed as `u64`, and the argument is
/// held until the call is over (its `use` comes last), so a last reference
/// is released only after C used the handle.
#[test]
fn ffi_wrapper_passes_an_external_argument_as_its_handle() {
    let lowered = store();
    let count = lowered.function("app", "app::count");
    assert!(
        count.contains("_1: u64 = unbox u64 _0"),
        "the handle is read at offset 8:\n{count}"
    );
    assert!(
        count.contains("call_foreign \"app_store_count\"(borrow _1) -> i64"),
        "the handle is passed:\n{count}"
    );
    let call = count
        .find("call_foreign \"app_store_count\"")
        .unwrap_or(usize::MAX);
    let keep = count.find("value = _0").unwrap_or(0);
    assert!(keep > call, "the argument is held past the call:\n{count}");
    assert!(
        !count.contains("cheby_rt_ffi_"),
        "an `Int` needs no helper:\n{count}"
    );
    let write = lowered.function("app", "app::write");
    assert!(
        write.contains("call_foreign \"app_store_write\"(borrow _2, borrow _1) -> void"),
        "{write}"
    );
}

/// D-947, D-590: an external result is the raw handle, `u64`, which
/// `cheby_rt_ffi_external` wraps with the type's key (D-559), NULL
/// included.
#[test]
fn ffi_wrapper_wraps_an_external_result() {
    let lowered = store();
    let open = lowered.function("app", "app::open");
    assert!(
        open.contains("call_foreign \"app_store_open\"(borrow _0) -> u64"),
        "the result is a raw word:\n{open}"
    );
    let key = crate::layout::type_key("app", "Store");
    assert!(
        open.contains(&format!(
            "call_foreign \"cheby_rt_ffi_external\"(borrow const ctx, borrow _1, borrow const u64 {key})"
        )),
        "the handle is wrapped with the type's key {key}:\n{open}"
    );
}

/// D-947, D-946, GAP(null-results-outside-std): a counted result is checked
/// by `cheby_rt_ffi_value` with D-947's message and the declaration's
/// location, right after the call, and returned as it is.
#[test]
fn ffi_wrapper_checks_a_counted_result_for_null() {
    let lowered = store();
    let contents = lowered.function("app", "app::contents");
    assert!(
        contents.contains("\"foreign function `app_store_contents` returned null\""),
        "{contents}"
    );
    assert!(
        contents.contains("str#0 = \"src/main.cheby:19:4\""),
        "{contents}"
    );
    assert!(
        contents.contains(
            "call_foreign \"cheby_rt_ffi_value\"(borrow const ctx, borrow _2, \
             borrow const str#2, borrow const str#0) -> void ; unwind bb3"
        ),
        "{contents}"
    );
    assert!(
        contents.contains("bb3:\n  drop _2\n"),
        "the helper's cleanup releases the result (D-946):\n{contents}"
    );
    assert!(contents.contains("return _2"), "{contents}");
}

/// GAP(null-results-outside-std): std's wrappers call no helper, so std's
/// code is as before.
#[test]
fn ffi_wrapper_leaves_std_wrappers_alone() {
    let lowered = store();
    for unit in &lowered.units {
        let text = crate::dump(unit).unwrap_or_default();
        if text.contains("fn std::") {
            assert!(!text.contains("cheby_rt_ffi_"), "{text}");
        }
    }
}

/// The helpers' names are the runtime's (`cheby_runtime::abi::sym`), read
/// from its source: this crate does not depend on the runtime.
#[test]
fn ffi_wrapper_helper_names_are_the_runtimes() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../cheby_runtime/src/abi.rs");
    let source = std::fs::read_to_string(path).unwrap_or_default();
    for (constant, name) in [
        ("FFI_EXTERNAL", super::FFI_EXTERNAL),
        ("FFI_VALUE", super::FFI_VALUE),
    ] {
        let line = format!("pub const {constant}: &str = \"{name}\";");
        assert!(source.contains(&line), "{path} has no `{line}`");
    }
}
