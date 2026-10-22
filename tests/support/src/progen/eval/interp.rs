//! Evaluation of a lowered program, as chapters 5, 6 and 11 of the spec
//! define it: strict, left to right (§5.15.1), checked `Int` arithmetic
//! (§5.4.2), structural equality (§3.13), `case` with guards tried at
//! most once per arm (§5.9), and panics that carry the message and the
//! location of §11.6 until a `fiber::scope` catches them (§11.7).

use std::rc::Rc;

use super::EvalError;
use super::ir::{
    Arm, Block, CtorDef, CtorShape, Expr, Global, Item, ListRest, Loc, Name, Op, Part, Pat,
    PatFields, Program, Stmt,
};
use super::stdlib;
use super::value::{self, Closure, CompareError, Env, Value};

/// The most expressions one program may evaluate before it is taken for a
/// runaway one. The generator bounds a function's work to about 6,000
/// body evaluations (`generator.rs`, `FN_BUDGET`): the 240 programs of the
/// self-check and differential seeds take at most about 65,000 steps, so
/// this leaves a margin of about 75 and stops a runaway program within a
/// few seconds.
pub(super) const MAX_STEPS: u64 = 5_000_000;

/// The deepest nesting of calls. Generated recursion is bounded by small
/// counters and short lists, far below this.
const MAX_DEPTH: u32 = 4_000;

/// The message of a panic from `==` or hashing (§11.6).
pub(super) const OPAQUE_EQUALITY: &str = "equality or hashing reached a function value or a handle";

/// How evaluation stops early: a panic, which a scope may catch, or an
/// error of the evaluator, which nothing catches.
#[derive(Debug)]
pub(super) enum Flow {
    Panic {
        message: String,
        /// `None` for a panic inside a standard-library function, whose
        /// location the spec leaves to the implementation (§11.6).
        loc: Option<Loc>,
    },
    Error(EvalError),
}

impl From<EvalError> for Flow {
    fn from(error: EvalError) -> Self {
        Self::Error(error)
    }
}

pub(super) type R<T> = Result<T, Flow>;

pub(super) fn malformed<T>(message: impl Into<String>) -> R<T> {
    Err(Flow::Error(EvalError::Malformed(message.into())))
}

pub(super) fn panic<T>(message: impl Into<String>, loc: Option<Loc>) -> R<T> {
    Err(Flow::Panic {
        message: message.into(),
        loc,
    })
}

/// The prelude's constructors that standard-library results use.
#[derive(Debug)]
pub(super) struct Prelude {
    pub(super) some: Rc<CtorDef>,
    pub(super) none: Rc<CtorDef>,
    pub(super) ok: Rc<CtorDef>,
    pub(super) err: Rc<CtorDef>,
}

/// The state of one run.
#[derive(Debug)]
pub(super) struct Interp<'p> {
    program: &'p Program,
    pub(super) prelude: Prelude,
    /// Everything `io::println` wrote.
    pub(super) stdout: String,
    steps: u64,
    depth: u32,
}

fn ctor(name: &str, shape: CtorShape) -> Rc<CtorDef> {
    Rc::new(CtorDef {
        name: Rc::from(name),
        shape,
    })
}

impl<'p> Interp<'p> {
    pub(super) fn new(program: &'p Program) -> Self {
        Self {
            program,
            prelude: Prelude {
                some: ctor("Some", CtorShape::Positional(1)),
                none: ctor("None", CtorShape::Unit),
                ok: ctor("Ok", CtorShape::Positional(1)),
                err: ctor("Err", CtorShape::Positional(1)),
            },
            stdout: String::new(),
            steps: 0,
            depth: 0,
        }
    }

    /// How many expressions the run has evaluated.
    pub(super) fn steps(&self) -> u64 {
        self.steps
    }

    /// Runs `main`.
    pub(super) fn run_main(&mut self) -> R<Value> {
        let main = Value::Global(Global::Fn(self.program.main));
        self.apply(&main, Vec::new())
    }

    fn step(&mut self) -> R<()> {
        self.steps = self.steps.saturating_add(1);
        if self.steps > MAX_STEPS {
            return Err(Flow::Error(EvalError::Budget));
        }
        Ok(())
    }

