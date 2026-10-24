//! Code records (D-914): what code generation defines for one unit, kept
//! apart from any object format, so the cache stores it and `cheby_link`
//! replays it into an object (D-915).
//!
//! A [`RecordingModule`] is a `cranelift_module::Module` that captures
//! every `define_function_bytes` and `define_data` instead of installing
//! them. [`RecordingModule::finish`] gives the [`CodeRecord`]:
//!
//! - the symbols in declaration order, each with its name, kind and
//!   linkage; a symbol's index in this table is its only id;
//! - each function defined, in definition order: its symbol, alignment,
//!   machine code and relocations, each naming its target by symbol index;
//! - each data object defined, in definition order: its symbol,
//!   alignment, bytes or zero-filled size, and its pointer-sized absolute
//!   relocations;
//! - the unit's source positions (D-938, D-914): the unit's source path
//!   once, D-421's diagnostic path (`src/main.cheby`, `std/io.cheby`),
//!   and each function's rows ([`crate::SourceRow`]), from which
//!   `cheby_link` writes DWARF line tables when it replays the record
//!   (D-915). Both are empty with positions off (`CHEBY_POSITIONS=off`,
//!   D-1173).
//!
//! Nothing in a record is an address, an absolute path, a hash-map order
//! or an id other than a declaration index, so its bytes follow from the
//! order in which code generation declares and defines (D-264); a unit
//! compiled from two package paths gives one record.
//!
//! # Encoding
//!
//! [`CodeRecord::encode`] writes, little-endian, with no padding:
//!
//! | Field     | Encoding                                                                                              |
//! | --------- | ----------------------------------------------------------------------------------------------------- |
//! | magic     | the 4 bytes `cbcr`                                                                                    |
//! | version   | `u32`, [`RECORD_VERSION`]                                                                             |
//! | source    | 0 for none, or 1 then the path (`u32` length, UTF-8 bytes)                                            |
//! | symbols   | `u32` count, then each: kind `u8`, linkage `u8`, name (`u32` length, UTF-8 bytes)                     |
//! | functions | `u32` count, then each: symbol `u32`, align `u64`, code (`u32` length, bytes), relocations, positions |
//! | data      | `u32` count, then each: symbol `u32`, align `u64` (0: none), contents, relocations                    |
//!
//! A symbol's kind is 0 for a function, 1 for read-only data and 2 for
//! writable data; its linkage 0 `Local`, 1 `Hidden`, 2 `Import`, 3
//! `Export`. A function's relocations are a `u32` count, then each:
//! offset `u32`, kind `u8` ([`RECORD_RELOCS`]), symbol `u32`, addend
//! `i64`. Its positions are a `u32` count of rows, then each row as three
//! unsigned LEB128 numbers, relative to the row before (the first to
//! offset 0, line 1, column 1): the offset's increase (at least 1 after
//! the first row), the line's change zigzag-encoded (`2n` for an increase
//! of `n`, `2n - 1` for a decrease of `n`), and the column itself. A data
//! object's contents are 0 then its bytes (`u32` length, bytes), or 1
//! then the zero-filled size (`u64`); its relocations are a `u32` count,
//! then each: offset `u32`, symbol `u32`, addend `i64`, every one an
//! absolute pointer-sized word. Nothing follows the data.
//!
//! Version 2 (D-938) added the source and the positions; version 1, which
//! had neither, is refused like every other ([`RecordError::Version`]).
//!
//! [`CodeRecord::decode`] reads exactly this form and never panics
//! (D-422): every length and count is checked against the bytes that
//! remain, nothing is preallocated from a count, and the result is checked
//! as [`RecordingModule::finish`] checks a fresh record, so
//! `encode(decode(b)) == b` for every `b` it accepts.

use std::fmt;

use cranelift_codegen::binemit::Reloc;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir;
use cranelift_codegen::isa::{OwnedTargetIsa, TargetIsa};
use cranelift_module::{
    DataDescription, DataId, FuncId, Init, Linkage, Module, ModuleDeclarations, ModuleError,
    ModuleReloc, ModuleRelocTarget, ModuleResult,
};

use crate::{CodegenError, SourceRow};

/// The version of [`CodeRecord`]'s encoding. A record of another version
/// is refused, never read. Version 2 holds the source positions of D-938.
pub const RECORD_VERSION: u32 = 2;

/// The first bytes of an encoded record.
const MAGIC: [u8; 4] = *b"cbcr";

/// The relocation kinds a record holds, with their codes in the encoding:
/// the PC-relative kinds D-913's code model leaves in text on x86-64 and
/// aarch64, and `Abs8`, the pointer-sized absolute kind.
pub const RECORD_RELOCS: [(u8, Reloc); 6] = [
    (1, Reloc::Abs8),
    (2, Reloc::X86PCRel4),
    (3, Reloc::X86CallPCRel4),
    (4, Reloc::Arm64Call),
    (5, Reloc::Aarch64AdrPrelPgHi21),
    (6, Reloc::Aarch64AddAbsLo12Nc),
];

/// The bytes a data relocation writes: one 64-bit word.
const POINTER_BYTES: u64 = 8;

/// The largest alignment a record's function or data object may ask for:
/// 64 KiB, the largest page size of the targets M3 writes. Code generation
/// asks for far less ([`crate::FUNCTION_ALIGN`] for code, 8 bytes for
/// data). An object writer pads its section up to the alignment, so a
/// larger one, which only a corrupt record holds, is refused by
/// [`CodeRecord::check`] instead of reaching the writer, where it would
/// panic or abort the process on the allocation (D-422).
pub const MAX_ALIGN: u64 = 1 << 16;

