//! `for ... in ... limit N { body }` — stop iteration after N
//! bodies have executed. The limit expression is evaluated once
//! before the loop; `break` / `continue` work as usual; `limit 0`
//! skips the body entirely. `limit` is a contextual keyword,
//! recognized only between the iter and the body block.

use rend::value::Value;
use rend::{Engine, Fuel};

#[test]
fn limit_caps_iteration_count_on_pvec_state() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module t;
        state log: pvec<u64>;
        fn main() -> u64 {
            pvec_push(log, 10u64);
            pvec_push(log, 20u64);
            pvec_push(log, 30u64);
            pvec_push(log, 40u64);
            pvec_push(log, 50u64);
            let total = 0u64;
            for x in log limit 3u64 {
                total = total + x;
            }
            return total;   // 10+20+30 = 60
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(20_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(60));
}

#[test]
fn limit_caps_iteration_count_on_array() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module t;
        fn main() -> u64 {
            let xs = [1u64, 2u64, 3u64, 4u64, 5u64];
            let total = 0u64;
            for x in xs limit 2u64 {
                total = total + x;
            }
            return total;   // 1+2 = 3
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(3));
}

#[test]
fn limit_zero_skips_body_entirely() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module t;
        fn main() -> u64 {
            let xs = [1u64, 2u64, 3u64];
            let total = 99u64;
            for x in xs limit 0u64 {
                total = total + x;
            }
            return total;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(5_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(99));
}

#[test]
fn limit_larger_than_collection_runs_full_loop() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module t;
        fn main() -> u64 {
            let xs = [1u64, 2u64, 3u64];
            let total = 0u64;
            for x in xs limit 100u64 {
                total = total + x;
            }
            return total;   // 1+2+3 = 6
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(5_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(6));
}

#[test]
fn limit_works_on_pbtree_streaming_iteration() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        module t;
        state book: pbtree<u64, u64>;
        fn main() -> u64 {
            book[1u64] = 10u64;
            book[2u64] = 20u64;
            book[3u64] = 30u64;
            book[4u64] = 40u64;
            let total = 0u64;
            for v in book limit 2u64 {
                total = total + v;
            }
            return total;   // 10+20 = 30 (sorted-key order)
        }
    "#;
    let out = Engine::new().execute(src, Fuel::new(20_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(30));
}

#[test]
fn limit_is_a_runtime_expression() {
    // The limit expression evaluates once before the loop.
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        module t;
        entry fn take(n: u64) -> u64 { return n; }
        fn main() -> u64 {
            let xs = [10u64, 20u64, 30u64, 40u64];
            let total = 0u64;
            for x in xs limit take(2u64) {
                total = total + x;
            }
            return total;   // 10+20 = 30
        }
    "#;
    let out = Engine::new().execute(src, Fuel::new(5_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(30));
}

#[test]
fn limit_can_be_a_local_variable() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module t;
        fn main() -> u64 {
            let n = 2u64;
            let xs = [10u64, 20u64, 30u64, 40u64];
            let total = 0u64;
            for x in xs limit n {
                total = total + x;
            }
            return total;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(5_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(30));
}

#[test]
fn break_inside_loop_works_with_limit_present() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module t;
        fn main() -> u64 {
            let xs = [1u64, 2u64, 3u64, 4u64, 5u64];
            let total = 0u64;
            for x in xs limit 4u64 {
                if x == 3u64 { break; }
                total = total + x;
            }
            return total;   // 1+2 = 3 (break before limit)
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(5_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(3));
}

#[test]
fn limit_keyword_does_not_break_limit_as_parameter_name() {
    // `limit` is a contextual keyword — code using it as a
    // parameter or local name keeps working.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module t;
        entry fn scale(limit: u64) -> u64 { return limit * 10u64; }
        fn main() -> u64 {
            return scale(5u64);   // 50
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(2_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(50));
}

#[test]
fn limit_rejects_non_u64_type() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module t;
        fn main() -> u64 {
            let xs = [1u64, 2u64];
            for x in xs limit 2 {
                let _ = x;
            }
            return 0u64;
        }
    ";
    let err = Engine::new()
        .execute(src, Fuel::new(2_000), &kv)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("requires `u64`"), "got: {msg}");
}
