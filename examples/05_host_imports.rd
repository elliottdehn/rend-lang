// SLICE 5: host imports.
//
// The guest declares typed imports. The host registers Rust closures matching
// those signatures. At link time the engine verifies every declared import
// has a binding; missing ones are a compile-time error.
//
// Imports must be deterministic w.r.t. their inputs for OCC soundness; this
// is the host's responsibility (the runtime cannot enforce it).
//
// Host wiring (Rust):
//
//   let mut engine = Engine::new();
//   engine.bind("host_log", |args| {
//       if let [Value::Int(n)] = args {
//           println!("[guest] {n}");
//       }
//       Ok(Value::Unit)
//   });
//   engine.bind("host_double", |args| {
//       if let [Value::Int(n)] = args { Ok(Value::Int(n * 2)) }
//       else { Err("host_double: expected i64".into()) }
//   });
//   engine.run(src, Fuel::new(100_000))?;

import host_log: fn(i64);
import host_double: fn(i64) -> i64;

fn main() -> i64 {
    let n = host_double(21);
    host_log(n);
    return n;
}
