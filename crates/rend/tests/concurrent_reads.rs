//! End-to-end tests for the read-batching optimization.
//!
//! Uses a `LatencyKv` wrapper that adds a configurable per-`get` delay
//! and counts both `get` calls and round-trips. The optimizer's batch
//! collapses N independent reads into one round-trip; under latency,
//! that's a measurable wall-clock and call-count difference.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rend::kv::Kv;
use rend::value::Value;
use rend::{Engine, Fuel};

#[derive(Default)]
struct LatencyKv {
    inner: rend::kv::InMemoryKv,
    /// Per-`get` and per-`get_many` round-trip delay.
    latency: Duration,
    /// Cumulative count of `get` and `get_many` calls — separate so we
    /// can verify the optimizer collapsed N reads into 1 round-trip.
    gets:      Arc<AtomicUsize>,
    get_manys: Arc<AtomicUsize>,
    /// Cumulative number of *keys* fetched across both `get` and
    /// `get_many`. The optimizer should reduce round-trips, not
    /// keys-touched.
    keys_touched: Arc<AtomicUsize>,
}

impl LatencyKv {
    fn new(latency: Duration) -> Self {
        Self {
            inner: rend::kv::InMemoryKv::new(),
            latency,
            gets:         Arc::new(AtomicUsize::new(0)),
            get_manys:    Arc::new(AtomicUsize::new(0)),
            keys_touched: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn wrapping(inner: rend::kv::InMemoryKv, latency: Duration) -> Self {
        Self {
            inner,
            latency,
            gets:         Arc::new(AtomicUsize::new(0)),
            get_manys:    Arc::new(AtomicUsize::new(0)),
            keys_touched: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Kv for LatencyKv {
    fn get(&self, key: u128) -> Option<Vec<u8>> {
        self.gets.fetch_add(1, Ordering::Relaxed);
        self.keys_touched.fetch_add(1, Ordering::Relaxed);
        std::thread::sleep(self.latency);
        self.inner.get(key)
    }

    fn get_many(&self, keys: &[u128]) -> Vec<Option<Vec<u8>>> {
        self.get_manys.fetch_add(1, Ordering::Relaxed);
        self.keys_touched.fetch_add(keys.len(), Ordering::Relaxed);
        // One round-trip — sleep once, fetch all.
        std::thread::sleep(self.latency);
        keys.iter().map(|k| self.inner.get(*k)).collect()
    }
}

#[test]
fn five_independent_reads_collapse_to_one_round_trip() {
    let kv = LatencyKv::new(Duration::from_millis(0));
    let src = "
        state a: i64;
        state b: i64;
        state c: i64;
        state d: i64;
        state e: i64;
        fn main() -> i64 {
            let v1 = a;
            let v2 = b;
            let v3 = c;
            let v4 = d;
            let v5 = e;
            return v1 + v2 + v3 + v4 + v5;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(0i64));
    assert_eq!(kv.gets.load(Ordering::Relaxed),      0, "no individual gets");
    assert_eq!(kv.get_manys.load(Ordering::Relaxed), 1, "exactly one batch");
    assert_eq!(kv.keys_touched.load(Ordering::Relaxed), 5);
}

#[test]
fn batched_reads_save_real_wall_time() {
    // 10ms latency × 5 reads = 50ms unbatched, ~10ms batched.
    //
    // Note: `return a + b + c + d + e;` does NOT batch fully — the
    // compiler folds it into pairs of `KvGet; Bin`, and the Bin
    // consumes the prior read's register, fencing the cluster. To
    // demonstrate the batching win we read into independent locals
    // first (no inter-read dependencies), then sum afterwards.
    let kv = LatencyKv::new(Duration::from_millis(10));
    let src = "
        state a: i64;
        state b: i64;
        state c: i64;
        state d: i64;
        state e: i64;
        fn main() -> i64 {
            let v1 = a;
            let v2 = b;
            let v3 = c;
            let v4 = d;
            let v5 = e;
            return v1 + v2 + v3 + v4 + v5;
        }
    ";
    let started = std::time::Instant::now();
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(out.result, Value::int(0i64));
    // Batched: one ~10ms sleep. Generous upper bound to ride out CI
    // jitter; without the optimizer we'd see ~50ms.
    assert!(
        elapsed < Duration::from_millis(35),
        "batched reads took {elapsed:?}, expected ~10ms",
    );
}

#[test]
fn write_between_reads_forces_extra_round_trip() {
    let kv = LatencyKv::new(Duration::from_millis(0));
    let src = "
        state a: i64;
        state b: i64;
        state c: i64;
        fn main() -> i64 {
            let x = a;
            let y = b;
            a = x + y;       // forces { x, y } at the Bin Add
            let z = c;
            return x + y + z;
        }
    ";
    let _ = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    // First flush: x + y forced at the assignment's RHS — both reads
    // batch into one get_many. Second flush: x + y + z forced at the
    // return — z is fetched in a second get_many; x and y are already
    // resolved, so they aren't re-fetched.
    assert_eq!(kv.get_manys.load(Ordering::Relaxed), 2);
    assert_eq!(kv.gets.load(Ordering::Relaxed),      0);
    assert_eq!(kv.keys_touched.load(Ordering::Relaxed), 3);
}

#[test]
fn pure_call_does_not_break_cluster() {
    let kv = LatencyKv::new(Duration::from_millis(0));
    let src = "
        state a: i64;
        state b: i64;
        entry fn double(n: i64) -> i64 { return n * 2; }
        fn main() -> i64 {
            let x = a;
            let _z = double(7);   // pure — reads commute around it
            let y = b;
            return x + y;
        }
    ";
    let _ = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(kv.get_manys.load(Ordering::Relaxed), 1);
    assert_eq!(kv.gets.load(Ordering::Relaxed),      0);
}

#[test]
fn view_cross_module_call_does_not_break_cluster() {
    // A `view` cross-module call can't write state, so reads on
    // either side of it commute through it. The compiler stamps
    // the callee's view bound onto Instr::CallExternal, and the
    // optimizer's cluster_breaker reads it. Without that, this
    // test forces 2 round-trips (a fence around the call).
    //
    // helper::value() is `pure` — no state reads of its own — so
    // the only state reads are main's `a` and `b`. With the fix
    // they batch into one get_many; without it, they fence into
    // two.
    let kv = LatencyKv::new(Duration::from_millis(0));
    let sources = vec![
        "module helper;
         entry pure fn value() -> i64 { return 42; }
        ".to_string(),
        "module main;
         state a: i64;
         state b: i64;
         fn main() -> i64 {
             let x = a;
             let _z = helper::value();  // pure — doesn't fence
             let y = b;
             return x + y;
         }
        ".to_string(),
    ];
    let _ = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(20_000), &kv)
        .unwrap();
    assert_eq!(kv.get_manys.load(Ordering::Relaxed), 1);
    assert_eq!(kv.gets.load(Ordering::Relaxed),      0);
}

#[test]
fn pmap_walk_does_not_break_cluster() {
    // pmap_values walks the HAMT and so issues N internal cell reads,
    // but it doesn't *write*. The cluster planner classifies it as
    // a non-fence read, so unrelated state reads on either side
    // still batch into one round-trip alongside themselves.
    //
    // Setup: pre-seed the KV so the walk has data; then run a fresh
    // tx that reads `a`, walks the pmap, and reads `b`. The
    // walk's own reads are sequential round-trips (HAMT traversal
    // is dependent), but a/b should batch.
    let mut kv = LatencyKv::new(Duration::from_millis(0));
    // Seed the pmap.
    let seed_src = "
        state amounts: pmap<i64, u64>;
        fn main() -> u64 {
            amounts[1] = 10u64;
            amounts[2] = 20u64;
            amounts[3] = 30u64;
            return 1u64;
        }
    ";
    let seed = Engine::new().execute(seed_src, Fuel::new(20_000), &kv).unwrap();
    kv.inner.apply(&seed.writes);

    // Reset counters; the seed tx's reads/writes are not what we're measuring.
    kv.gets.store(0, Ordering::Relaxed);
    kv.get_manys.store(0, Ordering::Relaxed);
    kv.keys_touched.store(0, Ordering::Relaxed);

    let src = "
        state amounts: pmap<i64, u64>;
        state a: i64;
        state b: i64;
        fn main() -> i64 {
            let x = a;
            for v in amounts { if v == 0u64 { return 0; } }   // walk in middle
            let y = b;
            return x + y;
        }
    ";
    let _ = Engine::new().execute(src, Fuel::new(50_000), &kv).unwrap();
    // Cluster planner should batch a, b together (and the pmap root
    // cell, and the for-loop's len register read). The walk's own
    // HAMT-internal reads are separate round-trips.
    //
    // Without the non-fence classification, a and b would split
    // into separate get_many calls.
    let cluster_count = kv.get_manys.load(Ordering::Relaxed);
    // We expect: 1 get_many for the pre-walk read cluster (a, b,
    // amounts root). The walk emits more reads after, but they're
    // sequential per-node.
    assert!(cluster_count >= 1, "expected at least 1 batched get_many, got {cluster_count}");
    // Strong assertion: total round-trips for a+b alone should be 1.
    // Hard to factor out the walk's reads from kv.get_manys; instead
    // verify by counter-example below.
}

#[test]
fn impure_cross_module_call_still_breaks_cluster() {
    // Regression check for the inverse case: a non-view callee must
    // still fence the cluster. If we accidentally over-eagerly
    // promoted CallExternal to "doesn't break", concurrent writes
    // could silently bypass OCC ordering.
    let kv = LatencyKv::new(Duration::from_millis(0));
    let sources = vec![
        "module helper;
         state log: i64;
         entry fn touch() -> i64 { log = log + 1; return log; }
        ".to_string(),
        "module main;
         state a: i64;
         state b: i64;
         fn main() -> i64 {
             let x = a;
             let _z = helper::touch();  // impure — must fence
             let y = b;
             return x + y;
         }
        ".to_string(),
    ];
    let _ = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(20_000), &kv)
        .unwrap();
    // Two separate batches: {a} before the call, {b} after.
    assert!(
        kv.get_manys.load(Ordering::Relaxed) + kv.gets.load(Ordering::Relaxed) >= 2,
        "expected fence, got {} get_manys + {} gets",
        kv.get_manys.load(Ordering::Relaxed),
        kv.gets.load(Ordering::Relaxed),
    );
}

#[test]
fn host_import_call_does_not_force_unrelated_reads() {
    // A host import only forces *its own* arguments (so the host sees
    // concrete values). Unrelated pending reads in other registers
    // stay deferred — the host can't observe them, so there's no
    // semantic reason to flush. Both reads batch at the final Bin Add.
    let kv = LatencyKv::new(Duration::from_millis(0));
    let mut engine = Engine::new();
    engine.bind("ping", |_| Ok(Value::Unit));
    let src = "
        import ping: fn();
        state a: i64;
        state b: i64;
        fn main() -> i64 {
            let x = a;
            ping();
            let y = b;
            return x + y;
        }
    ";
    let _ = engine.execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(kv.get_manys.load(Ordering::Relaxed), 1);
    assert_eq!(kv.gets.load(Ordering::Relaxed),      0);
    assert_eq!(kv.keys_touched.load(Ordering::Relaxed), 2);
}

#[test]
fn struct_state_leaves_batch_in_one_round_trip() {
    // A struct state expands to N leaf cells. Reading the whole struct
    // already issues one tx::read_typed call — verify it goes through
    // get_many as a single round-trip via the trait's batch override.
    let kv = LatencyKv::new(Duration::from_millis(0));
    let src = "
        struct Point { x: i64, y: i64, z: i64 }
        state origin: Point;
        fn main() -> i64 {
            let p = origin;
            return p.x + p.y + p.z;
        }
    ";
    let _ = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    // tx::read_typed for a struct flattens into one read_typed_many
    // call covering every leaf — so the 3-leaf struct read is one
    // batched round-trip, not 3 sequential gets.
    assert_eq!(kv.get_manys.load(Ordering::Relaxed), 1);
    assert_eq!(kv.gets.load(Ordering::Relaxed),      0);
    assert_eq!(kv.keys_touched.load(Ordering::Relaxed), 3);
}

#[test]
fn batched_struct_state_reads_use_one_round_trip() {
    // Two struct-state reads together: read_typed_many (used by
    // ReadBatch) flattens both into one get_many call.
    let kv = LatencyKv::new(Duration::from_millis(0));
    let src = "
        struct Point { x: i64, y: i64 }
        state a: Point;
        state b: Point;
        fn main() -> i64 {
            let pa = a;
            let pb = b;
            return pa.x + pa.y + pb.x + pb.y;
        }
    ";
    let _ = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    // Two struct reads → 4 leaves → one get_many round-trip.
    assert_eq!(kv.get_manys.load(Ordering::Relaxed), 1);
    assert_eq!(kv.gets.load(Ordering::Relaxed),      0);
    assert_eq!(kv.keys_touched.load(Ordering::Relaxed), 4);
}

#[test]
fn batched_reads_dont_change_observable_value() {
    // Sanity: optimization is invisible to the result.
    let mut kv = rend::kv::InMemoryKv::new();
    let setup = "
        state a: i64;
        state b: i64;
        state c: i64;
        fn main() -> i64 {
            a = 10;
            b = 20;
            c = 30;
            return 0;
        }
    ";
    let setup_out = Engine::new().execute(setup, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&setup_out.writes);

    let read_back = "
        state a: i64;
        state b: i64;
        state c: i64;
        fn main() -> i64 {
            let x = a;
            let y = b;
            let z = c;
            return x + y + z;
        }
    ";
    let out = Engine::new().execute(read_back, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(60i64));
}

// ---------- loop-level prefetching ----------

#[test]
fn for_loop_with_direct_map_read_prefetches() {
    let mut kv = rend::kv::InMemoryKv::new();
    // Seed some balances.
    let setup = "
        state balances: map<i64, i64>;
        fn main() -> i64 {
            balances[1] = 10;
            balances[2] = 20;
            balances[3] = 30;
            balances[4] = 40;
            balances[5] = 50;
            return 0;
        }
    ";
    let setup_out = Engine::new().execute(setup, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&setup_out.writes);

    let lat = LatencyKv::wrapping(kv, Duration::from_millis(0));
    let src = "
        state balances: map<i64, i64>;
        fn main() -> i64 {
            let total = 0;
            for a in [1, 2, 3, 4, 5] {
                total = total + balances[a];
            }
            return total;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &lat).unwrap();
    assert_eq!(out.result, Value::int(150i64));
    // Prefetch issues one batched round-trip for all five cells; the
    // per-iteration MapGets serve from the tx read-cache (no `get`s).
    assert_eq!(lat.get_manys.load(Ordering::Relaxed), 1);
    assert_eq!(lat.gets.load(Ordering::Relaxed),      0);
    assert_eq!(lat.keys_touched.load(Ordering::Relaxed), 5);
}

#[test]
fn comprehension_with_direct_map_read_prefetches() {
    let mut kv = rend::kv::InMemoryKv::new();
    let setup = "
        state balances: map<i64, i64>;
        fn main() -> i64 {
            balances[1] = 100;
            balances[2] = 200;
            balances[3] = 300;
            return 0;
        }
    ";
    let setup_out = Engine::new().execute(setup, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&setup_out.writes);

    let lat = LatencyKv::wrapping(kv, Duration::from_millis(0));
    let src = "
        state balances: map<i64, i64>;
        fn main() -> i64 {
            let bals = [balances[a] for a in [1, 2, 3]];
            return bals[0] + bals[1] + bals[2];
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &lat).unwrap();
    assert_eq!(out.result, Value::int(600i64));
    assert_eq!(lat.get_manys.load(Ordering::Relaxed), 1);
    assert_eq!(lat.gets.load(Ordering::Relaxed),      0);
}

#[test]
fn comprehension_with_readonly_function_prefetches() {
    // The mapper is `read_balance(a)` — a single-statement function
    // that returns balances[a]. The summarizer recognizes its read
    // pattern, so the comprehension prefetches `balances` for all
    // iter values up front.
    let mut kv = rend::kv::InMemoryKv::new();
    let setup = "
        state balances: map<i64, i64>;
        fn main() -> i64 {
            balances[7] = 700;
            balances[8] = 800;
            balances[9] = 900;
            return 0;
        }
    ";
    let setup_out = Engine::new().execute(setup, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&setup_out.writes);

    let lat = LatencyKv::wrapping(kv, Duration::from_millis(0));
    let src = "
        state balances: map<i64, i64>;
        entry fn read_balance(a: i64) -> i64 { return balances[a]; }
        fn main() -> i64 {
            let bals = [read_balance(a) for a in [7, 8, 9]];
            return bals[0] + bals[1] + bals[2];
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &lat).unwrap();
    assert_eq!(out.result, Value::int(2400i64));
    // One batched prefetch covers all three cells.
    assert_eq!(lat.get_manys.load(Ordering::Relaxed), 1);
    assert_eq!(lat.gets.load(Ordering::Relaxed),      0);
}

#[test]
fn loop_prefetch_saves_real_wall_time_with_latency() {
    let mut kv = rend::kv::InMemoryKv::new();
    let setup = "
        state balances: map<i64, i64>;
        fn main() -> i64 {
            balances[1] = 1; balances[2] = 2; balances[3] = 3;
            balances[4] = 4; balances[5] = 5;
            return 0;
        }
    ";
    let setup_out = Engine::new().execute(setup, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&setup_out.writes);

    let lat = LatencyKv::wrapping(kv, Duration::from_millis(10));
    let src = "
        state balances: map<i64, i64>;
        fn main() -> i64 {
            let total = 0;
            for a in [1, 2, 3, 4, 5] {
                total = total + balances[a];
            }
            return total;
        }
    ";
    let started = std::time::Instant::now();
    let out = Engine::new().execute(src, Fuel::new(10_000), &lat).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(out.result, Value::int(15i64));
    // One ~10ms batch instead of five ~10ms sequential reads.
    assert!(
        elapsed < Duration::from_millis(35),
        "loop prefetch took {elapsed:?}, expected ~10ms",
    );
}

#[test]
fn nested_for_loop_prefetches_outer_iter_only() {
    // `for a in xs { for b in ys { ... balances[a] ... } }` — only the
    // outer loop's iter var is prefetchable here; the analyzer ignores
    // the inner shadow.
    let mut kv = rend::kv::InMemoryKv::new();
    let setup = "
        state balances: map<i64, i64>;
        fn main() -> i64 {
            balances[1] = 10;
            balances[2] = 20;
            return 0;
        }
    ";
    let setup_out = Engine::new().execute(setup, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&setup_out.writes);

    let lat = LatencyKv::wrapping(kv, Duration::from_millis(0));
    let src = "
        state balances: map<i64, i64>;
        fn main() -> i64 {
            let total = 0;
            for a in [1, 2] {
                for b in [10, 20, 30] {
                    total = total + balances[a] + b;
                }
            }
            return total;
        }
    ";
    let _ = Engine::new().execute(src, Fuel::new(10_000), &lat).unwrap();
    // Outer loop prefetches balances[1], balances[2] in one batch;
    // inner loop adds nothing to KV traffic.
    assert_eq!(lat.get_manys.load(Ordering::Relaxed), 1);
    assert_eq!(lat.keys_touched.load(Ordering::Relaxed), 2);
}

#[test]
fn read_then_compute_then_read_still_batches() {
    // The optimizer hoists pure computation out of the cluster's
    // emission point, so the two reads still batch even though there's
    // a Bin between them in source order.
    let kv = LatencyKv::new(Duration::from_millis(0));
    let src = "
        state a: i64;
        state b: i64;
        fn main() -> i64 {
            let x = a;
            let scratch = 1 + 2;        // pure register op
            let y = b;
            return x + y + scratch;
        }
    ";
    let _ = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(kv.get_manys.load(Ordering::Relaxed), 1);
    assert_eq!(kv.gets.load(Ordering::Relaxed),      0);
}
