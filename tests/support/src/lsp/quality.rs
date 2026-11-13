//! The quality gate of plan W7 (`facts.md` §0.1, §10): how much of what
//! hover shows on clean code it still shows on code being edited.
//!
//! The mutants are `facts.md` §0.1's ten body edits, at the sites its
//! probe chose, found here from the syntax tree alone, since the harness
//! cannot reach the resolver: `dot` (`x.` after a local), `typo` (`z`
//! after a path), `open_call` (`foo(a, ` with the rest deleted),
//! `drop_arg`, `half_let`, `pipe_end`, `new_dot`, `open_case`, `mismatch`
//! (an integer literal replaced by `"s"`) and `drop_brace`. A local is a
//! one-segment lower-case path whose name some pattern or local function
//! of the same item binds: names are never shadowed (D-051), so that
//! name is the local. [`sample`] shuffles every site with a fixed seed
//! and keeps the first few of each kind, so a run is deterministic and a
//! drop in the numbers is a real change.
//!
//! What is measured goes through the real server. For every identifier of
//! the edited item whose clean hover shows a fully known type, the hover
//! on the mutated text is graded against the clean one ([`grade`]). What
//! is graded is every code block of the answer, joined ([`graded_text`]),
//! so an item's instantiated form counts. An answer whose only block is a
//! header built from interfaces, such as a non-generic item's signature,
//! is not graded, since no body edit can change it. **Same** is the share
//! of graded identifiers whose blocks are unchanged. Identifiers in the
//! replaced text are skipped.
//!
//! The **key probe** is what an editing feature needs at the cursor of
//! `dot`, `new_dot`, `pipe_end` and `open_call`: hover on the receiver,
//! the name just typed, the left side's last name or callee, and the
//! callee of the open call. It passes when the hover still shows a type
//! that is not wholly unknown ([`key_ok`]). For `open_call` it needs the
//! callee's call signature with no `_` parameter head ([`call_ok`]), and
//! asks only where the clean hover shows one: a generic item or a local. `facts.md` §0.1 asked the
//! checker's facts for the type of the whole expression; through the
//! protocol only names can be asked, so a `pipe_end` whose left side ends
//! in no name has no key probe.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use cheby_syntax::{SyntaxElement, SyntaxKind as K, SyntaxNode};
use serde_json::Value;

/// The seed of `facts.md` §0.1's shuffle.
pub const SEED: u64 = 0x9e37_79b9_7f4a_7c15;

/// The edits, in the order of `facts.md` §0.1's table.
pub const KINDS: [&str; 10] = [
    "dot",
    "new_dot",
    "open_call",
    "half_let",
    "open_case",
    "pipe_end",
    "drop_brace",
    "typo",
    "drop_arg",
    "mismatch",
];

/// The least share of same answers, in percent (plan W7).
pub const SAME_FLOOR: f64 = 90.0;

/// The least share of key probes that pass, in percent (plan W7).
pub const KEY_FLOOR: f64 = 95.0;

/// One edit of one file: `start..end` of the clean text replaced by
/// `insert`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mutant {
    /// One of [`KINDS`].
    pub kind: &'static str,
    /// The file, relative to its package.
    pub file: String,
    /// The start of the replaced range, a byte offset of the clean text.
    pub start: usize,
    /// The end of the replaced range.
    pub end: usize,
    /// The text put in its place.
    pub insert: String,
    /// The range of the edited item in the clean text.
    pub item: (usize, usize),
    /// Where the key probe asks, if this kind has one here.
    pub key: Option<KeyProbe>,
}

/// The identifier a key probe hovers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyProbe {
    /// Its start in the clean text, whose hover is the reference.
    pub clean: usize,
    /// Its start in the mutated text.
    pub mutated: usize,
}

impl Mutant {
    /// The mutated text, or `None` if the range does not split `text`.
    #[must_use]
    pub fn apply(&self, text: &str) -> Option<String> {
        Some(format!(
            "{}{}{}",
            text.get(..self.start)?,
            self.insert,
            text.get(self.end..)?
        ))
    }