/// What a symbol names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolKind {
    /// Code.
    Function,
    /// A data object, writable or read-only.
    Data {
        /// Whether the object is writable.
        writable: bool,
    },
}

/// A symbol's linkage, as `cranelift_module::Linkage` has it, without
/// `Preemptible`, which code generation never declares (D-917).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolLinkage {
    /// Defined by the record, visible only inside its object.
    Local,
    /// Defined by the record, global with hidden visibility (D-917).
    Hidden,
    /// Defined elsewhere.
    Import,
    /// Defined by the record and exported (only `main`, in the program
    /// object).
    Export,
}

impl SymbolLinkage {
    /// The `cranelift_module` linkage.
    #[must_use]
    pub const fn linkage(self) -> Linkage {
        match self {
            Self::Local => Linkage::Local,
            Self::Hidden => Linkage::Hidden,
            Self::Import => Linkage::Import,
            Self::Export => Linkage::Export,
        }
    }

    /// The record's linkage for `linkage`, `None` for `Preemptible`.
    #[must_use]
    pub const fn from_linkage(linkage: Linkage) -> Option<Self> {
        match linkage {
            Linkage::Local => Some(Self::Local),
            Linkage::Hidden => Some(Self::Hidden),
            Linkage::Import => Some(Self::Import),
            Linkage::Export => Some(Self::Export),
            Linkage::Preemptible => None,
        }
    }

    /// Whether the record defines a symbol of this linkage.
    #[must_use]
    pub const fn is_defined(self) -> bool {
        !matches!(self, Self::Import)
    }

    const fn code(self) -> u8 {
        match self {
            Self::Local => 0,
            Self::Hidden => 1,
            Self::Import => 2,
            Self::Export => 3,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Local),
            1 => Some(Self::Hidden),
            2 => Some(Self::Import),
            3 => Some(Self::Export),
            _ => None,
        }
    }
}

impl SymbolKind {
    const fn code(self) -> u8 {
        match self {
            Self::Function => 0,
            Self::Data { writable: false } => 1,
            Self::Data { writable: true } => 2,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Function),
            1 => Some(Self::Data { writable: false }),
            2 => Some(Self::Data { writable: true }),
            _ => None,
        }
    }
}

/// One symbol of a record's table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordSymbol {
    /// Its name, as code generation declared it (D-456).
    pub name: String,
    /// Function or data.
    pub kind: SymbolKind,
    /// Its linkage once every declaration merged (D-917).
    pub linkage: SymbolLinkage,
}

/// A relocation in a function's code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordReloc {
    /// Its offset in the code.
    pub offset: u32,
    /// Its kind, one of [`RECORD_RELOCS`].
    pub kind: Reloc,
    /// The target, an index into [`CodeRecord::symbols`].
    pub symbol: u32,
    /// The addend.
    pub addend: i64,
}

/// One function a record defines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordFunction {
    /// The function's symbol, an index into [`CodeRecord::symbols`].
    pub symbol: u32,
    /// Its alignment in bytes, a power of two.
    pub align: u64,
    /// Its machine code.
    pub bytes: Vec<u8>,
    /// Its relocations, in the order code generation gave them.
    pub relocs: Vec<RecordReloc>,
    /// Its source rows (D-938), sorted by strictly increasing offset, each
    /// inside the code, lines and columns from 1, in the record's
    /// [`CodeRecord::source`]; empty with positions off and for code
    /// without positions.
    pub positions: Vec<SourceRow>,
}

/// A data object's contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordContents {
    /// These bytes.
    Bytes(Vec<u8>),
    /// This many zero bytes. A zero-filled object has no relocations: one
    /// with relocations is recorded as [`RecordContents::Bytes`] of zeros,
    /// since object formats keep zero-filled sections out of the file and
    /// cannot relocate into them.
    Zeros(u64),
}

impl RecordContents {
    /// The size in bytes.
    #[must_use]
    pub fn size(&self) -> u64 {
        match self {
            Self::Bytes(bytes) => u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            Self::Zeros(size) => *size,
        }
    }
}

/// A relocation in a data object: an absolute pointer-sized word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordDataReloc {
    /// Its offset in the object.
    pub offset: u32,
    /// The target, an index into [`CodeRecord::symbols`].
    pub symbol: u32,
    /// The addend.
    pub addend: i64,
}

/// One data object a record defines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordData {
    /// The object's symbol, an index into [`CodeRecord::symbols`].
    pub symbol: u32,
    /// Its alignment in bytes, a power of two, or `None` for the module's
    /// default.
    pub align: Option<u64>,
    /// Its contents.
    pub contents: RecordContents,
    /// Its relocations: the function relocations of its
    /// `DataDescription`, then the data relocations, as
    /// `DataDescription::all_relocs` orders them.
    pub relocs: Vec<RecordDataReloc>,
}

/// One unit's code record (D-914).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeRecord {
    /// Every symbol declared, in declaration order; its index is the
    /// symbol's id in the record.
    pub symbols: Vec<RecordSymbol>,
    /// The functions defined, in definition order.
    pub functions: Vec<RecordFunction>,
    /// The data objects defined, in definition order.
    pub data: Vec<RecordData>,
    /// The unit's source path, D-421's diagnostic path (`src/main.cheby`,
    /// `std/io.cheby`), relative and `/`-separated, never absolute
    /// (D-938); `None` with positions off and for units without source
    /// (the entries unit). A record whose functions have rows names one.
    pub source: Option<String>,
}

