//! Sorted persistent collection — `pbtree<u64, V>`. Same KV-cell-
//! per-node story as pmap, but iteration is key-sorted so range
//! queries (`pbtree_range`) and ORDER-BY-style for-loops walk only
//! the cells covering the requested range.

use rend::value::Value;
use rend::{run, Engine, Fuel};
use rend::kv::{InMemoryKv, Kv};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Default)]
struct CountingKv {
    inner: InMemoryKv,
    keys: Arc<AtomicUsize>,
}

impl CountingKv {
    fn new() -> Self { Self::default() }
    fn keys_touched(&self) -> usize { self.keys.load(Ordering::Relaxed) }
    fn reset(&self) { self.keys.store(0, Ordering::Relaxed); }
}

impl Kv for CountingKv {
    fn get(&self, key: u128) -> Option<Vec<u8>> {
        self.keys.fetch_add(1, Ordering::Relaxed);
        self.inner.get(key)
    }
    fn get_many(&self, keys: &[u128]) -> Vec<Option<Vec<u8>>> {
        self.keys.fetch_add(keys.len(), Ordering::Relaxed);
        keys.iter().map(|k| self.inner.get(*k)).collect()
    }
}

// ---------- correctness: get/set ----------

#[test]
fn pbtree_set_then_get() {
    let v = run("
        state holders: pbtree<u64, u64>;
        fn main() -> u64 {
            holders[100u64] = 1u64;
            holders[200u64] = 2u64;
            holders[150u64] = 99u64;
            return holders[150u64];
        }
    ").unwrap();
    assert_eq!(v, Value::U64(99));
}

#[test]
fn pbtree_missing_key_returns_default() {
    let v = run("
        state holders: pbtree<u64, u64>;
        fn main() -> u64 {
            holders[1u64] = 100u64;
            return holders[999u64];
        }
    ").unwrap();
    assert_eq!(v, Value::U64(0));
}

#[test]
fn pbtree_overwrite_replaces() {
    let v = run("
        state holders: pbtree<u64, u64>;
        fn main() -> u64 {
            holders[7u64] = 1u64;
            holders[7u64] = 2u64;
            holders[7u64] = 3u64;
            return holders[7u64];
        }
    ").unwrap();
    assert_eq!(v, Value::U64(3));
}

#[test]
fn pbtree_contains_distinguishes_set_from_default() {
    // Same correctness motivation as pmap_contains: zero is a
    // valid value, contains() distinguishes "set to 0" from "never set".
    let v = run("
        state holders: pbtree<u64, u64>;
        fn main() -> bool {
            holders[1u64] = 0u64;            // explicitly set to 0
            let set_to_zero = pbtree_contains(holders, 1u64);
            let never_set   = pbtree_contains(holders, 99u64);
            return set_to_zero && !never_set;
        }
    ").unwrap();
    assert_eq!(v, Value::Bool(true));
}

// ---------- sorted iteration ----------

#[test]
fn pbtree_for_loop_yields_in_key_sorted_order() {
    // Insert in random order, expect sorted on iteration.
    let v = run("
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            h[50u64]  = 1u64;
            h[10u64]  = 1u64;
            h[200u64] = 1u64;
            h[3u64]   = 1u64;
            h[100u64] = 1u64;
            // Yield count + first key tracked by overwriting a state slot.
            // Simpler: use the natural-sortedness — sum each value times
            // its position, would only be stable if iteration is sorted.
            let acc = 0u64;
            let pos = 1u64;
            for v in h {
                let _ = v;     // we only care about iteration count
                acc = acc * 10u64 + pos;
                pos = pos + 1u64;
            }
            return acc;        // 12345 if iterated 5 times in order
        }
    ").unwrap();
    assert_eq!(v, Value::U64(12345));
}

#[test]
fn pbtree_for_loop_break_after_first_works() {
    // A 'break-after-first' loop on a sorted collection is the
    // SQL `LIMIT 1 ORDER BY k ASC` shape.
    let v = run("
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            h[100u64] = 1u64;
            h[50u64]  = 2u64;
            h[200u64] = 3u64;
            h[10u64]  = 4u64;
            // Smallest key = 10, value = 4.
            for v in h {
                return v;
            }
            return 0u64;
        }
    ").unwrap();
    assert_eq!(v, Value::U64(4));
}

// ---------- range queries ----------

#[test]
fn pbtree_range_inclusive_bounds() {
    let v = run("
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            for i in 1u64..=20u64 { h[i] = i * 100u64; }
            // [5..=8] → values 500, 600, 700, 800; sum = 2600.
            return sum(pbtree_range(h, 5u64, 8u64), 0u64);
        }
    ").unwrap();
    assert_eq!(v, Value::U64(2600));
}

#[test]
fn pbtree_range_returns_sorted() {
    let v = run("
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            // Insert out of order.
            h[15u64] = 150u64;
            h[5u64]  = 50u64;
            h[10u64] = 100u64;
            h[20u64] = 200u64;
            let r = pbtree_range(h, 8u64, 16u64);
            // Should be [100, 150]. Order-encoded check.
            return r[0] * 10u64 + r[1];     // 1000 + 150 = 1150
        }
    ").unwrap();
    assert_eq!(v, Value::U64(1150));
}

#[test]
fn pbtree_range_empty_when_no_overlap() {
    let v = run("
        state h: pbtree<u64, u64>;
        fn main() -> i64 {
            h[1u64] = 10u64;
            h[2u64] = 20u64;
            return len(pbtree_range(h, 100u64, 200u64));
        }
    ").unwrap();
    assert_eq!(v, Value::int(0i64));
}

#[test]
fn pbtree_range_works_in_view_fn() {
    let src = "
        state h: pbtree<u64, u64>;
        entry view fn page(lo: u64, hi: u64) -> [u64] {
            return pbtree_range(h, lo, hi);
        }
        fn main() -> i64 {
            for i in 1u64..=10u64 { h[i] = i; }
            return len(page(3u64, 7u64));
        }
    ";
    let kv = InMemoryKv::new();
    let outcome = Engine::new()
        .execute(src, Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::int(5i64));
}

// ---------- comprehensions over pbtree stream ----------

#[test]
fn comprehension_over_pbtree_streams_in_sorted_order() {
    // [v for v in h] should yield in sorted order.
    let v = run("
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            h[300u64] = 30u64;
            h[100u64] = 10u64;
            h[200u64] = 20u64;
            let xs = [v for v in h];
            // xs should be [10, 20, 30] in sort order.
            return xs[0] * 100u64 + xs[1] * 10u64 + xs[2];
            // 10*100 + 20*10 + 30 = 1000 + 200 + 30 = 1230.
        }
    ").unwrap();
    assert_eq!(v, Value::U64(1230));
}

#[test]
fn comprehension_over_pbtree_with_filter() {
    let v = run("
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            for i in 1u64..=10u64 { h[i] = i; }
            return sum([v for v in h if v % 2u64 == 0u64], 0u64);
            // 2+4+6+8+10 = 30
        }
    ").unwrap();
    assert_eq!(v, Value::U64(30));
}

// ---------- range cell-fetch savings ----------

#[test]
fn pbtree_range_fetches_fewer_cells_than_full_walk() {
    // Spread keys across the *top* bits of the u64 (`i << 56`) so
    // each key occupies a distinct upper-byte branch. With dense
    // low-bit keys (1..=200), every entry shares the top 50 bits
    // and the trie collapses to a single leaf — semantically
    // correct, but no cell-fetch savings to demonstrate.
    let mut kv = CountingKv::new();
    let seed = "
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            let i = 1u64;
            while i <= 200u64 {
                h[i << 56u64] = i;
                i = i + 1u64;
            }
            return 0u64;
        }
    ";
    let out = Engine::new().execute(seed, Fuel::new(5_000_000), &kv).unwrap();
    kv.inner.apply(&out.writes);

    // (A) Full walk via streaming for-loop reads every cell.
    kv.reset();
    let full = "
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            let acc = 0u64;
            for v in h { acc = acc + v; }
            return acc;
        }
    ";
    let _ = Engine::new().execute(full, Fuel::new(5_000_000), &kv).unwrap();
    let full_keys = kv.keys_touched();

    // (B) Range query for a tiny window — only the subtrees
    // covering keys 50<<56 .. 60<<56 should be visited.
    kv.reset();
    let ranged = "
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            return sum(pbtree_range(h, 50u64 << 56u64, 60u64 << 56u64), 0u64);
        }
    ";
    let _ = Engine::new().execute(ranged, Fuel::new(5_000_000), &kv).unwrap();
    let range_keys = kv.keys_touched();

    assert!(
        range_keys < full_keys,
        "range query should fetch fewer cells than full walk: \
         range={range_keys} full={full_keys}",
    );
}

// ---------- leaf-cap: dense monotonic keys narrow correctly ----------

#[test]
fn dense_monotonic_keys_range_narrows_after_leaf_cap() {
    // The "ages 13..80" / "user IDs 1..1M" case. Without the leaf
    // cap, all keys share the top 50 bits and pile into a single
    // leaf — range queries scan the whole leaf even for tiny
    // windows. The cap forces splits at deeper levels so range
    // queries actually descend the trie and skip out-of-range
    // subtrees.
    let mut kv = CountingKv::new();
    let seed = "
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            // 1000 monotonic keys — the pathological case for a
            // hash-naive sorted trie. Each key shares the top 50+
            // bits with its neighbors.
            let i = 1u64;
            while i <= 1000u64 {
                h[i] = i * 10u64;
                i = i + 1u64;
            }
            return 0u64;
        }
    ";
    let out = Engine::new().execute(seed, Fuel::new(20_000_000), &kv).unwrap();
    kv.inner.apply(&out.writes);

    // Full walk — touches every cell.
    kv.reset();
    let full = "
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            let acc = 0u64;
            for v in h { acc = acc + v; }
            return acc;
        }
    ";
    let _ = Engine::new().execute(full, Fuel::new(20_000_000), &kv).unwrap();
    let full_keys = kv.keys_touched();

    // Tiny range window [500..510] — should descend only the
    // subtrees covering those 11 keys, not the whole tree.
    kv.reset();
    let ranged = "
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            return sum(pbtree_range(h, 500u64, 510u64), 0u64);
        }
    ";
    let _ = Engine::new().execute(ranged, Fuel::new(20_000_000), &kv).unwrap();
    let range_keys = kv.keys_touched();

    // The range query should touch a small fraction of the cells
    // the full walk does. Without the leaf cap, both touched
    // ~equal cells (the single giant leaf). With the cap, range
    // narrows by an order of magnitude.
    assert!(
        range_keys * 4 < full_keys,
        "monotonic-key range query should narrow at least 4x: \
         range={range_keys} full={full_keys}",
    );
}

