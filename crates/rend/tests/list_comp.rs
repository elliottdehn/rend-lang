//! Slice 13c: list comprehensions.

use rend::value::Value;
use rend::{run, Engine, Fuel};

#[test]
fn map_over_array() {
    let v = run("fn main() -> [i64] { return [x * 2 for x in [1, 2, 3]]; }").unwrap();
    assert_eq!(
        v,
        Value::Array(vec![Value::Int(2), Value::Int(4), Value::Int(6)]),
    );
}

#[test]
fn filter_only_evens() {
    let v = run(
        "fn main() -> [i64] { return [x * x for x in [1, 2, 3, 4, 5] if x % 2 == 0]; }",
    )
    .unwrap();
    assert_eq!(v, Value::Array(vec![Value::Int(4), Value::Int(16)]));
}

#[test]
fn filter_that_excludes_everything_yields_empty() {
    let v = run(
        "fn main() -> [i64] { return [x for x in [1, 2, 3] if x > 100]; }",
    )
    .unwrap();
    assert_eq!(v, Value::Array(Vec::new()));
}

#[test]
fn list_comp_inside_function() {
    let v = run("
        entry fn doubled(xs: [i64]) -> [i64] {
            return [x * 2 for x in xs];
        }
        fn main() -> i64 {
            let d = doubled([10, 20, 30]);
            return d[0] + d[1] + d[2];
        }
    ").unwrap();
    assert_eq!(v, Value::Int(120));
}

#[test]
fn list_comp_with_state() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state seen: [i64];
        fn main() -> i64 {
            seen = [10, 20, 30, 40];
            let big = [x for x in seen if x >= 25];
            return len(big);
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(2));
}

#[test]
fn filter_must_be_bool_compile_error() {
    let err = run("fn main() -> [i64] { return [x for x in [1] if x]; }").unwrap_err();
    assert!(err.to_string().contains("bool"));
}

#[test]
fn iter_must_be_array() {
    let err = run("fn main() -> [i64] { return [x for x in 5]; }").unwrap_err();
    assert!(err.to_string().contains("array"));
}

#[test]
fn example_19_runs() {
    let kv = rend::kv::InMemoryKv::new();
    let src = std::fs::read_to_string("examples/19_list_comprehensions.rd").unwrap();
    let out = Engine::new().execute(&src, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(7));
}

#[test]
fn list_comp_in_pipe() {
    let v = run("
        fn main() -> i64 {
            return ([1, 2, 3, 4])
                |> [x * 10 for x in $$]
                |> $$[2];
        }
    ").unwrap();
    assert_eq!(v, Value::Int(30));
}
