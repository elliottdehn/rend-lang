//! Slice 13a: integer types u32/u64/u128/i32 + i64.

use rend::ast::Type;
use rend::hashing::state_root;
use rend::kv::InMemoryKv;
use rend::value::Value;
use rend::{run, Engine, Fuel};

#[test]
fn suffixed_literals_have_their_type() {
    assert_eq!(run("fn main() -> i64  { return 42; }").unwrap(), Value::int(42i64));
    assert_eq!(run("fn main() -> i32  { return 42i32; }").unwrap(), Value::I32(42));
    assert_eq!(run("fn main() -> u32  { return 42u32; }").unwrap(), Value::U32(42));
    assert_eq!(run("fn main() -> u64  { return 42u64; }").unwrap(), Value::U64(42));
    assert_eq!(run("fn main() -> u128 { return 42u128; }").unwrap(), Value::U128(42));
}

#[test]
fn arithmetic_is_per_type() {
    assert_eq!(
        run("fn main() -> u128 { return 1000u128 * 1000u128 * 1000u128; }").unwrap(),
        Value::U128(1_000_000_000),
    );
}

#[test]
fn mixed_type_arithmetic_is_compile_error() {
    let err = run("fn main() -> i64 { return 1 + 1u64; }").unwrap_err();
    assert!(err.to_string().contains("cannot apply"), "got {err}");
}

#[test]
fn checked_overflow_on_u128() {
    let max = format!("fn main() -> u128 {{ return {max}u128 + 1u128; }}",
        max = u128::MAX);
    let err = run(&max).unwrap_err();
    assert!(err.to_string().contains("overflow"), "got {err}");
}

#[test]
fn unsigned_neg_of_zero_is_zero() {
    assert_eq!(run("fn main() -> u64 { return -0u64; }").unwrap(), Value::U64(0));
}

#[test]
fn unsigned_neg_of_positive_overflows() {
    let err = run("fn main() -> u64 { return -1u64; }").unwrap_err();
    assert!(err.to_string().contains("overflow"), "got {err}");
}

#[test]
fn conversions_are_bounds_checked() {
    assert_eq!(
        run("fn main() -> u128 { return u128(100); }").unwrap(),
        Value::U128(100),
    );
    let err = run("fn main() -> u32 { return u32(-1); }").unwrap_err();
    assert!(err.to_string().contains("out of range"), "got {err}");
}

#[test]
fn round_trip_to_i64() {
    assert_eq!(
        run("fn main() -> i64 { return i64(42u128); }").unwrap(),
        Value::int(42i64),
    );
}

#[test]
fn u128_state_round_trips_through_kv() {
    let mut kv = InMemoryKv::new();
    let engine = Engine::new();
    let src = "
        state supply: u128;
        fn main() -> u128 { supply = 1000000u128; return supply; }
    ";
    let out = engine.execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::U128(1_000_000));
    kv.apply(&out.writes);
    let stored = kv.get_typed(state_root("main", "supply"), &Type::U128);
    assert_eq!(stored, Some(Value::U128(1_000_000)));
}

#[test]
fn u128_map_keys_work() {
    let kv = InMemoryKv::new();
    let src = "
        state weights: map<u64, u128>;
        fn main() -> u128 {
            weights[7u64] = 99u128;
            return weights[7u64];
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::U128(99));
}

#[test]
fn example_17_int_types_runs() {
    let kv = InMemoryKv::new();
    let src = std::fs::read_to_string("examples/17_integer_types.rd").unwrap();
    let out = Engine::new().execute(&src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::U128(350));
}
