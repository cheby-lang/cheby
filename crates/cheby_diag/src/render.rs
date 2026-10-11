//! Rendering of diagnostics (D-418, D-421).
//!
//! The layout follows rustc: a header, a location line, a snippet of the
//! affected lines with `^` under primary spans and `-` under secondary
//! spans, then `= note:` and `= help:` lines.
//!
//! The text is plain. With [`Colors::On`], SGR escapes are added around
//! parts of it, in rustc's palette, and nothing else changes: removing the
//! escapes ([`strip_colors`]) gives exactly the plain rendering, which is
//! what goldens hold. The caller decides when to color (the CLI's
//! `--color`); this module never looks at the terminal or the environment.
//!
//! The palette:
//!
//! - `error[E…]`, and the `^` markers, labels and rails of primary labels
//!   in an error: bold bright red. In a warning: bold bright yellow.
//! - The header message after the code: bold.
//! - Line numbers, the `|` gutter, `-->`, `:::`, `...` and the `=` of
//!   `= note:`, and the `-` markers, labels and rails of secondary labels:
//!   bold bright blue.
//! - `note` and `help` in `= note:` and `= help:`: bold.
//! - Source text, the file location and note text: not styled.
//!
//! Details that D-418 leaves open (D-443):
//!
//! - Tabs show as four spaces, other control characters as their Unicode
//!   control pictures, and marker widths use display widths, so `^` lines
//!   up under wide characters.
//! - Several labels on one line share one marker row. The label whose
//!   markers end last is written inline, the others hang below it,
//!   connected by `|`. Line numbers are left-aligned, as in rustc.
//! - A label that spans lines is drawn with a rail on the left, from `_…^`
//!   under its first character to `|_…^` under its last.
//! - Labels in another file get their own section, introduced by `:::`.
//! - Lines between shown lines are printed if there is just one, and
//!   replaced by `...` otherwise.

use unicode_width::UnicodeWidthChar;

use crate::diagnostic::{Diagnostic, Label, LabelStyle, Severity};
use crate::source::{SourceFile, SourceMap, floor_char_boundary};
use crate::text::{FileId, TextRange, TextSize};

/// Display width of a tab in snippets.
const TAB_WIDTH: usize = 4;

/// Whether rendered text carries color escapes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Colors {
    /// Plain text, as in goldens.
    #[default]
    Off,
    /// The plain text with SGR escapes around its styled parts.
    On,
}

/// The style of a run of rendered text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tag {
    Plain,
    /// The severity word and code, and primary labels.
    Level(Severity),
    /// The header message, and `note` and `help` in child lines.
    Bold,
    /// The gutter, location arrows, and secondary labels.
    Gutter,
    /// `help` in a tool-level `help:` line.
    Help,
}

/// SGR reset.
const RESET: &str = "\x1b[0m";

impl Tag {
    /// The SGR sequence that starts the style, if any.
    const fn sgr(self) -> Option<&'static str> {
        match self {
            Self::Plain => None,
            Self::Level(Severity::Error) => Some("\x1b[1;91m"),
            Self::Level(Severity::Warning) => Some("\x1b[1;93m"),
            Self::Bold => Some("\x1b[1m"),
            Self::Gutter => Some("\x1b[1;94m"),
            Self::Help => Some("\x1b[1;96m"),
        }
    }
}

/// Collects styled text. Consecutive pushes with the same tag share one
/// pair of escapes, and a line break always closes the open run, so every
/// line stands alone.
struct Painter {
    colors: Colors,
    out: String,
    run: Tag,
    pending: String,
}

impl Painter {
    const fn new(colors: Colors) -> Self {
        Self {
            colors,
            out: String::new(),
            run: Tag::Plain,
            pending: String::new(),
        }
    }

    /// Adds `text` in style `tag`. Control characters other than tabs and
    /// line breaks are shown as their control pictures, in both modes, so
    /// text quoted from the source (in a message, a label, a note or a
    /// help) can never write a terminal escape.
    fn push(&mut self, tag: Tag, text: &str) {
        if text.is_empty() {
            return;
        }
        if tag != self.run {
            self.flush();
            self.run = tag;
        }
        for c in text.chars() {
            self.pending.push(control_picture(c));
        }
    }

