//! From `cheby_syntax`'s tree to the evaluator's program ([`super::ir`]).
//!
//! The tree is read through the typed views of `cheby_syntax::ast`, the
//! parser that the compiler uses and that the cross-check pins against
//! `tree-sitter-cheby` (D-272): what a construct means is decided here and
//! in `interp.rs`, from the spec, never from the compiler's later phases.
//! Every name is resolved: a local of an enclosing scope, a function of
//! the module, a function of an imported module, a constructor, or a
//! standard-library function the evaluator knows. Anything else, and every
//! construct the generator never writes, is [`EvalError::Unsupported`].

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use cheby_diag::FileId;
use cheby_syntax::SyntaxKind;
use cheby_syntax::ast::{self, AstNode as _, AstToken as _};

use super::EvalError;
use super::ir::{
    Arm, Block, ClosureDef, CtorDef, CtorShape, Expr, FnDef, Global, Item, ListRest, Loc, Name, Op,
    Part, Pat, PatFields, Program, StdFn, Stmt,
};

/// The standard-library modules the evaluator knows.
const STD_MODULES: [&str; 6] = ["fiber", "int", "io", "list", "map", "string"];

/// The line and the column, both from 1, of byte `offset` in `text`; the
/// column counts code points, as diagnostics do (D-421).
pub(super) fn line_col(text: &str, offset: u32) -> (usize, usize) {
    let offset = usize::try_from(offset).unwrap_or(usize::MAX);
    let mut line = 1usize;
    let mut column = 1usize;
    for (at, c) in text.char_indices() {
        if at >= offset {
            break;
        }
        if c == '\n' {
            line = line.saturating_add(1);
            column = 1;
        } else {
            column = column.saturating_add(1);
        }
    }
    (line, column)
}