/// Why a record could not be made, read or replayed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordError {
    /// The bytes or the record are not well formed.
    Malformed(&'static str),
    /// The encoding has another version.
    Version(u32),
    /// A symbol of linkage `Local`, `Hidden` or `Export` was never defined.
    Undefined(String),
    /// Code generation asked for something a record does not hold.
    Unsupported(String),
    /// Code generation failed while recording (`CodegenError` is
    /// `Clone + Eq`).
    Codegen(CodegenError),
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "a malformed code record: {what}"),
            Self::Version(found) => write!(
                f,
                "a code record of version {found}, expected {RECORD_VERSION}"
            ),
            Self::Undefined(name) => {
                write!(f, "the code record declares `{name}` but never defines it")
            }
            Self::Unsupported(what) => write!(f, "a code record cannot hold {what}"),
            Self::Codegen(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for RecordError {}

impl From<CodegenError> for RecordError {
    fn from(error: CodegenError) -> Self {
        Self::Codegen(error)
    }
}

/// A `cranelift_module::Module` that captures what code generation
/// defines (D-914), for [`RecordingModule::finish`].
pub struct RecordingModule {
    isa: OwnedTargetIsa,
    declarations: ModuleDeclarations,
    /// Every symbol, in declaration order.
    symbols: Vec<RecordSymbol>,
    /// Whether each symbol is defined, by symbol index.
    defined: Vec<bool>,
    /// The symbol index of each `FuncId`, by its index.
    funcs: Vec<u32>,
    /// The symbol index of each `DataId`, by its index.
    datas: Vec<u32>,
    functions: Vec<RecordFunction>,
    data: Vec<RecordData>,
    /// The first request a record cannot hold; [`RecordingModule::finish`]
    /// returns it.
    failed: Option<String>,
    /// The record's [`CodeRecord::source`].
    source: Option<String>,
}

impl fmt::Debug for RecordingModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordingModule")
            .field("isa", &self.isa.triple())
            .field("symbols", &self.symbols.len())
            .field("functions", &self.functions.len())
            .field("data", &self.data.len())
            .finish_non_exhaustive()
    }
}

impl RecordingModule {
    /// An empty recording module for `isa`.
    #[must_use]
    pub fn new(isa: OwnedTargetIsa) -> Self {
        Self {
            isa,
            declarations: ModuleDeclarations::default(),
            symbols: Vec::new(),
            defined: Vec::new(),
            funcs: Vec::new(),
            datas: Vec::new(),
            functions: Vec::new(),
            data: Vec::new(),
            failed: None,
            source: None,
        }
    }

    /// Names the unit's source file (D-421's diagnostic path) in the
    /// record ([`CodeRecord::source`]).
    pub fn set_source(&mut self, source: Option<&str>) {
        self.source = source.map(str::to_owned);
    }

    /// Gives the function `func`, defined already, its source rows
    /// (D-938), as [`crate::DefinedCode::positions`] holds them.
    ///
    /// # Errors
    ///
    /// [`RecordError::Malformed`] if `func` is not a function the record
    /// defines.
    pub fn set_positions(&mut self, func: FuncId, rows: Vec<SourceRow>) -> Result<(), RecordError> {
        let symbol = usize::try_from(func.as_u32())
            .ok()
            .and_then(|i| self.funcs.get(i))
            .copied()
            .ok_or(RecordError::Malformed(
                "positions of an undeclared function",
            ))?;
        let function = self
            .functions
            .iter_mut()
            .find(|f| f.symbol == symbol)
            .ok_or(RecordError::Malformed("positions of an undefined function"))?;
        function.positions = rows;
        Ok(())
    }

    /// The record, symbols in declaration order.
    ///
    /// # Errors
    ///
    /// [`RecordError::Undefined`] if a declared symbol of linkage `Local`,
    /// `Hidden` or `Export` was never defined, and
    /// [`RecordError::Unsupported`] if code generation asked for something
    /// a record does not hold.
    pub fn finish(self) -> Result<CodeRecord, RecordError> {
        if let Some(what) = self.failed {
            return Err(RecordError::Unsupported(what));
        }
        let record = CodeRecord {
            symbols: self.symbols,
            functions: self.functions,
            data: self.data,
            source: self.source,
        };
        record.check()?;
        Ok(record)
    }

    /// Records `what` as the first unsupported request and returns the
    /// module error code generation sees.
    fn unsupported(&mut self, what: String) -> ModuleError {
        let error = ModuleError::Compilation(cranelift_codegen::CodegenError::Unsupported(
            format!("a code record cannot hold {what}"),
        ));
        self.failed.get_or_insert(what);
        error
    }

