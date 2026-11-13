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

use std::fmt::Write as _;

use serde_json::{Map, Value, json};

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
#[derive(Debug, Clone, Default, PartialEq)]
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
        .map(Vec::as_slice)
        .unwrap_or_default()
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
            .unwrap_or(label(item));
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
                .map(Vec::as_slice)
                .unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn range(a: (u32, u32), b: (u32, u32)) -> Value {
        json!({ "start": at(a.0, a.1).to_json(), "end": at(b.0, b.1).to_json() })
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
}
