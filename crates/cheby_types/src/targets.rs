//! The cross-target check (§12.6, C12, D-056, D-190, D-207): every use of
//! an item exists on every target the using code runs on, for every target
//! the package declares (E0067). Item targets are
//! [`cheby_iface::IfaceItem::targets`].
//!
//! Where code runs is computed per item of this module:
//!
//! - A signature exists where its item does: the module's targets (the
//!   package's targets narrowed by `@!target`, D-190, D-141, published by
//!   the resolver as `ResolvedModule::targets`) intersected with the
//!   item's own targets. Every type, alias and interface its written types
//!   name must exist there too.
//! - A body runs where its item exists, minus the targets on which the
//!   function has an `@external` (D-207). Every item its written paths name
//!   (functions, constants, constructors, types in annotations, interface
//!   functions through their interface) and every item the checker chose
//!   for it (satisfying functions of dictionaries, operator functions,
//!   `show` of interpolations) must exist there.
//!
//! The check covers exactly the targets the package declares and runs on
//! every build, whatever target is selected (§12.6). Each top-level item
//! used by one item of this module is reported once, at its first use
//! (the signature's before the body's): a type and its constructors, or
//! an interface and its functions, are one root cause
//! (D-512). A constant's initializer runs at compile time, so only the
//! constant's type is checked against its targets (D-512).
//!
//! The same pass checks `Int` literals in code that runs on JS against the
//! JS safe-integer range (§3.3.4, D-037, E0071). A constant's initializer
//! is evaluated at compile time and never runs on JS, so its literals are
//! not checked here; the evaluated value is (§4.5, E6, D-511).

use std::fmt;

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{DefId, DefIndex, DefKind, Ice, IfaceItem, ModuleInterface, Ty};
use cheby_resolve::{ItemDef, Res};
use cheby_syntax::NodeId;
use cheby_syntax::ast::{AstNode, PathNode};
use rustc_hash::FxHashMap;

use crate::TypesInput;
use crate::thir::{self, Callee, Dict, ExprKind, InterpPart, Lit, OpImpl, PatKind};

/// The largest magnitude of an `Int` on JS: 2⁵³ − 1 (D-037).
const JS_SAFE_MAX: i128 = 9_007_199_254_740_991;

/// The cross-target errors of the module's checked bodies. Called by
/// [`crate::check_bodies`] after every item is checked.
///
/// # Errors
///
/// An [`Ice`] if THIR and the interfaces disagree.
pub(crate) fn check_module(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    thir: &thir::Module,
) -> Result<Vec<Diagnostic>, Ice> {
    let cx = Cx { input, own };
    let mut uses = Vec::new();
    cx.signature_uses(&mut uses)?;
    cx.written_body_uses(&mut uses)?;
    for f in &thir.fns {
        if let Some(body) = &f.body {
            let mut found = UseSink::default();
            implicit_uses(body, &mut found);
            if let Some(entry) = &f.entry {
                entry_uses(entry, f.span, &mut found);
            }
            cx.push_body_uses(f.def.index, found, &mut uses)?;
        }
    }
    for c in &thir.consts {
        let mut found = UseSink::default();
        implicit_uses(&c.body, &mut found);
        cx.push_body_uses(c.def.index, found, &mut uses)?;
    }
    for t in &thir.tests {
        let mut found = UseSink::default();
        implicit_uses(&t.body, &mut found);
        entry_uses(&t.entry, t.span, &mut found);
        cx.push_body_uses(t.def.index, found, &mut uses)?;
    }
    let mut diags = cx.report(uses)?;
    for f in &thir.fns {
        if let Some(body) = &f.body {
            cx.js_literals(f.def.index, body, &mut diags)?;
        }
    }
    for t in &thir.tests {
        cx.js_literals(t.def.index, &t.body, &mut diags)?;
    }
    Ok(diags)
}

struct Cx<'a, 'i> {
    input: &'a TypesInput<'i>,
    own: &'a ModuleInterface,
}

