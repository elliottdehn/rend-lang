# Modifiers

A modifier is a Solidity-style function wrapper. The wrapper body runs the prologue,
splices the wrapped function's body at the `_;` placeholder, and then runs the
epilogue. Modifiers desugar before typeck, so they're just textual rewriting — but
parameter binding is type-aware.

## Declaration

```rd
modifier only_owner(owner: Address) {
    assert(msg_sender() == owner);
    _;
}

modifier rate_limited(per_call: u64) {
    let cap = root_cap;
    assert(cap.uses_left > 0u64);
    assert(per_call <= cap.max_per_call);
    _;
}
```

The `_;` placeholder marks where the wrapped function body splices in. A modifier
without a `_;` is a compile error.

## Application

```rd
[only_owner(admin), rate_limited(100u64)]
entry fn sensitive() -> bool {
    // body runs only after both modifiers' preludes pass
    return true;
}
```

Modifiers are listed in `[...]` before any `entry` / `view` / `pure` keywords. They
nest left-to-right: the first modifier wraps the second wraps the body.

After expansion, the example above becomes (roughly):

```rd
entry fn sensitive() -> bool {
    assert(msg_sender() == admin);            // from only_owner
    let cap = root_cap;                       // from rate_limited
    assert(cap.uses_left > 0u64);
    assert(100u64 <= cap.max_per_call);
    return true;                              // original body
}
```

## When to use modifiers vs caps

Modifiers are the right tool for repeated *checks* — a guard that's the same shape
across many functions. They're textual.

Capabilities are the right tool for *unforgeable permission*. A cap survives across
calls; you can hand it to another module and they keep proof-of-permission. A
modifier just runs an `assert`.

In practice, large contracts use both: caps for ground-truth permissions, modifiers
for frequent guard patterns built on top.

## See also

- [`tests/modifiers.rs`](../../tests/modifiers.rs) — parameter binding, expansion order, `_;` placement
