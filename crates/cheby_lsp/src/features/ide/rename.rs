//! Rename over the index, free of protocol types (X §3.2, D-1389 to
//! D-1392, D-1667 to D-1673, D-1698).
//!
//! [`prepare`] says whether the name at an offset can be renamed: it
//! classifies the name and refuses a target that cannot be renamed
//! (X §3.2 steps 1 and 2). [`rename`] plans the edit: it also checks
//! the new name (§3.3) and the state of the package (D-1390), finds the
//! functions a rename couples (D-1391, §3.4), writes every occurrence
//! (§3.5, D-1392, D-1640, D-1645), and has `verify` check the edit,
//! refusing one that adds an error or a W0007, or makes an error go away
//! (D-1389, D-1668). A refusal is a [`Refusal`], which the handler answers
//! as `RequestFailed`.
//!
//! Rename writes the name's tokens and nothing else (D-1670): no line is
//! reformatted, and no comment is edited, doc comments included (D-1672).
//! A doc comment that names the renamed item goes stale.
//!
//! In a std checkout's session (D-1706), rename works over the checkout's
//! std modules as over a package's, with three differences. std's API is
//! refused, since the examples, the tests and `docs/stdlib/` use it and the
//! session sees none of them: a `pub` item, a constructor or field of a
//! `pub` type, an interface function, and any function that makes a type
//! satisfy an interface. A name in a std-only attribute's arguments, such
//! as `@std_hooks(equal, hash, debug)`, is an occurrence and is written
//! with the rest. And a parameter that `@std_clamped` names on a function
//! with no body, where the index records no occurrence, is refused.
//!
//! What the check cannot see (plan §1.6):
//!
//! - A body with an error is dropped from the later checks (D-494), so a
//!   new E0061 or W0010 inside such a body passes.
//! - Constants are not evaluated (D-459, D-1676), so a new E0075 or E0077
//!   passes. A rename can change the text that a constant's `:?` prints
//!   and that another constant compares at compile time, and so give a new
//!   E0075 that only the next constants run reports. This is accepted.
//! - "Used" and "unused" satisfaction is `verify`'s E0063 (plan §1.12): a
//!   function whose satisfaction the package uses is refused, and one whose
//!   satisfaction nothing uses gets a note. So does an interface function
//!   that builtin or std types satisfy by their own functions (D-1710).

use std::path::PathBuf;

use cheby_driver::analysis::codes::{self, Code};
use cheby_driver::analysis::{
    BaselineEntry, DefId, DefKind, FileId, FileLocation, Globals, IfaceItem, Introduced, ItemSig,
    Member, ModuleId, ModuleView, ParamIdx, PlainEdit, Snapshot, Symbol, SymbolNs, SymbolTarget,
    TextRange, TextSize, Ty, TyKind, Verdict, VerifyOutcome, VerifyRequest, Vis, implementors,
    interface_fns, verify,
};
use cheby_syntax::names::{NameCase, check_candidate};
use cheby_syntax::{SyntaxElement, SyntaxKind as K, SyntaxNode, SyntaxToken, SyntaxTree};

use super::Refusal;
use super::cursor::{pick_ident, token_at};
use super::nav::{self, Picked};
use crate::features::index::{
    Classified, Index, Meaning, ModuleIndex, NameKind, Partial, Role, SymbolEntry, Unknown,
    UnknownName, Unnamed,
};
use crate::features::{Analyzed, CancelToken, RequestError};

/// What a rename or a file move edits (G1 plan §1.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RenamePlan {
    /// The edits, with byte ranges in each file's live text, sorted by
    /// path, then by start.
    pub edits: Vec<PlainEdit>,
    /// A note to show beside the edit, such as for a function that makes a
    /// type satisfy an interface (D-1391).
    pub note: Option<String>,
}

/// Why [`rename`] gives no plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RenameError {
    /// The rename cannot be done: the handler answers `RequestFailed`
    /// with the message.
    Refused(Refusal),
    /// A file that the snapshot read from disk changed there (D-1389): the
    /// refusal, and the package's files must be read again
    /// (`Effect::Sweep`, plan §1.7).
    Stale {
        /// What the user is told.
        refusal: Refusal,
        /// The file that changed.
        path: PathBuf,
    },
    /// The client cancelled the request.
    Cancelled,
}

impl From<Refusal> for RenameError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<RequestError> for RenameError {
    fn from(error: RequestError) -> Self {
        match error {
            RequestError::Cancelled => Self::Cancelled,
            RequestError::ContentModified => {
                Self::Refused(Refusal("the document changed, try again".to_owned()))
            }
            RequestError::InvalidParams(message) | RequestError::Failed(message) => {
                Self::Refused(Refusal(message))
            }
        }
    }
}

/// The range and text of the name at `offset` of `file` when it can be
/// renamed, `None` where there is no name to rename: whitespace, a keyword,
/// a literal, a test's name (X §3.1).
///
/// # Errors
///
/// A [`Refusal`] for a name that cannot be renamed (D-1390), and for
/// every name of a document that cannot be edited (D-1673).
pub(crate) fn prepare(
    analyzed: &Analyzed,
    file: FileId,
    offset: TextSize,
) -> Result<Option<(TextRange, String)>, Refusal> {
    if let Some(param) = bodiless_param(&analyzed.snapshot, file, offset)? {
        return Ok(Some((param.range, param.old)));
    }
    Ok(target(analyzed, file, offset)?.map(|target| (target.origin, target.old)))
}

/// The plan that renames the symbol at `offset` of `file` to `new_name`
/// (X §3.2), `None` where there is no name to rename. Renaming a name
/// to itself is an empty plan.
///
/// # Errors
///
/// [`RenameError::Refused`] for a symbol or a name that cannot be renamed,
/// a package that cannot be renamed in (D-1390), or an edit that
/// `verify` refuses (D-1389, D-1668, D-1671, D-1698);
/// [`RenameError::Stale`] when a closed file changed on disk; and
/// [`RenameError::Cancelled`].
pub(crate) fn rename(
    analyzed: &Analyzed,
    file: FileId,
    offset: TextSize,
    new_name: &str,
    cancel: &CancelToken,
) -> Result<Option<RenamePlan>, RenameError> {
    rename_with(analyzed, file, offset, new_name, cancel, None)
}

/// [`rename`] as E0006's fix renames a used name (D-1741): with every
/// check of a rename, but the E0006 of the name's declaration, which the
/// fix exists to remove, may go away; any other error that would go away
/// still refuses it (D-1668).
///
/// # Errors
///
/// As [`rename`].
pub(crate) fn rename_fixing_case(
    analyzed: &Analyzed,
    file: FileId,
    offset: TextSize,
    new_name: &str,
    cancel: &CancelToken,
) -> Result<Option<RenamePlan>, RenameError> {
    rename_with(
        analyzed,
        file,
        offset,
        new_name,
        cancel,
        Some(codes::NAMING_CONVENTION),
    )
}

/// [`rename`], where the errors of the code `fixed` that name the old name
/// may go away (D-1741).
fn rename_with(
    analyzed: &Analyzed,
    file: FileId,
    offset: TextSize,
    new_name: &str,
    cancel: &CancelToken,
    fixed: Option<Code>,
) -> Result<Option<RenamePlan>, RenameError> {
    if let Some(param) = bodiless_param(&analyzed.snapshot, file, offset)? {
        return rename_bodiless(analyzed, file, &param, new_name, cancel);
    }
    let Some(target) = target(analyzed, file, offset)? else {
        return Ok(None);
    };
    check_candidate(new_name, name_case(target.entry.kind)).map_err(Refusal)?;
    if new_name == target.old {
        return Ok(Some(RenamePlan::default()));
    }
    let snapshot = &*analyzed.snapshot;
    let index = analyzed.index();
    gates(index, &target)?;
    cancel.check()?;
    let coupled = coupling(snapshot, target.view, target.entry)?;
    let edits = write(index, &coupled.symbols, new_name, cancel)?;
    cancel.check()?;
    let renamed = [(target.old.as_str(), new_name)];
    let gone = fixed.map(|code| Gone {
        code,
        name: format!("`{}`", target.old),
    });
    check_except(
        analyzed,
        &VerifyRequest {
            edits: &edits,
            moves: &[],
            renamed: &renamed,
            hides: true,
            written: &[],
        },
        Action::Rename,
        gone.as_ref(),
    )?;
    let note = (!coupled.notes.is_empty()).then(|| coupled.notes.join("; "));
    Ok(Some(RenamePlan { edits, note }))
}

