//! Garbage-collection sweep for orphaned pmap/pvec node cells.
//!
//! The fundamental invariant under test: every cell reachable from
//! a live state survives the sweep, and every node cell that
//! *isn't* reachable from any live state gets removed.

use std::collections::HashSet;

use rend::ast::Type;
use rend::gc::{sweep, GcReport};
use rend::hashing::state_root;
use rend::kv::InMemoryKv;
use rend::value::Value;
use rend::{Engine, Fuel};

fn run_and_apply(src: &str, kv: &mut InMemoryKv) -> rend::engine::ExecOutcome {
    let out = Engine::new().execute(src, Fuel::new(200_000), kv as &InMemoryKv).unwrap();
    kv.apply(&out.writes);
    out
}

fn pmap_int_int_ty() -> Type {
    Type::PMap { key: Box::new(Type::Int), value: Box::new(Type::Int) }
}

fn pvec_int_ty() -> Type {
    Type::PVec { elem: Box::new(Type::Int) }
}

// ---------- pmap GC ----------

#[test]
fn gc_collects_orphaned_pmap_path_after_overwrite() {
    // Two transactions: the first builds a pmap with several keys.
    // The second overwrites one of them, which orphans the
    // previous root + the inner-path cells along that key. GC
    // should reclaim those orphans while preserving everything
    // reachable from the new root.
    let mut kv = InMemoryKv::new();

    let setup_src = "
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
    let setup = run_and_apply(setup_src, &mut kv);

    let overwrite_src = "
        state s: pmap<i64, i64>;
        fn main() -> i64 {
            s[7] = 999;
            return 0;
        }
    ";
    let overwrite = run_and_apply(overwrite_src, &mut kv);

    // Sweep candidates = every node cell either tx wrote.
    let mut candidates: HashSet<u128> = HashSet::new();
    candidates.extend(&setup.node_cells_written);
    candidates.extend(&overwrite.node_cells_written);
    let pre_count = kv.data.len();

    let s_cell = state_root("main", "s");
    let report = sweep(&[(s_cell, pmap_int_int_ty())], &candidates, &mut kv);

    assert!(
        report.swept > 0,
        "overwriting an existing pmap key must orphan at least the old path",
    );
    assert!(kv.data.len() < pre_count);

    // After GC, all keys must still be readable — the live root's
    // tree wasn't damaged.
    let read_back = "
        state s: pmap<i64, i64>;
        fn main() -> i64 {
            let total = 0;
            let i = 0;
            while i < 30 {
                total = total + s[i];
                i = i + 1;
            }
            return total;
        }
    ";
    let out = Engine::new().execute(read_back, Fuel::new(200_000), &kv).unwrap();
    // 0+1+...+29 = 435, but s[7] was overwritten to 999 (delta = 999 - 7 = 992).
    assert_eq!(out.result, Value::int(435 + 992i64));
}

#[test]
fn gc_no_op_after_single_insert() {
    // The minimal reachable shape: one tx, one insert. Every node
    // the tx wrote is on the live path, so GC has nothing to do.
    // (A multi-insert tx already orphans intermediate node states
    // — each new push rewrites the old root + path on top of the
    // previous one — which is itself a useful GC opportunity but
    // not what this test pins down.)
    let mut kv = InMemoryKv::new();
    let src = "
        state s: pmap<i64, i64>;
        fn main() -> i64 { s[7] = 42; return 0; }
    ";
    let outcome = run_and_apply(src, &mut kv);
    let pre = kv.data.len();

    let s_cell = state_root("main", "s");
    let report = sweep(&[(s_cell, pmap_int_int_ty())], &outcome.node_cells_written, &mut kv);
    assert_eq!(report.swept, 0, "no orphans after a single insert → no sweep");
    assert_eq!(kv.data.len(), pre);
}

