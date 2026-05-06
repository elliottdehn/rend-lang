// SLICE 4 (sketch): bytecode + fuel-metered VM.
//
// The compiler lowers typed AST to a register-based bytecode. Each instruction
// costs 1 fuel by default; some (calls, allocations) cost more. The host runs
// the program with a fuel budget:
//
//   let mut engine = Engine::new();
//   let module = engine.load(src)?;
//   let result = engine.call(&module, "main", &[], Fuel::new(100_000))?;
//
// If the budget runs out mid-execution, we get back a deterministic
// `OutOfFuel { steps_run, last_pc }`. No partial side effects on host state
// since slice 6+ writes are buffered until commit.
//
// This program exercises a fib that's expensive enough to demonstrate fuel
// would actually bite (fib(30) is exponential):

entry fn fib(n: i64) -> i64 {
    if n < 2 { return n; }
    return fib(n - 1) + fib(n - 2);
}

fn main() -> i64 {
    return fib(20);
}