/// The errors that a fix that renames exists to remove, so they may go
/// away (D-1741): every one of `code`, in any file, whose message names the
/// old name, `name` with its backticks. For E0006 these are the errors of
/// the declarations the rename writes, one per alternative of an
/// or-pattern and one per function that satisfies a renamed interface
/// function, since only a new name for its declaration removes one.
struct Gone {
    code: Code,
    name: String,
}

/// What an edit that `check` reads does, which its refusal names: a
/// rename's "Renaming would cause", a move's "Moving would cause", or a
/// fix's "Applying the fix would cause" (D-1671, D-1724, D-1731).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// A rename, and a fix that renames.
    Rename,
    /// A move of module files (`willRenameFiles`).
    Move,
    /// A fix that edits another file, checked on `codeAction/resolve`
    /// (D-1684, D-1731).
    Fix,
}

impl Action {
    /// The verb that starts a refusal for new errors.
    const fn verb(self) -> &'static str {
        match self {
            Self::Rename => "Renaming",
            Self::Move => "Moving",
            Self::Fix => "Applying the fix",
        }
    }
}

/// Runs `verify` on `request` and reads its outcome as the refusals of
/// `action` (plan §1.6, D-1671, D-1698, D-1724).
///
/// # Errors
///
/// [`RenameError::Stale`] for [`VerifyOutcome::Stale`], and
/// [`RenameError::Refused`] for every other outcome but
/// [`VerifyOutcome::Clean`], and for a request `verify` cannot run.
pub(crate) fn check(
    analyzed: &Analyzed,
    request: &VerifyRequest<'_>,
    action: Action,
) -> Result<(), RenameError> {
    check_except(analyzed, request, action, None)
}

/// [`check`] for a fix whose own text must check (D-1753): of the errors
/// that `verify` finds new, only those that `counts` accepts refuse it.
///
/// # Errors
///
/// As [`check`].
pub(crate) fn check_counting(
    analyzed: &Analyzed,
    request: &VerifyRequest<'_>,
    action: Action,
    counts: &dyn Fn(&Introduced) -> bool,
) -> Result<(), RenameError> {
    check_with(analyzed, request, action, None, Some(counts))
}

/// [`check`], where the errors that `gone` names may go away (D-1741).
fn check_except(
    analyzed: &Analyzed,
    request: &VerifyRequest<'_>,
    action: Action,
    gone: Option<&Gone>,
) -> Result<(), RenameError> {
    check_with(analyzed, request, action, gone, None)
}

/// [`check_except`], where only the new errors that `counts` accepts, when
/// given, refuse the edit.
fn check_with(
    analyzed: &Analyzed,
    request: &VerifyRequest<'_>,
    action: Action,
    gone: Option<&Gone>,
    counts: Option<&dyn Fn(&Introduced) -> bool>,
) -> Result<(), RenameError> {
    let snapshot = &*analyzed.snapshot;
    let outcome = verify(snapshot, request).map_err(|error| {
        RenameError::Refused(Refusal(format!("the edit could not be checked: {error}")))
    })?;
    match outcome {
        VerifyOutcome::Clean => Ok(()),
        VerifyOutcome::Introduces(mut diags) => {
            if let Some(counts) = counts {
                diags.retain(|diag| counts(diag));
            }
            if diags.is_empty() {
                return Ok(());
            }
            Err(Refusal(would_cause(&diags, action)).into())
        }
        VerifyOutcome::Stale(path) => Err(RenameError::Stale {
            refusal: Refusal(format!(
                "`{}` changed on disk, try again",
                shown_path(snapshot, &path)
            )),
            path,
        }),
        VerifyOutcome::Blocked(reason) => Err(Refusal(reason).into()),
        VerifyOutcome::Hides(mut entries) => {
            if let Some(gone) = gone {
                entries.retain(|(_, (code, _, message))| {
                    !(*code == gone.code && message.contains(&gone.name))
                });
            }
            if entries.is_empty() {
                return Ok(());
            }
            Err(Refusal(would_hide(snapshot, &entries)).into())
        }
    }
}

/// The edits that rename every written occurrence of `symbols` to `new`
/// (X §3.5), with no check: the writer that W11's E0044 fix calls too
/// (D-1668). Each symbol's old name is its text, and an occurrence whose
/// token reads otherwise, a use through an import alias, is left alone.
/// Implicit occurrences are never written, nor are names in attributes
/// outside a std checkout (D-1390), nor files outside the package. A
/// punned field keeps both its meanings: renaming the value of
/// `Book { title }` writes
/// `Book { title: new }`, renaming the field writes `Book { new: title }`,
/// and at the end of a punned path, `origin.y` becomes `origin.y: new` or
/// `origin.new: y` (D-1392, D-1640, D-1645). Sorted by path, then by start,
/// each range once. In a std checkout, a name in a std-only attribute's
/// arguments is an occurrence and is written too (D-1706).
///
/// # Errors
///
/// [`RequestError::Cancelled`] when `cancel` is set.
pub(crate) fn write(
    index: Index<'_>,
    symbols: &[Symbol],
    new: &str,
    cancel: &CancelToken,
) -> Result<Vec<PlainEdit>, RequestError> {
    let snapshot = index.snapshot();
    let attrs = snapshot.is_std_checkout();
    let mut out: Vec<PlainEdit> = Vec::new();
    for symbol in symbols {
        let old = symbol.text();
        let field = matches!(symbol.member, Member::Field(_));
        for hit in index.references(symbol, cancel)?.hits {
            let role = hit.occurrence.role;
            if matches!(role, Role::Implicit(_)) || (role == Role::Attr && !attrs) {
                continue;
            }
            let Some(file) = hit.module.file else {
                continue;
            };
            let (Some(FileLocation::Package(path)), Some(source), Some(tree)) = (
                snapshot.location(file),
                snapshot.source(file),
                snapshot
                    .module(hit.module.module)
                    .and_then(ModuleView::tree),
            ) else {
                continue;
            };
            let range = hit.occurrence.range;
            if range.slice(source.text()) != Some(old) {
                continue;
            }
            let replacement = match (punned(tree, range), field) {
                (false, _) => new.to_owned(),
                (true, true) => format!("{new}: {old}"),
                (true, false) => format!("{old}: {new}"),
            };
            out.push(PlainEdit {
                path: path.clone(),
                range,
                replacement,
            });
        }
    }
    out.sort_by(|a, b| {
        (&a.path, a.range.start(), a.range.end()).cmp(&(&b.path, b.range.start(), b.range.end()))
    });
    out.dedup_by(|a, b| a.path == b.path && a.range == b.range);
    Ok(out)
}

/// What the cursor names, once it may be renamed.
struct Target<'a> {
    /// The token that names it.
    origin: TextRange,
    /// The cursor's module.
    view: ModuleView<'a>,
    /// The cursor's module's index.
    module: &'a ModuleIndex,
    /// The symbol: the value of a punned field (D-1392).
    entry: &'a SymbolEntry,
    /// The token's text: the old name.
    old: String,
}

