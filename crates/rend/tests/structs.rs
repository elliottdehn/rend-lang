//! Slice 10b: structs — decl, literal, field access, storage, keys.

use rend::ast::Type;
use rend::hashing::{child, state_root};
use rend::kv::InMemoryKv;
use rend::serialize::serialize;
use rend::value::Value;
use rend::{run, Engine, Fuel};

fn point_type() -> Type {
    Type::Struct {
        name: "Point".into(),
        fields: vec![
            ("x".into(), Type::Int),
            ("y".into(), Type::Int),
        ],
    }
}

#[test]
fn struct_literal_and_field_access() {
    let src = "
        struct Point { x: i64, y: i64 }
        fn main() -> i64 {
            let p = Point { x: 3, y: 4 };
            return p.x + p.y;
        }
    ";
    let v = run(src).unwrap();
    assert_eq!(v, Value::int(7i64));
}

#[test]
fn fields_in_literal_can_be_out_of_order() {
    let src = "
        struct Point { x: i64, y: i64 }
        fn main() -> i64 {
            let p = Point { y: 100, x: 1 };
            return p.x;
        }
    ";
    assert_eq!(run(src).unwrap(), Value::int(1i64));
}

#[test]
fn missing_field_is_compile_error() {
    let src = "
        struct Point { x: i64, y: i64 }
        fn main() -> i64 {
            let p = Point { x: 3 };
            return p.x;
        }
    ";
    let err = run(src).unwrap_err();
    assert!(err.to_string().contains("field"));
}

#[test]
fn unknown_field_access_is_compile_error() {
    let src = "
        struct Point { x: i64, y: i64 }
        fn main() -> i64 {
            let p = Point { x: 3, y: 4 };
            return p.z;
        }
    ";
    let err = run(src).unwrap_err();
    assert!(err.to_string().contains("no field"));
}

#[test]
fn struct_passed_to_function() {
    let src = "
        struct Point { x: i64, y: i64 }
        entry fn dist_sq(p: Point) -> i64 { return p.x * p.x + p.y * p.y; }
        fn main() -> i64 {
            let p = Point { x: 3, y: 4 };
            return dist_sq(p);
        }
    ";
    assert_eq!(run(src).unwrap(), Value::int(25i64));
}

#[test]
fn struct_can_be_stored_in_state() {
    let mut kv = InMemoryKv::new();
    let engine = Engine::new();
    let src = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        fn main() -> i64 {
            origin = Point { x: 10, y: 20 };
            return origin.x + origin.y;
        }
    ";
    let out = engine.execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(30i64));
    kv.apply(&out.writes);

    // Granular state layout: each leaf field lives in its own cell at
    // child(state_root, field_name). The state root itself is just a
    // namespace and holds nothing.
    let root = state_root("main", "origin");
    let x_key = rend::hashing::child(root, b"x");
    let y_key = rend::hashing::child(root, b"y");
    assert_eq!(kv.get_typed(x_key, &rend::ast::Type::Int), Some(Value::int(10i64)));
    assert_eq!(kv.get_typed(y_key, &rend::ast::Type::Int), Some(Value::int(20i64)));
    let _ = point_type;
}

#[test]
fn struct_can_be_a_map_key() {
    let kv = InMemoryKv::new();
    let src = "
        struct Coord { x: i64, y: i64 }
        state grid: map<Coord, i64>;
        fn main() -> i64 {
            grid[Coord { x: 0, y: 0 }] = 5;
            grid[Coord { x: 1, y: 2 }] = 7;
            return grid[Coord { x: 0, y: 0 }] + grid[Coord { x: 1, y: 2 }];
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(12i64));

    // Compute the cell key host-side and verify
    let key00 = child(
        state_root("main", "grid"),
        &serialize(&Value::Struct {
            name: "Coord".into(),
            fields: vec![
                ("x".into(), Value::int(0i64)),
                ("y".into(), Value::int(0i64)),
            ],
        }),
    );
    assert_eq!(out.writes.get(&key00), Some(&Value::int(5i64)));
}

#[test]
fn struct_default_value_when_state_unset() {
    let kv = InMemoryKv::new();
    let src = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        fn main() -> i64 {
            return origin.x;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::int(0i64));
}

#[test]
fn struct_with_string_field() {
    let src = r#"
        struct Person { name: string, age: i64 }
        fn main() -> i64 {
            let p = Person { name: "alice", age: 30 };
            return p.age;
        }
    "#;
    assert_eq!(run(src).unwrap(), Value::int(30i64));
}

#[test]
fn duplicate_struct_decl_rejected() {
    let src = "
        struct P { x: i64 }
        struct P { y: i64 }
        fn main() -> i64 { return 0; }
    ";
    let err = run(src).unwrap_err();
    assert!(err.to_string().contains("duplicate"));
}

#[test]
fn unknown_struct_in_decl_rejected() {
    let src = "
        state foo: Bar;
        fn main() -> i64 { return 0; }
    ";
    let err = run(src).unwrap_err();
    assert!(err.to_string().contains("Bar") || err.to_string().contains("unknown"));
}
