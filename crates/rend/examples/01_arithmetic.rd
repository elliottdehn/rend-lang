// SLICE 1+: arithmetic, control flow, recursion.
// Runs as of slice 1 with untyped fns; runs as of slice 2 with the explicit
// type annotations below. Either way, main() returns 6765.

entry fn fib(n: i64) -> i64 {
    if n < 2 { return n; }
    return fib(n - 1) + fib(n - 2);
}

fn main() -> i64 {
    return fib(20);
}
