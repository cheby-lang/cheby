//! Constructors, records, field access, record updates and tuples (D2,
//! D6, D-454).
//!
//! - A unit constructor is a static object (D-454): the runtime's for the
//!   builtin ones, `cb$ctor$…` for user ones. `Nil` and an unboxed `Bool`
//!   are constants.
//! - A positional constructor call or a record expression evaluates its
//!   arguments in the order written (§5.11.1, §5.15.1), converts each to
//!   its field's representation (raw `Int`, `Float`, `Bool` and sized
//!   fields stay unboxed, the rest are uniform words, plan §5.2), and
//!   allocates the object with its fields in declaration order; the
//!   layout puts value fields first in memory (`LayoutDesc`).
//! - `x.name` is one `field` read when every constructor holds the field
//!   at the same offset in the same representation, otherwise a `switch`
//!   on the constructor's tag ([`Rvalue::ReadTag`], plan §5.3) whose
//!   arms read the field and join in a block parameter (D-071).
//! - `T { ..base, … }` evaluates `base`, then the new values in the order
//!   written, and builds a new object of `base`'s constructor (D-156,
//!   D-501): each field not listed is read from `base`, each listed one
//!   is its new value. On a type with several constructors the tag picks
//!   the constructor.
//! - A tuple is an object of uniform words (plan §5.4). One of more than
//!   [`TUPLE_INIT_THRESHOLD`] fields is filled one field at a time and,
//!   when large, continues in continuations (D-720, D-780, `split.rs`).

use cheby_diag::Span;
use cheby_iface::{CtorFields, DefId, Ice, ItemSig, Ty, builtin};
use cheby_types::thir::{ExprId, UpdateField};

use crate::ir::{BlockId, Const, LayoutRef, Local, Operand, Rvalue, Stmt, TagType, Terminator};
use crate::layout::{LayoutDesc, ctor_layout, type_layouts};
use crate::lower::body::BodyCx;
use crate::lower::split::{Items, TUPLE_INIT_THRESHOLD};
use crate::repr::Repr;

impl BodyCx<'_, '_> {
    /// A nullary constructor as a value of type `ty`: `Nil` and unboxed
    /// `Bool`s as constants, every other one as its static object,
    /// the runtime's for builtin constructors and `cb$ctor$…` for user
    /// ones (D-454). In a uniform position a `Bool` is the static `True`
    /// or `False` too.
    pub(super) fn unit_ctor(
        &mut self,
        ctor: DefId,
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let item = self.cx.defs.item(ctor)?;
        if !matches!(&item.sig, ItemSig::Ctor(sig) if sig.fields == CtorFields::Unit) {
            return Err(Ice::new(format!(
                "a constructor with fields as a unit constructor at {span:?}"
            )));
        }
        let repr = self.repr_of(ty);
        let c = match ctor {
            c if c == builtin::NIL => Const::Nil,
            c if c == builtin::TRUE && repr == Repr::Bool => Const::Bool(true),
            c if c == builtin::FALSE && repr == Repr::Bool => Const::Bool(false),
            c => Const::StaticCtor(c),
        };
        Ok(Some(Operand::Const(c)))
    }

    /// A positional constructor applied to `args`: the arguments in the
    /// order written, each in its field's representation, then one
    /// allocation (§5.11.1, plan §5.2).
    pub(super) fn ctor_call(
        &mut self,
        ctor: DefId,
        _ty_args: &[Ty],
        args: &[ExprId],
        _ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let desc = ctor_layout(ctor, &self.cx.defs)?;
        if desc.fields.len() != args.len() {
            return Err(Ice::new(format!(
                "constructor `{}` with {} arguments for {} fields at {span:?}",
                desc.name,
                args.len(),
                desc.fields.len()
            )));
        }
        let mut fields = Vec::with_capacity(args.len());
        for (&arg, field) in args.iter().zip(&desc.fields) {
            let Some(op) = self.expr_as(arg, field.repr)? else {
                return Ok(None);
            };
            fields.push(op);
        }
        self.alloc_ctor(&desc, fields).map(Some)
    }

