//! Function bodies: [`BodyCx`], the state of lowering one body, its
//! helpers for every lowering file, and statement dispatch (plan §4.2).
//!
//! Every lowering method is `pub(super)`, takes `&mut self` and returns
//! `Result<_, Ice>`. A method that lowers an expression returns
//! `Option<Operand>`: `None` means the expression diverged (the current
//! block ended in a tail call or a panic), so the rest of the enclosing
//! block is unreachable. A construct the back end cannot lower yet is
//! E0038 at the construct ([`BodyCx::unsupported`]), and its value is a
//! placeholder local, so lowering goes on and reports every such
//! construct of the body (D-510).

use cheby_diag::{Diagnostic, Span};
use cheby_iface::{Generics, Ice, Idx, Name, Ty};
use cheby_types::thir::{self, ExprId, PatKind};
use rustc_hash::FxHashSet;

use crate::builder::FunctionBuilder;
use crate::ir::{
    Abi, BlockId, Const, FnKey, Function, Local, Operand, Rvalue, Stmt, StrId, Terminator,
};
use crate::lower::{LowerCx, LowerError, Pending, not_supported};
use crate::repr::{Repr, repr_of};

/// Lowers a function, constant or test body into a Cheby-ABI function.
/// `dicts` uniform parameters come first (one per bound, D-453), then
/// the body's parameters. Closures and other functions the body needs go
/// to `pending`.
pub(super) fn lower_body(
    key: FnKey,
    span: Span,
    body: &thir::Body,
    dicts: usize,
    cx: &LowerCx<'_>,
    pending: &mut Pending,
) -> Result<Function, LowerError> {
    let generics = match key {
        FnKey::Item(def) => Some(&cx.defs.fn_sig(def)?.generics),
        _ => None,
    };
    let mut b = FunctionBuilder::new(key, Abi::Cheby, Repr::Value, span)?;
    b.set_entry_loc(&cx.location(span)?)?;
    let mut lx = BodyCx {
        cx,
        body,
        generics,
        b,
        locals: vec![None; body.locals.len()],
        diags: Vec::new(),
        poisoned: FxHashSet::default(),
        pending,
    };
    let lowered = lx.run(dicts);
    // A broken invariant after an unsupported construct is most likely
    // the placeholder's doing: the construct is what to report.
    if !lx.diags.is_empty() {
        return Err(LowerError::Unsupported(lx.diags));
    }
    lowered?;
    Ok(lx.b.finish()?)
}

/// The state of lowering one body.
pub(super) struct BodyCx<'a, 'cx> {
    pub(super) cx: &'a LowerCx<'cx>,
    pub(super) body: &'a thir::Body,
    /// The enclosing item's type parameters, to name types in E0038.
    pub(super) generics: Option<&'cx Generics>,
    pub(super) b: FunctionBuilder,
    /// The MIR local of each THIR local, by the resolver's id.
    pub(super) locals: Vec<Option<Local>>,
    /// E0038 for each outermost unsupported construct.
    pub(super) diags: Vec<Diagnostic>,
    /// Placeholder locals that stand for unsupported constructs. They are
    /// never defined; the function is dropped. Membership tests only, so
    /// the hash set's order never shows (D-264).
    pub(super) poisoned: FxHashSet<Local>,
    /// What the unit needs besides this body's function.
    pub(super) pending: &'a mut Pending,
}

