//! Slice 7+9: map state — per-cell read/write sets, indexed assignment,
//! u128-keyed cells composed via FNV-128(state_root, serialize(key)).

use rend::ast::Type;
use rend::hashing::{child, state_root};
use rend::kv::InMemoryKv;
use rend::serialize::serialize;
use rend::value::Value;
use rend::{Engine, Fuel};

fn engine() -> Engine {
    Engine::new()
}

fn cell(map_name: &str, key: &Value) -> u128 {
    child(state_root("main", map_name), &serialize(key))
}

#[test]
fn unset_map_cell_reads_as_default() {
    let kv = InMemoryKv::new();
    let src = "
        state balances: map<i64, i64>;
        fn main() -> i64 { return balances[42]; }
    ";
    let out = engine().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(0));
    assert_eq!(out.reads.get(&cell("balances", &Value::Int(42))), Some(&Value::Int(0)));
}

#[test]
fn assign_then_read_a_map_cell() {
    let kv = InMemoryKv::new();
    let src = "
        state balances: map<i64, i64>;
        fn main() -> i64 {
            balances[7] = 99;
            return balances[7];
        }
    ";
    let out = engine().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(99));
    assert_eq!(out.writes.get(&cell("balances", &Value::Int(7))), Some(&Value::Int(99)));
}

#[test]
fn transfer_produces_two_cell_rw_set() {
    let kv = InMemoryKv::new();
    let src = "
        state balances: map<i64, i64>;
        entry fn transfer(from: i64, to: i64, amount: i64) -> bool {
            let b = balances[from];
            if b < amount { return false; }
            balances[from] = b - amount;
            balances[to] = balances[to] + amount;
            return true;
        }
        fn main() -> bool {
            balances[1] = 100;
            return transfer(1, 2, 30);
        }
    ";
    let out = engine().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Bool(true));
    assert_eq!(out.writes.get(&cell("balances", &Value::Int(1))), Some(&Value::Int(70)));
    assert_eq!(out.writes.get(&cell("balances", &Value::Int(2))), Some(&Value::Int(30)));
}

#[test]
fn map_persistence_across_runs() {
    let mut kv = InMemoryKv::new();
    let src = "
        state balances: map<i64, i64>;
        fn main() -> i64 {
            balances[1] = balances[1] + 10;
            return balances[1];
        }
    ";
    for expected in [10, 20, 30] {
        let out = engine().execute(src, Fuel::new(10_000), &kv).unwrap();
        assert_eq!(out.result, Value::Int(expected));
        kv.apply(&out.writes);
    }
}

#[test]
fn map_key_type_mismatch_is_compile_error() {
    let kv = InMemoryKv::new();
    let src = "
        state balances: map<i64, i64>;
        fn main() -> i64 { return balances[true]; }
    ";
    let err = engine().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(err.to_string().contains("expected i64"));
}

#[test]
fn cant_index_a_non_map_state() {
    let kv = InMemoryKv::new();
    let src = "
        state count: i64;
        fn main() -> i64 { return count[0]; }
    ";
    let err = engine().execute(src, Fuel::new(1000), &kv).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("indexing requires") || msg.contains("not a map"),
        "got {msg}",
    );
}

#[test]
fn string_keyed_map_works() {
    let kv = InMemoryKv::new();
    let src = r#"
        state names: map<string, i64>;
        fn main() -> i64 {
            names["alice"] = 1;
            names["bob"]   = 2;
            return names["alice"] + names["bob"];
        }
    "#;
    let out = engine().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(3));
    assert_eq!(
        out.writes.get(&cell("names", &Value::Str("alice".into()))),
        Some(&Value::Int(1))
    );
}

#[test]
fn address_keyed_map_works() {
    let kv = InMemoryKv::new();
    let src = r#"
        state holders: map<Address, i64>;
        fn main() -> i64 {
            holders[address("0xfeed")] = 100;
            return holders[address("0xfeed")];
        }
    "#;
    let out = engine().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(100));
    let key = cell("holders", &Value::Address("0xfeed".into()));
    assert_eq!(out.writes.get(&key), Some(&Value::Int(100)));
    let _ = Type::Address;
}
