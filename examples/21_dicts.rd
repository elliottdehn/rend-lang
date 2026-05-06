// SLICE 15 (working): dicts — non-storage, in-memory.
//
// `dict<K, V>` mirrors `set<T>` in being a value type. Distinct from
// `state map<K, V>` (storage-backed): dicts live entirely inside a single
// execution and never touch the KV.
//
// Why both?
//   - state map<K, V>:   reads/writes a KV cell per access, contributes to
//                        the OCC RW set, costs gas per cell.
//   - local dict<K, V>:  pure in-memory, free of KV traffic. Useful for
//                        intermediate computations.
//
// Literal:        `dict{1: "a", 2: "b"}`
// Comprehension:  `dict{k: v for x in iter (if cond)?}`
// Builtins:       dict_get, dict_set, dict_remove, dict_has, dict_len
//
// `dict_get(d, k, default)` takes an explicit default so the caller is
// always type-aware about what missing means.

entry fn freq(xs: [i64]) -> dict<i64, i64> {
    let d = dict{0: 0};
    d = dict_remove(d, 0);     // start empty
    for x in xs {
        d = dict_set(d, x, dict_get(d, x, 0) + 1);
    }
    return d;
}

fn main() -> i64 {
    let f = freq([1, 2, 1, 3, 2, 1]);
    return dict_get(f, 1, 0) + dict_get(f, 3, 0);   // 3 + 1 = 4
}
