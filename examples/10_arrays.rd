// SLICE 10 (working): arrays — literals, indexing, length, storage, keys.
//
// Computes a recursive sum over an array stored in state. Arrays are values
// (not references), so passing one to a function copies it. With Copy
// element types this is fine; non-Copy element types are deferred to a
// future slice.

state log: [i64];

fn sum_from(xs: [i64], i: i64) -> i64 {
    if i >= len(xs) { return 0; }
    return xs[i] + sum_from(xs, i + 1);
}

entry fn sum(xs: [i64]) -> i64 {
    return sum_from(xs, 0);
}

fn main() -> i64 {
    log = [10, 20, 30, 40, 50];
    return sum(log);
}
