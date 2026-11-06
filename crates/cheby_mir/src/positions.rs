//! Source positions (D-938): the line and column of the construct each MIR
//! statement and terminator lowers, which code generation turns into
//! Cranelift source locations, code records into rows and `cheby_link`
//! into DWARF line tables.
//!
//! A function keeps the positions it uses in a table ([`Positions`]),
//! deduplicated in first-use order like its strings, and each block one
//! [`PosRef`] per statement and one for its terminator
//! ([`crate::Block::pos`]); the unit names its source file once
//! ([`crate::MirModule::source`]). They are side tables, as the call
//! locations of D-856 are: validation checks their shape, the dumps never
//! print them (D-1171) and code generation only sets
//! source locations from them, so the code and every dump are the same
//! with and without them.
//!
//! Which position (D-1170): lowering gives each statement and
//! terminator the position of the start of the innermost expression being
//! lowered when it is emitted (`BodyCx::expr`): a call's own statement
//! gets the call expression's, after its arguments got theirs; an
//! interpolation's value part and its `show` call and append get the
//! part's; a `case`'s shared tests get the `case`'s, and the code that
//! binds an arm's names the arm's first pattern's. A function's entry
//! (its prologue and stack check) gets its name's position
//! ([`Positions::entry`]), and a function lowering generates (a wrapper,
//! a shim, an intrinsic body) gets its entry position everywhere when its
//! span lies in the unit's file. Statements later passes make carry the
//! position of the statement they belong to: a count before or after a
//! statement or terminator takes its position, a block's leading drops
//! the block's first position, an edge's drops the branch's, and an
//! unwind pad the position of the first site that unwinds to it.
//! Positions are only recorded for spans in the unit's own file, since a
//! line table names one file (D-1172).
//!
//! With positions off (`CHEBY_POSITIONS=off`, [`crate::MirOptions::positions`],
//! D-1173) a unit holds none: every table and every
//! block's list is empty and its source is `None`.

use cheby_iface::Ice;

use crate::ir::{Block, Function, MirModule, Stmt};

/// A position in a source file (D-421): a line and a column counted from
/// 1, the column in Unicode code points.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Pos {
    pub line: u32,
    pub column: u32,
}

/// A position of a function's table ([`Positions::table`]): 0 for none,
/// `n` for the table's `n`-th entry. Code generation uses its bits as the
/// Cranelift source location, so no table is needed there.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct PosRef(u32);

impl PosRef {
    /// No position.
    pub const NONE: Self = Self(0);

    /// The reference with these bits ([`PosRef::bits`]).
    #[must_use]
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Its bits: 0 for none, else the table's entry number from 1.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether this is [`PosRef::NONE`].
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// The index into [`Positions::table`], `None` for none.
    fn index(self) -> Option<usize> {
        usize::try_from(self.0.checked_sub(1)?).ok()
    }
}

/// A function's positions (D-938): the table its blocks' [`PosRef`]s
/// index, and the position of its entry.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Positions {
    /// Each position the function uses, once, in first-use order.
    pub table: Vec<Pos>,
    /// The position of the function's entry: its name's, for its
    /// prologue and stack check.
    pub entry: PosRef,
}

impl Positions {
    /// The position `r` names, `None` for none or one outside the table.
    #[must_use]
    pub fn get(&self, r: PosRef) -> Option<Pos> {
        self.table.get(r.index()?).copied()
    }

    /// Whether the function holds no position.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }
}

/// Interning of positions into a function's table, for lowering.
#[derive(Debug, Default)]
pub(crate) struct PosTable {
    table: Vec<Pos>,
    ids: rustc_hash::FxHashMap<Pos, PosRef>,
}

impl PosTable {
    /// The reference of `pos`, added on first use.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the table is full.
    pub(crate) fn intern(&mut self, pos: Pos) -> Result<PosRef, Ice> {
        if let Some(&r) = self.ids.get(&pos) {
            return Ok(r);
        }
        let bits = u32::try_from(self.table.len())
            .ok()
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| Ice::new("too many source positions in one function"))?;
        self.table.push(pos);
        let r = PosRef(bits);
        self.ids.insert(pos, r);
        Ok(r)
    }

    /// The position `r` names.
    pub(crate) fn get(&self, r: PosRef) -> Option<Pos> {
        self.table.get(r.index()?).copied()
    }

    /// The finished table.
    pub(crate) fn finish(self, entry: PosRef) -> Positions {
        Positions {
            table: self.table,
            entry,
        }
    }
}

/// Statements being rebuilt by a pass, with their positions when the
/// block has them ([`Block::pos`]).
#[derive(Debug)]
pub(crate) struct StmtsOut {
    stmts: Vec<Stmt>,
    pos: Vec<PosRef>,
    keep: bool,
}

impl StmtsOut {
    /// An empty list, keeping positions when `keep` (the block had them).
    pub(crate) fn new(keep: bool, capacity: usize) -> Self {
        Self {
            stmts: Vec::with_capacity(capacity),
            pos: if keep {
                Vec::with_capacity(capacity.saturating_add(1))
            } else {
                Vec::new()
            },
            keep,
        }
    }

    /// Appends `stmt` at `pos`.
    pub(crate) fn push(&mut self, stmt: Stmt, pos: PosRef) {
        self.stmts.push(stmt);
        if self.keep {
            self.pos.push(pos);
        }
    }

