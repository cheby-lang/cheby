//! The built-in `Show` and `Compare` of tuples (D-167, D-355): the code
//! of the closures in a tuple's dictionary ([`FnKey::TupleDictFn`]).
//!
//! The closure holds the element dictionaries as its captures, fields
//! `1..=n`; the code has the uniform signature (D-453).
//!
//! - `show(self, t)`: `(a, b)`, each element written by its own `show`
//!   (D-355).
//! - `compare(self, a, b)`: the elements compared left to right by their
//!   own `compare`; the first result other than `Equal` is the result,
//!   and the last element's comparison is a tail call (D-136).

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, Ice, builtin};

use super::IfaceShape;
use crate::builder::FunctionBuilder;
use crate::ir::{
    Abi, Callee, CmpOp, Const, FnKey, Function, LayoutRef, Local, Operand, RuntimeFn, Rvalue,
    TagType, Terminator,
};
use crate::lower::LowerCx;
use crate::repr::Repr;

/// The bytes `show` reserves per element before appending.
const SHOW_ESTIMATE: i64 = 16;

/// The tag of `Equal` among `Order`'s constructors (§5.3).
const ORDER_EQUAL: i64 = 1;

/// The code of the built-in tuple function `iface_fn` (`Show::show` or
/// `Compare::compare`) for tuples of `arity` elements.
///
/// # Errors
///
/// An [`Ice`] for another interface function, or one not readable
/// through `cx`.
pub(in crate::lower) fn tuple_fn(
    iface_fn: DefId,
    arity: u32,
    cx: &LowerCx<'_>,
) -> Result<Function, Ice> {
    let item = cx.defs.item(iface_fn)?;
    let iface = item
        .parent
        .map(|p| DefId::new(iface_fn.module, p))
        .ok_or_else(|| Ice::new("an interface function without its interface"))?;
    let shape = IfaceShape::of(cx, iface)?;
    let elem = Elem {
        layout: shape.layout()?,
        field: shape.fn_field(cx, iface_fn)?,
        captures: LayoutRef::Closure(arity),
        tuple: LayoutRef::Tuple(arity),
    };
    let name = cx.defs.name(iface_fn)?;
    let is_compare = &*name.module == "std::ops" && &*name.item == "Compare::compare";
    if iface_fn != builtin::SHOW_FN && !is_compare {
        return Err(Ice::new(format!(
            "no built-in tuple function for {}",
            name.path()
        )));
    }
    // `Show::show` is builtin and has no source; its tuple code reports
    // a stack overflow at `<builtin>` (D-583).
    let (span, location) = match item.span {
        Some(span) => (span, cx.location(span)?),
        None => (
            Span::new(FileId::new(0), TextRange::empty(0)),
            "<builtin>".to_owned(),
        ),
    };
    let mut b = FunctionBuilder::new(
        FnKey::TupleDictFn { iface_fn, arity },
        Abi::Cheby,
        Repr::Value,
        span,
    )?;
    b.set_entry_loc(&location)?;
    let this = b.add_param(Repr::Value, None, None)?;
    if is_compare {
        let lhs = b.add_param(Repr::Value, None, None)?;
        let rhs = b.add_param(Repr::Value, None, None)?;
        compare(&mut b, &elem, this, lhs, rhs, arity)?;
    } else {
        let t = b.add_param(Repr::Value, None, None)?;
        show(&mut b, &elem, this, t, arity)?;
    }
    b.finish()
}

/// Where the parts of one element's call are.
struct Elem {
    /// The element dictionaries' layout.
    layout: LayoutRef,
    /// The function's closure in an element dictionary.
    field: u32,
    /// The tuple closure: the element dictionaries.
    captures: LayoutRef,
    /// The tuples.
    tuple: LayoutRef,
}

impl Elem {
    /// The closure of element `i`'s function: field `i + 1` of `this`,
    /// then the function's field of that dictionary.
    fn closure(&self, b: &mut FunctionBuilder, this: Local, i: u32) -> Result<Local, Ice> {
        let dict = b.assign(
            Repr::Value,
            Rvalue::Field {
                base: this,
                layout: self.captures,
                field: i
                    .checked_add(1)
                    .ok_or_else(|| Ice::new("a tuple too large"))?,
            },
        )?;
        b.assign(
            Repr::Value,
            Rvalue::Field {
                base: dict,
                layout: self.layout,
                field: self.field,
            },
        )
    }

