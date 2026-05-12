//! Smoke tests for the new examples (35–36) demonstrating
//! view/pure annotations, the deploy + query + tx flow, and
//! cross-module composition.

use rend::value::Value;
use rend::{Engine, Fuel};

// ---------- example 35: queryable token ----------

const TOKEN_PATH: &str = "examples/35_queryable_token.rd";

#[test]
fn example_35_deploy_seeds_initial_supply() {
    // Constructor (`fn main()`) runs at deploy and mints
    // 1_000_000 to the deployer. The constructor returns the
    // new total supply so the host can sanity-check the seed.
    let token_src = std::fs::read_to_string(TOKEN_PATH).unwrap();
    let token = Engine::new().compile(&token_src).unwrap();

    let kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .deploy(&token, Fuel::new(50_000), &kv)
        .unwrap()
        .expect("token has a constructor");
    assert_eq!(outcome.result, Value::U64(1_000_000));
}

#[test]
fn example_35_supports_query_for_view_entries() {
    // Deploy → apply writes → query the view entry without
    // running a write tx. This is the host pattern that
    // motivates the `view` annotation: read-only entries can
    // be served on a faster path, and the runtime statically
    // knows it's safe.
    let token_src = std::fs::read_to_string(TOKEN_PATH).unwrap();
    let token = Engine::new().compile(&token_src).unwrap();

    let mut kv = rend::kv::InMemoryKv::new();
    let deploy_out = Engine::new()
        .deploy(&token, Fuel::new(50_000), &kv)
        .unwrap()
        .unwrap();
    kv.apply(&deploy_out.writes);

    // Query supply via a tiny read-only tx.
    let query_tx = Engine::new()
        .compile_tx(
            "module main;
             view fn main() -> u64 { return token::supply(); }",
            &[token.clone()],
        )
        .unwrap();
    let q = Engine::new()
        .query(&query_tx, &[token], Fuel::new(20_000), &kv)
        .unwrap();
    assert_eq!(q.result, Value::U64(1_000_000));
}

