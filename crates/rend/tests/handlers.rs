//! Event handlers — `on Foo fn h(e: Foo) { ... }`.
//!
//! Emitting a struct value runs every matching handler inline in
//! declaration order. Handler bodies are ordinary fns: they read +
//! write state, emit follow-up events, and recursively trigger more
//! handlers. Cycles are bounded by fuel.

use rend::value::Value;
use rend::{Engine, Fuel};

#[test]
fn handler_runs_when_event_is_emitted() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Tick { n: i64 }
        state count: i64;
        on Tick fn bump(t: Tick) {
            count = count + t.n;
        }
        fn main() -> i64 {
            emit Tick { n: 1 };
            emit Tick { n: 2 };
            emit Tick { n: 7 };
            return count;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    // Each emit ran `bump`; total = 1 + 2 + 7 = 10.
    assert_eq!(out.result, Value::int(10i64));
}

#[test]
fn multiple_handlers_on_same_event_run_in_declaration_order() {
    let kv = rend::kv::InMemoryKv::new();
    // Encode "saw handler N at step S" as a base-10 digit. First
    // handler bumps the ones place, second bumps the tens place,
    // so after two emits the digit string reads first-second-first-second.
    let src = "
        struct Ping { n: i64 }
        state log: i64;
        on Ping fn first(p: Ping) {
            let _ = p.n;
            log = log * 10 + 1;
        }
        on Ping fn second(p: Ping) {
            let _ = p.n;
            log = log * 10 + 2;
        }
        fn main() -> i64 {
            emit Ping { n: 1 };
            emit Ping { n: 2 };
            return log;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    // First emit appends [1, 2]; second emit appends [1, 2] again.
    assert_eq!(out.result, Value::int(1212i64));
}

#[test]
fn handler_emitting_a_different_event_triggers_a_recursive_handler() {
    let kv = rend::kv::InMemoryKv::new();
    // handle_a runs and emits B; handle_b runs in turn, both writing
    // to `log` as base-10 digits to record the order.
    let src = r#"
        struct A { x: i64 }
        struct B { y: i64 }
        state log: i64;
        on A fn handle_a(e: A) {
            log = log * 10 + 1;
            emit B { y: e.x + 1 };
        }
        on B fn handle_b(e: B) {
            let _ = e.y;
            log = log * 10 + 2;
        }
        fn main() -> i64 {
            emit A { x: 5 };
            return log;
        }
    "#;
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    // A handler ran first (digit 1), then it emitted B which ran
    // its handler (digit 2). Final: 12.
    assert_eq!(out.result, Value::int(12i64));
}

#[test]
fn handler_signature_must_match_event_type() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Tick { n: i64 }
        struct Tock { m: i64 }
        on Tick fn handler(t: Tock) {
            let _ = t.m;
        }
        fn main() -> i64 { return 0; }
    ";
    let err = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("match the event type")
            || err.to_string().contains("Tock"),
        "expected mismatch error, got: {err}",
    );
}

#[test]
fn handler_must_take_exactly_one_param() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Tick { n: i64 }
        on Tick fn bad(a: Tick, b: i64) {
            let _ = b;
            let _ = a.n;
        }
        fn main() -> i64 { return 0; }
    ";
    let err = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(
        err.to_string().contains("one parameter"),
        "expected single-param error, got: {err}",
    );
}

#[test]
fn handler_can_read_and_write_state() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Credit { amount: u64 }
        state balance: u64;
        on Credit fn apply(c: Credit) {
            balance = balance + c.amount;
        }
        fn main() -> u64 {
            emit Credit { amount: 100u64 };
            emit Credit { amount: 50u64 };
            return balance;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(150));
}

#[test]
fn handlers_log_to_the_outcome_events_alongside_their_side_effects() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Foo { n: i64 }
        on Foo fn h(f: Foo) {
            let _ = f.n;
        }
        fn main() -> i64 {
            emit Foo { n: 1 };
            emit Foo { n: 2 };
            return 0;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    // Each emit is logged, regardless of whether a handler ran.
    assert_eq!(out.events.len(), 2);
    assert_eq!(out.events[0].name, "Foo");
    assert_eq!(out.events[1].name, "Foo");
}

// ---------- cross-module handlers ----------

#[test]
fn handler_in_module_a_fires_on_emit_from_module_m() {
    use std::collections::HashMap;
    let mut sources = HashMap::new();
    sources.insert(
        "ledger".into(),
        "
            pub struct Credited { user: i64, amount: i64 }
            entry fn credit(user: i64, amount: i64) -> i64 {
                emit Credited { user: user, amount: amount };
                return 0;
            }
        "
        .to_string(),
    );
    sources.insert(
        "auditor".into(),
        "
            state count: i64;
            on ledger::Credited fn track(c: ledger::Credited) {
                count = count + c.amount;
            }
            entry view fn total() -> i64 { return count; }
        "
        .to_string(),
    );
    sources.insert(
        "main".into(),
        "
            fn main() -> i64 {
                ledger::credit(1, 10);
                ledger::credit(2, 25);
                return auditor::total();
            }
        "
        .to_string(),
    );
    let kv = rend::kv::InMemoryKv::new();
    let out = Engine::new()
        .execute_main(&sources, "main", Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::int(35i64));
}

#[test]
fn non_pub_struct_cannot_be_referenced_cross_module() {
    use std::collections::HashMap;
    let mut sources = HashMap::new();
    sources.insert(
        "ledger".into(),
        "
            struct Credited { user: i64, amount: i64 }
            entry fn credit(user: i64, amount: i64) -> i64 {
                emit Credited { user: user, amount: amount };
                return 0;
            }
        "
        .to_string(),
    );
    sources.insert(
        "auditor".into(),
        "
            state count: i64;
            on ledger::Credited fn track(c: ledger::Credited) {
                count = count + c.amount;
            }
            entry view fn total() -> i64 { return count; }
        "
        .to_string(),
    );
    sources.insert(
        "main".into(),
        "
            fn main() -> i64 {
                ledger::credit(1, 10);
                return auditor::total();
            }
        "
        .to_string(),
    );
    let kv = rend::kv::InMemoryKv::new();
    let err = Engine::new()
        .execute_main(&sources, "main", Fuel::new(50_000), &kv)
        .unwrap_err();
    // Without `pub`, `ledger::Credited` is private — the auditor
    // module's `on ledger::Credited` reference fails to resolve.
    assert!(
        err.to_string().to_lowercase().contains("credited")
            || err.to_string().to_lowercase().contains("unknown"),
        "expected unknown-struct error, got: {err}",
    );
}

#[test]
fn cyclic_handler_chain_terminates_via_fuel() {
    // A → emit B → handler emits A → handler emits B → ...
    // The chain is unbounded; fuel exhaustion is the safety net.
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct A { n: i64 }
        struct B { n: i64 }
        on A fn ha(a: A) { emit B { n: a.n + 1 }; }
        on B fn hb(b: B) { emit A { n: b.n + 1 }; }
        fn main() -> i64 {
            emit A { n: 0 };
            return 0;
        }
    ";
    let err = Engine::new()
        .execute(src, Fuel::new(1000), &kv)
        .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("fuel"),
        "expected fuel exhaustion, got: {err}",
    );
}
