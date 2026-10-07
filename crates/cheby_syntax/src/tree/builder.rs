//! Builds a [`SyntaxTree`] from parser events and raw tokens (D-280).
//!
//! The parser names only significant tokens. The builder walks the raw
//! tokens in order and attaches every token no event names as trivia, so
//! the tree covers the whole text whatever the events say. Trivia goes:
//!
//! - before a `Token` or `Split` event: into the current node;
//! - before a `Start`: into the parent, so nodes stay tight around their
//!   first token, except that for a doc-attached kind (items, variants,
//!   record fields, interface functions) the trailing run of `///` comments,
//!   with the whitespace and line breaks between them, goes into the new
//!   node (§2.2);
//! - after the last event: into the root.
//!
//! The builder never panics on malformed events: a missing root is added, an
//! extra `Finish` is ignored, unclosed nodes are closed at the end, events
//! naming tokens out of order or out of range are skipped, and a node event
//! with a token kind becomes an `ERROR` node.

use std::sync::Arc;

use cheby_diag::{FileId, TextSize};

use super::{Element, NO_PARENT, SyntaxTree, id, index};
use crate::SyntaxKind;
use crate::event::Event;
use crate::lexer::Lexed;

/// Builds the tree of `text` from its tokens `lexed` and the parser's
/// `events`. `lexed` must be the result of lexing `text`.
#[must_use]
pub fn build(file: FileId, text: Arc<str>, lexed: &Lexed, mut events: Vec<Event>) -> SyntaxTree {
    let mut builder = Builder {
        lexed,
        elements: Vec::with_capacity(capacity(lexed, &events)),
        stack: Vec::new(),
        raw_pos: 0,
        split_off: 0,
        offset: 0,
        next_token_event: 0,
    };
    builder.open(SyntaxKind::SOURCE_FILE);

    // The root is opened above; skip the parser's own root `Start`.
    let first = match events.first() {
        Some(Event::Start {
            kind: SyntaxKind::SOURCE_FILE,
            forward_parent: None,
        }) => 1,
        _ => 0,
    };
    let mut kinds = Vec::new();
    for i in first..events.len() {
        let Some(event) = events.get(i).copied() else {
            break;
        };
        match event {
            Event::Tombstone => {}
            Event::Start {
                kind,
                forward_parent,
            } => {
                kinds.clear();
                kinds.push(kind);
                let mut at = i;
                let mut next = forward_parent;
                while let Some(distance) = next {
                    at = at.saturating_add(index(distance.get()));
                    let Some(slot) = events.get_mut(at) else {
                        break;
                    };
                    let Event::Start {
                        kind,
                        forward_parent,
                    } = *slot
                    else {
                        break;
                    };
                    *slot = Event::Tombstone;
                    kinds.push(kind);
                    next = forward_parent;
                }
                let next_raw = builder.next_token_raw(&events, i);
                for &kind in kinds.iter().rev() {
                    builder.start(kind, next_raw);
                }
            }
            Event::Finish => builder.finish(),
            Event::Token { raw } => builder.token(index(raw)),
            Event::Split {
                raw,
                kind,
                offset,
                len,
            } => builder.split(index(raw), kind, u32::from(offset), u32::from(len)),
        }
    }

    while builder.stack.len() > 1 {
        builder.close();
    }
    builder.flush_trivia(lexed.len());
    builder.close();

    SyntaxTree {
        file,
        text,
        elements: builder.elements,
    }
}

/// The number of elements the tree will have, when the events are well
/// formed: every raw token, one node per `Start` plus the root, and one more
/// token per `Split`.
fn capacity(lexed: &Lexed, events: &[Event]) -> usize {
    let extra = events
        .iter()
        .filter(|e| matches!(e, Event::Start { .. } | Event::Split { .. }))
        .count();
    lexed.len().saturating_add(extra).saturating_add(1)
}

struct Builder<'a> {
    lexed: &'a Lexed,
    elements: Vec<Element>,
    /// Indices of the open nodes, innermost last.
    stack: Vec<u32>,
    /// The first raw token not yet (fully) in the tree.
    raw_pos: usize,
    /// How many bytes of `raw_pos` `Split` events have already emitted.
    split_off: TextSize,
    /// The end of the last element emitted.
    offset: TextSize,
    /// The index of the next `Token` or `Split` event, cached so finding it
    /// is linear over the whole build.
    next_token_event: usize,
}

/// Kinds whose leading `///` comments belong inside the node (§2.2).
const fn is_doc_attached(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::FN_DECL
            | SyntaxKind::TYPE_DECL
            | SyntaxKind::ALIAS_DECL
            | SyntaxKind::CONST_DECL
            | SyntaxKind::INTERFACE_DECL
            | SyntaxKind::TEST_DECL
            | SyntaxKind::VARIANT
            | SyntaxKind::RECORD_FIELD
            | SyntaxKind::INTERFACE_FN
    )
}

