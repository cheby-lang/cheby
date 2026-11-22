//! The wrappers of a package's `@external(native, …)` (D-590, D-946,
//! D-947, §12.5.1): external values cross as handles, external results are
//! wrapped, counted results are checked, and std's wrappers do not change.
//! Direct calls of std kernels: the foreign call at the call site, with
//! each counted argument in an owned temporary (D-525).

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

/// D-947, D-946, D-1229: a counted result is checked
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

/// D-1229: std's wrappers call no helper, so std's
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

/// A program that calls std kernels directly, in and out of tail
/// position, on a value used afterwards, and as a function value.
const DIRECT: &str = r#"//! Direct calls of std kernels.

import std::float
import std::io
import std::list

fn scaled(n: Int) -> Float {
  let x = float::from_int(n)
  x * 2.0
}

fn root(x: Float) -> Float {
  float::sqrt(x)
}

fn kept(xs: List<Int>) -> List<Int> {
  let ys = list::set(xs, 0, 9)
  let n = list::length(xs)
  list::push(ys, n)
}

fn values(xs: List<Int>) -> List<Float> {
  list::map(xs, float::from_int)
}

fn main() {
  let xs = kept([1, 2, 3])
  let f = scaled(2) + root(4.0)
  let ys = values(xs)
  io::println("{xs} {f} {ys}")
}
"#;

/// D-455, D-453: a direct call of a plain std kernel is the foreign call
/// at the call site, and its scalar result stays unboxed: `float::from_int`
/// gives a raw `f64`, which the body multiplies without a box, and the
/// only box is the function's uniform return.
#[test]
fn direct_kernel_call_is_the_foreign_call() {
    let lowered = lower_source(&[("", DIRECT)]);
    let scaled = lowered.function("app", "app::scaled");
    assert!(
        scaled.contains(
            "_1: float = call_foreign \"cheby_float_from_int\"(borrow _0) -> f64 ; unwind bb1"
        ),
        "{scaled}"
    );
    assert!(!scaled.contains("std::float::from_int"), "{scaled}");
    assert!(!scaled.contains("unbox"), "{scaled}");
    assert_eq!(scaled.matches("box float").count(), 1, "{scaled}");
    // In tail position the result is boxed and returned, as the wrapper
    // would box it, since a foreign call is never a tail call (§12.8).
    let root = lowered.function("app", "app::root");
    assert!(
        root.contains("_1: float = call_foreign \"cheby_float_sqrt\"(borrow _0) -> f64"),
        "{root}"
    );
    assert!(root.contains("_2: value = box float _1"), "{root}");
    assert!(root.contains("return _2"), "{root}");
    assert!(!root.contains("tail_call"), "{root}");
}

/// D-525: each counted argument is moved into an owned temporary that the
/// kernel borrows and counting drops after the call. `xs`, used again
/// after `list::set`, is `dup`ed into its temporary, so the kernel sees
/// it shared; at its last use, `list::length`'s, it is moved without a
/// `dup`. `list::push` is an intrinsic (D-1758) and keeps its call.
#[test]
fn direct_kernel_call_moves_each_argument_into_a_temporary() {
    let lowered = lower_source(&[("", DIRECT)]);
    let kept = lowered.function("app", "app::kept");
    let stmts: Vec<&str> = kept.lines().map(str::trim).collect();
    let at = |line: &str| {
        stmts
            .iter()
            .position(|s| s.starts_with(line))
            .unwrap_or_else(|| panic!("no `{line}` in:\n{kept}"))
    };
    let dup = at("dup _0");
    let moved = at("_2: value = _0");
    let set =
        at("_4: value = call_foreign \"cheby_list_set\"(borrow _2, borrow const int 0, borrow _3)");
    let released = at("drop _2");
    assert!(dup < moved && moved < set && set < released, "{kept}");
    let last = at("_5: value = _0");
    let length = at("_6: int = call_foreign \"cheby_list_length\"(borrow _5) -> i64");
    assert!(released < last && last < length, "{kept}");
    assert_eq!(kept.matches("dup _0").count(), 1, "{kept}");
    assert!(kept.contains("tail_call std::list::push(_4, "), "{kept}");
}

/// The wrapper stays for everything but a direct call: a function value
/// of a kernel's function is its wrapper's (D-453), a call of an
/// effect-checked kernel (`io::println`, D-537) goes through its slot,
/// and so does every call of a package's `@external` (D-940).
#[test]
fn direct_kernel_calls_leave_other_calls_alone() {
    let lowered = lower_source(&[("", DIRECT)]);
    let values = lowered.function("app", "app::values");
    assert!(
        values.contains("tail_call std::list::map(_0, const fn std::float::from_int)"),
        "{values}"
    );
    let main = lowered.function("app", "app::main");
    assert!(main.contains("tail_call std::io::println("), "{main}");
    assert!(!main.contains("cheby_io_println"), "{main}");
    let wrapper = lowered.function("std::float", "std::float::from_int");
    assert!(
        wrapper.contains("call_foreign \"cheby_float_from_int\"(borrow _0) -> f64"),
        "{wrapper}"
    );
    let package = store().function("app", "app::main");
    assert!(package.contains("= call app::count("), "{package}");
    assert!(!package.contains("app_store_count"), "{package}");
}