    fn newline(&mut self) {
        self.flush();
        self.out.push('\n');
    }

    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        match (self.colors, self.run.sgr()) {
            (Colors::On, Some(start)) => {
                self.out.push_str(start);
                self.out.push_str(&self.pending);
                self.out.push_str(RESET);
            }
            _ => self.out.push_str(&self.pending),
        }
        self.pending.clear();
    }

    fn finish(mut self) -> String {
        self.flush();
        self.out
    }
}

/// Renders one diagnostic as plain text. The result ends with a line
/// break.
#[must_use]
pub fn render(diag: &Diagnostic, sources: &SourceMap) -> String {
    render_with(diag, sources, Colors::Off)
}

/// Renders one diagnostic, with color escapes if `colors` is
/// [`Colors::On`]. Without the escapes, the text is that of [`render`].
#[must_use]
pub fn render_with(diag: &Diagnostic, sources: &SourceMap, colors: Colors) -> String {
    let mut out = Painter::new(colors);
    render_into(&mut out, diag, sources);
    out.finish()
}

fn render_into(out: &mut Painter, diag: &Diagnostic, sources: &SourceMap) {
    let severity = diag.severity();
    out.push(
        Tag::Level(severity),
        &format!("{}[{}]", severity.as_str(), diag.code),
    );
    out.push(Tag::Bold, &format!(": {}", diag.message));
    out.newline();

    let sections = sections(diag, sources);
    let max_line = sections
        .iter()
        .filter_map(|s| s.lines.last().copied())
        .max()
        .unwrap_or(1);
    let width = digits(max_line);
    let pad = " ".repeat(width);

    let rule = format!("{pad} |");
    for (index, section) in sections.iter().enumerate() {
        let arrow = if index == 0 {
            "-->"
        } else {
            out.push(Tag::Gutter, &rule);
            out.newline();
            ":::"
        };
        out.push(Tag::Gutter, &format!("{pad}{arrow}"));
        out.push(Tag::Plain, &format!(" {}", section.location));
        out.newline();
        out.push(Tag::Gutter, &rule);
        out.newline();
        section.render(out, width);
    }

    let has_children = !diag.notes.is_empty() || !diag.helps.is_empty();
    if has_children && !sections.is_empty() {
        out.push(Tag::Gutter, &rule);
        out.newline();
    }
    let children = diag.notes.iter().map(|note| ("note", note.as_str())).chain(
        diag.helps
            .iter()
            .map(|help| ("help", help.message.as_str())),
    );
    for (kind, text) in children {
        let mut lines = text.split('\n');
        let first = lines.next().unwrap_or_default();
        out.push(Tag::Gutter, &format!("{pad} ="));
        out.push(Tag::Plain, " ");
        out.push(Tag::Bold, kind);
        out.push(Tag::Plain, &format!(": {first}"));
        out.newline();
        let indent = " ".repeat(width.saturating_add(kind.len()).saturating_add(5));
        for line in lines {
            out.push(Tag::Plain, format!("{indent}{line}").trim_end());
            out.newline();
        }
    }
}

/// Renders diagnostics as plain text in the given order, each followed by
/// an empty line.
#[must_use]
pub fn render_all(diags: &[Diagnostic], sources: &SourceMap) -> String {
    render_all_with(diags, sources, Colors::Off)
}

/// Like [`render_all`], with color escapes if `colors` is [`Colors::On`].
#[must_use]
pub fn render_all_with(diags: &[Diagnostic], sources: &SourceMap, colors: Colors) -> String {
    let mut out = Painter::new(colors);
    for diag in diags {
        render_into(&mut out, diag, sources);
        out.newline();
    }
    out.finish()
}

/// Renders an error that is not about the program, such as a missing
/// `cheby.toml`: `error: {message}` and a line break, without a code
/// (D-442). In color, `error` is bold bright red and the message bold.
#[must_use]
pub fn render_tool_error(message: &str, colors: Colors) -> String {
    let mut out = Painter::new(colors);
    out.push(Tag::Level(Severity::Error), "error");
    out.push(Tag::Bold, &format!(": {message}"));
    out.newline();
    out.finish()
}

