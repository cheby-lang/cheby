//! Signatures and bodies (B7): every path, binding and local, the
//! no-shadowing rule (E0044, §5.2.3, D-051, D-062), and the slice's
//! resolution-level gate (functions as values, interface function paths).

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{DefId, DefIndex, DefKind, Ice, Name, Namespace, ParamIdx};
use cheby_syntax::NodeId;
use cheby_syntax::ast::{
    Arg, AstNode, AstToken, Block, CaseArmKind, CtorPatFields, Expr, FieldPatEntry, Ident,
    InterfaceRef, Item, ListItem, ListPatItem, Param, Pat, PathNode, RecordEntry, RetType, Stmt,
    StringPart, Type, TypeBody, TypeParamList, VariantFields,
};
use rustc_hash::FxHashMap;

use crate::gate::not_supported;
use crate::paths::{PathCx, PathShape};
use crate::{
    BodyRes, ItemStatus, LocalDecl, LocalId, LocalKind, Res, ResolveInput, ResolvedModule,
    ScopeSource,
};

/// Whether the slice's resolution-level gate is on. Unit tests turn it
/// off to exercise the full resolver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Slice,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only unit tests resolve without the gate")
    )]
    Full,
}

pub(crate) fn resolve_items(
    input: &ResolveInput<'_>,
    module: &mut ResolvedModule,
    diags: &mut Vec<Diagnostic>,
    mode: Mode,
) -> Result<(), Ice> {
    let indexes: Vec<DefIndex> = module.items.indices().collect();
    for index in indexes {
        let item = module.items.try_get(index, "item")?;
        // A function whose body the syntax gate rejected still has its
        // signature resolved (D-246), but not its body.
        let body_gated = item.status == ItemStatus::BodyUnsupported;
        if item.status == ItemStatus::Unsupported || item.parent.is_some() {
            continue;
        }
        let node = input
            .tree
            .node(item.node)
            .and_then(Item::cast)
            .ok_or_else(|| Ice::new("an item's node is not an item"))?;
        let outcome = resolve_item(input, module, node, mode, !body_gated, diags)?;
        if outcome.sig_unsupported {
            mark_unsupported(module, index);
            continue;
        }
        module.sig_paths.extend(outcome.sig_paths);
        if !outcome.generics.is_empty() {
            module.generics.insert(index, outcome.generics);
        }
        if outcome.body_unsupported {
            crate::gate::mark_body(&mut module.items, index)?;
        } else if let Some(body) = outcome.body {
            module.bodies.insert(index, body);
        }
    }
    Ok(())
}

fn mark_unsupported(module: &mut ResolvedModule, index: DefIndex) {
    for item in &mut module.items {
        if item.parent == Some(index) {
            item.status = ItemStatus::Unsupported;
        }
    }
    if let Some(item) = module.items.get_mut(index) {
        item.status = ItemStatus::Unsupported;
    }
}

struct Outcome {
    sig_paths: FxHashMap<NodeId, Res>,
    generics: Vec<(Name, ParamIdx)>,
    body: Option<BodyRes>,
    /// E0038 in the declaration: the item becomes `Unsupported`.
    sig_unsupported: bool,
    /// E0038 in the body only: the item becomes `BodyUnsupported`.
    body_unsupported: bool,
}

