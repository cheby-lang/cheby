//! THIR to MIR (D2).
//!
//! [`lower_fn`] is the parallel unit (D-263): it reads one item's THIR,
//! the module's own interface and its dependencies' interfaces, and
//! returns the item's function followed by its shims and closures, with
//! function-local strings. [`lower_items`] runs it per selected item in
//! `DefIndex` order and computes the unit's callee declarations, layouts
//! and names; [`lower_module`] selects every item. [`lower_tests`] and
//! [`lower_entries`] produce the other two unit kinds.
//!
//! The front end no longer gates what the back end cannot run (D-451,
//! plan §8): a construct, a value representation or an external
//! signature that lowering does not implement yet is E0038 at the
//! construct, once per outermost construct, never an [`Ice`]. Lowering
//! goes on after one, so a run reports every such construct of the items
//! it needs at once; an item with one produces no function. `cheby run`
//! lowers only the items reachable from its entry ([`crate::reach`]), so
//! `cheby check` never reports these.

mod entry;
mod expr;
mod external;

use std::collections::BTreeSet;
use std::fmt;

use cheby_diag::{Diagnostic, SourceMap, Span, codes};
use cheby_iface::{DefId, DisplayCx, Generics, Ice, Ty, display};
use cheby_types::thir;

pub use entry::{EntryRequest, lower_entries};

use crate::abi::abi_sig;
use crate::defs::Defs;
use crate::ir::{
    Callee, CalleeDecl, Const, EntryKind, FnKey, Function, LayoutRef, MirModule, Operand, Rvalue,
    Stmt, Terminator, UnitKind,
};
use crate::layout::{LayoutDesc, module_layouts};

/// What lowering reads besides THIR.
#[derive(Clone, Copy, Debug)]
pub struct LowerCx<'a> {
    pub defs: Defs<'a>,
    /// To render locations as `path:line:col` (D-421), so MIR and its dump
    /// are self-contained.
    pub sources: &'a SourceMap,
}

impl LowerCx<'_> {
    /// `path:line:col` of the start of `span`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the span's file is not in the source map.
    pub fn location(&self, span: Span) -> Result<String, Ice> {
        self.sources
            .location(span)
            .map(|loc| loc.to_string())
            .ok_or_else(|| Ice::new(format!("no source file for span {span:?}")))
    }

    /// `ty` as users write it (D-463), for diagnostics about the module
    /// being lowered.
    #[must_use]
    pub fn show(&self, ty: Ty, generics: Option<&Generics>) -> String {
        display(
            ty,
            &DisplayCx {
                globals: self.defs.globals,
                modules: self.defs.modules,
                own: self.defs.own,
                deps: self.defs.deps,
                generics,
            },
        )
    }
}

/// Why lowering produced no MIR.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LowerError {
    /// The items use constructs the back end does not implement yet:
    /// E0038 diagnostics, one per outermost construct, unsorted.
    Unsupported(Vec<Diagnostic>),
    /// A broken invariant.
    Ice(Ice),
}

impl From<Ice> for LowerError {
    fn from(ice: Ice) -> Self {
        Self::Ice(ice)
    }
}

impl fmt::Display for LowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(diags) => {
                let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
                write!(f, "unsupported: {}", messages.join("; "))
            }
            Self::Ice(ice) => f.write_str(ice.message()),
        }
    }
}

/// Collects the E0038 diagnostics of several items, so one run reports
/// all of them; an [`Ice`] stops at once.
#[derive(Debug, Default)]
pub struct Unsupported {
    diags: Vec<Diagnostic>,
}

impl Unsupported {
    /// The value of `result`, or `None` after recording its diagnostics.
    ///
    /// # Errors
    ///
    /// The [`Ice`] of `result`.
    pub fn take<T>(&mut self, result: Result<T, LowerError>) -> Result<Option<T>, Ice> {
        match result {
            Ok(value) => Ok(Some(value)),
            Err(LowerError::Unsupported(diags)) => {
                self.diags.extend(diags);
                Ok(None)
            }
            Err(LowerError::Ice(ice)) => Err(ice),
        }
    }

    /// Records one diagnostic.
    pub fn push(&mut self, diag: Diagnostic) {
        self.diags.push(diag);
    }

    /// Whether nothing was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.diags.is_empty()
    }

    /// The recorded diagnostics, in recording order.
    #[must_use]
    pub fn into_diagnostics(self) -> Vec<Diagnostic> {
        self.diags
    }

    /// `value`, or the recorded diagnostics as the error.
    ///
    /// # Errors
    ///
    /// [`LowerError::Unsupported`] if anything was recorded.
    pub fn finish<T>(self, value: T) -> Result<T, LowerError> {
        if self.diags.is_empty() {
            Ok(value)
        } else {
            Err(LowerError::Unsupported(self.diags))
        }
    }
}

