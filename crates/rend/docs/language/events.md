# Events

An event is a structured log entry the host can read after the transaction finishes.
Events are scoped by module name — the host sees `module::EventName` records.

## Declaration

```rd
event Transfer(from: Address, to: Address, amount: u64);
event Approval(owner: Address, spender: Address, amount: u64);
```

Each event has a name and an ordered, named parameter list. There's no payload type
restriction beyond "must be a value type."

## Emission

```rd
entry fn transfer(to: Address, amount: u64) -> u64 {
    let from = msg_sender();
    balances[from] = balances[from] - amount;
    balances[to]   = balances[to]   + amount;
    emit Transfer(from, to, amount);
    return amount;
}
```

`emit` is a statement, not an expression. It writes a record into the per-transaction
event log; the host sees it after `Engine::execute_*` returns:

```rust
let outcome = engine.execute(src, fuel, &kv)?;
for event in &outcome.events {
    println!("{}::{}: {:?}", event.module, event.name, event.args);
}
```

## Effect classification

Emitting an event is `Impure`. A `view` or `pure` function that emits is a compile
error — the read-only `Engine::query` path doesn't surface events at all.

## See also

- [`examples/27_token.rd`](../../examples/27_token.rd) — `Transfer` and `Approval` events
- [`tests/events.rs`](../../tests/events.rs) — emission, log accumulation, module scoping
