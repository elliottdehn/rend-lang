//! Quality-of-life pack: range syntax, bitwise ops, constants, string ops.

use rend::value::Value;
use rend::{run, Engine, Fuel};

// ---------- range syntax ----------

#[test]
fn for_in_exclusive_range() {
    let v = run("
        fn main() -> i64 {
            let total = 0;
            for i in 0..5 { total = total + i; }
            return total;
        }
    ").unwrap();
    assert_eq!(v, Value::int(0i64 + 1 + 2 + 3 + 4));
}

#[test]
fn for_in_inclusive_range() {
    let v = run("
        fn main() -> i64 {
            let total = 0;
            for i in 1..=10 { total = total + i; }
            return total;
        }
    ").unwrap();
    assert_eq!(v, Value::int(55i64));
}

#[test]
fn empty_range_runs_zero_iterations() {
    let v = run("
        fn main() -> i64 {
            let n = 0;
            for _i in 5..5 { n = n + 1; }
            return n;
        }
    ").unwrap();
    assert_eq!(v, Value::int(0i64));
}

#[test]
fn range_with_runtime_bounds() {
    let v = run("
        entry fn count(start: i64, end: i64) -> i64 {
            let n = 0;
            for _i in start..end { n = n + 1; }
            return n;
        }
        fn main() -> i64 {
            return count(2, 7);
        }
    ").unwrap();
    assert_eq!(v, Value::int(5i64));
}

#[test]
fn range_break_continue_works() {
    let v = run("
        fn main() -> i64 {
            let n = 0;
            for i in 0..100 {
                if i == 5 { break; }
                if i == 2 { continue; }
                n = n + 1;
            }
            return n;
        }
    ").unwrap();
    assert_eq!(v, Value::int(4i64)); // 0, 1, 3, 4
}

#[test]
fn nested_range_loops() {
    let v = run("
        fn main() -> i64 {
            let total = 0;
            for i in 0..3 {
                for j in 0..3 {
                    total = total + i * 3 + j;
                }
            }
            return total;     // sum 0..9
        }
    ").unwrap();
    assert_eq!(v, Value::int(36i64));
}

#[test]
fn range_bound_type_mismatch_is_compile_error() {
    let err = run("
        fn main() -> i64 {
            for _i in 0..10u64 { }
            return 0;
        }
    ").unwrap_err();
    assert!(err.to_string().contains("same type") || err.to_string().contains("range"));
}

// ---------- bitwise ops ----------

#[test]
fn bitwise_and_or_xor() {
    let v = run("
        fn main() -> i64 {
            let a = 12;
            let b = 10;
            let and = a & b;
            let or  = a | b;
            let xor = a ^ b;
            return and * 100 + or * 10 + xor;
        }
    ").unwrap();
    // 12 = 0b1100, 10 = 0b1010 → and=8, or=14, xor=6
    assert_eq!(v, Value::int(8i64 * 100 + 14 * 10 + 6));
}

#[test]
fn left_shift_right_shift() {
    let v = run("
        fn main() -> i64 {
            return (1 << 10) + (256 >> 2);     // 1024 + 64
        }
    ").unwrap();
    assert_eq!(v, Value::int(1088i64));
}

#[test]
fn bitwise_works_on_other_int_types() {
    // 0xff00 = 65280, 0x0ff0 = 4080 → and = 0x0f00 = 3840
    // 0x000f = 15  shifted left 4 → 0x00f0 = 240
    // result = 3840 | 240 = 4080
    let v = run("
        fn main() -> u64 {
            return (65280u64 & 4080u64) | (15u64 << 4u64);
        }
    ").unwrap();
    assert_eq!(v, Value::U64(4080));
}

#[test]
fn precedence_arithmetic_above_bitwise() {
    // `a + b & c` parses as `(a + b) & c` because `+`/`-` bind
    // tighter than `&`. (Same as C / Rust.)
    let v = run("
        fn main() -> i64 {
            // (1 + 6) & 5  =  7 & 5  =  5
            return 1 + 6 & 5;
        }
    ").unwrap();
    assert_eq!(v, Value::int(5i64));
}

#[test]
fn sized_shift_overflow_is_runtime_error() {
    // `int` is arbitrary-precision; `1 << 100` is just 2^100, a
    // perfectly valid Int. Shift overflow is only meaningful for
    // sized types — check u64 here.
    let err = run("
        fn main() -> u64 {
            let bad = 1u64 << 100u64;
            return bad;
        }
    ").unwrap_err();
    assert!(err.to_string().contains("overflow"));
}

#[test]
fn bitwise_with_bool_is_compile_error() {
    let err = run("
        fn main() -> i64 {
            let b = true & false;
            return 0;
        }
    ").unwrap_err();
    assert!(err.to_string().contains("BitAnd") || err.to_string().contains("apply"));
}

// ---------- constants ----------

#[test]
fn const_is_usable_in_expression() {
    let v = run("
        const FEE_BPS: u64 = 30u64;
        fn main() -> u64 {
            let amount = 1000u64;
            return amount * FEE_BPS / 10000u64;
        }
    ").unwrap();
    assert_eq!(v, Value::U64(3));
}

#[test]
fn const_with_string_value() {
    let v = run(r#"
        const GREETING: string = "hello";
        fn main() -> i64 {
            return len(GREETING);
        }
    "#).unwrap();
    assert_eq!(v, Value::int(5i64));
}

#[test]
fn const_used_inside_modifier_arg() {
    let v = run("
        const MIN: u64 = 100u64;
        modifier AtLeast(t: u64) {
            assert(t > 0u64);
            _;
        }
        entry fn op() [AtLeast(MIN)] -> u64 { return MIN; }
        fn main() -> u64 { return op(); }
    ").unwrap();
    assert_eq!(v, Value::U64(100));
}

#[test]
fn const_type_mismatch_is_compile_error() {
    let err = run("
        const X: i64 = true;
        fn main() -> i64 { return X; }
    ").unwrap_err();
    assert!(err.to_string().contains("declared") || err.to_string().contains("value"));
}

#[test]
fn const_shadowing_state_is_compile_error() {
    let err = run("
        state c: i64;
        const c: i64 = 1;
        fn main() -> i64 { return c; }
    ").unwrap_err();
    assert!(err.to_string().contains("shadow") || err.to_string().contains("'c'"));
}

#[test]
fn local_shadows_const() {
    // A local with the same name as a const wins inside its scope.
    let v = run("
        const X: i64 = 100;
        fn main() -> i64 {
            let X = 7;
            return X;
        }
    ").unwrap();
    assert_eq!(v, Value::int(7i64));
}

// ---------- string ops ----------

#[test]
fn string_concat_basic() {
    let v = run(r#"
        fn main() -> i64 {
            let s = string_concat("foo", "barbaz");
            return len(s);
        }
    "#).unwrap();
    assert_eq!(v, Value::int(9i64));
}

#[test]
fn string_slice_basic() {
    let v = run(r#"
        fn main() -> bool {
            let s = string_slice("rendlang", 4, 8);
            return s == "lang";
        }
    "#).unwrap();
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn string_contains_finds_substring() {
    let v = run(r#"
        fn main() -> bool {
            return string_contains("hello world", "lo wo");
        }
    "#).unwrap();
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn string_contains_returns_false_for_missing() {
    let v = run(r#"
        fn main() -> bool {
            return string_contains("abc", "xyz");
        }
    "#).unwrap();
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn string_slice_out_of_range_is_runtime_error() {
    let err = run(r#"
        fn main() -> i64 {
            let _s = string_slice("abc", 0, 10);
            return 0;
        }
    "#).unwrap_err();
    assert!(err.to_string().contains("out-of-range"));
}

#[test]
fn string_slice_non_utf8_boundary_is_runtime_error() {
    // "héllo" — the `é` is 2 bytes (UTF-8 0xc3 0xa9). Slicing at
    // byte 2 would split it.
    let err = run(r#"
        fn main() -> bool {
            let s = string_slice("héllo", 2, 3);
            return s == "";
        }
    "#).unwrap_err();
    assert!(err.to_string().contains("char boundary"));
}

// ---------- showcase example ----------

#[test]
fn example_31_qol_pack_runs_end_to_end() {
    let kv = rend::kv::InMemoryKv::new();
    let src = std::fs::read_to_string("examples/31_qol_pack.rd").unwrap();
    let out = Engine::new().execute(&src, Fuel::new(50_000), &kv).unwrap();
    // main() grants alice READ|WRITE then describes her: "READ|WRITE".
    assert_eq!(out.result, Value::Str("READ|WRITE".into()));
}

#[test]
fn string_concat_chain_via_pipe() {
    // Pipe notation across string builtins composes naturally.
    let v = run(r#"
        fn main() -> i64 {
            return ("hello")
                |> string_concat($$, " ")
                |> string_concat($$, "world")
                |> len($$);
        }
    "#).unwrap();
    assert_eq!(v, Value::int(11i64));
}
