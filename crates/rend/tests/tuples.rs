//! Anonymous tuples — fixed-arity products. `(T1, T2, ...)` types,
//! `(a, b, ...)` literals, `t.0` / `t.1` indexed access, `let (a, b)`
//! destructuring. Local-only: not storable, not yet usable as map
//! keys, not allowed in event params.

use rend::value::Value;
use rend::run;

#[test]
fn tuple_literal_returned_from_fn() {
    let v = run("
        entry fn pair() -> (i64, i64) { return (1, 2); }
        fn main() -> i64 {
            let p = pair();
            return p.0 + p.1;
        }
    ").unwrap();
    assert_eq!(v, Value::int(3i64));
}

#[test]
fn tuple_destructuring_let_binds_each_element() {
    let v = run("
        entry fn pair() -> (i64, i64) { return (10, 20); }
        fn main() -> i64 {
            let (a, b) = pair();
            return a + b;
        }
    ").unwrap();
    assert_eq!(v, Value::int(30i64));
}

#[test]
fn three_arity_tuple_destructure() {
    let v = run(r#"
        entry fn triple() -> (i64, Address, bool) {
            return (7, address("0xa"), true);
        }
        fn main() -> i64 {
            let (n, _a, ok) = triple();
            if ok { return n; }
            return -1;
        }
    "#).unwrap();
    assert_eq!(v, Value::int(7i64));
}

#[test]
fn nested_tuple() {
    let v = run("
        entry fn pair_of_pairs() -> ((i64, i64), (i64, i64)) {
            return ((1, 2), (3, 4));
        }
        fn main() -> i64 {
            let p = pair_of_pairs();
            return p.0.0 + p.0.1 + p.1.0 + p.1.1;
        }
    ").unwrap();
    assert_eq!(v, Value::int(10i64));
}

#[test]
fn tuple_in_param_position() {
    let v = run("
        entry fn sum_pair(p: (i64, i64)) -> i64 {
            return p.0 + p.1;
        }
        fn main() -> i64 {
            return sum_pair((100, 200));
        }
    ").unwrap();
    assert_eq!(v, Value::int(300i64));
}

#[test]
fn destructure_arity_mismatch_is_compile_error() {
    let err = run("
        entry fn pair() -> (i64, i64) { return (1, 2); }
        fn main() -> i64 {
            let (a, b, c) = pair();
            return a;
        }
    ").unwrap_err();
    assert!(err.to_string().contains("name") || err.to_string().contains("component"));
}

#[test]
fn tuple_index_out_of_bounds_is_compile_error() {
    let err = run("
        entry fn pair() -> (i64, i64) { return (1, 2); }
        fn main() -> i64 {
            let p = pair();
            return p.5;
        }
    ").unwrap_err();
    assert!(err.to_string().contains("index"));
}

#[test]
fn tuple_destructure_on_non_tuple_is_compile_error() {
    let err = run("
        fn main() -> i64 {
            let (a, b) = 7;
            return a + b;
        }
    ").unwrap_err();
    assert!(err.to_string().contains("tuple"));
}

#[test]
fn destructured_locals_are_independent_after_binding() {
    let v = run("
        entry fn split() -> (i64, i64) { return (10, 20); }
        fn main() -> i64 {
            let (a, b) = split();
            let a = a + 1;     // shadowing-style rebind via let
            return a + b;      // 11 + 20 = 31
        }
    ").unwrap();
    assert_eq!(v, Value::int(31i64));
}

#[test]
fn tuple_state_is_compile_error() {
    // Tuples aren't storable — declaring a tuple-typed state is rejected.
    let kv = rend::kv::InMemoryKv::new();
    let err = rend::Engine::new()
        .execute(
            "state s: (i64, i64); fn main() -> i64 { return 0; }",
            rend::Fuel::new(1000),
            &kv,
        )
        .unwrap_err();
    assert!(err.to_string().contains("unsupported") || err.to_string().contains("tuple"));
}

#[test]
fn tuple_paren_expr_disambiguation() {
    // `(a)` is a parenthesized expression; `(a, b)` is a 2-tuple.
    // The expression `((1 + 2))` must NOT become a tuple.
    let v = run("
        fn main() -> i64 {
            let n = ((1 + 2));
            return n;
        }
    ").unwrap();
    assert_eq!(v, Value::int(3i64));
}

#[test]
fn tuple_with_lazy_state_reads_destructures_correctly() {
    let mut kv = rend::kv::InMemoryKv::new();
    let setup = "
        state a: i64; state b: i64;
        fn main() -> i64 {
            a = 100; b = 200;
            return 0;
        }
    ";
    let setup_out = rend::Engine::new()
        .execute(setup, rend::Fuel::new(10_000), &kv)
        .unwrap();
    kv.apply(&setup_out.writes);

    let src = "
        state a: i64; state b: i64;
        entry fn read_pair() -> (i64, i64) { return (a, b); }
        fn main() -> i64 {
            let (x, y) = read_pair();
            return x + y;
        }
    ";
    let out = rend::Engine::new()
        .execute(src, rend::Fuel::new(10_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::int(300i64));
}
