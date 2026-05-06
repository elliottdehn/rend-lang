//! Runtime lazy reads: `Value::Pending` propagates through Move,
//! MakeArray, MakeStruct, FieldGet, ArrayGet, function calls (same-
//! module Call and CallExternal), and Return — until something
//! consumes a value (Bin/Un/comparison/JumpIfFalse/host call/Builtin),
//! at which point the runtime flushes the entire pending queue with
//! one `Kv::get_many` and substitutes the resolved values.
//!
//! These tests verify the laziness gives us batching across function
//! boundaries and across loop iterations *when the body doesn't
//! consume the read* — patterns the static optimizer alone can't see
//! through.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rend::kv::Kv;
use rend::value::Value;
use rend::{Engine, Fuel};

#[derive(Default)]
struct LatencyKv {
    inner: rend::kv::InMemoryKv,
    latency: Duration,
    gets:      Arc<AtomicUsize>,
    get_manys: Arc<AtomicUsize>,
    keys_touched: Arc<AtomicUsize>,
}

impl LatencyKv {
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
        std::thread::sleep(self.latency);
        keys.iter().map(|k| self.inner.get(*k)).collect()
    }
}

fn seed(src: &str, latency: Duration) -> LatencyKv {
    let mut base = rend::kv::InMemoryKv::new();
    let setup = Engine::new().execute(src, Fuel::new(50_000), &base).unwrap();
    base.apply(&setup.writes);
    LatencyKv::wrapping(base, latency)
}

// ---------- cross-call batching ----------

#[test]
fn comprehension_with_chained_readonly_calls_batches_to_one_round_trip() {
    let kv = seed(
        "
            state balances: map<i64, i64>;
            fn main() -> i64 {
                balances[1] = 10;
                balances[2] = 20;
                balances[3] = 30;
                return 0;
            }
        ",
        Duration::from_millis(0),
    );
    // Three layers of indirection — main calls a helper that calls
    // another helper that finally reads. Static prefetch can't see
    // through this chain, but lazy reads still defer.
    let src = "
        state balances: map<i64, i64>;
        entry fn level3(a: i64) -> i64 { return balances[a]; }
        entry fn level2(a: i64) -> i64 { return level3(a); }
        entry fn level1(a: i64) -> i64 { return level2(a); }
        fn main() -> i64 {
            let bals = [level1(a) for a in [1, 2, 3]];
            return bals[0] + bals[1] + bals[2];
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(60));
    // Every read flowed up through Pending; all three flushed in
    // one batch at `bals[0] + bals[1] + bals[2]`.
    assert_eq!(kv.get_manys.load(Ordering::Relaxed), 1);
    assert_eq!(kv.gets.load(Ordering::Relaxed),      0);
    assert_eq!(kv.keys_touched.load(Ordering::Relaxed), 3);
}

#[test]
fn call_returning_pending_propagates_to_caller() {
    // The callee returns a Pending; the caller doesn't consume it
    // until later. No flush at the call boundary.
    let kv = seed(
        "
            state n: i64;
            fn main() -> i64 { n = 42; return 0; }
        ",
        Duration::from_millis(0),
    );
    let src = "
        state n: i64;
        entry fn read_n() -> i64 { return n; }
        fn main() -> i64 {
            let a = read_n();
            let b = read_n();   // sees the cached read since same key
            return a + b;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(84));
    // Single batch — both lookups of n share one queued read by the
    // tx's read cache (second call hits writes/reads cache).
    assert!(
        kv.get_manys.load(Ordering::Relaxed) <= 1,
        "expected ≤1 batched fetch, got {}",
        kv.get_manys.load(Ordering::Relaxed),
    );
}

#[test]
fn pending_propagates_through_array_construction() {
    // Build an array of state reads. The MakeArray instruction
    // doesn't force its inputs (just copies them), so the array
    // contains Pending elements. Forcing the whole array at the sum
    // step flushes everyone in one round-trip.
    let kv = seed(
        "
            state a: i64;
            state b: i64;
            state c: i64;
            state d: i64;
            fn main() -> i64 {
                a = 1; b = 2; c = 3; d = 4;
                return 0;
            }
        ",
        Duration::from_millis(0),
    );
    let src = "
        state a: i64;
        state b: i64;
        state c: i64;
        state d: i64;
        fn main() -> i64 {
            let xs = [a, b, c, d];
            return xs[0] + xs[1] + xs[2] + xs[3];
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(10));
    // The static optimizer already clusters all four into one
    // ReadBatch at the array literal — so we expect one get_many.
    assert_eq!(kv.get_manys.load(Ordering::Relaxed), 1);
    assert_eq!(kv.gets.load(Ordering::Relaxed),      0);
}

