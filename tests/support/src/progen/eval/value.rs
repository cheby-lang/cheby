//! The evaluator's values, with the spec's structural equality, ordering,
//! `show` and debug printing (§3.13, §3.14, §5.3.2, D-134, D-531, D-557).

use std::cmp::Ordering;
use std::fmt::Write as _;
use std::rc::Rc;

use super::EvalError;
use super::ir::{ClosureDef, CtorDef, CtorShape, Global, Name};

/// A value of the subset.
#[derive(Debug, Clone)]
pub(super) enum Value {
    Nil,
    Int(i64),
    Bool(bool),
    Str(Rc<str>),
    List(Rc<Vec<Value>>),
    Tuple(Rc<Vec<Value>>),
    /// A value built by a constructor; named fields are held in
    /// declaration order.
    Data(Rc<Data>),
    /// `Map<K, V>`: its entries in insertion order, each key once.
    Map(Rc<Vec<(Value, Value)>>),
    Closure(Rc<Closure>),
    /// A top-level function, a standard-library function or a positional
    /// constructor used as a value.
    Global(Global),
    /// A `fiber::Panic`: its message (§11.6).
    Panic(Rc<str>),
    /// The `Scope` a scope body gets.
    Scope,
}

#[derive(Debug)]
pub(super) struct Data {
    pub(super) ctor: Rc<CtorDef>,
    pub(super) fields: Vec<Value>,
}

/// A closure and what it captured. A local named function also binds its
/// own name when called (§5.8).
#[derive(Debug)]
pub(super) struct Closure {
    pub(super) def: Rc<ClosureDef>,
    pub(super) env: Env,
    pub(super) name: Option<Name>,
}

/// A persistent environment: each binding points at the ones before it,
/// so a closure keeps what it captured however the caller goes on.
#[derive(Debug, Clone, Default)]
pub(super) struct Env(Option<Rc<Binding>>);

#[derive(Debug)]
struct Binding {
    name: Name,
    value: Value,
    next: Env,
}

impl Env {
    pub(super) fn bind(&self, name: Name, value: Value) -> Self {
        Self(Some(Rc::new(Binding {
            name,
            value,
            next: self.clone(),
        })))
    }

    pub(super) fn get(&self, name: &str) -> Option<&Value> {
        let mut at = self.0.as_ref();
        while let Some(binding) = at {
            if &*binding.name == name {
                return Some(&binding.value);
            }
            at = binding.next.0.as_ref();
        }
        None
    }
}

impl Value {
    pub(super) fn str(text: &str) -> Self {
        Self::Str(Rc::from(text))
    }

    pub(super) fn list(items: Vec<Self>) -> Self {
        Self::List(Rc::new(items))
    }

    pub(super) fn data(ctor: &Rc<CtorDef>, fields: Vec<Self>) -> Self {
        Self::Data(Rc::new(Data {
            ctor: ctor.clone(),
            fields,
        }))
    }

    /// Whether the value is or holds a function value or a handle, which
    /// equality and hashing panic on (§3.13).
    fn is_opaque(&self) -> bool {
        matches!(self, Self::Closure(_) | Self::Global(_) | Self::Scope)
    }

    pub(super) fn kind(&self) -> &'static str {
        match self {
            Self::Nil => "Nil",
            Self::Int(_) => "an Int",
            Self::Bool(_) => "a Bool",
            Self::Str(_) => "a String",
            Self::List(_) => "a List",
            Self::Tuple(_) => "a tuple",
            Self::Data(_) => "a constructed value",
            Self::Map(_) => "a Map",
            Self::Closure(_) | Self::Global(_) => "a function",
            Self::Panic(_) => "a fiber::Panic",
            Self::Scope => "a Scope",
        }
    }
}

/// Why two values could not be compared.
#[derive(Debug)]
pub(super) enum CompareError {
    /// Equality reached a function value or a handle: a panic (§3.13).
    Opaque,
    /// Outside the subset.
    Error(EvalError),
}