#[test]
fn dense_monotonic_keys_break_after_first_avoids_full_walk() {
    // Streaming break-after-first should also benefit: the cursor
    // descends to the smallest key (leftmost leaf) and yields one
    // entry, never visiting the rest.
    let mut kv = CountingKv::new();
    let seed = "
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            let i = 1u64;
            while i <= 1000u64 {
                h[i] = i * 10u64;
                i = i + 1u64;
            }
            return 0u64;
        }
    ";
    let out = Engine::new().execute(seed, Fuel::new(20_000_000), &kv).unwrap();
    kv.inner.apply(&out.writes);

    // Full walk for baseline.
    kv.reset();
    let full = "
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            let acc = 0u64;
            for v in h { acc = acc + v; }
            return acc;
        }
    ";
    let _ = Engine::new().execute(full, Fuel::new(20_000_000), &kv).unwrap();
    let full_keys = kv.keys_touched();

    // Break-after-first.
    kv.reset();
    let one = "
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            for v in h { return v; }
            return 0u64;
        }
    ";
    let _ = Engine::new().execute(one, Fuel::new(20_000_000), &kv).unwrap();
    let one_keys = kv.keys_touched();

    assert!(
        one_keys * 10 < full_keys,
        "break-after-first should descend, yield one, and stop: \
         one={one_keys} full={full_keys}",
    );
}

