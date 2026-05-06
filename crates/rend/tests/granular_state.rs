//! Granular state for struct-typed state slots: each leaf field gets its own
//! KV cell at `child(parent_key, field_name)`. Tests focus on the storage
//! layout and the resulting OCC RW-set granularity, since execution semantics
//! are already covered by struct_field_paths.rs.

use rend::ast::Type;
use rend::hashing::{child, state_root};
use rend::value::Value;
use rend::{Engine, Fuel};

fn engine() -> Engine {
    Engine::new()
}

#[test]
fn struct_state_writes_one_cell_per_leaf() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        fn main() -> i64 {
            origin = Point { x: 10, y: 20 };
            return 0;
        }
    ";
    let out = engine().execute(src, Fuel::new(10_000), &kv).unwrap();
    let root = state_root("main", "origin");
    let x_key = child(root, b"x");
    let y_key = child(root, b"y");
    // Two leaf writes; root is just a namespace and gets nothing.
    assert_eq!(out.writes.get(&root), None);
    assert_eq!(out.writes.get(&x_key), Some(&Value::Int(10)));
    assert_eq!(out.writes.get(&y_key), Some(&Value::Int(20)));
}

#[test]
fn writing_one_field_only_writes_that_cell() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        fn main() -> i64 {
            origin.x = 7;
            return 0;
        }
    ";
    let out = engine().execute(src, Fuel::new(10_000), &kv).unwrap();
    let root = state_root("main", "origin");
    let x_key = child(root, b"x");
    let y_key = child(root, b"y");
    // Only x is in the write set; y is untouched.
    assert_eq!(out.writes.len(), 1);
    assert_eq!(out.writes.get(&x_key), Some(&Value::Int(7)));
    assert!(!out.writes.contains_key(&y_key));
}

#[test]
fn reading_one_field_only_reads_that_cell() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        fn main() -> i64 { return origin.x; }
    ";
    let out = engine().execute(src, Fuel::new(10_000), &kv).unwrap();
    let root = state_root("main", "origin");
    let x_key = child(root, b"x");
    let y_key = child(root, b"y");
    // Only x is observed; y stays out of the read set, so a concurrent
    // transaction modifying y does not conflict with this one.
    assert_eq!(out.reads.len(), 1);
    assert!(out.reads.contains_key(&x_key));
    assert!(!out.reads.contains_key(&y_key));
}

#[test]
fn reading_whole_struct_observes_every_leaf() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        fn main() -> i64 {
            let p = origin;
            return p.x + p.y;
        }
    ";
    let out = engine().execute(src, Fuel::new(10_000), &kv).unwrap();
    let root = state_root("main", "origin");
    let x_key = child(root, b"x");
    let y_key = child(root, b"y");
    assert_eq!(out.reads.len(), 2);
    assert!(out.reads.contains_key(&x_key));
    assert!(out.reads.contains_key(&y_key));
}

#[test]
fn nested_struct_paths_are_per_leaf() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Inner { v: i64, w: i64 }
        struct Outer { a: Inner, b: i64 }
        state s: Outer;
        fn main() -> i64 {
            s.a.v = 99;
            return 0;
        }
    ";
    let out = engine().execute(src, Fuel::new(10_000), &kv).unwrap();
    let root = state_root("main", "s");
    let a_key = child(root, b"a");
    let v_key = child(a_key, b"v");
    let w_key = child(a_key, b"w");
    let b_key = child(root, b"b");
    // Only `s.a.v` is written; the other leaves are untouched.
    assert_eq!(out.writes.len(), 1);
    assert_eq!(out.writes.get(&v_key), Some(&Value::Int(99)));
    assert!(!out.writes.contains_key(&w_key));
    assert!(!out.writes.contains_key(&b_key));
}

#[test]
fn disjoint_field_writes_dont_conflict_under_occ() {
    // Two transactions each mutate a different field of the same state.
    // Their write sets are disjoint, so OCC validation must accept both.
    let mut kv = rend::kv::InMemoryKv::new();
    let init = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        fn main() -> i64 { return 0; }
    ";
    engine().execute(init, Fuel::new(10_000), &kv).unwrap();

    let src_x = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        fn main() -> i64 { origin.x = 11; return 0; }
    ";
    let src_y = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        fn main() -> i64 { origin.y = 22; return 0; }
    ";
    let out_x = engine().execute(src_x, Fuel::new(10_000), &kv).unwrap();
    let out_y = engine().execute(src_y, Fuel::new(10_000), &kv).unwrap();
    let root = state_root("main", "origin");
    let x_key = child(root, b"x");
    let y_key = child(root, b"y");
    // Each transaction reads/writes only its own leaf — disjoint sets.
    assert_eq!(out_x.writes.keys().collect::<Vec<_>>(), vec![&x_key]);
    assert_eq!(out_y.writes.keys().collect::<Vec<_>>(), vec![&y_key]);

    // Apply both — order doesn't matter, no conflict.
    kv.apply(&out_x.writes);
    kv.apply(&out_y.writes);
    let read_back = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        fn main() -> i64 { return origin.x + origin.y; }
    ";
    let out = engine().execute(read_back, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(33));
}

