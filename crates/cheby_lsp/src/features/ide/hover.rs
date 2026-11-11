//! Hover (X §2.1, rewritten against F §1.8 and F §5, R-11): what the name,
//! operator, hole, literal or `todo` under the cursor is, and its type.
//!
//! A name's meaning comes from the occurrence index (F §5). Types come
//! from the body's facts, keyed by node (D-1380): at a use, the call's
//! instantiated signature or the expression's own type; at a binding, the
//! local's type. A name the index cannot place falls back to the local in
//! scope with that text, which no shadowing makes unique (D-051, D-1383).
//!
//! Items read through [`Printer`], so imported names are written as the
//! module writes them (D-1384). Docs come from the declaring module's tree,
//! or for std in a package run from [`StdIndex`] (D-1385). Satisfaction
//! joins with W12.
//!
//! A constant's value follows X §2.1's constant value rule as plan §1.8
//! amends it (D-1349, D-1413, D-1490, D-1491): a package constant reads
//! what the last evaluation in the child process said, through
//! [`ConstValues`], and a std constant shows its initializer as written
//! (`StdItem::initializer`), so it never waits for an evaluation. A
//! dependency's constant shows its initializer too, from
//! `Snapshot::const_initializer`, since the child evaluates the package's
//! own constants only. The child cuts a value at 4,096 characters, and
//! hover shows what it gets. Hover cuts a written initializer at the same
//! length, with the same `…` (D-1496).

use std::path::Path;

use cheby_driver::analysis::{
    BodyFacts, CtorFields, DefId, DefIndex, DefKind, FileId, FileLocation, Generics, ImportNames,
    ItemSig, LocalKind, ModuleId, ModuleView, Name, ParamIdx, Snapshot, StdIndex, Targets,
    TextRange, TextSize, Ty, builtin, cut_text, field_docs, item_docs, normalized_key,
};
use cheby_syntax::ast::{AstNode, CallExpr, SourceFile, module_doc_text};
use cheby_syntax::{SyntaxKind as K, SyntaxNode, SyntaxToken};

use crate::child::MAX_TEXT;
use crate::features::Analyzed;
use crate::features::ide::cursor::{TokenAt, pick_ident, token_at};
use crate::features::ide::print::Printer;
use crate::features::ide::scope;
use crate::features::index::{
    Classified, ImplicitKind, Meaning, ModuleIndex, Occurrence, Role, Unnamed,
};
use crate::features::stdlib::StdService;
use crate::server::{ConstReading, ConstValues};
use crate::text::lines::Bom;

/// A document of a snapshot, as a request names it by path.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Source<'s> {
    /// The text the snapshot analyzed.
    pub text: &'s str,
    /// How the client counts a BOM in it (D-1368): the editor's text when
    /// the snapshot holds an open document, else disk text.
    pub bom: Bom,
    file: FileId,
}

/// The document at the absolute, normalized `path` in `snapshot`: a file
/// of the package, or a std file of the std base by its display path
/// (D-1370). `None` for a path the snapshot does not hold.
pub(crate) fn source<'s>(snapshot: &'s Snapshot, path: &Path) -> Option<Source<'s>> {
    let file = snapshot.file_of(path).or_else(|| {
        snapshot.sources().files().find_map(|(file, _)| {
            matches!(
                snapshot.location(file),
                Some(FileLocation::Std(display)) if path.ends_with(display)
            )
            .then_some(file)
        })
    })?;
    let bom = if snapshot.version_of(path).is_some() {
        Bom::Counted
    } else {
        Bom::Hidden
    };
    Some(Source {
        text: snapshot.source(file)?.text(),
        bom,
        file,
    })
}

/// What a hover shows, before it becomes Markdown or plain text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HoverText {
    /// The token the hover is about.
    pub range: TextRange,
    /// Code blocks, in order: a declaration, then an instantiated form.
    pub code: Vec<String>,
    /// Short lines about it: where it is from, a mismatch, a conversion.
    pub notes: Vec<String>,
    /// Its doc comment, as Markdown, with bare fences marked as Cheby.
    pub docs: Option<String>,
}

impl HoverText {
    /// A hover of `range` with one code block.
    fn code(range: TextRange, code: String) -> Self {
        Self {
            range,
            code: vec![code],
            ..Self::default()
        }
    }

