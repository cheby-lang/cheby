//! Debug printing (§3.13, D-134, D-233): `Some(3)`, `Point { x: 1, y: 2 }`,
//! `(1, <fn>)`, `"text"`, `[1, 2]`, `map::from_list([(1, "a")])`.
//!
//! The walk keeps a stack of pieces on the heap, never Rust recursion over
//! the data (plan §0). Hashed collections print their entries sorted by
//! each entry's whole debug text (D-344, D-382): an entry's text is
//! collected in a buffer of its own, and the sorted group is written when
//! its last entry is done, still without recursion.
//!
//! The walk holds no Rust-side copy of the text that grows with the data
//! (D-617): `String` and `Bytes` values are escaped into the output in
//! chunks of a fixed size, and the worklists and the text of sorted groups
//! are scratch buffers ([`crate::alloc::scratch`]), which constant
//! evaluation's memory limit counts and whose growth the system may refuse
//! without ending the process.

use std::borrow::Cow;
use std::fmt;

use super::hooks;
use super::raw::{Raw, View, view};
use crate::abi::Word;
use crate::alloc::scratch::{ScratchString, ScratchVec};
use crate::desc::{self, Shape};
use crate::hamt::{self, Collection};
use crate::layout::{FieldKind, LayoutInfo};
use crate::object::{ptr_to_word, read_field, to_usize, word_to_ptr};
use crate::{rc, string};

/// One piece of output still to write.
#[derive(Debug)]
enum Piece {
    Text(Cow<'static, str>),
    Value(Word),
    /// A borrowed `String`, written in double quotes with its escapes.
    Quoted(Word),
    /// A borrowed `Bytes`, written as `bytes::from_list([…])`.
    ByteList(Word),
    /// An owned `String` written as it is, then released: the text a
    /// `debug` hook returned.
    Owned(Word),
    /// Starts a group of items written in the order of their text.
    SortedStart,
    /// Starts one item of the innermost sorted group.
    ItemStart,
    /// Ends the current item.
    ItemEnd,
    /// Ends the innermost sorted group: its items, sorted, separated by
    /// `, `.
    SortedEnd,
}

/// One entry of a group printed in sorted order.
#[derive(Debug, Clone, Copy)]
pub enum SortItem {
    /// A value, printed as itself (a `Set` element).
    Value(Word),
    /// A key and a value, printed as the tuple `(key, value)` (a `Map`
    /// entry).
    Pair(Word, Word),
}

/// The state of one printing walk, through which a runtime-defined kind's
/// `debug` function emits its text and its children, in order.
#[derive(Debug, Default)]
pub struct Printer {
    pending: ScratchVec<Piece>,
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

    /// Emits borrowed values separated by `, `.
    pub fn separated(&mut self, values: impl IntoIterator<Item = Word>) {
        for (index, value) in values.into_iter().enumerate() {
            if index > 0 {
                self.text(", ");
            }
            self.value(value);
        }
    }

    /// Emits the items separated by `, `, sorted by each item's whole debug
    /// text (D-344, D-382), compared as UTF-8 bytes, which is code point
    /// order; a `Map` entry's text is that of the tuple `(key, value)`
    /// (D-587). Items with the same text may come in any order, which
    /// cannot show, since their text is the same.
    pub fn sorted(&mut self, items: impl IntoIterator<Item = SortItem>) {
        self.pending.push(Piece::SortedStart);
        for item in items {
            self.pending.push(Piece::ItemStart);
            match item {
                SortItem::Value(value) => self.value(value),
                SortItem::Pair(key, value) => {
                    self.text("(");
                    self.value(key);
                    self.text(", ");
                    self.value(value);
                    self.text(")");
                }
            }
            self.pending.push(Piece::ItemEnd);
        }
        self.pending.push(Piece::SortedEnd);
    }
}

/// Where a walk writes its text.
trait Sink {
    fn put(&mut self, text: &str);
}

impl Sink for String {
    fn put(&mut self, text: &str) {
        // The host's own text, outside any memory limit; a growth the
        // system refuses still ends as any allocation that cannot be
        // served.
        if self.try_reserve(text.len()).is_err() {
            crate::alloc::out_of_memory(u64::try_from(text.len()).unwrap_or(u64::MAX));
        }
        self.push_str(text);
    }
}

/// Text written to a sink in chunks of at most [`Chunked::SIZE`] bytes,
/// through a buffer of that fixed size: the escaped text of a long
/// `String` or `Bytes` is never held whole.
struct Chunked<'a> {
    buffer: String,
    flush: &'a mut dyn FnMut(&str),
}