    /// Adds `name` to the symbol table if `id` (a `FuncId` or `DataId`
    /// index into `ids`) is new, or updates its merged linkage and kind.
    #[expect(
        clippy::result_large_err,
        reason = "the error is the `Module` trait's, which its methods return as is"
    )]
    fn note_symbol(
        &mut self,
        is_function: bool,
        id: u32,
        name: &str,
        kind: SymbolKind,
        linkage: Linkage,
    ) -> ModuleResult<()> {
        let Some(linkage) = SymbolLinkage::from_linkage(linkage) else {
            return Err(self.unsupported(format!("the preemptible symbol `{name}`")));
        };
        let Ok(id) = usize::try_from(id) else {
            return Err(self.unsupported("4 Gi symbols".into()));
        };
        let ids = if is_function {
            &self.funcs
        } else {
            &self.datas
        };
        let known = ids.len();
        if let Some(&index) = ids.get(id) {
            let symbol = usize::try_from(index)
                .ok()
                .and_then(|index| self.symbols.get_mut(index))
                .ok_or_else(|| ModuleError::Undeclared(name.to_owned()))?;
            symbol.linkage = linkage;
            if let (SymbolKind::Data { writable: old }, SymbolKind::Data { writable: new }) =
                (symbol.kind, kind)
            {
                symbol.kind = SymbolKind::Data {
                    writable: old || new,
                };
            }
            return Ok(());
        }
        if id != known {
            return Err(ModuleError::Undeclared(name.to_owned()));
        }
        let Ok(index) = u32::try_from(self.symbols.len()) else {
            return Err(self.unsupported("4 Gi symbols".into()));
        };
        self.symbols.push(RecordSymbol {
            name: name.to_owned(),
            kind,
            linkage,
        });
        self.defined.push(false);
        if is_function {
            self.funcs.push(index);
        } else {
            self.datas.push(index);
        }
        Ok(())
    }

    /// The symbol index of a relocation's target.
    fn target(&self, target: &ModuleRelocTarget) -> Option<u32> {
        match *target {
            ModuleRelocTarget::User {
                namespace: 0,
                index,
            } => self.funcs.get(usize::try_from(index).ok()?).copied(),
            ModuleRelocTarget::User {
                namespace: 1,
                index,
            } => self.datas.get(usize::try_from(index).ok()?).copied(),
            _ => None,
        }
    }

    /// Marks `symbol` defined, refusing an import or a second definition.
    #[expect(
        clippy::result_large_err,
        reason = "the error is the `Module` trait's, which its methods return as is"
    )]
    fn define_symbol(&mut self, symbol: u32) -> ModuleResult<()> {
        let index = usize::try_from(symbol).ok();
        let Some(entry) = index.and_then(|i| self.symbols.get(i)) else {
            return Err(ModuleError::Undeclared(format!("symbol {symbol}")));
        };
        if !entry.linkage.is_defined() {
            return Err(ModuleError::InvalidImportDefinition(entry.name.clone()));
        }
        let name = entry.name.clone();
        match index.and_then(|i| self.defined.get_mut(i)) {
            Some(defined) if !*defined => {
                *defined = true;
                Ok(())
            }
            _ => Err(ModuleError::DuplicateDefinition(name)),
        }
    }

    fn symbol_name(&self, symbol: u32) -> String {
        usize::try_from(symbol)
            .ok()
            .and_then(|i| self.symbols.get(i))
            .map_or_else(|| format!("symbol {symbol}"), |s| s.name.clone())
    }
}

impl Module for RecordingModule {
    fn isa(&self) -> &dyn TargetIsa {
        &*self.isa
    }

    fn declarations(&self) -> &ModuleDeclarations {
        &self.declarations
    }

    fn declare_function(
        &mut self,
        name: &str,
        linkage: Linkage,
        signature: &ir::Signature,
    ) -> ModuleResult<FuncId> {
        if linkage == Linkage::Preemptible {
            return Err(self.unsupported(format!("the preemptible symbol `{name}`")));
        }
        let (id, merged) = self
            .declarations
            .declare_function(name, linkage, signature)?;
        self.note_symbol(true, id.as_u32(), name, SymbolKind::Function, merged)?;
        Ok(id)
    }

    /// A record names every symbol; code generation declares none
    /// anonymously.
    fn declare_anonymous_function(&mut self, _signature: &ir::Signature) -> ModuleResult<FuncId> {
        Err(self.unsupported("an anonymous function".into()))
    }

    fn declare_data(
        &mut self,
        name: &str,
        linkage: Linkage,
        writable: bool,
        tls: bool,
    ) -> ModuleResult<DataId> {
        // Refused before `ModuleDeclarations` sees it, which panics when a
        // name changes between TLS and normal data.
        if tls {
            return Err(self.unsupported(format!("the thread-local object `{name}`")));
        }
        if linkage == Linkage::Preemptible {
            return Err(self.unsupported(format!("the preemptible symbol `{name}`")));
        }
        let (id, merged) = self
            .declarations
            .declare_data(name, linkage, writable, false)?;
        self.note_symbol(
            false,
            id.as_u32(),
            name,
            SymbolKind::Data { writable },
            merged,
        )?;
        Ok(id)
    }

    /// A record names every symbol; code generation declares none
    /// anonymously.
    fn declare_anonymous_data(&mut self, _writable: bool, _tls: bool) -> ModuleResult<DataId> {
        Err(self.unsupported("an anonymous data object".into()))
    }

    /// Code generation defines bytes (`define_with_stats`); a record never
    /// compiles.
    fn define_function_with_control_plane(
        &mut self,
        func: FuncId,
        _ctx: &mut cranelift_codegen::Context,
        _ctrl_plane: &mut ControlPlane,
    ) -> ModuleResult<()> {
        let name = self
            .funcs
            .get(usize::try_from(func.as_u32()).unwrap_or(usize::MAX))
            .map_or_else(|| func.to_string(), |&s| self.symbol_name(s));
        Err(self.unsupported(format!("a function compiled by the module (`{name}`)")))
    }