    /// A named-field construction; `fields` in written order with their
    /// declaration indices. The values are evaluated in the order written
    /// (§5.11.1) and stored in declaration order.
    pub(super) fn record(
        &mut self,
        ctor: DefId,
        _ty_args: &[Ty],
        fields: &[(u32, ExprId)],
        _ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let desc = ctor_layout(ctor, &self.cx.defs)?;
        let mut slots: Vec<Option<Operand>> = vec![None; desc.fields.len()];
        for &(index, value) in fields {
            let repr = field_of(&desc, index)?.repr;
            let Some(op) = self.expr_as(value, repr)? else {
                return Ok(None);
            };
            let slot = usize::try_from(index)
                .ok()
                .and_then(|i| slots.get_mut(i))
                .ok_or_else(|| Ice::new(format!("field {index} of `{}`", desc.name)))?;
            if slot.replace(op).is_some() {
                return Err(Ice::new(format!(
                    "field {index} of `{}` initialized twice at {span:?}",
                    desc.name
                )));
            }
        }
        let fields = slots
            .into_iter()
            .collect::<Option<Vec<Operand>>>()
            .ok_or_else(|| {
                Ice::new(format!(
                    "a record of `{}` without all its fields at {span:?}",
                    desc.name
                ))
            })?;
        self.alloc_ctor(&desc, fields).map(Some)
    }

    /// `base.name`, with the field's index in every constructor of the
    /// base's type (D-071). Constructors that hold the field at the same
    /// offset in the same representation share one read; when they do not
    /// all agree, the base's tag selects the read (plan §5.3).
    pub(super) fn field(
        &mut self,
        base: ExprId,
        index_per_ctor: &[(DefId, u32)],
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let Some(op) = self.expr(base, false)? else {
            return Ok(None);
        };
        let want = self.repr_of(ty);
        let base = self.base_local(op)?;
        let mut reads = Vec::with_capacity(index_per_ctor.len());
        for &(ctor, index) in index_per_ctor {
            let desc = ctor_layout(ctor, &self.cx.defs)?;
            let field = field_of(&desc, index)?;
            reads.push(Read {
                tag: desc.tag,
                layout: desc.layout,
                index,
                offset: field.offset,
                repr: field.repr,
                type_def: desc.type_def,
            });
        }
        reads.sort_by_key(|r| r.tag);
        let groups = group_reads(&reads);
        let [first, ..] = groups.as_slice() else {
            return Err(Ice::new(format!(
                "a field access without constructors at {span:?}"
            )));
        };
        if groups.len() == 1 {
            let read = first.0;
            return self.read_field(base, read, want, ty).map(Some);
        }
        let tagged = self.tag_switch(base, &reads, &groups, span)?;
        let join = self.new_block()?;
        let result = self.block_param(join, want)?;
        for ((read, _), block) in groups.iter().zip(tagged) {
            self.switch_to(block)?;
            let value = self.read_field(base, *read, want, ty)?;
            self.goto(join, vec![value])?;
        }
        self.switch_to(join)?;
        Ok(Some(Operand::Local(result)))
    }

    /// `T { ..base, fields }` (D-156, D-501): `base`, then the new values
    /// in the order written (§5.11.2), then a new object of `base`'s
    /// constructor with the fields not listed copied from `base`.
    pub(super) fn record_update(
        &mut self,
        base: ExprId,
        fields: &[UpdateField],
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let Some(base_op) = self.expr(base, false)? else {
            return Ok(None);
        };
        let mut values = Vec::with_capacity(fields.len());
        for field in fields {
            let Some(op) = self.expr(field.value, false)? else {
                return Ok(None);
            };
            values.push((op, self.ty(field.value)?));
        }
        let want = self.repr_of(ty);
        // §5.11.2 requires at least one field, and the checker reports an
        // update without one; with none the copy would equal the base.
        let Some(first) = fields.first() else {
            return Ok(Some(base_op));
        };
        let base = self.base_local(base_op)?;
        let mut ctors = Vec::with_capacity(first.index_per_ctor.len());
        for &(ctor, _) in &first.index_per_ctor {
            ctors.push(ctor_layout(ctor, &self.cx.defs)?);
        }
        ctors.sort_by_key(|d| d.tag);
        // The new values by field index, per constructor (D-071: every
        // constructor has every listed field).
        let mut updates: Vec<Vec<(u32, usize)>> = Vec::with_capacity(ctors.len());
        for desc in &ctors {
            let LayoutRef::Ctor(ctor) = desc.layout else {
                return Err(Ice::new("a constructor layout that is not a constructor's"));
            };
            let mut own = Vec::with_capacity(fields.len());
            for (n, field) in fields.iter().enumerate() {
                let index = field
                    .index_per_ctor
                    .iter()
                    .find(|(c, _)| *c == ctor)
                    .map(|&(_, i)| i)
                    .ok_or_else(|| {
                        Ice::new(format!(
                            "an updated field missing from `{}` at {span:?}",
                            desc.name
                        ))
                    })?;
                own.push((index, n));
            }
            updates.push(own);
        }
        let [only] = ctors.as_slice() else {
            return self.update_by_tag(base, &ctors, &updates, &values, want, span);
        };
        let own = updates.first().map_or(&[][..], Vec::as_slice);
        self.rebuild(base, only, own, &values).map(Some)
    }