/// What a module name that a module imports stands for.
#[derive(Debug, Clone)]
enum Import {
    Std(&'static str),
    /// A module of the package, by its index in the program's files.
    Package(usize),
}

/// One module of the package: its path, its text and its tree.
struct Module {
    path: String,
    /// `None` for the root module (`src/main.cheby`), else the module's
    /// name under the package (`helpers`).
    name: Option<String>,
    text: String,
    parse: cheby_syntax::Parse,
}

/// The package's name, from `cheby.toml`.
fn package_name(program: &super::super::Program) -> Result<String, EvalError> {
    let manifest = program
        .file("cheby.toml")
        .ok_or_else(|| EvalError::Malformed("the package has no cheby.toml".to_owned()))?;
    let table: toml::Table =
        toml::from_str(manifest).map_err(|e| EvalError::Malformed(format!("cheby.toml: {e}")))?;
    match table.get("name") {
        Some(toml::Value::String(name)) => Ok(name.clone()),
        _ => Err(EvalError::Malformed(
            "cheby.toml has no `name` string".to_owned(),
        )),
    }
}

/// The package's modules, the root first, then the others by name.
fn modules(program: &super::super::Program) -> Result<Vec<Module>, EvalError> {
    let mut modules = Vec::new();
    for (path, text) in &program.files {
        let Some(file) = path.strip_prefix("src/") else {
            continue;
        };
        let Some(stem) = file.strip_suffix(".cheby") else {
            continue;
        };
        if stem.contains('/') {
            return Err(EvalError::Unsupported(format!(
                "{path}: a module in a subdirectory"
            )));
        }
        let name = (stem != "main").then(|| stem.to_owned());
        modules.push((name, path.clone(), text.clone()));
    }
    modules.sort_by(|a, b| a.0.cmp(&b.0));
    if modules.first().is_none_or(|(name, _, _)| name.is_some()) {
        return Err(EvalError::Malformed(
            "the package has no src/main.cheby".to_owned(),
        ));
    }
    let mut out = Vec::new();
    for (index, (name, path, text)) in modules.into_iter().enumerate() {
        let id = FileId::new(u32::try_from(index).unwrap_or(u32::MAX));
        let parse = cheby_syntax::parse(id, Arc::from(text.as_str()));
        if let Some(error) = parse.errors().first() {
            return Err(EvalError::Malformed(format!(
                "{path} does not parse: {}",
                error.message
            )));
        }
        out.push(Module {
            path,
            name,
            text,
            parse,
        });
    }
    Ok(out)
}

fn name(text: &str) -> Name {
    Rc::from(text)
}

/// The prelude's constructors (`Option` and `Result`).
fn prelude() -> HashMap<String, Rc<CtorDef>> {
    let mut ctors = HashMap::new();
    for (ctor, shape) in [
        ("Some", CtorShape::Positional(1)),
        ("None", CtorShape::Unit),
        ("Ok", CtorShape::Positional(1)),
        ("Err", CtorShape::Positional(1)),
    ] {
        ctors.insert(
            ctor.to_owned(),
            Rc::new(CtorDef {
                name: name(ctor),
                shape,
            }),
        );
    }
    ctors
}

/// The constructors of one type declaration.
fn type_ctors(
    decl: ast::TypeDecl<'_>,
    ctors: &mut HashMap<String, Rc<CtorDef>>,
) -> Result<(), EvalError> {
    let type_name = decl
        .name()
        .ok_or_else(|| EvalError::Malformed("a type without a name".to_owned()))?;
    let named = |list: ast::RecordFieldList<'_>| -> Result<CtorShape, EvalError> {
        let fields = list
            .fields()
            .map(|field| {
                field.name().map(|n| name(n.text())).ok_or_else(|| {
                    EvalError::Malformed(format!("a field of `{}` has no name", type_name.text()))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CtorShape::Named(fields))
    };
    let mut add = |ctor: &str, shape: CtorShape| -> Result<(), EvalError> {
        let def = Rc::new(CtorDef {
            name: name(ctor),
            shape,
        });
        if ctors.insert(ctor.to_owned(), def).is_some() {
            return Err(EvalError::Unsupported(format!(
                "two constructors named `{ctor}`"
            )));
        }
        Ok(())
    };
    match decl.body() {
        Some(ast::TypeBody::Record(list)) => add(type_name.text(), named(list)?),
        Some(ast::TypeBody::Variants(list)) => {
            for variant in list.variants() {
                let ctor = variant.name().ok_or_else(|| {
                    EvalError::Malformed(format!("a variant of `{}` has no name", type_name.text()))
                })?;
                let shape = match variant.fields() {
                    None => CtorShape::Unit,
                    Some(ast::VariantFields::Positional(list)) => {
                        CtorShape::Positional(list.types().count())
                    }
                    Some(ast::VariantFields::Record(list)) => named(list)?,
                };
                add(ctor.text(), shape)?;
            }
            Ok(())
        }
        None => Err(EvalError::Unsupported(format!(
            "the external type `{}`",
            type_name.text()
        ))),
    }
}

/// Lowers a generated package.
#[expect(
    clippy::too_many_lines,
    reason = "the two passes over the modules read best together"
)]
pub(super) fn lower(program: &super::super::Program) -> Result<Program, EvalError> {
    let package = package_name(program)?;
    let modules = modules(program)?;
    let mut ctors = prelude();
    let mut fn_index: HashMap<(usize, String), usize> = HashMap::new();
    let mut decls = Vec::new();
    let mut imports = Vec::new();
    for (index, module) in modules.iter().enumerate() {
        let file = module.parse.source_file();
        if file.inner_attrs().next().is_some() {
            return Err(EvalError::Unsupported(format!(
                "{}: an inner attribute",
                module.path
            )));
        }
        let mut table = HashMap::new();
        for import in file.imports() {
            if import.list().is_some() || import.rename().is_some() {
                return Err(EvalError::Unsupported(format!(
                    "{}: an import list or a renamed import",
                    module.path
                )));
            }
            let segments: Vec<String> = import
                .module_path()
                .map(|p| p.segments().map(|s| s.text().to_owned()).collect())
                .unwrap_or_default();
            let target = match segments.as_slice() {
                [std, module] if std == "std" => STD_MODULES
                    .iter()
                    .find(|m| **m == module.as_str())
                    .map(|m| (module.clone(), Import::Std(m))),
                [pkg, module] if *pkg == package => modules
                    .iter()
                    .position(|m| m.name.as_deref() == Some(module.as_str()))
                    .map(|i| (module.clone(), Import::Package(i))),
                _ => None,
            };
            let Some((alias, target)) = target else {
                return Err(EvalError::Unsupported(format!(
                    "{}: `import {}`",
                    module.path,
                    segments.join("::")
                )));
            };
            table.insert(alias, target);
        }
        imports.push(table);
        for item in file.items() {
            if item.attrs().next().is_some() {
                return Err(EvalError::Unsupported(format!(
                    "{}: an attribute",
                    module.path
                )));
            }
            match item {
                ast::Item::Type(decl) => type_ctors(decl, &mut ctors)?,
                ast::Item::Fn(decl) => {
                    let fn_name = decl.name().map(|n| n.text().to_owned()).ok_or_else(|| {
                        EvalError::Malformed(format!("{}: a function without a name", module.path))
                    })?;
                    if decl.type_params().is_some() {
                        return Err(EvalError::Unsupported(format!(
                            "{}: the generic function `{fn_name}`",
                            module.path
                        )));
                    }
                    fn_index.insert((index, fn_name), decls.len());
                    decls.push((index, decl));
                }
                _ => {
                    return Err(EvalError::Unsupported(format!(
                        "{}: an item other than a function or a type",
                        module.path
                    )));
                }
            }
        }
    }
    let mut fns = Vec::new();
    for (module_index, decl) in decls {
        let module = modules
            .get(module_index)
            .ok_or_else(|| EvalError::Malformed("a module index out of range".to_owned()))?;
        let empty = HashMap::new();
        let mut lowerer = Lowerer {
            file: module_index,
            path: &module.path,
            text: &module.text,
            module: module_index,
            imports: imports.get(module_index).unwrap_or(&empty),
            fn_index: &fn_index,
            ctors: &ctors,
            scope: Vec::new(),
        };
        let fn_name = decl.name().map(|n| n.text().to_owned()).unwrap_or_default();
        let params = decl
            .params()
            .map(|param| lowerer.param(param))
            .collect::<Result<Vec<_>, _>>()?;
        let body = decl
            .body()
            .ok_or_else(|| lowerer.malformed(decl.syntax(), "a function without a body"))?;
        let body = lowerer.block(body)?;
        let full = match &module.name {
            Some(m) => format!("{package}::{m}::{fn_name}"),
            None => format!("{package}::{fn_name}"),
        };
        fns.push(FnDef {
            name: full,
            params,
            body,
        });
    }
    let main = fn_index
        .get(&(0, "main".to_owned()))
        .copied()
        .ok_or_else(|| EvalError::Malformed("src/main.cheby has no `main`".to_owned()))?;
    Ok(Program {
        files: modules.into_iter().map(|m| (m.path, m.text)).collect(),
        fns,
        main,
    })
}

/// Lowers the bodies of one module.
struct Lowerer<'a> {
    file: usize,
    path: &'a str,
    text: &'a str,
    module: usize,
    imports: &'a HashMap<String, Import>,
    fn_index: &'a HashMap<(usize, String), usize>,
    ctors: &'a HashMap<String, Rc<CtorDef>>,
    /// The locals in scope, innermost last. Names are never shadowed
    /// (§5.2.3), so a name is in scope or not.
    scope: Vec<Name>,
}