    /// Where the clean offset `offset` is in the mutated text, or `None`
    /// for an offset in the replaced range.
    #[must_use]
    pub fn map(&self, offset: usize) -> Option<usize> {
        if offset < self.start {
            Some(offset)
        } else if offset < self.end {
            None
        } else {
            offset
                .checked_sub(self.end)?
                .checked_add(self.start)?
                .checked_add(self.insert.len())
        }
    }

    /// Whether `offset` of the clean text lies in the edited item.
    #[must_use]
    pub const fn in_item(&self, offset: usize) -> bool {
        self.item.0 <= offset && offset < self.item.1
    }
}

fn start_of(node: SyntaxNode<'_>) -> usize {
    usize::try_from(node.text_range().start()).unwrap_or(0)
}

fn end_of(node: SyntaxNode<'_>) -> usize {
    usize::try_from(node.text_range().end()).unwrap_or(0)
}

const fn is_ident(kind: K) -> bool {
    matches!(kind, K::LOWER_IDENT | K::UPPER_IDENT)
}

/// The start of the last identifier token of `node`.
fn last_ident(node: SyntaxNode<'_>) -> Option<usize> {
    let token = node.tokens().rev().find(|t| is_ident(t.kind()))?;
    usize::try_from(token.text_range().start()).ok()
}

/// The names that patterns and local functions of `item` bind.
fn binders(item: SyntaxNode<'_>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for n in item.descendants() {
        let own = match n.kind() {
            // `x`, `p as x`, a punned `{ x }`, `[..rest]`, `fn x(…)`.
            K::IDENT_PAT | K::AS_PAT | K::REST_PAT | K::LOCAL_FN => true,
            K::FIELD_PAT => n.first_token_of_kind(K::COLON).is_none(),
            _ => false,
        };
        if !own {
            continue;
        }
        let token = n.children_with_tokens().find_map(|e| match e {
            SyntaxElement::Token(t) if t.kind() == K::LOWER_IDENT => Some(t),
            _ => None,
        });
        if let Some(token) = token {
            names.insert(token.text().to_owned());
        }
    }
    names
}