impl<'a> Chunked<'a> {
    /// The size of a chunk, in bytes.
    const SIZE: usize = 1024;

    fn new(flush: &'a mut dyn FnMut(&str)) -> Self {
        Self {
            buffer: String::with_capacity(Self::SIZE),
            flush,
        }
    }

    /// Writes what is left.
    fn finish(mut self) {
        if !self.buffer.is_empty() {
            (self.flush)(&self.buffer);
            self.buffer.clear();
        }
    }
}

impl fmt::Write for Chunked<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        // Every piece written here is a few bytes: one code point or one
        // escape.
        if self.buffer.len().saturating_add(text.len()) > Self::SIZE {
            (self.flush)(&self.buffer);
            self.buffer.clear();
        }
        self.buffer.push_str(text);
        Ok(())
    }
}

/// A `String` object being written: unique, grown by doubling (D-316).
struct ObjectSink {
    word: Word,
}

impl Sink for ObjectSink {
    fn put(&mut self, text: &str) {
        // SAFETY: the sink owns its unique `String`, and `text` never
        // points into it.
        self.word = unsafe { string::append(self.word, text.as_bytes()) };
    }
}

/// Appends the debug text of a borrowed uniform word to `out`.
///
/// # Safety
///
/// `word` is a tagged integer or a live object, not changed during the
/// call. On an entry's thread when the value may hold a type with hooks.
pub unsafe fn debug(word: Word, out: &mut String) {
    // SAFETY: forwarded from the caller.
    unsafe { walk(word, out) };
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

/// The debug text of a borrowed uniform word, written straight into a new
/// `String` object, which the caller owns.
///
/// # Safety
///
/// As for [`debug`].
#[must_use]
pub unsafe fn debug_object(word: Word) -> Word {
    let mut sink = ObjectSink {
        word: ptr_to_word(string::alloc_string(16)),
    };
    // SAFETY: forwarded from the caller.
    unsafe { walk(word, &mut sink) };
    std::mem::take(&mut sink.word)
}

impl Drop for ObjectSink {
    /// Releases the text of a walk that a hook's panic unwound out of
    /// (D-697); a finished walk has taken it.
    fn drop(&mut self) {
        // SAFETY: the sink owns its `String`, or holds 0, which `drop`
        // skips.
        unsafe { crate::rc::drop(self.word) };
    }
}

/// Prints `word` into `out`.
///
/// # Safety
///
/// As for [`debug`].
unsafe fn walk(word: Word, out: &mut dyn Sink) {
    let mut stack = ScratchVec::of(Piece::Value(word));
    let mut printer = Printer::default();
    let mut ropes = Ropes::default();
    while let Some(piece) = stack.pop() {
        match piece {
            Piece::Text(text) => ropes.put(out, &text),
            Piece::Value(word) => {
                // SAFETY: every word on the stack is borrowed from `word`.
                unsafe { step(word, &mut printer) };
                stack.push_reversed(&mut printer.pending);
            }
            Piece::Quoted(text) => {
                // SAFETY: a live `String` borrowed from `word`, which
                // holds UTF-8.
                let text = unsafe { string::as_str_unchecked(text) }.unwrap_or_default();
                let mut put = |chunk: &str| ropes.put(out, chunk);
                let mut chunked = Chunked::new(&mut put);
                write_string(&mut chunked, text);
                chunked.finish();
            }
            Piece::ByteList(data) => {
                // SAFETY: a live `Bytes` borrowed from `word`.
                let data = unsafe { string::text_bytes(data) }.unwrap_or_default();
                let mut put = |chunk: &str| ropes.put(out, chunk);
                let mut chunked = Chunked::new(&mut put);
                write_bytes(&mut chunked, data);
                chunked.finish();
            }
            Piece::Owned(text) => {
                // SAFETY: the walk owns this live `String`; it is released
                // once written and never read again.
                unsafe {
                    let bytes = string::text_bytes(text).unwrap_or_default();
                    ropes.put(out, &String::from_utf8_lossy(bytes));
                    rc::drop(text);
                }
            }
            Piece::SortedStart => ropes.open_groups.push(ScratchVec::new()),
            Piece::ItemStart => {
                ropes.open_items.push(ropes.items.len());
                ropes.items.push(ScratchVec::new());
            }
            Piece::ItemEnd => {
                if let (Some(item), Some(group)) =
                    (ropes.open_items.pop(), ropes.open_groups.last_mut())
                {
                    group.push(item);
                }
            }
            Piece::SortedEnd => ropes.end_group(out),
        }
    }
}

/// One piece of an item's text: a range of the arena, or a finished sorted
/// group.
#[derive(Debug, Clone, Copy)]
enum Seg {
    Text(usize, usize),
    Group(usize),
}

/// The text of the items of sorted groups, kept so that each character is
/// written once (D-586).
///
/// An item's text is a list of segments: ranges of one shared arena and
/// the sorted groups nested in it, by reference. A finished group is never
/// copied into its parent item; it is sorted by comparing the items' texts
/// through their segments, and written out once, when the outermost group
/// ends. So printing maps nested in maps costs time linear in the text,
/// plus the sorting, whatever the nesting depth (plan §0, deep data).
#[derive(Debug, Default)]
struct Ropes {
    arena: ScratchString,
    /// The segments of every item, open or finished.
    items: ScratchVec<ScratchVec<Seg>>,
    /// The items of every finished group, sorted.
    groups: ScratchVec<ScratchVec<usize>>,
    /// The items being printed, innermost last.
    open_items: ScratchVec<usize>,
    /// The finished items of each open group, innermost last.
    open_groups: ScratchVec<ScratchVec<usize>>,
}

impl Ropes {
    /// Writes `text` to the innermost open item, or to `out`.
    fn put(&mut self, out: &mut dyn Sink, text: &str) {
        let Some(segs) = self
            .open_items
            .last()
            .and_then(|&item| self.items.get_mut(item))
        else {
            out.put(text);
            return;
        };
        let start = self.arena.len();
        // The arena holds the text of sorted groups before it reaches
        // `out`, outside the objects the domain counts: a scratch buffer,
        // which the memory limit counts (D-617).
        self.arena.push_str(text);
        let end = self.arena.len();
        match segs.last_mut() {
            Some(Seg::Text(_, last)) if *last == start => *last = end,
            _ => segs.push(Seg::Text(start, end)),
        }
    }

    /// Ends the innermost open group: sorts its items by their text and
    /// adds it to the innermost open item, or writes it to `out`.
    fn end_group(&mut self, out: &mut dyn Sink) {
        let mut members = self.open_groups.pop().unwrap_or_default();
        // An unstable sort needs no buffer; items with the same text may
        // swap, which cannot show.
        members.sort_unstable_by(|&a, &b| self.compare(a, b));
        let group = self.groups.len();
        self.groups.push(members);
        if let Some(segs) = self
            .open_items
            .last()
            .and_then(|&item| self.items.get_mut(item))
        {
            segs.push(Seg::Group(group));
            return;
        }
        for chunk in Chunks::new(self, Frame::Group { group, next: 0 }) {
            out.put(chunk);
        }
        if self.open_groups.is_empty() {
            // Nothing refers to the finished text any more.
            self.arena.clear();
            self.items.clear();
            self.groups.clear();
        }
    }

    /// Compares the texts of two finished items as UTF-8 bytes, which is
    /// code point order.
    fn compare(&self, first: usize, second: usize) -> std::cmp::Ordering {
        let mut left = Chunks::new(
            self,
            Frame::Item {
                item: first,
                seg: 0,
            },
        );
        let mut right = Chunks::new(
            self,
            Frame::Item {
                item: second,
                seg: 0,
            },
        );
        let (mut lhs, mut rhs): (&[u8], &[u8]) = (&[], &[]);
        loop {
            if lhs.is_empty() {
                let Some(chunk) = left.next() else {
                    let rest = !rhs.is_empty() || right.any(|chunk| !chunk.is_empty());
                    return if rest {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Equal
                    };
                };
                lhs = chunk.as_bytes();
                continue;
            }
            if rhs.is_empty() {
                let Some(chunk) = right.next() else {
                    return std::cmp::Ordering::Greater;
                };
                rhs = chunk.as_bytes();
                continue;
            }
            let len = lhs.len().min(rhs.len());
            let (Some((lp, lr)), Some((rp, rr))) =
                (lhs.split_at_checked(len), rhs.split_at_checked(len))
            else {
                return lhs.cmp(rhs);
            };
            match lp.cmp(rp) {
                std::cmp::Ordering::Equal => (lhs, rhs) = (lr, rr),
                order => return order,
            }
        }
    }
}

/// A place in the text of a finished item or group.
#[derive(Debug, Clone, Copy)]
enum Frame {
    /// The next segment of an item.
    Item { item: usize, seg: usize },
    /// The next member of a group.
    Group { group: usize, next: usize },
}

/// The chunks of a finished item's or group's text, in order, walked with
/// an explicit stack.
struct Chunks<'r> {
    ropes: &'r Ropes,
    stack: Vec<Frame>,
}