    pub(super) fn some(&self, value: Value) -> Value {
        Value::data(&self.prelude.some, vec![value])
    }

    pub(super) fn none(&self) -> Value {
        Value::data(&self.prelude.none, Vec::new())
    }

    /// Calls a function value with `args`.
    pub(super) fn apply(&mut self, f: &Value, args: Vec<Value>) -> R<Value> {
        self.depth = self.depth.saturating_add(1);
        let out = if self.depth > MAX_DEPTH {
            Err(Flow::Error(EvalError::Budget))
        } else {
            self.apply_inner(f, args)
        };
        self.depth = self.depth.saturating_sub(1);
        out
    }

    fn apply_inner(&mut self, f: &Value, args: Vec<Value>) -> R<Value> {
        match f {
            Value::Closure(closure) => {
                let mut env = closure.env.clone();
                if let Some(name) = &closure.name {
                    env = env.bind(name.clone(), Value::Closure(closure.clone()));
                }
                let env = Self::bind_params(&closure.def.params, args, env, "a closure")?;
                self.block(&closure.def.body, &env)
            }
            Value::Global(Global::Fn(index)) => {
                let program = self.program;
                let Some(def) = program.fns.get(*index) else {
                    return malformed("a function index out of range");
                };
                let env = Self::bind_params(&def.params, args, Env::default(), &def.name)?;
                self.block(&def.body, &env)
            }
            Value::Global(Global::Std(f)) => stdlib::call(self, *f, &args),
            Value::Global(Global::Ctor(ctor)) => match ctor.shape {
                CtorShape::Positional(n) if n == args.len() => Ok(Value::data(ctor, args)),
                _ => malformed(format!("a wrong call of the constructor `{}`", ctor.name)),
            },
            other => malformed(format!("a call of {}", other.kind())),
        }
    }

    fn bind_params(params: &[Pat], args: Vec<Value>, env: Env, what: &str) -> R<Env> {
        if params.len() != args.len() {
            return malformed(format!(
                "{what} takes {} arguments, not {}",
                params.len(),
                args.len()
            ));
        }
        let mut env = env;
        for (pat, arg) in params.iter().zip(args) {
            match Self::matches(pat, &arg, env)? {
                Some(next) => env = next,
                None => return malformed(format!("a parameter pattern of {what} did not match")),
            }
        }
        Ok(env)
    }

    pub(super) fn block(&mut self, block: &Block, env: &Env) -> R<Value> {
        let mut env = env.clone();
        let mut last = Value::Nil;
        for stmt in &block.stmts {
            last = Value::Nil;
            match stmt {
                Stmt::Let { pat, value } => {
                    let v = self.eval(value, &env)?;
                    match Self::matches(pat, &v, env)? {
                        Some(next) => env = next,
                        None => return malformed("a refutable pattern in a plain `let`"),
                    }
                }
                Stmt::LetAssert {
                    pat,
                    value,
                    message,
                    loc,
                } => {
                    let v = self.eval(value, &env)?;
                    if let Some(next) = Self::matches(pat, &v, env.clone())? {
                        env = next;
                    } else {
                        // §11.4: the custom message, evaluated only now,
                        // then the value's debug text.
                        let mut text = String::from("`let assert` pattern did not match");
                        if let Some(m) = message {
                            let m = self.eval_str(m, &env)?;
                            text.push_str(": ");
                            text.push_str(&m);
                        }
                        text.push_str("\n  value: ");
                        value::debug(&v, &mut text)?;
                        return panic(text, Some(*loc));
                    }
                }
                Stmt::Assert {
                    cond,
                    message,
                    text,
                    loc,
                } => self.assert(cond, message.as_ref(), text, *loc, &env)?,
                Stmt::LocalFn { name, def } => {
                    let closure = Value::Closure(Rc::new(Closure {
                        def: def.clone(),
                        env: env.clone(),
                        name: Some(name.clone()),
                    }));
                    env = env.bind(name.clone(), closure);
                }
                Stmt::Expr(e) => last = self.eval(e, &env)?,
            }
        }
        Ok(last)
    }