/// `facts.md` §0.1's ten body edits of every function, constant and test
/// of `text`, the module at `file`, in source order.
#[must_use]
pub fn mutants(file: &str, text: &str) -> Vec<Mutant> {
    let parse = cheby_syntax::parse(cheby_diag::FileId::new(0), text.into());
    let root = parse.tree().root();
    let mut out = Vec::new();
    for item in root.children() {
        if !matches!(item.kind(), K::FN_DECL | K::CONST_DECL | K::TEST_DECL) {
            continue;
        }
        let range = (start_of(item), end_of(item));
        let locals = binders(item);
        let mut put = |kind, start, end, insert: &str, key| {
            out.push(Mutant {
                kind,
                file: file.to_owned(),
                start,
                end,
                insert: insert.to_owned(),
                item: range,
                key,
            });
        };
        for n in item.descendants() {
            let (ns, ne) = (start_of(n), end_of(n));
            match n.kind() {
                K::PATH_EXPR => {
                    let Some(path) = n.first_child_of_kind(K::PATH) else {
                        continue;
                    };
                    let segments: Vec<_> = path.tokens().filter(|t| is_ident(t.kind())).collect();
                    let is_local = matches!(segments.as_slice(), [one]
                        if one.kind() == K::LOWER_IDENT && locals.contains(one.text()));
                    let is_callee = n.parent().is_some_and(|p| {
                        p.kind() == K::CALL_EXPR
                            && p.first_child().map(SyntaxNode::id) == Some(n.id())
                    });
                    if is_local && !is_callee {
                        let key = last_ident(n).map(|at| KeyProbe {
                            clean: at,
                            mutated: at,
                        });
                        put("dot", ne, ne, ".", key);
                    }
                    if !segments.is_empty() {
                        put("typo", ne, ne, "z", None);
                    }
                }
                K::CALL_EXPR => {
                    let args: Vec<_> = n
                        .first_child_of_kind(K::ARG_LIST)
                        .map(|list| list.children().collect())
                        .unwrap_or_default();
                    if let Some(&first) = args.first() {
                        let key = n.first_child().and_then(last_ident).map(|at| KeyProbe {
                            clean: at,
                            mutated: at,
                        });
                        put("open_call", end_of(first), ne, ", ", key);
                    }
                    if let [.., prev, last] = args.as_slice() {
                        put("drop_arg", end_of(*prev), end_of(*last), "", None);
                    }
                }
                K::LET_STMT => {
                    put("half_let", ns, ns, "let probe_y = \n", None);
                    if let Some(value) = n.children().filter(|c| c.kind() != K::IDENT_PAT).last() {
                        let key = left_name(value).map(|at| KeyProbe {
                            clean: at,
                            mutated: at,
                        });
                        let ve = end_of(value);
                        put("pipe_end", ve, ve, " |> ", key);
                    }
                    if let Some(pat) = n.first_child_of_kind(K::IDENT_PAT) {
                        let name = pat.text().trim();
                        // The name typed on the new line, after "\n  ".
                        let key = last_ident(pat).map(|at| KeyProbe {
                            clean: at,
                            mutated: ne.saturating_add(3),
                        });
                        put("new_dot", ne, ne, &format!("\n  {name}."), key);
                        put("open_case", ne, ne, &format!("\n  case {name} {{"), None);
                    }
                }
                K::LITERAL => {
                    let digits = n.text().trim();
                    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                        put("mismatch", ns, ne, "\"s\"", None);
                    }
                }
                K::BLOCK => {
                    let is_body = n.parent().map(SyntaxNode::id) == Some(item.id());
                    if !is_body && n.text().ends_with('}') {
                        put("drop_brace", ne.saturating_sub(1), ne, "", None);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// The name a hover on the left side of `|>` asks at: the value's own
/// name, its field, or its callee's name. `None` for a value that ends in
/// no name, such as a literal or an operator.
fn left_name(value: SyntaxNode<'_>) -> Option<usize> {
    match value.kind() {
        K::PATH_EXPR | K::FIELD_EXPR => last_ident(value),
        K::CALL_EXPR => {
            let callee = value.first_child()?;
            matches!(callee.kind(), K::PATH_EXPR | K::FIELD_EXPR)
                .then(|| last_ident(callee))
                .flatten()
        }
        _ => None,
    }
}

/// Whether `text` has a grammar error, which `facts.md` §0.1's table
/// counts per kind.
#[must_use]
pub fn has_grammar_error(text: &str) -> bool {
    !cheby_syntax::parse(cheby_diag::FileId::new(0), text.into())
        .errors()
        .is_empty()
}

/// `facts.md` §0.1's xorshift generator.
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    const fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let bound = u64::try_from(i).unwrap_or(u64::MAX).saturating_add(1);
            let j = usize::try_from(self.next().checked_rem(bound).unwrap_or(0)).unwrap_or(0);
            items.swap(i, j);
        }
    }
}

/// `items` shuffled with [`SEED`], keeping the first `per_kind` of each
/// kind, in shuffled order.
#[must_use]
pub fn sample<T>(mut items: Vec<T>, per_kind: usize, kind: impl Fn(&T) -> &'static str) -> Vec<T> {
    Rng(SEED).shuffle(&mut items);
    let mut taken: BTreeMap<&'static str, usize> = BTreeMap::new();
    items
        .into_iter()
        .filter(|item| {
            let count = taken.entry(kind(item)).or_default();
            *count = count.saturating_add(1);
            *count <= per_kind
        })
        .collect()
}

/// What a hover answer shows: the text of its first code block, or for
/// an answer with none, its first paragraph. `None` for `null` or an
/// empty answer.
#[must_use]
pub fn shown(hover: &Value) -> Option<String> {
    let contents = hover.get("contents")?;
    let text = match contents {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            return items
                .first()
                .and_then(|first| shown(&serde_json::json!({ "contents": first })));
        }
        Value::Object(map) if map.contains_key("language") => {
            return map
                .get("value")
                .and_then(Value::as_str)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty());
        }
        Value::Object(map) => map.get("value")?.as_str()?.to_owned(),
        _ => return None,
    };
    let block = match text.split_once("```") {
        Some((_, rest)) => {
            let body = rest.split_once('\n').map_or("", |(_, body)| body);
            body.split_once("```").map_or(body, |(inside, _)| inside)
        }
        None => text.split("\n---").next().unwrap_or_default(),
    };
    let block = block.trim();
    (!block.is_empty()).then(|| block.to_owned())
}

