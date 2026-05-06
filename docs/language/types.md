# Types

## Primitive types

| Type | Description | Literal example |
|---|---|---|
| `i64` | signed 64-bit | `42`, `-7` |
| `i32` | signed 32-bit | `5i32` |
| `u32` | unsigned 32-bit | `5u32` |
| `u64` | unsigned 64-bit | `5u64` |
| `u128` | unsigned 128-bit | `5u128` |
| `bool` | boolean | `true`, `false` |
| `string` | UTF-8 text | `"hello"` |
| `Address` | account/module address | `address("alice")` |
| `bytes` | byte array | `to_bytes("hello")` |
| `json` | dynamic JSON value (jsonb-shaped) | `parse_json("{...}")` |
| `()` | unit / void return | (no literal; from `fn f() {}`) |

`Address` is a distinct type, not a string alias. Construct one with the `address(...)`
builtin, which takes a string and tags it.

`bytes` values are constructed from strings (`to_bytes(...)`) or returned by host
functions. They support equality, length, concatenation, and slicing — see
[builtins](builtins.md).

Integer arithmetic is checked for overflow at runtime. Mixing integer types is a
compile error; convert explicitly with `i64(x)`, `u64(x)`, etc.

## Type inference

`let x = expr;` infers the type from `expr`. Optional annotation:

```rd
let x: u64 = 5u64;
let y = expr;
```

Function parameters and return types are always explicit. State declarations always
list a type.

## Composite types

### Struct

```rd
struct Score {
    player: Address,
    points: i64,
}

let s = Score { player: address("alice"), points: 100 };
let pts = s.points;
```

Structs are nominal, not structural. `Score` and another struct with the same fields
are different types.

### Enum (sum type)

```rd
enum Outcome {
    Won(u64),
    Lost,
    Draw(string),
}

let o = Outcome::Won(100u64);
```

Variants can carry zero or more positional payload values. See [pattern matching](pattern-matching.md)
for destructuring.

### Tuple

```rd
let pair: (i64, Address) = (42, address("alice"));
let n = pair.0;
let who = pair.1;
let (a, b) = pair;
```

Tuples are anonymous, fixed-arity, ordered products. Use `.0`, `.1`, ... or
destructuring `let (a, b, c) = ...;`.

### Array

```rd
let xs: [i64] = [3, 1, 4, 1, 5, 9];
let n = xs[0];
let count = len(xs);
```

Arrays are homogeneous and fixed-size at the value level (no resize ops). The type
notation does not include a size — it's a shape, not a length.

### `set<T>` and `dict<K, V>`

In-memory only. Don't put them in `state` slots. Useful for transient computation
inside a transaction.

```rd
let s: set<i64> = set { 1, 2, 3 };
let d: dict<Address, u64> = dict { address("alice"): 100u64, address("bob"): 50u64 };
```

Both support [comprehensions](comprehensions.md) for building from a generator:

```rd
let evens = set { x for x in xs if x % 2 == 0 };
let by_id = dict { u.id: u for u in users };
```

See [builtins](builtins.md) for the operation surface.

### `map<K, V>`

Persistent map, one KV cell per key. Best for keyed indices where you only ever
access one key at a time:

```rd
state balances: map<Address, u64>;
balances[who] = 100u64;
let bal = balances[who];
```

A missing key reads as the value type's default (`0`, `false`, `""` etc.). To
distinguish "set to default" from "never set," use `pmap` instead.

### `pmap<K, V>` and `pvec<T>`

Persistent collections backed by HAMT and indexed trie respectively. Each tree node is
its own KV cell, so concurrent transactions touching disjoint subtrees don't conflict.
See [state and persistence](state-and-persistence.md) and [persistent structures](../internals/persistent-structures.md).

## Affine types

`cap` declarations and the `Resource` type are non-Copy. Reading them moves them; you
can't use a moved value. See [capabilities](capabilities.md).

## Interface types

`interface IFoo { ... }` produces a type `IFoo`. Values of type `IFoo` are runtime
bindings to a target module. See [interfaces](interfaces.md).