#[test]
fn pending_through_struct_field_propagation() {
    let kv = seed(
        "
            state x: i64;
            state y: i64;
            fn main() -> i64 { x = 100; y = 200; return 0; }
        ",
        Duration::from_millis(0),
    );
    // Build a struct with two state-read fields, then access fields —
    // the struct's spine is concrete (Value::Struct), but each field
    // is Pending until consumed.
    let src = "
        struct Point { a: i64, b: i64 }
        state x: i64;
        state y: i64;
        fn main() -> i64 {
            let p = Point { a: x, b: y };
            return p.a + p.b;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(300));
    // Both reads queue, batch at the final Bin Add.
    assert_eq!(kv.get_manys.load(Ordering::Relaxed), 1);
}

#[test]
fn map_cell_read_is_lazy_and_batches_with_others() {
    // Mix a top-level state read with a map cell read in the same
    // expression. Both queue and flush together.
    let kv = seed(
        "
            state counter: i64;
            state balances: map<i64, i64>;
            fn main() -> i64 {
                counter = 7;
                balances[42] = 100;
                return 0;
            }
        ",
        Duration::from_millis(0),
    );
    let src = "
        state counter: i64;
        state balances: map<i64, i64>;
        fn main() -> i64 {
            let c = counter;
            let b = balances[42];
            return c + b;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(107));
    // Two reads, one batched flush at the Bin Add.
    assert_eq!(kv.get_manys.load(Ordering::Relaxed), 1);
    assert_eq!(kv.gets.load(Ordering::Relaxed),      0);
    assert_eq!(kv.keys_touched.load(Ordering::Relaxed), 2);
}

#[test]
fn pending_value_returned_from_topframe_is_forced_for_host() {
    // A program that returns a state value directly. The host
    // expects a concrete Value, not a Pending — `vm::run_world`
    // forces the result before yielding it.
    let kv = seed(
        "
            state n: i64;
            fn main() -> i64 { n = 99; return 0; }
        ",
        Duration::from_millis(0),
    );
    let src = "
        state n: i64;
        fn main() -> i64 { return n; }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(99));
    // The result must not be Pending — host wouldn't know how to
    // interpret it. No way to assert this directly, but if force
    // didn't run we'd have failed the equality above.
}

#[test]
fn unconsumed_reads_still_appear_in_read_set() {
    // OCC validation needs the full read set even for cells the
    // program "observed" but never actually used. End-of-program
    // flush_pending guarantees that.
    let kv = seed(
        "
            state a: i64;
            state b: i64;
            fn main() -> i64 { a = 1; b = 2; return 0; }
        ",
        Duration::from_millis(0),
    );
    let src = "
        state a: i64;
        state b: i64;
        fn main() -> i64 {
            let _x = a;
            let _y = b;
            return 0;     // never consumes x or y
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(0));
    // Both reads must still be in the OCC read set even though their
    // values went nowhere.
    let a_root = rend::hashing::state_root("main", "a");
    let b_root = rend::hashing::state_root("main", "b");
    assert!(out.reads.contains_key(&a_root));
    assert!(out.reads.contains_key(&b_root));
}