/// The name at `offset` of `file` that a rename would act on (X §3.2
/// steps 1 and 2): classified as references does (D-1392), and refused
/// when it cannot be renamed.
fn target(
    analyzed: &Analyzed,
    file: FileId,
    offset: TextSize,
) -> Result<Option<Target<'_>>, Refusal> {
    let snapshot = &*analyzed.snapshot;
    let (Some(view), Some(source)) = (snapshot.module_at(file), snapshot.source(file)) else {
        return Ok(None);
    };
    let Some(tree) = view.tree() else {
        return Ok(None);
    };
    let index = analyzed.index();
    // A std source cannot be edited, so every name in it is refused, a
    // local's too (D-1673).
    let read_only = matches!(snapshot.location(file), Some(FileLocation::Std(_)));
    let Some(Picked {
        origin,
        module,
        entries,
        role,
        hidden,
    }) = nav::pick(index, view, offset)
    else {
        return unpicked(index, view, tree, source.text(), offset, read_only);
    };
    let old = origin.slice(source.text()).unwrap_or_default().to_owned();
    if read_only {
        return Err(not_editable(&old, "std"));
    }
    if hidden.is_some() {
        return Err(Refusal(format!("`{old}` does not resolve")));
    }
    let Some(entry) = entries.first().copied() else {
        return Ok(None);
    };
    match role {
        Some(Role::Implicit(_)) => {
            return Err(Refusal(format!(
                "no name is written here for `{}`: rename it where it is declared",
                entry.symbol.text()
            )));
        }
        // Only std's own attributes give names (`index::build`), and outside
        // std they are unknown (E0045) and give none, while a std document
        // is refused above (D-1673). In a std checkout they are occurrences,
        // written with the rest (D-1706).
        Some(Role::Attr) if !snapshot.is_std_checkout() => {
            return Err(Refusal(format!(
                "`{old}` in an attribute is renamed where it is declared"
            )));
        }
        Some(
            Role::Attr
            | Role::Decl
            | Role::Use
            | Role::Qualifier
            | Role::Import
            | Role::FieldRead
            | Role::FieldInit
            | Role::FieldPat
            | Role::InterpRoot
            | Role::InterpField,
        )
        | None => {}
    }
    allowed(snapshot, view, entry, &old)?;
    shared_import(index, entry, &old)?;
    Ok(Some(Target {
        origin,
        view,
        module,
        entry,
        old,
    }))
}

/// Why the identifier at `offset`, which names no symbol, cannot be
/// renamed, or `None` where nothing there is a name (X §3.2 step 2).
fn unpicked<'a>(
    index: Index<'a>,
    view: ModuleView<'a>,
    tree: &SyntaxTree,
    text: &str,
    offset: TextSize,
    read_only: bool,
) -> Result<Option<Target<'a>>, Refusal> {
    let Some(token) = pick_ident(token_at(tree.root(), offset))
        .filter(|t| matches!(t.kind(), K::LOWER_IDENT | K::UPPER_IDENT))
    else {
        return Ok(None);
    };
    let Some(module) = index.module(view.id()) else {
        return Ok(None);
    };
    let range = token.text_range();
    let old = range.slice(text).unwrap_or_default();
    match module.classify(range) {
        Classified::Unnamed(Unnamed::AttrArg)
            if index.snapshot().is_std_checkout() && clamped_label(token) =>
        {
            Err(unrecorded(old))
        }
        Classified::Unknown(_) if read_only => Err(not_editable(old, "std")),
        Classified::Unknown(name) => Err(Refusal(unknown_reason(name, old, None))),
        Classified::Unnamed(Unnamed::SelfType) => {
            Err(Refusal("`Self` is not a name you declare".to_owned()))
        }
        Classified::Unnamed(
            Unnamed::AttrName | Unnamed::AttrArg | Unnamed::Label | Unnamed::Package,
        )
        | Classified::Occurrence(_)
        | Classified::Missing => Ok(None),
    }
}

/// Refuses a symbol that a rename may not touch (D-1390): a module, a
/// builtin, and what another package declares, std included. In a std
/// checkout, also std's API (D-1706, `std_api`). `view` is the cursor's
/// module.
fn allowed(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    entry: &SymbolEntry,
    old: &str,
) -> Result<(), Refusal> {
    // An import's `as` name belongs to the importing module, whatever
    // it names (F §5.3).
    if matches!(entry.symbol.member, Member::ImportAlias(_)) {
        return Ok(());
    }
    let home = match entry.meaning {
        Meaning::Module(_) => {
            return Err(Refusal(
                "a module's name is its file's name: rename the file".to_owned(),
            ));
        }
        Meaning::Item(def) => def.module,
        Meaning::Field { ty, .. } => ty.module,
        Meaning::TypeParam { item, .. } => item.module,
        Meaning::Local { .. } => return Ok(()),
    };
    if home == ModuleId::BUILTIN {
        return Err(Refusal(format!("`{old}` is part of the language")));
    }
    let home_view = snapshot.module(home);
    if home_view.is_some_and(ModuleView::is_own) {
        return if snapshot.is_std_checkout() {
            std_api(snapshot, view, entry, old)
        } else {
            Ok(())
        };
    }
    let path = home_view.and_then(ModuleView::path);
    let package = path
        .and_then(|p| p.segments().first())
        .map_or("another package", String::as_str);
    let shown = match (entry.meaning, path.and_then(|p| p.segments().last())) {
        (Meaning::Item(_), Some(last)) => format!("{last}::{old}"),
        (Meaning::Item(_), None)
        | (
            Meaning::Module(_)
            | Meaning::Field { .. }
            | Meaning::TypeParam { .. }
            | Meaning::Local { .. },
            Some(_) | None,
        ) => old.to_owned(),
    };
    Err(not_editable(&shown, package))
}

/// The refusal for a name of a file that cannot be edited (D-1390).
fn not_editable(shown: &str, package: &str) -> Refusal {
    Refusal(format!(
        "`{shown}` is in `{package}`, which cannot be edited here"
    ))
}

/// Where std's API also lives, which D-1706's refusals say.
const STD_API: &str = "std's API also lives in `docs/stdlib/`, the examples and the tests";

/// Refuses std's API in a std checkout (D-1706): a `pub` item, a
/// constructor or field of a `pub` type, an interface function, and a
/// function that makes a type satisfy an interface, which every package
/// may use (D-447) and the session cannot see. `view` is the cursor's
/// module.
fn std_api(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    entry: &SymbolEntry,
    old: &str,
) -> Result<(), Refusal> {
    let is_pub = |def: DefId| item(snapshot, def).is_some_and(|item| item.vis == Vis::Pub);
    let def = match entry.meaning {
        Meaning::Item(def) => def,
        Meaning::Field { ty, .. } if is_pub(ty) => {
            return Err(Refusal(format!(
                "`{old}` is a field of the `pub` type `{}`: {STD_API}",
                name_of(snapshot, ty)
            )));
        }
        Meaning::Field { .. }
        | Meaning::Module(_)
        | Meaning::TypeParam { .. }
        | Meaning::Local { .. } => return Ok(()),
    };
    let Some(found) = item(snapshot, def) else {
        return Ok(());
    };
    let parent = found.parent.map(|parent| DefId::new(def.module, parent));
    if found.kind == DefKind::InterfaceFn {
        let interface = parent.map_or_default(|p| format!(" `{}`", name_of(snapshot, p)));
        return Err(Refusal(format!(
            "`{old}` is a function of the interface{interface}: {STD_API}"
        )));
    }
    if let Some(ty) = parent.filter(|&ty| found.kind == DefKind::Ctor && is_pub(ty)) {
        return Err(Refusal(format!(
            "`{old}` is a constructor of the `pub` type `{}`: {STD_API}",
            name_of(snapshot, ty)
        )));
    }
    if found.vis == Vis::Pub {
        let module = snapshot
            .module(def.module)
            .and_then(ModuleView::path)
            .and_then(|path| path.segments().last())
            .map_or_default(|last| format!("{last}::"));
        return Err(Refusal(format!("`{module}{old}` is `pub`: {STD_API}")));
    }
    match found.kind {
        DefKind::Fn => match satisfied(snapshot, view, def)? {
            Some((ty, interface)) => Err(Refusal(format!(
                "`{old}` makes `{}` satisfy `{}`, which packages outside the checkout may use",
                name_of(snapshot, ty),
                name_of(snapshot, interface)
            ))),
            None => Ok(()),
        },
        DefKind::Type
        | DefKind::Alias
        | DefKind::Const
        | DefKind::Interface
        | DefKind::InterfaceFn
        | DefKind::Ctor
        | DefKind::Test => Ok(()),
    }
}

