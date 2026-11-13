//! Positions as the protocol counts them, and edits.
//!
//! The protocol's lines end at `\n`, `\r\n` or a lone `\r`, and its
//! columns count units of the negotiated encoding (D-1368). The compiler's
//! lines end at `\n` only, and its columns count code points from 1, with
//! a leading byte-order mark not counted (D-421). A BOM is a character in
//! text the client sent, and not in disk text, which a client loads with
//! the BOM stripped ([`Bom`]). This module is the harness's own copy of
//! these rules, written apart from the server's, so that each checks the
//! other.

use std::collections::BTreeMap;

use serde_json::Value;

const BOM: char = '\u{feff}';

/// A position encoding of the protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Encoding {
    /// Bytes of UTF-8.
    Utf8,
    /// UTF-16 code units, the protocol's default.
    Utf16,
    /// Code points.
    Utf32,
}

impl Encoding {
    /// The protocol's name: `utf-8`, `utf-16` or `utf-32`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
            Self::Utf32 => "utf-32",
        }
    }

    /// The encoding the protocol names `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "utf-8" => Some(Self::Utf8),
            "utf-16" => Some(Self::Utf16),
            "utf-32" => Some(Self::Utf32),
            _ => None,
        }
    }

    /// How many units `c` takes.
    #[must_use]
    pub const fn units(self, c: char) -> usize {
        match self {
            Self::Utf8 => c.len_utf8(),
            Self::Utf16 => c.len_utf16(),
            Self::Utf32 => 1,
        }
    }

    /// The units of `text`.
    #[must_use]
    pub fn count(self, text: &str) -> usize {
        text.chars()
            .fold(0_usize, |n, c| n.saturating_add(self.units(c)))
    }
}

/// Whether a leading byte-order mark is a character of the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bom {
    /// The client sent the text, so the BOM is a character to it.
    Counted,
    /// Disk text: clients strip a BOM when they load a file.
    Hidden,
}

/// A position: a 0-based line and a column in units of an encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    /// The 0-based line.
    pub line: u32,
    /// The column in units.
    pub character: u32,
}

impl Position {
    /// The protocol's JSON form.
    #[must_use]
    pub fn to_json(self) -> Value {
        serde_json::json!({ "line": self.line, "character": self.character })
    }

    /// Reads the protocol's JSON form.
    ///
    /// # Errors
    ///
    /// Returns a message if `value` is not a position.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let field = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| format!("not a position: {value}"))
        };
        Ok(Self {
            line: field("line")?,
            character: field("character")?,
        })
    }
}

/// The protocol's line index of one text.
#[derive(Clone, Debug)]
pub struct Lines<'a> {
    text: &'a str,
    /// Byte offset where each line's content starts.
    starts: Vec<usize>,
    /// Byte offset where each line's content ends, before its terminator.
    ends: Vec<usize>,
}

impl<'a> Lines<'a> {
    /// Indexes `text` by the protocol's line rules.
    #[must_use]
    pub fn new(text: &'a str, bom: Bom) -> Self {
        let first = if bom == Bom::Hidden && text.starts_with(BOM) {
            BOM.len_utf8()
        } else {
            0
        };
        let bytes = text.as_bytes();
        let mut starts = vec![first];
        let mut ends = Vec::new();
        let mut i = first;
        while let Some(&byte) = bytes.get(i) {
            let next = i.saturating_add(1);
            match byte {
                b'\n' => {
                    ends.push(i);
                    starts.push(next);
                }
                b'\r' => {
                    ends.push(i);
                    if bytes.get(next) == Some(&b'\n') {
                        starts.push(next.saturating_add(1));
                        i = next;
                    } else {
                        starts.push(next);
                    }
                }
                _ => {}
            }
            i = i.saturating_add(1);
        }
        ends.push(text.len());
        Self { text, starts, ends }
    }