impl<'a> BodyCx<'a, '_> {
    fn run(&mut self, dicts: usize) -> Result<(), Ice> {
        for _ in 0..dicts {
            self.b.add_param(Repr::Value, None, None)?;
        }
        let body = self.body;
        for &p in &body.params {
            let decl = body
                .local(p)
                .ok_or_else(|| Ice::new("a parameter without its local"))?;
            let repr = self.repr_of(decl.ty);
            let local = self.b.add_param(repr, Some(decl.name), Some(decl.ty))?;
            self.bind(p, local)?;
        }
        let value = self.expr(body.value, true)?;
        if let Some(value) = value {
            // Every Cheby-ABI function returns a uniform word (D-453).
            let ty = self.ty(body.value)?;
            let value = self.to_uniform(value, ty)?;
            self.terminate(Terminator::Return(value))?;
        }
        Ok(())
    }

    /// Binds THIR local `id` to MIR local `local`.
    pub(super) fn bind<I: Idx>(&mut self, id: I, local: Local) -> Result<(), Ice> {
        let slot = self
            .locals
            .get_mut(id.index())
            .ok_or_else(|| Ice::new("a THIR local outside the body's table"))?;
        *slot = Some(local);
        Ok(())
    }

    /// The value of local `id`, of type `ty`.
    pub(super) fn lookup<I: Idx>(&mut self, id: I, ty: Ty) -> Result<Operand, Ice> {
        if let Some(local) = self.locals.get(id.index()).copied().flatten() {
            return Ok(Operand::Local(local));
        }
        if self.diags.is_empty() {
            return Err(Ice::new(format!(
                "THIR local {id:?} used before its binding"
            )));
        }
        // Bound by a construct that was not lowered.
        self.placeholder(self.repr_of(ty))
    }

    /// The THIR expression `id`.
    pub(super) fn expr_data(&self, id: ExprId) -> Result<&'a thir::Expr, Ice> {
        let body: &'a thir::Body = self.body;
        body.expr(id)
            .ok_or_else(|| Ice::new(format!("no THIR expression {id:?}")))
    }

    /// The type of expression `id`.
    pub(super) fn ty(&self, id: ExprId) -> Result<Ty, Ice> {
        Ok(self.expr_data(id)?.ty)
    }

    /// The representation of values of the closed type `ty`.
    pub(super) fn repr_of(&self, ty: Ty) -> Repr {
        repr_of(ty, self.cx.defs.globals)
    }

    /// `ty` as users write it (D-463).
    pub(super) fn show(&self, ty: Ty) -> String {
        self.cx.show(ty, self.generics)
    }

    /// A local of `repr` that stands for something not lowered.
    pub(super) fn placeholder(&mut self, repr: Repr) -> Result<Operand, Ice> {
        let local = self.new_local(repr)?;
        self.poisoned.insert(local);
        Ok(Operand::Local(local))
    }

    /// Whether `op` is a placeholder (or nothing was lowered for it).
    pub(super) fn is_poisoned(&self, op: &Operand) -> bool {
        matches!(op, Operand::Local(l) if self.poisoned.contains(l))
    }

    /// Reports E0038 "`<what>` are not supported yet" at `span` and returns
    /// a placeholder for the construct's value of type `ty`.
    pub(super) fn unsupported(
        &mut self,
        span: Span,
        what: &str,
        ty: Ty,
    ) -> Result<Option<Operand>, Ice> {
        self.diags.push(not_supported(span, what));
        self.placeholder(self.repr_of(ty)).map(Some)
    }

    /// A new local of `repr`, not yet defined.
    pub(super) fn new_local(&mut self, repr: Repr) -> Result<Local, Ice> {
        self.b.new_local(repr, None, None)
    }

    /// Defines a new local of `repr` as `value` in the current block.
    pub(super) fn define(&mut self, repr: Repr, value: Rvalue) -> Result<Local, Ice> {
        self.b.assign(repr, value)
    }

    /// Appends a statement to the current block.
    pub(super) fn emit(&mut self, stmt: Stmt) -> Result<(), Ice> {
        self.b.push(stmt)
    }

    /// Ends the current block.
    pub(super) fn terminate(&mut self, term: Terminator) -> Result<(), Ice> {
        self.b.terminate(term)
    }

    /// A static `String` constant holding `text`.
    pub(super) fn string_const(&mut self, text: &str) -> Result<Operand, Ice> {
        Ok(Operand::Const(Const::Str(self.b.string(text)?)))
    }

    /// The string-table id of `path:line:col` of the start of `span`
    /// (D-421, D-449), for [`Rvalue::Checked`] and [`Terminator::Panic`].
    pub(super) fn location_id(&mut self, span: Span) -> Result<StrId, Ice> {
        let text = self.cx.location(span)?;
        self.b.string(&text)
    }

    /// Binds every name of a pattern that is not lowered to a
    /// placeholder, so later uses lower without an error of their own.
    pub(super) fn bind_pattern(&mut self, pat: &thir::Pat) -> Result<(), Ice> {
        let mut bound = Vec::new();
        pat.walk(&mut |p| match &p.kind {
            PatKind::Bind(l) | PatKind::As { local: l, .. } => bound.push((*l, p.ty)),
            _ => {}
        });
        for (local, ty) in bound {
            let placeholder = self.placeholder(self.repr_of(ty))?;
            if let Operand::Local(l) = placeholder {
                self.bind(local, l)?;
            }
        }
        Ok(())
    }

    /// Binds a name to `op`, a value of type `ty` (already in `ty`'s
    /// representation). A fresh temporary takes the name; anything else
    /// (a constant, another binding) is moved into a new local, so every
    /// binding is its own local.
    pub(super) fn bind_name<I: Idx>(
        &mut self,
        id: I,
        name: Name,
        ty: Ty,
        op: Operand,
    ) -> Result<(), Ice> {
        let repr = self.repr_of(ty);
        let local = match op {
            Operand::Local(l) if self.poisoned.contains(&l) => l,
            Operand::Local(l) if self.b.name_local(l, name, ty)? => l,
            op => {
                let l = self.b.new_local(repr, Some(name), Some(ty))?;
                self.emit(Stmt::Assign {
                    dst: l,
                    value: Rvalue::Use(op),
                })?;
                l
            }
        };
        self.bind(id, local)
    }

    /// Lowers one statement of a block whose span is `block_span`.
    /// `None` means the statement diverged.
    pub(super) fn stmt(&mut self, stmt: &thir::Stmt, block_span: Span) -> Result<Option<()>, Ice> {
        match stmt {
            thir::Stmt::Let { pat, init } => self.let_stmt(pat, *init),
            thir::Stmt::Expr(e) => {
                // The value is unused; the RC pass drops it.
                Ok(self.expr(*e, false)?.map(|_| ()))
            }
            thir::Stmt::Assert { cond, message } => self.assert_stmt(*cond, *message),
            thir::Stmt::LetAssert { pat, init, message } => self.let_assert(pat, *init, *message),
            _ => {
                self.diags
                    .push(not_supported(block_span, "this kind of statement"));
                Ok(Some(()))
            }
        }
    }
}

