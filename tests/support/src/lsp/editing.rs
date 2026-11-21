//! Completion, signature help and inlay hints in the harness (plan W8,
//! `tests/lsp/README.md`, "Editing features").
//!
//! - [`completions`]: a completion answer as a client reads it, with the
//!   list's `itemDefaults` merged into each item and the items in the
//!   client's order, by `sortText`.
//! - [`summary`] and [`printed`]: the readable forms of an item that the
//!   goldens hold.
//! - [`check_edit`]: every item's edit sits on one line and holds the
//!   position (D-1394, and the protocol's rule for `textEdit`).
//! - [`apply_item`] and [`expand_snippet`]: the text after accepting an
//!   item, as a client writes it, and [`syntax_errors`] of that text.
//! - [`signature_help`] and [`inlay_hints`]: the readable forms of those
//!   answers.
//! - [`completion_points`] and [`signature_points`]: where the examples
//!   group asks for completion and signature help.
//! - [`check_applied`]: the applied-edit check of every edit golden (plan
//!   §1.7, D-1676): the edit is applied through the server with
//!   `didChange`, the server is waited for ([`Settle`]), and no file may
//!   show a new error and no touched line may lose `cheby fmt`'s form,
//!   which `rangeFormatting` answers (D-1399), with D-1670's narrowing for
//!   rename. [`revert`] undoes it, and [`shown_messages`] prints the
//!   `window/showMessage`s of a rename or a `willRename` block.
//! - [`action_block`]: an `action TITLE` marker resolves the titled code
//!   action and applies its fix through [`check_applied`], whose fixed code
//!   must be gone ([`gone_at_span`]); a `noAction TITLE` marker lists the
//!   actions and fails if one has that title ([`no_action_block`]);
//!   [`execute_block`] and
//!   [`explain_block`] are the `execute` and `explain` steps.
#![expect(
    clippy::wildcard_enum_match_arm,
    reason = "a test matches only the variants it checks"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::client::{self, Client, Paths};
use super::text::{self, Bom, Encoding, Lines, Position};

/// The protocol's `CompletionItemKind` names, from 1.
const COMPLETION_KINDS: [&str; 25] = [
    "Text",
    "Method",
    "Function",
    "Constructor",
    "Field",
    "Variable",
    "Class",
    "Interface",
    "Module",
    "Property",
    "Unit",
    "Value",
    "Enum",
    "Keyword",
    "Snippet",
    "Color",
    "File",
    "Reference",
    "Folder",
    "EnumMember",
    "Constant",
    "Struct",
    "Event",
    "Operator",
    "TypeParameter",
];

/// The protocol's `insertTextFormat` of a snippet.
const SNIPPET: u64 = 2;

/// The protocol's `insertTextMode` `asIs`.
const AS_IS: u64 = 1;

/// The name of `CompletionItemKind` `kind`, `-` for none or an unknown
/// number.
#[must_use]
pub fn kind_name(kind: Option<u64>) -> &'static str {
    kind.and_then(|k| usize::try_from(k).ok())
        .and_then(|k| k.checked_sub(1))
        .and_then(|k| COMPLETION_KINDS.get(k))
        .copied()
        .unwrap_or("-")
}

/// A completion answer as a client reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completions {
    /// `isIncomplete` of a `CompletionList`; `false` for a bare list.
    pub incomplete: bool,
    /// The items, with the list's `itemDefaults` merged in, in the
    /// client's order.
    pub items: Vec<Value>,
}

/// Reads a completion answer: `null`, a list of items, or a
/// `CompletionList`. The list's `itemDefaults` are merged into each item
/// that lacks the field, as a client merges them, and the items are put
/// in the client's order: by `sortText`, or the label where there is
/// none, and then as the server sent them.
///
/// # Errors
///
/// Returns a message for an answer of another shape.
pub fn completions(answer: &Value) -> Result<Completions, String> {
    let (incomplete, items, defaults) = match answer {
        Value::Null => (false, Vec::new(), None),
        Value::Array(items) => (false, items.clone(), None),
        Value::Object(list) => {
            let items = list
                .get("items")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("a CompletionList without items: {answer}"))?;
            let incomplete = list
                .get("isIncomplete")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            (incomplete, items.clone(), list.get("itemDefaults"))
        }
        _ => return Err(format!("not a completion answer: {answer}")),
    };
    let mut items: Vec<Value> = items
        .into_iter()
        .map(|item| with_defaults(item, defaults))
        .collect();
    items.sort_by(|a, b| sort_key(a).cmp(sort_key(b)));
    Ok(Completions { incomplete, items })
}

/// What a client sorts an item by.
fn sort_key(item: &Value) -> &str {
    item.get("sortText")
        .and_then(Value::as_str)
        .or_else(|| item.get("label").and_then(Value::as_str))
        .unwrap_or_default()
}

/// `item` with each field of `defaults`, a list's `itemDefaults`, that it
/// lacks. `editRange` becomes a `textEdit` whose text is the item's
/// `textEditText`, `insertText` or label.
fn with_defaults(mut item: Value, defaults: Option<&Value>) -> Value {
    let (Some(Value::Object(defaults)), Value::Object(map)) = (defaults, &mut item) else {
        return item;
    };
    for key in [
        "commitCharacters",
        "insertTextFormat",
        "insertTextMode",
        "data",
    ] {
        if let Some(value) = defaults.get(key)
            && !map.contains_key(key)
        {
            map.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(range) = defaults.get("editRange")
        && !map.contains_key("textEdit")
    {
        let new_text = ["textEditText", "insertText", "label"]
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str))
            .unwrap_or_default()
            .to_owned();
        let edit = match (range.get("insert"), range.get("replace")) {
            (Some(insert), Some(replace)) => {
                json!({ "newText": new_text, "insert": insert, "replace": replace })
            }
            _ => json!({ "newText": new_text, "range": range }),
        };
        map.insert("textEdit".to_owned(), edit);
    }
    item
}

