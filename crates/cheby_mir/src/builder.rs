//! Building one MIR function block by block.

use cheby_diag::Span;
use cheby_iface::{Ice, IndexVec, Name, Ty};
use rustc_hash::FxHashMap;

use crate::ir::{
    Abi, Block, BlockId, FnKey, Function, Local, LocalDecl, Operand, RuntimeFn, Rvalue, Stmt,
    StrId, Terminator,
};
use crate::positions::{Pos, PosRef, PosTable};
use crate::repr::Repr;

/// A block whose terminator may still be missing.
#[derive(Debug, Default)]
struct OpenBlock {
    params: Vec<Local>,
    stmts: Vec<Stmt>,
    term: Option<Terminator>,
    /// Sorted by statement index, at most one per index ([`Block::unwind`]).
    unwind: Vec<(u32, BlockId)>,
    /// The position of each statement, when the builder records them
    /// ([`FunctionBuilder::set_record_positions`]).
    pos: Vec<PosRef>,
    /// The terminator's position.
    term_pos: PosRef,
}

impl OpenBlock {
    /// Gives statement `i` (the terminator when `i == stmts.len()`) the
    /// unwind edge `pad`, keeping the edges sorted.
    fn add_unwind(&mut self, i: usize, pad: BlockId) -> Result<(), Ice> {
        let at = u32::try_from(i).map_err(|_| Ice::new("too many MIR statements in a block"))?;
        match self.unwind.binary_search_by_key(&at, |&(n, _)| n) {
            Ok(_) => Err(Ice::new(format!(
                "statement {i} already has an unwind edge"
            ))),
            Err(pos) => {
                self.unwind.insert(pos, (at, pad));
                Ok(())
            }
        }
    }
}

/// Builds a [`Function`]. Statements go to the current block; a block is
/// closed by [`FunctionBuilder::terminate`]. Every method that could fail
/// on a broken invariant returns an [`Ice`].
#[derive(Debug)]
pub struct FunctionBuilder {
    key: FnKey,
    abi: Abi,
    ret: Repr,
    span: Span,
    params: Vec<Local>,
    locals: IndexVec<Local, LocalDecl>,
    blocks: IndexVec<BlockId, OpenBlock>,
    strings: IndexVec<StrId, Box<str>>,
    /// Lookup only; numbering is first-use order.
    string_ids: FxHashMap<Box<str>, StrId>,
    entry_loc: Option<StrId>,
    entry_unwind: Option<BlockId>,
    precounted: bool,
    current: BlockId,
    /// The statements pushed so far, over every block.
    stmts: usize,
    /// Whether [`FunctionBuilder::record_site`] records anything
    /// ([`crate::MirOptions::record_sites`]).
    record_sites: bool,
    /// The sites recorded so far, which [`FunctionBuilder::finish`] moves
    /// into [`Function::sites`].
    sites: Vec<(Local, Span)>,
    /// Whether [`FunctionBuilder::record_call_loc`] records anything: on
    /// in units outside package `std` (D-856).
    record_call_locs: bool,
    /// The call locations recorded so far ([`Function::call_locs`]).
    call_locs: Vec<(Local, StrId)>,
    /// Their texts ([`Function::call_loc_strings`]).
    call_loc_strings: IndexVec<StrId, Box<str>>,
    /// Lookup only; numbering is first-use order.
    call_loc_ids: FxHashMap<Box<str>, StrId>,
    /// The positions recorded so far ([`Function::positions`], D-938), or
    /// `None` when the builder records none.
    positions: Option<PosTable>,
    /// The position the next statement or terminator takes.
    current_pos: PosRef,
    /// The position of the function's entry.
    entry_pos: PosRef,
}

impl FunctionBuilder {
    /// A function with an empty entry block `bb0` as the current block.
    ///
    /// # Errors
    ///
    /// Never in practice; an [`Ice`] if the first block cannot be created.
    pub fn new(key: FnKey, abi: Abi, ret: Repr, span: Span) -> Result<Self, Ice> {
        let mut blocks = IndexVec::new();
        let current = blocks.push(OpenBlock::default())?;
        Ok(Self {
            key,
            abi,
            ret,
            span,
            params: Vec::new(),
            locals: IndexVec::new(),
            blocks,
            strings: IndexVec::new(),
            string_ids: FxHashMap::default(),
            entry_loc: None,
            entry_unwind: None,
            precounted: false,
            current,
            stmts: 0,
            record_sites: false,
            sites: Vec::new(),
            record_call_locs: false,
            call_locs: Vec::new(),
            call_loc_strings: IndexVec::new(),
            call_loc_ids: FxHashMap::default(),
            positions: None,
            current_pos: PosRef::NONE,
            entry_pos: PosRef::NONE,
        })
    }

