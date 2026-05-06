// SLICE 13c + 17 (working): list comprehensions, Python-style.
//
// Syntax:
//   [expr <clause>+]
// where each clause is `for x in iter` or `if cond`. `for` clauses chain to
// produce a Cartesian product; `if` clauses gate iterations they enclose.
//
// Examples:
//   [x * 2 for x in xs]
//   [x * x for x in xs if x % 2 == 0]
//   [a + b for a in xs for b in ys]                 // 2-D product
//   [a + b for a in xs if a > 0 for b in ys if b > 0]
//
// Equivalent to nested for-in loops accumulating via array-append.

entry fn doubled(xs: [i64]) -> [i64] {
    return [x * 2 for x in xs];
}

entry fn evens_squared(xs: [i64]) -> [i64] {
    return [x * x for x in xs if x % 2 == 0];
}

fn main() -> i64 {
    let xs = [1, 2, 3, 4, 5];
    let d = doubled(xs);             // [2, 4, 6, 8, 10]
    let e = evens_squared(xs);       // [4, 16]
    return len(d) + len(e);          // 5 + 2 = 7
}
