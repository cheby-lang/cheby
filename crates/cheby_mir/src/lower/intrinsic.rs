//! Std intrinsics (D-693): standard-library externals whose native symbol
//! is in a fixed table of `cheby_rt_` names get a body the compiler
//! generates instead of a foreign call. Only `std` may bind `cheby_rt_`
//! symbols (E0055, D-455), so users cannot reach them. Nothing else about
//! them is special: the resolver, the interface and the JS binding
//! (`@external(js, …)`, D-661) treat them like any kernel.
//!
//! M2 has one intrinsic, `std::fiber`'s private
//! `scope_run<T>(timed: Bool, seconds: Int, nanoseconds: Int,
//! body: fn(Scope) -> T) -> Result<T, Nil>`. Its body opens a scope, calls
//! `body` with an unwind edge and closes the scope, with no Cheby call in
//! between, so no entry check can deliver a cancellation between the open
//! and the close (D-693, D-665, D-666). On failure it returns the static
//! `Err(Nil)`; why the scope failed stays in the fiber.
//!
//! The body is generated with its reference counts and its unwind edges in
//! place (`Function::precounted`, D-744): the RC pass
//! leaves it alone and the unwind pass keeps its edges and its entry pad.
//!
//! `std::list`'s `push` and `prepend` are intrinsics too (D-1758). The body of
//! each is one runtime helper, `list_push` or `list_prepend`, which takes
//! the function's two parameters over, so nothing is counted on the way:
//! a wrapper would pass them borrowed to a kernel and drop them after the
//! call (D-525). Code generation writes a unique list's leaf in place of
//! the call where it can. These two bodies are counted like any other
//! function's.
//!
//! `std::list`'s `sort_with` and the private `sys_push`, `sys_pop`,
//! `sys_from_list` and `sys_to_list` of `std::heap` are intrinsics too.
//! Their kernels call the comparison back through a trampoline (D-526,
//! D-1448, D-1613); each body runs the same algorithm in steps instead and
//! calls the comparison itself, an ordinary call of a function value that
//! the release profile clones and inlines like any other (D-1780). A
//! runtime helper starts the work and keeps its state; then a loop asks a
//! step helper for the next two elements to compare, reads them, calls the
//! comparison and gives the step the tag of its `Order`, until the step
//! returns 0 ([`stepped`], `cheby_runtime`'s `list/steps.rs` and
//! `list/heap/steps.rs`). These bodies are counted like any other
//! function's.

use cheby_iface::{FnSig, Ice, builtin};
use cheby_types::thir;

use crate::builder::FunctionBuilder;
use crate::ir::{
    Abi, Callee, CmpOp, Const, FnKey, Function, LayoutRef, Operand, RuntimeFn, Rvalue, Stmt,
    TagType, Terminator,
};
use crate::lower::LowerCx;
use crate::repr::{Repr, repr_of};

/// A std intrinsic, by the native symbol its `@external(native, …)` names
/// (D-693).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Intrinsic {
    /// `std::fiber`'s `scope_run`: open, call the body with an unwind
    /// edge, close (D-693).
    ScopeRun,
    /// `std::list`'s `push`: `list_push` of its two parameters (D-1758).
    ListPush,
    /// `std::list`'s `prepend`: `list_prepend` of its two parameters.
    ListPrepend,
    /// `std::list`'s `sort_with`: D-1448's merge sort in steps.
    ListSortWith,
    /// `std::heap`'s `sys_push`: D-1613's sift-up in steps.
    HeapPush,
    /// `std::heap`'s `sys_pop`: D-1613's sift-down in steps.
    HeapPop,
    /// `std::heap`'s `sys_from_list`: D-1613's heapify in steps.
    HeapFromList,
    /// `std::heap`'s `sys_to_list`: D-1613's pops of a copy in steps.
    HeapToList,
}