#[test]
fn gc_reclaims_intra_tx_intermediate_cells() {
    // A single tx with N sequential inserts orphans the
    // intermediate roots/paths after each step — only the final
    // shape is reachable from the post-tx root. GC sweeps those
    // intermediates without affecting the live tree.
    let mut kv = InMemoryKv::new();
    let src = "
        state s: pmap<i64, i64>;
        fn main() -> i64 {
            let i = 0;
            while i < 20 {
                s[i] = i;
                i = i + 1;
            }
            return 0;
        }
    ";
    let outcome = run_and_apply(src, &mut kv);
    let pre = kv.data.len();

    let s_cell = state_root("main", "s");
    let report = sweep(&[(s_cell, pmap_int_int_ty())], &outcome.node_cells_written, &mut kv);
    assert!(
        report.swept > 0,
        "iterated inserts should leave some orphaned intermediate paths",
    );
    assert!(kv.data.len() < pre);

    // Live data must still be readable after sweep.
    let read = "
        state s: pmap<i64, i64>;
        fn main() -> i64 {
            let total = 0;
            let i = 0;
            while i < 20 {
                total = total + s[i];
                i = i + 1;
            }
            return total;
        }
    ";
    let out = Engine::new().execute(read, Fuel::new(200_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int((0..20i64).sum()));
}

// ---------- pvec GC ----------

#[test]
fn gc_collects_orphaned_pvec_path_after_set() {
    let mut kv = InMemoryKv::new();
    let setup_src = "
        state v: pvec<i64>;
        fn main() -> i64 {
            let i = 0;
            while i < 50 {
                pvec_push(v, i * 10);
                i = i + 1;
            }
            return 0;
        }
    ";
    let setup = run_and_apply(setup_src, &mut kv);

    let overwrite_src = "
        state v: pvec<i64>;
        fn main() -> i64 { v[20] = 9999; return 0; }
    ";
    let overwrite = run_and_apply(overwrite_src, &mut kv);

    let mut candidates: HashSet<u128> = HashSet::new();
    candidates.extend(&setup.node_cells_written);
    candidates.extend(&overwrite.node_cells_written);
    let pre = kv.data.len();

    let v_cell = state_root("main", "v");
    let report = sweep(&[(v_cell, pvec_int_ty())], &candidates, &mut kv);

    assert!(report.swept > 0, "overwriting an index orphans the prior path");
    assert!(kv.data.len() < pre);

    let read_back = "
        state v: pvec<i64>;
        fn main() -> i64 { return v[0] + v[20] + v[49]; }
    ";
    let out = Engine::new().execute(read_back, Fuel::new(200_000), &kv).unwrap();
    // v[0] = 0, v[20] = 9999 (overwritten), v[49] = 490
    assert_eq!(out.result, Value::int(0 + 9999 + 490i64));
}

// ---------- cross-state sharing ----------

#[test]
fn gc_preserves_cells_referenced_by_another_state() {
    // Two pmap states. After we orphan a tree-segment from state A,
    // state B may still reference some of those exact cells (because
    // content-addressing dedups identical subtrees). The GC's
    // reachability walk must see B's references and keep those
    // cells alive.
    let mut kv = InMemoryKv::new();

    let setup_src = "
        state a: pmap<i64, i64>;
        state b: pmap<i64, i64>;
        fn main() -> i64 {
            // Insert the same set into both maps. Their root hashes
            // will be identical, and every interior cell is shared.
            let i = 0;
            while i < 10 {
                a[i] = i;
                b[i] = i;
                i = i + 1;
            }
            return 0;
        }
    ";
    let setup = run_and_apply(setup_src, &mut kv);

    // Now mutate `a` in a way that orphans some of its cells from
    // a's path. Crucially, those same cells are still reachable
    // from `b` (which we haven't touched).
    let mutate_src = "
        state a: pmap<i64, i64>;
        state b: pmap<i64, i64>;
        fn main() -> i64 { a[3] = 999; return 0; }
    ";
    let mutate = run_and_apply(mutate_src, &mut kv);

    let mut candidates: HashSet<u128> = HashSet::new();
    candidates.extend(&setup.node_cells_written);
    candidates.extend(&mutate.node_cells_written);

    let a_cell = state_root("main", "a");
    let b_cell = state_root("main", "b");
    let _report = sweep(
        &[(a_cell, pmap_int_int_ty()), (b_cell, pmap_int_int_ty())],
        &candidates,
        &mut kv,
    );

    // b must still read cleanly after GC.
    let read_b = "
        state a: pmap<i64, i64>;
        state b: pmap<i64, i64>;
        fn main() -> i64 {
            let total = 0;
            let i = 0;
            while i < 10 {
                total = total + b[i];
                i = i + 1;
            }
            return total;
        }
    ";
    let out = Engine::new().execute(read_b, Fuel::new(200_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int((0..10i64).sum()));

    // a should also still be coherent — only the modified cells were
    // candidates for sweep.
    let read_a = "
        state a: pmap<i64, i64>;
        state b: pmap<i64, i64>;
        fn main() -> i64 { return a[3] + a[5]; }
    ";
    let out = Engine::new().execute(read_a, Fuel::new(200_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(999 + 5i64));
}

#[test]
fn gc_report_counts_match_state() {
    // A simple sanity check that GcReport.kept reflects the live
    // set size and GcReport.swept reflects the actual sweep count.
    let mut kv = InMemoryKv::new();
    let src = "
        state s: pmap<i64, i64>;
        fn main() -> i64 {
            s[1] = 1;
            s[2] = 2;
            return 0;
        }
    ";
    let setup = run_and_apply(src, &mut kv);

    let overwrite = "
        state s: pmap<i64, i64>;
        fn main() -> i64 { s[1] = 999; return 0; }
    ";
    let mutate = run_and_apply(overwrite, &mut kv);

    let mut candidates: HashSet<u128> = HashSet::new();
    candidates.extend(&setup.node_cells_written);
    candidates.extend(&mutate.node_cells_written);
    let cells_before_sweep = kv.data.len();

    let s_cell = state_root("main", "s");
    let GcReport { kept, swept } = sweep(
        &[(s_cell, pmap_int_int_ty())], &candidates, &mut kv,
    );
    assert_eq!(kv.data.len(), cells_before_sweep - swept);
    // kept ≥ 1 (the state cell) plus whatever node cells survived.
    assert!(kept >= 1);
}