/// The first type and interface that the function `def` makes the type
/// satisfy (§8.2, D-1706): over the types of `def`'s module and the
/// builtin types, whose home modules are std's (D-447), and every interface
/// with a function of `def`'s name. An `@external` counts, unlike in
/// `nav::provisions`. `view` is the cursor's module.
///
/// # Errors
///
/// A [`Refusal`] when the satisfaction cannot be read, since a rename
/// that may break one must not pass unseen.
fn satisfied(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    def: DefId,
) -> Result<Option<(DefId, DefId)>, Refusal> {
    let interfaces = snapshot.interfaces();
    let Some(func) = item(snapshot, def) else {
        return Ok(None);
    };
    let unreadable = || {
        Refusal(format!(
            "cannot tell which interfaces `{}` satisfies",
            name_of(snapshot, def)
        ))
    };
    let types: Vec<DefId> = [def.module, ModuleId::BUILTIN]
        .into_iter()
        .filter_map(|module| Some((module, interfaces.get(module)?)))
        .flat_map(|(module, iface)| {
            iface
                .items
                .iter_enumerated()
                .filter(|(_, item)| item.kind == DefKind::Type)
                .map(move |(index, _)| DefId::new(module, index))
        })
        .collect();
    if types.is_empty() {
        return Ok(None);
    }
    let item_of = |d: DefId| interfaces.get(d.module).and_then(|i| i.item(d.index));
    for &module in snapshot.all_ids() {
        let Some(iface) = interfaces.get(module) else {
            continue;
        };
        for (index, candidate) in iface.items.iter_enumerated() {
            if candidate.kind != DefKind::Interface {
                continue;
            }
            let interface = DefId::new(module, index);
            let fns = interface_fns(interface, &item_of).map_err(|_| unreadable())?;
            if !fns
                .iter()
                .any(|&f| item_of(f).is_some_and(|f| f.name == func.name))
            {
                continue;
            }
            let input = snapshot
                .query_input(def.module, view)
                .ok_or_else(unreadable)?;
            let found = implementors(&input, interface, &types).map_err(|_| unreadable())?;
            for (ty, verdict) in found {
                if let Verdict::Yes {
                    fns: satisfying, ..
                } = verdict
                    && satisfying.contains(&def)
                {
                    return Ok(Some((ty, interface)));
                }
            }
        }
    }
    Ok(None)
}

/// The attribute whose arguments name its function's parameters: on a
/// function with no body they are labels, and the index records no
/// occurrence of them (`index::build`, `attr_arg`).
const CLAMPED: &str = "std_clamped";

/// D-1706's refusal of a parameter that `@std_clamped` names on a
/// function with no body, where rename cannot find the name.
fn unrecorded(old: &str) -> Refusal {
    Refusal(format!(
        "`{old}` is named in `@{CLAMPED}` of a function with no body, where rename cannot edit it"
    ))
}

/// The name of the attribute `attr`: `std_clamped` in `@std_clamped(n)`.
fn attr_name(attr: SyntaxNode<'_>) -> Option<&str> {
    attr.children_with_tokens().find_map(|child| match child {
        SyntaxElement::Token(t) if t.kind() == K::LOWER_IDENT => Some(t.text()),
        SyntaxElement::Token(_) | SyntaxElement::Node(_) => None,
    })
}

/// The names that the argument list `args` writes.
fn arg_names(args: SyntaxNode<'_>) -> Vec<&str> {
    args.children_with_tokens()
        .filter_map(|child| match child {
            SyntaxElement::Token(t) if t.kind() == K::LOWER_IDENT => Some(t.text()),
            SyntaxElement::Token(_) | SyntaxElement::Node(_) => None,
        })
        .collect()
}

/// Whether the function `decl` carries `@std_clamped` with `name` among
/// its arguments.
fn clamps(decl: SyntaxNode<'_>, name: &str) -> bool {
    decl.children()
        .filter(|node| node.kind() == K::ATTR && attr_name(*node) == Some(CLAMPED))
        .filter_map(|attr| attr.first_child_of_kind(K::ATTR_ARG_LIST))
        .any(|args| arg_names(args).contains(&name))
}

/// Whether `token`, an argument of `@std_clamped`, names a parameter of a
/// function with no body: a name the index does not record (D-1706).
fn clamped_label(token: SyntaxToken<'_>) -> bool {
    let decl = token
        .parent()
        .filter(|node| node.kind() == K::ATTR_ARG_LIST)
        .and_then(SyntaxNode::parent)
        .filter(|attr| attr.kind() == K::ATTR && attr_name(*attr) == Some(CLAMPED))
        .and_then(SyntaxNode::parent)
        .filter(|decl| decl.kind() == K::FN_DECL && decl.first_child_of_kind(K::BLOCK).is_none());
    let Some(list) = decl.and_then(|decl| decl.first_child_of_kind(K::PARAM_LIST)) else {
        return false;
    };
    list.children()
        .filter(|node| node.kind() == K::PARAM)
        .filter_map(|param| param.first_child_of_kind(K::IDENT_PAT))
        .any(|pat| arg_names(pat).contains(&token.text()))
}

/// The case a new name of a symbol of `kind` must have: the old name's
/// (X §3.3).
const fn name_case(kind: NameKind) -> NameCase {
    match kind {
        NameKind::Type
        | NameKind::Alias
        | NameKind::Interface
        | NameKind::Constructor
        | NameKind::TypeParam => NameCase::Upper,
        NameKind::Module
        | NameKind::Function
        | NameKind::Constant
        | NameKind::InterfaceFn
        | NameKind::Test
        | NameKind::Field
        | NameKind::Param
        | NameKind::Local
        | NameKind::LocalFn => NameCase::Lower,
    }
}

/// D-1390's state gates. A local, a type parameter and an import alias are
/// named in their own module only, so it alone must parse and place every
/// name of the old text. An item or a field needs every module to parse,
/// and every name of the old text placed; an item also no import through a
/// cut cycle. An item does not count a name in a field position whose
/// field is unknown, which cannot name it (D-061, D-1727).
fn gates(index: Index<'_>, target: &Target<'_>) -> Result<(), Refusal> {
    let snapshot = index.snapshot();
    let old = target.old.as_str();
    let own_module = matches!(target.entry.symbol.member, Member::ImportAlias(_))
        || matches!(
            target.entry.meaning,
            Meaning::Local { .. } | Meaning::TypeParam { .. }
        );
    if own_module {
        if target.view.is_broken() {
            return Err(Refusal(
                "fix the syntax errors in this file first".to_owned(),
            ));
        }
        return match nav::own_partial(target.module, old) {
            Some(partial) => Err(partial_reason(index, &partial, old)),
            None => Ok(()),
        };
    }
    let status = snapshot.status();
    if let Some(path) = status.broken.iter().next() {
        return Err(Refusal(format!(
            "`{}` has syntax errors",
            shown_path(snapshot, path)
        )));
    }
    if matches!(target.entry.meaning, Meaning::Item(_))
        && let Some((path, _)) = status.cut_imports.first()
    {
        return Err(Refusal(format!(
            "`{}` imports through a cycle",
            shown_path(snapshot, path)
        )));
    }
    let item = matches!(target.entry.meaning, Meaning::Item(_));
    match index.partial_where(old, |name| !(item && name.field)) {
        Some(partial) => Err(partial_reason(index, &partial, old)),
        None => Ok(()),
    }
}

/// Why a rename of `old` is refused when a list of its occurrences may
/// be partial (F §5.5, D-1390).
fn partial_reason(index: Index<'_>, partial: &Partial, old: &str) -> Refusal {
    let snapshot = index.snapshot();
    match partial {
        Partial::Unknown { module, name } => {
            let file = index.module(*module).and_then(|m| m.file);
            let place = file.and_then(|file| place(snapshot, file, name.range.start()));
            Refusal(unknown_reason(name, old, place.as_deref()))
        }
        Partial::Lost { module } => {
            let shown = snapshot
                .module(*module)
                .and_then(ModuleView::path)
                .map_or_else(|| "a module".to_owned(), |p| format!("`{p}`"));
            Refusal(format!("an internal error stopped the check of {shown}"))
        }
    }
}

