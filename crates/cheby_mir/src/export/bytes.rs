//! The byte form of [`PortableBody`] (design T15): how std's exports
//! travel in the build cache's export record, so a release build reads
//! them instead of checking, lowering and optimizing std again.
//!
//! The form is the body in its exporter's defs, each def written as its
//! module and item numbers, with the body's name table beside it, which
//! [`PortableBody::import`] reads as it does in memory. It holds nothing
//! of the exporter's front end besides those defs: a body in
//! [`portable_form`] has no local names or types, no reuse sites and no
//! span ([`PLACEHOLDER_SPAN`]), so decoding its bytes gives back the same
//! body. Every list keeps its order, and the bodies of [`PortableBodies`]
//! go in the map's order, so equal bodies give equal bytes (D-264).
//!
//! The bodies come one after the other, each under its name and with its
//! length, so a reader decodes only the bodies a build asks for
//! ([`EncodedBodies`]). Integers are little-endian; a count or a length is
//! a `u32`; an enum is a tag byte in declaration order, then its fields,
//! except that the rvalues `Convert`, `FloatSqrt` and `Wrapping`, added
//! later, take the tags after the last one.
//! The decoder never panics and never preallocates from a count it read:
//! malformed bytes are an [`Ice`], which the cache reads as a miss.

use std::collections::BTreeMap;
use std::ops::Range;

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, Ice, IndexVec, ModuleId};

use super::{Exported, PortableBodies, PortableBody};
use crate::borrow::BorrowMask;
use crate::ir::{
    Abi, ArithOp, BitOp, Block, BlockId, BuiltinDesc, BuiltinLayout, Callee, CmpOp, Const,
    EntryKind, FnKey, Function, LayoutRef, Local, LocalDecl, Operand, PanicKind, RuntimeFn, Rvalue,
    StaticId, StaticKind, StaticObject, Stmt, StrId, TagType, Terminator,
};
use crate::positions::{Pos, PosRef, Positions};
use crate::repr::{CType, ForeignSig, Repr};

/// The first bytes of [`encode_bodies`].
const MAGIC: &[u8; 4] = b"CBXB";
/// The version of the form: 2 added the operations of the instruction
/// kernels (`crate::kernels_instr`).
const VERSION: u8 = 2;

/// The span of every function of a body in [`portable_form`]: the
/// exporter's spans name files of its own source map, which no reader
/// has. An importer gives a copied closure or shared function the call's
/// span, and a spliced body's span is not kept, so nothing reads it.
pub const PLACEHOLDER_SPAN: Span = Span::new(FileId::new(0), TextRange::empty(0));

/// Puts `body` in the form its bytes keep: every function's locals
/// without names and types, no reuse sites, and [`PLACEHOLDER_SPAN`].
/// [`PortableBody::import`] drops the same parts, and an importer the
/// span, so the form changes no import.
pub(super) fn portable_form(body: &mut Exported) {
    for f in std::iter::once(&mut body.function)
        .chain(&mut body.closures)
        .chain(&mut body.shared)
    {
        for decl in &mut f.locals {
            decl.name = None;
            decl.ty = None;
        }
        f.sites.clear();
        f.span = PLACEHOLDER_SPAN;
    }
}

/// The bytes of `bodies`: a magic, a version, then each body in the
/// map's order under its function's module path and item, with its
/// length, so a reader finds one body without decoding the others
/// ([`EncodedBodies`]).
#[must_use]
pub fn encode_bodies(bodies: &PortableBodies) -> Vec<u8> {
    let mut w = Writer::default();
    w.raw(MAGIC);
    w.u8(VERSION);
    w.len(bodies.len());
    for ((module, item), body) in bodies {
        w.str(module);
        w.str(item);
        let mut one = Writer::default();
        one.portable(body);
        w.len(one.0.len());
        w.raw(&one.0);
    }
    w.0
}

/// The bodies [`encode_bodies`] wrote, each under its function's name,
/// all decoded.
///
/// # Errors
///
/// An [`Ice`] for malformed bytes, which a cache reads as a miss.
pub fn decode_bodies(bytes: &[u8]) -> Result<PortableBodies, Ice> {
    EncodedBodies::read(bytes.to_vec())?.decode_all()
}

/// Portable bodies in the bytes [`encode_bodies`] wrote, each decoded when
/// a build asks for it ([`super::PortableSource`]): a release build that
/// inlines a few of std's bodies decodes those few. Reading checks the
/// bytes' shape and the names; a body's own bytes are checked when it is
/// decoded.
pub struct EncodedBodies {
    bytes: Vec<u8>,
    /// Each body's range in `bytes`, by its function's module path and
    /// item.
    index: Index,
}

/// Each body's range in the bytes, by its function's module path and item.
type Index = BTreeMap<(Box<str>, Box<str>), Range<usize>>;

impl std::fmt::Debug for EncodedBodies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncodedBodies")
            .field("bodies", &self.index.len())
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl EncodedBodies {
    /// The bodies of `bytes`, which [`encode_bodies`] wrote; none is
    /// decoded yet.
    ///
    /// # Errors
    ///
    /// An [`Ice`] for malformed bytes, which a cache reads as a miss.
    pub fn read(bytes: Vec<u8>) -> Result<Self, Ice> {
        let index = read_index(&bytes).ok_or_else(|| Ice::new("malformed std export record"))?;
        Ok(Self { bytes, index })
    }

    /// The bytes it was read from.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The body of `name`'s function (module path and item), decoded;
    /// `None` when there is none.
    ///
    /// # Errors
    ///
    /// An [`Ice`] when its bytes do not decode to a body of that name.
    pub fn get(&self, name: &(Box<str>, Box<str>)) -> Result<Option<PortableBody>, Ice> {
        let Some(range) = self.index.get(name) else {
            return Ok(None);
        };
        let body = self
            .bytes
            .get(range.clone())
            .and_then(decode_one)
            .filter(|body| body.name() == Some((&*name.0, &*name.1)))
            .ok_or_else(|| Ice::new("malformed body in the std export record"))?;
        Ok(Some(body))
    }

    /// Every body, decoded.
    ///
    /// # Errors
    ///
    /// An [`Ice`] when one does not decode.
    pub fn decode_all(&self) -> Result<PortableBodies, Ice> {
        let mut out = PortableBodies::new();
        for name in self.index.keys() {
            if let Some(body) = self.get(name)? {
                out.insert(name.clone(), body);
            }
        }
        Ok(out)
    }
}

