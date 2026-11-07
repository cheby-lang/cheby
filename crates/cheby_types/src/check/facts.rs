//! The hooks that fill the fact log, and the log's end (D-1380).
//!
//! With recording off, `BodyCx::facts` is `None`, each hook is one branch,
//! and nothing allocates. A hook only appends to the log, so it cannot
//! change a diagnostic or THIR. A caller passes borrowed values, and the
//! hook clones them behind that branch.

use cheby_diag::Span;
use cheby_iface::{DefId, Ice, IndexVec, Ty, TyKind};
use cheby_resolve::LocalId;
use cheby_syntax::NodeId;
use rustc_hash::FxHashSet;

use super::call::{Signature, Target};
use super::constraints::is_placeholder;
use super::{BodyCx, Expected};
use crate::exhaust;
use crate::facts::{
    BodyFacts, CallFact, CallLog, CaseFact, DynFact, ExprFact, ExprLog, FactLog, FieldFact,
    FieldLog, FieldSite, MismatchFact, OpFact, ShowFact, UnsatisfiedFact,
};
use crate::has_error;
use crate::ifaces::Unsatisfied;
use crate::infer::{InferTy, TyVid};
use crate::thir::{self, Callee, Expr, ExprId, ExprKind, InterpPart, OpImpl, PatId};

