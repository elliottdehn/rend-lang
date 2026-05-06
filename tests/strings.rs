//! Slice 9: UTF-8 strings + Address.

use rend::ast::Type;
use rend::hashing::{child, state_root};
use rend::kv::InMemoryKv;
use rend::serialize::serialize;
use rend::value::Value;
use rend::{run, Engine, Fuel};

#[test]
fn string_literal_evaluates_to_string_value() {
    let r = run(r#"fn main() -> string { return "hello"; }"#).unwrap();
    assert_eq!(r, Value::Str("hello".into()));
}

#[test]
fn empty_string_is_default() {
    let kv = InMemoryKv::new();
    let src = "
        state name: string;
        fn main() -> string { return name; }
    ";
    let out = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::Str(String::new()));
}

#[test]
fn string_state_persists_across_runs() {
    let mut kv = InMemoryKv::new();
    let src = r#"
        state greeting: string;
        fn main() -> string {
            greeting = "ahoy";
            return greeting;
        }
    "#;
    let engine = Engine::new();
    let out = engine.execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::Str("ahoy".into()));
    kv.apply(&out.writes);

    let src2 = "
        state greeting: string;
        fn main() -> string { return greeting; }
    ";
    let out = engine.execute(src2, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::Str("ahoy".into()));
}

#[test]
fn utf8_strings_round_trip_through_storage() {
    let mut kv = InMemoryKv::new();
    let src = r#"
        state name: string;
        fn main() -> string {
            name = "héllo 🌍";
            return name;
        }
    "#;
    let engine = Engine::new();
    let out = engine.execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::Str("héllo 🌍".into()));
    kv.apply(&out.writes);
    assert_eq!(
        kv.get_typed(state_root("main", "name"), &Type::String),
        Some(Value::Str("héllo 🌍".into())),
    );
}

#[test]
fn string_equality() {
    let r = run(r#"fn main() -> bool { return "abc" == "abc"; }"#).unwrap();
    assert_eq!(r, Value::Bool(true));
    let r = run(r#"fn main() -> bool { return "abc" == "abd"; }"#).unwrap();
    assert_eq!(r, Value::Bool(false));
}

#[test]
fn address_constructed_from_string_keys_a_map() {
    let kv = InMemoryKv::new();
    let src = r#"
        state holders: map<Address, i64>;
        fn main() -> i64 {
            let a = address("0xfeed");
            holders[a] = 42;
            return holders[address("0xfeed")];
        }
    "#;
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(42));

    let key = child(
        state_root("main", "holders"),
        &serialize(&Value::Address("0xfeed".into())),
    );
    assert_eq!(out.writes.get(&key), Some(&Value::Int(42)));
}

#[test]
fn address_disjoint_from_string_in_type_system() {
    // can't pass a String where Address is expected
    let src = r#"
        state holders: map<Address, i64>;
        fn main() -> i64 {
            holders["foo"] = 1;
            return 0;
        }
    "#;
    let kv = InMemoryKv::new();
    let err = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(err.to_string().contains("Address"), "got {err}");
}

#[test]
fn unterminated_string_is_lex_error() {
    let kv = InMemoryKv::new();
    let src = r#"fn main() -> string { return "oops; }"#;
    let err = Engine::new().execute(src, Fuel::new(100), &kv).unwrap_err();
    assert!(err.to_string().contains("unterminated"), "got {err}");
}

#[test]
fn escape_sequences_work() {
    let r = run(r#"fn main() -> string { return "line\nbreak"; }"#).unwrap();
    assert_eq!(r, Value::Str("line\nbreak".into()));
}