    fn define_function_bytes(
        &mut self,
        func: FuncId,
        alignment: u64,
        bytes: &[u8],
        relocs: &[ModuleReloc],
    ) -> ModuleResult<()> {
        let Some(&symbol) = self
            .funcs
            .get(usize::try_from(func.as_u32()).unwrap_or(usize::MAX))
        else {
            return Err(ModuleError::Undeclared(func.to_string()));
        };
        let mut recorded = Vec::with_capacity(relocs.len());
        for reloc in relocs {
            let Some(target) = self.target(&reloc.name) else {
                let what = format!(
                    "the relocation to `{}` in `{}`",
                    reloc.name,
                    self.symbol_name(symbol)
                );
                return Err(self.unsupported(what));
            };
            if reloc_code(reloc.kind).is_none() {
                let what = format!(
                    "the `{:?}` relocation in `{}`",
                    reloc.kind,
                    self.symbol_name(symbol)
                );
                return Err(self.unsupported(what));
            }
            recorded.push(RecordReloc {
                offset: reloc.offset,
                kind: reloc.kind,
                symbol: target,
                addend: reloc.addend,
            });
        }
        self.define_symbol(symbol)?;
        self.functions.push(RecordFunction {
            symbol,
            align: alignment,
            bytes: bytes.to_vec(),
            relocs: recorded,
            positions: Vec::new(),
        });
        Ok(())
    }

    fn define_data(&mut self, data: DataId, desc: &DataDescription) -> ModuleResult<()> {
        let Some(&symbol) = self
            .datas
            .get(usize::try_from(data.as_u32()).unwrap_or(usize::MAX))
        else {
            return Err(ModuleError::Undeclared(data.to_string()));
        };
        let name = self.symbol_name(symbol);
        if desc.custom_section.is_some() || desc.used {
            return Err(self.unsupported(format!("the custom section or `used` flag of `{name}`")));
        }
        let mut relocs = Vec::with_capacity(
            desc.function_relocs
                .len()
                .saturating_add(desc.data_relocs.len()),
        );
        for &(offset, func_ref) in &desc.function_relocs {
            let target = desc
                .function_decls
                .get(func_ref)
                .and_then(|target| self.target(target));
            let Some(target) = target else {
                return Err(self.unsupported(format!("a function relocation in `{name}`")));
            };
            relocs.push(RecordDataReloc {
                offset,
                symbol: target,
                addend: 0,
            });
        }
        for &(offset, gv, addend) in &desc.data_relocs {
            let target = desc
                .data_decls
                .get(gv)
                .and_then(|target| self.target(target));
            let Some(target) = target else {
                return Err(self.unsupported(format!("a data relocation in `{name}`")));
            };
            relocs.push(RecordDataReloc {
                offset,
                symbol: target,
                addend,
            });
        }
        let contents = match &desc.init {
            Init::Uninitialized => {
                return Err(self.unsupported(format!("the uninitialized object `{name}`")));
            }
            Init::Bytes { contents } => RecordContents::Bytes(contents.to_vec()),
            // An object format cannot relocate into zero-fill: an object
            // with relocations keeps its zeros as bytes.
            Init::Zeros { size } if !relocs.is_empty() => RecordContents::Bytes(vec![0; *size]),
            Init::Zeros { size } => RecordContents::Zeros(
                u64::try_from(*size)
                    .map_err(|_| self.unsupported(format!("the size of `{name}`")))?,
            ),
        };
        self.define_symbol(symbol)?;
        self.data.push(RecordData {
            symbol,
            align: desc.align,
            contents,
            relocs,
        });
        Ok(())
    }
}

/// `unit` compiled alone ([`crate::compile_unit`]) into a fresh recording
/// module for `isa`, with its module record (D-919), captured (D-914):
/// `cb$mod$<path>` for a code unit and `cb$mod$test$<path>` for a tests
/// unit (D-1107, as D-572 names tests' symbols), each
/// read-only and `Hidden` (D-917).
///
/// # Errors
///
/// [`RecordError::Unsupported`] for an entries unit, which is never
/// cached (D-964) and gets no module record; where its code goes is the
/// program object's concern (N4). [`RecordError::Codegen`] if code
/// generation fails, and [`RecordError::Undefined`] or
/// [`RecordError::Unsupported`] if the record cannot be finished.
pub fn record_unit(
    isa: OwnedTargetIsa,
    unit: &cheby_mir::MirModule,
    ucx: &crate::UnitCx<'_>,
    opts: &crate::CodegenOptions,
) -> Result<CodeRecord, RecordError> {
    let name = match unit.unit {
        cheby_mir::UnitKind::Code(_) => crate::mangle::module_record(&unit.path),
        cheby_mir::UnitKind::Tests(_) => crate::mangle::tests_module_record(&unit.path),
        cheby_mir::UnitKind::Entries => {
            return Err(RecordError::Unsupported(
                "the entries unit, which is never cached (D-964)".into(),
            ));
        }
    };
    let mut module = RecordingModule::new(isa);
    let code = crate::compile_unit(&mut module, unit, ucx, opts)?;
    crate::modrec::define_unit(&mut module, &name, &code)?;
    // D-938: the rows of each function, and the unit's file once, when it
    // has any. A unit whose file has no diagnostic path (D-421; a file
    // outside the package would show an absolute one) keeps no rows, so no
    // absolute path reaches a record.
    if let Some(source) = unit.source.as_deref().filter(|s| is_diagnostic_path(s)) {
        let mut any = false;
        for defined in &code.defined {
            if !defined.positions.is_empty() {
                any = true;
                module.set_positions(defined.id, defined.positions.clone())?;
            }
        }
        if any {
            module.set_source(Some(source));
        }
    }
    module.finish()
}

