//! Objects and uniform words (E1, D6): boxing (D-521), allocation and
//! fields (D-454, D-524), constructor tags (D-522), constant slots (E6),
//! runtime helpers (D-520) and reference counting (D-273).

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{BlockArg, InstBuilder, MemFlagsData, Value, types};

use cheby_iface::{DefId, builtin};
use cheby_mir::{BuiltinLayout, Const, LayoutRef, Local, Operand, Repr, RuntimeFn, TagType};
use cheby_runtime::abi;

use crate::CodegenError;
use crate::data::is_user_ctor;
use crate::decls::{ctor_header, escaped_count_index, header};
use crate::func::{Translator, unsupported};
use crate::layout::builtin_ctor_layout;
use crate::runtime::RuntimeFunc;
use crate::sig::repr_type;

/// The size of a header or field slot, in bytes.
const WORD_SIZE: i64 = 8;

/// The smallest `U64` that does not fit a tagged word: 2^62 (D-521).
const U64_TAGGED_LIMIT: i64 = 1 << 62;

/// The most value fields the hit path of a release with a keep list, or
/// of a drop specialized by constructor, releases inline; more go through
/// `cheby_rt_release_fields` (D-850).
const INLINE_RELEASE_CAP: usize = 8;

/// The slots of a release with a keep list (D-850): the
/// offsets of the kept fields, each a value field of `shape`, and those of
/// the value fields not kept, in declaration order.
fn kept_slots(
    shape: &Shape,
    layout: LayoutRef,
    keep: &[(u32, Local)],
) -> Result<(Vec<i32>, Vec<i32>), CodegenError> {
    let mut kept = Vec::with_capacity(keep.len());
    for &(i, _) in keep {
        match usize::try_from(i).ok().and_then(|n| shape.fields.get(n)) {
            Some(&(offset, Repr::Value)) => kept.push(offset),
            _ => {
                return Err(CodegenError::ice(format!(
                    "a keep list names field {i} of {layout:?}, which is not a value field"
                )));
            }
        }
    }
    let rest = shape
        .fields
        .iter()
        .filter(|&&(offset, repr)| repr == Repr::Value && !kept.contains(&offset))
        .map(|&(offset, _)| offset)
        .collect();
    Ok((kept, rest))
}

/// Where an allocation's header word comes from.
enum HeaderSource {
    /// A fixed layout: the header is a constant.
    Fixed(u64),
    /// A user constructor: the header is loaded from its `cb$layout$` word,
    /// so no code embeds a user layout id (D-454).
    Word(cranelift_module::DataId),
}

/// The shape of objects of one layout: how allocation finds the header,
/// the field slots in declaration order, and the size.
struct Shape {
    header: HeaderSource,
    /// `(offset, repr)` of each field, in declaration order.
    fields: Vec<(i32, Repr)>,
    /// The size in bytes.
    size: i64,
    /// For an object with more fields than a scan count holds, the offset
    /// and value of its field count, the number of fields after it, stored
    /// as a tagged integer (D-279, `escaped_count_index`).
    escaped: Option<(i32, u64)>,
}

fn offset(index: u64, first: i64) -> Result<i32, CodegenError> {
    i64::try_from(index)
        .ok()
        .and_then(|n| n.checked_mul(WORD_SIZE))
        .and_then(|n| n.checked_add(first))
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| CodegenError::ice("an object too large to address"))
}

/// Uniform fields `0..count`, starting at offset `first`.
fn uniform_fields(count: u64, first: i64) -> Result<Vec<(i32, Repr)>, CodegenError> {
    (0..count)
        .map(|n| Ok((offset(n, first)?, Repr::Value)))
        .collect()
}

/// The tagged word `(n << 1) | 1` of a constant `Int`, `I64` or `U64`
/// operand boxed from `from`, when it fits a tagged word (D-521); `None`
/// for anything else, which [`Translator::box_value`] checks at run time.
fn constant_tagged(value: &Operand, from: Repr) -> Option<i64> {
    let Operand::Const(c) = value else {
        return None;
    };
    let fits = match (c, from) {
        (Const::Int(n), Repr::Int) => Some(*n).filter(|n| n.wrapping_shl(1) >> 1 == *n),
        (
            Const::Sized {
                repr: Repr::I64,
                bits,
            },
            Repr::I64,
        ) => Some(bits.cast_signed()).filter(|n| n.wrapping_shl(1) >> 1 == *n),
        (
            Const::Sized {
                repr: Repr::U64,
                bits,
            },
            Repr::U64,
        ) => i64::try_from(*bits).ok().filter(|n| *n < U64_TAGGED_LIMIT),
        _ => None,
    }?;
    Some(fits.wrapping_shl(1) | 1)
}

/// The offset of field `field` of a tuple of `layout`, the same as its
/// shape's ([`Translator::shape`]: above [`abi::SCAN_MAX`] fields the
/// escaped count comes first, D-279, D-555), without building the shape,
/// whose size is the arity.
fn tuple_field_offset(layout: LayoutRef, field: u32) -> Result<i32, CodegenError> {
    let LayoutRef::Tuple(arity) = layout else {
        return Err(CodegenError::ice(format!("{layout:?} is not a tuple")));
    };
    if field >= arity {
        return Err(CodegenError::ice(format!(
            "field {field} of a tuple of {arity}"
        )));
    }
    let index = match escaped_count_index(layout, u64::from(arity)) {
        None => u64::from(field),
        Some(0) => u64::from(field).saturating_add(1),
        Some(_) => return Err(CodegenError::ice("a tuple's escaped count is not first")),
    };
    offset(index, i64::from(abi::FIRST_FIELD_OFFSET))
}

