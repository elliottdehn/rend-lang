# Operators

## Arithmetic

| Op | Syntax | Types | Notes |
|---|---|---|---|
| Add | `a + b` | integer types, `string + string` | overflow-checked at runtime |
| Sub | `a - b` | integer types | overflow-checked |
| Mul | `a * b` | integer types | overflow-checked |
| Div | `a / b` | integer types | truncating; division by zero is a runtime error |
| Mod | `a % b` | integer types | sign follows numerator; div-by-zero is a runtime error |
| Neg | `-a` | signed integer types | overflow-checked |

`string + string` concatenates. Cross-integer-type arithmetic is rejected — convert
explicitly.

## Comparison

| Op | Syntax | Result |
|---|---|---|
| Eq | `a == b` | `bool` |
| NotEq | `a != b` | `bool` |
| Lt | `a < b` | `bool` |
| Gt | `a > b` | `bool` |
| LtEq | `a <= b` | `bool` |
| GtEq | `a >= b` | `bool` |

Comparisons require both operands to have the same type. Equality works on all value
types (including structs, enums, tuples, `Address`, `bytes`, `string`); ordering works
on integer types and on `string` (lexicographic).

## Logical

| Op | Syntax | Notes |
|---|---|---|
| And | `a && b` | short-circuiting |
| Or | `a \|\| b` | short-circuiting |
| Not | `!a` | logical negation on `bool` |

## Bitwise

On integer types only:

| Op | Syntax |
|---|---|
| BitAnd | `a & b` |
| BitOr | `a \| b` |
| BitXor | `a ^ b` |
| Shl | `a << b` |
| Shr | `a >> b` |

Shift amounts are typed the same as the value being shifted; out-of-range shifts are
runtime errors.

## Indexing and field access

| Op | Syntax | Notes |
|---|---|---|
| Index | `a[i]` | arrays, `map`, `pmap`, `pvec`, `dict` |
| Field | `a.f` | struct field |
| Tuple index | `t.0`, `t.1`, ... | tuple element by position |
| Module/iface scope | `M::name` | static call, enum constructor, interface bind |
| Dynamic dispatch | `$v::method(args)` | through interface value |

Indexing works on the LHS of `=` for assignment: `balances[who] = amount`,
`xs[0] = 5`. Tuple indexing is read-only.

## Pipe

```rd
let result = 10
    |> double($$)
    |> square($$);
```

`|>` threads the LHS into the next stage. `$$` is the placeholder for the prior
stage's result and only has meaning inside a pipe RHS.

## Range

```rd
for i in 0..10 { ... }      // exclusive: 0..9
for i in 0..=10 { ... }     // inclusive: 0..10
```

Ranges only appear in `for` loops; they're not first-class values.

## See also

- [`examples/24_advanced.rd`](../../examples/24_advanced.rd) — bitwise + ranges
- [`examples/23_pipe.rd`](../../examples/23_pipe.rd) — pipe and `$$`