    /// `assert cond` (§11.5): a failing comparison also shows both
    /// operands, each evaluated once.
    fn assert(
        &mut self,
        cond: &Expr,
        message: Option<&Expr>,
        text: &str,
        loc: Loc,
        env: &Env,
    ) -> R<()> {
        let operands = match cond {
            Expr::Binary {
                op,
                lhs,
                rhs,
                loc: op_loc,
            } if op.is_comparison() => {
                let l = self.eval(lhs, env)?;
                let r = self.eval(rhs, env)?;
                let holds = self.binary_values(*op, &l, &r, *op_loc)?;
                if holds {
                    return Ok(());
                }
                Some((l, r))
            }
            _ => {
                if self.eval_bool(cond, env)? {
                    return Ok(());
                }
                None
            }
        };
        let mut out = String::from("assertion failed: ");
        match message {
            Some(m) => {
                let m = self.eval_str(m, env)?;
                out.push_str(&m);
            }
            None => out.push_str(text),
        }
        if let Some((l, r)) = operands {
            out.push_str("\n  left: ");
            value::debug(&l, &mut out)?;
            out.push_str("\n right: ");
            value::debug(&r, &mut out)?;
        }
        panic(out, Some(loc))
    }

    fn eval_bool(&mut self, expr: &Expr, env: &Env) -> R<bool> {
        match self.eval(expr, env)? {
            Value::Bool(b) => Ok(b),
            other => malformed(format!("{} where a Bool is expected", other.kind())),
        }
    }

    fn eval_str(&mut self, expr: &Expr, env: &Env) -> R<Rc<str>> {
        match self.eval(expr, env)? {
            Value::Str(s) => Ok(s),
            other => malformed(format!("{} where a String is expected", other.kind())),
        }
    }

