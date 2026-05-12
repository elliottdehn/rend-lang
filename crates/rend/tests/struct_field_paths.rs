//! Slice 16: mutable struct field paths.
//!
//! `p.x = 5;` mutates a Copy struct local in place. Nested:
//! `obj.q.x = 5;` lowers to read-modify-write of each level. The same
//! mechanism applies to state struct slots (`state log: Point;` →
//! `log.x = 5;` reads, modifies, KvPuts).

use rend::value::Value;
use rend::{run, Engine, Fuel};

#[test]
fn assign_field_of_local_struct() {
    let v = run("
        struct Point { x: i64, y: i64 }
        fn main() -> i64 {
            let p = Point { x: 1, y: 2 };
            p.x = 100;
            return p.x + p.y;
        }
    ").unwrap();
    assert_eq!(v, Value::int(102i64));
}

#[test]
fn assign_field_then_field() {
    let v = run("
        struct Point { x: i64, y: i64 }
        fn main() -> i64 {
            let p = Point { x: 1, y: 2 };
            p.x = 10;
            p.y = 20;
            return p.x + p.y;
        }
    ").unwrap();
    assert_eq!(v, Value::int(30i64));
}

#[test]
fn nested_field_path_mutation() {
    let v = run("
        struct Inner { v: i64 }
        struct Outer { a: Inner, b: i64 }
        fn main() -> i64 {
            let o = Outer { a: Inner { v: 1 }, b: 100 };
            o.a.v = 42;
            return o.a.v + o.b;
        }
    ").unwrap();
    assert_eq!(v, Value::int(142i64));
}

#[test]
fn field_assign_uses_existing_field_in_rhs() {
    let v = run("
        struct Point { x: i64, y: i64 }
        fn main() -> i64 {
            let p = Point { x: 5, y: 10 };
            p.x = p.x + 100;
            return p.x;
        }
    ").unwrap();
    assert_eq!(v, Value::int(105i64));
}

#[test]
fn field_assign_in_state_struct() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        fn main() -> i64 {
            origin.x = 5;
            origin.y = 7;
            return origin.x + origin.y;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(12i64));
}

#[test]
fn field_assign_persists_across_calls() {
    let mut kv = rend::kv::InMemoryKv::new();
    let src1 = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        fn main() -> i64 {
            origin.x = 99;
            return 0;
        }
    ";
    let out1 = Engine::new().execute(src1, Fuel::new(50_000), &kv).unwrap();
    kv.apply(&out1.writes);
    let src2 = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        fn main() -> i64 { return origin.x; }
    ";
    let out2 = Engine::new().execute(src2, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(out2.result, Value::int(99i64));
}

#[test]
fn assign_to_unknown_field_is_compile_error() {
    let err = run("
        struct Point { x: i64, y: i64 }
        fn main() -> i64 {
            let p = Point { x: 1, y: 2 };
            p.z = 3;
            return p.x;
        }
    ").unwrap_err();
    assert!(err.to_string().contains("field") || err.to_string().contains("z"));
}

#[test]
fn assign_field_type_mismatch_is_compile_error() {
    let err = run(r#"
        struct Point { x: i64, y: i64 }
        fn main() -> i64 {
            let p = Point { x: 1, y: 2 };
            p.x = "not an int";
            return p.x;
        }
    "#).unwrap_err();
    assert!(err.to_string().contains("expected") || err.to_string().contains("type"));
}

#[test]
fn helper_returning_modified_struct() {
    let v = run("
        struct Point { x: i64, y: i64 }
        entry fn translate_x(p: Point, dx: i64) -> Point {
            let q = p;
            q.x = q.x + dx;
            return q;
        }
        fn main() -> i64 {
            let a = Point { x: 1, y: 2 };
            let b = translate_x(a, 100);
            return b.x + b.y;
        }
    ").unwrap();
    assert_eq!(v, Value::int(103i64));
}

#[test]
fn field_path_inside_loop() {
    let v = run("
        struct Acc { sum: i64, count: i64 }
        fn main() -> i64 {
            let a = Acc { sum: 0, count: 0 };
            for x in [10, 20, 30] {
                a.sum = a.sum + x;
                a.count = a.count + 1;
            }
            return a.sum + a.count;
        }
    ").unwrap();
    assert_eq!(v, Value::int(63i64));
}

#[test]
fn example_24_advanced_returns_321() {
    let kv = rend::kv::InMemoryKv::new();
    let src = std::fs::read_to_string("examples/24_advanced.rd").unwrap();
    let out = Engine::new().execute(&src, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(321i64));
}
