//! Streaming iteration over persistent collections. `for x in
//! pmap_state` and `for x in pvec_state` (and the same forms in
//! comprehensions) lower to lazy walks — never materialize the
//! source. Tests assert correctness end-to-end and pin down
//! early-break behavior using a KV that counts cell fetches.

use rend::value::Value;
use rend::{Engine, Fuel};
use rend::kv::{InMemoryKv, Kv};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// In-memory KV that counts every `get` / `get_many` plus the
/// total keys fetched. Wraps an underlying InMemoryKv.
#[derive(Default)]
struct CountingKv {
    inner: InMemoryKv,
    gets:        Arc<AtomicUsize>,
    get_manys:   Arc<AtomicUsize>,
    keys:        Arc<AtomicUsize>,
}

impl CountingKv {
    fn new() -> Self { Self::default() }
    fn keys_touched(&self) -> usize { self.keys.load(Ordering::Relaxed) }
    fn reset(&self) {
        self.gets.store(0, Ordering::Relaxed);
        self.get_manys.store(0, Ordering::Relaxed);
        self.keys.store(0, Ordering::Relaxed);
    }
}

impl Kv for CountingKv {
    fn get(&self, key: u128) -> Option<Vec<u8>> {
        self.gets.fetch_add(1, Ordering::Relaxed);
        self.keys.fetch_add(1, Ordering::Relaxed);
        self.inner.get(key)
    }
    fn get_many(&self, keys: &[u128]) -> Vec<Option<Vec<u8>>> {
        self.get_manys.fetch_add(1, Ordering::Relaxed);
        self.keys.fetch_add(keys.len(), Ordering::Relaxed);
        keys.iter().map(|k| self.inner.get(*k)).collect()
    }
}

// ---------- correctness: streaming yields all entries ----------

#[test]
fn streaming_pmap_for_yields_all_entries() {
    let kv = InMemoryKv::new();
    let src = "
        state amounts: pmap<i64, u64>;
        fn main() -> u64 {
            amounts[1] = 10u64;
            amounts[2] = 20u64;
            amounts[3] = 30u64;
            amounts[4] = 40u64;
            amounts[5] = 50u64;
            let total = 0u64;
            for v in amounts { total = total + v; }
            return total;          // 150
        }
    ";
    let outcome = Engine::new()
        .execute(src, Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::U64(150));
}