/// The code of a relocation kind in the encoding, `None` for a kind a
/// record does not hold.
#[must_use]
pub fn reloc_code(kind: Reloc) -> Option<u8> {
    RECORD_RELOCS
        .iter()
        .find(|&&(_, k)| k == kind)
        .map(|&(code, _)| code)
}

/// The relocation kind of `code`.
fn reloc_kind(code: u8) -> Option<Reloc> {
    RECORD_RELOCS
        .iter()
        .find(|&&(c, _)| c == code)
        .map(|&(_, kind)| kind)
}

/// The bytes a relocation of `kind` patches.
#[must_use]
pub const fn reloc_width(kind: Reloc) -> u64 {
    match kind {
        Reloc::Abs8 => 8,
        _ => 4,
    }
}

impl CodeRecord {
    /// Whether the record defines `name`: a symbol of linkage `Local`,
    /// `Hidden` or `Export`.
    #[must_use]
    pub fn defines(&self, name: &str) -> bool {
        self.symbol(name).is_some_and(|s| s.linkage.is_defined())
    }

    /// The symbol named `name`.
    #[must_use]
    pub fn symbol(&self, name: &str) -> Option<&RecordSymbol> {
        self.symbols.iter().find(|s| s.name == name)
    }

    /// The symbol at `index`.
    #[must_use]
    pub fn symbol_at(&self, index: u32) -> Option<&RecordSymbol> {
        self.symbols.get(usize::try_from(index).ok()?)
    }

    /// Checks what [`CodeRecord::decode`] and replay rely on: names are
    /// unique and hold no NUL, every index names a symbol of the right
    /// kind, alignments are powers of two no larger than [`MAX_ALIGN`],
    /// every relocation fits its object and has a kind of
    /// [`RECORD_RELOCS`], a zero-filled object has no relocations, and
    /// every symbol of linkage `Local`, `Hidden` or `Export` is defined
    /// exactly once and no import is.
    ///
    /// # Errors
    ///
    /// [`RecordError::Malformed`] naming the first problem, or
    /// [`RecordError::Undefined`] naming a symbol never defined.
    pub fn check(&self) -> Result<(), RecordError> {
        let mut names = std::collections::HashSet::with_capacity(self.symbols.len());
        for symbol in &self.symbols {
            if symbol.name.is_empty() || symbol.name.contains('\0') {
                return Err(RecordError::Malformed("an empty name or one with a NUL"));
            }
            if !names.insert(symbol.name.as_str()) {
                return Err(RecordError::Malformed("two symbols of one name"));
            }
        }
        let mut defined = vec![false; self.symbols.len()];
        let mut define = |symbol: u32, function: bool| -> Result<(), RecordError> {
            let index = usize::try_from(symbol)
                .map_err(|_| RecordError::Malformed("a symbol index out of range"))?;
            let entry = self
                .symbols
                .get(index)
                .ok_or(RecordError::Malformed("a symbol index out of range"))?;
            if (entry.kind == SymbolKind::Function) != function {
                return Err(RecordError::Malformed("a definition of the wrong kind"));
            }
            if !entry.linkage.is_defined() {
                return Err(RecordError::Malformed("a definition of an import"));
            }
            match defined.get_mut(index) {
                Some(seen) if !*seen => *seen = true,
                _ => return Err(RecordError::Malformed("a symbol defined twice")),
            }
            Ok(())
        };
        for function in &self.functions {
            define(function.symbol, true)?;
            if !function.align.is_power_of_two() {
                return Err(RecordError::Malformed("an alignment not a power of two"));
            }
            if function.align > MAX_ALIGN {
                return Err(RecordError::Malformed("an alignment too large"));
            }
            let len = u64::try_from(function.bytes.len()).unwrap_or(u64::MAX);
            if u32::try_from(len).is_err() {
                return Err(RecordError::Malformed("a function longer than 4 GiB"));
            }
            for reloc in &function.relocs {
                if reloc_code(reloc.kind).is_none() {
                    return Err(RecordError::Malformed("an unknown relocation kind"));
                }
                self.check_target(reloc.symbol)?;
                let end = u64::from(reloc.offset).checked_add(reloc_width(reloc.kind));
                if end.is_none_or(|end| end > len) {
                    return Err(RecordError::Malformed("a relocation outside its function"));
                }
            }
        }
        for object in &self.data {
            define(object.symbol, false)?;
            if object.align.is_some_and(|a| !a.is_power_of_two()) {
                return Err(RecordError::Malformed("an alignment not a power of two"));
            }
            if object.align.is_some_and(|a| a > MAX_ALIGN) {
                return Err(RecordError::Malformed("an alignment too large"));
            }
            let size = object.contents.size();
            if u32::try_from(size).is_err() {
                return Err(RecordError::Malformed("a data object larger than 4 GiB"));
            }
            if matches!(object.contents, RecordContents::Zeros(_)) && !object.relocs.is_empty() {
                return Err(RecordError::Malformed("a relocation in zero-filled data"));
            }
            for reloc in &object.relocs {
                self.check_target(reloc.symbol)?;
                let end = u64::from(reloc.offset).checked_add(POINTER_BYTES);
                if end.is_none_or(|end| end > size) {
                    return Err(RecordError::Malformed("a relocation outside its object"));
                }
            }
        }
        for (symbol, defined) in self.symbols.iter().zip(&defined) {
            if symbol.linkage.is_defined() && !defined {
                return Err(RecordError::Undefined(symbol.name.clone()));
            }
        }
        self.check_positions()
    }