/// Renders a `help: {message}` line that follows a tool-level error. In
/// color, `help` is bold bright cyan and the message is not styled.
#[must_use]
pub fn render_tool_help(message: &str, colors: Colors) -> String {
    let mut out = Painter::new(colors);
    out.push(Tag::Help, "help");
    out.push(Tag::Plain, &format!(": {message}"));
    out.newline();
    out.finish()
}

/// Removes SGR escapes (`ESC [ digits-and-semicolons m`), the only escapes
/// colored rendering adds. Other text, including other escapes, is kept.
#[must_use]
pub fn strip_colors(text: &str) -> String {
    const CSI: &str = "\x1b[";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, after)) = rest.split_once(CSI) {
        out.push_str(before);
        let params = after
            .find(|c: char| !(c.is_ascii_digit() || c == ';'))
            .unwrap_or(after.len());
        if let Some(tail) = after.get(params..).and_then(|tail| tail.strip_prefix('m')) {
            rest = tail;
        } else {
            out.push_str(CSI);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Number of decimal digits of `n`.
fn digits(n: u32) -> usize {
    n.checked_ilog10()
        .and_then(|log| usize::try_from(log).ok())
        .map_or(1, |log| log.saturating_add(1))
}

/// The labels of one file, with what is needed to draw them.
struct Section<'a> {
    file: &'a SourceFile,
    location: String,
    labels: Vec<Placed<'a>>,
    /// Line numbers to show, ascending, without gaps of one line.
    lines: Vec<u32>,
    /// Number of multi-line labels, which is the number of rails.
    rails: usize,
    /// The style of each rail, by rail index.
    rail_tags: Vec<Tag>,
    severity: Severity,
}

/// A label with its lines and display columns. For a single-line label,
/// `start_col..end_col` is the marker range. For a multi-line label,
/// `start_col` is on `start_line` and `end_col` (exclusive) on `end_line`.
struct Placed<'a> {
    label: &'a Label,
    start_line: u32,
    end_line: u32,
    start_col: usize,
    end_col: usize,
    /// Rail index of a multi-line label.
    rail: usize,
}

impl Placed<'_> {
    fn is_multiline(&self) -> bool {
        self.end_line > self.start_line
    }

    fn marker(&self) -> char {
        match self.label.style {
            LabelStyle::Primary => '^',
            LabelStyle::Secondary => '-',
        }
    }
}

/// Groups the labels by file: the file of the first primary label (or of
/// the first label) first, then the others in order of first use. Labels in
/// files the source map does not know are dropped.
fn sections<'a>(diag: &'a Diagnostic, sources: &'a SourceMap) -> Vec<Section<'a>> {
    let known: Vec<&Label> = diag
        .labels
        .iter()
        .filter(|label| sources.file(label.span.file).is_some())
        .collect();
    let anchor = known
        .iter()
        .find(|label| label.style == LabelStyle::Primary)
        .or_else(|| known.first());
    let mut order: Vec<FileId> = anchor.map(|label| label.span.file).into_iter().collect();
    for label in &known {
        if !order.contains(&label.span.file) {
            order.push(label.span.file);
        }
    }
    order
        .into_iter()
        .filter_map(|file_id| {
            let file = sources.file(file_id)?;
            let in_file: Vec<&Label> = known
                .iter()
                .copied()
                .filter(|label| label.span.file == file_id)
                .collect();
            let first = if anchor.is_some_and(|a| a.span.file == file_id) {
                anchor.copied()
            } else {
                in_file.first().copied()
            }?;
            let location = sources.location(first.span)?.to_string();
            Some(Section::new(file, location, &in_file, diag.severity()))
        })
        .collect()
}

