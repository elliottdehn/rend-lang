# Deploy / tx / query lifecycle

A real embedding deals with three distinct operations:

1. **Deploy** — install a module's bytecode and run its constructor.
2. **Transaction** — call into deployed modules, accumulating reads and writes.
3. **Query** — read-only inspection of state (no writes, no OCC).

`rend` separates these into distinct code paths so the runtime can give each one
exactly the cost it needs.

## Compile once, execute many

```rust
let token: Artifact = engine.compile(&token_source)?;
let market: Artifact = engine.compile_modules(&[market_src, helper_src])?;
```

Compilation is deterministic. The same source bytes always produce the same artifact
bytes. The artifact's content hash is the module-set's identity — it's what a tx
commits to when it's compiled against this dep.

## Deploy

```rust
let outcome = engine.deploy(&token, Fuel::new(50_000), &kv)?;
if let Some(out) = outcome {
    kv.apply(&out.writes);   // commit constructor's writes
    println!("deployer initial supply = {:?}", out.result);
}
```

`deploy` runs the module's `fn main()` constructor, if one exists. The return is
`Option<ExecOutcome>` — `None` means the module had no constructor (just functions and
state). The constructor's writes seed the initial state; the host applies them like
any other tx's writes.

## Transaction

A transaction is its own artifact compiled against the dep set:

```rust
let tx_src = "
    module main;
    fn main() -> u64 {
        return token::transfer(address(\"alice\"), 250u64);
    }
";
let tx = engine.compile_tx(tx_src, &[token.clone()])?;

let outcome = engine.execute_tx_with_context(&tx, &[token.clone()], ctx, fuel, &kv)?;
kv.apply(&outcome.writes);
```

The tx artifact references the `token` artifact by content hash. If the host applies
writes only after a successful OCC check, the deployment is consistent.

`execute_tx` is the cross-artifact equivalent of `execute_modules` — it loads all
provided artifacts into the world, then runs the tx's `main`.

## Query

```rust
let q_src = "
    module main;
    view fn main() -> u64 { return token::balance_of(address(\"alice\")); }
";
let q_tx = engine.compile_tx(q_src, &[token.clone()])?;

let result = engine.query(&q_tx, &[token], Fuel::new(20_000), &kv)?;
assert_eq!(result.result, Value::U64(250));
```

`query` requires the tx's `main` to be `view` or `pure`. The runtime takes a different
code path: no write log, no OCC tracking. If the source happens to write or emit, the
classifier rejects it before the VM runs.

`QueryOutcome` carries `result`, `reads`, and `events` (events from view fns are
disallowed by the classifier, but the field exists for consistency).

## Why three paths

Each path makes a different correctness/performance trade:

- **Deploy** is rare — ergonomic API, no special perf concerns.
- **Tx** is the hot path for state-changing operations. Needs full OCC bookkeeping.
- **Query** is the hottest path overall (think indexer reads, RPC). Skipping the
  write log + OCC is the win.

The `view` / `pure` annotations exist so the classifier can statically prove that a
function is safe on the query path, without trusting the host to know.

## See also

- [artifacts](artifacts.md) — content addressing and binary format
- [transactions](transactions.md) — `TxContext`, OCC, and merging