    /// The number of lines, counting an empty last line.
    #[must_use]
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    /// Whether there are no lines, which never happens: an empty text has
    /// one empty line.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// The content of 0-based `line`, without its terminator.
    #[must_use]
    pub fn line(&self, line: usize) -> Option<&'a str> {
        let start = *self.starts.get(line)?;
        let end = *self.ends.get(line)?;
        self.text.get(start..end)
    }

    /// The position of byte `offset`, floored to a character boundary. An
    /// offset inside a terminator is the line's content end.
    #[must_use]
    pub fn position(&self, offset: usize, encoding: Encoding) -> Position {
        let offset = floor_boundary(self.text, offset.min(self.text.len()));
        let line = self
            .starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        let start = self.starts.get(line).copied().unwrap_or(0);
        let end = self.ends.get(line).copied().unwrap_or(self.text.len());
        let offset = offset.clamp(start, end.max(start));
        let before = self.text.get(start..offset).unwrap_or_default();
        Position {
            line: u32::try_from(line).unwrap_or(u32::MAX),
            character: u32::try_from(encoding.count(before)).unwrap_or(u32::MAX),
        }
    }

    /// The byte offset of `position`. Past the last line: the end of the
    /// text. Past the line's end: its content end. Inside a character: its
    /// start.
    #[must_use]
    pub fn offset(&self, position: Position, encoding: Encoding) -> usize {
        let line = usize::try_from(position.line).unwrap_or(usize::MAX);
        let (Some(&start), Some(&end)) = (self.starts.get(line), self.ends.get(line)) else {
            return self.text.len();
        };
        let wanted = usize::try_from(position.character).unwrap_or(usize::MAX);
        let content = self.text.get(start..end).unwrap_or_default();
        let mut units = 0_usize;
        for (at, c) in content.char_indices() {
            let after = units.saturating_add(encoding.units(c));
            if after > wanted {
                return start.saturating_add(at);
            }
            units = after;
        }
        end
    }

    /// The text from `start` to `end`, both on one line and inside its
    /// content, or `None` for a range that is not.
    #[must_use]
    pub fn slice(&self, start: Position, end: Position, encoding: Encoding) -> Option<&'a str> {
        let content = self.line(usize::try_from(start.line).ok()?)?;
        if start.line != end.line
            || start > end
            || usize::try_from(end.character).ok()? > encoding.count(content)
        {
            return None;
        }
        let (a, b) = (self.offset(start, encoding), self.offset(end, encoding));
        self.text.get(a..b)
    }
}

/// `offset` moved back to a character boundary of `text`.
fn floor_boundary(text: &str, mut offset: usize) -> usize {
    while offset > 0 && !text.is_char_boundary(offset) {
        offset = offset.saturating_sub(1);
    }
    offset
}

/// The byte offset of the compiler's 1-based `line` and code-point `col`
/// (D-421): lines end at `\n` only, and a BOM at the start is not
/// counted. Clamped to the line's end and to the text.
#[must_use]
pub fn compiler_offset(text: &str, line: u32, col: u32) -> usize {
    let index = usize::try_from(line.saturating_sub(1)).unwrap_or(usize::MAX);
    let Some(mut start) = text
        .split_inclusive('\n')
        .scan(0_usize, |at, piece| {
            let start = *at;
            *at = at.saturating_add(piece.len());
            Some(start)
        })
        .nth(index)
        .or_else(|| (index == 0).then_some(0))
    else {
        return text.len();
    };
    if start == 0 && text.starts_with(BOM) {
        start = BOM.len_utf8();
    }
    let rest = text.get(start..).unwrap_or_default();
    let content = rest.split('\n').next().unwrap_or_default();
    let skip = usize::try_from(col.saturating_sub(1)).unwrap_or(usize::MAX);
    let within = content
        .char_indices()
        .nth(skip)
        .map_or(content.len(), |(at, _)| at);
    start.saturating_add(within)
}

/// Reads a protocol range as byte offsets of `lines`' text.
///
/// # Errors
///
/// Returns a message if `range` is not a range.
pub fn offsets(
    lines: &Lines<'_>,
    range: &Value,
    encoding: Encoding,
) -> Result<(usize, usize), String> {
    let start = Position::from_json(range.get("start").unwrap_or(&Value::Null))?;
    let end = Position::from_json(range.get("end").unwrap_or(&Value::Null))?;
    let (a, b) = (lines.offset(start, encoding), lines.offset(end, encoding));
    if b < a {
        return Err(format!("a range whose end is before its start: {range}"));
    }
    Ok((a, b))
}

/// `text` after the protocol's `TextEdit`s, which all refer to the
/// original text and may not overlap. Edits at the same offset apply in
/// the order given.
///
/// # Errors
///
/// Returns a message for a malformed or overlapping edit.
pub fn apply_text_edits(
    text: &str,
    edits: &[Value],
    encoding: Encoding,
    bom: Bom,
) -> Result<String, String> {
    let lines = Lines::new(text, bom);
    let mut spans = Vec::with_capacity(edits.len());
    for edit in edits {
        let range = edit
            .get("range")
            .ok_or_else(|| format!("an edit without a range: {edit}"))?;
        let new_text = edit
            .get("newText")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("an edit without newText: {edit}"))?;
        let (start, end) = offsets(&lines, range, encoding)?;
        spans.push((start, end, new_text));
    }
    // A stable sort keeps the given order of inserts at one offset.
    spans.sort_by_key(|&(start, end, _)| (start, end));
    let mut out = String::with_capacity(text.len());
    let mut at = 0_usize;
    for (start, end, new_text) in spans {
        if start < at {
            return Err(format!("overlapping edits at byte {start}"));
        }
        out.push_str(text.get(at..start).unwrap_or_default());
        out.push_str(new_text);
        at = end;
    }
    out.push_str(text.get(at..).unwrap_or_default());
    Ok(out)
}

