// SLICE 2+: explicit param + return types.
// The type checker rejects mismatches before we ever execute, so what would
// have been a runtime "type error" in slice 1 is now a compile error.
//
// Try changing `return a + b` to `return a == b` and the typeck pass will
// reject it: the function declares `-> i64` but you'd be returning bool.

entry fn add(a: i64, b: i64) -> i64 {
    return a + b;
}

entry fn pick(cond: bool, hi: i64, lo: i64) -> i64 {
    if cond { return hi; } else { return lo; }
}

fn main() -> i64 {
    let x = add(20, 22);
    return pick(x > 40, x, 0);
}