fn size_of(fields: &[(i32, Repr)], first: i64) -> Result<i64, CodegenError> {
    let words = i64::try_from(fields.len()).map_err(|_| CodegenError::ice("too many fields"))?;
    words
        .checked_mul(WORD_SIZE)
        .and_then(|n| n.checked_add(first))
        .ok_or_else(|| CodegenError::ice("an object too large to address"))
}

impl Translator<'_, '_> {
    /// The shape of objects of `layout` (plan §5.2, §5.4).
    fn shape(&self, layout: LayoutRef) -> Result<Shape, CodegenError> {
        let first = i64::from(abi::FIRST_FIELD_OFFSET);
        let fixed = |fields: Vec<(i32, Repr)>, value_fields: u64, id: u32| {
            Ok::<_, CodegenError>(Shape {
                header: HeaderSource::Fixed(header(abi::RC_UNIQUE, value_fields, id)?),
                size: size_of(&fields, first)?,
                fields,
                escaped: None,
            })
        };
        match layout {
            LayoutRef::Ctor(def) => {
                let fields: Vec<(i32, Repr)> = if let Some(desc) = self.unit.layout(layout) {
                    desc.fields.iter().map(|f| (f.offset, f.repr)).collect()
                } else if [builtin::SOME, builtin::OK, builtin::ERR].contains(&def) {
                    vec![(abi::FIRST_FIELD_OFFSET, Repr::Value)]
                } else {
                    return Err(CodegenError::ice(format!(
                        "the unit has no layout for the constructor {def:?}"
                    )));
                };
                if is_user_ctor(&layout) {
                    let word = self.decls.layout(layout)?.word;
                    Ok(Shape {
                        header: HeaderSource::Word(word),
                        size: size_of(&fields, first)?,
                        fields,
                        escaped: None,
                    })
                } else {
                    let id = builtin_ctor_layout(def)
                        .ok_or_else(|| CodegenError::ice("a builtin constructor without an id"))?;
                    let values = fields.iter().filter(|(_, r)| *r == Repr::Value).count();
                    let values = u64::try_from(values).unwrap_or(u64::MAX);
                    Ok(Shape {
                        header: HeaderSource::Fixed(ctor_header(abi::RC_UNIQUE, values, id)),
                        size: size_of(&fields, first)?,
                        fields,
                        escaped: None,
                    })
                }
            }
            LayoutRef::Tuple(_)
            | LayoutRef::Closure(_)
            | LayoutRef::Dict { .. }
            | LayoutRef::Desc { .. }
                if layout
                    .uniform_fields()
                    .and_then(|count| escaped_count_index(layout, count))
                    .is_some() =>
            {
                Self::escaped_shape(layout)
            }
            LayoutRef::Tuple(_)
            | LayoutRef::Closure(_)
            | LayoutRef::Dict { .. }
            | LayoutRef::Dyn
            | LayoutRef::Desc { .. }
            | LayoutRef::DescNominal => {
                let count = layout
                    .uniform_fields()
                    .ok_or_else(|| CodegenError::ice("a uniform layout without a count"))?;
                let id = crate::layout::builtin_layout_id(&layout)
                    .ok_or_else(|| CodegenError::ice("a runtime layout without an id"))?;
                fixed(uniform_fields(count, first)?, count, id)
            }
            LayoutRef::Builtin(b) => {
                let (repr, id) = match b {
                    BuiltinLayout::BoxedInt => (Repr::Int, abi::layout::BOXED_INT),
                    BuiltinLayout::BoxedU64 => (Repr::U64, abi::layout::BOXED_U64),
                    BuiltinLayout::BoxedFloat => (Repr::Float, abi::layout::BOXED_FLOAT),
                    BuiltinLayout::BoxedF32 => (Repr::F32, abi::layout::BOXED_F32),
                    BuiltinLayout::String | BuiltinLayout::Bytes => {
                        return Err(unsupported(&format!("`alloc` or `field` of {b:?}")));
                    }
                };
                fixed(vec![(abi::FIRST_FIELD_OFFSET, repr)], 0, id)
            }
        }
    }

    /// The shape of an object of `layout` whose uniform fields are more
    /// than a scan count holds (D-279): the escape value as its scan count,
    /// and the tagged count of the fields after it at the index that
    /// [`escaped_count_index`] names; the fields before and after it keep
    /// their order (D-555).
    fn escaped_shape(layout: LayoutRef) -> Result<Shape, CodegenError> {
        let first = i64::from(abi::FIRST_FIELD_OFFSET);
        let count = layout
            .uniform_fields()
            .ok_or_else(|| CodegenError::ice("a uniform layout without a count"))?;
        let index = escaped_count_index(layout, count)
            .ok_or_else(|| CodegenError::ice("an object that needs no escaped count"))?;
        let id = crate::layout::builtin_layout_id(&layout)
            .ok_or_else(|| CodegenError::ice("a runtime layout without an id"))?;
        let before = u64::try_from(index).map_err(|_| CodegenError::ice("a huge index"))?;
        let after = count
            .checked_sub(before)
            .ok_or_else(|| CodegenError::ice("an escaped object without its first field"))?;
        // The fields before the count, the count word, then the rest.
        let mut fields = uniform_fields(before, first)?;
        let count_offset = offset(before, first)?;
        let rest = i64::from(count_offset).saturating_add(WORD_SIZE);
        fields.extend(uniform_fields(after, rest)?);
        Ok(Shape {
            header: HeaderSource::Fixed(abi::header(abi::RC_UNIQUE, abi::SCAN_ESCAPE, id)),
            size: size_of(&fields, first.saturating_add(WORD_SIZE))?,
            fields,
            escaped: Some((count_offset, after)),
        })
    }

