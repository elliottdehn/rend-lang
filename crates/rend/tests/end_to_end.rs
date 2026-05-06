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
    assert_runs_to("fn main() -> i64 { return 42; }", Value::Int(42));
}

#[test]
fn returns_constant_bool() {
    assert_runs_to("fn main() -> bool { return true; }", Value::Bool(true));
}

#[test]
fn arithmetic_precedence() {
    assert_runs_to("fn main() -> i64 { return 1 + 2 * 3; }", Value::Int(7));
    assert_runs_to("fn main() -> i64 { return (1 + 2) * 3; }", Value::Int(9));
    assert_runs_to("fn main() -> i64 { return 10 - 3 - 2; }", Value::Int(5));
    assert_runs_to("fn main() -> i64 { return 8 / 4 / 2; }", Value::Int(1));
    assert_runs_to("fn main() -> i64 { return 17 % 5; }", Value::Int(2));
}

#[test]
fn unary_minus_and_not() {
    assert_runs_to("fn main() -> i64 { return -5 + 7; }", Value::Int(2));
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
    assert_runs_to(src, Value::Int(15));
}

#[test]
fn function_call_with_args() {
    let src = "entry fn add(a: i64, b: i64) -> i64 { return a + b; }
               fn main() -> i64 { return add(2, 3); }";
    assert_runs_to(src, Value::Int(5));
}

#[test]
fn recursive_fibonacci() {
    let src = "entry fn fib(n: i64) -> i64 {
        if n < 2 { return n; }
        return fib(n - 1) + fib(n - 2);
    }
    fn main() -> i64 { return fib(10); }";
    assert_runs_to(src, Value::Int(55));
}

#[test]
fn if_else_branching() {
    let src = "fn main() -> i64 {
        let x = 7;
        if x > 5 { return 100; }
        else { return 200; }
    }";
    assert_runs_to(src, Value::Int(100));
}

#[test]
fn else_if_chain() {
    let src = "entry fn classify(n: i64) -> i64 {
        if n < 0 { return -1; }
        else if n == 0 { return 0; }
        else { return 1; }
    }
    fn main() -> i64 { return classify(0); }";
    assert_runs_to(src, Value::Int(0));
}

#[test]
fn line_comments_dont_break_anything() {
    let src = "// top comment
    fn main() -> i64 {
        // local
        return 1 + 1; // trailing
    }";
    assert_runs_to(src, Value::Int(2));
}

#[test]
fn unit_returning_function_needs_no_return() {
    let src = "entry fn nop() {}
               fn main() -> i64 { nop(); return 1; }";
    assert_runs_to(src, Value::Int(1));
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
fn integer_overflow_is_runtime_error() {
    let src = "fn main() -> i64 { return 9223372036854775807 + 1; }";
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
    assert_errors(src, "expected i64, got bool");
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