    /// The hover as Markdown: fenced code blocks, then the notes, then the
    /// docs, the three parts separated by `---` (X §2.1). With `markdown`
    /// false, the same text without fences.
    pub(crate) fn render(&self, markdown: bool) -> String {
        let code: Vec<String> = self
            .code
            .iter()
            .map(|block| {
                if markdown {
                    format!("```cheby\n{block}\n```")
                } else {
                    block.clone()
                }
            })
            .collect();
        let parts: Vec<String> = [
            (!code.is_empty()).then(|| code.join("\n")),
            (!self.notes.is_empty()).then(|| self.notes.join("\n\n")),
            self.docs.clone(),
        ]
        .into_iter()
        .flatten()
        .collect();
        parts.join("\n\n---\n\n")
    }
}

/// Where hover reads the docs of std items whose trees the snapshot does
/// not keep (D-1385). The std base is asked only when such docs are
/// needed, so a hover on a local never waits for it to build.
#[derive(Clone, Copy, Debug)]
pub(crate) enum StdDocs<'a> {
    /// No std docs.
    None,
    /// This index.
    Index(&'a StdIndex),
    /// The std base's index, waited for on first need.
    Service(&'a StdService),
}

impl<'a> StdDocs<'a> {
    /// The index, if there is one.
    fn get(self) -> Option<&'a StdIndex> {
        match self {
            Self::None => None,
            Self::Index(index) => Some(index),
            Self::Service(service) => service.index(),
        }
    }
}

impl<'a> From<Option<&'a StdIndex>> for StdDocs<'a> {
    fn from(index: Option<&'a StdIndex>) -> Self {
        index.map_or(Self::None, Self::Index)
    }
}

/// The hover at `offset` of `source`, a document of `analyzed`'s snapshot.
/// `std` serves docs of std items whose trees the snapshot does not keep,
/// and std constants' initializers. `constants` holds what the package's
/// last evaluation said, `None` where constants are never evaluated, and
/// a package constant then shows no value line. `None` where hover has
/// nothing to say (X §1.4).
pub(crate) fn hover(
    analyzed: &Analyzed,
    source: &Source<'_>,
    offset: TextSize,
    std: StdDocs<'_>,
    constants: Option<&ConstValues>,
) -> Option<HoverText> {
    let snapshot = &*analyzed.snapshot;
    let view = snapshot.module_at(source.file)?;
    let tree = view.tree()?;
    let cx = Cx {
        snapshot,
        view,
        index: analyzed.index().of_file(source.file)?,
        printer: Printer::new(snapshot, view),
        imports: view
            .resolved()
            .map(|resolved| resolved.scope.import_names())
            .unwrap_or_default(),
        std,
        constants,
    };
    let at = token_at(tree.root(), offset);
    if let Some(token) = pick_ident(at) {
        let found = match token.kind() {
            K::LOWER_IDENT | K::UPPER_IDENT => return cx.name(token),
            K::UNDERSCORE => cx.hole(token),
            K::TODO_KW | K::PANIC_KW => cx.todo(token),
            kind if kind.is_keyword() => None,
            _ => cx.operator(token),
        };
        if found.is_some() {
            return found;
        }
    }
    let literal = |token: &SyntaxToken<'_>| is_literal(token.kind());
    let token = match at {
        TokenAt::None => None,
        TokenAt::One(t) => Some(t).filter(literal),
        TokenAt::Between(l, r) => Some(r).filter(literal).or(Some(l).filter(literal)),
    }?;
    match token.kind() {
        K::INTERP_START | K::INTERP_END => cx.interpolation(token),
        _ => cx.literal(token),
    }
}

/// Whether a token of `kind` is part of a literal, or a brace of `{x}`.
fn is_literal(kind: K) -> bool {
    matches!(
        kind,
        K::INT_NUMBER
            | K::FLOAT_NUMBER
            | K::RAW_STRING
            | K::STRING_START
            | K::STRING_TEXT
            | K::STRING_ESCAPE
            | K::STRING_END
            | K::INTERP_START
            | K::INTERP_END
    )
}

/// A type as hover shows it at a use: a type, or a call's instantiated
/// signature (F §1.8).
#[derive(Clone, Copy, Debug)]
enum Shown<'s> {
    Ty(Ty),
    Fn { params: &'s [Ty], ret: Ty },
}

/// What one hover reads.
struct Cx<'s> {
    snapshot: &'s Snapshot,
    view: ModuleView<'s>,
    index: &'s ModuleIndex,
    printer: Printer<'s>,
    imports: ImportNames,
    std: StdDocs<'s>,
    constants: Option<&'s ConstValues>,
}

