//! Comprehensions over the persistent-collection walks. The walks
//! (pmap_entries, pmap_keys, pmap_values, pvec_to_array) return
//! ordinary `[T]` arrays, so existing comprehension syntax works
//! unchanged. These tests pin down the integration end-to-end.

use rend::value::Value;
use rend::{run, Engine, Fuel};

// ---------- list comprehensions ----------

#[test]
fn list_comp_filters_pmap_values() {
    // The "SELECT * WHERE balance > 100" shape.
    let v = run("
        state balances: pmap<Address, u64>;
        fn main() -> u64 {
            balances[address(\"alice\")] = 50u64;
            balances[address(\"bob\")]   = 200u64;
            balances[address(\"carol\")] = 75u64;
            balances[address(\"dan\")]   = 300u64;
            let bigs = [b for b in pmap_values(balances) if b > 100u64];
            let total = 0u64;
            for b in bigs { total = total + b; }
            return total;          // 200 + 300
        }
    ").unwrap();
    assert_eq!(v, Value::U64(500));
}

#[test]
fn list_comp_projects_pmap_entries() {
    // The "SELECT key FROM map WHERE value > X" shape — project the
    // tuple.0 out of each entry.
    let v = run("
        state ledger: pmap<i64, u64>;
        fn main() -> i64 {
            ledger[1] = 10u64;
            ledger[2] = 50u64;
            ledger[3] = 30u64;
            ledger[4] = 80u64;
            let big_keys = [e.0 for e in pmap_entries(ledger) if e.1 > 25u64];
            let total = 0;
            for k in big_keys { total = total + k; }
            return total;          // 2 + 3 + 4
        }
    ").unwrap();
    assert_eq!(v, Value::int(9i64));
}

#[test]
fn list_comp_over_pvec_to_array() {
    let v = run("
        state log: pvec<i64>;
        fn main() -> i64 {
            for i in 1..=10 { pvec_push(log, i); }
            let evens = [x for x in pvec_to_array(log) if x % 2 == 0];
            let total = 0;
            for x in evens { total = total + x; }
            return total;          // 2 + 4 + 6 + 8 + 10
        }
    ").unwrap();
    assert_eq!(v, Value::int(30i64));
}

// ---------- set comprehensions ----------

#[test]
fn set_comp_dedups_over_pmap_values() {
    // pmap with duplicate values; set-comprehension naturally
    // dedupes.
    let v = run("
        state colors: pmap<i64, i64>;
        fn main() -> i64 {
            colors[1] = 7;
            colors[2] = 7;          // duplicate value
            colors[3] = 13;
            colors[4] = 7;          // another duplicate
            colors[5] = 21;
            let unique = set { v for v in pmap_values(colors) };
            return set_len(unique); // {7, 13, 21}
        }
    ").unwrap();
    assert_eq!(v, Value::int(3i64));
}

// ---------- dict comprehensions ----------

#[test]
fn dict_comp_inverts_pmap() {
    // Build a value-keyed lookup from a key-keyed pmap.
    let v = run("
        state ledger: pmap<i64, u64>;
        fn main() -> i64 {
            ledger[1] = 100u64;
            ledger[2] = 200u64;
            ledger[3] = 300u64;
            let inverted = dict { e.1: e.0 for e in pmap_entries(ledger) };
            // dict lookup: 200 maps back to 2.
            return dict_get(inverted, 200u64, 0);
        }
    ").unwrap();
    assert_eq!(v, Value::int(2i64));
}

// ---------- multi-generator: cross-collection joins ----------

#[test]
fn list_comp_two_pmaps_cross_product() {
    // The "JOIN" shape: two pmaps, one comprehension. Cartesian
    // product of values.
    let v = run("
        state xs: pmap<i64, i64>;
        state ys: pmap<i64, i64>;
        fn main() -> i64 {
            xs[1] = 10;
            xs[2] = 20;
            ys[10] = 1;
            ys[20] = 2;
            let pairs = [a + b
                for a in pmap_values(xs)
                for b in pmap_values(ys)];
            let total = 0;
            for p in pairs { total = total + p; }
            return total;
            // (10+1)+(10+2)+(20+1)+(20+2) = 11+12+21+22 = 66
        }
    ").unwrap();
    assert_eq!(v, Value::int(66i64));
}

// ---------- effects: comprehensions in view fns ----------

#[test]
fn comprehension_over_walk_is_callable_in_view() {
    // The walk is ReadOnly; a comprehension over it stays ReadOnly,
    // so a `view` fn can use it. This is the SQL-replacement
    // ergonomic: a query function is a view fn.
    let src = "
        state balances: pmap<Address, u64>;
        entry view fn count_active() -> i64 {
            return len([b for b in pmap_values(balances) if b > 0u64]);
        }
        fn main() -> i64 {
            balances[address(\"a\")] = 1u64;
            balances[address(\"b\")] = 0u64;
            balances[address(\"c\")] = 5u64;
            return count_active();
        }
    ";
    let kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .execute(src, Fuel::new(20_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::int(2i64));
}

// ---------- for-in sugar over pmap/pvec state directly ----------

#[test]
fn for_in_pmap_state_iterates_values() {
    // `for v in users` works without an explicit pmap_values() call.
    // The typeck recognizes pmap state as iterable; lowering emits
    // PMapValues automatically.
    let v = run("
        state amounts: pmap<i64, u64>;
        fn main() -> u64 {
            amounts[1] = 100u64;
            amounts[2] = 200u64;
            amounts[3] = 50u64;
            let total = 0u64;
            for v in amounts { total = total + v; }
            return total;
        }
    ").unwrap();
    assert_eq!(v, Value::U64(350));
}

#[test]
fn for_in_pvec_state_iterates_elements() {
    let v = run("
        state log: pvec<i64>;
        fn main() -> i64 {
            for i in 1..=10 { pvec_push(log, i); }
            let total = 0;
            for x in log { total = total + x; }
            return total;
        }
    ").unwrap();
    assert_eq!(v, Value::int(55i64));
}

#[test]
fn list_comp_over_pmap_state_directly() {
    // The query-shaped form: comprehension iterates the pmap state
    // by name, no explicit walk call.
    let v = run("
        state amounts: pmap<i64, u64>;
        fn main() -> i64 {
            amounts[1] = 50u64;
            amounts[2] = 200u64;
            amounts[3] = 75u64;
            amounts[4] = 300u64;
            return len([v for v in amounts if v > 100u64]);
        }
    ").unwrap();
    assert_eq!(v, Value::int(2i64));
}

#[test]
fn for_in_pmap_works_on_bytecode_vm() {
    // The compile path needs the same sugar as the interp path.
    let src = "
        state amounts: pmap<i64, u64>;
        fn main() -> u64 {
            amounts[1] = 1u64;
            amounts[2] = 2u64;
            amounts[3] = 3u64;
            let total = 0u64;
            for v in amounts { total = total + v; }
            return total;
        }
    ";
    let kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .execute(src, Fuel::new(20_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::U64(6));
}

#[test]
fn for_in_explicit_walk_still_works() {
    // The sugar shouldn't break the explicit form. People who want
    // entries (or keys) keep using the explicit call.
    let v = run("
        state amounts: pmap<i64, u64>;
        fn main() -> i64 {
            amounts[1] = 10u64;
            amounts[2] = 20u64;
            amounts[3] = 30u64;
            let total = 0;
            for kv in pmap_entries(amounts) {
                total = total + kv.0;       // sum of keys
            }
            return total;
        }
    ").unwrap();
    assert_eq!(v, Value::int(6i64));
}

// ---------- aggregation patterns (preview of slice 3) ----------

#[test]
fn manual_sum_over_pmap_values() {
    // Aggregation builtins land in slice 3, but the pattern works
    // today via for-in over the walk's result.
    let v = run("
        state amounts: pmap<i64, u64>;
        fn main() -> u64 {
            amounts[1] = 100u64;
            amounts[2] = 200u64;
            amounts[3] = 50u64;
            amounts[4] = 25u64;
            let total = 0u64;
            for v in pmap_values(amounts) { total = total + v; }
            return total;
        }
    ").unwrap();
    assert_eq!(v, Value::U64(375));
}

#[test]
fn manual_count_pattern_over_pmap_with_filter() {
    let v = run("
        state amounts: pmap<i64, u64>;
        fn main() -> i64 {
            amounts[1] = 10u64;
            amounts[2] = 200u64;
            amounts[3] = 50u64;
            amounts[4] = 5u64;
            return len([v for v in pmap_values(amounts) if v >= 50u64]);
        }
    ").unwrap();
    assert_eq!(v, Value::int(2i64));
}
