//! Slice 11: multi-module + entry function enforcement.

use std::collections::HashMap;

use rend::ast::Type;
use rend::hashing::{child, state_root};
use rend::kv::InMemoryKv;
use rend::serialize::serialize;
use rend::value::Value;
use rend::{Engine, Fuel};

fn read(path: &str) -> String {
    std::fs::read_to_string(format!("examples/{path}"))
        .unwrap_or_else(|e| panic!("read {path}: {e}"))
}

#[test]
fn cross_module_call_dispatches_correctly() {
    let mut sources = HashMap::new();
    sources.insert(
        "ledger".into(),
        "
            state balances: map<i64, i64>;
            entry fn balance_of(who: i64) -> i64 { return balances[who]; }
            entry fn mint(to: i64, amount: i64) {
                balances[to] = balances[to] + amount;
            }
        "
        .to_string(),
    );
    sources.insert(
        "main".into(),
        "
            fn main() -> i64 {
                ledger::mint(1, 100);
                return ledger::balance_of(1);
            }
        "
        .to_string(),
    );

    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute_main(&sources, "main", Fuel::new(10_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::int(100i64));
}

#[test]
fn each_modules_state_is_namespaced_separately() {
    // Both modules declare a `count` state. They must NOT collide.
    let mut sources = HashMap::new();
    sources.insert(
        "alpha".into(),
        "
            state count: i64;
            entry fn bump() { count = count + 1; }
            entry fn read_count() -> i64 { return count; }
        "
        .to_string(),
    );
    sources.insert(
        "beta".into(),
        "
            state count: i64;
            entry fn bump() { count = count + 10; }
            entry fn read_count() -> i64 { return count; }
        "
        .to_string(),
    );
    sources.insert(
        "main".into(),
        "
            fn main() -> i64 {
                alpha::bump();
                alpha::bump();
                beta::bump();
                return alpha::read_count() + beta::read_count();
            }
        "
        .to_string(),
    );

    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute_main(&sources, "main", Fuel::new(10_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::int(12i64)); // alpha=2, beta=10

    let alpha_root = state_root("alpha", "count");
    let beta_root = state_root("beta", "count");
    assert_eq!(out.writes.get(&alpha_root), Some(&Value::int(2i64)));
    assert_eq!(out.writes.get(&beta_root), Some(&Value::int(10i64)));
    assert_ne!(alpha_root, beta_root, "module-namespacing must distinguish state roots");
}

#[test]
fn non_entry_function_is_not_callable_externally() {
    let mut sources = HashMap::new();
    sources.insert(
        "lib".into(),
        "
            fn helper() -> i64 { return 42; }
            entry fn nope() -> i64 { return helper(); }
        "
        .to_string(),
    );
    sources.insert(
        "main".into(),
        "
            fn main() -> i64 { return lib::helper(); }
        "
        .to_string(),
    );

    let kv = InMemoryKv::new();
    let err = Engine::new()
        .execute_main(&sources, "main", Fuel::new(1000), &kv)
        .unwrap_err();
    assert!(
        err.to_string().contains("entry"),
        "expected entry-only enforcement; got {err}",
    );
}

#[test]
fn unknown_module_is_link_error() {
    let mut sources = HashMap::new();
    sources.insert(
        "main".into(),
        "
            fn main() -> i64 { return ghost::do_thing(); }
        "
        .to_string(),
    );
    let kv = InMemoryKv::new();
    let err = Engine::new()
        .execute_main(&sources, "main", Fuel::new(1000), &kv)
        .unwrap_err();
    assert!(err.to_string().contains("ghost"), "got {err}");
}

#[test]
fn cross_module_arg_type_mismatch_is_compile_error() {
    let mut sources = HashMap::new();
    sources.insert(
        "lib".into(),
        "
            entry fn need_int(n: i64) -> i64 { return n; }
        "
        .to_string(),
    );
    sources.insert(
        "main".into(),
        "
            fn main() -> i64 { return lib::need_int(true); }
        "
        .to_string(),
    );
    let kv = InMemoryKv::new();
    let err = Engine::new()
        .execute_main(&sources, "main", Fuel::new(1000), &kv)
        .unwrap_err();
    assert!(err.to_string().contains("expected i64"), "got {err}");
}

#[test]
fn folder_example_12_runs_end_to_end() {
    let mut sources = HashMap::new();
    sources.insert("ledger".into(), read("12_multi_module/ledger.rd"));
    sources.insert("governance".into(), read("12_multi_module/governance.rd"));
    sources.insert("main".into(), read("12_multi_module/main.rd"));

    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute_main(&sources, "main", Fuel::new(100_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::Bool(true));

    // Verify writes touched the right namespaced cells:
    let recipient_addr = Value::Address("0xrecipient".into());
    let recipient_cell = child(state_root("ledger", "balances"), &serialize(&recipient_addr));
    assert_eq!(out.writes.get(&recipient_cell), Some(&Value::int(100i64)));

    let proposal_cell = child(state_root("governance", "proposals"), &serialize(&Value::int(42i64)));
    assert_eq!(out.writes.get(&proposal_cell), Some(&Value::Bool(true)));

    // governance's state and ledger's state are distinct namespaces.
    assert_ne!(
        state_root("ledger", "balances"),
        state_root("governance", "balances"),
    );

    let _ = Type::Address;
}