    /// Checks the positions (D-938): the source, when named, is a
    /// relative `/`-separated path (D-421), never absolute; a record whose
    /// functions have rows names one; and each function's rows have
    /// strictly increasing offsets inside its code, and lines and columns
    /// from 1.
    fn check_positions(&self) -> Result<(), RecordError> {
        if let Some(source) = &self.source
            && !is_diagnostic_path(source)
        {
            return Err(RecordError::Malformed(
                "a source that is not a relative diagnostic path",
            ));
        }
        for function in &self.functions {
            if function.positions.is_empty() {
                continue;
            }
            if self.source.is_none() {
                return Err(RecordError::Malformed("positions without a source"));
            }
            let len = u64::try_from(function.bytes.len()).unwrap_or(u64::MAX);
            let mut last: Option<u32> = None;
            for row in &function.positions {
                if last.is_some_and(|l| row.offset <= l) || u64::from(row.offset) >= len {
                    return Err(RecordError::Malformed(
                        "a source row out of order or outside its function",
                    ));
                }
                if row.line == 0 || row.column == 0 {
                    return Err(RecordError::Malformed("a source row at line or column 0"));
                }
                last = Some(row.offset);
            }
        }
        Ok(())
    }

    fn check_target(&self, symbol: u32) -> Result<(), RecordError> {
        self.symbol_at(symbol)
            .map(|_| ())
            .ok_or(RecordError::Malformed("a relocation to an unknown symbol"))
    }

    /// The record's bytes in the encoding of the module documentation,
    /// fixed by declaration and definition order (D-264).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer(Vec::new());
        out.bytes(&MAGIC);
        out.u32(RECORD_VERSION);
        match &self.source {
            None => out.u8(0),
            Some(source) => {
                out.u8(1);
                out.blob(source.as_bytes());
            }
        }
        out.count(self.symbols.len());
        for symbol in &self.symbols {
            out.u8(symbol.kind.code());
            out.u8(symbol.linkage.code());
            out.blob(symbol.name.as_bytes());
        }
        out.count(self.functions.len());
        for function in &self.functions {
            out.u32(function.symbol);
            out.u64(function.align);
            out.blob(&function.bytes);
            out.count(function.relocs.len());
            for reloc in &function.relocs {
                out.u32(reloc.offset);
                out.u8(reloc_code(reloc.kind).unwrap_or(0));
                out.u32(reloc.symbol);
                out.i64(reloc.addend);
            }
            out.rows(&function.positions);
        }
        out.count(self.data.len());
        for object in &self.data {
            out.u32(object.symbol);
            out.u64(object.align.unwrap_or(0));
            match &object.contents {
                RecordContents::Bytes(bytes) => {
                    out.u8(0);
                    out.blob(bytes);
                }
                RecordContents::Zeros(size) => {
                    out.u8(1);
                    out.u64(*size);
                }
            }
            out.count(object.relocs.len());
            for reloc in &object.relocs {
                out.u32(reloc.offset);
                out.u32(reloc.symbol);
                out.i64(reloc.addend);
            }
        }
        out.0
    }

    /// Reads a record [`CodeRecord::encode`] wrote. Never panics (D-422).
    ///
    /// # Errors
    ///
    /// [`RecordError::Version`] for a record of another version, and
    /// [`RecordError::Malformed`] or [`RecordError::Undefined`] for bytes
    /// that are not a well-formed record of this version.
    pub fn decode(bytes: &[u8]) -> Result<Self, RecordError> {
        let mut input = Reader(bytes);
        if input.take(MAGIC.len())? != MAGIC {
            return Err(RecordError::Malformed("not a code record"));
        }
        let version = input.u32()?;
        if version != RECORD_VERSION {
            return Err(RecordError::Version(version));
        }
        let source = match input.u8()? {
            0 => None,
            1 => Some(
                std::str::from_utf8(input.blob()?)
                    .map_err(|_| RecordError::Malformed("a source that is not UTF-8"))?
                    .to_owned(),
            ),
            _ => return Err(RecordError::Malformed("an unknown kind of source")),
        };
        let mut symbols = Vec::new();
        for _ in 0..input.u32()? {
            let kind = SymbolKind::from_code(input.u8()?)
                .ok_or(RecordError::Malformed("an unknown symbol kind"))?;
            let linkage = SymbolLinkage::from_code(input.u8()?)
                .ok_or(RecordError::Malformed("an unknown linkage"))?;
            let name = std::str::from_utf8(input.blob()?)
                .map_err(|_| RecordError::Malformed("a name that is not UTF-8"))?
                .to_owned();
            symbols.push(RecordSymbol {
                name,
                kind,
                linkage,
            });
        }
        let mut functions = Vec::new();
        for _ in 0..input.u32()? {
            let symbol = input.u32()?;
            let align = input.u64()?;
            let code = input.blob()?.to_vec();
            let mut relocs = Vec::new();
            for _ in 0..input.u32()? {
                let offset = input.u32()?;
                let kind = reloc_kind(input.u8()?)
                    .ok_or(RecordError::Malformed("an unknown relocation kind"))?;
                relocs.push(RecordReloc {
                    offset,
                    kind,
                    symbol: input.u32()?,
                    addend: input.i64()?,
                });
            }
            let positions = input.rows()?;
            functions.push(RecordFunction {
                symbol,
                align,
                bytes: code,
                relocs,
                positions,
            });
        }
        let mut data = Vec::new();
        for _ in 0..input.u32()? {
            let symbol = input.u32()?;
            let align = match input.u64()? {
                0 => None,
                align => Some(align),
            };
            let contents = match input.u8()? {
                0 => RecordContents::Bytes(input.blob()?.to_vec()),
                1 => RecordContents::Zeros(input.u64()?),
                _ => return Err(RecordError::Malformed("an unknown kind of contents")),
            };
            let mut relocs = Vec::new();
            for _ in 0..input.u32()? {
                relocs.push(RecordDataReloc {
                    offset: input.u32()?,
                    symbol: input.u32()?,
                    addend: input.i64()?,
                });
            }
            data.push(RecordData {
                symbol,
                align,
                contents,
                relocs,
            });
        }
        if !input.0.is_empty() {
            return Err(RecordError::Malformed("bytes after the record"));
        }
        let record = Self {
            symbols,
            functions,
            data,
            source,
        };
        record.check()?;
        Ok(record)
    }
}

