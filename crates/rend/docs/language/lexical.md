# Lexical structure

## Comments

```
// Line comments run to end-of-line.
```

There are no block comments.

## Keywords

```
break    cap        const      continue   dict       else
emit     entry      enum       event      false      fn
for      if         import     in         interface  let
match    modifier   module     nore       pure       return
set      state      struct     true       view       while
```

`Address`, `bytes`, `bool`, `i32`, `i64`, `u32`, `u64`, `u128`, `string` are type names
(they appear in type positions, not statement positions, but the parser treats them as
reserved identifiers).

## Identifiers

`[A-Za-z_][A-Za-z0-9_]*`. Module names, function names, type names, and variable names
share the same identifier rule.

## Integer literals

Integer literals are typed by suffix:

| Literal | Type |
|---|---|
| `42` | `i64` (default) |
| `42i32` | `i32` |
| `42u32` | `u32` |
| `42u64` | `u64` |
| `42u128` | `u128` |

The default `i64` lets you write `42` everywhere `i64` is expected without ceremony,
but every other integer type needs the suffix at the literal site.

Negative literals are written with a unary minus: `-42`, `-1i32`. The parser produces
`UnOp::Neg` over a positive literal, so overflow is checked at runtime if the negation
itself would overflow.

## Boolean literals

`true`, `false`. Type `bool`.

## String literals

```
"hello, world"
"with \"quotes\" and a \n newline"
```

Escape sequences: `\"`, `\\`, `\n`, `\t`, `\r`, `\0`. Strings are UTF-8.

## Punctuation and operators

```
+   -   *   /   %        arithmetic
==  !=  <   >   <=  >=   comparison
&&  ||  !                logical
&   |   ^   <<  >>       bitwise
=                        assignment
->  =>                   arrows (fn return, match arm)
::                       module/enum/iface scope
..  ..=                  ranges (exclusive, inclusive)
|>                       pipe
$$                       pipe placeholder
$                        dynamic dispatch prefix
( ) { } [ ]              grouping, blocks, arrays/indexing
, ; :                    separators
```

## Whitespace

Spaces, tabs, and newlines separate tokens but otherwise carry no meaning. The
language is not whitespace-sensitive.
