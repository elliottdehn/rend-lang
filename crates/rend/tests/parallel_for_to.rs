//! `parallel for <id> in <source> to <output> { body }` — dynamic-
//! dispatch parallel-for over a runtime-sized batch. Each iteration
//! runs in its own shadow Tx with `id` bound to `source[idx]`; the
//! body's tail expression writes to `output[idx]`. Output slots are
//! disjoint by construction (the dispatcher writes them sequentially
//! in stable order after the parallel section completes), so the
//! merge has nothing to reconcile on the buffer side.

use rend::value::Value;
use rend::{Engine, Fuel};

#[test]
fn parallel_for_to_writes_each_output_slot_from_body_tail() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module pfto;
        fn main() -> u64 {
            let source = [10u64, 20u64, 30u64, 40u64];
            let output = [0u64, 0u64, 0u64, 0u64];
            parallel for id in source to output {
                id * 3u64
            }
            return output[0i64] + output[1i64] + output[2i64] + output[3i64];
        }
    ";
    // (10 + 20 + 30 + 40) * 3 = 300
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(300));
}

#[test]
fn parallel_for_to_id_is_bound_per_leg() {
    // Each leg sees its own `id` — confirms the dispatcher patches
    // the per-leg register before invoking body bytecode.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module pfto;
        fn main() -> u64 {
            let source = [1u64, 2u64, 3u64, 4u64, 5u64];
            let output = [0u64, 0u64, 0u64, 0u64, 0u64];
            parallel for id in source to output {
                id * id
            }
            return output[0i64] + output[1i64] + output[2i64]
                 + output[3i64] + output[4i64];
        }
    ";
    // 1 + 4 + 9 + 16 + 25 = 55
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(55));
}

#[test]
fn parallel_for_to_body_can_read_outer_locals() {
    // Outer locals are visible to each leg as read-only snapshot.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module pfto;
        fn main() -> u64 {
            let multiplier = 7u64;
            let source = [1u64, 2u64, 3u64];
            let output = [0u64, 0u64, 0u64];
            parallel for id in source to output {
                id * multiplier
            }
            return output[0i64] + output[1i64] + output[2i64];
        }
    ";
    // (1 + 2 + 3) * 7 = 42
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(42));
}

#[test]
fn parallel_for_to_disjoint_state_writes_merge_without_conflict() {
    // Each leg writes a distinct pmap key. The shadow-Tx merge
    // pass folds the deltas in stable order; HAMT-disjoint per-key
    // writes don't conflict, so no re-run.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module pfto;
        state balances: pmap<u64, u64>;
        fn main() -> u64 {
            let ids = [1u64, 2u64, 3u64, 4u64];
            let out = [0u64, 0u64, 0u64, 0u64];
            parallel for id in ids to out {
                balances[id] = id * 100u64;
                id
            }
            return balances[1u64] + balances[2u64]
                 + balances[3u64] + balances[4u64];
        }
    ";
    // 100 + 200 + 300 + 400 = 1000
    let out = Engine::new().execute(src, Fuel::new(20_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(1000));
}

#[test]
fn parallel_for_to_runtime_n_works_for_dynamic_source_length() {
    // Source array's length is determined at runtime (built up
    // through other code paths). The dispatcher reads `.len()` at
    // dispatch time and spawns that many legs.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module pfto;
        entry fn build_ids() -> [u64] { return [5u64, 6u64, 7u64]; }
        fn main() -> u64 {
            let ids = build_ids();
            let out = [0u64, 0u64, 0u64];
            parallel for id in ids to out {
                id + 10u64
            }
            return out[0i64] + out[1i64] + out[2i64];
        }
    ";
    // (5+10) + (6+10) + (7+10) = 48
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(48));
}

#[test]
fn parallel_for_to_length_mismatch_is_runtime_error() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module pfto;
        fn main() -> u64 {
            let source = [1u64, 2u64, 3u64];
            let output = [0u64, 0u64];
            parallel for id in source to output {
                id
            }
            return output[0i64];
        }
    ";
    let err = Engine::new()
        .execute(src, Fuel::new(10_000), &kv)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("length mismatch"),
        "expected length-mismatch error, got: {msg}",
    );
}

#[test]
fn parallel_for_to_body_must_yield_output_element_type() {
    // Typeck rejects body whose tail type can't flow into output's
    // element slot.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module pfto;
        fn main() -> u64 {
            let source = [1u64, 2u64];
            let output = [0u64, 0u64];
            parallel for id in source to output {
                true
            }
            return output[0i64];
        }
    ";
    let err = Engine::new()
        .execute(src, Fuel::new(10_000), &kv)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("body must yield"),
        "expected body-yield-type error, got: {msg}",
    );
}