/// The separator [`graded_text`] puts between code blocks. No block holds
/// a blank line.
pub const BLOCK_SEP: &str = "\n\n";

/// The code blocks of a hover answer, in order: the fenced blocks of a
/// Markdown answer before its first `---`, or each `MarkedString` with a
/// language. A plain-text answer is one block, its first paragraph.
#[must_use]
pub fn blocks(hover: &Value) -> Vec<String> {
    let Some(contents) = hover.get("contents") else {
        return Vec::new();
    };
    let text = match contents {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            return items
                .iter()
                .flat_map(|item| blocks(&serde_json::json!({ "contents": item })))
                .collect();
        }
        Value::Object(map) if map.contains_key("language") => {
            return map
                .get("value")
                .and_then(Value::as_str)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
                .into_iter()
                .collect();
        }
        Value::Object(map) => match map.get("value").and_then(Value::as_str) {
            Some(value) => value.to_owned(),
            None => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    let head = text.split("\n---").next().unwrap_or_default();
    if !head.contains("```") {
        let block = head.trim();
        return (!block.is_empty())
            .then(|| block.to_owned())
            .into_iter()
            .collect();
    }
    let mut out = Vec::new();
    let mut rest = head;
    while let Some((_, open)) = rest.split_once("```") {
        let body = open.split_once('\n').map_or("", |(_, body)| body);
        let Some((inside, after)) = body.split_once("```") else {
            break;
        };
        let inside = inside.trim();
        if !inside.is_empty() {
            out.push(inside.to_owned());
        }
        rest = after;
    }
    out
}

/// Whether a code block shows what the checker's facts say: a local
/// (`let x: Int`, `(parameter) xs: List<T>`, `use v: T`, `fn f: fn(Int)
/// -> Int`) or a field (`radius: Float`). Anything else is a header built
/// from interfaces, such as an item's signature, a module's import or a
/// type parameter's bounds, which no body edit can change.
#[must_use]
pub fn is_facts_block(block: &str) -> bool {
    let rest = ["let ", "(parameter) ", "use ", "fn "]
        .iter()
        .find_map(|prefix| block.strip_prefix(prefix))
        .unwrap_or(block);
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    let (name, after) = rest.split_at(end);
    name.starts_with(|c: char| c.is_lowercase() || c == '_')
        && (after.is_empty() || after.starts_with(": "))
}

/// What the quality gate grades in a hover answer (`facts.md` §0.1, which
/// graded the type from the facts): every code block, joined by
/// [`BLOCK_SEP`]. `None` for no answer and for one whose only block is a
/// header, such as a non-generic item's signature: it comes from
/// interfaces, so it would count as same however much of the facts the
/// edit lost.
#[must_use]
pub fn graded_text(hover: &Value) -> Option<String> {
    let blocks = blocks(hover);
    match blocks.as_slice() {
        [] => None,
        [only] if !is_facts_block(only) => None,
        _ => Some(blocks.join(BLOCK_SEP)),
    }
}

/// The parameter types of the call signature in graded text `shown`: the
/// instantiated `fn(…) -> …` block of an item, or the `fn(…)` type of a
/// local. `None` when it shows no call signature.
#[must_use]
pub fn call_params(shown: &str) -> Option<Vec<String>> {
    let blocks: Vec<&str> = shown.split(BLOCK_SEP).collect();
    let sig = match blocks.as_slice() {
        [local] if is_facts_block(local) => local.split_once(": ")?.1,
        [_, instantiated, ..] => instantiated,
        _ => return None,
    };
    let inside = sig.strip_prefix("fn(")?;
    let mut depth = 0_u32;
    let mut params = Vec::new();
    let mut current = String::new();
    for c in inside.chars() {
        // The `>` of `->` closes nothing.
        let arrow = c == '>' && current.ends_with('-');
        match c {
            _ if arrow => {}
            '(' | '<' | '[' | '{' => depth = depth.saturating_add(1),
            ')' if depth == 0 => {
                let last = current.trim();
                if !last.is_empty() {
                    params.push(last.to_owned());
                }
                return Some(params);
            }
            ')' | '>' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                params.push(current.trim().to_owned());
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    None
}

/// Whether the `open_call` key probe passes (`facts.md` §0.1: "a
/// `CallFact` with no `_` parameter head"): the callee still shows its
/// call signature, and no parameter's type is `_` at the head.
#[must_use]
pub fn call_ok(got: Option<&str>) -> bool {
    got.and_then(call_params).is_some_and(|params| {
        params
            .iter()
            .all(|p| p != "_" && !p.starts_with("_<") && !p.starts_with("_ "))
    })
}

/// Whether `shown` holds a `_` that stands alone, an unknown type or part
/// of one, rather than inside a name such as `probe_y`.
#[must_use]
pub fn has_hole(shown: &str) -> bool {
    let chars: Vec<char> = shown.chars().collect();
    let word = |c: Option<&char>| c.is_some_and(|c| c.is_alphanumeric() || *c == '_');
    chars.iter().enumerate().any(|(i, c)| {
        *c == '_'
            && !word(i.checked_sub(1).and_then(|p| chars.get(p)))
            && !word(chars.get(i.saturating_add(1)))
    })
}

/// How one identifier's hover on mutated text compares with its clean one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Grade {
    /// The same block.
    Same,
    /// A block with an unknown part.
    Partial,
    /// Another fully known block, often right, since the edit changed the
    /// code.
    Other,
    /// No answer, or one with no block.
    Missing,
}

/// Grades `got` against the clean block `clean`.
#[must_use]
pub fn grade(clean: &str, got: Option<&str>) -> Grade {
    match got {
        Some(got) if got == clean => Grade::Same,
        Some(got) if has_hole(got) => Grade::Partial,
        Some(_) => Grade::Other,
        None => Grade::Missing,
    }
}

/// Whether a key probe passes: the hover shows a type that is not wholly
/// unknown. A block whose type is `_` fails, and so does one that lost
/// the `: T` the clean `reference` had, since hover shows a local whose
/// type is all unknown by its name alone (`features.md` §2.1).
#[must_use]
pub fn key_ok(reference: Option<&str>, got: Option<&str>) -> bool {
    let Some(got) = got else {
        return false;
    };
    let typed = |s: &str| s.contains(": ");
    if reference.is_some_and(typed) && !typed(got) {
        return false;
    }
    got.rsplit_once(": ").is_none_or(|(_, ty)| ty.trim() != "_") && got.trim() != "_"
}

/// The counts of one kind of edit, or of all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    /// Mutants run.
    pub mutants: u32,
    /// Mutants with a grammar error.
    pub grammar: u32,
    /// Graded identifiers, by grade.
    pub same: u32,
    /// See [`Grade::Partial`].
    pub partial: u32,
    /// See [`Grade::Other`].
    pub other: u32,
    /// See [`Grade::Missing`].
    pub missing: u32,
    /// Graded identifiers before the edit, and those of them the same.
    pub before: u32,
    /// See [`Tally::before`].
    pub before_same: u32,
    /// Graded identifiers after the edit, and those of them the same.
    pub after: u32,
    /// See [`Tally::after`].
    pub after_same: u32,
    /// Key probes asked, and those that passed.
    pub keys: u32,
    /// See [`Tally::keys`].
    pub keys_ok: u32,
}

impl Tally {
    /// Counts one graded identifier, before or after the edit.
    pub fn grade(&mut self, grade: Grade, before: bool) {
        let slot = match grade {
            Grade::Same => &mut self.same,
            Grade::Partial => &mut self.partial,
            Grade::Other => &mut self.other,
            Grade::Missing => &mut self.missing,
        };
        *slot = slot.saturating_add(1);
        let same = u32::from(grade == Grade::Same);
        if before {
            self.before = self.before.saturating_add(1);
            self.before_same = self.before_same.saturating_add(same);
        } else {
            self.after = self.after.saturating_add(1);
            self.after_same = self.after_same.saturating_add(same);
        }
    }

    /// Counts one key probe.
    pub fn key(&mut self, ok: bool) {
        self.keys = self.keys.saturating_add(1);
        self.keys_ok = self.keys_ok.saturating_add(u32::from(ok));
    }

    /// Adds `other` to this tally.
    pub const fn add(&mut self, other: &Self) {
        self.mutants = self.mutants.saturating_add(other.mutants);
        self.grammar = self.grammar.saturating_add(other.grammar);
        self.same = self.same.saturating_add(other.same);
        self.partial = self.partial.saturating_add(other.partial);
        self.other = self.other.saturating_add(other.other);
        self.missing = self.missing.saturating_add(other.missing);
        self.before = self.before.saturating_add(other.before);
        self.before_same = self.before_same.saturating_add(other.before_same);
        self.after = self.after.saturating_add(other.after);
        self.after_same = self.after_same.saturating_add(other.after_same);
        self.keys = self.keys.saturating_add(other.keys);
        self.keys_ok = self.keys_ok.saturating_add(other.keys_ok);
    }

    /// Graded identifiers.
    #[must_use]
    pub const fn graded(&self) -> u32 {
        self.same
            .saturating_add(self.partial)
            .saturating_add(self.other)
            .saturating_add(self.missing)
    }

    /// The share of same answers, in percent; 100 when nothing is graded.
    #[must_use]
    pub fn same_pct(&self) -> f64 {
        pct(self.same, self.graded())
    }

    /// The share of key probes that passed, in percent; 100 when none ran.
    #[must_use]
    pub fn key_pct(&self) -> f64 {
        pct(self.keys_ok, self.keys)
    }
}

fn pct(part: u32, whole: u32) -> f64 {
    if whole == 0 {
        100.0
    } else {
        100.0 * f64::from(part) / f64::from(whole)
    }
}

/// The tallies of a run, by kind.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    /// Each kind's tally.
    pub by_kind: BTreeMap<&'static str, Tally>,
}