/// Why a name of the old text that the index cannot place stops a rename
/// of `old`: at `place`, `path:line`, or at the cursor when `None`.
fn unknown_reason(name: &UnknownName, old: &str, place: Option<&str>) -> String {
    let at = place.map_or_default(|p| format!(" at `{p}`"));
    match name.why {
        Unknown::UnresolvedPath => match place {
            Some(place) => format!("`{place}` may refer to `{old}` but does not resolve"),
            None => format!("`{old}` does not resolve"),
        },
        // A body of std that was not checked, a test of a closed module of a
        // std checkout (D-1701), has no facts for its fields, the only names
        // the index cannot place there (D-1706).
        Unknown::FieldOfUnknownType | Unknown::Unchecked => {
            format!("cannot tell what `.{}`{at} refers to", name.text)
        }
        // Only at the cursor: a name inside an error node elsewhere is in a
        // file with syntax errors, which `gates` refuses first, the
        // cursor's file for a local and every file for an item (D-1709).
        Unknown::InErrorNode => "fix the syntax errors in this file first".to_owned(),
        Unknown::BodyLost => format!("an internal error stopped the check of the code{at}"),
        Unknown::NotRecorded => format!("cannot tell what `{}`{at} refers to", name.text),
    }
}

/// `path:line` of `offset` in `file`, the path as diagnostics show it.
fn place(snapshot: &Snapshot, file: FileId, offset: TextSize) -> Option<String> {
    let source = snapshot.source(file)?;
    let path = match snapshot.location(file)? {
        FileLocation::Package(path) => shown_path(snapshot, path),
        FileLocation::Std(display) => display.clone(),
        FileLocation::Synthetic => source.path().to_owned(),
    };
    Some(format!("{path}:{}", source.line_col(offset).line))
}

/// `path` as diagnostics show it: relative to the package root, with
/// `/` between components on every platform (D-421).
fn shown_path(snapshot: &Snapshot, path: &std::path::Path) -> String {
    let relative = path.strip_prefix(snapshot.root()).unwrap_or(path);
    let parts: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.join("/")
}

/// D-1671's refusal for new errors: the first one's header and
/// `path:line:col`, or its file's path for one with no span (D-1709),
/// then how many more there are. A move's starts with "Moving" (D-1724),
/// and a fix's with "Applying the fix" (D-1731).
fn would_cause(diags: &[Introduced], action: Action) -> String {
    let verb = action.verb();
    let Some(first) = diags.first() else {
        return format!("{verb} would cause an error");
    };
    let diag = &first.diagnostic;
    let header = format!(
        "{}[{}]: {}",
        diag.severity.as_str(),
        diag.code,
        diag.message
    );
    with_more(
        format!("{verb} would cause: {header} at {}", first.at),
        diags.len(),
    )
}

/// D-1698's refusal for errors that would go away: the first one's header
/// and `path:line:col`, or its file's path when `verify` gives no place
/// (D-1709), then how many more there are.
fn would_hide(snapshot: &Snapshot, entries: &[(Option<String>, BaselineEntry)]) -> String {
    let Some((at, (code, path, message))) = entries.first() else {
        return "Renaming would hide an error".to_owned();
    };
    let header = format!("{}[{code}]: {message}", code.severity().as_str());
    let at = at
        .clone()
        .or_else(|| path.as_deref().map(|path| shown_path(snapshot, path)))
        .map_or_default(|at| format!(" at {at}"));
    with_more(
        format!("Renaming would hide an error: {header}{at}"),
        entries.len(),
    )
}

/// `text`, then "and N more" when `count` is over one.
fn with_more(text: String, count: usize) -> String {
    match count.checked_sub(1) {
        Some(more) if more > 0 => format!("{text} and {more} more"),
        Some(_) | None => text,
    }
}

/// What a rename writes beyond its symbol (D-1391, X §3.4): the symbols
/// to rename, the target's first, and the notes to show.
struct Coupled {
    symbols: Vec<Symbol>,
    notes: Vec<String>,
}

/// The coupling of renaming `entry`, named in `view` (X §3.4): a record
/// type's constructor, which is one name with it (F §5.3); an interface
/// function's satisfying functions; and the notes of a function that makes
/// a type satisfy an interface or serve an operator by its name, and of
/// `main`.
fn coupling(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    entry: &SymbolEntry,
) -> Result<Coupled, Refusal> {
    let mut out = Coupled {
        symbols: vec![entry.symbol.clone()],
        notes: Vec::new(),
    };
    if matches!(entry.symbol.member, Member::ImportAlias(_)) {
        out.symbols.extend(alias_twin(snapshot, &entry.symbol));
        return Ok(out);
    }
    let Meaning::Item(def) = entry.meaning else {
        return Ok(out);
    };
    if let Some(twin) = record_twin(snapshot, def).and_then(|d| snapshot.key_of(d)) {
        out.symbols.push(twin);
    }
    match item(snapshot, def).map(|item| item.kind) {
        Some(DefKind::InterfaceFn) => {
            let Satisfiers { local, outside } = satisfiers(snapshot, view, def)?;
            for satisfier in local {
                if let Some(symbol) = snapshot.key_of(satisfier)
                    && !out.symbols.contains(&symbol)
                {
                    out.symbols.push(symbol);
                    out.notes
                        .extend(losses(snapshot, view, satisfier, Some(def)));
                }
            }
            out.notes.extend(outside);
        }
        Some(DefKind::Fn) => {
            out.notes.extend(losses(snapshot, view, def, None));
            if is_main(snapshot, def) {
                out.notes
                    .push("the package's entry point is `main`".to_owned());
            }
        }
        Some(
            DefKind::Type
            | DefKind::Alias
            | DefKind::Const
            | DefKind::Interface
            | DefKind::Ctor
            | DefKind::Test,
        )
        | None => {}
    }
    Ok(out)
}

/// The symbols that renaming `entry`, named in `view`, writes (X §3.4):
/// its own, then those [`coupling`] adds, such as a record type's
/// constructor; its own alone when the coupling is refused. E0006's fix
/// reads them to tell a used name from an unused one (D-1741).
pub(crate) fn coupled_symbols(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    entry: &SymbolEntry,
) -> Vec<Symbol> {
    coupling(snapshot, view, entry).map_or_else(|_| vec![entry.symbol.clone()], |c| c.symbols)
}

/// The other symbol of the import alias `alias`, when its import binds
/// it in both namespaces: `Point as P` names a record type and its
/// constructor, `Id as Key` a type and a constructor of the same name
/// (D-111), and `{Circle as X, Shape as X}` gives two items one alias, one
/// in each namespace. Every use reads the alias's text, and the index
/// gives each `as` token of one import both symbols, so renaming both
/// writes them all.
fn alias_twin(snapshot: &Snapshot, alias: &Symbol) -> Option<Symbol> {
    let ns = match alias.ns {
        SymbolNs::Type => SymbolNs::Value,
        SymbolNs::Value => SymbolNs::Type,
        SymbolNs::Module | SymbolNs::Test => return None,
    };
    let other = Symbol {
        ns,
        ..alias.clone()
    };
    // The same import, not another import with the same alias in the other
    // namespace.
    let same = match (snapshot.find(alias)?, snapshot.find(&other)?) {
        (
            SymbolTarget::ImportAlias {
                module: a,
                import: i,
                ..
            },
            SymbolTarget::ImportAlias {
                module: b,
                import: j,
                ..
            },
        ) => a == b && i == j,
        (one, two) => one == two,
    };
    same.then_some(other)
}

/// The interface item `def`.
fn item(snapshot: &Snapshot, def: DefId) -> Option<&IfaceItem> {
    snapshot.interfaces().get(def.module)?.item(def.index)
}