impl Builder<'_> {
    /// The raw token named by the first `Token` or `Split` event after
    /// event `from`, or the token count if there is none.
    fn next_token_raw(&mut self, events: &[Event], from: usize) -> usize {
        if self.next_token_event <= from {
            self.next_token_event = from.saturating_add(1);
        }
        while let Some(event) = events.get(self.next_token_event) {
            match *event {
                Event::Token { raw } | Event::Split { raw, .. } => return index(raw),
                _ => self.next_token_event = self.next_token_event.saturating_add(1),
            }
        }
        self.lexed.len()
    }

    fn parent(&self) -> u32 {
        self.stack.last().copied().unwrap_or(NO_PARENT)
    }

    fn open(&mut self, kind: SyntaxKind) {
        let kind = if kind.is_node() {
            kind
        } else {
            SyntaxKind::ERROR
        };
        let at = id(self.elements.len());
        self.elements.push(Element {
            kind,
            parent: self.parent(),
            start: self.offset,
            end: self.offset,
            subtree_end: at.saturating_add(1),
        });
        self.stack.push(at);
    }

    fn close(&mut self) {
        let Some(at) = self.stack.pop() else {
            return;
        };
        let end = id(self.elements.len());
        if let Some(node) = self.elements.get_mut(index(at)) {
            node.end = self.offset;
            node.subtree_end = end;
        }
    }

    fn emit(&mut self, kind: SyntaxKind, start: TextSize, end: TextSize) {
        let at = id(self.elements.len());
        self.elements.push(Element {
            kind,
            parent: self.parent(),
            start,
            end,
            subtree_end: at.saturating_add(1),
        });
        self.offset = end;
    }

    /// Emits raw tokens up to (not including) `upto` into the current node,
    /// first the rest of a partly split token.
    fn flush_trivia(&mut self, upto: usize) {
        let upto = upto.min(self.lexed.len());
        if self.split_off > 0 && self.raw_pos < upto {
            let range = self.lexed.range(self.raw_pos);
            let start = range
                .start()
                .saturating_add(self.split_off)
                .min(range.end());
            if start < range.end() {
                self.emit(SyntaxKind::ERROR_TOKEN, start, range.end());
            }
            self.raw_pos = self.raw_pos.saturating_add(1);
            self.split_off = 0;
        }
        while self.raw_pos < upto {
            let range = self.lexed.range(self.raw_pos);
            self.emit(self.lexed.kind(self.raw_pos), range.start(), range.end());
            self.raw_pos = self.raw_pos.saturating_add(1);
        }
    }

    fn start(&mut self, kind: SyntaxKind, next_raw: usize) {
        let upto = next_raw.max(self.raw_pos).min(self.lexed.len());
        let mut split = upto;
        if is_doc_attached(kind) {
            let mut run = upto;
            while run > self.raw_pos {
                let before = run.saturating_sub(1);
                if !matches!(
                    self.lexed.kind(before),
                    SyntaxKind::DOC_COMMENT | SyntaxKind::WHITESPACE | SyntaxKind::NEWLINE
                ) {
                    break;
                }
                run = before;
            }
            if let Some(doc) = (run..upto).find(|&i| self.lexed.kind(i) == SyntaxKind::DOC_COMMENT)
            {
                split = doc;
            }
        }
        self.flush_trivia(split);
        self.open(kind);
    }

    fn finish(&mut self) {
        // The root stays open until all tokens are in.
        if self.stack.len() > 1 {
            self.close();
        }
    }

    fn token(&mut self, raw: usize) {
        if raw < self.raw_pos || raw >= self.lexed.len() {
            return;
        }
        self.flush_trivia(raw);
        if self.split_off > 0 {
            // The parser split this token and now names all of it; emit
            // the rest.
            self.flush_trivia(raw.saturating_add(1));
            return;
        }
        let range = self.lexed.range(raw);
        self.emit(self.lexed.kind(raw), range.start(), range.end());
        self.raw_pos = raw.saturating_add(1);
    }

    fn split(&mut self, raw: usize, kind: SyntaxKind, offset: u32, len: u32) {
        if raw < self.raw_pos || raw >= self.lexed.len() {
            return;
        }
        self.flush_trivia(raw);
        let range = self.lexed.range(raw);
        let done = range
            .start()
            .saturating_add(self.split_off)
            .min(range.end());
        let start = range
            .start()
            .saturating_add(offset)
            .clamp(done, range.end());
        let end = start.saturating_add(len).min(range.end());
        if start > done {
            self.emit(SyntaxKind::ERROR_TOKEN, done, start);
        }
        if end > start {
            let kind = if kind.is_token() {
                kind
            } else {
                SyntaxKind::ERROR_TOKEN
            };
            self.emit(kind, start, end);
        }
        let consumed = end.max(done);
        if consumed >= range.end() {
            self.raw_pos = raw.saturating_add(1);
            self.split_off = 0;
        } else {
            self.split_off = consumed.saturating_sub(range.start());
        }
    }
}
