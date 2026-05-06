// SLICE 13b (working): while + for-in loops with mutable Copy locals.
//
// Two new loop forms:
//
//   while cond { body }
//   for x in xs { body }       // x is bound fresh each iter; xs must be [T]
//
// Local variables introduced with `let` are mutable when their type is Copy
// (Int, Bool, Address, String, fixed-size arrays of Copy). Assignment uses
// the same `name = expr;` syntax that already exists for state.
//
// Non-Copy locals (Resource) remain affine — reassigning one is a compile
// error.

entry fn sum(xs: [i64]) -> i64 {
    let total = 0;
    for x in xs {
        total = total + x;
    }
    return total;
}

entry fn first_positive(xs: [i64]) -> i64 {
    let i = 0;
    while i < len(xs) {
        if xs[i] > 0 { return xs[i]; }
        i = i + 1;
    }
    return -1;
}

entry fn factorial(n: i64) -> i64 {
    let acc = 1;
    let i = 1;
    while i <= n {
        acc = acc * i;
        i = i + 1;
    }
    return acc;
}

fn main() -> i64 {
    let xs = [3, -1, 4, 1, 5, 9, 2, 6];
    let s = sum(xs);            // 29
    let f = factorial(6);       // 720
    return s + f;               // 749
}