impl<'s> Cx<'s> {
    /// The text of `name`.
    fn text(&self, name: Name) -> &'static str {
        self.snapshot.globals().names.get(name)
    }

    /// The facts of the body that holds `offset`, with its item.
    fn body_at(&self, offset: TextSize) -> Option<(DefIndex, &'s BodyFacts)> {
        let item = scope::body_at(self.view, offset)?;
        Some((item, self.view.facts()?.body(item)?))
    }

    /// The type parameters of `def`'s signature: a constructor's are its
    /// type's.
    fn generics(&self, def: DefId) -> Option<&'s Generics> {
        let iface = self.snapshot.interfaces().get(def.module)?;
        match &iface.item(def.index)?.sig {
            ItemSig::Fn(f) => Some(&f.generics),
            ItemSig::Type(t) => Some(&t.generics),
            ItemSig::Alias(a) => Some(&a.generics),
            ItemSig::Ctor(ctor) => match &iface.item(ctor.ty)?.sig {
                ItemSig::Type(t) => Some(&t.generics),
                _ => None,
            },
            ItemSig::Const(_)
            | ItemSig::Interface(_)
            | ItemSig::InterfaceFn(_)
            | ItemSig::Test
            | ItemSig::Error => None,
        }
    }

    /// The type parameters in force in the body of this module's `item`.
    fn body_generics(&self, item: DefIndex) -> Option<&'s Generics> {
        self.generics(DefId::new(self.view.id(), item))
    }

    /// `shown` as text, or `None` when it is unknown at its head (F §1.8).
    fn shown(&self, shown: Shown<'_>, generics: Option<&Generics>) -> Option<String> {
        match shown {
            Shown::Ty(ty) if self.printer.is_unknown(ty) => None,
            Shown::Ty(ty) => Some(self.printer.type_display(ty, generics)),
            Shown::Fn { params, ret } => {
                let params: Vec<String> = params
                    .iter()
                    .map(|&p| self.printer.type_display(p, generics))
                    .collect();
                let ret = self.printer.type_display(ret, generics);
                Some(format!("fn({}) -> {ret}", params.join(", ")))
            }
        }
    }

    /// `def` as the module writes it (D-1384): by an imported name, through
    /// the name it binds to the module, else D-463's form. This is
    /// `cheby_iface`'s rule for a type's head, for names hover shows alone.
    fn def_name(&self, def: DefId) -> String {
        if let Some(&bound) = self.imports.items.get(&def) {
            return self.text(bound).to_owned();
        }
        let name = self
            .snapshot
            .interfaces()
            .get(def.module)
            .and_then(|iface| iface.item(def.index))
            .map_or("?", |item| self.text(item.name));
        let table = self.snapshot.table();
        let (module, segment) = if def.module == ModuleId::BUILTIN {
            let homed = (!builtin::is_prelude(def))
                .then(|| builtin::HOMES.iter().find(|(home, _)| *home == def))
                .flatten();
            let Some(&(_, segment)) = homed else {
                return name.to_owned();
            };
            let home = builtin::home_path(segment);
            if self.view.path() == Some(&home) {
                return name.to_owned();
            }
            (table.find(&home), segment)
        } else if def.module == self.view.id() {
            return name.to_owned();
        } else {
            let segment = table
                .get(def.module)
                .and_then(|info| info.path.segments().last())
                .map_or("?", String::as_str);
            (Some(def.module), segment)
        };
        let binding = module
            .and_then(|m| self.imports.modules.get(&m))
            .map_or(segment, |&bound| self.text(bound));
        format!("{binding}::{name}")
    }

    /// An identifier token (F §1.8's rows by parent, through the index).
    fn name(&self, token: SyntaxToken<'s>) -> Option<HoverText> {
        match self.index.classify(token.text_range()) {
            Classified::Occurrence(occurrence) => self.occurrence(token, occurrence),
            Classified::Unknown(_) if may_be_local(token) => self.fallback(token),
            Classified::Unnamed(Unnamed::SelfType) => Self::self_type(token),
            Classified::Unknown(_) | Classified::Unnamed(_) | Classified::Missing => None,
        }
    }

    /// A placed name. A punned field shows its value or binding, the
    /// meaning rename acts on (D-1392), with a note for the field.
    fn occurrence(&self, token: SyntaxToken<'s>, o: &Occurrence) -> Option<HoverText> {
        let punned = matches!(o.role, Role::FieldInit | Role::FieldPat)
            .then_some(o.also)
            .flatten();
        let main = self.index.entry(punned.unwrap_or(o.symbol))?;
        let mut hover = match main.meaning {
            Meaning::Module(module) => self.module(token, module),
            Meaning::Item(def) => self.item(token, def, o.role),
            Meaning::Field { ty, name } => Some(self.field(token, ty, name, o.role)),
            Meaning::TypeParam { item, param } => self.type_param(token, item, param),
            Meaning::Local { item, slot } => self.local(token, item, slot, o.role),
        }?;
        if punned.is_some()
            && let Some(Meaning::Field { ty, name }) = self.index.entry(o.symbol).map(|e| e.meaning)
        {
            hover.notes.push(format!(
                "Also the field `{}` of `{}`.",
                self.text(name),
                self.def_name(ty)
            ));
        }
        Some(hover)
    }

    /// The node a path's last segment names a value or pattern at: the
    /// `PATH_EXPR` or `CTOR_PAT` that holds the `PATH`, with the facts of
    /// its body.
    fn use_site(
        &self,
        token: SyntaxToken<'s>,
    ) -> Option<(SyntaxNode<'s>, DefIndex, &'s BodyFacts)> {
        let path = token.parent().filter(|p| p.kind() == K::PATH)?;
        let last = path
            .children_with_tokens()
            .filter_map(cheby_syntax::SyntaxElement::as_token)
            .filter(|t| matches!(t.kind(), K::LOWER_IDENT | K::UPPER_IDENT))
            .last();
        if last != Some(token) {
            return None;
        }
        let grand = path
            .parent()
            .filter(|g| matches!(g.kind(), K::PATH_EXPR | K::CTOR_PAT))?;
        let (item, facts) = self.body_at(token.text_range().start())?;
        Some((grand, item, facts))
    }

    /// The type at a use of a value (F §1.8): the instantiated signature
    /// of a call, else the expression's type, else a pattern's.
    fn use_type(site: SyntaxNode<'_>, facts: &'s BodyFacts) -> Option<Shown<'s>> {
        if site.kind() == K::CTOR_PAT {
            return facts.pat(site.id()).map(Shown::Ty);
        }
        if let Some(call) = facts.call(site.id()) {
            return Some(Shown::Fn {
                params: &call.params,
                ret: call.ret,
            });
        }
        facts.expr(site.id()).map(|e| Shown::Ty(e.ty))
    }

    /// A local: `let total: Int`, `(parameter) x: T`, `use x: T`,
    /// `fn name: T` (X §2.1).
    fn local(
        &self,
        token: SyntaxToken<'s>,
        item: DefIndex,
        slot: u32,
        role: Role,
    ) -> Option<HoverText> {
        let decl = scope::local_decl(self.view, item, slot)?;
        let generics = self.body_generics(item);
        let site = (role == Role::Use).then(|| self.use_site(token)).flatten();
        let ty = site
            .and_then(|(node, _, facts)| Self::use_type(node, facts))
            .or_else(|| scope::local_fact(self.view, item, slot).map(|local| Shown::Ty(local.ty)));
        let prefix = match decl.kind {
            LocalKind::Param | LocalKind::ClosureParam => "(parameter) ",
            LocalKind::Let | LocalKind::CaseArm => "let ",
            LocalKind::UseBinder => "use ",
            LocalKind::LocalFn => "fn ",
        };
        let name = self.text(decl.name);
        let shown = ty.and_then(|ty| self.shown(ty, generics));
        let code = shown.as_ref().map_or_else(
            || format!("{prefix}{name}"),
            |ty| format!("{prefix}{name}: {ty}"),
        );
        let mut hover = HoverText::code(token.text_range(), code);
        if shown.is_none() {
            hover.notes.push("Its type is unknown.".to_owned());
        }
        if let Some((node, _, facts)) = site {
            self.use_notes(node, facts, generics, &mut hover);
        }
        Some(hover)
    }

    /// The notes of a value at `node`: a mismatch there (E0047) and a
    /// conversion to `dyn`.
    fn use_notes(
        &self,
        node: SyntaxNode<'_>,
        facts: &BodyFacts,
        generics: Option<&Generics>,
        hover: &mut HoverText,
    ) {
        if let Some(mismatch) = facts.mismatch_at(node.text_range()) {
            hover.notes.push(format!(
                "Expected `{}`, found `{}`.",
                self.printer.type_display(mismatch.expected, generics),
                self.printer.type_display(mismatch.found, generics)
            ));
        }
        if let Some(conversion) = facts.dyn_at(node.id()) {
            hover.notes.push(format!(
                "Converted to `dyn {}`.",
                self.def_name(conversion.iface)
            ));
        }
    }

    /// The local in scope at `token` with its text, for a name the index
    /// cannot place that could be a local (F §1.8's last row, D-1383).
    fn fallback(&self, token: SyntaxToken<'s>) -> Option<HoverText> {
        let found = scope::locals_at(self.view, token.text_range().start())
            .into_iter()
            .find(|local| self.text(local.decl.name) == token.text())?;
        self.local(token, found.item, found.slot, Role::Decl)
    }

    /// An item: its header (F §6.1), an instantiated form at a use, where
    /// it is from, and its docs.
    fn item(&self, token: SyntaxToken<'s>, def: DefId, role: Role) -> Option<HoverText> {
        let iface = self.snapshot.interfaces().get(def.module)?;
        let item = iface.item(def.index)?;
        let mut hover = HoverText::code(token.text_range(), self.printer.item_header(def)?);
        if role == Role::Use
            && let Some((site, body_item, facts)) = self.use_site(token)
        {
            let generics = self.body_generics(body_item);
            if let Some(shown) = self.instantiated(def, item.kind, site, facts)
                && let Some(text) = self.shown(shown, generics)
            {
                hover.code.push(text);
            }
            self.use_notes(site, facts, generics, &mut hover);
        }
        if item.kind == DefKind::Const {
            self.constant_value(def, &mut hover);
        }
        hover.notes.extend(self.origin(def));
        if item.targets != Targets::ALL {
            let on: Vec<String> = item.targets.iter().map(|t| format!("`{t}`")).collect();
            hover.notes.push(if on.is_empty() {
                "On no target.".to_owned()
            } else {
                format!("Only on {}.", on.join(" and "))
            });
        }
        if let Some(message) = &item.deprecated {
            hover.notes.push(if message.is_empty() {
                "Deprecated.".to_owned()
            } else {
                format!("Deprecated: {message}")
            });
        }
        hover.docs = self.item_docs(def);
        Some(hover)
    }

    /// The instantiated form of a generic function or constructor at a use
    /// (F §6.1): the call's signature, or the type of the value or pattern.
    fn instantiated(
        &self,
        def: DefId,
        kind: DefKind,
        site: SyntaxNode<'_>,
        facts: &'s BodyFacts,
    ) -> Option<Shown<'s>> {
        let generic = kind == DefKind::InterfaceFn
            || self.generics(def).is_some_and(|g| !g.params.is_empty());
        if !generic {
            return None;
        }
        match (kind, Self::use_type(site, facts)?) {
            (DefKind::Ctor, Shown::Fn { ret, .. }) => Some(Shown::Ty(ret)),
            (DefKind::Fn | DefKind::InterfaceFn | DefKind::Ctor, shown) => Some(shown),
            _ => None,
        }
    }

    /// The value line of the constant `def` (X §2.1's constant value rule,
    /// plan §1.8, D-1413, D-1490, D-1491, D-1496, D-1497, D-1501, D-1503).
    /// A current value joins the header, `const LIMIT: Int = 100`. A stale
    /// one leaves the header alone and adds "`100` when last saved", also
    /// every value while the server starts no run for errors on disk
    /// (D-1503). A constant whose evaluation failed names the diagnostic's
    /// code. One with no value says that values come on save, or, after a
    /// run that failed or was cut short, that it is not evaluated, or,
    /// after a run skipped because the saved files have errors or one the
    /// server did not start for them, that saving gives the value (D-1497,
    /// D-1501). A std
    /// constant shows its initializer as written, cut as a value is
    /// (D-1496), and never asks the child, and so does a constant of a
    /// dependency, which the child does not evaluate. A package that never
    /// evaluates (`constants` is `None`: Windows, single-file mode,
    /// `constants.evaluate` off) shows no line.
    fn constant_value(&self, def: DefId, hover: &mut HoverText) {
        let Some(view) = self.snapshot.module(def.module) else {
            return;
        };
        if !view.is_local() {
            let initializer = self
                .snapshot
                .key_of(def)
                .and_then(|key| self.std.get()?.get(&key)?.initializer.clone())
                .or_else(|| self.snapshot.const_initializer(def));
            if let Some(initializer) = initializer {
                show_initializer(hover, initializer);
            }
            return;
        }
        let Some(values) = self.constants else {
            return;
        };
        let Some(symbol) = self.snapshot.key_of(def) else {
            return;
        };
        let Some(text) = view.file().and_then(|file| self.snapshot.source(file)) else {
            return;
        };
        let line = match values.reading(&symbol, normalized_key(text.text())) {
            ConstReading::Value {
                text,
                truncated,
                stale: false,
            } => {
                if let Some(header) = hover.code.first_mut() {
                    header.push_str(" = ");
                    header.push_str(&text);
                    if truncated {
                        header.push('…');
                    }
                }
                truncated.then(|| "The value is cut short.".to_owned())
            }
            ConstReading::Value {
                text,
                truncated,
                stale: true,
            } => {
                let shown = if truncated {
                    format!("{text}…")
                } else {
                    text
                };
                Some(format!("{} when last saved.", inline_code(&shown)))
            }
            ConstReading::Failed { code } => Some(format!("Evaluation failed, see `{code}`.")),
            ConstReading::NotYet => {
                Some("Not evaluated yet: values are computed on save.".to_owned())
            }
            ConstReading::SkippedForErrors => Some(
                "Not evaluated yet: the saved files have errors; values are computed on save."
                    .to_owned(),
            ),
            ConstReading::AfterFailure => Some(
                "Not evaluated: the last evaluation failed or was cut short (see \
                 `cheby/status`)."
                    .to_owned(),
            ),
        };
        hover.notes.extend(line);
    }

    /// Where `def` is declared: its module, or a builtin's home module.
    /// `None` for an item of a single-file document (D-1477).
    fn origin(&self, def: DefId) -> Option<String> {
        if super::is_synthetic(self.snapshot, def.module) {
            return None;
        }
        let path = if def.module == ModuleId::BUILTIN {
            let &(_, segment) = builtin::HOMES.iter().find(|(home, _)| *home == def)?;
            builtin::home_path(segment)
        } else {
            self.snapshot.table().get(def.module)?.path.clone()
        };
        Some(format!("Defined in `{path}`."))
    }

    /// The doc comment of `def`, from its module's tree, else from the std
    /// index (D-1385).
    fn item_docs(&self, def: DefId) -> Option<String> {
        let local = self.snapshot.module(def.module).and_then(|view| {
            let resolved = view.resolved()?;
            item_docs(view.tree()?, resolved.item(def.index)?)
        });
        let docs = local.or_else(|| {
            let key = self.snapshot.key_of(def)?;
            self.std.get()?.get(&key)?.doc.clone()
        })?;
        Some(cheby_fences(&docs))
    }

    /// A field: `radius: Float`, its type, and its docs.
    fn field(&self, token: SyntaxToken<'s>, ty: DefId, name: Name, role: Role) -> HoverText {
        let at_use = (!role.is_decl())
            .then(|| {
                let (item, facts) = self.body_at(token.text_range().start())?;
                let field = facts.field_at(token.text_range().start())?;
                Some((field.ty?, self.body_generics(item)))
            })
            .flatten();
        let shown = at_use
            .or_else(|| Some((self.declared_field(ty, name)?, self.generics(ty))))
            .and_then(|(field_ty, generics)| self.shown(Shown::Ty(field_ty), generics));
        let text = self.text(name);
        let mut hover = HoverText::code(
            token.text_range(),
            shown.map_or_else(|| text.to_owned(), |t| format!("{text}: {t}")),
        );
        hover
            .notes
            .push(format!("Field of `{}`.", self.def_name(ty)));
        hover.docs = self.field_docs(ty, text).map(|d| cheby_fences(&d));
        hover
    }

    /// The declared type of field `name` of the type `ty`: the first
    /// constructor's that has it (D-071 gives them one type).
    fn declared_field(&self, ty: DefId, name: Name) -> Option<Ty> {
        let iface = self.snapshot.interfaces().get(ty.module)?;
        iface.items.iter().find_map(|item| match &item.sig {
            ItemSig::Ctor(ctor) if item.parent == Some(ty.index) => match &ctor.fields {
                CtorFields::Named(fields) => fields.iter().find(|(n, _)| *n == name).map(|f| f.1),
                CtorFields::Unit | CtorFields::Positional(_) => None,
            },
            _ => None,
        })
    }

    /// The doc comment of field `name` of `ty`.
    fn field_docs(&self, ty: DefId, name: &str) -> Option<String> {
        let local = self
            .snapshot
            .module(ty.module)
            .and_then(|view| field_docs(view.tree()?, view.resolved()?.item(ty.index)?, name));
        local.or_else(|| {
            let key = self.snapshot.key_of(ty)?;
            let item = self.std.get()?.get(&key)?;
            item.fields.iter().find(|f| f.name == name)?.doc.clone()
        })
    }

    /// A type parameter: `T: Show + Compare`, and its owner.
    fn type_param(
        &self,
        token: SyntaxToken<'s>,
        item: DefId,
        param: ParamIdx,
    ) -> Option<HoverText> {
        let generic = self.generics(item)?.params.get(param)?;
        let name = self.text(generic.name);
        let bounds: Vec<String> = generic.bounds.iter().map(|&b| self.def_name(b)).collect();
        let code = if bounds.is_empty() {
            name.to_owned()
        } else {
            format!("{name}: {}", bounds.join(" + "))
        };
        let mut hover = HoverText::code(token.text_range(), code);
        hover
            .notes
            .push(format!("Type parameter of `{}`.", self.def_name(item)));
        Some(hover)
    }

    /// A module: `import std::list`, and the first paragraph of its `//!`
    /// comment (X §2.1).
    fn module(&self, token: SyntaxToken<'s>, module: ModuleId) -> Option<HoverText> {
        let path = &self.snapshot.table().get(module)?.path;
        let mut hover = HoverText::code(token.text_range(), format!("import {path}"));
        let local = self
            .snapshot
            .module(module)
            .and_then(ModuleView::tree)
            .and_then(|tree| SourceFile::cast(tree.root()))
            .map(|file| module_doc_text(file.module_docs()));
        let docs = local
            .filter(|d| !d.trim().is_empty())
            .or_else(|| self.std.get()?.module(&path.to_string())?.doc.clone());
        hover.docs = docs
            .as_deref()
            .and_then(|d| d.split("\n\n").map(str::trim).find(|p| !p.is_empty()))
            .map(cheby_fences);
        Some(hover)
    }

    /// `Self` in an interface.
    fn self_type(token: SyntaxToken<'s>) -> Option<HoverText> {
        let decl = token
            .parent()?
            .ancestors()
            .find(|n| n.kind() == K::INTERFACE_DECL)?;
        let name = decl.first_token_of_kind(K::UPPER_IDENT)?;
        let mut hover = HoverText::code(token.text_range(), "Self".to_owned());
        hover
            .notes
            .push(format!("The type that satisfies `{}`.", name.text()));
        Some(hover)
    }

    /// An operator that calls an interface function (§8.7), or `==` and
    /// `!=`, which are built in (D-047).
    fn operator(&self, token: SyntaxToken<'s>) -> Option<HoverText> {
        let range = token.text_range();
        let implicit = self
            .index
            .implicit_at(range.start())
            .into_iter()
            .find(|o| o.range == range && o.role == Role::Implicit(ImplicitKind::Operator));
        if let Some(occurrence) = implicit {
            let Meaning::Item(def) = self.index.entry(occurrence.symbol)?.meaning else {
                return None;
            };
            let mut hover = self.item(token, def, Role::Implicit(ImplicitKind::Operator))?;
            hover
                .notes
                .insert(0, format!("`{}` calls this function.", token.text()));
            return Some(hover);
        }
        let in_binary = token.parent().is_some_and(|p| p.kind() == K::BIN_EXPR);
        (in_binary && matches!(token.kind(), K::EQ2 | K::NEQ)).then(|| HoverText {
            range,
            notes: vec![format!(
                "`{}` is built in and compares structurally.",
                token.text()
            )],
            ..HoverText::default()
        })
    }

    /// A brace of `{x}`: the `show` it calls.
    fn interpolation(&self, token: SyntaxToken<'s>) -> Option<HoverText> {
        let node = token.parent().filter(|p| p.kind() == K::INTERPOLATION)?;
        let occurrence = self
            .index
            .implicit_at(token.text_range().start())
            .into_iter()
            .find(|o| {
                o.range == node.text_range() && o.role == Role::Implicit(ImplicitKind::Show)
            })?;
        let Meaning::Item(def) = self.index.entry(occurrence.symbol)?.meaning else {
            return None;
        };
        let mut hover = self.item(token, def, Role::Implicit(ImplicitKind::Show))?;
        hover
            .notes
            .insert(0, "This interpolation calls this function.".to_owned());
        Some(hover)
    }

    /// The `_` of a capture or a pipe stage: the type of the value that
    /// fills it, from the call's instantiated signature (F §1.4).
    fn hole(&self, token: SyntaxToken<'s>) -> Option<HoverText> {
        let hole = token.parent().filter(|p| p.kind() == K::HOLE)?;
        let call = hole.ancestors().find_map(CallExpr::cast).filter(|call| {
            call.arg_list()
                .is_some_and(|l| Some(l.syntax()) == hole.parent())
        })?;
        let position = call.args().position(|arg| arg.syntax() == hole)?;
        let callee = call.callee()?.syntax();
        let (item, facts) = self.body_at(token.text_range().start())?;
        let &ty = facts.call(callee.id())?.params.get(position)?;
        let shown = self.shown(Shown::Ty(ty), self.body_generics(item))?;
        let mut hover = HoverText::code(token.text_range(), shown);
        hover
            .notes
            .push("`_` is filled with a value of this type.".to_owned());
        Some(hover)
    }

    /// A literal: its type, which context decides (§8.6.1).
    fn literal(&self, token: SyntaxToken<'s>) -> Option<HoverText> {
        let node = token
            .parent()?
            .ancestors()
            .take_while(|n| matches!(n.kind(), K::LITERAL | K::STRING_LIT | K::LITERAL_PAT))
            .last()?;
        let (item, facts) = self.body_at(token.text_range().start())?;
        let ty = facts
            .expr(node.id())
            .map(|e| e.ty)
            .or_else(|| facts.pat(node.id()))?;
        let shown = self.shown(Shown::Ty(ty), self.body_generics(item))?;
        Some(HoverText::code(token.text_range(), shown))
    }

    /// `todo` and `panic`: the type their context expects (D-1426,
    /// `ExprFact.expected`), else their own.
    fn todo(&self, token: SyntaxToken<'s>) -> Option<HoverText> {
        let node = token
            .parent()
            .filter(|p| matches!(p.kind(), K::TODO_EXPR | K::PANIC_EXPR))?;
        let (item, facts) = self.body_at(token.text_range().start())?;
        let fact = facts.expr(node.id())?;
        let generics = self.body_generics(item);
        let shown = fact
            .expected
            .and_then(|ty| self.shown(Shown::Ty(ty), generics))
            .or_else(|| self.shown(Shown::Ty(fact.ty), generics))?;
        Some(HoverText::code(token.text_range(), shown))
    }
}