/// One use of an item by code of this module.
struct Use {
    /// The top-level item whose signature or body holds the use.
    user: DefIndex,
    /// Whether the use is in a signature (where the item exists) or in a
    /// body (where it runs).
    site: Site,
    /// Where the using code exists or runs.
    runs: Set,
    used: DefId,
    span: Span,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Site {
    Signature,
    Body,
}

/// Uses found in one body, before they are tied to their item.
#[derive(Default)]
struct UseSink(Vec<(DefId, Span)>);

impl UseSink {
    fn push(&mut self, def: DefId, span: Span) {
        self.0.push((def, span));
    }

    fn callee(&mut self, callee: &Callee, span: Span) {
        match callee {
            Callee::Fn { def, dicts, .. } => {
                self.push(*def, span);
                self.dicts(dicts, span);
            }
            Callee::Ctor { ctor, .. } => self.push(*ctor, span),
            Callee::Iface { iface_fn, dict } => {
                self.push(*iface_fn, span);
                self.dict(dict, span);
            }
            Callee::Dyn { iface_fn } => self.push(*iface_fn, span),
            // A value of function type: whatever produced it is a use of
            // its own.
            _ => {}
        }
    }

    fn dicts(&mut self, dicts: &[Dict], span: Span) {
        for dict in dicts {
            self.dict(dict, span);
        }
    }

    fn dict(&mut self, dict: &Dict, span: Span) {
        match dict {
            Dict::Impl { iface, fns, .. } => {
                self.push(*iface, span);
                for f in fns {
                    self.push(f.def, span);
                    self.dicts(&f.dicts, span);
                }
            }
            Dict::Param { .. } => {}
            Dict::Embedded { of, iface } => {
                self.push(*iface, span);
                self.dict(of, span);
            }
            Dict::Tuple { iface, elems, .. } => {
                self.push(*iface, span);
                self.dicts(elems, span);
            }
            Dict::Dyn { iface, .. } => self.push(*iface, span),
        }
    }
}

/// Every item a checked body uses, written or chosen by the checker: the
/// callees, function values, constants, constructors (in expressions and
/// patterns), interface functions, dictionaries, operator functions and
/// `show` functions of interpolations.
fn implicit_uses(body: &thir::Body, out: &mut UseSink) {
    for expr in &body.exprs {
        let span = expr.span;
        match &expr.kind {
            ExprKind::Ctor { ctor, .. } | ExprKind::Record { ctor, .. } => out.push(*ctor, span),
            ExprKind::Call { callee, .. } => out.callee(callee, span),
            ExprKind::FnRef { def, dicts, .. } => {
                out.push(*def, span);
                out.dicts(dicts, span);
            }
            ExprKind::ConstRef { def } => out.push(*def, span),
            ExprKind::Binary { imp, .. } | ExprKind::Neg { imp, .. } => {
                if let OpImpl::Call(callee) = imp {
                    out.callee(callee, span);
                }
            }
            ExprKind::Interpolate { parts } => {
                for part in parts {
                    if let InterpPart::Value {
                        show: Some(callee), ..
                    } = part
                    {
                        out.callee(callee, span);
                    }
                }
            }
            ExprKind::DynCoerce { iface, dict, .. } => {
                out.push(*iface, span);
                out.dict(dict, span);
            }
            _ => {}
        }
    }
    for (_, pat) in body.pats() {
        pat.walk(&mut |p| {
            if let PatKind::Ctor { ctor, .. } = &p.kind {
                out.push(*ctor, p.span);
            }
        });
    }
}

/// The `Show` an entry uses to print an error (D-182).
fn entry_uses(entry: &thir::EntryInfo, span: Span, out: &mut UseSink) {
    if let thir::EntryInfo::Result {
        show: Some(dict), ..
    } = entry
    {
        out.dict(dict, span);
    }
}

impl Cx<'_, '_> {
    fn item_def(&self, index: DefIndex) -> Result<&ItemDef, Ice> {
        self.input
            .resolved
            .item(index)
            .ok_or_else(|| Ice::new("an item is missing from the resolution"))
    }

    /// The top-level item of `index`: a constructor's type, an interface
    /// function's interface, or the item itself.
    fn top_level(&self, index: DefIndex) -> Result<DefIndex, Ice> {
        Ok(self.item_def(index)?.parent.unwrap_or(index))
    }