    /// A tuple of `elems`, evaluated left to right (§5.12), each a uniform
    /// word.
    pub(super) fn tuple(
        &mut self,
        elems: &[ExprId],
        _ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        if elems.len() > TUPLE_INIT_THRESHOLD {
            return self.tuple_by_field(elems, span);
        }
        let mut fields = Vec::with_capacity(elems.len());
        for &elem in elems {
            let Some(op) = self.expr_as(elem, Repr::Value)? else {
                return Ok(None);
            };
            fields.push(op);
        }
        let arity = u32::try_from(fields.len()).map_err(|_| Ice::new("tuple too large"))?;
        let local = self.define(
            Repr::Value,
            Rvalue::Alloc {
                layout: LayoutRef::Tuple(arity),
                fields,
            },
        )?;
        Ok(Some(Operand::Local(local)))
    }

    /// A tuple of more than [`TUPLE_INIT_THRESHOLD`] `elems` (D-720,
    /// D-810): one allocation of placeholders, then each
    /// element computed, boxed and stored before the next, so only the
    /// tuple and one element are live; a large one continues in
    /// continuations that take the partly filled tuple (D-780).
    fn tuple_by_field(&mut self, elems: &[ExprId], span: Span) -> Result<Option<Operand>, Ice> {
        let arity = u32::try_from(elems.len()).map_err(|_| Ice::new("tuple too large"))?;
        let layout = LayoutRef::Tuple(arity);
        let mut chain = self.chain(span);
        let mut obj = self.define(Repr::Value, Rvalue::AllocPlaceholder { layout })?;
        for (k, &elem) in elems.iter().enumerate() {
            if self.split_due(&chain) {
                obj = self.split_cut(&mut chain, k, Operand::Local(obj), Items::Tuple(elems))?;
            }
            let Some(value) = self.expr_as(elem, Repr::Value)? else {
                return self.split_finish(chain, None);
            };
            let field = u32::try_from(k).map_err(|_| Ice::new("tuple too large"))?;
            self.emit(Stmt::Eval(Rvalue::InitField {
                obj,
                layout,
                field,
                value,
            }))?;
        }
        self.split_finish(chain, Some(Operand::Local(obj)))
    }

    /// A new object of the constructor `desc` from `fields`, in
    /// declaration order and already in their fields' representations. A
    /// constructor without fields is its static object (D-454).
    fn alloc_ctor(&mut self, desc: &LayoutDesc, fields: Vec<Operand>) -> Result<Operand, Ice> {
        let LayoutRef::Ctor(ctor) = desc.layout else {
            return Err(Ice::new("a constructor layout that is not a constructor's"));
        };
        if desc.fields.is_empty() {
            return Ok(Operand::Const(Const::StaticCtor(ctor)));
        }
        let local = self.define(
            Repr::Value,
            Rvalue::Alloc {
                layout: desc.layout,
                fields,
            },
        )?;
        Ok(Operand::Local(local))
    }

    /// `op` as a local, which `field` and `read_tag` need for their base.
    fn base_local(&mut self, op: Operand) -> Result<Local, Ice> {
        match op {
            Operand::Local(local) => Ok(local),
            op @ Operand::Const(_) => self.define(Repr::Value, Rvalue::Use(op)),
        }
    }

    /// Reads field `read` of `base` and converts it to `want`, the
    /// representation of the access's type `ty`.
    fn read_field(&mut self, base: Local, read: Read, want: Repr, ty: Ty) -> Result<Operand, Ice> {
        let local = self.define(
            read.repr,
            Rvalue::Field {
                base,
                layout: read.layout,
                field: read.index,
            },
        )?;
        self.to_repr(Operand::Local(local), read.repr, want, ty)
    }