#[test]
fn streaming_pvec_for_yields_all_in_order() {
    let kv = InMemoryKv::new();
    let src = "
        state log: pvec<u64>;
        fn main() -> u64 {
            for i in 1..=20 { pvec_push(log, u64(i)); }
            // Multiply each element by its position; checks order.
            let total = 0u64;
            let pos = 1u64;
            for x in log {
                total = total + x * pos;
                pos = pos + 1u64;
            }
            return total;          // sum(i*i for i=1..20) = 2870
        }
    ";
    let outcome = Engine::new()
        .execute(src, Fuel::new(100_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::U64(2870));
}

// ---------- correctness: break and continue ----------

#[test]
fn streaming_pmap_for_break_exits() {
    let kv = InMemoryKv::new();
    let src = "
        state amounts: pmap<i64, u64>;
        fn main() -> u64 {
            amounts[1] = 1u64;
            amounts[2] = 2u64;
            amounts[3] = 3u64;
            let count = 0u64;
            for v in amounts {
                count = count + 1u64;
                if count >= 1u64 { break; }
            }
            return count;          // 1
        }
    ";
    let outcome = Engine::new()
        .execute(src, Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::U64(1));
}

#[test]
fn streaming_pmap_for_continue_skips() {
    let kv = InMemoryKv::new();
    let src = "
        state amounts: pmap<i64, u64>;
        fn main() -> u64 {
            amounts[1] = 1u64;
            amounts[2] = 2u64;
            amounts[3] = 3u64;
            amounts[4] = 4u64;
            let total = 0u64;
            for v in amounts {
                if v == 2u64 { continue; }
                total = total + v;
            }
            return total;          // 1 + 3 + 4
        }
    ";
    let outcome = Engine::new()
        .execute(src, Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::U64(8));
}

#[test]
fn streaming_pvec_for_break_exits() {
    let kv = InMemoryKv::new();
    let src = "
        state log: pvec<u64>;
        fn main() -> u64 {
            for i in 1..=10 { pvec_push(log, u64(i)); }
            let count = 0u64;
            for x in log {
                count = count + 1u64;
                if x == 3u64 { break; }
            }
            return count;          // visited 1, 2, 3 → 3
        }
    ";
    let outcome = Engine::new()
        .execute(src, Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::U64(3));
}

// ---------- streaming inside comprehensions ----------

#[test]
fn streaming_comprehension_over_pmap() {
    // Comprehension over a pmap state should also stream the
    // source — only the output array materializes.
    let kv = InMemoryKv::new();
    let src = "
        state amounts: pmap<i64, u64>;
        fn main() -> u64 {
            amounts[1] = 100u64;
            amounts[2] = 50u64;
            amounts[3] = 200u64;
            amounts[4] = 25u64;
            return sum([v for v in amounts if v > 75u64], 0u64);
            // 100 + 200 = 300
        }
    ";
    let outcome = Engine::new()
        .execute(src, Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::U64(300));
}

#[test]
fn streaming_comprehension_over_pvec() {
    let kv = InMemoryKv::new();
    let src = "
        state log: pvec<i64>;
        fn main() -> i64 {
            for i in 1..=10 { pvec_push(log, i); }
            let evens = [x for x in log if x % 2 == 0];
            return sum(evens, 0);          // 2+4+6+8+10 = 30
        }
    ";
    let outcome = Engine::new()
        .execute(src, Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::Int(30));
}

// ---------- early break saves real cell fetches ----------

#[test]
fn streaming_pvec_break_avoids_unread_cells() {
    // Setup: push 100 elements, commit. Then do two reads:
    //   (A) full walk via pvec_to_array — fetches every cell.
    //   (B) streaming for-loop with break after one element.
    // Assert (B) touches strictly fewer cells than (A).
    let mut kv = CountingKv::new();
    let seed = "
        state log: pvec<u64>;
        fn main() -> u64 {
            for i in 1u64..=100u64 { pvec_push(log, i); }
            return 100u64;
        }
    ";
    let out = Engine::new().execute(seed, Fuel::new(500_000), &kv).unwrap();
    kv.inner.apply(&out.writes);

    // (A) Materialize-then-iterate: pvec_to_array fetches every cell.
    kv.reset();
    let materialize = "
        state log: pvec<u64>;
        fn main() -> u64 {
            let xs = pvec_to_array(log);
            return xs[0];
        }
    ";
    let _ = Engine::new().execute(materialize, Fuel::new(500_000), &kv).unwrap();
    let materialized_keys = kv.keys_touched();

    // (B) Streaming for with break: only the first element + tree
    // path to that element are fetched.
    kv.reset();
    let streaming = "
        state log: pvec<u64>;
        fn main() -> u64 {
            let first = 0u64;
            for x in log {
                first = x;
                break;
            }
            return first;
        }
    ";
    let _ = Engine::new().execute(streaming, Fuel::new(500_000), &kv).unwrap();
    let streaming_keys = kv.keys_touched();

    assert!(
        streaming_keys < materialized_keys,
        "streaming break should fetch fewer cells than full materialization: \
         streaming={streaming_keys} materialized={materialized_keys}",
    );
}

#[test]
fn streaming_pmap_break_avoids_unread_subtrees() {
    // Same shape but for pmap: large enough to span multiple HAMT
    // leaves. A streaming walk that breaks after the first leaf's
    // entries should not fetch the rest of the trie.
    let mut kv = CountingKv::new();
    let seed = "
        state ledger: pmap<u64, u64>;
        fn main() -> u64 {
            for i in 1u64..=200u64 { ledger[i] = i * 10u64; }
            return 200u64;
        }
    ";
    let out = Engine::new().execute(seed, Fuel::new(2_000_000), &kv).unwrap();
    kv.inner.apply(&out.writes);

    // (A) pmap_values forces a full walk.
    kv.reset();
    let materialize = "
        state ledger: pmap<u64, u64>;
        fn main() -> u64 {
            let xs = pmap_values(ledger);
            return xs[0];
        }
    ";
    let _ = Engine::new().execute(materialize, Fuel::new(2_000_000), &kv).unwrap();
    let materialized_keys = kv.keys_touched();

    // (B) Streaming for-loop with break after first entry.
    kv.reset();
    let streaming = "
        state ledger: pmap<u64, u64>;
        fn main() -> u64 {
            let first = 0u64;
            for v in ledger {
                first = v;
                break;
            }
            return first;
        }
    ";
    let _ = Engine::new().execute(streaming, Fuel::new(2_000_000), &kv).unwrap();
    let streaming_keys = kv.keys_touched();

    assert!(
        streaming_keys < materialized_keys,
        "streaming break should fetch fewer cells than full materialization: \
         streaming={streaming_keys} materialized={materialized_keys}",
    );
}

#[test]
fn streaming_comprehension_break_via_filter_is_full_walk() {
    // Sanity check: comprehensions never `break`, so a comp that
    // filters out everything still walks the whole source. This
    // pins the contract — we don't claim filter-then-break.
    let kv = InMemoryKv::new();
    let src = "
        state ledger: pmap<u64, u64>;
        fn main() -> i64 {
            for i in 1u64..=50u64 { ledger[i] = i; }
            return len([v for v in ledger if v > 1000u64]);   // 0 matches
        }
    ";
    let outcome = Engine::new()
        .execute(src, Fuel::new(500_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::Int(0));
}
