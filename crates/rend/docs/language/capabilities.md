# Capabilities

A capability is an unforgeable, non-Copy permission token. Holding one is the proof
that you're allowed to perform a privileged action. The type system enforces both
properties statically.

## Declaration

```rd
cap MintCap {
    uses_left:    u64,
    max_per_call: u64,
}
```

Syntactically a `cap` looks like a `struct`. Semantically it has two extra rules:

1. **Non-Copy.** A cap value is moved (affine), never duplicated. Reading a `cap` from
   a state slot or local moves it; you can't use the source after.
2. **Privileged construction.** The literal `MintCap { ... }` is only legal inside the
   module that declares the cap. Outside the declaring module, the only ways to
   obtain a cap are: receive it as a return value, read it from a struct field you
   own, or read it from a state slot you own.

Together these mean: if your code holds a `MintCap`, the only way it got there is
through a chain of moves rooted at the declaring module's literal. The cap's
existence is the proof.

## Usage

```rd
cap MintCap { uses_left: u64, max_per_call: u64 }

state root_cap: MintCap;

entry fn bootstrap(uses: u64, ceiling: u64) -> u64 {
    root_cap = MintCap { uses_left: uses, max_per_call: ceiling };
    return 1u64;
}

entry fn mint(amount: u64) -> u64 {
    // Reading a cap moves it out of state. We have to put one back
    // before we return, or the type system rejects this.
    let cap = root_cap;
    if amount > cap.max_per_call { return 0u64; }
    if cap.uses_left == 0u64    { return 0u64; }
    root_cap = MintCap {
        uses_left:    cap.uses_left - 1u64,
        max_per_call: cap.max_per_call,
    };
    return amount;
}
```

Notice the cap is read out of state, then a fresh cap is written back. Writing back
isn't ceremony — the affine pass requires it, because the cap can't be left "borrowed"
after the call.

## Why caps aren't Copy

If caps were Copy, calling code could clone the cap, mint with the clone, and keep
the original — defeating the rate-limiting. Affinity makes "I have one" structurally
mean "and there's one fewer than there was."

This is identical to Rust's affine resource pattern, applied to a domain where the
resource is a permission rather than memory.

## Caps in struct fields

A struct that contains a cap field becomes affine itself: the struct can't be Copy
because reading it would duplicate the cap. The affine pass propagates this
automatically.

## See also

- [`examples/32_capabilities.rd`](../../examples/32_capabilities.rd) — full demo
- [`tests/capabilities.rs`](../../tests/capabilities.rs) — affine + privileged-construction rules