    /// Makes the builder record source positions (D-938): from now on
    /// every statement and terminator takes the current position
    /// ([`FunctionBuilder::set_position`]). Off by default; lowering turns
    /// it on for the bodies of a unit that records positions
    /// ([`crate::MirOptions::positions`]). Call it before the first
    /// statement.
    pub fn set_record_positions(&mut self) {
        if self.positions.is_none() {
            self.positions = Some(PosTable::default());
        }
    }

    /// Whether the builder records positions.
    #[must_use]
    pub const fn records_positions(&self) -> bool {
        self.positions.is_some()
    }

    /// Sets the position of the function's entry (its name's), which is
    /// also the current position until another is set. A no-op unless
    /// the builder records positions.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the function's table of positions is full.
    pub fn set_entry_position(&mut self, pos: Pos) -> Result<(), Ice> {
        if let Some(table) = &mut self.positions {
            self.entry_pos = table.intern(pos)?;
            self.current_pos = self.entry_pos;
        }
        Ok(())
    }

    /// Makes `pos` the position of the statements and terminators that
    /// follow, and returns the one it replaces, for
    /// [`FunctionBuilder::restore_position`]: a position, not a reference
    /// into this function's table, since lowering may continue in another
    /// function (D-780) before it restores. A no-op returning `None`
    /// unless the builder records positions.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the function's table of positions is full.
    pub fn set_position(&mut self, pos: Pos) -> Result<Option<Pos>, Ice> {
        let Some(table) = &mut self.positions else {
            return Ok(None);
        };
        let previous = table.get(self.current_pos);
        self.current_pos = table.intern(pos)?;
        Ok(previous)
    }

    /// Makes `previous`, which [`FunctionBuilder::set_position`] returned,
    /// the current position again (none for `None`).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the function's table of positions is full.
    pub fn restore_position(&mut self, previous: Option<Pos>) -> Result<(), Ice> {
        let Some(table) = &mut self.positions else {
            return Ok(());
        };
        self.current_pos = match previous {
            Some(pos) => table.intern(pos)?,
            None => PosRef::NONE,
        };
        Ok(())
    }

    /// The current position, if the builder records positions and has
    /// one.
    #[must_use]
    pub fn position(&self) -> Option<Pos> {
        self.positions.as_ref()?.get(self.current_pos)
    }

    /// Makes [`FunctionBuilder::record_call_loc`] record (`on`) or ignore
    /// its locations; off by default. Lowering turns it on in units
    /// outside package `std` (D-856).
    pub const fn set_record_call_locs(&mut self, on: bool) {
        self.record_call_locs = on;
    }

    /// Whether [`FunctionBuilder::record_call_loc`] records.
    #[must_use]
    pub const fn records_call_locs(&self) -> bool {
        self.record_call_locs
    }

    /// Records `location` (`path:line:col`) as the location of the call
    /// that defines `local` (D-856): a no-op unless
    /// [`FunctionBuilder::set_record_call_locs`] turned recording on.
    /// [`FunctionBuilder::finish`] moves the locations into
    /// [`Function::call_locs`] and [`Function::call_loc_strings`].
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the table of location strings is full.
    pub fn record_call_loc(&mut self, local: Local, location: &str) -> Result<(), Ice> {
        if !self.record_call_locs {
            return Ok(());
        }
        let id = if let Some(&id) = self.call_loc_ids.get(location) {
            id
        } else {
            let id = self.call_loc_strings.push(location.into())?;
            self.call_loc_ids.insert(location.into(), id);
            id
        };
        self.call_locs.push((local, id));
        Ok(())
    }

    /// Makes [`FunctionBuilder::record_site`] record (`on`) or ignore its
    /// sites; off by default. Lowering sets it from
    /// [`crate::MirOptions::record_sites`], which `--reuse-report` sets
    /// (D-717).
    pub const fn set_record_sites(&mut self, on: bool) {
        self.record_sites = on;
    }

    /// Whether [`FunctionBuilder::record_site`] records.
    #[must_use]
    pub const fn records_sites(&self) -> bool {
        self.record_sites
    }

    /// Records `span` as the source site of `local`, the result of an
    /// `alloc` or a call, for the reuse report (D-717): a no-op unless
    /// [`FunctionBuilder::set_record_sites`] turned recording on.
    /// [`FunctionBuilder::finish`] moves the sites into
    /// [`Function::sites`], in the order recorded.
    pub fn record_site(&mut self, local: Local, span: Span) {
        if self.record_sites {
            self.sites.push((local, span));
        }
    }