/// The label of `item`.
fn label(item: &Value) -> &str {
    item.get("label")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// The `labelDetails.description` of `item`.
fn description(item: &Value) -> Option<&str> {
    item.get("labelDetails")
        .and_then(|d| d.get("description"))
        .and_then(Value::as_str)
}

/// The text the main edit of `item` writes, as sent: the `textEdit`'s, or
/// `insertText`.
fn written(item: &Value) -> Option<&str> {
    item.get("textEdit")
        .and_then(|e| e.get("newText"))
        .or_else(|| item.get("insertText"))
        .and_then(Value::as_str)
}

/// The `additionalTextEdits` of `item`.
fn additional(item: &Value) -> &[Value] {
    item.get("additionalTextEdits")
        .and_then(Value::as_array)
        .map_or_default(Vec::as_slice)
}

/// Whether `item` is deprecated, by its tags or its old flag.
fn deprecated(item: &Value) -> bool {
    let tagged = item
        .get("tags")
        .and_then(Value::as_array)
        .is_some_and(|tags| tags.iter().any(|t| t.as_u64() == Some(1)));
    tagged || item.get("deprecated").and_then(Value::as_bool) == Some(true)
}

/// One line for `item`: its label with `labelDetails.detail`, its kind,
/// and where present its `detail`, `labelDetails.description` in
/// parentheses, the text its edit writes in backticks when that differs
/// from the label, `+N edits` for `additionalTextEdits`, and `deprecated`,
/// joined by ` | `.
#[must_use]
pub fn summary(item: &Value) -> String {
    let label = label(item);
    let mut head = label.to_owned();
    if let Some(detail) = item
        .get("labelDetails")
        .and_then(|d| d.get("detail"))
        .and_then(Value::as_str)
    {
        head.push_str(detail);
    }
    let kind = item.get("kind").and_then(Value::as_u64);
    let mut parts = vec![head, kind_name(kind).to_owned()];
    if let Some(detail) = item.get("detail").and_then(Value::as_str) {
        parts.push(detail.to_owned());
    }
    if let Some(description) = description(item) {
        parts.push(format!("({description})"));
    }
    if let Some(text) = written(item).filter(|t| *t != label) {
        parts.push(format!("`{text}`"));
    }
    match additional(item).len() {
        0 => {}
        1 => parts.push("+1 edit".to_owned()),
        n => parts.push(format!("+{n} edits")),
    }
    if deprecated(item) {
        parts.push("deprecated".to_owned());
    }
    parts.join(" | ")
}

/// `item` as a golden prints it in full: without `data`, which is the
/// server's own.
#[must_use]
pub fn printed(item: &Value) -> Value {
    let mut item = item.clone();
    if let Value::Object(map) = &mut item {
        map.remove("data");
    }
    item
}

/// Whether `word` names `item`: its label, or `label@description` with
/// its `labelDetails.description`.
#[must_use]
pub fn names(item: &Value, word: &str) -> bool {
    let label = label(item);
    if word == label {
        return true;
    }
    description(item).is_some_and(|d| {
        word.strip_prefix(label)
            .and_then(|rest| rest.strip_prefix('@'))
            == Some(d)
    })
}

/// The words of a `completion` or `apply` marker: its arguments, joined by
/// spaces and split again, with a word in backticks kept whole, so that
/// `` `let guard` `` names one label.
///
/// # Errors
///
/// Returns a message for a backtick that is not closed.
pub fn words(args: &[String]) -> Result<Vec<String>, String> {
    let joined = args.join(" ");
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for c in joined.chars() {
        match c {
            '`' if quoted => {
                out.push(std::mem::take(&mut word));
                quoted = false;
            }
            '`' => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
                quoted = true;
            }
            c if c.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if quoted {
        return Err(format!("a backtick is not closed in `{joined}`"));
    }
    if !word.is_empty() {
        out.push(word);
    }
    Ok(out)
}

/// The `context` of a completion request at byte `offset` of `text`:
/// `triggerKind` 2 with the character before the offset when it is one of
/// `triggers`, as typing it would send, and `triggerKind` 1 otherwise.
#[must_use]
pub fn completion_context(text: &str, offset: usize, triggers: &[String]) -> Value {
    let before = text
        .get(..offset)
        .and_then(|t| t.chars().next_back())
        .map(String::from);
    match before {
        Some(c) if triggers.contains(&c) => json!({ "triggerKind": 2, "triggerCharacter": c }),
        _ => json!({ "triggerKind": 1 }),
    }
}

/// The ranges an edit writes: a `TextEdit`'s `range`, or an
/// `InsertReplaceEdit`'s `insert` and `replace`.
fn edit_ranges(edit: &Value) -> Result<Vec<(Position, Position)>, String> {
    let read = |range: &Value| -> Result<(Position, Position), String> {
        Ok((
            Position::from_json(range.get("start").unwrap_or(&Value::Null))?,
            Position::from_json(range.get("end").unwrap_or(&Value::Null))?,
        ))
    };
    if let Some(range) = edit.get("range") {
        return Ok(vec![read(range)?]);
    }
    match (edit.get("insert"), edit.get("replace")) {
        (Some(insert), Some(replace)) => Ok(vec![read(insert)?, read(replace)?]),
        _ => Err(format!(
            "an edit with neither a range nor insert and replace: {edit}"
        )),
    }
}

/// D-1394 and the protocol's rule for `textEdit`: the main edit of `item`,
/// answered at `at`, sits on one line and holds `at`. For an
/// `InsertReplaceEdit` both ranges must, and `insert` must be a prefix of
/// `replace`. No `additionalTextEdits` may overlap the main edit. Every
/// item must carry a `textEdit`, since the server sends one for each
/// (X §4.1): the list's `itemDefaults` are merged in before.
///
/// # Errors
///
/// Returns a message naming the item and what it breaks.
pub fn check_edit(item: &Value, at: Position) -> Result<(), String> {
    let name = label(item);
    let Some(edit) = item.get("textEdit") else {
        return Err(format!("`{name}`: it has no `textEdit` (X §4.1): {item}"));
    };
    let ranges = edit_ranges(edit).map_err(|e| format!("`{name}`: {e}"))?;
    for &(start, end) in &ranges {
        if start.line != end.line {
            return Err(format!(
                "`{name}`: its edit spans lines {} to {}, but a completion edit sits on one line \
                 (D-1394): {edit}",
                start.line, end.line
            ));
        }
        if at < start || end < at {
            return Err(format!(
                "`{name}`: its edit does not hold the position {}:{}: {edit}",
                at.line, at.character
            ));
        }
    }
    if let [(insert_start, insert_end), (replace_start, replace_end)] = ranges.as_slice()
        && (insert_start != replace_start || replace_end < insert_end)
    {
        return Err(format!(
            "`{name}`: its insert range is not a prefix of its replace range: {edit}"
        ));
    }
    let (main_start, main_end) = ranges
        .iter()
        .fold(None, |acc: Option<(Position, Position)>, &(s, e)| {
            Some(acc.map_or((s, e), |(a, b)| (a.min(s), b.max(e))))
        })
        .unwrap_or((at, at));
    for extra in additional(item) {
        for (start, end) in edit_ranges(extra).map_err(|e| format!("`{name}`: {e}"))? {
            if start < main_end && main_start < end {
                return Err(format!(
                    "`{name}`: an additional edit overlaps the main edit: {extra}"
                ));
            }
        }
    }
    Ok(())
}

/// The text a snippet writes when a client expands it with every
/// placeholder accepted: `${1:f}` gives `f`, `${1|a,b|}` gives `a`, `$1`,
/// `${1}` and `$0` give nothing, a variable gives its default or its name,
/// and `\$`, `\}` and `\\` give the character.
#[must_use]
pub fn expand_snippet(snippet: &str) -> String {
    let chars: Vec<char> = snippet.chars().collect();
    let mut out = String::with_capacity(snippet.len());
    let mut at = 0;
    expand(&chars, &mut at, &mut out, false);
    out
}

/// Expands from `chars[*at]` into `out`, until the end, or until the `}`
/// that closes a placeholder when `nested`.
fn expand(chars: &[char], at: &mut usize, out: &mut String, nested: bool) {
    while let Some(&c) = chars.get(*at) {
        match c {
            '\\' => {
                if let Some(&next @ ('$' | '}' | '\\')) = chars.get(at.saturating_add(1)) {
                    out.push(next);
                    *at = at.saturating_add(2);
                } else {
                    out.push('\\');
                    *at = at.saturating_add(1);
                }
            }
            '}' if nested => {
                *at = at.saturating_add(1);
                return;
            }
            '$' => dollar(chars, at, out),
            c => {
                out.push(c);
                *at = at.saturating_add(1);
            }
        }
    }
}

/// Expands the `$` at `chars[*at]`: a tab stop, a placeholder, a choice or
/// a variable.
fn dollar(chars: &[char], at: &mut usize, out: &mut String) {
    let next = at.saturating_add(1);
    let is_name = |c: char| c.is_ascii_alphanumeric() || c == '_';
    match chars.get(next) {
        Some(c) if c.is_ascii_digit() => {
            *at = skip_while(chars, next, |c| c.is_ascii_digit());
        }
        Some(&c) if c.is_ascii_alphabetic() || c == '_' => {
            let end = skip_while(chars, next, is_name);
            out.extend(chars.get(next..end).unwrap_or_default());
            *at = end;
        }
        Some('{') => {
            let start = next.saturating_add(1);
            let end = skip_while(chars, start, is_name);
            let name: String = chars.get(start..end).unwrap_or_default().iter().collect();
            let is_variable = !name.starts_with(|c: char| c.is_ascii_digit());
            *at = end;
            match chars.get(end) {
                Some('}') => {
                    if is_variable {
                        out.push_str(&name);
                    }
                    *at = end.saturating_add(1);
                }
                Some(':') => {
                    *at = end.saturating_add(1);
                    expand(chars, at, out, true);
                }
                Some('|') => {
                    *at = choice(chars, end.saturating_add(1), out);
                }
                _ => {
                    // A transform or a malformed snippet: skip to the `}`.
                    *at = skip_while(chars, end, |c| c != '}').saturating_add(1);
                }
            }
        }
        _ => {
            out.push('$');
            *at = next;
        }
    }
}

/// Writes the first option of the choice starting at `chars[at]` and
/// returns the index after its closing `|}`.
fn choice(chars: &[char], mut at: usize, out: &mut String) -> usize {
    let mut first = true;
    while let Some(&c) = chars.get(at) {
        match c {
            '\\' => {
                if first && let Some(&next) = chars.get(at.saturating_add(1)) {
                    out.push(next);
                }
                at = at.saturating_add(2);
            }
            ',' => {
                first = false;
                at = at.saturating_add(1);
            }
            '|' if chars.get(at.saturating_add(1)) == Some(&'}') => {
                return at.saturating_add(2);
            }
            c => {
                if first {
                    out.push(c);
                }
                at = at.saturating_add(1);
            }
        }
    }
    at
}

/// The first index from `at` whose character fails `keep`.
fn skip_while(chars: &[char], mut at: usize, keep: impl Fn(char) -> bool) -> usize {
    while chars.get(at).is_some_and(|&c| keep(c)) {
        at = at.saturating_add(1);
    }
    at
}

/// `text` after a client accepts `item`, answered at `at`: the main edit,
/// the `textEdit` or an `InsertReplaceEdit`'s `insert` range, with a
/// snippet expanded ([`expand_snippet`]), then the `additionalTextEdits`
/// on the same text. An item with no `textEdit` replaces the word before
/// `at` with its `insertText` or label, as clients do. Each later line of
/// the main edit's text gets the indentation of the cursor's line, up to
/// the cursor, as `insertTextMode` `adjustIndentation` and Zed's default
/// do, unless the item says `asIs` (D-1506).
///
/// # Errors
///
/// Returns a message for a malformed or overlapping edit.
pub fn apply_item(
    text: &str,
    item: &Value,
    at: Position,
    encoding: Encoding,
) -> Result<String, String> {
    let lines = Lines::new(text, Bom::Counted);
    let (range, new_text) = if let Some(edit) = item.get("textEdit") {
        let range = edit
            .get("range")
            .or_else(|| edit.get("insert"))
            .cloned()
            .ok_or_else(|| format!("an edit with no range: {edit}"))?;
        let new_text = edit
            .get("newText")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("an edit with no newText: {edit}"))?;
        (range, new_text.to_owned())
    } else {
        let end = lines.offset(at, encoding);
        let before = text.get(..end).unwrap_or_default();
        let word = before
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .map(char::len_utf8)
            .sum::<usize>();
        let start = lines.position(end.saturating_sub(word), encoding);
        let new_text = item
            .get("insertText")
            .and_then(Value::as_str)
            .unwrap_or_else(|| label(item));
        (
            json!({ "start": start.to_json(), "end": at.to_json() }),
            new_text.to_owned(),
        )
    };
    let snippet = item.get("insertTextFormat").and_then(Value::as_u64) == Some(SNIPPET);
    let new_text = if snippet {
        expand_snippet(&new_text)
    } else {
        new_text
    };
    let new_text = if item.get("insertTextMode").and_then(Value::as_u64) == Some(AS_IS) {
        new_text
    } else {
        // The protocol takes the indentation only up to the cursor. A
        // blank or tab is one unit in every encoding, so the cursor's
        // column counts characters up to the first one that is not.
        let line = usize::try_from(at.line)
            .ok()
            .and_then(|n| lines.line(n))
            .unwrap_or_default();
        let cursor = usize::try_from(at.character).unwrap_or(usize::MAX);
        let indent: String = line
            .chars()
            .take(cursor)
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        new_text.replace('\n', &format!("\n{indent}"))
    };
    let mut edits = vec![json!({ "range": range, "newText": new_text })];
    edits.extend(additional(item).iter().cloned());
    text::apply_text_edits(text, &edits, encoding, Bom::Counted)
}

/// The grammar errors of `text`, each as `L:C: message` with the
/// compiler's 1-based line and code-point column.
#[must_use]
pub fn syntax_errors(text: &str) -> Vec<String> {
    let parse = cheby_syntax::parse(cheby_diag::FileId::new(0), text.into());
    parse
        .errors()
        .iter()
        .map(|d| {
            let at = d
                .primary_span()
                .and_then(|s| usize::try_from(s.range.start()).ok());
            match at {
                Some(offset) => {
                    let (line, column) = compiler_line_column(text, offset);
                    format!("{line}:{column}: {} {}", d.code, d.message)
                }
                None => format!("{} {}", d.code, d.message),
            }
        })
        .collect()
}

/// The compiler's 1-based line and code-point column of byte `offset`
/// (D-421).
fn compiler_line_column(text: &str, offset: usize) -> (usize, usize) {
    let before = text.get(..offset).unwrap_or(text);
    let line = before.matches('\n').count().saturating_add(1);
    let start = before.rfind('\n').map_or(0, |i| i.saturating_add(1));
    let column = before
        .get(start..)
        .unwrap_or_default()
        .chars()
        .count()
        .saturating_add(1);
    (line, column)
}

/// A text as a golden prints it: one string per line.
#[must_use]
pub fn text_lines(text: &str) -> Value {
    Value::Array(text.split('\n').map(|l| json!(l)).collect())
}

/// A signature help answer as a golden prints it: each parameter's label
/// as text. A label given as offsets, in the negotiated `encoding`,
/// becomes `"[start, end] text"`; a string stays as it is.
///
/// # Errors
///
/// Returns a message for offsets outside their signature's label.
pub fn signature_help(answer: &Value, encoding: Encoding) -> Result<Value, String> {
    let mut answer = answer.clone();
    let Some(signatures) = answer.get_mut("signatures").and_then(Value::as_array_mut) else {
        return Ok(answer);
    };
    for signature in signatures {
        let label = signature
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(parameters) = signature
            .get_mut("parameters")
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        for parameter in parameters {
            let Some(offsets) = parameter.get("label").and_then(Value::as_array) else {
                continue;
            };
            let numbers: Vec<u64> = offsets.iter().filter_map(Value::as_u64).collect();
            let [start, end] = numbers.as_slice() else {
                return Err(format!(
                    "a parameter label that is not two offsets: {parameter}"
                ));
            };
            let text = slice_units(&label, *start, *end, encoding).ok_or_else(|| {
                format!("offsets [{start}, {end}] are not inside the label `{label}`")
            })?;
            let shown = format!("[{start}, {end}] {text}");
            if let Value::Object(map) = parameter {
                map.insert("label".to_owned(), json!(shown));
            }
        }
    }
    Ok(answer)
}

/// The part of `text` from unit `start` to unit `end` of `encoding`, or
/// `None` when either is not at a character boundary inside it.
fn slice_units(text: &str, start: u64, end: u64, encoding: Encoding) -> Option<&str> {
    let byte_of = |wanted: u64| -> Option<usize> {
        let mut units = 0_u64;
        for (at, c) in text.char_indices() {
            if units == wanted {
                return Some(at);
            }
            units = units.saturating_add(u64::try_from(encoding.units(c)).ok()?);
        }
        (units == wanted).then_some(text.len())
    };
    let (a, b) = (byte_of(start)?, byte_of(end)?);
    if b < a {
        return None;
    }
    text.get(a..b)
}

/// An inlay hint answer as a golden prints it: one line per hint, sorted
/// by position, then as sent. A line is the compiler's 1-based line and
/// code-point column of `text`, the kind (`type`, `parameter` or `-`), and
/// the label in backticks, or its parts each in backticks joined by `+`
/// with a part's location as `@path:L:C` (the protocol's position plus
/// one, through `display` for the URI). Then, where set,
/// `padding-left`, `padding-right`, `tooltip` and each of `textEdits` as
/// `edit L:C-L:C` and its text in backticks. Any other answer is returned
/// as it is.
#[must_use]
pub fn inlay_hints(
    answer: &Value,
    text: &str,
    encoding: Encoding,
    display: &dyn Fn(&str) -> String,
) -> Value {
    let Some(hints) = answer.as_array() else {
        return answer.clone();
    };
    let lines = Lines::new(text, Bom::Counted);
    let at = |position: &Value| -> (usize, usize) {
        let position = Position::from_json(position).unwrap_or(Position {
            line: u32::MAX,
            character: 0,
        });
        compiler_line_column(text, lines.offset(position, encoding))
    };
    let mut shown: Vec<((usize, usize), String)> = hints
        .iter()
        .map(|hint| {
            let position = at(hint.get("position").unwrap_or(&Value::Null));
            let kind = match hint.get("kind").and_then(Value::as_u64) {
                Some(1) => "type",
                Some(2) => "parameter",
                _ => "-",
            };
            let label = match hint.get("label") {
                Some(Value::Array(parts)) => parts
                    .iter()
                    .map(|part| label_part(part, display))
                    .collect::<Vec<_>>()
                    .join("+"),
                Some(Value::String(s)) => format!("`{s}`"),
                other => format!("{}", other.unwrap_or(&Value::Null)),
            };
            let mut line = format!("{}:{} {kind} {label}", position.0, position.1);
            for (key, word) in [
                ("paddingLeft", "padding-left"),
                ("paddingRight", "padding-right"),
            ] {
                if hint.get(key).and_then(Value::as_bool) == Some(true) {
                    line.push(' ');
                    line.push_str(word);
                }
            }
            if let Some(tooltip) = hint.get("tooltip").and_then(tooltip_text) {
                let _ = write!(line, " tooltip `{tooltip}`");
            }
            for edit in hint
                .get("textEdits")
                .and_then(Value::as_array)
                .map_or_default(Vec::as_slice)
            {
                let range = edit.get("range").unwrap_or(&Value::Null);
                let (sl, sc) = at(range.get("start").unwrap_or(&Value::Null));
                let (el, ec) = at(range.get("end").unwrap_or(&Value::Null));
                let new_text = edit
                    .get("newText")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let _ = write!(line, " edit {sl}:{sc}-{el}:{ec} `{new_text}`");
            }
            (position, line)
        })
        .collect();
    shown.sort_by_key(|(position, _)| *position);
    Value::Array(shown.into_iter().map(|(_, line)| json!(line)).collect())
}

/// One part of an inlay hint's label: its value in backticks, and its
/// location, if any.
fn label_part(part: &Value, display: &dyn Fn(&str) -> String) -> String {
    let value = part
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut shown = format!("`{value}`");
    if let Some(location) = part.get("location") {
        let uri = location
            .get("uri")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let start = location
            .get("range")
            .and_then(|r| r.get("start"))
            .and_then(|p| Position::from_json(p).ok());
        if let Some(start) = start {
            let _ = write!(
                shown,
                "@{}:{}:{}",
                display(uri),
                start.line.saturating_add(1),
                start.character.saturating_add(1)
            );
        }
    }
    shown
}

/// The text of a tooltip: a string, or a `MarkupContent`'s value.
fn tooltip_text(tooltip: &Value) -> Option<String> {
    match tooltip {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => map.get("value").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

/// Where the examples group asks for completion in `text`, as sorted byte
/// offsets: the end of every identifier and type name, and the point after
/// every `.` and `|>`.
#[must_use]
pub fn completion_points(text: &str) -> Vec<usize> {
    use cheby_syntax::SyntaxKind;
    let lexed = cheby_syntax::lexer::lex(cheby_diag::FileId::new(0), text);
    let mut points: Vec<usize> = (0..lexed.len())
        .filter(|&i| {
            matches!(
                lexed.kind(i),
                SyntaxKind::LOWER_IDENT
                    | SyntaxKind::UPPER_IDENT
                    | SyntaxKind::DOT
                    | SyntaxKind::PIPE_GT
            )
        })
        .filter_map(|i| usize::try_from(lexed.range(i).end()).ok())
        .collect();
    points.sort_unstable();
    points.dedup();
    points
}

/// Where the examples group asks for signature help in `text`, as sorted
/// byte offsets: inside every argument list, after its `(` and after each
/// of its own commas, and at its `)`.
#[must_use]
pub fn signature_points(text: &str) -> Vec<usize> {
    use cheby_syntax::{SyntaxElement, SyntaxKind};
    let parse = cheby_syntax::parse(cheby_diag::FileId::new(0), text.into());
    let mut points = Vec::new();
    for list in parse
        .tree()
        .root()
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::ARG_LIST)
    {
        for element in list.children_with_tokens() {
            let SyntaxElement::Token(token) = element else {
                continue;
            };
            let range = token.text_range();
            let at = match token.kind() {
                SyntaxKind::L_PAREN | SyntaxKind::COMMA => range.end(),
                SyntaxKind::R_PAREN => range.start(),
                _ => continue,
            };
            if let Ok(at) = usize::try_from(at) {
                points.push(at);
            }
        }
    }
    points.sort_unstable();
    points.dedup();
    points
}

/// The code actions of the `codeAction` answers of `prepared`, by the
/// labels of their requests: how many they list in all, and each that
/// carries `data`, with the label of its listing, for a resolve.
#[must_use]
pub fn listed_actions(prepared: &BTreeMap<String, Value>) -> (usize, Vec<(String, Value)>) {
    let listings = prepared
        .iter()
        .filter(|(label, _)| label.ends_with(" codeAction"))
        .map(|(label, answer)| (label, answer.as_array().map_or(&[][..], Vec::as_slice)));
    let mut all = 0_usize;
    let mut with_data = Vec::new();
    for (label, actions) in listings {
        all = all.saturating_add(actions.len());
        with_data.extend(
            actions
                .iter()
                .filter(|action| action.get("data").is_some_and(|d| !d.is_null()))
                .map(|action| (label.clone(), action.clone())),
        );
    }
    (all, with_data)
}

/// Picks every `step`-th of `points`, from the first, keeping at most
/// `budget`, so a sample is the same on every run. Returns the sample and
/// how many points were dropped.
#[must_use]
pub fn sample<T: Clone>(points: &[T], budget: usize) -> (Vec<T>, usize) {
    if points.len() <= budget || budget == 0 {
        return (points.to_vec(), 0);
    }
    let step = points.len().div_ceil(budget);
    let kept: Vec<T> = points.iter().step_by(step).cloned().collect();
    let dropped = points.len().saturating_sub(kept.len());
    (kept, dropped)
}

/// Every item of `points` in a fixed stride order: every `step`-th from
/// the first, then every `step`-th from the second, and so on, with the
/// `step` that makes the first pass at most `budget` long. A sweep that
/// stops after `budget` items has then spread them over the whole list,
/// not taken the first ones, and the order is the same on every run.
#[must_use]
pub fn by_stride<T: Clone>(points: &[T], budget: usize) -> Vec<T> {
    let step = points.len().div_ceil(budget.max(1)).max(1);
    (0..step)
        .flat_map(|start| points.iter().skip(start).step_by(step))
        .cloned()
        .collect()
}

/// The JSON object of `pairs`, in their order, which the golden printer
/// then sorts by key.
#[must_use]
pub fn object(pairs: Vec<(&str, Value)>) -> Value {
    let mut map = Map::new();
    for (key, value) in pairs {
        map.insert(key.to_owned(), value);
    }
    Value::Object(map)
}

// ---------------------------------------------------------------------
// The applied-edit check (plan §1.7, D-1676, D-1670)

/// The method of the server's message notification.
pub const SHOW_MESSAGE: &str = "window/showMessage";

/// The width a touched line must fit for D-1670's narrowing of the fixed
/// point, `cheby fmt`'s.
pub const WIDTH: usize = 100;

/// How long [`Settle::wait`] sleeps between two `cheby/status` requests.
const STATUS_POLL: Duration = Duration::from_millis(10);

/// Whether `message` is a `window/showMessage`.
#[must_use]
pub fn is_show_message(message: &Value) -> bool {
    message.get("method").and_then(Value::as_str) == Some(SHOW_MESSAGE)
}

/// The `window/showMessage` notifications among `notifications` as a
/// golden prints them, in order: one line each, the type's name and the
/// text, as `warning: …`. Other notifications are left out.
#[must_use]
pub fn shown_messages(notifications: &[Value]) -> Value {
    let lines = notifications
        .iter()
        .filter(|m| is_show_message(m))
        .map(|m| {
            let params = client::params(m);
            let kind = match params.get("type").and_then(Value::as_u64) {
                Some(1) => "error",
                Some(2) => "warning",
                Some(3) => "info",
                Some(4) => "log",
                _ => "-",
            };
            let message = params
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            json!(format!("{kind}: {message}"))
        })
        .collect();
    Value::Array(lines)
}

/// Which touched lines must be a `cheby fmt` fixed point after an edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedPoint {
    /// Every touched line range (`features.md` §13).
    Strict,
    /// D-1670: only the touched lines that were fixed points before the
    /// edit and still fit [`WIDTH`] columns, since rename writes tokens
    /// only. When every edit of the file swaps one name for another
    /// ([`names_only`]), an answer that only breaks or joins lines
    /// ([`reflowed_only`]) passes too: a name of another width moves where
    /// `cheby fmt` breaks a construct, which rename leaves to the next
    /// format (D-1712).
    Rename,
    /// A `willRenameFiles` move's edit: every touched line range, as
    /// [`FixedPoint::Strict`], except that an answer that only breaks or
    /// joins lines passes when every edit of the file swaps one name or
    /// module path for another, as [`FixedPoint::Rename`] (D-1712).
    Move,
}

/// The text edits of one file of a `WorkspaceEdit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdits {
    /// The file.
    pub uri: String,
    /// The version a `documentChanges` entry names, `None` for `null`
    /// and for a `changes` map.
    pub version: Option<i64>,
    /// Whether the edits came from `documentChanges`, whose version is
    /// checked.
    pub versioned: bool,
    /// The `TextEdit`s or `AnnotatedTextEdit`s.
    pub edits: Vec<Value>,
}