#[test]
fn cross_module_call_chain_batches_through_lazy_reads() {
    // Static prefetch can't see across module boundaries (no
    // cross-module summaries). Lazy reads carry pending values
    // across CallExternal boundaries naturally.
    let mut kv = rend::kv::InMemoryKv::new();
    let setup = "
        module main;
        fn main() -> i64 {
            ledger::seed();
            return 0;
        }
    ";
    let ledger_setup = "
        module ledger;
        state balances: map<Address, i64>;
        entry fn seed() {
            balances[address(\"a\")] = 11;
            balances[address(\"b\")] = 22;
            balances[address(\"c\")] = 33;
        }
        entry fn balance_of(who: Address) -> i64 {
            return balances[who];
        }
    ";
    let setup_out = Engine::new()
        .execute_modules(&[setup.to_string(), ledger_setup.to_string()], "main", Fuel::new(50_000), &kv)
        .unwrap();
    kv.apply(&setup_out.writes);

    let lat = LatencyKv::wrapping(kv, Duration::from_millis(0));
    let main_src = "
        module main;
        fn main() -> i64 {
            let xs = [
                ledger::balance_of(address(\"a\")),
                ledger::balance_of(address(\"b\")),
                ledger::balance_of(address(\"c\")),
            ];
            return xs[0] + xs[1] + xs[2];
        }
    ";
    let out = Engine::new()
        .execute_modules(
            &[main_src.to_string(), ledger_setup.to_string()],
            "main",
            Fuel::new(50_000),
            &lat,
        )
        .unwrap();
    assert_eq!(out.result, Value::Int(66));
    // All three cross-module calls returned Pending; flushed in one
    // batch at the array's first consumer.
    assert_eq!(lat.get_manys.load(Ordering::Relaxed), 1);
    assert_eq!(lat.gets.load(Ordering::Relaxed),      0);
    assert_eq!(lat.keys_touched.load(Ordering::Relaxed), 3);
}

#[test]
fn ten_independent_reads_via_calls_batch_to_one() {
    // Ten chained call sites, each producing one read. With lazy
    // reads + the static loop prefetch off (no `for` here), the
    // wins comes purely from runtime laziness.
    let kv = seed(
        "
            state v0: i64; state v1: i64; state v2: i64; state v3: i64;
            state v4: i64; state v5: i64; state v6: i64; state v7: i64;
            state v8: i64; state v9: i64;
            fn main() -> i64 {
                v0 = 1; v1 = 2; v2 = 3; v3 = 4; v4 = 5;
                v5 = 6; v6 = 7; v7 = 8; v8 = 9; v9 = 10;
                return 0;
            }
        ",
        Duration::from_millis(0),
    );
    let src = "
        state v0: i64; state v1: i64; state v2: i64; state v3: i64;
        state v4: i64; state v5: i64; state v6: i64; state v7: i64;
        state v8: i64; state v9: i64;
        entry fn r0() -> i64 { return v0; }
        entry fn r1() -> i64 { return v1; }
        entry fn r2() -> i64 { return v2; }
        entry fn r3() -> i64 { return v3; }
        entry fn r4() -> i64 { return v4; }
        entry fn r5() -> i64 { return v5; }
        entry fn r6() -> i64 { return v6; }
        entry fn r7() -> i64 { return v7; }
        entry fn r8() -> i64 { return v8; }
        entry fn r9() -> i64 { return v9; }
        fn main() -> i64 {
            let xs = [r0(), r1(), r2(), r3(), r4(), r5(), r6(), r7(), r8(), r9()];
            let sum = 0;
            for x in xs { sum = sum + x; }
            return sum;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(55));
    // The first iteration of the for-loop is the first consumer; one
    // flush covers all ten queued reads.
    assert_eq!(kv.get_manys.load(Ordering::Relaxed), 1);
    assert_eq!(kv.gets.load(Ordering::Relaxed),      0);
    assert_eq!(kv.keys_touched.load(Ordering::Relaxed), 10);
}

#[test]
fn aggregating_loop_still_needs_static_prefetch() {
    // Aggregating loops consume the read on every iteration — lazy
    // reads alone serialize them. The `for x in xs { total += x; }`
    // loop *with reads* needs static prefetch (which we already
    // emit). This test makes the contract explicit: when prefetch
    // applies, the loop runs in one round-trip; when it doesn't,
    // each iteration is a separate fetch.
    let kv = seed(
        "
            state balances: map<i64, i64>;
            fn main() -> i64 {
                balances[1] = 1; balances[2] = 2; balances[3] = 3;
                return 0;
            }
        ",
        Duration::from_millis(0),
    );
    let src = "
        state balances: map<i64, i64>;
        fn main() -> i64 {
            let total = 0;
            for a in [1, 2, 3] {
                total = total + balances[a];
            }
            return total;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(6));
    // Static prefetch warms balances[1..=3] in one batch before the
    // loop runs; in-loop MapGets serve from cache.
    assert_eq!(kv.get_manys.load(Ordering::Relaxed), 1);
    assert_eq!(kv.gets.load(Ordering::Relaxed),      0);
    assert_eq!(kv.keys_touched.load(Ordering::Relaxed), 3);
}