/// Whether an unplaced name `token` could be a local, so that the name
/// fallback may answer (F §1.8's last row): a name inside an `ERROR`
/// node, a one-segment path or an interpolation's root. A field, a later
/// path segment or an interpolation's field never names a local.
fn may_be_local(token: SyntaxToken<'_>) -> bool {
    let Some(parent) = token.parent() else {
        return false;
    };
    let idents = || {
        parent
            .children_with_tokens()
            .filter_map(cheby_syntax::SyntaxElement::as_token)
            .filter(|t| matches!(t.kind(), K::LOWER_IDENT | K::UPPER_IDENT))
    };
    match parent.kind() {
        K::ERROR => true,
        K::PATH => idents().count() == 1,
        K::INTERPOLATION => idents().next() == Some(token),
        _ => false,
    }
}

/// Joins a written initializer, std's or a dependency's, to the header of
/// `hover` after ` = `, cut as [`written_initializer`] cuts it (D-1413,
/// D-1491, D-1496).
fn show_initializer(hover: &mut HoverText, initializer: String) {
    if let Some(header) = hover.code.first_mut() {
        header.push_str(" = ");
        header.push_str(&written_initializer(initializer));
    }
}

/// A written initializer, std's or a dependency's, as hover shows it: cut
/// at [`MAX_TEXT`] characters with `…`, the cut and the mark of a value
/// the child computed (D-1413, D-1496).
fn written_initializer(text: String) -> String {
    match cut_text(text, MAX_TEXT) {
        (text, false) => text,
        (text, true) => format!("{text}…"),
    }
}

/// `text` as Markdown inline code: fenced by a run of backticks longer
/// than any in it, with a space inside when it starts or ends with one, so
/// a value that holds backticks shows as it is.
fn inline_code(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.saturating_add(1));
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

/// `docs` with each bare opening fence marked as Cheby, since doc
/// comments are Cheby by default (§13.10).
fn cheby_fences(docs: &str) -> String {
    let mut open = false;
    let lines: Vec<String> = docs
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("```") {
                return line.to_owned();
            }
            let was_open = open;
            open = !open;
            if !was_open && trimmed.trim_end() == "```" {
                let indent = line.len().saturating_sub(trimmed.len());
                format!("{}```cheby", line.get(..indent).unwrap_or(""))
            } else {
                line.to_owned()
            }
        })
        .collect();
    lines.join("\n")
}

#[cfg(test)]
#[path = "hover_tests.rs"]
pub(crate) mod tests;