/// The text edits of `edit`, a `WorkspaceEdit`, one entry per file: its
/// `documentChanges` when present, else its `changes`. `null` has none.
///
/// # Errors
///
/// Returns a message for a malformed edit, a resource operation, which no
/// edit of rename or a file operation makes (plan §1.2), and a file named
/// twice, since the edits of one file come as one entry (plan §1.2).
pub fn file_edits(edit: &Value) -> Result<Vec<FileEdits>, String> {
    let mut out: Vec<FileEdits> = Vec::new();
    if let Some(changes) = edit.get("documentChanges") {
        let changes = changes
            .as_array()
            .ok_or_else(|| format!("documentChanges is not an array: {changes}"))?;
        for change in changes {
            if let Some(kind) = change.get("kind") {
                return Err(format!(
                    "a {kind} resource operation, which no edit of rename or a file operation \
                     makes (plan §1.2): {change}"
                ));
            }
            let uri = change
                .pointer("/textDocument/uri")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("a text document edit without a URI: {change}"))?;
            let version = match change.pointer("/textDocument/version") {
                None | Some(Value::Null) => None,
                Some(version) => Some(
                    version
                        .as_i64()
                        .ok_or_else(|| format!("a version that is not a number: {change}"))?,
                ),
            };
            let edits = change
                .get("edits")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("a text document edit without edits: {change}"))?;
            out.push(FileEdits {
                uri: uri.to_owned(),
                version,
                versioned: true,
                edits: edits.clone(),
            });
        }
    } else if let Some(changes) = edit.get("changes") {
        let changes = changes
            .as_object()
            .ok_or_else(|| format!("changes is not an object: {changes}"))?;
        for (uri, edits) in changes {
            let edits = edits
                .as_array()
                .ok_or_else(|| format!("the edits of {uri} are not an array"))?;
            out.push(FileEdits {
                uri: uri.clone(),
                version: None,
                versioned: false,
                edits: edits.clone(),
            });
        }
    }
    let mut seen = BTreeSet::new();
    for file in &out {
        if !seen.insert(file.uri.as_str()) {
            return Err(format!(
                "{} has two entries; the edits of a file come as one (plan §1.2)",
                file.uri
            ));
        }
    }
    Ok(out)
}

/// The lines one group of edits of a file touches, 0-based and inclusive:
/// in the text before the edit and in the text after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Touched {
    /// The first and last line in the text before.
    pub before: (u32, u32),
    /// The first and last line in the text after.
    pub after: (u32, u32),
}

/// The lines `edits`, the text edits of one file, touch, in order: one
/// group per run of edits whose lines overlap, so two edits on one line
/// are one group. A line after an edit that inserts or removes lines moves
/// by as many in the text after.
///
/// # Errors
///
/// Returns a message for a malformed edit or a range that ends before it
/// starts.
pub fn touched(edits: &[Value]) -> Result<Vec<Touched>, String> {
    let mut spans = Vec::with_capacity(edits.len());
    for edit in edits {
        let range = edit
            .get("range")
            .ok_or_else(|| format!("an edit without a range: {edit}"))?;
        let start = Position::from_json(range.get("start").unwrap_or(&Value::Null))?;
        let end = Position::from_json(range.get("end").unwrap_or(&Value::Null))?;
        if end < start {
            return Err(format!("a range whose end is before its start: {edit}"));
        }
        let new_text = edit
            .get("newText")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("an edit without newText: {edit}"))?;
        let lines = i64::try_from(new_text.matches('\n').count()).unwrap_or(i64::MAX);
        spans.push((start, end, lines));
    }
    spans.sort_by_key(|&(start, end, _)| (start, end));
    let line = |n: i64| u32::try_from(n.max(0)).unwrap_or(u32::MAX);
    let mut out: Vec<Touched> = Vec::new();
    // How many lines the edits before this one added, less those removed.
    let mut delta = 0_i64;
    for (start, end, lines) in spans {
        let first = i64::from(start.line).saturating_add(delta);
        let last = first.saturating_add(lines);
        let removed = i64::from(end.line.saturating_sub(start.line));
        delta = delta.saturating_add(lines).saturating_sub(removed);
        let group = Touched {
            before: (start.line, end.line),
            after: (line(first), line(last)),
        };
        match out.last_mut() {
            Some(previous) if group.before.0 <= previous.before.1 => {
                previous.before.1 = previous.before.1.max(group.before.1);
                previous.after.1 = previous.after.1.max(group.after.1);
            }
            _ => out.push(group),
        }
    }
    Ok(out)
}

/// Whether every line of `lines` of `text` fits [`WIDTH`] columns, counted
/// in characters.
#[must_use]
pub fn fits(text: &str, lines: (u32, u32)) -> bool {
    let index = Lines::new(text, Bom::Counted);
    (lines.0..=lines.1).all(|n| {
        usize::try_from(n)
            .ok()
            .and_then(|n| index.line(n))
            .is_none_or(|line| line.chars().count() <= WIDTH)
    })
}

/// Whether `text` is one name: a letter or `_`, then letters, digits and
/// `_`.
fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// Whether `text` is one name or a module path, names joined by `::`.
fn is_name_or_path(text: &str) -> bool {
    text.split("::").all(is_name)
}

/// Whether every edit of `edits`, the text edits of one file whose text
/// before them is `before`, replaces one whole name or module path with
/// another, as rename's edits of a plain occurrence and a move's of an
/// import's path or a qualifier do (D-1670, D-1712).
#[must_use]
pub fn names_only(before: &str, edits: &[Value], encoding: Encoding) -> bool {
    let index = Lines::new(before, Bom::Counted);
    let part_of_name = |c: char| c.is_alphanumeric() || c == '_';
    edits.iter().all(|edit| {
        let Some(range) = edit.get("range") else {
            return false;
        };
        let Ok((start, end)) = text::offsets(&index, range, encoding) else {
            return false;
        };
        let new = edit
            .get("newText")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (Some(old), Some(head), Some(tail)) = (
            before.get(start..end),
            before.get(..start),
            before.get(end..),
        ) else {
            return false;
        };
        is_name_or_path(old)
            && is_name_or_path(new)
            && !head.chars().next_back().is_some_and(part_of_name)
            && !tail.chars().next().is_some_and(part_of_name)
    })
}

/// `text` with no whitespace and no comma before a closing bracket: what
/// stays the same when `cheby fmt` breaks a construct over lines or joins
/// it onto one, writing or dropping its trailing comma.
fn squeezed(text: &str) -> String {
    let kept: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    kept.iter()
        .enumerate()
        .filter(|&(at, &c)| {
            c != ','
                || !kept
                    .get(at.saturating_add(1))
                    .is_some_and(|n| matches!(n, ')' | ']' | '}'))
        })
        .map(|(_, &c)| c)
        .collect()
}

