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
        field_groups: Vec::new(),
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

// ---------- struct field grouping (storage granularity) ----------

#[test]
fn grouped_fields_share_one_cell() {
    // Slice 1: `group <name> { ... }` lets the user opt into shared
    // storage. Inside the group, the two fields are stored as a
    // single blob at child(state_root, "profile"); outside, the
    // granular `id` keeps its own cell.
    let mut kv = InMemoryKv::new();
    let engine = Engine::new();
    let src = r#"
        struct User {
            id: i64,
            group profile {
                name: string,
                email: string,
            },
        }
        state u: User;
        fn main() -> i64 {
            u = User { id: 1, name: "alice", email: "a@x.com" };
            return u.id;
        }
    "#;
    let out = engine.execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(1i64));
    kv.apply(&out.writes);

    let root = state_root("main", "u");
    let id_key = rend::hashing::child(root, b"id");
    let profile_key = rend::hashing::child(root, b"profile");
    assert_eq!(kv.get_typed(id_key, &Type::Int), Some(Value::int(1i64)));
    let group_ty = Type::Struct {
        name: "User::group::profile".into(),
        fields: vec![
            ("name".into(), Type::String),
            ("email".into(), Type::String),
        ],
        field_groups: Vec::new(),
    };
    assert_eq!(
        kv.get_typed(profile_key, &group_ty),
        Some(Value::Struct {
            name: "User::group::profile".into(),
            fields: vec![
                ("name".into(), Value::Str("alice".into())),
                ("email".into(), Value::Str("a@x.com".into())),
            ],
        }),
    );
    // No per-field cells were emitted for the grouped fields.
    let name_key = rend::hashing::child(root, b"name");
    let email_key = rend::hashing::child(root, b"email");
    assert_eq!(kv.get_typed(name_key, &Type::String), None);
    assert_eq!(kv.get_typed(email_key, &Type::String), None);
}

#[test]
fn reading_a_grouped_field_projects_from_the_group_blob() {
    let src = r#"
        struct User {
            id: i64,
            group profile { name: string, age: i64 },
        }
        state u: User;
        fn main() -> i64 {
            u = User { id: 1, name: "alice", age: 30 };
            return u.age;
        }
    "#;
    let out = Engine::new()
        .execute(src, Fuel::new(10_000), &InMemoryKv::new())
        .unwrap();
    assert_eq!(out.result, Value::int(30i64));
}

#[test]
fn writing_a_grouped_field_is_read_modify_write_of_the_group_cell() {
    let mut kv = InMemoryKv::new();
    let engine = Engine::new();
    let seed = r#"
        struct User {
            id: i64,
            group profile { name: string, age: i64 },
        }
        state u: User;
        fn main() -> i64 {
            u = User { id: 1, name: "alice", age: 30 };
            return 0;
        }
    "#;
    let out = engine.execute(seed, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&out.writes);

    let bump = r#"
        struct User {
            id: i64,
            group profile { name: string, age: i64 },
        }
        state u: User;
        fn main() -> i64 {
            u.age = u.age + 1;
            return u.age;
        }
    "#;
    let out2 = engine.execute(bump, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out2.result, Value::int(31i64));
    kv.apply(&out2.writes);

    let root = state_root("main", "u");
    let id_key = rend::hashing::child(root, b"id");
    let profile_key = rend::hashing::child(root, b"profile");
    assert_eq!(kv.get_typed(id_key, &Type::Int), Some(Value::int(1i64)));
    let group_ty = Type::Struct {
        name: "User::group::profile".into(),
        fields: vec![
            ("name".into(), Type::String),
            ("age".into(), Type::Int),
        ],
        field_groups: Vec::new(),
    };
    assert_eq!(
        kv.get_typed(profile_key, &group_ty),
        Some(Value::Struct {
            name: "User::group::profile".into(),
            fields: vec![
                ("name".into(), Value::Str("alice".into())),
                ("age".into(), Value::int(31i64)),
            ],
        }),
    );
}

