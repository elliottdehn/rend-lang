//! Multi-artifact execution. A "deployed program" is an artifact
//! the host has previously persisted; "txs" are short-lived
//! artifacts that compile against a deployed program's entry
//! signatures and dispatch into them at runtime.

use rend::artifact::Artifact;
use rend::kv::InMemoryKv;
use rend::value::Value;
use rend::{Engine, Fuel};

const BANK_SRC: &str = "
    module bank;
    state balances: map<Address, u64>;
    state total: u64;
    entry fn deposit(who: Address, amount: u64) -> u64 {
        balances[who] = balances[who] + amount;
        total = total + amount;
        return balances[who];
    }
    entry fn withdraw(who: Address, amount: u64) -> u64 {
        assert(balances[who] >= amount);
        balances[who] = balances[who] - amount;
        total = total - amount;
        return balances[who];
    }
    entry fn balance_of(who: Address) -> u64 {
        return balances[who];
    }
    entry fn total_supply() -> u64 { return total; }
";

// ---------- compile-time validation ----------

#[test]
fn compile_tx_against_dep_typechecks_cross_calls() {
    let bank = Engine::new().compile(BANK_SRC).unwrap();
    let tx_src = "
        module main;
        fn main() -> u64 {
            let alice = address(\"alice\");
            bank::deposit(alice, 100u64);
            bank::deposit(alice, 50u64);
            return bank::balance_of(alice);
        }
    ";
    // Should compile cleanly: bank's entries are visible.
    let tx = Engine::new().compile_tx(tx_src, &[bank]).unwrap();
    assert_eq!(tx.modules.len(), 1);
    assert_eq!(tx.modules[0].name, "main");
}

#[test]
fn compile_tx_rejects_unknown_dep_module() {
    let tx_src = "
        module main;
        fn main() -> i64 { return ghost::unknown(); }
    ";
    let err = Engine::new().compile_tx(tx_src, &[]).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("ghost") || msg.contains("unknown function") || msg.contains("not loaded"),
        "expected unresolved-call error, got: {msg}",
    );
}

#[test]
fn compile_tx_rejects_wrong_arg_types() {
    let bank = Engine::new().compile(BANK_SRC).unwrap();
    let tx_src = "
        module main;
        fn main() -> u64 {
            // deposit takes (Address, u64), not (i64, i64).
            return bank::deposit(7, 100);
        }
    ";
    let err = Engine::new().compile_tx(tx_src, &[bank]).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("address")
            || err.to_string().to_lowercase().contains("expected"),
        "expected arg-type mismatch error, got: {err}",
    );
}

#[test]
fn compile_tx_rejects_module_name_collision_with_dep() {
    let bank = Engine::new().compile(BANK_SRC).unwrap();
    let tx_src = "
        module bank;
        fn main() -> i64 { return 0; }
    ";
    let err = Engine::new().compile_tx(tx_src, &[bank]).unwrap_err();
    assert!(err.to_string().contains("collides"));
}

// ---------- multi-tx execution against a deployed program ----------