    /// Where item `index` exists in this build: the module's targets
    /// (package ∩ `@!target`) ∩ the item's own.
    fn exists(&self, index: DefIndex) -> Result<Set, Ice> {
        let module = Set::of(self.input.resolved.targets.iter());
        let item = Set::of(self.item_def(index)?.attrs.targets.iter());
        Ok(module.intersection(item))
    }

    /// Where the body of item `index` runs: where it exists, minus the
    /// targets with an `@external` (D-207).
    fn runs(&self, index: DefIndex) -> Result<Set, Ice> {
        let item = self.item_def(index)?;
        let externals = Set::of(item.attrs.externals.iter().map(external_target));
        Ok(self.exists(index)?.minus(externals))
    }

    /// Item declaration nodes, to find the item a signature path is in.
    fn item_nodes(&self) -> FxHashMap<NodeId, DefIndex> {
        self.input
            .resolved
            .items
            .iter_enumerated()
            .map(|(index, item)| (item.node, index))
            .collect()
    }

    /// The uses in signatures: every path of `sig_paths` that names an
    /// item, tied to the item whose declaration holds it.
    fn signature_uses(&self, out: &mut Vec<Use>) -> Result<(), Ice> {
        let nodes = self.item_nodes();
        for (&node, &res) in &self.input.resolved.sig_paths {
            let Res::Def(def) = res else { continue };
            let Some(syntax) = self.input.tree.node(node) else {
                return Err(Ice::new("a signature path is not in the tree"));
            };
            let Some(owner) = syntax.ancestors().find_map(|n| nodes.get(&n.id()).copied()) else {
                return Err(Ice::new("a signature path is outside every item"));
            };
            let user = self.top_level(owner)?;
            out.push(Use {
                user,
                site: Site::Signature,
                runs: self.exists(user)?,
                used: def,
                span: path_span(syntax),
            });
        }
        Ok(())
    }

    /// The written uses in bodies: every path of every body that names an
    /// item, including types in annotations, which THIR does not keep.
    fn written_body_uses(&self, out: &mut Vec<Use>) -> Result<(), Ice> {
        for (&index, body) in &self.input.resolved.bodies {
            let runs = self.runs(index)?;
            for (&node, &res) in &body.paths {
                let Res::Def(def) = res else { continue };
                let Some(syntax) = self.input.tree.node(node) else {
                    return Err(Ice::new("a body path is not in the tree"));
                };
                out.push(Use {
                    user: index,
                    site: Site::Body,
                    runs,
                    used: def,
                    span: path_span(syntax),
                });
            }
        }
        Ok(())
    }

    fn push_body_uses(
        &self,
        index: DefIndex,
        found: UseSink,
        out: &mut Vec<Use>,
    ) -> Result<(), Ice> {
        let runs = self.runs(index)?;
        out.extend(found.0.into_iter().map(|(used, span)| Use {
            user: index,
            site: Site::Body,
            runs,
            used,
            span,
        }));
        Ok(())
    }

    /// E0067 for every use of an item that does not exist everywhere its
    /// user runs, once per using item and used top-level item (a
    /// constructor counts as its type, an interface function as its
    /// interface), at the first use: signatures first, then bodies.
    fn report(&self, mut all: Vec<Use>) -> Result<Vec<Diagnostic>, Ice> {
        all.sort_by_key(|u| (u.user, u.site == Site::Body, span_key(u.span), u.used));
        let mut diags = Vec::new();
        let mut reported: Vec<(DefIndex, DefId)> = Vec::new();
        for u in all {
            if u.runs.is_empty() {
                continue;
            }
            let item = self.input.item(self.own, u.used)?;
            let there = Set::of(item.targets.iter());
            // An item on no target was already reported (E0045).
            if there.is_empty() || there.intersection(u.runs) == u.runs {
                continue;
            }
            let root = item
                .parent
                .map_or(u.used, |parent| DefId::new(u.used.module, parent));
            let key = (u.user, root);
            if reported.contains(&key) {
                continue;
            }
            reported.push(key);
            diags.push(self.mismatch(&u, item, there)?);
        }
        Ok(diags)
    }