impl<'a> Section<'a> {
    fn new(
        file: &'a SourceFile,
        location: String,
        labels: &[&'a Label],
        severity: Severity,
    ) -> Self {
        let mut placed: Vec<Placed<'a>> = labels.iter().map(|label| place(file, label)).collect();

        let mut multi: Vec<usize> = (0..placed.len())
            .filter(|&i| placed.get(i).is_some_and(Placed::is_multiline))
            .collect();
        multi.sort_by_key(|&i| {
            placed.get(i).map(|p| {
                (
                    p.label.span.range.start(),
                    std::cmp::Reverse(p.label.span.range.end()),
                )
            })
        });
        let mut rail_tags = Vec::with_capacity(multi.len());
        for (rail, &i) in multi.iter().enumerate() {
            if let Some(p) = placed.get_mut(i) {
                p.rail = rail;
                rail_tags.push(label_tag(p.label, severity));
            }
        }

        let mut wanted: Vec<u32> = Vec::new();
        for p in &placed {
            wanted.push(p.start_line);
            wanted.push(p.end_line);
            if p.is_multiline() {
                wanted.push(p.start_line.saturating_add(1));
                wanted.push(p.end_line.saturating_sub(1));
            }
        }
        wanted.sort_unstable();
        wanted.dedup();
        let mut lines = Vec::with_capacity(wanted.len());
        for line in wanted {
            // A gap of exactly one line is shown instead of `...`.
            if let Some(&last) = lines.last()
                && line.checked_sub(last) == Some(2)
            {
                lines.push(last.saturating_add(1));
            }
            lines.push(line);
        }

        Self {
            file,
            location,
            labels: placed,
            lines,
            rails: multi.len(),
            rail_tags,
            severity,
        }
    }

    fn tag(&self, p: &Placed<'_>) -> Tag {
        label_tag(p.label, self.severity)
    }

    /// A row with `|` at the position of every active rail.
    fn rails_row(&self, active: &[bool]) -> Row {
        Row::with_rails(active, &self.rail_tags)
    }

    fn render(&self, out: &mut Painter, width: usize) {
        let pad = " ".repeat(width);
        let text_start = self.rails.saturating_mul(2);
        let mut previous: Option<u32> = None;
        for &line in &self.lines {
            if previous.is_some_and(|p| line > p.saturating_add(1)) {
                out.push(Tag::Gutter, "...");
                out.newline();
            }
            previous = Some(line);

            // Rails active on this line before any of its marker rows.
            let mut active: Vec<bool> = vec![false; self.rails];
            for p in self.labels.iter().filter(|p| p.is_multiline()) {
                if let Some(slot) = active.get_mut(p.rail) {
                    *slot = p.start_line < line && line <= p.end_line;
                }
            }

            let content = line_content(self.file, line);
            let mut source = self.rails_row(&active);
            source.put_str(text_start, &display_text(content), Tag::Plain);
            emit(out, &format!("{line:<width$}"), &source);

            self.single_line_rows(out, &pad, line, &active, text_start);

            for p in self.multiline_starting(line) {
                let tag = self.tag(p);
                let mut row = self.rails_row(&active);
                let at = text_start.saturating_add(p.start_col);
                row.fill(p.rail.saturating_mul(2).saturating_add(1), at, '_', tag);
                row.put(at, p.marker(), tag);
                emit(out, &pad, &row);
                if let Some(slot) = active.get_mut(p.rail) {
                    *slot = true;
                }
            }

            for p in self.multiline_ending(line) {
                let tag = self.tag(p);
                let mut row = self.rails_row(&active);
                let rail_at = p.rail.saturating_mul(2);
                let at = text_start.saturating_add(p.end_col).saturating_sub(1);
                row.put(rail_at, '|', tag);
                row.fill(rail_at.saturating_add(1), at, '_', tag);
                row.put(at, p.marker(), tag);
                if !p.label.message.is_empty() {
                    row.put_str(at.saturating_add(2), &p.label.message, tag);
                }
                emit(out, &pad, &row);
                if let Some(slot) = active.get_mut(p.rail) {
                    *slot = false;
                }
            }
        }
    }

    /// The marker rows of the single-line labels on `line`.
    fn single_line_rows(
        &self,
        out: &mut Painter,
        pad: &str,
        line: u32,
        active: &[bool],
        text_start: usize,
    ) {
        let mut here: Vec<&Placed<'_>> = self
            .labels
            .iter()
            .filter(|p| !p.is_multiline() && p.start_line == line)
            .collect();
        if here.is_empty() {
            return;
        }
        here.sort_by_key(|p| (p.start_col, p.end_col));

        let mut markers = self.rails_row(active);
        // Secondary markers first, so primary markers win where they overlap.
        for style in [LabelStyle::Secondary, LabelStyle::Primary] {
            for p in here.iter().filter(|p| p.label.style == style) {
                let from = text_start.saturating_add(p.start_col);
                let to = text_start.saturating_add(p.end_col);
                markers.fill(from, to, p.marker(), self.tag(p));
            }
        }
        // The label whose markers end last is written inline.
        let inline = here
            .iter()
            .copied()
            .max_by_key(|p| (p.end_col, p.start_col))
            .filter(|p| !p.label.message.is_empty());
        if let Some(p) = inline {
            let at = text_start.saturating_add(p.end_col).saturating_add(1);
            markers.put_str(at, &p.label.message, self.tag(p));
        }
        emit(out, pad, &markers);

        let hanging: Vec<&Placed<'_>> = here
            .iter()
            .copied()
            .filter(|p| !p.label.message.is_empty())
            .filter(|p| !inline.is_some_and(|i| std::ptr::eq(*p, i)))
            .collect();
        for count in (1..=hanging.len()).rev() {
            let Some((current, before)) = hanging.get(..count).and_then(<[_]>::split_last) else {
                continue;
            };
            let mut connector = self.rails_row(active);
            for p in before.iter().chain(std::iter::once(current)) {
                connector.put(text_start.saturating_add(p.start_col), '|', self.tag(p));
            }
            emit(out, pad, &connector);
            let mut message = self.rails_row(active);
            for p in before {
                message.put(text_start.saturating_add(p.start_col), '|', self.tag(p));
            }
            message.put_str(
                text_start.saturating_add(current.start_col),
                &current.label.message,
                self.tag(current),
            );
            emit(out, pad, &message);
        }
    }

    fn multiline_starting(&self, line: u32) -> Vec<&Placed<'a>> {
        let mut found: Vec<&Placed<'a>> = self
            .labels
            .iter()
            .filter(|p| p.is_multiline() && p.start_line == line)
            .collect();
        found.sort_by_key(|p| p.rail);
        found
    }