impl Report {
    /// The tally of `kind`, created empty.
    pub fn kind(&mut self, kind: &'static str) -> &mut Tally {
        self.by_kind.entry(kind).or_default()
    }

    /// Adds every tally of `other`.
    pub fn merge(&mut self, other: &Self) {
        for (kind, tally) in &other.by_kind {
            self.kind(kind).add(tally);
        }
    }

    /// The tally of all kinds.
    #[must_use]
    pub fn total(&self) -> Tally {
        let mut total = Tally::default();
        for tally in self.by_kind.values() {
            total.add(tally);
        }
        total
    }

    /// The table of `facts.md` §0.1: one row per kind, then the total.
    #[must_use]
    pub fn table(&self) -> String {
        let mut out = String::from(
            "| Edit       | Mutants | Grammar | Graded | Same   | Partial | Other | Missing | Before | After  | Key     |\n",
        );
        let mut row = |name: &str, t: &Tally| {
            let key = if t.keys == 0 {
                String::new()
            } else {
                format!("{}/{}", t.keys_ok, t.keys)
            };
            let _ = writeln!(
                out,
                "| {name:<10} | {:>7} | {:>7} | {:>6} | {:>5.1}% | {:>7} | {:>5} | {:>7} | {:>5.1}% | {:>5.1}% | {key:>7} |",
                t.mutants,
                t.grammar,
                t.graded(),
                t.same_pct(),
                t.partial,
                t.other,
                t.missing,
                pct(t.before_same, t.before),
                pct(t.after_same, t.after),
            );
        };
        for kind in KINDS {
            if let Some(tally) = self.by_kind.get(kind) {
                row(kind, tally);
            }
        }
        row("all", &self.total());
        out
    }