    /// A new object of `layout` with owned `fields` (D-454): the header,
    /// `cheby_rt_alloc`, then the fields at their offsets.
    pub(crate) fn alloc(
        &mut self,
        layout: LayoutRef,
        fields: &[Operand],
    ) -> Result<Value, CodegenError> {
        let shape = self.shape(layout)?;
        if shape.fields.len() != fields.len() {
            return Err(CodegenError::ice(format!(
                "`alloc` of {layout:?} with {} fields instead of {}",
                fields.len(),
                shape.fields.len()
            )));
        }
        let ctx = self.ctx()?;
        let header = match shape.header {
            HeaderSource::Fixed(word) => self.b.ins().iconst(types::I64, word.cast_signed()),
            HeaderSource::Word(data) => {
                let address = self.data_address(data, true);
                self.b
                    .ins()
                    .load(types::I64, MemFlagsData::trusted(), address, 0)
            }
        };
        let size = self.b.ins().iconst(types::I64, shape.size);
        let object = self.edge_runtime_value(RuntimeFunc::Alloc, &[ctx, size, header])?;
        if let Some((at, count)) = shape.escaped {
            let tagged = abi::tag_int(i64::try_from(count).unwrap_or(i64::MAX))
                .ok_or_else(|| CodegenError::ice("a field count out of range"))?;
            let tagged = self.b.ins().iconst(types::I64, tagged.cast_signed());
            self.b
                .ins()
                .store(MemFlagsData::trusted(), tagged, object, at);
        }
        for (&(offset, repr), op) in shape.fields.iter().zip(fields) {
            let value = self.operand(op)?;
            self.store_field(object, offset, repr, value);
        }
        Ok(object)
    }

    /// The shape of a reused layout (D-716), after checking that its size
    /// is the one MIR paired by ([`cheby_mir::layout::object_size`], the
    /// one size function): the runtime frees the block with the size its
    /// header implies, so a disagreement would be undefined behavior.
    fn reuse_shape(&self, layout: LayoutRef) -> Result<Shape, CodegenError> {
        let shape = self.shape(layout)?;
        let size = u64::try_from(shape.size).ok();
        let rounded = size.and_then(|s| {
            s.max(abi::HEADER_SIZE)
                .checked_next_multiple_of(abi::OBJECT_ALIGN)
        });
        let mir = cheby_mir::layout::object_size(self.unit, layout);
        if rounded.is_none() || rounded != mir {
            return Err(CodegenError::ice(format!(
                "the size of {layout:?} is {rounded:?} bytes in code generation and {mir:?} in MIR"
            )));
        }
        Ok(shape)
    }

    /// The header word of a new object of `shape`.
    fn header_value(&mut self, shape: &Shape) -> Value {
        match shape.header {
            HeaderSource::Fixed(word) => self.b.ins().iconst(types::I64, word.cast_signed()),
            HeaderSource::Word(data) => {
                let address = self.data_address(data, true);
                self.b
                    .ins()
                    .load(types::I64, MemFlagsData::trusted(), address, 0)
            }
        }
    }

