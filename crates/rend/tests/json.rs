//! Native JSON handling — `json` is a dynamic value type (jsonb-shaped
//! internally: parsed structure, not raw text). Path access via
//! `expr -> ident`, `expr -> "key"`, `expr -> [n]`. Conversion via
//! `json_to_*` builtins. Buyer-beware: missing keys yield `null`,
//! shape mismatches at conversion abort with runtime errors.

use rend::value::Value;
use rend::{run, Engine, Fuel};

// ---------- parsing ----------

#[test]
fn parse_simple_object() {
    let v = run(r#"
        fn main() -> string {
            let j = parse_json("{\"name\": \"alice\", \"age\": 30}");
            return json_to_string(j -> name);
        }
    "#).unwrap();
    assert_eq!(v, Value::Str("alice".into()));
}

#[test]
fn parse_array() {
    let v = run(r#"
        fn main() -> i64 {
            let j = parse_json("[10, 20, 30]");
            return json_to_i64(j -> [1]);
        }
    "#).unwrap();
    assert_eq!(v, Value::int(20i64));
}

#[test]
fn parse_invalid_json_is_runtime_error() {
    let err = run(r#"
        fn main() -> i64 {
            let _ = parse_json("not json");
            return 0;
        }
    "#).unwrap_err();
    assert!(err.to_string().contains("JSON parse error"), "got: {err}");
}

// ---------- path access (-&gt;) ----------

#[test]
fn path_with_bare_ident() {
    // `j -> name` is sugar for `j -> "name"`.
    let v = run(r#"
        fn main() -> string {
            let j = parse_json("{\"name\": \"bob\"}");
            return json_to_string(j -> name);
        }
    "#).unwrap();
    assert_eq!(v, Value::Str("bob".into()));
}

#[test]
fn path_with_quoted_key() {
    // Keys with hyphens or other non-ident chars need quotes.
    let v = run(r#"
        fn main() -> string {
            let j = parse_json("{\"first-name\": \"carol\"}");
            return json_to_string(j -> "first-name");
        }
    "#).unwrap();
    assert_eq!(v, Value::Str("carol".into()));
}

#[test]
fn nested_path() {
    let v = run(r#"
        fn main() -> string {
            let j = parse_json("{\"user\": {\"profile\": {\"email\": \"a@x.com\"}}}");
            return json_to_string(j -> user -> profile -> email);
        }
    "#).unwrap();
    assert_eq!(v, Value::Str("a@x.com".into()));
}

#[test]
fn path_array_then_field() {
    let v = run(r#"
        fn main() -> string {
            let j = parse_json("{\"users\": [{\"name\": \"a\"}, {\"name\": \"b\"}]}");
            return json_to_string(j -> users -> [1] -> name);
        }
    "#).unwrap();
    assert_eq!(v, Value::Str("b".into()));
}

#[test]
fn missing_key_yields_null() {
    let v = run(r#"
        fn main() -> bool {
            let j = parse_json("{\"a\": 1}");
            return json_is_null(j -> nope);
        }
    "#).unwrap();
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn out_of_range_index_yields_null() {
    let v = run(r#"
        fn main() -> bool {
            let j = parse_json("[1, 2, 3]");
            return json_is_null(j -> [99]);
        }
    "#).unwrap();
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn path_through_null_yields_null() {
    // Buyer beware: navigating into null doesn't error, just stays null.
    let v = run(r#"
        fn main() -> bool {
            let j = parse_json("{\"a\": null}");
            return json_is_null(j -> a -> nested -> deeper);
        }
    "#).unwrap();
    assert_eq!(v, Value::Bool(true));
}

// ---------- conversion builtins ----------

#[test]
fn json_to_i64_extracts_int() {
    let v = run(r#"
        fn main() -> i64 {
            let j = parse_json("42");
            return json_to_i64(j);
        }
    "#).unwrap();
    assert_eq!(v, Value::int(42i64));
}

#[test]
fn json_to_u64_extracts_u64() {
    let v = run(r#"
        fn main() -> u64 {
            let j = parse_json("18446744073709551615");
            return json_to_u64(j);
        }
    "#).unwrap();
    assert_eq!(v, Value::U64(u64::MAX));
}

#[test]
fn json_to_bool_extracts_bool() {
    let v = run(r#"
        fn main() -> bool {
            let j = parse_json("true");
            return json_to_bool(j);
        }
    "#).unwrap();
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn json_to_string_on_non_string_aborts() {
    let err = run(r#"
        fn main() -> string {
            let j = parse_json("42");
            return json_to_string(j);   // panic — not a string
        }
    "#).unwrap_err();
    assert!(err.to_string().contains("not a string"), "got: {err}");
}

#[test]
fn json_to_i64_on_string_aborts() {
    let err = run(r#"
        fn main() -> i64 {
            let j = parse_json("\"hello\"");
            return json_to_i64(j);
        }
    "#).unwrap_err();
    assert!(err.to_string().contains("not an integer"), "got: {err}");
}

// ---------- stringify round-trip ----------

#[test]
fn stringify_canonical() {
    let v = run(r#"
        fn main() -> string {
            let j = parse_json("{ \"b\":  2 ,\"a\": 1 }");
            return json_stringify(j);
        }
    "#).unwrap();
    // Canonical form preserves insertion order, drops whitespace.
    assert_eq!(v, Value::Str(r#"{"b":2,"a":1}"#.into()));
}

// ---------- state storage ----------

#[test]
fn json_in_pmap_state_round_trips() {
    let v = run(r#"
        state events: pmap<u64, json>;
        fn main() -> string {
            events[1u64] = parse_json("{\"kind\": \"login\", \"user\": \"alice\"}");
            let j = events[1u64];
            return json_to_string(j -> user);
        }
    "#).unwrap();
    assert_eq!(v, Value::Str("alice".into()));
}

#[test]
fn json_in_struct_field() {
    let v = run(r#"
        struct Event { id: u64, payload: json }
        state log: pmap<u64, Event>;
        fn main() -> string {
            log[1u64] = Event {
                id: 1u64,
                payload: parse_json("{\"action\": \"click\"}"),
            };
            return json_to_string(log[1u64].payload -> action);
        }
    "#).unwrap();
    assert_eq!(v, Value::Str("click".into()));
}

// ---------- bytecode VM parity ----------

#[test]
fn json_works_on_bytecode_vm() {
    let src = r#"
        state events: pmap<u64, json>;
        fn main() -> string {
            events[1u64] = parse_json("{\"user\": {\"id\": 42, \"role\": \"admin\"}}");
            let role = json_to_string(events[1u64] -> user -> role);
            return role;
        }
    "#;
    let kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .execute(src, Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::Str("admin".into()));
}

// ---------- aggregating typed values out of JSON arrays ----------

#[test]
fn extract_then_aggregate() {
    // Real-world shape: opaque JSON event log, run a query that
    // navigates into each event and sums a field.
    let v = run(r#"
        state events: pvec<json>;
        fn main() -> i64 {
            pvec_push(events, parse_json("{\"amount\": 10}"));
            pvec_push(events, parse_json("{\"amount\": 25}"));
            pvec_push(events, parse_json("{\"amount\": 7}"));
            let total = 0;
            for e in events {
                total = total + json_to_i64(e -> amount);
            }
            return total;       // 42
        }
    "#).unwrap();
    assert_eq!(v, Value::int(42i64));
}

// ---------- typeck rejection ----------

#[test]
fn path_on_non_json_is_compile_error() {
    let err = rend::frontend(r#"
        fn main() -> string {
            let s = "hello";
            return s -> field;
        }
    "#).unwrap_err();
    assert!(
        err.to_string().contains("must be json"),
        "got: {err}",
    );
}

#[test]
fn parse_json_with_non_string_arg_is_compile_error() {
    let err = rend::frontend(r#"
        fn main() -> string {
            let j = parse_json(42);
            return json_stringify(j);
        }
    "#).unwrap_err();
    assert!(
        err.to_string().contains("string"),
        "got: {err}",
    );
}


// ---------- source-level JSON literals ----------

#[test]
fn json_object_literal_at_top_level() {
    // `{...}` at expression position with string-key+colon pattern
    // is a JSON object literal, distinct from a block expression.
    let v = rend::run(r#"
        fn main() -> string {
            let j = {"a": 2, "b": true};
            return json_stringify(j);
        }
    "#).unwrap();
    assert_eq!(v, rend::Value::Str(r#"{"a":2,"b":true}"#.into()));
}

#[test]
fn empty_json_object_literal() {
    let v = rend::run(r#"
        fn main() -> string {
            return json_stringify({});
        }
    "#).unwrap();
    assert_eq!(v, rend::Value::Str("{}".into()));
}

#[test]
fn json_object_with_nested_array_and_object() {
    let v = rend::run(r#"
        fn main() -> string {
            let j = {"list": [1, "x", null, false], "nested": {"k": 7}};
            return json_stringify(j);
        }
    "#).unwrap();
    assert_eq!(v, rend::Value::Str(
        r#"{"list":[1,"x",null,false],"nested":{"k":7}}"#.into(),
    ));
}

#[test]
fn json_object_value_can_reference_local() {
    // Variable references inside JSON literal values evaluate
    // through the normal expression path.
    let v = rend::run(r#"
        fn main() -> string {
            let x = 42;
            return json_stringify({"answer": x + 1});
        }
    "#).unwrap();
    assert_eq!(v, rend::Value::Str(r#"{"answer":43}"#.into()));
}

#[test]
fn json_null_literal_distinct_from_block() {
    // `null` inside a json-value context is Json::Null. As a
    // top-level expression it stays a regular ident lookup, which
    // is what undefined-variable errors are for — we don't make
    // `null` a global keyword, just a contextual one.
    let err = rend::run("fn main() -> i64 { return null; }").unwrap_err();
    assert!(err.to_string().contains("undefined"), "got: {err}");
}