    fn mismatch(&self, u: &Use, item: &IfaceItem, there: Set) -> Result<Diagnostic, Ice> {
        let names = &self.input.globals.names;
        let item_name = names.get(item.name);
        let owner = self.item_def(u.user)?;
        let owner_name = names.get(owner.name);
        let missing = u.runs.minus(there);
        let overlap = u.runs.intersection(there);
        let also = if overlap.is_empty() { "" } else { "also " };
        let (who, verb) = match (u.site, owner.kind) {
            (Site::Signature, _) | (Site::Body, DefKind::Const) => {
                (format!("`{owner_name}`"), "exists")
            }
            (Site::Body, DefKind::Test) => (format!("the test \"{owner_name}\""), "runs"),
            (Site::Body, _) if !owner.attrs.externals.is_empty() => {
                (format!("the body of `{owner_name}`"), "runs")
            }
            (Site::Body, _) => (format!("`{owner_name}`"), "runs"),
        };
        let mut diag = Diagnostic::new(
            codes::TARGET_MISMATCH,
            format!(
                "`{item_name}` exists only on {}, but {who} {also}{verb} on {}",
                there.list(),
                missing.list()
            ),
        )
        .with_primary(u.span, format!("not available on {}", missing.list()));
        if let Some(span) = item.span
            && span != u.span
        {
            diag = diag.with_secondary(span, format!("`{item_name}` is declared here"));
        }
        if u.site == Site::Body && !owner.attrs.externals.is_empty() {
            diag = diag.with_note(format!(
                "the body of `{owner_name}` runs only on the targets it has no `@external` for"
            ));
        }
        if let Some(target) = overlap.single() {
            let restrict = match (u.site, owner.kind) {
                (_, DefKind::Test) => format!("the test \"{owner_name}\""),
                _ => format!("`{owner_name}`"),
            };
            diag = diag.with_help(format!(
                "restrict {restrict} to `{target}` with `@target({target})`, or the whole \
                 module with `@!target({target})`"
            ));
            if u.runs == Set::ALL {
                diag = diag.with_help(format!(
                    "if the package supports only `{target}`, declare `targets = \
                     [\"{target}\"]` in `cheby.toml`"
                ));
            }
        }
        Ok(diag)
    }

    /// E0071 for an `Int` literal outside the JS safe-integer range in a
    /// body that runs on JS (§3.3.4, D-037).
    fn js_literals(
        &self,
        index: DefIndex,
        body: &thir::Body,
        diags: &mut Vec<Diagnostic>,
    ) -> Result<(), Ice> {
        let runs = self.runs(index)?;
        if !runs.js {
            return Ok(());
        }
        let user = self.input.globals.names.get(self.item_def(index)?.name);
        let kind = self.item_def(index)?.kind;
        let mut found: Vec<(i128, Span)> = Vec::new();
        for expr in &body.exprs {
            if let ExprKind::Lit(Lit::Int(value)) = &expr.kind
                && expr.ty == Ty::INT
            {
                found.push((*value, expr.span));
            }
        }
        for (_, pat) in body.pats() {
            pat.walk(&mut |p| {
                if let PatKind::Lit(Lit::Int(value)) = &p.kind
                    && p.ty == Ty::INT
                {
                    found.push((*value, p.span));
                }
            });
        }
        found.sort_by_key(|&(_, span)| span_key(span));
        for (value, span) in found {
            if (-JS_SAFE_MAX..=JS_SAFE_MAX).contains(&value) {
                continue;
            }
            let mut diag = Diagnostic::new(
                codes::LITERAL_OUT_OF_RANGE,
                "literal out of range for `Int` on the `js` target",
            )
            .with_primary(span, format!("`{value}` does not fit in `Int` on JS"))
            .with_note(format!(
                "on JS, `Int` holds -{JS_SAFE_MAX} to {JS_SAFE_MAX}, the safe-integer range"
            ));
            let help = if runs.native {
                let who = if kind == DefKind::Test {
                    format!("the test \"{user}\"")
                } else {
                    format!("`{user}`")
                };
                format!(
                    "use an `I64`, which holds every 64-bit value on both targets, or restrict \
                     {who} to `native` with `@target(native)`"
                )
            } else {
                "use an `I64`, which holds every 64-bit value on both targets".to_owned()
            };
            diag = diag.with_help(help);
            diags.push(diag);
        }
        Ok(())
    }
}

/// The span of a path's text, without trivia.
fn path_span(node: cheby_syntax::SyntaxNode<'_>) -> Span {
    PathNode::cast(node).map_or_else(|| node.span(), AstNode::significant_span)
}

/// An ordering key for a span: file, start, end.
fn span_key(span: Span) -> (u32, u32, u32) {
    (span.file.index(), span.range.start(), span.range.end())
}

/// A set of targets (§12.1), as this pass computes with them.
/// `cheby_package::Targets` is not a dependency of this crate outside
/// tests, so item targets are read through their names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Set {
    native: bool,
    js: bool,
}