/// Whether `answer`, a `rangeFormatting` answer for `text`, only breaks
/// or joins lines: applied, it changes nothing but whitespace and the
/// trailing comma of a list broken over lines (D-1670).
#[must_use]
pub fn reflowed_only(text: &str, answer: &Value, encoding: Encoding) -> bool {
    let Some(edits) = answer.as_array() else {
        return false;
    };
    text::apply_text_edits(text, edits, encoding, Bom::Counted)
        .is_ok_and(|formatted| squeezed(&formatted) == squeezed(text))
}

/// The range of whole `lines` of `text`: from the start of the first to the
/// end of the last, in `encoding`.
#[must_use]
pub fn line_range(text: &str, lines: (u32, u32), encoding: Encoding) -> Value {
    let index = Lines::new(text, Bom::Counted);
    let end = usize::try_from(lines.1)
        .ok()
        .and_then(|n| index.line(n))
        .map_or(0, |line| encoding.count(line));
    json!({
        "start": { "line": lines.0, "character": 0 },
        "end": { "line": lines.1, "character": u32::try_from(end).unwrap_or(u32::MAX) },
    })
}

/// The errors of a published list as the check compares them: each one's
/// key, its code or, with none, its first line, and how a failure shows
/// it. A diagnostic with no severity counts as an error, as clients read
/// it.
fn errors_of(diagnostics: &[Value]) -> Vec<(String, String)> {
    diagnostics
        .iter()
        .filter(|d| {
            d.get("severity")
                .and_then(Value::as_u64)
                .is_none_or(|s| s == 1)
        })
        .map(|d| {
            let first = d
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or_default();
            let key = match d.get("code") {
                Some(Value::String(code)) => code.clone(),
                Some(Value::Number(code)) => code.to_string(),
                _ => format!("no code: {first}"),
            };
            let start = d
                .pointer("/range/start")
                .and_then(|p| Position::from_json(p).ok())
                .unwrap_or(Position {
                    line: 0,
                    character: 0,
                });
            let shown = format!(
                "{key} at {}:{}: {first}",
                start.line.saturating_add(1),
                start.character.saturating_add(1)
            );
            (key, shown)
        })
        .collect()
}

/// D-1676's comparison: every file of `after`, the published lists by path
/// after an edit, shows no error that `before` does not, compared by code as
/// multisets, except one `allowed` names: each code there excuses one new
/// error, in any file.
///
/// # Errors
///
/// Returns a message naming each new error and its file.
pub fn new_errors(
    before: &BTreeMap<String, Vec<Value>>,
    after: &BTreeMap<String, Vec<Value>>,
    allowed: &[String],
) -> Result<(), String> {
    let mut excused: BTreeMap<&str, usize> = BTreeMap::new();
    for code in allowed {
        let n = excused.entry(code.as_str()).or_insert(0);
        *n = n.saturating_add(1);
    }
    let mut found = Vec::new();
    for (file, list) in after {
        let mut old: BTreeMap<String, usize> = BTreeMap::new();
        for (key, _) in errors_of(before.get(file).map_or(&[], Vec::as_slice)) {
            let n = old.entry(key).or_insert(0);
            *n = n.saturating_add(1);
        }
        for (key, shown) in errors_of(list) {
            if let Some(n) = old.get_mut(&key).filter(|n| **n > 0) {
                *n = n.saturating_sub(1);
            } else if let Some(n) = excused.get_mut(key.as_str()).filter(|n| **n > 0) {
                *n = n.saturating_sub(1);
            } else {
                found.push(format!("{file}: {shown}"));
            }
        }
    }
    if found.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the applied edit adds errors (D-1676):\n  {}",
        found.join("\n  ")
    ))
}

/// The grammar errors that `after` has and `before` does not, by code as
/// multisets, except one `allowed` names, each shown as
/// [`syntax_errors`] shows it.
#[must_use]
pub fn new_syntax_errors(before: &str, after: &str, allowed: &[String]) -> Vec<String> {
    let codes = |text: &str| -> Vec<(String, String)> {
        let parse = cheby_syntax::parse(cheby_diag::FileId::new(0), text.into());
        let shown = syntax_errors(text);
        parse
            .errors()
            .iter()
            .map(|d| d.code.to_string())
            .zip(shown)
            .collect()
    };
    let mut old: BTreeMap<String, usize> = BTreeMap::new();
    for (code, _) in codes(before) {
        let n = old.entry(code).or_insert(0);
        *n = n.saturating_add(1);
    }
    for code in allowed {
        let n = old.entry(code.clone()).or_insert(0);
        *n = n.saturating_add(1);
    }
    codes(after)
        .into_iter()
        .filter_map(
            |(code, shown)| match old.get_mut(&code).filter(|n| **n > 0) {
                Some(n) => {
                    *n = n.saturating_sub(1);
                    None
                }
                None => Some(shown),
            },
        )
        .collect()
}

/// The last published list of each file below the client's root, by path.
#[must_use]
pub fn published_by_path(client: &Client) -> BTreeMap<String, Vec<Value>> {
    client
        .published()
        .iter()
        .filter_map(|(uri, params)| {
            let path = client.paths().relative(uri)?;
            Some((path, client::diagnostics_of(params)))
        })
        .collect()
}

/// Whether `message` is a `cheby/idle` of the session at `root`, a path
/// below the client's root.
fn is_idle_of(message: &Value, paths: &Paths, root: &str) -> bool {
    message.get("method").and_then(Value::as_str) == Some("cheby/idle")
        && client::params(message)
            .get("root")
            .and_then(Value::as_str)
            .and_then(|uri| paths.relative(uri))
            .is_some_and(|relative| relative == root)
}

/// Whether the server has caught up with the client's inputs, for the
/// applied-edit check (D-1676): the server's work is waited for after the
/// last change, since the publish of the edited version is not enough. A
/// new error in a file the edit did not touch comes in a publish with no
/// new version, and a phase-A publish can come before the full run's
/// (D-1347, D-1442, D-1443).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settle {
    /// The session whose `cheby/idle` is waited for, below the client's
    /// root: empty for a package, the file's name in single-file mode
    /// (D-1492).
    pub root: String,
    /// Whether the client takes `cheby/idle` (D-1415). One that does not, the
    /// `empty` client, asks `cheby/status` until no package runs, debounces or
    /// holds a parked request.
    pub idle: bool,
    /// The index of [`Client::received`] after the server's answer to the
    /// client's last input: a `cheby/idle` there or later was sent after it.
    pub since: usize,
}

impl Settle {
    /// A session at `root` whose last input was answered by the messages
    /// `client` has read so far.
    #[must_use]
    pub fn new(root: &str, idle: bool, client: &Client) -> Self {
        Self {
            root: root.to_owned(),
            idle,
            since: client.received().len(),
        }
    }

    /// Records that the server's answers to the last inputs, the publishes
    /// of `reactions` by URI and version, have been read.
    pub fn mark(&mut self, client: &Client, reactions: &[(String, Option<i32>)]) {
        let last = client.received().iter().rposition(|m| {
            reactions.iter().any(|(uri, version)| {
                client::is_publish_of(m, uri)
                    && client::params(m).get("version").and_then(Value::as_i64)
                        == version.map(i64::from)
            })
        });
        if let Some(at) = last {
            self.since = self.since.max(at.saturating_add(1));
        }
    }

    /// Waits for each publish of `reactions`, then [`Settle::mark`]s them.
    ///
    /// # Errors
    ///
    /// As [`Client::diagnostics`].
    pub fn after(
        &mut self,
        client: &mut Client,
        reactions: &[(String, Option<i32>)],
    ) -> Result<(), String> {
        for (uri, version) in reactions {
            client.diagnostics(uri, *version)?;
        }
        self.mark(client, reactions);
        Ok(())
    }

    /// Records an input that no publish answers, such as a settings change:
    /// what the client has read so far came before it.
    pub fn sent(&mut self, client: &Client) {
        self.since = client.received().len();
    }

    /// Waits until the server has caught up with every input: for a
    /// `cheby/idle` of the session at or after [`Settle::since`], which takes no
    /// notification, so other waits still see it; or, for a client without
    /// `cheby/idle`, until `cheby/status` shows no package running, debouncing
    /// or holding a parked request.
    ///
    /// # Errors
    ///
    /// Returns a message for a broken stream or a server that does not
    /// catch up in time.
    pub fn wait(&mut self, client: &mut Client) -> Result<(), String> {
        if self.idle {
            let paths = client.paths().clone();
            let root = self.root.clone();
            let at =
                client.wait_received(self.since, "`cheby/idle` after the edit (D-1676)", |m| {
                    is_idle_of(m, &paths, &root)
                })?;
            self.since = at;
            return Ok(());
        }
        let deadline = Instant::now()
            .checked_add(client.timeout())
            .unwrap_or_else(Instant::now);
        loop {
            let status = client.request("cheby/status", Value::Null)?;
            let packages = status
                .get("packages")
                .and_then(Value::as_array)
                .map_or_default(Vec::as_slice);
            let quiet = packages.iter().all(|p| {
                p.get("run").and_then(Value::as_str) == Some("idle")
                    && p.get("parked").and_then(Value::as_u64) == Some(0)
            });
            if quiet {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(client.fail(&format!(
                    "the server did not catch up in {:?}: {status}",
                    client.timeout()
                )));
            }
            std::thread::sleep(STATUS_POLL);
        }
    }
}

/// What an applied edit changed, which [`revert`] undoes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Each document the edit changed: its path below the client's root
    /// before any move, its URI there, and its text before the edit.
    pub changed: Vec<(String, String, String)>,
    /// The paths among them that were not open, which the edit opened as
    /// Zed opens a buffer to edit it.
    pub opened: BTreeSet<String>,
    /// Each moved file's path before and after the move.
    pub moved: BTreeMap<String, String>,
    /// Every file's last published list before the edit, by path below the
    /// client's root, which [`gone_at_span`] compares with the list after.
    pub published_before: BTreeMap<String, Vec<Value>>,
    /// The publishes that answer the edit's changes, by URI and version.
    reactions: Vec<(String, Option<i32>)>,
}

/// Applies `edit`, a `WorkspaceEdit`, to the open documents `texts`, by path
/// below the client's root, with one `didChange` per file, as a client
/// applies one. A file that is not open is opened first with its text on
/// disk, without a BOM and with `\n` line ends, as Zed opens a buffer to
/// edit it. A `documentChanges` entry names the version of an open document
/// and `null` for a closed one (plan §1.2, D-1445).
///
/// # Errors
///
/// Returns a message for a malformed edit, an edit outside the root, a
/// version that does not match, or a broken stream.
pub fn apply_edit(
    client: &mut Client,
    texts: &mut BTreeMap<String, String>,
    edit: &Value,
) -> Result<Applied, String> {
    apply_edit_opened(client, texts, edit, &BTreeSet::new())
}

/// [`apply_edit`], where the paths of `opened` were not open when the
/// edit was made and have been opened since with their text on disk, as
/// [`open_closed`] opens them: their entries must name `null`, and
/// [`Applied::opened`] holds them, so [`revert`] closes them.
fn apply_edit_opened(
    client: &mut Client,
    texts: &mut BTreeMap<String, String>,
    edit: &Value,
    opened: &BTreeSet<String>,
) -> Result<Applied, String> {
    let encoding = client.encoding();
    let mut applied = Applied::default();
    for file in file_edits(edit)? {
        if file.edits.is_empty() {
            continue;
        }
        let path = client
            .paths()
            .relative(&file.uri)
            .ok_or_else(|| format!("an edit of {}, outside the package", file.uri))?;
        let open = texts.contains_key(&path) && !opened.contains(&path);
        if opened.contains(&path) {
            applied.opened.insert(path.clone());
        }
        if file.versioned {
            let current = client.version(&file.uri).map(i64::from);
            match (open, file.version) {
                (true, Some(version)) if Some(version) == current => {}
                (false, None) => {}
                (true, version) => {
                    return Err(format!(
                        "the edit of {path} is for version {version:?}, and the open document \
                         is at {current:?} (D-1445)"
                    ));
                }
                (false, Some(version)) => {
                    return Err(format!(
                        "the edit of {path}, which is not open, names version {version}, not \
                         null (D-1445)"
                    ));
                }
            }
        }
        // Older publishes of the file do not answer this change.
        drop(client.take_where(|m| client::is_publish_of(m, &file.uri)));
        let before = if let Some(text) = texts.get(&path) {
            text.clone()
        } else {
            let text = disk_text(client, &path)?;
            client.open_text(&file.uri, &text)?;
            applied.opened.insert(path.clone());
            text
        };
        let after = text::apply_text_edits(&before, &file.edits, encoding, Bom::Counted)
            .map_err(|e| format!("{path}: {e}"))?;
        let version = client.change(&file.uri, &after)?;
        texts.insert(path.clone(), after);
        applied.reactions.push((file.uri.clone(), Some(version)));
        applied.changed.push((path, file.uri, before));
    }
    Ok(applied)
}

/// The text on disk of `path`, below the client's root, as Zed opens a
/// buffer of it: without a BOM and with `\n` line ends.
fn disk_text(client: &Client, path: &str) -> Result<String, String> {
    let disk_path = client.paths().root().join(path);
    let disk =
        std::fs::read_to_string(&disk_path).map_err(|e| format!("{}: {e}", disk_path.display()))?;
    Ok(disk
        .strip_prefix('\u{feff}')
        .unwrap_or(&disk)
        .replace("\r\n", "\n"))
}

