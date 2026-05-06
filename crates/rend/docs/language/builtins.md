# Builtins

The runtime ships a fixed set of built-in functions, callable from any module without
declaration. They live in three categories: context, collection, and conversion.

## Transaction context

| Function | Purpose |
|---|---|
| `address(s: string) -> Address` | tag a string as an `Address` literal |
| `msg_sender() -> Address` | the transaction's sender (from `TxContext`) |
| `block_timestamp() -> u64` | block timestamp (from `TxContext`) |
| `block_number() -> u64` | block height (from `TxContext`) |

These read fields out of the `TxContext` the host passes to `Engine::execute_*`. If
you call `Engine::execute(...)` (no context variant), the defaults are zero-address /
zero timestamp / zero block.

## Assertions

| Function | Purpose |
|---|---|
| `assert(cond: bool)` | abort the tx if `cond` is `false` |
| `assert(cond: bool, msg: string)` | abort with a message |

A failed `assert` returns a runtime error; the tx's writes are discarded.

## Arrays

| Function | Purpose |
|---|---|
| `len(xs: [T]) -> i64` | array length |

## Aggregations

Fold-style operations over arrays. The second arg fixes the result type — for
`sum` it's the starting accumulator, for `max`/`min` it's the value returned when
the array is empty.

| Function | Purpose |
|---|---|
| `sum(xs: [T], init: T) -> T` | `init + xs[0] + xs[1] + ...`. `T` must be an integer type. |
| `max(xs: [T], default_if_empty: T) -> T` | largest element; returns default only when empty |
| `min(xs: [T], default_if_empty: T) -> T` | smallest element; returns default only when empty |

For `max`/`min` on a non-empty array, the default is **not** compared against
elements — the fold starts from `xs[0]`. So `max([5, 3], 100)` is `5`, not `100`.

User-defined functions named `sum` / `max` / `min` shadow the builtin in their
declaring module. The intended use is "drop-in like SQL aggregations"; if you
need a function with one of these names that does something else, you can have it.

```rd
state amounts: pmap<i64, u64>;

entry view fn total_supply() -> u64 {
    return sum(pmap_values(amounts), 0u64);
}

entry view fn high_water_mark() -> u64 {
    return max(pmap_values(amounts), 0u64);
}

entry view fn count_active() -> i64 {
    return len([v for v in amounts if v > 0u64]);
}
```

## Sets (`set<T>`)

| Function | Purpose |
|---|---|
| `set_insert(s, e) -> set<T>` | functional insert |
| `set_remove(s, e) -> set<T>` | functional remove |
| `set_contains(s, e) -> bool` | membership |
| `set_len(s) -> i64` | size |

`set_insert` and `set_remove` return new sets; the language is value-semantic for
in-memory collections.

## Dicts (`dict<K, V>`)

| Function | Purpose |
|---|---|
| `dict_set(d, k, v) -> dict<K,V>` | insert or update |
| `dict_remove(d, k) -> dict<K,V>` | remove |
| `dict_get(d, k, default) -> V` | lookup with default |
| `dict_has(d, k) -> bool` | key membership |
| `dict_len(d) -> i64` | size |

## Persistent maps (`pmap<K, V>`)

`pmap` access uses indexing syntax: `p[k]` reads, `p[k] = v` writes. Iteration over
the whole map is via three walk builtins, each returning a fresh array in
hash-of-key order (deterministic but not user-meaningful):

| Function | Purpose |
|---|---|
| `pmap_contains(p, k) -> bool` | distinguishes "set to default" from "never set" |
| `pmap_entries(p) -> [(K, V)]` | every (key, value) pair |
| `pmap_keys(p) -> [K]` | every key |
| `pmap_values(p) -> [V]` | every value |

Each walk is `O(N)` cell reads — one per HAMT node. There's no `pmap_len`: tracking
length would require every insert to rewrite the root, which would defeat the
disjoint-subtree concurrency property. If you need a count, maintain it in a
separate state slot.

`for x in p` desugars to `for x in pmap_values(p)` — see [comprehensions](comprehensions.md).

## Sorted persistent maps (`pbtree<K, V>`)

Same indexing syntax as `pmap`. The walk order is **key-sorted**, which lets
range queries narrow before fetching:

| Function | Purpose |
|---|---|
| `pbtree_contains(p, k) -> bool` | distinguishes "set to default" from "never set" |
| `pbtree_range(p, lo, hi) -> [V]` | values whose keys fall in `[lo, hi]` (inclusive), in sorted order |

`for x in p` desugars to a streaming walk in sorted order — `break` after the
first match gives you `LIMIT 1 ORDER BY k ASC` for free. Slice-1 supports `u64`
keys only.

## Persistent vectors (`pvec<T>`)

| Function | Purpose |
|---|---|
| `pvec_push(v, x) -> u64` | append, return new index |
| `pvec_len(v) -> u64` | current length |
| `pvec_to_array(v) -> [T]` | materialize the whole vector in index order |

Indexed reads/writes via `v[i]` work on already-pushed slots.

`for x in v` desugars to `for x in pvec_to_array(v)` — see
[comprehensions](comprehensions.md).

## Strings

| Function | Purpose |
|---|---|
| `string_concat(a, b) -> string` | concatenate (`a + b` works too) |
| `string_slice(s, start, end) -> string` | byte-range slice; caller must respect UTF-8 boundaries |
| `string_contains(haystack, needle) -> bool` | substring search |

## Bytes

| Function | Purpose |
|---|---|
| `to_bytes(s: string) -> bytes` | UTF-8 encode |
| `bytes_len(b) -> i64` | length |
| `bytes_concat(a, b) -> bytes` | concatenate |
| `bytes_eq(a, b) -> bool` | equality |
| `bytes_slice(b, start, end) -> bytes` | byte-range slice |

## Integer conversion

| Function | Source | Result |
|---|---|---|
| `i64(x)` | any integer type | `i64`, bounds-checked |
| `i32(x)` | any integer type | `i32`, bounds-checked |
| `u32(x)` | any integer type | `u32`, bounds-checked |
| `u64(x)` | any integer type | `u64`, bounds-checked |
| `u128(x)` | any integer type | `u128`, bounds-checked |

A conversion that can't represent the source value is a runtime error. There's no
unchecked / wrapping variant — use explicit masks if you want truncation.

## Effect classification

Most builtins are `Pure`. Exceptions:

- `msg_sender`, `block_timestamp`, `block_number` are `Pure` (they read tx context, not state)
- Indexed `pmap` / `pvec` / `map` access is `ReadOnly` (read) or `WriteOnly` (write)
- `pvec_push` is a write
- `assert` is `Pure` (no side effect except control flow)

The classifier lattices these up into the containing function's tier; see [functions
and effects](functions-and-effects.md).
