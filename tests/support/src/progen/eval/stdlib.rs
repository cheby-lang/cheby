//! The standard-library functions the generator calls, as `docs/stdlib`
//! defines them: `list.md`, `string.md`, `map.md`, `int.md`, `io.md` and
//! `fiber.md`. Positions, counts and lengths of strings are in code
//! points (§3.5, D-302), and clamped arguments are clamped (D-298).

use std::cmp::Ordering;
use std::rc::Rc;

use super::EvalError;
use super::interp::{Flow, Interp, OPAQUE_EQUALITY, R, malformed, panic};
use super::ir::StdFn;
use super::value::{self, CompareError, Value};

/// The longest string `string::repeat` builds; a longer one is outside
/// the subset (the generator repeats at most 3 times).
const MAX_REPEAT: usize = 1 << 20;

fn int(v: &Value) -> R<i64> {
    match v {
        Value::Int(n) => Ok(*n),
        other => malformed(format!("{} where an Int is expected", other.kind())),
    }
}

fn text(v: &Value) -> R<&str> {
    match v {
        Value::Str(s) => Ok(s),
        other => malformed(format!("{} where a String is expected", other.kind())),
    }
}

fn list(v: &Value) -> R<&[Value]> {
    match v {
        Value::List(xs) => Ok(xs),
        other => malformed(format!("{} where a List is expected", other.kind())),
    }
}

fn entries(v: &Value) -> R<&[(Value, Value)]> {
    match v {
        Value::Map(m) => Ok(m),
        other => malformed(format!("{} where a Map is expected", other.kind())),
    }
}

fn boolean(v: &Value) -> R<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => malformed(format!("{} where a Bool is expected", other.kind())),
    }
}

fn len_value(n: usize) -> R<Value> {
    i64::try_from(n).map_or_else(|_| malformed("a length beyond Int"), |n| Ok(Value::Int(n)))
}

/// `count` clamped to `0..=len` (D-298).
fn clamp(count: i64, len: usize) -> usize {
    usize::try_from(count.max(0)).unwrap_or(usize::MAX).min(len)
}

/// Equality inside a kernel: a panic without a Cheby location (§11.6).
fn equal(a: &Value, b: &Value) -> R<bool> {
    match value::equal(a, b) {
        Ok(eq) => Ok(eq),
        Err(CompareError::Opaque) => panic(OPAQUE_EQUALITY, None),
        Err(CompareError::Error(e)) => Err(Flow::Error(e)),
    }
}

fn map_find(m: &[(Value, Value)], key: &Value) -> R<Option<usize>> {
    match value::map_find(m, key) {
        Ok(found) => Ok(found),
        Err(CompareError::Opaque) => panic(OPAQUE_EQUALITY, None),
        Err(CompareError::Error(e)) => Err(Flow::Error(e)),
    }
}

/// `m` with `value` under `key`: a stored `==` key keeps its place and
/// its own text (D-588).
fn map_insert(m: &mut Vec<(Value, Value)>, key: Value, value: Value) -> R<()> {
    match map_find(m, &key)? {
        Some(index) => {
            if let Some(entry) = m.get_mut(index) {
                entry.1 = value;
            }
        }
        None => m.push((key, value)),
    }
    Ok(())
}

/// A stable sort by `key`'s `compare` (D-299).
fn sort_by_compare<T>(items: &mut [T], key: impl Fn(&T) -> &Value) -> R<()> {
    let mut failure: Option<EvalError> = None;
    items.sort_by(|a, b| {
        if failure.is_some() {
            return Ordering::Equal;
        }
        value::compare(key(a), key(b)).unwrap_or_else(|e| {
            failure = Some(e);
            Ordering::Equal
        })
    });
    failure.map_or(Ok(()), |e| Err(Flow::Error(e)))
}

fn arity(f: StdFn, args: &[Value]) -> EvalError {
    EvalError::Malformed(format!("{f:?} called with {} arguments", args.len()))
}