impl Set {
    const ALL: Self = Self {
        native: true,
        js: true,
    };

    /// The set of the given targets, by their names: `native` and `js`.
    fn of<T: fmt::Display>(targets: impl IntoIterator<Item = T>) -> Self {
        targets.into_iter().fold(
            Self {
                native: false,
                js: false,
            },
            |acc, target| {
                let name = target.to_string();
                Self {
                    native: acc.native || name == "native",
                    js: acc.js || name == "js",
                }
            },
        )
    }

    const fn intersection(self, other: Self) -> Self {
        Self {
            native: self.native && other.native,
            js: self.js && other.js,
        }
    }

    /// The targets of `self` that are not in `other`.
    const fn minus(self, other: Self) -> Self {
        Self {
            native: self.native && !other.native,
            js: self.js && !other.js,
        }
    }

    const fn is_empty(self) -> bool {
        !self.native && !self.js
    }

    fn names(self) -> impl Iterator<Item = &'static str> {
        [(self.native, "native"), (self.js, "js")]
            .into_iter()
            .filter_map(|(on, name)| on.then_some(name))
    }

    /// The only target of the set, if it has exactly one.
    fn single(self) -> Option<&'static str> {
        let mut names = self.names();
        let first = names.next()?;
        names.next().is_none().then_some(first)
    }

    /// "`native`", "`js`" or "`native` and `js`".
    fn list(self) -> String {
        let names: Vec<String> = self.names().map(|t| format!("`{t}`")).collect();
        names.join(" and ")
    }
}

const fn external_target(external: &cheby_resolve::ExternalAttr) -> &'static str {
    match external {
        cheby_resolve::ExternalAttr::Native { .. } => "native",
        cheby_resolve::ExternalAttr::Js { .. } => "js",
    }
}

#[cfg(test)]
mod tests {
    //! The cross-target check over packages built for both targets (the
    //! test builds' default). Native-only packages (D-190) are covered by
    //! the UI tests, which carry a `cheby.toml`.

    use crate::test_support::{Build, build};

    /// The errors, as `CODE: message [labels]`, sorted; warnings left out.
    fn errors(b: &Build) -> Vec<String> {
        b.messages()
            .into_iter()
            .filter(|m| m.starts_with('E'))
            .collect()
    }

    const JS_RANDOM: &str =
        "@target(js)\n@external(js, \"./random_ffi.mjs\", \"random\")\nfn js_random() -> Float\n";

    #[test]
    fn a_js_only_function_used_on_both_targets() {
        let b = build(&[(
            "",
            &format!("{JS_RANDOM}\npub fn roll() -> Float {{\n  js_random()\n}}\n"),
        )]);
        assert_eq!(
            errors(&b),
            [
                "E0067: `js_random` exists only on `js`, but `roll` also runs on `native` [\"not available on `native`\", \"`js_random` is declared here\"]"
            ]
        );
        assert!(
            b.rendered()
                .contains("help: restrict `roll` to `js` with `@target(js)`, or the whole module with `@!target(js)`"),
            "{}",
            b.rendered()
        );
    }