impl<'r> Chunks<'r> {
    fn new(ropes: &'r Ropes, start: Frame) -> Self {
        Self {
            ropes,
            stack: vec![start],
        }
    }
}

impl<'r> Iterator for Chunks<'r> {
    type Item = &'r str;

    fn next(&mut self) -> Option<&'r str> {
        loop {
            match self.stack.last_mut()? {
                Frame::Item { item, seg } => {
                    let Some(&next) = self.ropes.items.get(*item).and_then(|segs| segs.get(*seg))
                    else {
                        self.stack.pop();
                        continue;
                    };
                    *seg = seg.wrapping_add(1);
                    match next {
                        Seg::Text(start, end) => {
                            return Some(self.ropes.arena.get(start..end).unwrap_or_default());
                        }
                        Seg::Group(group) => self.stack.push(Frame::Group { group, next: 0 }),
                    }
                }
                Frame::Group { group, next } => {
                    let Some(&item) = self.ropes.groups.get(*group).and_then(|m| m.get(*next))
                    else {
                        self.stack.pop();
                        continue;
                    };
                    let first = *next == 0;
                    *next = next.wrapping_add(1);
                    self.stack.push(Frame::Item { item, seg: 0 });
                    if !first {
                        return Some(", ");
                    }
                }
            }
        }
    }
}

