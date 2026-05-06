//! Slices 14 + 15: sets, dicts, and their comprehensions.

use rend::value::Value;
use rend::{run, Engine, Fuel};

#[test]
fn set_literal_dedupes() {
    let v = run("fn main() -> i64 { return set_len(set{1, 2, 1, 3, 2}); }").unwrap();
    assert_eq!(v, Value::Int(3));
}

#[test]
fn set_contains_works() {
    let v = run("
        fn main() -> bool {
            let s = set{10, 20, 30};
            return set_contains(s, 20);
        }
    ").unwrap();
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn set_insert_returns_new_set() {
    let v = run("
        fn main() -> i64 {
            let a = set{1, 2};
            let b = set_insert(a, 3);
            return set_len(a) + set_len(b);
        }
    ").unwrap();
    assert_eq!(v, Value::Int(5)); // 2 + 3
}

#[test]
fn set_insert_duplicate_is_noop() {
    let v = run("
        fn main() -> i64 {
            let s = set_insert(set_insert(set{1}, 1), 2);
            return set_len(s);
        }
    ").unwrap();
    assert_eq!(v, Value::Int(2));
}

#[test]
fn set_remove() {
    let v = run("
        fn main() -> bool {
            let s = set_remove(set{1, 2, 3}, 2);
            return set_contains(s, 2);
        }
    ").unwrap();
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn set_comprehension_with_filter() {
    let v = run("
        fn main() -> i64 {
            let s = set{x * x for x in [1, 2, 3, 4, 5] if x % 2 == 0};
            return set_len(s);
        }
    ").unwrap();
    assert_eq!(v, Value::Int(2)); // {4, 16}
}

#[test]
fn set_element_must_be_keyable() {
    // Resource is not keyable
    let err = run("fn main() -> i64 { return set_len(set{resource(1)}); }").unwrap_err();
    assert!(err.to_string().contains("keyable") || err.to_string().contains("element"));
}

#[test]
fn set_in_state_is_compile_error() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state s: set<i64>;
        fn main() -> i64 { return 0; }
    ";
    let err = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(err.to_string().contains("unsupported") || err.to_string().contains("set"));
}

#[test]
fn dict_literal_and_get() {
    let v = run(r#"
        fn main() -> i64 {
            let d = dict{1: 100, 2: 200};
            return dict_get(d, 2, 0);
        }
    "#).unwrap();
    assert_eq!(v, Value::Int(200));
}

#[test]
fn dict_get_missing_returns_default() {
    let v = run("
        fn main() -> i64 {
            let d = dict{1: 100};
            return dict_get(d, 99, -1);
        }
    ").unwrap();
    assert_eq!(v, Value::Int(-1));
}

#[test]
fn dict_set_overwrites_existing() {
    let v = run("
        fn main() -> i64 {
            let d = dict{1: 100};
            d = dict_set(d, 1, 999);
            return dict_get(d, 1, 0);
        }
    ").unwrap();
    assert_eq!(v, Value::Int(999));
}

#[test]
fn dict_has_and_remove() {
    let v = run("
        fn main() -> bool {
            let d = dict{1: 10, 2: 20};
            d = dict_remove(d, 1);
            return dict_has(d, 1);
        }
    ").unwrap();
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn dict_comprehension() {
    let v = run("
        fn main() -> i64 {
            let d = dict{x: x * x for x in [1, 2, 3, 4]};
            return dict_get(d, 3, 0);
        }
    ").unwrap();
    assert_eq!(v, Value::Int(9));
}

#[test]
fn dict_comprehension_with_filter() {
    let v = run("
        fn main() -> i64 {
            let d = dict{x: x for x in [1, 2, 3, 4, 5] if x > 2};
            return dict_len(d);
        }
    ").unwrap();
    assert_eq!(v, Value::Int(3));
}

#[test]
fn nested_set_in_dict_value() {
    let v = run("
        fn main() -> i64 {
            let d = dict{1: set{10, 20}, 2: set{30}};
            return set_len(dict_get(d, 1, set{0}));
        }
    ").unwrap();
    assert_eq!(v, Value::Int(2));
}

#[test]
fn dict_in_pipe_chain() {
    let v = run("
        fn main() -> i64 {
            return ([1, 2, 3, 4])
                |> dict{x: x * 10 for x in $$}
                |> dict_get($$, 3, 0);
        }
    ").unwrap();
    assert_eq!(v, Value::Int(30));
}

#[test]
fn example_20_sets_runs() {
    let kv = rend::kv::InMemoryKv::new();
    let src = std::fs::read_to_string("examples/20_sets.rd").unwrap();
    let out = Engine::new().execute(&src, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(10));
}

#[test]
fn example_21_dicts_runs() {
    let kv = rend::kv::InMemoryKv::new();
    let src = std::fs::read_to_string("examples/21_dicts.rd").unwrap();
    let out = Engine::new().execute(&src, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(4));
}
