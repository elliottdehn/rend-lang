//! End-to-end source → value tests covering the full pipeline (lex/parse/typeck/interp).

use rend::{run, Value};

fn assert_runs_to(src: &str, expected: Value) {
    match run(src) {
        Ok(v) => assert_eq!(v, expected, "src:\n{src}"),
        Err(e) => panic!("unexpected error in:\n{src}\n→ {e}"),
    }
}

fn assert_errors(src: &str, contains: &str) {
    match run(src) {
        Ok(v) => panic!("expected error, got {v}; src:\n{src}"),
        Err(e) => assert!(
            e.to_string().contains(contains),
            "error '{e}' did not contain '{contains}'",
        ),
    }
}

#[test]
fn returns_constant_int() {
    assert_runs_to("fn main() -> i64 { return 42; }", Value::int(42i64));
}

#[test]
fn returns_constant_bool() {
    assert_runs_to("fn main() -> bool { return true; }", Value::Bool(true));
}

#[test]
fn arithmetic_precedence() {
    assert_runs_to("fn main() -> i64 { return 1 + 2 * 3; }", Value::int(7i64));
    assert_runs_to("fn main() -> i64 { return (1 + 2) * 3; }", Value::int(9i64));
    assert_runs_to("fn main() -> i64 { return 10 - 3 - 2; }", Value::int(5i64));
    assert_runs_to("fn main() -> i64 { return 8 / 4 / 2; }", Value::int(1i64));
    assert_runs_to("fn main() -> i64 { return 17 % 5; }", Value::int(2i64));
}

#[test]
fn unary_minus_and_not() {
    assert_runs_to("fn main() -> i64 { return -5 + 7; }", Value::int(2i64));
    assert_runs_to("fn main() -> bool { return !false; }", Value::Bool(true));
    assert_runs_to("fn main() -> bool { return !!true; }", Value::Bool(true));
}

#[test]
fn comparisons() {
    assert_runs_to("fn main() -> bool { return 1 < 2; }", Value::Bool(true));
    assert_runs_to("fn main() -> bool { return 3 == 3; }", Value::Bool(true));
    assert_runs_to("fn main() -> bool { return 3 != 3; }", Value::Bool(false));
    assert_runs_to("fn main() -> bool { return 5 >= 5; }", Value::Bool(true));
}

#[test]
fn boolean_logic() {
    assert_runs_to("fn main() -> bool { return true && false; }", Value::Bool(false));
    assert_runs_to("fn main() -> bool { return true || false; }", Value::Bool(true));
    // and binds tighter than or
    assert_runs_to(
        "fn main() -> bool { return true || false && false; }",
        Value::Bool(true),
    );
}

#[test]
fn let_binds_locals() {
    let src = "fn main() -> i64 {
        let x = 5;
        let y = 10;
        return x + y;
    }";
    assert_runs_to(src, Value::int(15i64));
}

#[test]
fn function_call_with_args() {
    let src = "entry fn add(a: i64, b: i64) -> i64 { return a + b; }
               fn main() -> i64 { return add(2, 3); }";
    assert_runs_to(src, Value::int(5i64));
}

#[test]
fn recursive_fibonacci() {
    let src = "entry fn fib(n: i64) -> i64 {
        if n < 2 { return n; }
        return fib(n - 1) + fib(n - 2);
    }
    fn main() -> i64 { return fib(10); }";
    assert_runs_to(src, Value::int(55i64));
}

#[test]
fn if_else_branching() {
    let src = "fn main() -> i64 {
        let x = 7;
        if x > 5 { return 100; }
        else { return 200; }
    }";
    assert_runs_to(src, Value::int(100i64));
}

#[test]
fn else_if_chain() {
    let src = "entry fn classify(n: i64) -> i64 {
        if n < 0 { return -1; }
        else if n == 0 { return 0; }
        else { return 1; }
    }
    fn main() -> i64 { return classify(0); }";
    assert_runs_to(src, Value::int(0i64));
}

#[test]
fn line_comments_dont_break_anything() {
    let src = "// top comment
    fn main() -> i64 {
        // local
        return 1 + 1; // trailing
    }";
    assert_runs_to(src, Value::int(2i64));
}

#[test]
fn unit_returning_function_needs_no_return() {
    let src = "entry fn nop() {}
               fn main() -> i64 { nop(); return 1; }";
    assert_runs_to(src, Value::int(1i64));
}