    /// Appends each of `stmts` at `pos`.
    pub(crate) fn extend(&mut self, stmts: impl IntoIterator<Item = Stmt>, pos: PosRef) {
        for stmt in stmts {
            self.push(stmt, pos);
        }
    }

    /// Moves every statement of `pairs`, each with its position, to the
    /// end, leaving `pairs` empty.
    pub(crate) fn append(&mut self, pairs: &mut Vec<(Stmt, PosRef)>) {
        for (stmt, pos) in pairs.drain(..) {
            self.push(stmt, pos);
        }
    }

    /// The statements and, when kept, their positions followed by the
    /// terminator's `term`, as [`Block::pos`] holds them.
    pub(crate) fn finish(self, term: PosRef) -> (Vec<Stmt>, Vec<PosRef>) {
        let mut pos = self.pos;
        if self.keep {
            pos.push(term);
        }
        (self.stmts, pos)
    }
}

impl Block {
    /// Whether the block keeps positions: [`Block::pos`] then holds one
    /// per statement and one for the terminator.
    #[must_use]
    pub fn has_positions(&self) -> bool {
        !self.pos.is_empty()
    }

    /// The position of statement `i`, or of the terminator when `i` is
    /// the number of statements; none when the block keeps none.
    #[must_use]
    pub fn pos_at(&self, i: usize) -> PosRef {
        self.pos.get(i).copied().unwrap_or(PosRef::NONE)
    }

    /// The position of the terminator.
    #[must_use]
    pub fn term_pos(&self) -> PosRef {
        self.pos_at(self.stmts.len())
    }

    /// The position of the block's first statement, or of its terminator
    /// when it has none: what a drop at the block's start takes.
    #[must_use]
    pub fn first_pos(&self) -> PosRef {
        self.pos_at(0)
    }

    /// Keeps the statements `keep` accepts, with their positions.
    pub fn retain_stmts(&mut self, mut keep: impl FnMut(&Stmt) -> bool) {
        if !self.has_positions() {
            self.stmts.retain(keep);
            return;
        }
        let term = self.term_pos();
        let stmts = std::mem::take(&mut self.stmts);
        let pos = std::mem::take(&mut self.pos);
        let mut out = StmtsOut::new(true, stmts.len());
        for (stmt, p) in stmts.into_iter().zip(pos) {
            if keep(&stmt) {
                out.push(stmt, p);
            }
        }
        (self.stmts, self.pos) = out.finish(term);
    }

    /// Removes statement `i` with its position; `None` if there is none.
    pub fn remove_stmt(&mut self, i: usize) -> Option<Stmt> {
        if i >= self.stmts.len() {
            return None;
        }
        if self.has_positions() && i < self.pos.len() {
            self.pos.remove(i);
        }
        Some(self.stmts.remove(i))
    }

    /// Inserts `stmts` before statement `at` (at the end when `at` is the
    /// number of statements), each at `pos`.
    pub fn insert_stmts(&mut self, at: usize, stmts: impl IntoIterator<Item = Stmt>, pos: PosRef) {
        let at = at.min(self.stmts.len());
        let new: Vec<Stmt> = stmts.into_iter().collect();
        if self.has_positions() {
            let at_pos = at.min(self.pos.len());
            self.pos
                .splice(at_pos..at_pos, std::iter::repeat_n(pos, new.len()));
        }
        self.stmts.splice(at..at, new);
    }

    /// Replaces the statements and their positions with `stmts` and
    /// `pos`, as [`StmtsOut::finish`] gives them.
    pub(crate) fn set_stmts(&mut self, (stmts, pos): (Vec<Stmt>, Vec<PosRef>)) {
        self.stmts = stmts;
        self.pos = pos;
    }
}

/// Drops every position of `module`: its source, each function's table
/// and each block's list, as with positions off (D-1173).
pub fn clear(module: &mut MirModule) {
    module.source = None;
    for f in &mut module.functions {
        clear_function(f);
    }
}

/// Drops every position of `f`.
pub fn clear_function(f: &mut Function) {
    f.positions = Positions::default();
    for block in &mut f.blocks {
        block.pos = Vec::new();
    }
}

/// Checks the shape of `f`'s positions: each block keeps none or one per
/// statement and one for its terminator, and every reference, the entry's
/// included, names an entry of the function's table.
///
/// # Errors
///
/// A message naming the first block that breaks the shape.
pub(crate) fn check(f: &Function) -> Result<(), String> {
    let len = f.positions.table.len();
    let in_table = |r: PosRef| r.index().is_none_or(|i| i < len);
    if !in_table(f.positions.entry) {
        return Err("the entry position is outside the function's table".into());
    }
    for (id, block) in f.blocks.iter_enumerated() {
        if block.pos.is_empty() {
            continue;
        }
        if block.pos.len() != block.stmts.len().saturating_add(1) {
            return Err(format!(
                "{id:?} has {} positions for {} statements and a terminator",
                block.pos.len(),
                block.stmts.len()
            ));
        }
        if let Some(bad) = block.pos.iter().find(|&&r| !in_table(r)) {
            return Err(format!(
                "{id:?} has the position {} outside the function's table of {len}",
                bad.bits()
            ));
        }
    }
    Ok(())
}