    /// Element `i` of the tuple `t`.
    fn element(&self, b: &mut FunctionBuilder, t: Local, i: u32) -> Result<Local, Ice> {
        b.assign(
            Repr::Value,
            Rvalue::Field {
                base: t,
                layout: self.tuple,
                field: i,
            },
        )
    }
}

/// Appends `piece` (borrowed) to the unique string `acc`.
fn append(b: &mut FunctionBuilder, acc: Local, piece: Operand) -> Result<Local, Ice> {
    b.call_runtime(RuntimeFn::StrAppend, vec![Operand::Local(acc), piece])?
        .ok_or_else(|| Ice::new("`str_append` without a result"))
}

/// Appends the literal `text` to `acc`.
fn append_text(b: &mut FunctionBuilder, acc: Local, text: &str) -> Result<Local, Ice> {
    let piece = Operand::Const(Const::Str(b.string(text)?));
    append(b, acc, piece)
}

/// `(a, b, …)`, each element by its own `show` (D-355).
fn show(
    b: &mut FunctionBuilder,
    elem: &Elem,
    this: Local,
    t: Local,
    arity: u32,
) -> Result<(), Ice> {
    let capacity = i64::from(arity)
        .saturating_mul(SHOW_ESTIMATE)
        .saturating_add(2);
    let mut acc = b
        .call_runtime(
            RuntimeFn::StrWithCapacity,
            vec![Operand::Const(Const::Int(capacity))],
        )?
        .ok_or_else(|| Ice::new("`str_with_capacity` without a result"))?;
    acc = append_text(b, acc, "(")?;
    for i in 0..arity {
        if i > 0 {
            acc = append_text(b, acc, ", ")?;
        }
        let value = elem.element(b, t, i)?;
        let f = elem.closure(b, this, i)?;
        let text = b.assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Indirect(f),
                args: vec![Operand::Local(value)],
            },
        )?;
        acc = append(b, acc, Operand::Local(text))?;
    }
    acc = append_text(b, acc, ")")?;
    b.terminate(Terminator::Return(Operand::Local(acc)))
}

/// The elements of `lhs` and `rhs` compared left to right: the first
/// result other than `Equal`, or the last element's (a tail call).
fn compare(
    b: &mut FunctionBuilder,
    elem: &Elem,
    this: Local,
    lhs: Local,
    rhs: Local,
    arity: u32,
) -> Result<(), Ice> {
    let Some(last) = arity.checked_sub(1) else {
        return b.terminate(Terminator::Return(Operand::Const(Const::StaticCtor(
            builtin::EQUAL,
        ))));
    };
    for i in 0..arity {
        let x = elem.element(b, lhs, i)?;
        let y = elem.element(b, rhs, i)?;
        let f = elem.closure(b, this, i)?;
        let args = vec![Operand::Local(x), Operand::Local(y)];
        if i == last {
            return b.terminate(Terminator::TailCall {
                callee: Callee::Indirect(f),
                args,
            });
        }
        let order = b.assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Indirect(f),
                args,
            },
        )?;
        let tag = b.assign(
            Repr::Int,
            Rvalue::ReadTag {
                base: Operand::Local(order),
                ty: TagType::Order,
            },
        )?;
        let equal = b.assign(
            Repr::Bool,
            Rvalue::Compare {
                op: CmpOp::Eq,
                lhs: Operand::Local(tag),
                rhs: Operand::Const(Const::Int(ORDER_EQUAL)),
                repr: Repr::Int,
            },
        )?;
        let next = b.new_block()?;
        let decided = b.new_block()?;
        b.terminate(Terminator::Branch {
            cond: Operand::Local(equal),
            then_: next,
            else_: decided,
        })?;
        b.switch_to(decided)?;
        b.terminate(Terminator::Return(Operand::Local(order)))?;
        b.switch_to(next)?;
    }
    Err(Ice::new(format!(
        "tuple compare of arity {arity} fell through"
    )))
}