/// Structural equality (§3.13).
pub(super) fn equal(a: &Value, b: &Value) -> Result<bool, CompareError> {
    if a.is_opaque() || b.is_opaque() {
        return Err(CompareError::Opaque);
    }
    Ok(match (a, b) {
        (Value::Nil, Value::Nil) => true,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::List(x), Value::List(y)) | (Value::Tuple(x), Value::Tuple(y)) => all_equal(x, y)?,
        (Value::Data(x), Value::Data(y)) => {
            x.ctor.name == y.ctor.name && all_equal(&x.fields, &y.fields)?
        }
        (Value::Map(x), Value::Map(y)) => {
            if x.len() != y.len() {
                return Ok(false);
            }
            for (key, value) in x.iter() {
                match map_find(y, key)? {
                    Some(index) => match y.get(index) {
                        Some((_, other)) if equal(value, other)? => {}
                        _ => return Ok(false),
                    },
                    None => return Ok(false),
                }
            }
            true
        }
        _ => {
            return Err(CompareError::Error(EvalError::Malformed(format!(
                "`==` on {} and {}",
                a.kind(),
                b.kind()
            ))));
        }
    })
}

fn all_equal(x: &[Value], y: &[Value]) -> Result<bool, CompareError> {
    if x.len() != y.len() {
        return Ok(false);
    }
    for (a, b) in x.iter().zip(y) {
        if !equal(a, b)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The index of `key` in a map's entries.
pub(super) fn map_find(
    entries: &[(Value, Value)],
    key: &Value,
) -> Result<Option<usize>, CompareError> {
    for (index, (k, _)) in entries.iter().enumerate() {
        if equal(k, key)? {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

/// The built-in `compare` of the subset's ordered types (§3.14): `Int`,
/// `Bool` (`False < True`), `String` by scalar values, and lists and
/// tuples lexicographically.
pub(super) fn compare(a: &Value, b: &Value) -> Result<Ordering, EvalError> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Ok(x.cmp(y)),
        // UTF-8 byte order is the order of scalar values.
        (Value::Str(x), Value::Str(y)) => Ok(x.as_bytes().cmp(y.as_bytes())),
        (Value::List(x), Value::List(y)) | (Value::Tuple(x), Value::Tuple(y)) => {
            for (p, q) in x.iter().zip(y.iter()) {
                let order = compare(p, q)?;
                if order != Ordering::Equal {
                    return Ok(order);
                }
            }
            Ok(x.len().cmp(&y.len()))
        }
        _ => Err(EvalError::Unsupported(format!(
            "`compare` on {} and {}",
            a.kind(),
            b.kind()
        ))),
    }
}

/// The text `{x}` interpolates: the `show` of `Int`, `Bool`, `String` and
/// of lists of a type that shows (§5.3.2, `list::show`).
pub(super) fn show(value: &Value, out: &mut String) -> Result<(), EvalError> {
    match value {
        Value::Int(n) => {
            let _ = write!(out, "{n}");
        }
        Value::Bool(b) => out.push_str(if *b { "True" } else { "False" }),
        Value::Str(s) => out.push_str(s),
        Value::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                show(item, out)?;
            }
            out.push(']');
        }
        other => {
            return Err(EvalError::Unsupported(format!(
                "`show` of {}",
                other.kind()
            )));
        }
    }
    Ok(())
}

/// A string as debug printing writes it: in quotes, with the escapes a
/// literal needs (§3.13, D-531, D-557).
fn debug_str(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '{' => out.push_str("\\{"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn debug_seq(items: &[Value], out: &mut String) -> Result<(), EvalError> {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        debug(item, out)?;
    }
    Ok(())
}

/// Built-in debug printing, `{x:?}` (§3.13, D-134).
pub(super) fn debug(value: &Value, out: &mut String) -> Result<(), EvalError> {
    match value {
        Value::Nil => out.push_str("Nil"),
        Value::Int(n) => {
            let _ = write!(out, "{n}");
        }
        Value::Bool(b) => out.push_str(if *b { "True" } else { "False" }),
        Value::Str(s) => debug_str(s, out),
        Value::List(items) => {
            out.push('[');
            debug_seq(items, out)?;
            out.push(']');
        }
        Value::Tuple(items) => {
            out.push('(');
            debug_seq(items, out)?;
            out.push(')');
        }
        Value::Data(data) => {
            out.push_str(&data.ctor.name);
            match &data.ctor.shape {
                CtorShape::Unit => {}
                CtorShape::Positional(_) => {
                    out.push('(');
                    debug_seq(&data.fields, out)?;
                    out.push(')');
                }
                CtorShape::Named(names) => {
                    out.push_str(" { ");
                    for (i, (field, item)) in names.iter().zip(&data.fields).enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        out.push_str(field);
                        out.push_str(": ");
                        debug(item, out)?;
                    }
                    out.push_str(" }");
                }
            }
        }
        Value::Map(entries) => {
            // `map::from_list([…])`, the entries sorted by the bytes of
            // their printed pairs (D-344, D-382, D-587).
            let mut texts = Vec::new();
            for (key, item) in entries.iter() {
                let mut text = String::from("(");
                debug(key, &mut text)?;
                text.push_str(", ");
                debug(item, &mut text)?;
                text.push(')');
                texts.push(text);
            }
            texts.sort();
            out.push_str("map::from_list([");
            out.push_str(&texts.join(", "));
            out.push_str("])");
        }
        Value::Closure(_) | Value::Global(_) => out.push_str("<fn>"),
        Value::Scope => out.push_str("<Scope>"),
        Value::Panic(_) => {
            return Err(EvalError::Unsupported(
                "debug printing of a fiber::Panic".to_owned(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dbg(value: &Value) -> String {
        let mut out = String::new();
        debug(value, &mut out).unwrap();
        out
    }

    #[test]
    fn progen_eval_debug_escapes_strings() {
        assert_eq!(
            dbg(&Value::str("a\"b\\c\n{x}\u{1b}")),
            r#""a\"b\\c\n\{x}\u{1b}""#
        );
        assert_eq!(dbg(&Value::str("\u{0}\u{7f}é")), "\"\\u{0}\\u{7f}é\"");
    }

    #[test]
    fn progen_eval_debug_prints_collections() {
        let pair = Value::Tuple(Rc::new(vec![Value::Int(1), Value::str("x")]));
        assert_eq!(dbg(&Value::list(vec![pair.clone()])), "[(1, \"x\")]");
        let map = Value::Map(Rc::new(vec![
            (Value::str("b"), Value::Int(9)),
            (Value::str("a"), Value::Int(10)),
        ]));
        assert_eq!(dbg(&map), "map::from_list([(\"a\", 10), (\"b\", 9)])");
        let named = Rc::new(CtorDef {
            name: Rc::from("Rec0"),
            shape: CtorShape::Named(vec![Rc::from("k0"), Rc::from("k1")]),
        });
        let rec = Value::data(&named, vec![Value::Int(-3), Value::Bool(true)]);
        assert_eq!(dbg(&rec), "Rec0 { k0: -3, k1: True }");
        let pos = Rc::new(CtorDef {
            name: Rc::from("S0B"),
            shape: CtorShape::Positional(2),
        });
        assert_eq!(
            dbg(&Value::data(&pos, vec![Value::Int(1), pair])),
            "S0B(1, (1, \"x\"))"
        );
    }

    #[test]
    fn progen_eval_equality_and_order() {
        let a = Value::list(vec![Value::Int(1), Value::Int(2)]);
        let b = Value::list(vec![Value::Int(1), Value::Int(3)]);
        assert!(!equal(&a, &b).unwrap());
        assert!(equal(&a, &a.clone()).unwrap());
        assert_eq!(compare(&a, &b).unwrap(), Ordering::Less);
        assert_eq!(
            compare(&Value::str("Zed"), &Value::str("a")).unwrap(),
            Ordering::Less
        );
        let m1 = Value::Map(Rc::new(vec![
            (Value::str("a"), Value::Int(1)),
            (Value::str("b"), Value::Int(2)),
        ]));
        let m2 = Value::Map(Rc::new(vec![
            (Value::str("b"), Value::Int(2)),
            (Value::str("a"), Value::Int(1)),
        ]));
        assert!(equal(&m1, &m2).unwrap());
        assert!(matches!(
            equal(&Value::Scope, &Value::Scope),
            Err(CompareError::Opaque)
        ));
    }
}