    #[test]
    fn bodiless_functions_exist_on_their_externals() {
        // Without `@target`, a function with only a native external
        // exists only on native (§12.5).
        let b = build(&[(
            "",
            "@external(native, \"now\")\nfn now() -> Int\n\npub fn later() -> Int {\n  now()\n}\n\n@target(native)\npub fn sooner() -> Int {\n  now()\n}\n",
        )]);
        assert_eq!(
            errors(&b),
            [
                "E0067: `now` exists only on `native`, but `later` also runs on `js` [\"not available on `js`\", \"`now` is declared here\"]"
            ]
        );
    }

    #[test]
    fn a_partly_external_body_runs_on_the_other_targets() {
        // The §12.6 example (D-207): the body runs only on JS, so it may
        // call the JS-only function; a native-only one is an error there.
        let b = build(&[(
            "",
            &format!(
                "{JS_RANDOM}\n@external(native, \"now\")\nfn now() -> Int\n\n@external(native, \"arc4random_uniform\")\npub fn below(bound: Int) -> Float {{\n  js_random()\n}}\n\n@external(native, \"twice\")\npub fn twice() -> Int {{\n  now()\n}}\n"
            ),
        )]);
        assert_eq!(
            errors(&b),
            [
                "E0067: `now` exists only on `native`, but the body of `twice` runs on `js` [\"not available on `js`\", \"`now` is declared here\"]"
            ]
        );
    }

    #[test]
    fn signatures_name_types_of_their_targets() {
        let b = build(&[(
            "",
            "@target(native)\npub type Handle {\n  Handle(Int),\n}\n\n@target(native)\npub type Alias = Int\n\npub fn open(h: Handle) -> Handle {\n  h\n}\n\npub fn size(a: Alias) -> Int {\n  a\n}\n\n@target(native)\npub fn close(h: Handle) -> Int {\n  let Handle(n) = h\n  n\n}\n\npub type Wrapper {\n  Wrapper(Handle),\n}\n",
        )]);
        assert_eq!(
            errors(&b),
            [
                "E0067: `Alias` exists only on `native`, but `size` also exists on `js` [\"not available on `js`\", \"`Alias` is declared here\"]",
                "E0067: `Handle` exists only on `native`, but `Wrapper` also exists on `js` [\"not available on `js`\", \"`Handle` is declared here\"]",
                "E0067: `Handle` exists only on `native`, but `open` also exists on `js` [\"not available on `js`\", \"`Handle` is declared here\"]",
            ]
        );
    }

    #[test]
    fn constructors_patterns_constants_and_interfaces() {
        let b = build(&[(
            "",
            "@target(js)\npub type Shape {\n  Dot,\n  Box { side: Int },\n}\n\n@target(js)\nconst limit: Int = 3\n\n@target(js)\npub interface Named {\n  fn name(Self) -> String\n}\n\npub fn dot() -> Int {\n  let _ = Dot\n  limit\n}\n\npub fn measure(s: Int) -> Int {\n  let b = Box { side: s }\n  case b {\n    Box { side } => side\n    _ => 0\n  }\n}\n\npub fn named<T: Named>(x: T) -> String {\n  Named::name(x)\n}\n",
        )]);
        let errs = errors(&b);
        let short: Vec<&str> = errs.iter().map(|m| m.split(" [").next().unwrap()).collect();
        assert_eq!(
            short,
            [
                "E0067: `Box` exists only on `js`, but `measure` also runs on `native`",
                "E0067: `Dot` exists only on `js`, but `dot` also runs on `native`",
                "E0067: `Named` exists only on `js`, but `named` also exists on `native`",
                "E0067: `limit` exists only on `js`, but `dot` also runs on `native`",
            ]
        );
        // The body of `named` calls `Named::name`, but the interface is
        // already reported in its signature: one root cause, one error.
        let b = build(&[(
            "",
            "@target(js)\npub interface Named {\n  fn name(Self) -> String\n}\n\npub type Point {\n  Point(Int),\n}\n\npub fn name(p: Point) -> String {\n  \"point\"\n}\n\npub fn describe() -> String {\n  Named::name(Point(1))\n}\n",
        )]);
        let errs = errors(&b);
        let short: Vec<&str> = errs.iter().map(|m| m.split(" [").next().unwrap()).collect();
        assert_eq!(
            short,
            ["E0067: `name` exists only on `js`, but `describe` also runs on `native`"]
        );
    }

