//! Typed generation of a Cheby package (D-718).
//!
//! Every expression is built for a type that is known when it is chosen,
//! so a program checks by construction. Termination is built in too:
//! a function calls only functions generated before it, recursion goes
//! through a counter that call sites set from a small literal or through
//! a list that gets shorter, and every list a program builds is cut to
//! at most [`MAX_LIST`] items and every built string to [`MAX_TEXT`]
//! characters. A cost estimate per function keeps the work of one call
//! under [`FN_BUDGET`].
//!
//! Integer overflow and division by zero may happen anywhere in a
//! function; `main` calls each function inside `fiber::scope`, which
//! turns such a panic into an `Err` that it prints. Only the planted
//! final panic ends the program with status 101.
//!
//! The program follows AGENTS.md's rules for Cheby: no `if` or loops,
//! no shadowing (every binding has a fresh name), explicit pipe holes,
//! and full signatures on top-level functions. A binding that nothing
//! reads is written with a leading `_`, so `cheby check` warns about
//! nothing.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use super::GenConfig;
use super::model::{AdtDef, FnDef, RecDef, Shape, Ty, Types, VariantKind, Vis};
use super::source::Source;

/// The largest counter literal a call site passes.
pub(crate) const MAX_COUNTER: usize = 6;
/// The most items any list of a program holds.
pub(crate) const MAX_LIST: usize = 8;
/// [`MAX_LIST`] as a cost factor.
const LIST_TIMES: u64 = 8;
/// [`MAX_COUNTER`] plus 1, as a cost factor: the most times a counted
/// recursion runs its body.
const COUNTER_TIMES: u64 = 7;
/// The most characters any string a program builds holds.
const MAX_TEXT: usize = 40;
/// The most work one call of a function may do, in body evaluations.
const FN_BUDGET: u64 = 6_000;
/// The text of the planted panic.
pub(crate) const PLANTED_TEXT: &str = "planted panic";

const WORDS: &[&str] = &[
    "a", "bc", "red", "tree", "x y", "", "seven", "Zed", "ok", "list", "-", "k=v",
];
const KEYS: &[&str] = &["a", "b", "c", "d", "e", "f"];

/// Starts and ends of a binder's placeholder in the text; resolved to
/// the name, or the name with a leading `_` if nothing reads it.
const OPEN: char = '\u{1}';
const CLOSE: char = '\u{2}';

/// `weight` if `on`, else 0: a choice that is possible only sometimes.
fn weight(on: bool, weight: usize) -> usize {
    if on { weight } else { 0 }
}

/// A `case` subject: parenthesized when it holds a `{`, which would
/// otherwise open the arms (§5.9.6).
fn subject_text(text: String) -> String {
    if text.contains('{') {
        format!("({text})")
    } else {
        text
    }
}

#[derive(Debug, Clone, Copy)]
struct Ctx {
    /// The expected type is known from the context, so `[]` and `None`
    /// may be written.
    known: bool,
    /// `case` and blocks may be written (a statement position, an arm or
    /// a closure body).
    multi: bool,
}

const INLINE: Ctx = Ctx {
    known: true,
    multi: false,
};
const INLINE_UNKNOWN: Ctx = Ctx {
    known: false,
    multi: false,
};

#[derive(Debug, Clone)]
struct Binder {
    name: String,
    used: bool,
}

#[derive(Debug, Clone)]
struct Var {
    name: String,
    binder: usize,
    ty: Ty,
    /// For a closure: the cost of one call.
    cost: u64,
}

/// Which module is being generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Module {
    Main,
    Helpers,
}

pub(crate) struct Gen<'c> {
    src: Source,
    cfg: &'c GenConfig,
    pub(crate) types: Types,
    fns: Vec<FnDef>,
    binders: Vec<Binder>,
    env: Vec<Var>,
    indent: usize,
    module: Module,
    imports: BTreeSet<&'static str>,
    fresh: usize,
    /// The work of the body being generated so far, its multiplier (the
    /// iterations around the current position) and its limit.
    cost: u64,
    mult: u64,
    budget: u64,
    /// No calls, no `*`, no large literals, no computed divisors and no
    /// assertions: values of `main` that must not panic.
    safe: bool,
    /// The function whose body is being generated.
    current: Option<usize>,
}

/// The generated text of a package.
pub(crate) struct Output {
    pub(crate) main: String,
    pub(crate) helpers: Option<String>,
    pub(crate) planted: bool,
}

impl<'c> Gen<'c> {
    pub(crate) fn new(src: Source, cfg: &'c GenConfig) -> Self {
        Self {
            src,
            cfg,
            types: Types::default(),
            fns: Vec::new(),
            binders: Vec::new(),
            env: Vec::new(),
            indent: 0,
            module: Module::Main,
            imports: BTreeSet::new(),
            fresh: 0,
            cost: 0,
            mult: 1,
            budget: u64::MAX,
            safe: false,
            current: None,
        }
    }

    // ---- Names, binders and the environment -------------------------

    fn fresh_name(&mut self, base: &str) -> String {
        self.fresh = self.fresh.saturating_add(1);
        format!("{base}{}", self.fresh)
    }

    /// A new binder named `base<n>`: its placeholder text and its index.
    fn binder(&mut self, base: &str) -> (String, usize) {
        let name = self.fresh_name(base);
        let id = self.binders.len();
        self.binders.push(Binder { name, used: false });
        (format!("{OPEN}{id}{CLOSE}"), id)
    }

    /// Binds a new local of type `ty`; returns its placeholder.
    fn bind(&mut self, base: &str, ty: Ty) -> String {
        let (text, id) = self.binder(base);
        let name = self
            .binders
            .get(id)
            .map(|b| b.name.clone())
            .unwrap_or_default();
        self.env.push(Var {
            name,
            binder: id,
            ty,
            cost: 0,
        });
        text
    }

    fn use_var(&mut self, index: usize) -> String {
        let Some(var) = self.env.get(index) else {
            return String::new();
        };
        let name = var.name.clone();
        let binder = var.binder;
        if let Some(b) = self.binders.get_mut(binder) {
            b.used = true;
        }
        name
    }

    /// A variable or a field path of a record variable of type `ty`.
    fn pick_path(&mut self, ty: &Ty) -> Option<String> {
        let mut found: Vec<(usize, String)> = Vec::new();
        for (index, var) in self.env.iter().enumerate() {
            let mut paths = Vec::new();
            self.paths_of(&var.ty, "", ty, 2, &mut paths);
            found.extend(paths.into_iter().map(|p| (index, p)));
        }
        let (index, path) = self.src.pick(&found)?.clone();
        let name = self.use_var(index);
        Some(format!("{name}{path}"))
    }

    /// The field paths (`""`, `".k0"`, `".k1.k0"`) of a value of type
    /// `from` that have type `want`.
    fn paths_of(&self, from: &Ty, prefix: &str, want: &Ty, depth: u32, out: &mut Vec<String>) {
        if from == want {
            out.push(prefix.to_owned());
        }
        if depth == 0 {
            return;
        }
        if let Ty::Rec(index) = from
            && let Some(rec) = self.types.recs.get(*index)
        {
            for (field, ty) in &rec.fields {
                let path = format!("{prefix}.{field}");
                self.paths_of(ty, &path, want, depth.saturating_sub(1), out);
            }
        }
    }

    fn import(&mut self, module: &'static str) -> &'static str {
        self.imports.insert(module);
        module
    }

    fn ind(&self) -> String {
        "  ".repeat(self.indent)
    }

    fn charge(&mut self, cost: u64) -> bool {
        let add = cost.saturating_mul(self.mult);
        let total = self.cost.saturating_add(add);
        if total > self.budget {
            return false;
        }
        self.cost = total;
        true
    }

    fn affordable(&self, cost: u64) -> bool {
        self.cost.saturating_add(cost.saturating_mul(self.mult)) <= self.budget
    }

    fn with_mult<T>(&mut self, factor: u64, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = self.mult;
        self.mult = self.mult.saturating_mul(factor);
        let out = f(self);
        self.mult = saved;
        out
    }

    fn scoped<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let mark = self.env.len();
        let out = f(self);
        self.env.truncate(mark);
        out
    }

