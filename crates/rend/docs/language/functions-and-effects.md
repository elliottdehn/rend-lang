# Functions and effects

A function declaration has the shape:

```rd
[modifiers] [annotations] fn name(param: Type, ...) -> ReturnType {
    body
}
```

## Annotations

| Annotation | Meaning |
|---|---|
| `entry` | Reachable across module boundaries (cross-module calls, tx `main`). Required for any function the host or another module needs to call. |
| `view` | May read state but not write, emit, or call anything that does. Verified by the effects classifier. Enables read-only `Engine::query`. |
| `pure` | `view` plus "no state reads either" — a deterministic transform of arguments. No host calls, no side effects. |
| `nore` | Non-reentrant: the runtime tracks active `(module, fn)` pairs and aborts if entered while already on the stack. |

Examples:

```rd
entry view fn balance_of(who: Address) -> u64 {
    return balances[who];
}

entry nore fn withdraw(amount: u64) -> u64 {
    // protected from reentrancy via host callback
}

pure fn apply_haircut(value: u64, bps: u64) -> u64 {
    return value * (10000u64 - bps) / 10000u64;
}
```

`view` and `pure` are mutually exclusive — `pure` is strictly stricter.

## The effects classifier

Every function is classified into one of these effect tiers:

```
Pure         — no state reads, no writes, no host calls, no events
ReadOnly     — reads state; no writes, emits, or impure host calls
WriteOnly    — writes state; no reads, emits, or impure host calls
ReadWrite    — reads and writes state
Impure       — emits events, calls impure hosts, or has unknown effects
```

The classifier walks the AST, tags every function, and verifies that:

- A function annotated `pure` is classified `Pure`.
- A function annotated `view` is classified `Pure` or `ReadOnly`.
- A `view` function only calls `view` or `pure` functions.
- A `pure` function only calls `pure` functions.

A mismatch is a **compile error**, not a runtime check.

```rd
view fn bad() -> u64 {
    counter = counter + 1;   // compile error: view fn writes state
    return counter;
}
```

## Effects through dynamic dispatch

When a `view` function calls through an interface, the interface method's `view` flag
is read and the dispatch is treated as a `view` call:

```rd
interface IPriceFeed {
    entry view fn price_of(symbol: string) -> u64;
}

state feed: IPriceFeed;

view fn portfolio_value(holder: Address) -> u64 {
    let units = holdings[holder];
    let price = $feed::price_of("ETH");   // view dispatch — fine in a view fn
    return units * price;
}
```

The bind-time conformance check guarantees the bound module's implementation is at
least as strict as the interface declares. See [interfaces](interfaces.md).

## Cross-module call effects

Calling another module's `entry` function adopts that function's classification —
`view` if the callee is `view`, `Impure` otherwise. The compiler reads the callee's
`is_view` / `is_pure` flags from its bytecode (which is why those flags are persisted
in the artifact format).

## See also

- [`examples/35_queryable_token.rd`](../../examples/35_queryable_token.rd) — `view`/`pure` end-to-end
- [`tests/typeck.rs`](../../tests/typeck.rs) — verification rules