#[expect(clippy::too_many_lines, reason = "one arm per item kind")]
fn resolve_item(
    input: &ResolveInput<'_>,
    module: &ResolvedModule,
    item: Item<'_>,
    mode: Mode,
    with_body: bool,
    diags: &mut Vec<Diagnostic>,
) -> Result<Outcome, Ice> {
    let mut cx = Cx {
        paths: PathCx {
            input,
            items: &module.items,
            scope: &module.scope,
            gated: std::cell::Cell::new(false),
        },
        diags,
        res: BodyRes::default(),
        scopes: Vec::new(),
        in_scope: FxHashMap::default(),
        generics: Vec::new(),
        in_interface: false,
        sig_paths: FxHashMap::default(),
        in_sig: true,
        mode,
        gated: Gated::default(),
    };
    let mut has_body = false;
    match item {
        Item::Fn(f) => {
            cx.generics(f.type_params())?;
            for param in f.params() {
                if let Some(ty) = param.ty() {
                    cx.ty(ty)?;
                }
            }
            cx.ret(f.ret_type())?;
            if let Some(body) = f.body().filter(|_| with_body) {
                has_body = true;
                cx.enter_body();
                cx.push_scope();
                for param in f.params() {
                    let Some(name) = param.name() else { continue };
                    let local = cx.bind(name, LocalKind::Param, param.syntax().id())?;
                    cx.res.params.push(local);
                }
                cx.block_contents(body)?;
                cx.pop_scope();
            }
        }
        Item::Const(c) => {
            if let Some(ty) = c.ty() {
                cx.ty(ty)?;
            }
            if let Some(value) = c.value().filter(|_| with_body) {
                has_body = true;
                cx.enter_body();
                cx.push_scope();
                cx.expr(value, false)?;
                cx.pop_scope();
            }
        }
        Item::Test(t) => {
            if let Some(body) = t.body().filter(|_| with_body) {
                has_body = true;
                cx.enter_body();
                cx.push_scope();
                cx.block_contents(body)?;
                cx.pop_scope();
            }
        }
        Item::Type(t) => {
            cx.generics(t.type_params())?;
            match t.body() {
                Some(TypeBody::Variants(list)) => {
                    for variant in list.variants() {
                        match variant.fields() {
                            Some(VariantFields::Positional(fields)) => {
                                for ty in fields.types() {
                                    cx.ty(ty)?;
                                }
                            }
                            Some(VariantFields::Record(fields)) => {
                                for field in fields.fields() {
                                    if let Some(ty) = field.ty() {
                                        cx.ty(ty)?;
                                    }
                                }
                            }
                            None => {}
                        }
                    }
                }
                Some(TypeBody::Record(fields)) => {
                    for field in fields.fields() {
                        if let Some(ty) = field.ty() {
                            cx.ty(ty)?;
                        }
                    }
                }
                None => {}
            }
        }
        Item::Alias(a) => {
            cx.generics(a.type_params())?;
            if let Some(ty) = a.ty() {
                cx.ty(ty)?;
            }
        }
        Item::Interface(i) => {
            cx.in_interface = true;
            if let Some(bounds) = i.bounds() {
                for iface in bounds.interfaces() {
                    cx.interface_ref(iface)?;
                }
            }
            for f in i.fns() {
                if let Some(list) = f.param_types() {
                    for ty in list.types() {
                        cx.ty(ty)?;
                    }
                }
                cx.ret(f.ret_type())?;
            }
        }
    }
    cx.fold_path_gate();
    let sig_paths = std::mem::take(&mut cx.sig_paths);
    let generics = std::mem::take(&mut cx.generics);
    let body = has_body.then(|| std::mem::take(&mut cx.res));
    Ok(Outcome {
        sig_paths,
        generics,
        body,
        sig_unsupported: cx.gated.sig,
        body_unsupported: cx.gated.body,
    })
}

struct Cx<'a, 'd> {
    paths: PathCx<'a>,
    diags: &'d mut Vec<Diagnostic>,
    res: BodyRes,
    /// Innermost last.
    scopes: Vec<Vec<(Name, LocalId)>>,
    /// The bindings of each name in `scopes`, innermost last, so a lookup
    /// does not scan every scope.
    in_scope: FxHashMap<Name, Vec<LocalId>>,
    generics: Vec<(Name, ParamIdx)>,
    in_interface: bool,
    sig_paths: FxHashMap<NodeId, Res>,
    /// Whether types go to `sig_paths` rather than the body's paths.
    in_sig: bool,
    mode: Mode,
    gated: Gated,
}

/// Where E0038 was reported for the item being resolved.
#[derive(Clone, Copy, Default)]
struct Gated {
    /// In its signature: the item becomes `Unsupported`.
    sig: bool,
    /// In its body: the item becomes `BodyUnsupported`.
    body: bool,
}

impl Gated {
    const fn set(&mut self, in_sig: bool) {
        if in_sig {
            self.sig = true;
        } else {
            self.body = true;
        }
    }
}

impl Cx<'_, '_> {
    fn intern(&self, text: &str) -> Name {
        self.paths.input.globals.names.intern(text)
    }

