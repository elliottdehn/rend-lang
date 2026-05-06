//! Persistent map (pmap<K, V>) — state-only, HAMT-backed, with a
//! `pmap_contains` builtin that distinguishes "explicitly set"
//! from "never set" (something `map<K,V>` cannot do).

use rend::value::Value;
use rend::{run, Engine, Fuel};

// ---------- basic indexing (interp path) ----------

#[test]
fn pmap_set_and_get_via_index_syntax() {
    let v = run("
        state balances: pmap<Address, u64>;
        fn main() -> u64 {
            let alice = address(\"alice\");
            balances[alice] = 42u64;
            return balances[alice];
        }
    ").unwrap();
    assert_eq!(v, Value::U64(42));
}

#[test]
fn pmap_default_for_missing_key() {
    let v = run("
        state balances: pmap<Address, u64>;
        fn main() -> u64 {
            let alice = address(\"alice\");
            return balances[alice];     // never written
        }
    ").unwrap();
    assert_eq!(v, Value::U64(0));
}

#[test]
fn pmap_overwrite_replaces_value() {
    let v = run("
        state balances: pmap<Address, u64>;
        fn main() -> u64 {
            let alice = address(\"alice\");
            balances[alice] = 1u64;
            balances[alice] = 999u64;
            return balances[alice];
        }
    ").unwrap();
    assert_eq!(v, Value::U64(999));
}

#[test]
fn pmap_with_int_keys() {
    let v = run("
        state scores: pmap<i64, i64>;
        fn main() -> i64 {
            scores[1] = 10;
            scores[2] = 20;
            scores[3] = 30;
            return scores[1] + scores[2] + scores[3];
        }
    ").unwrap();
    assert_eq!(v, Value::Int(60));
}

#[test]
fn pmap_with_string_keys() {
    let v = run(r#"
        state tags: pmap<string, i64>;
        fn main() -> i64 {
            tags["apple"]  = 1;
            tags["banana"] = 2;
            tags["cherry"] = 3;
            return tags["banana"];
        }
    "#).unwrap();
    assert_eq!(v, Value::Int(2));
}

// ---------- pmap_contains ----------

#[test]
fn pmap_contains_present_key() {
    let v = run("
        state s: pmap<i64, i64>;
        fn main() -> bool {
            s[7] = 42;
            return pmap_contains(s, 7);
        }
    ").unwrap();
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn pmap_contains_absent_key() {
    let v = run("
        state s: pmap<i64, i64>;
        fn main() -> bool {
            s[7] = 42;
            return pmap_contains(s, 999);
        }
    ").unwrap();
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn pmap_contains_distinguishes_explicit_default() {
    // `s[5] = 0` is distinct from "5 isn't there" — pmap_contains
    // returns true for an explicitly-set zero. (`map<K,V>` can't
    // tell these apart; that's a real win.)
    let v = run("
        state s: pmap<i64, i64>;
        fn main() -> bool {
            s[5] = 0;
            return pmap_contains(s, 5);
        }
    ").unwrap();
    assert_eq!(v, Value::Bool(true));
}

// ---------- type-system enforcement ----------

#[test]
fn pmap_key_type_mismatch_is_compile_error() {
    let err = run(r#"
        state s: pmap<Address, u64>;
        fn main() -> u64 {
            return s[42];     // 42 is i64, key type is Address
        }
    "#).unwrap_err();
    assert!(err.to_string().contains("pmap key") || err.to_string().contains("expected Address"));
}

#[test]
fn pmap_contains_on_non_pmap_is_compile_error() {
    let err = run("
        state s: map<i64, i64>;
        fn main() -> bool { return pmap_contains(s, 1); }
    ").unwrap_err();
    assert!(err.to_string().contains("pmap state") || err.to_string().contains("pmap_contains"));
}

// ---------- bytecode VM path ----------

#[test]
fn pmap_works_through_bytecode_vm() {
    // The interp path is exercised by `run`; the bytecode VM goes
    // through Engine. Run a small pmap workload there to make sure
    // the same operations produce identical results.
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state ledger: pmap<Address, u64>;
        entry fn deposit(who: Address, amount: u64) -> u64 {
            ledger[who] = ledger[who] + amount;
            return ledger[who];
        }
        fn main() -> u64 {
            let a = address(\"alice\");
            let b = address(\"bob\");
            deposit(a, 30u64);
            deposit(a, 12u64);
            deposit(b, 5u64);
            let alice_has = pmap_contains(ledger, a);
            let carol_has = pmap_contains(ledger, address(\"carol\"));
            // 47 + 1 (alice present) + 0 (carol absent) = 48
            let total = ledger[a] + ledger[b];
            let total = if alice_has { total + 1u64 } else { total };
            let total = if carol_has { total + 1u64 } else { total };
            return total;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(50_000), &kv).unwrap();
    // alice 42, bob 5 → 47, plus 1 because alice is present, 0 for carol
    assert_eq!(out.result, Value::U64(48));
}

// ---------- persistence across transactions ----------

#[test]
fn pmap_persists_across_transactions() {
    let mut kv = rend::kv::InMemoryKv::new();
    let src1 = "
        state ledger: pmap<i64, i64>;
        fn main() -> i64 {
            ledger[1] = 100;
            ledger[2] = 200;
            return 0;
        }
    ";
    let out1 = Engine::new().execute(src1, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&out1.writes);

    let src2 = "
        state ledger: pmap<i64, i64>;
        fn main() -> i64 {
            return ledger[1] + ledger[2];     // expect 300
        }
    ";
    let out = Engine::new().execute(src2, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(300));
}

// ---------- tree-spread storage ----------
//
// These tests examine the *shape* of the KV after a pmap operation,
// proving the tree is genuinely spread across many cells rather than
// stuffed into one. They're what makes "gigabytes of pmap" tractable.

#[test]
fn tree_spread_writes_multiple_node_cells() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state s: pmap<i64, i64>;
        fn main() -> i64 {
            // 50 keys force the tree to grow past a single leaf.
            // Each new node writes a cell; structural sharing means
            // the cell count grows roughly linearly with depth, not
            // with N.
            let i = 0;
            while i < 50 {
                s[i] = i * 7;
                i = i + 1;
            }
            return 0;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(500_000), &kv).unwrap();
    // The state-root cell holds the *root hash*, not the tree.
    // Plus we expect many node cells (interior + leaf).
    assert!(
        out.writes.len() > 5,
        "expected many node-cells written for a 50-entry pmap, got {}",
        out.writes.len(),
    );
}

#[test]
fn tree_spread_handles_large_pmap() {
    // The whole point of tree-spread: a pmap that would never fit
    // in memory if serialized whole still operates with constant
    // working-set. 1k entries here is plenty to exercise inner
    // node creation; the per-op cost remains O(log32 N).
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state s: pmap<i64, i64>;
        fn main() -> i64 {
            let i = 0;
            while i < 1000 {
                s[i] = i * 2;
                i = i + 1;
            }
            // Sample-check a few keys instead of asking for a count —
            // pmap doesn't maintain one (it would force every insert
            // to write a new root + serialize a conflict-prone size).
            return s[0] + s[500] + s[999];
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(0 + 1000 + 1998));
}

#[test]
fn tree_spread_disjoint_inserts_share_unmodified_subtrees() {
    // Insert into two disjoint key ranges in successive
    // transactions, and verify the second tx writes a tree whose
    // unmodified subtree cells come from the first tx — i.e., we
    // don't rewrite the entire tree on every update. (This is a
    // necessary precondition for the OCC win: per-tx writes are
    // path-shaped, not whole-tree-shaped.)
    let mut kv = rend::kv::InMemoryKv::new();
    let src1 = "
        state s: pmap<i64, i64>;
        fn main() -> i64 {
            let i = 0;
            while i < 30 {
                s[i] = i;
                i = i + 1;
            }
            return 0;
        }
    ";
    let out1 = Engine::new().execute(src1, Fuel::new(500_000), &kv).unwrap();
    let after_tx1 = out1.writes.len();
    kv.apply(&out1.writes);

    let src2 = "
        state s: pmap<i64, i64>;
        fn main() -> i64 {
            // Modify a single key. This walks one path of the tree;
            // only that path's nodes should be in the write set.
            s[15] = 999;
            return 0;
        }
    ";
    let out2 = Engine::new().execute(src2, Fuel::new(500_000), &kv).unwrap();
    // Single-key update must produce far fewer writes than the
    // initial 30-entry build. If it doesn't, the implementation is
    // copying the whole tree on each write — the OCC win evaporates.
    assert!(
        out2.writes.len() < after_tx1 / 2,
        "single-key update wrote {} cells; initial 30-key build wrote {} — \
         tree-spread isn't sharing unmodified subtrees",
        out2.writes.len(), after_tx1,
    );
}

#[test]
fn pmap_overwrite_reads_back_latest() {
    // Sanity check: repeated writes to the same keys leave the
    // most recent value in place. Used to track len here too, but
    // pmap intentionally doesn't maintain one — the size of an
    // O(log32 N) write would inflate to "rewrite the whole path
    // every time the count changes".
    let v = run("
        state s: pmap<i64, i64>;
        fn main() -> i64 {
            s[1] = 100;
            s[2] = 200;
            s[1] = 999;
            s[2] = 888;
            return s[1] + s[2];
        }
    ").unwrap();
    assert_eq!(v, Value::Int(999 + 888));
}

// ---------- CRDT-style merge in the OCC validator ----------
//
// The point of tree-spread storage is that two transactions writing
// disjoint subtrees touch disjoint cell sets — but the *root pointer*
// (the state cell holding the root hash) was still a write-write
// conflict. The OCC validator now does a 3-way HAMT merge on that
// pointer when the tree changes don't overlap, so disjoint inserts
// commit in parallel without re-execution.

#[test]
fn parallel_disjoint_inserts_merge_without_re_execution() {
    use rend::occ::{commit_batch, TxRequest};
    let engine = Engine::new();
    let mut kv = rend::kv::InMemoryKv::new();

    // Each tx inserts a single, disjoint key into the same pmap.
    // Without merge support, the second tx would see its read of
    // the root state cell mismatch the live value (post-tx-1) and
    // be forced to re-execute. With merge support, both commit.
    let src_a = "
        state s: pmap<i64, i64>;
        fn main() -> i64 { s[1] = 100; return 0; }
    ";
    let src_b = "
        state s: pmap<i64, i64>;
        fn main() -> i64 { s[2] = 200; return 0; }
    ";

    let report = commit_batch(
        &engine,
        &mut kv,
        &[TxRequest::new(src_a, 10_000), TxRequest::new(src_b, 10_000)],
    )
    .unwrap();

    assert_eq!(report.conflicts, 0, "disjoint inserts must not re-execute");
    assert!(
        report.merged >= 1,
        "expected at least one merge salvage, got {}",
        report.merged,
    );

    // Both keys must be visible in the final state.
    let read_back_src = "
        state s: pmap<i64, i64>;
        fn main() -> i64 { return s[1] + s[2]; }
    ";
    let out = Engine::new()
        .execute(read_back_src, Fuel::new(10_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::Int(300));
}

#[test]
fn many_parallel_disjoint_inserts_all_merge() {
    use rend::occ::{commit_batch, TxRequest};
    let engine = Engine::new();
    let mut kv = rend::kv::InMemoryKv::new();

    // Five txs, each inserting a different key. Every tx after the
    // first should hit the merge path; none should re-execute.
    let sources: Vec<String> = (1..=5)
        .map(|k| format!(
            "state s: pmap<i64, i64>;
             fn main() -> i64 {{ s[{k}] = {k} * 10; return 0; }}",
        ))
        .collect();
    let txs: Vec<TxRequest> = sources
        .iter()
        .map(|s| TxRequest::new(s.as_str(), 10_000))
        .collect();

    let report = commit_batch(&engine, &mut kv, &txs).unwrap();
    assert_eq!(report.conflicts, 0);
    assert!(report.txs.iter().all(|t| !t.re_executed));
    // First tx commits cleanly; the rest take the merge path.
    assert!(
        report.merged >= 4,
        "expected ≥4 merges across 5 disjoint inserts, got {}",
        report.merged,
    );

    // Confirm every key landed.
    for k in 1..=5i64 {
        let src = format!(
            "state s: pmap<i64, i64>;
             fn main() -> i64 {{ return s[{k}]; }}",
        );
        let out = Engine::new()
            .execute(&src, Fuel::new(10_000), &kv)
            .unwrap();
        assert_eq!(out.result, Value::Int(k * 10), "key {k} missing");
    }
}

#[test]
fn conflicting_overwrite_falls_back_to_re_execution() {
    use rend::occ::{commit_batch, TxRequest};
    let engine = Engine::new();
    let mut kv = rend::kv::InMemoryKv::new();

    // Both txs overwrite the same key to different values. The
    // 3-way merge can't choose; OCC must fall back to re-executing
    // the loser against the committed first write.
    let src_a = "
        state s: pmap<i64, i64>;
        fn main() -> i64 { s[1] = 100; return 0; }
    ";
    let src_b = "
        state s: pmap<i64, i64>;
        fn main() -> i64 { s[1] = 200; return 0; }
    ";

    let report = commit_batch(
        &engine,
        &mut kv,
        &[TxRequest::new(src_a, 10_000), TxRequest::new(src_b, 10_000)],
    )
    .unwrap();
    assert_eq!(
        report.conflicts, 1,
        "the conflicting overwrite must re-execute since merge can't resolve it",
    );
}

// ---------- showcase example ----------

#[test]
fn example_33_pmap_runs_end_to_end() {
    let kv = rend::kv::InMemoryKv::new();
    let src = std::fs::read_to_string("examples/33_pmap.rd").unwrap();
    let out = Engine::new().execute(&src, Fuel::new(50_000), &kv).unwrap();
    // alice credit 50 * 1000 + bob credit 50 + 1 (carol is a member)
    // = 50_051
    assert_eq!(out.result, Value::U64(50_051));
}

#[test]
fn pmap_contains_persists_across_transactions() {
    let mut kv = rend::kv::InMemoryKv::new();
    let src1 = "
        state ledger: pmap<i64, i64>;
        fn main() -> i64 {
            ledger[1] = 100;
            ledger[2] = 200;
            ledger[3] = 300;
            return 0;
        }
    ";
    let out1 = Engine::new().execute(src1, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&out1.writes);

    let src2 = "
        state ledger: pmap<i64, i64>;
        fn main() -> bool {
            return pmap_contains(ledger, 2) && !pmap_contains(ledger, 999);
        }
    ";
    let out = Engine::new().execute(src2, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Bool(true));
}
