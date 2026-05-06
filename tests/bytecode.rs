//! Slice 4: bytecode VM tests.
//!
//! Two main goals:
//!  1. Parity — the VM produces the same Value as the tree-walk interpreter
//!     for every program we care about.
//!  2. Fuel — the VM bounds execution and surfaces `out of fuel` deterministically.

use rend::{run, run_bc, Fuel, Value};

fn assert_parity(src: &str) {
    let tw = run(src).expect("tree-walk failed");
    let bc = run_bc(src, Fuel::unlimited()).expect("bytecode failed");
    assert_eq!(tw, bc, "mismatch on:\n{src}");
}

#[test]
fn parity_constant() {
    assert_parity("fn main() -> i64 { return 42; }");
}

#[test]
fn parity_arithmetic_precedence() {
    assert_parity("fn main() -> i64 { return 1 + 2 * 3; }");
    assert_parity("fn main() -> i64 { return (1 + 2) * 3; }");
    assert_parity("fn main() -> i64 { return 100 - 50 - 10; }");
}

#[test]
fn parity_logic() {
    assert_parity("fn main() -> bool { return true && false; }");
    assert_parity("fn main() -> bool { return true || false && false; }");
    assert_parity("fn main() -> bool { return !!true; }");
}

#[test]
fn parity_if_else() {
    let src = "fn main() -> i64 {
        let x = 7;
        if x > 5 { return 100; } else { return 200; }
    }";
    assert_parity(src);
}

#[test]
fn parity_else_if_chain() {
    let src = "entry fn classify(n: i64) -> i64 {
        if n < 0 { return -1; }
        else if n == 0 { return 0; }
        else { return 1; }
    }
    fn main() -> i64 { return classify(0) + classify(7) + classify(-3); }";
    assert_parity(src);
}

#[test]
fn parity_recursion_fib() {
    let src = "entry fn fib(n: i64) -> i64 {
        if n < 2 { return n; }
        return fib(n - 1) + fib(n - 2);
    }
    fn main() -> i64 { return fib(12); }";
    assert_parity(src);
}

#[test]
fn parity_resource_round_trip() {
    let src = "entry fn use_it(r: Resource) -> i64 { return unwrap(r); }
               fn main() -> i64 {
                   let r = resource(99);
                   return use_it(r);
               }";
    assert_parity(src);
}

#[test]
fn parity_let_chain() {
    let src = "fn main() -> i64 {
        let a = 1;
        let b = a + 2;
        let c = b * 3;
        return c - a;
    }";
    assert_parity(src);
}

#[test]
fn fuel_exhaustion_returns_out_of_fuel() {
    // fib(20) is ~14k recursive calls, each costing ~5 ops; well over
    // 1000 fuel. Kept shallow so debug-mode stack frames (the match
    // arms in `vm::run_world` have grown large in unoptimized builds)
    // don't overflow the test thread's stack before we reach the
    // fuel limit. Release builds can do `fib(30)` without issues.
    let src = "entry fn fib(n: i64) -> i64 {
        if n < 2 { return n; }
        return fib(n - 1) + fib(n - 2);
    }
    fn main() -> i64 { return fib(20); }";
    let err = run_bc(src, Fuel::new(1000)).unwrap_err();
    assert!(
        err.to_string().contains("out of fuel"),
        "expected fuel error, got {err}",
    );
}

#[test]
fn small_program_finishes_within_modest_fuel() {
    let src = "fn main() -> i64 { return 1 + 2 + 3 + 4 + 5; }";
    let v = run_bc(src, Fuel::new(100)).unwrap();
    assert_eq!(v, Value::Int(15));
}

#[test]
fn fuel_one_is_too_little() {
    let src = "fn main() -> i64 { return 1; }";
    let err = run_bc(src, Fuel::new(0)).unwrap_err();
    assert!(err.to_string().contains("out of fuel"));
}

#[test]
fn parity_runtime_errors_match() {
    // both backends should report div-by-zero
    let src = "fn main() -> i64 { return 1 / 0; }";
    let tw = run(src).unwrap_err();
    let bc = run_bc(src, Fuel::unlimited()).unwrap_err();
    assert!(tw.to_string().contains("division by zero"));
    assert!(bc.to_string().contains("division by zero"));
}

#[test]
fn parity_examples_runnable() {
    for name in ["01_arithmetic.rd", "02_typed.rd"] {
        let src = std::fs::read_to_string(std::path::Path::new("examples").join(name)).unwrap();
        assert_parity(&src);
    }
}
