//! Debug printing (§3.13, D-134, D-233): `Some(3)`, `Point { x: 1, y: 2 }`,
//! `(1, <fn>)`, `"text"`.

use std::borrow::Cow;
use std::fmt::Write as _;

use super::raw::{Raw, View, view};
use crate::abi::Word;
use crate::layout::{FieldKind, LayoutInfo};
use crate::object::{read_field, to_usize, word_to_ptr};

/// One piece of output still to write.
#[derive(Debug)]
enum Piece {
    Text(Cow<'static, str>),
    Value(Word),
}

/// The state of one printing walk, through which a runtime-defined kind's
/// `debug` function emits its text and its children, in order.
#[derive(Debug, Default)]
pub struct Printer {
    pending: Vec<Piece>,
}

impl Printer {
    /// Emits fixed text.
    pub fn text(&mut self, text: &'static str) {
        self.pending.push(Piece::Text(Cow::Borrowed(text)));
    }

    /// Emits computed text.
    pub fn text_owned(&mut self, text: String) {
        self.pending.push(Piece::Text(Cow::Owned(text)));
    }

    /// Emits the debug text of a borrowed child value.
    pub fn value(&mut self, child: Word) {
        self.pending.push(Piece::Value(child));
    }
}

/// Appends the debug text of a borrowed uniform word to `out`.
///
/// # Safety
///
/// `word` is a tagged integer or a live object, not changed during the
/// call.
pub unsafe fn debug(word: Word, out: &mut String) {
    let mut stack = vec![Piece::Value(word)];
    let mut printer = Printer::default();
    while let Some(piece) = stack.pop() {
        match piece {
            Piece::Text(text) => out.push_str(&text),
            Piece::Value(word) => {
                // SAFETY: every word on the stack is borrowed from `word`.
                unsafe { step(word, out, &mut printer) };
                stack.extend(printer.pending.drain(..).rev());
            }
        }
    }
}

/// The debug text of a borrowed uniform word.
///
/// # Safety
///
/// As for [`debug`].
#[must_use]
pub unsafe fn debug_string(word: Word) -> String {
    let mut out = String::new();
    // SAFETY: forwarded from the caller.
    unsafe { debug(word, &mut out) };
    out
}

/// Writes the start of `word`'s text to `out` and schedules the rest.
///
/// # Safety
///
/// As for [`debug`].
unsafe fn step(word: Word, out: &mut String, p: &mut Printer) {
    // SAFETY: forwarded from the caller.
    match unsafe { view(word, "debug printing") } {
        View::Tagged(value) | View::Int(value) => write_display(out, value),
        View::U64(value) => write_display(out, value),
        View::Float(value) => out.push_str(&float_text(value)),
        View::F32(value) => out.push_str(&f32_text(value)),
        View::String(bytes) => write_string(out, &String::from_utf8_lossy(bytes)),
        View::Bytes(bytes) => write_bytes(out, bytes),
        View::Tuple(fields) => {
            out.push('(');
            separated(p, fields);
            p.text(")");
        }
        View::Closure => out.push_str("<fn>"),
        View::Ctor(info) => ctor(word, info, out, p),
        // A `dyn` value prints as the value inside (§3.13).
        View::Dyn(value) => p.value(value),
        View::Custom(kind) => {
            if let Some(debug) = kind.debug {
                // SAFETY: a live object of this kind.
                unsafe { debug(word, p) };
            } else {
                out.push('<');
                out.push_str(kind.name);
                out.push('>');
            }
        }
        View::Internal(info) => {
            out.push('<');
            out.push_str(&info.name);
            out.push('>');
        }
    }
}

/// Schedules `values` separated by `, `.
fn separated(p: &mut Printer, values: impl IntoIterator<Item = Word>) {
    for (index, value) in values.into_iter().enumerate() {
        if index > 0 {
            p.text(", ");
        }
        p.value(value);
    }
}

/// A constructor: `None`, `Some(3)`, `Point { x: 1, y: 2 }`.
fn ctor(word: Word, info: &LayoutInfo, out: &mut String, p: &mut Printer) {
    // W3-B: a type with `@std_hooks` prints through its `debug` hook, and
    // external types print as `<Name>` (D-233, D-345).
    out.push_str(&info.ctor_name);
    if info.fields.is_empty() {
        return;
    }
    out.push_str(if info.record { " { " } else { "(" });
    let obj = word_to_ptr(word);
    for (index, field) in info.fields.iter().enumerate() {
        if index > 0 {
            p.text(", ");
        }
        if let Some(name) = field.name.as_deref().filter(|_| info.record) {
            p.text_owned(format!("{name}: "));
        }
        // SAFETY: a live constructor with this field.
        let bits = unsafe { read_field(obj, to_usize(u64::from(field.offset))) };
        if field.kind == FieldKind::Value {
            p.value(bits);
        } else {
            p.text_owned(raw_text(Raw::decode(field.kind, bits)));
        }
    }
    p.text(if info.record { " }" } else { ")" });
}

fn raw_text(raw: Raw) -> String {
    match raw {
        Raw::Int(value) => value.to_string(),
        Raw::Unsigned(value) => value.to_string(),
        Raw::Float(value) => float_text(value),
        Raw::F32(value) => f32_text(value),
        Raw::Bool(true) => "True".to_owned(),
        Raw::Bool(false) => "False".to_owned(),
    }
}

fn write_display(out: &mut String, value: impl std::fmt::Display) {
    write!(out, "{value}").ok();
}

/// A `String` in double quotes with exactly the escapes a string literal
/// needs to read back as the same text (D-531): `\"`, `\\`, `\n`, `\t`,
/// `\r`, `\{`, and `\u{…}` for every other control character, in
/// lower-case hex without leading zeros (D-557).
pub(super) fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '{' => out.push_str("\\{"),
            c if c.is_control() => {
                write!(out, "\\u{{{:x}}}", u32::from(c)).ok();
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `Bytes` as `bytes::from_list([0x66, 0x6f])` (D-388).
fn write_bytes(out: &mut String, bytes: &[u8]) {
    out.push_str("bytes::from_list([");
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        write!(out, "0x{byte:02x}").ok();
    }
    out.push_str("])");
}

/// The text of a `Float` (§3.3.6), from `std::float`'s kernel.
fn float_text(value: f64) -> String {
    crate::num::text::float_to_string(value)
}

/// The text of an `F32` (§3.3.6), judged at `F32` precision.
fn f32_text(value: f32) -> String {
    crate::num::text::f32_to_string(value)
}
