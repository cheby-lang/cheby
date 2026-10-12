//! Numeric kernels (F6, H2): the `@external(native, …)` functions of
//! `std::int`, `std::float`, `std::f32` and the eight sized integer
//! modules, one symbol per type (D-484), and the text of numbers.
//!
//! - [`text`]: `show` and `parse` of every numeric type (§3.3.6, D-312,
//!   D-313, D-395), `int::parse_base` and `int::to_base` (D-390).
//! - `int`: `std::int` and the sized integer modules: conversions (D-308,
//!   D-357), parsing and text, checked (D-309) and wrapping (D-025)
//!   arithmetic, and `int::checked_pow` with its clamped exponent (D-298,
//!   D-392).
//! - `float`: `std::float` and `std::f32`: conversions (D-359), parsing and
//!   text, `truncate`, `round`, `floor` and `ceil` (D-132, D-314) and
//!   `float::sqrt`.
//! - [`math`]: the correctly rounded functions of D-396 (`pow`, `exp`,
//!   `ln`, `sin`, `cos`, `tan`, `atan2`), in Rust with double-double
//!   evaluation and Ziv's rounding test (D-568), verified as D-553
//!   requires.
//!
//! Kernels follow plan §6: `Int` and `I64` are `i64`, the sized types their
//! C types, `Float` `f64`, `F32` `f32`, and every other value a borrowed
//! uniform word; results are owned, and `Result`, `String` and boxed values
//! are built only through [`crate::values`].

mod float;
mod int;
pub(crate) mod math;
pub(crate) mod text;

use crate::abi::Word;
use crate::unwind::guard::DropOnUnwind;
use crate::values;

/// One kernel: its symbol, its address, and its C signature as the names of
/// the Rust types it takes and returns (`Word` for a uniform word), which a
/// test checks against the `@external` declarations in `std/`.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the signature is read by the std cross-check test"
    )
)]
pub(crate) struct Kernel {
    pub(crate) name: &'static str,
    pub(crate) address: *const u8,
    pub(crate) params: &'static [&'static str],
    pub(crate) result: &'static str,
}

/// Every numeric kernel.
fn table() -> Vec<Kernel> {
    let mut table = Vec::new();
    int::table(&mut table);
    float::table(&mut table);
    math::table(&mut table);
    table
}

/// Appends every numeric kernel symbol and its address to `out`.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    out.extend(
        table()
            .into_iter()
            .map(|kernel| (kernel.name, kernel.address)),
    );
}

/// The std modules whose every kernel is registered here; `float` since
/// [`math`] registers the seven correctly rounded functions (D-396,
/// D-568).
pub(crate) const COMPLETE: &[&str] = &[
    "int", "float", "f32", "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64",
];

/// A value that a kernel returns inside a uniform word (plan §5.1).
trait ToWord {
    fn to_word(self) -> Word;
}

macro_rules! to_word {
    ($($ty:ty => $builder:path),* $(,)?) => {
        $(impl ToWord for $ty {
            fn to_word(self) -> Word {
                $builder(self)
            }
        })*
    };
}

to_word! {
    i8 => values::i8_word,
    i16 => values::i16_word,
    i32 => values::i32_word,
    i64 => values::int_word,
    u8 => values::u8_word,
    u16 => values::u16_word,
    u32 => values::u32_word,
    u64 => values::u64_word,
    f64 => values::f64_word,
    f32 => values::f32_word,
}

impl ToWord for String {
    fn to_word(self) -> Word {
        values::string(&self)
    }
}

/// `Ok(value)` or `Err(Nil)`, the `Result<T, Nil>` of D-132 and D-309.
fn result<T: ToWord>(value: Option<T>) -> Word {
    match value {
        Some(value) => {
            // A boxed word is released if the `Ok` cannot be allocated.
            let held = DropOnUnwind::new([value.to_word()]);
            let [word] = held.words();
            let ok = values::ok(word);
            held.disarm();
            ok
        }
        None => values::err_nil(),
    }
}

/// The bytes of a borrowed `String` argument.
///
/// # Safety
///
/// `text` is the uniform word of a live `String`, borrowed for the call.
unsafe fn text_arg<'a>(text: Word) -> &'a [u8] {
    // SAFETY: forwarded from the caller.
    unsafe { values::bytes_of(text) }
}

#[cfg(test)]
mod tests;
