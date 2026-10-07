//! Signatures and bodies (B7): every path, binding and local, the
//! no-shadowing rule (E0044, §5.2.3, D-051, D-062), names bound twice in
//! one pattern (E0044, §6.2), alternatives that bind different names
//! (E0062, §6.7), and the opacity of constructors where they are used
//! (E0043, §4.3.4).

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{DefId, DefIndex, DefKind, Ice, Name, Namespace, ParamIdx};
use cheby_syntax::NodeId;
use cheby_syntax::ast::{
    Arg, AstNode, AstToken, Block, CaseArmKind, CtorPatFields, Expr, FieldPatEntry, Ident,
    InterfaceRef, Item, ListItem, ListPatItem, Param, Pat, PathNode, RecordEntry, RetType, Stmt,
    StringPart, Type, TypeBody, TypeParamList, VariantFields,
};
use rustc_hash::FxHashMap;

use crate::paths::{PathCx, PathShape};
use crate::{
    BodyRes, ItemStatus, LocalDecl, LocalId, LocalKind, Res, ResolveInput, ResolvedModule,
    ScopeSource,
};

pub(crate) fn resolve_items(
    input: &ResolveInput<'_>,
    module: &mut ResolvedModule,
    diags: &mut Vec<Diagnostic>,
) -> Result<(), Ice> {
    let indexes: Vec<DefIndex> = module.items.indices().collect();
    for index in indexes {
        let item = module.items.try_get(index, "item")?;
        if item.status == ItemStatus::Unsupported || item.parent.is_some() {
            continue;
        }
        let node = input
            .tree
            .node(item.node)
            .and_then(Item::cast)
            .ok_or_else(|| Ice::new("an item's node is not an item"))?;
        let outcome = resolve_item(input, module, node, diags)?;
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
        gated: Gated::default(),
        later_fns: Vec::new(),
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
            if let Some(body) = f.body() {
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
            if let Some(value) = c.value() {
                has_body = true;
                cx.enter_body();
                cx.push_scope();
                cx.expr(value)?;
                cx.pop_scope();
            }
        }
        Item::Test(t) => {
            if let Some(body) = t.body() {
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
    gated: Gated,
    /// The local functions of the enclosing blocks that are not declared
    /// yet, for the note on a use before the declaration (D-110).
    later_fns: Vec<(Name, Span)>,
}

/// The names bound so far by one pattern, or by the patterns of one `case`
/// arm, closure, local function or `use` (§6.2, §6.7).
#[derive(Default)]
struct PatState {
    /// Every name bound, across alternatives, with its local: a name bound
    /// in several alternatives is one local.
    shared: FxHashMap<Name, LocalId>,
    /// The names bound on the current path through the alternatives, with
    /// where.
    current: Vec<(Name, Span)>,
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
        let mut first: Vec<(Name, Span)> = Vec::new();
        for (n, param) in (0_u32..).zip(&params) {
            let Some(name) = param.name() else { continue };
            let interned = self.intern(name.text());
            if let Some(&(_, earlier)) = first.iter().find(|(g, _)| *g == interned) {
                self.diags.push(
                    Diagnostic::new(
                        codes::DUPLICATE_NAME,
                        format!("the type parameter `{}` is declared twice", name.text()),
                    )
                    .with_primary(name.span(), "declared again here")
                    .with_secondary(earlier, "first declared here"),
                );
                continue;
            }
            first.push((interned, name.span()));
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
        let outer = self.later_fns.len();
        for stmt in block.stmts() {
            if let Stmt::LocalFn(f) = stmt
                && let Some(name) = f.name()
            {
                let interned = self.intern(name.text());
                self.later_fns.push((interned, name.span()));
            }
        }
        let result = block.stmts().try_for_each(|stmt| self.stmt(stmt));
        self.later_fns.truncate(outer);
        result
    }

    fn stmt(&mut self, stmt: Stmt<'_>) -> Result<(), Ice> {
        match stmt {
            Stmt::Let(s) => {
                if let Some(value) = s.value() {
                    self.expr(value)?;
                }
                if let Some(ty) = s.ty() {
                    self.ty(ty)?;
                }
                if let Some(message) = s.message().and_then(cheby_syntax::ast::Message::value) {
                    self.expr(message)?;
                }
                if let Some(pat) = s.pattern() {
                    self.pat(pat, LocalKind::Let, &mut PatState::default())?;
                }
            }
            Stmt::Use(s) => {
                if let Some(value) = s.value() {
                    self.expr(value)?;
                }
                let mut state = PatState::default();
                for binder in s.binders() {
                    if let Some(ty) = binder.ty() {
                        self.ty(ty)?;
                    }
                    if let Some(pat) = binder.pattern() {
                        self.pat(pat, LocalKind::UseBinder, &mut state)?;
                    }
                }
            }
            Stmt::Assert(s) => {
                if let Some(cond) = s.condition() {
                    self.expr(cond)?;
                }
                if let Some(message) = s.message().and_then(cheby_syntax::ast::Message::value) {
                    self.expr(message)?;
                }
            }
            Stmt::LocalFn(f) => {
                if let Some(name) = f.name() {
                    // In scope from here on, and inside its own body
                    // (D-110).
                    let span = name.span();
                    if let Some(pos) = self.later_fns.iter().rposition(|&(_, s)| s == span) {
                        self.later_fns.remove(pos);
                    }
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
            Stmt::Expr(e) => self.expr(e)?,
        }
        Ok(())
    }

    /// Closure and local-function parameters: irrefutable patterns with
    /// optional types (D-242). A name bound twice among them is E0044.
    fn params<'t>(&mut self, params: impl Iterator<Item = Param<'t>>) -> Result<(), Ice> {
        let mut state = PatState::default();
        for param in params {
            if let Some(ty) = param.ty() {
                self.ty(ty)?;
            }
            if let Some(pat) = param.pattern() {
                self.pat(pat, LocalKind::ClosureParam, &mut state)?;
            }
        }
        Ok(())
    }

    #[expect(clippy::too_many_lines, reason = "one arm per expression kind")]
    fn expr(&mut self, expr: Expr<'_>) -> Result<(), Ice> {
        match expr {
            Expr::Literal(_) | Expr::Hole(_) => {}
            Expr::String(s) => {
                for part in s.parts() {
                    if let StringPart::Interpolation(interp) = part
                        && let Some(root) = interp.root()
                    {
                        let res = self.name_ref(root)?;
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
                    let res = self.value_path(path)?;
                    self.record_path(path, res);
                }
            }
            Expr::Record(r) => {
                // A record update names the type, a construction the
                // constructor (§5.11). Either needs the type's structure
                // to be visible here (§4.3.4).
                if let Some(path) = r.path() {
                    let res = if r.is_update() {
                        let res = self.paths.type_path(
                            path,
                            &self.generics,
                            self.in_interface,
                            self.diags,
                        )?;
                        self.structure(res, path.significant_span())?
                    } else {
                        self.ctor_path(path)?
                    };
                    self.record_path(path, res);
                }
                for entry in r.entries() {
                    match entry {
                        RecordEntry::Field(field) => match field.value() {
                            Some(value) => self.expr(value)?,
                            None => {
                                if let Some(name) = field.name() {
                                    let res = self.name_ref(name)?;
                                    self.res.names.insert(field.syntax().id(), res);
                                }
                            }
                        },
                        RecordEntry::Spread(spread) => {
                            if let Some(value) = spread.value() {
                                self.expr(value)?;
                            }
                        }
                    }
                }
            }
            Expr::Tuple(t) => {
                for elem in t.elements() {
                    self.expr(elem)?;
                }
            }
            Expr::Paren(p) => {
                if let Some(inner) = p.inner() {
                    self.expr(inner)?;
                }
            }
            Expr::List(l) => {
                for item in l.items() {
                    match item {
                        ListItem::Expr(e) => self.expr(e)?,
                        ListItem::Spread(s) => {
                            if let Some(value) = s.value() {
                                self.expr(value)?;
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
                    self.expr(subject)?;
                }
                for arm in c.arms() {
                    self.push_scope();
                    match arm {
                        CaseArmKind::Case(arm) => {
                            let mut state = PatState::default();
                            for pat in arm.patterns() {
                                self.pat(pat, LocalKind::CaseArm, &mut state)?;
                            }
                            if let Some(cond) =
                                arm.guard().and_then(cheby_syntax::ast::Guard::condition)
                            {
                                self.expr(cond)?;
                            }
                            if let Some(body) = arm.body() {
                                self.expr(body)?;
                            }
                        }
                        CaseArmKind::Cond(arm) => {
                            if let Some(cond) = arm.condition() {
                                self.expr(cond)?;
                            }
                            if let Some(body) = arm.body() {
                                self.expr(body)?;
                            }
                        }
                    }
                    self.pop_scope();
                }
            }
            Expr::Panic(p) => {
                if let Some(message) = p.message().and_then(cheby_syntax::ast::Message::value) {
                    self.expr(message)?;
                }
            }
            Expr::Todo(t) => {
                if let Some(message) = t.message().and_then(cheby_syntax::ast::Message::value) {
                    self.expr(message)?;
                }
            }
            Expr::Binary(b) => {
                if let Some(lhs) = b.lhs() {
                    self.expr(lhs)?;
                }
                if let Some(rhs) = b.rhs() {
                    self.expr(rhs)?;
                }
            }
            Expr::Prefix(p) => {
                if let Some(operand) = p.operand() {
                    self.expr(operand)?;
                }
            }
            Expr::Call(c) => {
                if let Some(callee) = c.callee() {
                    self.expr(callee)?;
                }
                for arg in c.args() {
                    if let Arg::Expr(e) = arg {
                        self.expr(e)?;
                    }
                }
            }
            Expr::Field(f) => {
                if let Some(receiver) = f.receiver() {
                    self.expr(receiver)?;
                }
            }
        }
        Ok(())
    }

    /// A bare name used as a value: an interpolation root or a punned
    /// record field.
    fn name_ref(&mut self, name: Ident<'_>) -> Result<Res, Ice> {
        let interned = self.intern(name.text());
        self.unqualified_value(interned, name.span())
    }

    fn unqualified_value(&mut self, name: Name, span: Span) -> Result<Res, Ice> {
        if let Some(local) = self.lookup_local(name) {
            return Ok(Res::Local(local));
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
            if let Some(&(_, declared)) = self.later_fns.iter().rev().find(|(n, _)| *n == name) {
                diag = diag.with_secondary(declared, "declared later").with_note(
                    "a local function is in scope only after its declaration and inside its \
                         own body, so it can call itself and the local functions before it (D-110)",
                );
            }
            self.diags.push(diag);
            return Ok(Res::Err);
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
                Ok(Res::Err)
            }
            res => self.structure(res, span),
        }
    }

    /// `res`, or [`Res::Err`] if it is a constructor or type whose
    /// structure is not visible here (E0043, §4.3.4, D-048, D-151).
    fn structure(&mut self, res: Res, span: Span) -> Result<Res, Ice> {
        if let Res::Def(def) = res
            && !self.paths.check_structure(def, span, self.diags)?
        {
            return Ok(Res::Err);
        }
        Ok(res)
    }

    fn def_kind(&self, def: DefId) -> Option<DefKind> {
        if def.module == self.paths.input.module {
            return self.paths.items.get(def.index).map(|item| item.kind);
        }
        self.paths.input.deps.item(def).map(|item| item.kind)
    }

    fn value_path(&mut self, path: PathNode<'_>) -> Result<Res, Ice> {
        match self.paths.shape(path) {
            PathShape::Name(name, span) => self.unqualified_value(name, span),
            PathShape::Qualified { module, name } => {
                let Some(target) = self.paths.module_qualifier(module, self.diags) else {
                    return Ok(Res::Err);
                };
                let res =
                    self.paths
                        .in_module(target, Namespace::Value, name, module.1, self.diags)?;
                self.structure(res, name.1)
            }
            PathShape::InterfaceFn {
                module,
                iface,
                name,
            } => self.paths.interface_fn(module, iface, name, self.diags),
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
        let span = path.name().map_or(path.significant_span(), AstToken::span);
        self.structure(res, span)
    }

    /// Binds the names of a pattern. A name bound in several alternatives
    /// is one local; a name bound twice on one path through the pattern is
    /// E0044 (§6.2); alternatives that bind different names are E0062
    /// (§6.7).
    fn pat(&mut self, pat: Pat<'_>, kind: LocalKind, state: &mut PatState) -> Result<(), Ice> {
        match pat {
            Pat::Wildcard(_) | Pat::Literal(_) => {}
            Pat::Ident(p) => {
                if let Some(name) = p.name() {
                    self.bind_pat(name, kind, p.syntax().id(), state)?;
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
                            self.pat(sub, kind, state)?;
                        }
                    }
                    Some(CtorPatFields::Named(list)) => {
                        for entry in list.entries() {
                            if let FieldPatEntry::Field(field) = entry {
                                match field.pattern() {
                                    Some(sub) => self.pat(sub, kind, state)?,
                                    None => {
                                        if let Some(name) = field.name() {
                                            self.bind_pat(name, kind, field.syntax().id(), state)?;
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
                    self.pat(sub, kind, state)?;
                }
            }
            Pat::List(p) => {
                for item in p.items() {
                    match item {
                        ListPatItem::Pat(sub) => self.pat(sub, kind, state)?,
                        ListPatItem::Rest(rest) => {
                            if let Some(name) = rest.name() {
                                self.bind_pat(name, kind, rest.syntax().id(), state)?;
                            }
                        }
                    }
                }
            }
            Pat::Paren(p) => {
                if let Some(inner) = p.inner() {
                    self.pat(inner, kind, state)?;
                }
            }
            Pat::Or(p) => self.alternatives(p, kind, state)?,
            Pat::As(p) => {
                if let Some(inner) = p.pattern() {
                    self.pat(inner, kind, state)?;
                }
                if let Some(name) = p.name() {
                    self.bind_pat(name, kind, p.syntax().id(), state)?;
                }
            }
        }
        Ok(())
    }

    /// `p1 | p2 | …`: each alternative starts from the names bound before
    /// it, and every alternative must bind the same names (§6.7).
    fn alternatives(
        &mut self,
        p: cheby_syntax::ast::OrPat<'_>,
        kind: LocalKind,
        state: &mut PatState,
    ) -> Result<(), Ice> {
        let before = state.current.len();
        let mut per_alt: Vec<(Span, Vec<(Name, Span)>)> = Vec::new();
        for alt in p.alternatives() {
            state.current.truncate(before);
            self.pat(alt, kind, state)?;
            let bound = state.current.get(before..).unwrap_or_default().to_vec();
            per_alt.push((alt.significant_span(), bound));
        }
        // Every name some alternative binds, at its first binding.
        let mut union: Vec<(Name, Span)> = Vec::new();
        for (_, bound) in &per_alt {
            for &(name, span) in bound {
                if !union.iter().any(|(n, _)| *n == name) {
                    union.push((name, span));
                }
            }
        }
        for (alt_span, bound) in &per_alt {
            let missing: Vec<(Name, Span)> = union
                .iter()
                .copied()
                .filter(|(name, _)| !bound.iter().any(|(n, _)| n == name))
                .collect();
            if !missing.is_empty() {
                self.diags.push(self.missing_bindings(*alt_span, &missing));
            }
        }
        state.current.truncate(before);
        state.current.extend(union);
        Ok(())
    }

    /// E0062 for an alternative that does not bind `missing`.
    fn missing_bindings(&self, alt: Span, missing: &[(Name, Span)]) -> Diagnostic {
        let names: Vec<String> = missing
            .iter()
            .map(|&(name, _)| format!("`{}`", self.text(name)))
            .collect();
        let list = match names.as_slice() {
            [] => String::new(),
            [one] => one.clone(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        };
        let mut diag = Diagnostic::new(
            codes::ALTERNATIVE_BINDINGS,
            format!("this alternative does not bind {list}"),
        )
        .with_primary(alt, format!("binds no {list}"));
        for &(name, span) in missing {
            diag = diag.with_secondary(span, format!("`{}` is bound here", self.text(name)));
        }
        diag.with_note("every alternative of a pattern must bind the same names (§6.7)")
            .with_help(
                "bind the same names in every alternative, or match the value with `_` where it \
                 is not needed",
            )
    }

    fn bind_pat(
        &mut self,
        name: Ident<'_>,
        kind: LocalKind,
        node: NodeId,
        state: &mut PatState,
    ) -> Result<(), Ice> {
        let interned = self.intern(name.text());
        let span = name.span();
        if let Some(&(_, first)) = state.current.iter().find(|(n, _)| *n == interned) {
            let text = name.text();
            self.diags.push(
                Diagnostic::new(
                    codes::SHADOWED_NAME,
                    format!("`{text}` is bound twice in this pattern"),
                )
                .with_primary(span, "bound again here")
                .with_secondary(first, "first bound here")
                .with_help(format!(
                    "give one of them a new name, such as `{text}_2`, or match it with `_`"
                )),
            );
            if let Some(&local) = state.shared.get(&interned) {
                self.res.bindings.insert(node, local);
            }
            return Ok(());
        }
        if let Some(&local) = state.shared.get(&interned) {
            // Bound by an earlier alternative: the same local.
            self.res.bindings.insert(node, local);
        } else {
            let local = self.bind(name, kind, node)?;
            state.shared.insert(interned, local);
        }
        state.current.push((interned, span));
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
