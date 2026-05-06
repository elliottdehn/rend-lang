// SLICE 6: persistent state — single i64/bool slots.
//
// `state` declarations name a typed KV slot. Inside functions you read by
// using the name as an expression and write with `name = expr;`. The compiler
// emits KvGet/KvPut. The runtime returns the read & write sets to the host.
//
// Host wiring (Rust):
//
//   let kv = InMemoryKv::new();
//   let outcome = engine.execute(src, Fuel::new(10_000), &kv)?;
//   // outcome.result      → Value::Int(1)
//   // outcome.reads       → { "count" -> 0 }   (default for unset i64)
//   // outcome.writes      → { "count" -> 1 }
//   kv.apply(&outcome.writes);                  // commit
//
// Re-running against the same kv now produces:
//   reads = { "count" -> 1 }, writes = { "count" -> 2 }, result = 2.

state count: i64;

entry fn incr() {
    count = count + 1;
}

fn main() -> i64 {
    incr();
    return count;
}
