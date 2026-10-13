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

use cheby_iface::{FnSig, Ice, builtin};
use cheby_types::thir;

use crate::builder::FunctionBuilder;
use crate::ir::{
    Abi, Callee, Const, FnKey, Function, LayoutRef, Operand, RuntimeFn, Rvalue, Stmt, Terminator,
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
}

impl Intrinsic {
    /// Every intrinsic, in table order.
    pub const ALL: [Self; 1] = [Self::ScopeRun];

    /// The native symbol that names the intrinsic.
    #[must_use]
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::ScopeRun => "cheby_rt_scope_run",
        }
    }

    /// The intrinsic named by `symbol`, if any.
    #[must_use]
    pub fn from_symbol(symbol: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|i| i.symbol() == symbol)
    }
}

/// The native symbols of [`Intrinsic::ALL`], in table order.
static SYMBOLS: [&str; Intrinsic::ALL.len()] = [Intrinsic::ScopeRun.symbol()];

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
    }
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

/// The internal error for a std declaration of `intrinsic` whose
/// parameters or result do not have the representations it needs.
fn signature_ice(intrinsic: Intrinsic, reprs: &[Repr]) -> Ice {
    Ice::new(format!(
        "the std intrinsic `{}` is declared with parameters {reprs:?}, not the ones its \
         generated body takes (D-693)",
        intrinsic.symbol()
    ))
}