    /// `Ok` when the total meets both floors, else the shortfall.
    ///
    /// # Errors
    ///
    /// Returns a message naming each floor the total misses.
    pub fn verdict(&self) -> Result<(), String> {
        let total = self.total();
        let mut misses = Vec::new();
        if total.graded() == 0 {
            misses.push("no identifier was graded".to_owned());
        }
        if total.same_pct() < SAME_FLOOR {
            misses.push(format!(
                "same is {:.1}%, below {SAME_FLOOR}%",
                total.same_pct()
            ));
        }
        if total.key_pct() < KEY_FLOOR {
            misses.push(format!(
                "the key probe is {:.1}%, below {KEY_FLOOR}%",
                total.key_pct()
            ));
        }
        if misses.is_empty() {
            Ok(())
        } else {
            Err(misses.join("; "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TEXT: &str = "fn main() {\n  let total = add(1, 2)\n  let s = { total }\n  io::println(\"{s}\")\n}\n\nfn add(a: Int, b: Int) -> Int {\n  a + b\n}\n";

    fn kinds(found: &[Mutant]) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for m in found {
            let n: &mut usize = counts.entry(m.kind).or_default();
            *n = n.saturating_add(1);
        }
        counts
    }

    #[test]
    fn sites_of_every_kind() {
        let found = mutants("src/main.cheby", TEXT);
        let counts = kinds(&found);
        assert_eq!(counts.get("dot"), Some(&3), "{found:#?}");
        assert_eq!(counts.get("new_dot"), Some(&2));
        assert_eq!(counts.get("open_call"), Some(&2));
        assert_eq!(counts.get("drop_arg"), Some(&1));
        assert_eq!(counts.get("mismatch"), Some(&2));
        assert_eq!(counts.get("drop_brace"), Some(&1));
        for m in &found {
            let mutated = m.apply(TEXT).unwrap();
            assert_ne!(mutated, TEXT, "{m:?}");
            if let Some(key) = m.key {
                let clean = TEXT.get(key.clean..).unwrap();
                let name: String = clean.chars().take_while(|c| c.is_alphanumeric()).collect();
                assert!(
                    mutated.get(key.mutated..).unwrap().starts_with(&name),
                    "{m:?}: {name}"
                );
            }
        }
        let dot = found.iter().find(|m| m.kind == "dot").unwrap();
        assert_eq!(
            TEXT.get(dot.start.saturating_sub(5)..dot.start),
            Some("total")
        );
    }

    #[test]
    fn callee_and_items_are_not_locals() {
        // `add` is called, `io::println` is qualified: no `dot` on them.
        let found = mutants("src/main.cheby", "fn main() {\n  add(io::x)\n}\n");
        assert!(found.iter().all(|m| m.kind != "dot"), "{found:#?}");
    }

    #[test]
    fn mapping_skips_the_replaced_range() {
        let m = Mutant {
            kind: "drop_arg",
            file: String::new(),
            start: 10,
            end: 14,
            insert: "xy".to_owned(),
            item: (0, 100),
            key: None,
        };
        assert_eq!(m.map(9), Some(9));
        assert_eq!(m.map(10), None);
        assert_eq!(m.map(13), None);
        assert_eq!(m.map(14), Some(12));
    }

    #[test]
    fn sample_is_deterministic_and_capped() {
        let items: Vec<(&'static str, u32)> = (0..100)
            .map(|n| (if n % 2 == 0 { "a" } else { "b" }, n))
            .collect();
        let one = sample(items.clone(), 7, |i| i.0);
        assert_eq!(one, sample(items, 7, |i| i.0));
        assert_eq!(one.len(), 14);
        assert_ne!(
            one.iter().map(|i| i.1).collect::<Vec<_>>(),
            (0..14).collect::<Vec<_>>()
        );
    }

    #[test]
    fn shown_blocks() {
        let markdown = json!({ "contents": { "kind": "markdown",
            "value": "```cheby\nlet total: Int\n```\n---\nA note" } });
        assert_eq!(shown(&markdown).as_deref(), Some("let total: Int"));
        let plain = json!({ "contents": { "kind": "plaintext", "value": "x: Int\n---\nnote" } });
        assert_eq!(shown(&plain).as_deref(), Some("x: Int"));
        let marked = json!({ "contents": [{ "language": "cheby", "value": "fn f() -> Int" }] });
        assert_eq!(shown(&marked).as_deref(), Some("fn f() -> Int"));
        assert_eq!(shown(&Value::Null), None);
    }

    #[test]
    fn graded_text_skips_headers() {
        let answer = |value: &str| json!({ "contents": { "kind": "markdown", "value": value } });
        let header = answer("```cheby\npub fn add(a: Int) -> Int\n```\n\n---\n\nDefined in `app`.");
        assert_eq!(graded_text(&header), None);
        let generic = answer(
            "```cheby\nfn first<T>(xs: List<T>) -> T\n```\n```cheby\nfn(List<Int>) -> Int\n```\n\n---\n\n```cheby\nfirst(xs)\n```",
        );
        assert_eq!(
            graded_text(&generic).as_deref(),
            Some("fn first<T>(xs: List<T>) -> T\n\nfn(List<Int>) -> Int")
        );
        for local in [
            "let x: Int",
            "(parameter) xs: List<T>",
            "fn go: fn(Int) -> Int",
            "radius: Float",
            "let y",
        ] {
            assert!(is_facts_block(local), "{local}");
        }
        for item in [
            "fn twice(Self) -> Self",
            "type Point",
            "import std::list",
            "T: Show",
            "Point { x: Int }\nof type Point",
            "const N: Int = 3",
        ] {
            assert!(!is_facts_block(item), "{item}");
        }
    }

    #[test]
    fn call_probe_reads_parameter_heads() {
        let generic = "fn map<T, U>(xs: List<T>, f: fn(T) -> U) -> List<U>\n\nfn(List<Int>, fn(Int) -> String) -> List<String>";
        assert_eq!(
            call_params(generic),
            Some(vec!["List<Int>".to_owned(), "fn(Int) -> String".to_owned()])
        );
        assert!(call_ok(Some(generic)));
        assert!(call_ok(Some(
            "let go: fn(Map<Int, fn(Int) -> Int>, Int) -> Int"
        )));
        assert!(call_ok(Some(
            "fn f<T>(x: T) -> T\n\nfn(List<_>) -> List<_>"
        )));
        assert!(!call_ok(Some("fn f<T>(x: T) -> T\n\nfn(_) -> _")));
        assert!(!call_ok(Some(
            "fn f<T>(x: T, y: Int) -> T\n\nfn(Int, _) -> _"
        )));
        assert!(!call_ok(Some("pub fn add(a: Int) -> Int")));
        assert!(!call_ok(Some("let go")));
        assert!(!call_ok(None));
    }

    #[test]
    fn grades_and_keys() {
        assert!(has_hole("List<_>"));
        assert!(has_hole("_"));
        assert!(!has_hole("let probe_y: Int"));
        assert_eq!(grade("x: Int", Some("x: Int")), Grade::Same);
        assert_eq!(grade("x: Int", Some("x: List<_>")), Grade::Partial);
        assert_eq!(grade("x: Int", Some("x: String")), Grade::Other);
        assert_eq!(grade("x: Int", None), Grade::Missing);
        assert!(key_ok(Some("let x: Int"), Some("let x: List<_>")));
        assert!(!key_ok(Some("let x: Int"), Some("let x")));
        assert!(!key_ok(Some("let x: Int"), Some("let x: _")));
        assert!(!key_ok(Some("let x: Int"), None));
    }

    #[test]
    fn verdict_floors() {
        let mut report = Report::default();
        let tally = report.kind("dot");
        tally.mutants = 1;
        for _ in 0..9 {
            tally.grade(Grade::Same, true);
        }
        tally.grade(Grade::Missing, false);
        tally.key(true);
        assert!(report.verdict().is_ok(), "{}", report.table());
        report.kind("typo").grade(Grade::Other, true);
        assert!(report.verdict().is_err());
        assert!(report.table().contains("| all "));
    }
}
