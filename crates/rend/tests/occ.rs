//! Slice 8+9: OCC commit driver, parallel phase 1, u128 keys.

use rend::ast::Type;
use rend::hashing::{child, state_root};
use rend::kv::InMemoryKv;
use rend::occ::{commit_batch, TxRequest};
use rend::serialize::serialize;
use rend::value::Value;
use rend::Engine;

const VOTING_SRC: &str = "
    state votes: map<i64, i64>;
    state tally: map<i64, i64>;
    entry fn vote(voter: i64, choice: i64) -> bool {
        if votes[voter] != 0 { return false; }
        votes[voter] = 1;
        tally[choice] = tally[choice] + 1;
        return true;
    }
";

fn root(name: &str) -> u128 {
    state_root("main", name)
}

fn cell(map_name: &str, key: &Value) -> u128 {
    child(state_root("main", map_name), &serialize(key))
}

#[test]
fn disjoint_writes_commit_without_re_execution() {
    let engine = Engine::new();
    let mut kv = InMemoryKv::new();

    let src_a = "
        state x: i64;
        fn main() -> i64 { x = 7; return x; }
    ";
    let src_b = "
        state y: i64;
        fn main() -> i64 { y = 9; return y; }
    ";

    let report = commit_batch(
        &engine,
        &mut kv,
        &[TxRequest::new(src_a, 1000), TxRequest::new(src_b, 1000)],
    )
    .unwrap();

    assert_eq!(report.conflicts, 0);
    assert!(report.txs.iter().all(|t| !t.re_executed));
    assert_eq!(kv.get_typed(root("x"), &Type::Int), Some(Value::int(7i64)));
    assert_eq!(kv.get_typed(root("y"), &Type::Int), Some(Value::int(9i64)));
}

#[test]
fn conflicting_writes_force_re_execution() {
    let engine = Engine::new();
    let mut kv = InMemoryKv::new();
    let src = "
        state count: i64;
        fn main() -> i64 { count = count + 1; return count; }
    ";
    let report = commit_batch(
        &engine,
        &mut kv,
        &[TxRequest::new(src, 1000), TxRequest::new(src, 1000)],
    )
    .unwrap();

    assert_eq!(report.conflicts, 1);
    assert_eq!(report.txs[0].re_executed, false);
    assert_eq!(report.txs[1].re_executed, true);
    assert_eq!(report.txs[0].result, Value::int(1i64));
    assert_eq!(report.txs[1].result, Value::int(2i64));
    assert_eq!(kv.get_typed(root("count"), &Type::Int), Some(Value::int(2i64)));
}

#[test]
fn voting_disjoint_voters_disjoint_choices_no_conflicts() {
    let engine = Engine::new();
    let mut kv = InMemoryKv::new();
    let voter1 = format!("{VOTING_SRC} fn main() -> bool {{ return vote(1, 10); }}");
    let voter2 = format!("{VOTING_SRC} fn main() -> bool {{ return vote(2, 20); }}");

    let report = commit_batch(
        &engine,
        &mut kv,
        &[TxRequest::new(&voter1, 5000), TxRequest::new(&voter2, 5000)],
    )
    .unwrap();

    assert_eq!(report.conflicts, 0);
    assert_eq!(kv.get_typed(cell("votes", &Value::int(1i64)), &Type::Int), Some(Value::int(1i64)));
    assert_eq!(kv.get_typed(cell("votes", &Value::int(2i64)), &Type::Int), Some(Value::int(1i64)));
    assert_eq!(kv.get_typed(cell("tally", &Value::int(10i64)), &Type::Int), Some(Value::int(1i64)));
    assert_eq!(kv.get_typed(cell("tally", &Value::int(20i64)), &Type::Int), Some(Value::int(1i64)));
}

#[test]
fn voting_same_choice_conflicts_but_both_committed() {
    let engine = Engine::new();
    let mut kv = InMemoryKv::new();
    let voter1 = format!("{VOTING_SRC} fn main() -> bool {{ return vote(1, 7); }}");
    let voter2 = format!("{VOTING_SRC} fn main() -> bool {{ return vote(2, 7); }}");

    let report = commit_batch(
        &engine,
        &mut kv,
        &[TxRequest::new(&voter1, 5000), TxRequest::new(&voter2, 5000)],
    )
    .unwrap();

    assert_eq!(report.conflicts, 1);
    assert_eq!(report.txs[0].result, Value::Bool(true));
    assert_eq!(report.txs[1].result, Value::Bool(true));
    assert_eq!(kv.get_typed(cell("tally", &Value::int(7i64)), &Type::Int), Some(Value::int(2i64)));
}

#[test]
fn double_vote_is_rejected_after_re_execution() {
    let engine = Engine::new();
    let mut kv = InMemoryKv::new();
    let voter1 = format!("{VOTING_SRC} fn main() -> bool {{ return vote(1, 5); }}");
    let voter1_again = format!("{VOTING_SRC} fn main() -> bool {{ return vote(1, 5); }}");

    let report = commit_batch(
        &engine,
        &mut kv,
        &[TxRequest::new(&voter1, 5000), TxRequest::new(&voter1_again, 5000)],
    )
    .unwrap();

    assert_eq!(report.conflicts, 1);
    assert_eq!(report.txs[0].result, Value::Bool(true));
    assert_eq!(report.txs[1].result, Value::Bool(false));
    assert_eq!(kv.get_typed(cell("tally", &Value::int(5i64)), &Type::Int), Some(Value::int(1i64)));
}

#[test]
fn batch_result_matches_serial_execution() {
    let txs: Vec<String> = (1..=5)
        .map(|i| format!("{VOTING_SRC} fn main() -> bool {{ return vote({i}, 9); }}"))
        .collect();

    let engine = Engine::new();

    let mut kv_occ = InMemoryKv::new();
    let req: Vec<TxRequest> = txs.iter().map(|s| TxRequest::new(s, 5000)).collect();
    commit_batch(&engine, &mut kv_occ, &req).unwrap();

    let mut kv_serial = InMemoryKv::new();
    for s in &txs {
        let out = engine.execute(s, rend::Fuel::new(5000), &kv_serial).unwrap();
        kv_serial.apply(&out.writes);
    }

    assert_eq!(kv_occ.data, kv_serial.data);
    assert_eq!(
        kv_occ.get_typed(cell("tally", &Value::int(9i64)), &Type::Int),
        Some(Value::int(5i64)),
    );
}