/// Whether `path` is a diagnostic path a record may name (D-421, D-938):
/// relative, `/`-separated, without empty, `.` or `..` components, a NUL
/// or a backslash, and with no drive or root, so no absolute path of the
/// machine that compiled it reaches a record or a line table.
#[must_use]
pub fn is_diagnostic_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\0', '\\', ':'])
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// `n` zigzag-encoded: `2n` for `n >= 0`, `-2n - 1` otherwise.
const fn zigzag(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)).cast_unsigned()
}

/// The number [`zigzag`] encoded as `z`.
const fn unzigzag(z: u64) -> i64 {
    (z >> 1).cast_signed() ^ 0_i64.wrapping_sub((z & 1).cast_signed())
}

/// The encoder's output.
struct Writer(Vec<u8>);

impl Writer {
    fn bytes(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.bytes(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes(&value.to_le_bytes());
    }

    /// A count of a record's table. [`CodeRecord::check`] bounds what a
    /// record holds well below 4 Gi entries; a larger count saturates and
    /// the result fails to decode.
    fn count(&mut self, count: usize) {
        self.u32(u32::try_from(count).unwrap_or(u32::MAX));
    }

    /// Length-prefixed bytes.
    fn blob(&mut self, bytes: &[u8]) {
        self.count(bytes.len());
        self.bytes(bytes);
    }

    /// A function's source rows (module documentation): the count, then
    /// each row relative to the one before.
    fn rows(&mut self, rows: &[SourceRow]) {
        self.count(rows.len());
        let (mut offset, mut line) = (0_u32, 1_u32);
        for row in rows {
            self.uleb(u64::from(row.offset.wrapping_sub(offset)));
            self.uleb(zigzag(i64::from(row.line).wrapping_sub(i64::from(line))));
            self.uleb(u64::from(row.column));
            (offset, line) = (row.offset, row.line);
        }
    }

    /// `value` as unsigned LEB128, in its shortest form.
    fn uleb(&mut self, mut value: u64) {
        loop {
            let low = u8::try_from(value & 0x7f).unwrap_or(0);
            value >>= 7;
            if value == 0 {
                self.u8(low);
                return;
            }
            self.u8(low | 0x80);
        }
    }
}

/// The decoder's input: what remains.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], RecordError> {
        if len > self.0.len() {
            return Err(RecordError::Malformed("the record ends early"));
        }
        let (head, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], RecordError> {
        let bytes = self.take(N)?;
        bytes
            .try_into()
            .map_err(|_| RecordError::Malformed("the record ends early"))
    }

    fn u8(&mut self) -> Result<u8, RecordError> {
        Ok(u8::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, RecordError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, RecordError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, RecordError> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    /// Length-prefixed bytes, the length checked against what remains.
    fn blob(&mut self) -> Result<&'a [u8], RecordError> {
        let len = usize::try_from(self.u32()?)
            .map_err(|_| RecordError::Malformed("a length out of range"))?;
        self.take(len)
    }

    /// A function's source rows, as [`Writer::rows`] writes them; a row
    /// out of range is malformed ([`CodeRecord::check`] checks the rest).
    fn rows(&mut self) -> Result<Vec<SourceRow>, RecordError> {
        let mut rows = Vec::new();
        let (mut offset, mut line) = (0_u32, 1_u32);
        for _ in 0..self.u32()? {
            let too_large = || RecordError::Malformed("a source row out of range");
            offset = u32::try_from(self.uleb()?)
                .ok()
                .and_then(|step| offset.checked_add(step))
                .ok_or_else(too_large)?;
            line = i64::from(line)
                .checked_add(unzigzag(self.uleb()?))
                .and_then(|l| u32::try_from(l).ok())
                .ok_or_else(too_large)?;
            let column = u32::try_from(self.uleb()?).map_err(|_| too_large())?;
            rows.push(SourceRow {
                offset,
                line,
                column,
            });
        }
        Ok(rows)
    }

    /// An unsigned LEB128 number in its shortest form (what
    /// [`Writer::uleb`] writes), so that a record re-encodes to its bytes;
    /// a longer form or one beyond 64 bits is malformed.
    fn uleb(&mut self) -> Result<u64, RecordError> {
        let bad = RecordError::Malformed("a number not in its shortest LEB128 form");
        let mut value = 0_u64;
        for shift in (0_u32..64).step_by(7) {
            let byte = self.u8()?;
            let low = u64::from(byte & 0x7f);
            if shift == 63 && low > 1 {
                return Err(bad);
            }
            value |= low << shift;
            if byte & 0x80 == 0 {
                // A last byte of 0 after another is a longer form.
                return if byte == 0 && shift > 0 {
                    Err(bad)
                } else {
                    Ok(value)
                };
            }
        }
        Err(bad)
    }
}
