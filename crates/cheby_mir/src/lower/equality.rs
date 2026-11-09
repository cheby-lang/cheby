//! `==` on values whose structure the module knows (§3.13): tuples and the
//! ADTs whose constructors it may read, compared inline, field by field,
//! instead of by the runtime's structural walk ([`RuntimeFn::Equal`]).
//!
//! The comparison follows the walk's order: tags first, then the fields of
//! the constructor, left to right and depth first, stopping at the first
//! difference. A field the module cannot compare inline, such as a type
//! parameter, a `dyn` value, a function or a list, goes to the runtime with
//! the operator's location, so the result and the panic on a function value
//! (D-233, D-556) are those of the walk.
//!
//! An ADT is inlined only when its constructors belong to the module's own
//! interface or to a public one: the module's own types, the builtin ones
//! and `exposed` types. An opaque type of another module keeps its fields
//! private, and its module may change them without changing its interface,
//! so code that read them could outlive their layout. A type with
//! `@std_hooks` compares through its hook (D-342), so it goes to the
//! runtime too. One `==` inlines at most [`FIELD_BUDGET`] fields, so a
//! large or recursive type costs a bounded amount of code.

use cheby_diag::Span;
use cheby_iface::{CtorFields, DefId, Ice, ItemSig, Ty, TyKind, TypeDefKind, builtin};
use cheby_types::infer::substitute;
use cheby_types::thir::{Callee, ExprId, ExprKind};

use crate::ir::{
    BlockId, CmpOp, Const, LayoutRef, Local, Operand, RuntimeFn, Rvalue, TagType, Terminator,
};
use crate::layout::ctor_layout;
use crate::lower::body::BodyCx;
use crate::repr::Repr;

/// The most fields one `==` compares inline, across every level; the
/// fields after them go to the runtime.
const FIELD_BUDGET: u32 = 16;

/// The most constructors an inlined ADT may have.
const MAX_CTORS: usize = 16;

/// How a type `==` can take apart inline.
enum Shape {
    /// A tuple, with its element types.
    Tuple(Vec<Ty>),
    /// An ADT: the test of its tag, when it has more than one constructor,
    /// and its constructors in tag order, with their field types.
    Adt {
        tag: Option<TagType>,
        ctors: Vec<(u32, DefId, Vec<Ty>)>,
    },
}

/// One operand of an inlined `==`.
enum Side {
    /// A value, whose tag and fields are read.
    Value(Local),
    /// A tuple (`ctor` is `None`) or a constructor written in the `==`
    /// itself, whose parts were evaluated in written order and are never
    /// built: the elements, or the fields in declaration order, each in
    /// its type's representation.
    Parts {
        ctor: Option<DefId>,
        parts: Vec<Operand>,
    },
}

/// A tuple or a constructor application written as an operand.
enum Written {
    Tuple(Box<[ExprId]>),
    /// The fields in written order, each with its declaration index.
    Ctor {
        ctor: DefId,
        fields: Vec<(u32, ExprId)>,
    },
}

impl BodyCx<'_, '_> {
    /// `a == b` on two evaluated operands of type `ty`, as a `Bool`
    /// operand; `span` locates a panic of the runtime's walk.
    pub(super) fn values_equal(
        &mut self,
        a: Operand,
        b: Operand,
        ty: Ty,
        span: Span,
    ) -> Result<Operand, Ice> {
        let mut budget = FIELD_BUDGET;
        self.equal_at(a, b, ty, span, &mut budget)
    }

    /// Whether the built-in `lhs == rhs` on `ty` compares an operand
    /// written as a tuple or a constructor without building it: `ty` is
    /// inlined, and one operand is written so.
    pub(super) fn compares_written(&self, lhs: ExprId, rhs: ExprId, ty: Ty) -> Result<bool, Ice> {
        if self.repr_of(ty) != Repr::Value || ty == Ty::STRING || self.shape(ty).is_none() {
            return Ok(false);
        }
        Ok(self.written(lhs)?.is_some() || self.written(rhs)?.is_some())
    }

    /// `lhs == rhs`, or `!=` when `ne`, on `ty`, where
    /// [`Self::compares_written`] holds: both operands are evaluated in
    /// order, a written one part by part, and compared inline. `None` when
    /// an operand does not return.
    pub(super) fn equality_written(
        &mut self,
        ne: bool,
        (lhs, rhs): (ExprId, ExprId),
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let Some(shape) = self.shape(ty) else {
            return Err(Ice::new("an inlined `==` without a shape"));
        };
        let Some(a) = self.side(lhs, &shape)? else {
            return Ok(None);
        };
        let Some(b) = self.side(rhs, &shape)? else {
            return Ok(None);
        };
        let mut budget = FIELD_BUDGET;
        let eq = self.equal_inline(&a, &b, &shape, span, &mut budget)?;
        if !ne {
            return Ok(Some(eq));
        }
        Ok(Some(Operand::Local(
            self.define(Repr::Bool, Rvalue::Not(eq))?,
        )))
    }

