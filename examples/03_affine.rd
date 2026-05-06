// SLICE 3: affine ownership.
//
// `Resource` is the first non-Copy type. Two builtins manipulate it:
//   resource(n: i64) -> Resource     // construct
//   unwrap(r: Resource) -> i64       // destruct, consuming r
//
// The affine pass tracks each binding's live/moved state through control
// flow. Using a moved value is a compile-time error.
//
// Try uncommenting the second `unwrap(r)` — you'll get
//   "value 'r' used after move (moved at ...)"

entry fn process(r: Resource) -> i64 {
    return unwrap(r);
}

fn main() -> i64 {
    let r = resource(42);
    let value = process(r);
    // let again = unwrap(r);   // <-- compile error: r already moved
    return value;
}
