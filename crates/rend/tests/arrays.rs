//! Slice 10: arrays — literal, index, length, storage, keys.

use rend::ast::Type;
use rend::hashing::{child, state_root};
use rend::kv::InMemoryKv;
use rend::serialize::serialize;
use rend::value::Value;
use rend::{run, Engine, Fuel};

#[test]
fn array_literal_evaluates_to_array_value() {
    let v = run("fn main() -> [i64] { return [1, 2, 3]; }").unwrap();
    assert_eq!(v, Value::Array(vec![Value::int(1i64), Value::int(2i64), Value::int(3i64)]));
}

#[test]
fn array_index_reads_element() {
    let v = run("fn main() -> i64 { return [10, 20, 30][1]; }").unwrap();
    assert_eq!(v, Value::int(20i64));
}

#[test]
fn array_index_out_of_bounds_is_runtime_error() {
    let err = run("fn main() -> i64 { return [1, 2][5]; }").unwrap_err();
    assert!(err.to_string().contains("out of bounds"), "got {err}");
}

#[test]
fn len_returns_array_length() {
    let v = run("fn main() -> i64 { return len([10, 20, 30, 40]); }").unwrap();
    assert_eq!(v, Value::int(4i64));
}

#[test]
fn len_works_on_strings_too() {
    let v = run(r#"fn main() -> i64 { return len("hello"); }"#).unwrap();
    assert_eq!(v, Value::int(5i64));
}

#[test]
fn arrays_are_homogeneous_at_compile_time() {
    let err = run("fn main() -> i64 { return [1, true][0]; }").unwrap_err();
    assert!(err.to_string().contains("expected"), "got {err}");
}

#[test]
fn array_passes_through_function_calls() {
    let src = "
        entry fn sum(xs: [i64]) -> i64 {
            let n = len(xs);
            let i = 0;
            let total = 0;
            return seq(xs, n, i, total);
        }
        fn seq(xs: [i64], n: i64, i: i64, total: i64) -> i64 {
            if i >= n { return total; }
            return seq(xs, n, i + 1, total + xs[i]);
        }
        fn main() -> i64 { return sum([1, 2, 3, 4, 5]); }
    ";
    let v = run(src).unwrap();
    assert_eq!(v, Value::int(15i64));
}

#[test]
fn arrays_can_be_stored_in_state() {
    let mut kv = InMemoryKv::new();
    let engine = Engine::new();
    let src = "
        state log: [i64];
        fn main() -> i64 {
            log = [10, 20, 30];
            return len(log);
        }
    ";
    let out = engine.execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(3i64));
    kv.apply(&out.writes);

    // re-read after commit
    let stored = kv.get_typed(state_root("main", "log"), &Type::Array(Box::new(Type::Int)));
    assert_eq!(
        stored,
        Some(Value::Array(vec![Value::int(10i64), Value::int(20i64), Value::int(30i64)])),
    );
}

#[test]
fn arrays_can_be_map_keys() {
    let kv = InMemoryKv::new();
    let src = "
        state visits: map<[i64], i64>;
        fn main() -> i64 {
            visits[[1, 2]] = visits[[1, 2]] + 1;
            visits[[1, 2]] = visits[[1, 2]] + 1;
            visits[[3, 4]] = visits[[3, 4]] + 5;
            return visits[[1, 2]] + visits[[3, 4]];
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(7i64)); // 2 + 5

    let key12 = child(
        state_root("main", "visits"),
        &serialize(&Value::Array(vec![Value::int(1i64), Value::int(2i64)])),
    );
    let key34 = child(
        state_root("main", "visits"),
        &serialize(&Value::Array(vec![Value::int(3i64), Value::int(4i64)])),
    );
    assert_eq!(out.writes.get(&key12), Some(&Value::int(2i64)));
    assert_eq!(out.writes.get(&key34), Some(&Value::int(5i64)));
    assert_ne!(key12, key34, "different array keys must not collide");
}

#[test]
fn nested_array_round_trips_through_storage() {
    let mut kv = InMemoryKv::new();
    let engine = Engine::new();
    let src = "
        state matrix: [[i64]];
        fn main() -> i64 {
            matrix = [[1, 2], [3, 4]];
            return matrix[1][0];
        }
    ";
    let out = engine.execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(3i64));
    kv.apply(&out.writes);

    let stored = kv.get_typed(
        state_root("main", "matrix"),
        &Type::Array(Box::new(Type::Array(Box::new(Type::Int)))),
    );
    assert_eq!(
        stored,
        Some(Value::Array(vec![
            Value::Array(vec![Value::int(1i64), Value::int(2i64)]),
            Value::Array(vec![Value::int(3i64), Value::int(4i64)]),
        ])),
    );
}

#[test]
fn string_array_keys() {
    let kv = InMemoryKv::new();
    let src = r#"
        state seen: map<[string], bool>;
        fn main() -> bool {
            seen[["alice", "bob"]] = true;
            return seen[["alice", "bob"]];
        }
    "#;
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Bool(true));
}
