//! Persistent vector — indexed-trie storage with O(log32 N) cell
//! touches per operation. Same OCC merge story as pmap for
//! disjoint indexed writes; push is a serial point because two
//! concurrent pushes both want the next index.

use rend::value::Value;
use rend::{run, Engine, Fuel};

// ---------- basic operations ----------

#[test]
fn pvec_push_and_get() {
    let v = run("
        state log: pvec<i64>;
        fn main() -> i64 {
            pvec_push(log, 10);
            pvec_push(log, 20);
            pvec_push(log, 30);
            return log[0] + log[1] + log[2];
        }
    ").unwrap();
    assert_eq!(v, Value::Int(60));
}

#[test]
fn pvec_push_returns_assigned_index() {
    let v = run("
        state log: pvec<i64>;
        fn main() -> u64 {
            let i0 = pvec_push(log, 10);
            let i1 = pvec_push(log, 20);
            let i2 = pvec_push(log, 30);
            // Indices should be 0, 1, 2.
            return i0 * 100u64 + i1 * 10u64 + i2;
        }
    ").unwrap();
    assert_eq!(v, Value::U64(12));
}

#[test]
fn pvec_set_overwrites_existing_index() {
    let v = run("
        state log: pvec<i64>;
        fn main() -> i64 {
            pvec_push(log, 1);
            pvec_push(log, 2);
            pvec_push(log, 3);
            log[1] = 999;
            return log[0] + log[1] + log[2];
        }
    ").unwrap();
    assert_eq!(v, Value::Int(1003));
}

#[test]
fn pvec_len_tracks_pushes() {
    let v = run("
        state log: pvec<i64>;
        fn main() -> u64 {
            pvec_push(log, 1);
            pvec_push(log, 2);
            pvec_push(log, 3);
            return pvec_len(log);
        }
    ").unwrap();
    assert_eq!(v, Value::U64(3));
}

#[test]
fn pvec_get_out_of_bounds_is_runtime_error() {
    let err = run("
        state log: pvec<i64>;
        fn main() -> i64 {
            pvec_push(log, 1);
            return log[5];
        }
    ").unwrap_err();
    assert!(err.to_string().contains("out of bounds"));
}

#[test]
fn pvec_set_out_of_bounds_is_runtime_error() {
    let err = run("
        state log: pvec<i64>;
        fn main() -> i64 {
            pvec_push(log, 1);
            log[5] = 999;
            return 0;
        }
    ").unwrap_err();
    assert!(err.to_string().contains("out of bounds"));
}

// ---------- depth growth (tree-spread proof) ----------

#[test]
fn pvec_grows_past_one_leaf() {
    // 100 elements forces the tree to grow to depth 1 (32^1 = 32 < 100).
    let v = run("
        state log: pvec<i64>;
        fn main() -> i64 {
            let i = 0;
            while i < 100 {
                pvec_push(log, i);
                i = i + 1;
            }
            return log[0] + log[50] + log[99];
        }
    ").unwrap();
    assert_eq!(v, Value::Int(0 + 50 + 99));
}

#[test]
fn pvec_persists_across_transactions() {
    let mut kv = rend::kv::InMemoryKv::new();
    let src1 = "
        state log: pvec<i64>;
        fn main() -> i64 {
            pvec_push(log, 100);
            pvec_push(log, 200);
            pvec_push(log, 300);
            return 0;
        }
    ";
    let out1 = Engine::new().execute(src1, Fuel::new(50_000), &kv).unwrap();
    kv.apply(&out1.writes);

    let src2 = "
        state log: pvec<i64>;
        fn main() -> i64 {
            return log[0] + log[1] + log[2];
        }
    ";
    let out = Engine::new().execute(src2, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(600));
}

// ---------- type-system enforcement ----------

#[test]
fn pvec_index_must_be_int() {
    let err = run(r#"
        state log: pvec<i64>;
        fn main() -> i64 {
            pvec_push(log, 1);
            return log["zero"];
        }
    "#).unwrap_err();
    assert!(err.to_string().contains("pvec index"));
}

#[test]
fn pvec_push_value_type_is_checked() {
    let err = run(r#"
        state log: pvec<u64>;
        fn main() -> u64 {
            return pvec_push(log, "not a u64");
        }
    "#).unwrap_err();
    assert!(err.to_string().contains("pvec_push"));
}

#[test]
fn pvec_len_on_non_pvec_is_compile_error() {
    let err = run("
        state s: map<i64, i64>;
        fn main() -> u64 { return pvec_len(s); }
    ").unwrap_err();
    assert!(err.to_string().contains("pvec state") || err.to_string().contains("pvec_len"));
}

// ---------- bytecode VM path ----------

#[test]
fn pvec_works_through_bytecode_vm() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state log: pvec<u64>;
        entry fn append(v: u64) -> u64 {
            return pvec_push(log, v);
        }
        fn main() -> u64 {
            append(7u64);
            append(11u64);
            append(13u64);
            return log[0] + log[1] + log[2] + pvec_len(log);
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(50_000), &kv).unwrap();
    // 7 + 11 + 13 + 3 = 34
    assert_eq!(out.result, Value::U64(34));
}

// ---------- showcase example ----------

#[test]
fn example_34_pvec_runs_end_to_end() {
    let kv = rend::kv::InMemoryKv::new();
    let src = std::fs::read_to_string("examples/34_pvec.rd").unwrap();
    let out = Engine::new().execute(&src, Fuel::new(100_000), &kv).unwrap();
    // 100 (alice) + 300 (carol); bob's 200 flagged out.
    assert_eq!(out.result, Value::U64(400));
}

// ---------- OCC merge for disjoint indexed writes ----------

#[test]
fn parallel_disjoint_pvec_set_merges() {
    use rend::occ::{commit_batch, TxRequest};
    let engine = Engine::new();
    let mut kv = rend::kv::InMemoryKv::new();

    // First populate the pvec with three slots.
    let setup = "
        state log: pvec<i64>;
        fn main() -> i64 {
            pvec_push(log, 0);
            pvec_push(log, 0);
            pvec_push(log, 0);
            return 0;
        }
    ";
    let out = engine.execute(setup, Fuel::new(50_000), &kv).unwrap();
    kv.apply(&out.writes);

    // Two parallel txs each set a different index. Disjoint paths
    // → 3-way merge cleanly resolves the root-pointer conflict.
    let src_a = "
        state log: pvec<i64>;
        fn main() -> i64 { log[0] = 100; return 0; }
    ";
    let src_b = "
        state log: pvec<i64>;
        fn main() -> i64 { log[2] = 300; return 0; }
    ";

    let report = commit_batch(
        &engine,
        &mut kv,
        &[TxRequest::new(src_a, 50_000), TxRequest::new(src_b, 50_000)],
    )
    .unwrap();
    assert_eq!(report.conflicts, 0, "disjoint pvec sets must not re-execute");
    assert!(report.merged >= 1);

    let read_back = "
        state log: pvec<i64>;
        fn main() -> i64 { return log[0] + log[1] + log[2]; }
    ";
    let out = engine.execute(read_back, Fuel::new(50_000), &kv).unwrap();
    // 100 + 0 + 300 = 400
    assert_eq!(out.result, Value::Int(400));
}

#[test]
fn parallel_pvec_pushes_re_execute() {
    // Push contention is a genuine conflict: both txs want the
    // next index, so the merge bails and OCC re-executes.
    use rend::occ::{commit_batch, TxRequest};
    let engine = Engine::new();
    let mut kv = rend::kv::InMemoryKv::new();

    let src_a = "
        state log: pvec<i64>;
        fn main() -> i64 { pvec_push(log, 100); return 0; }
    ";
    let src_b = "
        state log: pvec<i64>;
        fn main() -> i64 { pvec_push(log, 200); return 0; }
    ";

    let report = commit_batch(
        &engine,
        &mut kv,
        &[TxRequest::new(src_a, 50_000), TxRequest::new(src_b, 50_000)],
    )
    .unwrap();
    // The second tx must re-execute against the first's commit so
    // that both pushes end up at distinct indices.
    assert_eq!(report.conflicts, 1);

    // Both elements should be present.
    let read_back = "
        state log: pvec<i64>;
        fn main() -> u64 { return u64(log[0]) + u64(log[1]) + pvec_len(log); }
    ";
    let out = engine.execute(read_back, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(100 + 200 + 2));
}

#[test]
fn parallel_conflicting_set_falls_back_to_re_execute() {
    use rend::occ::{commit_batch, TxRequest};
    let engine = Engine::new();
    let mut kv = rend::kv::InMemoryKv::new();

    // Setup: vec with one slot.
    let setup = "
        state log: pvec<i64>;
        fn main() -> i64 { pvec_push(log, 0); return 0; }
    ";
    let out = engine.execute(setup, Fuel::new(50_000), &kv).unwrap();
    kv.apply(&out.writes);

    // Both txs overwrite the same index — merge can't choose.
    let src_a = "
        state log: pvec<i64>;
        fn main() -> i64 { log[0] = 100; return 0; }
    ";
    let src_b = "
        state log: pvec<i64>;
        fn main() -> i64 { log[0] = 200; return 0; }
    ";

    let report = commit_batch(
        &engine,
        &mut kv,
        &[TxRequest::new(src_a, 50_000), TxRequest::new(src_b, 50_000)],
    )
    .unwrap();
    assert_eq!(
        report.conflicts, 1,
        "same-index overwrites must re-execute since merge can't pick a winner",
    );
}