    /// The size of the function so far: its statements and its blocks
    /// (each block's terminator). Lowering splits a large literal by it
    /// (D-780).
    #[must_use]
    pub fn size(&self) -> usize {
        self.stmts.saturating_add(self.blocks.len())
    }

    /// The function's key.
    #[must_use]
    pub const fn key(&self) -> FnKey {
        self.key
    }

    /// The function's result representation.
    #[must_use]
    pub const fn ret(&self) -> Repr {
        self.ret
    }

    /// The function's ABI.
    #[must_use]
    pub const fn abi(&self) -> Abi {
        self.abi
    }

    /// A new local, not yet defined.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the function has too many locals.
    pub fn new_local(
        &mut self,
        repr: Repr,
        name: Option<Name>,
        ty: Option<Ty>,
    ) -> Result<Local, Ice> {
        self.locals.push(LocalDecl { repr, name, ty })
    }

    /// The declaration of `local`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] for an unknown local.
    pub fn local(&self, local: Local) -> Result<&LocalDecl, Ice> {
        self.locals.try_get(local, "MIR local")
    }

    /// Sets the source name of `local` if it has none. Returns whether it
    /// was set.
    ///
    /// # Errors
    ///
    /// An [`Ice`] for an unknown local.
    pub fn name_local(&mut self, local: Local, name: Name, ty: Ty) -> Result<bool, Ice> {
        let decl = self
            .locals
            .get_mut(local)
            .ok_or_else(|| Ice::new("naming an unknown MIR local"))?;
        if decl.name.is_some() {
            return Ok(false);
        }
        decl.name = Some(name);
        decl.ty = Some(ty);
        Ok(true)
    }

    /// Appends a function parameter.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the function has too many locals.
    pub fn add_param(
        &mut self,
        repr: Repr,
        name: Option<Name>,
        ty: Option<Ty>,
    ) -> Result<Local, Ice> {
        let local = self.new_local(repr, name, ty)?;
        self.params.push(local);
        Ok(local)
    }

    /// A new empty block. The current block does not change.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the function has too many blocks.
    pub fn new_block(&mut self) -> Result<BlockId, Ice> {
        self.blocks.push(OpenBlock::default())
    }

    /// Appends a parameter to `block`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] for an unknown block or too many locals.
    pub fn add_block_param(&mut self, block: BlockId, repr: Repr) -> Result<Local, Ice> {
        let local = self.new_local(repr, None, None)?;
        self.open(block)?.params.push(local);
        Ok(local)
    }

    /// Makes `block` the current block.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `block` is unknown or already terminated.
    pub fn switch_to(&mut self, block: BlockId) -> Result<(), Ice> {
        if self.open(block)?.term.is_some() {
            return Err(Ice::new(format!("{block:?} is already terminated")));
        }
        self.current = block;
        Ok(())
    }

    /// The current block.
    #[must_use]
    pub const fn current(&self) -> BlockId {
        self.current
    }

    /// Whether the current block is already terminated (code after a
    /// tail call or a panic is unreachable).
    #[must_use]
    pub fn is_terminated(&self) -> bool {
        self.blocks
            .get(self.current)
            .is_none_or(|b| b.term.is_some())
    }

    fn open(&mut self, block: BlockId) -> Result<&mut OpenBlock, Ice> {
        self.blocks
            .get_mut(block)
            .ok_or_else(|| Ice::new(format!("no MIR block {block:?}")))
    }

    /// Appends a statement to the current block.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the current block is terminated.
    pub fn push(&mut self, stmt: Stmt) -> Result<(), Ice> {
        let current = self.current;
        let (records, pos) = (self.positions.is_some(), self.current_pos);
        let block = self.open(current)?;
        if block.term.is_some() {
            return Err(Ice::new(format!("statement after the end of {current:?}")));
        }
        block.stmts.push(stmt);
        if records {
            block.pos.push(pos);
        }
        self.stmts = self.stmts.saturating_add(1);
        Ok(())
    }

    /// Defines a new local of `repr` as `value` in the current block.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the current block is terminated.
    pub fn assign(&mut self, repr: Repr, value: Rvalue) -> Result<Local, Ice> {
        let dst = self.new_local(repr, None, None)?;
        self.push(Stmt::Assign { dst, value })?;
        Ok(dst)
    }

