//! Pipe notation: `head |> step` with `$$` placeholder.

use rend::value::Value;
use rend::{run, Engine, Fuel};

#[test]
fn single_pipe() {
    let v = run("
        entry fn double(n: i64) -> i64 { return n * 2; }
        fn main() -> i64 { return (5) |> double($$); }
    ").unwrap();
    assert_eq!(v, Value::Int(10));
}

#[test]
fn left_associative_chain() {
    let v = run("
        entry fn double(n: i64) -> i64 { return n * 2; }
        entry fn square(n: i64) -> i64 { return n * n; }
        fn main() -> i64 {
            return (10)
                |> double($$)
                |> square($$);
        }
    ").unwrap();
    assert_eq!(v, Value::Int(400));
}

#[test]
fn dollar_dollar_can_appear_anywhere() {
    let v = run("
        entry fn add(a: i64, b: i64) -> i64 { return a + b; }
        fn main() -> i64 {
            return (3)
                |> add($$, 1)            // $$ as first arg
                |> add(10, $$)           // $$ as second arg
                |> $$ * $$;              // $$ used twice
        }
    ").unwrap();
    assert_eq!(v, Value::Int(196));    // ((3+1)+10)^2 = 14^2 = 196
}

#[test]
fn dollar_dollar_outside_pipe_is_an_error() {
    let err = run("fn main() -> i64 { return $$ + 1; }").unwrap_err();
    assert!(err.to_string().contains("$$"));
}

#[test]
fn arithmetic_in_pipe_stage() {
    let v = run("
        fn main() -> i64 { return (10) |> ($$ - 1) * 2 |> $$ + 1; }
    ").unwrap();
    assert_eq!(v, Value::Int(19));   // ((10-1)*2)+1 = 19
}

#[test]
fn pipe_with_indexing() {
    let v = run("
        fn main() -> i64 {
            return ([100, 200, 300])
                |> $$[1];
        }
    ").unwrap();
    assert_eq!(v, Value::Int(200));
}

#[test]
fn pipe_through_state_with_engine() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state n: i64;
        entry fn double(x: i64) -> i64 { return x * 2; }
        fn main() -> i64 {
            n = 10;
            return n |> double($$) |> $$ + 1;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(21));
}

#[test]
fn example_23_pipe_runs() {
    let kv = rend::kv::InMemoryKv::new();
    let src = std::fs::read_to_string("examples/23_pipe.rd").unwrap();
    let out = Engine::new().execute(&src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(800));
}
