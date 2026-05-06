# Pattern matching

`match` performs exhaustive pattern dispatch on enum values. All arms must produce
the same type when used in expression position.

## Patterns

| Pattern | Matches | Bindings |
|---|---|---|
| `_` | any value | none |
| `Enum::Variant` | unit variant (no payload) | none |
| `Enum::Variant(b1, b2, ...)` | variant with payload | `b1`, `b2`, ... bound to the payload values |

Use `_` inside a payload-bearing variant to ignore a field:

```rd
match outcome {
    Outcome::Won(amount)         => amount,
    Outcome::Lost                => 0u64,
    Outcome::Draw(_label)        => 0u64,
}
```

## As an expression

```rd
let payout = match stage {
    Stage::Open                 => 0u64,
    Stage::Voting(deadline)     => if now >= deadline { 0u64 } else { deadline - now },
    Stage::Closed(_winner, amt) => amt,
};
```

Each arm is a single expression. Use a block if you need multiple statements:

```rd
match outcome {
    Outcome::Won(amount) => {
        emit Payout(msg_sender(), amount);
        amount
    },
    _ => 0u64,
}
```

## Exhaustiveness

The typecheck rejects `match` expressions that don't cover every variant of the
scrutinee's enum type. Use `_` as a catch-all when you intentionally want to ignore
the rest:

```rd
match status {
    Status::Active   => 1u64,
    Status::Paused   => 2u64,
    _                => 0u64,   // catch-all for any future variants
}
```

For a sum type with many variants, an explicit catch-all is the safe escape.

## See also

- [`examples/30_sum_types.rd`](../../examples/30_sum_types.rd) — `Result`-style patterns, lifecycle encoding
- [`tests/match_exhaustive.rs`](../../tests/match_exhaustive.rs) — exhaustiveness rules
- [`tests/sum_types.rs`](../../tests/sum_types.rs) — variant payload destructuring