impl Lowerer<'_> {
    fn position(&self, node: cheby_syntax::SyntaxNode<'_>) -> String {
        let offset = significant_start(node);
        let (line, column) = line_col(self.text, offset);
        format!("{}:{line}:{column}", self.path)
    }

    fn unsupported(&self, node: cheby_syntax::SyntaxNode<'_>, what: &str) -> EvalError {
        EvalError::Unsupported(format!("{}: {what}", self.position(node)))
    }

    fn malformed(&self, node: cheby_syntax::SyntaxNode<'_>, what: &str) -> EvalError {
        EvalError::Malformed(format!("{}: {what}", self.position(node)))
    }

    fn loc(&self, node: cheby_syntax::SyntaxNode<'_>) -> Loc {
        Loc {
            file: self.file,
            offset: significant_start(node),
        }
    }

    /// Runs `f` with the scope as it is, then forgets what `f` bound.
    fn scoped<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let mark = self.scope.len();
        let out = f(self);
        self.scope.truncate(mark);
        out
    }

    fn param(&mut self, param: ast::Param<'_>) -> Result<Pat, EvalError> {
        let pat = param
            .pattern()
            .ok_or_else(|| self.malformed(param.syntax(), "a parameter without a pattern"))?;
        self.pat(pat)
    }

    fn block(&mut self, block: ast::Block<'_>) -> Result<Block, EvalError> {
        self.scoped(|l| {
            let stmts = block
                .stmts()
                .map(|stmt| l.stmt(stmt))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Block { stmts })
        })
    }

    fn message(&mut self, message: Option<ast::Message<'_>>) -> Result<Option<Expr>, EvalError> {
        match message {
            None => Ok(None),
            Some(m) => {
                let value = m
                    .value()
                    .ok_or_else(|| self.malformed(m.syntax(), "`as` without a message"))?;
                Ok(Some(self.expr(value)?))
            }
        }
    }

    fn stmt(&mut self, stmt: ast::Stmt<'_>) -> Result<Stmt, EvalError> {
        match stmt {
            ast::Stmt::Let(stmt) => {
                let value = stmt
                    .value()
                    .ok_or_else(|| self.malformed(stmt.syntax(), "`let` without a value"))?;
                let value = self.expr(value)?;
                let message = self.message(stmt.message())?;
                let pat_node = stmt
                    .pattern()
                    .ok_or_else(|| self.malformed(stmt.syntax(), "`let` without a pattern"))?;
                let loc = self.loc(pat_node.syntax());
                let pat = self.pat(pat_node)?;
                if stmt.is_assert() {
                    Ok(Stmt::LetAssert {
                        pat,
                        value,
                        message,
                        loc,
                    })
                } else if message.is_some() {
                    Err(self.malformed(stmt.syntax(), "`as` on a plain `let`"))
                } else {
                    Ok(Stmt::Let { pat, value })
                }
            }
            ast::Stmt::Assert(stmt) => {
                let cond = stmt
                    .condition()
                    .ok_or_else(|| self.malformed(stmt.syntax(), "`assert` without a condition"))?;
                let range = cond.significant_span().range;
                let text = range.slice(self.text).unwrap_or_default().to_owned();
                let loc = self.loc(cond.syntax());
                let cond = self.expr(cond)?;
                let message = self.message(stmt.message())?;
                Ok(Stmt::Assert {
                    cond,
                    message,
                    text,
                    loc,
                })
            }
            ast::Stmt::LocalFn(local) => {
                if local.type_params().is_some() {
                    return Err(self.unsupported(local.syntax(), "a generic local function"));
                }
                let fn_name = local.name().map(|n| name(n.text())).ok_or_else(|| {
                    self.malformed(local.syntax(), "a local function without a name")
                })?;
                // The name is in scope in its own body (§5.8) and after it.
                self.scope.push(fn_name.clone());
                let def = self.scoped(|l| {
                    let params = local
                        .params()
                        .map(|p| l.param(p))
                        .collect::<Result<Vec<_>, _>>()?;
                    let body = local.body().ok_or_else(|| {
                        l.malformed(local.syntax(), "a local function without a body")
                    })?;
                    let body = l.block(body)?;
                    Ok::<_, EvalError>(ClosureDef { params, body })
                })?;
                Ok(Stmt::LocalFn {
                    name: fn_name,
                    def: Rc::new(def),
                })
            }
            ast::Stmt::Use(stmt) => Err(self.unsupported(stmt.syntax(), "`use`")),
            ast::Stmt::Expr(expr) => Ok(Stmt::Expr(self.expr(expr)?)),
        }
    }

    fn ctor(
        &self,
        node: cheby_syntax::SyntaxNode<'_>,
        ctor: &str,
    ) -> Result<Rc<CtorDef>, EvalError> {
        self.ctors
            .get(ctor)
            .cloned()
            .ok_or_else(|| self.unsupported(node, &format!("the constructor `{ctor}`")))
    }

    /// The single name of a path such as `None` or `S0A`, refusing a
    /// qualified one.
    fn single_name<'t>(
        &self,
        node: cheby_syntax::SyntaxNode<'t>,
        path: Option<ast::PathNode<'t>>,
    ) -> Result<&'t str, EvalError> {
        let path = path.ok_or_else(|| self.malformed(node, "a missing path"))?;
        let segments: Vec<ast::Name<'t>> = path.segments().collect();
        match segments.as_slice() {
            [one] => Ok(one.text()),
            _ => Err(self.unsupported(node, "a qualified constructor")),
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per kind of pattern, as chapter 6 lists them"
    )]
    fn pat(&mut self, pat: ast::Pat<'_>) -> Result<Pat, EvalError> {
        match pat {
            ast::Pat::Wildcard(_) => Ok(Pat::Wild),
            ast::Pat::Ident(p) => {
                let bound = p
                    .name()
                    .map(|n| name(n.text()))
                    .ok_or_else(|| self.malformed(p.syntax(), "a binding without a name"))?;
                self.scope.push(bound.clone());
                Ok(Pat::Bind(bound))
            }
            ast::Pat::Literal(p) => match p.kind() {
                Some(ast::LiteralPatKind::Int(lit)) => {
                    let value = lit
                        .value()
                        .ok_or_else(|| self.malformed(p.syntax(), "a malformed integer"))?;
                    let value = if p.is_negative() {
                        negative(value)
                    } else {
                        i64::try_from(value).ok()
                    };
                    value
                        .map(Pat::Int)
                        .ok_or_else(|| self.malformed(p.syntax(), "an integer out of range"))
                }
                Some(ast::LiteralPatKind::String(lit)) => lit
                    .plain_value()
                    .map(|s| Pat::Str(name(&s)))
                    .ok_or_else(|| self.malformed(p.syntax(), "an interpolated string pattern")),
                _ => Err(self.unsupported(p.syntax(), "a float or raw string pattern")),
            },
            ast::Pat::Ctor(p) => {
                let ctor_name = self.single_name(p.syntax(), p.path())?;
                if let Some(value) = bool_name(ctor_name) {
                    return match p.fields() {
                        None => Ok(Pat::Bool(value)),
                        Some(_) => Err(self.malformed(p.syntax(), "`True` or `False` with fields")),
                    };
                }
                let ctor = self.ctor(p.syntax(), ctor_name)?;
                let fields = match p.fields() {
                    None => PatFields::None,
                    Some(ast::CtorPatFields::Positional(list)) => PatFields::Positional(
                        list.patterns()
                            .map(|q| self.pat(q))
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                    Some(ast::CtorPatFields::Named(list)) => {
                        let mut fields = Vec::new();
                        for field in list.fields() {
                            let field_name =
                                field.name().map(|n| name(n.text())).ok_or_else(|| {
                                    self.malformed(field.syntax(), "a field without a name")
                                })?;
                            let sub = if let Some(q) = field.pattern() {
                                self.pat(q)?
                            } else {
                                // `C { f }` binds `f` (punning, §6.4).
                                self.scope.push(field_name.clone());
                                Pat::Bind(field_name.clone())
                            };
                            fields.push((field_name, sub));
                        }
                        PatFields::Named(fields)
                    }
                };
                Ok(Pat::Ctor { ctor, fields })
            }
            ast::Pat::Tuple(p) => Ok(Pat::Tuple(
                p.elements()
                    .map(|q| self.pat(q))
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            ast::Pat::List(p) => {
                let mut before = Vec::new();
                let mut after = Vec::new();
                let mut rest = ListRest::No;
                for item in p.items() {
                    match item {
                        ast::ListPatItem::Pat(q) => {
                            let q = self.pat(q)?;
                            if matches!(rest, ListRest::No) {
                                before.push(q);
                            } else {
                                after.push(q);
                            }
                        }
                        ast::ListPatItem::Rest(r) => {
                            if !matches!(rest, ListRest::No) {
                                return Err(
                                    self.malformed(r.syntax(), "two rests in a list pattern")
                                );
                            }
                            rest = match r.name() {
                                Some(n) => {
                                    let bound = name(n.text());
                                    self.scope.push(bound.clone());
                                    ListRest::Bind(bound)
                                }
                                None => ListRest::Anonymous,
                            };
                        }
                    }
                }
                Ok(Pat::List {
                    before,
                    rest,
                    after,
                })
            }
            ast::Pat::Paren(p) => {
                let inner = p
                    .inner()
                    .ok_or_else(|| self.malformed(p.syntax(), "an empty parenthesized pattern"))?;
                self.pat(inner)
            }
            ast::Pat::Or(p) => {
                // Every alternative binds the same names (§6.7): keep the
                // first alternative's in scope.
                let mut alternatives = Vec::new();
                for (i, q) in p.alternatives().enumerate() {
                    let mark = self.scope.len();
                    alternatives.push(self.pat(q)?);
                    if i > 0 {
                        self.scope.truncate(mark);
                    }
                }
                Ok(Pat::Or(alternatives))
            }
            ast::Pat::As(p) => {
                let inner = p
                    .pattern()
                    .ok_or_else(|| self.malformed(p.syntax(), "`as` without a pattern"))?;
                let inner = self.pat(inner)?;
                let bound = p
                    .name()
                    .map(|n| name(n.text()))
                    .ok_or_else(|| self.malformed(p.syntax(), "`as` without a name"))?;
                self.scope.push(bound.clone());
                Ok(Pat::As(Box::new(inner), bound))
            }
        }
    }

    fn boxed(
        &mut self,
        node: cheby_syntax::SyntaxNode<'_>,
        expr: Option<ast::Expr<'_>>,
    ) -> Result<Box<Expr>, EvalError> {
        let expr = expr.ok_or_else(|| self.malformed(node, "a missing expression"))?;
        Ok(Box::new(self.expr(expr)?))
    }

    /// A name used as a value.
    fn path(
        &self,
        node: cheby_syntax::SyntaxNode<'_>,
        path: Option<ast::PathNode<'_>>,
    ) -> Result<Expr, EvalError> {
        let path = path.ok_or_else(|| self.malformed(node, "a missing path"))?;
        let segments: Vec<&str> = path.segments().map(ast::Name::text).collect();
        match segments.as_slice() {
            [one] => {
                if let Some(local) = self.scope.iter().rev().find(|n| &***n == *one) {
                    return Ok(Expr::Local(local.clone()));
                }
                if let Some(value) = bool_name(one) {
                    return Ok(Expr::Global(Global::Bool(value)));
                }
                if let Some(ctor) = self.ctors.get(*one) {
                    return Ok(Expr::Global(Global::Ctor(ctor.clone())));
                }
                if let Some(index) = self.fn_index.get(&(self.module, (*one).to_owned())) {
                    return Ok(Expr::Global(Global::Fn(*index)));
                }
                Err(self.unsupported(node, &format!("the name `{one}`")))
            }
            [module, item] => match self.imports.get(*module) {
                Some(Import::Std(m)) => StdFn::lookup(m, item)
                    .map(|f| Expr::Global(Global::Std(f)))
                    .ok_or_else(|| self.unsupported(node, &format!("`{m}::{item}`"))),
                Some(Import::Package(index)) => self
                    .fn_index
                    .get(&(*index, (*item).to_owned()))
                    .map(|i| Expr::Global(Global::Fn(*i)))
                    .ok_or_else(|| self.unsupported(node, &format!("`{module}::{item}`"))),
                None => Err(self.unsupported(node, &format!("the module `{module}`"))),
            },
            _ => Err(self.unsupported(node, "a path of three or more segments")),
        }
    }

    fn string(&self, lit: ast::StringLit<'_>) -> Result<Expr, EvalError> {
        if !lit.is_terminated() {
            return Err(self.malformed(lit.syntax(), "an unterminated string"));
        }
        if let Some(value) = lit.plain_value() {
            return Ok(Expr::Str(name(&value)));
        }
        let mut parts = Vec::new();
        let mut pending: Vec<(SyntaxKind, &str)> = Vec::new();
        let flush = |pending: &mut Vec<(SyntaxKind, &str)>, parts: &mut Vec<Part>| {
            if !pending.is_empty() {
                let text = cheby_syntax::lexer::literal::string_value(pending.drain(..));
                parts.push(Part::Text(text));
            }
        };
        for part in lit.parts() {
            match part {
                ast::StringPart::Text(t) => pending.push((SyntaxKind::STRING_TEXT, t.text())),
                ast::StringPart::Escape(e) => {
                    if e.value().is_err() {
                        return Err(self.malformed(lit.syntax(), "an invalid escape"));
                    }
                    pending.push((SyntaxKind::STRING_ESCAPE, e.text()));
                }
                ast::StringPart::Interpolation(i) => {
                    flush(&mut pending, &mut parts);
                    let root = i
                        .root()
                        .map(ast::AstToken::text)
                        .ok_or_else(|| self.malformed(i.syntax(), "an empty interpolation"))?;
                    let Some(local) = self.scope.iter().rev().find(|n| &***n == root) else {
                        return Err(self.unsupported(
                            i.syntax(),
                            &format!("an interpolation of `{root}`, which is not a local"),
                        ));
                    };
                    parts.push(Part::Value {
                        root: local.clone(),
                        fields: i.fields().map(|f| name(f.text())).collect(),
                        debug: i.is_debug(),
                    });
                }
            }
        }
        flush(&mut pending, &mut parts);
        Ok(Expr::Interp(parts))
    }

    fn closure(&mut self, closure: ast::Closure<'_>) -> Result<Expr, EvalError> {
        let def = self.scoped(|l| {
            let params = closure
                .params()
                .map(|p| l.param(p))
                .collect::<Result<Vec<_>, _>>()?;
            let body = closure
                .body()
                .ok_or_else(|| l.malformed(closure.syntax(), "a closure without a body"))?;
            let body = l.block(body)?;
            Ok::<_, EvalError>(ClosureDef { params, body })
        })?;
        Ok(Expr::Closure(Rc::new(def)))
    }

    fn case(&mut self, case: ast::CaseExpr<'_>) -> Result<Expr, EvalError> {
        if case.is_subjectless() {
            let mut arms = Vec::new();
            let mut default = None;
            for arm in case.arms() {
                let ast::CaseArmKind::Cond(arm) = arm else {
                    return Err(self
                        .malformed(case.syntax(), "a pattern arm in a `case` without a subject"));
                };
                let body = self.boxed(arm.syntax(), arm.body())?;
                if arm.is_default() {
                    default = Some(body);
                } else {
                    if default.is_some() {
                        return Err(self.malformed(arm.syntax(), "an arm after `_`"));
                    }
                    let cond = self.boxed(arm.syntax(), arm.condition())?;
                    arms.push((*cond, *body));
                }
            }
            return Ok(Expr::Cond { arms, default });
        }
        let subjects = case
            .subjects()
            .map(|s| self.expr(s))
            .collect::<Result<Vec<_>, _>>()?;
        let mut arms = Vec::new();
        for arm in case.arms() {
            let ast::CaseArmKind::Case(arm) = arm else {
                return Err(
                    self.malformed(case.syntax(), "a condition arm in a `case` with subjects")
                );
            };
            let lowered = self.scoped(|l| {
                let pats = arm
                    .patterns()
                    .map(|p| l.pat(p))
                    .collect::<Result<Vec<_>, _>>()?;
                if pats.len() != subjects.len() {
                    return Err(
                        l.malformed(arm.syntax(), "an arm with the wrong number of patterns")
                    );
                }
                let guard = match arm.guard() {
                    None => None,
                    Some(g) => Some(*l.boxed(g.syntax(), g.condition())?),
                };
                let body = *l.boxed(arm.syntax(), arm.body())?;
                Ok(Arm { pats, guard, body })
            })?;
            arms.push(lowered);
        }
        Ok(Expr::Case { subjects, arms })
    }

    fn record(&mut self, record: ast::RecordExpr<'_>) -> Result<Expr, EvalError> {
        let mut fields = Vec::new();
        for field in record.fields() {
            let field_name = field
                .name()
                .map(|n| name(n.text()))
                .ok_or_else(|| self.malformed(field.syntax(), "a field without a name"))?;
            let value = if field.is_punned() {
                self.path_of_local(field.syntax(), &field_name)?
            } else {
                *self.boxed(field.syntax(), field.value())?
            };
            fields.push((field_name, value));
        }
        if let Some(spread) = record.spread() {
            // The base first, then the fields in the order written
            // (§5.11.2): lowering order is irrelevant, evaluation order is
            // the interpreter's.
            let base = self.boxed(spread.syntax(), spread.value())?;
            if fields.is_empty() {
                return Err(self.malformed(record.syntax(), "a record update with no field"));
            }
            return Ok(Expr::Update { base, fields });
        }
        let ctor_name = self.single_name(record.syntax(), record.path())?;
        let ctor = self.ctor(record.syntax(), ctor_name)?;
        Ok(Expr::Construct { ctor, fields })
    }

    fn path_of_local(
        &self,
        node: cheby_syntax::SyntaxNode<'_>,
        local: &str,
    ) -> Result<Expr, EvalError> {
        self.scope
            .iter()
            .rev()
            .find(|n| &***n == local)
            .map(|n| Expr::Local(n.clone()))
            .ok_or_else(|| self.unsupported(node, &format!("the punned field `{local}`")))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per kind of expression, as chapter 5 lists them"
    )]
    fn expr(&mut self, expr: ast::Expr<'_>) -> Result<Expr, EvalError> {
        match expr {
            ast::Expr::Literal(lit) => match lit.kind() {
                Some(ast::LiteralKind::Int(n)) => n
                    .value()
                    .and_then(|v| i64::try_from(v).ok())
                    .map(Expr::Int)
                    .ok_or_else(|| self.malformed(lit.syntax(), "an integer out of range")),
                _ => Err(self.unsupported(lit.syntax(), "a float or raw string literal")),
            },
            ast::Expr::String(lit) => self.string(lit),
            ast::Expr::Path(path) => self.path(path.syntax(), path.path()),
            ast::Expr::Record(record) => self.record(record),
            ast::Expr::Tuple(tuple) => Ok(Expr::Tuple(
                tuple
                    .elements()
                    .map(|e| self.expr(e))
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            ast::Expr::Paren(paren) => Ok(*self.boxed(paren.syntax(), paren.inner())?),
            ast::Expr::List(list) => {
                let mut items = Vec::new();
                for item in list.items() {
                    items.push(match item {
                        ast::ListItem::Expr(e) => Item::One(self.expr(e)?),
                        ast::ListItem::Spread(s) => {
                            Item::Spread(*self.boxed(s.syntax(), s.value())?)
                        }
                    });
                }
                Ok(Expr::List(items))
            }
            ast::Expr::Block(block) => Ok(Expr::Block(self.block(block)?)),
            ast::Expr::Closure(closure) => self.closure(closure),
            ast::Expr::Case(case) => self.case(case),
            ast::Expr::Panic(panic) => Ok(Expr::Panic {
                todo: false,
                message: self.message(panic.message())?.map(Box::new),
                loc: self.loc(panic.syntax()),
            }),
            ast::Expr::Todo(todo) => Ok(Expr::Panic {
                todo: true,
                message: self.message(todo.message())?.map(Box::new),
                loc: self.loc(todo.syntax()),
            }),
            ast::Expr::Binary(bin) => {
                let op = match bin.op() {
                    Some(ast::BinOp::Add) => Op::Add,
                    Some(ast::BinOp::Sub) => Op::Sub,
                    Some(ast::BinOp::Mul) => Op::Mul,
                    Some(ast::BinOp::Div) => Op::Div,
                    Some(ast::BinOp::Rem) => Op::Rem,
                    Some(ast::BinOp::Eq) => Op::Eq,
                    Some(ast::BinOp::Ne) => Op::Ne,
                    Some(ast::BinOp::Lt) => Op::Lt,
                    Some(ast::BinOp::Le) => Op::Le,
                    Some(ast::BinOp::Gt) => Op::Gt,
                    Some(ast::BinOp::Ge) => Op::Ge,
                    Some(ast::BinOp::And) => Op::And,
                    Some(ast::BinOp::Or) => Op::Or,
                    Some(other) => {
                        return Err(self.unsupported(
                            bin.syntax(),
                            &format!("the operator `{}`", other.text()),
                        ));
                    }
                    None => {
                        return Err(
                            self.malformed(bin.syntax(), "a binary expression without an operator")
                        );
                    }
                };
                let lhs = self.boxed(bin.syntax(), bin.lhs())?;
                let rhs = self.boxed(bin.syntax(), bin.rhs())?;
                Ok(Expr::Binary {
                    op,
                    lhs,
                    rhs,
                    loc: self.loc(bin.syntax()),
                })
            }
            ast::Expr::Prefix(prefix) => {
                if let Some(lit) = prefix.negative_literal() {
                    return match lit.kind() {
                        Some(ast::LiteralKind::Int(n)) => {
                            n.value().and_then(negative).map(Expr::Int).ok_or_else(|| {
                                self.malformed(lit.syntax(), "an integer out of range")
                            })
                        }
                        _ => Err(self.unsupported(lit.syntax(), "a negative float")),
                    };
                }
                let operand = self.boxed(prefix.syntax(), prefix.operand())?;
                match prefix.op() {
                    Some(ast::PrefixOp::Not) => Ok(Expr::Not(operand)),
                    Some(ast::PrefixOp::Neg) => Ok(Expr::Neg {
                        operand,
                        loc: self.loc(prefix.syntax()),
                    }),
                    None => {
                        Err(self
                            .malformed(prefix.syntax(), "a prefix expression without an operator"))
                    }
                }
            }
            ast::Expr::Call(call) => {
                let callee = self.boxed(call.syntax(), call.callee())?;
                let mut args = Vec::new();
                for arg in call.args() {
                    match arg {
                        ast::Arg::Expr(e) => args.push(self.expr(e)?),
                        ast::Arg::Hole(h) => {
                            return Err(self.unsupported(h.syntax(), "a function capture"));
                        }
                    }
                }
                Ok(Expr::Call { callee, args })
            }
            ast::Expr::Hole(hole) => Err(self.malformed(hole.syntax(), "`_` as a value")),
            ast::Expr::Field(field) => {
                let receiver = self.boxed(field.syntax(), field.receiver())?;
                let field_name = field
                    .field()
                    .map(|f| name(f.text()))
                    .ok_or_else(|| self.malformed(field.syntax(), "a positional field access"))?;
                Ok(Expr::Field {
                    receiver,
                    field: field_name,
                })
            }
        }
    }
}

/// `-value` for a negative literal, if it fits in `Int` (§5.3.1).
fn negative(value: u128) -> Option<i64> {
    i128::try_from(value)
        .ok()
        .and_then(i128::checked_neg)
        .and_then(|v| i64::try_from(v).ok())
}

fn bool_name(text: &str) -> Option<bool> {
    match text {
        "True" => Some(true),
        "False" => Some(false),
        _ => None,
    }
}

/// The offset of a node's first significant token, where a panic it
/// raises is located (D-570).
fn significant_start(node: cheby_syntax::SyntaxNode<'_>) -> u32 {
    node.first_significant_token()
        .map_or_else(|| node.text_range().start(), |t| t.text_range().start())
}