impl Intrinsic {
    /// Every intrinsic, in table order.
    pub const ALL: [Self; 8] = [
        Self::ScopeRun,
        Self::ListPush,
        Self::ListPrepend,
        Self::ListSortWith,
        Self::HeapPush,
        Self::HeapPop,
        Self::HeapFromList,
        Self::HeapToList,
    ];

    /// The native symbol that names the intrinsic.
    #[must_use]
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::ScopeRun => "cheby_rt_scope_run",
            Self::ListPush => "cheby_rt_std_list_push",
            Self::ListPrepend => "cheby_rt_std_list_prepend",
            Self::ListSortWith => "cheby_rt_std_list_sort_with",
            Self::HeapPush => "cheby_rt_std_heap_push",
            Self::HeapPop => "cheby_rt_std_heap_pop",
            Self::HeapFromList => "cheby_rt_std_heap_from_list",
            Self::HeapToList => "cheby_rt_std_heap_to_list",
        }
    }

    /// The intrinsic named by `symbol`, if any.
    #[must_use]
    pub fn from_symbol(symbol: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|i| i.symbol() == symbol)
    }
}

/// The native symbols of [`Intrinsic::ALL`], in table order.
static SYMBOLS: [&str; Intrinsic::ALL.len()] = [
    Intrinsic::ScopeRun.symbol(),
    Intrinsic::ListPush.symbol(),
    Intrinsic::ListPrepend.symbol(),
    Intrinsic::ListSortWith.symbol(),
    Intrinsic::HeapPush.symbol(),
    Intrinsic::HeapPop.symbol(),
    Intrinsic::HeapFromList.symbol(),
    Intrinsic::HeapToList.symbol(),
];

/// The native symbols of the std intrinsics (D-693). The runtime never
/// registers them (`cheby_runtime::abi::sym::INTRINSICS` lists the same
/// names), because no code calls them.
#[must_use]
pub fn intrinsic_symbols() -> &'static [&'static str] {
    &SYMBOLS
}

/// The generated body of `intrinsic`, the std function `f` with signature
/// `sig`.
///
/// # Errors
///
/// An [`Ice`] if the std declaration does not have the signature the
/// intrinsic is generated for.
pub(super) fn lower_intrinsic(
    intrinsic: Intrinsic,
    f: &thir::FnDef,
    sig: &FnSig,
    cx: &LowerCx<'_>,
) -> Result<Function, Ice> {
    match intrinsic {
        Intrinsic::ScopeRun => scope_run(f, sig, cx),
        Intrinsic::ListPush => list_end(intrinsic, RuntimeFn::ListPush, f, sig, cx),
        Intrinsic::ListPrepend => list_end(intrinsic, RuntimeFn::ListPrepend, f, sig, cx),
        Intrinsic::ListSortWith
        | Intrinsic::HeapPush
        | Intrinsic::HeapPop
        | Intrinsic::HeapFromList
        | Intrinsic::HeapToList => stepped(intrinsic, f, sig, cx),
    }
}

/// The body of `std::list`'s `push` or `prepend`, `(xs: List<T>, item: T)
/// -> List<T>`:
///
/// ```text
/// entry: result = helper(xs, item)
///        return result
/// ```
///
/// `helper` is `list_push` or `list_prepend`, which consumes both
/// parameters, even when it raises (D-698), so the body counts nothing
/// and has nothing to release on an unwinding.
fn list_end(
    intrinsic: Intrinsic,
    helper: RuntimeFn,
    f: &thir::FnDef,
    sig: &FnSig,
    cx: &LowerCx<'_>,
) -> Result<Function, Ice> {
    let globals = cx.defs.globals;
    let reprs: Vec<Repr> = sig.params.iter().map(|p| repr_of(p.ty, globals)).collect();
    let ([xs, item], [Repr::Value, Repr::Value]) = (&*sig.params, reprs.as_slice()) else {
        return Err(signature_ice(intrinsic, &reprs));
    };
    if repr_of(sig.ret, globals) != Repr::Value {
        return Err(signature_ice(intrinsic, &reprs));
    }
    let mut b = FunctionBuilder::new(FnKey::Item(f.def), Abi::Cheby, Repr::Value, f.span)?;
    b.set_entry_loc(&cx.location(f.span)?)?;
    let xs = b.add_param(Repr::Value, Some(xs.name), Some(xs.ty))?;
    let item = b.add_param(Repr::Value, Some(item.name), Some(item.ty))?;
    let result = b
        .call_runtime(helper, vec![Operand::Local(xs), Operand::Local(item)])?
        .ok_or_else(|| Ice::new(format!("`{}` has no result", helper.name())))?;
    b.terminate(Terminator::Return(Operand::Local(result)))?;
    b.finish()
}