/// Applies a `WorkspaceEdit` to `files`, a map from URI to text: its
/// `changes`, or its `documentChanges` with text edits and the `create`,
/// `rename` and `delete` operations, in order. Every text counts a BOM, as
/// the client's own buffers do.
///
/// # Errors
///
/// Returns a message for a malformed edit, an edit of a file not in
/// `files`, or an operation that the protocol says must fail.
pub fn apply_workspace_edit(
    files: &mut BTreeMap<String, String>,
    edit: &Value,
    encoding: Encoding,
) -> Result<(), String> {
    if let Some(changes) = edit.get("documentChanges") {
        let changes = changes
            .as_array()
            .ok_or_else(|| format!("documentChanges is not an array: {changes}"))?;
        for change in changes {
            apply_document_change(files, change, encoding)?;
        }
        return Ok(());
    }
    if let Some(changes) = edit.get("changes") {
        let changes = changes
            .as_object()
            .ok_or_else(|| format!("changes is not an object: {changes}"))?;
        for (uri, edits) in changes {
            edit_file(files, uri, edits, encoding)?;
        }
    }
    Ok(())
}

fn apply_document_change(
    files: &mut BTreeMap<String, String>,
    change: &Value,
    encoding: Encoding,
) -> Result<(), String> {
    let uri_field = |name: &str| {
        change
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("`{name}` is missing in {change}"))
    };
    let flag = |name: &str| {
        change
            .get("options")
            .and_then(|o| o.get(name))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    match change.get("kind").and_then(Value::as_str) {
        None => {
            let uri = change
                .get("textDocument")
                .and_then(|d| d.get("uri"))
                .and_then(Value::as_str)
                .ok_or_else(|| format!("a text document edit without a URI: {change}"))?;
            let edits = change.get("edits").unwrap_or(&Value::Null);
            edit_file(files, uri, edits, encoding)
        }
        Some("create") => {
            let uri = uri_field("uri")?;
            if files.contains_key(&uri) && !flag("overwrite") {
                if flag("ignoreIfExists") {
                    return Ok(());
                }
                return Err(format!("create of {uri}, which exists"));
            }
            files.insert(uri, String::new());
            Ok(())
        }
        Some("rename") => {
            let (old, new) = (uri_field("oldUri")?, uri_field("newUri")?);
            if files.contains_key(&new) && !flag("overwrite") {
                if flag("ignoreIfExists") {
                    return Ok(());
                }
                return Err(format!("rename onto {new}, which exists"));
            }
            let text = files
                .remove(&old)
                .ok_or_else(|| format!("rename of {old}, which does not exist"))?;
            files.insert(new, text);
            Ok(())
        }
        Some("delete") => {
            let uri = uri_field("uri")?;
            if files.remove(&uri).is_none() && !flag("ignoreIfNotExists") {
                return Err(format!("delete of {uri}, which does not exist"));
            }
            Ok(())
        }
        Some(other) => Err(format!("an unknown resource operation `{other}`")),
    }
}