    /// Ends the current block with a `switch` on `base`'s tag: one new
    /// block per group of `groups` (in order), the last group being the
    /// default. The type's constructors must all be among `reads`.
    fn tag_switch(
        &mut self,
        base: Local,
        reads: &[Read],
        groups: &[(Read, Vec<u32>)],
        span: Span,
    ) -> Result<Vec<BlockId>, Ice> {
        let type_def = reads
            .first()
            .map(|r| r.type_def)
            .ok_or_else(|| Ice::new("a tag switch without constructors"))?;
        let all = type_layouts(type_def, &self.cx.defs)?;
        if all.len() != reads.len() || reads.iter().any(|r| r.type_def != type_def) {
            return Err(Ice::new(format!(
                "a field read on some but not all constructors of {type_def:?} at {span:?}"
            )));
        }
        let tag = self.define(
            Repr::Int,
            Rvalue::ReadTag {
                base: Operand::Local(base),
                ty: TagType::Adt(type_def),
            },
        )?;
        let mut blocks = Vec::with_capacity(groups.len());
        for _ in groups {
            blocks.push(self.new_block()?);
        }
        let Some((&default, others)) = blocks.split_last() else {
            return Err(Ice::new("a tag switch without constructors"));
        };
        let mut cases = Vec::new();
        for ((_, tags), &block) in groups.iter().zip(others) {
            cases.extend(tags.iter().map(|&t| (u64::from(t), block)));
        }
        cases.sort_unstable_by_key(|&(t, _)| t);
        self.terminate(Terminator::Switch {
            on: Operand::Local(tag),
            cases,
            default: Some(default),
        })?;
        Ok(blocks)
    }

    /// A record update on a type with several constructors: a `switch` on
    /// `base`'s tag, one rebuild per constructor, joined in a block
    /// parameter.
    fn update_by_tag(
        &mut self,
        base: Local,
        ctors: &[LayoutDesc],
        updates: &[Vec<(u32, usize)>],
        values: &[(Operand, Ty)],
        want: Repr,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let reads: Vec<Read> = ctors
            .iter()
            .map(|d| Read {
                tag: d.tag,
                layout: d.layout,
                index: 0,
                offset: 0,
                repr: Repr::Value,
                type_def: d.type_def,
            })
            .collect();
        let groups: Vec<(Read, Vec<u32>)> = reads.iter().map(|&r| (r, vec![r.tag])).collect();
        let blocks = self.tag_switch(base, &reads, &groups, span)?;
        let join = self.new_block()?;
        let result = self.block_param(join, want)?;
        for ((desc, own), block) in ctors.iter().zip(updates).zip(blocks) {
            self.switch_to(block)?;
            let value = self.rebuild(base, desc, own, values)?;
            self.goto(join, vec![value])?;
        }
        self.switch_to(join)?;
        Ok(Some(Operand::Local(result)))
    }

    /// A new object of `desc`'s constructor: field `i` is the value
    /// `values[n]` when `(i, n)` is in `own`, else `base`'s field `i`.
    fn rebuild(
        &mut self,
        base: Local,
        desc: &LayoutDesc,
        own: &[(u32, usize)],
        values: &[(Operand, Ty)],
    ) -> Result<Operand, Ice> {
        let mut fields = Vec::with_capacity(desc.fields.len());
        for (i, field) in desc.fields.iter().enumerate() {
            let index = u32::try_from(i).map_err(|_| Ice::new("constructor too large"))?;
            // The last listing wins; the checker rejects a field listed
            // twice.
            let op = if let Some(&(_, n)) = own.iter().rev().find(|&&(at, _)| at == index) {
                let (op, ty) = values
                    .get(n)
                    .cloned()
                    .ok_or_else(|| Ice::new("an update value out of range"))?;
                let from = self.repr_of(ty);
                self.to_repr(op, from, field.repr, ty)?
            } else {
                let local = self.define(
                    field.repr,
                    Rvalue::Field {
                        base,
                        layout: desc.layout,
                        field: index,
                    },
                )?;
                Operand::Local(local)
            };
            fields.push(op);
        }
        self.alloc_ctor(desc, fields)
    }
}

/// Where one constructor holds a field.
#[derive(Clone, Copy, Debug)]
struct Read {
    /// The constructor's tag.
    tag: u32,
    layout: LayoutRef,
    /// The field's declaration index.
    index: u32,
    /// The field's byte offset.
    offset: i32,
    repr: Repr,
    type_def: DefId,
}

/// `reads` (in tag order) grouped by offset and representation, in order
/// of first appearance: one read serves each group, with its tags.
fn group_reads(reads: &[Read]) -> Vec<(Read, Vec<u32>)> {
    let mut groups: Vec<(Read, Vec<u32>)> = Vec::new();
    for &read in reads {
        match groups
            .iter_mut()
            .find(|(g, _)| g.offset == read.offset && g.repr == read.repr)
        {
            Some((_, tags)) => tags.push(read.tag),
            None => groups.push((read, vec![read.tag])),
        }
    }
    groups
}

