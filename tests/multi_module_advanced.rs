//! Multi-module integration: `module <name>;` declarations + larger
//! end-to-end programs (NFT marketplace, collateralized lending).

use rend::hashing::{child, state_root};
use rend::kv::InMemoryKv;
use rend::occ::{commit_batch, TxRequest};
use rend::serialize::serialize;
use rend::value::Value;
use rend::{Engine, Fuel};

fn read_dir(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(format!("examples/{path}")).unwrap() {
        let p = e.unwrap().path();
        if p.extension().and_then(|s| s.to_str()) == Some("rd") {
            out.push(std::fs::read_to_string(p).unwrap());
        }
    }
    out
}

// ---------- `module <name>;` declaration mechanics ----------

#[test]
fn execute_modules_groups_by_declared_name() {
    // The host hands the engine a flat list of sources; each declares its
    // own name with `module <name>;`. The engine groups them and dispatches
    // `main_module`'s `main()`.
    let sources = vec![
        "module ledger;
         state n: i64;
         entry fn add(d: i64) { n = n + d; }
         entry fn read_n() -> i64 { return n; }
        ".to_string(),
        "module main;
         fn main() -> i64 {
             ledger::add(7);
             ledger::add(8);
             return ledger::read_n();
         }
        ".to_string(),
    ];
    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(10_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::Int(15));
}

#[test]
fn execute_modules_missing_declaration_is_error() {
    let sources = vec![
        "module a;
         entry fn ping() -> i64 { return 1; }
        ".to_string(),
        // No `module ...;` here.
        "fn main() -> i64 { return a::ping(); }".to_string(),
    ];
    let kv = InMemoryKv::new();
    let err = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(1000), &kv)
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("module") && (msg.contains("declaration") || msg.contains("declared") || msg.contains("missing")),
        "expected missing-declaration message; got: {msg}",
    );
}

#[test]
fn execute_modules_duplicate_names_is_error() {
    let sources = vec![
        "module dup;
         entry fn x() -> i64 { return 1; }
        ".to_string(),
        "module dup;
         entry fn y() -> i64 { return 2; }
        ".to_string(),
    ];
    let kv = InMemoryKv::new();
    let err = Engine::new()
        .execute_modules(&sources, "dup", Fuel::new(1000), &kv)
        .unwrap_err();
    assert!(err.to_string().contains("duplicate"), "got: {err}");
}

#[test]
fn execute_main_validates_declaration_matches_key() {
    // If a source carries `module foo;` but is loaded under a different
    // map key, that's an explicit configuration error.
    use std::collections::HashMap;
    let mut sources: HashMap<String, String> = HashMap::new();
    sources.insert(
        "loaded_as".into(),
        "module declared_as;
         entry fn ping() -> i64 { return 0; }
        ".into(),
    );
    sources.insert(
        "main".into(),
        "module main;
         fn main() -> i64 { return 0; }
        ".into(),
    );
    let kv = InMemoryKv::new();
    let err = Engine::new()
        .execute_main(&sources, "main", Fuel::new(1000), &kv)
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("declared") && msg.contains("loaded"), "got: {msg}");
}

#[test]
fn module_declaration_must_be_first() {
    // Anything before `module <name>;` makes it not the first item, which
    // the parser rejects so the declaration's role is unambiguous.
    let sources = vec![
        "state x: i64;
         module foo;
         entry fn ping() -> i64 { return 0; }
        ".to_string(),
        "module main;
         fn main() -> i64 { return foo::ping(); }
        ".to_string(),
    ];
    let kv = InMemoryKv::new();
    let err = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(1000), &kv)
        .unwrap_err();
    assert!(err.to_string().contains("first"), "got: {err}");
}

// ---------- example 25: NFT marketplace ----------

#[test]
fn example_25_nft_marketplace_runs() {
    let sources = read_dir("25_nft_marketplace");
    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(100_000), &kv)
        .unwrap();
    // alice paid nothing, received 250 from bob → 1250
    assert_eq!(out.result, Value::U64(1250));

    // The buy moved NFT #1 to bob.
    let owners_root = state_root("nft", "owners");
    let owner_cell = child(owners_root, &serialize(&Value::Int(1)));
    assert_eq!(out.writes.get(&owner_cell), Some(&Value::Address("0xb0b".into())));

    // The buy and the cancel both flipped `active` to false. The Listing
    // is one map-cell value; field-path writes go through read-modify-write
    // of that whole cell. After the trade, the cell should hold an inactive
    // Listing for #1 and #2.
    let listings_root = state_root("market", "listings");
    let l1 = child(listings_root, &serialize(&Value::Int(1)));
    let l2 = child(listings_root, &serialize(&Value::Int(2)));
    let active = |cell| match out.writes.get(cell) {
        Some(Value::Struct { fields, .. }) => fields
            .iter()
            .find(|(n, _)| n == "active")
            .map(|(_, v)| v.clone()),
        _ => None,
    };
    assert_eq!(active(&l1), Some(Value::Bool(false)));
    assert_eq!(active(&l2), Some(Value::Bool(false)));
}