/// Helpers for the owners of plan §4.2 that today's lowering does not
/// use yet.
#[expect(
    dead_code,
    reason = "helpers of plan §4.2 for wave 2's lowering files; the wave-2 IR steward removes this once all are used"
)]
impl BodyCx<'_, '_> {
    /// A new empty block. The current block does not change.
    pub(super) fn new_block(&mut self) -> Result<BlockId, Ice> {
        self.b.new_block()
    }

    /// A new parameter of `block`.
    pub(super) fn block_param(&mut self, block: BlockId, repr: Repr) -> Result<Local, Ice> {
        self.b.add_block_param(block, repr)
    }

    /// Makes `block` the current block.
    pub(super) fn switch_to(&mut self, block: BlockId) -> Result<(), Ice> {
        self.b.switch_to(block)
    }

    /// Whether the current block has ended.
    pub(super) fn is_terminated(&self) -> bool {
        self.b.is_terminated()
    }

    /// Defines the existing local `dst` as `value`.
    pub(super) fn assign(&mut self, dst: Local, value: Rvalue) -> Result<(), Ice> {
        self.emit(Stmt::Assign { dst, value })
    }

    /// Ends the current block with a jump to `block`, passing `args`.
    pub(super) fn goto(&mut self, block: BlockId, args: Vec<Operand>) -> Result<(), Ice> {
        self.terminate(Terminator::Goto {
            target: block,
            args,
        })
    }

    /// The `path:line:col` of the start of `span` as a static `String`
    /// (D-449).
    pub(super) fn location(&mut self, span: Span) -> Result<Operand, Ice> {
        let id = self.location_id(span)?;
        Ok(Operand::Const(Const::Str(id)))
    }

    /// The source text of `span`, as written (for `assert` messages,
    /// D-096).
    pub(super) fn source_text(&self, span: Span) -> Result<String, Ice> {
        self.cx
            .sources
            .file(span.file)
            .and_then(|f| span.range.slice(f.text()))
            .map(str::to_owned)
            .ok_or_else(|| Ice::new(format!("no source text for span {span:?}")))
    }

    /// The unit's pending functions, shims, statics and trampolines.
    pub(super) fn pending(&mut self) -> &mut Pending {
        self.pending
    }

    /// The span of the value a body returns: the innermost tail of its
    /// blocks.
    pub(super) fn tail_span(&self, mut id: ExprId) -> Result<Span, Ice> {
        loop {
            let e = self.expr_data(id)?;
            match &e.kind {
                thir::ExprKind::Block { tail: Some(t), .. } => id = *t,
                _ => return Ok(e.span),
            }
        }
    }
}
