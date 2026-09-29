# 2. Lexical structure

This chapter defines how source text is split into tokens, including the rules that decide when a newline ends a statement.

## 2.1 Source files

A Cheby source file is UTF-8 text. A byte-order mark at the start of the file is ignored. Any other invalid UTF-8 is a compile error.

Source files use the extension `.cheby` (D-126).

Line terminators are LF (`U+000A`) and CRLF (`U+000D U+000A`). A lone CR is a compile error. Whitespace is space (`U+0020`) and horizontal tab (`U+0009`). Whitespace separates tokens and is otherwise insignificant. Newlines are significant as described in [§2.8](#28-newlines-and-statement-separation).

## 2.2 Comments

There are three kinds of comments, and all run to the end of the line (D-078):

| Form    | Meaning                                                                              |
| ------- | ------------------------------------------------------------------------------------ |
| `// …`  | ordinary comment, ignored                                                            |
| `/// …` | doc comment, attached to the item, variant, field or interface function that follows |
| `//! …` | module doc comment, allowed only before the first item of a file                     |

There are no block comments (D-078). A comment does not affect newline handling: a line that ends with a comment ends where the comment starts.

A `///` comment must be followed, possibly after more `///` lines and attributes, by a top-level declaration, a variant or field of a type declaration, or a function of an interface declaration (D-211, D-227). A `///` comment anywhere else is a compile error.

## 2.3 Identifiers

There are two identifier classes. Their spelling is enforced by the lexer, which is how the naming conventions in D-059 are enforced:

```ebnf
LOWER = ( "a"…"z" | "_" ) { "a"…"z" | "0"…"9" | "_" } ;   (* snake_case *)
UPPER = "A"…"Z" { "A"…"Z" | "a"…"z" | "0"…"9" } ;          (* PascalCase *)
```

- `LOWER` names values, functions, constants, modules and packages. Every binding is a `LOWER` name.
- `UPPER` names types, type parameters, constructors and interfaces.

An identifier that starts with a lower-case letter but contains an upper-case letter (`myValue`), or starts with an upper-case letter and contains `_` (`My_Type`), is a compile error with a suggested spelling.

The single underscore `_` is not an identifier. It is the wildcard pattern and the function-capture placeholder. A `LOWER` identifier that starts with `_` (`_unused`) is a binding that suppresses the unused-variable warning (D-080).

Identifiers are ASCII only (D-127). A non-ASCII letter in an identifier is a compile error. This keeps the case rules of D-059 unambiguous and avoids confusable identifiers.

## 2.4 Keywords

The following words are reserved and cannot be used as `LOWER` identifiers:

```
as        assert    case      const     dyn       exposed   fn
import    interface let       panic     priv      pub       test
todo      type      use       when
```

The following words are also reserved, although Cheby gives them no meaning (D-128, D-240):

```
if        else      for       while     loop      break     continue
return    mut       impl      trait     struct    enum      match
async     await     macro     self      super     where     yield
go
```

They are reserved so the compiler can give targeted errors for constructs Cheby deliberately lacks (for example "Cheby has no `if`, use `case`", or "Cheby has no `go`, spawn a fiber in a scope" for Go's `go` statement), and so they stay free for later additions, since adding keywords after 1.0 would break code (D-075, D-128).

`Self` is a reserved `UPPER` identifier, valid only inside interface declarations ([chapter 8](08-interfaces.md)).

`True`, `False`, `Nil`, `Ok`, `Err`, `Some`, `None`, `Less`, `Equal` and `Greater` are not keywords. They are constructors from the prelude ([§7.6](07-modules-and-packages.md#76-prelude)).

## 2.5 Literals

### 2.5.1 Integer literals

```ebnf
INT      = DEC_INT | HEX_INT | BIN_INT | OCT_INT ;
DEC_INT  = "0" | nonzero_digit { digit | "_" } ;
HEX_INT  = "0x" hex_digit { hex_digit | "_" } ;
BIN_INT  = "0b" bin_digit { bin_digit | "_" } ;
OCT_INT  = "0o" oct_digit { oct_digit | "_" } ;
```

Underscores are separators and carry no meaning. An integer literal has no sign: `-5` is unary minus applied to `5`, but see [§5.3](05-expressions.md#53-literals) for how negative literals are range-checked. There are no type suffixes (D-084). A literal's type comes from context ([§3.3.4](03-types.md#334-numeric-literals)).

Octal literals `0o…` are allowed alongside the decimal, hex and binary literals of D-084 (D-129), since Unix permission bits are a common FFI need.

A decimal literal must not have a leading zero unless it is `0`. This prevents confusion with C-style octal.

### 2.5.2 Float literals

```ebnf
FLOAT    = DEC_INT "." digit { digit | "_" } [ exponent ]
         | DEC_INT exponent ;
exponent = ( "e" | "E" ) [ "+" | "-" ] digit { digit | "_" } ;
```

Digits are required on both sides of the `.`, so `1.` and `.5` are not float literals. This keeps `x.0` (tuple field access) and `..` unambiguous.

### 2.5.3 String literals

```ebnf
STRING      = '"' { string_char | escape | interpolation } '"' ;
RAW_STRING  = "r" { "#" } '"' { any character } '"' { "#" } ;   (* same number of "#" on both sides *)
escape      = "\n" | "\t" | "\r" | "\\" | '\"' | "\{" | "\u{" hex_digit { hex_digit } "}" ;
```

- A string literal may span several lines. Line breaks inside it become part of the value, normalized to LF (D-114).
- The escapes are exactly those listed (D-114). Any other `\` sequence is a compile error. A `\u{…}` escape must name a Unicode scalar value (not a surrogate) with 1 to 6 hex digits.
- An unescaped `{` starts an interpolation ([§2.5.4](#254-string-interpolation)). An unescaped `}` outside an interpolation is an ordinary character.
- A raw string `r"…"` has no escapes and no interpolation (D-114). All characters, including `\`, `{` and line breaks, are taken literally.
- A raw string may be delimited with hashes, `r#"…"#`, `r##"…"##` and so on (D-130). It ends at the first `"` followed by the same number of `#` as after the opening `r`, so `r#"say "hi""#` contains `"`. A raw string without hashes ends at the first `"`.

There is no character literal and no `Char` type (D-114).

### 2.5.4 String interpolation

Inside a non-raw string literal, `{` starts an interpolation, which ends at the next `}`:

```ebnf
interpolation = "{" LOWER { "." ( LOWER | DEC_INT ) } [ ":?" ] "}" ;
```

The contents are a name, optionally followed by field or tuple-index accesses, optionally followed by `:?` (D-115). Whitespace is not allowed inside the braces. Anything else inside `{…}` is a compile error that suggests binding the value to a name first or escaping the brace as `\{`.

The meaning of interpolation is defined in [§5.3.2](05-expressions.md#532-string-interpolation).

```cheby
"hello {name}"             // Show of name
"at {point.x}, {point.y}"  // field paths
"pair: {pair.0}"           // tuple index
"debug: {state:?}"         // built-in debug printing
"a literal \{brace}"       // no interpolation
r"^\d{3}-\d{4}$"           // raw string, no escapes or interpolation
r#"a "quoted" word"#       // raw string containing quotes
```

## 2.6 Operators and punctuation

```
+   -   *   /   %   !   &   |   ^   <<  >>
==  !=  <   <=  >   >=  &&  ||  |>
=   =>  ->  <-  ::  .   ..  ,   :   :?  @   _
(   )   [   ]   {   }
```

The lexer always produces the longest matching token. In type contexts the parser splits `>>` into two `>` tokens, so that `List<List<Int>>` parses.

## 2.7 Attributes

An attribute is `@` followed immediately by a `LOWER` name and an optional parenthesized argument list. The set of attributes is fixed ([§4.8](04-declarations.md#48-attributes), D-200).

## 2.8 Newlines and statement separation

Cheby has no semicolons. Newlines separate statements, `case` arms and top-level items (D-051, D-107). The lexer turns each line break (outside string literals and comments) into an `NL` token. The parser then decides, by the rules below, which `NL` tokens end something and which it ignores (D-231).

The lexer merges consecutive `NL` tokens into one and drops `NL` tokens directly after `{` and directly before `}`. It does not decide continuation by itself.

The parser ignores an `NL` token, so the line continues, if any of the following holds (D-107, D-231):

1. **Open brackets.** The innermost unclosed delimiter is `(`, `[`, or the `<` of a type-argument or type-parameter list (`type_args`, `type_params` or `plain_params` in [Appendix A](appendix-a-grammar.md), including a turbofish `::<…>`). Inside `{`, newlines are significant again, so a closure body passed as an argument still separates its statements by newlines.
2. **Trailing continuation token.** The token before the line break is one of:
   - a token the parser has read as a binary operator (`+ - * / % & | ^ << >> == != < <= > >= && || |>`),
   - `,`, `=`, `=>`, `->` or `<-`.

   A `>` that closes a type-argument or type-parameter list is not a binary operator, and neither is either half of a `>>` split in a type context ([§2.6](#26-operators-and-punctuation)). A line that ends with such a `>` ends normally.

3. **Leading continuation token.** The first token of the next non-blank, non-comment line is `|>`, `&&` or `||`.

In addition:

- A leading `-` on the next line does **not** continue the previous line, so it is always unary (D-107).
- The parser always knows whether a `<` or `>` is a bracket or an operator. In type positions `<` always opens type arguments, and in expressions type arguments appear only after `::` (a turbofish, `list::new::<Int>()`).

_Example:_ continuation in pipelines and conditions.

```cheby
let total =
  orders
  |> list::filter(is_paid)
  |> list::map(price)
  |> list::fold(0, add)

let ok = is_valid(order)
  && has_stock(order)
  || is_backorder(order)
```

_Example:_ a leading `-` starts a new statement.

```cheby
let a = b
-c        // a new expression statement `-c`, not `b - c`
```

The formatter never produces the second example. It is shown only to fix the rule.

_Example:_ type arguments. A closing `>` ends the line, and type arguments may span lines.

```cheby
type Cell = Option<player::Player>
type Grid = List<List<Cell>>      // `>>` split into two closing `>`, the line ends

fn index() -> map::Map<
  String,
  List<Int>,
> {
  map::new::<
    String,
    List<Int>,
  >()
}
```

_Rationale:_ the innermost-delimiter rule is what makes `list::map(xs, fn(x) { … })` work with multi-statement closure bodies. The leading-operator rule is what makes multi-line pipelines work without trailing operators (D-107). Continuation is decided by the parser because only the parser can tell a `>` that closes type arguments from a comparison. With a purely lexical rule, `type Cell = Option<Player>` would continue into the next item, and type arguments could not span lines. The old rule already depended on parsing through "binary position" and unary `-`. The cost is that tools such as syntax highlighters and the formatter need the parser's view of `<` and `>` (D-231).