impl super::PortableSource for EncodedBodies {
    fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    fn import(
        &self,
        name: &(Box<str>, Box<str>),
        defs: &crate::defs::Defs<'_>,
    ) -> Option<Exported> {
        self.get(name).ok().flatten()?.import(defs).ok().flatten()
    }
}

/// The index of `bytes`: each body's name and range, the names sorted
/// and distinct as [`encode_bodies`] writes them.
fn read_index(bytes: &[u8]) -> Option<Index> {
    let mut r = Reader { rest: bytes };
    if r.take(MAGIC.len())? != MAGIC.as_slice() || r.u8()? != VERSION {
        return None;
    }
    let mut index = BTreeMap::new();
    let mut last: Option<(Box<str>, Box<str>)> = None;
    for _ in 0..r.len()? {
        let name = (r.str()?, r.str()?);
        let len = r.len()?;
        let start = bytes.len().checked_sub(r.rest.len())?;
        r.take(len)?;
        if last.as_ref().is_some_and(|before| *before >= name) {
            return None;
        }
        last = Some(name.clone());
        index.insert(name, start..start.checked_add(len)?);
    }
    r.rest.is_empty().then_some(index)
}

/// One body's bytes, as [`Writer::portable`] wrote them.
fn decode_one(bytes: &[u8]) -> Option<PortableBody> {
    let mut r = Reader { rest: bytes };
    let body = r.portable()?;
    r.rest.is_empty().then_some(body)
}

