//! Diagnostics stored structured and replayed (D-965, `cache.md` §4.10).
//!
//! A module's diagnostics are stored with their code, message, labels
//! (span, message, style), notes and help lines with their edits; the
//! severity follows from the code (D-419). A span is stored in one of two
//! forms ([`StoredSpan`]):
//!
//! - a span in the module's own file, as its byte range: the module's key
//!   covers its source, so the range stays right;
//! - a span into another module, as that module's path and the item it
//!   points at, by the item's position in the module's encoded order
//!   (`cheby_iface::encoded_order`, the order of the interface artifact's
//!   section S, D-953). Replay resolves it against that module's current
//!   spans, so a note stays right after the item moves without changing
//!   the interface hash.
//!
//! Any other span into another module cannot be stored: the module is then
//! not cached, and debug toolchains raise an internal error naming it
//! (D-965's debug assertion; an `Ice` rather than a panic, D-422).
//! Diagnostics are rendered only at replay, so color, D-421's paths and
//! `--deny-warnings` (D-549) apply to them as to fresh ones.
//!
//! A blob holds the module's diagnostics as several lists (front records
//! hold three: phase A's, which a module that is not parsed replays, and
//! the front end's early and late ones), so replay pushes them where a
//! fresh check would. A module without diagnostics has the blob of
//! [`empty_blob`], whose name a reader recognizes without opening it.

use cheby_diag::{Code, Diagnostic, Edit, FileId, Help, Label, LabelStyle, Severity, Span};
use cheby_diag::{TextRange, TextSize};
use cheby_iface::{Globals, InterfaceSet, ModuleId, ModuleTable, encoded_order};

use super::wire::{Reader, Writer};
use crate::parse_module_path;

/// The version of the blob's encoding.
const VERSION: u8 = 1;

/// A stored span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoredSpan {
    /// A byte range in the module's own file.
    Own {
        /// The first byte.
        start: u32,
        /// The length in bytes.
        len: u32,
    },
    /// The span of an item of another module.
    Item {
        /// The module's path.
        module: String,
        /// The item's position in the module's encoded order.
        item: u32,
    },
}

/// Turns spans into [`StoredSpan`]s and back for one module: its own file,
/// and the interfaces of the modules its diagnostics may point into.
pub struct SpanCx<'a> {
    /// The module's own file.
    pub own: FileId,
    /// The length of the module's own file in bytes.
    pub own_len: u32,
    /// The modules of the build, with their files.
    pub modules: &'a ModuleTable,
    /// The interfaces published so far: the module's dependencies'.
    pub set: &'a InterfaceSet,
    /// The interner the interfaces use.
    pub globals: &'a Globals,
}

impl std::fmt::Debug for SpanCx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpanCx")
            .field("own", &self.own)
            .field("own_len", &self.own_len)
            .finish_non_exhaustive()
    }
}

impl SpanCx<'_> {
    /// The stored form of `span`, or `None` when it points into another
    /// module but not at one of its items.
    #[must_use]
    pub fn store(&self, span: Span) -> Option<StoredSpan> {
        if span.file == self.own {
            return Some(StoredSpan::Own {
                start: span.range.start(),
                len: span.range.len(),
            });
        }
        let (module, info) = self
            .modules
            .iter()
            .find(|(_, info)| info.file == Some(span.file))?;
        let iface = self.set.get(module)?;
        let order = encoded_order(iface, self.globals).ok()?;
        let at = order
            .iter()
            .position(|&index| iface.item(index).and_then(|i| i.span) == Some(span))?;
        Some(StoredSpan::Item {
            module: info.path.to_string(),
            item: u32::try_from(at).ok()?,
        })
    }

    /// The span a stored span names today, or `None` when it names
    /// nothing: a range past the module's file, an unknown module, or an
    /// item that is gone or has no span.
    #[must_use]
    pub fn resolve(&self, stored: &StoredSpan) -> Option<Span> {
        match stored {
            StoredSpan::Own { start, len } => {
                (start.checked_add(*len)? <= self.own_len).then(|| {
                    Span::new(
                        self.own,
                        TextRange::at(TextSize::from(*start), TextSize::from(*len)),
                    )
                })
            }
            StoredSpan::Item { module, item } => {
                let id: ModuleId = self.modules.find(&parse_module_path(module)?)?;
                let iface = self.set.get(id)?;
                let order = encoded_order(iface, self.globals).ok()?;
                let index = *order.get(usize::try_from(*item).ok()?)?;
                iface.item(index)?.span
            }
        }
    }
}

/// The blob of a module's diagnostics, as `lists` (each in order), or
/// `None` when a span cannot be stored ([`SpanCx::store`]).
#[must_use]
pub fn encode_diagnostics(lists: &[&[Diagnostic]], cx: &SpanCx<'_>) -> Option<Vec<u8>> {
    let mut w = Writer::new();
    w.u8(VERSION);
    w.len(lists.len());
    for list in lists {
        w.len(list.len());
        for d in *list {
            encode_one(&mut w, d, cx)?;
        }
    }
    Some(w.finish())
}