/// Opens each file that `edit` edits and that is not open, with
/// [`disk_text`], so the applied-edit check can ask its form before the
/// edit too (D-1712), and waits for their publishes. Returns their paths.
///
/// # Errors
///
/// Returns a message for a malformed edit, an edit outside the root, a
/// file that cannot be read, or a broken stream.
fn open_closed(
    client: &mut Client,
    texts: &mut BTreeMap<String, String>,
    settle: &mut Settle,
    edit: &Value,
) -> Result<BTreeSet<String>, String> {
    let mut opened = BTreeSet::new();
    let mut reactions = Vec::new();
    for file in file_edits(edit)? {
        if file.edits.is_empty() {
            continue;
        }
        let path = client
            .paths()
            .relative(&file.uri)
            .ok_or_else(|| format!("an edit of {}, outside the package", file.uri))?;
        if texts.contains_key(&path) {
            continue;
        }
        let text = disk_text(client, &path)?;
        drop(client.take_where(|m| client::is_publish_of(m, &file.uri)));
        client.open_text(&file.uri, &text)?;
        reactions.push((file.uri.clone(), client.version(&file.uri)));
        texts.insert(path.clone(), text);
        opened.insert(path);
    }
    if !opened.is_empty() {
        settle.after(client, &reactions)?;
        settle.wait(client)?;
    }
    Ok(opened)
}

/// Moves each `(old, new)` of `moves`, paths below the client's root of a
/// file or a folder, on disk, as Zed moves them after
/// `workspace/willRenameFiles`: each open document below `old` is closed
/// and opened again at its new path with its text, from version 0, as Zed
/// opens a buffer again, and each moved module file gets a watched change,
/// deleted at its old path and created at its new one. `texts` follows,
/// and `reactions` gets the publish of each document opened again in place
/// of those of its old path. Returns each moved module file's old and new
/// path.
///
/// # Errors
///
/// Returns a message if a file cannot be moved or the stream is broken.
pub fn move_files(
    client: &mut Client,
    texts: &mut BTreeMap<String, String>,
    moves: &[(String, String)],
    reactions: &mut Vec<(String, Option<i32>)>,
) -> Result<BTreeMap<String, String>, String> {
    let root = client.paths().root().to_path_buf();
    let mut targets = BTreeMap::new();
    let mut watched = Vec::new();
    for (old, new) in moves {
        let (from, to) = (root.join(old), root.join(new));
        let files: Vec<String> = if from.is_dir() {
            crate::files_with_extension(&from, "cheby")
                .map_err(|e| format!("{}: {e}", from.display()))?
                .iter()
                .map(|path| crate::display_relative(path, &root))
                .collect()
        } else {
            vec![old.clone()]
        };
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::rename(&from, &to)
            .map_err(|e| format!("cannot move {} to {}: {e}", from.display(), to.display()))?;
        for file in files {
            let rest = file.strip_prefix(old.as_str()).unwrap_or_default();
            let target = format!("{new}{rest}");
            let (old_uri, new_uri) = (
                client.uri(std::path::Path::new(&file))?,
                client.uri(std::path::Path::new(&target))?,
            );
            if let Some(text) = texts.remove(&file) {
                client.close(&old_uri)?;
                drop(client.take_where(|m| client::is_publish_of(m, &new_uri)));
                client.open_version(&new_uri, &text, 0)?;
                texts.insert(target.clone(), text);
                reactions.retain(|(uri, _)| uri != &old_uri);
                reactions.push((new_uri.clone(), Some(0)));
            }
            watched.push(json!({ "uri": old_uri, "type": 3 }));
            watched.push(json!({ "uri": new_uri, "type": 1 }));
            targets.insert(file, target);
        }
    }
    if !watched.is_empty() {
        client.notify(
            "workspace/didChangeWatchedFiles",
            json!({ "changes": watched }),
        )?;
    }
    Ok(targets)
}

/// The rules of one applied-edit check beyond D-1676's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditRules<'a> {
    /// The codes of the errors the edit may add, a header's `@newErrors`.
    pub new_errors: &'a [String],
    /// Which touched lines must be a fixed point.
    pub fixed_point: FixedPoint,
}

/// `rangeFormatting` on whole `lines` of the open `uri`, whose text is
/// `text`: its answer.
fn range_formatting(
    client: &mut Client,
    uri: &str,
    text: &str,
    lines: (u32, u32),
) -> Result<Value, String> {
    let range = line_range(text, lines, client.encoding());
    client.request(
        "textDocument/rangeFormatting",
        json!({
            "textDocument": { "uri": uri },
            "range": range,
            "options": super::case::formatting_options(),
        }),
    )
}

/// Whether a `rangeFormatting` answer has an edit. `null`, which the server
/// gives for a text `cheby fmt` refuses, has none.
fn has_edits(answer: &Value) -> bool {
    answer.as_array().is_some_and(|edits| !edits.is_empty())
}

/// One edited file of [`check_applied`], read before the edit.
struct EditedFile {
    /// Its path below the client's root.
    path: String,
    /// Whether its edits swap names only ([`names_only`]).
    names: bool,
    /// Its groups of touched lines, each with whether it was a fixed point.
    groups: Vec<(Touched, bool)>,
}

/// The applied-edit check of plan §1.7 (D-1676), for a rename marker's
/// answer or a `willRename` step's:
///
/// 1. The server is waited for ([`Settle::wait`]), so the published lists are
///    those of the text the edit was made for.
/// 2. `edit` is applied with `didChange` ([`apply_edit`]), then `moves` are
///    made ([`move_files`]), and the server is waited for again.
/// 3. No file's last published list may show an error it did not show
///    before, a moved file compared at its new path, as [`new_errors`]
///    says, and no edited file may parse with a grammar error it did not
///    have ([`new_syntax_errors`]).
/// 4. `textDocument/rangeFormatting` on each group of touched lines must
///    answer with no edit (D-1399): every group, or with
///    [`FixedPoint::Rename`] those that were fixed points before and still
///    fit [`WIDTH`] columns (D-1670). With [`FixedPoint::Rename`] and
///    [`FixedPoint::Move`], an answer that only breaks or joins lines
///    passes when the file's edits swap names or module paths only
///    (D-1712). Each edited file that was not open is opened with its text
///    on disk first ([`open_closed`]), so its form before is known and it
///    is checked as an open one is (D-1712).
///
/// Returns what was applied, for [`revert`].
///
/// # Errors
///
/// Returns a message naming what the edit broke, or why it could not be
/// applied.
pub fn check_applied(
    client: &mut Client,
    texts: &mut BTreeMap<String, String>,
    settle: &mut Settle,
    edit: &Value,
    moves: &[(String, String)],
    rules: EditRules<'_>,
) -> Result<Applied, String> {
    settle.wait(client)?;
    let opened = open_closed(client, texts, settle, edit)?;
    let before = published_by_path(client);
    let mut groups: Vec<EditedFile> = Vec::new();
    let encoding = client.encoding();
    for file in file_edits(edit)? {
        if file.edits.is_empty() {
            continue;
        }
        let path = client
            .paths()
            .relative(&file.uri)
            .ok_or_else(|| format!("an edit of {}, outside the package", file.uri))?;
        let mut known = Vec::new();
        let text = texts
            .get(&path)
            .cloned()
            .ok_or_else(|| format!("{path} is edited but was not opened"))?;
        for group in touched(&file.edits)? {
            let fixed = match rules.fixed_point {
                FixedPoint::Strict | FixedPoint::Move => true,
                FixedPoint::Rename => {
                    let answer = range_formatting(client, &file.uri, &text, group.before)?;
                    answer.as_array().is_some_and(Vec::is_empty)
                }
            };
            known.push((group, fixed));
        }
        let names = names_only(&text, &file.edits, encoding);
        groups.push(EditedFile {
            path,
            names,
            groups: known,
        });
    }
    let mut applied = apply_edit_opened(client, texts, edit, &opened)?;
    applied.published_before.clone_from(&before);
    let mut reactions = std::mem::take(&mut applied.reactions);
    applied.moved = move_files(client, texts, moves, &mut reactions)?;
    settle.after(client, &reactions)?;
    settle.wait(client)?;
    let targets = applied.moved.clone();
    let now = |path: &String| targets.get(path).unwrap_or(path).clone();
    let before: BTreeMap<String, Vec<Value>> = before
        .into_iter()
        .map(|(path, list)| (now(&path), list))
        .collect();
    let after: BTreeMap<String, Vec<Value>> = published_by_path(client)
        .into_iter()
        .filter(|(path, _)| !targets.contains_key(path))
        .collect();
    new_errors(&before, &after, rules.new_errors)?;
    for (path, _, old) in &applied.changed {
        let path = now(path);
        let new = texts.get(&path).map_or("", String::as_str);
        let added = new_syntax_errors(old, new, rules.new_errors);
        if !added.is_empty() {
            return Err(format!(
                "the applied edit adds grammar errors to {path} (D-1676):\n  {}",
                added.join("\n  ")
            ));
        }
    }
    for EditedFile {
        path,
        names,
        groups: known,
    } in groups
    {
        let path = now(&path);
        let uri = client.uri(std::path::Path::new(&path))?;
        let text = texts.get(&path).cloned().unwrap_or_default();
        for (group, fixed) in known {
            let asked = match rules.fixed_point {
                FixedPoint::Strict | FixedPoint::Move => true,
                FixedPoint::Rename => fixed && fits(&text, group.after),
            };
            if !asked {
                continue;
            }
            let answer = range_formatting(client, &uri, &text, group.after)?;
            let reflow = matches!(rules.fixed_point, FixedPoint::Rename | FixedPoint::Move)
                && names
                && reflowed_only(&text, &answer, encoding);
            if has_edits(&answer) && !reflow {
                return Err(format!(
                    "{path}: lines {}..={} of the edited text are not in `cheby fmt`'s form \
                     (D-1676, D-1399): `rangeFormatting` answers {answer}",
                    group.after.0.saturating_add(1),
                    group.after.1.saturating_add(1)
                ));
            }
        }
    }
    Ok(applied)
}

/// Undoes `applied`, an edit with no move, so the next marker reads the
/// text it was written for: each changed document gets its text back with
/// `didChange`, and each document the edit opened is closed again. Then
/// waits for their publishes.
///
/// # Errors
///
/// Returns a message for an edit with moves, which cannot be undone, or a
/// broken stream.
pub fn revert(
    client: &mut Client,
    texts: &mut BTreeMap<String, String>,
    settle: &mut Settle,
    applied: Applied,
) -> Result<(), String> {
    if !applied.moved.is_empty() {
        return Err("an edit that moved files cannot be undone".to_owned());
    }
    let mut reactions = Vec::new();
    for (path, uri, before) in applied.changed {
        drop(client.take_where(|m| client::is_publish_of(m, &uri)));
        if applied.opened.contains(&path) {
            client.close(&uri)?;
            texts.remove(&path);
            reactions.push((uri, None));
        } else {
            let version = client.change(&uri, &before)?;
            texts.insert(path, before);
            reactions.push((uri, Some(version)));
        }
    }
    settle.after(client, &reactions)
}

/// The command of the Explain action (X §8.3, D-1693).
pub const EXPLAIN_COMMAND: &str = "cheby.explain";

/// The block of an `action TITLE` marker (plan §4.3 B), from `list`, the
/// `textDocument/codeAction` answer the marker's request got after the
/// server was waited for (D-1685):
///
/// - `actions`: the titles listed, in order.
/// - No action with `title` is an error, as a listing that is no list is:
///   a negative is a `noAction` marker ([`no_action_block`]), so a title
///   that changes cannot turn a fix's golden into a negative one (T-10).
/// - An action that runs a command is not resolved: the command is sent
///   with `workspace/executeCommand` ([`execute_block`]), and its block
///   joins this one.
/// - Any other is resolved with `codeAction/resolve`, whose answer, or
///   error, is `answer`, and `showMessage` the messages the server sent
///   before it. An answer with an `edit` is applied and checked as
///   [`check_applied`] says, held to [`FixedPoint::Strict`], or E0044's
///   rename to [`FixedPoint::Rename`] (D-1735), and the diagnostic the
///   action names must be gone ([`gone_at_span`]). Then it is undone, so
///   the next marker reads the text it was written for. An answer with no
///   `edit`, such as a stale refusal (D-1731), applies nothing.
///
/// # Errors
///
/// Returns a message naming what the applied fix broke, or why it could
/// not be applied.
pub fn action_block(
    client: &mut Client,
    texts: &mut BTreeMap<String, String>,
    settle: &mut Settle,
    new_errors: &[String],
    list: &Value,
    title: &str,
) -> Result<Value, String> {
    let Some(items) = list.as_array() else {
        // An error, or `null` where the request found no snapshot.
        return Err(format!(
            "`action {title}`: the listing is {list}, not a list (a negative is `noAction`)"
        ));
    };
    let titles: Vec<Value> = items
        .iter()
        .map(|a| a.get("title").cloned().unwrap_or(Value::Null))
        .collect();
    let Some(action) = items
        .iter()
        .find(|a| a.get("title").and_then(Value::as_str) == Some(title))
        .cloned()
    else {
        // A renamed or mistyped title must not turn a fix's golden into a
        // negative one under bless (T-10).
        return Err(format!(
            "`action {title}`: no action has that title; listed: {} (a negative is `noAction`)",
            Value::Array(titles)
        ));
    };
    drop(client.take_where(is_show_message));
    drop(client.take_shown_documents());
    if let Some(command) = action.get("command") {
        let name = command
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("a command with no name: {command}"))?;
        let arguments = command
            .get("arguments")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut block = execute_block(client, name, &arguments)?;
        if let Some(map) = block.as_object_mut() {
            map.insert("actions".to_owned(), Value::Array(titles));
        }
        return Ok(block);
    }
    let answer = client.call("codeAction/resolve", action)?;
    let messages = shown_messages(&client.take_where(is_show_message));
    let resolved = match answer {
        Ok(resolved) => resolved,
        Err(e) => {
            let error = json!({ "error": { "code": e.code, "message": e.message } });
            return Ok(json!({ "actions": titles, "answer": error, "showMessage": messages }));
        }
    };
    if let Some(edit) = resolved.get("edit").filter(|edit| !edit.is_null()) {
        let renames = resolved
            .pointer("/diagnostics/0/code")
            .and_then(Value::as_str)
            == Some("E0044");
        let rules = EditRules {
            new_errors,
            fixed_point: if renames {
                FixedPoint::Rename
            } else {
                FixedPoint::Strict
            },
        };
        let applied = check_applied(client, texts, settle, edit, &[], rules)?;
        if let (Some(fixed), Some(uri)) = (
            resolved.pointer("/diagnostics/0"),
            resolved.pointer("/data/uri").and_then(Value::as_str),
        ) {
            gone_at_span(client, texts, &applied, edit, uri, fixed)?;
        }
        revert(client, texts, settle, applied)?;
    }
    let answer = client.normalize(&super::case::without_hash(resolved));
    Ok(json!({ "actions": titles, "answer": answer, "showMessage": messages }))
}