fn edit_file(
    files: &mut BTreeMap<String, String>,
    uri: &str,
    edits: &Value,
    encoding: Encoding,
) -> Result<(), String> {
    let edits = edits
        .as_array()
        .ok_or_else(|| format!("the edits of {uri} are not an array"))?;
    let text = files
        .get_mut(uri)
        .ok_or_else(|| format!("an edit of {uri}, which is not open"))?;
    *text = apply_text_edits(text, edits, encoding, Bom::Counted)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ALL: [Encoding; 3] = [Encoding::Utf8, Encoding::Utf16, Encoding::Utf32];

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn line_ends() {
        let text = "a\nb\r\nc\rd";
        let lines = Lines::new(text, Bom::Counted);
        assert_eq!(lines.len(), 4);
        let shown: Vec<_> = (0..4).map(|i| lines.line(i).unwrap()).collect();
        assert_eq!(shown, ["a", "b", "c", "d"]);
        // Inside `\r\n` is the line's content end.
        assert_eq!(lines.position(4, Encoding::Utf16), pos(1, 1));
        assert_eq!(lines.offset(pos(9, 0), Encoding::Utf16), text.len());
        assert_eq!(lines.offset(pos(0, 9), Encoding::Utf16), 1);
        assert_eq!(Lines::new("", Bom::Counted).len(), 1);
        assert_eq!(Lines::new("x\n", Bom::Counted).len(), 2);
    }

    #[test]
    fn encodings() {
        let text = "é😀x";
        let lines = Lines::new(text, Bom::Counted);
        let x = text.len().saturating_sub(1);
        assert_eq!(lines.position(x, Encoding::Utf8), pos(0, 6));
        assert_eq!(lines.position(x, Encoding::Utf16), pos(0, 3));
        assert_eq!(lines.position(x, Encoding::Utf32), pos(0, 2));
        // Inside the surrogate pair: the character's start.
        assert_eq!(lines.offset(pos(0, 2), Encoding::Utf16), 2);
        for enc in ALL {
            for offset in [0, 2, 6, 7] {
                assert_eq!(lines.offset(lines.position(offset, enc), enc), offset);
            }
        }
    }

    #[test]
    fn bom() {
        let text = "\u{feff}ab\nc";
        let counted = Lines::new(text, Bom::Counted);
        let hidden = Lines::new(text, Bom::Hidden);
        assert_eq!(counted.position(3, Encoding::Utf16), pos(0, 1));
        assert_eq!(hidden.position(3, Encoding::Utf16), pos(0, 0));
        assert_eq!(hidden.offset(pos(0, 0), Encoding::Utf16), 3);
        assert_eq!(hidden.line(0), Some("ab"));
    }

    #[test]
    fn compiler_columns() {
        let text = "\u{feff}fn é\r\n  x\ry\n";
        // Line 1 starts after the BOM, line 2's lone `\r` is no line break.
        assert_eq!(compiler_offset(text, 1, 1), 3);
        assert_eq!(compiler_offset(text, 1, 4), 6);
        let y = text.find('y').unwrap();
        assert_eq!(compiler_offset(text, 2, 5), y);
        assert_eq!(compiler_offset(text, 3, 1), text.len());
        assert_eq!(compiler_offset(text, 9, 1), text.len());
        assert_eq!(compiler_offset("", 1, 1), 0);
        // Mapped onto the protocol's lines, the lone `\r` moves `y`.
        let lines = Lines::new(text, Bom::Hidden);
        assert_eq!(lines.position(y, Encoding::Utf16), pos(2, 0));
    }

    #[test]
    fn text_edits() {
        let text = "let a = 1\nlet 😀 = 2\n";
        let edits = [
            json!({"range": {"start": pos(1, 4).to_json(), "end": pos(1, 6).to_json()}, "newText": "b"}),
            json!({"range": {"start": pos(0, 4).to_json(), "end": pos(0, 5).to_json()}, "newText": "x"}),
            json!({"range": {"start": pos(0, 0).to_json(), "end": pos(0, 0).to_json()}, "newText": "// a\n"}),
        ];
        assert_eq!(
            apply_text_edits(text, &edits, Encoding::Utf16, Bom::Counted).as_deref(),
            Ok("// a\nlet x = 1\nlet b = 2\n")
        );
        let overlapping = [
            json!({"range": {"start": pos(0, 0).to_json(), "end": pos(0, 5).to_json()}, "newText": ""}),
            json!({"range": {"start": pos(0, 2).to_json(), "end": pos(0, 3).to_json()}, "newText": ""}),
        ];
        assert!(apply_text_edits(text, &overlapping, Encoding::Utf16, Bom::Counted).is_err());
    }

    #[test]
    fn workspace_edits() {
        let mut files = BTreeMap::from([
            ("file:///a".to_owned(), "abc".to_owned()),
            ("file:///b".to_owned(), "x".to_owned()),
        ]);
        let range = json!({"start": pos(0, 0).to_json(), "end": pos(0, 1).to_json()});
        let edit = json!({"changes": {"file:///a": [{"range": range, "newText": "z"}]}});
        apply_workspace_edit(&mut files, &edit, Encoding::Utf16).unwrap();
        assert_eq!(files["file:///a"], "zbc");
        let edit = json!({"documentChanges": [
            {"kind": "rename", "oldUri": "file:///b", "newUri": "file:///c"},
            {"textDocument": {"uri": "file:///c", "version": 1}, "edits": [{"range": range, "newText": "y"}]},
            {"kind": "create", "uri": "file:///d"},
            {"kind": "delete", "uri": "file:///a"},
        ]});
        apply_workspace_edit(&mut files, &edit, Encoding::Utf16).unwrap();
        let keys: Vec<_> = files.keys().map(String::as_str).collect();
        assert_eq!(keys, ["file:///c", "file:///d"]);
        assert_eq!(files["file:///c"], "y");
        let bad = json!({"documentChanges": [{"kind": "delete", "uri": "file:///zz"}]});
        assert!(apply_workspace_edit(&mut files, &bad, Encoding::Utf16).is_err());
    }
}