#[test]
fn nft_trades_on_disjoint_listings_dont_conflict() {
    // Two buyers buying two different NFTs from two different sellers
    // should commit in parallel under OCC: their reads/writes touch
    // disjoint owners[id], disjoint balances[buyer], disjoint
    // balances[seller], and disjoint listings[id] cells.
    let mut kv = InMemoryKv::new();
    let setup_sources = read_setup_sources();
    let setup = "
        module main;
        fn main() -> i64 {
            let alice = address(\"0xa11ce\");
            let bob   = address(\"0xb0b\");
            let cara  = address(\"0xca7a\");
            let dave  = address(\"0xdade\");
            coin::mint(alice, 1000u64);
            coin::mint(cara,  1000u64);
            coin::mint(bob,    500u64);
            coin::mint(dave,   500u64);
            let id1 = nft::mint(alice);   // 1
            let id2 = nft::mint(cara);    // 2
            market::list(alice, id1, 250u64);
            market::list(cara,  id2, 300u64);
            return 0;
        }
    ".to_string();
    let mut all_setup = setup_sources.clone();
    all_setup.push(setup);
    let setup_out = Engine::new()
        .execute_modules(&all_setup, "main", Fuel::new(100_000), &kv)
        .unwrap();
    kv.apply(&setup_out.writes);

    // bob buys #1, dave buys #2 — disjoint everything.
    let buy_src = |buyer: &str, id: i64| {
        let mut srcs = setup_sources.clone();
        srcs.push(format!(
            "module main;
             fn main() -> bool {{
                 return market::buy(address(\"{buyer}\"), {id});
             }}",
        ));
        srcs
    };
    let bob_buys = buy_src("0xb0b",  1).join("\n");
    let dave_buys = buy_src("0xdade", 2).join("\n");
    // commit_batch wants flat strings, but it can only handle single-source
    // txs today. Drive them serially via execute_modules to assert they
    // produce disjoint write sets — the relevant invariant.
    let bob_out = Engine::new()
        .execute_modules(&buy_src("0xb0b", 1), "main", Fuel::new(100_000), &kv)
        .unwrap();
    let dave_out = Engine::new()
        .execute_modules(&buy_src("0xdade", 2), "main", Fuel::new(100_000), &kv)
        .unwrap();

    // Disjoint write sets: no key in both.
    let bob_keys: std::collections::HashSet<_> = bob_out.writes.keys().copied().collect();
    let dave_keys: std::collections::HashSet<_> = dave_out.writes.keys().copied().collect();
    let intersect: Vec<_> = bob_keys.intersection(&dave_keys).copied().collect();
    assert!(
        intersect.is_empty(),
        "disjoint trades should produce disjoint writes; overlap: {intersect:?}",
    );
    // …and disjoint reads.
    let bob_rkeys: std::collections::HashSet<_> = bob_out.reads.keys().copied().collect();
    let dave_rkeys: std::collections::HashSet<_> = dave_out.reads.keys().copied().collect();
    let r_intersect: Vec<_> = bob_rkeys.intersection(&dave_rkeys).copied().collect();
    assert!(
        r_intersect.is_empty(),
        "disjoint trades should read disjoint state; overlap: {r_intersect:?}",
    );
    let _ = (bob_buys, dave_buys); // silence unused warnings
}

fn read_setup_sources() -> Vec<String> {
    // The three module sources for example 25 (nft, coin, market) — main
    // is replaced per-test.
    let mut srcs = read_dir("25_nft_marketplace");
    srcs.retain(|s| !s.contains("module main;"));
    srcs
}

// commit_batch is generic over Error; we only need it for type inference
// in the suppression line. Provide a tiny shim:
#[allow(dead_code)]
fn occ_passthrough<'a>(eng: &'a Engine, kv: &mut InMemoryKv, txs: &[TxRequest<'a>]) {
    let _ = commit_batch(eng, kv, txs);
}

// ---------- example 26: lending market ----------

#[test]
fn example_26_lending_lifecycle_runs() {
    let sources = read_dir("26_lending");
    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(100_000), &kv)
        .unwrap();
    // After deposit + open + repay, alice is back to her original 100
    // collateral balance.
    assert_eq!(out.result, Value::U64(100));
}

#[test]
fn lending_open_rejected_when_undercollateralized() {
    let mut srcs = read_dir("26_lending");
    srcs.retain(|s| !s.contains("module main;"));
    srcs.push(
        "module main;
         fn main() -> i64 {
             let alice = address(\"0xa11ce\");
             let eth   = address(\"0xeth\");
             oracle::set_price(eth, 2000u64);
             vault::deposit(alice, 100u64);
             // Asking for 200000 against 100 ETH @ 2000 = 200000 value:
             // ratio is exactly 100%, below the 150% requirement → rejected.
             return loans::open(alice, eth, 100u64, 200000u64);
         }
        ".to_string(),
    );
    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute_modules(&srcs, "main", Fuel::new(100_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::Int(-1));
}

#[test]
fn lending_liquidation_seizes_collateral_below_threshold() {
    let mut srcs = read_dir("26_lending");
    srcs.retain(|s| !s.contains("module main;"));
    srcs.push(
        "module main;
         fn main() -> u64 {
             let alice  = address(\"0xa11ce\");
             let keeper = address(\"0xfee7\");
             let eth    = address(\"0xeth\");
             oracle::set_price(eth, 2000u64);
             vault::deposit(alice, 100u64);
             let id = loans::open(alice, eth, 100u64, 100000u64);
             // Price crashes from 2000 to 500 cents. New collateral value:
             // 100 * 500 = 50000 cents vs 100000 debt → 50% — way under
             // the 110% liquidation threshold.
             oracle::set_price(eth, 500u64);
             loans::liquidate(id, keeper);
             return vault::balance_of(keeper);
         }
        ".to_string(),
    );
    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute_modules(&srcs, "main", Fuel::new(100_000), &kv)
        .unwrap();
    // Keeper gets the 100 units of collateral.
    assert_eq!(out.result, Value::U64(100));
}

#[test]
fn lending_disjoint_borrowers_dont_conflict() {
    // Two borrowers, two different collateral assets, two different
    // loan ids → fully disjoint cells across all three modules.
    let mut kv = InMemoryKv::new();
    let mut module_srcs = read_dir("26_lending");
    module_srcs.retain(|s| !s.contains("module main;"));
    let setup = "
        module main;
        fn main() -> i64 {
            oracle::set_price(address(\"0xeth\"), 2000u64);
            oracle::set_price(address(\"0xbtc\"), 50000u64);
            vault::deposit(address(\"0xa11ce\"), 100u64);
            vault::deposit(address(\"0xb0b\"),    10u64);
            return 0;
        }
    ".to_string();
    let mut all_setup = module_srcs.clone();
    all_setup.push(setup);
    let setup_out = Engine::new()
        .execute_modules(&all_setup, "main", Fuel::new(100_000), &kv)
        .unwrap();
    kv.apply(&setup_out.writes);

    let alice_open = || {
        let mut s = module_srcs.clone();
        s.push(
            "module main;
             fn main() -> i64 {
                 return loans::open(address(\"0xa11ce\"), address(\"0xeth\"), 50u64, 50000u64);
             }
            ".to_string(),
        );
        s
    };
    let bob_open = || {
        let mut s = module_srcs.clone();
        s.push(
            "module main;
             fn main() -> i64 {
                 return loans::open(address(\"0xb0b\"), address(\"0xbtc\"), 5u64, 100000u64);
             }
            ".to_string(),
        );
        s
    };
    let a = Engine::new().execute_modules(&alice_open(), "main", Fuel::new(100_000), &kv).unwrap();
    let b = Engine::new().execute_modules(&bob_open(),   "main", Fuel::new(100_000), &kv).unwrap();
    let a_w: std::collections::HashSet<_> = a.writes.keys().copied().collect();
    let b_w: std::collections::HashSet<_> = b.writes.keys().copied().collect();

    // Per-user / per-asset cells are disjoint. The two contended cells
    // are `loans::next_id` (allocator) and the freshly-allocated
    // `loans::book[<id>]` — both txs naively pick id=1 from a shared
    // allocator, so they collide there. Everything else commutes.
    let alice_dep_cell = child(state_root("vault", "deposits"),
        &serialize(&Value::Address("0xa11ce".into())));
    let bob_dep_cell = child(state_root("vault", "deposits"),
        &serialize(&Value::Address("0xb0b".into())));
    assert!(a_w.contains(&alice_dep_cell), "alice tx must write her own deposit cell");
    assert!(b_w.contains(&bob_dep_cell),   "bob tx must write his own deposit cell");
    assert!(!a_w.contains(&bob_dep_cell),  "alice tx must not touch bob's deposit");
    assert!(!b_w.contains(&alice_dep_cell), "bob tx must not touch alice's deposit");

    let eth_price_cell = child(state_root("oracle", "prices"),
        &serialize(&Value::Address("0xeth".into())));
    let btc_price_cell = child(state_root("oracle", "prices"),
        &serialize(&Value::Address("0xbtc".into())));
    // Prices are reads, not writes — but they should land in disjoint
    // read sets too.
    let a_r: std::collections::HashSet<_> = a.reads.keys().copied().collect();
    let b_r: std::collections::HashSet<_> = b.reads.keys().copied().collect();
    assert!(a_r.contains(&eth_price_cell));
    assert!(b_r.contains(&btc_price_cell));
    assert!(!a_r.contains(&btc_price_cell));
    assert!(!b_r.contains(&eth_price_cell));

    // Confirm only the allocator and the freshly-claimed loan id collide.
    let next_id_root = state_root("loans", "next_id");
    let book_id1 = child(state_root("loans", "book"), &serialize(&Value::Int(1)));
    let mut overlap_w: Vec<_> = a_w.intersection(&b_w).copied().collect();
    overlap_w.sort();
    let mut expected = vec![next_id_root, book_id1];
    expected.sort();
    assert_eq!(overlap_w, expected);
}
