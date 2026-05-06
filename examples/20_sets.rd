// SLICE 14 (working): sets — non-storage, in-memory.
//
// `set<T>` is an ordered (insertion-order) collection of unique elements
// of any keyable type. Sets exist only inside an execution; for
// persistence use `state map<T, bool>`.
//
// Literal:        `set{1, 2, 3}`
// Comprehension:  `set{e for x in iter (if cond)?}`
// Builtins:       set_insert, set_remove, set_contains, set_len
//
// Construction de-duplicates eagerly, so `set{1, 2, 1}` has length 2.

entry fn unique_count(xs: [i64]) -> i64 {
    let s = set{x for x in xs};
    return set_len(s);
}

entry fn evens_squared(xs: [i64]) -> set<i64> {
    return set{x * x for x in xs if x % 2 == 0};
}

fn main() -> i64 {
    let n = unique_count([3, 1, 4, 1, 5, 9, 2, 6, 5, 3, 5]);   // 7
    let s = evens_squared([1, 2, 3, 4, 5, 6]);                  // {4, 16, 36}
    return n + set_len(s);                                       // 7 + 3 = 10
}