/// Appends the form's values.
#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn raw(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }

    fn u32(&mut self, v: u32) {
        self.raw(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.raw(&v.to_le_bytes());
    }

    /// A count or a length, saturated: no list of MIR comes near it.
    fn len(&mut self, n: usize) {
        self.u32(u32::try_from(n).unwrap_or(u32::MAX));
    }

    fn str(&mut self, s: &str) {
        self.len(s.len());
        self.raw(s.as_bytes());
    }

    fn opt_str(&mut self, s: Option<&str>) {
        match s {
            Some(s) => {
                self.u8(1);
                self.str(s);
            }
            None => self.u8(0),
        }
    }

    fn strings(&mut self, strings: &IndexVec<StrId, Box<str>>) {
        self.len(strings.len());
        for s in strings {
            self.str(s);
        }
    }

    fn def(&mut self, def: DefId) {
        self.u32(def.module.as_u32());
        self.u32(def.index.as_u32());
    }

    fn local(&mut self, l: Local) {
        self.u32(l.as_u32());
    }

    fn locals(&mut self, ls: &[Local]) {
        self.len(ls.len());
        for &l in ls {
            self.local(l);
        }
    }

    fn block_id(&mut self, b: BlockId) {
        self.u32(b.as_u32());
    }

    fn opt_block(&mut self, b: Option<BlockId>) {
        match b {
            Some(b) => {
                self.u8(1);
                self.block_id(b);
            }
            None => self.u8(0),
        }
    }

    fn portable(&mut self, p: &PortableBody) {
        self.len(p.names.len());
        for (def, module, item) in &p.names {
            self.def(*def);
            self.str(module);
            self.str(item);
        }
        self.exported(&p.body);
    }

    fn exported(&mut self, e: &Exported) {
        self.def(e.def);
        self.function(&e.function);
        self.len(e.closures.len());
        for f in &e.closures {
            self.function(f);
        }
        self.len(e.shared.len());
        for f in &e.shared {
            self.function(f);
        }
        self.len(e.statics.len());
        for s in &e.statics {
            self.static_object(s);
        }
        self.opt_str(e.source.as_deref());
    }

    fn static_object(&mut self, s: &StaticObject) {
        self.u32(s.id.as_u32());
        self.u8(match s.kind {
            StaticKind::Dict => 0,
            StaticKind::Desc => 1,
            StaticKind::FnValue => 2,
        });
        self.str(&s.module);
        self.str(&s.name);
        self.layout(s.layout);
        self.len(s.fields.len());
        for c in &s.fields {
            self.constant(c);
        }
        self.strings(&s.strings);
    }

    /// A function in [`portable_form`]: its span, its locals' names and
    /// types and its sites are not written.
    fn function(&mut self, f: &Function) {
        self.key(f.key);
        self.u8(match f.abi {
            Abi::Cheby => 0,
            Abi::C => 1,
        });
        self.locals(&f.params);
        self.repr(f.ret);
        self.len(f.locals.len());
        for decl in &f.locals {
            self.repr(decl.repr);
        }
        self.len(f.blocks.len());
        for b in &f.blocks {
            self.block(b);
        }
        self.strings(&f.strings);
        match f.entry_loc {
            Some(s) => {
                self.u8(1);
                self.u32(s.as_u32());
            }
            None => self.u8(0),
        }
        self.opt_block(f.entry_unwind);
        self.bool(f.precounted);
        self.u64(f.borrow.0);
        self.len(f.call_locs.len());
        for &(l, s) in &f.call_locs {
            self.local(l);
            self.u32(s.as_u32());
        }
        self.strings(&f.call_loc_strings);
        self.len(f.positions.table.len());
        for pos in &f.positions.table {
            self.u32(pos.line);
            self.u32(pos.column);
        }
        self.u32(f.positions.entry.bits());
    }

    fn block(&mut self, b: &Block) {
        self.locals(&b.params);
        self.len(b.stmts.len());
        for s in &b.stmts {
            self.stmt(s);
        }
        self.term(&b.term);
        self.len(b.unwind.len());
        for &(i, pad) in &b.unwind {
            self.u32(i);
            self.block_id(pad);
        }
        self.len(b.pos.len());
        for p in &b.pos {
            self.u32(p.bits());
        }
    }

    fn keep(&mut self, keep: &[(u32, Local)]) {
        self.len(keep.len());
        for &(i, l) in keep {
            self.u32(i);
            self.local(l);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Assign { dst, value } => {
                self.u8(0);
                self.local(*dst);
                self.rvalue(value);
            }
            Stmt::Eval(value) => {
                self.u8(1);
                self.rvalue(value);
            }
            Stmt::Dup(l) => {
                self.u8(2);
                self.local(*l);
            }
            Stmt::Drop(l) => {
                self.u8(3);
                self.local(*l);
            }
            Stmt::DropKeep {
                local,
                layout,
                keep,
            } => {
                self.u8(4);
                self.local(*local);
                self.layout(*layout);
                self.keep(keep);
            }
            Stmt::ClearUnwind => self.u8(5),
        }
    }

    fn operands(&mut self, ops: &[Operand]) {
        self.len(ops.len());
        for op in ops {
            self.operand(op);
        }
    }

    fn operand(&mut self, op: &Operand) {
        match op {
            Operand::Local(l) => {
                self.u8(0);
                self.local(*l);
            }
            Operand::Const(c) => {
                self.u8(1);
                self.constant(c);
            }
        }
    }

    fn callee(&mut self, callee: &Callee) {
        match callee {
            Callee::Direct(d) => {
                self.u8(0);
                self.def(*d);
            }
            Callee::Borrowing(d, mask) => {
                self.u8(1);
                self.def(*d);
                self.u64(mask.0);
            }
            Callee::Code(key) => {
                self.u8(2);
                self.key(*key);
            }
            Callee::Indirect(l) => {
                self.u8(3);
                self.local(*l);
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per rvalue, in declaration order"
    )]
    fn rvalue(&mut self, value: &Rvalue) {
        match value {
            Rvalue::Use(op) => {
                self.u8(0);
                self.operand(op);
            }
            Rvalue::Call { callee, args } => {
                self.u8(1);
                self.callee(callee);
                self.operands(args);
            }
            Rvalue::CallForeign {
                symbol,
                sig,
                args,
                blocking,
            } => {
                self.u8(2);
                self.str(symbol);
                self.len(sig.params.len());
                for &c in &sig.params {
                    self.ctype(c);
                }
                self.ctype(sig.ret);
                self.operands(args);
                self.bool(*blocking);
            }
            Rvalue::CallRuntime { func, args } => {
                self.u8(3);
                self.runtime_fn(*func);
                self.operands(args);
            }
            Rvalue::Alloc { layout, fields } => {
                self.u8(4);
                self.layout(*layout);
                self.operands(fields);
            }
            Rvalue::AllocPlaceholder { layout } => {
                self.u8(5);
                self.layout(*layout);
            }
            Rvalue::InitField {
                obj,
                layout,
                field,
                value,
            } => {
                self.u8(6);
                self.local(*obj);
                self.layout(*layout);
                self.u32(*field);
                self.operand(value);
            }
            Rvalue::Field {
                base,
                layout,
                field,
            } => {
                self.u8(7);
                self.local(*base);
                self.layout(*layout);
                self.u32(*field);
            }
            Rvalue::ReadLayout { base } => {
                self.u8(8);
                self.local(*base);
            }
            Rvalue::LoadConst(d) => {
                self.u8(9);
                self.def(*d);
            }
            Rvalue::Box { value, from } => {
                self.u8(10);
                self.operand(value);
                self.repr(*from);
            }
            Rvalue::Unbox { value, to } => {
                self.u8(11);
                self.operand(value);
                self.repr(*to);
            }
            Rvalue::Checked {
                op,
                lhs,
                rhs,
                repr,
                loc,
            } => {
                self.u8(12);
                self.arith(*op);
                self.operand(lhs);
                self.operand(rhs);
                self.repr(*repr);
                self.u32(loc.as_u32());
            }
            Rvalue::Compare { op, lhs, rhs, repr } => {
                self.u8(13);
                self.u8(match op {
                    CmpOp::Eq => 0,
                    CmpOp::Ne => 1,
                    CmpOp::Lt => 2,
                    CmpOp::Le => 3,
                    CmpOp::Gt => 4,
                    CmpOp::Ge => 5,
                });
                self.operand(lhs);
                self.operand(rhs);
                self.repr(*repr);
            }
            Rvalue::FloatArith { op, lhs, rhs, repr } => {
                self.u8(14);
                self.arith(*op);
                self.operand(lhs);
                self.operand(rhs);
                self.repr(*repr);
            }
            Rvalue::Bitwise { op, lhs, rhs, repr } => {
                self.u8(15);
                self.u8(match op {
                    BitOp::And => 0,
                    BitOp::Or => 1,
                    BitOp::Xor => 2,
                });
                self.operand(lhs);
                self.operand(rhs);
                self.repr(*repr);
            }
            Rvalue::FloatNeg { value, repr } => {
                self.u8(16);
                self.operand(value);
                self.repr(*repr);
            }
            // The operations of the instruction kernels came later: their
            // tags follow the others', so those keep their values.
            Rvalue::Convert { value, from, to } => {
                self.u8(21);
                self.operand(value);
                self.repr(*from);
                self.repr(*to);
            }
            Rvalue::FloatSqrt { value, repr } => {
                self.u8(22);
                self.operand(value);
                self.repr(*repr);
            }
            Rvalue::Wrapping { op, lhs, rhs, repr } => {
                self.u8(23);
                self.arith(*op);
                self.operand(lhs);
                self.operand(rhs);
                self.repr(*repr);
            }
            Rvalue::Not(op) => {
                self.u8(17);
                self.operand(op);
            }
            Rvalue::ReadTag { base, ty } => {
                self.u8(18);
                self.operand(base);
                match ty {
                    TagType::Option => self.u8(0),
                    TagType::Result => self.u8(1),
                    TagType::Order => self.u8(2),
                    TagType::Adt(d) => {
                        self.u8(3);
                        self.def(*d);
                    }
                }
            }
            Rvalue::DropReuse {
                local,
                layout,
                keep,
            } => {
                self.u8(19);
                self.local(*local);
                self.layout(*layout);
                self.keep(keep);
            }
            Rvalue::AllocReuse {
                token,
                layout,
                fields,
            } => {
                self.u8(20);
                self.local(*token);
                self.layout(*layout);
                self.operands(fields);
            }
        }
    }

    fn term(&mut self, term: &Terminator) {
        match term {
            Terminator::Return(op) => {
                self.u8(0);
                self.operand(op);
            }
            Terminator::TailCall { callee, args } => {
                self.u8(1);
                self.callee(callee);
                self.operands(args);
            }
            Terminator::Goto { target, args } => {
                self.u8(2);
                self.block_id(*target);
                self.operands(args);
            }
            Terminator::Branch { cond, then_, else_ } => {
                self.u8(3);
                self.operand(cond);
                self.block_id(*then_);
                self.block_id(*else_);
            }
            Terminator::Switch { on, cases, default } => {
                self.u8(4);
                self.operand(on);
                self.len(cases.len());
                for &(bits, b) in cases {
                    self.u64(bits);
                    self.block_id(b);
                }
                self.opt_block(*default);
            }
            Terminator::Panic { kind, message, loc } => {
                self.u8(5);
                self.u32(kind.code());
                match message {
                    Some(m) => {
                        self.u8(1);
                        self.operand(m);
                    }
                    None => self.u8(0),
                }
                self.u32(loc.as_u32());
            }
            Terminator::Unreachable => self.u8(6),
            Terminator::Resume => self.u8(7),
        }
    }

    fn constant(&mut self, c: &Const) {
        match c {
            Const::Int(v) => {
                self.u8(0);
                self.raw(&v.to_le_bytes());
            }
            Const::Float(bits) => {
                self.u8(1);
                self.u64(*bits);
            }
            Const::Bool(b) => {
                self.u8(2);
                self.bool(*b);
            }
            Const::Sized { repr, bits } => {
                self.u8(3);
                self.repr(*repr);
                self.u64(*bits);
            }
            Const::Str(s) => {
                self.u8(4);
                self.u32(s.as_u32());
            }
            Const::Nil => self.u8(5),
            Const::StaticCtor(d) => {
                self.u8(6);
                self.def(*d);
            }
            Const::FnValue(d) => {
                self.u8(7);
                self.def(*d);
            }
            Const::Code(key) => {
                self.u8(8);
                self.key(*key);
            }
            Const::EmptyList => self.u8(9),
            Const::Static(id) => {
                self.u8(10);
                self.u32(id.as_u32());
            }
            Const::BuiltinDesc(d) => {
                self.u8(11);
                self.u8(builtin_desc_tag(*d));
            }
            Const::Ctx => self.u8(12),
            Const::ErrNil => self.u8(13),
        }
    }

    fn key(&mut self, key: FnKey) {
        match key {
            FnKey::Item(d) => {
                self.u8(0);
                self.def(d);
            }
            FnKey::ValueShim(d) => {
                self.u8(1);
                self.def(d);
            }
            FnKey::Closure { owner, index } => {
                self.u8(2);
                self.def(owner);
                self.u32(index);
            }
            FnKey::ConstInit(d) => {
                self.u8(3);
                self.def(d);
            }
            FnKey::Test(d) => {
                self.u8(4);
                self.def(d);
            }
            FnKey::Entry(kind) => {
                self.u8(5);
                match kind {
                    EntryKind::Main(d) => {
                        self.u8(0);
                        self.def(d);
                    }
                    EntryKind::Test(d) => {
                        self.u8(1);
                        self.def(d);
                    }
                    EntryKind::Const(d) => {
                        self.u8(2);
                        self.def(d);
                    }
                    EntryKind::Repl(n) => {
                        self.u8(3);
                        self.u32(n);
                    }
                }
            }
            FnKey::Trampoline(n) => {
                self.u8(6);
                self.u32(n);
            }
            FnKey::TupleDictFn { iface_fn, arity } => {
                self.u8(7);
                self.def(iface_fn);
                self.u32(arity);
            }
            FnKey::DynForward {
                iface_fn,
                dyn_iface,
            } => {
                self.u8(8);
                self.def(iface_fn);
                self.def(dyn_iface);
            }
            FnKey::Continuation { owner, index } => {
                self.u8(9);
                self.def(owner);
                self.u32(index);
            }
        }
    }

    fn layout(&mut self, layout: LayoutRef) {
        match layout {
            LayoutRef::Builtin(b) => {
                self.u8(0);
                self.u8(match b {
                    BuiltinLayout::String => 0,
                    BuiltinLayout::Bytes => 1,
                    BuiltinLayout::BoxedInt => 2,
                    BuiltinLayout::BoxedFloat => 3,
                    BuiltinLayout::BoxedF32 => 4,
                    BuiltinLayout::BoxedU64 => 5,
                });
            }
            LayoutRef::Ctor(d) => {
                self.u8(1);
                self.def(d);
            }
            LayoutRef::Tuple(n) => {
                self.u8(2);
                self.u32(n);
            }
            LayoutRef::Closure(n) => {
                self.u8(3);
                self.u32(n);
            }
            LayoutRef::Dict { fns, embedded } => {
                self.u8(4);
                self.u32(fns);
                self.u32(embedded);
            }
            LayoutRef::Dyn => self.u8(5),
            LayoutRef::Desc { args } => {
                self.u8(6);
                self.u32(args);
            }
            LayoutRef::DescNominal => self.u8(7),
        }
    }

    fn repr(&mut self, repr: Repr) {
        self.u8(repr_tag(repr));
    }

    fn ctype(&mut self, c: CType) {
        self.u8(match c {
            CType::I8 => 0,
            CType::I16 => 1,
            CType::I32 => 2,
            CType::I64 => 3,
            CType::U8 => 4,
            CType::U16 => 5,
            CType::U32 => 6,
            CType::U64 => 7,
            CType::F32 => 8,
            CType::F64 => 9,
            CType::Bool => 10,
            CType::Ptr => 11,
            CType::Void => 12,
        });
    }

    fn arith(&mut self, op: ArithOp) {
        self.u8(match op {
            ArithOp::Add => 0,
            ArithOp::Sub => 1,
            ArithOp::Mul => 2,
            ArithOp::Div => 3,
            ArithOp::Rem => 4,
            ArithOp::Shl => 5,
            ArithOp::Shr => 6,
        });
    }

    /// A helper as its position in [`RuntimeFn::ALL`], which lists every
    /// one; the decoder reads the same list.
    fn runtime_fn(&mut self, func: RuntimeFn) {
        let at = RuntimeFn::ALL.iter().position(|&f| f == func);
        self.u8(at.and_then(|n| u8::try_from(n).ok()).unwrap_or(u8::MAX));
    }
}

/// The representations in tag order.
const REPRS: [Repr; 13] = [
    Repr::Value,
    Repr::Int,
    Repr::Float,
    Repr::Bool,
    Repr::I8,
    Repr::I16,
    Repr::I32,
    Repr::I64,
    Repr::U8,
    Repr::U16,
    Repr::U32,
    Repr::U64,
    Repr::F32,
];

const fn repr_tag(repr: Repr) -> u8 {
    match repr {
        Repr::Value => 0,
        Repr::Int => 1,
        Repr::Float => 2,
        Repr::Bool => 3,
        Repr::I8 => 4,
        Repr::I16 => 5,
        Repr::I32 => 6,
        Repr::I64 => 7,
        Repr::U8 => 8,
        Repr::U16 => 9,
        Repr::U32 => 10,
        Repr::U64 => 11,
        Repr::F32 => 12,
    }
}

/// The builtin descriptors in tag order.
const BUILTIN_DESCS: [BuiltinDesc; 23] = [
    BuiltinDesc::Int,
    BuiltinDesc::Float,
    BuiltinDesc::F32,
    BuiltinDesc::I8,
    BuiltinDesc::I16,
    BuiltinDesc::I32,
    BuiltinDesc::I64,
    BuiltinDesc::U8,
    BuiltinDesc::U16,
    BuiltinDesc::U32,
    BuiltinDesc::U64,
    BuiltinDesc::Bool,
    BuiltinDesc::Nil,
    BuiltinDesc::String,
    BuiltinDesc::Bytes,
    BuiltinDesc::List,
    BuiltinDesc::Map,
    BuiltinDesc::Set,
    BuiltinDesc::Option,
    BuiltinDesc::Result,
    BuiltinDesc::Order,
    BuiltinDesc::Tuple,
    BuiltinDesc::Fn,
];

const fn builtin_desc_tag(d: BuiltinDesc) -> u8 {
    match d {
        BuiltinDesc::Int => 0,
        BuiltinDesc::Float => 1,
        BuiltinDesc::F32 => 2,
        BuiltinDesc::I8 => 3,
        BuiltinDesc::I16 => 4,
        BuiltinDesc::I32 => 5,
        BuiltinDesc::I64 => 6,
        BuiltinDesc::U8 => 7,
        BuiltinDesc::U16 => 8,
        BuiltinDesc::U32 => 9,
        BuiltinDesc::U64 => 10,
        BuiltinDesc::Bool => 11,
        BuiltinDesc::Nil => 12,
        BuiltinDesc::String => 13,
        BuiltinDesc::Bytes => 14,
        BuiltinDesc::List => 15,
        BuiltinDesc::Map => 16,
        BuiltinDesc::Set => 17,
        BuiltinDesc::Option => 18,
        BuiltinDesc::Result => 19,
        BuiltinDesc::Order => 20,
        BuiltinDesc::Tuple => 21,
        BuiltinDesc::Fn => 22,
    }
}

/// Reads the form's values back; `None` for anything malformed.
struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.rest.split_at_checked(n)?;
        self.rest = tail;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    fn bool(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    /// A count, which must not exceed the bytes left (every element takes
    /// at least one byte), so a corrupt count never allocates.
    fn len(&mut self) -> Option<usize> {
        let n = usize::try_from(self.u32()?).ok()?;
        (n <= self.rest.len()).then_some(n)
    }

    fn str(&mut self) -> Option<Box<str>> {
        let n = self.len()?;
        std::str::from_utf8(self.take(n)?).ok().map(Box::from)
    }

    #[expect(
        clippy::option_option,
        reason = "the outer `None` is malformed bytes, the inner an absent string"
    )]
    fn opt_str(&mut self) -> Option<Option<Box<str>>> {
        match self.u8()? {
            0 => Some(None),
            1 => self.str().map(Some),
            _ => None,
        }
    }

    fn list<T>(&mut self, mut each: impl FnMut(&mut Self) -> Option<T>) -> Option<Vec<T>> {
        let n = self.len()?;
        let mut out = Vec::new();
        for _ in 0..n {
            out.push(each(self)?);
        }
        Some(out)
    }

    fn strings(&mut self) -> Option<IndexVec<StrId, Box<str>>> {
        IndexVec::from_raw(self.list(Self::str)?).ok()
    }

    fn def(&mut self) -> Option<DefId> {
        let module = ModuleId::from_u32(self.u32()?);
        let index = DefIndex::from_u32(self.u32()?);
        Some(DefId::new(module, index))
    }

    fn local(&mut self) -> Option<Local> {
        self.u32().map(Local::from_u32)
    }

    fn locals(&mut self) -> Option<Vec<Local>> {
        self.list(Self::local)
    }

    fn block_id(&mut self) -> Option<BlockId> {
        self.u32().map(BlockId::from_u32)
    }

    #[expect(
        clippy::option_option,
        reason = "the outer `None` is malformed bytes, the inner an absent block"
    )]
    fn opt_block(&mut self) -> Option<Option<BlockId>> {
        match self.u8()? {
            0 => Some(None),
            1 => self.block_id().map(Some),
            _ => None,
        }
    }

    fn portable(&mut self) -> Option<PortableBody> {
        let names = self.list(|r| Some((r.def()?, r.str()?, r.str()?)))?;
        let body = self.exported()?;
        Some(PortableBody { body, names })
    }

    fn exported(&mut self) -> Option<Exported> {
        Some(Exported {
            def: self.def()?,
            function: self.function()?,
            closures: self.list(Self::function)?,
            shared: self.list(Self::function)?,
            statics: self.list(Self::static_object)?,
            source: self.opt_str()?,
        })
    }

    fn static_object(&mut self) -> Option<StaticObject> {
        Some(StaticObject {
            id: StaticId::from_u32(self.u32()?),
            kind: match self.u8()? {
                0 => StaticKind::Dict,
                1 => StaticKind::Desc,
                2 => StaticKind::FnValue,
                _ => return None,
            },
            module: self.str()?,
            name: self.str()?,
            layout: self.layout()?,
            fields: self.list(Self::constant)?.into_boxed_slice(),
            strings: self.strings()?,
        })
    }

    fn function(&mut self) -> Option<Function> {
        let key = self.key()?;
        let abi = match self.u8()? {
            0 => Abi::Cheby,
            1 => Abi::C,
            _ => return None,
        };
        let params = self.locals()?;
        let ret = self.repr()?;
        let locals = self.list(|r| {
            Some(LocalDecl {
                repr: r.repr()?,
                name: None,
                ty: None,
            })
        })?;
        let blocks = self.list(Self::block)?;
        let strings = self.strings()?;
        let entry_loc = match self.u8()? {
            0 => None,
            1 => Some(StrId::from_u32(self.u32()?)),
            _ => return None,
        };
        let entry_unwind = self.opt_block()?;
        let precounted = self.bool()?;
        let borrow = BorrowMask(self.u64()?);
        let call_locs = self.list(|r| Some((r.local()?, StrId::from_u32(r.u32()?))))?;
        let call_loc_strings = self.strings()?;
        let table = self.list(|r| {
            Some(Pos {
                line: r.u32()?,
                column: r.u32()?,
            })
        })?;
        let entry = PosRef::from_bits(self.u32()?);
        Some(Function {
            key,
            abi,
            params,
            ret,
            locals: IndexVec::from_raw(locals).ok()?,
            blocks: IndexVec::from_raw(blocks).ok()?,
            span: PLACEHOLDER_SPAN,
            strings,
            entry_loc,
            entry_unwind,
            precounted,
            borrow,
            sites: Vec::new(),
            call_locs,
            call_loc_strings,
            positions: Positions { table, entry },
        })
    }

    fn block(&mut self) -> Option<Block> {
        Some(Block {
            params: self.locals()?,
            stmts: self.list(Self::stmt)?,
            term: self.term()?,
            unwind: self.list(|r| Some((r.u32()?, r.block_id()?)))?,
            pos: self.list(|r| r.u32().map(PosRef::from_bits))?,
        })
    }

    fn keep(&mut self) -> Option<Vec<(u32, Local)>> {
        self.list(|r| Some((r.u32()?, r.local()?)))
    }

    fn stmt(&mut self) -> Option<Stmt> {
        Some(match self.u8()? {
            0 => Stmt::Assign {
                dst: self.local()?,
                value: self.rvalue()?,
            },
            1 => Stmt::Eval(self.rvalue()?),
            2 => Stmt::Dup(self.local()?),
            3 => Stmt::Drop(self.local()?),
            4 => Stmt::DropKeep {
                local: self.local()?,
                layout: self.layout()?,
                keep: self.keep()?,
            },
            5 => Stmt::ClearUnwind,
            _ => return None,
        })
    }

    fn operands(&mut self) -> Option<Vec<Operand>> {
        self.list(Self::operand)
    }

    fn operand(&mut self) -> Option<Operand> {
        Some(match self.u8()? {
            0 => Operand::Local(self.local()?),
            1 => Operand::Const(self.constant()?),
            _ => return None,
        })
    }

    fn callee(&mut self) -> Option<Callee> {
        Some(match self.u8()? {
            0 => Callee::Direct(self.def()?),
            1 => Callee::Borrowing(self.def()?, BorrowMask(self.u64()?)),
            2 => Callee::Code(self.key()?),
            3 => Callee::Indirect(self.local()?),
            _ => return None,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per rvalue, in declaration order"
    )]
    fn rvalue(&mut self) -> Option<Rvalue> {
        Some(match self.u8()? {
            0 => Rvalue::Use(self.operand()?),
            1 => Rvalue::Call {
                callee: self.callee()?,
                args: self.operands()?,
            },
            2 => Rvalue::CallForeign {
                symbol: self.str()?,
                sig: ForeignSig {
                    params: self.list(Self::ctype)?,
                    ret: self.ctype()?,
                },
                args: self.operands()?,
                blocking: self.bool()?,
            },
            3 => Rvalue::CallRuntime {
                func: *RuntimeFn::ALL.get(usize::from(self.u8()?))?,
                args: self.operands()?,
            },
            4 => Rvalue::Alloc {
                layout: self.layout()?,
                fields: self.operands()?,
            },
            5 => Rvalue::AllocPlaceholder {
                layout: self.layout()?,
            },
            6 => Rvalue::InitField {
                obj: self.local()?,
                layout: self.layout()?,
                field: self.u32()?,
                value: self.operand()?,
            },
            7 => Rvalue::Field {
                base: self.local()?,
                layout: self.layout()?,
                field: self.u32()?,
            },
            8 => Rvalue::ReadLayout {
                base: self.local()?,
            },
            9 => Rvalue::LoadConst(self.def()?),
            10 => Rvalue::Box {
                value: self.operand()?,
                from: self.repr()?,
            },
            11 => Rvalue::Unbox {
                value: self.operand()?,
                to: self.repr()?,
            },
            12 => Rvalue::Checked {
                op: self.arith()?,
                lhs: self.operand()?,
                rhs: self.operand()?,
                repr: self.repr()?,
                loc: StrId::from_u32(self.u32()?),
            },
            13 => Rvalue::Compare {
                op: match self.u8()? {
                    0 => CmpOp::Eq,
                    1 => CmpOp::Ne,
                    2 => CmpOp::Lt,
                    3 => CmpOp::Le,
                    4 => CmpOp::Gt,
                    5 => CmpOp::Ge,
                    _ => return None,
                },
                lhs: self.operand()?,
                rhs: self.operand()?,
                repr: self.repr()?,
            },
            14 => Rvalue::FloatArith {
                op: self.arith()?,
                lhs: self.operand()?,
                rhs: self.operand()?,
                repr: self.repr()?,
            },
            15 => Rvalue::Bitwise {
                op: match self.u8()? {
                    0 => BitOp::And,
                    1 => BitOp::Or,
                    2 => BitOp::Xor,
                    _ => return None,
                },
                lhs: self.operand()?,
                rhs: self.operand()?,
                repr: self.repr()?,
            },
            16 => Rvalue::FloatNeg {
                value: self.operand()?,
                repr: self.repr()?,
            },
            17 => Rvalue::Not(self.operand()?),
            18 => Rvalue::ReadTag {
                base: self.operand()?,
                ty: match self.u8()? {
                    0 => TagType::Option,
                    1 => TagType::Result,
                    2 => TagType::Order,
                    3 => TagType::Adt(self.def()?),
                    _ => return None,
                },
            },
            19 => Rvalue::DropReuse {
                local: self.local()?,
                layout: self.layout()?,
                keep: self.keep()?,
            },
            20 => Rvalue::AllocReuse {
                token: self.local()?,
                layout: self.layout()?,
                fields: self.operands()?,
            },
            21 => Rvalue::Convert {
                value: self.operand()?,
                from: self.repr()?,
                to: self.repr()?,
            },
            22 => Rvalue::FloatSqrt {
                value: self.operand()?,
                repr: self.repr()?,
            },
            23 => Rvalue::Wrapping {
                op: self.arith()?,
                lhs: self.operand()?,
                rhs: self.operand()?,
                repr: self.repr()?,
            },
            _ => return None,
        })
    }

    fn term(&mut self) -> Option<Terminator> {
        Some(match self.u8()? {
            0 => Terminator::Return(self.operand()?),
            1 => Terminator::TailCall {
                callee: self.callee()?,
                args: self.operands()?,
            },
            2 => Terminator::Goto {
                target: self.block_id()?,
                args: self.operands()?,
            },
            3 => Terminator::Branch {
                cond: self.operand()?,
                then_: self.block_id()?,
                else_: self.block_id()?,
            },
            4 => Terminator::Switch {
                on: self.operand()?,
                cases: self.list(|r| Some((r.u64()?, r.block_id()?)))?,
                default: self.opt_block()?,
            },
            5 => Terminator::Panic {
                kind: *PanicKind::ALL.get(usize::try_from(self.u32()?).ok()?)?,
                message: match self.u8()? {
                    0 => None,
                    1 => Some(self.operand()?),
                    _ => return None,
                },
                loc: StrId::from_u32(self.u32()?),
            },
            6 => Terminator::Unreachable,
            7 => Terminator::Resume,
            _ => return None,
        })
    }

    fn constant(&mut self) -> Option<Const> {
        Some(match self.u8()? {
            0 => Const::Int(i64::from_le_bytes(self.take(8)?.try_into().ok()?)),
            1 => Const::Float(self.u64()?),
            2 => Const::Bool(self.bool()?),
            3 => Const::Sized {
                repr: self.repr()?,
                bits: self.u64()?,
            },
            4 => Const::Str(StrId::from_u32(self.u32()?)),
            5 => Const::Nil,
            6 => Const::StaticCtor(self.def()?),
            7 => Const::FnValue(self.def()?),
            8 => Const::Code(self.key()?),
            9 => Const::EmptyList,
            10 => Const::Static(StaticId::from_u32(self.u32()?)),
            11 => Const::BuiltinDesc(*BUILTIN_DESCS.get(usize::from(self.u8()?))?),
            12 => Const::Ctx,
            13 => Const::ErrNil,
            _ => return None,
        })
    }

    fn key(&mut self) -> Option<FnKey> {
        Some(match self.u8()? {
            0 => FnKey::Item(self.def()?),
            1 => FnKey::ValueShim(self.def()?),
            2 => FnKey::Closure {
                owner: self.def()?,
                index: self.u32()?,
            },
            3 => FnKey::ConstInit(self.def()?),
            4 => FnKey::Test(self.def()?),
            5 => FnKey::Entry(match self.u8()? {
                0 => EntryKind::Main(self.def()?),
                1 => EntryKind::Test(self.def()?),
                2 => EntryKind::Const(self.def()?),
                3 => EntryKind::Repl(self.u32()?),
                _ => return None,
            }),
            6 => FnKey::Trampoline(self.u32()?),
            7 => FnKey::TupleDictFn {
                iface_fn: self.def()?,
                arity: self.u32()?,
            },
            8 => FnKey::DynForward {
                iface_fn: self.def()?,
                dyn_iface: self.def()?,
            },
            9 => FnKey::Continuation {
                owner: self.def()?,
                index: self.u32()?,
            },
            _ => return None,
        })
    }

    fn layout(&mut self) -> Option<LayoutRef> {
        Some(match self.u8()? {
            0 => LayoutRef::Builtin(match self.u8()? {
                0 => BuiltinLayout::String,
                1 => BuiltinLayout::Bytes,
                2 => BuiltinLayout::BoxedInt,
                3 => BuiltinLayout::BoxedFloat,
                4 => BuiltinLayout::BoxedF32,
                5 => BuiltinLayout::BoxedU64,
                _ => return None,
            }),
            1 => LayoutRef::Ctor(self.def()?),
            2 => LayoutRef::Tuple(self.u32()?),
            3 => LayoutRef::Closure(self.u32()?),
            4 => LayoutRef::Dict {
                fns: self.u32()?,
                embedded: self.u32()?,
            },
            5 => LayoutRef::Dyn,
            6 => LayoutRef::Desc { args: self.u32()? },
            7 => LayoutRef::DescNominal,
            _ => return None,
        })
    }

    fn repr(&mut self) -> Option<Repr> {
        REPRS.get(usize::from(self.u8()?)).copied()
    }

    fn ctype(&mut self) -> Option<CType> {
        Some(match self.u8()? {
            0 => CType::I8,
            1 => CType::I16,
            2 => CType::I32,
            3 => CType::I64,
            4 => CType::U8,
            5 => CType::U16,
            6 => CType::U32,
            7 => CType::U64,
            8 => CType::F32,
            9 => CType::F64,
            10 => CType::Bool,
            11 => CType::Ptr,
            12 => CType::Void,
            _ => return None,
        })
    }

    fn arith(&mut self) -> Option<ArithOp> {
        Some(match self.u8()? {
            0 => ArithOp::Add,
            1 => ArithOp::Sub,
            2 => ArithOp::Mul,
            3 => ArithOp::Div,
            4 => ArithOp::Rem,
            5 => ArithOp::Shl,
            6 => ArithOp::Shr,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_bytes_tables_follow_their_tags() {
        for (n, &repr) in REPRS.iter().enumerate() {
            assert_eq!(usize::from(repr_tag(repr)), n, "{repr:?}");
        }
        for (n, &d) in BUILTIN_DESCS.iter().enumerate() {
            assert_eq!(usize::from(builtin_desc_tag(d)), n, "{d:?}");
        }
        for (n, &kind) in PanicKind::ALL.iter().enumerate() {
            assert_eq!(usize::try_from(kind.code()).ok(), Some(n), "{kind:?}");
        }
    }

    /// A body with each operation of the instruction kernels
    /// (`crate::kernels_instr`) decodes to itself. Std's exports cover
    /// `Convert` and `Wrapping` too, but no exported std body takes a
    /// square root.
    #[test]
    fn export_bytes_round_trip_instruction_kernels() {
        let def = DefId::new(ModuleId::from_u32(1), DefIndex::from_u32(0));
        let mut b = crate::builder::FunctionBuilder::new(
            FnKey::Item(def),
            Abi::Cheby,
            Repr::Value,
            PLACEHOLDER_SPAN,
        )
        .unwrap();
        let n = b.add_param(Repr::I8, None, None).unwrap();
        let x = b.add_param(Repr::Float, None, None).unwrap();
        let wide = b
            .assign(
                Repr::Int,
                Rvalue::Convert {
                    value: Operand::Local(n),
                    from: Repr::I8,
                    to: Repr::Int,
                },
            )
            .unwrap();
        let product = b
            .assign(
                Repr::Int,
                Rvalue::Wrapping {
                    op: ArithOp::Mul,
                    lhs: Operand::Local(wide),
                    rhs: Operand::Const(Const::Int(-3)),
                    repr: Repr::Int,
                },
            )
            .unwrap();
        let root = b
            .assign(
                Repr::Float,
                Rvalue::FloatSqrt {
                    value: Operand::Local(x),
                    repr: Repr::Float,
                },
            )
            .unwrap();
        let _ = product;
        let boxed = b
            .assign(
                Repr::Value,
                Rvalue::Box {
                    value: Operand::Local(root),
                    from: Repr::Float,
                },
            )
            .unwrap();
        b.terminate(Terminator::Return(Operand::Local(boxed)))
            .unwrap();
        let mut body = Exported {
            def,
            function: b.finish().unwrap(),
            closures: Vec::new(),
            shared: Vec::new(),
            statics: Vec::new(),
            source: None,
        };
        portable_form(&mut body);
        let portable = PortableBody {
            body,
            names: vec![(def, "app".into(), "f".into())],
        };
        let bodies: PortableBodies =
            std::iter::once(((Box::from("app"), Box::from("f")), portable)).collect();
        let bytes = encode_bodies(&bodies);
        assert_eq!(decode_bodies(&bytes).unwrap(), bodies);
    }

    #[test]
    fn export_bytes_reject_damage() {
        let empty = PortableBodies::new();
        let bytes = encode_bodies(&empty);
        assert_eq!(decode_bodies(&bytes).unwrap(), empty);
        assert!(decode_bodies(&bytes[..bytes.len() - 1]).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(decode_bodies(&longer).is_err());
        let mut other = bytes;
        other[0] ^= 1;
        assert!(decode_bodies(&other).is_err());
    }
}