/// The block of a `noAction TITLE` marker, a negative: `actions`, the titles
/// `list` holds, or `list` itself when it is no list, such as `null` where
/// the request found no snapshot.
///
/// # Errors
///
/// Returns a message when an action has the title.
pub fn no_action_block(list: &Value, title: &str) -> Result<Value, String> {
    let Some(items) = list.as_array() else {
        return Ok(json!({ "actions": list }));
    };
    let titles: Vec<Value> = items
        .iter()
        .map(|a| a.get("title").cloned().unwrap_or(Value::Null))
        .collect();
    if titles.iter().any(|t| t.as_str() == Some(title)) {
        return Err(format!(
            "`noAction {title}`: an action has that title; listed: {}",
            Value::Array(titles)
        ));
    }
    Ok(json!({ "actions": titles }))
}

/// "The fixed code is gone at its span" (F §9.4): after `edit` was applied
/// ([`check_applied`]), the last published list of the file at `uri` holds
/// fewer diagnostics of `fixed`'s code, of any severity, than the list
/// before the edit, or none of them starts where `fixed` started, its start
/// moved by the edits before it, or anywhere in what the edits wrote
/// ([`fixed_is_gone`]). A position alone cannot see a wrap that leaves its
/// diagnostic: an insertion at the diagnostic's start moves the looked-up
/// start past what it wrote, where the survivor does not start.
///
/// # Errors
///
/// Returns a message naming the diagnostic that is still there.
pub fn gone_at_span(
    client: &Client,
    texts: &BTreeMap<String, String>,
    applied: &Applied,
    edit: &Value,
    uri: &str,
    fixed: &Value,
) -> Result<(), String> {
    let path = client
        .paths()
        .relative(uri)
        .ok_or_else(|| format!("a fix of {uri}, outside the package"))?;
    let code = fixed.get("code").cloned().unwrap_or(Value::Null);
    let start = Position::from_json(fixed.pointer("/range/start").unwrap_or(&Value::Null))?;
    let encoding = client.encoding();
    let after = texts.get(&path).map_or("", String::as_str);
    let (start, written) = match applied.changed.iter().find(|(p, _, _)| *p == path) {
        Some((_, _, before)) => {
            let edits = file_edits(edit)?
                .into_iter()
                .find(|file| file.uri == uri)
                .map_or_default(|file| file.edits);
            let offset = Lines::new(before, Bom::Counted).offset(start, encoding);
            (
                moved_offset(before, &edits, offset, encoding)?,
                written_ranges(before, &edits, encoding)?,
            )
        }
        None => (
            Lines::new(after, Bom::Counted).offset(start, encoding),
            Vec::new(),
        ),
    };
    let published = published_by_path(client);
    let list = |lists: &BTreeMap<String, Vec<Value>>| lists.get(&path).cloned().unwrap_or_default();
    fixed_is_gone(
        &code,
        &list(&applied.published_before),
        &list(&published),
        after,
        start,
        &written,
        encoding,
    )
    .map_err(|e| format!("{path}: {e}"))
}

/// The rule of [`gone_at_span`] on the lists `before` and `after` an edit
/// of a file whose text is now `after_text`: `Ok` when `after` holds fewer
/// diagnostics of `code` than `before`, or none that starts at `start` or
/// in one of `written`, byte ranges of `after_text`, ends included.
///
/// # Errors
///
/// Returns a message naming the diagnostic that is still there.
pub fn fixed_is_gone(
    code: &Value,
    before: &[Value],
    after: &[Value],
    after_text: &str,
    start: usize,
    written: &[(usize, usize)],
    encoding: Encoding,
) -> Result<(), String> {
    let of_code = |list: &[Value]| list.iter().filter(|d| d.get("code") == Some(code)).count();
    if of_code(after) < of_code(before) {
        return Ok(());
    }
    let lines = Lines::new(after_text, Bom::Counted);
    let still = after
        .iter()
        .filter(|d| d.get("code") == Some(code))
        .find(|d| {
            d.pointer("/range/start")
                .and_then(|p| Position::from_json(p).ok())
                .map(|p| lines.offset(p, encoding))
                .is_some_and(|at| at == start || written.iter().any(|&(s, e)| s <= at && at <= e))
        });
    match still {
        None => Ok(()),
        Some(d) => {
            let at = lines.position(start, encoding);
            Err(format!(
                "the fix leaves its diagnostic {code} at {}:{} (F §9.4): {d}",
                at.line.saturating_add(1),
                at.character.saturating_add(1)
            ))
        }
    }
}

/// The byte ranges of the text after `edits`, the protocol's text edits
/// of `text`, that the edits wrote, ascending.
///
/// # Errors
///
/// Returns a message for a malformed edit.
pub fn written_ranges(
    text: &str,
    edits: &[Value],
    encoding: Encoding,
) -> Result<Vec<(usize, usize)>, String> {
    let lines = Lines::new(text, Bom::Counted);
    let mut spans = Vec::with_capacity(edits.len());
    for edit in edits {
        let range = edit
            .get("range")
            .ok_or_else(|| format!("an edit without a range: {edit}"))?;
        let written = edit
            .get("newText")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("an edit without newText: {edit}"))?
            .len();
        spans.push((text::offsets(&lines, range, encoding)?, written));
    }
    spans.sort_by_key(|&((start, end), _)| (start, end));
    let mut out = Vec::with_capacity(spans.len());
    // What the edits so far added, less what they removed.
    let (mut added, mut removed) = (0_usize, 0_usize);
    for ((start, end), written) in spans {
        let at = start.saturating_add(added).saturating_sub(removed);
        out.push((at, at.saturating_add(written)));
        added = added.saturating_add(written);
        removed = removed.saturating_add(end.saturating_sub(start));
    }
    Ok(out)
}

/// Where `offset` of `text` lies after `edits`, the protocol's text edits
/// of that text: moved by the edits wholly before it, and at the start of
/// what an edit around it wrote.
///
/// # Errors
///
/// Returns a message for a malformed edit.
pub fn moved_offset(
    text: &str,
    edits: &[Value],
    offset: usize,
    encoding: Encoding,
) -> Result<usize, String> {
    let lines = Lines::new(text, Bom::Counted);
    let mut spans = Vec::with_capacity(edits.len());
    for edit in edits {
        let range = edit
            .get("range")
            .ok_or_else(|| format!("an edit without a range: {edit}"))?;
        let written = edit
            .get("newText")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("an edit without newText: {edit}"))?
            .len();
        spans.push((text::offsets(&lines, range, encoding)?, written));
    }
    spans.sort_by_key(|&((start, end), _)| (start, end));
    let mut moved = offset;
    for ((start, end), written) in spans {
        if end <= offset && start < offset {
            moved = moved
                .saturating_sub(end.saturating_sub(start))
                .saturating_add(written);
        } else if start < offset {
            // Inside what the edit replaced: where it wrote.
            let shift = moved.saturating_sub(offset);
            return Ok(start.saturating_add(shift));
        } else if start == offset && end == offset {
            // An insertion at the offset itself goes before it.
            moved = moved.saturating_add(written);
        }
    }
    Ok(moved)
}

/// The block of a `workspace/executeCommand` of `command` with
/// `arguments`, an `execute` step's or a command action's (D-1693): the
/// `answer`, each `window/showDocument`'s params with its path as goldens
/// show it, and the `window/showMessage` lines the server sent before it.
/// A document `cheby.explain` opened must hold the explanation that
/// `cheby explain` prints for the code it was given.
///
/// # Errors
///
/// Returns a message for an opened explanation that differs, or that
/// cannot be read.
pub fn execute_block(
    client: &mut Client,
    command: &str,
    arguments: &[Value],
) -> Result<Value, String> {
    drop(client.take_where(is_show_message));
    drop(client.take_shown_documents());
    let params = json!({ "command": command, "arguments": arguments });
    let answer = match client.call("workspace/executeCommand", params)? {
        Ok(value) => value,
        Err(e) => json!({ "error": { "code": e.code, "message": e.message } }),
    };
    let documents = client.take_shown_documents();
    let messages = shown_messages(&client.take_where(is_show_message));
    if command == EXPLAIN_COMMAND {
        let code = arguments
            .first()
            .and_then(Value::as_str)
            .unwrap_or_default();
        for document in &documents {
            let uri = document
                .get("uri")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("a `window/showDocument` with no URI: {document}"))?;
            let path = super::uri::uri_to_path(uri)
                .ok_or_else(|| format!("`window/showDocument` of {uri}, which is no file"))?;
            let written = std::fs::read_to_string(&path)
                .map_err(|e| format!("the explanation at {}: {e}", path.display()))?;
            same_explanation(code, &written)?;
        }
    }
    Ok(json!({
        "answer": client.normalize(&answer),
        "showDocument": client.normalize(&Value::Array(documents)),
        "showMessage": messages,
    }))
}

/// The block of an `explain CODE` step: `cheby/explain`'s answer's `code`
/// and `summary`, its `markdown` compared with the explanation `cheby
/// explain` prints, so no golden holds a `docs/diagnostics` text (X §8.3).
/// Any other answer, an error or `null`, is the block as it is.
///
/// # Errors
///
/// Returns a message for a `markdown` that differs.
pub fn explain_block(client: &mut Client, code: &str) -> Result<Value, String> {
    let answer = match client.call("cheby/explain", json!({ "code": code }))? {
        Ok(value) => value,
        Err(e) => return Ok(json!({ "error": { "code": e.code, "message": e.message } })),
    };
    let Some(markdown) = answer.get("markdown").and_then(Value::as_str) else {
        return Ok(answer);
    };
    same_explanation(code, markdown)?;
    Ok(json!({
        "code": answer.get("code").cloned().unwrap_or(Value::Null),
        "summary": answer.get("summary").cloned().unwrap_or(Value::Null),
    }))
}