impl BodyCx<'_> {
    /// Logs the expression `id` under `node`. `out` is what its context
    /// got: `id`, or a `dyn` conversion around it.
    pub(crate) fn note_expr(
        &mut self,
        node: NodeId,
        id: ExprId,
        out: ExprId,
        expected: Option<&Expected>,
    ) {
        if let Some(log) = &mut self.facts {
            log.exprs.push(ExprLog {
                node,
                id,
                out,
                expected: expected.map(|e| e.ty.clone()),
            });
        }
    }

    /// Logs the pattern `id` under `node`.
    pub(crate) fn note_pat(&mut self, node: NodeId, id: PatId) {
        if let Some(log) = &mut self.facts {
            log.pats.push((node, id));
        }
    }

    /// Logs that the pattern `id` failed.
    pub(crate) fn note_failed_pat(&mut self, id: PatId) {
        if let Some(log) = &mut self.facts {
            log.failed_pats.push(id);
        }
    }

    /// Logs the callee `sig` of the expression `callee`, and returns `sig`
    /// with its place in the log. A callee with an error has no fact.
    pub(crate) fn note_call(&mut self, callee: NodeId, mut sig: Signature) -> Signature {
        let Some(log) = &mut self.facts else {
            return sig;
        };
        let (def, ty_args): (Option<DefId>, Box<[InferTy]>) = match &sig.target {
            Target::Fn { def, args, .. } | Target::Ctor { def, args } => (Some(*def), args.clone()),
            Target::Iface { iface_fn, self_ty } => (Some(*iface_fn), Box::new([self_ty.clone()])),
            Target::Value(_) => (None, Box::new([])),
            Target::Error => return sig,
        };
        sig.fact = Some(log.calls.len());
        log.calls.push(CallLog {
            callee,
            def,
            ty_args,
            params: sig.params.clone(),
            ret: sig.ret.clone(),
            call: None,
            args: Vec::new(),
        });
        sig
    }

    /// Logs the arguments of a call with the wrong number of them, one
    /// per parameter: in written order, around a capture's `hole`. The
    /// last of `ids` is a `use` callback when `callback` is set, and it
    /// belongs to the last parameter, whatever is written before it.
    pub(crate) fn note_call_args(
        &mut self,
        fact: Option<usize>,
        ids: Vec<ExprId>,
        hole: Option<usize>,
        callback: bool,
    ) {
        if let Some(call) = self.call_log(fact) {
            let mut args: Vec<Option<ExprId>> = ids.into_iter().map(Some).collect();
            if let Some(hole) = hole {
                args.insert(hole.min(args.len()), None);
            }
            if callback {
                let last = args.pop().flatten();
                args.resize(call.params.len().saturating_sub(1), None);
                args.push(last);
            }
            call.args = args;
        }
    }

    /// Logs the call expression of the callee at `fact`.
    pub(crate) fn note_call_expr(&mut self, fact: Option<usize>, id: ExprId) {
        if let Some(call) = self.call_log(fact) {
            call.call = Some(id);
        }
    }

    fn call_log(&mut self, fact: Option<usize>) -> Option<&mut CallLog> {
        self.facts.as_mut()?.calls.get_mut(fact?)
    }

    /// Logs a field read, and returns its place in the log for
    /// [`Self::note_field_owner`] and [`Self::note_field_ty`].
    pub(crate) fn note_field_read(
        &mut self,
        name_span: Span,
        name: &str,
        base: &InferTy,
    ) -> Option<usize> {
        let log = self.facts.as_mut()?;
        let at = log.fields.len();
        log.fields.push(FieldLog {
            name_span,
            name: self.input.globals.names.intern(name),
            site: FieldSite::Read,
            base: Some(base.clone()),
            owner: None,
            ctor: None,
            ty: None,
        });
        Some(at)
    }

    /// The type that has the field read at `fact`.
    pub(crate) fn note_field_owner(&mut self, fact: Option<usize>, owner: DefId) {
        if let Some(field) = self.field_log(fact) {
            field.owner = Some(owner);
        }
    }

    /// The type of the field read at `fact`.
    pub(crate) fn note_field_ty(&mut self, fact: Option<usize>, ty: &InferTy) {
        if let Some(field) = self.field_log(fact) {
            field.ty = Some(ty.clone());
        }
    }

    fn field_log(&mut self, fact: Option<usize>) -> Option<&mut FieldLog> {
        self.facts.as_mut()?.fields.get_mut(fact?)
    }

    /// Logs a field name in a record, an update or a pattern. `ty` is
    /// `None` when `owner` has no such field.
    pub(crate) fn note_field_label(
        &mut self,
        (name_span, name): (Span, &str),
        site: FieldSite,
        owner: DefId,
        ctor: Option<DefId>,
        ty: Option<&InferTy>,
    ) {
        if let Some(log) = &mut self.facts {
            log.fields.push(FieldLog {
                name_span,
                name: self.input.globals.names.intern(name),
                site,
                base: None,
                owner: Some(owner),
                ctor,
                ty: ty.cloned(),
            });
        }
    }

    /// Keeps the type that `set_ty` replaces on `id`.
    pub(crate) fn note_overwritten(&mut self, id: ExprId, old: &InferTy) {
        if let Some(log) = &mut self.facts {
            log.overwritten.push((id, old.clone()));
        }
    }

    /// Logs an E0047 at `span`.
    pub(crate) fn note_mismatch(&mut self, span: Span, expected: &InferTy, found: &InferTy) {
        if let Some(log) = &mut self.facts {
            log.mismatches.push((span, expected.clone(), found.clone()));
        }
    }

    /// Logs an E0063 or an E0065 at `span`.
    pub(crate) fn note_unsatisfied(&mut self, span: Span, err: &Unsatisfied) {
        if let Some(log) = &mut self.facts {
            log.unsatisfied.push((span, err.clone()));
        }
    }

    /// Turns the log into `facts_out`. This runs after every diagnostic
    /// that reads an inference type, and it reports nothing, so it cannot
    /// change one. `exprs` must be the zonked THIR, with deferred
    /// constraints resolved.
    pub(super) fn extract_facts(
        &mut self,
        exprs: &IndexVec<ExprId, Expr>,
        pat_tys: &IndexVec<PatId, Ty>,
        locals: &IndexVec<LocalId, thir::Local>,
    ) -> Result<(), Ice> {
        let Some(log) = self.facts.take() else {
            return Ok(());
        };
        // E0057 has run, so what stayed unknown here is dropped.
        let mut scratch = Vec::new();
        let scratch = &mut scratch;
        let mut expr_facts = Vec::with_capacity(log.exprs.len());
        let mut implied = Implied::default();
        for e in &log.exprs {
            let peeled = peel(exprs, e.id);
            // The first value the slot held, from before any E0047.
            let old = log.overwritten.iter().find(|(id, _)| *id == peeled);
            let own = match old {
                Some((_, ty)) => ty.clone(),
                None => self.ty_of(peeled)?,
            };
            expr_facts.push(ExprFact {
                node: e.node,
                ty: self.infcx.zonk(&own, scratch)?,
                expected: self.zonk_opt(e.expected.as_ref(), scratch)?,
            });
            implied.read(exprs, e, peeled)?;
        }
        let mut pats: Vec<(NodeId, Ty)> = log
            .pats
            .iter()
            .filter_map(|&(node, id)| Some((node, *pat_tys.get(id)?)))
            .collect();
        let mut fields = Vec::with_capacity(log.fields.len());
        for f in &log.fields {
            fields.push(FieldFact {
                name_span: f.name_span,
                name: f.name,
                site: f.site,
                base: self.zonk_opt(f.base.as_ref(), scratch)?,
                owner: f.owner,
                ctor: f.ctor,
                ty: self.zonk_opt(f.ty.as_ref(), scratch)?,
            });
        }
        let mut mismatches = Vec::with_capacity(log.mismatches.len());
        for (span, expected, found) in &log.mismatches {
            mismatches.push(MismatchFact {
                span: *span,
                expected: self.infcx.zonk(expected, scratch)?,
                found: self.infcx.zonk(found, scratch)?,
            });
        }
        let mut unsatisfied: Vec<UnsatisfiedFact> = log
            .unsatisfied
            .iter()
            .map(|(span, err)| UnsatisfiedFact {
                span: *span,
                err: err.clone(),
            })
            .collect();
        // Last among the types: pairing a failed call's arguments with its
        // parameters binds variables that only call facts may see.
        let mut calls = self.call_facts(exprs, &log, scratch)?;
        let cases = self.case_facts_step(exprs, &log);
        let Implied {
            mut ops,
            mut shows,
            mut dyns,
            ..
        } = implied;
        // Stable, so the first entry of a key stays.
        expr_facts.sort_by_key(|e| e.node);
        expr_facts.dedup_by_key(|e| e.node);
        pats.sort_by_key(|p| p.0);
        pats.dedup_by_key(|p| p.0);
        fields.sort_by_key(|f| f.name_span.range.start());
        fields.dedup_by_key(|f| f.name_span.range.start());
        calls.sort_by_key(|c| c.callee);
        calls.dedup_by_key(|c| c.callee);
        ops.sort_by_key(|o| o.node);
        ops.dedup_by_key(|o| o.node);
        shows.sort_by_key(|s| (s.node, s.part));
        shows.dedup_by_key(|s| (s.node, s.part));
        dyns.sort_by_key(|d| d.node);
        dyns.dedup_by_key(|d| d.node);
        mismatches.sort_by_key(|m| m.span.range);
        mismatches.dedup_by_key(|m| m.span.range);
        unsatisfied.sort_by_key(|u| u.span.range);
        unsatisfied.dedup_by_key(|u| u.span.range);
        self.facts_out = Some(BodyFacts {
            item: self.item,
            errored: self.errored || has_error(&self.diags),
            exprs: expr_facts.into(),
            pats: pats.into(),
            locals: locals.clone(),
            fields: fields.into(),
            calls: calls.into(),
            ops: ops.into(),
            shows: shows.into(),
            dyns: dyns.into(),
            cases,
            mismatches: mismatches.into(),
            unsatisfied: unsatisfied.into(),
        });
        Ok(())
    }

    /// The one call of [`exhaust::case_facts`] (F §1.4, step 6). It walks
    /// the log, not the tree from the body's root, so a `case` in an
    /// orphaned argument counts.
    fn case_facts_step(&self, exprs: &IndexVec<ExprId, Expr>, log: &FactLog) -> Box<[CaseFact]> {
        let nodes = log.exprs.iter().map(|e| (e.node, e.id));
        exhaust::case_facts(self.input, self.own, exprs, nodes, &log.failed_pats).into()
    }

    /// The calls of `log`. A call with the wrong number of arguments never
    /// unified them with its parameters, so that happens here, and a
    /// failure is ignored. Only such a call has `args`, and it reported
    /// E0048, so the body is dropped and nothing reads `infcx` after this.
    fn call_facts(
        &mut self,
        exprs: &IndexVec<ExprId, Expr>,
        log: &FactLog,
        scratch: &mut Vec<TyVid>,
    ) -> Result<Vec<CallFact>, Ice> {
        // Every pairing first: a variable can be shared with a call that
        // the log holds earlier, and its fact must see the binding too.
        for c in &log.calls {
            for (param, arg) in c.params.iter().zip(&c.args) {
                if let Some(arg) = arg {
                    let found = self.ty_of(*arg)?;
                    let _ = self.infcx.unify(param, &found)?;
                }
            }
        }
        let mut calls = Vec::with_capacity(log.calls.len());
        for c in &log.calls {
            let ty_args = self.zonk_list(&c.ty_args, scratch)?;
            let resolved = c
                .call
                .and_then(|id| exprs.get(id))
                .and_then(|call| self.finished_callee(call, &ty_args, log));
            calls.push(CallFact {
                callee: c.callee,
                def: c.def,
                params: self.zonk_list(&c.params, scratch)?,
                ret: self.infcx.zonk(&c.ret, scratch)?,
                ty_args,
                resolved,
            });
        }
        Ok(calls)
    }

    /// The callee of `call`, or `None` when a constraint on it failed and
    /// left a placeholder. An interface call starts as [`Callee::Dyn`], so
    /// that one counts only for a `Self` that is a `dyn` type and a
    /// constraint that did not fail.
    fn finished_callee(&self, call: &Expr, ty_args: &[Ty], log: &FactLog) -> Option<Callee> {
        let ExprKind::Call { callee, .. } = &call.kind else {
            return None;
        };
        let finished = match callee {
            Callee::Fn { dicts, .. } => !dicts.iter().any(is_placeholder),
            Callee::Dyn { .. } => {
                let types = &self.input.globals.types;
                ty_args
                    .first()
                    .is_some_and(|&ty| matches!(types.kind(ty), TyKind::Dyn { .. }))
                    && !log.unsatisfied.iter().any(|(span, _)| *span == call.span)
            }
            _ => true,
        };
        finished.then(|| callee.clone())
    }

    fn zonk_opt(&self, ty: Option<&InferTy>, scratch: &mut Vec<TyVid>) -> Result<Option<Ty>, Ice> {
        ty.map(|t| self.infcx.zonk(t, scratch)).transpose()
    }

    fn zonk_list(&self, tys: &[InferTy], scratch: &mut Vec<TyVid>) -> Result<Box<[Ty]>, Ice> {
        tys.iter().map(|t| self.infcx.zonk(t, scratch)).collect()
    }
}

