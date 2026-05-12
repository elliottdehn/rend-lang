//! End-to-end smoke tests for the examples that exercise the
//! recently-landed surface area: arbitrary-precision integers,
//! source-level JSON literals, and struct field grouping.
//!
//! These exist mainly so the examples can't bitrot — every demo
//! we ship in `crates/rend/examples/` should compile and produce
//! the value its commentary advertises.

use num_bigint::BigInt;
use num_traits::Pow;

use rend::value::Value;

#[test]
fn example_41_bigint_arithmetic_overflows_u128_without_breaking() {
    // Compounds 1u at 100% for 256 rounds: the answer is 2^256,
    // ~1.16e77, well past u128::MAX (~3.4e38). A `u128` would
    // panic; `uint` just keeps going.
    let src = std::fs::read_to_string("examples/41_bigint_arithmetic.rd").unwrap();
    let out = rend::run(&src).unwrap();
    let expected = BigInt::from(2u32).pow(256u32);
    assert_eq!(out, Value::uint(expected));
}

#[test]
fn example_42_polymorphic_json_round_trips_mixed_shapes() {
    // Stores a bool, an int, an object-of-int, an object-holding-an-array,
    // and an explicit null in one `pmap<string, json>`, then reads each
    // back with the right extractor. Encoded result:
    //   attempts*1_000_000 + max_rate*100 + tier2*10 + (1 if unset)
    // = 5*1_000_000 + 100*100 + 50*10 + 1 = 5_010_501.
    let src = std::fs::read_to_string("examples/42_polymorphic_json.rd").unwrap();
    let out = rend::run(&src).unwrap();
    assert_eq!(out, Value::int(5_010_501i64));
}

#[test]
fn example_43_field_groups_keeps_balance_granular_and_profile_grouped() {
    // Two users at disjoint state roots. Alice gets credited twice
    // through the granular `balance` cell; her email gets updated
    // through the `profile` group via RMW. Final result is
    // alice.balance (150) + bob.balance (9) = 159.
    let src = std::fs::read_to_string("examples/43_field_groups.rd").unwrap();
    let out = rend::run(&src).unwrap();
    assert_eq!(out, Value::U64(159));
}
