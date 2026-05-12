//! Slice 6+9: persistent state, read/write sets, repeated commit.
//! Now keyed by u128 state-root, with values canonically serialized.

use rend::ast::Type;
use rend::hashing::state_root;
use rend::kv::InMemoryKv;
use rend::value::Value;
use rend::{Engine, Fuel};

fn engine() -> Engine {
    Engine::new()
}

fn root(name: &str) -> u128 {
    state_root("main", name)
}

#[test]
fn unset_state_reads_as_default_zero() {
    let kv = InMemoryKv::new();
    let src = "
        state count: i64;
        fn main() -> i64 { return count; }
    ";
    let out = engine().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::int(0i64));
    assert_eq!(out.reads.get(&root("count")), Some(&Value::int(0i64)));
    assert!(out.writes.is_empty());
}

#[test]
fn assignment_records_a_write() {
    let kv = InMemoryKv::new();
    let src = "
        state count: i64;
        fn main() -> i64 {
            count = 7;
            return count;
        }
    ";
    let out = engine().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::int(7i64));
    assert_eq!(out.writes.get(&root("count")), Some(&Value::int(7i64)));
}

#[test]
fn read_your_writes_within_a_tx() {
    let kv = InMemoryKv::new();
    let src = "
        state count: i64;
        fn main() -> i64 {
            count = 5;
            count = count + 10;
            return count;
        }
    ";
    let out = engine().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::int(15i64));
    assert_eq!(out.writes.get(&root("count")), Some(&Value::int(15i64)));
}

#[test]
fn host_can_commit_and_re_run() {
    let mut kv = InMemoryKv::new();
    let src = "
        state count: i64;
        entry fn incr() { count = count + 1; }
        fn main() -> i64 { incr(); return count; }
    ";

    let out = engine().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::int(1i64));
    assert_eq!(out.reads.get(&root("count")), Some(&Value::int(0i64)));
    assert_eq!(out.writes.get(&root("count")), Some(&Value::int(1i64)));
    kv.apply(&out.writes);

    let out = engine().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::int(2i64));
    assert_eq!(out.reads.get(&root("count")), Some(&Value::int(1i64)));
    kv.apply(&out.writes);

    let out = engine().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::int(3i64));
}

#[test]
fn read_set_only_records_first_observation() {
    let kv = InMemoryKv::new();
    let src = "
        state x: i64;
        fn main() -> i64 {
            let a = x;
            let b = x;
            let c = x;
            return a + b + c;
        }
    ";
    let out = engine().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::int(0i64));
    assert_eq!(out.reads.len(), 1);
    assert_eq!(out.reads.get(&root("x")), Some(&Value::int(0i64)));
}

#[test]
fn local_mutation_is_allowed_for_copy_types() {
    let kv = InMemoryKv::new();
    let src = "
        fn main() -> i64 {
            let x = 5;
            x = 6;
            return x;
        }
    ";
    let out = engine().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::int(6i64));
}

#[test]
fn assignment_type_mismatch_is_compile_error() {
    let kv = InMemoryKv::new();
    let src = "
        state count: i64;
        fn main() -> i64 {
            count = true;
            return count;
        }
    ";
    let err = engine().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(err.to_string().contains("expected i64"));
}

#[test]
fn bool_state_works() {
    let kv = InMemoryKv::new();
    let src = "
        state flag: bool;
        fn main() -> bool {
            flag = true;
            return flag;
        }
    ";
    let out = engine().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::Bool(true));
    assert_eq!(out.writes.get(&root("flag")), Some(&Value::Bool(true)));
}

#[test]
fn unwritten_keys_dont_appear_in_write_set() {
    let kv = InMemoryKv::new();
    let src = "
        state a: i64;
        state b: i64;
        fn main() -> i64 { return a; }
    ";
    let out = engine().execute(src, Fuel::new(1000), &kv).unwrap();
    assert!(out.reads.contains_key(&root("a")));
    assert!(!out.reads.contains_key(&root("b")));
    assert!(out.writes.is_empty());
}

#[test]
fn kv_stores_serialized_bytes_after_apply() {
    let mut kv = InMemoryKv::new();
    let src = "
        state count: i64;
        fn main() -> i64 { count = 42; return count; }
    ";
    let out = engine().execute(src, Fuel::new(1000), &kv).unwrap();
    kv.apply(&out.writes);
    let stored = kv.get_typed(root("count"), &Type::Int);
    assert_eq!(stored, Some(Value::int(42i64)));
}
