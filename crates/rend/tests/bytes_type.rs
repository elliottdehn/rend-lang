//! `bytes` — variable-length byte sequence with concat/slice/eq/len
//! and a string-→-bytes conversion. Storable, keyable, copyable.

use rend::value::Value;
use rend::{run, Engine, Fuel};

#[test]
fn to_bytes_round_trips_utf8() {
    let v = run(r#"
        fn main() -> i64 {
            let b = to_bytes("hello");
            return bytes_len(b);
        }
    "#).unwrap();
    assert_eq!(v, Value::Int(5));
}

#[test]
fn bytes_concat_combines() {
    let v = run(r#"
        fn main() -> i64 {
            let a = to_bytes("foo");
            let b = to_bytes("bar");
            let c = bytes_concat(a, b);
            return bytes_len(c);
        }
    "#).unwrap();
    assert_eq!(v, Value::Int(6));
}

#[test]
fn bytes_eq_compares_content_not_identity() {
    let v = run(r#"
        fn main() -> bool {
            let a = to_bytes("xyz");
            let b = to_bytes("xyz");
            return bytes_eq(a, b);
        }
    "#).unwrap();
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn bytes_eq_returns_false_for_differing_content() {
    let v = run(r#"
        fn main() -> bool {
            return bytes_eq(to_bytes("xy"), to_bytes("xz"));
        }
    "#).unwrap();
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn bytes_slice_produces_subrange() {
    let v = run(r#"
        fn main() -> bool {
            let b = to_bytes("rendlang");
            let s = bytes_slice(b, 4, 8);
            return bytes_eq(s, to_bytes("lang"));
        }
    "#).unwrap();
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn bytes_slice_out_of_range_is_runtime_error() {
    let err = run(r#"
        fn main() -> i64 {
            let b = to_bytes("abc");
            let _s = bytes_slice(b, 0, 10);
            return 0;
        }
    "#).unwrap_err();
    assert!(err.to_string().contains("out-of-range"));
}

#[test]
fn bytes_round_trips_through_state() {
    let mut kv = rend::kv::InMemoryKv::new();
    let setup = r#"
        state blob: bytes;
        fn main() -> i64 {
            blob = to_bytes("persisted");
            return 0;
        }
    "#;
    let setup_out = Engine::new().execute(setup, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&setup_out.writes);
    let read = r#"
        state blob: bytes;
        fn main() -> i64 { return bytes_len(blob); }
    "#;
    let out = Engine::new().execute(read, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(9));
}

#[test]
fn bytes_can_key_a_state_map() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        state by_hash: map<bytes, i64>;
        fn main() -> i64 {
            let k = to_bytes("alice");
            by_hash[k] = 42;
            return by_hash[k];
        }
    "#;
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(42));
}

#[test]
fn empty_bytes_default_is_canonical() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        state b: bytes;
        fn main() -> i64 { return bytes_len(b); }
    "#;
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(0));
}

#[test]
fn bytes_in_struct_field() {
    let v = run(r#"
        struct Sig { r: bytes, s: bytes }
        fn main() -> i64 {
            let sig = Sig { r: to_bytes("rrr"), s: to_bytes("sssss") };
            return bytes_len(sig.r) + bytes_len(sig.s);
        }
    "#).unwrap();
    assert_eq!(v, Value::Int(8));
}

#[test]
fn type_mismatch_rejects_string_where_bytes_expected() {
    let err = run(r#"
        fn main() -> i64 {
            return bytes_len("not bytes");
        }
    "#).unwrap_err();
    assert!(err.to_string().contains("bytes"));
}