/// E0038: "<what> are not supported yet", worded like the front end's old
/// gates, with the primary label on the construct (D-451).
pub(crate) fn not_supported(span: Span, what: &str) -> Diagnostic {
    Diagnostic::new(
        codes::NOT_SUPPORTED_YET,
        format!("{what} are not supported yet"),
    )
    .with_primary(span, "")
}

/// Lowers every function and constant initializer of a module into its
/// code unit. The result has no reference counts yet
/// ([`crate::count_and_check`]).
///
/// # Errors
///
/// [`LowerError::Unsupported`] for constructs the back end does not
/// implement yet, and an [`Ice`] on input inconsistent with the
/// interfaces.
pub fn lower_module(module: &thir::Module, cx: &LowerCx<'_>) -> Result<MirModule, LowerError> {
    lower_items(module, &|_| true, cx)
}

/// Lowers the functions and constant initializers of `module` that
/// `wanted` selects, in `DefIndex` order, with every constructor layout
/// of the module, into its code unit. `cheby run` selects the items
/// reachable from its entry ([`crate::reach`]).
///
/// # Errors
///
/// As [`lower_module`]: every selected item is lowered, so the error
/// holds the diagnostics of all of them.
pub fn lower_items(
    module: &thir::Module,
    wanted: &dyn Fn(DefId) -> bool,
    cx: &LowerCx<'_>,
) -> Result<MirModule, LowerError> {
    let mut fns: Vec<&thir::FnDef> = module.fns.iter().filter(|f| wanted(f.def)).collect();
    fns.sort_by_key(|f| f.def);
    let mut consts: Vec<&thir::ConstDef> = module.consts.iter().filter(|c| wanted(c.def)).collect();
    consts.sort_by_key(|c| c.def);
    let mut functions = Vec::new();
    let mut unsupported = Unsupported::default();
    // Items in `DefIndex` order: merge functions and constants.
    let mut fi = fns.into_iter().peekable();
    let mut ci = consts.into_iter().peekable();
    loop {
        let take_fn = match (fi.peek(), ci.peek()) {
            (Some(f), Some(c)) => f.def < c.def,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        if take_fn {
            if let Some(f) = fi.next()
                && let Some(lowered) = unsupported.take(lower_fn(f, cx))?
            {
                functions.extend(lowered);
            }
        } else if let Some(c) = ci.next() {
            let lowered = expr::lower_body(FnKey::ConstInit(c.def), c.span, &c.body, 0, cx);
            if let Some(f) = unsupported.take(lowered)? {
                functions.push(f);
            }
        }
    }
    let layouts = module_layouts(module.module, &cx.defs)?;
    let unit = finish_unit(UnitKind::Code(module.module), functions, layouts, cx)?;
    unsupported.finish(unit)
}

/// Lowers one module's `test` blocks into its tests unit (D-265).
///
/// # Errors
///
/// As [`lower_module`].
pub fn lower_tests(module: &thir::Module, cx: &LowerCx<'_>) -> Result<MirModule, LowerError> {
    let mut tests: Vec<&thir::TestDef> = module.tests.iter().collect();
    tests.sort_by_key(|t| t.def);
    let mut functions = Vec::new();
    let mut unsupported = Unsupported::default();
    for t in tests {
        let lowered = expr::lower_body(FnKey::Test(t.def), t.span, &t.body, 0, cx);
        if let Some(f) = unsupported.take(lowered)? {
            functions.push(f);
        }
    }
    let unit = finish_unit(UnitKind::Tests(module.module), functions, Vec::new(), cx)?;
    unsupported.finish(unit)
}

/// Lowers one top-level function: its body, or the Cheby-ABI wrapper of
/// its `@external(native, …)` (D-455).
///
/// # Errors
///
/// As [`lower_module`].
pub fn lower_fn(f: &thir::FnDef, cx: &LowerCx<'_>) -> Result<Vec<Function>, LowerError> {
    let sig = cx.defs.fn_sig(f.def)?;
    let dicts = sig
        .generics
        .params
        .iter()
        .map(|p| p.bounds.len())
        .sum::<usize>();
    match (&f.native_external, &f.body) {
        // With a body too, the body is for the other targets (D-207,
        // GAP(native-external-over-body-in-mir)).
        (Some(ext), _) => Ok(vec![external::lower_wrapper(f, ext, sig, cx)?]),
        (None, Some(body)) => Ok(vec![expr::lower_body(
            FnKey::Item(f.def),
            f.span,
            body,
            dicts,
            cx,
        )?]),
        // Only a JS external: the function does not exist on native
        // (§12.5), which only the cross-target check (C12) can reject.
        (None, None) => Err(LowerError::Unsupported(vec![not_supported(
            f.span,
            "functions without an `@external(native, …)` implementation",
        )])),
    }
}

/// Builds a unit from its functions: callee declarations, names, path.
fn finish_unit(
    unit: UnitKind,
    functions: Vec<Function>,
    layouts: Vec<LayoutDesc>,
    cx: &LowerCx<'_>,
) -> Result<MirModule, Ice> {
    let mut direct = BTreeSet::new();
    let mut mentioned = BTreeSet::new();
    for f in &functions {
        collect_defs(f, &mut direct, &mut mentioned);
    }
    for l in &layouts {
        if let LayoutRef::Ctor(d) = l.layout {
            mentioned.insert(d);
        }
    }
    let callees = direct
        .iter()
        .map(|&def| {
            Ok(CalleeDecl {
                def,
                sig: abi_sig(cx.defs.fn_sig(def)?, cx.defs.globals),
            })
        })
        .collect::<Result<Vec<_>, Ice>>()?;
    mentioned.extend(direct);
    let names = mentioned
        .into_iter()
        .map(|d| cx.defs.name(d))
        .collect::<Result<Vec<_>, Ice>>()?;
    let path = match unit {
        UnitKind::Code(m) | UnitKind::Tests(m) => cx.defs.modules.try_get(m)?.path.to_string(),
        UnitKind::Entries => String::new(),
    };
    Ok(MirModule {
        unit,
        path: path.into(),
        functions,
        callees,
        layouts,
        names,
    })
}

/// The direct callees of `f`, and every other def it mentions.
fn collect_defs(f: &Function, direct: &mut BTreeSet<DefId>, mentioned: &mut BTreeSet<DefId>) {
    key_defs(f.key, mentioned);
    let operand = |op: &Operand, mentioned: &mut BTreeSet<DefId>| {
        if let Operand::Const(c) = op {
            match c {
                Const::StaticCtor(d) | Const::FnValue(d) => {
                    mentioned.insert(*d);
                }
                Const::Code(key) => key_defs(*key, mentioned),
                Const::Int(_)
                | Const::Float(_)
                | Const::Bool(_)
                | Const::Sized { .. }
                | Const::Str(_)
                | Const::Nil => {}
            }
        }
    };
    let callee = |c: &Callee, direct: &mut BTreeSet<DefId>, mentioned: &mut BTreeSet<DefId>| match c
    {
        Callee::Direct(d) => {
            direct.insert(*d);
        }
        Callee::Code(key) => key_defs(*key, mentioned),
        Callee::Indirect(_) => {}
    };
    for block in &f.blocks {
        for stmt in &block.stmts {
            let (Stmt::Assign { value, .. } | Stmt::Eval(value)) = stmt else {
                continue;
            };
            match value {
                Rvalue::Use(op)
                | Rvalue::Box { value: op, .. }
                | Rvalue::Unbox { value: op, .. } => {
                    operand(op, mentioned);
                }
                Rvalue::Call { callee: c, args } => {
                    callee(c, direct, mentioned);
                    for a in args {
                        operand(a, mentioned);
                    }
                }
                Rvalue::CallForeign { args, .. } => args.iter().for_each(|a| operand(a, mentioned)),
                Rvalue::CallRuntime { func, .. } => match *func {},
                Rvalue::Alloc { layout, fields } => {
                    if let LayoutRef::Ctor(d) = layout {
                        mentioned.insert(*d);
                    }
                    for a in fields {
                        operand(a, mentioned);
                    }
                }
                Rvalue::Field { layout, .. } => {
                    if let LayoutRef::Ctor(d) = layout {
                        mentioned.insert(*d);
                    }
                }
                Rvalue::LoadConst(d) => {
                    mentioned.insert(*d);
                }
                Rvalue::ReadLayout { .. } => {}
                Rvalue::Checked { lhs, rhs, .. } | Rvalue::Compare { lhs, rhs, .. } => {
                    operand(lhs, mentioned);
                    operand(rhs, mentioned);
                }
            }
        }
        match &block.term {
            Terminator::Return(op) => operand(op, mentioned),
            Terminator::TailCall { callee: c, args } => {
                callee(c, direct, mentioned);
                for a in args {
                    operand(a, mentioned);
                }
            }
            Terminator::Goto { args, .. } => args.iter().for_each(|a| operand(a, mentioned)),
            Terminator::Branch { cond, .. } => operand(cond, mentioned),
            Terminator::Switch { on, .. } => operand(on, mentioned),
            Terminator::Panic { message, .. } => {
                if let Some(m) = message {
                    operand(m, mentioned);
                }
            }
            Terminator::Unreachable => {}
        }
    }
}

fn key_defs(key: FnKey, out: &mut BTreeSet<DefId>) {
    match key {
        FnKey::Item(d)
        | FnKey::ValueShim(d)
        | FnKey::Closure { owner: d, .. }
        | FnKey::ConstInit(d)
        | FnKey::Test(d)
        | FnKey::Entry(EntryKind::Main(d) | EntryKind::Test(d) | EntryKind::Const(d)) => {
            out.insert(d);
        }
        FnKey::Entry(EntryKind::Repl(_)) | FnKey::Trampoline(_) => {}
    }
}
