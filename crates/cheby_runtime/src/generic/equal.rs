//! Structural equality (`==`, §3.13).

use super::raw::{Raw, View, f32_eq, float_eq, view};
use super::{Reached, hooks};
use crate::abi::Word;
use crate::alloc::scratch::ScratchVec;
use crate::layout::FieldKind;
use crate::object::{read_field, to_usize, word_to_ptr};
use crate::panic::fatal;

/// One comparison still to make.
enum Task {
    /// Two uniform words.
    Pair(Word, Word),
    /// Two raw fields.
    Raw(Raw, Raw),
}

/// The context of one equality walk, through which a runtime-defined
/// kind's `equal` function schedules the children that must also be equal.
#[derive(Debug, Default)]
pub struct Cx {
    pending: ScratchVec<(Word, Word)>,
}

impl Cx {
    /// Schedules the comparison of two borrowed children. Pairs are
    /// compared in the order they are pushed, after the current pair.
    pub fn push(&mut self, a: Word, b: Word) {
        self.pending.push((a, b));
    }
}

/// Whether two borrowed uniform words of the same type are structurally
/// equal (§3.13). Compares left to right, depth first, and stops at the
/// first difference; reaching a function value or a value of an external
/// type first gives `Err(Reached::Function)`, which callers turn into an
/// `EqualityOnFunction` panic (D-233). Identity is never compared, except
/// that of nominal descriptors, which is the identity of nominal types
/// (D-530, [`crate::desc`]).
///
/// A type with `@std_hooks` compares through its `equal` hook (D-342),
/// called on the running entry ([`crate::callback::call`]).
///
/// # Errors
///
/// [`Reached::Function`] when the walk reaches a function value or a
/// handle.
///
/// # Safety
///
/// `a` and `b` are tagged integers or live objects, not changed during the
/// call. On an entry's thread when the values may hold a type with hooks.
pub unsafe fn equal(a: Word, b: Word) -> Result<bool, Reached> {
    let mut stack = ScratchVec::of(Task::Pair(a, b));
    let mut later = ScratchVec::new();
    let mut cx = Cx::default();
    while let Some(task) = stack.pop() {
        let (a, b) = match task {
            Task::Raw(x, y) => {
                if x.same(y) {
                    continue;
                }
                return Ok(false);
            }
            Task::Pair(a, b) => (a, b),
        };
        // SAFETY: every word on the stack is borrowed from `a` or `b`.
        if !unsafe { step(a, b, &mut later, &mut cx) }? {
            return Ok(false);
        }
        stack.push_reversed(&mut later);
    }
    Ok(true)
}

/// Compares one level of `a` and `b`, appending the comparisons of their
/// parts to `later` in order. `Ok(false)` when they already differ.
///
/// # Safety
///
/// As for [`equal`].
unsafe fn step(
    a: Word,
    b: Word,
    later: &mut ScratchVec<Task>,
    cx: &mut Cx,
) -> Result<bool, Reached> {
    // SAFETY: forwarded from the caller.
    let (va, vb) = unsafe { (view(a, "`==`"), view(b, "`==`")) };
    let same = match (va, vb) {
        (View::Closure | View::External(_), _) | (_, View::Closure | View::External(_)) => {
            return Err(Reached::Function);
        }
        (View::Tagged(x), View::Tagged(y)) | (View::Int(x), View::Int(y)) => x == y,
        (View::U64(x), View::U64(y)) => x == y,
        (View::Float(x), View::Float(y)) => float_eq(x, y),
        (View::F32(x), View::F32(y)) => f32_eq(x, y),
        (View::String(x), View::String(y)) | (View::Bytes(x), View::Bytes(y)) => x == y,
        (View::Tuple(xs), View::Tuple(ys)) => {
            if xs.len() != ys.len() {
                return Ok(false);
            }
            later.extend(xs.into_iter().zip(ys).map(|(x, y)| Task::Pair(x, y)));
            true
        }
        (View::Ctor(x), View::Ctor(y)) => {
            // Hashing and equality never use layout ids (D-538): the same
            // constructor may be registered twice under different ids.
            if x.type_key != y.type_key {
                return Ok(false);
            }
            // A type with hooks compares through its `equal` hook alone,
            // whatever the two constructors (D-584).
            if let Some(hooks) = x.hooks().or_else(|| y.hooks()) {
                // SAFETY: two live values of one type with these hooks.
                return Ok(unsafe { hooks::equal(hooks, a, b) });
            }
            if x.tag != y.tag {
                return Ok(false);
            }
            let (pa, pb) = (word_to_ptr(a), word_to_ptr(b));
            for (fx, fy) in x.fields.iter().zip(y.fields.iter()) {
                let offsets = (
                    to_usize(u64::from(fx.offset)),
                    to_usize(u64::from(fy.offset)),
                );
                // SAFETY: live constructors with these fields.
                let (wx, wy) = unsafe { (read_field(pa, offsets.0), read_field(pb, offsets.1)) };
                later.push(if fx.kind == FieldKind::Value {
                    Task::Pair(wx, wy)
                } else {
                    Task::Raw(Raw::decode(fx.kind, wx), Raw::decode(fy.kind, wy))
                });
            }
            true
        }
        (View::Custom(kind), View::Custom(other)) => {
            if kind != other {
                return Ok(false);
            }
            let Some(equal) = kind.equal else {
                fatal(format_args!(
                    "not implemented yet: equality of `{}` values",
                    kind.name
                ));
            };
            // SAFETY: live objects of one runtime-defined kind.
            let same = unsafe { equal(a, b, cx) };
            later.extend(cx.pending.drain().map(|(x, y)| Task::Pair(x, y)));
            same
        }
        // Two `dyn` values are equal when their descriptors are equal and
        // their values are equal (D-530): the descriptors first, so values
        // of different types are never compared.
        (View::Dyn { desc: da, value: x }, View::Dyn { desc: db, value: y }) => {
            later.push(Task::Pair(da, db));
            later.push(Task::Pair(x, y));
            true
        }
        // Descriptors are equal when they name the same nominal type with
        // equal arguments (D-530).
        (View::Desc(x), View::Desc(y)) => {
            if x.nominal != y.nominal || x.args.len() != y.args.len() {
                return Ok(false);
            }
            later.extend(
                x.args
                    .into_iter()
                    .zip(y.args)
                    .map(|(x, y)| Task::Pair(x, y)),
            );
            true
        }
        (View::Internal(info), _) | (_, View::Internal(info)) => fatal(format_args!(
            "`==` reached a `{}`, which is not a value",
            info.name
        )),
        // The encoding is canonical (D-521), so two shapes are two values.
        _ => false,
    };
    Ok(same)
}
