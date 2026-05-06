//! End-to-end runs of the new flagship examples (token + DAO). The
//! tests check the final return value, the emitted event log, and a
//! few derived state cells so any regression in events / asserts /
//! granular state / cross-module composition surfaces here.

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

fn event_names(out: &rend::engine::ExecOutcome) -> Vec<String> {
    out.events
        .iter()
        .map(|e| format!("{}::{}", e.module, e.name))
        .collect()
}

// ---------- example 27: ERC20 token ----------

#[test]
fn token_full_lifecycle_returns_alice_balance() {
    let kv = rend::kv::InMemoryKv::new();
    let src = std::fs::read_to_string("examples/27_token.rd").unwrap();
    let out = Engine::new()
        .execute(&src, Fuel::new(50_000), &kv)
        .unwrap();
    // alice: 1000 mint − 300 transfer − 50 burn = 650
    assert_eq!(out.result, Value::U64(650));
}

#[test]
fn token_event_log_records_full_workflow() {
    let kv = rend::kv::InMemoryKv::new();
    let src = std::fs::read_to_string("examples/27_token.rd").unwrap();
    let out = Engine::new()
        .execute(&src, Fuel::new(50_000), &kv)
        .unwrap();

    // mint emits Mint+Transfer; transfer emits Transfer; approve emits
    // Approval; transfer_from emits Transfer; burn emits Burn+Transfer.
    let names = event_names(&out);
    assert_eq!(
        names,
        vec![
            "token::Mint",
            "token::Transfer",        // alice from-zero on mint
            "token::Transfer",        // alice → bob
            "token::Approval",        // bob → carol
            "token::Transfer",        // bob → carol via transfer_from
            "token::Burn",
            "token::Transfer",        // alice → zero on burn
        ],
    );
}

#[test]
fn token_burn_more_than_balance_aborts_with_assert() {
    let kv = rend::kv::InMemoryKv::new();
    // Modify the script: burn far more than alice ever holds.
    let src = r#"
        module token;
        struct Meta { name: string, symbol: string, decimals: u32 }
        struct AllowanceKey { owner: Address, spender: Address }
        state meta: Meta;
        state total_supply: u64;
        state balances: map<Address, u64>;
        state allowances: map<AllowanceKey, u64>;
        event Transfer(from: Address, to: Address, amount: u64);
        event Approval(owner: Address, spender: Address, amount: u64);
        event Mint(to: Address, amount: u64);
        event Burn(from: Address, amount: u64);
        entry fn mint(to: Address, amount: u64) -> bool {
            balances[to] = balances[to] + amount;
            total_supply = total_supply + amount;
            emit Mint(to, amount);
            return true;
        }
        entry fn burn(from: Address, amount: u64) -> bool {
            let b = balances[from];
            assert(b >= amount, "insufficient balance");
            balances[from] = b - amount;
            total_supply   = total_supply - amount;
            emit Burn(from, amount);
            return true;
        }
        fn main() -> u64 {
            let alice = address("0xa11ce");
            mint(alice, 100u64);
            burn(alice, 999u64);          // assertion failure
            return 0u64;
        }
    "#;
    let err = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap_err();
    assert!(
        err.to_string().contains("insufficient balance"),
        "expected balance assertion; got: {err}",
    );
}

#[test]
fn token_transfer_from_without_allowance_aborts() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        module token;
        struct AllowanceKey { owner: Address, spender: Address }
        state balances: map<Address, u64>;
        state allowances: map<AllowanceKey, u64>;
        entry fn mint(to: Address, amount: u64) {
            balances[to] = balances[to] + amount;
        }
        entry fn transfer_from(spender: Address, from: Address, to: Address, amount: u64) -> bool {
            let k = AllowanceKey { owner: from, spender: spender };
            let allowed = allowances[k];
            assert(allowed >= amount, "insufficient allowance");
            balances[from] = balances[from] - amount;
            balances[to]   = balances[to] + amount;
            return true;
        }
        fn main() -> u64 {
            let alice = address("0xa11ce");
            let bob   = address("0xb0b");
            mint(alice, 100u64);
            transfer_from(bob, alice, bob, 10u64);    // no approval ever granted
            return 0u64;
        }
    "#;
    let err = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap_err();
    assert!(err.to_string().contains("insufficient allowance"));
}