#[test]
fn assigning_substruct_splits_into_leaf_writes() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Inner { v: i64, w: i64 }
        struct Outer { a: Inner, b: i64 }
        state s: Outer;
        fn main() -> i64 {
            s.a = Inner { v: 1, w: 2 };
            return 0;
        }
    ";
    let out = engine().execute(src, Fuel::new(10_000), &kv).unwrap();
    let root = state_root("main", "s");
    let a_key = child(root, b"a");
    let v_key = child(a_key, b"v");
    let w_key = child(a_key, b"w");
    // s.a's two leaves are written; s.b is untouched.
    assert_eq!(out.writes.len(), 2);
    assert_eq!(out.writes.get(&v_key), Some(&Value::Int(1)));
    assert_eq!(out.writes.get(&w_key), Some(&Value::Int(2)));
}

#[test]
fn array_field_stays_one_cell() {
    // Arrays inside structs are NOT sharded — one cell holds the whole
    // array. Sharding arrays is a separate, much harder problem.
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Bag { items: [i64], count: i64 }
        state bag: Bag;
        fn main() -> i64 {
            bag.items = [1, 2, 3];
            return 0;
        }
    ";
    let out = engine().execute(src, Fuel::new(10_000), &kv).unwrap();
    let root = state_root("main", "bag");
    let items_key = child(root, b"items");
    let count_key = child(root, b"count");
    assert_eq!(out.writes.len(), 1);
    assert_eq!(
        out.writes.get(&items_key),
        Some(&Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)])),
    );
    assert!(!out.writes.contains_key(&count_key));
}

#[test]
fn read_of_nested_substruct_is_granular() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Inner { v: i64, w: i64 }
        struct Outer { a: Inner, b: i64 }
        state s: Outer;
        fn main() -> i64 {
            let inner = s.a;
            return inner.v + inner.w;
        }
    ";
    let out = engine().execute(src, Fuel::new(10_000), &kv).unwrap();
    let root = state_root("main", "s");
    let a_key = child(root, b"a");
    let v_key = child(a_key, b"v");
    let w_key = child(a_key, b"w");
    let b_key = child(root, b"b");
    // Reading the substruct s.a touches v and w, but not b.
    assert!(out.reads.contains_key(&v_key));
    assert!(out.reads.contains_key(&w_key));
    assert!(!out.reads.contains_key(&b_key));
}

#[test]
fn local_variable_shadowing_state_uses_local_path() {
    // A local `s` shadowing a state of the same name must operate on the
    // local — no granular cell access.
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Point { x: i64, y: i64 }
        state s: Point;
        fn main() -> i64 {
            let s = Point { x: 100, y: 200 };
            return s.x + s.y;
        }
    ";
    let out = engine().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(300));
    // The local shadow means no state cells are touched at all.
    let root = state_root("main", "s");
    let x_key = child(root, b"x");
    let y_key = child(root, b"y");
    assert!(!out.reads.contains_key(&x_key));
    assert!(!out.reads.contains_key(&y_key));
    assert!(out.writes.is_empty());
}

#[test]
fn occ_commits_disjoint_field_writes_without_conflict() {
    // Run two txs in parallel: one mutates origin.x, the other origin.y.
    // Their RW sets are disjoint (different leaf cells), so OCC must commit
    // both without re-execution.
    let mut kv = rend::kv::InMemoryKv::new();
    let init = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        fn main() -> i64 { return 0; }
    ";
    let setup = engine().execute(init, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&setup.writes);

    let src_x = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        fn main() -> i64 { origin.x = 11; return origin.x; }
    ";
    let src_y = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        fn main() -> i64 { origin.y = 22; return origin.y; }
    ";
    let req_x = rend::occ::TxRequest { src: src_x, fuel_first: 10_000, fuel_retry: 10_000 };
    let req_y = rend::occ::TxRequest { src: src_y, fuel_first: 10_000, fuel_retry: 10_000 };
    let report = rend::occ::commit_batch(&engine(), &mut kv, &[req_x, req_y]).unwrap();
    assert_eq!(report.conflicts, 0, "disjoint leaves should not conflict");
    assert!(report.txs.iter().all(|t| !t.re_executed));
}

#[test]
fn primitive_state_unaffected() {
    // Non-struct states still use a single cell at the state root.
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state n: i64;
        fn main() -> i64 { n = 42; return n; }
    ";
    let out = engine().execute(src, Fuel::new(10_000), &kv).unwrap();
    let root = state_root("main", "n");
    assert_eq!(out.writes.get(&root), Some(&Value::Int(42)));
    let _ = Type::Int; // silence import
}