/// Field `index` of `desc`.
fn field_of(desc: &LayoutDesc, index: u32) -> Result<&crate::layout::FieldDesc, Ice> {
    usize::try_from(index)
        .ok()
        .and_then(|i| desc.fields.get(i))
        .ok_or_else(|| {
            Ice::new(format!(
                "field {index} of `{}`, which has {}",
                desc.name,
                desc.fields.len()
            ))
        })
}

/// Lowering tests of this file, `list.rs` and `interp.rs` (D2), from
/// source through `test_support::lower_source`, pinned by goldens under
/// `tests/golden/data/`. `cargo test -p cheby_mir data` runs them.
#[cfg(test)]
mod tests {
    use crate::golden_tests::check_golden;
    use crate::test_support::{Lowered, lower_source};

    /// Lowers `source` as the root module of `app`.
    fn lower(source: &str) -> Lowered {
        lower_source(&[("", source)])
    }

    /// The dump of the `app` unit.
    fn app(lowered: &Lowered) -> String {
        crate::dump(lowered.unit("app")).unwrap()
    }

    /// Positional and named constructors: arguments in the order written
    /// (`second` before `first`), fields in declaration order with raw
    /// `Int`, `Float` and `Bool` fields unboxed and value fields first in
    /// memory, a generic field boxed, the builtin `Some`, `Ok` and `Err`,
    /// and user unit constructors as static objects (D-454).
    #[test]
    fn data_constructors() {
        let lowered = lower(
            r#"//! Constructors.

type Point {
  x: Int,
  y: Int,
}

type Mixed {
  count: Int,
  name: String,
  ratio: Float,
  flag: Bool,
}

type Pair<T> {
  Pair(T, Int),
}

type Shape {
  Circle(Float),
  Dot,
}

@external(native, "first")
@external(js, "./ffi.mjs", "first")
fn first() -> Int

@external(native, "second")
@external(js, "./ffi.mjs", "second")
fn second() -> Int

fn point() -> Point {
  Point { y: second(), x: first() }
}

fn mixed(name: String) -> Mixed {
  Mixed { flag: True, ratio: 0.5, name, count: 3 }
}

fn pair(x: Int) -> Pair<Int> {
  Pair(x, second())
}

fn circle() -> Shape {
  Circle(2.5)
}

fn dot() -> Shape {
  Dot
}

fn some(x: Int) -> Option<Int> {
  Some(x)
}

fn ok(text: String) -> Result<String, Int> {
  Ok(text)
}

fn err(code: Int) -> Result<String, Int> {
  Err(code)
}

fn main() {
  let _ = point()
  let _ = mixed("m")
  let _ = pair(1)
  let _ = circle()
  let _ = dot()
  let _ = some(1)
  let _ = ok("yes")
  let _ = err(2)
}
"#,
        );
        check_golden("data/constructors", &app(&lowered));
    }

    /// Field access: one `field` read on a single-constructor record, on
    /// a generic field (unboxed after the read), and on a field that
    /// several constructors hold at different indexes but the same
    /// offset; a `read_tag` switch when the offsets differ, for a raw and
    /// for a value field (D-071, plan §5.3).
    #[test]
    fn data_field_access() {
        let lowered = lower(
            r#"//! Field access.

type Point {
  x: Int,
  y: Int,
}

type Labeled<T> {
  value: T,
  label: String,
}

type Shape {
  Circle { name: String, radius: Int },
  Rect { width: Int, height: Int, name: String },
}

type Item {
  Book { title: String, pages: Int },
  Disc { label: String, title: String, minutes: Int, pages: Int },
}

fn y(p: Point) -> Int {
  p.y
}

fn value(l: Labeled<Int>) -> Int {
  l.value
}

fn name(s: Shape) -> String {
  s.name
}

fn pages(item: Item) -> Int {
  item.pages
}

fn title(item: Item) -> String {
  item.title
}

fn main() {
  let _ = y(Point { x: 1, y: 2 })
  let _ = value(Labeled { value: 1, label: "one" })
  let _ = name(Circle { name: "c", radius: 1 })
  let _ = pages(Book { title: "b", pages: 10 })
  let _ = title(Book { title: "b", pages: 10 })
}
"#,
        );
        check_golden("data/field_access", &app(&lowered));
    }

