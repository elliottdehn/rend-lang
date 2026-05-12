//! `parallel { stmt; stmt; ... }` — statement-granularity parallel
//! execution. Each statement runs in its own shadow `Tx` under
//! rayon; deltas merge in stable declaration order with conflict
//! re-run. `let` bindings inside escape into the enclosing scope.
//! Intra-block references are rejected at typeck.

use rend::value::Value;
use rend::{Engine, Fuel};

#[test]
fn let_bindings_inside_parallel_outlive_the_block() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state a: i64;
        state b: i64;
        fn main() -> i64 {
            a = 10;
            b = 32;
            parallel {
                let x = a;
                let y = b;
            }
            return x + y;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(42i64));
}

#[test]
fn parallel_block_with_disjoint_state_writes_merges_cleanly() {
    // Each stmt writes a distinct cell; no read-set overlaps with
    // another's write-set → no re-run, deltas merge as-is.
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state x: i64;
        state y: i64;
        state z: i64;
        fn main() -> i64 {
            parallel {
                x = 1;
                y = 2;
                z = 4;
            }
            return x + y * 10 + z * 100;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(421i64));
}

#[test]
fn parallel_block_can_call_view_fns_in_parallel() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state inventory: pmap<i64, i64>;
        entry view fn stock_of(id: i64) -> i64 {
            return inventory[id];
        }
        fn main() -> i64 {
            inventory[1] = 100;
            inventory[2] = 250;
            inventory[3] = 7;
            parallel {
                let a = stock_of(1);
                let b = stock_of(2);
                let c = stock_of(3);
            }
            return a + b + c;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(357i64));
}

#[test]
fn intra_block_reference_is_a_compile_error() {
    // stmt 2 references `x` bound by stmt 1 — would secretly
    // serialize stmt 2 behind stmt 1. Typeck rejects.
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state s: i64;
        fn main() -> i64 {
            s = 5;
            parallel {
                let x = s;
                let y = x + 1;
            }
            return y;
        }
    ";
    let err = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("parallel"),
        "expected parallel-block ref error, got: {err}",
    );
}

#[test]
fn outer_locals_can_be_read_inside_parallel() {
    // Names from outer scope (let, params) are fair game — only
    // references to *other inner stmts'* names are forbidden.
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state base: pmap<i64, i64>;
        fn main() -> i64 {
            base[10] = 100;
            base[20] = 200;
            let key1 = 10;
            let key2 = 20;
            parallel {
                let a = base[key1];
                let b = base[key2];
            }
            return a + b;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(300i64));
}

#[test]
fn nested_parallel_blocks_are_rejected_at_parse() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        fn main() -> i64 {
            parallel {
                parallel {
                    let x = 1;
                }
            }
            return 0;
        }
    ";
    let err = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("nested")
            || err.to_string().to_lowercase().contains("parallel"),
        "expected nesting rejection, got: {err}",
    );
}

#[test]
fn control_flow_inside_parallel_is_rejected_at_parse() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        fn main() -> i64 {
            parallel {
                if true { let x = 1; } else { let x = 2; }
            }
            return 0;
        }
    ";
    let err = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("parallel"),
        "expected control-flow rejection, got: {err}",
    );
}

#[test]
fn parallel_block_writes_appear_in_outcome_writes() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state alice: u64;
        state bob: u64;
        fn main() -> u64 {
            parallel {
                alice = 100u64;
                bob   = 250u64;
            }
            return alice + bob;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(350));
    // Both writes ended up in the outcome write set.
    assert!(!out.writes.is_empty());
}