/// What no text names, read from the finished THIR of logged expressions.
#[derive(Default)]
struct Implied {
    ops: Vec<OpFact>,
    shows: Vec<ShowFact>,
    dyns: Vec<DynFact>,
    /// The expressions read so far. A parenthesis logs the expression
    /// inside it again, after it, and only its own node gets these facts.
    seen: FxHashSet<ExprId>,
}

impl Implied {
    /// Reads the logged expression `e`, whose value is `peeled`.
    fn read(
        &mut self,
        exprs: &IndexVec<ExprId, Expr>,
        e: &ExprLog,
        peeled: ExprId,
    ) -> Result<(), Ice> {
        let coerced = [e.out, e.id].into_iter().find(|&id| {
            matches!(
                exprs.get(id).map(|x| &x.kind),
                Some(ExprKind::DynCoerce { .. })
            )
        });
        if let Some(id) = coerced
            && self.seen.insert(id)
            && let Some(ExprKind::DynCoerce { iface, dict, .. }) = exprs.get(id).map(|x| &x.kind)
        {
            self.dyns.push(DynFact {
                node: e.node,
                iface: *iface,
                dict: dict.clone(),
            });
        }
        if !self.seen.insert(peeled) {
            return Ok(());
        }
        match exprs.get(peeled).map(|x| &x.kind) {
            Some(ExprKind::Binary { imp, .. } | ExprKind::Neg { imp, .. }) => {
                self.ops.push(OpFact {
                    node: e.node,
                    callee: match imp {
                        OpImpl::Call(callee) => Some(callee.clone()),
                        OpImpl::Builtin => None,
                    },
                });
            }
            Some(ExprKind::Interpolate { parts }) => {
                for (part, value) in parts.iter().enumerate() {
                    if let InterpPart::Value {
                        show: Some(callee), ..
                    } = value
                    {
                        self.shows.push(ShowFact {
                            node: e.node,
                            part: u32::try_from(part)
                                .map_err(|_| Ice::new("too many parts in a string"))?,
                            callee: callee.clone(),
                        });
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// The value inside any `dyn` conversion.
fn peel(exprs: &IndexVec<ExprId, Expr>, mut id: ExprId) -> ExprId {
    while let Some(ExprKind::DynCoerce { value, .. }) = exprs.get(id).map(|e| &e.kind) {
        id = *value;
    }
    id
}

#[cfg(test)]
mod tests;