    fn text(&self, name: Name) -> &'static str {
        self.paths.input.globals.names.get(name)
    }

    fn gate(&mut self, span: Span, what: &str) {
        self.diags.push(not_supported(span, what));
        self.gated.set(self.in_sig);
    }

    /// Moves a pending E0038 from path lookup (a name missing from a partly
    /// shipped std module) to the part being resolved.
    fn fold_path_gate(&mut self) {
        if self.paths.gated.replace(false) {
            self.gated.set(self.in_sig);
        }
    }

    /// Ends the signature: what follows is the item's body.
    fn enter_body(&mut self) {
        self.fold_path_gate();
        self.in_sig = false;
    }

    fn generics(&mut self, list: Option<TypeParamList<'_>>) -> Result<(), Ice> {
        let Some(list) = list else { return Ok(()) };
        let params: Vec<_> = list.params().collect();
        for (n, param) in (0_u32..).zip(&params) {
            let Some(name) = param.name() else { continue };
            let interned = self.intern(name.text());
            if self.generics.iter().any(|(g, _)| *g == interned) {
                self.diags.push(
                    Diagnostic::new(
                        codes::DUPLICATE_NAME,
                        format!("the type parameter `{}` is declared twice", name.text()),
                    )
                    .with_primary(name.span(), ""),
                );
                continue;
            }
            self.generics.push((interned, ParamIdx::from_u32(n)));
        }
        for param in params {
            if let Some(bounds) = param.bounds() {
                for iface in bounds.interfaces() {
                    self.interface_ref(iface)?;
                }
            }
        }
        Ok(())
    }

    fn record_path(&mut self, path: PathNode<'_>, res: Res) {
        let id = path.syntax().id();
        if self.in_sig {
            self.sig_paths.insert(id, res);
        } else {
            self.res.paths.insert(id, res);
        }
    }

    fn ret(&mut self, ret: Option<RetType<'_>>) -> Result<(), Ice> {
        match ret.and_then(RetType::ty) {
            Some(ty) => self.ty(ty),
            None => Ok(()),
        }
    }

    fn ty(&mut self, ty: Type<'_>) -> Result<(), Ice> {
        match ty {
            Type::Path(path_ty) => {
                if let Some(path) = path_ty.path() {
                    let res = self.paths.type_path(
                        path,
                        &self.generics,
                        self.in_interface,
                        self.diags,
                    )?;
                    self.record_path(path, res);
                }
                if let Some(args) = path_ty.type_args() {
                    for arg in args.types() {
                        self.ty(arg)?;
                    }
                }
            }
            Type::Tuple(tuple) => {
                for elem in tuple.elements() {
                    self.ty(elem)?;
                }
            }
            Type::Fn(f) => {
                if let Some(list) = f.param_types() {
                    for param in list.types() {
                        self.ty(param)?;
                    }
                }
                self.ret(f.ret_type())?;
            }
            Type::Dyn(d) => {
                if let Some(iface) = d.interface() {
                    self.interface_ref(iface)?;
                }
            }
        }
        Ok(())
    }

    fn interface_ref(&mut self, iface: InterfaceRef<'_>) -> Result<(), Ice> {
        if let Some(path) = iface.path() {
            let res = self
                .paths
                .type_path(path, &self.generics, self.in_interface, self.diags)?;
            self.record_path(path, res);
        }
        if let Some(args) = iface.type_args() {
            for arg in args.types() {
                self.ty(arg)?;
            }
        }
        Ok(())
    }

    // ---- bindings -----------------------------------------------------

    fn lookup_local(&self, name: Name) -> Option<LocalId> {
        self.in_scope
            .get(&name)
            .and_then(|stack| stack.last())
            .copied()
    }

    fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn pop_scope(&mut self) {
        for (name, _) in self.scopes.pop().unwrap_or_default().into_iter().rev() {
            if let Some(stack) = self.in_scope.get_mut(&name) {
                stack.pop();
                if stack.is_empty() {
                    self.in_scope.remove(&name);
                }
            }
        }
    }

    /// Binds `name` in the innermost scope, reporting E0044 if it is
    /// already in scope.
    fn bind(&mut self, name: Ident<'_>, kind: LocalKind, node: NodeId) -> Result<LocalId, Ice> {
        let interned = self.intern(name.text());
        let span = name.span();
        self.check_shadowing(interned, span)?;
        let local = self.res.locals.push(LocalDecl {
            name: interned,
            span,
            kind,
        })?;
        self.scopes
            .last_mut()
            .ok_or_else(|| Ice::new("binding outside any scope"))?
            .push((interned, local));
        self.in_scope.entry(interned).or_default().push(local);
        self.res.bindings.insert(node, local);
        Ok(local)
    }

    fn check_shadowing(&mut self, name: Name, span: Span) -> Result<(), Ice> {
        let text = self.text(name);
        let earlier = if let Some(local) = self.lookup_local(name) {
            let decl = self.res.locals.try_get(local, "local")?;
            Some((
                decl.span,
                format!("`{text}` is already bound"),
                "first bound here",
            ))
        } else {
            match self.paths.scope.get(Namespace::Value, name) {
                Some(entry) => match (entry.source, entry.res) {
                    (ScopeSource::Import(_), Res::Module(_)) => Some((
                        entry.span.unwrap_or(span),
                        format!("`{text}` is the name of an imported module"),
                        "imported here",
                    )),
                    (ScopeSource::Import(_), _) => Some((
                        entry.span.unwrap_or(span),
                        format!("`{text}` is imported into this module"),
                        "imported here",
                    )),
                    (ScopeSource::Item, _) => {
                        let kind = self
                            .paths
                            .items
                            .iter()
                            .find(|item| {
                                item.name == name
                                    && item.parent.is_none()
                                    && item.kind.namespace() == Some(Namespace::Value)
                            })
                            .map(|item| item.kind);
                        match kind {
                            Some(DefKind::Fn | DefKind::Const) => Some((
                                entry.span.unwrap_or(span),
                                format!(
                                    "`{text}` is the name of a {} in this module",
                                    kind.map_or("function", DefKind::describe)
                                ),
                                "declared here",
                            )),
                            _ => None,
                        }
                    }
                    _ => None,
                },
                None => None,
            }
        };
        if let Some((first, message, label)) = earlier {
            self.diags.push(
                Diagnostic::new(codes::SHADOWED_NAME, message)
                    .with_primary(span, "bound again here")
                    .with_secondary(first, label)
                    .with_help(format!(
                        "give this binding a new name, such as `{text}_2`; names cannot be \
                         shadowed (§5.2.3)"
                    )),
            );
        }
        Ok(())
    }

    // ---- statements and expressions -------------------------------------

    fn block(&mut self, block: Block<'_>) -> Result<(), Ice> {
        self.push_scope();
        let result = self.block_contents(block);
        self.pop_scope();
        result
    }

    /// The statements of a block, in the current scope.
    fn block_contents(&mut self, block: Block<'_>) -> Result<(), Ice> {
        for stmt in block.stmts() {
            self.stmt(stmt)?;
        }
        Ok(())
    }

    fn stmt(&mut self, stmt: Stmt<'_>) -> Result<(), Ice> {
        match stmt {
            Stmt::Let(s) => {
                if let Some(value) = s.value() {
                    self.expr(value, false)?;
                }
                if let Some(ty) = s.ty() {
                    self.ty(ty)?;
                }
                if let Some(message) = s.message().and_then(cheby_syntax::ast::Message::value) {
                    self.expr(message, false)?;
                }
                if let Some(pat) = s.pattern() {
                    self.pat(pat, LocalKind::Let, &mut None)?;
                }
            }
            Stmt::Use(s) => {
                if let Some(value) = s.value() {
                    self.expr(value, false)?;
                }
                for binder in s.binders() {
                    if let Some(ty) = binder.ty() {
                        self.ty(ty)?;
                    }
                    if let Some(pat) = binder.pattern() {
                        self.pat(pat, LocalKind::UseBinder, &mut None)?;
                    }
                }
            }
            Stmt::Assert(s) => {
                if let Some(cond) = s.condition() {
                    self.expr(cond, false)?;
                }
                if let Some(message) = s.message().and_then(cheby_syntax::ast::Message::value) {
                    self.expr(message, false)?;
                }
            }
            Stmt::LocalFn(f) => {
                if let Some(name) = f.name() {
                    self.bind(name, LocalKind::LocalFn, f.syntax().id())?;
                }
                self.push_scope();
                self.params(f.params())?;
                self.ret(f.ret_type())?;
                if let Some(body) = f.body() {
                    self.block_contents(body)?;
                }
                self.pop_scope();
            }
            Stmt::Expr(e) => self.expr(e, false)?,
        }
        Ok(())
    }

    /// Closure and local-function parameters: patterns with optional types.
    fn params<'t>(&mut self, params: impl Iterator<Item = Param<'t>>) -> Result<(), Ice> {
        for param in params {
            if let Some(ty) = param.ty() {
                self.ty(ty)?;
            }
            if let Some(pat) = param.pattern() {
                self.pat(pat, LocalKind::ClosureParam, &mut None)?;
            } else if let Some(name) = param.name() {
                self.bind(name, LocalKind::ClosureParam, param.syntax().id())?;
            }
        }
        Ok(())
    }

    #[expect(clippy::too_many_lines, reason = "one arm per expression kind")]
    fn expr(&mut self, expr: Expr<'_>, callee: bool) -> Result<(), Ice> {
        match expr {
            Expr::Literal(_) | Expr::Hole(_) => {}
            Expr::String(s) => {
                for part in s.parts() {
                    if let StringPart::Interpolation(interp) = part
                        && let Some(root) = interp.root()
                    {
                        let res = self.name_ref(root);
                        self.res.names.insert(interp.syntax().id(), res);
                    }
                }
            }
            Expr::Path(p) => {
                if let Some(args) = p.type_args() {
                    for arg in args.types() {
                        self.ty(arg)?;
                    }
                }
                if let Some(path) = p.path() {
                    let res = self.value_path(path, callee)?;
                    self.record_path(path, res);
                }
            }
            Expr::Record(r) => {
                // A record update names the type, a construction the
                // constructor (§5.11).
                if let Some(path) = r.path() {
                    let res = if r.is_update() {
                        self.paths
                            .type_path(path, &self.generics, self.in_interface, self.diags)?
                    } else {
                        self.ctor_path(path)?
                    };
                    self.record_path(path, res);
                }
                for entry in r.entries() {
                    match entry {
                        RecordEntry::Field(field) => match field.value() {
                            Some(value) => self.expr(value, false)?,
                            None => {
                                if let Some(name) = field.name() {
                                    let res = self.name_ref(name);
                                    self.res.names.insert(field.syntax().id(), res);
                                }
                            }
                        },
                        RecordEntry::Spread(spread) => {
                            if let Some(value) = spread.value() {
                                self.expr(value, false)?;
                            }
                        }
                    }
                }
            }
            Expr::Tuple(t) => {
                for elem in t.elements() {
                    self.expr(elem, false)?;
                }
            }
            Expr::Paren(p) => {
                if let Some(inner) = p.inner() {
                    self.expr(inner, callee)?;
                }
            }
            Expr::List(l) => {
                for item in l.items() {
                    match item {
                        ListItem::Expr(e) => self.expr(e, false)?,
                        ListItem::Spread(s) => {
                            if let Some(value) = s.value() {
                                self.expr(value, false)?;
                            }
                        }
                    }
                }
            }
            Expr::Block(b) => self.block(b)?,
            Expr::Closure(c) => {
                self.push_scope();
                self.params(c.params())?;
                self.ret(c.ret_type())?;
                if let Some(body) = c.body() {
                    self.block_contents(body)?;
                }
                self.pop_scope();
            }
            Expr::Case(c) => {
                for subject in c.subjects() {
                    self.expr(subject, false)?;
                }
                for arm in c.arms() {
                    self.push_scope();
                    match arm {
                        CaseArmKind::Case(arm) => {
                            for pat in arm.patterns() {
                                self.pat(pat, LocalKind::CaseArm, &mut None)?;
                            }
                            if let Some(cond) =
                                arm.guard().and_then(cheby_syntax::ast::Guard::condition)
                            {
                                self.expr(cond, false)?;
                            }
                            if let Some(body) = arm.body() {
                                self.expr(body, false)?;
                            }
                        }
                        CaseArmKind::Cond(arm) => {
                            if let Some(cond) = arm.condition() {
                                self.expr(cond, false)?;
                            }
                            if let Some(body) = arm.body() {
                                self.expr(body, false)?;
                            }
                        }
                    }
                    self.pop_scope();
                }
            }
            Expr::Panic(p) => {
                if let Some(message) = p.message().and_then(cheby_syntax::ast::Message::value) {
                    self.expr(message, false)?;
                }
            }
            Expr::Todo(t) => {
                if let Some(message) = t.message().and_then(cheby_syntax::ast::Message::value) {
                    self.expr(message, false)?;
                }
            }
            Expr::Binary(b) => {
                if let Some(lhs) = b.lhs() {
                    self.expr(lhs, false)?;
                }
                if let Some(rhs) = b.rhs() {
                    self.expr(rhs, false)?;
                }
            }
            Expr::Prefix(p) => {
                if let Some(operand) = p.operand() {
                    self.expr(operand, false)?;
                }
            }
            Expr::Call(c) => {
                if let Some(callee) = c.callee() {
                    self.expr(callee, true)?;
                }
                for arg in c.args() {
                    if let Arg::Expr(e) = arg {
                        self.expr(e, false)?;
                    }
                }
            }
            Expr::Field(f) => {
                if let Some(receiver) = f.receiver() {
                    self.expr(receiver, false)?;
                }
            }
        }
        Ok(())
    }

    /// A bare name used as a value: an interpolation root or a punned
    /// record field.
    fn name_ref(&mut self, name: Ident<'_>) -> Res {
        let interned = self.intern(name.text());
        self.unqualified_value(interned, name.span(), false)
    }

    fn unqualified_value(&mut self, name: Name, span: Span, callee: bool) -> Res {
        if let Some(local) = self.lookup_local(name) {
            return Res::Local(local);
        }
        let Some(entry) = self.paths.scope.get(Namespace::Value, name).copied() else {
            let mut diag = self.paths.unknown(name, span, Namespace::Value, "value");
            if diag.helps.is_empty() {
                let text = self.text(name);
                let mut locals: Vec<&str> = self
                    .scopes
                    .iter()
                    .flatten()
                    .map(|&(n, _)| self.text(n))
                    .collect();
                locals.sort_unstable();
                if let Some(close) = crate::closest_name(text, locals) {
                    diag = diag.with_suggestion(
                        format!("a binding with a similar name exists: `{close}`"),
                        span,
                        close,
                    );
                }
            }
            self.diags.push(diag);
            return Res::Err;
        };
        match entry.res {
            Res::Module(_) => {
                let text = self.text(name);
                self.diags.push(
                    Diagnostic::new(
                        codes::UNRESOLVED_NAME,
                        format!("`{text}` is a module, not a value"),
                    )
                    .with_primary(span, "")
                    .with_help(format!("use one of its items, such as `{text}::name`")),
                );
                Res::Err
            }
            res => self.check_fn_value(res, span, callee),
        }
    }

    /// The slice's gate on functions used as values.
    fn check_fn_value(&mut self, res: Res, span: Span, callee: bool) -> Res {
        if callee || self.mode != Mode::Slice {
            return res;
        }
        if let Res::Def(def) = res
            && self.def_kind(def) == Some(DefKind::Fn)
        {
            self.gate(span, "functions used as values");
            return Res::Err;
        }
        res
    }

    fn def_kind(&self, def: DefId) -> Option<DefKind> {
        if def.module == self.paths.input.module {
            return self.paths.items.get(def.index).map(|item| item.kind);
        }
        self.paths.input.deps.item(def).map(|item| item.kind)
    }

    fn value_path(&mut self, path: PathNode<'_>, callee: bool) -> Result<Res, Ice> {
        match self.paths.shape(path) {
            PathShape::Name(name, span) => Ok(self.unqualified_value(name, span, callee)),
            PathShape::Qualified { module, name } => {
                let Some(target) = self.paths.module_qualifier(module, self.diags) else {
                    return Ok(Res::Err);
                };
                let res =
                    self.paths
                        .in_module(target, Namespace::Value, name, module.1, self.diags)?;
                Ok(self.check_fn_value(res, path.significant_span(), callee))
            }
            PathShape::InterfaceFn {
                module,
                iface,
                name,
            } => {
                if self.mode == Mode::Slice {
                    self.gate(path.significant_span(), "interface function paths");
                    return Ok(Res::Err);
                }
                self.paths.interface_fn(module, iface, name, self.diags)
            }
            PathShape::Invalid => {
                self.diags.push(invalid_path(path.significant_span()));
                Ok(Res::Err)
            }
        }
    }

    /// The path of a constructor pattern or record expression.
    fn ctor_path(&mut self, path: PathNode<'_>) -> Result<Res, Ice> {
        let res = match self.paths.shape(path) {
            PathShape::Name(name, span) => {
                let Some(entry) = self.paths.scope.get(Namespace::Value, name) else {
                    let diag = self
                        .paths
                        .unknown(name, span, Namespace::Value, "constructor");
                    self.diags.push(diag);
                    return Ok(Res::Err);
                };
                entry.res
            }
            PathShape::Qualified { module, name } => {
                let Some(target) = self.paths.module_qualifier(module, self.diags) else {
                    return Ok(Res::Err);
                };
                self.paths
                    .in_module(target, Namespace::Value, name, module.1, self.diags)?
            }
            PathShape::InterfaceFn { .. } | PathShape::Invalid => {
                self.diags.push(invalid_path(path.significant_span()));
                return Ok(Res::Err);
            }
        };
        if let Res::Def(def) = res
            && self.def_kind(def) != Some(DefKind::Ctor)
        {
            let text = path.name().map_or("", |n| n.text());
            self.diags.push(
                Diagnostic::new(
                    codes::UNRESOLVED_NAME,
                    format!("`{text}` is not a constructor"),
                )
                .with_primary(path.significant_span(), ""),
            );
            return Ok(Res::Err);
        }
        Ok(res)
    }

    /// Binds the names of a pattern. In an or-pattern, `alts` maps the
    /// names the first alternative bound, and later alternatives reuse
    /// them.
    fn pat(
        &mut self,
        pat: Pat<'_>,
        kind: LocalKind,
        alts: &mut Option<FxHashMap<Name, LocalId>>,
    ) -> Result<(), Ice> {
        match pat {
            Pat::Wildcard(_) | Pat::Literal(_) => {}
            Pat::Ident(p) => {
                if let Some(name) = p.name() {
                    self.bind_pat(name, kind, p.syntax().id(), alts)?;
                }
            }
            Pat::Ctor(p) => {
                if let Some(path) = p.path() {
                    let res = self.ctor_path(path)?;
                    self.record_path(path, res);
                }
                match p.fields() {
                    Some(CtorPatFields::Positional(list)) => {
                        for sub in list.patterns() {
                            self.pat(sub, kind, alts)?;
                        }
                    }
                    Some(CtorPatFields::Named(list)) => {
                        for entry in list.entries() {
                            if let FieldPatEntry::Field(field) = entry {
                                match field.pattern() {
                                    Some(sub) => self.pat(sub, kind, alts)?,
                                    None => {
                                        if let Some(name) = field.name() {
                                            self.bind_pat(name, kind, field.syntax().id(), alts)?;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    None => {}
                }
            }
            Pat::Tuple(p) => {
                for sub in p.elements() {
                    self.pat(sub, kind, alts)?;
                }
            }
            Pat::List(p) => {
                for item in p.items() {
                    match item {
                        ListPatItem::Pat(sub) => self.pat(sub, kind, alts)?,
                        ListPatItem::Rest(rest) => {
                            if let Some(name) = rest.name() {
                                self.bind_pat(name, kind, rest.syntax().id(), alts)?;
                            }
                        }
                    }
                }
            }
            Pat::Paren(p) => {
                if let Some(inner) = p.inner() {
                    self.pat(inner, kind, alts)?;
                }
            }
            Pat::Or(p) => {
                let outer = alts.take();
                let mut first = true;
                let mut names: Option<FxHashMap<Name, LocalId>> = outer.clone();
                for alt in p.alternatives() {
                    if first {
                        // The first alternative binds; record what it bound.
                        let before = self.res.locals.len();
                        self.pat(alt, kind, &mut names)?;
                        let map = names.get_or_insert_with(FxHashMap::default);
                        for (local, decl) in self.res.locals.iter_enumerated().skip(before) {
                            map.insert(decl.name, local);
                        }
                        first = false;
                    } else {
                        self.pat(alt, kind, &mut names)?;
                    }
                }
                *alts = names.or(outer);
            }
            Pat::As(p) => {
                if let Some(inner) = p.pattern() {
                    self.pat(inner, kind, alts)?;
                }
                if let Some(name) = p.name() {
                    self.bind_pat(name, kind, p.syntax().id(), alts)?;
                }
            }
        }
        Ok(())
    }

    fn bind_pat(
        &mut self,
        name: Ident<'_>,
        kind: LocalKind,
        node: NodeId,
        alts: &mut Option<FxHashMap<Name, LocalId>>,
    ) -> Result<(), Ice> {
        let interned = self.intern(name.text());
        if let Some(local) = alts.as_ref().and_then(|map| map.get(&interned)).copied() {
            self.res.bindings.insert(node, local);
            return Ok(());
        }
        self.bind(name, kind, node)?;
        Ok(())
    }
}

fn invalid_path(span: Span) -> Diagnostic {
    Diagnostic::new(
        codes::UNRESOLVED_NAME,
        "a qualified path names one imported module and one item",
    )
    .with_primary(span, "")
    .with_help("write `module::item` after `import …::module` (§7.5.2)")
}