#[test]
fn deployed_program_state_persists_across_txs() {
    let mut kv = InMemoryKv::new();
    let bank = Engine::new().compile(BANK_SRC).unwrap();

    // Tx 1: alice deposits 100.
    let tx1_src = "
        module main;
        fn main() -> u64 {
            return bank::deposit(address(\"alice\"), 100u64);
        }
    ";
    let tx1 = Engine::new().compile_tx(tx1_src, std::slice::from_ref(&bank)).unwrap();
    let out1 = Engine::new()
        .execute_tx(&tx1, std::slice::from_ref(&bank), Fuel::new(20_000), &kv)
        .unwrap();
    assert_eq!(out1.result, Value::U64(100));
    kv.apply(&out1.writes);

    // Tx 2: alice deposits another 50, bob deposits 30.
    let tx2_src = "
        module main;
        fn main() -> u64 {
            bank::deposit(address(\"alice\"), 50u64);
            bank::deposit(address(\"bob\"),   30u64);
            return bank::total_supply();
        }
    ";
    let tx2 = Engine::new().compile_tx(tx2_src, std::slice::from_ref(&bank)).unwrap();
    let out2 = Engine::new()
        .execute_tx(&tx2, std::slice::from_ref(&bank), Fuel::new(20_000), &kv)
        .unwrap();
    // 100 (alice from tx1) + 50 (alice tx2) + 30 (bob) = 180
    assert_eq!(out2.result, Value::U64(180));
    kv.apply(&out2.writes);

    // Tx 3: alice withdraws 70 — should leave her at 80.
    let tx3_src = "
        module main;
        fn main() -> u64 {
            return bank::withdraw(address(\"alice\"), 70u64);
        }
    ";
    let tx3 = Engine::new().compile_tx(tx3_src, std::slice::from_ref(&bank)).unwrap();
    let out3 = Engine::new()
        .execute_tx(&tx3, std::slice::from_ref(&bank), Fuel::new(20_000), &kv)
        .unwrap();
    assert_eq!(out3.result, Value::U64(80));
}

#[test]
fn tx_artifact_round_trips_through_bytes() {
    // The whole point of separately-compiled tx artifacts is that
    // they can ship over the wire. Round-trip a tx through bytes
    // and run it.
    let bank = Engine::new().compile(BANK_SRC).unwrap();
    let tx_src = "
        module main;
        fn main() -> u64 {
            return bank::deposit(address(\"alice\"), 42u64);
        }
    ";
    let tx_a = Engine::new().compile_tx(tx_src, &[bank.clone()]).unwrap();
    let tx_b = Artifact::from_bytes(tx_a.bytes.clone()).unwrap();
    assert_eq!(tx_a.content_hash, tx_b.content_hash);

    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute_tx(&tx_b, &[bank], Fuel::new(20_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::U64(42));
}

#[test]
fn deployed_artifact_is_not_recompiled_per_tx() {
    // The deployed program compiles once; many txs reference the
    // same `bank` artifact. The content hash stays stable across
    // tx executions — important for caching, signing, etc.
    let bank = Engine::new().compile(BANK_SRC).unwrap();
    let bank_hash = bank.content_hash;

    for amount in [1u64, 7, 13, 99] {
        let tx_src = format!(
            "module main; fn main() -> u64 {{ return bank::deposit(address(\"a\"), {amount}u64); }}",
        );
        let tx = Engine::new().compile_tx(&tx_src, &[bank.clone()]).unwrap();
        let kv = InMemoryKv::new();
        let _ = Engine::new()
            .execute_tx(&tx, &[bank.clone()], Fuel::new(20_000), &kv)
            .unwrap();
    }
    // Bank's artifact hash didn't change.
    assert_eq!(bank.content_hash, bank_hash);
}

// ---------- multiple deployed deps ----------

#[test]
fn tx_can_call_into_multiple_deps() {
    let counter_src = "
        module counter;
        state n: i64;
        entry fn bump() -> i64 { n = n + 1; return n; }
        entry fn get() -> i64 { return n; }
    ";
    let logger_src = "
        module logger;
        state log_size: i64;
        entry fn log() -> i64 { log_size = log_size + 1; return log_size; }
    ";
    let counter = Engine::new().compile(counter_src).unwrap();
    let logger = Engine::new().compile(logger_src).unwrap();

    let tx_src = "
        module main;
        fn main() -> i64 {
            counter::bump();
            counter::bump();
            logger::log();
            return counter::get() * 100 + logger::log();
        }
    ";
    let tx = Engine::new().compile_tx(tx_src, &[counter.clone(), logger.clone()]).unwrap();
    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute_tx(&tx, &[counter, logger], Fuel::new(20_000), &kv)
        .unwrap();
    // counter.get() = 2, logger.log() = 2 (after bumping twice). 2*100 + 2 = 202.
    assert_eq!(out.result, Value::Int(202));
}

// ---------- deploy: optional constructor ----------

#[test]
fn deploy_runs_main_when_program_has_one() {
    // A deployed program with `main` uses it as a constructor —
    // sets initial state once, then never runs again.
    let mut kv = InMemoryKv::new();
    let token_src = "
        module token;
        state total: u64;
        state owner: Address;
        entry fn balance() -> u64 { return total; }
        entry fn transfer(_to: Address, amount: u64) -> u64 {
            total = total - amount;
            return total;
        }
        // Constructor: seed total supply and remember the owner.
        fn main() -> u64 {
            total = 1000000u64;
            owner = msg_sender();
            return total;
        }
    ";
    let token = Engine::new().compile(token_src).unwrap();
    let outcome = Engine::new().deploy(&token, Fuel::new(50_000), &kv).unwrap();
    let outcome = outcome.expect("deploy with `main` returns Some");
    assert_eq!(outcome.result, Value::U64(1_000_000));
    kv.apply(&outcome.writes);

    // After deploy, txs should see the constructor's state.
    let tx_src = "
        module main;
        fn main() -> u64 { return token::balance(); }
    ";
    let tx = Engine::new().compile_tx(tx_src, &[token.clone()]).unwrap();
    let out = Engine::new().execute_tx(&tx, &[token], Fuel::new(20_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(1_000_000));
}

#[test]
fn deploy_is_noop_when_program_has_no_main() {
    let kv = InMemoryKv::new();
    // Pure-library deployed program: just exposes entries, no
    // constructor. deploy() should return None.
    let lib_src = "
        module mathlib;
        entry fn add_one(x: i64) -> i64 { return x + 1; }
    ";
    let lib = Engine::new().compile(lib_src).unwrap();
    let outcome = Engine::new().deploy(&lib, Fuel::new(10_000), &kv).unwrap();
    assert!(outcome.is_none(), "deploy with no main is a no-op");

    // The library is still callable via tx.
    let tx_src = "
        module main;
        fn main() -> i64 { return mathlib::add_one(41); }
    ";
    let tx = Engine::new().compile_tx(tx_src, &[lib.clone()]).unwrap();
    let out = Engine::new().execute_tx(&tx, &[lib], Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(42));
}

#[test]
fn deploy_constructor_writes_become_initial_state() {
    // Two-stage flow: deploy seeds state, then a tx mutates it.
    // Verify that the tx sees the constructor's writes.
    let mut kv = InMemoryKv::new();
    let counter_src = "
        module counter;
        state n: i64;
        entry fn bump() -> i64 { n = n + 1; return n; }
        entry fn get() -> i64 { return n; }
        fn main() -> i64 { n = 100; return n; }   // constructor
    ";
    let counter = Engine::new().compile(counter_src).unwrap();
    let deploy_out = Engine::new()
        .deploy(&counter, Fuel::new(20_000), &kv)
        .unwrap()
        .expect("constructor present");
    kv.apply(&deploy_out.writes);

    let tx_src = "
        module main;
        fn main() -> i64 {
            counter::bump();
            return counter::get();
        }
    ";
    let tx = Engine::new().compile_tx(tx_src, &[counter.clone()]).unwrap();
    let out = Engine::new().execute_tx(&tx, &[counter], Fuel::new(20_000), &kv).unwrap();
    // Constructor set n=100; tx bumped once → n=101.
    assert_eq!(out.result, Value::Int(101));
}

#[test]
fn compile_tx_rejects_source_without_main() {
    let bank = Engine::new().compile(BANK_SRC).unwrap();
    let no_main_src = "
        module main;
        fn helper() -> i64 { return 0; }
    ";
    let err = Engine::new().compile_tx(no_main_src, &[bank]).unwrap_err();
    assert!(err.to_string().contains("main"));
}

#[test]
fn compile_tx_constructor_runs_with_msg_sender() {
    // Constructor receives a TxContext like any other execution —
    // useful for "owner" patterns where the deployer is recorded.
    let mut kv = InMemoryKv::new();
    let src = "
        module guarded;
        state owner: Address;
        entry fn current_owner() -> Address { return owner; }
        fn main() -> Address {
            owner = msg_sender();
            return owner;
        }
    ";
    let prog = Engine::new().compile(src).unwrap();
    let ctx = rend::tx::TxContext {
        sender: Value::Address("alice".to_string()),
        block_timestamp: 0,
        block_number: 0,
    };
    let _ = Engine::new()
        .deploy_with_context(&prog, ctx, Fuel::new(20_000), &kv)
        .unwrap()
        .expect("constructor runs");
    // (The deploy outcome's writes haven't been applied; check
    // its result directly.)
    // Re-deploy to a fresh kv just to assert the result.
    let kv2 = InMemoryKv::new();
    let ctx2 = rend::tx::TxContext {
        sender: Value::Address("alice".to_string()),
        block_timestamp: 0,
        block_number: 0,
    };
    let outcome = Engine::new()
        .deploy_with_context(&prog, ctx2, Fuel::new(20_000), &kv2)
        .unwrap()
        .unwrap();
    assert_eq!(outcome.result, Value::Address("alice".to_string()));

    // Now apply writes to the original kv and verify the entry
    // reads back the same owner.
    let _ = kv;
    let mut kv3 = InMemoryKv::new();
    kv3.apply(&outcome.writes);
    let tx_src = "
        module main;
        fn main() -> Address { return guarded::current_owner(); }
    ";
    let tx = Engine::new().compile_tx(tx_src, &[prog.clone()]).unwrap();
    let out = Engine::new().execute_tx(&tx, &[prog], Fuel::new(20_000), &kv3).unwrap();
    assert_eq!(out.result, Value::Address("alice".to_string()));
}

#[test]
fn execute_tx_rejects_dep_name_collision() {
    // Two deployed deps with the same module name → ambiguous,
    // would alias state cells. Reject at execute time.
    let a_src = "
        module shared;
        entry fn ping() -> i64 { return 1; }
    ";
    let b_src = "
        module shared;
        entry fn ping() -> i64 { return 2; }
    ";
    let dep_a = Engine::new().compile(a_src).unwrap();
    let dep_b = Engine::new().compile(b_src).unwrap();
    let tx_src = "
        module main;
        fn main() -> i64 { return shared::ping(); }
    ";
    let tx = Engine::new().compile_tx(tx_src, &[dep_a.clone()]).unwrap();
    let kv = InMemoryKv::new();
    let err = Engine::new()
        .execute_tx(&tx, &[dep_a, dep_b], Fuel::new(10_000), &kv)
        .unwrap_err();
    assert!(err.to_string().contains("duplicate module"));
}
