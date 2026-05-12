//! Sum types + match. Variants are positional-tuple-style; match
//! is an expression with exhaustiveness over the scrutinee's enum.
//! Wildcards (`_`) cover any unenumerated cases.

use rend::value::Value;
use rend::{run, Engine, Fuel};

#[test]
fn unit_variant_constructs_and_matches() {
    let v = run("
        enum Status { Pending, Active, Closed }
        fn main() -> i64 {
            let s = Status::Active;
            return match s {
                Status::Pending => 0,
                Status::Active  => 1,
                Status::Closed  => 2,
            };
        }
    ").unwrap();
    assert_eq!(v, Value::int(1i64));
}

#[test]
fn tuple_variant_carries_payload() {
    let v = run("
        enum Result { Ok(i64), Err(i64) }
        fn main() -> i64 {
            let r = Result::Ok(42);
            return match r {
                Result::Ok(n)  => n,
                Result::Err(_) => -1,
            };
        }
    ").unwrap();
    assert_eq!(v, Value::int(42i64));
}

#[test]
fn multi_payload_variant_destructures_each_position() {
    let v = run(r#"
        enum Event { Connect, Transfer(Address, u64), Close }
        fn main() -> u64 {
            let e = Event::Transfer(address("0xa"), 123u64);
            return match e {
                Event::Connect => 0u64,
                Event::Transfer(_who, amount) => amount,
                Event::Close => 0u64,
            };
        }
    "#).unwrap();
    assert_eq!(v, Value::U64(123));
}

#[test]
fn wildcard_arm_matches_uncovered_variants() {
    let v = run("
        enum Status { A, B, C, D }
        fn main() -> i64 {
            let s = Status::C;
            return match s {
                Status::A => 1,
                _ => 99,
            };
        }
    ").unwrap();
    assert_eq!(v, Value::int(99i64));
}

#[test]
fn non_exhaustive_match_is_compile_error() {
    let err = run("
        enum Status { A, B, C }
        fn main() -> i64 {
            let s = Status::A;
            return match s {
                Status::A => 1,
                Status::B => 2,
            };
        }
    ").unwrap_err();
    assert!(err.to_string().contains("non-exhaustive"), "got: {err}");
    assert!(err.to_string().contains("\"C\""), "should name missing variant");
}

#[test]
fn duplicate_arm_is_compile_error() {
    let err = run("
        enum E { A, B }
        fn main() -> i64 {
            let v = E::A;
            return match v {
                E::A => 1,
                E::A => 2,
                E::B => 3,
            };
        }
    ").unwrap_err();
    assert!(err.to_string().contains("duplicate"), "got: {err}");
}

#[test]
fn pattern_binding_arity_mismatch_is_compile_error() {
    let err = run("
        enum E { Pair(i64, i64) }
        fn main() -> i64 {
            let v = E::Pair(1, 2);
            return match v {
                E::Pair(a) => a,
            };
        }
    ").unwrap_err();
    assert!(err.to_string().contains("binding"), "got: {err}");
}

#[test]
fn unknown_variant_in_pattern_is_compile_error() {
    let err = run("
        enum E { A, B }
        fn main() -> i64 {
            let v = E::A;
            return match v {
                E::A => 1,
                E::Ghost => 2,
                E::B => 3,
            };
        }
    ").unwrap_err();
    assert!(err.to_string().contains("Ghost"), "got: {err}");
}

#[test]
fn unknown_enum_in_pattern_is_compile_error() {
    let err = run("
        enum E { A }
        enum F { X }
        fn main() -> i64 {
            let v = E::A;
            return match v {
                F::X => 1,
            };
        }
    ").unwrap_err();
    assert!(err.to_string().contains("doesn't match") || err.to_string().contains("'F'"));
}

#[test]
fn arm_bodies_must_have_same_type() {
    let err = run("
        enum E { A, B }
        fn main() -> i64 {
            let v = E::A;
            return match v {
                E::A => 1,
                E::B => true,
            };
        }
    ").unwrap_err();
    assert!(err.to_string().contains("agree"), "got: {err}");
}

#[test]
fn enum_round_trips_through_state() {
    let mut kv = rend::kv::InMemoryKv::new();
    let setup = "
        enum Stage { Draft, Voting, Executed }
        state stage: Stage;
        fn main() -> i64 {
            stage = Stage::Voting;
            return 0;
        }
    ";
    let setup_out = Engine::new().execute(setup, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&setup_out.writes);

    let read = "
        enum Stage { Draft, Voting, Executed }
        state stage: Stage;
        fn main() -> i64 {
            return match stage {
                Stage::Draft    => 0,
                Stage::Voting   => 1,
                Stage::Executed => 2,
            };
        }
    ";
    let out = Engine::new().execute(read, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(1i64));
}

#[test]
fn enum_with_payload_round_trips_through_state() {
    let mut kv = rend::kv::InMemoryKv::new();
    let setup = r#"
        enum Status { Open, Closed(Address, u64) }
        state s: Status;
        fn main() -> i64 {
            s = Status::Closed(address("0xowner"), 999u64);
            return 0;
        }
    "#;
    let setup_out = Engine::new().execute(setup, Fuel::new(10_000), &kv).unwrap();
    kv.apply(&setup_out.writes);

    let read = r#"
        enum Status { Open, Closed(Address, u64) }
        state s: Status;
        fn main() -> u64 {
            return match s {
                Status::Open => 0u64,
                Status::Closed(_, payout) => payout,
            };
        }
    "#;
    let out = Engine::new().execute(read, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(999));
}

#[test]
fn enum_default_is_first_variant() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        enum Stage { Draft, Voting, Executed }
        state stage: Stage;
        fn main() -> i64 {
            return match stage {
                Stage::Draft    => 0,
                Stage::Voting   => 1,
                Stage::Executed => 2,
            };
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    // Unwritten enum state defaults to the first variant.
    assert_eq!(out.result, Value::int(0i64));
}

#[test]
fn option_idiom_via_two_variants() {
    let v = run("
        enum Lookup { Found(u64), Missing }
        entry fn try_get(k: i64) -> Lookup {
            if k == 7 { return Lookup::Found(700u64); }
            return Lookup::Missing;
        }
        fn main() -> u64 {
            return match try_get(7) {
                Lookup::Found(v) => v,
                Lookup::Missing  => 0u64,
            };
        }
    ").unwrap();
    assert_eq!(v, Value::U64(700));
}

#[test]
fn result_idiom_with_chained_match() {
    let v = run(r#"
        enum Outcome { Win(u64), Loss, Draw(string) }
        entry fn play() -> Outcome { return Outcome::Win(1500u64); }
        fn main() -> u64 {
            return match play() {
                Outcome::Win(prize) => prize,
                Outcome::Loss => 0u64,
                Outcome::Draw(_) => 1u64,
            };
        }
    "#).unwrap();
    assert_eq!(v, Value::U64(1500));
}

#[test]
fn enum_in_struct_field() {
    let v = run("
        enum Tier { Bronze, Silver, Gold }
        struct Card { holder: i64, tier: Tier }
        fn main() -> i64 {
            let c = Card { holder: 7, tier: Tier::Gold };
            return match c.tier {
                Tier::Bronze => 1,
                Tier::Silver => 2,
                Tier::Gold   => 3,
            };
        }
    ").unwrap();
    assert_eq!(v, Value::int(3i64));
}

#[test]
fn enum_payload_can_carry_struct_value() {
    let v = run(r#"
        struct Bid { who: Address, amount: u64 }
        enum Auction { Open, Sold(Bid) }
        fn main() -> u64 {
            let a = Auction::Sold(Bid { who: address("0xx"), amount: 250u64 });
            return match a {
                Auction::Open => 0u64,
                Auction::Sold(b) => b.amount,
            };
        }
    "#).unwrap();
    assert_eq!(v, Value::U64(250));
}

#[test]
fn match_inside_function_returns_value_to_caller() {
    let v = run("
        enum Op { Add(i64, i64), Sub(i64, i64), Neg(i64) }
        entry fn run_op(o: Op) -> i64 {
            return match o {
                Op::Add(a, b) => a + b,
                Op::Sub(a, b) => a - b,
                Op::Neg(n)    => -n,
            };
        }
        fn main() -> i64 {
            let a = run_op(Op::Add(10, 5));
            let b = run_op(Op::Sub(10, 5));
            let c = run_op(Op::Neg(7));
            return a + b + c;     // 15 + 5 + (-7) = 13
        }
    ").unwrap();
    assert_eq!(v, Value::int(13i64));
}

#[test]
fn example_30_sum_types_runs_end_to_end() {
    let kv = rend::kv::InMemoryKv::new();
    let src = std::fs::read_to_string("examples/30_sum_types.rd").unwrap();
    let out = Engine::new().execute(&src, Fuel::new(50_000), &kv).unwrap();
    // 100 (lookup) + 999 (closed payout) = 1099
    assert_eq!(out.result, Value::U64(1099));
}

#[test]
fn map_keyed_by_enum_variant_value() {
    // Enums are keyable since payload-free or all-keyable variants
    // are stable.
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        enum Side { Left, Right }
        state counts: map<Side, i64>;
        fn main() -> i64 {
            counts[Side::Left]  = counts[Side::Left] + 1;
            counts[Side::Right] = counts[Side::Right] + 5;
            return counts[Side::Left] + counts[Side::Right];
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(6i64));
}