    #[test]
    fn each_item_is_reported_once_per_body() {
        let b = build(&[(
            "",
            &format!(
                "{JS_RANDOM}\npub fn roll() -> Float {{\n  let a = js_random()\n  let b = js_random()\n  a + b\n}}\n"
            ),
        )]);
        assert_eq!(b.codes().iter().filter(|c| *c == "E0067").count(), 1);
        // The first use is the one reported.
        assert!(
            b.rendered().contains("let a = js_random()\n  |"),
            "{}",
            b.rendered()
        );
    }

    #[test]
    fn modules_restricted_to_one_target() {
        // D-141: every item of a module with `@!target(native)` exists only
        // on native, so a module for both targets cannot use it, and one
        // restricted the same way can.
        let platform = (
            "platform",
            "//! Native only.\n\n@!target(native)\n\npub fn name() -> String {\n  \"native\"\n}\n",
        );
        let b = build(&[
            (
                "",
                "import app::platform\n\npub fn describe() -> String {\n  platform::name()\n}\n",
            ),
            platform,
        ]);
        assert_eq!(
            errors(&b),
            [
                "E0067: `name` exists only on `native`, but `describe` also runs on `js` [\"not available on `js`\", \"`name` is declared here\"]"
            ]
        );
        let b = build(&[
            (
                "",
                "import app::platform\n\n@target(native)\npub fn describe() -> String {\n  platform::name()\n}\n",
            ),
            platform,
        ]);
        assert_eq!(errors(&b), Vec::<String>::new());
    }

    #[test]
    fn implicit_uses_through_dictionaries() {
        // `Show` for `Point` comes from a native-only `show`: interpolating
        // a `Point` uses it.
        let b = build(&[(
            "",
            "pub type Point {\n  Point(Int),\n}\n\n@target(native)\npub fn show(p: Point) -> String {\n  \"point\"\n}\n\npub fn describe(p: Point) -> String {\n  \"{p}\"\n}\n",
        )]);
        let errs = errors(&b);
        assert!(
            errs.iter().any(|m| m.starts_with(
                "E0067: `show` exists only on `native`, but `describe` also runs on `js`"
            )),
            "{errs:?}"
        );
    }

    #[test]
    fn int_literals_outside_the_js_safe_range() {
        let b = build(&[(
            "",
            "pub fn edge() -> Int {\n  9007199254740991\n}\n\npub fn low() -> Int {\n  -9007199254740991\n}\n\npub fn over() -> Int {\n  9007199254740992\n}\n\npub fn under() -> Int {\n  -9007199254740992\n}\n\n@target(native)\npub fn native() -> Int {\n  9007199254740992\n}\n\npub fn wide() -> I64 {\n  9007199254740992\n}\n\nconst big: Int = 9007199254740993\n\n@external(native, \"ext\")\npub fn ext() -> Int {\n  9007199254740992\n}\n\npub fn pattern(n: Int) -> Int {\n  case n {\n    9007199254740992 => 1\n    _ => 0\n  }\n}\n",
        )]);
        let errs = errors(&b);
        let short: Vec<&str> = errs.iter().map(|m| m.split(" [").nth(1).unwrap()).collect();
        assert_eq!(
            short,
            [
                "\"`-9007199254740992` does not fit in `Int` on JS\"]",
                "\"`9007199254740992` does not fit in `Int` on JS\"]",
                "\"`9007199254740992` does not fit in `Int` on JS\"]",
                "\"`9007199254740992` does not fit in `Int` on JS\"]",
            ]
        );
        assert!(
            errs.iter()
                .all(|m| m.starts_with("E0071: literal out of range for `Int` on the `js` target"))
        );
        // The body of `ext` runs only on JS, so the help does not suggest
        // native.
        let rendered = b.rendered();
        assert!(
            rendered
                .contains("help: use an `I64`, which holds every 64-bit value on both targets\n"),
            "{rendered}"
        );
    }
}