    /// Record updates: `base` first, then the new values in the order
    /// written; on a single-constructor record a copy with the fields not
    /// listed read from `base`, through an alias too; on a type with
    /// several constructors a `read_tag` switch with one rebuild per
    /// constructor, keeping `base`'s variant (D-156, D-501).
    #[test]
    fn data_record_updates() {
        let lowered = lower(
            r#"//! Record updates.

type Point {
  x: Int,
  y: Int,
}

type Position = Point

type Labeled<T> {
  value: T,
  label: String,
}

type Shape {
  Circle { name: String, radius: Int },
  Rect { width: Int, height: Int, name: String },
}

@external(native, "first")
@external(js, "./ffi.mjs", "first")
fn first() -> Int

fn moved(p: Point) -> Point {
  Point { ..p, x: first() }
}

fn aliased(p: Position) -> Point {
  Position { ..p, y: 10 }
}

fn relabeled(l: Labeled<Int>, n: Int) -> Labeled<Int> {
  Labeled { ..l, value: n }
}

fn renamed(s: Shape, name: String) -> Shape {
  Shape { ..s, name }
}

fn main() {
  let _ = moved(Point { x: 1, y: 2 })
  let _ = aliased(Point { x: 1, y: 2 })
  let _ = relabeled(Labeled { value: 1, label: "one" }, 2)
  let _ = renamed(Circle { name: "c", radius: 1 }, "d")
}
"#,
        );
        check_golden("data/record_updates", &app(&lowered));
    }

    /// Tuples: elements left to right, each a uniform word.
    #[test]
    fn data_tuples() {
        let lowered = lower(
            r#"//! Tuples.

fn triple(n: Int, x: Float, s: String) -> (Int, Float, String) {
  (n, x, s)
}

fn pair(b: Bool) -> (Bool, Nil) {
  (b, Nil)
}

fn main() {
  let _ = triple(1, 2.5, "s")
  let _ = pair(True)
}
"#,
        );
        check_golden("data/tuples", &app(&lowered));
    }

    /// List literals: `[]` is the static empty list; elements are pushed
    /// onto one accumulator; a leading spread is the accumulator itself,
    /// moved when it is the operand's last use (so `[..found, item]`
    /// pushes in place), and later spreads are appended (D-157, F3).
    #[test]
    fn data_list_literals() {
        let lowered = lower(
            r#"//! List literals.

fn empty() -> List<Int> {
  []
}

fn elements(n: Int) -> List<Int> {
  [1, n, 3]
}

fn strings(s: String) -> List<String> {
  [s, "b"]
}

fn push_found(found: List<String>, item: String) -> List<String> {
  [..found, item]
}

fn first(xs: List<Int>, ys: List<Int>) -> List<Int> {
  [..xs, ..ys]
}

fn middle(xs: List<Int>, ys: List<Int>) -> List<Int> {
  [0, ..xs, 99, ..ys]
}

fn last(xs: List<Int>) -> List<Int> {
  [0, ..xs]
}

fn twice(xs: List<Int>) -> List<Int> {
  [..xs, ..xs]
}

fn main() {
  let _ = empty()
  let _ = elements(2)
  let _ = strings("a")
  let _ = push_found(["a"], "b")
  let _ = first([1], [2])
  let _ = middle([1], [2])
  let _ = last([1])
  let _ = twice([1])
}
"#,
        );
        check_golden("data/list_literals", &app(&lowered));
    }

    /// Interpolation: one `str_with_capacity`, then one `str_append` per
    /// part. A `String` is its own text; `Int` and `Float` call their
    /// `show`; a user type its own `show` (D-086); `{x:?}` the runtime's
    /// debug text of the boxed value; `{p.x}` reads the field. A string
    /// of one value is that value's text, with no buffer.
    #[test]
    fn data_interpolation() {
        let lowered = lower(
            r#"//! Interpolation.

pub type Point {
  x: Int,
  y: Int,
}

pub fn show(p: Point) -> String {
  "a point"
}

fn texts(name: String, n: Int, x: Float) -> String {
  "hi {name}, {n} at {x}!"
}

fn user(p: Point) -> String {
  "{p} with x {p.x}: {p:?} and {p.y:?}"
}

fn only(name: String) -> String {
  "{name}"
}

fn only_int(n: Int) -> String {
  "{n}"
}

fn main() {
  let _ = texts("a", 1, 2.5)
  let _ = user(Point { x: 1, y: 2 })
  let _ = only("b")
  let _ = only_int(3)
}
"#,
        );
        check_golden("data/interpolation", &app(&lowered));
    }
}