fn encode_one(w: &mut Writer, d: &Diagnostic, cx: &SpanCx<'_>) -> Option<()> {
    w.u8(match d.code.severity() {
        Severity::Error => 0,
        Severity::Warning => 1,
    });
    w.u16(d.code.number());
    w.str(&d.message);
    w.len(d.labels.len());
    for label in &d.labels {
        encode_span(w, &cx.store(label.span)?);
        w.str(&label.message);
        w.u8(match label.style {
            LabelStyle::Primary => 0,
            LabelStyle::Secondary => 1,
        });
    }
    w.len(d.notes.len());
    for note in &d.notes {
        w.str(note);
    }
    w.len(d.helps.len());
    for help in &d.helps {
        w.str(&help.message);
        w.len(help.edits.len());
        for edit in &help.edits {
            encode_span(w, &cx.store(edit.span)?);
            w.str(&edit.replacement);
        }
    }
    Some(())
}

fn encode_span(w: &mut Writer, span: &StoredSpan) {
    match span {
        StoredSpan::Own { start, len } => {
            w.u8(0);
            w.u32(*start);
            w.u32(*len);
        }
        StoredSpan::Item { module, item } => {
            w.u8(1);
            w.str(module);
            w.u32(*item);
        }
    }
}

/// The lists of a blob, with every span resolved by `cx`; `None` when the
/// blob is malformed, holds another number of lists than `lists`, or a
/// span no longer resolves.
#[must_use]
pub fn decode_diagnostics(
    bytes: &[u8],
    lists: usize,
    cx: &SpanCx<'_>,
) -> Option<Vec<Vec<Diagnostic>>> {
    decode_with(bytes, lists, &mut |stored| cx.resolve(stored))
}

/// The paths of the other modules whose items a blob's spans name,
/// sorted, without repeats: the interfaces [`decode_diagnostics`] needs
/// besides the module's own file. `None` when the blob is malformed or
/// holds another number of lists than `lists`.
#[must_use]
pub fn item_modules(bytes: &[u8], lists: usize) -> Option<Vec<String>> {
    let mut modules = Vec::new();
    let placeholder = Span::new(FileId::new(0), TextRange::empty(0));
    decode_with(bytes, lists, &mut |stored| {
        if let StoredSpan::Item { module, .. } = stored {
            modules.push(module.clone());
        }
        Some(placeholder)
    })?;
    modules.sort();
    modules.dedup();
    Some(modules)
}

/// The blob of `lists` empty lists: every module without diagnostics has
/// this one, so a reader that knows its name need not open it.
#[must_use]
pub fn empty_blob(lists: usize) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(VERSION);
    w.len(lists);
    for _ in 0..lists {
        w.len(0);
    }
    w.finish()
}

/// [`decode_diagnostics`] with every stored span resolved by `resolve`.
fn decode_with(
    bytes: &[u8],
    lists: usize,
    resolve: &mut dyn FnMut(&StoredSpan) -> Option<Span>,
) -> Option<Vec<Vec<Diagnostic>>> {
    let mut r = Reader::new(bytes);
    if r.u8()? != VERSION || r.len()? != lists {
        return None;
    }
    let mut out = Vec::new();
    for _ in 0..lists {
        let mut list = Vec::new();
        for _ in 0..r.len()? {
            list.push(decode_one(&mut r, resolve)?);
        }
        out.push(list);
    }
    r.is_done().then_some(out)
}

fn decode_one(
    r: &mut Reader<'_>,
    resolve: &mut dyn FnMut(&StoredSpan) -> Option<Span>,
) -> Option<Diagnostic> {
    let letter = match r.u8()? {
        0 => 'E',
        1 => 'W',
        _ => return None,
    };
    let code = Code::parse(&format!("{letter}{:04}", r.u16()?))?;
    let mut d = Diagnostic::new(code, r.str()?);
    for _ in 0..r.len()? {
        let span = resolve(&decode_span(r)?)?;
        let message = r.str()?.to_owned();
        let style = match r.u8()? {
            0 => LabelStyle::Primary,
            1 => LabelStyle::Secondary,
            _ => return None,
        };
        d.labels.push(Label {
            span,
            message,
            style,
        });
    }
    for _ in 0..r.len()? {
        d.notes.push(r.str()?.to_owned());
    }
    for _ in 0..r.len()? {
        let message = r.str()?.to_owned();
        let mut edits = Vec::new();
        for _ in 0..r.len()? {
            let span = resolve(&decode_span(r)?)?;
            edits.push(Edit {
                span,
                replacement: r.str()?.to_owned(),
            });
        }
        d.helps.push(Help { message, edits });
    }
    Some(d)
}

fn decode_span(r: &mut Reader<'_>) -> Option<StoredSpan> {
    match r.u8()? {
        0 => Some(StoredSpan::Own {
            start: r.u32()?,
            len: r.u32()?,
        }),
        1 => Some(StoredSpan::Item {
            module: r.str()?.to_owned(),
            item: r.u32()?,
        }),
        _ => None,
    }
}