#[test]
fn example_35_supports_tx_for_mutating_entries() {
    // Deploy + tx (transfer) + query (balance_of). End-to-end
    // demonstration of the deploy/tx/query split.
    let token_src = std::fs::read_to_string(TOKEN_PATH).unwrap();
    let token = Engine::new().compile(&token_src).unwrap();

    let mut kv = rend::kv::InMemoryKv::new();
    // Deploy as a specific deployer so the constructor's
    // `msg_sender()` is predictable.
    let deployer = Value::Address("deployer".to_string());
    let deploy_out = Engine::new()
        .deploy_with_context(
            &token,
            rend::tx::TxContext {
                sender: deployer.clone(),
                block_timestamp: 0,
                block_number: 0,
            },
            Fuel::new(50_000),
            &kv,
        )
        .unwrap()
        .unwrap();
    kv.apply(&deploy_out.writes);

    // Tx: deployer transfers 250 to alice. main is *not*
    // annotated view (it writes), so this can't be a query.
    let tx = Engine::new()
        .compile_tx(
            "module main;
             fn main() -> u64 {
                 return token::transfer(address(\"alice\"), 250u64);
             }",
            &[token.clone()],
        )
        .unwrap();
    let tx_out = Engine::new()
        .execute_tx_with_context(
            &tx, &[token.clone()],
            rend::tx::TxContext {
                sender: deployer.clone(),
                block_timestamp: 0,
                block_number: 0,
            },
            Fuel::new(20_000), &kv,
        )
        .unwrap();
    kv.apply(&tx_out.writes);

    // Query alice's balance.
    let q_tx = Engine::new()
        .compile_tx(
            "module main;
             view fn main() -> u64 { return token::balance_of(address(\"alice\")); }",
            &[token.clone()],
        )
        .unwrap();
    let q = Engine::new()
        .query(&q_tx, &[token], Fuel::new(20_000), &kv)
        .unwrap();
    assert_eq!(q.result, Value::U64(250));
}

// ---------- example 36: modular market ----------

#[test]
fn example_36_modular_market_runs_end_to_end() {
    let mut sources = Vec::new();
    for e in std::fs::read_dir("examples/36_modular_market").unwrap() {
        let p = e.unwrap().path();
        if p.extension().and_then(|s| s.to_str()) == Some("rd") {
            sources.push(std::fs::read_to_string(p).unwrap());
        }
    }
    let kv = rend::kv::InMemoryKv::new();
    let out = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(200_000), &kv)
        .unwrap();
    // 3 listings * 1_000_000 + 70 (sum of sold-listing prices: 50 + 20).
    assert_eq!(out.result, Value::U64(3_000_070));
}

// ---------- example 39: indexed queries (SQL-replacement shape) ----------

const DIRECTORY_PATH: &str = "examples/39_indexed_queries.rd";

#[test]
fn example_39_deploy_seeds_directory() {
    let src = std::fs::read_to_string(DIRECTORY_PATH).unwrap();
    let artifact = Engine::new().compile(&src).unwrap();
    let kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .deploy(&artifact, Fuel::new(200_000), &kv)
        .unwrap()
        .expect("directory has a constructor");
    assert_eq!(outcome.result, Value::U64(5));
}

#[test]
fn example_39_indexed_lookup_and_aggregations_via_query_path() {
    // Deploy, apply the constructor's writes, then exercise the
    // query path against `view` entries. The host serves these
    // without OCC bookkeeping — exactly what the SQL replacement
    // pitch promises for read-only workloads.
    let src = std::fs::read_to_string(DIRECTORY_PATH).unwrap();
    let artifact = Engine::new().compile(&src).unwrap();
    let mut kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .deploy(&artifact, Fuel::new(200_000), &kv)
        .unwrap()
        .unwrap();
    kv.apply(&outcome.writes);

    // Index lookup: alice@x.com → id 1.
    let q = "module q;
        view fn main() -> u64 { return directory::find_by_email(\"alice@x.com\").id; }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact.clone()], Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::U64(1));

    // Total payroll: 150 + 130 + 200 + 110 + 140 = 730 (in thousands).
    let q = "module q;
        view fn main() -> u64 { return directory::total_payroll(); }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact.clone()], Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::U64(730_000));

    // Top eng salary: 150_000 (alice).
    let q = "module q;
        view fn main() -> u64 { return directory::top_salary(\"eng\"); }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact.clone()], Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::U64(150_000));

    // Count hired since 2021: bob, dan, eve = 3.
    let q = "module q;
        view fn main() -> i64 { return directory::count_hired_since(2021u64); }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact.clone()], Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::int(3i64));

    // Average eng salary: (150 + 130 + 140) / 3 = 140 thousand.
    let q = "module q;
        view fn main() -> u64 { return directory::avg_salary(\"eng\"); }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact], Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::U64(140_000));
}

#[test]
fn example_39_multi_index_queries() {
    // Exercise the by_dept multi-index: list_in_dept_indexed and
    // dept_payroll_indexed both pull from the index, not from a
    // scan.
    let src = std::fs::read_to_string(DIRECTORY_PATH).unwrap();
    let artifact = Engine::new().compile(&src).unwrap();
    let mut kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .deploy(&artifact, Fuel::new(200_000), &kv)
        .unwrap()
        .unwrap();
    kv.apply(&outcome.writes);

    // dept_payroll_indexed("eng") = 150 + 130 + 140 = 420 thousand.
    let q = "module q;
        view fn main() -> u64 { return directory::dept_payroll_indexed(\"eng\"); }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact.clone()], Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::U64(420_000));

    // list_in_dept_indexed("sales") should return 2 records.
    let q = "module q;
        view fn main() -> i64 { return len(directory::list_in_dept_indexed(\"sales\")); }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact], Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::int(2i64));
}

#[test]
fn example_39_paginated_query_through_sorted_index() {
    // The new sorted-index path: `by_id` is a pbtree<u64, u64>,
    // so `page_ids(after_id, n)` returns primary keys via
    // pbtree_range. Verifies the maintenance landed correctly
    // and the range query walks in key order.
    let src = std::fs::read_to_string(DIRECTORY_PATH).unwrap();
    let artifact = Engine::new().compile(&src).unwrap();
    let mut kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .deploy(&artifact, Fuel::new(200_000), &kv)
        .unwrap()
        .unwrap();
    kv.apply(&outcome.writes);

    // page_ids(2, 3) — ids in [2, 5] inclusive — should be [2, 3, 4, 5].
    let q = "module q;
        view fn main() -> i64 { return len(directory::page_ids(2u64, 3u64)); }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact.clone()], Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::int(4i64));

    // first_id() — smallest id seeded is 1.
    let q = "module q;
        view fn main() -> u64 { return directory::first_id(); }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact], Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::U64(1));
}

#[test]
fn example_39_writes_through_index() {
    // Run a tx that registers a new user, then verify the index
    // lookup picks them up — proves the auto-maintained index is
    // populated by the user-level write, not by hand.
    let src = std::fs::read_to_string(DIRECTORY_PATH).unwrap();
    let artifact = Engine::new().compile(&src).unwrap();
    let mut kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .deploy(&artifact, Fuel::new(200_000), &kv)
        .unwrap()
        .unwrap();
    kv.apply(&outcome.writes);

    let tx_src = "module tx;
        fn main() -> u64 {
            return directory::register(
                99u64, \"frank\", \"frank@x.com\", \"eng\", 2024u64, 125000u64,
            );
        }";
    let tx = Engine::new().compile_tx(tx_src, &[artifact.clone()]).unwrap();
    let tx_out = Engine::new()
        .execute_tx(&tx, &[artifact.clone()], Fuel::new(100_000), &kv)
        .unwrap();
    kv.apply(&tx_out.writes);

    // Look up via the index — frank@x.com should resolve to id 99.
    let q = "module q;
        view fn main() -> u64 { return directory::find_by_email(\"frank@x.com\").id; }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact], Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::U64(99));
}

// ---------- example 40: event log (JSON + delete + indexes) ----------

const EVENT_LOG_PATH: &str = "examples/40_event_log.rd";

#[test]
fn example_40_deploy_seeds_events() {
    let src = std::fs::read_to_string(EVENT_LOG_PATH).unwrap();
    let artifact = Engine::new().compile(&src).unwrap();
    let kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .deploy(&artifact, Fuel::new(500_000), &kv)
        .unwrap()
        .expect("constructor exists");
    // 5 events seeded; next_seq returns 5.
    assert_eq!(outcome.result, Value::U64(5));
}

#[test]
fn example_40_indexed_lookups_and_aggregations() {
    let src = std::fs::read_to_string(EVENT_LOG_PATH).unwrap();
    let artifact = Engine::new().compile(&src).unwrap();
    let mut kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .deploy(&artifact, Fuel::new(500_000), &kv)
        .unwrap()
        .unwrap();
    kv.apply(&outcome.writes);

    // count_for_user(100) — alice has 3 events (login + 2 purchases).
    let q = "module q;
        view fn main() -> i64 { return event_log::count_for_user(100u64); }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact.clone()], Fuel::new(100_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::int(3i64));

    // total_purchase_amount(100) — sums "amount" field from JSON
    // payloads of alice's purchase events: 25 + 50 = 75.
    let q = "module q;
        view fn main() -> i64 { return event_log::total_purchase_amount(100u64); }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact.clone()], Fuel::new(100_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::int(75i64));

    // page(0, 3) — sorted-index range over keys [0, 3] inclusive.
    // Seq numbers start at 1, so this matches seqs 1, 2, 3 = 3 events.
    let q = "module q;
        view fn main() -> i64 { return len(event_log::page(0u64, 3u64)); }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact.clone()], Fuel::new(100_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::int(3i64));

    // session_id_for(2) — JSON path access on stored payload.
    let q = "module q;
        view fn main() -> string { return event_log::session_id_for(2u64); }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact], Fuel::new(100_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::Str("s1".to_string()));
}

#[test]
fn example_40_delete_cleans_indexes() {
    let src = std::fs::read_to_string(EVENT_LOG_PATH).unwrap();
    let artifact = Engine::new().compile(&src).unwrap();
    let mut kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .deploy(&artifact, Fuel::new(500_000), &kv)
        .unwrap()
        .unwrap();
    kv.apply(&outcome.writes);

    // Run a tx that deletes event 2.
    let tx_src = "module tx;
        fn main() -> u64 { return event_log::delete_event(2u64); }";
    let tx = Engine::new().compile_tx(tx_src, &[artifact.clone()]).unwrap();
    let tx_out = Engine::new()
        .execute_tx(&tx, &[artifact.clone()], Fuel::new(100_000), &kv)
        .unwrap();
    kv.apply(&tx_out.writes);

    // After deletion, alice's count drops to 2 and the purchase
    // total drops by event 2's amount (25 → leaving 50).
    let q = "module q;
        view fn main() -> i64 { return event_log::count_for_user(100u64); }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact.clone()], Fuel::new(100_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::int(2i64));

    let q = "module q;
        view fn main() -> i64 { return event_log::total_purchase_amount(100u64); }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact], Fuel::new(100_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::int(50i64));
}

#[test]
fn example_40_record_via_tx_with_raw_json() {
    // Deploy fresh, then send a host-shaped raw JSON payload via tx.
    let src = std::fs::read_to_string(EVENT_LOG_PATH).unwrap();
    let artifact = Engine::new().compile(&src).unwrap();
    let mut kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .deploy(&artifact, Fuel::new(500_000), &kv)
        .unwrap()
        .unwrap();
    kv.apply(&outcome.writes);

    let tx_src = r#"module tx;
        fn main() -> u64 {
            return event_log::record(
                999u64,
                "{\"user\": {\"id\": 42}, \"kind\": \"signup\", \"timestamp\": 1700000999, \"session_id\": \"new\"}",
            );
        }"#;
    let tx = Engine::new().compile_tx(tx_src, &[artifact.clone()]).unwrap();
    let tx_out = Engine::new()
        .execute_tx(&tx, &[artifact.clone()], Fuel::new(200_000), &kv)
        .unwrap();
    kv.apply(&tx_out.writes);

    // Look up the new event via index.
    let q = "module q;
        view fn main() -> string {
            return event_log::session_id_for(999u64);
        }";
    let q_tx = Engine::new().compile_tx(q, &[artifact.clone()]).unwrap();
    let r = Engine::new()
        .query(&q_tx, &[artifact], Fuel::new(100_000), &kv)
        .unwrap();
    assert_eq!(r.result, Value::Str("new".to_string()));
}

// ---------- example 38: swappable oracle ----------

#[test]
fn example_38_swappable_oracle_routes_correctly() {
    let mut sources = Vec::new();
    for e in std::fs::read_dir("examples/38_swappable_oracle").unwrap() {
        let p = e.unwrap().path();
        if p.extension().and_then(|s| s.to_str()) == Some("rd") {
            sources.push(std::fs::read_to_string(p).unwrap());
        }
    }
    let kv = rend::kv::InMemoryKv::new();
    let out = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(200_000), &kv)
        .unwrap();
    // simple = 15000, premium = 15750, haircut(15750, 100bps) = 15592
    // return: 15000 * 1_000_000 + 15750 * 100 + 15592
    assert_eq!(out.result, Value::U64(15_001_590_592));
}

#[test]
fn example_36_view_query_against_multi_module_artifact() {
    // Bundle the three modules into one artifact, then exercise
    // `Engine::query` against it. The query tx defines its own
    // `main` (compile_tx requires it) that calls into the
    // already-compiled `market::how_many` view entry.
    let mut sources = Vec::new();
    for e in std::fs::read_dir("examples/36_modular_market").unwrap() {
        let p = e.unwrap().path();
        if p.extension().and_then(|s| s.to_str()) != Some("rd") { continue; }
        let body = std::fs::read_to_string(&p).unwrap();
        // The directory's `main.rd` would clash on module-name
        // with the query tx (both want `module main;`); leave it
        // out for this query-only test.
        if !body.contains("module main;") {
            sources.push(body);
        }
    }
    let artifact = Engine::new().compile_modules(&sources).unwrap();

    // Query: how many listings (zero, since we haven't run the
    // demo flow). Just exercises that `view` cross-module calls
    // route through `Engine::query` cleanly.
    let q_tx = Engine::new()
        .compile_tx(
            "module q;
             view fn main() -> u64 { return market::how_many(); }",
            &[artifact.clone()],
        )
        .unwrap();
    let kv = rend::kv::InMemoryKv::new();
    let q = Engine::new()
        .query(&q_tx, &[artifact], Fuel::new(20_000), &kv)
        .unwrap();
    assert_eq!(q.result, Value::U64(0));
}