#[test]
fn parallel_for_to_source_must_be_u64_array() {
    // Slice 1 fixes the iteration type at u64 (the reserve-from-
    // counter story); other element types come in later slices.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module pfto;
        fn main() -> u64 {
            let source = [1i64, 2i64];
            let output = [0i64, 0i64];
            parallel for id in source to output {
                id
            }
            return 0u64;
        }
    ";
    let err = Engine::new()
        .execute(src, Fuel::new(10_000), &kv)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("must be `[u64]`") || msg.contains("must be [u64]"),
        "expected source-type error, got: {msg}",
    );
}

#[test]
fn reserve_atomically_bumps_state_and_yields_consecutive_ids() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module rs;
        state counter: u64;
        fn main() -> u64 {
            let a = reserve 3u64 from counter;
            let b = reserve 2u64 from counter;
            // a = [1,2,3], b = [4,5]. Counter now 5.
            return a[0i64] + a[1i64] + a[2i64]
                 + b[0i64] + b[1i64]
                 + counter;
        }
    ";
    // (1+2+3) + (4+5) + 5 = 20
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(20));
}

#[test]
fn reserve_rejects_non_u64_state() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module rs;
        state counter: i64;
        fn main() -> u64 {
            let _ = reserve 3u64 from counter;
            return 0u64;
        }
    ";
    let err = Engine::new()
        .execute(src, Fuel::new(10_000), &kv)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("state must be u64"), "got: {msg}");
}

#[test]
fn arr_t_n_allocates_default_filled_buffer() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module rs;
        fn main() -> u64 {
            let buf = arr<u64>[4u64];
            return buf[0i64] + buf[1i64] + buf[2i64] + buf[3i64];
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(1_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(0));
}

#[test]
fn arr_t_n_length_is_runtime_expression() {
    // The buffer length is a runtime expression, not a literal.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module rs;
        entry fn how_many() -> u64 { return 7u64; }
        fn main() -> u64 {
            let buf = arr<u64>[how_many()];
            // Sum element 6 (which is 0 by default) — works only if
            // buf has at least 7 slots.
            return buf[6i64];
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(2_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(0));
}

#[test]
fn continue_in_parallel_for_to_body_leaves_slot_at_default() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module rs;
        state counter: u64;
        fn main() -> u64 {
            let ids = reserve 5u64 from counter;
            let out = arr<u64>[5u64];
            parallel for id in ids to out {
                if id == 3u64 { continue; }
                id * 10u64
            }
            // ids = [1,2,3,4,5]; id=3 skipped → out[2]=0.
            // sum = 10+20+0+40+50 = 120.
            return out[0i64] + out[1i64] + out[2i64]
                 + out[3i64] + out[4i64];
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(20_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(120));
}

#[test]
fn continue_preserves_state_writes_made_before_the_skip() {
    // State writes that happen in the body BEFORE `continue`
    // still merge into the parent Tx — only the output slot is
    // skipped.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module rs;
        state counter: u64;
        state marks: pmap<u64, u64>;
        fn main() -> u64 {
            let ids = reserve 3u64 from counter;
            let out = arr<u64>[3u64];
            parallel for id in ids to out {
                marks[id] = id * 100u64;   // recorded before continue
                if id == 2u64 { continue; }
                id * 10u64
            }
            // out = [10, 0, 30] (id=2 slot skipped → 0)
            // marks = {1:100, 2:200, 3:300} — all writes persist
            return (out[0i64] + out[1i64] + out[2i64]) * 10000u64
                 + (marks[1u64] + marks[2u64] + marks[3u64]);
        }
    ";
    // out sum = 10+0+30 = 40 → 400_000
    // marks sum = 100+200+300 = 600
    // total = 400_600
    let out = Engine::new().execute(src, Fuel::new(20_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(400_600));
}

#[test]
fn continue_inside_nested_for_loop_still_jumps_to_inner_loop_top() {
    // `continue` inside a for-loop nested inside a parallel-for-to
    // body should jump to the inner loop's iteration top, not
    // skip the parallel slot.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module rs;
        state counter: u64;
        fn main() -> u64 {
            let ids = reserve 3u64 from counter;
            let out = arr<u64>[3u64];
            parallel for id in ids to out {
                let total = 0u64;
                for j in 0u64..5u64 {
                    if j == 2u64 { continue; }
                    total = total + j;
                }
                // j=0+1+3+4 = 8 (j=2 skipped via inner continue)
                total * id
            }
            // out = [8, 16, 24]; sum = 48
            return out[0i64] + out[1i64] + out[2i64];
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(48));
}

#[test]
fn parallel_for_to_keyword_does_not_break_to_as_parameter_name() {
    // `to` is a *contextual* keyword — only recognized in the
    // parallel-for production. Existing code using `to` as a
    // parameter or local name keeps parsing.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module pfto;
        entry fn double_to(to: u64) -> u64 { return to * 2u64; }
        fn main() -> u64 {
            return double_to(21u64);
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(1_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(42));
}