    /// Calls the runtime helper `func` in the current block: a new local
    /// of its result representation, or `None` for a helper without a
    /// result, which is evaluated for effect ([`RuntimeFn::ret`]).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the current block is terminated.
    pub fn call_runtime(
        &mut self,
        func: RuntimeFn,
        args: Vec<Operand>,
    ) -> Result<Option<Local>, Ice> {
        let value = Rvalue::CallRuntime { func, args };
        match func.ret() {
            Some(repr) => self.assign(repr, value).map(Some),
            None => self.push(Stmt::Eval(value)).map(|()| None),
        }
    }

    /// Ends the current block.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the current block is already terminated.
    pub fn terminate(&mut self, term: Terminator) -> Result<(), Ice> {
        let current = self.current;
        let pos = self.current_pos;
        let block = self.open(current)?;
        if block.term.is_some() {
            return Err(Ice::new(format!("{current:?} terminated twice")));
        }
        block.term = Some(term);
        block.term_pos = pos;
        Ok(())
    }

    /// Gives the statement pushed last in the current block the unwind
    /// edge `pad` (D-698).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the current block has no statement, is already
    /// terminated, or its last statement already has an edge.
    pub fn unwind_last(&mut self, pad: BlockId) -> Result<(), Ice> {
        let current = self.current;
        let block = self.open(current)?;
        if block.term.is_some() {
            return Err(Ice::new(format!(
                "an unwind edge for a statement of {current:?}, which is terminated"
            )));
        }
        let Some(last) = block.stmts.len().checked_sub(1) else {
            return Err(Ice::new(format!(
                "an unwind edge in {current:?}, which has no statement"
            )));
        };
        block.add_unwind(last, pad)
    }

    /// Gives the terminator of `block`, once set, the unwind edge `pad`
    /// (D-698).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `block` is unknown, not yet terminated, or its
    /// terminator already has an edge.
    pub fn unwind_terminator(&mut self, block: BlockId, pad: BlockId) -> Result<(), Ice> {
        let open = self.open(block)?;
        if open.term.is_none() {
            return Err(Ice::new(format!(
                "an unwind edge for the terminator of {block:?}, which has none yet"
            )));
        }
        let at = open.stmts.len();
        open.add_unwind(at, pad)
    }

    /// Sets the pad the function-entry check unwinds to (D-698).
    pub const fn set_entry_unwind(&mut self, pad: BlockId) {
        self.entry_unwind = Some(pad);
    }

    /// Marks the body as generated with its counts in place (D-693):
    /// the RC pass leaves it alone (D-744).
    pub const fn set_precounted(&mut self) {
        self.precounted = true;
    }

    /// Pushes [`Stmt::ClearUnwind`] (D-693).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the current block is terminated.
    pub fn clear_unwind(&mut self) -> Result<(), Ice> {
        self.push(Stmt::ClearUnwind)
    }

    /// The id of `text` in the string table, adding it on first use.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the table is full.
    pub fn string(&mut self, text: &str) -> Result<StrId, Ice> {
        if let Some(&id) = self.string_ids.get(text) {
            return Ok(id);
        }
        let id = self.strings.push(text.into())?;
        self.string_ids.insert(text.into(), id);
        Ok(id)
    }

    /// Sets the location of the function-entry stack check. Call it before
    /// adding other strings, so it is `str#0`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the table is full.
    pub fn set_entry_loc(&mut self, location: &str) -> Result<(), Ice> {
        self.entry_loc = Some(self.string(location)?);
        Ok(())
    }

    /// The finished function.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a block has no terminator.
    pub fn finish(self) -> Result<Function, Ice> {
        let records = self.positions.is_some();
        let mut blocks = IndexVec::new();
        for (id, open) in self.blocks.into_raw().into_iter().enumerate() {
            let term = open
                .term
                .ok_or_else(|| Ice::new(format!("MIR block bb{id} has no terminator")))?;
            let mut pos = open.pos;
            if records {
                pos.push(open.term_pos);
            }
            blocks.push(Block {
                params: open.params,
                stmts: open.stmts,
                term,
                unwind: open.unwind,
                pos,
            })?;
        }
        let positions = self
            .positions
            .map(|table| table.finish(self.entry_pos))
            .unwrap_or_default();
        Ok(Function {
            key: self.key,
            abi: self.abi,
            params: self.params,
            ret: self.ret,
            locals: self.locals,
            blocks,
            span: self.span,
            strings: self.strings,
            entry_loc: self.entry_loc,
            entry_unwind: self.entry_unwind,
            precounted: self.precounted,
            borrow: crate::borrow::BorrowMask::OWNED,
            sites: self.sites,
            call_locs: self.call_locs,
            call_loc_strings: self.call_loc_strings,
            positions,
        })
    }
}
