//! End-to-end smoke tests for the examples that exercise the
//! recently-landed surface area: arbitrary-precision integers,
//! source-level JSON literals, and struct field grouping.
//!
//! These exist mainly so the examples can't bitrot — every demo
//! we ship in `crates/rend/examples/` should compile and produce
//! the value its commentary advertises.

use num_bigint::BigInt;
use num_traits::Pow;

use rend::value::Value;

#[test]
fn example_41_bigint_arithmetic_overflows_u128_without_breaking() {
    // Compounds 1u at 100% for 256 rounds: the answer is 2^256,
    // ~1.16e77, well past u128::MAX (~3.4e38). A `u128` would
    // panic; `uint` just keeps going.
    let src = std::fs::read_to_string("examples/41_bigint_arithmetic.rd").unwrap();
    let out = rend::run(&src).unwrap();
    let expected = BigInt::from(2u32).pow(256u32);
    assert_eq!(out, Value::uint(expected));
}

#[test]
fn example_42_polymorphic_json_round_trips_mixed_shapes() {
    // Stores a bool, an int, an object-of-int, an object-holding-an-array,
    // and an explicit null in one `pmap<string, json>`, then reads each
    // back with the right extractor. Encoded result:
    //   attempts*1_000_000 + max_rate*100 + tier2*10 + (1 if unset)
    // = 5*1_000_000 + 100*100 + 50*10 + 1 = 5_010_501.
    let src = std::fs::read_to_string("examples/42_polymorphic_json.rd").unwrap();
    let out = rend::run(&src).unwrap();
    assert_eq!(out, Value::int(5_010_501i64));
}

#[test]
fn example_43_field_groups_keeps_balance_granular_and_profile_grouped() {
    // Two users at disjoint state roots. Alice gets credited twice
    // through the granular `balance` cell; her email gets updated
    // through the `profile` group via RMW. Final result is
    // alice.balance (150) + bob.balance (9) = 159.
    let src = std::fs::read_to_string("examples/43_field_groups.rd").unwrap();
    let out = rend::run(&src).unwrap();
    assert_eq!(out, Value::U64(159));
}

#[test]
fn example_44_event_handlers_fires_each_handler_on_every_emit() {
    // Four `submit` calls × one `bump_total` handler = 4. Disjoint
    // sibling handlers (`record_score`, `track_seen`) merge cleanly
    // in parallel with no re-run.
    let src = std::fs::read_to_string("examples/44_event_handlers.rd").unwrap();
    let out = rend::run(&src).unwrap();
    assert_eq!(out, Value::U64(4));
}

#[test]
fn example_48_multi_symbol_order_book_uses_three_field_composite() {
    // Three-field composite index `(symbol ASC, price ASC|DESC,
    // placed_at ASC)`; one shared pbtree per side serves all
    // instruments. Three fills + non-trivial leftovers per
    // symbol; encoded result lands at 3_501_030_200 — see the
    // example's commentary for the breakdown.
    use rend::{Engine, Fuel};
    let src = std::fs::read_to_string("examples/48_order_book_v2.rd").unwrap();
    let kv = rend::kv::InMemoryKv::new();
    let out = Engine::new()
        .execute(&src, Fuel::new(500_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::U64(3_501_030_200));
}

#[test]
fn example_47_order_book_with_composite_pbtree_bytes_keys() {
    // Bytes-keyed pbtree gives unbounded composite-index arity.
    // The order-book example hand-packs (price, time) into the
    // pbtree key (DESC = `bit_not_bytes` on the price bytes);
    // matching sweeps the book in priority order with explicit
    // back-link deletion.
    //
    //   2 fills (10@100, 5@102) × 1_000_000 = 2_000_000
    //   best_bid_price (105) × 1000          =     105_000
    //   best_ask_price (0, fully consumed)   =           0
    //   total                                = 2_105_000
    use rend::{Engine, Fuel};
    let src = std::fs::read_to_string("examples/47_order_book.rd").unwrap();
    let kv = rend::kv::InMemoryKv::new();
    let out = Engine::new()
        .execute(&src, Fuel::new(200_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::U64(2_105_000));
}

#[test]
fn example_46_parallel_bulk_credit_runs_disjoint_rmw_concurrently() {
    // Two batch credits, each doing three R→W ops on disjoint
    // balance cells inside a `parallel` block. The reads issue
    // concurrently (instead of serializing behind the writes
    // that would otherwise fence them) and the disjoint writes
    // merge without re-run. Final balances:
    //   alice: 1000 + 100 + 200 = 1300
    //   bob:    500 +  50 +  75 =  625
    //   carol:  250 +  25 +  30 =  305
    //   sum                     = 2230
    let src = std::fs::read_to_string("examples/46_parallel_bulk_credit.rd").unwrap();
    let out = rend::run(&src).unwrap();
    assert_eq!(out, Value::U64(2230));
}

#[test]
fn example_45_audit_plugin_cross_module_handler_records_every_transfer() {
    // The `audit` module's `on bank::Transferred fn record` handler
    // appends to a pmap. Three transfers in `main` → three entries.
    use rend::{Engine, Fuel};
    use std::collections::HashMap;
    let bank  = std::fs::read_to_string("examples/45_audit_plugin/bank.rd").unwrap();
    let audit = std::fs::read_to_string("examples/45_audit_plugin/audit.rd").unwrap();
    let main_ = std::fs::read_to_string("examples/45_audit_plugin/main.rd").unwrap();
    let mut sources = HashMap::new();
    sources.insert("bank".to_string(), bank);
    sources.insert("audit".to_string(), audit);
    sources.insert("main".to_string(), main_);
    let kv = rend::kv::InMemoryKv::new();
    let out = Engine::new()
        .execute_main(&sources, "main", Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::U64(3));
    // The bank module never imports audit — the audit handler still
    // ran. That's the decoupling guarantee the example demonstrates.
    assert_eq!(out.events.len(), 3);
    assert!(out.events.iter().all(|e| e.module == "bank" && e.name == "Transferred"));
}
