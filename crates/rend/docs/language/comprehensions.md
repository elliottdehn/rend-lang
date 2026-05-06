# Comprehensions

`rend` has Python-style comprehensions for arrays, sets, and dicts. They're
expressions that build a collection from an iteration plan: a mapper, one or more
`for` generators, and optional `if` filters.

## List comprehensions

```rd
let doubled = [x * 2 for x in xs];
let evens   = [x * x for x in xs if x % 2 == 0];
```

The mapper goes first, then a sequence of clauses. Each clause is either:

- `for ident in iter` — bind `ident` to each element of `iter`
- `if cond` — gate iterations through this point

Multiple `for` clauses produce a Cartesian product:

```rd
let pairs = [a + b for a in xs for b in ys];
```

`if` clauses are scoped: they gate the iterations that flow through them, so the
order matters:

```rd
let xs = [a + b
    for a in [1, 2, 3, 4] if a > 1
    for b in [1, 2, 3, 4] if b < 3];
// a ∈ {2,3,4}, b ∈ {1,2} → 6 elements
```

The result type is `[T]` where `T` is the mapper's type.

## Set comprehensions

```rd
let s = set { x * x for x in [1, 2, 3, 4, 5] if x % 2 == 0 };
// {4, 16}
```

Same clause structure as list comprehensions; the result is `set<T>`. Duplicates are
deduplicated:

```rd
let s = set { a + b for a in [1, 2] for b in [3, 4] };
// {4, 5, 6}  — three distinct sums
```

## Dict comprehensions

```rd
let d = dict { x: x * x for x in [1, 2, 3, 4] };
// {1: 1, 2: 4, 3: 9, 4: 16}
```

The first colon-separated pair is the key/value mapper. Multi-generator and filter
clauses work exactly like the others:

```rd
let d = dict { a * 10 + b: a + b for a in [1, 2] for b in [3, 4] };
// 4 entries
```

When two iterations produce the same key, the later one wins (last-write-wins
semantics — same as `dict_set` called repeatedly).

## What you can iterate over

The `iter` in a `for` clause can be:

- an array (`[T]`)
- a `pmap<K, V>` state — yields *values*, **streamed** (no source materialization)
- a `pvec<T>` state — yields *elements*, **streamed**

For pmap/pvec sources, the comprehension only materializes its *output* array;
the source flows one element at a time through the predicate and into the
collector. Same shape as a streaming `for x in p` loop, just collecting results
instead of running statements.

```rd
state amounts: pmap<i64, u64>;

let bigs = [v for v in amounts if v > 100u64];     // SQL-style filter
let count = len([v for v in amounts if v > 0u64]); // count-where
```

For pmap entries (key + value) or just keys, call the walk explicitly:

```rd
let by_key   = [e.0 for e in pmap_entries(amounts) if e.1 > 100u64];
let key_set  = set { k for k in pmap_keys(amounts) };
```

Range syntax (`1..=5`) is only valid in regular `for` loops, not in comprehension
generators — the parser rejects `[n for n in 1..5]` with "expected ']' to close
list comprehension."

```rd
let squares = [n * n for n in [1, 2, 3, 4, 5]];   // works
// [n * n for n in 1..=5]                         // parse error
```

## Aggregations

`sum` / `max` / `min` are common terminators for a comprehension result — the
SQL-`SELECT-WHERE-AGGREGATE` shape:

```rd
state amounts: pmap<Address, u64>;

entry view fn total_above(threshold: u64) -> u64 {
    return sum([v for v in amounts if v > threshold], 0u64);
}

entry view fn highest_holder() -> u64 {
    return max(pmap_values(amounts), 0u64);
}
```

See [builtins](builtins.md#aggregations) for the operation surface.

## Effects

The mapper and filter expressions are part of the surrounding function's body, so
they participate in the effects classification. A comprehension that calls a `view`
function inside the mapper is fine in a `view` enclosing function; calling something
impure makes the whole comprehension impure.

## See also

- [`examples/19_list_comprehensions.rd`](../../examples/19_list_comprehensions.rd) — list comprehension forms
- [`tests/sets_dicts.rs`](../../tests/sets_dicts.rs) — set/dict comprehension semantics
- [`tests/multi_gen_comp.rs`](../../tests/multi_gen_comp.rs) — multi-generator and filter scoping