/// The text of the name of `def`.
fn name_of(snapshot: &Snapshot, def: DefId) -> &str {
    item(snapshot, def).map_or("", |item| snapshot.globals().names.get(item.name))
}

/// The other half of a record type and its constructor, which share one
/// name token: the type of a record constructor, the constructor of a
/// record type. `None` for `type Id { Id(Int) }`, whose two names are two
/// tokens and two symbols (X §1.5).
fn record_twin(snapshot: &Snapshot, def: DefId) -> Option<DefId> {
    let iface = snapshot.interfaces().get(def.module)?;
    let item = iface.item(def.index)?;
    let span = item.span?;
    match item.kind {
        DefKind::Ctor => {
            let ty = item.parent?;
            (iface.item(ty)?.span == Some(span)).then(|| DefId::new(def.module, ty))
        }
        DefKind::Type => iface
            .items
            .iter_enumerated()
            .find(|(_, ctor)| {
                ctor.kind == DefKind::Ctor
                    && ctor.parent == Some(def.index)
                    && ctor.span == Some(span)
            })
            .map(|(index, _)| DefId::new(def.module, index)),
        DefKind::Fn
        | DefKind::Alias
        | DefKind::Const
        | DefKind::Interface
        | DefKind::InterfaceFn
        | DefKind::Test => None,
    }
}

/// What [`satisfiers`] finds.
struct Satisfiers {
    /// The package's functions that a rename of the interface function
    /// renames with it.
    local: Vec<DefId>,
    /// The note for the types that satisfy the interface through functions
    /// outside the package, which the rename cannot follow (D-1710).
    outside: Option<String>,
}

/// The functions that make a type satisfy the interface of the interface
/// function `def` by providing `def` (D-1391): over every type of the run,
/// those through an interface that embeds it included (D-1552). A builtin
/// or std type whose own function satisfies it does not block the rename:
/// it will no longer satisfy the interface, which the note says, and
/// `verify` refuses the rename with E0063 when the package uses that
/// satisfaction (D-1710, X §3.4).
///
/// # Errors
///
/// A [`Refusal`] when the satisfaction cannot be read.
fn satisfiers(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    def: DefId,
) -> Result<Satisfiers, Refusal> {
    let unreadable = || {
        Refusal(format!(
            "cannot tell which functions provide `{}`",
            name_of(snapshot, def)
        ))
    };
    let interfaces = snapshot.interfaces();
    let interface = item(snapshot, def)
        .and_then(|item| item.parent)
        .map(|parent| DefId::new(def.module, parent))
        .ok_or_else(unreadable)?;
    let item_of = |d: DefId| interfaces.get(d.module).and_then(|i| i.item(d.index));
    let fns = interface_fns(interface, &item_of).map_err(|_| unreadable())?;
    let Some(k) = fns.iter().position(|&f| f == def) else {
        return Err(unreadable());
    };
    let input = snapshot
        .query_input(interface.module, view)
        .ok_or_else(unreadable)?;
    let types: Vec<DefId> = snapshot
        .all_ids()
        .iter()
        .filter_map(|&module| Some((module, interfaces.get(module)?)))
        .flat_map(|(module, iface)| {
            iface
                .items
                .iter_enumerated()
                .filter(|(_, item)| item.kind == DefKind::Type)
                .map(move |(index, _)| DefId::new(module, index))
        })
        .collect();
    let found = implementors(&input, interface, &types).map_err(|_| unreadable())?;
    let mut local = Vec::new();
    let mut outside: Vec<String> = Vec::new();
    for (ty, verdict) in found {
        let Verdict::Yes {
            fns: satisfying, ..
        } = verdict
        else {
            continue;
        };
        let Some(&satisfier) = satisfying.get(k) else {
            continue;
        };
        if !snapshot
            .module(satisfier.module)
            .is_some_and(ModuleView::is_own)
        {
            // A builtin type has no module to name (§7.6).
            let module = snapshot
                .module(ty.module)
                .filter(|_| ty.module != ModuleId::BUILTIN)
                .and_then(ModuleView::path)
                .map_or_default(|p| format!("{p}::"));
            let shown = format!("`{module}{}`", name_of(snapshot, ty));
            if !outside.contains(&shown) {
                outside.push(shown);
            }
            continue;
        }
        if !local.contains(&satisfier) {
            local.push(satisfier);
        }
    }
    let outside = listed(&outside).map(|types| {
        format!(
            "{types} will no longer satisfy `{}`",
            name_of(snapshot, interface)
        )
    });
    Ok(Satisfiers { local, outside })
}

/// `items` as a list in a sentence: `a`, `a and b`, `a, b and c`;
/// `None` for none.
fn listed(items: &[String]) -> Option<String> {
    let (last, rest) = items.split_last()?;
    Some(if rest.is_empty() {
        last.clone()
    } else {
        format!("{} and {last}", rest.join(", "))
    })
}

/// An item of the type namespace and a constructor of one module that
/// share a name in two tokens, as `namesake` finds them.
struct Namesakes {
    /// The type, type alias or interface.
    ty: DefId,
    /// The constructor.
    ctor: DefId,
}

/// The constructor of the same name as the type, type alias or interface
/// `def`, or the item of the type namespace of the same name as the
/// constructor `def`, in `def`'s module, in two tokens:
/// `type Id { Id(Int) }`, or `type Shape { Circle }`, `type Shape = …`
/// or `interface Shape` beside `type Other { Shape }` (D-1718, D-1723).
/// Never a record type's constructor, which is one name with its type
/// (`record_twin`).
fn namesake(snapshot: &Snapshot, def: DefId) -> Option<Namesakes> {
    let iface = snapshot.interfaces().get(def.module)?;
    let item = iface.item(def.index)?;
    let is_type = match item.kind {
        DefKind::Type | DefKind::Alias | DefKind::Interface => true,
        DefKind::Ctor => false,
        DefKind::Fn | DefKind::Const | DefKind::InterfaceFn | DefKind::Test => return None,
    };
    let other = |kind: DefKind| match kind {
        DefKind::Type | DefKind::Alias | DefKind::Interface => !is_type,
        DefKind::Ctor => is_type,
        DefKind::Fn | DefKind::Const | DefKind::InterfaceFn | DefKind::Test => false,
    };
    // A type's own constructor first, which D-1718's text names.
    let (found, _) = iface
        .items
        .iter_enumerated()
        .filter(|(_, o)| other(o.kind) && o.name == item.name)
        .min_by_key(|(index, o)| {
            let own = if is_type {
                o.parent == Some(def.index)
            } else {
                item.parent == Some(*index)
            };
            !own
        })?;
    let found = DefId::new(def.module, found);
    if record_twin(snapshot, def) == Some(found) {
        return None;
    }
    Some(if is_type {
        Namesakes {
            ty: def,
            ctor: found,
        }
    } else {
        Namesakes {
            ty: found,
            ctor: def,
        }
    })
}

/// Refuses a type, type alias, interface or constructor whose namesake
/// (`namesake`) an import list names with the same token,
/// `import m::{Id}`: the one token names both, so writing it for one would
/// leave the other unimported. The text names the constructor's type when
/// it is another item, and that item's kind (D-1718, D-1723).
fn shared_import(index: Index<'_>, entry: &SymbolEntry, old: &str) -> Result<(), Refusal> {
    let Meaning::Item(def) = entry.meaning else {
        return Ok(());
    };
    if matches!(entry.symbol.member, Member::ImportAlias(_)) {
        return Ok(());
    }
    let snapshot = index.snapshot();
    let Some(pair) = namesake(snapshot, def) else {
        return Ok(());
    };
    // A cancel that never fires: `prepare` takes none, and the search
    // runs only for such a pair.
    let Ok(found) = index.references(&entry.symbol, &CancelToken::new()) else {
        return Ok(());
    };
    let Some(hit) = found
        .hits
        .iter()
        .find(|hit| hit.occurrence.role == Role::Import)
    else {
        return Ok(());
    };
    let at = hit
        .module
        .file
        .and_then(|file| place(snapshot, file, hit.occurrence.range.start()))
        .map_or_default(|place| format!(" at `{place}`"));
    let parent = item(snapshot, pair.ctor)
        .and_then(|ctor| ctor.parent)
        .map(|parent| DefId::new(pair.ctor.module, parent));
    let kind = match item(snapshot, pair.ty).map(|ty| ty.kind) {
        Some(DefKind::Alias) => "type alias",
        Some(DefKind::Interface) => "interface",
        Some(
            DefKind::Type
            | DefKind::Ctor
            | DefKind::Fn
            | DefKind::Const
            | DefKind::InterfaceFn
            | DefKind::Test,
        )
        | None => "type",
    };
    let both = match parent {
        Some(parent) if parent != pair.ty => format!(
            "the {kind} `{old}` and the constructor `{old}` of `{}`",
            name_of(snapshot, parent)
        ),
        Some(_) | None => "the type and its constructor".to_owned(),
    };
    Err(Refusal(format!(
        "the import's `{old}`{at} names both {both}"
    )))
}