    /// The operand `e` as a written tuple or constructor, if it is one.
    fn written(&self, e: ExprId) -> Result<Option<Written>, Ice> {
        Ok(match &self.expr_data(e)?.kind {
            ExprKind::Tuple(elems) => Some(Written::Tuple(elems.clone())),
            ExprKind::Call {
                callee: Callee::Ctor { ctor, .. },
                args,
            } => {
                let mut fields = Vec::with_capacity(args.len());
                for (index, &arg) in args.iter().enumerate() {
                    let index = u32::try_from(index).map_err(|_| Ice::new("too many fields"))?;
                    fields.push((index, arg));
                }
                Some(Written::Ctor {
                    ctor: *ctor,
                    fields,
                })
            }
            ExprKind::Record { ctor, fields, .. } => Some(Written::Ctor {
                ctor: *ctor,
                fields: fields.to_vec(),
            }),
            ExprKind::Ctor { ctor, .. } => Some(Written::Ctor {
                ctor: *ctor,
                fields: Vec::new(),
            }),
            _ => None,
        })
    }

    /// Evaluates the operand `e` of a `==` on `shape`: a written tuple or
    /// constructor part by part, in written order, and anything else as a
    /// value. `None` when it does not return.
    fn side(&mut self, e: ExprId, shape: &Shape) -> Result<Option<Side>, Ice> {
        let (ctor, exprs, tys) = match (self.written(e)?, shape) {
            (Some(Written::Tuple(elems)), Shape::Tuple(tys)) => {
                let exprs = (0_u32..).zip(elems.iter().copied()).collect();
                (None, exprs, tys.clone())
            }
            (Some(Written::Ctor { ctor, fields }), Shape::Adt { ctors, .. }) => {
                let Some((_, _, tys)) = ctors.iter().find(|(_, c, _)| *c == ctor) else {
                    return Err(Ice::new("a written constructor of another type"));
                };
                (Some(ctor), fields, tys.clone())
            }
            _ => {
                let Some(op) = self.expr_as(e, Repr::Value)? else {
                    return Ok(None);
                };
                return Ok(Some(Side::Value(self.value_local(op)?)));
            }
        };
        let mut slots: Vec<Option<Operand>> = vec![None; tys.len()];
        for (index, expr) in exprs {
            let ty = usize::try_from(index)
                .ok()
                .and_then(|i| tys.get(i))
                .copied()
                .ok_or_else(|| Ice::new("a written field outside its constructor"))?;
            let Some(op) = self.expr_as(expr, self.repr_of(ty))? else {
                return Ok(None);
            };
            if let Some(slot) = usize::try_from(index).ok().and_then(|i| slots.get_mut(i)) {
                *slot = Some(op);
            }
        }
        let parts = slots
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| Ice::new("a written constructor with a missing field"))?;
        Ok(Some(Side::Parts { ctor, parts }))
    }

    fn equal_at(
        &mut self,
        a: Operand,
        b: Operand,
        ty: Ty,
        span: Span,
        budget: &mut u32,
    ) -> Result<Operand, Ice> {
        let repr = self.repr_of(ty);
        let eq = if repr != Repr::Value {
            Rvalue::Compare {
                op: CmpOp::Eq,
                lhs: a,
                rhs: b,
                repr,
            }
        } else if ty == Ty::STRING {
            Rvalue::CallRuntime {
                func: RuntimeFn::StrEq,
                args: vec![a, b],
            }
        } else if *budget > 0
            && let Some(shape) = self.shape(ty)
        {
            let a = Side::Value(self.value_local(a)?);
            let b = Side::Value(self.value_local(b)?);
            return self.equal_inline(&a, &b, &shape, span, budget);
        } else {
            let loc = self.location(span)?;
            Rvalue::CallRuntime {
                func: RuntimeFn::Equal,
                args: vec![a, b, loc],
            }
        };
        Ok(Operand::Local(self.define(Repr::Bool, eq)?))
    }

    /// Compares two operands of `shape`; the current block ends in the
    /// comparison's join, whose parameter is the result.
    fn equal_inline(
        &mut self,
        a: &Side,
        b: &Side,
        shape: &Shape,
        span: Span,
        budget: &mut u32,
    ) -> Result<Operand, Ice> {
        let join = self.new_block()?;
        let result = self.block_param(join, Repr::Bool)?;
        let differ = self.new_block()?;
        let exits = (join, differ);
        match shape {
            Shape::Tuple(elems) => {
                let arity = u32::try_from(elems.len())
                    .map_err(|_| Ice::new("a tuple too large to compare"))?;
                let fields = elems.iter().map(|&ty| (ty, Repr::Value)).collect();
                let layout = LayoutRef::Tuple(arity);
                self.equal_fields((a, b, layout), fields, exits, span, budget)?;
            }
            Shape::Adt { tag: None, ctors } => {
                let Some((_, ctor, tys)) = ctors.first() else {
                    return Err(Ice::new("an inlined ADT without constructors"));
                };
                let fields = self.stored(*ctor, tys.clone())?;
                let layout = LayoutRef::Ctor(*ctor);
                self.equal_fields((a, b, layout), fields, exits, span, budget)?;
            }
            Shape::Adt {
                tag: Some(test),
                ctors,
            } => {
                let ta = self.tag_of(a, *test, ctors)?;
                let tb = self.tag_of(b, *test, ctors)?;
                let same = self.define(
                    Repr::Bool,
                    Rvalue::Compare {
                        op: CmpOp::Eq,
                        lhs: ta.clone(),
                        rhs: tb,
                        repr: Repr::Int,
                    },
                )?;
                let fields_of = self.new_block()?;
                self.terminate(Terminator::Branch {
                    cond: Operand::Local(same),
                    then_: fields_of,
                    else_: differ,
                })?;
                self.switch_to(fields_of)?;
                // A written constructor names the one both tags are. Two
                // different ones never get past the tags.
                let written: Vec<DefId> = [a, b]
                    .into_iter()
                    .filter_map(|side| match side {
                        Side::Parts { ctor, .. } => *ctor,
                        Side::Value(_) => None,
                    })
                    .collect();
                if let [first, second] = written[..]
                    && first != second
                {
                    self.goto(differ, Vec::new())?;
                    self.switch_to(differ)?;
                    self.goto(join, vec![Operand::Const(Const::Bool(false))])?;
                    self.switch_to(join)?;
                    return Ok(Operand::Local(result));
                }
                let known = written.first().copied();
                let with_fields = ctors
                    .iter()
                    .filter(|(_, c, tys)| !tys.is_empty() && known.is_none_or(|k| k == *c));
                // Constructors without fields are equal once their tags
                // are; the others compare their fields.
                let mut cases = Vec::new();
                let mut bodies = Vec::new();
                for (tag, ctor, tys) in with_fields {
                    let block = self.new_block()?;
                    cases.push((u64::from(*tag), block));
                    bodies.push((block, *ctor, tys.clone()));
                }
                match (known, bodies.len()) {
                    (_, 0) => self.goto(join, vec![Operand::Const(Const::Bool(true))])?,
                    (Some(_), _) => {
                        let Some((block, ..)) = bodies.first() else {
                            return Err(Ice::new("a known constructor without a body"));
                        };
                        self.goto(*block, Vec::new())?;
                    }
                    (None, _) => {
                        let unit = self.new_block()?;
                        self.terminate(Terminator::Switch {
                            on: ta,
                            cases,
                            default: Some(unit),
                        })?;
                        self.switch_to(unit)?;
                        self.goto(join, vec![Operand::Const(Const::Bool(true))])?;
                    }
                }
                for (block, ctor, tys) in bodies {
                    self.switch_to(block)?;
                    let fields = self.stored(ctor, tys)?;
                    let layout = LayoutRef::Ctor(ctor);
                    self.equal_fields((a, b, layout), fields, exits, span, budget)?;
                }
            }
        }
        self.switch_to(differ)?;
        self.goto(join, vec![Operand::Const(Const::Bool(false))])?;
        self.switch_to(join)?;
        Ok(Operand::Local(result))
    }

    /// The tag of an operand of an ADT whose tags `test` reads: read from a
    /// value, known for a written constructor.
    fn tag_of(
        &mut self,
        side: &Side,
        test: TagType,
        ctors: &[(u32, DefId, Vec<Ty>)],
    ) -> Result<Operand, Ice> {
        match side {
            Side::Value(base) => {
                let tag = self.define(
                    Repr::Int,
                    Rvalue::ReadTag {
                        base: Operand::Local(*base),
                        ty: test,
                    },
                )?;
                Ok(Operand::Local(tag))
            }
            Side::Parts { ctor, .. } => {
                let tag = ctors
                    .iter()
                    .find(|(_, c, _)| Some(*c) == *ctor)
                    .map(|&(tag, ..)| tag)
                    .ok_or_else(|| Ice::new("a written constructor of another type"))?;
                Ok(Operand::Const(Const::Int(i64::from(tag))))
            }
        }
    }

    /// Compares the fields of `a` and `b`, both of `layout`, in order: each
    /// difference jumps to `differ`, and equal fields end in `join` with
    /// `True`. Each field is a type and the representation it is stored in.
    fn equal_fields(
        &mut self,
        (a, b, layout): (&Side, &Side, LayoutRef),
        fields: Vec<(Ty, Repr)>,
        (join, differ): (BlockId, BlockId),
        span: Span,
        budget: &mut u32,
    ) -> Result<(), Ice> {
        for (index, (ty, stored)) in fields.into_iter().enumerate() {
            *budget = budget.saturating_sub(1);
            let fa = self.part(a, (layout, index), stored, ty)?;
            let fb = self.part(b, (layout, index), stored, ty)?;
            let same = self.equal_at(fa, fb, ty, span, budget)?;
            let next = self.new_block()?;
            self.terminate(Terminator::Branch {
                cond: same,
                then_: next,
                else_: differ,
            })?;
            self.switch_to(next)?;
        }
        self.goto(join, vec![Operand::Const(Const::Bool(true))])
    }

    /// Field `index` of `side`, of type `ty`, in its type's representation:
    /// read from a value, where it is stored as `stored`, or the part of a
    /// written operand.
    fn part(
        &mut self,
        side: &Side,
        (layout, index): (LayoutRef, usize),
        stored: Repr,
        ty: Ty,
    ) -> Result<Operand, Ice> {
        match side {
            Side::Value(base) => {
                let field = u32::try_from(index).map_err(|_| Ice::new("too many fields"))?;
                let read = self.define(
                    stored,
                    Rvalue::Field {
                        base: *base,
                        layout,
                        field,
                    },
                )?;
                let want = self.repr_of(ty);
                self.to_repr(Operand::Local(read), stored, want, ty)
            }
            Side::Parts { parts, .. } => parts
                .get(index)
                .cloned()
                .ok_or_else(|| Ice::new("a written operand without this part")),
        }
    }

    /// A uniform operand as a local (a constant is moved into one).
    fn value_local(&mut self, op: Operand) -> Result<Local, Ice> {
        match op {
            Operand::Local(l) => Ok(l),
            op @ Operand::Const(_) => self.define(Repr::Value, Rvalue::Use(op)),
        }
    }

    /// The field types of `ctor` with the representation each is stored in.
    fn stored(&self, ctor: DefId, tys: Vec<Ty>) -> Result<Vec<(Ty, Repr)>, Ice> {
        let layout = ctor_layout(ctor, &self.cx.defs)?;
        if layout.fields.len() != tys.len() {
            return Err(Ice::new(format!("{ctor:?} has a layout of another size")));
        }
        Ok(tys
            .into_iter()
            .zip(layout.fields.iter().map(|f| f.repr))
            .collect())
    }

    /// How `==` can take `ty` apart inline, or `None` when it goes to the
    /// runtime.
    fn shape(&self, ty: Ty) -> Option<Shape> {
        let types = &self.cx.defs.globals.types;
        let (def, args) = match types.kind(ty) {
            TyKind::Tuple(elems) => return Some(Shape::Tuple(elems.to_vec())),
            TyKind::Named { def, args } => (*def, args),
            _ => return None,
        };
        let Ok(item) = self.cx.defs.item(def) else {
            return None;
        };
        let ItemSig::Type(t) = &item.sig else {
            return None;
        };
        let own = self.cx.defs.own.is_some_and(|own| own.module == def.module);
        let TypeDefKind::Adt { ctors } = &t.kind else {
            return None;
        };
        if t.hooks.is_some() || !(own || t.exposed) || ctors.len() > MAX_CTORS {
            return None;
        }
        let mut out = Vec::with_capacity(ctors.len());
        for &index in ctors {
            let ctor = DefId::new(def.module, index);
            let Ok(item) = self.cx.defs.item(ctor) else {
                return None;
            };
            let ItemSig::Ctor(sig) = &item.sig else {
                return None;
            };
            let declared: Vec<Ty> = match &sig.fields {
                CtorFields::Unit => Vec::new(),
                CtorFields::Positional(tys) => tys.to_vec(),
                CtorFields::Named(fields) => fields.iter().map(|&(_, ty)| ty).collect(),
            };
            let fields = declared
                .into_iter()
                .map(|field| substitute(types, field, args))
                .collect();
            out.push((sig.tag, ctor, fields));
        }
        out.sort_unstable_by_key(|&(tag, ..)| tag);
        let tag = (out.len() > 1).then(|| match def {
            d if d == builtin::OPTION => TagType::Option,
            d if d == builtin::RESULT => TagType::Result,
            d if d == builtin::ORDER => TagType::Order,
            d => TagType::Adt(d),
        });
        Some(Shape::Adt { tag, ctors: out })
    }
}