/// Calls the standard-library function `f`.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per function, each a few lines, reads best in one place"
)]
pub(super) fn call(interp: &mut Interp<'_>, f: StdFn, args: &[Value]) -> R<Value> {
    let bad = || Flow::Error(arity(f, args));
    match (f, args) {
        (StdFn::ListLength, [xs]) => len_value(list(xs)?.len()),
        (StdFn::ListSum, [xs, zero]) => {
            // Front to back from `zero`, panicking where `+` does.
            let mut acc = zero.clone();
            for x in list(xs)? {
                acc = match (&acc, x) {
                    (Value::Int(a), Value::Int(b)) => match a.checked_add(*b) {
                        Some(v) => Value::Int(v),
                        None => return panic("integer overflow", None),
                    },
                    (Value::Str(a), Value::Str(b)) => Value::str(&format!("{a}{b}")),
                    (a, b) => {
                        return malformed(format!("list::sum of {} and {}", a.kind(), b.kind()));
                    }
                };
            }
            Ok(acc)
        }
        (StdFn::ListIsEmpty, [xs]) => Ok(Value::Bool(list(xs)?.is_empty())),
        (StdFn::ListContains, [xs, item]) => {
            for x in list(xs)? {
                if equal(x, item)? {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        }
        (StdFn::ListFilter, [xs, keep]) => {
            let mut out = Vec::new();
            for x in list(xs)? {
                let kept = interp.apply(keep, vec![x.clone()])?;
                if boolean(&kept)? {
                    out.push(x.clone());
                }
            }
            Ok(Value::list(out))
        }
        (StdFn::ListReverse, [xs]) => Ok(Value::list(list(xs)?.iter().rev().cloned().collect())),
        (StdFn::ListDrop, [xs, count]) => {
            let xs = list(xs)?;
            let n = clamp(int(count)?, xs.len());
            Ok(Value::list(xs.iter().skip(n).cloned().collect()))
        }
        (StdFn::ListTake, [xs, count]) => {
            let xs = list(xs)?;
            let n = clamp(int(count)?, xs.len());
            Ok(Value::list(xs.iter().take(n).cloned().collect()))
        }
        (StdFn::ListRange, [start, end]) => {
            let (start, end) = (int(start)?, int(end)?);
            if end.saturating_sub(start) > 1 << 24 {
                return Err(Flow::Error(EvalError::Unsupported(format!(
                    "list::range({start}, {end}), a range too long to build"
                ))));
            }
            Ok(Value::list((start..end).map(Value::Int).collect()))
        }
        (StdFn::ListSort, [xs]) => {
            let mut items = list(xs)?.to_vec();
            sort_by_compare(&mut items, |v| v)?;
            Ok(Value::list(items))
        }
        (StdFn::ListUnique, [xs]) => {
            // The first of each group of `==` elements, in order.
            let mut out: Vec<Value> = Vec::new();
            for x in list(xs)? {
                let mut seen = false;
                for kept in &out {
                    if equal(kept, x)? {
                        seen = true;
                        break;
                    }
                }
                if !seen {
                    out.push(x.clone());
                }
            }
            Ok(Value::list(out))
        }
        (StdFn::ListMap, [xs, g]) => {
            let mut out = Vec::new();
            for x in list(xs)? {
                out.push(interp.apply(g, vec![x.clone()])?);
            }
            Ok(Value::list(out))
        }
        (StdFn::ListFold, [xs, initial, g]) => {
            let mut acc = initial.clone();
            for x in list(xs)? {
                acc = interp.apply(g, vec![acc, x.clone()])?;
            }
            Ok(acc)
        }
        (StdFn::ListFirst, [xs]) => Ok(list(xs)?
            .first()
            .map_or_else(|| interp.none(), |x| interp.some(x.clone()))),
        (StdFn::ListGet, [xs, index]) => {
            let xs = list(xs)?;
            let found = usize::try_from(int(index)?).ok().and_then(|i| xs.get(i));
            Ok(found.map_or_else(|| interp.none(), |x| interp.some(x.clone())))
        }
        (StdFn::StringLength, [s]) => len_value(text(s)?.chars().count()),
        (StdFn::StringSlice, [s, start, end]) => {
            let s = text(s)?;
            let len = s.chars().count();
            let (start, end) = (clamp(int(start)?, len), clamp(int(end)?, len));
            let taken: String = s
                .chars()
                .skip(start)
                .take(end.saturating_sub(start))
                .collect();
            Ok(Value::str(&taken))
        }
        (StdFn::StringUppercase, [s]) => Ok(Value::str(&text(s)?.to_uppercase())),
        (StdFn::StringLowercase, [s]) => Ok(Value::str(&text(s)?.to_lowercase())),
        (StdFn::StringReverse, [s]) => Ok(Value::str(&text(s)?.chars().rev().collect::<String>())),
        // `char::is_whitespace` is Unicode's `White_Space` (D-310).
        (StdFn::StringTrim, [s]) => Ok(Value::str(text(s)?.trim())),
        (StdFn::StringJoin, [parts, separator]) => {
            let separator = text(separator)?;
            let mut out = String::new();
            for (i, part) in list(parts)?.iter().enumerate() {
                if i > 0 {
                    out.push_str(separator);
                }
                out.push_str(text(part)?);
            }
            Ok(Value::str(&out))
        }
        (StdFn::StringRepeat, [s, count]) => {
            let s = text(s)?;
            let count = clamp(int(count)?, usize::MAX);
            if s.len().saturating_mul(count) > MAX_REPEAT {
                return Err(Flow::Error(EvalError::Unsupported(
                    "string::repeat of a result too long to build".to_owned(),
                )));
            }
            Ok(Value::str(&s.repeat(count)))
        }
        (StdFn::StringSplit, [s, separator]) => {
            let (s, separator) = (text(s)?, text(separator)?);
            // An empty separator gives the code points (D-315).
            let parts: Vec<Value> = if separator.is_empty() {
                s.chars()
                    .map(|c| Value::str(c.encode_utf8(&mut [0; 4])))
                    .collect()
            } else {
                s.split(separator).map(Value::str).collect()
            };
            Ok(Value::list(parts))
        }
        (StdFn::StringStartsWith, [s, prefix]) => {
            Ok(Value::Bool(text(s)?.starts_with(text(prefix)?)))
        }
        (StdFn::StringContains, [s, part]) => Ok(Value::Bool(text(s)?.contains(text(part)?))),
        (StdFn::StringEndsWith, [s, suffix]) => Ok(Value::Bool(text(s)?.ends_with(text(suffix)?))),
        (StdFn::MapNew, []) => Ok(Value::Map(Rc::new(Vec::new()))),
        (StdFn::MapFromList, [pairs]) => {
            // As if each pair were inserted in turn (D-329, D-588).
            let mut m = Vec::new();
            for pair in list(pairs)? {
                let Value::Tuple(items) = pair else {
                    return malformed(format!("map::from_list of {}", pair.kind()));
                };
                let [key, item] = items.as_slice() else {
                    return malformed("map::from_list of a tuple that is not a pair");
                };
                map_insert(&mut m, key.clone(), item.clone())?;
            }
            Ok(Value::Map(Rc::new(m)))
        }
        (StdFn::MapInsert, [m, key, item]) => {
            let mut m = entries(m)?.to_vec();
            map_insert(&mut m, key.clone(), item.clone())?;
            Ok(Value::Map(Rc::new(m)))
        }
        (StdFn::MapRemove, [m, key]) => {
            let mut m = entries(m)?.to_vec();
            if let Some(index) = map_find(&m, key)? {
                m.remove(index);
            }
            Ok(Value::Map(Rc::new(m)))
        }
        (StdFn::MapGet, [m, key]) => {
            let m = entries(m)?;
            let found = map_find(m, key)?.and_then(|i| m.get(i));
            Ok(found.map_or_else(|| interp.none(), |(_, v)| interp.some(v.clone())))
        }
        (StdFn::MapSize, [m]) => len_value(entries(m)?.len()),
        (StdFn::MapToSortedList, [m]) => {
            let mut pairs = entries(m)?.to_vec();
            sort_by_compare(&mut pairs, |(k, _)| k)?;
            Ok(Value::list(
                pairs
                    .into_iter()
                    .map(|(k, v)| Value::Tuple(Rc::new(vec![k, v])))
                    .collect(),
            ))
        }
        (StdFn::IntMin, [a, b]) => Ok(Value::Int(int(a)?.min(int(b)?))),
        (StdFn::IntMax, [a, b]) => Ok(Value::Int(int(a)?.max(int(b)?))),
        (StdFn::IoPrintln, [s]) => {
            let s = text(s)?;
            interp.stdout.push_str(s);
            interp.stdout.push('\n');
            Ok(Value::Nil)
        }
        (StdFn::FiberScope, [body]) => {
            // The body runs on the calling fiber; its panic stops at the
            // scope, which returns it as `Err` and prints nothing (§11.7).
            match interp.apply(body, vec![Value::Scope]) {
                Ok(v) => Ok(Value::data(&interp.prelude.ok, vec![v])),
                Err(Flow::Panic { message, .. }) => Ok(Value::data(
                    &interp.prelude.err,
                    vec![Value::Panic(Rc::from(message.as_str()))],
                )),
                Err(error) => Err(error),
            }
        }
        (StdFn::FiberMessage, [failure]) => match failure {
            Value::Panic(message) => Ok(Value::Str(message.clone())),
            other => malformed(format!("fiber::message of {}", other.kind())),
        },
        _ => Err(bad()),
    }
}
