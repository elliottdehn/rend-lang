// SLICES 16+17+18 (working together): the three small ergonomics features.
//
//   1. Mutable struct field paths: `p.x = 5;` works directly. Nested:
//      `obj.q.x = 5;` lowers to read-modify-write of each level.
//
//   2. Multi-generator comprehensions: a single comprehension can have
//      multiple `for` clauses (Cartesian product) and any number of `if`
//      filters. Same shape applies to `[ ]`, `set{ }`, and `dict{ }`.
//
//   3. Empty literal type inference: `let x: T = ...;` lets the annotation
//      drive the type of an empty `[]`, `set{}`, or `dict{}`.
//
// All three compose freely.

struct Point { x: i64, y: i64 }

state log: [Point];

entry fn translate_x(p: Point, dx: i64) -> Point {
    let q = p;
    q.x = q.x + dx;          // (1) mutable field via local Copy
    return q;
}

entry fn pairs_summing_to(xs: [i64], ys: [i64], target: i64) -> [Point] {
    return [Point { x: a, y: b }
            for a in xs
            for b in ys
            if a + b == target];           // (2) multi-gen + filter
}

fn main() -> i64 {
    let pts: [Point] = [];                         // (3) empty literal inferred
    log = pts;

    log = pairs_summing_to([1, 2, 3], [4, 5, 6], 7);    // expects 3 pairs
    let s = 0;
    for p in log {
        let q = translate_x(p, 100);
        s = s + q.x + q.y;
    }
    return s;
    // pairs that sum to 7: (1,6) (2,5) (3,4)
    // after +100 to x: (101,6)(102,5)(103,4) → totals 107+107+107 = 321
}