#[test]
fn nested_struct_inside_group_supports_chained_field_access() {
    // Outer struct's `profile` group holds an Inner struct. Reading
    // and writing `u.who.name` exercises the full projection chain
    // through the group blob, not just the first hop.
    let mut kv = InMemoryKv::new();
    let engine = Engine::new();
    let seed = r#"
        struct Who { name: string, age: i64 }
        struct U {
            id: i64,
            group profile { who: Who, flag: bool },
        }
        state u: U;
        fn main() -> i64 {
            u = U { id: 7, who: Who { name: "alice", age: 30 }, flag: true };
            return 0;
        }
    "#;
    let out = engine.execute(seed, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&out.writes);

    let bump = r#"
        struct Who { name: string, age: i64 }
        struct U {
            id: i64,
            group profile { who: Who, flag: bool },
        }
        state u: U;
        fn main() -> i64 {
            u.who.age = u.who.age + 1;
            return u.who.age;
        }
    "#;
    let out2 = engine.execute(bump, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out2.result, Value::int(31i64));
    kv.apply(&out2.writes);

    // Group cell still holds the rebuilt Who blob with the bumped age.
    let root = state_root("main", "u");
    let profile_key = rend::hashing::child(root, b"profile");
    let who_ty = Type::Struct {
        name: "Who".into(),
        fields: vec![
            ("name".into(), Type::String),
            ("age".into(), Type::Int),
        ],
        field_groups: Vec::new(),
    };
    let group_ty = Type::Struct {
        name: "U::group::profile".into(),
        fields: vec![
            ("who".into(), who_ty),
            ("flag".into(), Type::Bool),
        ],
        field_groups: Vec::new(),
    };
    assert_eq!(
        kv.get_typed(profile_key, &group_ty),
        Some(Value::Struct {
            name: "U::group::profile".into(),
            fields: vec![
                (
                    "who".into(),
                    Value::Struct {
                        name: "Who".into(),
                        fields: vec![
                            ("name".into(), Value::Str("alice".into())),
                            ("age".into(), Value::int(31i64)),
                        ],
                    },
                ),
                ("flag".into(), Value::Bool(true)),
            ],
        }),
    );
}

#[test]
fn inner_groups_inside_an_outer_group_are_storage_no_ops() {
    // Slice 1 semantics: a group is the unit of storage. If an inner
    // struct that *itself* declares groups gets nested inside another
    // group, the inner annotations are inert — the parent cell already
    // swallowed the subtree, so the inner group doesn't get its own
    // cell.
    let mut kv = InMemoryKv::new();
    let engine = Engine::new();
    let src = r#"
        struct Inner {
            x: i64,
            group inner_g { a: i64, b: i64 },
        }
        struct Outer {
            id: i64,
            group outer_g { inner: Inner, flag: bool },
        }
        state o: Outer;
        fn main() -> i64 {
            o = Outer {
                id: 1,
                inner: Inner { x: 10, a: 20, b: 30 },
                flag: true,
            };
            return o.inner.a + o.inner.b + o.inner.x;
        }
    "#;
    let out = engine.execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(60i64));
    kv.apply(&out.writes);

    // No standalone cell for Inner's `inner_g` — the whole Inner is
    // inlined in the outer_g blob.
    let root = state_root("main", "o");
    let inner_g_key = rend::hashing::child(
        rend::hashing::child(root, b"outer_g"),
        b"inner_g",
    );
    assert_eq!(
        kv.get_typed(inner_g_key, &Type::Int),
        None,
        "inner group inside an outer group should be inert (no own cell)",
    );
}

#[test]
fn whole_struct_read_reassembles_through_group_cells() {
    let mut kv = InMemoryKv::new();
    let engine = Engine::new();
    let seed = r#"
        struct User {
            id: i64,
            group profile { name: string, age: i64 },
        }
        state u: User;
        entry fn store(i: i64, n: string, a: i64) -> i64 {
            u = User { id: i, name: n, age: a };
            return 0;
        }
        fn main() -> i64 {
            store(7, "bob", 42);
            return 0;
        }
    "#;
    let out = engine.execute(seed, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&out.writes);

    let read = r#"
        struct User {
            id: i64,
            group profile { name: string, age: i64 },
        }
        state u: User;
        fn main() -> i64 {
            let copy = u;
            return copy.id + copy.age;
        }
    "#;
    let out2 = engine.execute(read, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out2.result, Value::int(7 + 42));
}