/// `scope_run`'s body (plan §2.4, D-693):
///
/// ```text
/// precounted
/// entry unwind <open raised>
/// entry:        eval effect_check(str "cheby_rt_scope_run") ; unwind <open raised>
///               scope = scope_open(timed, seconds, nanoseconds) ; unwind <open raised>
///               dup scope
///               value = call indirect body(scope) ; unwind <body unwound>
///               ok = scope_close(scope, false) ; unwind <close raised>
///               branch ok -> <succeeded>, <failed>
/// succeeded:    result = alloc Ok(value) ; unwind <close raised>
///               return result
/// failed:       drop value
///               return static Err(Nil)
/// body unwound: clear_unwind
///               _ = scope_close(scope, true) ; unwind <resume>
///               return static Err(Nil)
/// close raised: drop value
///               goto <resume>
/// open raised:  drop body
///               goto <resume>
/// resume:       resume
/// ```
///
/// The effect check comes first, so constant evaluation reaching `scope`
/// is E0077 (D-537) before anything is open. `scope_open` may raise only
/// out of memory, before anything is open. The body's call consumes `body`
/// and one count of `scope`, even when it unwinds; `scope_close` consumes
/// `scope`, even when it raises (D-698). The edges and the entry pad are
/// the ones the unwind pass would give each statement, since its pads are
/// chosen by the owned set at the edge's source (D-698).
fn scope_run(f: &thir::FnDef, sig: &FnSig, cx: &LowerCx<'_>) -> Result<Function, Ice> {
    let globals = cx.defs.globals;
    let reprs: Vec<Repr> = sig.params.iter().map(|p| repr_of(p.ty, globals)).collect();
    let ([timed, seconds, nanoseconds, body], [Repr::Bool, Repr::Int, Repr::Int, Repr::Value]) =
        (&*sig.params, reprs.as_slice())
    else {
        return Err(signature_ice(Intrinsic::ScopeRun, &reprs));
    };
    // The generic result `Result<T, Nil>` is a uniform word on every
    // instantiation, as every Cheby-ABI result is.
    if repr_of(sig.ret, globals) != Repr::Value {
        return Err(signature_ice(Intrinsic::ScopeRun, &reprs));
    }
    let mut b = FunctionBuilder::new(FnKey::Item(f.def), Abi::Cheby, Repr::Value, f.span)?;
    b.set_entry_loc(&cx.location(f.span)?)?;
    b.set_precounted();
    let timed = b.add_param(Repr::Bool, Some(timed.name), Some(timed.ty))?;
    let seconds = b.add_param(Repr::Int, Some(seconds.name), Some(seconds.ty))?;
    let nanoseconds = b.add_param(Repr::Int, Some(nanoseconds.name), Some(nanoseconds.ty))?;
    let body = b.add_param(Repr::Value, Some(body.name), Some(body.ty))?;
    let entry = b.current();
    let succeeded = b.new_block()?;
    let failed = b.new_block()?;
    let body_unwound = b.new_block()?;
    let close_raised = b.new_block()?;
    let open_raised = b.new_block()?;
    let resume = b.new_block()?;
    // The entry check raises with `{body}` owned.
    b.set_entry_unwind(open_raised);

    b.switch_to(entry)?;
    let symbol = b.string(Intrinsic::ScopeRun.symbol())?;
    b.call_runtime(
        RuntimeFn::EffectCheck,
        vec![Operand::Const(Const::Str(symbol))],
    )?;
    b.unwind_last(open_raised)?;
    let scope = b
        .call_runtime(
            RuntimeFn::ScopeOpen,
            vec![
                Operand::Local(timed),
                Operand::Local(seconds),
                Operand::Local(nanoseconds),
            ],
        )?
        .ok_or_else(|| Ice::new("`scope_open` has no result"))?;
    b.unwind_last(open_raised)?;
    b.push(Stmt::Dup(scope))?;
    let value = b.assign(
        Repr::Value,
        Rvalue::Call {
            callee: Callee::Indirect(body),
            args: vec![Operand::Local(scope)],
        },
    )?;
    b.unwind_last(body_unwound)?;
    let ok = b
        .call_runtime(
            RuntimeFn::ScopeClose,
            vec![Operand::Local(scope), Operand::Const(Const::Bool(false))],
        )?
        .ok_or_else(|| Ice::new("`scope_close` has no result"))?;
    b.unwind_last(close_raised)?;
    b.terminate(Terminator::Branch {
        cond: Operand::Local(ok),
        then_: succeeded,
        else_: failed,
    })?;

    // The scope has ended, so raising here is ordinary.
    b.switch_to(succeeded)?;
    let result = b.assign(
        Repr::Value,
        Rvalue::Alloc {
            layout: LayoutRef::Ctor(builtin::OK),
            fields: vec![Operand::Local(value)],
        },
    )?;
    b.unwind_last(close_raised)?;
    b.terminate(Terminator::Return(Operand::Local(result)))?;

    b.switch_to(failed)?;
    b.push(Stmt::Drop(value))?;
    b.terminate(Terminator::Return(Operand::Const(Const::ErrNil)))?;

    // The body unwound into this frame: the runtime's close decides
    // whether the unwinding is caught (D-666); if it passes through,
    // `scope_close` raises again.
    b.switch_to(body_unwound)?;
    b.clear_unwind()?;
    b.call_runtime(
        RuntimeFn::ScopeClose,
        vec![Operand::Local(scope), Operand::Const(Const::Bool(true))],
    )?;
    b.unwind_last(resume)?;
    b.terminate(Terminator::Return(Operand::Const(Const::ErrNil)))?;

    // The body's value, when the close or the `Ok` raises.
    b.switch_to(close_raised)?;
    b.push(Stmt::Drop(value))?;
    b.terminate(Terminator::Goto {
        target: resume,
        args: Vec::new(),
    })?;

    // The entry check, the effect check or the open raised: nothing is
    // open yet, and only `body` is owned.
    b.switch_to(open_raised)?;
    b.push(Stmt::Drop(body))?;
    b.terminate(Terminator::Goto {
        target: resume,
        args: Vec::new(),
    })?;

    b.switch_to(resume)?;
    b.terminate(Terminator::Resume)?;
    b.finish()
}