/// Emits the text of `word`: fixed text, and its parts as pieces.
///
/// # Safety
///
/// As for [`debug`].
unsafe fn step(word: Word, p: &mut Printer) {
    // SAFETY: forwarded from the caller.
    match unsafe { view(word, "debug printing") } {
        View::Tagged(value) | View::Int(value) => p.text_owned(value.to_string()),
        View::U64(value) => p.text_owned(value.to_string()),
        View::Float(value) => p.text_owned(float_text(value)),
        View::F32(value) => p.text_owned(f32_text(value)),
        // Escaped as they are written, in chunks (D-617).
        View::String(_) => p.pending.push(Piece::Quoted(word)),
        View::Bytes(_) => p.pending.push(Piece::ByteList(word)),
        View::Tuple(fields) => {
            p.text("(");
            p.separated(fields);
            p.text(")");
        }
        View::Closure => p.text("<fn>"),
        View::Ctor(info) => {
            if let Some(hooks) = info.hooks() {
                // A type with hooks prints the text of its `debug` hook,
                // as it is (D-342, D-345).
                // SAFETY: a live value of a type with these hooks.
                p.pending
                    .push(Piece::Owned(unsafe { hooks::debug_text(hooks, word) }));
            } else {
                ctor(word, info, p);
            }
        }
        // A `dyn` value prints as the value inside (§3.13).
        View::Dyn { value, .. } => p.value(value),
        // A value of an external type prints as `<Name>` (D-233).
        View::External(info) => p.text_owned(format!("<{}>", info.type_name)),
        View::Desc(parts) => descriptor(&parts, p),
        // SAFETY: a live object; `collection_of` reads its layout id.
        View::Custom(kind) => match unsafe { hamt::collection_of(word) } {
            // SAFETY: a live map or set.
            Some(collection) => unsafe { hashed_collection(word, collection, p) },
            None => match kind.debug {
                // SAFETY: a live object of this kind.
                Some(debug) => unsafe { debug(word, p) },
                None => p.text_owned(format!("<{}>", kind.name)),
            },
        },
        View::Internal(info) => p.text_owned(format!("<{}>", info.name)),
    }
}