/// A parameter of a function with no body, an `@external` one: no body
/// names it, so its one occurrence is its name in the signature
/// (D-1717).
struct BodilessParam {
    /// The name's token.
    range: TextRange,
    /// The name.
    old: String,
    /// The function's name.
    function: String,
    /// The names of the signature's other parameters, which the compiler
    /// does not check in a function with no body (D-1722).
    others: Vec<String>,
}

/// The parameter of a function with no body whose name is at `offset` of
/// `file`, if one is (D-1717).
///
/// # Errors
///
/// A [`Refusal`] in a document that cannot be edited (D-1673), and in a
/// file with syntax errors, as for a local (D-1390).
fn bodiless_param(
    snapshot: &Snapshot,
    file: FileId,
    offset: TextSize,
) -> Result<Option<BodilessParam>, Refusal> {
    let (Some(view), Some(source)) = (snapshot.module_at(file), snapshot.source(file)) else {
        return Ok(None);
    };
    let Some(tree) = view.tree() else {
        return Ok(None);
    };
    let Some(token) =
        pick_ident(token_at(tree.root(), offset)).filter(|t| t.kind() == K::LOWER_IDENT)
    else {
        return Ok(None);
    };
    let Some(param) = token
        .parent()
        .filter(|node| node.kind() == K::IDENT_PAT)
        .and_then(SyntaxNode::parent)
        .filter(|node| node.kind() == K::PARAM)
    else {
        return Ok(None);
    };
    let Some(list) = param.parent().filter(|node| node.kind() == K::PARAM_LIST) else {
        return Ok(None);
    };
    let Some(decl) = list
        .parent()
        .filter(|node| node.kind() == K::FN_DECL && node.first_child_of_kind(K::BLOCK).is_none())
    else {
        return Ok(None);
    };
    let range = token.text_range();
    let old = range.slice(source.text()).unwrap_or_default().to_owned();
    let function = decl
        .children_with_tokens()
        .find_map(|child| match child {
            SyntaxElement::Token(t) if t.kind() == K::LOWER_IDENT => Some(t.text().to_owned()),
            SyntaxElement::Token(_) | SyntaxElement::Node(_) => None,
        })
        .unwrap_or_default();
    let others: Vec<String> = list
        .children()
        .filter(|node| node.kind() == K::PARAM && *node != param)
        .filter_map(|node| node.first_child_of_kind(K::IDENT_PAT))
        .flat_map(|pat| {
            pat.children_with_tokens()
                .filter_map(|child| match child {
                    SyntaxElement::Token(t) if t.kind() == K::LOWER_IDENT => {
                        Some(t.text().to_owned())
                    }
                    SyntaxElement::Token(_) | SyntaxElement::Node(_) => None,
                })
                .collect::<Vec<_>>()
        })
        .collect();
    if matches!(snapshot.location(file), Some(FileLocation::Std(_))) {
        return Err(not_editable(&old, "std"));
    }
    if view.is_broken() {
        return Err(Refusal(
            "fix the syntax errors in this file first".to_owned(),
        ));
    }
    if snapshot.is_std_checkout() && clamps(decl, &old) {
        return Err(unrecorded(&old));
    }
    Ok(Some(BodilessParam {
        range,
        old,
        function,
        others,
    }))
}

/// The plan that renames `param`, of `file`, to `new_name`: its one
/// occurrence, checked by `verify` as any rename (D-1717). The compiler
/// checks no parameter name of a function with no body, so a name that
/// another parameter of the signature has is refused here (D-1722).
///
/// # Errors
///
/// As [`rename()`].
fn rename_bodiless(
    analyzed: &Analyzed,
    file: FileId,
    param: &BodilessParam,
    new_name: &str,
    cancel: &CancelToken,
) -> Result<Option<RenamePlan>, RenameError> {
    check_candidate(new_name, NameCase::Lower).map_err(Refusal)?;
    if new_name == param.old {
        return Ok(Some(RenamePlan::default()));
    }
    if param.others.iter().any(|other| other == new_name) {
        return Err(Refusal(format!(
            "`{new_name}` is already a parameter of `{}`",
            param.function
        ))
        .into());
    }
    let Some(FileLocation::Package(path)) = analyzed.snapshot.location(file) else {
        return Ok(None);
    };
    let edits = vec![PlainEdit {
        path: path.clone(),
        range: param.range,
        replacement: new_name.to_owned(),
    }];
    cancel.check()?;
    let renamed = [(param.old.as_str(), new_name)];
    check(
        analyzed,
        &VerifyRequest {
            edits: &edits,
            moves: &[],
            renamed: &renamed,
            hides: true,
            written: &[],
        },
        Action::Rename,
    )?;
    Ok(Some(RenamePlan { edits, note: None }))
}

/// The notes of renaming the function `def` (X §3.4): each type that would
/// stop satisfying an interface through it, and each operator that would
/// stop applying to a type. With `renamed`, the interface function renamed
/// with `def`, the interfaces whose functions hold it keep their
/// satisfaction and give no note.
fn losses(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    def: DefId,
    renamed: Option<DefId>,
) -> Vec<String> {
    let interfaces = snapshot.interfaces();
    let item_of = |d: DefId| interfaces.get(d.module).and_then(|i| i.item(d.index));
    let mut noted: Vec<DefId> = Vec::new();
    let mut out = Vec::new();
    for provision in nav::provisions(snapshot, view, def) {
        let follows = renamed.is_some_and(|renamed| {
            interface_fns(provision.interface, &item_of).is_ok_and(|fns| fns.contains(&renamed))
        });
        if follows {
            continue;
        }
        let note = format!(
            "`{}` will no longer satisfy `{}`",
            name_of(snapshot, provision.ty),
            name_of(snapshot, provision.interface)
        );
        if !out.contains(&note) {
            out.push(note);
        }
        noted.push(provision.ty);
    }
    if let Some((tys, operator)) = operator(snapshot, def) {
        for ty in tys.into_iter().filter(|ty| !noted.contains(ty)) {
            out.push(format!(
                "`{operator}` will no longer apply to `{}`",
                name_of(snapshot, ty)
            ));
        }
    }
    out
}

/// The functions behind the overloadable operators (§8.7, D-118): name,
/// number of parameters, the operator, and whether the function returns
/// its type.
const OPERATORS: [(&str, usize, &str, bool); 6] = [
    ("add", 2, "+", true),
    ("sub", 2, "-", true),
    ("mul", 2, "*", true),
    ("div", 2, "/", true),
    ("neg", 1, "-", true),
    ("compare", 2, "<", false),
];

