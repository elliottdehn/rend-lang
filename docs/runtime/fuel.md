# Fuel and sandboxing

Fuel is the actual sandbox enforcement. Every instruction the VM executes ticks one
unit off the fuel counter; running out is a runtime error. There's no other escape
hatch, so the host can run untrusted code with a hard upper bound on time and
allocation.

## Fuel API

```rust
pub struct Fuel { pub remaining: u64 }

impl Fuel {
    pub fn new(amount: u64) -> Self;
    pub fn unlimited() -> Self;       // u64::MAX, but still ticks
}
```

Pass `Fuel::new(N)` into any `Engine::execute_*` / `Engine::query_*` /
`Engine::run_*` / `Engine::deploy_*` call.

## What costs fuel

- Every `Instr::*` dispatch in the VM main loop ticks one unit.
- Host calls (`CallHost`) tick on entry. The host function itself can't be charged
  per-instruction; that's the host's responsibility.
- KV reads happen lazily. Forcing a `Pending` value charges for the materialization
  cost.

`out of fuel` is `ErrorKind::Runtime`. The tx's writes are discarded.

## Choosing a budget

There's no automatic right answer; it depends on workload. Rules of thumb:

- A simple cross-module call + arithmetic: low thousands.
- A loop over a `pvec` of any size: scale by the bound.
- A constructor that seeds a thousand state slots: tens of thousands.

The `tests/` and `examples/` directories use `Fuel::new(20_000)` to `Fuel::new(200_000)`
for typical multi-module flows; that range is a reasonable starting point.

For long-running offline analysis, `Fuel::unlimited()` is fine — the cost is bounded
by `u64::MAX` ticks, which is more than enough for any real program but still gives
you the sandbox guarantee.

## What sandboxing does *not* cover

- Memory pressure inside the VM (large `pvec_push` loops grow the in-memory write
  set).
- Side effects in custom host functions you bind. If your host function spawns a
  thread, that's outside the sandbox.

Treat fuel as a CPU bound, not a resource budget. For full resource control, also
limit the size of arguments and the size of values returned from host functions.

## See also

- [`src/vm.rs`](../../src/vm.rs) — `tick()` and the main loop