/// A `Map` as `map::from_list([(k, v), …])` and a `Set` as
/// `set::from_list([e, …])`, the entries sorted by their whole debug text
/// so the output is the same on every run (D-344, D-382).
///
/// # Safety
///
/// `word` is a live map or set root.
unsafe fn hashed_collection(word: Word, collection: Collection, p: &mut Printer) {
    let mut entries = ScratchVec::new();
    // SAFETY: forwarded from the caller; the entries are borrowed from the
    // collection, which outlives the walk.
    unsafe {
        hamt::for_each_entry(word, &mut |key, value| {
            entries.push(match value {
                Some(value) => SortItem::Pair(key, value),
                None => SortItem::Value(key),
            });
        });
    }
    p.text(match collection {
        Collection::Map => "map::from_list([",
        Collection::Set => "set::from_list([",
    });
    p.sorted(entries.drain());
    p.text("])");
}

/// A descriptor as its type: `Option<Int>`, `(Int, String)`,
/// `fn(Int) -> Bool`. Descriptors are not values; this text is for the
/// runtime's own messages.
fn descriptor(parts: &desc::Parts, p: &mut Printer) {
    match parts.shape() {
        Shape::Tuple => {
            p.text("(");
            p.separated(parts.args.iter().copied());
            p.text(")");
        }
        Shape::Fn => {
            let (params, result) = match parts.args.split_last() {
                Some((result, params)) => (params, Some(*result)),
                None => (&[][..], None),
            };
            p.text("fn(");
            p.separated(params.iter().copied());
            p.text(")");
            if let Some(result) = result {
                p.text(" -> ");
                p.value(result);
            }
        }
        Shape::Named => {
            // SAFETY: a descriptor's nominal descriptor is live.
            let (name, _) = unsafe { desc::names(parts.nominal) };
            p.text_owned(String::from_utf8_lossy(name).into_owned());
            if !parts.args.is_empty() {
                p.text("<");
                p.separated(parts.args.iter().copied());
                p.text(">");
            }
        }
    }
}

/// A constructor: `None`, `Some(3)`, `Point { x: 1, y: 2 }`.
fn ctor(word: Word, info: &'static LayoutInfo, p: &mut Printer) {
    p.text(&info.ctor_name);
    if info.fields.is_empty() {
        return;
    }
    p.text(if info.record { " { " } else { "(" });
    let obj = word_to_ptr(word);
    for (index, field) in info.fields.iter().enumerate() {
        if index > 0 {
            p.text(", ");
        }
        if let Some(name) = field.name.as_deref().filter(|_| info.record) {
            p.text(name);
            p.text(": ");
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

/// A `String` in double quotes with exactly the escapes a string literal
/// needs to read back as the same text (D-531): `\"`, `\\`, `\n`, `\t`,
/// `\r`, `\{`, and `\u{…}` for every other control character, in
/// lower-case hex without leading zeros (D-557).
/// Writes one code point or one escape at a time, as [`Chunked`] needs.
fn write_string(out: &mut impl fmt::Write, text: &str) {
    out.write_char('"').ok();
    for c in text.chars() {
        match c {
            '"' => out.write_str("\\\""),
            '\\' => out.write_str("\\\\"),
            '\n' => out.write_str("\\n"),
            '\t' => out.write_str("\\t"),
            '\r' => out.write_str("\\r"),
            '{' => out.write_str("\\{"),
            c if c.is_control() => write!(out, "\\u{{{:x}}}", u32::from(c)),
            c => out.write_char(c),
        }
        .ok();
    }
    out.write_char('"').ok();
}

/// `Bytes` as `bytes::from_list([0x66, 0x6f])` (D-388), one byte at a
/// time.
fn write_bytes(out: &mut impl fmt::Write, bytes: &[u8]) {
    out.write_str("bytes::from_list([").ok();
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            out.write_str(", ").ok();
        }
        write!(out, "0x{byte:02x}").ok();
    }
    out.write_str("])").ok();
}

/// The text of a `Float` (§3.3.6), from `std::float`'s kernel.
fn float_text(value: f64) -> String {
    crate::num::text::float_to_string(value)
}

/// The text of an `F32` (§3.3.6), judged at `F32` precision.
fn f32_text(value: f32) -> String {
    crate::num::text::f32_to_string(value)
}
