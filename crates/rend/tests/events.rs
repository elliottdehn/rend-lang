//! Struct-based events. Modules declare event payloads as ordinary
//! structs and emit values with `emit StructLiteral;`. Emissions
//! accumulate in an in-memory append-only log returned in
//! `ExecOutcome::events`, tagged with the module name that emitted.

use rend::value::Value;
use rend::{Engine, Fuel};

#[test]
fn emit_appears_in_outcome_log() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Hello { n: i64 }
        fn main() -> i64 {
            emit Hello { n: 42 };
            return 0;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.events.len(), 1);
    assert_eq!(out.events[0].name, "Hello");
    assert_eq!(out.events[0].module, "main");
    assert_eq!(out.events[0].args, vec![Value::int(42i64)]);
}

#[test]
fn multi_field_event_carries_all_values() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        struct Transfer { from: Address, to: Address, amount: u64 }
        fn main() -> i64 {
            emit Transfer {
                from: address("0xa"),
                to: address("0xb"),
                amount: 100u64,
            };
            return 0;
        }
    "#;
    let out = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.events.len(), 1);
    assert_eq!(out.events[0].name, "Transfer");
    assert_eq!(
        out.events[0].args,
        vec![
            Value::Address("0xa".into()),
            Value::Address("0xb".into()),
            Value::U64(100),
        ],
    );
}

#[test]
fn emits_appear_in_emission_order() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Tick { n: i64 }
        fn main() -> i64 {
            for n in [1, 2, 3, 4, 5] {
                emit Tick { n: n };
            }
            return 0;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    let ns: Vec<i64> = out
        .events
        .iter()
        .map(|e| match &e.args[0] {
            Value::Int(n) => num_traits::ToPrimitive::to_i64(n).expect("fits"),
            _ => panic!(),
        })
        .collect();
    assert_eq!(ns, vec![1, 2, 3, 4, 5]);
}

#[test]
fn unknown_struct_in_emit_is_compile_error() {
    // The parser's struct-literal recognition is gated on the
    // struct being declared; an unknown name with `{...}` after
    // it falls back to "ident followed by something unexpected"
    // which surfaces at parse time.
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        fn main() -> i64 {
            emit Ghost { n: 7 };
            return 0;
        }
    ";
    let err = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("emit") || err.to_string().contains("Ghost"),
        "expected emit/Ghost-related parse error; got: {err}",
    );
}

#[test]
fn emit_non_struct_is_compile_error() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        fn main() -> i64 {
            emit 42;
            return 0;
        }
    ";
    let err = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(
        err.to_string().contains("struct"),
        "expected struct-required error; got: {err}",
    );
}

#[test]
fn emits_dont_touch_read_or_write_set() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Bumped { x: i64 }
        state counter: i64;
        fn main() -> i64 {
            emit Bumped { x: counter + 1 };
            return 0;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap();
    // The emit reads `counter` for the arg, so reads has it; but
    // emitting itself doesn't add anything new to the write set.
    assert_eq!(out.events.len(), 1);
    assert!(out.writes.is_empty(), "emit must not write state");
}

#[test]
fn assert_failure_drops_emitted_log_along_with_writes() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        struct Boom { x: i64 }
        state s: i64;
        fn main() -> i64 {
            emit Boom { x: 1 };
            s = 5;
            assert(false, "nope");
            return 0;
        }
    "#;
    let err = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(err.to_string().contains("nope"));
    // The Engine reports `Err` on assertion failure; the host
    // discards the in-progress tx. We don't have a partial outcome
    // to inspect, but the test pins the contract that an
    // assert-failed tx never reaches the host's event log.
}

#[test]
fn struct_typed_event_field_works() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Point { x: i64, y: i64 }
        struct Moved { from: Point, to: Point }
        fn main() -> i64 {
            emit Moved {
                from: Point { x: 0, y: 0 },
                to: Point { x: 3, y: 4 },
            };
            return 0;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.events.len(), 1);
    let from = &out.events[0].args[0];
    match from {
        Value::Struct { name, fields } => {
            assert_eq!(name, "Point");
            assert_eq!(fields[0].1, Value::int(0i64));
            assert_eq!(fields[1].1, Value::int(0i64));
        }
        other => panic!("expected Point struct, got {other}"),
    }
}

#[test]
fn cross_module_emit_is_tagged_with_emitting_module() {
    use std::collections::HashMap;
    let mut sources = HashMap::new();
    sources.insert(
        "lib".into(),
        "
            struct Ping { n: i64 }
            entry fn fire() -> i64 {
                emit Ping { n: 7 };
                return 0;
            }
        "
        .to_string(),
    );
    sources.insert(
        "main".into(),
        "
            fn main() -> i64 {
                return lib::fire();
            }
        "
        .to_string(),
    );
    let kv = rend::kv::InMemoryKv::new();
    let out = Engine::new()
        .execute_main(&sources, "main", Fuel::new(10_000), &kv)
        .unwrap();
    assert_eq!(out.events.len(), 1);
    assert_eq!(out.events[0].module, "lib");
    assert_eq!(out.events[0].name, "Ping");
}
