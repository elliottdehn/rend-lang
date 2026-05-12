//! Whole-tree iteration over pmap and pvec — the foundation for
//! comprehensions, aggregations, and SQL-replacement queries over
//! persistent state. Hash-of-key order on pmap walks is deterministic
//! but not user-meaningful; tests assert on shape, sums, or membership
//! rather than position.

use rend::value::Value;
use rend::{run, Engine, Fuel};

// ---------- pmap_entries / keys / values ----------

#[test]
fn pmap_entries_walks_every_pair() {
    let v = run("
        state balances: pmap<Address, u64>;
        fn main() -> i64 {
            balances[address(\"alice\")] = 100u64;
            balances[address(\"bob\")]   = 200u64;
            balances[address(\"carol\")] = 300u64;
            return len(pmap_entries(balances));
        }
    ").unwrap();
    assert_eq!(v, Value::int(3i64));
}

#[test]
fn pmap_entries_yields_tuple_payloads() {
    // Sum every value in the map. Iteration order is hash-of-key,
    // so the sum is the only stable assertion we can make.
    let v = run("
        state ledger: pmap<i64, u64>;
        fn main() -> u64 {
            ledger[1] = 10u64;
            ledger[2] = 20u64;
            ledger[3] = 30u64;
            ledger[4] = 40u64;
            let entries = pmap_entries(ledger);
            let total = 0u64;
            for kv in entries {
                total = total + kv.1;
            }
            return total;
        }
    ").unwrap();
    assert_eq!(v, Value::U64(100));
}

#[test]
fn pmap_keys_returns_just_keys() {
    let v = run("
        state ledger: pmap<i64, u64>;
        fn main() -> i64 {
            ledger[5] = 1u64;
            ledger[7] = 1u64;
            ledger[9] = 1u64;
            let keys = pmap_keys(ledger);
            let total = 0;
            for k in keys { total = total + k; }
            return total;     // 5 + 7 + 9
        }
    ").unwrap();
    assert_eq!(v, Value::int(21i64));
}

#[test]
fn pmap_values_returns_just_values() {
    let v = run("
        state ledger: pmap<string, u64>;
        fn main() -> u64 {
            ledger[\"a\"] = 100u64;
            ledger[\"b\"] = 200u64;
            ledger[\"c\"] = 300u64;
            let values = pmap_values(ledger);
            let total = 0u64;
            for v in values { total = total + v; }
            return total;
        }
    ").unwrap();
    assert_eq!(v, Value::U64(600));
}

#[test]
fn pmap_entries_on_empty_map_is_empty() {
    let v = run("
        state ledger: pmap<i64, u64>;
        fn main() -> i64 {
            return len(pmap_entries(ledger));
        }
    ").unwrap();
    assert_eq!(v, Value::int(0i64));
}

#[test]
fn pmap_entries_after_overwrite_dedupes() {
    let v = run("
        state ledger: pmap<i64, u64>;
        fn main() -> i64 {
            ledger[1] = 1u64;
            ledger[1] = 2u64;     // overwrite, not new entry
            ledger[1] = 3u64;
            return len(pmap_entries(ledger));
        }
    ").unwrap();
    assert_eq!(v, Value::int(1i64));
}

// ---------- pvec_to_array ----------

#[test]
fn pvec_to_array_yields_index_order() {
    let v = run("
        state log: pvec<i64>;
        fn main() -> i64 {
            pvec_push(log, 10);
            pvec_push(log, 20);
            pvec_push(log, 30);
            let arr = pvec_to_array(log);
            return arr[0] + arr[1] * 10 + arr[2] * 100;
            // 10 + 200 + 3000 = 3210
        }
    ").unwrap();
    assert_eq!(v, Value::int(3210i64));
}

#[test]
fn pvec_to_array_on_empty_is_empty() {
    let v = run("
        state log: pvec<i64>;
        fn main() -> i64 { return len(pvec_to_array(log)); }
    ").unwrap();
    assert_eq!(v, Value::int(0i64));
}

#[test]
fn pvec_to_array_handles_partial_last_block() {
    // 33 elements crosses one trie level boundary (32-way branching);
    // the last leaf is partial. Verify the walk doesn't emit padding.
    let v = run("
        state log: pvec<i64>;
        fn main() -> i64 {
            for i in 0..33 { pvec_push(log, i); }
            let arr = pvec_to_array(log);
            return len(arr);
        }
    ").unwrap();
    assert_eq!(v, Value::int(33i64));
}

#[test]
fn pvec_to_array_sum_after_many_pushes() {
    let v = run("
        state log: pvec<i64>;
        fn main() -> i64 {
            for i in 1..=100 { pvec_push(log, i); }
            let arr = pvec_to_array(log);
            let total = 0;
            for x in arr { total = total + x; }
            return total;            // 1..=100 = 5050
        }
    ").unwrap();
    assert_eq!(v, Value::int(5050i64));
}

// ---------- effects: walks classify as ReadOnly ----------

#[test]
fn pmap_entries_is_callable_from_view_fn() {
    // pmap_entries reads state; should be ReadOnly, fine in view.
    let src = "
        state ledger: pmap<i64, u64>;
        view fn main() -> i64 {
            return len(pmap_entries(ledger));
        }
    ";
    let kv = rend::kv::InMemoryKv::new();
    Engine::new()
        .execute(src, Fuel::new(10_000), &kv)
        .unwrap();
}

#[test]
fn pmap_entries_is_rejected_in_pure_fn() {
    // Pure forbids state reads. pmap_entries reads → reject.
    let src = "
        state ledger: pmap<i64, u64>;
        pure fn main() -> i64 {
            return len(pmap_entries(ledger));
        }
    ";
    let kv = rend::kv::InMemoryKv::new();
    let err = Engine::new()
        .execute(src, Fuel::new(10_000), &kv)
        .unwrap_err();
    assert!(err.to_string().contains("pure"), "got: {err}");
}

// ---------- bytecode parity: walks work on the VM path too ----------

#[test]
fn pmap_entries_works_on_bytecode_vm() {
    // run() uses the interp; execute() goes through the bytecode VM.
    // Verify both paths agree.
    let src = "
        state ledger: pmap<i64, u64>;
        fn main() -> u64 {
            ledger[1] = 10u64;
            ledger[2] = 20u64;
            ledger[3] = 30u64;
            let total = 0u64;
            for v in pmap_values(ledger) { total = total + v; }
            return total;
        }
    ";
    let kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .execute(src, Fuel::new(20_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::U64(60));
}

#[test]
fn pvec_to_array_works_on_bytecode_vm() {
    let src = "
        state log: pvec<u64>;
        fn main() -> u64 {
            pvec_push(log, 1u64);
            pvec_push(log, 2u64);
            pvec_push(log, 3u64);
            let total = 0u64;
            for x in pvec_to_array(log) { total = total + x; }
            return total;
        }
    ";
    let kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .execute(src, Fuel::new(20_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::U64(6));
}