    fn multiline_ending(&self, line: u32) -> Vec<&Placed<'a>> {
        let mut found: Vec<&Placed<'a>> = self
            .labels
            .iter()
            .filter(|p| p.is_multiline() && p.end_line == line)
            .collect();
        found.sort_by_key(|p| std::cmp::Reverse(p.rail));
        found
    }
}

/// Computes the lines and display columns of a label.
fn place<'a>(file: &SourceFile, label: &'a Label) -> Placed<'a> {
    let text = file.text();
    let index = file.line_index();
    let start = floor_char_boundary(text, label.span.range.start());
    let end = floor_char_boundary(text, label.span.range.end()).max(start);
    let start_line = index.line_of(start);
    let end_line = if end > start {
        // The line of the last byte in the span, so a span that ends with
        // a line break stays on that line.
        index.line_of(floor_char_boundary(text, end.saturating_sub(1)))
    } else {
        start_line
    };
    let start_col = column(file, start_line, start);
    let end_col = end_column(file, end_line, end);
    let end_col = if end_line == start_line {
        end_col.max(start_col.saturating_add(1))
    } else {
        end_col.max(1)
    };
    Placed {
        label,
        start_line,
        end_line,
        start_col,
        end_col,
        rail: 0,
    }
}

/// The content range of `line` (no terminator, no byte-order mark).
fn content_range(file: &SourceFile, line: u32) -> TextRange {
    file.line_index()
        .line_range(file.text(), line)
        .unwrap_or_default()
}

fn line_content(file: &SourceFile, line: u32) -> &str {
    content_range(file, line).slice(file.text()).unwrap_or("")
}

/// Display column (0-based) of `offset` on `line`. Offsets in the line
/// terminator are at the end of the text.
fn column(file: &SourceFile, line: u32, offset: TextSize) -> usize {
    let range = content_range(file, line);
    let offset = offset.clamp(range.start(), range.end());
    display_width(
        TextRange::new(range.start(), offset)
            .slice(file.text())
            .unwrap_or(""),
    )
}

/// Display column (0-based, exclusive) of a span end on `line`. A span that
/// includes the line terminator ends one column after the text.
fn end_column(file: &SourceFile, line: u32, end: TextSize) -> usize {
    let range = content_range(file, line);
    let col = column(file, line, end);
    if end > range.end() {
        col.saturating_add(1)
    } else {
        col
    }
}

fn char_width(c: char) -> usize {
    match c {
        '\t' => TAB_WIDTH,
        c if c.is_control() => 1,
        c => c.width().unwrap_or(0),
    }
}

fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// The text of a line as displayed: tabs as spaces, control characters as
/// control pictures.
fn display_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\t' => out.push_str(&" ".repeat(TAB_WIDTH)),
            c => out.push(control_picture(c)),
        }
    }
    out
}

/// The Unicode control picture of a C0 control character other than a tab
/// or a line break, or of DEL; any other character unchanged.
fn control_picture(c: char) -> char {
    match c {
        '\t' | '\n' => c,
        '\u{7f}' => '\u{2421}',
        c if u32::from(c) < 0x20 => {
            char::from_u32(u32::from(c).saturating_add(0x2400)).unwrap_or('\u{fffd}')
        }
        c => c,
    }
}

/// The style of a label's markers, message and rails.
fn label_tag(label: &Label, severity: Severity) -> Tag {
    match label.style {
        LabelStyle::Primary => Tag::Level(severity),
        LabelStyle::Secondary => Tag::Gutter,
    }
}

/// One cell of a [`Row`]: its text and style.
#[derive(Clone)]
struct Cell {
    text: String,
    tag: Tag,
}

/// One output row after the gutter, as a grid of display columns. A cell
/// holds the text drawn in that column: usually one character, a wide
/// character's second column holds nothing, and zero-width characters are
/// appended to the cell before them.
struct Row {
    cells: Vec<Cell>,
}

impl Row {
    /// A row with `|` at the position of every active rail, each in the
    /// style of its rail.
    fn with_rails(active: &[bool], tags: &[Tag]) -> Self {
        let mut row = Self { cells: Vec::new() };
        for (rail, &on) in active.iter().enumerate() {
            if on {
                let tag = tags.get(rail).copied().unwrap_or(Tag::Plain);
                row.put(rail.saturating_mul(2), '|', tag);
            }
        }
        row
    }

    fn cell(&mut self, at: usize) -> Option<&mut Cell> {
        if self.cells.len() <= at {
            let blank = Cell {
                text: " ".to_owned(),
                tag: Tag::Plain,
            };
            self.cells.resize(at.saturating_add(1), blank);
        }
        self.cells.get_mut(at)
    }

    fn put(&mut self, at: usize, c: char, tag: Tag) {
        if let Some(cell) = self.cell(at) {
            cell.text.clear();
            cell.text.push(c);
            cell.tag = tag;
        }
    }

    /// Writes `c` in the cells `from..to`.
    fn fill(&mut self, from: usize, to: usize, c: char, tag: Tag) {
        for at in from..to {
            self.put(at, c, tag);
        }
    }

    /// Writes `text` from display column `at`, giving each character as
    /// many cells as its display width, so markers computed with the same
    /// widths line up.
    fn put_str(&mut self, at: usize, text: &str, tag: Tag) {
        let mut col = at;
        for c in text.chars() {
            let width = char_width(c);
            if width == 0 {
                let previous = col.checked_sub(1).filter(|&p| p >= at).unwrap_or(col);
                if let Some(cell) = self.cell(previous) {
                    cell.text.push(c);
                }
                continue;
            }
            self.put(col, c, tag);
            for extra in 1..width {
                if let Some(cell) = self.cell(col.saturating_add(extra)) {
                    cell.text.clear();
                }
            }
            col = col.saturating_add(width);
        }
    }
}

/// Writes `gutter | row` with trailing whitespace trimmed. The gutter and
/// its `|` are in the gutter style, each cell in its own.
fn emit(out: &mut Painter, gutter: &str, row: &Row) {
    let body: String = row.cells.iter().map(|cell| cell.text.as_str()).collect();
    let line = format!("{gutter} | {body}");
    // The bytes to keep: the trimmed line, cut at a character boundary.
    let mut keep = line.trim_end().len();
    let rule = format!("{gutter} |");
    let parts = [(rule.as_str(), Tag::Gutter), (" ", Tag::Plain)]
        .into_iter()
        .chain(row.cells.iter().map(|cell| (cell.text.as_str(), cell.tag)));
    for (text, tag) in parts {
        if keep == 0 {
            break;
        }
        let take = text.len().min(keep);
        out.push(tag, text.get(..take).unwrap_or(text));
        keep = keep.saturating_sub(take);
    }
    out.newline();
}

#[cfg(test)]
mod tests;