    fn field(value: &Value, field: &Name) -> R<Value> {
        let Value::Data(data) = value else {
            return malformed(format!("the field `{field}` of {}", value.kind()));
        };
        let CtorShape::Named(names) = &data.ctor.shape else {
            return malformed(format!("the field `{field}` of `{}`", data.ctor.name));
        };
        names
            .iter()
            .position(|n| n == field)
            .and_then(|i| data.fields.get(i))
            .cloned()
            .map_or_else(
                || malformed(format!("`{}` has no field `{field}`", data.ctor.name)),
                Ok,
            )
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per kind of expression, as the spec's sections run"
    )]
    pub(super) fn eval(&mut self, expr: &Expr, env: &Env) -> R<Value> {
        self.step()?;
        match expr {
            Expr::Int(n) => Ok(Value::Int(*n)),
            Expr::Str(s) => Ok(Value::Str(s.clone())),
            Expr::Interp(parts) => {
                let mut out = String::new();
                for part in parts {
                    match part {
                        Part::Text(text) => out.push_str(text),
                        Part::Value {
                            root,
                            fields,
                            debug,
                        } => {
                            let Some(mut v) = env.get(root).cloned() else {
                                return malformed(format!("`{root}` is not bound"));
                            };
                            for f in fields {
                                v = Self::field(&v, f)?;
                            }
                            if *debug {
                                value::debug(&v, &mut out)?;
                            } else {
                                value::show(&v, &mut out)?;
                            }
                        }
                    }
                }
                Ok(Value::str(&out))
            }
            Expr::Local(name) => env
                .get(name)
                .cloned()
                .map_or_else(|| malformed(format!("`{name}` is not bound")), Ok),
            Expr::Global(global) => match global {
                Global::Bool(b) => Ok(Value::Bool(*b)),
                Global::Ctor(ctor) => match ctor.shape {
                    CtorShape::Unit => Ok(Value::data(ctor, Vec::new())),
                    CtorShape::Positional(_) => Ok(Value::Global(global.clone())),
                    CtorShape::Named(_) => {
                        malformed(format!("the named constructor `{}` as a value", ctor.name))
                    }
                },
                Global::Fn(_) | Global::Std(_) => Ok(Value::Global(global.clone())),
            },
            Expr::Construct { ctor, fields } => self.construct(ctor, fields, env),
            Expr::Update { base, fields } => {
                // The base first, then the fields in the order written;
                // the result has the base's variant (§5.11.2).
                let base = self.eval(base, env)?;
                let Value::Data(data) = &base else {
                    return malformed(format!("a record update of {}", base.kind()));
                };
                let CtorShape::Named(names) = &data.ctor.shape else {
                    return malformed(format!("a record update of `{}`", data.ctor.name));
                };
                let mut values = data.fields.clone();
                for (field, e) in fields {
                    let v = self.eval(e, env)?;
                    let slot = names
                        .iter()
                        .position(|n| n == field)
                        .and_then(|i| values.get_mut(i));
                    match slot {
                        Some(slot) => *slot = v,
                        None => {
                            return malformed(format!(
                                "`{}` has no field `{field}`",
                                data.ctor.name
                            ));
                        }
                    }
                }
                Ok(Value::data(&data.ctor, values))
            }
            Expr::Tuple(items) => {
                let mut values = Vec::new();
                for e in items {
                    values.push(self.eval(e, env)?);
                }
                Ok(Value::Tuple(Rc::new(values)))
            }
            Expr::List(items) => {
                let mut values = Vec::new();
                for item in items {
                    match item {
                        Item::One(e) => values.push(self.eval(e, env)?),
                        Item::Spread(e) => match self.eval(e, env)? {
                            Value::List(xs) => values.extend(xs.iter().cloned()),
                            other => return malformed(format!("a spread of {}", other.kind())),
                        },
                    }
                }
                Ok(Value::list(values))
            }
            Expr::Block(block) => self.block(block, env),
            Expr::Closure(def) => Ok(Value::Closure(Rc::new(Closure {
                def: def.clone(),
                env: env.clone(),
                name: None,
            }))),
            Expr::Case { subjects, arms } => self.case(subjects, arms, env),
            Expr::Cond { arms, default } => {
                for (cond, body) in arms {
                    if self.eval_bool(cond, env)? {
                        return self.eval(body, env);
                    }
                }
                match default {
                    Some(body) => self.eval(body, env),
                    None => malformed("a `case` without a subject and without `_`"),
                }
            }
            Expr::Panic { todo, message, loc } => {
                let text = match message {
                    Some(m) => Some(self.eval_str(m, env)?),
                    None => None,
                };
                let text = match (todo, text) {
                    (false, Some(m)) => m.to_string(),
                    (false, None) => "explicit panic".to_owned(),
                    (true, Some(m)) => format!("not implemented: {m}"),
                    (true, None) => "not implemented".to_owned(),
                };
                panic(text, Some(*loc))
            }
            Expr::Binary { op, lhs, rhs, loc } => match op {
                // Short circuits (§5.4.5).
                Op::And => {
                    if self.eval_bool(lhs, env)? {
                        Ok(Value::Bool(self.eval_bool(rhs, env)?))
                    } else {
                        Ok(Value::Bool(false))
                    }
                }
                Op::Or => {
                    if self.eval_bool(lhs, env)? {
                        Ok(Value::Bool(true))
                    } else {
                        Ok(Value::Bool(self.eval_bool(rhs, env)?))
                    }
                }
                _ => {
                    let l = self.eval(lhs, env)?;
                    let r = self.eval(rhs, env)?;
                    self.binary(*op, &l, &r, *loc)
                }
            },
            Expr::Not(operand) => Ok(Value::Bool(!self.eval_bool(operand, env)?)),
            Expr::Neg { operand, loc } => match self.eval(operand, env)? {
                Value::Int(n) => n.checked_neg().map_or_else(
                    || panic("integer overflow", Some(*loc)),
                    |v| Ok(Value::Int(v)),
                ),
                other => malformed(format!("`-` on {}", other.kind())),
            },
            Expr::Call { callee, args } => {
                // The callee first, then the arguments (§5.15.1).
                let f = self.eval(callee, env)?;
                let mut values = Vec::new();
                for a in args {
                    values.push(self.eval(a, env)?);
                }
                self.apply(&f, values)
            }
            Expr::Field { receiver, field } => {
                let v = self.eval(receiver, env)?;
                Self::field(&v, field)
            }
        }
    }

    fn construct(&mut self, ctor: &Rc<CtorDef>, fields: &[(Name, Expr)], env: &Env) -> R<Value> {
        let CtorShape::Named(names) = &ctor.shape else {
            return malformed(format!("`{}` built with named fields", ctor.name));
        };
        let mut slots: Vec<Option<Value>> = vec![None; names.len()];
        // In the order written (§5.11.1), stored in declaration order.
        for (field, e) in fields {
            let v = self.eval(e, env)?;
            match names
                .iter()
                .position(|n| n == field)
                .and_then(|i| slots.get_mut(i))
            {
                Some(slot @ None) => *slot = Some(v),
                _ => {
                    return malformed(format!(
                        "the field `{field}` of `{}` is unknown or repeated",
                        ctor.name
                    ));
                }
            }
        }
        let values: Option<Vec<Value>> = slots.into_iter().collect();
        match values {
            Some(values) => Ok(Value::data(ctor, values)),
            None => malformed(format!("`{}` built without every field", ctor.name)),
        }
    }

    /// `case` with subjects (§5.9.1): subjects left to right, then each
    /// arm in order; a guard runs once per arm tried, with the bindings of
    /// the first alternative that matched (§5.9.3).
    fn case(&mut self, subjects: &[Expr], arms: &[Arm], env: &Env) -> R<Value> {
        let mut values = Vec::new();
        for s in subjects {
            values.push(self.eval(s, env)?);
        }
        'arms: for arm in arms {
            let mut arm_env = env.clone();
            for (pat, v) in arm.pats.iter().zip(&values) {
                match Self::matches(pat, v, arm_env)? {
                    Some(next) => arm_env = next,
                    None => continue 'arms,
                }
            }
            if let Some(guard) = &arm.guard
                && !self.eval_bool(guard, &arm_env)?
            {
                continue;
            }
            return self.eval(&arm.body, &arm_env);
        }
        malformed("no arm of a `case` matched")
    }

    /// `pat` against `value`: the environment extended with the pattern's
    /// bindings, or `None`.
    pub(super) fn matches(pat: &Pat, value: &Value, env: Env) -> R<Option<Env>> {
        Ok(match (pat, value) {
            (Pat::Wild, _) => Some(env),
            (Pat::Bind(name), _) => Some(env.bind(name.clone(), value.clone())),
            (Pat::Int(n), Value::Int(v)) => (n == v).then_some(env),
            (Pat::Str(s), Value::Str(v)) => (s == v).then_some(env),
            (Pat::Bool(b), Value::Bool(v)) => (b == v).then_some(env),
            (Pat::Ctor { ctor, fields }, Value::Data(data)) => {
                if ctor.name != data.ctor.name {
                    return Ok(None);
                }
                match fields {
                    PatFields::None => Some(env),
                    PatFields::Positional(pats) => {
                        if pats.len() != data.fields.len() {
                            return malformed(format!(
                                "a pattern of `{}` with the wrong arity",
                                ctor.name
                            ));
                        }
                        Self::match_all(pats.iter().zip(&data.fields), env)?
                    }
                    PatFields::Named(pats) => {
                        let CtorShape::Named(names) = &data.ctor.shape else {
                            return malformed(format!("a named pattern of `{}`", ctor.name));
                        };
                        let mut pairs = Vec::new();
                        for (field, p) in pats {
                            let Some(v) = names
                                .iter()
                                .position(|n| n == field)
                                .and_then(|i| data.fields.get(i))
                            else {
                                return malformed(format!(
                                    "`{}` has no field `{field}`",
                                    ctor.name
                                ));
                            };
                            pairs.push((p, v));
                        }
                        Self::match_all(pairs.into_iter(), env)?
                    }
                }
            }
            (Pat::Tuple(pats), Value::Tuple(items)) => {
                if pats.len() != items.len() {
                    return malformed("a tuple pattern with the wrong arity");
                }
                Self::match_all(pats.iter().zip(items.iter()), env)?
            }
            (
                Pat::List {
                    before,
                    rest,
                    after,
                },
                Value::List(items),
            ) => {
                let fixed = before.len().saturating_add(after.len());
                let fits = match rest {
                    ListRest::No => items.len() == fixed,
                    ListRest::Anonymous | ListRest::Bind(_) => items.len() >= fixed,
                };
                if !fits {
                    return Ok(None);
                }
                let tail_start = items.len().saturating_sub(after.len());
                let head = before.iter().zip(items.iter().take(before.len()));
                let tail = after.iter().zip(items.iter().skip(tail_start));
                let env = Self::match_all(head.chain(tail), env)?;
                match (env, rest) {
                    (Some(env), ListRest::Bind(name)) => {
                        let middle: Vec<Value> = items
                            .iter()
                            .take(tail_start)
                            .skip(before.len())
                            .cloned()
                            .collect();
                        Some(env.bind(name.clone(), Value::list(middle)))
                    }
                    (env, _) => env,
                }
            }
            (Pat::Or(alternatives), _) => {
                for p in alternatives {
                    if let Some(next) = Self::matches(p, value, env.clone())? {
                        return Ok(Some(next));
                    }
                }
                None
            }
            (Pat::As(p, name), _) => {
                Self::matches(p, value, env)?.map(|next| next.bind(name.clone(), value.clone()))
            }
            (p, v) => {
                return malformed(format!("the pattern {p:?} against {}", v.kind()));
            }
        })
    }

    /// Each pattern against its value, in order.
    fn match_all<'a>(
        pairs: impl Iterator<Item = (&'a Pat, &'a Value)>,
        env: Env,
    ) -> R<Option<Env>> {
        let mut env = env;
        for (p, v) in pairs {
            match Self::matches(p, v, env)? {
                Some(next) => env = next,
                None => return Ok(None),
            }
        }
        Ok(Some(env))
    }

    /// A built-in operator on two values that are not short-circuited.
    fn binary(&mut self, op: Op, l: &Value, r: &Value, loc: Loc) -> R<Value> {
        match (op, l, r) {
            (Op::Add, Value::Int(a), Value::Int(b)) => checked(a.checked_add(*b), loc),
            (Op::Sub, Value::Int(a), Value::Int(b)) => checked(a.checked_sub(*b), loc),
            (Op::Mul, Value::Int(a), Value::Int(b)) => checked(a.checked_mul(*b), loc),
            // Truncating toward zero; the minimum divided by -1 overflows
            // (§3.3.1).
            (Op::Div, Value::Int(a), Value::Int(b)) => {
                if *b == 0 {
                    panic("division by zero", Some(loc))
                } else {
                    checked(a.checked_div(*b), loc)
                }
            }
            // The sign of the dividend; only a zero divisor panics
            // (§5.4.2), so the minimum's remainder by -1 is 0.
            (Op::Rem, Value::Int(a), Value::Int(b)) => {
                if *b == 0 {
                    panic("division by zero", Some(loc))
                } else {
                    Ok(Value::Int(a.checked_rem(*b).unwrap_or(0)))
                }
            }
            // `String` satisfies `Add` by concatenation (§5.4.3).
            (Op::Add, Value::Str(a), Value::Str(b)) => {
                let mut text = String::with_capacity(a.len().saturating_add(b.len()));
                text.push_str(a);
                text.push_str(b);
                Ok(Value::str(&text))
            }
            _ => Ok(Value::Bool(self.binary_values(op, l, r, loc)?)),
        }
    }

    /// A comparison.
    fn binary_values(&mut self, op: Op, l: &Value, r: &Value, loc: Loc) -> R<bool> {
        let _ = self;
        match op {
            Op::Eq | Op::Ne => match value::equal(l, r) {
                Ok(eq) => Ok(eq == (op == Op::Eq)),
                Err(CompareError::Opaque) => panic(OPAQUE_EQUALITY, Some(loc)),
                Err(CompareError::Error(e)) => Err(Flow::Error(e)),
            },
            Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                let order = value::compare(l, r)?;
                Ok(match op {
                    Op::Lt => order.is_lt(),
                    Op::Le => order.is_le(),
                    Op::Gt => order.is_gt(),
                    _ => order.is_ge(),
                })
            }
            _ => malformed(format!(
                "the operator {op:?} on {} and {}",
                l.kind(),
                r.kind()
            )),
        }
    }
}

/// The result of checked `Int` arithmetic: the value, or the overflow
/// panic at `loc` (§5.4.2).
fn checked(result: Option<i64>, loc: Loc) -> R<Value> {
    result.map_or_else(
        || panic("integer overflow", Some(loc)),
        |v| Ok(Value::Int(v)),
    )
}