    fn indented<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.indent = self.indent.saturating_add(1);
        let out = f(self);
        self.indent = self.indent.saturating_sub(1);
        out
    }

    // ---- Types --------------------------------------------------------

    /// A type for a value: any type of the main module, or only builtin
    /// types.
    fn random_type(&mut self, builtin_only: bool) -> Ty {
        let recs = self.types.recs.len();
        let adts = self.types.adts.len();
        let user = !builtin_only && self.module == Module::Main;
        let weights = [
            6,
            3,
            1,
            3,
            1,
            usize::from(user && recs > 0),
            weight(user && recs > 0, 3),
            weight(user && adts > 0, 3),
            1,
            1,
        ];
        match self.src.weighted(&weights) {
            1 => Ty::Str,
            2 => Ty::Bool,
            3 => Ty::list(Ty::Int),
            4 => Ty::list(Ty::Str),
            5 => Ty::list(Ty::Rec(self.src.below(recs))),
            6 => Ty::Rec(self.src.below(recs)),
            7 => Ty::Adt(self.src.below(adts)),
            8 => Ty::Map,
            9 => Ty::Opt,
            _ => Ty::Int,
        }
    }

    /// A field type of record `index` or sum type `adt` being declared.
    fn field_type(&mut self, recs: usize, adts: usize, own_adt: Option<usize>) -> Ty {
        let weights = [
            5,
            3,
            1,
            2,
            1,
            weight(recs > 0, 2),
            usize::from(adts > 0),
            usize::from(own_adt.is_some()),
        ];
        match self.src.weighted(&weights) {
            1 => Ty::Str,
            2 => Ty::Bool,
            3 => Ty::list(Ty::Int),
            4 => Ty::list(Ty::Str),
            5 => Ty::Rec(self.src.below(recs)),
            6 => Ty::Adt(self.src.below(adts)),
            7 => own_adt.map_or(Ty::Int, Ty::Adt),
            _ => Ty::Int,
        }
    }

    pub(crate) fn declare_types(&mut self) {
        let records = self.src.range(1, self.cfg.records.max(1));
        for index in 0..records {
            let count = self.src.range(1, 4);
            let fields = (0..count)
                .map(|f| (format!("k{f}"), self.field_type(index, 0, None)))
                .collect();
            self.types.recs.push(RecDef { fields });
        }
        let sums = self.src.range(1, self.cfg.sums.max(1));
        for index in 0..sums {
            let count = self.src.range(2, 4);
            let mut variants = Vec::new();
            for v in 0..count {
                let own = (v > 0 && self.src.chance(25)).then_some(index);
                let kind = match self.src.below(3) {
                    _ if v == 0 && self.src.chance(40) => VariantKind::Unit,
                    0 => VariantKind::Unit,
                    1 => {
                        let n = self.src.range(1, 3);
                        VariantKind::Positional(
                            (0..n)
                                .map(|_| self.field_type(records, index, own))
                                .collect(),
                        )
                    }
                    _ => {
                        let n = self.src.range(1, 3);
                        VariantKind::Named(
                            (0..n)
                                .map(|f| (format!("k{f}"), self.field_type(records, index, own)))
                                .collect(),
                        )
                    }
                };
                variants.push(kind);
            }
            self.types.adts.push(AdtDef { variants });
        }
    }

    fn render_types(&self) -> String {
        let mut out = String::new();
        for (index, rec) in self.types.recs.iter().enumerate() {
            let _ = writeln!(out, "type {} {{", self.types.rec_name(index));
            for (name, ty) in &rec.fields {
                let _ = writeln!(out, "  {name}: {},", ty.render(&self.types));
            }
            out.push_str("}\n\n");
        }
        for (index, adt) in self.types.adts.iter().enumerate() {
            let _ = writeln!(out, "type {} {{", self.types.adt_name(index));
            for (v, kind) in adt.variants.iter().enumerate() {
                let name = Types::variant_name(index, v);
                let line = match kind {
                    VariantKind::Unit => name,
                    VariantKind::Positional(tys) => {
                        let list: Vec<String> = tys.iter().map(|t| t.render(&self.types)).collect();
                        format!("{name}({})", list.join(", "))
                    }
                    VariantKind::Named(fields) => {
                        let list: Vec<String> = fields
                            .iter()
                            .map(|(f, t)| format!("{f}: {}", t.render(&self.types)))
                            .collect();
                        format!("{name} {{ {} }}", list.join(", "))
                    }
                };
                let _ = writeln!(out, "  {line},");
            }
            out.push_str("}\n\n");
        }
        out
    }

    // ---- Literals and leaves -----------------------------------------

    fn int_literal(&mut self) -> String {
        if !self.safe && self.src.chance(4) {
            // Large, but in the JS target's safe range (E0071): their
            // products overflow.
            let big = ["3_037_000_500", "9_007_199_254_740_991"];
            return (*self.src.pick(&big).unwrap_or(&"1")).to_owned();
        }
        match self.src.weighted(&[8, 2, 2]) {
            1 => format!("-{}", self.src.range(1, 9)),
            2 => format!("{}", self.src.range(10, 999)),
            _ => format!("{}", self.src.below(10)),
        }
    }

    fn word(&mut self) -> String {
        format!("\"{}\"", self.src.pick(WORDS).copied().unwrap_or(""))
    }

    fn key(&mut self) -> String {
        format!("\"{}\"", self.src.pick(KEYS).copied().unwrap_or("a"))
    }

    /// A value of `ty` with no operator, call or `case`.
    fn leaf(&mut self, ty: &Ty, ctx: Ctx) -> String {
        if self.src.chance(60)
            && let Some(path) = self.pick_path(ty)
        {
            return path;
        }
        match ty {
            Ty::Int => self.int_literal(),
            Ty::Bool => (if self.src.chance(50) { "True" } else { "False" }).to_owned(),
            Ty::Str => self.word(),
            Ty::List(item) => {
                if ctx.known && self.src.chance(30) {
                    "[]".to_owned()
                } else {
                    let first = self.leaf(
                        item,
                        Ctx {
                            known: ctx.known,
                            multi: false,
                        },
                    );
                    format!("[{first}]")
                }
            }
            Ty::Rec(index) => self.record_literal(*index, 0),
            Ty::Adt(index) => self.variant_value(*index, 0, None),
            Ty::Map => {
                let m = self.import("map");
                let key = self.key();
                let value = self.int_literal();
                format!("{m}::from_list([({key}, {value})])")
            }
            Ty::Opt => {
                if ctx.known && self.src.chance(40) {
                    "None".to_owned()
                } else {
                    let value = self.int_literal();
                    format!("Some({value})")
                }
            }
            Ty::Fun(param, result) => {
                let p = param.render(&self.types);
                let body = self.leaf(result, INLINE_UNKNOWN);
                format!("fn(_: {p}) {{ {body} }}")
            }
        }
    }

    fn record_literal(&mut self, index: usize, depth: u32) -> String {
        let fields = self
            .types
            .recs
            .get(index)
            .map(|r| r.fields.clone())
            .unwrap_or_default();
        let inits: Vec<String> = fields
            .iter()
            .map(|(name, ty)| {
                let value = self.sub_expr(ty, depth, INLINE);
                format!("{name}: {value}")
            })
            .collect();
        format!("{} {{ {} }}", self.types.rec_name(index), inits.join(", "))
    }

    /// A value of sum type `adt`: variant `forced` if given, or a random
    /// one (a variant that recurses only when `depth` allows it).
    fn variant_value(&mut self, adt: usize, depth: u32, forced: Option<usize>) -> String {
        let variants = self
            .types
            .adts
            .get(adt)
            .map(|a| a.variants.clone())
            .unwrap_or_default();
        let mut choice = forced.unwrap_or_else(|| self.src.below(variants.len()));
        let kind = variants.get(choice).cloned().unwrap_or(VariantKind::Unit);
        let recursive = match &kind {
            VariantKind::Unit => false,
            VariantKind::Positional(tys) => tys.iter().any(|t| self.types.mentions(t, adt)),
            VariantKind::Named(fields) => fields.iter().any(|(_, t)| self.types.mentions(t, adt)),
        };
        let kind = if recursive && depth == 0 && forced.is_none() {
            choice = 0;
            variants.first().cloned().unwrap_or(VariantKind::Unit)
        } else {
            kind
        };
        let name = Types::variant_name(adt, choice);
        match kind {
            VariantKind::Unit => name,
            VariantKind::Positional(tys) => {
                let args: Vec<String> = tys
                    .iter()
                    .map(|t| self.sub_expr(t, depth, INLINE))
                    .collect();
                format!("{name}({})", args.join(", "))
            }
            VariantKind::Named(fields) => {
                let inits: Vec<String> = fields
                    .iter()
                    .map(|(f, t)| {
                        let value = self.sub_expr(t, depth, INLINE);
                        format!("{f}: {value}")
                    })
                    .collect();
                format!("{name} {{ {} }}", inits.join(", "))
            }
        }
    }

    /// An expression one level below `depth` (a leaf at depth 0).
    fn sub_expr(&mut self, ty: &Ty, depth: u32, ctx: Ctx) -> String {
        if depth == 0 {
            self.leaf(ty, ctx)
        } else {
            self.expr(ty, depth.saturating_sub(1), ctx)
        }
    }

    // ---- Expressions ---------------------------------------------------

    fn expr(&mut self, ty: &Ty, depth: u32, ctx: Ctx) -> String {
        if depth == 0 {
            return self.leaf(ty, ctx);
        }
        if ctx.multi && self.src.chance(15) {
            return self.case_expr(ty, depth, ctx);
        }
        if !self.safe
            && self.src.chance(22)
            && let Some(text) = self.call_returning(ty, depth)
        {
            return text;
        }
        if self.src.chance(6)
            && let Some(text) = self.closure_call(ty, depth)
        {
            return text;
        }
        match ty {
            Ty::Int => self.int_expr(depth, ctx),
            Ty::Bool => self.bool_expr(depth),
            Ty::Str => self.str_expr(depth),
            Ty::List(item) => self.list_expr(item, depth, ctx),
            Ty::Rec(index) => self.rec_expr(*index, depth),
            Ty::Adt(index) => self.variant_value(*index, depth, None),
            Ty::Map => self.map_expr(depth),
            Ty::Opt => self.opt_expr(depth, ctx),
            Ty::Fun(..) => self.leaf(ty, ctx),
        }
    }

    fn int_expr(&mut self, depth: u32, ctx: Ctx) -> String {
        let sub = depth.saturating_sub(1);
        match self.src.weighted(&[2, 6, 2, 2, 1, 2, 1, 1]) {
            1 => self.arith(sub),
            2 => {
                let item = self.random_type(false);
                let xs = self.expr(&Ty::list(item), sub, INLINE_UNKNOWN);
                let l = self.import("list");
                format!("{l}::length({xs})")
            }
            3 => {
                let s = self.expr(&Ty::Str, sub, INLINE);
                let m = self.import("string");
                format!("{m}::length({s})")
            }
            4 => {
                let m = self.expr(&Ty::Map, sub, INLINE);
                let map = self.import("map");
                format!("{map}::size({m})")
            }
            5 => self.fold(&Ty::Int, sub),
            6 => {
                let (a, b) = (
                    self.expr(&Ty::Int, sub, INLINE),
                    self.expr(&Ty::Int, sub, INLINE),
                );
                let int = self.import("int");
                let f = *self.src.pick(&["min", "max"]).unwrap_or(&"min");
                format!("{int}::{f}({a}, {b})")
            }
            7 => {
                let xs = self.expr(&Ty::list(Ty::Int), sub, INLINE_UNKNOWN);
                let l = self.import("list");
                format!("{l}::sum({xs}, 0)")
            }
            _ => self.leaf(&Ty::Int, ctx),
        }
    }

    fn arith(&mut self, sub: u32) -> String {
        let a = self.expr(&Ty::Int, sub, INLINE);
        let op = self.src.weighted(&[4, 3, 2, 1, 1]);
        match op {
            2 if !self.safe && self.src.chance(25) => {
                let b = self.expr(&Ty::Int, sub, INLINE);
                format!("({a} * {b})")
            }
            2 => format!("({a} * {})", self.src.range(2, 9)),
            3 | 4 => {
                let sym = if op == 3 { "/" } else { "%" };
                if !self.safe && self.src.chance(8) {
                    let b = self.expr(&Ty::Int, sub, INLINE);
                    format!("({a} {sym} {b})")
                } else {
                    let divisor = self.src.range(1, 9);
                    format!("({a} {sym} {divisor})")
                }
            }
            1 => {
                let b = self.expr(&Ty::Int, sub, INLINE);
                format!("({a} - {b})")
            }
            _ => {
                let b = self.expr(&Ty::Int, sub, INLINE);
                format!("({a} + {b})")
            }
        }
    }

    fn bool_expr(&mut self, depth: u32) -> String {
        let sub = depth.saturating_sub(1);
        match self.src.weighted(&[1, 5, 1, 1, 1, 1, 2, 1, 1]) {
            1 => {
                let op = *self
                    .src
                    .pick(&["<", "<=", "==", "!=", ">", ">="])
                    .unwrap_or(&"<");
                let a = self.expr(&Ty::Int, sub, INLINE);
                let b = self.expr(&Ty::Int, sub, INLINE);
                format!("{a} {op} {b}")
            }
            2 => {
                let a = self.expr(&Ty::Str, sub, INLINE);
                let b = self.word();
                format!("{a} == {b}")
            }
            3 => {
                let item = self.random_type(false);
                let xs = self.expr(&Ty::list(item), sub, INLINE_UNKNOWN);
                let l = self.import("list");
                format!("{l}::is_empty({xs})")
            }
            4 => {
                let xs = self.expr(&Ty::list(Ty::Int), sub, INLINE_UNKNOWN);
                let x = self.expr(&Ty::Int, sub, INLINE);
                let l = self.import("list");
                format!("{l}::contains({xs}, {x})")
            }
            5 => {
                let b = self.expr(&Ty::Bool, sub, INLINE);
                format!("!({b})")
            }
            6 => {
                let op = if self.src.chance(50) { "&&" } else { "||" };
                let a = self.expr(&Ty::Bool, sub, INLINE);
                let b = self.expr(&Ty::Bool, sub, INLINE);
                format!("({a}) {op} ({b})")
            }
            7 => {
                let ty = self.random_type(false);
                if !ty.comparable() {
                    return "True".to_owned();
                }
                let a = self.expr(&ty, sub, INLINE_UNKNOWN);
                let b = self.expr(&ty, sub, INLINE_UNKNOWN);
                // Comparisons do not chain, so `Bool` operands get
                // parentheses.
                if ty == Ty::Bool {
                    format!("({a}) == ({b})")
                } else {
                    format!("{a} == {b}")
                }
            }
            8 => {
                let s = self.expr(&Ty::Str, sub, INLINE);
                let w = self.word();
                let m = self.import("string");
                let f = *self
                    .src
                    .pick(&["starts_with", "contains", "ends_with"])
                    .unwrap_or(&"contains");
                format!("{m}::{f}({s}, {w})")
            }
            _ => self.leaf(&Ty::Bool, INLINE),
        }
    }

    fn clip_text(&mut self, text: &str) -> String {
        let m = self.import("string");
        format!("{m}::slice({text}, 0, {MAX_TEXT})")
    }

    fn str_expr(&mut self, depth: u32) -> String {
        let sub = depth.saturating_sub(1);
        match self.src.weighted(&[2, 6, 2, 1, 1, 1]) {
            1 => self.interpolation(),
            2 => {
                let a = self.expr(&Ty::Str, sub, INLINE);
                let b = self.expr(&Ty::Str, sub, INLINE);
                self.clip_text(&format!("{a} + {b}"))
            }
            3 => {
                let s = self.expr(&Ty::Str, sub, INLINE);
                let m = self.import("string");
                let f = *self
                    .src
                    .pick(&["uppercase", "reverse", "trim", "lowercase"])
                    .unwrap_or(&"trim");
                format!("{m}::{f}({s})")
            }
            4 => {
                let xs = self.expr(&Ty::list(Ty::Str), sub, INLINE);
                let m = self.import("string");
                let sep = self.word();
                self.clip_text(&format!("{m}::join({xs}, {sep})"))
            }
            5 => {
                let s = self.expr(&Ty::Str, sub, INLINE);
                let m = self.import("string");
                let times = self.src.range(0, 3);
                self.clip_text(&format!("{m}::repeat({s}, {times})"))
            }
            _ => self.leaf(&Ty::Str, INLINE),
        }
    }

    /// `"text {x} {r.k0} {v:?}"`, cut to [`MAX_TEXT`] when it holds a
    /// value.
    fn interpolation(&mut self) -> String {
        let mut refs: Vec<(usize, String)> = Vec::new();
        for (index, var) in self.env.iter().enumerate() {
            if var.ty.shows() {
                refs.push((index, String::new()));
            } else if var.ty.debug_prints(&self.types) {
                refs.push((index, ":?".to_owned()));
            }
            if let Ty::Rec(_) = var.ty {
                let mut paths = Vec::new();
                for want in [Ty::Int, Ty::Str, Ty::Bool, Ty::list(Ty::Int)] {
                    self.paths_of(&var.ty, "", &want, 2, &mut paths);
                }
                refs.extend(
                    paths
                        .into_iter()
                        .filter(|p| !p.is_empty())
                        .map(|p| (index, p)),
                );
            }
        }
        let parts = self.src.range(1, 3);
        let mut text = String::new();
        let mut holds_value = false;
        for _ in 0..parts {
            if !refs.is_empty() && self.src.chance(70) {
                let pick = self.src.pick(&refs).cloned();
                if let Some((index, suffix)) = pick {
                    let name = self.use_var(index);
                    let _ = write!(text, "{{{name}{suffix}}}");
                    holds_value = true;
                }
            } else {
                let word = self.src.pick(WORDS).copied().unwrap_or("");
                text.push_str(word);
            }
            if self.src.chance(50) {
                text.push(' ');
            }
        }
        let literal = format!("\"{text}\"");
        if holds_value {
            self.clip_text(&literal)
        } else {
            literal
        }
    }

    fn clip_list(&mut self, text: &str) -> String {
        let l = self.import("list");
        format!("{l}::take({text}, {MAX_LIST})")
    }

    fn list_expr(&mut self, item: &Ty, depth: u32, ctx: Ctx) -> String {
        let sub = depth.saturating_sub(1);
        let sortable = matches!(item, Ty::Int | Ty::Str);
        let weights = [
            2,
            3,
            3,
            2,
            2,
            2,
            usize::from(*item == Ty::Int),
            usize::from(*item == Ty::Str),
            usize::from(sortable),
            1,
        ];
        let l = self.import("list");
        match self.src.weighted(&weights) {
            1 => {
                let count = self.src.range(1, 4);
                let inner = Ctx {
                    known: ctx.known,
                    multi: false,
                };
                let items: Vec<String> = (0..count).map(|_| self.expr(item, sub, inner)).collect();
                format!("[{}]", items.join(", "))
            }
            2 => self.list_map(item, sub),
            3 => {
                let xs = self.expr(&Ty::list(item.clone()), sub, INLINE_UNKNOWN);
                let keep = self.closure(std::slice::from_ref(item), &Ty::Bool, sub, LIST_TIMES);
                format!("{l}::filter({xs}, {keep})")
            }
            4 => {
                let xs = self.expr(&Ty::list(item.clone()), sub, INLINE_UNKNOWN);
                let f = *self
                    .src
                    .pick(&["reverse", "drop", "take"])
                    .unwrap_or(&"reverse");
                if f == "reverse" {
                    format!("{l}::reverse({xs})")
                } else {
                    let n = self.src.range(0, 4);
                    format!("{l}::{f}({xs}, {n})")
                }
            }
            5 => {
                // Only the parts the text uses are generated: a part that
                // were dropped would still mark its variables as read.
                // `list::take` is generic, so nothing inside knows the type.
                let a = self.expr(&Ty::list(item.clone()), sub, INLINE_UNKNOWN);
                let text = match self.src.below(3) {
                    0 => {
                        let b = self.expr(item, sub, INLINE_UNKNOWN);
                        format!("[..{a}, {b}]")
                    }
                    1 => {
                        let b = self.expr(item, sub, INLINE_UNKNOWN);
                        format!("[{b}, ..{a}]")
                    }
                    _ => {
                        let c = self.expr(&Ty::list(item.clone()), sub, INLINE_UNKNOWN);
                        format!("[..{a}, ..{c}]")
                    }
                };
                self.clip_list(&text)
            }
            6 => {
                let n = self.src.range(0, MAX_LIST);
                let start = self.src.range(0, 3);
                format!("{l}::range({start}, {n})")
            }
            7 => {
                let s = self.expr(&Ty::Str, sub, INLINE);
                let m = self.import("string");
                let sep = *self
                    .src
                    .pick(&["\" \"", "\"e\"", "\"\""])
                    .unwrap_or(&"\" \"");
                self.clip_list(&format!("{m}::split({s}, {sep})"))
            }
            8 => {
                let xs = self.expr(&Ty::list(item.clone()), sub, INLINE_UNKNOWN);
                format!("{l}::sort({xs})")
            }
            9 => {
                let xs = self.expr(&Ty::list(item.clone()), sub, INLINE_UNKNOWN);
                format!("{l}::unique({xs})")
            }
            _ => self.leaf(&Ty::list(item.clone()), ctx),
        }
    }

    /// `list::map(xs, f)` with a closure, a closure local or a function
    /// passed as a value.
    fn list_map(&mut self, item: &Ty, sub: u32) -> String {
        let l = self.import("list");
        let from = match self.src.below(4) {
            0 => item.clone(),
            1 => Ty::Str,
            _ => Ty::Int,
        };
        if self.src.chance(15)
            && let Some(f) = self.function_value(&from, item)
        {
            let xs = self.expr(&Ty::list(from), sub, INLINE_UNKNOWN);
            return format!("{l}::map({xs}, {f})");
        }
        if self.src.chance(30)
            && let Some(g) = self.closure_var(&from, item)
        {
            let xs = self.expr(&Ty::list(from), sub, INLINE_UNKNOWN);
            return format!("{l}::map({xs}, {g})");
        }
        let xs = self.expr(&Ty::list(from.clone()), sub, INLINE_UNKNOWN);
        let f = self.closure(&[from], item, sub, LIST_TIMES);
        format!("{l}::map({xs}, {f})")
    }

    fn fold(&mut self, ty: &Ty, sub: u32) -> String {
        let l = self.import("list");
        let item = if self.src.chance(50) {
            Ty::Int
        } else {
            Ty::Str
        };
        let xs = self.expr(&Ty::list(item.clone()), sub, INLINE_UNKNOWN);
        let init = self.expr(ty, sub, INLINE_UNKNOWN);
        let f = self.closure(&[ty.clone(), item], ty, sub, LIST_TIMES);
        format!("{l}::fold({xs}, {init}, {f})")
    }

    fn rec_expr(&mut self, index: usize, depth: u32) -> String {
        let sub = depth.saturating_sub(1);
        match self.src.weighted(&[1, 3, 2]) {
            1 => self.record_literal(index, sub),
            2 => {
                let base = self.expr(&Ty::Rec(index), sub, INLINE);
                let fields = self
                    .types
                    .recs
                    .get(index)
                    .map(|r| r.fields.clone())
                    .unwrap_or_default();
                let Some((name, ty)) = self.src.pick(&fields).cloned() else {
                    return base;
                };
                let value = self.expr(&ty, sub, INLINE);
                format!(
                    "{} {{ ..{base}, {name}: {value} }}",
                    self.types.rec_name(index)
                )
            }
            _ => self.leaf(&Ty::Rec(index), INLINE),
        }
    }

    fn map_expr(&mut self, depth: u32) -> String {
        let sub = depth.saturating_sub(1);
        let m = self.import("map");
        match self.src.weighted(&[1, 3, 3, 1]) {
            1 => {
                let count = self.src.range(0, 3);
                if count == 0 {
                    return format!("{m}::new::<String, Int>()");
                }
                let pairs: Vec<String> = (0..count)
                    .map(|_| {
                        let key = self.key();
                        let value = self.expr(&Ty::Int, sub, INLINE);
                        format!("({key}, {value})")
                    })
                    .collect();
                format!("{m}::from_list([{}])", pairs.join(", "))
            }
            2 => {
                let base = self.expr(&Ty::Map, sub, INLINE);
                let key = self.key();
                let value = self.expr(&Ty::Int, sub, INLINE);
                format!("{m}::insert({base}, {key}, {value})")
            }
            3 => {
                let base = self.expr(&Ty::Map, sub, INLINE);
                let key = self.key();
                format!("{m}::remove({base}, {key})")
            }
            _ => self.leaf(&Ty::Map, INLINE),
        }
    }

    fn opt_expr(&mut self, depth: u32, ctx: Ctx) -> String {
        let sub = depth.saturating_sub(1);
        match self.src.weighted(&[1, 3, 1, 1]) {
            1 => {
                let base = self.expr(&Ty::Map, sub, INLINE);
                let key = self.key();
                let m = self.import("map");
                format!("{m}::get({base}, {key})")
            }
            2 => {
                let xs = self.expr(&Ty::list(Ty::Int), sub, INLINE_UNKNOWN);
                let l = self.import("list");
                format!("{l}::first({xs})")
            }
            3 => {
                let xs = self.expr(&Ty::list(Ty::Int), sub, INLINE_UNKNOWN);
                let l = self.import("list");
                let index = self.src.range(0, 3);
                format!("{l}::get({xs}, {index})")
            }
            _ => self.leaf(&Ty::Opt, ctx),
        }
    }

    // ---- Closures and calls ------------------------------------------

    /// A closure `fn(x, …) { body }` over `params` returning `ret`, whose
    /// body runs up to `times` times per evaluation of the enclosing
    /// expression.
    fn closure(&mut self, params: &[Ty], ret: &Ty, sub: u32, times: u64) -> String {
        self.with_mult(times, |g| {
            g.scoped(|g| {
                let annotate = g.src.chance(30);
                let list: Vec<String> = params
                    .iter()
                    .map(|ty| {
                        let binder = g.bind("x", ty.clone());
                        if annotate {
                            format!("{binder}: {}", ty.render(&g.types))
                        } else {
                            binder
                        }
                    })
                    .collect();
                let body = g.body(ret, sub, false);
                format!("fn({}) {body}", list.join(", "))
            })
        })
    }

    /// `{ e }` or a block with statements, at the current indentation.
    fn body(&mut self, ret: &Ty, sub: u32, known: bool) -> String {
        let stmts = if sub > 0 && self.src.chance(30) {
            self.src.range(1, 2)
        } else {
            0
        };
        if stmts == 0 {
            let e = self.indented(|g| g.expr(ret, sub, Ctx { known, multi: true }));
            if !e.contains('\n') {
                return format!("{{ {e} }}");
            }
            let ind = self.ind();
            return format!("{{\n{ind}  {e}\n{ind}}}");
        }
        self.block(ret, sub, known, stmts)
    }

    /// A block with `stmts` statements and a result of type `ret`.
    fn block(&mut self, ret: &Ty, sub: u32, known: bool, stmts: usize) -> String {
        let ind = self.ind();
        let inner = self.scoped(|g| {
            g.indented(|g| {
                let mut lines = Vec::new();
                for _ in 0..stmts {
                    lines.push(g.stmt(sub));
                }
                let ind = g.ind();
                let e = g.expr(ret, sub, Ctx { known, multi: true });
                lines.push(format!("{ind}{e}"));
                lines.join("\n")
            })
        });
        format!("{{\n{inner}\n{ind}}}")
    }

    /// A call of a closure local returning `ty`.
    fn closure_call(&mut self, ty: &Ty, depth: u32) -> Option<String> {
        let found: Vec<usize> = (0..self.env.len())
            .filter(|i| {
                self.env.get(*i).is_some_and(|v| {
                    matches!(&v.ty, Ty::Fun(_, r) if **r == *ty) && self.affordable(v.cost)
                })
            })
            .collect();
        let index = *self.src.pick(&found)?;
        let (param, cost) = match self.env.get(index) {
            Some(Var {
                ty: Ty::Fun(p, _),
                cost,
                ..
            }) => ((**p).clone(), *cost),
            _ => return None,
        };
        if !self.charge(cost) {
            return None;
        }
        let name = self.use_var(index);
        let arg = self.expr(&param, depth.saturating_sub(1), INLINE);
        Some(format!("{name}({arg})"))
    }

    /// A closure local of type `fn(from) -> to`, charged for
    /// [`MAX_LIST`] calls.
    fn closure_var(&mut self, from: &Ty, to: &Ty) -> Option<String> {
        let want = Ty::Fun(Box::new(from.clone()), Box::new(to.clone()));
        let found: Vec<usize> = (0..self.env.len())
            .filter(|i| self.env.get(*i).is_some_and(|v| v.ty == want))
            .collect();
        let index = *self.src.pick(&found)?;
        let cost = self.env.get(index).map_or(0, |v| v.cost);
        if !self.charge(cost.saturating_mul(LIST_TIMES)) {
            return None;
        }
        Some(self.use_var(index))
    }

    /// The functions the current position may call.
    fn callable(&self) -> Vec<usize> {
        // Only functions generated before the current one: no function
        // calls itself except through its shape's counted recursion.
        let end = self.current.unwrap_or(self.fns.len()).min(self.fns.len());
        (0..end)
            .filter(|i| {
                self.fns
                    .get(*i)
                    .is_some_and(|f| match (self.module, f.module) {
                        (Module::Helpers, Some(_)) | (Module::Main, None) => true,
                        (Module::Main, Some(_)) => f.vis != Vis::Priv,
                        (Module::Helpers, None) => false,
                    })
            })
            .collect()
    }

    fn qualified(&mut self, index: usize) -> String {
        let Some(f) = self.fns.get(index) else {
            return String::new();
        };
        let name = f.name.clone();
        match (f.module, self.module) {
            (Some(m), Module::Main) => {
                self.imports.insert("progen::helpers");
                format!("{m}::{name}")
            }
            _ => name,
        }
    }

    /// A call of a top-level function returning `ty` that fits the budget.
    fn call_returning(&mut self, ty: &Ty, depth: u32) -> Option<String> {
        let found: Vec<usize> = self
            .callable()
            .into_iter()
            .filter(|i| {
                self.fns
                    .get(*i)
                    .is_some_and(|f| f.ret == *ty && self.affordable(f.cost))
            })
            .collect();
        let index = *self.src.pick(&found)?;
        Some(self.call(index, depth))
    }

    /// A call of function `index`, with its counter set from a literal.
    fn call(&mut self, index: usize, depth: u32) -> String {
        let Some(f) = self.fns.get(index).cloned() else {
            return String::new();
        };
        let _ = self.charge(f.cost);
        if let Some(def) = self.fns.get_mut(index) {
            def.uses = def.uses.saturating_add(1);
        }
        let name = self.qualified(index);
        let args: Vec<String> = f
            .params
            .iter()
            .enumerate()
            .map(|(i, (_, ty))| {
                if f.counter == Some(i) {
                    self.counter_literal()
                } else {
                    self.sub_expr(ty, depth, INLINE)
                }
            })
            .collect();
        format!("{name}({})", args.join(", "))
    }

    fn counter_literal(&mut self) -> String {
        if self.src.chance(5) {
            "-1".to_owned()
        } else {
            format!("{}", self.src.range(0, MAX_COUNTER))
        }
    }

    /// A one-parameter function of type `fn(from) -> to`, passed as a
    /// value (which keeps it from borrowing, D-715).
    fn function_value(&mut self, from: &Ty, to: &Ty) -> Option<String> {
        let found: Vec<usize> = self
            .callable()
            .into_iter()
            .filter(|i| {
                self.fns.get(*i).is_some_and(|f| {
                    f.ret == *to
                        && f.counter.is_none()
                        && f.params.len() == 1
                        && f.params.first().is_some_and(|(_, t)| t == from)
                        && self.affordable(f.cost.saturating_mul(LIST_TIMES))
                })
            })
            .collect();
        let index = *self.src.pick(&found)?;
        let cost = self.fns.get(index).map_or(0, |f| f.cost);
        let _ = self.charge(cost.saturating_mul(LIST_TIMES));
        if let Some(def) = self.fns.get_mut(index) {
            def.uses = def.uses.saturating_add(1);
        }
        Some(self.qualified(index))
    }

    // ---- Case ----------------------------------------------------------

    fn case_expr(&mut self, ty: &Ty, depth: u32, ctx: Ctx) -> String {
        let sub = depth.saturating_sub(1);
        let adts = self.types.adts.len();
        let has_adt = self.module == Module::Main && adts > 0;
        let choice = self.src.weighted(&[3, 3, weight(has_adt, 4), 2, 2, 1, 1]);
        let ind = self.ind();
        let arm_ctx = Ctx {
            known: ctx.known,
            multi: true,
        };
        let (head, arms) = self.indented(|g| match choice {
            1 => g.int_case(ty, sub, arm_ctx),
            2 => g.adt_case(ty, sub, arm_ctx),
            3 => g.list_case(ty, sub, arm_ctx),
            4 => g.opt_case(ty, sub, arm_ctx),
            5 => g.two_subject_case(ty, sub, arm_ctx),
            6 => g.str_case(ty, sub, arm_ctx),
            _ => g.cond_case(ty, sub, arm_ctx),
        });
        let inner = self.indented(|g| {
            let ind = g.ind();
            arms.iter()
                .map(|a| format!("{ind}{a}"))
                .collect::<Vec<_>>()
                .join("\n")
        });
        format!("{head} {{\n{inner}\n{ind}}}")
    }

    /// An arm body (an expression or a block), with the arm's bindings
    /// in scope (the caller scopes them).
    fn arm(&mut self, ty: &Ty, sub: u32, ctx: Ctx) -> String {
        if sub > 0 && self.src.chance(15) {
            let stmts = self.src.range(1, 2);
            return self.block(ty, sub, ctx.known, stmts);
        }
        self.expr(ty, sub, ctx)
    }

    fn cond_case(&mut self, ty: &Ty, sub: u32, ctx: Ctx) -> (String, Vec<String>) {
        let mut arms = Vec::new();
        for _ in 0..self.src.range(1, 2) {
            let cond = self.expr(&Ty::Bool, sub, INLINE);
            let body = self.arm(ty, sub, ctx);
            arms.push(format!("{cond} => {body}"));
        }
        let body = self.arm(ty, sub, ctx);
        arms.push(format!("_ => {body}"));
        ("case".to_owned(), arms)
    }

    fn int_case(&mut self, ty: &Ty, sub: u32, ctx: Ctx) -> (String, Vec<String>) {
        let subject = self.expr(&Ty::Int, sub, INLINE);
        let mut arms = Vec::new();
        let lit = self.src.range(0, 3);
        let body = self.arm(ty, sub, ctx);
        arms.push(format!("{lit} => {body}"));
        if self.src.chance(40) {
            let (a, b) = (lit.saturating_add(1), lit.saturating_add(2));
            let body = self.arm(ty, sub, ctx);
            arms.push(format!("{a} | {b} => {body}"));
        }
        if self.src.chance(50) {
            let arm = self.scoped(|g| {
                let k = g.bind("k", Ty::Int);
                let name = g.env.last().map(|v| v.name.clone()).unwrap_or_default();
                if let Some(b) = g.env.last().and_then(|v| g.binders.get_mut(v.binder)) {
                    b.used = true;
                }
                let limit = g.src.range(0, 9);
                let body = g.arm(ty, sub, ctx);
                format!("{k} when {name} > {limit} => {body}")
            });
            arms.push(arm);
        }
        let body = self.arm(ty, sub, ctx);
        arms.push(format!("_ => {body}"));
        (format!("case {}", subject_text(subject)), arms)
    }

    fn str_case(&mut self, ty: &Ty, sub: u32, ctx: Ctx) -> (String, Vec<String>) {
        let subject = self.expr(&Ty::Str, sub, INLINE);
        let word = self.word();
        let first = self.arm(ty, sub, ctx);
        let rest = self.arm(ty, sub, ctx);
        (
            format!("case {}", subject_text(subject)),
            vec![format!("{word} => {first}"), format!("_ => {rest}")],
        )
    }

    fn two_subject_case(&mut self, ty: &Ty, sub: u32, ctx: Ctx) -> (String, Vec<String>) {
        let a = self.expr(&Ty::Int, sub, INLINE);
        let b = self.expr(&Ty::Bool, sub, INLINE);
        let mut arms = Vec::new();
        let body = self.arm(ty, sub, ctx);
        arms.push(format!("0, True => {body}"));
        let body = self.arm(ty, sub, ctx);
        arms.push(format!("_, False => {body}"));
        let body = self.arm(ty, sub, ctx);
        arms.push(format!("_, _ => {body}"));
        (
            format!("case {}, {}", subject_text(a), subject_text(b)),
            arms,
        )
    }

    fn list_case(&mut self, ty: &Ty, sub: u32, ctx: Ctx) -> (String, Vec<String>) {
        let item = if self.src.chance(60) {
            Ty::Int
        } else {
            Ty::Str
        };
        let subject = self.expr(&Ty::list(item.clone()), sub, INLINE_UNKNOWN);
        let mut arms = Vec::new();
        let body = self.arm(ty, sub, ctx);
        arms.push(format!("[] => {body}"));
        if self.src.chance(40) {
            let arm = self.scoped(|g| {
                let h = g.bind("h", item.clone());
                let body = g.arm(ty, sub, ctx);
                format!("[{h}] => {body}")
            });
            arms.push(arm);
        }
        let guarded = self.src.chance(40);
        let arm = self.scoped(|g| {
            let h = g.bind("h", item.clone());
            let t = g.bind("t", Ty::list(item.clone()));
            let guard = if guarded {
                let cond = g.expr(&Ty::Bool, sub, INLINE);
                format!(" when {cond}")
            } else {
                String::new()
            };
            let body = g.arm(ty, sub, ctx);
            format!("[{h}, ..{t}]{guard} => {body}")
        });
        arms.push(arm);
        if guarded {
            let body = self.arm(ty, sub, ctx);
            arms.push(format!("_ => {body}"));
        }
        (format!("case {}", subject_text(subject)), arms)
    }

    fn opt_case(&mut self, ty: &Ty, sub: u32, ctx: Ctx) -> (String, Vec<String>) {
        let subject = self.expr(&Ty::Opt, sub, INLINE_UNKNOWN);
        let mut arms = Vec::new();
        if self.src.chance(40) {
            let arm = self.scoped(|g| {
                let v = g.bind("v", Ty::Int);
                let name = g.env.last().map(|v| v.name.clone()).unwrap_or_default();
                if let Some(b) = g.env.last().and_then(|v| g.binders.get_mut(v.binder)) {
                    b.used = true;
                }
                let body = g.arm(ty, sub, ctx);
                format!("Some({v}) when {name} < 0 => {body}")
            });
            arms.push(arm);
        }
        let arm = self.scoped(|g| {
            let v = g.bind("v", Ty::Int);
            let body = g.arm(ty, sub, ctx);
            format!("Some({v}) => {body}")
        });
        arms.push(arm);
        let body = self.arm(ty, sub, ctx);
        arms.push(format!("None => {body}"));
        (format!("case {}", subject_text(subject)), arms)
    }

    fn adt_case(&mut self, ty: &Ty, sub: u32, ctx: Ctx) -> (String, Vec<String>) {
        let adt = self.src.below(self.types.adts.len());
        let subject = self.expr(&Ty::Adt(adt), sub, INLINE);
        let variants = self
            .types
            .adts
            .get(adt)
            .map(|a| a.variants.clone())
            .unwrap_or_default();
        let guard_at = self.src.below(variants.len());
        let fallback = self.src.chance(25) && variants.len() > 2;
        let mut arms = Vec::new();
        for (v, kind) in variants.iter().enumerate() {
            if fallback && v.saturating_add(1) == variants.len() {
                break;
            }
            let name = Types::variant_name(adt, v);
            let has_fields = !matches!(kind, VariantKind::Unit);
            if v == guard_at && has_fields {
                let arm = self.scoped(|g| {
                    let pattern = g.variant_pattern(&name, kind);
                    let cond = g.expr(&Ty::Bool, sub, INLINE);
                    let body = g.arm(ty, sub, ctx);
                    format!("{pattern} when {cond} => {body}")
                });
                arms.push(arm);
            }
            let arm = self.scoped(|g| {
                let pattern = g.variant_pattern(&name, kind);
                let body = g.arm(ty, sub, ctx);
                format!("{pattern} => {body}")
            });
            arms.push(arm);
        }
        if fallback {
            let body = self.arm(ty, sub, ctx);
            arms.push(format!("_ => {body}"));
        }
        (format!("case {}", subject_text(subject)), arms)
    }

    /// A pattern for one variant, binding some of its fields.
    fn variant_pattern(&mut self, name: &str, kind: &VariantKind) -> String {
        match kind {
            VariantKind::Unit => name.to_owned(),
            VariantKind::Positional(tys) => {
                let parts: Vec<String> = tys
                    .iter()
                    .map(|t| {
                        if self.src.chance(70) {
                            self.bind("f", t.clone())
                        } else {
                            "_".to_owned()
                        }
                    })
                    .collect();
                format!("{name}({})", parts.join(", "))
            }
            VariantKind::Named(fields) => {
                let mut parts = Vec::new();
                for (f, t) in fields {
                    if self.src.chance(70) {
                        let b = self.bind("f", t.clone());
                        parts.push(format!("{f}: {b}"));
                    }
                }
                parts.push("..".to_owned());
                format!("{name} {{ {} }}", parts.join(", "))
            }
        }
    }

    // ---- Statements ------------------------------------------------------

    /// One statement at the current indentation (possibly several lines).
    fn stmt(&mut self, sub: u32) -> String {
        let ind = self.ind();
        let safe = self.safe;
        let main = self.module == Module::Main && !self.types.recs.is_empty();
        let weights = [
            6,
            1,
            weight(sub > 0, 2),
            weight(sub > 0, 2),
            1,
            usize::from(main),
            usize::from(!safe),
            usize::from(!safe),
        ];
        match self.src.weighted(&weights) {
            1 => {
                let (ta, tb) = (self.random_type(false), self.random_type(false));
                let a = self.expr(&ta, sub, INLINE_UNKNOWN);
                let b = self.expr(&tb, sub, INLINE_UNKNOWN);
                let (ba, bb) = (self.bind("v", ta), self.bind("v", tb));
                format!("{ind}let ({ba}, {bb}) = ({a}, {b})")
            }
            2 => self.loop_fn(sub),
            3 => self.closure_let(sub),
            4 => {
                let text = self.expr(&Ty::Str, sub, INLINE);
                let io = self.import("io");
                format!("{ind}{io}::println({text})")
            }
            5 => {
                let index = self.src.below(self.types.recs.len());
                let value = self.expr(&Ty::Rec(index), sub, INLINE);
                let fields = self
                    .types
                    .recs
                    .get(index)
                    .map(|r| r.fields.clone())
                    .unwrap_or_default();
                let (field, ty) = self
                    .src
                    .pick(&fields)
                    .cloned()
                    .unwrap_or_else(|| ("k0".to_owned(), Ty::Int));
                let b = self.bind("v", ty);
                let rec = self.types.rec_name(index);
                format!("{ind}let {rec} {{ {field}: {b}, .. }} = {value}")
            }
            6 if self.src.chance(30) => {
                let xs = self.expr(&Ty::list(Ty::Int), sub, INLINE);
                let h = self.bind("h", Ty::Int);
                let t = self.bind("t", Ty::list(Ty::Int));
                format!("{ind}let assert [{h}, ..{t}] = {xs}")
            }
            7 if self.src.chance(30) => {
                let cond = self.expr(&Ty::Bool, sub, INLINE);
                format!("{ind}assert {cond}")
            }
            _ => {
                let ty = self.random_type(false);
                let annotate = self.src.chance(40);
                let value = self.expr(
                    &ty,
                    sub,
                    Ctx {
                        known: annotate,
                        multi: true,
                    },
                );
                let annotation = if annotate {
                    format!(": {}", ty.render(&self.types))
                } else {
                    String::new()
                };
                let b = self.bind("v", ty);
                format!("{ind}let {b}{annotation} = {value}")
            }
        }
    }

    /// A local function that counts down, and a call of it.
    fn loop_fn(&mut self, sub: u32) -> String {
        let ind = self.ind();
        let ty = self.random_type(false);
        let (fname, fid) = self.binder("step");
        let fn_name = self
            .binders
            .get(fid)
            .map(|b| b.name.clone())
            .unwrap_or_default();
        if let Some(b) = self.binders.get_mut(fid) {
            b.used = true;
        }
        let iterations = self.src.range(0, MAX_COUNTER);
        let init = self.expr(&ty, sub.saturating_sub(1), INLINE);
        let rendered = ty.render(&self.types);
        let times = u64::try_from(iterations)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let body = self.with_mult(times, |g| {
            g.scoped(|g| {
                let n = g.bind("n", Ty::Int);
                let n_name = g.env.last().map(|v| v.name.clone()).unwrap_or_default();
                if let Some(b) = g.env.last().and_then(|v| g.binders.get_mut(v.binder)) {
                    b.used = true;
                }
                let acc = g.bind("acc", ty.clone());
                let acc_name = g.use_var(g.env.len().saturating_sub(1));
                g.indented(|g| {
                    let i1 = g.ind();
                    let i2 = format!("{i1}  ");
                    let next = g.indented(|g| g.expr(&ty, sub.saturating_sub(1), INLINE));
                    format!(
                        "{ind}fn {fname}({n}: Int, {acc}: {rendered}) -> {rendered} {{\n\
                         {i1}case {{\n\
                         {i2}{n_name} <= 0 => {acc_name}\n\
                         {i2}_ => {fn_name}({n_name} - 1, {next})\n\
                         {i1}}}\n\
                         {ind}}}"
                    )
                })
            })
        });
        let result = self.bind("v", ty);
        format!("{body}\n{ind}let {result} = {fn_name}({iterations}, {init})")
    }

    /// `let g = fn(x: A) -> B { … }`, callable later.
    fn closure_let(&mut self, sub: u32) -> String {
        let ind = self.ind();
        let from = self.random_type(false);
        let to = self.random_type(false);
        let saved = (self.cost, self.mult, self.budget);
        self.cost = 0;
        self.mult = 1;
        self.budget = saved.2.saturating_sub(saved.0) / 4;
        let text = self.scoped(|g| {
            let x = g.bind("x", from.clone());
            let body = g.body(&to, sub.saturating_sub(1), true);
            format!(
                "fn({x}: {}) -> {} {body}",
                from.render(&g.types),
                to.render(&g.types)
            )
        });
        let cost = self.cost.max(1);
        (self.cost, self.mult, self.budget) = saved;
        let b = self.bind("g", Ty::Fun(Box::new(from), Box::new(to)));
        if let Some(var) = self.env.last_mut() {
            var.cost = cost;
        }
        format!("{ind}let {b} = {text}")
    }

    // ---- Functions -------------------------------------------------------

    fn signature(&mut self, module: Option<&'static str>, name: String) -> FnDef {
        let vis = match self.src.weighted(&[4, 4, 2]) {
            1 => Vis::Package,
            2 => Vis::Pub,
            _ => Vis::Priv,
        };
        let builtin = vis == Vis::Pub || module.is_some();
        let shape = match self.src.weighted(&[4, 2, 2, 2]) {
            1 => Shape::TailCounter,
            2 => Shape::BodyCounter,
            3 => Shape::ListWalk,
            _ => Shape::Plain,
        };
        let ret = self.random_type(builtin);
        let mut params = Vec::new();
        let mut counter = None;
        match shape {
            Shape::Plain => {}
            Shape::TailCounter | Shape::BodyCounter => {
                counter = Some(0);
                params.push(("n".to_owned(), Ty::Int));
                if shape == Shape::TailCounter {
                    params.push(("acc".to_owned(), ret.clone()));
                }
            }
            Shape::ListWalk => {
                let item = if self.src.chance(60) {
                    Ty::Int
                } else {
                    Ty::Str
                };
                params.push(("xs".to_owned(), Ty::list(item)));
                params.push(("acc".to_owned(), ret.clone()));
            }
        }
        let extra = self.src.range(usize::from(shape == Shape::Plain), 3);
        for i in 0..extra {
            params.push((format!("p{i}"), self.random_type(builtin)));
        }
        FnDef {
            module,
            name,
            vis,
            shape,
            params,
            ret,
            counter,
            cost: 1,
            uses: 0,
        }
    }

    /// Generates function `index` of `self.fns`; returns its text.
    fn function(&mut self, index: usize) -> String {
        let Some(def) = self.fns.get(index).cloned() else {
            return String::new();
        };
        let iterations: u64 = match def.shape {
            Shape::Plain => 1,
            Shape::TailCounter | Shape::BodyCounter => COUNTER_TIMES,
            Shape::ListWalk => LIST_TIMES + 1,
        };
        self.env.clear();
        self.cost = 0;
        self.mult = 1;
        self.budget = FN_BUDGET.checked_div(iterations).unwrap_or(FN_BUDGET);
        self.indent = 1;
        self.current = Some(index);
        let params: Vec<String> = def
            .params
            .iter()
            .map(|(name, ty)| {
                let b = self.bind(name, ty.clone());
                format!("{b}: {}", ty.render(&self.types))
            })
            .collect();
        let depth = self.cfg.max_depth;
        let mut lines = Vec::new();
        for _ in 0..self.src.range(0, self.cfg.max_stmts) {
            lines.push(self.stmt(depth.saturating_sub(1)));
        }
        lines.push(self.function_result(&def, depth));
        self.current = None;
        let cost = iterations.saturating_mul(self.cost.max(1));
        if let Some(f) = self.fns.get_mut(index) {
            f.cost = cost;
        }
        let ret = def.ret.render(&self.types);
        format!(
            "{}fn {}({}) -> {ret} {{\n{}\n}}\n",
            def.vis.keyword(),
            def.name,
            params.join(", "),
            lines.join("\n")
        )
    }

    /// Parameter `index` of the function being generated (parameters are
    /// the first locals in scope).
    fn param(&mut self, index: usize) -> String {
        self.use_var(index)
    }

    /// The arguments of a recursive call: the counter or list given, an
    /// accumulator if the shape has one, and each other parameter passed
    /// through or replaced.
    fn self_args(&mut self, def: &FnDef, first: String, acc: Option<String>) -> String {
        let mut args = vec![first];
        args.extend(acc);
        let skip = args.len();
        for (index, (_, ty)) in def.params.iter().enumerate().skip(skip) {
            if self.src.chance(65) {
                args.push(self.use_var(index));
            } else {
                args.push(self.expr(ty, 1, INLINE));
            }
        }
        args.join(", ")
    }

    fn function_result(&mut self, def: &FnDef, depth: u32) -> String {
        let ind = self.ind();
        let i1 = format!("{ind}  ");
        let sub = depth.saturating_sub(1);
        let name = def.name.clone();
        match def.shape {
            Shape::Plain => {
                let e = self.expr(
                    &def.ret,
                    depth,
                    Ctx {
                        known: true,
                        multi: true,
                    },
                );
                format!("{ind}{e}")
            }
            Shape::TailCounter => {
                let n = self.param(0);
                let acc = self.param(1);
                let next = self.indented(|g| g.expr(&def.ret, sub, INLINE));
                let args = self.indented(|g| g.self_args(def, format!("{n} - 1"), Some(next)));
                format!("{ind}case {{\n{i1}{n} <= 0 => {acc}\n{i1}_ => {name}({args})\n{ind}}}")
            }
            Shape::BodyCounter => {
                let n = self.param(0);
                let base = self.indented(|g| g.expr(&def.ret, sub, INLINE));
                let rec = self.indented(|g| {
                    g.indented(|g| {
                        let args = g.self_args(def, format!("{n} - 1"), None);
                        let i2 = g.ind();
                        let r = g.bind("r", def.ret.clone());
                        let e = g.expr(
                            &def.ret,
                            sub,
                            Ctx {
                                known: true,
                                multi: true,
                            },
                        );
                        format!("{i2}let {r} = {name}({args})\n{i2}{e}")
                    })
                });
                format!("{ind}case {{\n{i1}{n} <= 0 => {base}\n{i1}_ => {{\n{rec}\n{i1}}}\n{ind}}}")
            }
            Shape::ListWalk => {
                let xs = self.param(0);
                let acc = self.param(1);
                let item = match def.params.first() {
                    Some((_, Ty::List(item))) => (**item).clone(),
                    _ => Ty::Int,
                };
                let arm = self.indented(|g| {
                    g.scoped(|g| {
                        let h = g.bind("h", item.clone());
                        let t = g.bind("t", Ty::list(item.clone()));
                        let t_name = g.use_var(g.env.len().saturating_sub(1));
                        let next = g.expr(&def.ret, sub, INLINE);
                        let args = g.self_args(def, t_name, Some(next));
                        format!("[{h}, ..{t}] => {name}({args})")
                    })
                });
                format!("{ind}case {xs} {{\n{i1}[] => {acc}\n{i1}{arm}\n{ind}}}")
            }
        }
    }

    // ---- Modules -------------------------------------------------------------

    fn helpers_module(&mut self) -> Option<String> {
        let count = self.cfg.helper_functions;
        if count == 0 {
            return None;
        }
        self.module = Module::Helpers;
        self.imports.clear();
        let count = self.src.range(1, count);
        let mut texts = Vec::new();
        for i in 0..count {
            let def = self.signature(Some("helpers"), format!("h{i}"));
            self.fns.push(def);
            let index = self.fns.len().saturating_sub(1);
            texts.push(self.function(index));
        }
        // A `priv` helper that no helper calls becomes package-visible,
        // so that `main` can call it.
        let mut body = String::new();
        for (i, text) in texts.iter().enumerate() {
            let index = self.fns.len().saturating_sub(count).saturating_add(i);
            let flip = self
                .fns
                .get(index)
                .is_some_and(|f| f.vis == Vis::Priv && f.uses == 0);
            if flip {
                if let Some(f) = self.fns.get_mut(index) {
                    f.vis = Vis::Package;
                }
                body.push_str(text.strip_prefix("priv ").unwrap_or(text));
            } else {
                body.push_str(text);
            }
            body.push('\n');
        }
        let mut out =
            String::from("//! Helpers of a generated program, called from `main`'s module.\n\n");
        out.push_str(&Self::render_imports(&body));
        out.push_str(body.trim_end());
        out.push('\n');
        Some(out)
    }

    /// The imports `body` needs: each module it names as `module::`,
    /// found in the final text, so that a part generated and then dropped
    /// never leaves an unused import.
    fn render_imports(body: &str) -> String {
        let names = |module: &str| {
            let needle = format!("{module}::");
            body.match_indices(&needle).any(|(at, _)| {
                at == 0
                    || body
                        .get(..at)
                        .and_then(|before| before.chars().next_back())
                        .is_some_and(|c| !(c.is_ascii_alphanumeric() || c == '_'))
            })
        };
        let mut out = String::new();
        for module in ["fiber", "int", "io", "list", "map", "string"] {
            if names(module) {
                let _ = writeln!(out, "import std::{module}");
            }
        }
        if names("helpers") {
            out.push_str("import progen::helpers\n");
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }

    const RUN: &'static str = "fn run(name: String, body: fn() -> String) {
  case fiber::scope(fn(_) { body() }) {
    Ok(text) => io::println(\"{name}: {text}\")
    Err(failure) => {
      let reason = fiber::message(failure)
      io::println(\"{name} panicked: {reason}\")
    }
  }
}
";

    /// The planted panic: a recursion that is not a tail call and
    /// panics at its bottom, so that the panic unwinds several frames.
    fn finale(&mut self) -> (String, Vec<Ty>) {
        self.env.clear();
        self.cost = 0;
        self.mult = 1;
        self.budget = u64::MAX;
        self.indent = 3;
        let count = self.src.range(1, 3);
        let tys: Vec<Ty> = (0..count).map(|_| self.random_type(false)).collect();
        let n = self.bind("n", Ty::Int);
        let n_name = self.use_var(0);
        let params: Vec<String> = tys
            .iter()
            .map(|t| {
                let b = self.bind("p", t.clone());
                format!("{b}: {}", t.render(&self.types))
            })
            .collect();
        let pass: Vec<String> = (1..=count).map(|i| self.use_var(i)).collect();
        let r = self.bind("r", Ty::Int);
        let e = self.expr(&Ty::Int, 2, INLINE);
        let text = format!(
            "fn finale({n}: Int, {}) -> Int {{\n  case {{\n    {n_name} <= 0 => panic as \"{PLANTED_TEXT}\"\n    _ => {{\n      let {r} = finale({n_name} - 1, {})\n      {e}\n    }}\n  }}\n}}\n",
            params.join(", "),
            pass.join(", ")
        );
        (text, tys)
    }

    /// `main`: values shared by the sections, one section per function,
    /// and the planted panic.
    fn main_fn(&mut self, planted: Option<&[Ty]>) -> String {
        self.env.clear();
        self.indent = 1;
        self.cost = 0;
        self.mult = 1;
        self.budget = u64::MAX;
        let mut lines = Vec::new();
        // Every variant and record type, built once and printed.
        self.safe = true;
        let mut all = Vec::new();
        for index in 0..self.types.recs.len() {
            all.push(self.record_literal(index, 1));
        }
        for adt in 0..self.types.adts.len() {
            let count = self.types.adts.get(adt).map_or(0, |a| a.variants.len());
            for v in 0..count {
                all.push(self.variant_value(adt, 1, Some(v)));
            }
        }
        for _ in 0..self.src.range(2, 5) {
            let ty = self.random_type(false);
            let value = self.expr(&ty, 2, INLINE);
            let b = self.bind("s", ty.clone());
            lines.push(format!("  let {b}: {} = {value}", ty.render(&self.types)));
        }
        self.safe = false;
        self.import("fiber");
        self.import("io");
        for (i, value) in all.iter().enumerate() {
            lines.push(format!(
                "  run(\"value {i}\", fn() {{\n    let shown = {value}\n    \"{{shown:?}}\"\n  }})"
            ));
        }
        let order: Vec<usize> = (0..self.fns.len())
            .filter(|i| self.callable().contains(i))
            .collect();
        for index in order {
            lines.push(self.section(index));
        }
        if let Some(tys) = planted {
            self.safe = true;
            let args: Vec<String> = tys.iter().map(|t| self.expr(t, 1, INLINE)).collect();
            self.safe = false;
            let depth = self.src.range(1, 4);
            lines.push(format!("  let last = finale({depth}, {})", args.join(", ")));
            lines.push("  io::println(\"{last}\")".to_owned());
        }
        format!("fn main() {{\n{}\n}}\n", lines.join("\n"))
    }

    fn section(&mut self, index: usize) -> String {
        let Some(def) = self.fns.get(index).cloned() else {
            return String::new();
        };
        self.indent = 2;
        let label = match def.module {
            Some(m) => format!("{m}::{}", def.name),
            None => def.name.clone(),
        };
        let call = self.call(index, 2);
        let show = if def.ret.shows() {
            "\"{result}\"".to_owned()
        } else if def.ret == Ty::Map {
            let m = self.import("map");
            format!(
                "{{\n      let pairs = {m}::to_sorted_list(result)\n      \"{{pairs:?}}\"\n    }}"
            )
        } else {
            "\"{result:?}\"".to_owned()
        };
        format!("  run(\"{label}\", fn() {{\n    let result = {call}\n    {show}\n  }})")
    }

    /// Generates the whole package.
    pub(crate) fn program(&mut self, seed_text: &str) -> Output {
        let helpers = self.helpers_module();
        self.module = Module::Main;
        self.imports.clear();
        self.declare_types();
        let count = self.src.range(1, self.cfg.functions.max(1));
        let mut texts = Vec::new();
        for i in 0..count {
            let def = self.signature(None, format!("f{i}"));
            self.fns.push(def);
            let index = self.fns.len().saturating_sub(1);
            texts.push(self.function(index));
        }
        let planted =
            self.cfg.final_panic_percent > 0 && self.src.chance(self.cfg.final_panic_percent);
        let finale = planted.then(|| self.finale());
        let main = self.main_fn(finale.as_ref().map(|(_, tys)| tys.as_slice()));
        let mut out = format!("//! A generated program ({seed_text}).\n");
        if planted {
            let _ = writeln!(out, "//! {}", super::PLANTED_MARKER);
        }
        out.push('\n');
        let mut body = self.render_types();
        for text in &texts {
            body.push_str(text);
            body.push('\n');
        }
        body.push_str(Self::RUN);
        body.push('\n');
        if let Some((text, _)) = &finale {
            body.push_str(text);
            body.push('\n');
        }
        body.push_str(&main);
        out.push_str(&Self::render_imports(&body));
        out.push_str(&body);
        Output {
            main: self.resolve(&out),
            helpers: helpers.map(|h| self.resolve(&h)),
            planted,
        }
    }

    /// Replaces each binder's placeholder with its name, or `_name` when
    /// nothing reads it.
    fn resolve(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find(OPEN) {
            out.push_str(rest.get(..start).unwrap_or(""));
            let after = rest
                .get(start.saturating_add(OPEN.len_utf8())..)
                .unwrap_or("");
            let Some(end) = after.find(CLOSE) else {
                out.push_str(after);
                return out;
            };
            let id: usize = after
                .get(..end)
                .and_then(|s| s.parse().ok())
                .unwrap_or(usize::MAX);
            if let Some(b) = self.binders.get(id) {
                if !b.used {
                    out.push('_');
                }
                out.push_str(&b.name);
            }
            rest = after
                .get(end.saturating_add(CLOSE.len_utf8())..)
                .unwrap_or("");
        }
        out.push_str(rest);
        out
    }
}
