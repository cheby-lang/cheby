# 5. Expressions

Cheby is expression-oriented. Function bodies, blocks, `case` expressions and `use` statements all produce values. There are no loops, no `if`, no `return` and no assignment (D-002, D-017).

## 5.1 Blocks and statements

```ebnf
block       = "{" [ NL ] [ stmt { NL stmt } ] [ NL ] "}" ;
stmt        = let_stmt
            | let_assert_stmt
            | use_stmt
            | assert_stmt
            | local_fn
            | expr ;
```

A block is a sequence of statements separated by newlines (D-051, [§2.8](02-lexical-structure.md#28-newlines-and-statement-separation)). Statements run in order. The value of a block is:

- the value of its last statement, if that statement is an expression,
- `Nil`, if the block is empty or its last statement is a `let`, `let assert`, `assert` or local function ([§3.6](03-types.md#36-nil)).

A `use` statement consumes the rest of its block ([§5.10](#510-use)), so it is always effectively the last statement, and the block's value is the value of the `use`.

A block is itself an expression and may appear wherever an expression may, for example as a `case` arm body. Bindings introduced inside a block are visible only in the rest of that block.

```cheby
let area = {
  let w = rect.width
  let h = rect.height
  w * h
}
```

An expression statement that is not the last statement of its block is evaluated and its value discarded. Discarding a value of type `Result` this way is a warning, because it silently drops an error. The warning is silenced by binding the value explicitly with `let _ = …`. Other values may be discarded silently (D-153).

```cheby
fn save_all(items: List<Item>) -> Nil {
  write_log("saving")              // warning: discarded `Result`
  let _ = write_log("saving")      // no warning
  Nil
}
```

There is no `return`. A function returns the value of its body block. Early exits are written with `case` or `use`.

## 5.2 Let bindings

```ebnf
let_stmt    = "let" pattern [ ":" type ] "=" expr ;
```

`let` evaluates the expression and matches it against the pattern, binding the pattern's names for the rest of the block. The pattern must be **irrefutable**: it must match every value of the type ([§6.1](06-patterns.md#61-pattern-syntax)). Refutable patterns need `let assert` ([§11.4](11-errors-and-panics.md#114-let-assert)).

The optional type annotation constrains the expression's type.

```cheby
let count = 10
let total: Float = 0.0
let (word, n) = entry
let Point { x, y } = origin
```

### 5.2.1 Immutability

A binding can never be changed after it is made. There is no assignment operator and no `mut` (D-017).

### 5.2.2 Scope

A binding is visible from the statement after the one that introduces it to the end of the enclosing block, including inside nested blocks, closures, local functions, `case` arms and `use` callbacks within that range. A binding's own initializer cannot see it.

### 5.2.3 No shadowing

A new binding must not have the same name as any name already in scope at that point (D-051, D-062). "In scope" includes:

- earlier bindings in the same block and in all enclosing blocks,
- parameters of the enclosing function, closure or local function,
- names bound by enclosing `case` arm patterns and `use` binders,
- every top-level function and constant of the current module, whether declared before or after,
- every module name and value brought into scope by an `import` ([§7.5](07-modules-and-packages.md#75-imports)).

The rule applies to every construct that introduces a value name: `let`, `let assert`, pattern bindings in `case` arms, `use` binders, function and closure parameters, and local function names (D-062).

Names that are never in scope at the same time do not conflict. Two sibling `case` arms, or two closures that are not nested in each other, may bind the same name.

A violation is a compile error. The diagnostic should point to both bindings and suggest a new name (D-062).

```cheby
import std::list
import std::string

fn process(input: String) -> Int {
  let trimmed = string::trim(input)
  let trimmed = string::lowercase(trimmed)   // error: `trimmed` is already bound
  let list = [1, 2, 3]                        // error: `list` is an imported module
  0
}
```

_Rationale:_ within a function, a name always refers to one value (ADR-0021).

## 5.3 Literals

```ebnf
literal     = INT | FLOAT | STRING | RAW_STRING ;
```

The lexical forms are defined in [§2.5](02-lexical-structure.md#25-literals). The types of numeric literals are defined in [§3.3.4](03-types.md#334-numeric-literals).

### 5.3.1 Negative literals

When unary `-` is applied directly to an integer or float literal, the pair is treated as one negative literal for type inference and range checking. `-128` is a valid `I8` literal and `-9223372036854775808` is a valid `Int` literal, even though `128` and `9223372036854775808` are out of range for those types.

### 5.3.2 String interpolation

An interpolation `{path}` inside a string literal ([§2.5.4](02-lexical-structure.md#254-string-interpolation)) is replaced by text computed from the value of `path`:

- `{path}` calls the `show` function of the value's type. The type must satisfy `Show` ([§8.8](08-interfaces.md#88-standard-interfaces)), which is checked at the end of the body ([§3.12.4](03-types.md#3124-order-of-inference)). If it does not, it is a compile error that suggests `{path:?}` (D-067, D-086).
- `{path:?}` uses built-in debug printing ([§3.13](03-types.md#313-equality-hashing-and-debug-printing)), which works for every type (D-086). Functions and handles print as placeholders such as `<fn>` (D-233).

The path is evaluated like a field-access expression ([§5.11.3](#5113-field-access)). Interpolations are evaluated left to right, and the string is built from the literal parts and the interpolated texts in order.

```cheby
let message = "user {user.name} has {count} items, state {state:?}"
```

## 5.4 Operators

### 5.4.1 Precedence and associativity

From highest to lowest (D-108):

| Level | Operators                                    | Associativity |
| ----- | -------------------------------------------- | ------------- |
| 1     | call `f(…)`, field `.name`, tuple index `.0` | left          |
| 2     | unary `-`, `!`                               | prefix        |
| 3     | `*` `/` `%`                                  | left          |
| 4     | `+` `-`                                      | left          |
| 5     | `<<` `>>`                                    | left          |
| 6     | `&`                                          | left          |
| 7     | `^`                                          | left          |
| 8     | `\|`                                         | left          |
| 9     | `\|>`                                        | left          |
| 10    | `==` `!=` `<` `<=` `>` `>=`                  | none          |
| 11    | `&&`                                         | left          |
| 12    | `\|\|`                                       | left          |

Comparison operators do not chain. `a < b < c` and `a == b == c` are compile errors that suggest `&&` (D-108).

Because `|>` binds tighter than comparisons, `xs |> list::len == 3` means `(xs |> list::len) == 3` (D-108).

The operand type of an arithmetic, comparison or bitwise operator need not be known when the operator is checked. The choice between the built-in operation and the operand type's functions is made at the end of the body ([§3.12.4](03-types.md#3124-order-of-inference)) (D-234).

### 5.4.2 Built-in arithmetic

For the built-in numeric types, both operands must have the **same type**, and the result has that type. There are no implicit conversions (D-047).

| Operator                  | Integer types                                                     | `Float`, `F32`      |
| ------------------------- | ----------------------------------------------------------------- | ------------------- |
| `a + b`, `a - b`, `a * b` | checked, panics on overflow (D-025)                               | IEEE 754            |
| `a / b`                   | truncates toward zero, panics on zero divisor or overflow (D-069) | IEEE 754            |
| `a % b`                   | remainder with the sign of `a`, panics on zero divisor (D-092)    | IEEE `fmod` (D-154) |
| `-a`                      | checked negation, panics on overflow                              | IEEE 754 negation   |

`%` on `Float` and `F32` has IEEE `fmod` semantics: the result has the sign of the dividend `a`, as for integers (D-154).

On the JS target, every `Int` result is additionally checked against the safe-integer range ([§3.3.1](03-types.md#331-int)) (D-037).

### 5.4.3 Overloaded arithmetic

For other types, `+ - * /` and unary `-` call the `add`, `sub`, `mul`, `div` and `neg` functions of the operand type through the operator interfaces ([§8.7](08-interfaces.md#87-operator-interfaces)) (D-034, D-047, D-118). Both operands must have the same type.

`String` satisfies `Add`, so `+` concatenates strings (D-067):

```cheby
let full = first + " " + last
```

### 5.4.4 Comparison

- `a == b` and `a != b` use built-in structural equality ([§3.13](03-types.md#313-equality-hashing-and-debug-printing)) (D-033). Both operands must have the same type. Equality cannot be overloaded (D-047). It panics if it reaches a function value or a handle ([§3.13](03-types.md#313-equality-hashing-and-debug-printing)) (D-070, D-233).
- `a < b`, `a <= b`, `a > b` and `a >= b` call the `compare` function of the operand type and test the resulting `Order` (D-047). Both operands must have the same type, which must satisfy `Compare` ([§8.7](08-interfaces.md#87-operator-interfaces)). The table writes the call in the interface-qualified form `Compare::compare` (D-173).

| Expression | Meaning                             |
| ---------- | ----------------------------------- |
| `a < b`    | `Compare::compare(a, b) == Less`    |
| `a <= b`   | `Compare::compare(a, b) != Greater` |
| `a > b`    | `Compare::compare(a, b) == Greater` |
| `a >= b`   | `Compare::compare(a, b) != Less`    |

### 5.4.5 Logical operators

`&&`, `||` and `!` take and return `Bool` (D-092).

- `a && b` evaluates `a`. If it is `False`, the result is `False` and `b` is not evaluated. Otherwise the result is `b`.
- `a || b` evaluates `a`. If it is `True`, the result is `True` and `b` is not evaluated. Otherwise the result is `b`.
- `!a` is logical negation.

The right operand of `&&` and `||` is in tail position when the whole expression is ([§5.15](#515-evaluation-order-and-tail-position)) (D-120).

### 5.4.6 Bitwise operators

`&`, `|`, `^`, `<<` and `>>` are defined on all integer types, with both operands of the same type (D-092). They cannot be overloaded (D-118).

- `&`, `|` and `^` operate on the two's-complement representation at the type's width. For `Int` the width is 64 bits, on every target.
- `a << n` is `a` multiplied by 2ⁿ, and panics if the result overflows, like multiplication.
- `a >> n` is an arithmetic shift for signed types and a logical shift for unsigned types.
- A shift amount `n` that is negative, or not less than the type's width in bits, panics (D-092).

On JS, `Int` results of bitwise operators are checked against the safe-integer range like every other `Int` result.

## 5.5 Function calls

```ebnf
call        = postfix "(" [ arg { "," arg } [ "," ] ] ")" ;
arg         = expr | "_" ;
```

A call evaluates the callee, then the arguments from left to right, then calls the function (D-120). The number of arguments must equal the number of parameters. There is no implicit partial application. An argument written `_` makes the call a function capture ([§5.6](#56-function-capture)).

The callee may be any expression of function type: a function name, a qualified path `module::f`, a constructor, a local binding or a parenthesized expression.

Explicit type arguments are written with the turbofish after the function name, `list::new::<Int>()` (D-049, [§3.12.6](03-types.md#3126-explicit-type-arguments)).

For type checking, closure and capture arguments are checked after the other arguments, so a closure may come before the data it works on ([§3.12.4](03-types.md#3124-order-of-inference)) (D-234). Evaluation stays left to right (D-120).

## 5.6 Function capture

A call whose argument list contains exactly one `_` argument is a **function capture** (D-050). It produces a one-parameter function:

```cheby
list::map(numbers, int::add(1, _))
// is equivalent to
list::map(numbers, fn(x) { int::add(1, x) })
```

A `_` belongs to the innermost call that has it as a whole argument (D-202). A capture may itself be an argument of another call, which then receives the capture as an ordinary function value: `f(a, g(_))` means `f(a, fn(x) { g(x) })`.

- At most one `_` may appear in one argument list. `f(_, _)` is a compile error.
- A `_` must be a whole argument. `f(_ + 1)` and `f(x._)` are compile errors, and so is a `_` anywhere else in an expression.

The other arguments of a capture are evaluated once, from left to right, when the capture expression itself is evaluated, and the resulting closure holds their values (D-155). Calling the closure does not evaluate them again, so their side effects happen exactly once. This deliberately differs from Gleam, where they are evaluated at each call.

```cheby
let add_next = int::add(next_id(), _)   // `next_id()` runs here, once
let a = add_next(1)
let b = add_next(2)                      // same id as for `a`
```

## 5.7 Anonymous functions

```ebnf
closure     = "fn" "(" [ closure_param { "," closure_param } [ "," ] ] ")" [ "->" type ] block ;
closure_param = LOWER [ ":" type ] ;
```

An anonymous function (closure) uses the Gleam form `fn(x) { … }` (D-036). Parameter types and the return type are optional and inferred when omitted (D-011).

- A closure may refer to any binding in scope where it is written. Because all values are immutable, capturing a binding captures its value.
- A closure's parameters follow the no-shadowing rule ([§5.2.3](#523-no-shadowing)).
- A closure is monomorphic: it has exactly one function type, fixed by inference within the enclosing body (D-123, [§3.12.3](03-types.md#3123-inference-inside-bodies)).
- A closure passed as an argument takes its parameter types from the callee's signature, and is checked after the call's other arguments ([§3.12.4](03-types.md#3124-order-of-inference)) (D-234).
- A tail call inside a closure's body is guaranteed like any other tail call (D-015).

```cheby
let double = fn(x) { x * 2 }
let parse_all = fn(lines: List<String>) -> List<Int> {
  list::filter_map(lines, int::parse)
}
```

## 5.8 Local named functions

```ebnf
local_fn    = "fn" LOWER "(" [ closure_param { "," closure_param } [ "," ] ] ")" [ "->" type ] block ;
```

A statement of the form `fn name(…) { … }` inside a block declares a local named function (D-110):

- It may refer to bindings in scope before it, like a closure.
- Inside its own body, its name refers to itself, so it may call itself recursively. Only self-recursion is supported: a local function cannot refer to local functions declared after it (D-110).
- Its name is a binding and follows the no-shadowing rule ([§5.2.3](#523-no-shadowing)).
- Parameter and return types are optional and inferred. It is monomorphic (D-123).
- Tail calls to itself, and all other tail calls in its body, are guaranteed (D-015).

Local functions are the usual way to write loops that carry state (ADR-0001):

```cheby
fn count_positive(numbers: List<Int>) -> Int {
  fn step(acc, rest) {
    case rest {
      [] => acc
      [first, ..tail] when first > 0 => step(acc + 1, tail)
      [_, ..tail] => step(acc, tail)
    }
  }
  step(0, numbers)
}
```

A local function declaration is a statement, not an expression, and does not produce a value. To pass a function as a value, use its name after the declaration, or use a closure.

## 5.9 Case expressions

`case` is the only branching construct (D-002). There is no `if`.

```ebnf
case_expr   = "case" subjects "{" [ NL ] case_arm { NL case_arm } [ NL ] "}"
            | "case" "{" [ NL ] cond_arm { NL cond_arm } [ NL ] "}" ;
subjects    = subject_expr { "," subject_expr } ;   (* subject_expr: §5.9.6 *)
case_arm    = pattern { "," pattern } [ "when" expr ] "=>" expr ;
cond_arm    = ( expr | "_" ) "=>" expr ;
```

### 5.9.1 Matching

`case` evaluates its subjects from left to right, then tries the arms in order. An arm matches when each of its patterns matches the corresponding subject ([chapter 6](06-patterns.md)) and, if it has a guard, the guard evaluates to `True` with the pattern's bindings in scope. The value of the `case` is the value of the first matching arm's body, evaluated with that arm's bindings in scope.

- All arm bodies must have the same type, which is the type of the `case`. Arms of type `Never`, such as `panic`, take the type of the other arms ([§3.10](03-types.md#310-never)) (D-235).
- Each arm must have exactly as many patterns as there are subjects (D-052).
- Arms are separated by newlines and never end with a comma (D-107).
- An arm body is a single expression. A block `{ … }` groups several statements.

```cheby
fn describe(shape: Shape) -> String {
  case shape {
    Circle { radius } when radius == 0.0 => "a point"
    Circle { radius } => "a circle of radius {radius}"
    Rect { width, height } when width == height => "a square"
    Rect { .. } => "a rectangle"
    Dot => "a dot"
  }
}
```

### 5.9.2 Multiple subjects

`case a, b { … }` matches several subjects at once, with one pattern per subject in each arm (D-052):

```cheby
fn fizzbuzz(n: Int) -> String {
  case n % 3, n % 5 {
    0, 0 => "FizzBuzz"
    0, _ => "Fizz"
    _, 0 => "Buzz"
    _, _ => int::to_string(n)
  }
}
```

### 5.9.3 Guards

A guard `when expr` follows the patterns of an arm (D-052). The guard must have type `Bool` and may use the arm's bindings. Guards are not considered by exhaustiveness checking ([§6.9](06-patterns.md#69-exhaustiveness-and-reachability)).

### 5.9.4 Exhaustiveness

A `case` with subjects must be exhaustive: every possible combination of subject values must be matched by some arm, ignoring guards (D-022). A non-exhaustive `case` is a compile error that lists missing patterns. An arm that can never match is a warning ([§6.9](06-patterns.md#69-exhaustiveness-and-reachability)).

### 5.9.5 Case without a subject

`case { … }` with no subject is a chain of conditions (D-109):

```cheby
fn sign(n: Int) -> String {
  case {
    n < 0 => "negative"
    n == 0 => "zero"
    _ => "positive"
  }
}
```

- Each arm's left side is an expression of type `Bool`, except the last, which must be `_` (D-109).
- The conditions are evaluated in order. The first arm whose condition is `True` is taken, and later conditions are not evaluated. If none is `True`, the `_` arm is taken.
- Arm bodies follow the same rules as in [§5.9.1](#591-matching).

_Rationale:_ this avoids nested `case b { True => … False => case … }` chains while keeping `case` the only branch. It is part of `case`, not a hidden `if` (ADR-0031).

### 5.9.6 Parsing subjects

In the subject position of a `case`, an `UPPER` name followed by `{` is never parsed as a record construction, because the `{` opens the arms. A record construction or record update used as a subject must be parenthesized. A `{` right after `case` always starts a subjectless `case`, so a block used as a subject must be parenthesized too.

```cheby
case (Point { x: 0.0, y: 0.0 }) { … }
```

## 5.10 Use

```ebnf
use_stmt    = "use" [ use_binder { "," use_binder } ] "<-" expr ;
use_binder  = pattern [ ":" type ] ;
```

`use` turns the rest of the enclosing block into a callback and passes it as the **last argument** of a function call (D-018, D-023). It is the general Gleam form: it works with any function, not only with `Result` or `Option` (D-023).

The right side must be a call `f(a, …)` or a function expression `f`. The statement

```cheby
use p1, p2 <- f(a, b)
rest…
```

is equivalent to

```cheby
f(a, b, fn(t1, t2) {
  let p1 = t1
  let p2 = t2
  rest…
})
```

where `t1` and `t2` are fresh names. If the right side is `f` without arguments, it means `f(fn(…) { … })`. With no binders, `use <- f(a)` passes a zero-parameter callback.

- The binders are patterns and must be irrefutable ([§6.1](06-patterns.md#61-pattern-syntax)). Binder names follow the no-shadowing rule.
- The callback's body is the rest of the enclosing block. If nothing follows the `use`, the body is empty and its value is `Nil` ([§5.1](#51-blocks-and-statements)).
- The value of the `use` statement is the value of the call, and it is the value of the enclosing block.
- The callback's parameter and return types are inferred from the called function's signature.
- When the `use` statement is in tail position, the call it desugars to is a tail call.

`use` handles early returns on errors ([chapter 11](11-errors-and-panics.md)), resource scopes and fiber scopes:

```cheby
fn load_config(path: String) -> Result<Config, ConfigError> {
  use text <- result::try(read(path))
  use json <- result::try(parse_json(text))
  decode_config(json)
}

fn with_log(path: String) -> Result<Nil, io::Error> {
  use handle <- file::with_open(path)
  file::write_line(handle, "started")
}
```

## 5.11 Constructors and records

```ebnf
constructor   = ctor_path "{" [ NL ] [ field_init { "," [ NL ] field_init } [ "," ] ] [ NL ] "}" ;
record_update = UPPER "{" [ NL ] ".." expr { "," [ NL ] field_init } [ "," ] [ NL ] "}" ;
ctor_path     = [ LOWER "::" ] UPPER ;
(* a unit or positional constructor used as a value, `None`, `Some(1)` or `Some`, is a path_expr, §5.17 *)
field_init  = LOWER [ ":" expr ] ;
```

### 5.11.1 Construction

- A unit variant is written by its name: `Red`, `None`, `shape::Dot`.
- A positional variant is called like a function: `Some(1)`, `Node(left, value, right)`. Used without arguments, it is a function value, for example `list::map(xs, Some)`.
- A named-field variant is built with braces and must initialize every field exactly once, in any order: `Circle { radius: 1.0 }`.
- **Field punning** (D-113): `Point { x, y }` is short for `Point { x: x, y: y }`.

Field initializers are evaluated in the order written.

Constructing a value of a `priv` type is possible only in its own module (D-151). Constructing a value of a type from another package requires the type to be `exposed` ([§4.3.4](04-declarations.md#434-opacity-and-exposed)) (D-048).

### 5.11.2 Record update

`T { ..base, field: value, … }` produces a copy of `base` with the listed fields replaced (D-035). `T` is the **type name**, not a constructor name, and may be qualified with a module (D-156):

- `base` is evaluated first, then the field values in the order written.
- Every listed field must exist in the type and must be accessible under the rules of [§5.11.3](#5113-field-access). For a type with several variants, every variant must have each listed field with the same type (D-071).
- The result has the same variant as `base` (D-156).
- At least one field must be listed.

For a single-variant type, the type name and the constructor name coincide. For a multi-variant type, the update works on whichever variant `base` has, which is why every variant must have the listed fields (D-071, D-156).

With Perceus reuse, updating a uniquely owned record happens in place ([§9.3](09-memory-model.md#93-reuse)).

```cheby
let moved = Point { ..origin, x: 10.0 }
```

### 5.11.3 Field access

```ebnf
field_access = postfix "." ( LOWER | DEC_INT ) ;
```

- `x.name` reads a named field. If `x`'s type has several variants, every variant must have a field `name` of the same type (D-071). Otherwise, use `case`.
- `x`'s type must already be known at the point of the access, from an annotation, a signature or a use checked earlier (D-213). Statements are checked in order, and closure arguments after the other arguments of a call ([§3.12.4](03-types.md#3124-order-of-inference)) (D-234), so `list::filter(books, fn(entry) { entry.format == format::Paperback })` knows `entry`'s type from `books`. Field names are not unique across types, so the compiler does not guess the type from the field name or wait for later uses. Otherwise it is a compile error that asks for a type annotation. The same rule applies to record update ([§5.11](#511-constructors-and-records)). In local functions and closures, whose parameter types are inferred, this usually means annotating the parameter: `fn step(current: Input) { current.rest }`.
- `x.0`, `x.1`, … read tuple elements ([§3.7](03-types.md#37-tuples)). The index must be less than the tuple's length. Positional variant fields are not accessible with `.0`; use a pattern.
- Reading fields of a `priv` type is possible only in its own module (D-151). Reading fields of a type from another package requires the type to be `exposed` (D-048).

The `.` operator is only for field access. Module members are accessed with `::` (D-060).

## 5.12 Tuples

```ebnf
tuple       = "(" expr "," expr { "," expr } [ "," ] ")" ;
paren       = "(" expr ")" ;
```

A tuple expression has two or more elements (D-076). Elements are evaluated from left to right. `(e)` is a parenthesized expression, not a tuple.

```cheby
let pair = (name, 42)
let name_again = pair.0
```

## 5.13 List literals

```ebnf
list        = "[" [ list_item { "," list_item } [ "," ] ] "]" ;
list_item   = expr | ".." expr ;
```

`[a, b, c]` builds a `List` with the given elements, in order. All elements must have the same type. `[]` is the empty list, and its element type is inferred.

A **spread** item `..xs`, where `xs` is a `List` of the same element type, inserts all elements of `xs` at that position. Spreads may appear any number of times and in any position, so a list literal with spreads is a concatenation (D-157). Concatenation is cheap on the RRB `List` (D-039).

```cheby
let all = [0, ..xs, 99, ..ys]
let with_last = [..xs, 99]
```

Items are evaluated from left to right.

## 5.14 Panic and todo

```ebnf
panic_expr  = "panic" [ "as" expr ] ;
todo_expr   = "todo" [ "as" expr ] ;
```

`panic` and `todo` are keyword expressions of type `Never`, so they can be used where any type is expected ([§3.12.5](03-types.md#3125-expected-types-and-conversions)) (D-161, D-235). The optional message follows `as` and is an expression of type `String`: `panic`, `panic as "computation failed"`, `todo as "area of a dot"` (D-161). Their behavior is defined in [§11.6](11-errors-and-panics.md#116-panic-and-todo).

## 5.15 Evaluation order and tail position

### 5.15.1 Evaluation order

Evaluation is strict and left to right (D-120):

- In a call, the callee first, then the arguments in order.
- In a binary operator, the left operand, then the right, except for the short-circuit rules of `&&` and `||`.
- In tuples, lists, constructors and record updates, items in the order written.
- In `x |> f(a)`, `x` first, then `a`.
- In `case`, the subjects in order, then guards in arm order.

### 5.15.2 Tail position

An expression is in **tail position** if its value becomes the result of the enclosing function or closure with no further work. The following are tail positions (D-120):

- the body block of a function, closure or local function,
- the last statement of a block that is in tail position,
- the body of each arm of a `case` in tail position,
- the right operand of `&&` or `||` in tail position,
- the call that a `|>` in tail position desugars to,
- the call that a `use` in tail position desugars to.

### 5.15.3 Guaranteed tail calls

Every call in tail position is a **tail call** and must not grow the stack (D-015). This includes calls to the same function, mutual recursion between top-level functions, calls to local functions, closures and function values, and calls through `dyn` interface values. A program may loop forever through tail calls in constant stack space (ADR-0006).

Calls to `@external` functions are not covered by the guarantee ([§12.8](12-targets-and-ffi.md#128-tail-calls)).

## 5.16 Pipe

```ebnf
pipe        = expr "|>" expr ;
```

`x |> rhs` passes `x` into a function call (D-050):

| Right side                            | Meaning                                      |
| ------------------------------------- | -------------------------------------------- |
| a call with a `_` argument, `f(a, _)` | `x` fills the hole: `f(a, x)`                |
| any other call, `f(a, b)`             | `x` becomes the first argument: `f(x, a, b)` |
| any other expression `g`              | `g` is called with `x`: `g(x)`               |

Only a `_` that is a whole argument of the call on the right side counts as its hole (D-221). A `_` nested in another argument belongs to that inner call ([§5.6](#56-function-capture)), so `x |> f(a, g(_))` means `f(x, a, fn(y) { g(y) })`. A `_` therefore means the same thing with or without a pipe in front.

`x` is evaluated before the rest of the right side ([§5.15.1](#5151-evaluation-order)). To pipe into a function returned by a call, parenthesize the call: `x |> (make_handler(config))`.

```cheby
let names =
  users
  |> list::filter(is_active)
  |> list::map(fn(user) { user.name })
  |> list::sort(string::compare)
  |> string::join(_, ", ")
```

## 5.17 Paths

```ebnf
path_expr   = [ LOWER "::" ] ( LOWER | UPPER ) [ "::" type_args ]
            | [ LOWER "::" ] UPPER "::" LOWER ;             (* interface function, Show::show, D-173 *)
```

An unqualified name refers to a local binding, a top-level item of the current module, an imported value or a prelude constructor ([§7.7](07-modules-and-packages.md#77-namespaces-and-name-resolution)). A qualified name `module::name` refers to an item of an imported module that is visible to the current module: a package-visible or `pub` item of a module in the same package, or a `pub` item of a module in another package, never a `priv` item ([§7.3](07-modules-and-packages.md#73-visibility), D-060, D-162).