// ----- runtime errors -----

#[test]
fn division_by_zero_is_runtime_error() {
    assert_errors("fn main() -> i64 { return 1 / 0; }", "division by zero");
}

#[test]
fn modulo_by_zero_is_runtime_error() {
    assert_errors("fn main() -> i64 { return 1 % 0; }", "division by zero");
}

#[test]
fn float_literal_and_arithmetic() {
    use rend::value::F64Bits;
    assert_runs_to(
        "fn main() -> float { return 1.5 + 0.25; }",
        rend::Value::Float(F64Bits(1.75)),
    );
    assert_runs_to(
        "fn main() -> float { return 1e10 + 5e9; }",
        rend::Value::Float(F64Bits(1.5e10)),
    );
    assert_runs_to(
        "fn main() -> bool { return 0.1 + 0.2 == 0.3; }",
        // Classic IEEE-754 trap — included to assert we *do* expose
        // standard float semantics rather than silently lying.
        rend::Value::Bool(false),
    );
}

#[test]
fn float_division_by_zero_is_inf_not_error() {
    use rend::value::F64Bits;
    assert_runs_to(
        "fn main() -> float { return 1.0 / 0.0; }",
        rend::Value::Float(F64Bits(f64::INFINITY)),
    );
}

#[test]
fn uint_literal_and_arithmetic() {
    assert_runs_to(
        "fn main() -> uint { return 5u + 3u; }",
        rend::Value::uint(8u32),
    );
}

#[test]
fn uint_arbitrary_precision() {
    // 2^200, comfortably beyond i256 / u256 — uint just handles it.
    let big = num_bigint::BigInt::from(1) << 200u32;
    assert_runs_to(
        "fn main() -> uint {
            let x = 1u;
            for i in 0..200 { x = x * 2u; }
            return x;
        }",
        rend::Value::UInt(big),
    );
}

#[test]
fn uint_subtraction_underflow_is_runtime_error() {
    let src = "fn main() -> uint { return 3u - 5u; }";
    assert_errors(src, "uint underflow");
}

#[test]
fn sized_integer_overflow_is_runtime_error() {
    // `int` / `i64` is arbitrary-precision now — adding past i64::MAX
    // is a valid operation that just produces a bigger BigInt. The
    // overflow behavior is only meaningful for the fixed-width sized
    // types; check `u64` here.
    let src = "fn main() -> u64 { return 18446744073709551615u64 + 1u64; }";
    assert_errors(src, "overflow");
}

// ----- compile-time errors -----

#[test]
fn undefined_variable_is_compile_error() {
    assert_errors("fn main() -> i64 { return x; }", "undefined variable");
}

#[test]
fn unknown_function_is_compile_error() {
    assert_errors("fn main() -> i64 { return ghost(); }", "unknown function");
}

#[test]
fn arity_mismatch_is_compile_error() {
    let src = "entry fn add(a: i64, b: i64) -> i64 { return a + b; }
               fn main() -> i64 { return add(1); }";
    assert_errors(src, "expects 2");
}

#[test]
fn type_error_in_arithmetic() {
    assert_errors("fn main() -> i64 { return 1 + true; }", "Add");
}

#[test]
fn if_condition_must_be_bool() {
    assert_errors(
        "fn main() -> i64 { if 5 { return 1; } else { return 2; } }",
        "must be bool",
    );
}

#[test]
fn return_type_mismatch_is_compile_error() {
    assert_errors(
        "fn main() -> i64 { return true; }",
        "return type mismatch",
    );
}

#[test]
fn arg_type_mismatch_is_compile_error() {
    let src = "entry fn add(a: i64, b: i64) -> i64 { return a + b; }
               fn main() -> i64 { return add(1, true); }";
    assert_errors(src, "expected int, got bool");
}

#[test]
fn function_must_return_on_all_paths() {
    let src = "fn main() -> i64 {
        let x = 5;
        if x > 0 { return 1; }
    }";
    assert_errors(src, "must return on all paths");
}

#[test]
fn parse_error_propagates() {
    assert_errors("fn main(", "expected");
}

#[test]
fn lex_error_propagates() {
    assert_errors("fn main() -> i64 { return @ ; }", "unexpected character");
}