    /// `drop_reuse local of layout` (D-716): the token. Inline, an object
    /// of count 1 is kept: its value fields are released and zeroed by
    /// `cheby_rt_release_fields` (only when the layout has any), and the
    /// token is the object. A count above 1 is decremented, a static
    /// object (count 0) is skipped, and a shared one (negative) goes to
    /// `cheby_rt_drop`, as `drop` does; the token is then 0. The local is
    /// an object, never a tagged integer: a field read of its layout
    /// dominates the drop. With `checked_rc`, the checked runtime entry,
    /// which also checks the header and the size.
    ///
    /// With a keep list (D-850): on a hit the kept fields are
    /// zeroed, their references moving to their projections, and the
    /// others released ([`Self::release_on_hit`]); on a miss each kept
    /// projection is `dup`'d before the reference is given up. With
    /// `checked_rc`, the `dup`s and then the checked entry.
    pub(crate) fn drop_reuse(
        &mut self,
        local: Local,
        layout: LayoutRef,
        keep: &[(u32, Local)],
    ) -> Result<Value, CodegenError> {
        let shape = self.reuse_shape(layout)?;
        let v = self.local(local)?;
        let ctx = self.ctx()?;
        if self.opts.checked_rc {
            self.dup_kept(keep)?;
            let size = self.b.ins().iconst(types::I64, shape.size);
            return self.runtime_value(RuntimeFunc::DropReuseChecked, &[ctx, v, size]);
        }
        let (kept, rest) = kept_slots(&shape, layout, keep)?;
        let has_values = shape.fields.iter().any(|&(_, r)| r == Repr::Value);
        let hit = self.b.create_block();
        let not_unique = self.b.create_block();
        let fast = self.b.create_block();
        let other = self.b.create_block();
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let join = self.b.create_block();
        let token = self.b.append_block_param(join, types::I64);
        let zero = self.b.ins().iconst(types::I64, 0);
        let rc = self.load_rc(v);
        let unique = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, rc, i64::from(abi::RC_UNIQUE));
        self.b.ins().brif(unique, hit, &[], not_unique, &[]);
        self.b.switch_to_block(hit);
        if keep.is_empty() {
            if has_values {
                self.runtime_call(RuntimeFunc::ReleaseFields, &[ctx, v])?;
            }
        } else {
            self.release_on_hit(v, &kept, &rest, true)?;
        }
        self.b.ins().jump(join, &[BlockArg::Value(v)]);
        self.b.switch_to_block(not_unique);
        self.dup_kept(keep)?;
        let shared_by_others = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, rc, 1);
        self.b.ins().brif(shared_by_others, fast, &[], other, &[]);
        self.b.switch_to_block(fast);
        let next = self.b.ins().iadd_imm_s(rc, -1);
        self.store_rc(v, next);
        self.b.ins().jump(join, &[BlockArg::Value(zero)]);
        self.b.switch_to_block(other);
        let static_object = self.b.ins().icmp_imm_s(IntCC::Equal, rc, 0);
        self.b
            .ins()
            .brif(static_object, join, &[BlockArg::Value(zero)], slow, &[]);
        self.b.switch_to_block(slow);
        self.drop_call(v)?;
        self.b.ins().jump(join, &[BlockArg::Value(zero)]);
        self.b.switch_to_block(join);
        Ok(token)
    }

    /// `alloc_reuse token, layout(fields)` (D-716): a nonzero token gets
    /// the new header (it already has count 1 and was charged when it was
    /// allocated) and the hit is counted out of line while the counting
    /// flag is not off; a 0 token allocates through
    /// `cheby_rt_alloc_miss`, with `alloc`'s unwind edge. Both then store
    /// the fields as [`Self::alloc`] does. With `checked_rc`, the checked
    /// runtime entry, which checks the token and both sizes.
    pub(crate) fn alloc_reuse(
        &mut self,
        token: Local,
        layout: LayoutRef,
        fields: &[Operand],
    ) -> Result<Value, CodegenError> {
        let shape = self.reuse_shape(layout)?;
        if shape.fields.len() != fields.len() {
            return Err(CodegenError::ice(format!(
                "`alloc_reuse` of {layout:?} with {} fields instead of {}",
                fields.len(),
                shape.fields.len()
            )));
        }
        let t = self.local(token)?;
        let ctx = self.ctx()?;
        let header = self.header_value(&shape);
        let size = self.b.ins().iconst(types::I64, shape.size);
        let object = if self.opts.checked_rc {
            self.edge_runtime_value(RuntimeFunc::AllocReuseChecked, &[ctx, t, size, header])?
        } else {
            let hit = self.b.create_block();
            let count = self.b.create_block();
            self.b.set_cold_block(count);
            let miss = self.b.create_block();
            let join = self.b.create_block();
            let object = self.b.append_block_param(join, types::I64);
            self.b.ins().brif(t, hit, &[], miss, &[]);
            self.b.switch_to_block(hit);
            self.b
                .ins()
                .store(MemFlagsData::trusted(), header, t, abi::HEADER_RC_OFFSET);
            let flag_address = self.static_object(abi::sym::STATS_COUNTING)?;
            let flag = self
                .b
                .ins()
                .load(types::I8, MemFlagsData::trusted(), flag_address, 0);
            let off =
                self.b
                    .ins()
                    .icmp_imm_s(IntCC::Equal, flag, i64::from(abi::sym::COUNTING_OFF));
            self.b
                .ins()
                .brif(off, join, &[BlockArg::Value(t)], count, &[]);
            self.b.switch_to_block(count);
            self.runtime_call(RuntimeFunc::ReuseHit, &[])?;
            self.b.ins().jump(join, &[BlockArg::Value(t)]);
            self.b.switch_to_block(miss);
            let fresh = self.edge_runtime_value(RuntimeFunc::AllocMiss, &[ctx, size, header])?;
            self.b.ins().jump(join, &[BlockArg::Value(fresh)]);
            self.b.switch_to_block(join);
            object
        };
        if let Some((at, count)) = shape.escaped {
            let tagged = abi::tag_int(i64::try_from(count).unwrap_or(i64::MAX))
                .ok_or_else(|| CodegenError::ice("a field count out of range"))?;
            let tagged = self.b.ins().iconst(types::I64, tagged.cast_signed());
            self.b
                .ins()
                .store(MemFlagsData::trusted(), tagged, object, at);
        }
        for (&(offset, repr), op) in shape.fields.iter().zip(fields) {
            let value = self.operand(op)?;
            self.store_field(object, offset, repr, value);
        }
        Ok(object)
    }

    /// `dup` of each projection a keep list names (D-850): the
    /// miss path of a release with one, and its form under `checked_rc`.
    fn dup_kept(&mut self, keep: &[(u32, Local)]) -> Result<(), CodegenError> {
        for &(_, p) in keep {
            let value = self.local(p)?;
            self.dup(value)?;
        }
        Ok(())
    }

    /// The hit path of a release with a keep list (D-850): `v`
    /// is unique. The slots `kept` hand their references to the kept
    /// projections; the value slots `rest` are released, inline when there
    /// are at most [`INLINE_RELEASE_CAP`], else by
    /// `cheby_rt_release_fields` after the kept slots are zeroed. With
    /// `zero`, every value slot ends zeroed (a reuse token owns nothing);
    /// without it, the inline path leaves them, since the block is freed
    /// next without being read.
    fn release_on_hit(
        &mut self,
        v: Value,
        kept: &[i32],
        rest: &[i32],
        zero: bool,
    ) -> Result<(), CodegenError> {
        let inline = rest.len() <= INLINE_RELEASE_CAP;
        if zero || !inline {
            let null = self.b.ins().iconst(types::I64, 0);
            for &offset in kept {
                self.b.ins().store(MemFlagsData::trusted(), null, v, offset);
            }
        }
        if !inline {
            let ctx = self.ctx()?;
            self.runtime_call(RuntimeFunc::ReleaseFields, &[ctx, v])?;
            return Ok(());
        }
        for &offset in rest {
            let child = self
                .b
                .ins()
                .load(types::I64, MemFlagsData::trusted(), v, offset);
            if zero {
                let null = self.b.ins().iconst(types::I64, 0);
                self.b.ins().store(MemFlagsData::trusted(), null, v, offset);
            }
            self.drop_value(child)?;
        }
        Ok(())
    }

    /// `drop local of layout keep [...]`, a drop specialized by constructor
    /// (D-716, D-850). Inline, an object of count 1 hands each
    /// kept field's reference to its projection, releases its other value
    /// fields ([`Self::release_on_hit`]) and frees its block through
    /// `cheby_rt_free_unique`; any other count first `dup`s each kept
    /// projection, then gives up a reference as `drop` does: a count above
    /// 1 is decremented, a static object (count 0) is skipped and a shared
    /// one goes to `cheby_rt_drop`. A field read of `layout` pins the
    /// object, so it is never a tagged integer. With `checked_rc`, the
    /// `dup`s and the checked `drop`.
    pub(crate) fn drop_keep(
        &mut self,
        local: Local,
        layout: LayoutRef,
        keep: &[(u32, Local)],
    ) -> Result<(), CodegenError> {
        let v = self.local(local)?;
        if self.opts.checked_rc {
            self.dup_kept(keep)?;
            return self.drop_call(v);
        }
        let shape = self.shape(layout)?;
        let (kept, rest) = kept_slots(&shape, layout, keep)?;
        let ctx = self.ctx()?;
        let hit = self.b.create_block();
        let not_unique = self.b.create_block();
        let fast = self.b.create_block();
        let other = self.b.create_block();
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let done = self.b.create_block();
        let rc = self.load_rc(v);
        let unique = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, rc, i64::from(abi::RC_UNIQUE));
        self.b.ins().brif(unique, hit, &[], not_unique, &[]);
        self.b.switch_to_block(hit);
        self.release_on_hit(v, &kept, &rest, false)?;
        self.runtime_call(RuntimeFunc::FreeUnique, &[ctx, v])?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(not_unique);
        self.dup_kept(keep)?;
        let shared_by_others = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, rc, 1);
        self.b.ins().brif(shared_by_others, fast, &[], other, &[]);
        self.b.switch_to_block(fast);
        let next = self.b.ins().iadd_imm_s(rc, -1);
        self.store_rc(v, next);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(other);
        let static_object = self.b.ins().icmp_imm_s(IntCC::Equal, rc, 0);
        self.b.ins().brif(static_object, done, &[], slow, &[]);
        self.b.switch_to_block(slow);
        self.drop_call(v)?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        Ok(())
    }

    /// A `drop` of a reuse token outside a pad (D-716): 0 is skipped,
    /// anything else is dropped as [`Self::drop_value`] does.
    pub(crate) fn drop_token(&mut self, v: Value) -> Result<(), CodegenError> {
        let object = self.b.create_block();
        let done = self.b.create_block();
        self.b.ins().brif(v, object, &[], done, &[]);
        self.b.switch_to_block(object);
        self.drop_value(v)?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        Ok(())
    }

    /// A new tuple of `layout` whose every field is the static `Nil`
    /// (D-720, D-810): the header and `cheby_rt_alloc` as
    /// in [`Self::alloc`], then a loop that stores the placeholder in each
    /// field slot, so the code does not grow with the arity.
    pub(crate) fn alloc_placeholder(&mut self, layout: LayoutRef) -> Result<Value, CodegenError> {
        if !matches!(layout, LayoutRef::Tuple(_)) {
            return Err(CodegenError::ice(format!(
                "`alloc_placeholder` of {layout:?}, which is not a tuple"
            )));
        }
        let shape = self.shape(layout)?;
        let (Some(&(first, _)), Some(&(last, _))) = (shape.fields.first(), shape.fields.last())
        else {
            return Err(CodegenError::ice("`alloc_placeholder` of an empty tuple"));
        };
        // The slots are contiguous: the loop below steps through them.
        let span = i64::try_from(shape.fields.len().saturating_sub(1))
            .ok()
            .and_then(|n| n.checked_mul(WORD_SIZE));
        if span != i64::from(last).checked_sub(i64::from(first)) {
            return Err(CodegenError::ice(format!(
                "the fields of {layout:?} are not contiguous"
            )));
        }
        let HeaderSource::Fixed(word) = shape.header else {
            return Err(CodegenError::ice("a tuple's header is not fixed"));
        };
        let ctx = self.ctx()?;
        let header = self.b.ins().iconst(types::I64, word.cast_signed());
        let size = self.b.ins().iconst(types::I64, shape.size);
        let object = self.edge_runtime_value(RuntimeFunc::Alloc, &[ctx, size, header])?;
        if let Some((at, count)) = shape.escaped {
            let tagged = abi::tag_int(i64::try_from(count).unwrap_or(i64::MAX))
                .ok_or_else(|| CodegenError::ice("a field count out of range"))?;
            let tagged = self.b.ins().iconst(types::I64, tagged.cast_signed());
            self.b
                .ins()
                .store(MemFlagsData::trusted(), tagged, object, at);
        }
        let nil = self.static_object(abi::sym::STATIC_NIL)?;
        let start = self.b.ins().iadd_imm_s(object, i64::from(first));
        let end = self.b.ins().iadd_imm_s(object, i64::from(last));
        let fill = self.b.create_block();
        let slot = self.b.append_block_param(fill, types::I64);
        let done = self.b.create_block();
        self.b.ins().jump(fill, &[BlockArg::Value(start)]);
        self.b.switch_to_block(fill);
        self.b.ins().store(MemFlagsData::trusted(), nil, slot, 0);
        let next = self.b.ins().iadd_imm_s(slot, WORD_SIZE);
        let more = self.b.ins().icmp(IntCC::UnsignedLessThanOrEqual, next, end);
        self.b
            .ins()
            .brif(more, fill, &[BlockArg::Value(next)], done, &[]);
        self.b.switch_to_block(done);
        Ok(object)
    }

    /// Stores the owned `value` into field `field` of `obj`, a tuple of
    /// `layout` still holding the placeholder there (D-720): a plain store,
    /// since the placeholder holds no reference. The offset is computed
    /// directly, so a store costs the same whatever the arity.
    pub(crate) fn init_field(
        &mut self,
        obj: Local,
        layout: LayoutRef,
        field: u32,
        value: &Operand,
    ) -> Result<(), CodegenError> {
        let offset = tuple_field_offset(layout, field)?;
        let object = self.local(obj)?;
        let value = self.operand(value)?;
        self.store_field(object, offset, Repr::Value, value);
        Ok(())
    }

    /// Stores a field in its 8-byte slot: uniform words and 64-bit numbers
    /// as they are, narrower integers and `Bool` extended to 64 bits by
    /// their signedness, an `F32` in the low 4 bytes with the rest zero
    /// (plan §5.2).
    fn store_field(&mut self, object: Value, offset: i32, repr: Repr, value: Value) {
        let flags = MemFlagsData::trusted();
        let word = match repr {
            Repr::Value | Repr::Int | Repr::I64 | Repr::U64 | Repr::Float => value,
            Repr::I8 | Repr::I16 | Repr::I32 => self.b.ins().sextend(types::I64, value),
            Repr::Bool | Repr::U8 | Repr::U16 | Repr::U32 => {
                self.b.ins().uextend(types::I64, value)
            }
            Repr::F32 => {
                let bits = self.b.ins().bitcast(types::I32, MemFlagsData::new(), value);
                self.b.ins().uextend(types::I64, bits)
            }
        };
        self.b.ins().store(flags, word, object, offset);
    }

    /// Field `field` of `base` by the offsets of its layout. A uniform
    /// field is borrowed from the base (the RC pass dups it).
    pub(crate) fn field(
        &mut self,
        base: Local,
        layout: LayoutRef,
        field: u32,
    ) -> Result<Value, CodegenError> {
        // A tuple's offset without its shape, whose size is the arity
        // (D-720's tuples can be large).
        let (offset, repr) = if let LayoutRef::Tuple(_) = layout {
            (tuple_field_offset(layout, field)?, Repr::Value)
        } else {
            let shape = self.shape(layout)?;
            *usize::try_from(field)
                .ok()
                .and_then(|n| shape.fields.get(n))
                .ok_or_else(|| {
                    CodegenError::ice(format!("`field` {field} of {layout:?} does not exist"))
                })?
        };
        let base = self.local(base)?;
        Ok(self
            .b
            .ins()
            .load(repr_type(repr), MemFlagsData::trusted(), base, offset))
    }

    /// The declaration-order tag of `base` (D-522): its layout id minus the
    /// id of the type's first constructor, read from that constructor's
    /// `cb$layout$` word for a user type, or the fixed 22, 24 or 19.
    pub(crate) fn read_tag(&mut self, base: &Operand, ty: TagType) -> Result<Value, CodegenError> {
        let base = self.operand(base)?;
        let id = self.layout_id(base);
        let first = match ty {
            TagType::Option => i64::from(abi::layout::SOME),
            TagType::Result => i64::from(abi::layout::OK),
            TagType::Order => i64::from(abi::layout::LESS),
            TagType::Adt(type_def) => {
                let first = self.first_ctor(type_def)?;
                let word = self.decls.layout(first)?.word;
                let address = self.data_address(word, true);
                let header = self
                    .b
                    .ins()
                    .load(types::I64, MemFlagsData::trusted(), address, 0);
                let shift = i64::from(abi::HEADER_LAYOUT_SHIFT);
                let first_id = self.b.ins().ushr_imm_u(header, shift);
                return Ok(self.b.ins().isub(id, first_id));
            }
        };
        Ok(self.b.ins().iadd_imm_s(id, first.wrapping_neg()))
    }

    fn first_ctor(&self, type_def: DefId) -> Result<LayoutRef, CodegenError> {
        self.unit
            .type_layouts(type_def)
            .first()
            .filter(|desc| desc.tag == 0)
            .map(|desc| desc.layout)
            .ok_or_else(|| {
                CodegenError::ice(format!(
                    "the unit has no first constructor of the type {type_def:?}"
                ))
            })
    }

    /// The value in the slot of the constant `def`, borrowed from the slot;
    /// an empty slot (0) asks `cheby_rt_const_missing(ctx, slot)` (E6),
    /// whose unwinding, when the load has an unwind edge, is tested only on
    /// that cold path (D-698).
    pub(crate) fn load_const(&mut self, def: DefId) -> Result<Value, CodegenError> {
        let slot = self.decls.const_data(def)?;
        let address = self.data_address(slot, true);
        let value = self
            .b
            .ins()
            .load(types::I64, MemFlagsData::trusted(), address, 0);
        let missing = self.b.create_block();
        self.b.set_cold_block(missing);
        let join = self.b.create_block();
        let result = self.b.append_block_param(join, types::I64);
        let empty = self.b.ins().icmp_imm_s(IntCC::Equal, value, 0);
        self.b
            .ins()
            .brif(empty, missing, &[], join, &[BlockArg::Value(value)]);
        self.b.switch_to_block(missing);
        let ctx = self.ctx()?;
        let forced = self.edge_runtime_value(RuntimeFunc::ConstMissing, &[ctx, address])?;
        self.b.ins().jump(join, &[BlockArg::Value(forced)]);
        self.b.switch_to_block(join);
        Ok(result)
    }

    /// A runtime helper (D-520), with `ctx` first where it takes one.
    pub(crate) fn call_helper(
        &mut self,
        helper: RuntimeFn,
        args: &[Operand],
    ) -> Result<Option<Value>, CodegenError> {
        let (f, takes_ctx) = RuntimeFunc::of(helper);
        let mut vals = Vec::with_capacity(args.len().saturating_add(1));
        if takes_ctx {
            vals.push(self.ctx()?);
        }
        vals.extend(self.operands(args)?);
        let result = self.edge_runtime_call(f, &vals)?;
        match (helper.ret(), result) {
            (Some(_), Some(v)) => Ok(Some(v)),
            (None, None) => Ok(None),
            _ => Err(CodegenError::ice(format!(
                "the runtime helper {helper:?} and its C signature disagree on a result"
            ))),
        }
    }

    /// A raw `from` value boxed canonically into a uniform word (D-521):
    /// tagged when it fits in 63 bits (in 62 for `U64`), else a boxed
    /// number from the runtime; floats always boxed; `Bool` as the static
    /// `True` or `False`. The result is owned.
    pub(crate) fn box_value(&mut self, value: &Operand, from: Repr) -> Result<Value, CodegenError> {
        if let Some(tagged) = constant_tagged(value, from) {
            // A constant that fits: its tagged word, without the check
            // and the cold block (a literal of many integers).
            return Ok(self.b.ins().iconst(types::I64, tagged));
        }
        let v = self.operand(value)?;
        Ok(match from {
            Repr::Int | Repr::I64 => {
                let shifted = self.b.ins().ishl_imm_u(v, 1);
                let back = self.b.ins().sshr_imm_u(shifted, 1);
                let fits = self.b.ins().icmp(IntCC::Equal, back, v);
                self.tag_or_call(fits, shifted, RuntimeFunc::BoxI64, v)?
            }
            Repr::U64 => {
                let fits = self
                    .b
                    .ins()
                    .icmp_imm_u(IntCC::UnsignedLessThan, v, U64_TAGGED_LIMIT);
                let shifted = self.b.ins().ishl_imm_u(v, 1);
                self.tag_or_call(fits, shifted, RuntimeFunc::BoxU64, v)?
            }
            Repr::I8 | Repr::I16 | Repr::I32 => {
                let wide = self.b.ins().sextend(types::I64, v);
                self.tag(wide)
            }
            Repr::U8 | Repr::U16 | Repr::U32 => {
                let wide = self.b.ins().uextend(types::I64, v);
                self.tag(wide)
            }
            Repr::Float | Repr::F32 => {
                let f = if from == Repr::Float {
                    RuntimeFunc::BoxF64
                } else {
                    RuntimeFunc::BoxF32
                };
                let ctx = self.ctx()?;
                self.edge_runtime_value(f, &[ctx, v])?
            }
            Repr::Bool => {
                let truth = self.static_object(abi::sym::STATIC_TRUE)?;
                let falsity = self.static_object(abi::sym::STATIC_FALSE)?;
                self.b.ins().select(v, truth, falsity)
            }
            Repr::Value => return Err(unsupported("`box` of a uniform word")),
        })
    }

    /// `(v << 1) | 1`.
    fn tag(&mut self, v: Value) -> Value {
        let shifted = self.b.ins().ishl_imm_u(v, 1);
        self.b.ins().bor_imm_u(shifted, 1)
    }

    /// `shifted | 1` when `fits`, else `f(ctx, raw)` in a cold block.
    fn tag_or_call(
        &mut self,
        fits: Value,
        shifted: Value,
        f: RuntimeFunc,
        raw: Value,
    ) -> Result<Value, CodegenError> {
        let tagged = self.b.ins().bor_imm_u(shifted, 1);
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let join = self.b.create_block();
        let result = self.b.append_block_param(join, types::I64);
        self.b
            .ins()
            .brif(fits, join, &[BlockArg::Value(tagged)], slow, &[]);
        self.b.switch_to_block(slow);
        let ctx = self.ctx()?;
        let boxed = self.edge_runtime_value(f, &[ctx, raw])?;
        self.b.ins().jump(join, &[BlockArg::Value(boxed)]);
        self.b.switch_to_block(join);
        Ok(result)
    }

    /// A uniform word (borrowed) unboxed into `to` (D-521).
    pub(crate) fn unbox(&mut self, value: &Operand, to: Repr) -> Result<Value, CodegenError> {
        let w = self.operand(value)?;
        let payload = abi::FIRST_FIELD_OFFSET;
        Ok(match to {
            Repr::Int | Repr::I64 | Repr::U64 => {
                // Tagged, or a boxed `Int`/`U64` holding the raw value.
                let tagged = self.b.ins().band_imm_u(w, 1);
                let boxed = self.b.create_block();
                let join = self.b.create_block();
                let result = self.b.append_block_param(join, types::I64);
                let untagged = self.b.ins().sshr_imm_u(w, 1);
                self.b
                    .ins()
                    .brif(tagged, join, &[BlockArg::Value(untagged)], boxed, &[]);
                self.b.switch_to_block(boxed);
                let raw = self
                    .b
                    .ins()
                    .load(types::I64, MemFlagsData::trusted(), w, payload);
                self.b.ins().jump(join, &[BlockArg::Value(raw)]);
                self.b.switch_to_block(join);
                result
            }
            Repr::I8 | Repr::I16 | Repr::I32 | Repr::U8 | Repr::U16 | Repr::U32 => {
                let untagged = self.b.ins().sshr_imm_u(w, 1);
                self.b.ins().ireduce(repr_type(to), untagged)
            }
            Repr::Float | Repr::F32 => {
                self.b
                    .ins()
                    .load(repr_type(to), MemFlagsData::trusted(), w, payload)
            }
            Repr::Bool => {
                let truth = self.static_object(abi::sym::STATIC_TRUE)?;
                self.b.ins().icmp(IntCC::Equal, w, truth)
            }
            Repr::Value => return Err(unsupported("`unbox` to a uniform word")),
        })
    }

    /// One more reference to the uniform word `v` (D-273). Inline: tagged
    /// integers and static objects are skipped and a local count below the
    /// maximum is incremented; anything else (a shared count, an overflow)
    /// calls `cheby_rt_dup`. With `checked_rc`, always the runtime call,
    /// which checks the header (plan §4.3.4).
    pub(crate) fn dup(&mut self, v: Value) -> Result<(), CodegenError> {
        if self.opts.checked_rc {
            self.runtime_call(RuntimeFunc::Dup, &[v])?;
            return Ok(());
        }
        let object = self.b.create_block();
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let done = self.b.create_block();
        let tagged = self.b.ins().band_imm_u(v, 1);
        self.b.ins().brif(tagged, done, &[], object, &[]);
        self.b.switch_to_block(object);
        let rc = self.load_rc(v);
        // A count in 1..=i32::MAX-1 is local and can grow: `rc - 1` below
        // `i32::MAX - 1` as an unsigned number.
        let minus_one = self.b.ins().iadd_imm_s(rc, -1);
        let local = self.b.ins().icmp_imm_u(
            IntCC::UnsignedLessThan,
            minus_one,
            i64::from(i32::MAX.saturating_sub(1)),
        );
        let static_object = self.b.ins().icmp_imm_s(IntCC::Equal, rc, 0);
        let skip = self.b.create_block();
        self.b.ins().brif(local, fast, &[], skip, &[]);
        self.b.switch_to_block(skip);
        self.b.ins().brif(static_object, done, &[], slow, &[]);
        self.b.switch_to_block(fast);
        let next = self.b.ins().iadd_imm_s(rc, 1);
        self.store_rc(v, next);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(slow);
        self.runtime_call(RuntimeFunc::Dup, &[v])?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        Ok(())
    }

    /// One reference less to the uniform word `v` (D-273). Inline: tagged
    /// integers and static objects are skipped and a local count above 1
    /// is decremented; the last reference and shared counts call
    /// `cheby_rt_drop`. With `checked_rc`, always the runtime call.
    pub(crate) fn drop_value(&mut self, v: Value) -> Result<(), CodegenError> {
        if self.opts.checked_rc {
            return self.drop_call(v);
        }
        let object = self.b.create_block();
        let fast = self.b.create_block();
        let other = self.b.create_block();
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let done = self.b.create_block();
        let tagged = self.b.ins().band_imm_u(v, 1);
        self.b.ins().brif(tagged, done, &[], object, &[]);
        self.b.switch_to_block(object);
        let rc = self.load_rc(v);
        let shared_by_others = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, rc, 1);
        self.b.ins().brif(shared_by_others, fast, &[], other, &[]);
        self.b.switch_to_block(fast);
        let next = self.b.ins().iadd_imm_s(rc, -1);
        self.store_rc(v, next);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(other);
        let static_object = self.b.ins().icmp_imm_s(IntCC::Equal, rc, 0);
        self.b.ins().brif(static_object, done, &[], slow, &[]);
        self.b.switch_to_block(slow);
        self.drop_call(v)?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        Ok(())
    }

    /// The out-of-line drop of the uniform word `v`:
    /// `cheby_rt_drop(ctx, v)`, given the function's `ctx` so that the
    /// free path reads the block cache through the control block
    /// (D-772, D-739).
    pub(crate) fn drop_call(&mut self, v: Value) -> Result<(), CodegenError> {
        let ctx = self.ctx()?;
        self.runtime_call(RuntimeFunc::Drop, &[ctx, v])?;
        Ok(())
    }

    fn load_rc(&mut self, v: Value) -> Value {
        self.b.ins().load(
            types::I32,
            MemFlagsData::trusted(),
            v,
            abi::HEADER_RC_OFFSET,
        )
    }

    fn store_rc(&mut self, v: Value, rc: Value) {
        self.b
            .ins()
            .store(MemFlagsData::trusted(), rc, v, abi::HEADER_RC_OFFSET);
    }
}
