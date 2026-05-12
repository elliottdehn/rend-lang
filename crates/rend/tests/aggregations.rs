//! sum / max / min over arrays. The init/default arg fixes the
//! result type — the typeck checks element types match init type;
//! the runtime returns the default when the array is empty
//! (max/min) or just init when sum starts the fold.

use rend::value::Value;
use rend::{run, Engine, Fuel};

// ---------- sum ----------

#[test]
fn sum_over_array_literal() {
    let v = run("fn main() -> i64 { return sum([1, 2, 3, 4, 5], 0); }").unwrap();
    assert_eq!(v, Value::int(15i64));
}

#[test]
fn sum_with_nonzero_init() {
    let v = run("fn main() -> i64 { return sum([10, 20, 30], 100); }").unwrap();
    assert_eq!(v, Value::int(160i64));
}

#[test]
fn sum_over_empty_returns_init() {
    let v = run("fn main() -> i64 {
        let xs: [i64] = [];
        return sum(xs, 42);
    }").unwrap();
    assert_eq!(v, Value::int(42i64));
}

#[test]
fn sum_over_u64_array() {
    let v = run("fn main() -> u64 {
        return sum([10u64, 20u64, 30u64], 0u64);
    }").unwrap();
    assert_eq!(v, Value::U64(60));
}

#[test]
fn sum_with_mismatched_init_type_is_compile_error() {
    // Element is i64, init is u64 — mismatch.
    let err = rend::frontend("fn main() -> i64 {
        return sum([1, 2, 3], 0u64);
    }").unwrap_err();
    assert!(
        err.to_string().contains("init") || err.to_string().contains("element"),
        "got: {err}",
    );
}

#[test]
fn sum_on_non_integer_is_compile_error() {
    // Strings are not summable.
    let err = rend::frontend("fn main() -> string {
        return sum([\"a\", \"b\"], \"\");
    }").unwrap_err();
    assert!(err.to_string().contains("integer"), "got: {err}");
}

#[test]
fn sum_over_pmap_values_directly() {
    // The SQL "SELECT SUM(amount) FROM ledger" shape.
    let v = run("
        state amounts: pmap<i64, u64>;
        fn main() -> u64 {
            amounts[1] = 100u64;
            amounts[2] = 200u64;
            amounts[3] = 50u64;
            return sum(pmap_values(amounts), 0u64);
        }
    ").unwrap();
    assert_eq!(v, Value::U64(350));
}

#[test]
fn sum_over_filtered_comprehension() {
    // "SELECT SUM(amount) FROM ledger WHERE amount > 50".
    let v = run("
        state amounts: pmap<i64, u64>;
        fn main() -> u64 {
            amounts[1] = 100u64;
            amounts[2] = 30u64;
            amounts[3] = 200u64;
            amounts[4] = 25u64;
            return sum([v for v in amounts if v > 50u64], 0u64);
        }
    ").unwrap();
    assert_eq!(v, Value::U64(300));
}

#[test]
fn sum_over_pvec_to_array() {
    let v = run("
        state log: pvec<i64>;
        fn main() -> i64 {
            for i in 1..=10 { pvec_push(log, i); }
            return sum(pvec_to_array(log), 0);
        }
    ").unwrap();
    assert_eq!(v, Value::int(55i64));
}

// ---------- max ----------

#[test]
fn max_over_array() {
    let v = run("fn main() -> i64 { return max([3, 7, 1, 9, 4], -1); }").unwrap();
    assert_eq!(v, Value::int(9i64));
}

#[test]
fn max_over_empty_returns_default() {
    let v = run("fn main() -> i64 {
        let xs: [i64] = [];
        return max(xs, -999);
    }").unwrap();
    assert_eq!(v, Value::int(-999i64));
}

#[test]
fn max_default_does_not_compete_with_elements() {
    // The default is only returned for empty. For non-empty, the
    // first element starts the fold — even if default is bigger.
    let v = run("fn main() -> i64 { return max([3, 5, 1], 100); }").unwrap();
    assert_eq!(v, Value::int(5i64));
}

#[test]
fn max_over_pmap_values() {
    let v = run("
        state amounts: pmap<i64, u64>;
        fn main() -> u64 {
            amounts[1] = 100u64;
            amounts[2] = 500u64;
            amounts[3] = 50u64;
            return max(pmap_values(amounts), 0u64);
        }
    ").unwrap();
    assert_eq!(v, Value::U64(500));
}

#[test]
fn max_with_negative_default_works() {
    let v = run("fn main() -> i64 { return max([-5, -10, -3], 0); }").unwrap();
    assert_eq!(v, Value::int(-3i64));
}

// ---------- min ----------

#[test]
fn min_over_array() {
    let v = run("fn main() -> i64 { return min([3, 7, 1, 9, 4], 100); }").unwrap();
    assert_eq!(v, Value::int(1i64));
}

#[test]
fn min_over_empty_returns_default() {
    let v = run("fn main() -> u64 {
        let xs: [u64] = [];
        return min(xs, 9999u64);
    }").unwrap();
    assert_eq!(v, Value::U64(9999));
}

#[test]
fn min_over_pmap_values() {
    let v = run("
        state amounts: pmap<i64, u64>;
        fn main() -> u64 {
            amounts[1] = 100u64;
            amounts[2] = 500u64;
            amounts[3] = 50u64;
            return min(pmap_values(amounts), 0u64);
        }
    ").unwrap();
    assert_eq!(v, Value::U64(50));
}

// ---------- effect classification ----------

#[test]
fn aggregations_are_callable_in_view_fn() {
    // sum/max/min are pure (over their array argument). When the
    // array comes from a view-classified walk, the whole expression
    // stays ReadOnly — fine inside `view`.
    let src = "
        state amounts: pmap<i64, u64>;
        entry view fn total() -> u64 { return sum(pmap_values(amounts), 0u64); }
        fn main() -> u64 {
            amounts[1] = 1u64; amounts[2] = 2u64; amounts[3] = 3u64;
            return total();
        }
    ";
    let kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .execute(src, Fuel::new(20_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::U64(6));
}

#[test]
fn aggregations_callable_in_pure_fn() {
    // sum over a literal array is fully Pure (no state). Should
    // typecheck inside `pure fn`.
    let v = run("
        entry pure fn double_total(xs: [i64]) -> i64 {
            return sum(xs, 0) * 2;
        }
        fn main() -> i64 { return double_total([1, 2, 3, 4]); }
    ").unwrap();
    assert_eq!(v, Value::int(20i64));
}

// ---------- bytecode VM parity ----------

#[test]
fn aggregations_work_on_bytecode_vm() {
    // run() uses the interp; execute() goes through the bytecode VM.
    let src = "
        state amounts: pmap<i64, u64>;
        fn main() -> u64 {
            amounts[1] = 10u64;
            amounts[2] = 20u64;
            amounts[3] = 30u64;
            let total = sum(pmap_values(amounts), 0u64);
            let high  = max(pmap_values(amounts), 0u64);
            return total + high;
        }
    ";
    let kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .execute(src, Fuel::new(20_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::U64(60 + 30));
}
