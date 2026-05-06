# Quick start

`rend` is a Rust crate. Add it to your `Cargo.toml`, write a `.rd` source string, and
run it through `Engine`.

## Smallest possible example

```rust
use rend::{Engine, Fuel};
use rend::value::Value;

fn main() {
    let src = "
        module main;
        fn main() -> u64 {
            return 1u64 + 2u64;
        }
    ";
    let result = Engine::new().run(src, Fuel::new(1_000)).unwrap();
    assert_eq!(result, Value::U64(3));
}
```

`Engine::new().run(...)` is the simplest entry point: parse, typecheck, compile to
bytecode, run `main()`, return the value. There's no persistent state and no host
imports.

## A persistent counter

```rust
use rend::{Engine, Fuel};
use rend::kv::InMemoryKv;
use rend::value::Value;

fn main() {
    let src = "
        module main;
        state ctr: i64;
        fn main() -> i64 {
            ctr = ctr + 1;
            return ctr;
        }
    ";

    let mut kv = InMemoryKv::new();

    let out1 = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&out1.writes);          // commit the write set
    assert_eq!(out1.result, Value::Int(1));

    let out2 = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&out2.writes);
    assert_eq!(out2.result, Value::Int(2));
}
```

Notice the host explicitly applies `out.writes` to commit. The engine never mutates
storage on your behalf — it produces a write set you choose to apply (or discard, if
the tx is rejected by your own logic).

## A view query (no OCC, no writes)

`Engine::query` runs against compiled artifacts. To query the counter from above
without modifying it, compile a `view` tx against the deployed program:

```rust
let engine = Engine::new();
let counter = engine.compile("
    module counter;
    state ctr: i64;
    entry view fn current() -> i64 { return ctr; }
")?;
// (deploy + apply some increments here so kv has state)

let q_tx = engine.compile_tx("
    module q;
    view fn main() -> i64 { return counter::current(); }
", &[counter.clone()])?;

let q = engine.query(&q_tx, &[counter], Fuel::new(10_000), &kv)?;
assert_eq!(q.result, Value::Int(2));
```

`view` is verified by the effects classifier — if the body wrote state or emitted an
event, this would be a compile error. `Engine::query` runs the read-only path: no
write log, no conflict tracking.

## Where to go next

- [language reference](README.md#language-reference) — the syntax and semantics
- [Engine API](runtime/engine.md) — the full embedding surface
- [`examples/`](../examples) — working programs for every feature