/// Whether `text` is the explanation `cheby explain CODE` prints.
///
/// # Errors
///
/// Returns a message for an unknown code or a text that differs.
pub fn same_explanation(code: &str, text: &str) -> Result<(), String> {
    let expected = cheby_diag::Code::parse(code)
        .and_then(cheby_diag::codes::explanation)
        .ok_or_else(|| format!("`{code}` has no explanation"))?;
    if text == expected {
        Ok(())
    } else {
        Err(format!(
            "the explanation of {code} differs from `cheby explain {code}`'s:\n{text}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn range(a: (u32, u32), b: (u32, u32)) -> Value {
        json!({ "start": at(a.0, a.1).to_json(), "end": at(b.0, b.1).to_json() })
    }

    fn diagnostic(code: &str, line: u32, severity: u64) -> Value {
        json!({
            "range": range((line, 0), (line, 1)),
            "severity": severity,
            "code": code,
            "message": format!("{code} here\nmore"),
        })
    }

    /// An offset moves by the edits before it, stays before an edit after
    /// it, and lands where an edit around it wrote (F §9.4).
    #[test]
    fn offsets_move_through_edits() {
        let text = "ab\ncd\nef\n";
        let edit = |l: u32, c: u32, e: u32, new: &str| json!({ "range": range((l, c), (l, e)), "newText": new });
        let edits = [edit(0, 0, 2, "xyz"), edit(2, 0, 0, "q\n")];
        let enc = Encoding::Utf16;
        // `c`, after the first edit: one byte later.
        assert_eq!(moved_offset(text, &edits, 3, enc), Ok(4));
        // `b`, inside the first edit: where it wrote.
        assert_eq!(moved_offset(text, &edits, 1, enc), Ok(0));
        // `e`, at the insertion: after what it wrote.
        assert_eq!(moved_offset(text, &edits, 6, enc), Ok(9));
        assert_eq!(moved_offset(text, &[], 6, enc), Ok(6));
    }

    /// A fix's code is gone when its count drops, or when none of it starts
    /// at the fixed start or in what the edits wrote: a wrap that writes
    /// `Err(` before the diagnostic's start and leaves it is caught, though
    /// the start moved past the insertion (T-2).
    #[test]
    fn a_fix_that_leaves_its_code_is_caught() {
        let before_text = "fn f() -> Option<Int> {\n  n\n}\n";
        let edits = [
            json!({ "range": range((1, 2), (1, 2)), "newText": "Err(" }),
            json!({ "range": range((1, 3), (1, 3)), "newText": ")" }),
        ];
        let enc = Encoding::Utf16;
        let after_text = "fn f() -> Option<Int> {\n  Err(n)\n}\n";
        let written = written_ranges(before_text, &edits, enc).unwrap();
        assert_eq!(written, [(26, 30), (31, 32)]);
        let start = moved_offset(before_text, &edits, 26, enc).unwrap();
        let e0047 = |line: u32, character: u32| json!({ "code": "E0047", "range": range((line, character), (line, character.saturating_add(1))) });
        let code = json!("E0047");
        let before = [e0047(1, 2)];
        // The survivor starts where the wrap wrote.
        let left = fixed_is_gone(
            &code,
            &before,
            &[e0047(1, 2)],
            after_text,
            start,
            &written,
            enc,
        );
        assert!(left.is_err_and(|e| e.contains("leaves its diagnostic")));
        // Gone, or only elsewhere, passes; so does a count that dropped.
        assert_eq!(
            fixed_is_gone(&code, &before, &[], after_text, start, &written, enc),
            Ok(())
        );
        let elsewhere = [e0047(2, 0)];
        assert_eq!(
            fixed_is_gone(&code, &before, &elsewhere, after_text, start, &written, enc),
            Ok(())
        );
        let two = [e0047(1, 2), e0047(2, 0)];
        assert_eq!(
            fixed_is_gone(
                &code,
                &two,
                &[e0047(1, 2)],
                after_text,
                start,
                &written,
                enc
            ),
            Ok(())
        );
    }

    /// An explanation must be the text `cheby explain` prints, and an
    /// unknown code has none.
    #[test]
    fn explanations_are_compared_with_cheby_explain() {
        let code = cheby_diag::codes::SHADOWED_NAME;
        let text = cheby_diag::codes::explanation(code).unwrap();
        assert_eq!(same_explanation("E0044", text), Ok(()));
        assert!(same_explanation("E0044", "something else").is_err());
        assert!(same_explanation("E9999", text).is_err());
        assert!(same_explanation("nonsense", text).is_err());
    }

    #[test]
    fn show_message_lines() {
        let notes = [
            json!({ "jsonrpc": "2.0", "method": SHOW_MESSAGE, "params": { "type": 2, "message": "`Point` will no longer satisfy `Show`" } }),
            json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": { "uri": "file:///a", "diagnostics": [] } }),
            json!({ "jsonrpc": "2.0", "method": SHOW_MESSAGE, "params": { "type": 1, "message": "no" } }),
            json!({ "jsonrpc": "2.0", "method": "window/logMessage", "params": { "type": 2, "message": "log" } }),
        ];
        assert_eq!(
            shown_messages(&notes),
            json!([
                "warning: `Point` will no longer satisfy `Show`",
                "error: no"
            ])
        );
        assert_eq!(shown_messages(&[]), json!([]));
        assert!(is_show_message(&notes[0]));
        assert!(!is_show_message(&notes[3]));
    }

    #[test]
    fn edits_by_file() {
        let edit = |line: u32| json!({ "range": range((line, 0), (line, 1)), "newText": "b" });
        let documents = json!({ "documentChanges": [
            { "textDocument": { "uri": "file:///a", "version": 3 }, "edits": [edit(0)] },
            { "textDocument": { "uri": "file:///b", "version": null }, "edits": [edit(1), edit(2)] },
        ] });
        let files = file_edits(&documents).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!((files[0].version, files[0].versioned), (Some(3), true));
        assert_eq!((files[1].version, files[1].edits.len()), (None, 2));
        let changes = json!({ "changes": { "file:///a": [edit(0)] } });
        let files = file_edits(&changes).unwrap();
        assert_eq!((files[0].version, files[0].versioned), (None, false));
        assert_eq!(file_edits(&Value::Null).unwrap(), Vec::new());
        // A resource operation, and a file named twice.
        let create = json!({ "documentChanges": [{ "kind": "create", "uri": "file:///c" }] });
        assert!(
            file_edits(&create)
                .unwrap_err()
                .contains("resource operation")
        );
        let twice = json!({ "documentChanges": [
            { "textDocument": { "uri": "file:///a", "version": 1 }, "edits": [edit(0)] },
            { "textDocument": { "uri": "file:///a", "version": 1 }, "edits": [edit(1)] },
        ] });
        assert!(file_edits(&twice).unwrap_err().contains("two entries"));
    }

    #[test]
    fn touched_lines_before_and_after() {
        let edit = |a: (u32, u32), b: (u32, u32), text: &str| json!({ "range": range(a, b), "newText": text });
        // Two edits on line 1 are one group; a line inserted at line 3
        // moves line 6 down by one; line 8 and 9 joined move line 10 up.
        let edits = [
            edit((6, 2), (6, 4), "xy"),
            edit((1, 0), (1, 3), "abc"),
            edit((3, 0), (3, 0), "inserted\n"),
            edit((1, 5), (1, 6), "d"),
            edit((8, 4), (9, 0), ""),
            edit((10, 0), (10, 1), "z"),
        ];
        let groups = touched(&edits).unwrap();
        let shown: Vec<((u32, u32), (u32, u32))> =
            groups.iter().map(|g| (g.before, g.after)).collect();
        assert_eq!(
            shown,
            [
                ((1, 1), (1, 1)),
                ((3, 3), (3, 4)),
                ((6, 6), (7, 7)),
                ((8, 9), (9, 9)),
                ((10, 10), (10, 10)),
            ]
        );
        let backwards = [edit((2, 0), (1, 0), "")];
        assert!(touched(&backwards).is_err());
    }

    #[test]
    fn a_name_swap_may_move_line_breaks() {
        let edit = |a: (u32, u32), b: (u32, u32), text: &str| json!({ "range": range(a, b), "newText": text });
        let before = "  f(long_name, b)\n  g(x)\n";
        assert!(names_only(
            before,
            &[edit((0, 4), (0, 13), "c")],
            Encoding::Utf16
        ));
        // Part of a name, a name with a space, and an insertion are no swap.
        assert!(!names_only(
            before,
            &[edit((0, 4), (0, 8), "c")],
            Encoding::Utf16
        ));
        assert!(!names_only(
            before,
            &[edit((0, 4), (0, 13), "c ")],
            Encoding::Utf16
        ));
        assert!(!names_only(
            before,
            &[edit((0, 15), (0, 15), "  ")],
            Encoding::Utf16
        ));
        // A move's module path is a swap too (D-1712), but not one that
        // adds an alias or ends in `::`.
        let import = "import app::util\n";
        let path = |text: &str| [edit((0, 7), (0, 16), text)];
        assert!(names_only(import, &path("app::lib::util"), Encoding::Utf16));
        assert!(!names_only(
            import,
            &path("app::tools as util"),
            Encoding::Utf16
        ));
        assert!(!names_only(import, &path("app::"), Encoding::Utf16));
        let broken = "  f(\n    c,\n    b,\n  )\n  g(x)\n";
        let join = json!([edit((0, 0), (4, 0), "  f(c, b)\n")]);
        assert!(reflowed_only(broken, &join, Encoding::Utf16));
        // Whitespace inside a line is not compared: `names_only` keeps a
        // rename's edit from writing any.
        let spaced = json!([edit((0, 0), (4, 0), "  f(c, b )\n")]);
        assert!(reflowed_only(broken, &spaced, Encoding::Utf16));
        let changed = json!([edit((0, 0), (4, 0), "  f(c, d)\n")]);
        assert!(!reflowed_only(broken, &changed, Encoding::Utf16));
        let comma = json!([edit((0, 0), (4, 0), "  f(c b)\n")]);
        assert!(!reflowed_only(broken, &comma, Encoding::Utf16));
    }

    #[test]
    fn width_and_line_ranges() {
        let long = "x".repeat(WIDTH);
        let text = format!("short\n{long}\n{long}y\n");
        assert!(fits(&text, (0, 1)));
        assert!(!fits(&text, (1, 2)));
        assert_eq!(
            line_range("ab\né😀\n", (0, 1), Encoding::Utf16),
            json!({ "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 3 } })
        );
    }

    #[test]
    fn new_errors_are_found_in_every_file() {
        let before = BTreeMap::from([
            ("src/main.cheby".to_owned(), vec![diagnostic("E0001", 1, 1)]),
            ("src/util.cheby".to_owned(), vec![]),
        ]);
        // Unchanged lists, an error that moved within its file, a warning,
        // and an error gone all pass.
        let same = BTreeMap::from([
            ("src/main.cheby".to_owned(), vec![diagnostic("E0001", 4, 1)]),
            ("src/util.cheby".to_owned(), vec![diagnostic("W0001", 0, 2)]),
        ]);
        assert_eq!(new_errors(&before, &same, &[]), Ok(()));
        let gone = BTreeMap::from([("src/main.cheby".to_owned(), vec![])]);
        assert_eq!(new_errors(&before, &gone, &[]), Ok(()));
        // A new error in a file the edit did not touch fails.
        let elsewhere = BTreeMap::from([
            ("src/main.cheby".to_owned(), vec![diagnostic("E0001", 1, 1)]),
            ("src/util.cheby".to_owned(), vec![diagnostic("E0013", 2, 1)]),
        ]);
        let e = new_errors(&before, &elsewhere, &[]).unwrap_err();
        assert!(
            e.contains("src/util.cheby: E0013 at 3:1: E0013 here"),
            "{e}"
        );
        // A second error of a code the file had fails too: multisets.
        let twice = BTreeMap::from([(
            "src/main.cheby".to_owned(),
            vec![diagnostic("E0001", 1, 1), diagnostic("E0001", 2, 1)],
        )]);
        assert!(new_errors(&before, &twice, &[]).is_err());
        // A file that had no list, and an error with no code.
        let new_file = BTreeMap::from([(
            "src/late.cheby".to_owned(),
            vec![json!({ "message": "bad" })],
        )]);
        let e = new_errors(&before, &new_file, &[]).unwrap_err();
        assert!(e.contains("src/late.cheby: no code: bad"), "{e}");
        // The header excuses one occurrence per listing.
        let allowed = ["E0013".to_owned()];
        assert_eq!(new_errors(&before, &elsewhere, &allowed), Ok(()));
        let both = BTreeMap::from([
            (
                "src/main.cheby".to_owned(),
                vec![diagnostic("E0001", 1, 1), diagnostic("E0013", 0, 1)],
            ),
            ("src/util.cheby".to_owned(), vec![diagnostic("E0013", 2, 1)]),
        ]);
        assert!(new_errors(&before, &both, &allowed).is_err());
        let twice_allowed = ["E0013".to_owned(), "E0013".to_owned()];
        assert_eq!(new_errors(&before, &both, &twice_allowed), Ok(()));
    }

    #[test]
    fn new_grammar_errors() {
        let clean = "fn main() {\n  let a = 1\n}\n";
        let broken = "fn main() {\n  let = 1\n}\n";
        assert_eq!(new_syntax_errors(clean, clean, &[]), Vec::<String>::new());
        assert_eq!(new_syntax_errors(broken, clean, &[]), Vec::<String>::new());
        let added = new_syntax_errors(clean, broken, &[]);
        assert_ne!(added, Vec::<String>::new());
        assert!(added[0].starts_with("2:"), "{added:?}");
        assert_eq!(new_syntax_errors(broken, broken, &[]), Vec::<String>::new());
        let parse = cheby_syntax::parse(cheby_diag::FileId::new(0), broken.into());
        let codes: Vec<String> = parse.errors().iter().map(|d| d.code.to_string()).collect();
        assert_eq!(
            new_syntax_errors(clean, broken, &codes),
            Vec::<String>::new()
        );
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_name(Some(3)), "Function");
        assert_eq!(kind_name(Some(25)), "TypeParameter");
        assert_eq!(kind_name(Some(0)), "-");
        assert_eq!(kind_name(Some(26)), "-");
        assert_eq!(kind_name(None), "-");
    }

    #[test]
    fn lists_merge_defaults_and_sort() {
        let answer = json!({
            "isIncomplete": true,
            "itemDefaults": { "editRange": range((0, 0), (0, 2)), "insertTextFormat": 2 },
            "items": [
                { "label": "b", "sortText": "10b" },
                { "label": "a", "sortText": "10a", "insertText": "a($0)" },
                { "label": "z", "sortText": "00z", "insertTextFormat": 1 },
            ],
        });
        let read = completions(&answer).unwrap();
        assert!(read.incomplete);
        let labels: Vec<&str> = read.items.iter().map(label).collect();
        assert_eq!(labels, ["z", "a", "b"]);
        assert_eq!(read.items[1]["textEdit"]["newText"], json!("a($0)"));
        assert_eq!(read.items[1]["insertTextFormat"], json!(2));
        assert_eq!(read.items[0]["insertTextFormat"], json!(1));
        let bare = completions(&json!([{ "label": "y" }, { "label": "x" }])).unwrap();
        assert!(!bare.incomplete);
        assert_eq!(bare.items[0]["label"], json!("x"));
        assert_eq!(completions(&Value::Null).unwrap(), Completions::default());
        assert!(completions(&json!(3)).is_err());
        let split = json!({
            "items": [{ "label": "m" }],
            "itemDefaults": { "editRange": { "insert": range((0, 0), (0, 1)), "replace": range((0, 0), (0, 2)) } },
        });
        let read = completions(&split).unwrap();
        assert!(read.items[0]["textEdit"].get("insert").is_some());
    }

    #[test]
    fn summaries() {
        let item = json!({
            "label": "map",
            "labelDetails": { "detail": "(items, f)", "description": "std::list" },
            "kind": 3,
            "detail": "fn map<T, U>(List<T>, fn(T) -> U) -> List<U>",
            "textEdit": { "range": range((1, 4), (1, 7)), "newText": " |> list::map(_, ${1:f})$0" },
            "additionalTextEdits": [{ "range": range((0, 0), (0, 0)), "newText": "import std::list\n" }],
            "tags": [1],
        });
        assert_eq!(
            summary(&item),
            "map(items, f) | Function | fn map<T, U>(List<T>, fn(T) -> U) -> List<U> | (std::list) \
             | ` |> list::map(_, ${1:f})$0` | +1 edit | deprecated"
        );
        assert_eq!(
            summary(&json!({ "label": "x", "insertText": "x" })),
            "x | -"
        );
        assert!(names(&item, "map"));
        assert!(names(&item, "map@std::list"));
        assert!(!names(&item, "map@std::map"));
        assert!(!names(&item, "ma"));
        let shown = printed(&json!({ "label": "x", "data": { "k": 1 } }));
        assert_eq!(shown, json!({ "label": "x" }));
    }

    #[test]
    fn marker_words() {
        let args: Vec<String> = ["a", "`let", "guard`", "b@std::list"]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(words(&args).unwrap(), ["a", "let guard", "b@std::list"]);
        assert_eq!(
            words(&["`case".to_owned(), "{`".to_owned()]).unwrap(),
            ["case {"]
        );
        assert!(words(&["`open".to_owned()]).is_err());
        assert_eq!(words(&[]).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn trigger_context() {
        let triggers = vec![".".to_owned(), ">".to_owned()];
        assert_eq!(
            completion_context("xs.", 3, &triggers),
            json!({ "triggerKind": 2, "triggerCharacter": "." })
        );
        assert_eq!(
            completion_context("xs.m", 4, &triggers),
            json!({ "triggerKind": 1 })
        );
        assert_eq!(
            completion_context("", 0, &triggers),
            json!({ "triggerKind": 1 })
        );
    }

    #[test]
    fn edit_checks() {
        let one_line = json!({ "label": "a", "textEdit": { "range": range((2, 4), (2, 6)), "newText": "ab" } });
        assert!(check_edit(&one_line, at(2, 6)).is_ok());
        assert!(check_edit(&one_line, at(2, 7)).is_err());
        let no_edit = json!({ "label": "a", "insertText": "ab" });
        let e = check_edit(&no_edit, at(2, 6)).unwrap_err();
        assert!(e.contains("no `textEdit`"), "{e}");
        let two_lines = json!({ "label": "a", "textEdit": { "range": range((1, 4), (2, 6)), "newText": "ab" } });
        let e = check_edit(&two_lines, at(2, 6)).unwrap_err();
        assert!(e.contains("D-1394"), "{e}");
        let replace = json!({ "label": "a", "textEdit": {
            "insert": range((0, 1), (0, 3)), "replace": range((0, 1), (0, 5)), "newText": "x" } });
        assert!(check_edit(&replace, at(0, 3)).is_ok());
        let not_prefix = json!({ "label": "a", "textEdit": {
            "insert": range((0, 2), (0, 3)), "replace": range((0, 1), (0, 5)), "newText": "x" } });
        assert!(check_edit(&not_prefix, at(0, 3)).is_err());
        let overlap = json!({ "label": "a",
            "textEdit": { "range": range((2, 4), (2, 6)), "newText": "ab" },
            "additionalTextEdits": [{ "range": range((2, 5), (2, 5)), "newText": "q" }] });
        assert!(check_edit(&overlap, at(2, 6)).is_err());
        let apart = json!({ "label": "a",
            "textEdit": { "range": range((2, 4), (2, 6)), "newText": "ab" },
            "additionalTextEdits": [{ "range": range((0, 0), (0, 0)), "newText": "q" }] });
        assert!(check_edit(&apart, at(2, 6)).is_ok());
        assert!(check_edit(&json!({ "label": "plain" }), at(0, 0)).is_err());
    }

    #[test]
    fn snippets() {
        assert_eq!(
            expand_snippet(" |> list::map(_, ${1:f})$0"),
            " |> list::map(_, f)"
        );
        assert_eq!(
            expand_snippet("fn ${1:name}($2) -> ${3:Nil} {\n  $0\n}"),
            "fn name() -> Nil {\n  \n}"
        );
        assert_eq!(expand_snippet("${1:outer ${2:inner}}"), "outer inner");
        assert_eq!(expand_snippet("${1|a,b|}"), "a");
        assert_eq!(expand_snippet("${1}x"), "x");
        assert_eq!(expand_snippet(r"\$1 \} \\ a$"), r"$1 } \ a$");
        assert_eq!(expand_snippet("${TM_SELECTED_TEXT:sel}"), "sel");
        assert_eq!(expand_snippet("$TM_FILENAME"), "TM_FILENAME");
        assert_eq!(expand_snippet("${1:Some(x)}"), "Some(x)");
    }

    #[test]
    fn applying_items() {
        let text = "import std::io\n\nfn main() {\n  let n = xs.le\n}\n";
        let item = json!({
            "label": "length",
            "insertTextFormat": 2,
            "textEdit": { "range": range((3, 12), (3, 15)), "newText": " |> list::length$0" },
            "additionalTextEdits": [{ "range": range((0, 0), (0, 0)), "newText": "import std::list\n" }],
        });
        let applied = apply_item(text, &item, at(3, 15), Encoding::Utf16).unwrap();
        assert_eq!(
            applied,
            "import std::list\nimport std::io\n\nfn main() {\n  let n = xs |> list::length\n}\n"
        );
        assert_eq!(syntax_errors(&applied), Vec::<String>::new());
        // No `textEdit`: the word before the position is replaced.
        let plain = json!({ "label": "length" });
        let applied = apply_item("f(le)", &plain, at(0, 4), Encoding::Utf16).unwrap();
        assert_eq!(applied, "f(length)");
        // Later lines are indented as the edit's first line, with no mode
        // or `adjustIndentation`, unless the item says `asIs` (D-1506).
        let text = "fn main() {\n  case x {\n    _ => gu\n  }\n}\n";
        let guard = json!({
            "label": "guard",
            "insertTextFormat": 2,
            "textEdit": {
                "range": range((2, 9), (2, 11)),
                "newText": "guard ${1:c} else {\n  $0\n}",
            },
        });
        let applied = apply_item(text, &guard, at(2, 11), Encoding::Utf16).unwrap();
        assert_eq!(
            applied,
            "fn main() {\n  case x {\n    _ => guard c else {\n      \n    }\n  }\n}\n"
        );
        // `adjustIndentation`, which the server sends, is the same.
        let mut adjust = guard.clone();
        adjust["insertTextMode"] = json!(2);
        assert_eq!(
            apply_item(text, &adjust, at(2, 11), Encoding::Utf16).unwrap(),
            applied
        );
        let mut as_is = guard;
        as_is["insertTextMode"] = json!(1);
        let applied = apply_item(text, &as_is, at(2, 11), Encoding::Utf16).unwrap();
        assert_eq!(
            applied,
            "fn main() {\n  case x {\n    _ => guard c else {\n  \n}\n  }\n}\n"
        );
        // Only the indentation up to the cursor counts, as the protocol
        // says: `<2 blanks><cursor><2 blanks>x` indents by two.
        let text = "    x\n";
        let item = json!({
            "label": "ab",
            "textEdit": { "range": range((0, 2), (0, 2)), "newText": "a\nb" },
        });
        let applied = apply_item(text, &item, at(0, 2), Encoding::Utf16).unwrap();
        assert_eq!(applied, "  a\n  b  x\n");
    }

    #[test]
    fn errors_have_positions() {
        let errors = syntax_errors("fn main() {\n  let = 1\n}\n");
        assert_ne!(errors, Vec::<String>::new());
        assert!(errors[0].starts_with("2:"), "{errors:?}");
        assert_eq!(text_lines("a\nb"), json!(["a", "b"]));
    }

    #[test]
    fn signature_labels() {
        let answer = json!({
            "activeSignature": 0,
            "activeParameter": 1,
            "signatures": [{
                "label": "fn f(é: Int, b: Int)",
                "parameters": [{ "label": [5, 11] }, { "label": "b: Int" }],
            }],
        });
        let shown = signature_help(&answer, Encoding::Utf16).unwrap();
        assert_eq!(
            shown["signatures"][0]["parameters"][0]["label"],
            json!("[5, 11] é: Int")
        );
        assert_eq!(
            shown["signatures"][0]["parameters"][1]["label"],
            json!("b: Int")
        );
        // In UTF-8 the same text is one unit longer.
        assert!(signature_help(&answer, Encoding::Utf8).is_ok());
        let bad = json!({ "signatures": [{ "label": "f", "parameters": [{ "label": [0, 9] }] }] });
        assert!(signature_help(&bad, Encoding::Utf16).is_err());
        assert_eq!(
            signature_help(&Value::Null, Encoding::Utf16).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn inlay_lines() {
        let text = "fn main() {\n  let é = [1]\n}\n";
        let answer = json!([
            {
                "position": { "line": 1, "character": 7 },
                "label": ": List<Int>",
                "kind": 1,
                "textEdits": [{ "range": range((1, 7), (1, 7)), "newText": ": List<Int>" }],
            },
            {
                "position": { "line": 0, "character": 0 },
                "label": [{ "value": "a" }, { "value": "B", "location": { "uri": "file:///p/x", "range": range((4, 2), (4, 3)) } }],
                "paddingLeft": true,
                "tooltip": { "kind": "plaintext", "value": "t" },
            },
        ]);
        let display = |uri: &str| uri.trim_start_matches("file:///p/").to_owned();
        let shown = inlay_hints(&answer, text, Encoding::Utf16, &display);
        assert_eq!(
            shown,
            json!([
                "1:1 - `a`+`B`@x:5:3 padding-left tooltip `t`",
                "2:8 type `: List<Int>` edit 2:8-2:8 `: List<Int>`",
            ])
        );
        assert_eq!(
            inlay_hints(&Value::Null, text, Encoding::Utf16, &display),
            Value::Null
        );
    }

    #[test]
    fn sweep_points() {
        let text = "fn main() {\n  xs.m |> f(a, g(b), )\n}\n";
        let completion: Vec<&str> = completion_points(text)
            .into_iter()
            .map(|at| text.get(..at).unwrap().rsplit(['\n', ' ']).next().unwrap())
            .collect();
        assert_eq!(
            completion,
            ["main", "xs", "xs.", "xs.m", "|>", "f", "f(a", "g", "g(b"]
        );
        let signature: Vec<usize> = signature_points(text);
        let shown: Vec<&str> = signature
            .iter()
            .map(|&at| text.get(at..at + 1).unwrap())
            .collect();
        // After `f(`, after its first comma, after `g(`, at `g`'s `)`,
        // after `f`'s second comma and at its `)`. `main()` is a parameter
        // list.
        assert_eq!(shown, ["a", " ", "b", ")", " ", ")"]);
        let (kept, dropped) = sample(&[1, 2, 3, 4, 5], 2);
        assert_eq!(kept, [1, 4]);
        assert_eq!(dropped, 3);
        assert_eq!(sample(&[1, 2], 5), (vec![1, 2], 0));
    }

    /// The examples group's resolves take the actions with `data` of the
    /// `codeAction` answers only, each with its listing's label, and count
    /// every action listed (T-4).
    #[test]
    fn listed_actions_of_the_examples() {
        let prepared = BTreeMap::from([
            (
                "src/main.cheby:1:2 codeAction".to_owned(),
                json!([
                    { "title": "A", "data": { "id": 1 } },
                    { "title": "B" },
                    { "title": "C", "data": null },
                ]),
            ),
            (
                "src/main.cheby:1:2 hover".to_owned(),
                json!([{ "data": 1 }]),
            ),
            ("src/main.cheby:3:0 codeAction".to_owned(), json!(null)),
        ]);
        let (all, with_data) = listed_actions(&prepared);
        assert_eq!(all, 3);
        assert_eq!(
            with_data,
            [(
                "src/main.cheby:1:2 codeAction".to_owned(),
                json!({ "title": "A", "data": { "id": 1 } })
            )]
        );
        // The budget's stride keeps the first and every `step`-th after it.
        let (kept, dropped) = sample(&(0..450).collect::<Vec<u32>>(), 200);
        assert_eq!(kept.len(), 150);
        assert_eq!(kept.first().copied(), Some(0));
        assert_eq!(kept.get(1).copied(), Some(3));
        assert_eq!(dropped, 300);
    }

    /// D-1716: the stride order spreads the first `budget` items over the
    /// list and still holds every item once.
    #[test]
    fn stride_order() {
        let points: Vec<u32> = (0..10).collect();
        assert_eq!(by_stride(&points, 3), [0, 4, 8, 1, 5, 9, 2, 6, 3, 7]);
        assert_eq!(by_stride(&points, 20), points);
        // A budget of one, or none, keeps the list's order.
        assert_eq!(by_stride(&points, 1), points);
        assert_eq!(by_stride(&points, 0), points);
        assert_eq!(by_stride::<u32>(&[], 5), Vec::<u32>::new());
    }
}