/// The types and the operator that the function `def` serves by its name
/// and signature, as the checker's `find_fn` matches it for `Want::Op`
/// (`cheby_types`, `ifaces/solve.rs`; §8.7, D-118). The checker binds
/// the function's type parameters by matching its first parameter against
/// the operand's type, and every one must be bound there (D-504); then each
/// parameter must be that type, and the return type too, or `Order` for
/// `compare`; and the function must be as visible as the type. So
/// `fn add<T>(a: Cell<T>, b: Cell<Int>) -> Cell<T>` serves `+` on
/// `Cell<Int>`. A type is served when some instance of it is: the
/// signature's types unify, with the function's type parameters as
/// variables, into one operand type whose head is a type of `def`'s
/// module, or into a bare parameter, which every type of the module
/// takes. Last the checker asks that the bounds of the function's type
/// parameters hold (`bound_dicts`). A type that meets a bare parameter's
/// bounds then satisfies std's interface for the operator through `def`,
/// which the satisfaction's note says, so a bounded bare parameter takes
/// no type here.
fn operator(snapshot: &Snapshot, def: DefId) -> Option<(Vec<DefId>, &'static str)> {
    let func = item(snapshot, def)?;
    let ItemSig::Fn(sig) = &func.sig else {
        return None;
    };
    let globals = snapshot.globals();
    let name = globals.names.get(func.name);
    let &(_, arity, operator, returns_self) = OPERATORS.iter().find(|(n, ..)| *n == name)?;
    if sig.params.len() != arity {
        return None;
    }
    let first = sig.params.first()?.ty;
    let mut bound: Vec<(ParamIdx, Option<Ty>)> = sig
        .generics
        .params
        .iter_enumerated()
        .map(|(param, _)| (param, None))
        .collect();
    if !bound
        .iter()
        .all(|&(param, _)| mentions(globals, first, param))
    {
        return None;
    }
    let ret = if returns_self { first } else { Ty::ORDER };
    let fits = sig
        .params
        .iter()
        .all(|p| unify(globals, first, p.ty, &mut bound))
        && unify(globals, ret, sig.ret, &mut bound);
    if !fits {
        return None;
    }
    let iface = snapshot.interfaces().get(def.module)?;
    let types: Vec<DefId> = match globals.types.kind(resolve(globals, first, &bound)) {
        TyKind::Named { def: head, .. } if head.module == def.module => vec![*head],
        // A bounded parameter takes only the types that meet its bounds,
        // and each of those then satisfies std's interface for the
        // operator through `def`, so the satisfaction's note names it.
        TyKind::Param(param)
            if sig
                .generics
                .params
                .get(*param)
                .is_some_and(|p| !p.bounds.is_empty()) =>
        {
            Vec::new()
        }
        TyKind::Param(_) => iface
            .items
            .iter_enumerated()
            .filter(|(_, item)| item.kind == DefKind::Type)
            .map(|(index, _)| DefId::new(def.module, index))
            .collect(),
        TyKind::Named { .. }
        | TyKind::Tuple(_)
        | TyKind::Fn { .. }
        | TyKind::Dyn { .. }
        | TyKind::Error
        | TyKind::Never => Vec::new(),
    };
    let served: Vec<DefId> = types
        .into_iter()
        .filter(|&ty| item(snapshot, ty).is_some_and(|ty| rank(func.vis) >= rank(ty.vis)))
        .collect();
    (!served.is_empty()).then_some((served, operator))
}

/// The rank of a visibility, as the checker orders them: `priv` below the
/// package's below `pub`.
const fn rank(vis: Vis) -> u8 {
    match vis {
        Vis::Priv => 0,
        Vis::Package => 1,
        Vis::Pub => 2,
    }
}

/// The type each of a function's type parameters is bound to so far.
type Bound = [(ParamIdx, Option<Ty>)];

/// `ty`, followed through `bound` while it is a bound type parameter.
fn resolve(globals: &Globals, ty: Ty, bound: &Bound) -> Ty {
    let mut ty = ty;
    while let TyKind::Param(param) = globals.types.kind(ty)
        && let Some(&(_, Some(next))) = bound.iter().find(|(p, _)| p == param)
    {
        ty = next;
    }
    ty
}

/// Whether `ty`, read through `bound`, mentions `param`.
fn occurs(globals: &Globals, param: ParamIdx, ty: Ty, bound: &Bound) -> bool {
    let all = |tys: &[Ty]| tys.iter().any(|&t| occurs(globals, param, t, bound));
    match globals.types.kind(resolve(globals, ty, bound)) {
        TyKind::Param(p) => *p == param,
        TyKind::Named { args, .. } | TyKind::Dyn { args, .. } | TyKind::Tuple(args) => all(args),
        TyKind::Fn { params, ret } => all(params) || occurs(globals, param, *ret, bound),
        TyKind::Error | TyKind::Never => false,
    }
}

/// Whether `ty` mentions the type parameter `param`.
fn mentions(globals: &Globals, ty: Ty, param: ParamIdx) -> bool {
    occurs(globals, param, ty, &[])
}

/// Unifies `a` and `b`, whose type parameters are the variables that
/// `bound` binds. Whether they unify; `bound` may be partly filled when
/// they do not.
fn unify(globals: &Globals, a: Ty, b: Ty, bound: &mut Bound) -> bool {
    let (a, b) = (resolve(globals, a, bound), resolve(globals, b, bound));
    if a == b {
        return true;
    }
    let all = |xs: &[Ty], ys: &[Ty], bound: &mut Bound| {
        xs.len() == ys.len()
            && xs
                .iter()
                .zip(ys)
                .all(|(&x, &y)| unify(globals, x, y, bound))
    };
    match (globals.types.kind(a), globals.types.kind(b)) {
        (TyKind::Param(param), _) => bind(globals, *param, b, bound),
        (_, TyKind::Param(param)) => bind(globals, *param, a, bound),
        (TyKind::Named { def: x, args: xs }, TyKind::Named { def: y, args: ys })
        | (TyKind::Dyn { iface: x, args: xs }, TyKind::Dyn { iface: y, args: ys }) => {
            x == y && all(xs, ys, bound)
        }
        (TyKind::Tuple(xs), TyKind::Tuple(ys)) => all(xs, ys, bound),
        (
            TyKind::Fn {
                params: xs,
                ret: rx,
            },
            TyKind::Fn {
                params: ys,
                ret: ry,
            },
        ) => all(xs, ys, bound) && unify(globals, *rx, *ry, bound),
        _ => false,
    }
}

/// Binds the type parameter `param` to `ty`, unless `ty` mentions it.
fn bind(globals: &Globals, param: ParamIdx, ty: Ty, bound: &mut Bound) -> bool {
    if occurs(globals, param, ty, bound) {
        return false;
    }
    match bound.iter_mut().find(|(p, _)| *p == param) {
        Some((_, found)) => {
            *found = Some(ty);
            true
        }
        None => false,
    }
}

/// Whether `def` is the package's entry point: `main` of its root module.
fn is_main(snapshot: &Snapshot, def: DefId) -> bool {
    name_of(snapshot, def) == "main"
        && snapshot
            .module(def.module)
            .and_then(ModuleView::path)
            .is_some_and(|path| path.segments().len() == 1)
}

/// Whether the token at `range` is the punned end of a field: the name
/// of `Book { title }`, `P { x }` as a pattern, or the last of
/// `Shape { ..s, origin.y }` (D-1640), which names a field and a value
/// at once.
fn punned(tree: &SyntaxTree, range: TextRange) -> bool {
    let Some(token) = tree
        .root()
        .token_at_offset(range.start())
        .filter(|t| t.text_range() == range)
    else {
        return false;
    };
    let Some(parent) = token
        .parent()
        .filter(|p| matches!(p.kind(), K::FIELD_INIT | K::FIELD_PAT))
    else {
        return false;
    };
    let mut colon = false;
    let mut last = None;
    for child in parent.children_with_tokens() {
        match child {
            SyntaxElement::Token(t) if t.kind() == K::COLON => colon = true,
            SyntaxElement::Token(t) if matches!(t.kind(), K::LOWER_IDENT | K::UPPER_IDENT) => {
                last = Some(t);
            }
            SyntaxElement::Token(_) | SyntaxElement::Node(_) => {}
        }
    }
    !colon && last == Some(token)
}

#[cfg(test)]
#[path = "rename_tests.rs"]
mod tests;