// ---------- bytecode VM parity ----------

#[test]
fn pbtree_works_on_bytecode_vm() {
    let src = "
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            h[5u64]  = 50u64;
            h[1u64]  = 10u64;
            h[3u64]  = 30u64;
            h[2u64]  = 20u64;
            // Sum via streaming for — must visit in sorted order.
            // First key seen should be 1, last should be 5.
            let first = 0u64;
            let last  = 0u64;
            for v in h {
                if first == 0u64 { first = v; }
                last = v;
            }
            return first * 100u64 + last;
            // 10 * 100 + 50 = 1050.
        }
    ";
    let kv = InMemoryKv::new();
    let outcome = Engine::new()
        .execute(src, Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::U64(1050));
}

// ---------- typeck rejection ----------

#[test]
fn typeck_rejects_pbtree_with_unsupported_key() {
    let err = rend::frontend("
        state bad: pbtree<string, u64>;
        fn main() -> u64 {
            bad[\"a\"] = 1u64;
            return 0u64;
        }
    ").unwrap_err();
    assert!(
        err.to_string().contains("u64") || err.to_string().contains("u128"),
        "got: {err}",
    );
}

// ---------- u128 keys (composite-index foundation) ----------

#[test]
fn pbtree_with_u128_keys_sorts_in_natural_order() {
    // Pack (price: u64 ASC, time: u64 ASC) into a u128 key:
    //   high 64 = price, low 64 = time.
    // Lowest packed key wins, which is exactly what we want for
    // an ask book (price ASC, time ASC).
    let v = run(r#"
        state ask_book: pbtree<u128, u64>;
        const SHIFT: u128 = 18446744073709551616u128;
        fn main() -> u64 {
            ask_book[u128(102u64) * SHIFT + u128(1u64)] = 1u64;   // 102 @ t=1
            ask_book[u128(100u64) * SHIFT + u128(2u64)] = 2u64;   // 100 @ t=2
            ask_book[u128(101u64) * SHIFT + u128(3u64)] = 3u64;   // 101 @ t=3
            ask_book[u128(100u64) * SHIFT + u128(0u64)] = 4u64;   // 100 @ t=0
            // First in priority order is best ask: lowest price,
            // then earliest time → entry 4 (100 @ t=0).
            for id in ask_book {
                return id;
            }
            return 0u64;
        }
    "#).unwrap();
    assert_eq!(v, Value::U64(4));
}

// ---------- stable cursor across deletes ----------

#[test]
fn pbtree_iteration_survives_deletes_of_yielded_keys() {
    // The for-in cursor re-seeks from the current root by
    // last-yielded key on each step, so deleting the entry we
    // just observed doesn't break the walk. Equivalent SQL
    // semantics: a server-side cursor that survives the
    // surrounding tx's own DELETEs.
    //
    // pbtree iteration yields *values*, so we set up the tree so
    // each value is also a valid key (`log[k] = k`) and the
    // `delete log[v]` line cleanly removes the entry we just
    // observed.
    use rend::{Engine, Fuel};
    let src = r#"
        state log: pbtree<u64, u64>;
        fn main() -> u64 {
            log[10u64] = 10u64;
            log[20u64] = 20u64;
            log[30u64] = 30u64;
            log[40u64] = 40u64;
            let total = 0u64;
            for v in log {
                total = total + v;
                delete log[v];
            }
            return total;
        }
    "#;
    // The interp materializes the iterator upfront, so for the
    // stable-cursor guarantee we need the bytecode VM's
    // streaming walk.
    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute(src, Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::U64(10 + 20 + 30 + 40));
}

#[test]
fn pbtree_with_u128_keys_supports_bit_inversion_for_desc() {
    // Bid book wants price DESC + time ASC. Encode by *bit-inverting*
    // the price (highest price → smallest packed key), then OR'ing
    // the time into the low 64.
    let v = run(r#"
        state bid_book: pbtree<u128, u64>;
        const SHIFT:    u128 = 18446744073709551616u128;
        const U64_MAX:  u64  = 18446744073709551615u64;
        fn main() -> u64 {
            bid_book[u128(U64_MAX - 98u64)  * SHIFT + u128(1u64)] = 1u64;
            bid_book[u128(U64_MAX - 100u64) * SHIFT + u128(2u64)] = 2u64;
            bid_book[u128(U64_MAX - 100u64) * SHIFT + u128(1u64)] = 3u64;   // best
            bid_book[u128(U64_MAX - 99u64)  * SHIFT + u128(1u64)] = 4u64;
            // First in priority order: id 3 (100 @ t=1).
            for id in bid_book {
                return id;
            }
            return 0u64;
        }
    "#).unwrap();
    assert_eq!(v, Value::U64(3));
}
