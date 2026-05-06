// SLICE 12+ (sketch): native compilation via Cranelift.
//
// The frontend (lex/parse/typeck/affine) is unchanged. The bytecode VM
// remains as the reference implementation and the deterministic baseline.
// In addition, the engine gains an opt-in JIT codegen path:
//
//   let mut engine = Engine::new();
//   engine.set_codegen(Codegen::Cranelift);  // or Codegen::Bytecode (default)
//   engine.execute(src, Fuel::new(1_000_000_000), &kv)?;
//
// Invariants the JIT must preserve:
//   1. Determinism — every observable output is identical to the bytecode
//      VM for the same input + state snapshot.
//   2. Fuel metering — Cranelift inlines a fuel-decrement at every basic
//      block boundary; out-of-fuel raises the same Error as the VM.
//   3. KV/Tx interaction — KvGet/KvPut lower to thunk calls back into the
//      runtime so the read/write set is preserved exactly.
//   4. Sandbox — no codegen of host pointers; imports go through the same
//      Host trait dispatch.
//
// Naive fib is enough to motivate JIT: bytecode runs ~5x slower than a
// trivial native equivalent.

entry fn fib(n: i64) -> i64 {
    if n < 2 { return n; }
    return fib(n - 1) + fib(n - 2);
}

fn main() -> i64 {
    return fib(20);
}