/// The parameters' representations of a stepped intrinsic, the comparison
/// last.
fn stepped_params(intrinsic: Intrinsic) -> &'static [Repr] {
    match intrinsic {
        Intrinsic::ListSortWith => &[Repr::Value, Repr::Value],
        Intrinsic::HeapPush => &[Repr::Value, Repr::Value, Repr::Bool, Repr::Value],
        Intrinsic::HeapPop | Intrinsic::HeapFromList | Intrinsic::HeapToList => {
            &[Repr::Value, Repr::Bool, Repr::Value]
        }
        Intrinsic::ScopeRun | Intrinsic::ListPush | Intrinsic::ListPrepend => &[],
    }
}

/// The body of an intrinsic whose comparison generated code calls (module
/// documentation). `sort_with(xs, by)`:
///
/// ```text
/// entry:        state = sort_start(xs)
///               at0 = buffer_step(state, -1)
///               goto head(at0)
/// head(at):     more = ne at, 0
///               branch more -> body, done
/// body:         eval loop_check()
///               a = pair_left(at)
///               b = pair_right(at)
///               order = call indirect by(a, b)
///               tag = tag order
///               next = buffer_step(state, tag)
///               goto head(next)
/// done:         result = buffer_finish(state, xs)
///               return result
/// ```
///
/// `sys_to_list(items, max, by)` is the same with `heap_list_start(items,
/// max, by)`. `sys_push(items, item, max, by)` keeps its steps in a stack
/// slot and returns the list its start returned:
///
/// ```text
/// entry:        slot = heap_slot()
///               list = heap_push(items, item, max, by, slot)
///               at0 = heap_first(slot)
///               goto head(at0)
/// ...           (as above, with next = heap_step(slot, tag))
/// done:         return list
/// ```
///
/// and `sys_pop` and `sys_from_list` start with `heap_pop` and
/// `heap_heapify`. The state and the list stay held to the end, so a
/// comparison that panics or is cancelled leaves through the call's edge,
/// whose cleanup releases them as any value (D-698). The loop is no self
/// tail call, so it makes the entry check itself (`loop_check`), once per
/// comparison, as the comparison's own entry did when a kernel called it:
/// with the comparison inlined, the loop is still interrupted.
#[expect(
    clippy::too_many_lines,
    reason = "the whole body, block by block, as its documentation shows it"
)]
fn stepped(
    intrinsic: Intrinsic,
    f: &thir::FnDef,
    sig: &FnSig,
    cx: &LowerCx<'_>,
) -> Result<Function, Ice> {
    let globals = cx.defs.globals;
    let reprs: Vec<Repr> = sig.params.iter().map(|p| repr_of(p.ty, globals)).collect();
    if reprs.as_slice() != stepped_params(intrinsic) || repr_of(sig.ret, globals) != Repr::Value {
        return Err(signature_ice(intrinsic, &reprs));
    }
    let mut b = FunctionBuilder::new(FnKey::Item(f.def), Abi::Cheby, Repr::Value, f.span)?;
    b.set_entry_loc(&cx.location(f.span)?)?;
    let mut params = Vec::with_capacity(sig.params.len());
    for (param, &repr) in sig.params.iter().zip(&reprs) {
        params.push(b.add_param(repr, Some(param.name), Some(param.ty))?);
    }
    let (&by, args) = params
        .split_last()
        .ok_or_else(|| signature_ice(intrinsic, &reprs))?;
    let local = |l| Operand::Local(l);
    let result = |l: Option<_>, helper: RuntimeFn| {
        l.ok_or_else(|| Ice::new(format!("`{}` has no result", helper.name())))
    };
    // The start: the state the steps take, and what the end returns.
    let (step, state, end) = match intrinsic {
        Intrinsic::ListSortWith | Intrinsic::HeapToList => {
            let start = if intrinsic == Intrinsic::ListSortWith {
                RuntimeFn::SortStart
            } else {
                RuntimeFn::HeapListStart
            };
            // `sort_with` gives the start its list; `sys_to_list` its
            // list, direction and comparison.
            let mut start_args: Vec<Operand> = args.iter().copied().map(local).collect();
            if start == RuntimeFn::HeapListStart {
                start_args.push(local(by));
            }
            let state = result(b.call_runtime(start, start_args)?, start)?;
            (RuntimeFn::BufferStep, state, None)
        }
        Intrinsic::HeapPush | Intrinsic::HeapPop | Intrinsic::HeapFromList => {
            let start = match intrinsic {
                Intrinsic::HeapPush => RuntimeFn::HeapPush,
                Intrinsic::HeapPop => RuntimeFn::HeapPop,
                Intrinsic::HeapFromList
                | Intrinsic::ScopeRun
                | Intrinsic::ListPush
                | Intrinsic::ListPrepend
                | Intrinsic::ListSortWith
                | Intrinsic::HeapToList => RuntimeFn::HeapHeapify,
            };
            let slot = result(
                b.call_runtime(RuntimeFn::HeapSlot, Vec::new())?,
                RuntimeFn::HeapSlot,
            )?;
            let mut start_args: Vec<Operand> = args.iter().copied().map(local).collect();
            start_args.push(local(by));
            start_args.push(local(slot));
            let list = result(b.call_runtime(start, start_args)?, start)?;
            (RuntimeFn::HeapStep, slot, Some(list))
        }
        Intrinsic::ScopeRun | Intrinsic::ListPush | Intrinsic::ListPrepend => {
            return Err(signature_ice(intrinsic, &reprs));
        }
    };
    // The first pair: a step of a buffered state, or the address a heap's
    // start left in its slot.
    let first = if step == RuntimeFn::HeapStep {
        result(
            b.call_runtime(RuntimeFn::HeapFirst, vec![local(state)])?,
            RuntimeFn::HeapFirst,
        )?
    } else {
        result(
            b.call_runtime(step, vec![local(state), Operand::Const(Const::Int(-1))])?,
            step,
        )?
    };
    let head = b.new_block()?;
    let at = b.add_block_param(head, Repr::U64)?;
    let body = b.new_block()?;
    let done = b.new_block()?;
    b.terminate(Terminator::Goto {
        target: head,
        args: vec![local(first)],
    })?;

    b.switch_to(head)?;
    let more = b.assign(
        Repr::Bool,
        Rvalue::Compare {
            op: CmpOp::Ne,
            lhs: local(at),
            rhs: Operand::Const(Const::Sized {
                repr: Repr::U64,
                bits: 0,
            }),
            repr: Repr::U64,
        },
    )?;
    b.terminate(Terminator::Branch {
        cond: local(more),
        then_: body,
        else_: done,
    })?;

    b.switch_to(body)?;
    b.call_runtime(RuntimeFn::LoopCheck, Vec::new())?;
    let left = result(
        b.call_runtime(RuntimeFn::PairLeft, vec![local(at)])?,
        RuntimeFn::PairLeft,
    )?;
    let right = result(
        b.call_runtime(RuntimeFn::PairRight, vec![local(at)])?,
        RuntimeFn::PairRight,
    )?;
    let order = b.assign(
        Repr::Value,
        Rvalue::Call {
            callee: Callee::Indirect(by),
            args: vec![local(left), local(right)],
        },
    )?;
    let tag = b.assign(
        Repr::Int,
        Rvalue::ReadTag {
            base: local(order),
            ty: TagType::Order,
        },
    )?;
    let next = result(b.call_runtime(step, vec![local(state), local(tag)])?, step)?;
    b.terminate(Terminator::Goto {
        target: head,
        args: vec![local(next)],
    })?;

    b.switch_to(done)?;
    let value = if let Some(list) = end {
        list
    } else {
        // The finish takes the state and the list the start borrowed,
        // which stays held to here.
        let source = args
            .first()
            .copied()
            .ok_or_else(|| signature_ice(intrinsic, &reprs))?;
        result(
            b.call_runtime(RuntimeFn::BufferFinish, vec![local(state), local(source)])?,
            RuntimeFn::BufferFinish,
        )?
    };
    b.terminate(Terminator::Return(local(value)))?;
    b.finish()
}

/// The internal error for a std declaration of `intrinsic` whose
/// parameters or result do not have the representations it needs.
fn signature_ice(intrinsic: Intrinsic, reprs: &[Repr]) -> Ice {
    Ice::new(format!(
        "the std intrinsic `{}` is declared with parameters {reprs:?}, not the ones its \
         generated body takes (D-693)",
        intrinsic.symbol()
    ))
}

#[cfg(test)]
#[path = "steps_tests.rs"]
mod steps_tests;
