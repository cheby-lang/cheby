# Appendix A. Grammar

This appendix collects the complete grammar of Cheby. Chapter snippets are excerpts of it ([README](README.md#grammar-notation)). Token classes (`LOWER`, `UPPER`, `INT`, `FLOAT`, `STRING`, `RAW_STRING`, `NL`) are defined in [chapter 2](02-lexical-structure.md). `NL` tokens are produced by the lexer and ignored by the parser where a line continues, as described in [§2.8](02-lexical-structure.md#28-newlines-and-statement-separation) (D-231). The rules below show `NL` only where it is significant, so newlines inside `( )`, `[ ]` and the `< >` of `type_args`, `type_params` and `plain_params` need no `[ NL ]`.

Operator precedence is given by the layered `expr` rules below and summarized in [§5.4.1](05-expressions.md#541-precedence-and-associativity) (D-108).

## A.1 Modules

```ebnf
module          = [ NL ] { module_doc NL }
                  { inner_attribute NL }                  (* after module docs, before imports, D-141, D-201 *)
                  { import_decl NL }                      (* all imports before all items, D-143 *)
                  { item NL } ;
module_doc      = "//!" text ;                            (* §2.2 *)
inner_attribute = "@!" LOWER [ "(" [ attr_arg { "," attr_arg } [ "," ] ] ")" ] ;   (* only @!target(...), D-141 *)

import_decl     = "import" module_path [ "as" LOWER ]
                | "import" module_path "::" "{" [ NL ] import_item { "," [ NL ] import_item } [ "," ] [ NL ] "}" ;
module_path     = LOWER { "::" LOWER } ;
import_item     = LOWER [ "as" LOWER ]
                | UPPER [ "as" UPPER ] ;
```

## A.2 Items

```ebnf
item            = { doc_comment NL } { attribute NL } item_body ;
doc_comment     = "///" text ;
attribute       = "@" LOWER [ "(" [ attr_arg { "," attr_arg } [ "," ] ] ")" ] ;
attr_arg        = LOWER | STRING ;

item_body       = fn_decl
                | type_decl
                | alias_decl
                | const_decl
                | interface_decl
                | test_decl ;

visibility      = "pub" | "priv" ;                        (* default: package-visible, D-162 *)

fn_decl         = [ visibility ] "fn" LOWER [ type_params ] "(" [ params ] ")" [ "->" type ] [ block ] ;
type_params     = "<" type_param { "," type_param } [ "," ] ">" ;
type_param      = UPPER [ ":" bound ] ;
bound           = interface_ref { "+" interface_ref } ;
interface_ref   = [ LOWER "::" ] UPPER ;
params          = param { "," param } [ "," ] ;
param           = LOWER ":" type ;

type_decl       = [ "pub" [ "exposed" ] | "priv" ] "type" UPPER [ plain_params ] [ type_body ] ;   (* exposed requires pub, D-150 *)
plain_params    = "<" UPPER { "," UPPER } [ "," ] ">" ;
type_body       = "{" [ NL ] variant { "," [ NL ] variant } [ "," ] [ NL ] "}"
                | "{" [ NL ] field { "," [ NL ] field } [ "," ] [ NL ] "}" ;   (* single-variant shorthand, §4.3.3 *)
variant         = { doc_comment NL } variant_body ;              (* D-211 *)
variant_body    = UPPER
                | UPPER "(" type { "," type } [ "," ] ")"
                | UPPER "{" [ NL ] field { "," [ NL ] field } [ "," ] [ NL ] "}" ;
field           = { doc_comment NL } LOWER ":" type ;           (* D-211 *)

alias_decl      = [ visibility ] "type" UPPER [ plain_params ] "=" type ;

const_decl      = [ visibility ] "const" LOWER ":" type "=" expr ;

interface_decl  = [ visibility ] "interface" UPPER [ ":" bound ]
                  [ "{" [ NL ] { interface_fn NL } "}" ] ;
interface_fn    = { doc_comment NL } "fn" LOWER "(" [ type { "," type } [ "," ] ] ")" [ "->" type ] ;   (* doc comments: D-227 *)

test_decl       = "test" STRING block ;                   (* no visibility *)
```

Notes:

- An item without `pub` or `priv` is visible to every module of its package. `pub` makes it visible to other packages, and `priv` restricts it to its own module (D-162).

- A `type_decl` without a body is an external type and requires an `@external` attribute ([§12.5.3](12-targets-and-ffi.md#1253-external-types)).
- An `fn_decl` without a block requires an `@external` attribute ([§12.5](12-targets-and-ffi.md#125-foreign-functions)).
- The two `type_body` forms are told apart by the first token after any doc comments: `UPPER` starts variants, `LOWER ":"` starts fields.
- Newlines inside `{ … }` of type bodies are allowed only where the rules show `[ NL ]`, so variants and fields may be written one per line with commas.

## A.3 Types

```ebnf
type            = path_type
                | tuple_type
                | fn_type
                | dyn_type
                | "Self" ;                                (* only inside interface_decl *)
path_type       = [ LOWER "::" ] UPPER [ type_args ] ;
type_args       = "<" type { "," type } [ "," ] ">" ;
tuple_type      = "(" type "," type { "," type } [ "," ] ")" ;
fn_type         = "fn" "(" [ type { "," type } [ "," ] ] ")" [ "->" type ] ;
dyn_type        = "dyn" interface_ref ;
```

In type contexts, a `>>` token is split into two `>` tokens ([§2.6](02-lexical-structure.md#26-operators-and-punctuation)).

## A.4 Blocks and statements

```ebnf
block           = "{" [ NL ] [ stmt { NL stmt } ] [ NL ] "}" ;
stmt            = let_stmt
                | let_assert_stmt
                | use_stmt
                | assert_stmt
                | local_fn
                | expr ;

let_stmt        = "let" pattern [ ":" type ] "=" expr ;
let_assert_stmt = "let" "assert" pattern [ ":" type ] "=" expr [ "as" expr ] ;   (* String message, D-159 *)
use_stmt        = "use" [ use_binder { "," use_binder } ] "<-" expr ;
use_binder      = pattern [ ":" type ] ;
assert_stmt     = "assert" expr [ "as" expr ] ;                                  (* String message, D-160 *)
local_fn        = "fn" LOWER "(" [ closure_params ] ")" [ "->" type ] block ;
```

A `use_stmt` consumes the rest of its block ([§5.10](05-expressions.md#510-use)).

## A.5 Expressions

Rules are listed from lowest to highest precedence. `a { op b }` means left-associative. Comparison is non-associative.

```ebnf
expr            = or_expr ;
or_expr         = and_expr { "||" and_expr } ;
and_expr        = cmp_expr { "&&" cmp_expr } ;
cmp_expr        = pipe_expr [ cmp_op pipe_expr ] ;               (* non-chaining, D-108 *)
cmp_op          = "==" | "!=" | "<" | "<=" | ">" | ">=" ;
pipe_expr       = bor_expr { "|>" bor_expr } ;
bor_expr        = bxor_expr { "|" bxor_expr } ;
bxor_expr       = band_expr { "^" band_expr } ;
band_expr       = shift_expr { "&" shift_expr } ;
shift_expr      = add_expr { ( "<<" | ">>" ) add_expr } ;
add_expr        = mul_expr { ( "+" | "-" ) mul_expr } ;
mul_expr        = unary_expr { ( "*" | "/" | "%" ) unary_expr } ;
unary_expr      = ( "-" | "!" ) unary_expr
                | postfix ;
postfix         = primary { call_suffix | field_suffix } ;
call_suffix     = "(" [ arg { "," arg } [ "," ] ] ")" ;
arg             = expr | "_" ;                                  (* at most one "_" per argument list; it belongs to this call, §5.6, D-202 *)
field_suffix    = "." LOWER ;                                  (* no tuple index, D-241 *)

primary         = literal
                | path_expr
                | constructor
                | record_update
                | tuple
                | "(" expr ")"
                | list
                | block
                | closure
                | case_expr
                | panic_expr
                | todo_expr ;

literal         = INT | FLOAT | STRING | RAW_STRING ;
path_expr       = [ LOWER "::" ] ( LOWER | UPPER ) [ "::" type_args ]
                | [ LOWER "::" ] UPPER "::" LOWER ;             (* interface function, Show::show, D-173 *)
constructor     = ctor_path "{" [ NL ] [ field_init { "," [ NL ] field_init } [ "," ] ] [ NL ] "}" ;
record_update   = UPPER "{" [ NL ] ".." expr { "," [ NL ] field_init } [ "," ] [ NL ] "}" ;
ctor_path       = [ LOWER "::" ] UPPER ;
field_init      = LOWER [ ":" expr ] ;
tuple           = "(" expr "," expr { "," expr } [ "," ] ")" ;
list            = "[" [ list_item { "," list_item } [ "," ] ] "]" ;
list_item       = expr | ".." expr ;                            (* any number of spreads, anywhere, D-157 *)
closure         = "fn" "(" [ closure_params ] ")" [ "->" type ] block ;
closure_params  = closure_param { "," closure_param } [ "," ] ;
closure_param   = pattern [ ":" type ] ;                       (* irrefutable, D-242 *)
panic_expr      = "panic" [ "as" expr ] ;                      (* D-161 *)
todo_expr       = "todo" [ "as" expr ] ;                       (* D-161 *)

case_expr       = "case" subjects "{" [ NL ] case_arm { NL case_arm } [ NL ] "}"
                | "case" "{" [ NL ] cond_arm { NL cond_arm } [ NL ] "}" ;
subjects        = subject_expr { "," subject_expr } ;
case_arm        = pattern { "," pattern } [ "when" expr ] "=>" expr ;
cond_arm        = ( expr | "_" ) "=>" expr ;                    (* last arm must be "_", D-109 *)
```

Notes:

- A unit or positional constructor used as a value (`None`, `Some(1)`, `Some`) parses as `path_expr` followed by an optional `call_suffix`.
- An interface function call (`Show::show(x)`, `shape::Shape::area(x)`) parses as the interface-function form of `path_expr` followed by a `call_suffix` (D-173). After `UPPER "::"`, a `LOWER` token selects this form and `<` selects `type_args`.
- The message after `as` in `panic_expr`, `todo_expr`, `assert_stmt` and `let_assert_stmt` is a `String` expression (D-159, D-160, D-161).
- `subject_expr` is `expr` with two restrictions ([§5.9.6](05-expressions.md#596-parsing-subjects)): an `UPPER` path followed by `{` ends the subject instead of starting a `constructor` or `record_update`, and a subject cannot start with `{`. Parenthesize to use either form.
- `-` directly applied to an `INT` or `FLOAT` literal is treated as a negative literal for typing ([§5.3.1](05-expressions.md#531-negative-literals)).
- In `cmp_expr`, a second comparison operator is a compile error with a suggestion, not a parse of a different expression.

## A.6 Patterns

```ebnf
pattern         = alt_pattern [ "as" LOWER ] ;
alt_pattern     = primary_pat { "|" primary_pat } ;
primary_pat     = "_"
                | LOWER
                | literal_pat
                | ctor_pattern
                | tuple_pattern
                | list_pattern
                | "(" pattern ")" ;
literal_pat     = [ "-" ] INT | STRING | RAW_STRING ;           (* no interpolation *)
ctor_pattern    = ctor_path
                | ctor_path "(" pattern { "," pattern } [ "," ] ")"
                | ctor_path "{" [ field_pats ] "}" ;
field_pats      = field_pat { "," field_pat } [ "," ".." ] [ "," ]
                | ".." ;
field_pat       = LOWER [ ":" pattern ] ;
tuple_pattern   = "(" pattern "," pattern { "," pattern } [ "," ] ")" ;
list_pattern    = "[" [ list_pat_item { "," list_pat_item } [ "," ] ] "]" ;
list_pat_item   = pattern | ".." [ LOWER ] ;                    (* at most one spread, in any position, D-158 *)
```

## A.7 String literals

```ebnf
STRING          = '"' { string_char | escape | interpolation } '"' ;
RAW_STRING      = "r" { "#" } '"' { any character } '"' { "#" } ;
                  (* D-130: the literal ends at the first '"' followed by as many "#" as
                     were written after "r", so r"..." cannot contain '"',
                     r#"..."# cannot contain '"#', and so on *)
escape          = "\n" | "\t" | "\r" | "\\" | '\"' | "\{" | "\u{" hex_digit { hex_digit } "}" ;
interpolation   = "{" LOWER { "." LOWER } [ ":?" ] "}" ;
string_char     = any character except '"', "\" and "{" ;
```

## A.8 Numeric literals

```ebnf
INT             = DEC_INT | HEX_INT | BIN_INT | OCT_INT ;        (* OCT_INT: D-129 *)
DEC_INT         = "0" | nonzero_digit { digit | "_" } ;
HEX_INT         = "0x" hex_digit { hex_digit | "_" } ;
BIN_INT         = "0b" bin_digit { bin_digit | "_" } ;
OCT_INT         = "0o" oct_digit { oct_digit | "_" } ;
FLOAT           = DEC_INT "." digit { digit | "_" } [ exponent ]
                | DEC_INT exponent ;
exponent        = ( "e" | "E" ) [ "+" | "-" ] digit { digit | "_" } ;
```

## A.9 Identifiers and keywords

```ebnf
LOWER           = ( "a"…"z" | "_" ) { "a"…"z" | "0"…"9" | "_" } ;   (* not "_" alone, not a keyword *)
UPPER           = "A"…"Z" { "A"…"Z" | "a"…"z" | "0"…"9" } ;
```

Identifiers are ASCII only (D-127). `LOWER` excludes keywords and reserved words.

Keywords ([§2.4](02-lexical-structure.md#24-keywords)):

```
as  assert  case  const  dyn  exposed  fn  import  interface
let  panic  priv  pub  test  todo  type  use  when
```

Reserved words, which have no meaning but cannot be used as identifiers (D-128, D-240):

```
if  else  for  while  loop  break  continue  return  mut  impl
trait  struct  enum  match  async  await  macro  self  super  where  yield
go
```

Reserved `UPPER` identifier: `Self`.