#[test]
fn token_approve_records_allowance_in_composite_key_cell() {
    use rend::hashing::{child, state_root};
    use rend::serialize::serialize;

    let kv = rend::kv::InMemoryKv::new();
    let src = std::fs::read_to_string("examples/27_token.rd").unwrap();
    let out = Engine::new().execute(&src, Fuel::new(50_000), &kv).unwrap();
    // The (bob, carol) approval was set to 100, then 60 was pulled,
    // leaving 40 in the cell.
    let key = Value::Struct {
        name: "AllowanceKey".into(),
        fields: vec![
            ("owner".into(), Value::Address("0xb0b".into())),
            ("spender".into(), Value::Address("0xca7a".into())),
        ],
    };
    let cell = child(state_root("token", "allowances"), &serialize(&key));
    assert_eq!(out.writes.get(&cell), Some(&Value::U64(40)));
}

// ---------- example 28: DAO ----------

#[test]
fn dao_proposal_passes_with_majority_yes() {
    let kv = rend::kv::InMemoryKv::new();
    let sources = read_dir("28_dao");
    let out = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(100_000), &kv)
        .unwrap();
    // alice (10) + bob (5) yes vs carol (3) no → 15 vs 3 → passes.
    assert_eq!(out.result, Value::Int(1));
}

#[test]
fn dao_event_log_threads_modules_in_emission_order() {
    let kv = rend::kv::InMemoryKv::new();
    let sources = read_dir("28_dao");
    let out = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(100_000), &kv)
        .unwrap();

    let names = event_names(&out);
    assert_eq!(
        names,
        vec![
            "members::Added",       // alice
            "members::Added",       // bob
            "members::Added",       // carol
            "proposals::Proposed",
            "proposals::Voted",     // alice
            "proposals::Voted",     // bob
            "proposals::Voted",     // carol
            "proposals::Executed",
        ],
    );
}

#[test]
fn dao_double_vote_is_rejected_with_assert() {
    let kv = rend::kv::InMemoryKv::new();
    let mut sources = read_dir("28_dao");
    sources.retain(|s| !s.contains("module main;"));
    sources.push(
        r#"
            module main;
            fn main() -> i64 {
                let alice = address("0xa11ce");
                members::add(alice, 5u64);
                let id = proposals::propose(alice, address("0xt"), 1u64);
                proposals::vote(id, alice, true);
                proposals::vote(id, alice, true);     // boom
                return 0;
            }
        "#.to_string(),
    );
    let err = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(100_000), &kv)
        .unwrap_err();
    assert!(err.to_string().contains("already voted"), "got: {err}");
}

#[test]
fn dao_non_member_cannot_propose() {
    let kv = rend::kv::InMemoryKv::new();
    let mut sources = read_dir("28_dao");
    sources.retain(|s| !s.contains("module main;"));
    sources.push(
        r#"
            module main;
            fn main() -> i64 {
                let stranger = address("0xff");
                proposals::propose(stranger, address("0xt"), 1u64);
                return 0;
            }
        "#.to_string(),
    );
    let err = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(100_000), &kv)
        .unwrap_err();
    assert!(err.to_string().contains("only members"), "got: {err}");
}

#[test]
fn dao_proposal_can_fail_when_majority_votes_no() {
    let kv = rend::kv::InMemoryKv::new();
    let mut sources = read_dir("28_dao");
    sources.retain(|s| !s.contains("module main;"));
    sources.push(
        r#"
            module main;
            fn main() -> i64 {
                let a = address("0xa");
                let b = address("0xb");
                members::add(a, 3u64);
                members::add(b, 9u64);
                let id = proposals::propose(a, address("0xt"), 1u64);
                proposals::vote(id, a, true);
                proposals::vote(id, b, false);    // 9 no > 3 yes
                let passed = proposals::execute(id);
                if passed { return 1; }
                return 0;
            }
        "#.to_string(),
    );
    let out = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(100_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::Int(0));

    // Executed event should report passed=false.
    let executed = out.events.iter().find(|e| e.name == "Executed").unwrap();
    // args: (id, passed, yes, no)
    assert_eq!(executed.args[1], Value::Bool(false));
    assert_eq!(executed.args[2], Value::U64(3));
    assert_eq!(executed.args[3], Value::U64(9));
}
